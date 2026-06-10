import abilityMetadata from '@dive/client-contract/abilities.json' with { type: 'json' };

export const SIM_TICKS_PER_SECOND = abilityMetadata.tick_rate;
export const SECONDS_PER_TICK = 1 / SIM_TICKS_PER_SECOND;

export function ticksToSeconds(ticks: bigint): number {
  return Number(ticks) / SIM_TICKS_PER_SECOND;
}
