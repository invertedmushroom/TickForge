export type InputFocusMode = 'gameplay' | 'text' | 'panel' | 'modal' | 'blocked';

type KeyboardFocusLike = Pick<KeyboardEvent, 'target'>;

const UI_KEYBOARD_TARGET_SELECTOR = 'input, textarea, select, button, [contenteditable], [role="textbox"]';

export function gameplayKeyboardInputAllowed(
  event: KeyboardFocusLike,
  mode: InputFocusMode = 'gameplay',
): boolean {
  if (mode !== 'gameplay') {
    return false;
  }
  return !isUiKeyboardTarget(event.target) && !isUiKeyboardTarget(globalThis.document?.activeElement ?? null);
}

export function isUiKeyboardTarget(target: EventTarget | null): boolean {
  const element = elementFromTarget(target);
  if (!element) {
    return false;
  }
  if (element instanceof HTMLElement && element.isContentEditable) {
    return true;
  }
  return element.closest(UI_KEYBOARD_TARGET_SELECTOR) !== null;
}

function elementFromTarget(target: EventTarget | null): Element | undefined {
  if (target instanceof Element) {
    return target;
  }
  if (target instanceof Node) {
    return target.parentElement ?? undefined;
  }
  return undefined;
}
