export const TOUCH_CONTROL_PROFILES = ['auto', 'mmo-stick', 'tank-single', 'tap-target', 'hidden'] as const;

export type TouchControlProfileId = (typeof TOUCH_CONTROL_PROFILES)[number];

export type TouchControlPreferences = {
  profile: TouchControlProfileId;
  stickScale: number;
  deadzone: number;
};

export const DEFAULT_TOUCH_CONTROL_PREFERENCES: Readonly<TouchControlPreferences> = Object.freeze({
  profile: 'auto',
  stickScale: 1,
  deadzone: 0.12,
});

const STICK_SCALE_RANGE = { min: 0.75, max: 1.4 };
const DEADZONE_RANGE = { min: 0.05, max: 0.35 };

export function normalizeTouchControlPreferences(payload: unknown): TouchControlPreferences {
  const record = isRecord(payload) ? payload : {};
  return {
    profile: enumValue(record.profile, TOUCH_CONTROL_PROFILES, DEFAULT_TOUCH_CONTROL_PREFERENCES.profile),
    stickScale: numberInRange(record.stickScale, STICK_SCALE_RANGE, DEFAULT_TOUCH_CONTROL_PREFERENCES.stickScale),
    deadzone: numberInRange(record.deadzone, DEADZONE_RANGE, DEFAULT_TOUCH_CONTROL_PREFERENCES.deadzone),
  };
}

function numberInRange(
  value: unknown,
  range: { min: number; max: number },
  fallback: number,
): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    return fallback;
  }
  return Math.max(range.min, Math.min(range.max, value));
}

function enumValue<T extends string>(value: unknown, allowed: readonly T[], fallback: T): T {
  return typeof value === 'string' && allowed.includes(value as T) ? (value as T) : fallback;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
