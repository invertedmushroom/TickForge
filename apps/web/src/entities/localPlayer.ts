import { normalizeDirection } from '../utils/math';
import type { EntityTransform } from '../stdb/connection';
import type { QueuedIntent } from '../net/inputQueue';
import { SECONDS_PER_TICK, SIM_TICKS_PER_SECOND } from '../timing';

export const PLAYER_SPEED = 4.2;
export const LOCAL_RECONCILE_RATE = 8.0;
export const LOCAL_SNAP_DISTANCE = 3.0;
export const LOCAL_RECONCILE_PEAK_DECAY = 0.96;

export { SIM_TICKS_PER_SECOND };

export type PlayerPosition = {
  x: number;
  y: number;
  z: number;
};

export type LocalPlayerReconcileStats = {
  reconcileErrEwma: number;
  reconcileErrMax: number;
  snapCorrections: number;
  replayedInputs: number;
  lastReplayedSequence?: bigint;
  lastAuthoritativeTick?: bigint;
  targetLeadMeters: number;
  replayCollisionCount: number;
  replayRequestedMeters: number;
  replayCorrectedMeters: number;
  replayBlockedRatio: number;
  replayPhysicsMs: number;
};

export type ReconcileDecision = {
  target: PlayerPosition;
  correctionMeters: number;
  shouldSnap: boolean;
};

export type LocalPlayerReconcilerOptions = {
  speed?: number;
  reconcileRate?: number;
  snapDistance?: number;
  secondsPerIntent?: number;
  replayMovement?: ReplayMovementResolver;
};

export type ReplayMovementDiagnostics = {
  collisionCount: number;
  requestedMeters: number;
  correctedMeters: number;
  blockedRatio: number;
  durationMs?: number;
};

export type ReplayMovementResult = {
  position: PlayerPosition;
  diagnostics?: ReplayMovementDiagnostics;
};

export type ReplayMovementResolver = (
  from: PlayerPosition,
  direction: { x: number; z: number },
  speed: number,
  deltaSeconds: number,
) => ReplayMovementResult;

export type ReplayPendingIntentsOptions = {
  speed?: number;
  secondsPerIntent?: number;
  replayMovement?: ReplayMovementResolver;
};

export type PendingIntentReplay = {
  position: PlayerPosition;
  replayedInputs: number;
  lastReplayedSequence?: bigint;
  collisionCount: number;
  requestedMeters: number;
  correctedMeters: number;
  blockedRatio: number;
  physicsMs: number;
};

export class LocalPlayerReconciler {
  private readonly speed: number;
  private readonly reconcileRate: number;
  private readonly snapDistance: number;
  private readonly secondsPerIntent: number;
  private readonly replayMovement?: ReplayMovementResolver;
  private reconcileErrEwma = 0;
  private reconcileErrMax = 0;
  private snapCorrections = 0;
  private replayedInputs = 0;
  private lastReplayedSequence: bigint | undefined;
  private lastAuthoritativeTick: bigint | undefined;
  private targetLeadMeters = 0;
  private correctionOffset: PlayerPosition = { x: 0, y: 0, z: 0 };
  private replayCollisionCount = 0;
  private replayRequestedMeters = 0;
  private replayCorrectedMeters = 0;
  private replayBlockedRatio = 0;
  private replayPhysicsMs = 0;

  constructor(options: LocalPlayerReconcilerOptions = {}) {
    this.speed = options.speed ?? PLAYER_SPEED;
    this.reconcileRate = options.reconcileRate ?? LOCAL_RECONCILE_RATE;
    this.snapDistance = options.snapDistance ?? LOCAL_SNAP_DISTANCE;
    this.secondsPerIntent = options.secondsPerIntent ?? SECONDS_PER_TICK;
    this.replayMovement = options.replayMovement;
  }

  reconcile(
    transform: EntityTransform,
    pendingIntents: readonly QueuedIntent[],
    current: PlayerPosition,
    measureCorrection: boolean,
  ): ReconcileDecision {
    const authoritative = positionFromTransform(transform);
    const replay = replayPendingIntents(authoritative, pendingIntents, {
      speed: this.speed,
      secondsPerIntent: this.secondsPerIntent,
      replayMovement: this.replayMovement,
    });
    const correctionMeters = distance(current, replay.position);
    this.replayedInputs = replay.replayedInputs;
    this.lastReplayedSequence = replay.lastReplayedSequence;
    this.lastAuthoritativeTick = transform.lastTick;
    this.targetLeadMeters = distance(authoritative, replay.position);
    this.replayCollisionCount = replay.collisionCount;
    this.replayRequestedMeters = replay.requestedMeters;
    this.replayCorrectedMeters = replay.correctedMeters;
    this.replayBlockedRatio = replay.blockedRatio;
    this.replayPhysicsMs = replay.physicsMs;

    if (measureCorrection) {
      this.reconcileErrEwma = this.reconcileErrEwma * 0.9 + correctionMeters * 0.1;
      this.reconcileErrMax = Math.max(correctionMeters, this.reconcileErrMax * LOCAL_RECONCILE_PEAK_DECAY);
    }

    const shouldSnap = measureCorrection && correctionMeters > this.snapDistance;
    if (measureCorrection) {
      this.correctionOffset = shouldSnap
        ? { x: 0, y: 0, z: 0 }
        : {
            x: replay.position.x - current.x,
            y: replay.position.y - current.y,
            z: replay.position.z - current.z,
          };
    }
    if (shouldSnap && measureCorrection) {
      this.snapCorrections += 1;
    }

    return {
      target: replay.position,
      correctionMeters,
      shouldSnap,
    };
  }

  predictFrame(current: PlayerPosition, movement: { x: number; z: number }, deltaSeconds: number): PlayerPosition {
    const normalized = normalizeDirection(movement.x, movement.z);
    const next = {
      x: current.x + normalized.x * this.speed * deltaSeconds,
      y: current.y,
      z: current.z + normalized.z * this.speed * deltaSeconds,
    };

    if (hasCorrection(this.correctionOffset)) {
      const blend = Math.min(1, deltaSeconds * this.reconcileRate);
      const correction = {
        x: this.correctionOffset.x * blend,
        y: this.correctionOffset.y * blend,
        z: this.correctionOffset.z * blend,
      };
      next.x += correction.x;
      next.y += correction.y;
      next.z += correction.z;
      this.correctionOffset = {
        x: this.correctionOffset.x - correction.x,
        y: this.correctionOffset.y - correction.y,
        z: this.correctionOffset.z - correction.z,
      };
    }

    return next;
  }

  stats(): LocalPlayerReconcileStats {
    return {
      reconcileErrEwma: this.reconcileErrEwma,
      reconcileErrMax: this.reconcileErrMax,
      snapCorrections: this.snapCorrections,
      replayedInputs: this.replayedInputs,
      lastReplayedSequence: this.lastReplayedSequence,
      lastAuthoritativeTick: this.lastAuthoritativeTick,
      targetLeadMeters: this.targetLeadMeters,
      replayCollisionCount: this.replayCollisionCount,
      replayRequestedMeters: this.replayRequestedMeters,
      replayCorrectedMeters: this.replayCorrectedMeters,
      replayBlockedRatio: this.replayBlockedRatio,
      replayPhysicsMs: this.replayPhysicsMs,
    };
  }
}

export function replayPendingIntents(
  authoritative: PlayerPosition,
  pendingIntents: readonly QueuedIntent[],
  speedOrOptions: number | ReplayPendingIntentsOptions = PLAYER_SPEED,
  legacySecondsPerIntent = SECONDS_PER_TICK,
): PendingIntentReplay {
  const options =
    typeof speedOrOptions === 'number'
      ? { speed: speedOrOptions, secondsPerIntent: legacySecondsPerIntent }
      : speedOrOptions;
  const speed = options.speed ?? PLAYER_SPEED;
  const secondsPerIntent = options.secondsPerIntent ?? SECONDS_PER_TICK;
  const replayMovement = options.replayMovement;
  let position = { ...authoritative };
  let replayedInputs = 0;
  let lastReplayedSequence: bigint | undefined;
  let collisionCount = 0;
  let requestedMeters = 0;
  let correctedMeters = 0;
  let physicsMs = 0;

  const ordered = pendingIntents.slice().sort((a, b) => compareBigInt(a.sequenceId, b.sequenceId));
  for (const intent of ordered) {
    lastReplayedSequence = intent.sequenceId;
    switch (intent.action.tag) {
      case 'Move': {
        const dir = normalizeDirection(intent.action.value.dirX, intent.action.value.dirZ);
        if (dir.x !== 0 || dir.z !== 0) {
          const expectedMeters = speed * secondsPerIntent;
          if (replayMovement) {
            const replayStartedAtMs = nowMs();
            const result = replayMovement(position, dir, speed, secondsPerIntent);
            const diagnostics = result.diagnostics;
            const elapsedMs = diagnostics?.durationMs ?? nowMs() - replayStartedAtMs;
            collisionCount += diagnostics?.collisionCount ?? 0;
            requestedMeters += diagnostics?.requestedMeters ?? expectedMeters;
            correctedMeters += diagnostics?.correctedMeters ?? distance(position, result.position);
            physicsMs += elapsedMs;
            position = { ...result.position };
          } else {
            position = {
              ...position,
              x: position.x + dir.x * expectedMeters,
              z: position.z + dir.z * expectedMeters,
            };
            requestedMeters += expectedMeters;
            correctedMeters += expectedMeters;
          }
        }
        replayedInputs += 1;
        break;
      }
      case 'Stop': {
        replayedInputs += 1;
        break;
      }
      default: {
        break;
      }
    }
  }

  return {
    position,
    replayedInputs,
    lastReplayedSequence,
    collisionCount,
    requestedMeters,
    correctedMeters,
    blockedRatio: blockedRatio(requestedMeters, correctedMeters),
    physicsMs,
  };
}

function positionFromTransform(transform: EntityTransform): PlayerPosition {
  return {
    x: transform.posX,
    y: transform.posY,
    z: transform.posZ,
  };
}


function distance(a: PlayerPosition, b: PlayerPosition): number {
  return Math.hypot(a.x - b.x, a.y - b.y, a.z - b.z);
}

function blockedRatio(requestedMeters: number, correctedMeters: number): number {
  if (requestedMeters < 0.0001) {
    return 0;
  }
  return Math.max(0, Math.min(1, 1 - correctedMeters / requestedMeters));
}

function hasCorrection(offset: PlayerPosition): boolean {
  return Math.abs(offset.x) > 0.0001 || Math.abs(offset.y) > 0.0001 || Math.abs(offset.z) > 0.0001;
}

function nowMs(): number {
  return typeof performance === 'undefined' ? Date.now() : performance.now();
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
