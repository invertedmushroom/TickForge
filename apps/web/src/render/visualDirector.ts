import type { CombatEvent } from '@dive/client-contract/bindings/types';
import * as THREE from 'three';

import { abilityById } from '../abilities/catalog';
import type { CharacterModel } from './characterModel';
import type { FloatingOverlayLayer } from './floatingOverlay';
import { visualFor } from './skillVisuals';
import type { CasterAttachment } from './vfx';
import { VfxManager } from './vfx';

type CharacterAnimationRequest = Parameters<CharacterModel['playOverlayAnimation']>[0];

export type VisualDirectorOptions = {
  vfxManager: VfxManager;
  floatingOverlay?: FloatingOverlayLayer;
  localCaster: CasterAttachment;
  localPosition: () => THREE.Vector3;
  localCharacterModel: () => CharacterModel | undefined;
  remotePosition: (entityId: bigint) => THREE.Vector3 | undefined;
  remoteCaster: (entityId: bigint) => CasterAttachment;
  playRemoteAnimation: (entityId: bigint, request: CharacterAnimationRequest) => void;
  cancelRemoteAnimation: (entityId: bigint) => void;
};

/**
 * Presentation-only dispatcher for authoritative combat events. It mirrors the
 * Bevy client's combat_log -> vfx bridge: CastStart drives cast/character
 * presentation, while richer lifecycle events drive projectiles, hazards,
 * contact hitboxes, teleports, and cleanup by execution id.
 */
export class VisualDirector {
  constructor(private readonly options: VisualDirectorOptions) {}

  handleCombatEvent(row: CombatEvent, ownEntityId: bigint): void {
    const kind = row.eventKind;
    switch (kind.tag) {
      case 'ProjectileLaunched': {
        const data = kind.value;
        this.options.vfxManager.spawnProjectileLaunched(
          data.executionId,
          data.abilityId,
          new THREE.Vector3(data.originX, data.originY, data.originZ),
          new THREE.Vector3(data.directionX, data.directionY, data.directionZ),
          data.speed,
          data.maxRange,
        );
        return;
      }
      case 'HazardSpawned': {
        const data = kind.value;
        const ability = abilityById(data.abilityId);
        if (ability?.targetingMode === 'self_only') return;
        this.options.vfxManager.spawnHazard(
          data.executionId,
          data.abilityId,
          new THREE.Vector3(data.posX, data.posY, data.posZ),
          data.radius,
        );
        return;
      }
      case 'ContactHitboxSpawned': {
        const data = kind.value;
        this.options.vfxManager.spawnContactHitbox(
          data.executionId,
          data.abilityId,
          new THREE.Vector3(data.posX, data.posY, data.posZ),
          data.radius,
          data.durationTicks,
        );
        return;
      }
      case 'SkillObjectRemoved':
        this.options.vfxManager.removeSkillObject(kind.value);
        return;
      case 'CastStart':
        this.handleCastStart(row.sourceEntity, kind.value.abilityId, ownEntityId);
        return;
      case 'AbilityCancelled':
        this.cancelCharacterAnimation(row.sourceEntity, ownEntityId);
        return;
      case 'Teleported': {
        const data = kind.value;
        this.options.vfxManager.spawnTeleport(
          row.sourceEntity,
          new THREE.Vector3(data.fromX, data.fromY, data.fromZ),
          new THREE.Vector3(data.toX, data.toY, data.toZ),
        );
        return;
      }
      case 'Damage':
        this.spawnFloatingNumber(row.targetEntity, ownEntityId, kind.value.amount, 'damage');
        return;
      case 'Healed':
        this.spawnFloatingNumber(row.targetEntity, ownEntityId, kind.value.amount, 'heal');
        return;
      default:
        return;
    }
  }

  private handleCastStart(sourceEntity: bigint, abilityId: number, ownEntityId: bigint): void {
    const ability = abilityById(abilityId);
    if (!ability) return;
    const visual = visualFor(ability);
    const isLocal = sourceEntity === ownEntityId;
    const sourcePosition = isLocal
      ? this.options.localPosition()
      : this.options.remotePosition(sourceEntity);
    const caster = isLocal
      ? this.options.localCaster
      : this.options.remoteCaster(sourceEntity);

    if (visual.characterAnimation) {
      if (isLocal) {
        this.options.localCharacterModel()?.playOverlayAnimation(visual.characterAnimation);
      } else {
        this.options.playRemoteAnimation(sourceEntity, visual.characterAnimation);
      }
    }

    const castStartDrivesVfx =
      visual.vfx.kind !== 'none' &&
      visual.vfx.kind !== 'projectile' &&
      visual.vfx.kind !== 'ground_torus';
    if (sourcePosition && castStartDrivesVfx) {
      this.options.vfxManager.spawnSkillCast(abilityId, sourcePosition, { caster });
    }
  }

  private cancelCharacterAnimation(sourceEntity: bigint, ownEntityId: bigint): void {
    if (sourceEntity === ownEntityId) {
      this.options.localCharacterModel()?.cancelSkillAnimation();
    } else {
      this.options.cancelRemoteAnimation(sourceEntity);
    }
  }

  private spawnFloatingNumber(
    targetEntity: bigint,
    ownEntityId: bigint,
    amount: number,
    tone: 'damage' | 'heal',
  ): void {
    const overlay = this.options.floatingOverlay;
    if (!overlay) return;
    const base = targetEntity === ownEntityId
      ? this.options.localPosition()
      : this.options.remotePosition(targetEntity);
    if (!base) return;
    overlay.spawnDamage(base.clone().add(new THREE.Vector3(0, 1.35, 0)), amount, tone);
  }
}
