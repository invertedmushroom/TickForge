import abilityMetadata from '@dive/client-contract/abilities.json' with { type: 'json' };

export type TargetingModeKind =
  | 'direction_target'
  | 'entity_target'
  | 'ground_target'
  | 'raycast_strict'
  | 'aim_assist'
  | 'lock_on'
  | 'self_only'
  | 'caster_offset';

export type PreviewShape =
  | { kind: 'none' }
  | { kind: 'capsule'; radius: number; halfHeight: number }
  | { kind: 'sphere'; radius: number };

export type ChargeTier = {
  minTicks: number;
  damageMult: number;
};

export type AbilityOffset = {
  x: number;
  y: number;
  z: number;
};

export type AbilityCatalogEntry = {
  abilityId: number;
  name: string;
  targetingMode: TargetingModeKind;
  maxRange?: number;
  projectileSpeed?: number;
  cooldownTicks: number;
  lingerTicks: number;
  damageIntervalTicks: number;
  timelineDurationTicks: number;
  hitboxSpawnTick?: number;
  damageFrameTick?: number;
  hitboxRemoveTick?: number;
  chargeTiers: readonly ChargeTier[];
  chargeTierCount: number;
  previewShape: PreviewShape;
  offset: AbilityOffset;
  enabled: boolean;
  disabledReason?: string;
};

export type AbilitySlot = 1 | 2 | 3 | 4;

export type AbilitySlotBinding = {
  slot: AbilitySlot;
  abilityId: number;
  ability: AbilityCatalogEntry;
};

export const ENABLED_TARGETING_MODES: ReadonlySet<TargetingModeKind> = new Set([
  'direction_target',
  'aim_assist',
  'ground_target',
  'self_only',
  'caster_offset',
  'entity_target',
  'raycast_strict',
]);

export const ABILITY_TICK_RATE = abilityMetadata.tick_rate;
export const DEFAULT_MAX_ABILITY_RANGE = 30;

export const ABILITY_CATALOG: readonly AbilityCatalogEntry[] = abilityMetadata.abilities.map((ability) => {
  const targetingMode = ability.targeting_mode.kind;
  const enabled = ENABLED_TARGETING_MODES.has(targetingMode);
  return {
    abilityId: ability.ability_id,
    name: ability.name,
    targetingMode,
    maxRange: ability.max_range ?? undefined,
    projectileSpeed: ability.projectile_speed ?? undefined,
    cooldownTicks: ability.cooldown_ticks,
    lingerTicks: ability.linger_ticks,
    damageIntervalTicks: ability.damage_interval_ticks,
    timelineDurationTicks: ability.timeline_duration_ticks ?? ability.linger_ticks,
    hitboxSpawnTick: ability.hitbox_spawn_tick ?? undefined,
    damageFrameTick: ability.damage_frame_tick ?? undefined,
    hitboxRemoveTick: ability.hitbox_remove_tick ?? undefined,
    chargeTiers: ability.charge_tiers.map((tier) => ({
      minTicks: tier.min_ticks,
      damageMult: tier.damage_mult,
    })),
    chargeTierCount: ability.charge_tiers.length,
    previewShape: normalizePreviewShape(ability.preview_shape),
    offset: {
      x: ability.offset[0],
      y: ability.offset[1],
      z: ability.offset[2],
    },
    enabled,
    disabledReason: enabled ? undefined : `targeting mode ${targetingMode} is not enabled in the web client yet`,
  };
});

export const DEFAULT_ABILITY_SLOTS: readonly AbilitySlotBinding[] = [
  { slot: 1, abilityId: 1 },
  { slot: 2, abilityId: 2 },
  { slot: 3, abilityId: 24 },
  { slot: 4, abilityId: 23 },
].map((binding) => ({
  ...binding,
  ability: requireAbility(binding.abilityId),
})) as readonly AbilitySlotBinding[];

export function abilityById(abilityId: number): AbilityCatalogEntry | undefined {
  return ABILITY_CATALOG.find((ability) => ability.abilityId === abilityId);
}

export function abilityForSlot(slot: AbilitySlot): AbilityCatalogEntry {
  return DEFAULT_ABILITY_SLOTS.find((binding) => binding.slot === slot)!.ability;
}

export function targetingModeLabel(mode: TargetingModeKind): string {
  switch (mode) {
    case 'direction_target':
      return 'Direction';
    case 'aim_assist':
      return 'Aim Assist';
    case 'ground_target':
      return 'Ground';
    case 'self_only':
      return 'Self';
    case 'caster_offset':
      return 'Caster Offset';
    case 'entity_target':
      return 'Entity';
    case 'raycast_strict':
      return 'Raycast';
    case 'lock_on':
      return 'Lock-On';
  }
}

function requireAbility(abilityId: number): AbilityCatalogEntry {
  const ability = abilityById(abilityId);
  if (!ability) {
    throw new Error(`generated ability catalog is missing ability ${abilityId}`);
  }
  return ability;
}

function normalizePreviewShape(shape: (typeof abilityMetadata.abilities)[number]['preview_shape']): PreviewShape {
  switch (shape.kind) {
    case 'capsule':
      return { kind: 'capsule', radius: shape.radius, halfHeight: shape.half_height };
    case 'sphere':
      return { kind: 'sphere', radius: shape.radius };
    case 'none':
      return { kind: 'none' };
  }
}
