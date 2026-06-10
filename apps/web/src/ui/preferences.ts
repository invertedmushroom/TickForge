import {
  DEFAULT_TOUCH_CONTROL_PREFERENCES,
  normalizeTouchControlPreferences,
  type TouchControlPreferences,
} from '../input/touchPreferences';

export const UI_PREFERENCES_STORAGE_KEY = 'dive.uiPreferences.v1';
export const UI_PREFERENCES_VERSION = 1;

export type UiDensity = 'comfortable' | 'compact';
export type UiHandedness = 'right' | 'left';
export type ChatPlacement = 'bottom-left' | 'bottom-right';

export type UiPreferencesV1 = {
  version: typeof UI_PREFERENCES_VERSION;
  uiScale: number;
  density: UiDensity;
  actionBarScale: number;
  handedness: UiHandedness;
  touchControls: TouchControlPreferences;
  chat: {
    fontScale: number;
    placement: ChatPlacement;
  };
};

export type UiPreferencesStorage = Pick<Storage, 'getItem' | 'setItem'>;

const DEFAULT_CHAT_PREFERENCES = Object.freeze({
  fontScale: 1,
  placement: 'bottom-left' as const,
});

export const DEFAULT_UI_PREFERENCES: Readonly<UiPreferencesV1> = Object.freeze({
  version: UI_PREFERENCES_VERSION,
  uiScale: 1,
  density: 'comfortable',
  actionBarScale: 1,
  handedness: 'right',
  touchControls: DEFAULT_TOUCH_CONTROL_PREFERENCES,
  chat: DEFAULT_CHAT_PREFERENCES,
});

const UI_SCALE_RANGE = { min: 0.8, max: 1.4 };
const ACTION_BAR_SCALE_RANGE = { min: 0.75, max: 1.35 };
const CHAT_FONT_SCALE_RANGE = { min: 0.85, max: 1.4 };

export function loadUiPreferences(storage: UiPreferencesStorage | undefined = browserStorage()): UiPreferencesV1 {
  if (!storage) {
    return clonePreferences(DEFAULT_UI_PREFERENCES);
  }
  return parseUiPreferencesJson(storage.getItem(UI_PREFERENCES_STORAGE_KEY));
}

export function saveUiPreferences(
  preferences: unknown,
  storage: UiPreferencesStorage | undefined = browserStorage(),
): UiPreferencesV1 {
  const normalized = normalizeUiPreferences(preferences);
  storage?.setItem(UI_PREFERENCES_STORAGE_KEY, JSON.stringify(normalized));
  return normalized;
}

export function parseUiPreferencesJson(raw: string | null | undefined): UiPreferencesV1 {
  if (!raw) {
    return clonePreferences(DEFAULT_UI_PREFERENCES);
  }

  try {
    return normalizeUiPreferences(JSON.parse(raw));
  } catch {
    return clonePreferences(DEFAULT_UI_PREFERENCES);
  }
}

export function normalizeUiPreferences(payload: unknown): UiPreferencesV1 {
  if (!isRecord(payload)) {
    return clonePreferences(DEFAULT_UI_PREFERENCES);
  }

  if (payload.version === UI_PREFERENCES_VERSION) {
    return normalizeV1Preferences(payload);
  }

  return migrateLegacyPreferences(payload);
}

function normalizeV1Preferences(payload: Record<string, unknown>): UiPreferencesV1 {
  const chat = isRecord(payload.chat) ? payload.chat : {};
  return {
    version: UI_PREFERENCES_VERSION,
    uiScale: numberInRange(payload.uiScale, UI_SCALE_RANGE, DEFAULT_UI_PREFERENCES.uiScale),
    density: enumValue(payload.density, ['comfortable', 'compact'], DEFAULT_UI_PREFERENCES.density),
    actionBarScale: numberInRange(
      payload.actionBarScale,
      ACTION_BAR_SCALE_RANGE,
      DEFAULT_UI_PREFERENCES.actionBarScale,
    ),
    handedness: enumValue(payload.handedness, ['right', 'left'], DEFAULT_UI_PREFERENCES.handedness),
    touchControls: normalizeTouchControlPreferences(payload.touchControls),
    chat: {
      fontScale: numberInRange(chat.fontScale, CHAT_FONT_SCALE_RANGE, DEFAULT_UI_PREFERENCES.chat.fontScale),
      placement: enumValue(chat.placement, ['bottom-left', 'bottom-right'], DEFAULT_UI_PREFERENCES.chat.placement),
    },
  };
}

function migrateLegacyPreferences(payload: Record<string, unknown>): UiPreferencesV1 {
  const chat = isRecord(payload.chat) ? payload.chat : {};
  return {
    version: UI_PREFERENCES_VERSION,
    uiScale: numberInRange(payload.uiScale, UI_SCALE_RANGE, DEFAULT_UI_PREFERENCES.uiScale),
    density: enumValue(payload.density, ['comfortable', 'compact'], DEFAULT_UI_PREFERENCES.density),
    actionBarScale: numberInRange(
      payload.actionBarScale,
      ACTION_BAR_SCALE_RANGE,
      DEFAULT_UI_PREFERENCES.actionBarScale,
    ),
    handedness: enumValue(payload.handedness, ['right', 'left'], DEFAULT_UI_PREFERENCES.handedness),
    touchControls: normalizeTouchControlPreferences(
      payload.touchControls ?? {
        profile: payload.touchProfile,
        stickScale: payload.touchStickScale,
        deadzone: payload.touchDeadzone,
      },
    ),
    chat: {
      fontScale: numberInRange(
        payload.chatFontScale ?? chat.fontScale,
        CHAT_FONT_SCALE_RANGE,
        DEFAULT_UI_PREFERENCES.chat.fontScale,
      ),
      placement: enumValue(
        payload.chatPlacement ?? chat.placement,
        ['bottom-left', 'bottom-right'],
        DEFAULT_UI_PREFERENCES.chat.placement,
      ),
    },
  };
}

function clonePreferences(preferences: Readonly<UiPreferencesV1>): UiPreferencesV1 {
  return {
    ...preferences,
    touchControls: { ...preferences.touchControls },
    chat: { ...preferences.chat },
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

function browserStorage(): UiPreferencesStorage | undefined {
  if (typeof window === 'undefined') {
    return undefined;
  }
  try {
    return window.localStorage;
  } catch {
    return undefined;
  }
}
