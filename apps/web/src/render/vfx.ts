import * as THREE from 'three';

import { abilityById } from '../abilities/catalog';
import { visualFor } from './skillVisuals';
import { spawnCapsuleFront, spawnSphereAround } from './vfx/attached';
import { spawnImpactBurst, spawnSphereAt } from './vfx/burst';
import { spawnGroundTorus } from './vfx/hazard';
import { spawnProjectileByDirection, spawnProjectileToTarget } from './vfx/projectile';
import { spawnTeleportRing } from './vfx/teleport';
import { disposeMesh, type ActiveEffect, type CasterAttachment } from './vfx/types';

export type { CasterAttachment } from './vfx/types';

const DEFAULT_PROJECTILE_SPEED_UNITS_PER_SECOND = 20;
const DEFAULT_PROJECTILE_MAX_RANGE = 30;

export class VfxManager {
  public group: THREE.Group;
  private activeEffects: ActiveEffect[] = [];

  constructor() {
    this.group = new THREE.Group();
    this.group.name = 'skill-vfx';
  }

  update(deltaSeconds: number) {
    this.activeEffects = this.activeEffects.filter(effect => effect.update(deltaSeconds));
  }

  activeCount(): number {
    return this.activeEffects.length;
  }

  spawnSkillCast(
    abilityId: number,
    fallbackOrigin: THREE.Vector3,
    options: {
      caster?: CasterAttachment;
      targetPosition?: THREE.Vector3;
    } = {},
  ) {
    const ability = abilityById(abilityId);
    if (!ability) return;
    const visual = visualFor(ability);

    switch (visual.vfx.kind) {
      case 'none':
        return;
      case 'capsule_front':
        spawnCapsuleFront(
          this.group,
          this.activeEffects,
          visual.vfx.radius,
          visual.vfx.halfHeight,
          visual.vfx.color,
          visual.lifetimeSeconds,
          options.caster,
          fallbackOrigin,
        );
        return;
      case 'sphere_around':
        spawnSphereAround(
          this.group,
          this.activeEffects,
          visual.vfx.radius,
          visual.vfx.color,
          visual.lifetimeSeconds,
          visual.vfx.pulseSeconds,
          options.caster,
          fallbackOrigin,
        );
        return;
      case 'ground_torus':
        spawnGroundTorus(
          this.group,
          this.activeEffects,
          visual.vfx.radius,
          visual.vfx.color,
          visual.lifetimeSeconds,
          visual.vfx.pulseSeconds,
          options.targetPosition ?? fallbackOrigin,
        );
        return;
      case 'projectile': {
        const origin = fallbackOrigin.clone();
        origin.y += 1.0;
        const target = options.targetPosition;
        if (!target) {
          spawnImpactBurst(this.group, this.activeEffects, origin, visual.vfx.color);
          return;
        }
        const dest = target.clone();
        dest.y = Math.max(dest.y, origin.y);
        spawnProjectileToTarget(
          this.group,
          this.activeEffects,
          origin,
          dest,
          visual.vfx.color,
          visual.vfx.speed,
          (position, color) => spawnImpactBurst(this.group, this.activeEffects, position, color),
        );
        return;
      }
    }
  }

  spawnProjectileLaunched(
    executionId: bigint,
    abilityId: number,
    origin: THREE.Vector3,
    direction: THREE.Vector3,
    speedUnitsPerTick: number,
    maxRange: number,
  ): void {
    const ability = abilityById(abilityId);
    const visual = ability ? visualFor(ability) : undefined;
    const color = visual?.vfx.kind === 'projectile' ? visual.vfx.color : 0xff6619;
    const speed = Number.isFinite(speedUnitsPerTick) && speedUnitsPerTick > 0
      ? speedUnitsPerTick * 20
      : DEFAULT_PROJECTILE_SPEED_UNITS_PER_SECOND;
    const range = Number.isFinite(maxRange) && maxRange > 0
      ? maxRange
      : ability?.maxRange ?? DEFAULT_PROJECTILE_MAX_RANGE;
    this.removeSkillObject(executionId);
    spawnProjectileByDirection(this.group, this.activeEffects, executionId, origin, direction, color, speed, range);
  }

  spawnHazard(
    executionId: bigint,
    abilityId: number,
    position: THREE.Vector3,
    radius: number,
  ): void {
    const ability = abilityById(abilityId);
    const visual = ability ? visualFor(ability) : undefined;
    const color = visual?.vfx.kind === 'ground_torus' || visual?.vfx.kind === 'sphere_around'
      ? visual.vfx.color
      : 0xe64d00;
    const pulseSeconds = ability && ability.damageIntervalTicks > 0
      ? ability.damageIntervalTicks * 0.05
      : 0;
    const lifetimeSeconds = ability ? Math.max(ability.lingerTicks * 0.05, 1.0) : 3.0;
    spawnGroundTorus(this.group, this.activeEffects, radius, color, lifetimeSeconds, pulseSeconds, position, executionId);
  }

  spawnContactHitbox(
    executionId: bigint,
    abilityId: number,
    position: THREE.Vector3,
    radius: number,
    durationTicks: number,
  ): void {
    const ability = abilityById(abilityId);
    const visual = ability ? visualFor(ability) : undefined;
    const color = visual?.vfx.kind !== 'none' ? visual?.vfx.color ?? 0xffffff : 0xffffff;
    spawnSphereAt(
      this.group,
      this.activeEffects,
      executionId,
      position,
      radius,
      color,
      Math.max(durationTicks * 0.05, 0.2),
    );
  }

  spawnTeleport(entityId: bigint, from: THREE.Vector3, to: THREE.Vector3): void {
    spawnTeleportRing(this.group, this.activeEffects, entityId, from, 0x66e6ff);
    spawnTeleportRing(this.group, this.activeEffects, entityId, to, 0xffee66);
  }

  removeSkillObject(executionId: bigint): void {
    const remaining: ActiveEffect[] = [];
    for (const effect of this.activeEffects) {
      if (effect.executionId === executionId) {
        effect.dispose?.();
      } else {
        remaining.push(effect);
      }
    }
    this.activeEffects = remaining;
  }

  dispose() {
    for (let i = this.group.children.length - 1; i >= 0; i--) {
      const child = this.group.children[i];
      if (child instanceof THREE.Mesh) {
        disposeMesh(child);
      } else {
        this.group.remove(child);
      }
    }
    this.activeEffects = [];
  }
}
