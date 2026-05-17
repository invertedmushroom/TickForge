# Community 0

> 185 nodes · cohesion 0.04

## Key Concepts

- **.get()** (76 connections) — `game_core\src\encounter\mod.rs`
- **.as_usize()** (58 connections) — `game_core\src\entity\entity_index.rs`
- **.run_tick()** (51 connections) — `simulation_worker\src\tick_pipeline\mod.rs`
- **.lookup()** (50 connections) — `game_core\src\entity\entity_store.rs`
- **.len()** (45 connections) — `simulation_worker\src\lag_compensation.rs`
- **TickPipeline** (44 connections) — `simulation_worker\src\tick_pipeline\mod.rs`
- **.push()** (44 connections) — `game_core\src\stats.rs`
- **.get_mut()** (41 connections) — `game_core\src\combat\hitbox.rs`
- **.remove()** (38 connections) — `game_core\src\combat\tactical.rs`
- **.execute_ability_action()** (36 connections) — `simulation_worker\src\tick_pipeline\skill_dispatch.rs`
- **.phase_state_finalization()** (33 connections) — `simulation_worker\src\tick_pipeline\finalization.rs`
- **.apply_hit_damage()** (28 connections) — `simulation_worker\src\tick_pipeline\combat.rs`
- **.handle_use_ability()** (27 connections) — `simulation_worker\src\tick_pipeline\controller.rs`
- **.phase_ai_decisions()** (26 connections) — `simulation_worker\src\tick_pipeline\ai.rs`
- **.emit_event()** (24 connections) — `simulation_worker\src\tick_pipeline\mod.rs`
- **MockPhysics** (23 connections) — `simulation_worker\tests\restart_continuity.rs`
- **.resolve_compensated_hits()** (22 connections) — `simulation_worker\src\tick_pipeline\combat.rs`
- **.phase_controller_update()** (22 connections) — `simulation_worker\src\tick_pipeline\controller.rs`
- **.force_remove_entities()** (21 connections) — `simulation_worker\src\tick_pipeline\mod.rs`
- **.resolve_projectile_hits()** (20 connections) — `simulation_worker\src\tick_pipeline\combat.rs`
- **.get_transform()** (20 connections) — `simulation_worker\tests\restart_continuity.rs`
- **.contains()** (19 connections) — `simulation_worker\src\simulation_runner.rs`
- **TickPipeline** (18 connections) — `simulation_worker\src\tick_pipeline\controller.rs`
- **.handle_tag_target()** (16 connections) — `simulation_worker\src\tick_pipeline\controller.rs`
- **SimState** (16 connections) — `game_core\src\sim_state.rs`
- *... and 160 more nodes in this community*

## Relationships

- No strong cross-community connections detected

## Source Files

- `game_core\src\combat\hitbox.rs`
- `game_core\src\combat\skill.rs`
- `game_core\src\combat\status.rs`
- `game_core\src\combat\tactical.rs`
- `game_core\src\encounter\mod.rs`
- `game_core\src\entity\entity_index.rs`
- `game_core\src\entity\entity_store.rs`
- `game_core\src\sim_state.rs`
- `game_core\src\sparse_set.rs`
- `game_core\src\stats.rs`
- `simulation_worker\src\lag_compensation.rs`
- `simulation_worker\src\physics\rapier_world.rs`
- `simulation_worker\src\simulation_runner.rs`
- `simulation_worker\src\tick_pipeline\ai.rs`
- `simulation_worker\src\tick_pipeline\collectors.rs`
- `simulation_worker\src\tick_pipeline\combat.rs`
- `simulation_worker\src\tick_pipeline\controller.rs`
- `simulation_worker\src\tick_pipeline\finalization.rs`
- `simulation_worker\src\tick_pipeline\mod.rs`
- `simulation_worker\src\tick_pipeline\skill_dispatch.rs`

## Audit Trail

- EXTRACTED: 528 (30%)
- INFERRED: 1212 (70%)
- AMBIGUOUS: 0 (0%)

---

*Part of the graphify knowledge wiki. See [[index]] to navigate.*