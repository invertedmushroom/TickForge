The design:
- Reuses `game_client`'s `module_bindings` (via dependency)
- `SpacetimePlugin` — connects, subscribes, pumps `frame_tick()` each Bevy frame
- Entity sync — maps SpacetimeDB `nearby_transforms` rows → Bevy entities with 3D mesh representations
- Camera — orbiting 3D camera following the player
- Input — WASD movement, tab target selection, crosshair-driven lock-on tagging, and ability submission via reducer calls
- HUD — debug overlay with health, position, facing, target state, and intent ack stats
- Crosshair — shared HUD/raycast reticle with soft-target / lock-on color feedback and canonical world aim point
- Facing indicator — visible forward marker for the local player plus NPCs/bosses for testing body-facing-dependent skills like Blink and block

*Build and test*

cargo check -p game_client_bevy

Summary:

**`game_client_bevy`** — a Bevy 0.15 3D client for the TickForge MMO server.

### Files created:
- Cargo.toml — Bevy 0.15 + SpacetimeDB SDK 2.1, reuses `game_client` module_bindings
- main.rs — App bootstrap with ground plane, directional light, ambient light
- spacetime.rs — `SpacetimePlugin`: connects to SpacetimeDB, subscribes to `nearby_transforms`/`entity`/`entity_health`/`sim_tick`/`combat_event`, calls `spawn_player`, pumps `frame_tick()` each Bevy frame
- sync.rs — `SyncPlugin`: maps server entities → Bevy capsule meshes (green = local player, blue = other players, red = NPCs, purple = bosses), smooth position interpolation, health sync, auto-despawn on leave, screen-projected name tags, and facing indicator meshes for the local player plus NPCs/bosses
- input.rs — `InputPlugin`: WASD movement, tab targeting, shared-reticle crosshair solving, ground targeting, crosshair-only lock-on tagging, block look direction, and ability submission via reducer intents
- camera.rs — `CameraPlugin`: orbiting follow camera with smooth lerp
- hud.rs — `HudPlugin`: debug text overlay showing position, facing, health, current target state, intent ack counts, and the active crosshair aim solution

### To run:
```powershell
# Start SpacetimeDB + deploy + worker first (via the existing Dev Deploy + Run task)
# Then launch the Bevy client:
cargo run -p game_client_bevy --features connected
```

Or with custom server:
```powershell
$env:STDB_URI = "http://localhost:3000"
$env:STDB_MODULE = "tickforge"
cargo run -p game_client_bevy --features connected
```
