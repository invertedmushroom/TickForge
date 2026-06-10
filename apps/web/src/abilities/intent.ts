import type { AbilityTarget, IntentAction, Vec3F } from '@dive/client-contract/bindings/types';

import { DEFAULT_MAX_ABILITY_RANGE, type AbilityCatalogEntry } from './catalog';

export type Vec3 = {
  x: number;
  y: number;
  z: number;
};

export type AbilityAimState = {
  playerPosition: Vec3;
  aimDirection?: Vec3;
  groundPoint?: Vec3;
  groundValid: boolean;
  softTarget?: bigint;
  selectedTarget?: bigint;
};

export type AbilityIntentResult =
  | {
      ok: true;
      action: IntentAction;
      targetPoint?: Vec3;
      targetHint?: bigint;
    }
  | {
      ok: false;
      reason: 'disabled' | 'missing_direction' | 'missing_ground' | 'missing_target';
    };

export type RemoteTransformLike = {
  entityId: bigint;
  posX: number;
  posY: number;
  posZ: number;
};

export type SoftTargetCandidate = {
  entityId: bigint;
  position: Vec3;
  radius?: number;
};

const MIN_DIRECTION_LENGTH_SQ = 1e-8;
const DEFAULT_SOFT_TARGET_RADIUS = 0.75;

export function buildUseAbilityIntent(ability: AbilityCatalogEntry, aim: AbilityAimState): AbilityIntentResult {
  if (!ability.enabled) {
    return { ok: false, reason: 'disabled' };
  }

  switch (ability.targetingMode) {
    case 'direction_target': {
      const direction = normalizeVec3(aim.aimDirection);
      if (!direction) {
        return { ok: false, reason: 'missing_direction' };
      }
      return {
        ok: true,
        action: useAbilityAction(ability.abilityId, { tag: 'Direction', value: vec3f(direction) }, undefined),
      };
    }
    case 'aim_assist': {
      const direction = normalizeVec3(aim.aimDirection);
      if (!direction) {
        return { ok: false, reason: 'missing_direction' };
      }
      return {
        ok: true,
        action: useAbilityAction(ability.abilityId, { tag: 'Direction', value: vec3f(direction) }, aim.softTarget),
        targetHint: aim.softTarget,
      };
    }
    case 'raycast_strict': {
      // Direction-only path. Server resolves the raycast against authoritative
      // geometry; client target hints are intentionally not attached.
      const direction = normalizeVec3(aim.aimDirection);
      if (!direction) {
        return { ok: false, reason: 'missing_direction' };
      }
      return {
        ok: true,
        action: useAbilityAction(ability.abilityId, { tag: 'Direction', value: vec3f(direction) }, undefined),
      };
    }
    case 'entity_target': {
      if (aim.selectedTarget === undefined) {
        return { ok: false, reason: 'missing_target' };
      }
      return {
        ok: true,
        action: useAbilityAction(
          ability.abilityId,
          { tag: 'Entity', value: aim.selectedTarget },
          aim.selectedTarget,
        ),
        targetHint: aim.selectedTarget,
      };
    }
    case 'ground_target': {
      if (!aim.groundPoint || !aim.groundValid) {
        return { ok: false, reason: 'missing_ground' };
      }
      const clamped = clampGroundTargetPosition(
        aim.playerPosition,
        aim.groundPoint,
        ability.maxRange ?? DEFAULT_MAX_ABILITY_RANGE,
      );
      return {
        ok: true,
        action: useAbilityAction(ability.abilityId, { tag: 'Position', value: vec3f(clamped) }, undefined),
        targetPoint: clamped,
      };
    }
    case 'self_only':
    case 'caster_offset':
      return {
        ok: true,
        action: useAbilityAction(ability.abilityId, { tag: 'None' }, undefined),
      };
    default:
      return { ok: false, reason: 'disabled' };
  }
}

export function directionFromAimPoint(playerPosition: Vec3, aimPoint: Vec3 | undefined, fallback?: Vec3): Vec3 | undefined {
  if (aimPoint) {
    const direction = normalizeVec3({
      x: aimPoint.x - playerPosition.x,
      y: aimPoint.y - playerPosition.y,
      z: aimPoint.z - playerPosition.z,
    });
    if (direction) {
      return direction;
    }
  }
  return normalizeVec3(fallback);
}

export function clampGroundTargetPosition(playerPosition: Vec3, point: Vec3, maxRange: number): Vec3 {
  if (!Number.isFinite(maxRange) || maxRange <= 0) {
    return { ...point };
  }

  const dx = point.x - playerPosition.x;
  const dz = point.z - playerPosition.z;
  const distSq = dx * dx + dz * dz;
  const maxSq = maxRange * maxRange;
  if (!Number.isFinite(distSq) || distSq <= maxSq) {
    return { ...point };
  }

  const invDist = 1 / Math.sqrt(distSq);
  return {
    x: playerPosition.x + dx * invDist * maxRange,
    y: point.y,
    z: playerPosition.z + dz * invDist * maxRange,
  };
}

export function findSoftTarget(
  rayOrigin: Vec3,
  rayDirection: Vec3,
  candidates: readonly SoftTargetCandidate[],
  maxDistance = DEFAULT_MAX_ABILITY_RANGE * 2,
): SoftTargetCandidate | undefined {
  const direction = normalizeVec3(rayDirection);
  if (!direction) {
    return undefined;
  }

  let best: { candidate: SoftTargetCandidate; distanceAlongRay: number } | undefined;
  for (const candidate of candidates) {
    const radius = candidate.radius ?? DEFAULT_SOFT_TARGET_RADIUS;
    const toCenter = subtract(candidate.position, rayOrigin);
    const distanceAlongRay = dot(toCenter, direction);
    if (distanceAlongRay < 0 || distanceAlongRay > maxDistance) {
      continue;
    }
    const closest = {
      x: rayOrigin.x + direction.x * distanceAlongRay,
      y: rayOrigin.y + direction.y * distanceAlongRay,
      z: rayOrigin.z + direction.z * distanceAlongRay,
    };
    const missDistanceSq = lengthSq(subtract(candidate.position, closest));
    if (missDistanceSq > radius * radius) {
      continue;
    }
    if (!best || distanceAlongRay < best.distanceAlongRay) {
      best = { candidate, distanceAlongRay };
    }
  }
  return best?.candidate;
}

export function softTargetCandidatesFromTransforms(
  transforms: readonly RemoteTransformLike[],
): SoftTargetCandidate[] {
  return transforms.map((transform) => ({
    entityId: transform.entityId,
    position: {
      x: transform.posX,
      y: transform.posY + 0.35,
      z: transform.posZ,
    },
  }));
}

export function releaseAbilityAction(abilityId: number): IntentAction {
  return {
    tag: 'ReleaseAbility',
    value: abilityId,
  };
}

function useAbilityAction(abilityId: number, target: AbilityTarget, targetHint: bigint | undefined): IntentAction {
  return {
    tag: 'UseAbility',
    value: {
      abilityId,
      target,
      targetHint,
    },
  };
}

function vec3f(value: Vec3): Vec3F {
  return {
    x: value.x,
    y: value.y,
    z: value.z,
  };
}

function normalizeVec3(value: Vec3 | undefined): Vec3 | undefined {
  if (!value) {
    return undefined;
  }
  const lenSq = lengthSq(value);
  if (!Number.isFinite(lenSq) || lenSq < MIN_DIRECTION_LENGTH_SQ) {
    return undefined;
  }
  const invLen = 1 / Math.sqrt(lenSq);
  return {
    x: value.x * invLen,
    y: value.y * invLen,
    z: value.z * invLen,
  };
}

function subtract(a: Vec3, b: Vec3): Vec3 {
  return {
    x: a.x - b.x,
    y: a.y - b.y,
    z: a.z - b.z,
  };
}

function dot(a: Vec3, b: Vec3): number {
  return a.x * b.x + a.y * b.y + a.z * b.z;
}

function lengthSq(value: Vec3): number {
  return dot(value, value);
}
