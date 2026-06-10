import type {
  CombatEvent,
  DeathState,
  EntityHealth,
  EntityLayer,
  Instance,
  InstanceMembership,
  LootPile,
  LootPileItem,
  PlayerInventory,
  WorldEvent,
} from '@dive/client-contract/bindings/types';

import { orderGameEvents, summarizeGameEvents, type GameEventLogEntry, type OrderedGameEvent } from '../events/gameEvents';

export type GameplayRows = {
  healthRows: readonly EntityHealth[];
  deathRows: readonly DeathState[];
  combatEvents: readonly CombatEvent[];
  worldEvents: readonly WorldEvent[];
  entityLayers: readonly EntityLayer[];
  instanceMemberships: readonly InstanceMembership[];
  instances: readonly Instance[];
  lootPiles: readonly LootPile[];
  lootPileItems: readonly LootPileItem[];
  playerInventory: readonly PlayerInventory[];
};

export type GameplaySnapshotState = {
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
};

export function collectGameplaySnapshot(rows: GameplayRows, ownEntityId: bigint | undefined): GameplaySnapshotState {
  const remoteHealth = new Map<bigint, EntityHealth>();
  let ownHealth: EntityHealth | undefined;
  for (const row of rows.healthRows) {
    if (row.entityId === ownEntityId) {
      ownHealth = row;
    } else {
      remoteHealth.set(row.entityId, row);
    }
  }

  const ownDeath = ownEntityId === undefined ? undefined : rows.deathRows.find((row) => row.entityId === ownEntityId);
  const entityLayer = ownEntityId === undefined ? undefined : rows.entityLayers.find((row) => row.entityId === ownEntityId);
  const ownInstanceMembership =
    ownEntityId === undefined ? undefined : rows.instanceMemberships.find((row) => row.entityId === ownEntityId);
  const ownInstance =
    ownInstanceMembership === undefined
      ? undefined
      : rows.instances.find((row) => row.instanceId === ownInstanceMembership.instanceId);
  const lootPiles = rows.lootPiles.slice();
  const lootPileItems = rows.lootPileItems.slice();
  const ownInventory =
    ownEntityId === undefined
      ? []
      : rows.playerInventory
          .filter((row) => row.ownerEntity === ownEntityId)
          .sort((a, b) => a.slotIndex - b.slotIndex);
  const combatEvents = rows.combatEvents.slice();
  const worldEvents = rows.worldEvents.slice();
  const gameEvents = orderGameEvents(combatEvents, worldEvents);

  return {
    ownHealth,
    remoteHealth,
    ownDeath,
    entityLayer,
    ownInstanceMembership,
    ownInstance,
    lootPiles,
    lootPileItems,
    ownInventory,
    combatEvents,
    worldEvents,
    gameEvents,
    gameEventLog: summarizeGameEvents(gameEvents, ownEntityId),
    healthRows: rows.healthRows.length,
    deathRows: rows.deathRows.length,
    combatEventCount: rows.combatEvents.length,
    worldEventCount: rows.worldEvents.length,
  };
}
