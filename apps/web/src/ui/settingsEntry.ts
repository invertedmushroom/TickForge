import { openSettingsPanel, SETTINGS_PANEL_ID, type SettingsPanelHandle } from './settingsPanel';
import type { UiRuntime } from './runtime';

const SETTINGS_KEY = 'KeyO';
const TEXT_ENTRY_TARGETS = ['textarea', '[contenteditable]', '[role="textbox"]'].join(',');
const TEXT_INPUT_TYPES = new Set([
  'text',
  'search',
  'email',
  'password',
  'tel',
  'url',
  'number',
]);

export type SettingsEntryOptions = {
  runtime: UiRuntime;
  /** Where to mount the floating settings button. Usually the UI shell root. */
  buttonHost: HTMLElement;
  /** Element whose diagnostics visibility is toggled from the panel. Optional. */
  diagnosticsElement?: HTMLElement | null;
};

export type SettingsEntryHandle = {
  open: () => SettingsPanelHandle;
  destroy: () => void;
};

/**
 * Mount the floating settings button and register the keyboard shortcut
 * that opens the settings panel. The shortcut is gated through the shared
 * focus gate so text inputs and modals do not trigger it.
 */
export function mountSettingsEntry(options: SettingsEntryOptions): SettingsEntryHandle {
  const { runtime, buttonHost } = options;
  let activePanelHandle: SettingsPanelHandle | null = null;

  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'ui-settings-button';
  button.dataset.testid = 'settings-button';
  button.setAttribute('aria-label', 'Open settings');
  button.title = 'Settings (O)';
  button.textContent = '\u2699';
  button.addEventListener('click', () => {
    toggleSettings();
  });
  buttonHost.append(button);

  function updateButtonState(isOpen: boolean) {
    if (isOpen) {
      button.classList.add('ui-settings-button--active');
      button.setAttribute('aria-label', 'Close settings');
    } else {
      button.classList.remove('ui-settings-button--active');
      button.setAttribute('aria-label', 'Open settings');
    }
  }

  function toggleSettings(): void {
    const isOpen = activePanelHandle?.isOpen() ?? runtime.overlayState().panels.includes(SETTINGS_PANEL_ID);
    if (isOpen) {
      if (activePanelHandle) {
        activePanelHandle.close();
        activePanelHandle = null;
      } else {
        const overlayState = runtime.overlayState();
        if (overlayState.panels[overlayState.panels.length - 1] === SETTINGS_PANEL_ID) {
          runtime.closeTopOverlay();
        }
      }
      updateButtonState(false);
    } else {
      openSettings();
    }
  }

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.code !== SETTINGS_KEY) {
      return;
    }
    if (event.repeat || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) {
      return;
    }
    const isSettingsOpen = activePanelHandle?.isOpen() ?? runtime.overlayState().panels.includes(SETTINGS_PANEL_ID);
    if (!isSettingsOpen && runtime.inputMode() !== 'gameplay') {
      return;
    }
    if (isTextEntryTarget(event.target) || isTextEntryTarget(document.activeElement)) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    toggleSettings();
  };

  window.addEventListener('keydown', onKeyDown, true);

  return {
    open: () => openSettings(),
    destroy() {
      window.removeEventListener('keydown', onKeyDown, true);
      button.remove();
    },
  };

  function openSettings(): SettingsPanelHandle {
    const handle = openSettingsPanel({
      runtime,
      diagnosticsElement: options.diagnosticsElement ?? null,
      onClose: () => {
        if (activePanelHandle === handle) {
          activePanelHandle = null;
          updateButtonState(false);
        }
      },
    });
    activePanelHandle = handle;
    updateButtonState(true);
    return handle;
  }
}

function isTextEntryTarget(target: EventTarget | Element | null): boolean {
  const element = target instanceof Element ? target : null;
  if (!element) {
    return false;
  }
  if (element instanceof HTMLInputElement) {
    return TEXT_INPUT_TYPES.has(element.type);
  }
  if (element instanceof HTMLElement && element.isContentEditable) {
    return true;
  }
  return element.closest(TEXT_ENTRY_TARGETS) !== null;
}
