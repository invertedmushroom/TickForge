The design:
- Reuses `game_client`'s `module_bindings` (via dependency)
- `SpacetimePlugin` — connects, subscribes, pumps `frame_tick()` each Bevy frame
- Entity sync — maps SpacetimeDB `nearby_transforms` rows → Bevy entities with 3D mesh representations
- Camera — orbiting 3D camera following the player
- Input — WASD movement, tab target selection, lock-on tagging, and ability submission via reducer calls
- HUD — debug overlay with health, position, facing, target state, and intent ack stats
- Crosshair — reticle with soft-target / lock-on color feedback
- Facing indicator — visible local-player forward marker for testing body-facing-dependent skills like Blink and block

*Build and test*

cargo check -p game_client_bevy

Summary:

**`game_client_bevy`** — a Bevy 0.15 3D client for the Jump MMO server.

### Files created:
- Cargo.toml — Bevy 0.15 + SpacetimeDB SDK 2.0, reuses `game_client` module_bindings
- main.rs — App bootstrap with ground plane, directional light, ambient light
- spacetime.rs — `SpacetimePlugin`: connects to SpacetimeDB, subscribes to `nearby_transforms`/`entity`/`entity_health`/`sim_tick`/`combat_event`, calls `spawn_player`, pumps `frame_tick()` each Bevy frame
- sync.rs — `SyncPlugin`: maps server entities → Bevy capsule meshes (green = local player, blue = other players, red = NPCs, purple = bosses), smooth position interpolation, health sync, auto-despawn on leave, and a local facing indicator mesh for manual testing
- input.rs — `InputPlugin`: WASD movement, tab targeting, ground targeting, lock-on tagging, block look direction, and ability submission via reducer intents
- camera.rs — `CameraPlugin`: orbiting follow camera with smooth lerp
- hud.rs — `HudPlugin`: debug text overlay showing position, facing, health, current target state, and intent ack counts

### To run:
```powershell
# Start SpacetimeDB + deploy + worker first (via the existing Dev Deploy + Run task)
# Then launch the Bevy client:
cargo run -p game_client_bevy --features connected
```

Or with custom server:
```powershell
$env:STDB_URI = "http://localhost:3000"
$env:STDB_MODULE = "jump"
cargo run -p game_client_bevy --features connected
```