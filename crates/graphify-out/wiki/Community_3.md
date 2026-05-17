# Community 3

> 80 nodes · cohesion 0.07

## Key Concepts

- **.insert()** (116 connections) — `game_core\src\combat\tactical.rs`
- **reducers.rs** (63 connections) — `server_module\src\reducers.rs`
- **is_module_admin()** (22 connections) — `server_module\src\reducers.rs`
- **is_debug_caller()** (21 connections) — `server_module\src\reducers.rs`
- **.entity_layer()** (19 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **Entity** (18 connections) — `server_module\src\tables.rs`
- **create_instance()** (13 connections) — `server_module\src\reducers.rs`
- **.at_position()** (12 connections) — `game_protocol\src\types.rs`
- **expire_instances()** (10 connections) — `server_module\src\reducers.rs`
- **spawn_npc_internal()** (9 connections) — `server_module\src\reducers.rs`
- **.next()** (9 connections) — `game_protocol\src\tick.rs`
- **debug_join_instance()** (8 connections) — `server_module\src\reducers.rs`
- **is_trusted_caller()** (8 connections) — `server_module\src\reducers.rs`
- **Instance** (8 connections) — `server_module\src\tables.rs`
- **debug_create_instance()** (7 connections) — `server_module\src\reducers.rs`
- **debug_spawn_prop()** (7 connections) — `server_module\src\reducers.rs`
- **Party** (7 connections) — `server_module\src\tables.rs`
- **debug_apply_buff()** (6 connections) — `server_module\src\reducers.rs`
- **world_clock()** (6 connections) — `server_module\src\reducers.rs`
- **commit_tick_results()** (5 connections) — `server_module\src\reducers.rs`
- **debug_grant_item()** (5 connections) — `server_module\src\reducers.rs`
- **debug_remove_entity()** (5 connections) — `server_module\src\reducers.rs`
- **debug_set_team()** (5 connections) — `server_module\src\reducers.rs`
- **debug_spawn_combat()** (5 connections) — `server_module\src\reducers.rs`
- **increment_zone_counter()** (5 connections) — `server_module\src\reducers.rs`
- *... and 55 more nodes in this community*

## Relationships

- No strong cross-community connections detected

## Source Files

- `game_core\src\combat\tactical.rs`
- `game_protocol\src\tick.rs`
- `game_protocol\src\types.rs`
- `server_module\src\reducers.rs`
- `server_module\src\tables.rs`
- `simulation_worker\src\coordinator.rs`
- `simulation_worker\src\physics\rapier_world.rs`
- `simulation_worker\src\simulation_runner.rs`
- `simulation_worker\tests\encounter_pipeline.rs`
- `simulation_worker\tests\evade_heal_and_slot_reuse.rs`

## Audit Trail

- EXTRACTED: 265 (48%)
- INFERRED: 282 (52%)
- AMBIGUOUS: 0 (0%)

---

*Part of the graphify knowledge wiki. See [[index]] to navigate.*