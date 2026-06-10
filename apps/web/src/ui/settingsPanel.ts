import type { UiOverlayHandle, UiRuntime } from './runtime';
import {
  DEFAULT_UI_PREFERENCES,
  saveUiPreferences,
  type ChatPlacement,
  type UiDensity,
  type UiHandedness,
  type UiPreferencesV1,
} from './preferences';
import {
  TOUCH_CONTROL_PROFILES,
  type TouchControlProfileId,
} from '../input/touchPreferences';
import {
  createSelectRow,
  createSliderRow,
  createToggleRow,
  type FormRow,
} from './primitives/formControls';
import { createPanelForm } from './primitives/panelForm';

export const SETTINGS_PANEL_ID = 'settings-v1';

export type SettingsPanelOptions = {
  runtime: UiRuntime;
  /** When provided, the settings panel toggles diagnostics visibility on this element. */
  diagnosticsElement?: HTMLElement | null;
  /** Override storage for the persisted preference write. Tests use this. */
  storage?: Pick<Storage, 'getItem' | 'setItem'> | null;
  onClose?: () => void;
};

export type SettingsPanelHandle = UiOverlayHandle;

/**
 * Open (or refocus) the settings panel. Subsequent calls close any prior
 * settings panel so the entry point stays idempotent for shortcut presses
 * and button clicks.
 */
export function openSettingsPanel(options: SettingsPanelOptions): SettingsPanelHandle {
  const { runtime } = options;
  const existing = runtime.overlayState().panels.includes(SETTINGS_PANEL_ID);
  if (existing) {
    runtime.closeTopOverlay();
  }

  const initial = runtime.preferences();
  const rows: {
    uiScale: FormRow<number>;
    actionBarScale: FormRow<number>;
    density: FormRow<UiDensity>;
    handedness: FormRow<UiHandedness>;
    touchProfile: FormRow<TouchControlProfileId>;
    touchStickScale: FormRow<number>;
    touchDeadzone: FormRow<number>;
    chatFontScale: FormRow<number>;
    chatPlacement: FormRow<ChatPlacement>;
    diagnosticsVisible: FormRow<boolean>;
    wakeLock: FormRow<boolean>;
    fullscreen: FormRow<boolean>;
  } = {
    uiScale: createSliderRow({
      id: 'settings-uiScale',
      label: 'UI scale',
      min: 0.8,
      max: 1.4,
      step: 0.05,
      value: initial.uiScale,
      format: (value) => `${Math.round(value * 100)}%`,
      onInput: (value) => writePreferences({ uiScale: value }),
    }),
    actionBarScale: createSliderRow({
      id: 'settings-actionBarScale',
      label: 'Action bar scale',
      min: 0.75,
      max: 1.35,
      step: 0.05,
      value: initial.actionBarScale,
      format: (value) => `${Math.round(value * 100)}%`,
      onInput: (value) => writePreferences({ actionBarScale: value }),
    }),
    density: createSelectRow<UiDensity>({
      id: 'settings-density',
      label: 'Density',
      value: initial.density,
      options: [
        { value: 'comfortable', label: 'Comfortable' },
        { value: 'compact', label: 'Compact' },
      ],
      onChange: (value) => writePreferences({ density: value }),
    }),
    handedness: createSelectRow<UiHandedness>({
      id: 'settings-handedness',
      label: 'Handedness',
      value: initial.handedness,
      options: [
        { value: 'right', label: 'Right-handed' },
        { value: 'left', label: 'Left-handed' },
      ],
      onChange: (value) => writePreferences({ handedness: value }),
    }),
    touchProfile: createSelectRow<TouchControlProfileId>({
      id: 'settings-touchProfile',
      label: 'Touch profile',
      value: initial.touchControls.profile,
      options: TOUCH_CONTROL_PROFILES.map((profile) => ({
        value: profile,
        label: profileLabel(profile),
      })),
      onChange: (value) => writeTouch({ profile: value }),
    }),
    touchStickScale: createSliderRow({
      id: 'settings-touchStickScale',
      label: 'Stick scale',
      min: 0.75,
      max: 1.4,
      step: 0.05,
      value: initial.touchControls.stickScale,
      format: (value) => `${Math.round(value * 100)}%`,
      onInput: (value) => writeTouch({ stickScale: value }),
    }),
    touchDeadzone: createSliderRow({
      id: 'settings-touchDeadzone',
      label: 'Stick deadzone',
      min: 0.05,
      max: 0.35,
      step: 0.01,
      value: initial.touchControls.deadzone,
      format: (value) => value.toFixed(2),
      onInput: (value) => writeTouch({ deadzone: value }),
    }),
    chatFontScale: createSliderRow({
      id: 'settings-chatFontScale',
      label: 'Chat font scale',
      min: 0.85,
      max: 1.4,
      step: 0.05,
      value: initial.chat.fontScale,
      format: (value) => `${Math.round(value * 100)}%`,
      onInput: (value) => writeChat({ fontScale: value }),
    }),
    chatPlacement: createSelectRow<ChatPlacement>({
      id: 'settings-chatPlacement',
      label: 'Chat placement',
      value: initial.chat.placement,
      options: [
        { value: 'bottom-left', label: 'Bottom-left' },
        { value: 'bottom-right', label: 'Bottom-right' },
      ],
      onChange: (value) => writeChat({ placement: value }),
    }),
    diagnosticsVisible: createToggleRow({
      id: 'settings-diagnosticsVisible',
      label: 'Show diagnostics overlay',
      description: 'Session-only. Hides the on-screen telemetry panel for screenshots and streaming.',
      value: !isDiagnosticsHidden(options.diagnosticsElement),
      onChange: (value) => setDiagnosticsHidden(options.diagnosticsElement, !value),
    }),
    wakeLock: createToggleRow({
      id: 'settings-wakeLock',
      label: 'Prevent display sleep',
      description: 'Keeps the screen on while the game is running.',
      disabled: !runtime.capabilities().wakeLockSupported,
      value: runtime.capabilities().wakeLockRequested,
      onChange: (value) => void runtime.setWakeLock(value),
    }),
    fullscreen: createToggleRow({
      id: 'settings-fullscreen',
      label: 'Fullscreen mode',
      description: 'Hides the browser UI and status bar.',
      disabled: !runtime.capabilities().fullscreenSupported,
      value: runtime.capabilities().fullscreenRequested,
      onChange: (value) => void runtime.setFullscreen(value),
    }),
  };

  const isCoarsePointer =
    typeof globalThis.matchMedia === 'function' && globalThis.matchMedia('(pointer: coarse)').matches;
  const touchHint = isCoarsePointer
    ? 'Touch detected. On phones, leave the profile on Auto for the default stick.'
    : 'Force a touch profile here to preview phone controls on desktop.';

  const renderContent = createPanelForm([
    {
      title: 'Display',
      rows: [
        rows.uiScale,
        rows.actionBarScale,
        rows.density,
        rows.handedness,
      ],
    },
    {
      title: 'Touch controls',
      hint: touchHint,
      rows: [
        rows.touchProfile,
        rows.touchStickScale,
        rows.touchDeadzone,
      ],
    },
    {
      title: 'Chat',
      rows: [rows.chatFontScale, rows.chatPlacement],
    },
    {
      title: 'System',
      rows: [rows.wakeLock, rows.fullscreen, rows.diagnosticsVisible],
    },
  ]);

  let unsubscribe: (() => void) | null = null;
  let unsubscribeCapabilities: (() => void) | null = null;

  const handle = runtime.openPanel({
    id: SETTINGS_PANEL_ID,
    title: 'Settings',
    closeOnEscape: true,
    restoreFocus: true,
    hideCloseButton: true,
    onClose: () => {
      unsubscribe?.();
      unsubscribeCapabilities?.();
      options.onClose?.();
    },
    content: (body) => {
      renderContent(body);
      appendActions(body);
    },
  });

  unsubscribe = runtime.subscribePreferences((next) => {
    if (!handle.isOpen()) {
      return;
    }
    rows.uiScale.setValue(next.uiScale);
    rows.actionBarScale.setValue(next.actionBarScale);
    rows.density.setValue(next.density);
    rows.handedness.setValue(next.handedness);
    rows.touchProfile.setValue(next.touchControls.profile);
    rows.touchStickScale.setValue(next.touchControls.stickScale);
    rows.touchDeadzone.setValue(next.touchControls.deadzone);
    rows.chatFontScale.setValue(next.chat.fontScale);
    rows.chatPlacement.setValue(next.chat.placement);
  });

  unsubscribeCapabilities = runtime.subscribeCapabilities((next) => {
    if (!handle.isOpen()) {
      return;
    }
    rows.wakeLock.setValue(next.wakeLockRequested);
    rows.wakeLock.setDisabled?.(!next.wakeLockSupported);
    rows.fullscreen.setValue(next.fullscreenRequested);
    rows.fullscreen.setDisabled?.(!next.fullscreenSupported);
  });

  return handle;

  function writePreferences(patch: Partial<UiPreferencesV1>): void {
    const current = runtime.preferences();
    const next = runtime.setPreferences({ ...current, ...patch });
    persist(next);
  }

  function writeTouch(patch: Partial<UiPreferencesV1['touchControls']>): void {
    const current = runtime.preferences();
    const next = runtime.setPreferences({
      ...current,
      touchControls: { ...current.touchControls, ...patch },
    });
    persist(next);
  }

  function writeChat(patch: Partial<UiPreferencesV1['chat']>): void {
    const current = runtime.preferences();
    const next = runtime.setPreferences({
      ...current,
      chat: { ...current.chat, ...patch },
    });
    persist(next);
  }

  function persist(next: UiPreferencesV1): void {
    if (options.storage === null) {
      return;
    }
    saveUiPreferences(next, options.storage ?? undefined);
  }

  function appendActions(body: HTMLElement): void {
    const actions = document.createElement('div');
    actions.className = 'ui-form__actions';
    const reset = document.createElement('button');
    reset.type = 'button';
    reset.className = 'ui-form__reset';
    reset.dataset.testid = 'settings-reset';
    reset.textContent = 'Reset to defaults';
    reset.addEventListener('click', () => {
      const next = runtime.setPreferences(DEFAULT_UI_PREFERENCES);
      persist(next);
      setDiagnosticsHidden(options.diagnosticsElement, false);
      rows.diagnosticsVisible.setValue(true);
    });
    actions.append(reset);
    body.append(actions);
  }
}

function profileLabel(profile: TouchControlProfileId): string {
  switch (profile) {
    case 'auto':
      return 'Auto';
    case 'mmo-stick':
      return 'MMO virtual stick';
    case 'tank-single':
      return 'Tank single-stick';
    case 'tap-target':
      return 'Tap-to-target';
    case 'hidden':
      return 'Hidden';
    default:
      return profile;
  }
}

function isDiagnosticsHidden(element: HTMLElement | null | undefined): boolean {
  return element?.dataset.diagnosticsHidden === 'true';
}

function setDiagnosticsHidden(element: HTMLElement | null | undefined, hidden: boolean): void {
  if (!element) {
    return;
  }
  element.dataset.diagnosticsHidden = hidden ? 'true' : 'false';
}
