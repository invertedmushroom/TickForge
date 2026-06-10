# dive-web

Browser showcase client for TickForge. It now lives inside the Rust workspace
under `apps/web`, but remains a Node/Vite app, so browser tooling, Playwright,
Rapier WASM, and web rendering dependencies are kept in the separate `apps/web`
folder rather than being managed by Cargo.

The client is deliberately framework-light: TypeScript runtime code, manual DOM
reconciliation, vanilla CSS, Three.js scene rendering, and Rapier WASM for local
player feel and map queries. Server authority still lives in SpacetimeDB; the
browser subscribes, renders, and calls allowed reducers through the generated
`@dive/client-contract` package.

## Local Development

From the workspace root, refresh the generated contract package:

```powershell
cargo xtask dev web-contract --skip-schema
```

Then from `apps/web`:

```powershell
npm install
npm run dev
```

If you want to open the page from another device on the same network, the
client will now default to the web host for STDB transport instead of
`127.0.0.1`. If SpacetimeDB is running on a different machine or interface,
set:

```powershell
$env:VITE_STDB_URI = 'ws://<host-ip>:3000'
npm run dev
```

The local package dependency points at `../../target/web-contract`, the
workspace-local generated contract package refreshed by the Rust command above.

The workspace root also exposes thin web lanes:

```powershell
cargo xtask dev web
cargo xtask build web
cargo xtask test web
```

Those commands refresh `target/web-contract` first, then run the matching npm
script inside `apps/web`.

## Checks

```powershell
npm run test
npm run test:ci
npm run test:e2e
```

`npm run test` now runs the static subscription policy check, production build,
and the smoke E2E test for the rendered scene. `npm run test:ci` runs the
full CI gate including all Playwright tests.

The static subscription check verifies the repo-local contract package exposes
`abilities.json` and `physics-prediction.json`, then scans `src/` and fails if
browser code tries to subscribe to forbidden raw tables, use privileged reducers,
call debug reducers, or bypass the generated subscription policy.
