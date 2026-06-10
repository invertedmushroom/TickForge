import type { BatchedIntent, IntentAction } from '@dive/client-contract/bindings/types';

export const INTENT_RING_CAPACITY = 12;
export const RESEND_MIN_AGE_MS = 75;
export const RESEND_HZ = 20;

export type QueuedIntent = {
  sequenceId: bigint;
  clientObservedTick: bigint;
  action: IntentAction;
  pushedAtMs: number;
};

export type IntentRejectKind =
  | 'stale_sequence'
  | 'queue_full'
  | 'batch_too_large'
  | 'client_not_registered'
  | 'wrong_owner'
  | 'instance_state'
  | 'other';

export type IntentQueueSnapshot = {
  nextSequenceId: bigint;
  highestAckedSequence: bigint;
  pendingCount: number;
  resendCount: number;
  rejectCounts: Record<IntentRejectKind, number>;
};

const REJECT_KINDS: IntentRejectKind[] = [
  'stale_sequence',
  'queue_full',
  'batch_too_large',
  'client_not_registered',
  'wrong_owner',
  'instance_state',
  'other',
];

export class IntentQueue {
  private pending: QueuedIntent[] = [];
  private nextSequenceId: bigint;
  private highestAckedSequence: bigint;
  private resendCountValue = 0;
  private readonly rejectCountsValue = Object.fromEntries(REJECT_KINDS.map((kind) => [kind, 0])) as Record<
    IntentRejectKind,
    number
  >;

  constructor(lastProcessedSequence: bigint = 0n) {
    this.highestAckedSequence = lastProcessedSequence;
    this.nextSequenceId = lastProcessedSequence + 1n;
  }

  enqueue(action: IntentAction, clientObservedTick: bigint, nowMs: number = performance.now()): QueuedIntent {
    const sequenceId = this.nextSequenceId;
    this.nextSequenceId += 1n;

    const entry: QueuedIntent = {
      sequenceId,
      clientObservedTick,
      action,
      pushedAtMs: nowMs,
    };

    this.pending = this.pending.filter((intent) => intent.sequenceId > this.highestAckedSequence);
    while (this.pending.length >= INTENT_RING_CAPACITY) {
      this.pending.shift();
    }
    this.pending.push(entry);
    return entry;
  }

  ackUpTo(sequenceId: bigint): void {
    if (sequenceId > this.highestAckedSequence) {
      this.highestAckedSequence = sequenceId;
    }
    this.pending = this.pending.filter((intent) => intent.sequenceId > this.highestAckedSequence);
    if (this.nextSequenceId <= this.highestAckedSequence) {
      this.nextSequenceId = this.highestAckedSequence + 1n;
    }
  }

  resyncFromServer(lastProcessedSequence: bigint): void {
    this.ackUpTo(lastProcessedSequence);
    if (this.nextSequenceId <= lastProcessedSequence) {
      this.nextSequenceId = lastProcessedSequence + 1n;
    }
  }

  staleBatch(nowMs: number = performance.now(), minAgeMs = RESEND_MIN_AGE_MS): BatchedIntent[] {
    this.pending = this.pending.filter((intent) => intent.sequenceId > this.highestAckedSequence);
    const batch = this.pending
      .filter((intent) => nowMs - intent.pushedAtMs >= minAgeMs)
      .map((intent) => ({
        sequenceId: intent.sequenceId,
        clientObservedTick: intent.clientObservedTick,
        action: intent.action,
      }));

    if (batch.length > 0) {
      this.resendCountValue += 1;
    }

    return batch;
  }

  noteReducerReject(message: string, sequenceId?: bigint): IntentRejectKind {
    const kind = classifyReducerError(message);
    this.rejectCountsValue[kind] += 1;
    if (kind === 'stale_sequence') {
      const ackSequence = staleSequenceAck(message) ?? sequenceId;
      if (ackSequence !== undefined) {
        this.ackUpTo(ackSequence);
      }
    }
    return kind;
  }

  snapshot(): IntentQueueSnapshot {
    return {
      nextSequenceId: this.nextSequenceId,
      highestAckedSequence: this.highestAckedSequence,
      pendingCount: this.pending.length,
      resendCount: this.resendCountValue,
      rejectCounts: { ...this.rejectCountsValue },
    };
  }

  /** Returns a shallow-copied list of pending (unacked) intents in sequence order. */
  pendingIntents(): QueuedIntent[] {
    return this.pending
      .filter((intent) => intent.sequenceId > this.highestAckedSequence)
      .slice()
      .sort((a, b) => (a.sequenceId < b.sequenceId ? -1 : a.sequenceId > b.sequenceId ? 1 : 0));
  }
}

export function classifyReducerError(message: string): IntentRejectKind {
  if (message.startsWith('Stale sequence')) {
    return 'stale_sequence';
  }
  if (message.startsWith('Intent queue full')) {
    return 'queue_full';
  }
  if (message.startsWith('Intent batch too large')) {
    return 'batch_too_large';
  }
  if (message.startsWith('Client not registered')) {
    return 'client_not_registered';
  }
  if (message.startsWith('Client does not own this entity')) {
    return 'wrong_owner';
  }
  if (
    message.startsWith('Instance not found') ||
    message.startsWith('Instance full') ||
    message.startsWith('Instance closed')
  ) {
    return 'instance_state';
  }
  return 'other';
}

function staleSequenceAck(message: string): bigint | undefined {
  const authoritative = /(?:last processed|last_processed_sequence|lastProcessedSequence)\D+(\d+)/i.exec(message)?.[1];
  const rejected = /\bgot\D+(\d+)/i.exec(message)?.[1];
  const value = authoritative ?? rejected;
  return value === undefined ? undefined : BigInt(value);
}

export function stopIntent(): IntentAction {
  return { tag: 'Stop' };
}

export function moveIntent(dirX: number, dirY: number, dirZ: number): IntentAction {
  return { tag: 'Move', value: { dirX, dirY, dirZ } };
}

export function interactIntent(entityId: bigint): IntentAction {
  return { tag: 'Interact', value: entityId };
}

export function jumpIntent(): IntentAction {
  return { tag: 'Jump' };
}

export function weaponSwapIntent(): IntentAction {
  return { tag: 'WeaponSwap' };
}

export function blockIntent(dirX: number, dirY: number, dirZ: number): IntentAction {
  return {
    tag: 'Block',
    value: {
      lookDir: {
        dirX,
        dirY,
        dirZ,
      },
    },
  };
}

export function faceToIntent(dirX: number, dirY: number, dirZ: number): IntentAction {
  return { tag: 'FaceTo', value: { dirX, dirY, dirZ } };
}

export function tagTargetIntent(entityId: bigint): IntentAction {
  return { tag: 'TagTarget', value: entityId };
}
