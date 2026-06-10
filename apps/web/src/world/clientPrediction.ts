import RAPIER from '@dimforge/rapier3d-compat';

import type {
  PlayerPosition,
  ReplayMovementDiagnostics,
  ReplayMovementResolver,
  ReplayMovementResult,
} from '../entities/localPlayer';
import { PlayerCharacterController } from './characterController';

export type ClientPhysicsPredictionDiagnostics = {
  replayCollisionCount: number;
  replayRequestedMeters: number;
  replayCorrectedMeters: number;
  replayBlockedRatio: number;
  lastReplayMs: number;
};

export class ClientPhysicsPrediction {
  private readonly world: RAPIER.World;
  private readonly body: RAPIER.RigidBody;
  private readonly collider: RAPIER.Collider;
  private readonly replayController: PlayerCharacterController;
  private readonly staticColliderHandles = new Set<number>();
  private readonly staticColliderFilter = (collider: RAPIER.Collider) => this.staticColliderHandles.has(collider.handle);
  private destroyed = false;
  private lastDiagnostics: ClientPhysicsPredictionDiagnostics = {
    replayCollisionCount: 0,
    replayRequestedMeters: 0,
    replayCorrectedMeters: 0,
    replayBlockedRatio: 0,
    lastReplayMs: 0,
  };

  constructor(world: RAPIER.World, body: RAPIER.RigidBody, collider: RAPIER.Collider) {
    this.world = world;
    this.body = body;
    this.collider = collider;
    this.replayController = new PlayerCharacterController(world, { gravity: 0, groundPull: 0, snapToGroundDistance: 0 });
    this.refreshStaticColliders();
  }

  movementResolver(): ReplayMovementResolver {
    return (from, direction, speed, deltaSeconds) => this.resolveMovement(from, direction, speed, deltaSeconds);
  }

  diagnostics(): ClientPhysicsPredictionDiagnostics {
    return { ...this.lastDiagnostics };
  }

  movementFilterPredicate(): (collider: RAPIER.Collider) => boolean {
    return this.staticColliderFilter;
  }

  refreshStaticColliders(): void {
    this.staticColliderHandles.clear();
    this.world.forEachCollider((collider) => {
      if (isStaticPredictionCollider(collider)) {
        this.staticColliderHandles.add(collider.handle);
      }
    });
  }

  destroy(): void {
    if (this.destroyed) {
      return;
    }
    this.replayController.destroy();
    this.destroyed = true;
  }

  private resolveMovement(
    from: PlayerPosition,
    direction: { x: number; z: number },
    speed: number,
    deltaSeconds: number,
  ): ReplayMovementResult {
    if (this.destroyed) {
      throw new Error('client physics prediction has been destroyed');
    }

    const startedAtMs = nowMs();
    const original = vec3FromVector(this.body.translation());
    let stats!: ReturnType<PlayerCharacterController['moveToward']>;
    let position!: PlayerPosition;
    try {
      this.replayController.snapTo(this.body, from);
      const target = {
        x: from.x + direction.x * speed * deltaSeconds,
        y: from.y,
        z: from.z + direction.z * speed * deltaSeconds,
      };
      stats = this.replayController.moveToward(this.body, this.collider, target, deltaSeconds, {
        applyMode: 'immediate',
        filterPredicate: this.staticColliderFilter,
      });
      position = vec3FromVector(this.body.translation());
    } finally {
      this.body.setTranslation(original, true);
      this.body.setNextKinematicTranslation(original);
      this.world.propagateModifiedBodyPositionsToColliders();
    }

    const durationMs = nowMs() - startedAtMs;
    const diagnostics: ReplayMovementDiagnostics = {
      collisionCount: stats.collisionCount,
      requestedMeters: stats.requestedMeters,
      correctedMeters: stats.correctedMeters,
      blockedRatio: stats.blockedRatio,
      durationMs,
    };
    this.lastDiagnostics = {
      replayCollisionCount: diagnostics.collisionCount,
      replayRequestedMeters: diagnostics.requestedMeters,
      replayCorrectedMeters: diagnostics.correctedMeters,
      replayBlockedRatio: diagnostics.blockedRatio,
      lastReplayMs: durationMs,
    };

    return {
      position,
      diagnostics,
    };
  }
}

export function isStaticPredictionCollider(collider: RAPIER.Collider): boolean {
  const parent = collider.parent();
  return parent === null || parent.isFixed();
}

function vec3FromVector(value: { x: number; y: number; z: number }): PlayerPosition {
  return {
    x: value.x,
    y: value.y,
    z: value.z,
  };
}

function nowMs(): number {
  return typeof performance === 'undefined' ? Date.now() : performance.now();
}
