# Testing

## Purpose & Role

The web client test suite protects browser policy, generated contract
compatibility, visual readiness, input/reducer wrappers, loot UX, and local
KCC/static-map prediction. The suite is still focused, but it now covers the
browser runtime paths most likely to drift from server-authoritative gameplay.

## Commands

```powershell
npm run test
npm run test:ci
npm run test:e2e
```

`npm run test` runs the static policy check, production build, and the smoke
E2E test that verifies contract load and nonblank scene rendering.

`npm run test:ci` runs the full continuous integration gate: static policy,
production build, and the full Playwright test suite.

`npm run test:e2e` runs Playwright tests against the Vite dev server.

## Static Policy Check

`scripts/check-subscriptions.mjs` first verifies `@dive/client-contract` resolves to the workspace-local `../../target/web-contract` package with exported `abilities.json` and `physics-prediction.json`, then reads `@dive/client-contract/browser-policy.json` and scans `src/` for forbidden browser behavior:

- Raw entity-table subscriptions such as `SELECT * FROM entity_transform`.
- Direct generated DB accessors for forbidden tables.
- Privileged reducer names such as `registerWorker`, `commitTickResults`, and terrain upserts.
- Debug reducer access through `reducers.debug*`.
- `subscribeToAllTables()`.

This is the client-side guardrail that keeps generated bindings from becoming accidental authority leakage.

## Build Check

The build check runs:

```powershell
tsc --noEmit
vite build
```

TypeScript catches generated binding drift, JSON contract shape drift, and strict-mode errors. Vite verifies that the generated local package can be bundled for the browser.

The build also runs `scripts/check-build-budget.mjs`. Vite currently splits
Three.js and Rapier into vendor chunks, and the budget check keeps those chunks
and the main entry from regressing silently.

## Playwright Tests

Current Playwright coverage:

| Test | Boundary |
|---|---|
| Smoke scene boot | Contract import, startup bundle load, Rapier init, nonblank WebGL canvas |
| Content bundle tests | Startup layer and training dungeon bundle integrity |
| Input queue and interact tests | Monotonic sequencing, stale resend gate, ack pruning, ring capacity, reducer error classification, helper constructors for server-supported intent variants, `Interact` payloads, selected/hover/loot fallback |
| Scene input actions | Keyboard `Jump`, `WeaponSwap`, held `Block` repeat, and clearing held block when text input takes focus |
| Camera rig | Orbit clamp math, camera-relative movement rotation, `KeyC` detach/reattach, pointer-lock rejection fallback, touch look-stick yaw |
| Loot HUD | Loot subscriptions, same-layer eligibility filtering, range panel state, first-free-slot claim, reducer rejection feedback, responsive marker/panel layout |
| KCC and static physics | Generated physics constants, wall shortening, floor grounding, static-collider replay, and actual training-dungeon flat-floor progress |
| Targeting & selection | Hover/select state machine, screen-space target candidacy projection & sorting, click-to-place ground indicators, AOI prune on snapshot ticks |
| Ability intent | Ability targeted action creation (using `entity_target`, `raycast_strict`, and `ground_target` rules), casting error validation on missing target requirements |

The smoke test samples several WebGL pixels after forcing a render, rather than trusting a single center pixel. This makes the visual readiness check stable in headless Chromium.

## Rust-Side Contract Check

From the Rust workspace:

```powershell
cargo xtask dev web-contract --skip-schema --check
```

This regenerates the web contract into a staging directory and compares it against `target/web-contract`. It fails when the local artifact is stale.

## Acceptance Direction

As the browser client grows, add tests in this order:

1. Map bundle transitions for layer and instance changes.
2. Lock-on camera behavior and multi-target lock handling.
3. Rapier owned-player replay after authoritative correction in mixed
   remote/owned scenes.
4. Reconnect with persisted token.
5. End-to-end local SpacetimeDB smoke once the test harness can reliably launch `tickforge`.
