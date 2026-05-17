# Community 4

> 78 nodes · cohesion 0.09

## Key Concepts

- **TickId** (77 connections) — `game_protocol\src\tick.rs`
- **EntityId** (59 connections) — `game_protocol\src\entity_id.rs`
- **.default()** (40 connections) — `simulation_worker\src\tick_driver.rs`
- **.spawn_entity_from_snapshot()** (23 connections) — `simulation_worker\src\tick_pipeline\mod.rs`
- **.sync_insert()** (22 connections) — `simulation_worker\src\entity_sync.rs`
- **entity_sync.rs** (21 connections) — `simulation_worker\src\entity_sync.rs`
- **test_runner()** (16 connections) — `simulation_worker\src\entity_sync.rs`
- **.seed_runtime_state()** (15 connections) — `simulation_worker\src\tick_pipeline\mod.rs`
- **spawn_boss_with_encounter()** (14 connections) — `simulation_worker\tests\encounter_pipeline.rs`
- **buff_full_modifier_fields_survive_restart_via_registry()** (14 connections) — `simulation_worker\tests\restart_continuity.rs`
- **.new()** (13 connections) — `simulation_worker\tests\encounter_pipeline.rs`
- **evade_arrival_heals_and_emits_event()** (13 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- **evade_heal_caps_at_max_hp()** (13 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- **encounter_pipeline.rs** (13 connections) — `simulation_worker\tests\encounter_pipeline.rs`
- **.sync_update()** (12 connections) — `simulation_worker\src\entity_sync.rs`
- **evade_at_full_hp_no_heal_event()** (12 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- **slot_reuse_npc_to_npc_resets_sparse_to_defaults()** (11 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- **.seed()** (11 connections) — `simulation_worker\src\simulation_runner.rs`
- **empty_ability_registry()** (10 connections) — `simulation_worker\tests\encounter_pipeline.rs`
- **encounter_cleanup_on_entity_removal()** (10 connections) — `simulation_worker\tests\encounter_pipeline.rs`
- **make_pipeline()** (10 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- **slot_reuse_clears_all_sparse_components()** (10 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- **buff_state_survives_restart()** (10 connections) — `simulation_worker\tests\restart_continuity.rs`
- **npc_ai_state_survives_restart()** (10 connections) — `simulation_worker\tests\restart_continuity.rs`
- **evade_heal_and_slot_reuse.rs** (10 connections) — `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- *... and 53 more nodes in this community*

## Relationships

- No strong cross-community connections detected

## Source Files

- `game_core\src\sim_state.rs`
- `game_protocol\src\entity_id.rs`
- `game_protocol\src\tick.rs`
- `simulation_worker\src\commit_builder.rs`
- `simulation_worker\src\entity_sync.rs`
- `simulation_worker\src\simulation_runner.rs`
- `simulation_worker\src\tick_driver.rs`
- `simulation_worker\src\tick_pipeline\mod.rs`
- `simulation_worker\tests\encounter_pipeline.rs`
- `simulation_worker\tests\evade_heal_and_slot_reuse.rs`
- `simulation_worker\tests\event_sequence_preservation.rs`
- `simulation_worker\tests\restart_continuity.rs`

## Audit Trail

- EXTRACTED: 306 (41%)
- INFERRED: 449 (59%)
- AMBIGUOUS: 0 (0%)

---

*Part of the graphify knowledge wiki. See [[index]] to navigate.*