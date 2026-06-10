import type { AbilityCatalogEntry, TargetingModeKind } from '../abilities/catalog';
import type { CharacterOverlayAnimationRequest } from './characterModel';

export type CharacterAnimationProfile = {
  /** Clip names to try first. Exact match wins, substring match is accepted. */
  clipNames?: readonly string[];
  /** Fallback keyword search if custom clip names are unavailable. */
  fallbackKeywords?: readonly string[];
  /** Minimum readable overlay duration; derived timeline can extend this. */
  minDurationMs?: number;
};

export type AbilityVisualProfile = {
  color: number;
  characterAnimation?: CharacterAnimationProfile;
  vfxStyle?:
    | 'none'
    | 'melee_hitbox'
    | 'projectile'
    | 'caster_aura'
    | 'ground_hazard'
    | 'contact_burst'
    | 'teleport';
};

const DEFAULT_ATTACK_ANIMATION: CharacterAnimationProfile = {
  clipNames: ['Attack'],
  fallbackKeywords: ['attack'],
  minDurationMs: 350,
};

const PROJECTILE_CAST_ANIMATION: CharacterAnimationProfile = {
  clipNames: ['cast_projectile', 'projectile_cast', 'Attack'],
  fallbackKeywords: ['cast', 'attack'],
  minDurationMs: 450,
};

const TELEPORT_ANIMATION: CharacterAnimationProfile = {
  clipNames: ['teleport', 'blink', 'Attack'],
  fallbackKeywords: ['teleport', 'blink', 'attack'],
  minDurationMs: 450,
};

/**
 * Per-ability art profile. This table owns only visual/art choices: colors,
 * preferred animation clip names, and rough VFX style. Gameplay timing and
 * shape come from generated ability metadata derived from data/abilities.ron.
 */
const PROFILE_OVERRIDES: Record<number, Partial<AbilityVisualProfile>> = {
  1: { color: 0xffcc33, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  2: { color: 0xff6619, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  3: { color: 0x994dff, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  4: { color: 0xffe666, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  10: { color: 0x808080, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  11: { color: 0xcc1919, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  12: { color: 0x19cc19, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  13: { color: 0x996633, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  20: { color: 0x4db3ff, vfxStyle: 'contact_burst' },
  21: { color: 0xcc3380, characterAnimation: TELEPORT_ANIMATION, vfxStyle: 'teleport' },
  22: { color: 0x33e6e6, characterAnimation: TELEPORT_ANIMATION, vfxStyle: 'teleport' },
  23: { color: 0xff8000, vfxStyle: 'caster_aura' },
  24: { color: 0xe64d00, vfxStyle: 'ground_hazard' },
  25: { color: 0xff3300, vfxStyle: 'ground_hazard' },
  42: { color: 0xcc8033, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  50: { color: 0x99ccff, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  51: { color: 0xb366ff, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  52: { color: 0xff4d4d, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  60: { color: 0x80ffcc, vfxStyle: 'contact_burst' },
  61: { color: 0x4de6b3, vfxStyle: 'contact_burst' },
  62: { color: 0x33ccff, vfxStyle: 'contact_burst' },
  63: { color: 0x66ff99, vfxStyle: 'contact_burst' },
  70: { color: 0xb3b3ff, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  71: { color: 0x9999e6, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  72: { color: 0xe680b3, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  80: { color: 0xcc994d, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  81: { color: 0xffb333, characterAnimation: DEFAULT_ATTACK_ANIMATION, vfxStyle: 'melee_hitbox' },
  90: { color: 0x80e680, characterAnimation: TELEPORT_ANIMATION, vfxStyle: 'teleport' },
  99: { color: 0xffd94d, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  101: { color: 0xff9966, characterAnimation: PROJECTILE_CAST_ANIMATION, vfxStyle: 'projectile' },
  124: { color: 0xe64d00, vfxStyle: 'ground_hazard' },
  125: { color: 0xff3300, vfxStyle: 'ground_hazard' },
};

export function visualProfileFor(ability: AbilityCatalogEntry): AbilityVisualProfile {
  const base = defaultProfileFor(ability);
  const override = PROFILE_OVERRIDES[ability.abilityId] ?? {};
  return {
    ...base,
    ...override,
    characterAnimation: override.characterAnimation ?? base.characterAnimation,
  };
}

export function characterAnimationRequestFor(
  ability: AbilityCatalogEntry,
  durationMs: number,
): CharacterOverlayAnimationRequest | undefined {
  const profile = visualProfileFor(ability).characterAnimation;
  if (!profile) return undefined;
  return {
    clipNames: profile.clipNames,
    fallbackKeywords: profile.fallbackKeywords,
    durationMs: Math.max(durationMs, profile.minDurationMs ?? 0),
  };
}

function defaultProfileFor(ability: AbilityCatalogEntry): AbilityVisualProfile {
  return {
    color: defaultColorForTargeting(ability.targetingMode),
    characterAnimation: defaultAnimationFor(ability),
    vfxStyle: defaultVfxStyleFor(ability),
  };
}

function defaultAnimationFor(ability: AbilityCatalogEntry): CharacterAnimationProfile | undefined {
  if (ability.projectileSpeed !== undefined) return PROJECTILE_CAST_ANIMATION;
  if (ability.previewShape.kind === 'capsule') return DEFAULT_ATTACK_ANIMATION;
  if (ability.targetingMode === 'raycast_strict' || ability.targetingMode === 'entity_target') {
    return DEFAULT_ATTACK_ANIMATION;
  }
  return undefined;
}

function defaultVfxStyleFor(ability: AbilityCatalogEntry): AbilityVisualProfile['vfxStyle'] {
  if (ability.projectileSpeed !== undefined) return 'projectile';
  if (ability.targetingMode === 'ground_target' || ability.targetingMode === 'caster_offset') return 'ground_hazard';
  if (ability.targetingMode === 'self_only' && ability.previewShape.kind === 'sphere') return 'caster_aura';
  if (ability.previewShape.kind === 'capsule') return 'melee_hitbox';
  if (ability.previewShape.kind === 'sphere') return 'contact_burst';
  return 'none';
}

function defaultColorForTargeting(targeting: TargetingModeKind): number {
  switch (targeting) {
    case 'ground_target':
    case 'caster_offset':
      return 0xe64d00;
    case 'self_only':
      return 0xff8000;
    case 'aim_assist':
    case 'raycast_strict':
    case 'entity_target':
      return 0xffcc66;
    case 'lock_on':
      return 0x4db3ff;
    case 'direction_target':
    default:
      return 0xffcc33;
  }
}
