import type { InputFocusMode } from '../input/focusGate';
import {
  loadUiPreferences,
  normalizeUiPreferences,
  type UiPreferencesV1,
} from './preferences';
import {
  createUiCapabilityManager,
  type UiCapabilityManager,
  type UiCapabilityState,
} from './capabilities';

export type UiOverlayKind = 'panel' | 'modal';

export type UiLayerRoots = {
  sceneRoot: HTMLDivElement;
  gameplayHudRoot: HTMLDivElement;
  touchControlsRoot: HTMLDivElement;
  actionBarRoot: HTMLDivElement;
  panelRoot: HTMLDivElement;
  modalRoot: HTMLDivElement;
  chatRoot: HTMLDivElement;
  mapLoadingRoot: HTMLDivElement;
};

export type UiOverlayContent = string | Node | ((body: HTMLElement) => void);

export type UiOverlayOptions = {
  id: string;
  title: string;
  content?: UiOverlayContent;
  closeOnEscape?: boolean;
  restoreFocus?: boolean;
  hideCloseButton?: boolean;
  onClose?: () => void;
};

export type UiOverlayHandle = {
  id: string;
  kind: UiOverlayKind;
  element: HTMLElement;
  close: () => void;
  isOpen: () => boolean;
};

export type UiOverlayState = {
  panels: string[];
  modals: string[];
};

export type UiRuntime = {
  layers: UiLayerRoots;
  inputMode: () => InputFocusMode;
  gameplayInputSuspended: () => boolean;
  preferences: () => UiPreferencesV1;
  setPreferences: (preferences: unknown) => UiPreferencesV1;
  subscribePreferences: (listener: (preferences: UiPreferencesV1) => void) => () => void;
  capabilities: () => UiCapabilityState;
  setWakeLock: (active: boolean) => Promise<UiCapabilityState>;
  setFullscreen: (active: boolean) => Promise<UiCapabilityState>;
  subscribeCapabilities: (listener: (state: UiCapabilityState) => void) => () => void;
  setTextMode: (active: boolean) => void;
  setBlocked: (active: boolean) => void;
  subscribeInputMode: (listener: (mode: InputFocusMode) => void) => () => void;
  overlayState: () => UiOverlayState;
  openPanel: (options: UiOverlayOptions) => UiOverlayHandle;
  openModal: (options: UiOverlayOptions) => UiOverlayHandle;
  closeTopOverlay: () => boolean;
  destroy: () => void;
};

export type UiRuntimeShell = UiLayerRoots & {
  shellRoot: HTMLElement;
  scenePanel: HTMLElement;
  uiRuntime: UiRuntime;
  contractStatus: HTMLDivElement;
  startupStatus: HTMLDivElement;
  connectionStatus: HTMLDivElement;
  inputStatus: HTMLDivElement;
  predictionStatus: HTMLDivElement;
  remoteStatus: HTMLDivElement;
  mapStatus: HTMLDivElement;
};

type OverlayEntry = {
  id: string;
  kind: UiOverlayKind;
  element: HTMLElement;
  root: HTMLElement;
  restoreFocus?: HTMLElement;
  closeOnEscape: boolean;
  onClose?: () => void;
};

const FOCUSABLE_SELECTOR = [
  'button:not([disabled])',
  '[href]',
  'input:not([disabled])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[tabindex]:not([tabindex="-1"])',
].join(',');

export function mountUiRuntimeShell(
  container: HTMLElement,
  options: { subscriptionCount: number; preferences?: unknown },
): UiRuntimeShell {
  container.innerHTML = `
    <main class="shell ui-shell" data-testid="ui-shell">
      <section class="scene-panel ui-layer-manager" aria-label="Dive web preview">
        <div class="ui-layer ui-layer--scene" data-ui-layer="scene">
          <div id="scene-root" class="scene-root"></div>
        </div>
        <div id="gameplay-hud-root" class="gameplay-hud ui-layer ui-layer--hud" data-ui-layer="hud"></div>
        <div id="touch-controls-root" class="touch-controls-layer ui-layer ui-layer--touch-controls" data-ui-layer="touch-controls"></div>
        <div id="action-hud-root" class="action-hud ui-layer ui-layer--action-bar" data-ui-layer="action-bar"></div>
        <div id="map-loading-root" class="map-loading-layer ui-layer ui-layer--map-loading" data-ui-layer="map-loading"></div>
        <div id="panel-root" class="ui-panel-layer" data-ui-layer="panel" hidden></div>
        <div id="modal-root" class="ui-modal-layer" data-ui-layer="modal" hidden></div>
        <div id="chat-root" class="ui-chat-layer" data-ui-layer="chat" hidden></div>
        <aside class="diagnostics" aria-live="polite">
          <div class="diagnostics__title">Dive Web</div>
          <div data-testid="contract-status" class="diagnostics__line">contract: loading</div>
          <div data-testid="startup-status" class="diagnostics__line">startup: initializing</div>
          <div data-testid="connection-status" class="diagnostics__line">stdb: idle</div>
          <div data-testid="input-status" class="diagnostics__line">input: idle</div>
          <div data-testid="prediction-status" class="diagnostics__line">prediction: idle</div>
          <div data-testid="remote-status" class="diagnostics__line">remote: idle</div>
          <div data-testid="map-status" class="diagnostics__line">map: loading</div>
          <div class="diagnostics__line">subscriptions: ${options.subscriptionCount}</div>
        </aside>
      </section>
    </main>
  `;

  const shellRoot = requiredChild<HTMLElement>(container, '[data-testid="ui-shell"]');
  const scenePanel = requiredChild<HTMLElement>(container, '.scene-panel');
  const layers: UiLayerRoots = {
    sceneRoot: requiredChild<HTMLDivElement>(container, '#scene-root'),
    gameplayHudRoot: requiredChild<HTMLDivElement>(container, '#gameplay-hud-root'),
    touchControlsRoot: requiredChild<HTMLDivElement>(container, '#touch-controls-root'),
    actionBarRoot: requiredChild<HTMLDivElement>(container, '#action-hud-root'),
    mapLoadingRoot: requiredChild<HTMLDivElement>(container, '#map-loading-root'),
    panelRoot: requiredChild<HTMLDivElement>(container, '#panel-root'),
    modalRoot: requiredChild<HTMLDivElement>(container, '#modal-root'),
    chatRoot: requiredChild<HTMLDivElement>(container, '#chat-root'),
  };
  const uiRuntime = createUiRuntime(shellRoot, layers, options.preferences ?? loadUiPreferences());

  return {
    shellRoot,
    scenePanel,
    uiRuntime,
    ...layers,
    contractStatus: requiredChild<HTMLDivElement>(container, '[data-testid="contract-status"]'),
    startupStatus: requiredChild<HTMLDivElement>(container, '[data-testid="startup-status"]'),
    connectionStatus: requiredChild<HTMLDivElement>(container, '[data-testid="connection-status"]'),
    inputStatus: requiredChild<HTMLDivElement>(container, '[data-testid="input-status"]'),
    predictionStatus: requiredChild<HTMLDivElement>(container, '[data-testid="prediction-status"]'),
    remoteStatus: requiredChild<HTMLDivElement>(container, '[data-testid="remote-status"]'),
    mapStatus: requiredChild<HTMLDivElement>(container, '[data-testid="map-status"]'),
  };
}

export function createUiRuntime(
  shellRoot: HTMLElement,
  layers: UiLayerRoots,
  initialPreferences: unknown = loadUiPreferences(),
): UiRuntime {
  let preferences = normalizeUiPreferences(initialPreferences);
  let textMode = false;
  let blocked = false;
  let destroyed = false;
  let currentMode: InputFocusMode = 'gameplay';
  const overlays: OverlayEntry[] = [];
  const inputModeListeners = new Set<(mode: InputFocusMode) => void>();
  const preferenceListeners = new Set<(preferences: UiPreferencesV1) => void>();
  const capabilityManager: UiCapabilityManager = createUiCapabilityManager(shellRoot);

  const syncInputMode = () => {
    const nextMode = computeInputMode(blocked, textMode, overlays);
    shellRoot.dataset.inputMode = nextMode;
    layers.chatRoot.hidden = !textMode;
    if (nextMode === currentMode) {
      return;
    }
    currentMode = nextMode;
    for (const listener of inputModeListeners) {
      listener(nextMode);
    }
  };

  const syncOverlayRoots = () => {
    const hasPanels = overlays.some((entry) => entry.kind === 'panel');
    const hasModals = overlays.some((entry) => entry.kind === 'modal');
    layers.panelRoot.hidden = !hasPanels;
    layers.modalRoot.hidden = !hasModals;
    layers.panelRoot.dataset.hasOverlay = hasPanels ? 'true' : 'false';
    layers.modalRoot.dataset.hasOverlay = hasModals ? 'true' : 'false';
  };

  const closeOverlay = (entry: OverlayEntry, restoreFocus = true) => {
    const index = overlays.indexOf(entry);
    if (index === -1) {
      return;
    }
    overlays.splice(index, 1);
    entry.element.remove();
    syncOverlayRoots();
    syncInputMode();
    if (restoreFocus && entry.restoreFocus?.isConnected) {
      entry.restoreFocus.focus({ preventScroll: true });
    }
    entry.onClose?.();
  };

  const openOverlay = (kind: UiOverlayKind, options: UiOverlayOptions): UiOverlayHandle => {
    const root = kind === 'modal' ? layers.modalRoot : layers.panelRoot;
    const element = renderOverlay(kind, options);
    const activeElement = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
    const entry: OverlayEntry = {
      id: options.id,
      kind,
      element,
      root,
      restoreFocus: options.restoreFocus === false ? undefined : activeElement,
      closeOnEscape: options.closeOnEscape ?? true,
      onClose: options.onClose,
    };

    const closeButton = element.querySelector<HTMLButtonElement>('.ui-overlay__close');
    closeButton?.addEventListener('click', () => closeOverlay(entry));

    root.append(element);
    overlays.push(entry);
    syncOverlayRoots();
    syncInputMode();
    focusOverlay(entry);

    return {
      id: entry.id,
      kind: entry.kind,
      element: entry.element,
      close: () => closeOverlay(entry),
      isOpen: () => overlays.includes(entry),
    };
  };

  const closeTopOverlay = () => {
    const entry = [...overlays].reverse().find((candidate) => candidate.closeOnEscape);
    if (!entry) {
      return false;
    }
    closeOverlay(entry);
    return true;
  };

  const onKeyDown = (event: KeyboardEvent) => {
    if (event.key !== 'Escape') {
      return;
    }
    if (closeTopOverlay()) {
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (textMode) {
      textMode = false;
      syncInputMode();
      event.preventDefault();
      event.stopPropagation();
    }
  };

  window.addEventListener('keydown', onKeyDown, true);
  applyUiPreferences(shellRoot, preferences);
  syncOverlayRoots();
  syncInputMode();

  return {
    layers,
    inputMode: () => currentMode,
    gameplayInputSuspended: () => currentMode !== 'gameplay',
    preferences: () => clonePreferences(preferences),
    setPreferences(nextPreferences) {
      preferences = normalizeUiPreferences(nextPreferences);
      applyUiPreferences(shellRoot, preferences);
      const snapshot = clonePreferences(preferences);
      for (const listener of preferenceListeners) {
        listener(snapshot);
      }
      return clonePreferences(preferences);
    },
    subscribePreferences(listener) {
      preferenceListeners.add(listener);
      return () => preferenceListeners.delete(listener);
    },
    capabilities: capabilityManager.state,
    setWakeLock: capabilityManager.setWakeLock,
    setFullscreen: capabilityManager.setFullscreen,
    subscribeCapabilities: capabilityManager.subscribe,
    setTextMode(active) {
      textMode = active;
      syncInputMode();
    },
    setBlocked(active) {
      blocked = active;
      syncInputMode();
    },
    subscribeInputMode(listener) {
      inputModeListeners.add(listener);
      return () => inputModeListeners.delete(listener);
    },
    overlayState() {
      return {
        panels: overlays.filter((entry) => entry.kind === 'panel').map((entry) => entry.id),
        modals: overlays.filter((entry) => entry.kind === 'modal').map((entry) => entry.id),
      };
    },
    openPanel: (options) => openOverlay('panel', options),
    openModal: (options) => openOverlay('modal', options),
    closeTopOverlay,
    destroy() {
      if (destroyed) {
        return;
      }
      destroyed = true;
      window.removeEventListener('keydown', onKeyDown, true);
      for (const entry of [...overlays].reverse()) {
        closeOverlay(entry, false);
      }
      capabilityManager.destroy();
      inputModeListeners.clear();
      preferenceListeners.clear();
    },
  };
}

function renderOverlay(kind: UiOverlayKind, options: UiOverlayOptions): HTMLElement {
  const element = document.createElement(kind === 'modal' ? 'section' : 'aside');
  element.className = `ui-overlay ui-overlay--${kind}`;
  element.dataset.testid = `ui-${kind}-${options.id}`;
  element.setAttribute('role', kind === 'modal' ? 'dialog' : 'region');
  element.setAttribute('aria-label', options.title);
  if (kind === 'modal') {
    element.setAttribute('aria-modal', 'true');
  }
  element.tabIndex = -1;

  const header = document.createElement('div');
  header.className = options.hideCloseButton
    ? 'ui-overlay__header ui-overlay__header--no-close'
    : 'ui-overlay__header';
  const title = document.createElement('h2');
  title.className = 'ui-overlay__title';
  title.textContent = options.title;
  header.append(title);

  if (!options.hideCloseButton) {
    const close = document.createElement('button');
    close.type = 'button';
    close.className = 'ui-overlay__close';
    close.setAttribute('aria-label', `Close ${options.title}`);
    close.title = 'Close';
    close.textContent = 'x';
    header.append(close);
  }

  const body = document.createElement('div');
  body.className = 'ui-overlay__body';
  appendOverlayContent(body, options.content);
  element.append(header, body);
  return element;
}

function appendOverlayContent(body: HTMLElement, content: UiOverlayContent | undefined): void {
  if (content === undefined) {
    return;
  }
  if (typeof content === 'string') {
    body.textContent = content;
    return;
  }
  if (content instanceof Node) {
    body.append(content);
    return;
  }
  content(body);
}

function focusOverlay(entry: OverlayEntry): void {
  const target = entry.element.querySelector<HTMLElement>(FOCUSABLE_SELECTOR) ?? entry.element;
  target.focus({ preventScroll: true });
}

function computeInputMode(
  blocked: boolean,
  textMode: boolean,
  overlays: readonly OverlayEntry[],
): InputFocusMode {
  if (blocked) {
    return 'blocked';
  }
  if (overlays.some((entry) => entry.kind === 'modal')) {
    return 'modal';
  }
  if (overlays.some((entry) => entry.kind === 'panel')) {
    return 'panel';
  }
  if (textMode) {
    return 'text';
  }
  return 'gameplay';
}

function applyUiPreferences(shellRoot: HTMLElement, preferences: UiPreferencesV1): void {
  shellRoot.style.setProperty('--dive-ui-scale', String(preferences.uiScale));
  shellRoot.style.setProperty('--dive-action-bar-scale', String(preferences.actionBarScale));
  shellRoot.style.setProperty('--dive-chat-font-scale', String(preferences.chat.fontScale));
  shellRoot.style.setProperty('--dive-touch-stick-scale', String(preferences.touchControls.stickScale));
  shellRoot.dataset.uiDensity = preferences.density;
  shellRoot.dataset.handedness = preferences.handedness;
  shellRoot.dataset.touchProfile = preferences.touchControls.profile;
  shellRoot.dataset.chatPlacement = preferences.chat.placement;
}

function clonePreferences(preferences: UiPreferencesV1): UiPreferencesV1 {
  return {
    ...preferences,
    chat: { ...preferences.chat },
  };
}

function requiredChild<T extends Element>(root: ParentNode, selector: string): T {
  const element = root.querySelector<T>(selector);
  if (!element) {
    throw new Error(`missing UI shell element ${selector}`);
  }
  return element;
}
