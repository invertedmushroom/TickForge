import { DbConnection, type SubscriptionHandle } from '@dive/client-contract/bindings';
import type {
  ClientSequence,
  CombatEvent,
  DeathState,
  Entity as NearbyEntity,
  EntityHealth,
  EntityLayer,
  EntityTransform,
  Instance,
  InstanceMembership,
  IntentAction,
  LootPile,
  LootPileItem,
  ModuleConfig,
  PlayerInventory,
  RegionInfo,
  SimTick,
  WorldEvent,
} from '@dive/client-contract/bindings/types';
import type { Identity } from 'spacetimedb';

import { ALWAYS_ON_SUBSCRIPTIONS, DEFAULT_STDB_URI, FEATURE_SUBSCRIPTIONS, MODULE_NAME, shortHash } from '../contract';
import type { GameEventLogEntry, OrderedGameEvent } from '../events/gameEvents';
import { collectGameplaySnapshot } from './gameplaySnapshot';
import { IntentQueue, type IntentQueueSnapshot, type QueuedIntent } from '../net/inputQueue';
import { filterLiveRemoteSnapshot } from './liveRemoteSnapshot';
import {
  FeatureSubscriptionManager,
  type FeatureSubscriptionStats,
  subscriptionErrorMessage,
} from './subscriptions';

export type { DeathState, EntityHealth, EntityLayer, EntityTransform, Instance, InstanceMembership, NearbyEntity };
export type { LootPile, LootPileItem, PlayerInventory };

export type StdbConnectionState =
  | 'idle'
  | 'connecting'
  | 'connected'
  | 'subscribed'
  | 'ready'
  | 'offline'
  | 'error';

export type StartupReadiness = {
  hasClientSequence: boolean;
  hasModuleConfig: boolean;
  hasSimTick: boolean;
  hasOwnTransform: boolean;
  ready: boolean;
};

export type StdbSnapshot = {
  state: StdbConnectionState;
  uri: string;
  moduleName: string;
  identityHex?: string;
  identityShort?: string;
  entityId?: bigint;
  latestTick?: bigint;
  nextTickId?: bigint;
  layer?: number;
  nearbyTransforms: number;
  subscriptionCount: number;
  featureSubscriptions: FeatureSubscriptionStats;
  readiness: StartupReadiness;
  inputQueue: IntentQueueSnapshot;
  pendingIntents: QueuedIntent[];
  ownTransform?: EntityTransform;
  remoteTransforms: EntityTransform[];
  remoteEntities: Map<bigint, NearbyEntity>;
  ownHealth?: EntityHealth;
  remoteHealth: Map<bigint, EntityHealth>;
  ownDeath?: DeathState;
  entityLayer?: EntityLayer;
  ownInstanceMembership?: InstanceMembership;
  ownInstance?: Instance;
  lootPiles: LootPile[];
  lootPileItems: LootPileItem[];
  ownInventory: PlayerInventory[];
  combatEvents: CombatEvent[];
  worldEvents: WorldEvent[];
  gameEvents: OrderedGameEvent[];
  gameEventLog: GameEventLogEntry[];
  healthRows: number;
  deathRows: number;
  combatEventCount: number;
  worldEventCount: number;
  tickStallMs: number;
  serverStalled: boolean;
  reconnectAttempts: number;
  error?: string;
};

export type StdbClientOptions = {
  uri?: string;
  moduleName?: string;
  autoSpawn?: boolean;
  autoReconnect?: boolean;
};

type Listener = (snapshot: StdbSnapshot) => void;

const EVENT_BUFFER_LIMIT = 512;
const SNAPSHOT_HEARTBEAT_MS = 1000;

type RowCallback<Row> = (ctx: unknown, row: Row) => void;
type RowUpdateCallback<Row> = (ctx: unknown, oldRow: Row, row: Row) => void;

type WatchableTable<Row> = {
  onInsert(callback: RowCallback<Row>): void;
  removeOnInsert(callback: RowCallback<Row>): void;
  onDelete(callback: RowCallback<Row>): void;
  removeOnDelete(callback: RowCallback<Row>): void;
  onUpdate?(callback: RowUpdateCallback<Row>): void;
  removeOnUpdate?(callback: RowUpdateCallback<Row>): void;
};

export class StdbClient {
  private connection?: DbConnection;
  private identity?: Identity;
  private subscription?: SubscriptionHandle;
  private pollHandle?: number;
  private snapshotFrameHandle?: number;
  private state: StdbConnectionState = 'idle';
  private error?: string;
  private snapshotValue: StdbSnapshot;
  private reconnectHandle?: number;
  private reconnectAttempts = 0;
  private manualDisconnect = false;
  private lastObservedTick?: bigint;
  private lastObservedTickAtMs = performance.now();
  private readonly inputQueue = new IntentQueue();
  private readonly listeners = new Set<Listener>();
  private readonly uri: string;
  private readonly moduleName: string;
  private readonly autoSpawn: boolean;
  private readonly autoReconnect: boolean;
  private readonly featureSubscriptions: FeatureSubscriptionManager;
  private readonly combatEventBuffer: CombatEvent[] = [];
  private readonly worldEventBuffer: WorldEvent[] = [];
  private readonly aoiRegionByEntity = new Map<bigint, RegionInfo>();
  private readonly nearbyTransformsByEntity = new Map<bigint, EntityTransform>();
  private readonly nearbyEntitiesByEntity = new Map<bigint, NearbyEntity>();
  private readonly nearbyHealthByEntity = new Map<bigint, EntityHealth>();
  private readonly tableListenerCleanups: Array<() => void> = [];
  private combatEventInsertHandler?: (ctx: unknown, row: CombatEvent) => void;
  private worldEventInsertHandler?: (ctx: unknown, row: WorldEvent) => void;

  constructor(options: StdbClientOptions = {}) {
    this.uri = options.uri ?? import.meta.env.VITE_STDB_URI ?? DEFAULT_STDB_URI;
    this.moduleName = options.moduleName ?? import.meta.env.VITE_STDB_MODULE ?? MODULE_NAME;
    this.autoSpawn = options.autoSpawn ?? true;
    this.autoReconnect = options.autoReconnect ?? true;
    this.featureSubscriptions = new FeatureSubscriptionManager(FEATURE_SUBSCRIPTIONS, (message) => {
      this.state = 'error';
      this.error = message;
      this.refreshSnapshot();
    }, () => this.scheduleSnapshot());
    this.snapshotValue = this.emptySnapshot('idle');
  }

  connect(): void {
    if (this.connection || this.state === 'connecting') {
      return;
    }

    this.state = 'connecting';
    this.error = undefined;
    this.manualDisconnect = false;
    this.clearReconnect();
    this.refreshSnapshot();

    try {
      this.connection = DbConnection.builder()
        .withUri(this.uri)
        .withDatabaseName(this.moduleName)
        .withToken(loadToken(tokenStorageKey(this.uri, this.moduleName)) ?? undefined)
        .onConnect((connection, identity, token) => {
          this.identity = identity;
          saveToken(tokenStorageKey(this.uri, this.moduleName), token);
          this.state = 'connected';
          this.reconnectAttempts = 0;
          this.featureSubscriptions.setConnection(connection);
          this.clearReplicaStores();
          this.installReplicaListeners(connection);
          this.installCacheChangeListeners(connection);
          this.installEventTableListeners(connection);
          this.installSubscription(connection);
          this.startPolling();
          this.refreshSnapshot();
        })
        .onConnectError((_ctx, error) => {
          this.state = 'error';
          this.error = error.message;
          this.subscription = undefined;
          this.connection = undefined;
          this.featureSubscriptions.setConnection(undefined);
          this.stopPolling();
          this.cancelScheduledSnapshot();
          this.uninstallTableListeners();
          this.clearReplicaStores();
          this.refreshSnapshot();
          this.scheduleReconnect();
        })
        .onDisconnect((_ctx, error) => {
          this.state = error ? 'error' : 'offline';
          this.error = error?.message;
          this.subscription = undefined;
          this.uninstallEventTableListeners(this.connection);
          this.uninstallTableListeners();
          this.connection = undefined;
          this.featureSubscriptions.setConnection(undefined);
          this.stopPolling();
          this.cancelScheduledSnapshot();
          this.clearReplicaStores();
          this.refreshSnapshot();
          this.scheduleReconnect();
        })
        .build();
    } catch (error) {
      this.state = 'error';
      this.error = error instanceof Error ? error.message : String(error);
      this.refreshSnapshot();
    }
  }

  disconnect(): void {
    this.manualDisconnect = true;
    this.clearReconnect();
    this.stopPolling();
    this.subscription?.unsubscribe();
    this.subscription = undefined;
    this.uninstallEventTableListeners(this.connection);
    this.uninstallTableListeners();
    this.featureSubscriptions.setConnection(undefined);
    this.connection?.disconnect();
    this.connection = undefined;
    this.cancelScheduledSnapshot();
    this.clearReplicaStores();
    this.state = 'offline';
    this.refreshSnapshot();
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    listener(this.snapshotValue);
    return () => this.listeners.delete(listener);
  }

  snapshot(): StdbSnapshot {
    return this.snapshotValue;
  }

  activateFeatureSubscriptions(featureName: string, queries: readonly string[]): () => void {
    const release = this.featureSubscriptions.retain(featureName, queries);
    this.refreshSnapshot();
    return () => {
      release();
      this.refreshSnapshot();
    };
  }

  async spawnPlayer(): Promise<void> {
    if (!this.connection) {
      return;
    }
    try {
      await this.connection.reducers.spawnPlayer({});
    } catch (error) {
      const message = errorMessage(error);
      if (!message.includes('Player already spawned')) {
        this.error = message;
      }
    } finally {
      this.refreshSnapshot();
    }
  }

  async respawnPlayer(): Promise<void> {
    try {
      await this.connection?.reducers.respawnPlayer({});
    } catch (error) {
      this.error = errorMessage(error);
    } finally {
      this.refreshSnapshot();
    }
  }

  async joinInstance(instanceId: bigint): Promise<void> {
    await this.connection?.reducers.joinInstance({ instanceId });
  }

  async leaveInstance(): Promise<void> {
    await this.connection?.reducers.leaveInstance({});
  }

  async claimLoot(lootPileId: bigint, itemId: number, targetSlot: number): Promise<void> {
    if (!this.connection) {
      this.error = 'Not connected';
      this.refreshSnapshot();
      throw new Error(this.error);
    }
    try {
      await this.connection.reducers.claimLoot({ lootPileId, itemId, targetSlot });
    } catch (error) {
      this.error = errorMessage(error);
      throw error;
    } finally {
      this.refreshSnapshot();
    }
  }

  async submitIntent(action: IntentAction): Promise<void> {
    if (
      !this.connection ||
      this.snapshotValue.state !== 'ready' ||
      this.snapshotValue.entityId === undefined ||
      this.snapshotValue.ownDeath
    ) {
      return;
    }

    const queued = this.inputQueue.enqueue(action, this.snapshotValue.latestTick ?? 0n);
    try {
      await this.connection.reducers.submitIntent({
        entityId: this.snapshotValue.entityId,
        sequenceId: queued.sequenceId,
        action: queued.action,
        clientObservedTick: queued.clientObservedTick,
      });
    } catch (error) {
      this.inputQueue.noteReducerReject(errorMessage(error), queued.sequenceId);
    } finally {
      this.refreshSnapshot();
    }
  }

  async resendPendingIntents(nowMs = performance.now()): Promise<void> {
    if (
      !this.connection ||
      this.snapshotValue.state !== 'ready' ||
      this.snapshotValue.entityId === undefined ||
      this.snapshotValue.ownDeath
    ) {
      return;
    }
    const intents = this.inputQueue.staleBatch(nowMs);
    if (intents.length === 0) {
      return;
    }
    try {
      await this.connection.reducers.submitIntentsBatch({
        entityId: this.snapshotValue.entityId,
        intents,
      });
    } catch (error) {
      this.inputQueue.noteReducerReject(errorMessage(error));
    } finally {
      this.refreshSnapshot();
    }
  }

  private installSubscription(connection: DbConnection): void {
    this.subscription = connection
      .subscriptionBuilder()
      .onApplied(() => {
        this.seedReplicaStores(connection);
        this.state = 'subscribed';
        this.refreshSnapshot();
        if (this.autoSpawn && !this.snapshotValue.readiness.hasClientSequence) {
          void this.spawnPlayer();
        }
      })
      .onError((ctx) => {
        this.state = 'error';
        this.error = subscriptionErrorMessage(ctx);
        this.refreshSnapshot();
      })
      .subscribe(ALWAYS_ON_SUBSCRIPTIONS);
  }

  private installEventTableListeners(connection: DbConnection): void {
    this.uninstallEventTableListeners(this.connection);
    this.combatEventBuffer.length = 0;
    this.worldEventBuffer.length = 0;

    this.combatEventInsertHandler = (_ctx: unknown, row: CombatEvent) => {
      this.combatEventBuffer.push(row);
      trimEventBuffer(this.combatEventBuffer);
      this.scheduleSnapshot();
    };
    this.worldEventInsertHandler = (_ctx: unknown, row: WorldEvent) => {
      this.worldEventBuffer.push(row);
      trimEventBuffer(this.worldEventBuffer);
      this.scheduleSnapshot();
    };

    connection.db.combat_event.onInsert(this.combatEventInsertHandler);
    connection.db.world_event.onInsert(this.worldEventInsertHandler);
  }

  private uninstallEventTableListeners(connection: DbConnection | undefined): void {
    if (connection && this.combatEventInsertHandler) {
      connection.db.combat_event.removeOnInsert(this.combatEventInsertHandler);
    }
    if (connection && this.worldEventInsertHandler) {
      connection.db.world_event.removeOnInsert(this.worldEventInsertHandler);
    }
    this.combatEventInsertHandler = undefined;
    this.worldEventInsertHandler = undefined;
  }

  private installReplicaListeners(connection: DbConnection): void {
    this.tableListenerCleanups.push(
      watchReplicatedTable(connection.db.my_region, this.aoiRegionByEntity, () => this.scheduleSnapshot()),
      watchReplicatedTable(connection.db.nearby_transforms, this.nearbyTransformsByEntity, () =>
        this.scheduleSnapshot(),
      ),
      watchReplicatedTable(connection.db.nearby_entities, this.nearbyEntitiesByEntity, () => this.scheduleSnapshot()),
      watchReplicatedTable(connection.db.nearby_health, this.nearbyHealthByEntity, () => this.scheduleSnapshot()),
    );
  }

  private installCacheChangeListeners(connection: DbConnection): void {
    const schedule = () => this.scheduleSnapshot();
    this.tableListenerCleanups.push(
      watchTable(connection.db.client_sequence, schedule),
      watchTable(connection.db.sim_tick, schedule),
      watchTable(connection.db.module_config, schedule),
      watchTable(connection.db.entity_layer, schedule),
      watchTable(connection.db.instance, schedule),
      watchTable(connection.db.instance_membership, schedule),
      watchTable(connection.db.death_state, schedule),
      watchTable(connection.db.player_inventory, schedule),
      watchTable(connection.db.player_equipment, schedule),
      watchTable(connection.db.bank, schedule),
      watchTable(connection.db.party, schedule),
      watchTable(connection.db.party_member, schedule),
      watchTable(connection.db.party_invite, schedule),
      watchTable(connection.db.boss_phase, schedule),
      watchTable(connection.db.world_phase, schedule),
      watchTable(connection.db.respawn_point, schedule),
      watchTable(connection.db.interactable_config, schedule),
      watchTable(connection.db.loot_pile, schedule),
      watchTable(connection.db.loot_pile_item, schedule),
      watchTable(connection.db.active_buff, schedule),
      watchTable(connection.db.npc_state, schedule),
      watchTable(connection.db.entity_team, schedule),
    );
  }

  private uninstallTableListeners(): void {
    for (const cleanup of this.tableListenerCleanups.splice(0)) {
      cleanup();
    }
  }

  private seedReplicaStores(connection: DbConnection): void {
    this.clearReplicaStores();
    seedEntityMap(this.aoiRegionByEntity, connection.db.my_region.iter());
    seedEntityMap(this.nearbyTransformsByEntity, connection.db.nearby_transforms.iter());
    seedEntityMap(this.nearbyEntitiesByEntity, connection.db.nearby_entities.iter());
    seedEntityMap(this.nearbyHealthByEntity, connection.db.nearby_health.iter());
  }

  private clearReplicaStores(): void {
    this.aoiRegionByEntity.clear();
    this.nearbyTransformsByEntity.clear();
    this.nearbyEntitiesByEntity.clear();
    this.nearbyHealthByEntity.clear();
  }

  private startPolling(): void {
    this.stopPolling();
    this.pollHandle = window.setInterval(() => this.refreshSnapshot(), SNAPSHOT_HEARTBEAT_MS);
  }

  private stopPolling(): void {
    if (this.pollHandle !== undefined) {
      window.clearInterval(this.pollHandle);
      this.pollHandle = undefined;
    }
  }

  private scheduleReconnect(): void {
    if (this.manualDisconnect || !this.autoReconnect || this.reconnectHandle !== undefined) {
      return;
    }
    this.reconnectAttempts += 1;
    const delayMs = Math.min(5000, 500 * 2 ** Math.min(4, this.reconnectAttempts - 1));
    this.reconnectHandle = window.setTimeout(() => {
      this.reconnectHandle = undefined;
      this.connect();
    }, delayMs);
  }

  private clearReconnect(): void {
    if (this.reconnectHandle !== undefined) {
      window.clearTimeout(this.reconnectHandle);
      this.reconnectHandle = undefined;
    }
  }

  private scheduleSnapshot(): void {
    if (this.snapshotFrameHandle !== undefined) {
      return;
    }
    this.snapshotFrameHandle = window.requestAnimationFrame(() => {
      this.snapshotFrameHandle = undefined;
      this.refreshSnapshot();
    });
  }

  private cancelScheduledSnapshot(): void {
    if (this.snapshotFrameHandle !== undefined) {
      window.cancelAnimationFrame(this.snapshotFrameHandle);
      this.snapshotFrameHandle = undefined;
    }
  }

  private refreshSnapshot(): void {
    const snapshot = this.readSnapshot();
    this.snapshotValue = snapshot;
    for (const listener of this.listeners) {
      listener(snapshot);
    }
  }

  private readSnapshot(): StdbSnapshot {
    const connection = this.connection;
    if (!connection) {
      return this.emptySnapshot(this.state);
    }

    const clientSequence = readOwnClientSequence(connection, this.identity);
    if (clientSequence) {
      this.inputQueue.resyncFromServer(clientSequence.lastProcessedSequence);
    }

    const moduleConfig = latestModuleConfig(connection);
    const latestTick = latestSimTick(connection);
    const tickStall = this.observeTick(latestTick?.tickId);
    const subscriptionStats = this.featureSubscriptions.stats(ALWAYS_ON_SUBSCRIPTIONS.length);
    const ownEntityId = clientSequence?.entityId;
    const allTransforms = Array.from(this.nearbyTransformsByEntity.values());
    const ownTransform =
      ownEntityId !== undefined ? allTransforms.find((row) => row.entityId === ownEntityId) : undefined;
    const { remoteTransforms, remoteEntities } = filterLiveRemoteSnapshot(
      allTransforms,
      this.nearbyEntitiesByEntity.values(),
      ownEntityId,
    );
    const region = Array.from(this.aoiRegionByEntity.values())[0];
    const gameplay = collectGameplaySnapshot(
      {
        healthRows: Array.from(this.nearbyHealthByEntity.values()),
        deathRows: Array.from(connection.db.death_state.iter()),
        combatEvents: mergeEventRows(Array.from(connection.db.combat_event.iter()), this.combatEventBuffer),
        worldEvents: mergeEventRows(Array.from(connection.db.world_event.iter()), this.worldEventBuffer),
        entityLayers: Array.from(connection.db.entity_layer.iter()),
        instanceMemberships: Array.from(connection.db.instance_membership.iter()),
        instances: Array.from(connection.db.instance.iter()),
        lootPiles: Array.from(connection.db.loot_pile.iter()),
        lootPileItems: Array.from(connection.db.loot_pile_item.iter()),
        playerInventory: Array.from(connection.db.player_inventory.iter()),
      },
      ownEntityId,
    );
    const readiness = {
      hasClientSequence: clientSequence !== undefined,
      hasModuleConfig: moduleConfig !== undefined,
      hasSimTick: latestTick !== undefined,
      hasOwnTransform: ownTransform !== undefined,
      ready:
        clientSequence !== undefined &&
        moduleConfig !== undefined &&
        latestTick !== undefined &&
        ownTransform !== undefined,
    };

    const state = readiness.ready
      ? 'ready'
      : this.state === 'ready'
      ? 'subscribed'
      : this.state;
    return {
      state,
      uri: this.uri,
      moduleName: this.moduleName,
      identityHex: this.identity?.toHexString(),
      identityShort: this.identity ? shortHash(this.identity.toHexString()) : undefined,
      entityId: clientSequence?.entityId,
      latestTick: latestTick?.tickId,
      nextTickId: moduleConfig?.nextTickId,
      layer: gameplay.entityLayer?.layer ?? region?.layer,
      nearbyTransforms: allTransforms.length,
      subscriptionCount: subscriptionStats.totalActiveCount,
      featureSubscriptions: subscriptionStats,
      readiness,
      inputQueue: this.inputQueue.snapshot(),
      pendingIntents: this.inputQueue.pendingIntents(),
      ownTransform,
      remoteTransforms,
      remoteEntities,
      ownHealth: gameplay.ownHealth,
      remoteHealth: gameplay.remoteHealth,
      ownDeath: gameplay.ownDeath,
      entityLayer: gameplay.entityLayer,
      ownInstanceMembership: gameplay.ownInstanceMembership,
      ownInstance: gameplay.ownInstance,
      lootPiles: gameplay.lootPiles,
      lootPileItems: gameplay.lootPileItems,
      ownInventory: gameplay.ownInventory,
      combatEvents: gameplay.combatEvents,
      worldEvents: gameplay.worldEvents,
      gameEvents: gameplay.gameEvents,
      gameEventLog: gameplay.gameEventLog,
      healthRows: gameplay.healthRows,
      deathRows: gameplay.deathRows,
      combatEventCount: gameplay.combatEventCount,
      worldEventCount: gameplay.worldEventCount,
      tickStallMs: tickStall.ms,
      serverStalled: tickStall.stalled,
      reconnectAttempts: this.reconnectAttempts,
      error: this.error,
    };
  }

  private emptySnapshot(state: StdbConnectionState): StdbSnapshot {
    const subscriptionStats = this.featureSubscriptions.stats(ALWAYS_ON_SUBSCRIPTIONS.length);
    return {
      state,
      uri: this.uri,
      moduleName: this.moduleName,
      nearbyTransforms: 0,
      subscriptionCount: subscriptionStats.totalActiveCount,
      featureSubscriptions: subscriptionStats,
      readiness: {
        hasClientSequence: false,
        hasModuleConfig: false,
        hasSimTick: false,
        hasOwnTransform: false,
        ready: false,
      },
      inputQueue: this.inputQueue.snapshot(),
      pendingIntents: this.inputQueue.pendingIntents(),
      remoteTransforms: [],
      remoteEntities: new Map(),
      remoteHealth: new Map(),
      lootPiles: [],
      lootPileItems: [],
      ownInventory: [],
      combatEvents: [],
      worldEvents: [],
      gameEvents: [],
      gameEventLog: [],
      healthRows: 0,
      deathRows: 0,
      combatEventCount: 0,
      worldEventCount: 0,
      tickStallMs: 0,
      serverStalled: false,
      reconnectAttempts: this.reconnectAttempts,
      error: this.error,
    };
  }

  private observeTick(tickId: bigint | undefined): { ms: number; stalled: boolean } {
    const nowMs = performance.now();
    if (tickId === undefined) {
      return { ms: 0, stalled: false };
    }
    if (this.lastObservedTick === undefined || tickId !== this.lastObservedTick) {
      this.lastObservedTick = tickId;
      this.lastObservedTickAtMs = nowMs;
      return { ms: 0, stalled: false };
    }
    const ms = nowMs - this.lastObservedTickAtMs;
    return { ms, stalled: ms > 1000 };
  }
}

export function createStdbClient(options?: StdbClientOptions): StdbClient {
  return new StdbClient(options);
}

function readOwnClientSequence(connection: DbConnection, identity?: Identity): ClientSequence | undefined {
  if (identity) {
    return connection.db.client_sequence.client_identity.find(identity) ?? undefined;
  }
  return Array.from(connection.db.client_sequence.iter())[0];
}

function latestModuleConfig(connection: DbConnection): ModuleConfig | undefined {
  let latest: ModuleConfig | undefined;
  for (const row of connection.db.module_config.iter()) {
    if (!latest || row.nextTickId > latest.nextTickId) {
      latest = row;
    }
  }
  return latest;
}

function latestSimTick(connection: DbConnection): SimTick | undefined {
  let latest: SimTick | undefined;
  for (const row of connection.db.sim_tick.iter()) {
    if (!latest || row.tickId > latest.tickId) {
      latest = row;
    }
  }
  return latest;
}

function trimEventBuffer<T>(rows: T[]): void {
  if (rows.length > EVENT_BUFFER_LIMIT) {
    rows.splice(0, rows.length - EVENT_BUFFER_LIMIT);
  }
}

function mergeEventRows<T extends { eventId: bigint }>(cachedRows: T[], bufferedRows: readonly T[]): T[] {
  if (bufferedRows.length === 0) {
    return cachedRows;
  }
  const byId = new Map<bigint, T>();
  for (const row of cachedRows) {
    byId.set(row.eventId, row);
  }
  for (const row of bufferedRows) {
    byId.set(row.eventId, row);
  }
  return Array.from(byId.values());
}

function watchReplicatedTable<Row extends { entityId: bigint }>(
  table: WatchableTable<Row>,
  rowsByEntity: Map<bigint, Row>,
  onChange: () => void,
): () => void {
  const upsert: RowCallback<Row> = (_ctx, row) => {
    rowsByEntity.set(row.entityId, row);
    onChange();
  };
  const update: RowUpdateCallback<Row> = (_ctx, _oldRow, row) => {
    rowsByEntity.set(row.entityId, row);
    onChange();
  };
  const remove: RowCallback<Row> = (_ctx, row) => {
    rowsByEntity.delete(row.entityId);
    onChange();
  };

  table.onInsert(upsert);
  table.onUpdate?.(update);
  table.onDelete(remove);
  return () => {
    table.removeOnInsert(upsert);
    table.removeOnUpdate?.(update);
    table.removeOnDelete(remove);
  };
}

function watchTable<Row>(table: WatchableTable<Row>, onChange: () => void): () => void {
  const insert: RowCallback<Row> = () => onChange();
  const update: RowUpdateCallback<Row> = () => onChange();
  const remove: RowCallback<Row> = () => onChange();

  table.onInsert(insert);
  table.onUpdate?.(update);
  table.onDelete(remove);
  return () => {
    table.removeOnInsert(insert);
    table.removeOnUpdate?.(update);
    table.removeOnDelete(remove);
  };
}

function seedEntityMap<Row extends { entityId: bigint }>(rowsByEntity: Map<bigint, Row>, rows: Iterable<Row>): void {
  for (const row of rows) {
    rowsByEntity.set(row.entityId, row);
  }
}

function loadToken(key: string): string | undefined {
  return window.localStorage.getItem(key) ?? undefined;
}

function saveToken(key: string, token: string): void {
  window.localStorage.setItem(key, token);
}

function tokenStorageKey(uri: string, moduleName: string): string {
  return `dive-web:stdb-token:${uri}:${moduleName}`;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
