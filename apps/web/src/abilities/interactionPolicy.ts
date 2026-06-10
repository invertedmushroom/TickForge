import type { AbilityCatalogEntry } from './catalog';

export type AbilityInteractionKind = 'instant' | 'aim_release' | 'charge_release' | 'disabled';

export type AbilityInteractionPolicy = {
  kind: AbilityInteractionKind;
  label: string;
};

export function deriveAbilityInteractionPolicy(ability: AbilityCatalogEntry): AbilityInteractionPolicy {
  if (!ability.enabled) {
    return { kind: 'disabled', label: 'Unavailable' };
  }

  if (ability.chargeTiers.length > 0) {
    return { kind: 'charge_release', label: 'Hold' };
  }

  switch (ability.targetingMode) {
    case 'self_only':
    case 'caster_offset':
      return { kind: 'instant', label: 'Tap' };
    case 'direction_target':
    case 'aim_assist':
    case 'ground_target':
    case 'entity_target':
    case 'raycast_strict':
      return { kind: 'aim_release', label: 'Aim' };
    default:
      return { kind: 'disabled', label: 'Unavailable' };
  }
}
