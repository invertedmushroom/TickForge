# TickForge — Action MMO Server

Rust workspace for an action MMO game server built on SpacetimeDB with Rapier3D physics.

## Crate Layout

| Crate | Purpose |
|-------|---------|
| `game_schema` | Shared types (wire formats, enums) — derives SpacetimeType + serde |
| `game_protocol` | Protocol types (EntityId, TickId, events, intents) |
| `game_core` | Simulation logic (SimState, ECS stores, combat, buffs, AI, dungeons, items, encounters) |
| `simulation_worker` | 10-phase tick pipeline, Rapier physics, coordinator |
| `server_module` | SpacetimeDB module (34 tables, 4 views, reducers) — compiles to WASM |
| `game_client` | SDK test client (AOI tests, smoke/fault/denial validation) — feature-gated `connected` |
| `game_client_bevy` | Bevy 0.15 3D client (WASD movement, ability bar, inventory, dungeon instances) |

## App Layout

| App | Purpose |
|-----|---------|
| `apps/web` | Browser showcase client (Vite, TypeScript, three.js, Rapier WASM, SpacetimeDB TS SDK) |

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (edition 2024)
- WASM target: `rustup target add wasm32-unknown-unknown`
- [SpacetimeDB CLI](https://spacetimedb.com/install) v2.4+, with `spacetime` available on `PATH`

## Build & Test (Offline — No Server)

All core logic and physics tests run without SpacetimeDB:

```sh
# Build all crates except server_module
cargo build --workspace --exclude server_module

# Run all unit tests across game_core and simulation_worker
cargo test --workspace --exclude server_module
```

These validate physics integration, entity lifecycle, combat resolution, and the tick pipeline without requiring SpacetimeDB.

The test suite (350+ tests) covers:
- **Physics**: gravity, kinematic movement, raycasting, intersection queries, entity removal
- **Entity lifecycle**: spawn, activate, despawn, remove, collision layer bit uniqueness
- **Combat**: hitbox lifecycle, damage, death, cover mechanics, combo routing, projectile pierce
- **CC system**: stun, knockback, knockdown, pull, sleep, silence, fear, diminishing returns
- **CC counterplay**: cleanse, stunbreak, CC immunity
- **Abilities**: weapon swap, charge tiers, lock-on/teleport targeting, caster-attached hazard zones
- **Movement**: jump, gravity-while-rooted, timed roots, stance/block root overlap
- **AI**: NPC threat tables, AI state transitions, AI overrides, director spawning
- **Layer isolation**: cross-layer damage rejection, same-layer damage, layer change mid-combat
- **Target filtering**: Hostile/Friendly/All filters, team-based targeting, team change mid-combat
- **Lag compensation**: transform history rewind, compensated hit resolution
- **Region**: cell transitions, hysteresis, layer preservation
- **Dungeons**: encounter pipeline, instance lifecycle
- **Determinism**: replay fixture corpus (byte-identical tick output)
- **Full pipeline integration**: hitbox → damage → death → despawn → cooldown

### Build the WASM server module

```sh
cargo build -p server_module --target wasm32-unknown-unknown --release
```

Output: `target/wasm32-unknown-unknown/release/server_module.wasm`

## SpacetimeDB Setup

### Quick Start — VS Code Tasks (recommended)

Each process gets its own terminal tab inside VS Code:

1. Press `Ctrl+Shift+B` (default build task) → runs **Dev Deploy + Run**
2. Three terminals appear:
   - **SpacetimeDB Server** — the local database
   - **Deploy & Register** — publishes module, generates bindings, builds, registers worker
   - **Simulation Worker** — processes ticks

For a clean deploy (reset all state), open the Command Palette (`Ctrl+Shift+P`), run **Tasks: Run Task**, and select **Clean Deploy + Run**.

### Quick Start — `cargo xtask` command matrix

This is the recommended CLI path for local development. The manual SpacetimeDB flow remains below for reference.

```sh
# Terminal 1 (server)
cargo xtask dev server

# Terminal 2 (worker)
cargo xtask dev schema
cargo xtask dev worker-register
cargo xtask dev worker

# Terminal 3 (Bevy client)
cargo xtask dev client

# Terminal 3 alt (two Bevy clients for demo/testing)
cargo xtask dev clients

# Terminal 4 (CLI test client; requires running server + registered worker)
cargo xtask test cli
```

# Terminal 5 (browser client)

```bash
cargo xtask dev web-contract --skip-schema
# Then from `apps/web`:
npm install
npm run dev
# or from project root
cargo xtask dev web
```

Useful one-shot commands:

```sh
# Full local reset (database + tokens)
cargo xtask dev reset --all

# Same as above; no flags defaults to resetting both
cargo xtask dev reset

# Reset only the deployed database/module state
cargo xtask dev reset --db

# Reset only local auth tokens
cargo xtask dev reset --tokens

# Build lanes
cargo xtask build wasm
cargo xtask build worker
cargo xtask build client         # Bevy client (game_client_bevy)
cargo xtask build cli            # headless SDK client (game_client)
cargo xtask build web            # browser client (apps/web)
cargo xtask build all            # wasm + worker + Bevy client + CLI client
cargo xtask build all --release

# Test lanes
cargo xtask test fast            # game_core + simulation_worker unit tests
cargo xtask test worker          # simulation_worker with connected feature
cargo xtask test worker --release
cargo xtask test workspace       # all crates except server_module
cargo xtask test cli             # single-client smoke test; requires running server + worker
cargo xtask test multi-client    # multi-client integration tests; requires running server + worker
cargo xtask test web             # browser client CI lane
cargo xtask test replay          # deterministic replay fixtures
cargo xtask test replay --release


# Fixture capture
cargo xtask dev capture-fixture
cargo xtask dev capture-fixture --out "path/to/output.generated.json"

# Terrain smoke (§4.8b) — push a synthetic flat chunk via admin reducers
# Bind a layer to --set-name in data/layers.ron or a DungeonTemplate,
# then restart the worker and look for "TerrainState: applied N insert(s)"
cargo xtask dev seed-terrain
cargo xtask dev seed-terrain --set-name smoke_floor --set-id 1 --half-extent 10 --elevation 0
```

Useful flags:

```sh
cargo xtask dev worker --log debug
cargo xtask dev worker-register --seed-npc
cargo xtask dev clients --token-a .client_token_left --token-b .client_token_right
```


To run Bevy clients manually in separate terminals with distinct persisted identities, set `STDB_TOKEN_FILE` per process:

```sh
STDB_TOKEN_FILE=.client_token_a cargo xtask dev client
STDB_TOKEN_FILE=.client_token_b cargo xtask dev client
```

> **Note:** `cargo xtask dev server` has no `--release` flag — it starts the installed `spacetime` CLI binary, not a workspace binary.

### glTF Terrain Import

Real terrain meshes (`.gltf` / `.glb`) are imported into SpacetimeDB by a pure-Rust xtask command. The importer walks all default-scene roots, applies node transforms, optionally applies a uniform scale + offset, chunks triangles on an XZ grid, welds duplicate vertices per chunk, and pushes one `terrain_chunk` row per non-empty cell via the admin reducers (`terrain_set_upsert` / `terrain_chunk_upsert` / `terrain_manifest_upsert`). Reducers are invoked over plain HTTP rather than `spacetime call` so large vertex arrays do not hit the Windows command-line length limit.

```sh
cargo xtask dev import-terrain --gltf path/to/level.glb --set-name level1 \
    [--set-id 1] [--chunk-size 32] [--scale 1.0] [--offset-x/y/z 0] [--flip-winding] [--skip-set]
```

`--set-id` is a fallback only — the importer queries the DB after `terrain_set_upsert` to resolve the auto-incremented `terrain_set_id` and uses that for `terrain_chunk_upsert`.

After import, bind a layer (`data/layers.ron`) or a `DungeonTemplate` to the same `--set-name` and restart the worker. The deferred terrain edit queue applies the new chunks at the next tick boundary; look for `TerrainState: applied N insert(s)` in the worker log.

**Asset convention for the client visual** (see `wiki/bevy_presentation_client.md` and `wiki/web_map_assets.md`):
```
crates/game_client_bevy/assets/terrain/{stem}/{stem}.gltf  ← visual mesh
crates/game_client_bevy/assets/terrain/{stem}/{stem}.bin   ← buffer (URI inside .gltf must match)
crates/game_client_bevy/assets/terrain/{stem}/textures/... ← PBR textures
```

By default `{stem}` is the same string as the layer's `terrain_set` (server) and the importer's `--set-name`. To decouple the client visual from server collision, set `client_visual: Some("...")` on the `WorldLayerDef` in `data/layers.ron` — the Bevy and Web clients will use that stem instead, while the worker keeps loading the `terrain_set` collision chunks. Useful when the artist-authored mesh is higher poly than the baked collision, or when several collision sets share one visual.

**Scale gotcha:** glTF assets exported from Unreal/FBX often carry a baked `0.01` cm→m conversion in their root node `matrix`. The importer applies node transforms before the `--scale` flag, so passing `--scale 0.01` on top double-scales the mesh. Inspect node `matrix`/`scale` first; only pass `--scale` when the source coordinates are still in raw centimetres.

Example for the bundled FAB modular terrain pack:
```sh
cargo xtask dev import-terrain \
    --gltf data/terrain/free_pack_modular_terrain_gltf/hills_terrain.gltf \
    --set-name hills_terrain --chunk-size 32
```

**Included examples:**
```sh
cargo xtask dev import-terrain --gltf crates/game_client_bevy/assets/terrain/hills_terrain/hills_terrain.gltf --set-name hills_terrain --chunk-size 32
cargo xtask dev import-terrain --gltf crates/game_client_bevy/assets/terrain/biome_terrain/biome_terrain.gltf --set-name biome_terrain --chunk-size 32
```

**Web**
```sh
npm --prefix apps/web install
cargo xtask dev web-contract --skip-schema
cargo xtask dev web
```

For production deployment of assets, configure `VITE_ASSET_BASE_URL` in the environment to point to your CDN (e.g. `VITE_ASSET_BASE_URL=https://cdn.example/assets/`). In development, Vite middleware automatically mounts generated assets under `/__dive_assets__/`.

Useful web checks:

```sh
cargo xtask build web
cargo xtask test web
```

PowerShell is now a thin compatibility shim that forwards to xtask:

```powershell
.\scripts\dev-deploy.ps1 -Clean -SetupOnly
.\scripts\dev-deploy.ps1 -Release
```

When the shim needs to start SpacetimeDB itself, it opens the server in a separate PowerShell window so the deploy flow can continue.

### Manual Steps (reference flow)

These are the underlying SpacetimeDB CLI steps that `xtask` automates. Keep using them if you prefer the manual flow.

#### 1. Start SpacetimeDB (local)

```sh
spacetime start
```

Default: `http://localhost:3000`

### 2. Publish the module

```sh
spacetime publish tickforge -p crates/server_module -s local
```

This compiles and deploys the WASM module. The module's `init` reducer runs automatically, seeding tick 0 and starting the 20Hz tick scheduler.

### 3. Generate Rust bindings manually

```sh
spacetime generate --lang rust --out-dir crates/simulation_worker/src/module_bindings --module-path crates/server_module
spacetime generate --lang rust --out-dir crates/game_client/src/module_bindings --module-path crates/server_module
```

This regenerates the Rust bindings consumed by both connected clients. The generated `module_bindings/` directories are imported when building the worker or client with the `connected` feature.

If you want the automated version instead, `cargo xtask dev schema` builds the WASM module, republishes it, and regenerates both Rust binding directories in one step.

> **Note:** Re-run this command whenever `tables.rs` or `reducers.rs` change. Otherwise the generated bindings will be out of sync with the module schema.

### 4. Build the simulation worker with server connectivity

```sh
cargo build -p simulation_worker --features connected
```

The `connected` feature enables:
- `spacetimedb-sdk` dependency
- `coordinator.rs` module (WebSocket connection, subscription callbacks)
- `module_bindings/` import

### 5. Run the simulation worker

```sh
$env:RUST_LOG = "info"
cargo run -p simulation_worker --features connected
```

The coordinator will:
1. Connect to SpacetimeDB via WebSocket
2. Print its Identity in the logs
3. Subscribe to entity, transform, health, intent, buff, team, layer, instance, and equipment tables
4. On each tick update: gather intents → run pipeline → commit results

### 6. Register the worker as trusted (one-time, before first commit)

When the worker connects it prints its Identity in the logs. Register it as trusted before it can commit tick results:

```sh
spacetime call tickforge register_worker '{"__identity__":"0x<identity from worker logs>"}' -s local
```
The Identity must be JSON-encoded with the `__identity__` field and `0x` hex prefix.

Only trusted workers can call `commit_tick_results`. Without this step, every commit will be rejected.

## Running the Physics Demo (No Server)

A standalone demo that validates Rapier physics without SpacetimeDB:

```sh
$env:RUST_LOG = "info"
cargo run -p simulation_worker
```

Spawns a ball at y=10, simulates 200 ticks, verifies it falls and rests on the ground plane.

## Running the Bevy 3D Client

A visual client built with Bevy 0.15. Requires a running SpacetimeDB instance with the module deployed and a simulation worker connected.

```powershell
# Start everything first (Ctrl+Shift+B in VS Code, or manually):
.\scripts\dev-deploy.ps1 -Clean

# Launch the Bevy client:
cargo run -p game_client_bevy --features connected
```

Controls: WASD movement, 1/2/3/4 ability hotkeys, mouse look. Entity colors: green (local player), blue (other players), red (NPCs), purple (bosses).

Custom server:
```powershell
$env:STDB_URI = "http://localhost:3000"
$env:STDB_MODULE = "tickforge"
cargo run -p game_client_bevy --features connected
```

## Testing

### Automated Tests (Always Available)

```sh
cargo test --workspace --exclude server_module
```

These tests run entirely offline. No SpacetimeDB instance needed.

### Deterministic Replay Fixtures (Corpus Gate)

The replay fixture system captures a complete combat scenario — entities, intents, and expected per-tick outputs — into a self-contained JSON file, then replays it to verify the simulation produces **byte-identical** results every time.

**Why it exists:** The tick pipeline must be deterministic — given the same entities, intents, and ability definitions, `TickPipeline::run_tick()` must produce identical event sequences and state transitions. Replay fixtures guard this property across refactors, and double as a debugging tool for inspecting per-tick event traces.

**How it works:**

1. **Capture** — A fixture records initial entities, intents-per-tick, and `expected_ticks` (events + entity state updates). The capture test sets up a `TickPipeline`, runs intents through it, collects `TickResult` outputs, and serializes everything to JSON.
2. **Replay** — On every test run, each `.json` fixture in `tests/fixtures/` is loaded, the pipeline is run **twice** from the same inputs, and the test asserts:
   - Both runs produce identical `serde_json::to_vec()` output (byte-level determinism)
   - Events and entity state updates match the fixture's `expected_ticks`

Tests use `MockPhysics` (deterministic fake contacts — every hitbox overlaps every entity) so there's no floating-point nondeterminism from real physics.

**Debugging workflow:** To debug a skill or combat interaction, create a fixture with the exact entities and intents that reproduce the problem. Run it and inspect the per-tick JSON output — you get a complete trace of `CastStart` → `HitboxSpawned` → `DamageFrame` → `Damage` → `EntityDied` → `EntityDespawned` → `CooldownReady` (or whichever events your scenario produces). Since it's fully reproducible, you can iterate on the game logic and re-run until the trace matches expectations.

**Regenerating after intentional changes:** When you change behavior on purpose, re-run `cargo xtask dev capture-fixture` to produce a new `.generated.json`, verify the output looks correct, then rename it to replace the old fixture.

Replay fixtures live in `crates/simulation_worker/tests/fixtures/`.

Run the deterministic replay suite:

```sh
cargo xtask test replay
```

This runs:
- baseline determinism checks (two pipelines, same intents, identical output)
- combat/lifecycle determinism scenario (Slash → damage → death → despawn → cooldown)
- fixture corpus validation (`deterministic_replay_fixture_corpus_expected`) against all committed `*.json` fixtures (excluding `*.generated.json`)

Capture or regenerate a fixture JSON:

```powershell
cargo xtask dev capture-fixture
cargo xtask dev capture-fixture --out "crates/simulation_worker/tests/fixtures/my_case_v1.generated.json"
```

This runs the `#[ignore]` test `capture_combat_lifecycle_fixture_to_json` and writes a JSON fixture to disk.

### After Generating Bindings

The `connected` feature and coordinator are not covered by the automated tests — they require a live SpacetimeDB instance. Integration testing with the server is done via the Rust SDK client:

```powershell
# Full clean deploy first:
.\scripts\dev-deploy.ps1 -Clean

# Then run the end-to-end integration tests (needs server + worker running):
cargo run -p game_client --features connected -- --test
```

The integration tests cover: AOI view subscriptions (T1-T5), smoke tests — `spawn_player` → `submit_intent` (Move + UseAbility) → tick → commit → assert transforms/health/combat events (S1-S9), fault tests — stale sequence, ownership enforcement, double-spawn, cooldown, unauthorized commit, cursor safety, intent consumption (F1-F8), and denial-of-service guards (D1).

### CLI

```sh
# Spawn combat NPC pairs (pair_count, pair_spacing, no_chase)
spacetime call tickforge debug_spawn_combat 5000 60.0 false -s local

# Spawn a predefined scenario
spacetime call tickforge debug_spawn_scenario '"combat"' -s local
spacetime call tickforge debug_spawn_scenario '"stress"' -s local

# Spawn passive NPCs (count, spacing)
spacetime call tickforge debug_spawn_many 500 2.0 -s local

# Spawn entities
spacetime call tickforge debug_spawn_prop 0.0 10.0 0.0 -s local
spacetime call tickforge debug_spawn_boss 0.0 5.0 0.0 5000.0 -s local
# Spawn an encounter-configured boss near an existing entity on that entity's layer
spacetime call tickforge debug_spawn_encounter_boss 1 '"manayas_core_demo"' 0.0 0.0 8.0 5000.0 -s local

# Entity manipulation
spacetime call tickforge debug_set_hp 1 500.0 1000.0 -s local
spacetime call tickforge debug_teleport 1 100.0 5.0 50.0 -s local
spacetime call tickforge debug_remove_entity 1 -s local

# Layer and team assignment (dungeon isolation, friendly fire control)
spacetime call tickforge debug_set_layer 1 3 -s local
spacetime call tickforge debug_set_team 1 10 -s local

# Dungeon instances
spacetime call tickforge debug_create_instance 1 -s local
spacetime call tickforge debug_create_instance_for_template 1 '"test_dungeon_01"' 4 -s local
spacetime call tickforge debug_join_instance 1 1 -s local

# Buffs and items
spacetime call tickforge debug_apply_buff 1 1 1 100 -s local
spacetime call tickforge debug_grant_item 1 1 1 -s local
```

### SpacetimeDB Observability

```sh
# Ad-hoc table inspection
spacetime sql tickforge "SELECT * FROM entity" -s local

# Interactive SQL REPL
spacetime sql --interactive tickforge -s local

# Live reducer/module logs
spacetime logs tickforge -s local --follow
spacetime logs tickforge -s local --level warn
spacetime subscribe tickforge "SELECT * FROM sim_log" -s local

# Schema introspection
spacetime describe tickforge --json -s local

# Live subscription stream
spacetime subscribe tickforge "SELECT * FROM entity_transform" -s local

# Module management
spacetime delete tickforge -s local
spacetime publish tickforge -p crates/server_module -s local
```


```sh
spacetime subscribe tickforge "SELECT * FROM npc_config" -s local
spacetime subscribe tickforge "SELECT * FROM entity" -s local
spacetime subscribe tickforge "SELECT * FROM entity_layer" -s local
spacetime subscribe tickforge "SELECT * FROM entity_transform" -s local
spacetime subscribe tickforge "SELECT * FROM entity_health" -s local
spacetime subscribe tickforge "SELECT * FROM combat_event" -s local
spacetime subscribe tickforge "SELECT * FROM interactable_config" -s local
spacetime subscribe tickforge "SELECT * FROM loot_pile" -s local
spacetime subscribe tickforge "SELECT * FROM loot_pile_item" -s local
```


No local web dashboard in standalone mode. The web dashboard (metrics, table browser, query monitor) is only available on maincloud deployments.

### Live Integration Tests

All integration tests run via the Rust SDK client in `crates/game_client/src/smoke_test.rs`, gated behind `#[cfg(feature = "connected")]`. They require a running SpacetimeDB instance with the module deployed and a simulation worker connected.

Reward-loop smoke uses the same connected SDK identity:

```sh
RUST_LOG=info cargo run -p game_client --features connected -- --loot-smoke
cargo run -p game_client --features connected -- --claim-loot <loot_pile_id> <item_id> <target_slot>
```

Use the SDK client for `claim_loot`; calling it through `spacetime call` runs as the CLI/admin identity and is expected to fail client registration validation.

## Project Structure

```
tickforge/
├── Cargo.toml                          # Workspace root
├── apps/
│   └── web/                            # Browser client (Vite + TypeScript + three.js)
├── data/
│   ├── abilities.ron                   # Data-driven ability definitions
│   ├── buffs.ron                       # Buff/debuff definitions
│   ├── dungeons.ron                    # Dungeon templates (geometry, spawns)
│   ├── encounters.ron                  # Boss encounter phase scripts
│   ├── items.ron                       # Item definitions (equipment, consumables)
│   └── spawn_rules.ron                 # Director spawn rules (open-world + dungeons)
├── scripts/
│   ├── dev-deploy.ps1                  # PowerShell deploy shim (forwards to xtask)
│   └── compare_logs.py                 # Log diffing utility
├── docs/
│   ├── architecture.md                 # Architecture reference
│   ├── diagrams.md                     # Mermaid architecture diagrams
│   ├── skills.md                       # Skill/ability design notes
│   ├── world.md                        # World systems design
│   ├── adr/                            # Architecture decision records
│   ├── contracts/                      # Behavior contracts (ordering, trust, intents)
│   └── plan/                           # Roadmap
├── crates/
│   ├── game_schema/                    # Shared wire types
│   ├── game_protocol/                  # EntityId, TickId, events, intents
│   ├── game_core/
│   │   └── src/
│   │       ├── sim_state.rs            # SimState (PhysicsState, CombatState, StatusState, AiState, StatsStore)
│   │       ├── entity/                 # EntityStore, EntityIndex, lifecycle
│   │       ├── combat/                 # HitboxStore, ThreatTable, skills, status, tactical, loadout
│   │       ├── encounter/              # Boss encounter framework
│   │       ├── director.rs             # World director (spawn pacing, population)
│   │       ├── dungeon.rs              # Dungeon template definitions
│   │       ├── stats.rs                # Item registry, equipment stat calc
│   │       ├── region.rs               # Region cells, repulsion rules
│   │       ├── sparse_set.rs           # Sparse-set data structure
│   │       ├── physics_constants.rs    # Physics tuning constants
│   │       ├── collision_layers.rs     # Layer bitmasks
│   │       └── physics_backend.rs      # PhysicsBackend trait
│   ├── simulation_worker/
│   │   ├── src/
│   │   │   ├── main.rs                 # Standalone physics demo
│   │   │   ├── tick_pipeline/          # 10-phase tick pipeline
│   │   │   │   ├── mod.rs              # Pipeline orchestrator, dense caches
│   │   │   │   ├── combat.rs           # Hit resolution, damage, TargetFilter
│   │   │   │   ├── ai.rs               # NPC AI, threat, director integration
│   │   │   │   ├── controller.rs       # KCC movement, physics step
│   │   │   │   ├── skill_dispatch.rs   # Ability timeline execution
│   │   │   │   ├── collectors.rs       # Region/transform output collection
│   │   │   │   ├── finalization.rs     # End-of-tick cleanup
│   │   │   │   └── tests.rs            # 221+ unit tests
│   │   │   ├── simulation_runner.rs    # SDK-free simulation lifecycle facade
│   │   │   ├── commit_authority.rs     # Commit acknowledgement authority
│   │   │   ├── commit_builder.rs       # TickResult → CommitPackage marshalling
│   │   │   ├── tick_driver.rs          # Per-tick orchestration loop
│   │   │   ├── entity_sync.rs          # Entity lifecycle mirroring
│   │   │   ├── lag_compensation.rs     # Transform history ring buffer
│   │   │   ├── coordinator.rs          # SpacetimeDB coordinator (feature = "connected")
│   │   │   ├── physics/
│   │   │   │   ├── rapier_world.rs     # Rapier3D PhysicsBackend impl
│   │   │   │   ├── collision_groups.rs # Rapier interaction groups
│   │   │   │   └── conversions.rs      # Vec3f ↔ nalgebra conversions
│   │   │   └── module_bindings/        # Generated (cargo xtask dev schema)
│   │   └── tests/                      # Integration tests
│   │       ├── deterministic_replay.rs # Fixture-based determinism checks
│   │       ├── encounter_pipeline.rs   # Boss encounter integration
│   │       ├── evade_heal_and_slot_reuse.rs
│   │       ├── event_sequence_preservation.rs
│   │       ├── mock_physics_teardown.rs
│   │       ├── restart_continuity.rs
│   │       └── fixtures/               # JSON replay fixtures
│   ├── server_module/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── tables.rs               # 34 SpacetimeDB tables
│   │       ├── reducers.rs             # All server-side logic
│   │       └── views.rs                # 4 per-caller views (my_region, nearby_transforms, nearby_health, nearby_entities)
│   ├── game_client/
│   │   └── src/
│   │       ├── main.rs                 # Entry point
│   │       ├── client.rs               # SDK client connection
│   │       ├── smoke_test.rs           # Integration tests (AOI, smoke, fault, denial)
│   │       └── multi_client_test.rs    # Multi-client integration tests
│   └── game_client_bevy/
│       └── src/
│           ├── main.rs                 # Bevy app bootstrap
│           ├── spacetime.rs            # SpacetimePlugin (connect, subscribe, frame_tick)
│           ├── sync.rs                 # Entity sync (server entities → Bevy meshes)
│           ├── input.rs                # WASD movement + ability hotkeys
│           ├── camera.rs               # Orbiting follow camera
│           ├── hud.rs                  # Debug text overlay
│           ├── ability_bar.rs          # Ability bar UI + cooldowns
│           ├── ability_visuals.rs      # Hitbox/telegraph visualization
│           ├── combat_log.rs           # Scrolling combat event log
│           ├── inventory.rs            # Inventory/equipment UI
│           ├── instance_panel.rs       # Dungeon instance panel
│           ├── vfx.rs                  # Visual effects
│           ├── admin.rs                # Debug/admin controls
│           ├── inspector.rs            # Entity inspector
│           └── diagnostics.rs          # Performance diagnostics
└── xtask/                              # Build/dev/test task runner
    └── src/main.rs
```
