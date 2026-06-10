import type { InputFocusMode } from './focusGate';
import type { TouchControlPreferences, TouchControlProfileId } from './touchPreferences';

export type TouchMovementProfileId = Extract<TouchControlProfileId, 'mmo-stick' | 'tank-single'>;

export type TouchMovementVector = {
  x: number;
  z: number;
  magnitude: number;
  profile: TouchMovementProfileId;
};

export type TouchLookVector = {
  x: number;
  y: number;
  magnitude: number;
  profile: TouchMovementProfileId;
};

export type TouchMovementControlsOptions = {
  preferences: () => TouchControlPreferences;
  subscribePreferences?: (listener: (preferences: TouchControlPreferences) => void) => () => void;
  inputMode?: () => InputFocusMode;
  subscribeInputMode?: (listener: (mode: InputFocusMode) => void) => () => void;
  onMove: (vector: TouchMovementVector) => void;
  onStop: () => void;
};

export type TouchLookControlsOptions = {
  preferences: () => TouchControlPreferences;
  subscribePreferences?: (listener: (preferences: TouchControlPreferences) => void) => () => void;
  inputMode?: () => InputFocusMode;
  subscribeInputMode?: (listener: (mode: InputFocusMode) => void) => () => void;
  onLook: (vector: TouchLookVector) => void;
  onStop: () => void;
};

export type TouchStickControlsHandle = {
  cancel: () => void;
  destroy: () => void;
  refresh: () => void;
};

export type TouchMovementControlsHandle = TouchStickControlsHandle;
export type TouchLookControlsHandle = TouchStickControlsHandle;

type ActiveStickPointer = {
  pointerId: number;
  profile: TouchMovementProfileId;
};

type NormalizedTouchStickAxes = {
  x: number;
  y: number;
  magnitude: number;
};

type TouchStickControlsOptions<TVector> = {
  containerClassName: string;
  containerTestId: string;
  stickClassName: string;
  stickTestId: string;
  ariaLabel: string;
  preferences: () => TouchControlPreferences;
  subscribePreferences?: (listener: (preferences: TouchControlPreferences) => void) => () => void;
  inputMode?: () => InputFocusMode;
  subscribeInputMode?: (listener: (mode: InputFocusMode) => void) => () => void;
  toVector: (axes: NormalizedTouchStickAxes, profile: TouchMovementProfileId) => TVector;
  onVector: (vector: TVector) => void;
  onStop: () => void;
};

export function createTouchMovementControls(
  root: HTMLElement,
  options: TouchMovementControlsOptions,
): TouchMovementControlsHandle {
  return createTouchStickControls(root, {
    containerClassName: 'touch-controls touch-controls--movement',
    containerTestId: 'touch-controls',
    stickClassName: 'touch-stick touch-stick--movement',
    stickTestId: 'touch-stick',
    ariaLabel: 'Move',
    preferences: options.preferences,
    subscribePreferences: options.subscribePreferences,
    inputMode: options.inputMode,
    subscribeInputMode: options.subscribeInputMode,
    toVector: (axes, profile) => ({
      x: axes.x,
      z: axes.y,
      magnitude: axes.magnitude,
      profile,
    }),
    onVector: options.onMove,
    onStop: options.onStop,
  });
}

export function createTouchLookControls(
  root: HTMLElement,
  options: TouchLookControlsOptions,
): TouchLookControlsHandle {
  return createTouchStickControls(root, {
    containerClassName: 'touch-controls touch-controls--look',
    containerTestId: 'touch-look-controls',
    stickClassName: 'touch-stick touch-stick--look',
    stickTestId: 'touch-look-stick',
    ariaLabel: 'Look',
    preferences: options.preferences,
    subscribePreferences: options.subscribePreferences,
    inputMode: options.inputMode,
    subscribeInputMode: options.subscribeInputMode,
    toVector: (axes, profile) => ({
      x: axes.x,
      y: axes.y,
      magnitude: axes.magnitude,
      profile,
    }),
    onVector: options.onLook,
    onStop: options.onStop,
  });
}

function createTouchStickControls<TVector>(
  root: HTMLElement,
  options: TouchStickControlsOptions<TVector>,
): TouchStickControlsHandle {
  const container = document.createElement('div');
  container.className = options.containerClassName;
  container.dataset.testid = options.containerTestId;

  const stick = document.createElement('button');
  stick.type = 'button';
  stick.className = options.stickClassName;
  stick.dataset.testid = options.stickTestId;
  stick.setAttribute('aria-label', options.ariaLabel);
  stick.draggable = false;

  const base = document.createElement('span');
  base.className = 'touch-stick__base';
  const thumb = document.createElement('span');
  thumb.className = 'touch-stick__thumb';
  stick.append(base, thumb);
  container.append(stick);
  root.append(container);

  let activePointer: ActiveStickPointer | undefined;
  let hasMovement = false;
  let destroyed = false;

  const cancel = () => {
    if (activePointer === undefined && !hasMovement) {
      resetStickVisuals(thumb);
      return;
    }
    activePointer = undefined;
    resetStickVisuals(thumb);
    if (hasMovement) {
      hasMovement = false;
      options.onStop();
    }
  };

  function refresh(): void {
    const preferences = options.preferences();
    container.dataset.profile = preferences.profile;
    container.style.setProperty('--dive-touch-stick-scale', String(preferences.stickScale));
    if (!movementProfileFromPreferences(preferences.profile)) {
      cancel();
    }
  }

  const onPointerDown = (event: PointerEvent) => {
    if (activePointer) {
      return;
    }
    const profile = movementProfileFromPreferences(options.preferences().profile);
    if (!profile || (options.inputMode?.() ?? 'gameplay') !== 'gameplay') {
      return;
    }

    event.preventDefault();
    activePointer = {
      pointerId: event.pointerId,
      profile,
    };
    try {
      stick.setPointerCapture(event.pointerId);
    } catch {
      // Synthetic tests may not have an active native pointer capture.
    }
    applyPointer(event);
  };

  const onPointerMove = (event: PointerEvent) => {
    if (activePointer?.pointerId !== event.pointerId) {
      return;
    }
    event.preventDefault();
    applyPointer(event);
  };

  const onPointerUp = (event: PointerEvent) => {
    if (activePointer?.pointerId !== event.pointerId) {
      return;
    }
    event.preventDefault();
    cancel();
  };

  const onPointerCancel = (event: PointerEvent) => {
    if (activePointer?.pointerId === event.pointerId) {
      cancel();
    }
  };

  const preventNativeAction = (event: Event) => {
    event.preventDefault();
  };

  function applyPointer(event: PointerEvent): void {
    if (!activePointer) {
      return;
    }
    const rect = stick.getBoundingClientRect();
    const radius = Math.max(1, Math.min(rect.width, rect.height) * 0.42);
    const dx = event.clientX - (rect.left + rect.width / 2);
    const dy = event.clientY - (rect.top + rect.height / 2);
    const normalized = normalizeTouchStick(dx, dy, radius, options.preferences().deadzone, activePointer.profile);
    const thumbRadius = Math.min(rect.width, rect.height) * 0.27;
    thumb.style.transform = `translate(${normalized.x * thumbRadius}px, ${normalized.z * thumbRadius}px)`;

    if (normalized.magnitude === 0) {
      if (hasMovement) {
        hasMovement = false;
        options.onStop();
      }
      return;
    }

    hasMovement = true;
    options.onVector(options.toVector({
      x: normalized.x,
      y: normalized.z,
      magnitude: normalized.magnitude,
    }, activePointer.profile));
  }

  stick.addEventListener('pointerdown', onPointerDown);
  stick.addEventListener('pointermove', onPointerMove);
  stick.addEventListener('pointerup', onPointerUp);
  stick.addEventListener('pointercancel', onPointerCancel);
  stick.addEventListener('contextmenu', preventNativeAction);
  stick.addEventListener('selectstart', preventNativeAction);
  stick.addEventListener('dragstart', preventNativeAction);
  const unsubscribeInputMode = options.subscribeInputMode?.((mode) => {
    if (mode !== 'gameplay') {
      cancel();
    }
  });
  const unsubscribePreferences = options.subscribePreferences?.(() => refresh());
  refresh();

  return {
    cancel,
    destroy() {
      if (destroyed) {
        return;
      }
      destroyed = true;
      cancel();
      unsubscribeInputMode?.();
      unsubscribePreferences?.();
      stick.removeEventListener('pointerdown', onPointerDown);
      stick.removeEventListener('pointermove', onPointerMove);
      stick.removeEventListener('pointerup', onPointerUp);
      stick.removeEventListener('pointercancel', onPointerCancel);
      stick.removeEventListener('contextmenu', preventNativeAction);
      stick.removeEventListener('selectstart', preventNativeAction);
      stick.removeEventListener('dragstart', preventNativeAction);
      container.remove();
    },
    refresh,
  };
}

export function normalizeTouchStick(
  dx: number,
  dy: number,
  radius: number,
  deadzone: number,
  profile: TouchMovementProfileId,
): TouchMovementVector {
  const rawX = Number.isFinite(dx) ? dx / Math.max(1, radius) : 0;
  const rawZ = Number.isFinite(dy) ? dy / Math.max(1, radius) : 0;
  const clampedLength = Math.min(1, Math.hypot(rawX, rawZ));
  if (clampedLength <= deadzone) {
    return { x: 0, z: 0, magnitude: 0, profile };
  }

  const normalizedLength = (clampedLength - deadzone) / Math.max(0.001, 1 - deadzone);
  const unitX = rawX / Math.max(0.001, Math.hypot(rawX, rawZ));
  const unitZ = rawZ / Math.max(0.001, Math.hypot(rawX, rawZ));
  return {
    x: clampUnit(unitX * normalizedLength),
    z: clampUnit(unitZ * normalizedLength),
    magnitude: normalizedLength,
    profile,
  };
}

function movementProfileFromPreferences(profile: TouchControlProfileId): TouchMovementProfileId | undefined {
  switch (profile) {
    case 'auto':
    case 'mmo-stick':
      return 'mmo-stick';
    case 'tank-single':
      return 'tank-single';
    case 'tap-target':
    case 'hidden':
      return undefined;
  }
}

function resetStickVisuals(thumb: HTMLElement): void {
  thumb.style.transform = 'translate(0px, 0px)';
}

function clampUnit(value: number): number {
  return Math.max(-1, Math.min(1, value));
}
