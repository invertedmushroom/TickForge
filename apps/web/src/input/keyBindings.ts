import type { AbilitySlot } from '../abilities/catalog';

export type KeyChord = {
  code: string;
  shift: boolean;
  ctrl: boolean;
  alt: boolean;
  meta: boolean;
};

export type AbilityKeyBinding = {
  slot: AbilitySlot;
  chord: KeyChord;
};

type KeyboardLike = Pick<KeyboardEvent, 'code' | 'shiftKey' | 'ctrlKey' | 'altKey' | 'metaKey'>;

export const DEFAULT_ABILITY_KEY_BINDINGS: readonly AbilityKeyBinding[] = Object.freeze([
  abilityBinding(1, 'Digit1'),
  abilityBinding(2, 'Digit2'),
  abilityBinding(3, 'Digit3'),
  abilityBinding(4, 'Digit4'),
]);

export function abilitySlotFromKeyboardEvent(
  event: KeyboardLike,
  bindings: readonly AbilityKeyBinding[] = DEFAULT_ABILITY_KEY_BINDINGS,
): AbilitySlot | undefined {
  const chord = keyChordFromEvent(event);
  return bindings.find((binding) => keyChordsEqual(binding.chord, chord))?.slot;
}

export function keyChordFromEvent(event: KeyboardLike): KeyChord {
  return {
    code: event.code,
    shift: event.shiftKey,
    ctrl: event.ctrlKey,
    alt: event.altKey,
    meta: event.metaKey,
  };
}

export function keyChordsEqual(left: KeyChord, right: KeyChord): boolean {
  return (
    left.code === right.code &&
    left.shift === right.shift &&
    left.ctrl === right.ctrl &&
    left.alt === right.alt &&
    left.meta === right.meta
  );
}

function abilityBinding(slot: AbilitySlot, code: string): AbilityKeyBinding {
  return {
    slot,
    chord: {
      code,
      shift: false,
      ctrl: false,
      alt: false,
      meta: false,
    },
  };
}
