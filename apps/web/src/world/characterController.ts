import RAPIER from '@dimforge/rapier3d-compat';
import physicsPrediction from '@dive/client-contract/physics-prediction.json' with { type: 'json' };

export const PHYSICS_PREDICTION_HASH = physicsPrediction.hash;
export const PLAYER_CAPSULE_HALF_HEIGHT = physicsPrediction.player_capsule.half_height;
export const PLAYER_CAPSULE_RADIUS = physicsPrediction.player_capsule.radius;
export const PLAYER_CAPSULE_FULL_HEIGHT = (PLAYER_CAPSULE_HALF_HEIGHT + PLAYER_CAPSULE_RADIUS) * 2;
export const KCC_OFFSET = relativeKccLength(physicsPrediction.kcc.offset_relative);
export const KCC_NORMAL_NUDGE_FACTOR = physicsPrediction.kcc.normal_nudge_factor;
export const KCC_MAX_SLOPE_CLIMB_RADIANS = physicsPrediction.kcc.max_slope_climb_radians;
export const KCC_SNAP_TO_GROUND_DISTANCE = relativeKccLength(physicsPrediction.kcc.snap_to_ground_relative);
export const KCC_AUTOSTEP_MAX_HEIGHT = relativeKccLength(physicsPrediction.kcc.autostep_max_height_relative);
export const KCC_AUTOSTEP_MIN_WIDTH = relativeKccLength(physicsPrediction.kcc.autostep_min_width_relative);
export const KCC_AUTOSTEP_INCLUDE_DYNAMIC_BODIES = physicsPrediction.kcc.autostep_include_dynamic_bodies;
export const KCC_GROUND_PULL = physicsPrediction.kcc.ground_pull_meters_per_second;
export const KCC_GRAVITY = -physicsPrediction.kcc.gravity_meters_per_second_squared;
export const KCC_MAX_FALL_SPEED = -30;
export const KCC_SURFACE_PROBE_UP = 1.0;
export const KCC_SURFACE_PROBE_DISTANCE = 2.0;
export const KCC_MAX_GROUNDED_SNAP_DELTA = 0.35;

export type CharacterControllerDiagnostics = {
  grounded: boolean;
  collisionCount: number;
  requestedMeters: number;
  correctedMeters: number;
  horizontalRequestedMeters: number;
  horizontalCorrectedMeters: number;
  horizontalBlockedRatio: number;
  verticalRequestedMeters: number;
  verticalCorrectedMeters: number;
  verticalBlockedRatio: number;
  requestedDelta: Vec3;
  correctedDelta: Vec3;
  blockedRatio: number;
  verticalVelocity: number;
};

export type CharacterControllerOptions = {
  offset?: number;
  normalNudgeFactor?: number;
  snapToGroundDistance?: number;
  autostepMaxHeight?: number;
  autostepMinWidth?: number;
  maxSlopeClimbRadians?: number;
  groundPull?: number;
  gravity?: number;
  maxFallSpeed?: number;
};

export type Vec3 = {
  x: number;
  y: number;
  z: number;
};

export type CharacterControllerMoveOptions = {
  applyMode?: 'next' | 'immediate';
  filterFlags?: RAPIER.QueryFilterFlags;
  filterGroups?: RAPIER.InteractionGroups;
  filterPredicate?: (collider: RAPIER.Collider) => boolean;
};

const EMPTY_DIAGNOSTICS: CharacterControllerDiagnostics = {
  grounded: false,
  collisionCount: 0,
  requestedMeters: 0,
  correctedMeters: 0,
  horizontalRequestedMeters: 0,
  horizontalCorrectedMeters: 0,
  horizontalBlockedRatio: 0,
  verticalRequestedMeters: 0,
  verticalCorrectedMeters: 0,
  verticalBlockedRatio: 0,
  requestedDelta: { x: 0, y: 0, z: 0 },
  correctedDelta: { x: 0, y: 0, z: 0 },
  blockedRatio: 0,
  verticalVelocity: 0,
};

export class PlayerCharacterController {
  private readonly world: RAPIER.World;
  private readonly controller: RAPIER.KinematicCharacterController;
  private readonly groundPull: number;
  private readonly gravity: number;
  private readonly maxFallSpeed: number;
  private verticalVelocity = 0;
  private destroyed = false;
  private lastDiagnostics = { ...EMPTY_DIAGNOSTICS };

  constructor(world: RAPIER.World, options: CharacterControllerOptions = {}) {
    this.world = world;
    this.groundPull = options.groundPull ?? KCC_GROUND_PULL;
    this.gravity = options.gravity ?? KCC_GRAVITY;
    this.maxFallSpeed = options.maxFallSpeed ?? KCC_MAX_FALL_SPEED;
    this.controller = world.createCharacterController(options.offset ?? KCC_OFFSET);
    this.controller.setUp({ x: 0, y: 1, z: 0 });
    this.controller.setNormalNudgeFactor(options.normalNudgeFactor ?? KCC_NORMAL_NUDGE_FACTOR);
    this.controller.setSlideEnabled(true);
    this.controller.setApplyImpulsesToDynamicBodies(false);
    this.controller.setMaxSlopeClimbAngle(options.maxSlopeClimbRadians ?? KCC_MAX_SLOPE_CLIMB_RADIANS);
    this.controller.enableSnapToGround(options.snapToGroundDistance ?? KCC_SNAP_TO_GROUND_DISTANCE);
    this.controller.enableAutostep(
      options.autostepMaxHeight ?? KCC_AUTOSTEP_MAX_HEIGHT,
      options.autostepMinWidth ?? KCC_AUTOSTEP_MIN_WIDTH,
      KCC_AUTOSTEP_INCLUDE_DYNAMIC_BODIES,
    );
  }

  moveToward(
    body: RAPIER.RigidBody,
    collider: RAPIER.Collider,
    target: Vec3,
    deltaSeconds: number,
    options: CharacterControllerMoveOptions = {},
  ): CharacterControllerDiagnostics {
    if (this.destroyed) {
      throw new Error('player character controller has been destroyed');
    }

    const current = body.translation();
    const desired = {
      x: target.x - current.x,
      y: target.y - current.y + this.verticalDisplacement(deltaSeconds),
      z: target.z - current.z,
    };

    this.world.propagateModifiedBodyPositionsToColliders();
    this.controller.computeColliderMovement(
      collider,
      desired,
      options.filterFlags,
      options.filterGroups,
      options.filterPredicate,
    );
    let corrected = toVec3(this.controller.computedMovement());
    let next = {
      x: current.x + corrected.x,
      y: current.y + corrected.y,
      z: current.z + corrected.z,
    };
    let grounded = this.controller.computedGrounded();
    const repair = this.repairGroundedSurface(body, collider, current, next, desired, options);
    if (repair) {
      corrected = repair.corrected;
      next = repair.next;
      grounded = true;
    }

    if (options.applyMode === 'immediate') {
      body.setTranslation(next, true);
      body.setNextKinematicTranslation(next);
      this.world.propagateModifiedBodyPositionsToColliders();
    } else {
      body.setNextKinematicTranslation(next);
    }

    if (grounded && this.verticalVelocity < 0) {
      this.verticalVelocity = 0;
    }
    const requestedMeters = vectorLength(desired);
    const correctedMeters = vectorLength(corrected);
    const horizontalRequestedMeters = horizontalLength(desired);
    const horizontalCorrectedMeters = horizontalLength(corrected);
    const verticalRequestedMeters = Math.abs(desired.y);
    const verticalCorrectedMeters = Math.abs(corrected.y);
    const rawCollisionCount = this.controller.numComputedCollisions();
    const movementWasBlocked = correctedMeters < requestedMeters - 0.001;

    this.lastDiagnostics = {
      grounded,
      collisionCount: rawCollisionCount > 0 || movementWasBlocked ? Math.max(1, rawCollisionCount) : 0,
      requestedMeters,
      correctedMeters,
      horizontalRequestedMeters,
      horizontalCorrectedMeters,
      horizontalBlockedRatio: blockedRatio(horizontalRequestedMeters, horizontalCorrectedMeters),
      verticalRequestedMeters,
      verticalCorrectedMeters,
      verticalBlockedRatio: blockedRatio(verticalRequestedMeters, verticalCorrectedMeters),
      requestedDelta: toVec3(desired),
      correctedDelta: toVec3(corrected),
      blockedRatio: blockedRatio(requestedMeters, correctedMeters),
      verticalVelocity: this.verticalVelocity,
    };
    return this.lastDiagnostics;
  }

  snapTo(body: RAPIER.RigidBody, position: Vec3): void {
    this.verticalVelocity = 0;
    body.setTranslation(position, true);
    body.setNextKinematicTranslation(position);
    this.world.propagateModifiedBodyPositionsToColliders();
    this.lastDiagnostics = {
      ...this.lastDiagnostics,
      verticalVelocity: this.verticalVelocity,
    };
  }

  diagnostics(): CharacterControllerDiagnostics {
    return this.lastDiagnostics;
  }

  destroy(): void {
    if (this.destroyed) {
      return;
    }
    this.world.removeCharacterController(this.controller);
    this.destroyed = true;
  }

  private verticalDisplacement(deltaSeconds: number): number {
    const clampedDelta = Math.max(0, Math.min(deltaSeconds, 0.1));
    if (clampedDelta === 0) {
      return 0;
    }
    if (this.lastDiagnostics.grounded && this.groundPull > 0) {
      this.verticalVelocity = 0;
      return -this.groundPull * clampedDelta;
    }
    if (this.gravity === 0) {
      return 0;
    }
    this.verticalVelocity = Math.max(this.maxFallSpeed, this.verticalVelocity + this.gravity * clampedDelta);
    return this.verticalVelocity * clampedDelta;
  }

  private repairGroundedSurface(
    body: RAPIER.RigidBody,
    collider: RAPIER.Collider,
    current: Vec3,
    next: Vec3,
    desired: Vec3,
    options: CharacterControllerMoveOptions,
  ): { next: Vec3; corrected: Vec3 } | undefined {
    if (desired.y < -0.25 || desired.y > 1.0e-5) {
      return undefined;
    }

    const ray = new RAPIER.Ray(
      { x: next.x, y: next.y + KCC_SURFACE_PROBE_UP, z: next.z },
      { x: 0, y: -1, z: 0 },
    );
    const hit = this.world.castRayAndGetNormal(
      ray,
      KCC_SURFACE_PROBE_DISTANCE,
      true,
      options.filterFlags,
      options.filterGroups,
      collider,
      body,
      options.filterPredicate,
    );
    if (!hit) {
      return undefined;
    }

    const surfaceY = ray.pointAt(hit.timeOfImpact).y;
    const restY = surfaceY + PLAYER_CAPSULE_HALF_HEIGHT + PLAYER_CAPSULE_RADIUS + KCC_OFFSET;
    const snapDelta = restY - next.y;
    if (Math.abs(snapDelta) > KCC_MAX_GROUNDED_SNAP_DELTA) {
      return undefined;
    }

    const snappedNext = { ...next, y: restY };
    return {
      next: snappedNext,
      corrected: {
        x: snappedNext.x - current.x,
        y: snappedNext.y - current.y,
        z: snappedNext.z - current.z,
      },
    };
  }
}

function vectorLength(vector: Vec3): number {
  return Math.hypot(vector.x, vector.y, vector.z);
}

function horizontalLength(vector: Vec3): number {
  return Math.hypot(vector.x, vector.z);
}

function toVec3(vector: Vec3): Vec3 {
  return {
    x: vector.x,
    y: vector.y,
    z: vector.z,
  };
}

function blockedRatio(requestedMeters: number, correctedMeters: number): number {
  if (requestedMeters < 0.0001) {
    return 0;
  }
  return Math.max(0, Math.min(1, 1 - correctedMeters / requestedMeters));
}

function relativeKccLength(value: number): number {
  return value * PLAYER_CAPSULE_FULL_HEIGHT;
}
