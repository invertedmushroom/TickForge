# Runtime Connection & Input

## Purpose & Role

The runtime connection layer prepares the browser client for the same protocol used by native clients. It does not invent a browser transport. It uses the generated SpacetimeDB TypeScript bindings and keeps gameplay state driven by subscribed table/view updates.

The input layer is deliberately separate from rendering. This lets tests exercise sequencing, ack pruning, and resend behavior without a WebGL scene or a live database.

## Connection Flow

`src/stdb/connection.ts` owns the connection shell:

1. Resolve URI and module name from Vite env vars, defaulting to `ws://<page-hostname>:3000` and `tickforge`.
2. Load a persisted identity token from `localStorage`.
3. Build the generated `DbConnection`.
4. Save the token returned by `onConnect`.
5. Subscribe to the generated always-on AOI query set.
6. Call `spawnPlayer` if no owned `client_sequence` row is visible after subscriptions apply.
7. Publish a compact diagnostic snapshot for the overlay.

The snapshot tracks connection state, identity, entity ID, latest tick, next tick, layer, nearby transform count, startup readiness, and input queue counters.

## Startup Readiness

The web client treats gameplay input as ready only after these facts are visible:

| Readiness Fact | Source |
|---|---|
| Owned sequence row | `client_sequence` |
| Module clock/config | `module_config` |
| Latest observed tick | `sim_tick` |
| Owned transform | `nearby_transforms` |

This mirrors the authoritative ownership boundary: the browser can only drive intents after the server has created ownership state and AOI has exposed the owned entity.

## Intent Queue

`src/net/inputQueue.ts` owns the pending intent ring. It follows the same policy as the native client:

| Constant | Value | Purpose |
|---|---:|---|
| `INTENT_RING_CAPACITY` | 12 | Recent unacked entries retained for resend |
| `RESEND_MIN_AGE_MS` | 75 | Avoids resending normal in-flight single calls |
| `RESEND_HZ` | 20 | Intended resend cadence |

Each local input allocates a strictly monotonic `sequenceId`, records the latest observed `sim_tick`, and remains pending until `client_sequence.last_processed_sequence` or a stale-sequence rejection proves the server is already past it.

Keyboard and touch movement feed the same queue. Held movement is resent from
the scene loop at the configured 20 Hz cadence, while release submits a single
`Stop` and clears local held state.

### Browser Runtime Details

- `src/stdb/connection.ts` wraps the generated `DbConnection` and maintains a local snapshot refreshed on every subscription application and a 100ms poll loop.
- `src/ui/runtime.ts` owns the DOM layer stack and publishes the current UI `inputMode` for scene input gating.
- Subscriptions are installed from `browser-policy.json` as `ALWAYS_ON_SUBSCRIPTIONS`.
- The client auto-spawns the player if no owned `client_sequence` row is visible after subscriptions apply.
- `submitIntent()` enqueues the user action locally and then calls the contract reducer. Reducer rejects are classified and may advance the ack cursor for stale-sequence failures.
- `resendPendingIntents()` runs on a timer, batches stale pending intents, and resends them while preserving sequence ordering.
- Input is gated by `inputAllowed(snapshot, options)`, so gameplay input is submitted only when the connection is ready, the owned entity is alive, and the UI runtime is in `gameplay` mode.
- Pending Stop behavior is intentional but narrow: if UI mode leaves gameplay while movement keys are held and the client is otherwise reducer-ready, the scene sends one `Stop` and clears local held input. If readiness is lost, the client stops sending new commands until readiness returns.
- `Space` submits a single `Jump` intent. `KeyX` submits `WeaponSwap`.
- Holding `ShiftLeft` or `ShiftRight` submits `Block` at the same 20 Hz cadence
  as movement, using the current aim/camera direction as `look_dir`. Releasing
  Shift, focusing text, leaving gameplay UI mode, or losing window focus stops
  block submission; there is no explicit unblock intent.
- `KeyE` uses `resolveInteractTarget()`: selected entity first, then hovered
  entity, then nearest eligible in-range loot pile with an open inventory slot.
  Entity interactions submit `IntentAction::Interact` through the intent queue.
  Loot claims call the existing `claimLoot` reducer directly, because loot
  claiming is not an intent action.

### What This Client Does Not Do

- It does not evaluate game reducers locally, nor does it simulate the authoritative game world.
- It does not subscribe to raw forbidden tables outside the browser policy.
- It does not simulate remote entity physics; remote characters are displayed via transform interpolation and short-term extrapolation.

## Reducer Error Policy

Reducer errors are classified by prefix:

| Prefix | Client Classification |
|---|---|
| `Stale sequence` | Implicit ack for that sequence |
| `Intent queue full` | Backpressure; keep pending for later resend |
| `Intent batch too large` | Client bug or bad clamp |
| `Client not registered` | Spawn/re-resolve ownership path |
| `Client does not own this entity` | Re-resolve `client_sequence.entity_id` |
| `Instance not found/full/closed` | Surface instance state to UI |
| Anything else | Count as `other` and avoid blind retry loops |

Only stale sequence advances the local ack cursor. Batch success is not an ack; the authoritative cursor remains `client_sequence.last_processed_sequence`.

## Current Scope

The current implementation wires keyboard/touch input through the intent queue
and drives local owned-player motion in Rapier while reconciling against
authoritative transform snapshots. It can connect, subscribe, spawn,
submit/resend typed intents, display diagnostics, claim visible loot, resolve
basic interactions, replay pending movement against static map colliders, rotate
movement by browser camera yaw, submit jump/block/weapon-swap intents, and
repair small dungeon-floor grounding misses in browser KCC. Desktop `Alt`
controls pointer-lock camera capture, touch uses a dedicated right-side look
stick, and `KeyC` detaches/reattaches the follow anchor. The next layer is to
expand mixed remote/owned coverage, phone guard-button ergonomics, tap-target
skill policies, and lock-on handling without making browser physics
authoritative.

Client Rapier is intentionally a feel/query layer:

- local wall and terrain collision makes owned-player movement responsive before
  authoritative correction arrives;
- local static-map raycasts support ground-target previews and help avoid sending
  obviously invalid ground-target reducer calls;
- pending-input replay uses the same local static colliders to estimate where
  the owned body should be between authoritative snapshots.
- heightfield map-bundle colliders are converted to TriMesh in the browser so
  authored dungeon terrain works consistently in Three.js and Rapier JS.

It is not used for browser authority, remote-player simulation, combat hit
detection, NPC simulation, or persistent world state. If the cost becomes too
high on low-end mobile, simplify prediction before expanding browser physics.
