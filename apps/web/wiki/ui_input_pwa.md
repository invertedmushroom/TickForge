# UI, Input Profiles & PWA Direction

## Purpose & Role

This page records browser-client decisions that sit above raw movement physics:
MMO UI foundations, modality-specific settings, keyboard/gamepad controls, and
PWA behavior. The web client remains online-only; PWA features improve launch,
installed-app presentation, and testing ergonomics, not authoritative gameplay.

## UI Runtime

MMO UI is DOM/CSS over the three.js scene. Chat, settings, inventory, party UI,
diagnostics, buttons, and forms should stay as real DOM so text input, IME,
selection, copy/paste, accessibility, and responsive layout remain browser-native.

`src/ui/runtime.ts` is now the shell owner for browser UI layers. App startup
mounts a fixed layer stack for scene, HUD, touch controls, action bar, panels,
chat/text mode, and modals. New browser UI should attach to those roots instead
of adding independent absolute-positioned DOM beside the canvas.

`src/ui/lootHud.ts` is the first gameplay HUD consumer beyond diagnostics and
the action bar. It mounts on `gameplayHudRoot`, subscribes to `loot_pile`,
`loot_pile_item`, and `player_inventory`, renders eligible same-layer piles as
an in-world marker plus compact panel, and calls `claimLoot` with the first free
slot when the user claims an item. Success feedback comes from subscriptions:
inventory row insertion and pile/item deletion.

UI consistency is a contract, not only a test habit:

- Stable layout primitives use `min-width: 0`, bounded grid tracks, safe-area
  padding, and explicit overflow behavior.
- Buttons and tabs must fit their longest expected labels at 360px portrait.
  Narrow grids collapse before text clips.
- Touch-primary controls target at least 44px; fine-pointer desktop controls may
  be denser.
- Text entry always uses real `<input>` or `<textarea>` elements.

The runtime is also the source of truth for `inputMode`. Priority is
`blocked > modal > panel > text > gameplay`. Only `gameplay` allows movement
keys, ability hotkeys, or action button submits. Panels and modals focus
themselves when opened, restore prior focus when closed, and close from Escape
when they are the topmost closeable overlay.

Action buttons are browser-gesture guarded. The action bar prevents native
`contextmenu`, selection, drag, and WebKit touch-callout behavior, then captures
long press with its own timer and drag threshold. Long press is currently an
observable input gesture only; it is not mapped to a separate gameplay action.

Touch controls are also runtime-layered. `src/input/touchControls.ts` owns the
fixed stick primitive used by both the movement stick and the look stick.
Movement feeds vectors into the same Move/Stop path as keyboard input; look
feeds continuous yaw/pitch rotation into the orbit camera while held. Both clear
on pointer cancel, blur, UI mode changes, and destroy. Current profile
groundwork is `auto`, `mmo-stick`, `tank-single`, `tap-target`, and `hidden`;
only the stick profiles emit movement/look today.

Camera and tactical controls are scene-owned but follow the same input-mode
rules. `Space` jumps, `X` swaps weapon set, and held `Shift` reasserts block
with the current aim/camera direction. Block is currently keyboard-only on web;
a phone guard button or right-mouse binding should share the same hold-to-block
path once the final combat control layout is chosen.

Camera control is scene-owned but follows the same input-mode rules. The browser
uses a third-person orbit follow camera whose yaw makes keyboard and touch
movement camera-relative. Desktop `Alt` toggles pointer-lock capture for mouse
camera rotation; if the browser rejects the key-triggered request, the next
canvas click retries. `Esc`, `Alt`, blur, and non-gameplay UI modes release
capture. `KeyC` toggles a detached camera anchor: the camera stops following the
player until `KeyC` is pressed again, leaving room for future point-and-click or
look-around modes without treating detached camera as debug-only. On touch, the
left stick remains movement and a dedicated right-side look stick rotates the
camera continuously while held. The scene canvas no longer doubles as the
default touch camera surface, so tap/selection/interact/ground targeting can use
canvas touch without competing with camera rotation. Captured mouse and touch
look-stick modes aim from the center ray.

## Settings Split

Persist local preferences under the versioned key `dive.uiPreferences.v1`.
The implemented groundwork currently stores UI scale, density, action bar
scale, handedness, touch-control profile/stick/deadzone values, and chat
font/placement placeholders. The parser clamps unsafe numeric values, defaults
unknown enum values, tolerates invalid JSON, and migrates legacy flat chat and
touch placeholders into the v1 shape.

As settings grow, split them by active input profile rather than by viewport
alone:

| Profile | Example settings |
|---|---|
| `common` | language, audio, UI scale baseline, accessibility, chat font size |
| `desktop-kbm` | keybinds, mouse sensitivity, invert look, action-bar labels |
| `desktop-gamepad` | stick deadzones, axis inversion, sensitivity, trigger thresholds, button mapping |
| `touch-phone` | touch layout, joystick/action size, handedness, chat placement, safe-area behavior |
| `touch-tablet` | touch layout with larger default spacing and less compressed panels |
| `hybrid` | remembers each profile independently and switches by last meaningful modality |

Viewport rules keep layout safe. Capability detection and the user's last
meaningful input decide which control profile is active.

## Keyboard Skill Bindings

Keyboard bindings are exact physical key chords:
`KeyboardEvent.code + shift + ctrl + alt + meta`.

Current defaults use unmodified `Digit1` through `Digit4`. Modifier chords are
reserved for explicit user configuration later, for example alternate bars,
self-cast, focus-target cast, or editor shortcuts.

Default combat bindings should avoid `Ctrl`, `Alt`, and `Meta` because browsers
and operating systems reserve many of those combinations. In the current browser
defaults, `Shift` is reserved for hold-to-block, so future modifier chords need
explicit conflict detection and visible binding labels before they become
user-configurable.

Chat, text focus, settings, and modal panels disable gameplay hotkeys regardless
of binding.

## Gamepad

Gamepad support is a first-class input modality and does not require a special
PWA permission prompt.

Implementation rules:

- Listen for `gamepadconnected` and `gamepaddisconnected`.
- Poll `navigator.getGamepads()` in the input/render loop.
- Show a "press any controller button" activation hint because some browsers do
  not expose already-connected pads until the user interacts with them.
- Normalize mappings where possible, but keep remapping UI because browser and
  controller layouts vary.
- Apply deadzones and trigger thresholds before generating intents.
- Catch `SecurityError` from `getGamepads()` and surface it as an input
  capability problem.
- Reset held gamepad state on disconnect, blur, visibility hidden, text focus,
  and map transitions.

## Chat Keyboard

Normal chat does not use Keyboard Lock. Chat opens the native/mobile keyboard by
focusing a real editable element from a user action.

When chat is focused:

- Gameplay input is suspended.
- Movement sends Stop if needed.
- Escape, blur, or successful submit exits text mode.
- Orientation, visual viewport, and safe-area changes keep the composer visible.

Keyboard Lock is only a possible future fullscreen desktop-gameplay enhancement.
It must be ignored or released whenever chat, settings, or modal UI is active.

## Client Rapier

Client Rapier exists for player feel and valid local queries, not authority:

- The owned player collides locally with the same authored static map shapes, so
  wall contact and terrain response feel immediate instead of waiting on
  round-trip correction.
- Ground-target previews and outgoing ground-target intents can use the same
  local map collider data for raycasts, range clamping, and "is this ground"
  feedback before calling reducers.
- Pending movement replay can reconcile against authoritative transforms without
  pretending the browser owns combat, hit detection, NPCs, or persistent state.
- Generated heightfield colliders are converted to TriMesh in the browser and
  shared by render/debug geometry and Rapier static collision.
- The browser KCC mirrors the server's small grounded snap repair with a
  downward surface probe, which keeps authored dungeon floors and terrain tiles
  from intermittently behaving like sticky edges in the web client.

This is intentionally heavier than a pure transform-interpolation client. Keep
checking the cost. If Rapier becomes too expensive for low-end mobile, the
fallback should be to keep authoritative snapshots and simplify local prediction,
not to make browser physics authoritative.

## PWA

V1 PWA support is progressive enhancement:

- Add a web app manifest for installability, icons, start URL, display mode,
  theme color, and background color.
- Detect installed/standalone display mode so layout can account for safe areas
  and missing browser chrome.
- Feature-detect wake lock and fullscreen through `src/ui/capabilities.ts`.
  Both are opt-in runtime calls; neither is requested automatically at startup.
- Optional install prompt UI never blocks browser play.
- Optional Screen Wake Lock can be toggled during active gameplay and released
  on pause, disconnect, visibility hidden, or rejection.

There is no offline play. A testing-only service-worker/app-shell cache may be
useful behind an explicit development flag, but it must be easy to unregister,
cache by build/contract hash, and never queue gameplay reducers for later.

Deferred PWA features include production service-worker caching, push
notifications, notification permission, app badging, background sync, file
handling, share targets, protocol handlers, and store packaging.
