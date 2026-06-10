import type { CombatEvent, WorldEvent } from '@dive/client-contract/bindings/types';

export type OrderedGameEvent =
  | {
      source: 'combat';
      eventId: bigint;
      tickId: bigint;
      eventSequence: number;
      row: CombatEvent;
    }
  | {
      source: 'world';
      eventId: bigint;
      tickId: bigint;
      eventSequence: number;
      row: WorldEvent;
    };

export type GameEventLogEntry = {
  key: string;
  tone: 'damage' | 'heal' | 'death' | 'world' | 'neutral';
  text: string;
};

export function orderGameEvents(
  combatEvents: readonly CombatEvent[],
  worldEvents: readonly WorldEvent[],
): OrderedGameEvent[] {
  const byKey = new Map<string, OrderedGameEvent>();

  for (const row of combatEvents) {
    byKey.set(eventIdentityKey('combat', row.tickId, row.eventSequence, row.eventId), {
      source: 'combat',
      eventId: row.eventId,
      tickId: row.tickId,
      eventSequence: row.eventSequence,
      row,
    });
  }

  for (const row of worldEvents) {
    byKey.set(eventIdentityKey('world', row.tickId, row.eventSequence, row.eventId), {
      source: 'world',
      eventId: row.eventId,
      tickId: row.tickId,
      eventSequence: row.eventSequence,
      row,
    });
  }

  return Array.from(byKey.values()).sort(compareOrderedEvents);
}

export function summarizeGameEvents(
  events: readonly OrderedGameEvent[],
  ownEntityId: bigint | undefined,
  limit = 8,
): GameEventLogEntry[] {
  return events
    .map((event) => summarizeGameEvent(event, ownEntityId))
    .filter((entry): entry is GameEventLogEntry => entry !== undefined)
    .slice(-limit);
}

export function summarizeGameEvent(
  event: OrderedGameEvent,
  ownEntityId: bigint | undefined,
): GameEventLogEntry | undefined {
  if (event.source === 'world') {
    return summarizeWorldEvent(event);
  }

  const kind = event.row.eventKind;
  const target = shortEntity(event.row.targetEntity, ownEntityId);
  const source = shortEntity(event.row.sourceEntity, ownEntityId);

  switch (kind.tag) {
    case 'Damage':
      return {
        key: eventKey(event),
        tone: 'damage',
        text: `${source} hit ${target} for ${formatNumber(kind.value.amount)}`,
      };
    case 'Healed':
      return {
        key: eventKey(event),
        tone: 'heal',
        text: `${target} healed ${formatNumber(kind.value.amount)}`,
      };
    case 'EntityDied':
      return {
        key: eventKey(event),
        tone: 'death',
        text: `${target} died`,
      };
    case 'Blocked':
      return {
        key: eventKey(event),
        tone: 'neutral',
        text: `${target} blocked ${source}`,
      };
    default:
      return undefined;
  }
}

function summarizeWorldEvent(event: Extract<OrderedGameEvent, { source: 'world' }>): GameEventLogEntry | undefined {
  const kind = event.row.eventKind;
  switch (kind.tag) {
    case 'EntitySpawned':
      return {
        key: eventKey(event),
        tone: 'world',
        text: `entity ${event.row.entityId} spawned`,
      };
    case 'EntityDespawned':
      return {
        key: eventKey(event),
        tone: 'world',
        text: `entity ${event.row.entityId} despawned`,
      };
    case 'PickupCollected':
      return {
        key: eventKey(event),
        tone: 'world',
        text: `pickup ${kind.value} collected`,
      };
    case 'InteractTriggered':
      return {
        key: eventKey(event),
        tone: 'world',
        text: `interaction ${kind.value} triggered`,
      };
    default:
      return undefined;
  }
}

function compareOrderedEvents(a: OrderedGameEvent, b: OrderedGameEvent): number {
  const tick = compareBigInt(a.tickId, b.tickId);
  if (tick !== 0) {
    return tick;
  }

  const sequence = a.eventSequence - b.eventSequence;
  if (sequence !== 0) {
    return sequence;
  }

  const id = compareBigInt(a.eventId, b.eventId);
  if (id !== 0) {
    return id;
  }

  return a.source.localeCompare(b.source);
}

function compareBigInt(a: bigint, b: bigint): number {
  if (a < b) {
    return -1;
  }
  if (a > b) {
    return 1;
  }
  return 0;
}

function eventKey(event: OrderedGameEvent): string {
  return eventIdentityKey(event.source, event.tickId, event.eventSequence, event.eventId);
}

function eventIdentityKey(source: OrderedGameEvent['source'], tickId: bigint, eventSequence: number, eventId: bigint): string {
  return `${source}:${tickId}:${eventSequence}:${eventId}`;
}

function shortEntity(entityId: bigint, ownEntityId: bigint | undefined): string {
  return entityId === ownEntityId ? 'you' : `#${entityId}`;
}

function formatNumber(value: number): string {
  return Number.isInteger(value) ? `${value}` : value.toFixed(1);
}
