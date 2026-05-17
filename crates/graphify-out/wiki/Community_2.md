# Community 2

> 98 nodes · cohesion 0.04

## Key Concepts

- **.iter()** (81 connections) — `game_core\src\sparse_set.rs`
- **.new()** (63 connections) — `simulation_worker\tests\restart_continuity.rs`
- **run()** (48 connections) — `simulation_worker\src\coordinator.rs`
- **coordinator.rs** (42 connections) — `simulation_worker\src\coordinator.rs`
- **SimulationRunner** (29 connections) — `simulation_worker\src\simulation_runner.rs`
- **.is_empty()** (22 connections) — `game_core\src\entity\entity_store.rs`
- **send_commit()** (19 connections) — `simulation_worker\src\coordinator.rs`
- **.register()** (12 connections) — `game_core\src\encounter\mod.rs`
- **make_runner()** (10 connections) — `simulation_worker\src\simulation_runner.rs`
- **.acknowledge_success()** (10 connections) — `simulation_worker\src\simulation_runner.rs`
- **simulation_runner.rs** (10 connections) — `simulation_worker\src\simulation_runner.rs`
- **subscribe_to_tables()** (9 connections) — `simulation_worker\src\coordinator.rs`
- **.run_tick()** (8 connections) — `simulation_worker\src\simulation_runner.rs`
- **load_abilities()** (7 connections) — `simulation_worker\src\coordinator.rs`
- **recompute_equipment()** (7 connections) — `simulation_worker\src\coordinator.rs`
- **mark_despawn_transitions_entity()** (7 connections) — `simulation_worker\src\simulation_runner.rs`
- **conversions.rs** (7 connections) — `simulation_worker\src\physics\conversions.rs`
- **load_items()** (6 connections) — `simulation_worker\src\coordinator.rs`
- **entity_spawn_and_remove_lifecycle()** (6 connections) — `simulation_worker\src\simulation_runner.rs`
- **.configure_npc()** (6 connections) — `simulation_worker\src\simulation_runner.rs`
- **.set_entity_team()** (6 connections) — `simulation_worker\src\simulation_runner.rs`
- **.register_timeline()** (6 connections) — `game_core\src\combat\skill.rs`
- **build_ability_registry()** (5 connections) — `simulation_worker\src\coordinator.rs`
- **build_item_registry()** (5 connections) — `simulation_worker\src\coordinator.rs`
- **load_buffs()** (5 connections) — `simulation_worker\src\coordinator.rs`
- *... and 73 more nodes in this community*

## Relationships

- No strong cross-community connections detected

## Source Files

- `game_core\src\combat\hitbox.rs`
- `game_core\src\combat\skill.rs`
- `game_core\src\encounter\mod.rs`
- `game_core\src\entity\entity_store.rs`
- `game_core\src\sparse_set.rs`
- `server_module\src\reducers.rs`
- `server_module\src\views.rs`
- `simulation_worker\src\_test_parry.rs`
- `simulation_worker\src\coordinator.rs`
- `simulation_worker\src\physics\conversions.rs`
- `simulation_worker\src\simulation_runner.rs`
- `simulation_worker\src\tick_pipeline\ai.rs`
- `simulation_worker\src\tick_pipeline\mod.rs`
- `simulation_worker\tests\restart_continuity.rs`

## Audit Trail

- EXTRACTED: 314 (49%)
- INFERRED: 331 (51%)
- AMBIGUOUS: 0 (0%)

---

*Part of the graphify knowledge wiki. See [[index]] to navigate.*