import {
  DEFAULT_ABILITY_SLOTS,
  targetingModeLabel,
  type AbilitySlot,
  type AbilitySlotBinding,
} from '../abilities/catalog';
import { deriveAbilityInteractionPolicy } from '../abilities/interactionPolicy';

export type ActionBarPointerHandlers = {
  pointerDown: (slot: AbilitySlot, event: PointerEvent) => void;
  pointerMove: (slot: AbilitySlot, event: PointerEvent) => void;
  pointerUp: (slot: AbilitySlot, event: PointerEvent) => void;
  pointerCancel: (slot: AbilitySlot, event: PointerEvent) => void;
  longPress?: (slot: AbilitySlot, event: PointerEvent) => void;
};

export type ActionBarSlotVisualState = {
  focused?: boolean;
  armed?: boolean;
  selected?: boolean;
  invalid?: boolean;
  pressed?: boolean;
  charging?: boolean;
  disabled?: boolean;
  cooldownFraction?: number;
  chargeFraction?: number;
  chargeLabel?: string;
  aimPadX?: number;
  aimPadY?: number;
};

export type ActionBarHandle = {
  update: (states: ReadonlyMap<AbilitySlot, ActionBarSlotVisualState>) => void;
  destroy: () => void;
};

type SlotElements = {
  button: HTMLButtonElement;
  cooldown: HTMLElement;
  charge: HTMLElement;
  aimPad: HTMLElement;
  status: HTMLElement;
};

type ActiveActionPointer = {
  slot: AbilitySlot;
  button: HTMLButtonElement;
  event: PointerEvent;
  startClientX: number;
  startClientY: number;
  timer: number;
  longPressFired: boolean;
};

const LONG_PRESS_MS = 480;
const LONG_PRESS_CANCEL_DISTANCE = 10;

export function createActionBar(
  root: HTMLElement,
  handlers: ActionBarPointerHandlers,
  bindings: readonly AbilitySlotBinding[] = DEFAULT_ABILITY_SLOTS,
): ActionBarHandle {
  const container = document.createElement('div');
  container.className = 'ability-bar';
  container.dataset.testid = 'ability-bar';
  const slotElements = new Map<AbilitySlot, SlotElements>();
  const cleanups: Array<() => void> = [];

  for (const binding of bindings) {
    const policy = deriveAbilityInteractionPolicy(binding.ability);
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'ability-slot';
    button.draggable = false;
    button.dataset.testid = `ability-slot-${binding.slot}`;
    button.dataset.slot = String(binding.slot);
    button.dataset.policy = policy.kind;
    button.dataset.defaultStatus = policy.label;
    button.title = `${binding.slot}: ${binding.ability.name} (${policy.label})`;
    button.setAttribute('aria-label', `${binding.ability.name}, ${targetingModeLabel(binding.ability.targetingMode)}`);

    const cooldown = document.createElement('span');
    cooldown.className = 'ability-slot__cooldown';
    const charge = document.createElement('span');
    charge.className = 'ability-slot__charge';
    const aimPad = document.createElement('span');
    aimPad.className = 'ability-slot__aim-pad';

    const top = document.createElement('span');
    top.className = 'ability-slot__top';
    const key = document.createElement('span');
    key.className = 'ability-slot__key';
    key.textContent = String(binding.slot);
    const status = document.createElement('span');
    status.className = 'ability-slot__status';
    status.textContent = policy.label;
    top.append(key, status);

    const name = document.createElement('span');
    name.className = 'ability-slot__name';
    name.textContent = binding.ability.name;
    const mode = document.createElement('span');
    mode.className = 'ability-slot__mode';
    mode.textContent = targetingModeLabel(binding.ability.targetingMode);

    button.append(cooldown, charge, aimPad, top, name, mode);
    button.classList.toggle('ability-slot--disabled', !binding.ability.enabled || policy.kind === 'disabled');
    container.append(button);
    slotElements.set(binding.slot, { button, cooldown, charge, aimPad, status });

    const activePointers = new Map<number, ActiveActionPointer>();

    const onPointerDown = (event: PointerEvent) => {
      event.preventDefault();
      try {
        button.setPointerCapture(event.pointerId);
      } catch {
        // Synthetic tests may not have an active native pointer capture.
      }
      if (shouldDetectLongPress(event)) {
        activePointers.set(event.pointerId, startLongPress(binding.slot, button, event, handlers));
      }
      handlers.pointerDown(binding.slot, event);
    };
    const onPointerMove = (event: PointerEvent) => {
      const pointer = activePointers.get(event.pointerId);
      if (pointer && longPressMovedTooFar(pointer, event)) {
        clearLongPress(activePointers, event.pointerId);
      }
      handlers.pointerMove(binding.slot, event);
    };
    const onPointerUp = (event: PointerEvent) => {
      event.preventDefault();
      clearLongPress(activePointers, event.pointerId);
      handlers.pointerUp(binding.slot, event);
    };
    const onPointerCancel = (event: PointerEvent) => {
      clearLongPress(activePointers, event.pointerId);
      handlers.pointerCancel(binding.slot, event);
    };
    const preventNativeAction = (event: Event) => {
      event.preventDefault();
    };

    button.addEventListener('pointerdown', onPointerDown);
    button.addEventListener('pointermove', onPointerMove);
    button.addEventListener('pointerup', onPointerUp);
    button.addEventListener('pointercancel', onPointerCancel);
    button.addEventListener('contextmenu', preventNativeAction);
    button.addEventListener('selectstart', preventNativeAction);
    button.addEventListener('dragstart', preventNativeAction);
    cleanups.push(() => {
      for (const pointerId of activePointers.keys()) {
        clearLongPress(activePointers, pointerId);
      }
      button.removeEventListener('pointerdown', onPointerDown);
      button.removeEventListener('pointermove', onPointerMove);
      button.removeEventListener('pointerup', onPointerUp);
      button.removeEventListener('pointercancel', onPointerCancel);
      button.removeEventListener('contextmenu', preventNativeAction);
      button.removeEventListener('selectstart', preventNativeAction);
      button.removeEventListener('dragstart', preventNativeAction);
    });
  }

  root.append(container);

  return {
    update(states) {
      for (const [slot, elements] of slotElements) {
        const state = states.get(slot) ?? {};
        const cooldownFraction = clamp01(state.cooldownFraction ?? 0);
        const chargeFraction = clamp01(state.chargeFraction ?? 0);
        const focused = state.focused === true || state.selected === true;
        elements.button.classList.toggle('ability-slot--selected', state.selected === true);
        elements.button.classList.toggle('ability-slot--focused', focused);
        elements.button.classList.toggle('ability-slot--armed', state.armed === true);
        elements.button.classList.toggle('ability-slot--invalid', state.invalid === true);
        elements.button.classList.toggle('ability-slot--pressed', state.pressed === true);
        elements.button.classList.toggle('ability-slot--charging', state.charging === true);
        elements.button.classList.toggle('ability-slot--disabled', state.disabled === true);
        elements.button.dataset.cooldown = cooldownFraction.toFixed(2);
        elements.button.dataset.charge = chargeFraction.toFixed(2);
        elements.cooldown.style.transform = `scaleY(${cooldownFraction})`;
        elements.charge.style.transform = `scaleX(${chargeFraction})`;
        elements.aimPad.style.transform = `translate(${state.aimPadX ?? 0}px, ${state.aimPadY ?? 0}px)`;
        elements.status.textContent = state.chargeLabel ?? elements.button.dataset.defaultStatus ?? '';
      }
    },
    destroy() {
      for (const cleanup of cleanups.splice(0)) {
        cleanup();
      }
      container.remove();
    },
  };
}

function clamp01(value: number): number {
  if (!Number.isFinite(value)) {
    return 0;
  }
  return Math.max(0, Math.min(1, value));
}

function shouldDetectLongPress(event: PointerEvent): boolean {
  return event.pointerType === 'touch' || event.pointerType === 'pen' || event.pointerType === '';
}

function startLongPress(
  slot: AbilitySlot,
  button: HTMLButtonElement,
  event: PointerEvent,
  handlers: ActionBarPointerHandlers,
): ActiveActionPointer {
  const pointer: ActiveActionPointer = {
    slot,
    button,
    event,
    startClientX: event.clientX,
    startClientY: event.clientY,
    timer: 0,
    longPressFired: false,
  };
  pointer.timer = window.setTimeout(() => {
    pointer.longPressFired = true;
    pointer.button.classList.add('ability-slot--long-pressing');
    pointer.button.dataset.longPress = 'true';
    handlers.longPress?.(pointer.slot, pointer.event);
  }, LONG_PRESS_MS);
  return pointer;
}

function clearLongPress(activePointers: Map<number, ActiveActionPointer>, pointerId: number): void {
  const pointer = activePointers.get(pointerId);
  if (!pointer) {
    return;
  }
  window.clearTimeout(pointer.timer);
  pointer.button.classList.remove('ability-slot--long-pressing');
  delete pointer.button.dataset.longPress;
  activePointers.delete(pointerId);
}

function longPressMovedTooFar(pointer: ActiveActionPointer, event: PointerEvent): boolean {
  return Math.hypot(event.clientX - pointer.startClientX, event.clientY - pointer.startClientY) > LONG_PRESS_CANCEL_DISTANCE;
}
