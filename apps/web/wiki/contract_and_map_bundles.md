# Contract & Map Bundles

## Purpose & Role

The web client consumes a generated package named `@dive/client-contract`. This package is produced by the Rust workspace with:

```powershell
cargo xtask dev web-contract --skip-schema
```

The package is an artifact, not hand-authored source. Its job is to keep browser code aligned with the authoritative SpacetimeDB module and the RON-authored content pipeline.

## Package Contents

| Artifact | Purpose |
|---|---|
| `bindings/` | Generated SpacetimeDB TypeScript bindings |
| `contract.json` | Contract version, schema hash, content hash, metadata hash, physics hash, and artifact layout |
| `browser-policy.json` | Allowed subscriptions/reducers and forbidden browser surfaces |
| `abilities.json` | Exported ability metadata used by the browser ability catalog |
| `physics-prediction.json` | Rapier prediction capsule, KCC, collision-mask, and tick metadata |
| `content-metadata.json` | Exported layer and dungeon content metadata |
| `content-lookup.json` | Stable lookup indexes for layers, dungeon templates, and terrain sets |
| `map-bundles/` | Local map-bundle fixtures with manifests and collider JSON |

The package lives at `target/web-contract` in the workspace root and is consumed from `apps/web/package.json` through `file:../../target/web-contract`.

## Ability Presentation Metadata

`abilities.json` is generated from `data/abilities.ron` by `xtask`. It is the
browser's presentation-safe view of ability authoring data. The file includes
targeting/cooldown basics plus timeline facts used by the web skill
presentation layer:

- `cooldown_ticks` — fallback cooldown duration for local optimistic UI.
- `timeline_duration_ticks` — total authored timeline length.
- `hitbox_spawn_tick` — first hitbox/projection spawn action, when present.
- `damage_frame_tick` — first damage/application frame, when present.
- `hitbox_remove_tick` — first removal frame, when present.
- `linger_ticks` — `hitbox_remove_tick - hitbox_spawn_tick` for the first hitbox.
- `damage_interval_ticks` — periodic pulse/damage cadence for lingering effects.
- `preview_shape` and `offset` — shape and local-space offset for presentation.
- `projectile_speed` and `max_range` — projectile presentation fallbacks.

These values are consumed by `src/abilities/catalog.ts` and then by
`src/render/skillVisuals.ts`. They are intentionally not gameplay authority:
damage, hit resolution, target validation, cooldown truth after CDR, and contact
outcomes remain server-owned and arrive through authoritative event rows.

## Browser Policy

`browser-policy.json` is the boundary between generated bindings and safe browser use. The raw generated binding surface includes every public table and reducer, including tables/reducers the browser must not use directly. The policy narrows that surface for client code.

The web app consumes this policy in two places:

- `src/contract.ts` exports the generated always-on subscription list and reducer policy.
- `scripts/check-subscriptions.mjs` scans browser source and fails on forbidden raw table access, privileged reducer usage, debug reducers, or `subscribeToAllTables()`.

This keeps the web client honest even though the generated package necessarily contains broader SDK bindings.

## Physics Prediction Metadata

`physics-prediction.json` is generated from the same Rust constants used by the
simulation worker for character capsules and KCC movement. The browser derives
its local Rapier capsule, KCC skin width, normal nudge, snap-to-ground,
autostep lengths, ground pull, gravity, movement collision masks, and tick rate
from this file. Browser KCC also uses this metadata when repairing small
grounded-floor false negatives on static dungeon/terrain colliders.

`contract.json` includes the file's deterministic `physics_hash`. A worker-side
physics tuning change therefore makes `cargo xtask dev web-contract --skip-schema
--check` fail until the browser contract package is regenerated.

## Map Bundle Format

Local bundle fixtures are generated under `map-bundles/`. Each bundle has:

```text
map-bundles/
  index.json
  index.ts
  layer-0-open-world/
    manifest.json
    colliders/static-colliders.json
```

The manifest records `bundle_version`, `content_hash`, source identity, render mesh references, collider JSON references, and debug markers. The collider JSON records a `format_version`, coordinate convention, source identity, and static colliders.

Current collider shapes:

| Shape | Browser Use |
|---|---|
| `cuboid` | Floors, walls, simple blocking volumes |
| `cylinder` | Columns and round blockers |
| `heightfield` | Authored terrain ramps and uneven ground; converted to TriMesh in the browser |
| `tri_mesh` | Future baked terrain and static mesh collision |

Heightfield samples follow Parry/Rapier column-major layout:
`heights[row + col * nrows]`. The browser conversion lives in
`src/world/heightfield.ts` and feeds both Three.js geometry and Rapier static
colliders so visual terrain and local collision remain aligned. The browser does
not call the Rapier JS heightfield constructor for generated map bundles.

## Validation Rules

The browser refuses incompatible map data:

- Manifest `content_hash` must match the expected generated contract content hash.
- Collider `content_hash` must match the same expected content hash.
- Collider `format_version` must match the browser-supported format version.
- Bundle ID in the index must match the bundle manifest.
- Physics prediction metadata hash must match `contract.json`.
- Heightfield and TriMesh payloads must be large enough to build triangles; when
  a malformed static shape slips through, browser physics installs a tiny fixed
  fallback collider instead of panicking during scene startup.

This gives the web client a local, deterministic map-loading path now, while preserving the future CDN shape.

## Authoring Workflow

1. Edit RON content in the Rust workspace.
2. Run `cargo xtask dev web-contract --skip-schema`.
3. Run `npm run test` from `apps/web`.
4. Run `npm run test:e2e` to verify generated bundles still render.

If content changes without regenerating the package, `cargo xtask dev web-contract --skip-schema --check` reports the workspace-local artifact as stale.
