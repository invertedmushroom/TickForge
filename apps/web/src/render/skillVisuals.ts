import type { AbilityCatalogEntry } from '../abilities/catalog';
import type { CharacterOverlayAnimationRequest } from './characterModel';
import { characterAnimationRequestFor, visualProfileFor } from './visualProfiles';

/** How a skill cast should be visualized on a casting entity. */
export type SkillVisual = {
  characterAnimation?: CharacterOverlayAnimationRequest;
  /**
   * Whether to play the character's melee `Attack` animation overlay.
   * Disabled for skills whose identity comes from the world VFX (ground
   * hazards, auras, etc.) so we don't replay the same swing for every cast.
   */
  playMeleeSwing: boolean;
  /**
   * How the cast manifests in the world. The VFX manager dispatches on this.
   */
  vfx:
    | { kind: 'none' }
    /** Melee capsule oriented in the caster's facing direction. */
    | { kind: 'capsule_front'; radius: number; halfHeight: number; color: number }
    /** Hovering / orbiting sphere centered on the caster (PBAoE / aura). */
    | {
        kind: 'sphere_around';
        radius: number;
        color: number;
        /** Seconds between pulse flashes (0 = single steady glow). */
        pulseSeconds: number;
      }
    /** Flat ground reticle that drops at the target position. */
    | { kind: 'ground_torus'; radius: number; color: number; pulseSeconds: number }
    /** Glowing projectile that travels caster -> target then bursts. */
    | { kind: 'projectile'; color: number; speed: number };
  /** Lifetime of the world VFX in seconds. */
  lifetimeSeconds: number;
  /**
   * Suggested duration (ms) to fit the character melee animation into. Only
   * used when `playMeleeSwing` is true.
   */
  meleeWindowMs: number;
};

const SECONDS_PER_TICK = 0.05; // server runs at 20 Hz
const MS_PER_TICK = SECONDS_PER_TICK * 1000;

/**
 * Compute the visual contract for a given ability cast.
 *
 * Derives geometry, animation, lifetime and pulse cadence from the
 * authoritative catalog metadata (`previewShape`, `targetingMode`,
 * `lingerTicks`, `damageIntervalTicks`, `projectileSpeed`) so behaviour
 * stays aligned with the server without a hand-maintained switch.
 */
export function visualFor(ability: AbilityCatalogEntry): SkillVisual {
  const profile = visualProfileFor(ability);
  const color = profile.color;
  const lingerSeconds = Math.max(ability.lingerTicks * SECONDS_PER_TICK, 0);
  const pulseSeconds = ability.damageIntervalTicks > 0
    ? ability.damageIntervalTicks * SECONDS_PER_TICK
    : 0;
  const meleeWindowMs = Math.max(ability.timelineDurationTicks, 1) * MS_PER_TICK;
  const characterAnimation = characterAnimationRequestFor(ability, meleeWindowMs);

  switch (ability.targetingMode) {
    case 'ground_target': {
      const radius = ability.previewShape.kind === 'sphere'
        ? ability.previewShape.radius
        : ability.previewShape.kind === 'capsule'
          ? ability.previewShape.radius
          : 1.5;
      return {
        characterAnimation,
        playMeleeSwing: false,
        vfx: { kind: 'ground_torus', radius, color, pulseSeconds },
        lifetimeSeconds: Math.max(lingerSeconds, 1.0),
        meleeWindowMs,
      };
    }
    case 'self_only': {
      if (ability.previewShape.kind === 'sphere') {
        return {
          characterAnimation,
          playMeleeSwing: false,
          vfx: {
            kind: 'sphere_around',
            radius: ability.previewShape.radius,
            color,
            pulseSeconds,
          },
          lifetimeSeconds: Math.max(lingerSeconds, 0.6),
          meleeWindowMs,
        };
      }
      if (ability.previewShape.kind === 'capsule') {
        return {
          characterAnimation,
          playMeleeSwing: true,
          vfx: {
            kind: 'capsule_front',
            radius: ability.previewShape.radius,
            halfHeight: ability.previewShape.halfHeight,
            color,
          },
          lifetimeSeconds: Math.max(lingerSeconds, 0.3),
          meleeWindowMs,
        };
      }
      // Self / caster-offset with no preview shape: subtle burst, no swing.
      return {
        characterAnimation,
        playMeleeSwing: false,
        vfx: { kind: 'sphere_around', radius: 1.2, color, pulseSeconds: 0 },
        lifetimeSeconds: 0.6,
        meleeWindowMs,
      };
    }
    case 'caster_offset': {
      const radius = ability.previewShape.kind === 'sphere'
        ? ability.previewShape.radius
        : ability.previewShape.kind === 'capsule'
          ? ability.previewShape.radius
          : 1.5;
      return {
        characterAnimation,
        playMeleeSwing: false,
        vfx: { kind: 'ground_torus', radius, color, pulseSeconds },
        lifetimeSeconds: Math.max(lingerSeconds, 1.0),
        meleeWindowMs,
      };
    }
    case 'direction_target':
    case 'aim_assist':
    case 'raycast_strict':
    case 'entity_target': {
      if (ability.projectileSpeed !== undefined) {
        return {
          characterAnimation,
          playMeleeSwing: true,
          vfx: {
            kind: 'projectile',
            color,
            // projectile_speed in abilities.ron is units/tick → units/sec
            speed: Math.max(8, ability.projectileSpeed * 20),
          },
          lifetimeSeconds: 2.0,
          meleeWindowMs,
        };
      }
      if (ability.previewShape.kind === 'capsule') {
        return {
          characterAnimation,
          playMeleeSwing: true,
          vfx: {
            kind: 'capsule_front',
            radius: ability.previewShape.radius,
            halfHeight: ability.previewShape.halfHeight,
            color,
          },
          lifetimeSeconds: Math.max(lingerSeconds, 0.3),
          meleeWindowMs,
        };
      }
      if (ability.previewShape.kind === 'sphere') {
        return {
          characterAnimation,
          playMeleeSwing: true,
          vfx: {
            kind: 'sphere_around',
            radius: ability.previewShape.radius,
            color,
            pulseSeconds,
          },
          lifetimeSeconds: Math.max(lingerSeconds, 0.4),
          meleeWindowMs,
        };
      }
      return {
        characterAnimation,
        playMeleeSwing: true,
        vfx: { kind: 'none' },
        lifetimeSeconds: 0,
        meleeWindowMs,
      };
    }
    case 'lock_on':
    default:
      return {
        characterAnimation,
        playMeleeSwing: false,
        vfx: { kind: 'none' },
        lifetimeSeconds: 0,
        meleeWindowMs,
      };
  }
}
