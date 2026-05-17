# Community 1

> 105 nodes · cohesion 0.05

## Key Concepts

- **PhysicsWorld** (57 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.new()** (34 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **rapier_world.rs** (24 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **collision_groups.rs** (16 connections) — `simulation_worker\src\physics\collision_groups.rs`
- **.reuse_or_spawn_character()** (14 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **interaction_groups()** (11 connections) — `simulation_worker\src\physics\collision_groups.rs`
- **player_body_groups()** (11 connections) — `simulation_worker\src\physics\collision_groups.rs`
- **main()** (11 connections) — `simulation_worker\src\main.rs`
- **ud_stamp()** (11 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **skill_hurtbox_groups()** (10 connections) — `simulation_worker\src\physics\collision_groups.rs`
- **.add_environment_collider_on_layer()** (10 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.add_sensor_to_entity()** (10 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.spawn_world_sensor()** (10 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.step()** (10 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.add_dynamic_sphere()** (9 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.add_kinematic_capsule()** (9 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.move_character()** (9 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.raycast()** (9 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **kinematic_body_movement()** (8 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.sensor_intersections()** (8 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **.spawn_prop_body()** (8 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **ud_set_layer()** (8 connections) — `simulation_worker\src\physics\rapier_world.rs`
- **kcc_movement_groups()** (7 connections) — `simulation_worker\src\physics\collision_groups.rs`
- **npc_body_groups()** (7 connections) — `simulation_worker\src\physics\collision_groups.rs`
- **ball_falls_onto_ground()** (7 connections) — `simulation_worker\src\physics\rapier_world.rs`
- *... and 80 more nodes in this community*

## Relationships

- No strong cross-community connections detected

## Source Files

- `game_core\src\collision_layers.rs`
- `game_protocol\src\tick.rs`
- `server_module\src\reducers.rs`
- `simulation_worker\src\main.rs`
- `simulation_worker\src\physics\collision_groups.rs`
- `simulation_worker\src\physics\conversions.rs`
- `simulation_worker\src\physics\rapier_world.rs`
- `simulation_worker\tests\restart_continuity.rs`

## Audit Trail

- EXTRACTED: 443 (73%)
- INFERRED: 167 (27%)
- AMBIGUOUS: 0 (0%)

---

*Part of the graphify knowledge wiki. See [[index]] to navigate.*