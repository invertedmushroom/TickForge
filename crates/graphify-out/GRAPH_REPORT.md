# Graph Report - E:\dev\spacerust\jump\crates  (2026-04-19)

## Corpus Check
- 65 files · ~93,842 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1178 nodes · 3232 edges · 51 communities detected
- Extraction: 58% EXTRACTED · 42% INFERRED · 0% AMBIGUOUS · INFERRED: 1357 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- [[_COMMUNITY_Community 0|Community 0]]
- [[_COMMUNITY_Community 1|Community 1]]
- [[_COMMUNITY_Community 2|Community 2]]
- [[_COMMUNITY_Community 3|Community 3]]
- [[_COMMUNITY_Community 4|Community 4]]
- [[_COMMUNITY_Community 5|Community 5]]
- [[_COMMUNITY_Community 6|Community 6]]
- [[_COMMUNITY_Community 7|Community 7]]
- [[_COMMUNITY_Community 8|Community 8]]
- [[_COMMUNITY_Community 9|Community 9]]
- [[_COMMUNITY_Community 10|Community 10]]
- [[_COMMUNITY_Community 11|Community 11]]
- [[_COMMUNITY_Community 12|Community 12]]
- [[_COMMUNITY_Community 13|Community 13]]
- [[_COMMUNITY_Community 14|Community 14]]
- [[_COMMUNITY_Community 15|Community 15]]
- [[_COMMUNITY_Community 16|Community 16]]
- [[_COMMUNITY_Community 17|Community 17]]
- [[_COMMUNITY_Community 18|Community 18]]
- [[_COMMUNITY_Community 19|Community 19]]
- [[_COMMUNITY_Community 20|Community 20]]
- [[_COMMUNITY_Community 21|Community 21]]
- [[_COMMUNITY_Community 22|Community 22]]
- [[_COMMUNITY_Community 23|Community 23]]
- [[_COMMUNITY_Community 24|Community 24]]
- [[_COMMUNITY_Community 25|Community 25]]
- [[_COMMUNITY_Community 26|Community 26]]
- [[_COMMUNITY_Community 27|Community 27]]
- [[_COMMUNITY_Community 28|Community 28]]
- [[_COMMUNITY_Community 29|Community 29]]
- [[_COMMUNITY_Community 30|Community 30]]
- [[_COMMUNITY_Community 31|Community 31]]
- [[_COMMUNITY_Community 32|Community 32]]
- [[_COMMUNITY_Community 33|Community 33]]
- [[_COMMUNITY_Community 34|Community 34]]
- [[_COMMUNITY_Community 35|Community 35]]
- [[_COMMUNITY_Community 36|Community 36]]
- [[_COMMUNITY_Community 37|Community 37]]
- [[_COMMUNITY_Community 38|Community 38]]
- [[_COMMUNITY_Community 39|Community 39]]
- [[_COMMUNITY_Community 40|Community 40]]
- [[_COMMUNITY_Community 41|Community 41]]
- [[_COMMUNITY_Community 42|Community 42]]
- [[_COMMUNITY_Community 43|Community 43]]
- [[_COMMUNITY_Community 44|Community 44]]
- [[_COMMUNITY_Community 45|Community 45]]
- [[_COMMUNITY_Community 46|Community 46]]
- [[_COMMUNITY_Community 47|Community 47]]
- [[_COMMUNITY_Community 48|Community 48]]
- [[_COMMUNITY_Community 49|Community 49]]
- [[_COMMUNITY_Community 50|Community 50]]

## God Nodes (most connected - your core abstractions)
1. `TickId` - 77 edges
2. `EntityId` - 59 edges
3. `PhysicsWorld` - 57 edges
4. `run()` - 48 edges
5. `TickPipeline` - 44 edges
6. `SimulationRunner` - 29 edges
7. `HitboxStore` - 26 edges
8. `MockPhysics` - 23 edges
9. `MockPhysics` - 23 edges
10. `MockPhysics` - 23 edges

## Surprising Connections (you probably didn't know these)
- `make_transform()` --calls--> `EntityId`  [INFERRED]
  simulation_worker\src\commit_builder.rs → game_protocol\src\entity_id.rs
- `event_fires_and_respects_cooldown()` --calls--> `TickId`  [INFERRED]
  game_core\src\director.rs → game_protocol\src\tick.rs
- `count_players_per_region_filters_non_players()` --calls--> `EntityId`  [INFERRED]
  game_core\src\director.rs → game_protocol\src\entity_id.rs
- `eid()` --calls--> `EntityId`  [INFERRED]
  game_core\src\sim_state.rs → game_protocol\src\entity_id.rs
- `spawn_and_activate()` --calls--> `TickId`  [INFERRED]
  game_core\src\sim_state.rs → game_protocol\src\tick.rs

## Communities

### Community 0 - "Community 0"
Cohesion: 0.04
Nodes (12): TickPipeline, TickPipeline, TickPipeline, TickPipeline, EntityStore, TickPipeline, TickPipeline, MockPhysics (+4 more)

### Community 1 - "Community 1"
Cohesion: 0.05
Nodes (46): environment_groups(), hitbox_collides_with_hurtbox(), hitbox_does_not_collide_with_environment(), interaction_groups(), kcc_movement_groups(), npc_body_groups(), player_body_groups(), player_collides_with_environment() (+38 more)

### Community 2 - "Community 2"
Cohesion: 0.04
Nodes (62): quatf_identity_roundtrip(), quatf_to_rotation(), rotation_to_quatf(), vec3f_roundtrip(), vec3f_to_vector(), vector_to_vec3f(), build_ability_registry(), build_item_registry() (+54 more)

### Community 3 - "Community 3"
Cohesion: 0.07
Nodes (63): send_secondary(), accept_party_invite(), add_respawn_point(), BuffUpdate, client_connected(), CombatEventInput, commit_boss_phase(), commit_tick_results() (+55 more)

### Community 4 - "Community 4"
Cohesion: 0.09
Nodes (52): build_empty_tick_result(), build_marshals_director_spawn_with_layer(), boss_encounter_rules(), both_thresholds_same_tick(), empty_ability_registry(), encounter_cleanup_on_entity_removal(), encounter_coexists_with_normal_entities(), multiple_thresholds_fire_in_sequence() (+44 more)

### Community 5 - "Community 5"
Cohesion: 0.03
Nodes (60): ActiveBuff, Bank, BlockedData, BossPhase, BuffAppliedData, CastStartData, CCClearedData, CCImmuneData (+52 more)

### Community 6 - "Community 6"
Cohesion: 0.06
Nodes (25): EntityIndex, AiState, AuditDomain, AuditRecord, AuditSubsystem, CombatState, cooldown_enforcement_panics_on_invalid_write(), damage_rejects_negative_and_non_finite() (+17 more)

### Community 7 - "Community 7"
Cohesion: 0.06
Nodes (31): count_players_per_region(), count_players_per_region_filters_non_players(), DirectorSpawn, DirectorState, DirectorTrigger, DynamicEvent, event_fires_and_respects_cooldown(), event_max_activations() (+23 more)

### Community 8 - "Community 8"
Cohesion: 0.11
Nodes (29): acked_tick_cannot_be_reprocessed_or_resent(), already_processed_ticks_are_skipped(), already_simulated_ticks_are_skipped(), CanProcessResult, CommitAuthority, consecutive_commits(), exhausted_retries_clear_pipeline(), failure_keeps_pending_for_retry() (+21 more)

### Community 9 - "Community 9"
Cohesion: 0.09
Nodes (16): compute_rewind_ticks(), hazard_zone_shape_matches_tick_pipeline_radius(), hitbox_candidate_radius(), hitbox_sensor_shape(), hitbox_world_position(), hitbox_world_position_forward_offset(), hitbox_world_position_rotated_offset(), hitbox_world_position_zero_offset() (+8 more)

### Community 10 - "Community 10"
Cohesion: 0.15
Nodes (14): ActiveHitbox, arm_gates_damage_frame(), clear_hit_ignored_when_not_reentry(), clear_hit_reentry(), delayed_arm_reanchors_periodic_interval_to_arm_tick(), eid(), exec(), hit_dedup() (+6 more)

### Community 11 - "Community 11"
Cohesion: 0.07
Nodes (19): AbilityAction, AbilityData, AbilityExecutionContext, AbilityExecutionId, AbilityExecutionStore, AbilityFile, AbilityParams, AbilityRegistry (+11 more)

### Community 12 - "Community 12"
Cohesion: 0.09
Nodes (33): build(), build_marshals_entity_state_updates(), build_marshals_health_updates(), build_marshals_transforms(), build_passes_consumed_intent_ids(), classify_events(), classify_events_buff_lifecycle(), classify_events_damage() (+25 more)

### Community 13 - "Community 13"
Cohesion: 0.11
Nodes (17): base_attack_power(), base_speed(), compute_cooldown_reduce_clamped(), compute_damage_in_debuff(), compute_no_buffs(), compute_npc_base_speed(), compute_with_damage_buffs(), compute_with_speed_buff() (+9 more)

### Community 14 - "Community 14"
Cohesion: 0.22
Nodes (12): contains_tracks_presence(), idx(), insert_and_get(), iter_is_dense_and_complete(), iter_mut_allows_modification(), remove_and_swap(), remove_last_element(), remove_nonexistent_returns_none() (+4 more)

### Community 15 - "Community 15"
Cohesion: 0.09
Nodes (1): MockPhysics

### Community 16 - "Community 16"
Cohesion: 0.11
Nodes (1): MockPhysics

### Community 17 - "Community 17"
Cohesion: 0.11
Nodes (1): MockPhysics

### Community 18 - "Community 18"
Cohesion: 0.11
Nodes (1): MockPhysics

### Community 19 - "Community 19"
Cohesion: 0.18
Nodes (8): DungeonRegistry, registry_insert_and_lookup(), resolve_linked_entities(), resolve_linked_entities_basic(), resolve_linked_entities_chain(), resolve_linked_entities_dangling_ref(), resolve_linked_entities_from_parsed_ron(), ResolvedInteractable

### Community 20 - "Community 20"
Cohesion: 0.12
Nodes (7): apply_dr_reduction(), ArcState, CCCategory, DREntry, DRTracker, MovementConditions, TacticalState

### Community 21 - "Community 21"
Cohesion: 0.17
Nodes (8): ActiveBuff, AiOverride, BuffFile, BuffKind, BuffModifiers, BuffRegistry, BuffTemplate, ThreatEntry

### Community 22 - "Community 22"
Cohesion: 0.2
Nodes (7): is_acting_collider(), LockOnSession, normalize_contact_pair(), RegionCell, skill_shape_to_sensor(), TickResult, TickSummary

### Community 23 - "Community 23"
Cohesion: 0.33
Nodes (6): arena_rules(), dungeon_rules(), open_world_rules(), props_never_repulse(), RegionType, RepulsionRules

### Community 24 - "Community 24"
Cohesion: 0.22
Nodes (7): DungeonFile, DungeonTemplate, GeometryDef, InteractableDef, InteractKindDef, LayerCollisionPolicy, ShapeDef

### Community 25 - "Community 25"
Cohesion: 0.25
Nodes (7): ColliderKind, CollisionEvent, EnvironmentShape, MoveResult, PhysicsBackend, RayHit, SensorShape

### Community 26 - "Community 26"
Cohesion: 0.38
Nodes (1): WeaponLoadout

### Community 27 - "Community 27"
Cohesion: 0.33
Nodes (5): AbilityTarget, BlockData, IntentAction, MoveDir, UseAbilityData

### Community 28 - "Community 28"
Cohesion: 0.5
Nodes (1): EntityRecord

### Community 29 - "Community 29"
Cohesion: 0.5
Nodes (3): EntityKind, EntityState, NpcAiState

### Community 30 - "Community 30"
Cohesion: 0.67
Nodes (2): EventPayload, SimEvent

### Community 31 - "Community 31"
Cohesion: 0.67
Nodes (1): Vec3f

### Community 32 - "Community 32"
Cohesion: 1.0
Nodes (1): PlayerIntent

### Community 33 - "Community 33"
Cohesion: 1.0
Nodes (1): CCEffect

### Community 34 - "Community 34"
Cohesion: 1.0
Nodes (1): DamageType

### Community 35 - "Community 35"
Cohesion: 1.0
Nodes (1): EquipmentSlot

### Community 36 - "Community 36"
Cohesion: 1.0
Nodes (0): 

### Community 37 - "Community 37"
Cohesion: 1.0
Nodes (0): 

### Community 38 - "Community 38"
Cohesion: 1.0
Nodes (0): 

### Community 39 - "Community 39"
Cohesion: 1.0
Nodes (0): 

### Community 40 - "Community 40"
Cohesion: 1.0
Nodes (0): 

### Community 41 - "Community 41"
Cohesion: 1.0
Nodes (0): 

### Community 42 - "Community 42"
Cohesion: 1.0
Nodes (0): 

### Community 43 - "Community 43"
Cohesion: 1.0
Nodes (0): 

### Community 44 - "Community 44"
Cohesion: 1.0
Nodes (0): 

### Community 45 - "Community 45"
Cohesion: 1.0
Nodes (0): 

### Community 46 - "Community 46"
Cohesion: 1.0
Nodes (0): 

### Community 47 - "Community 47"
Cohesion: 1.0
Nodes (0): 

### Community 48 - "Community 48"
Cohesion: 1.0
Nodes (0): 

### Community 49 - "Community 49"
Cohesion: 1.0
Nodes (0): 

### Community 50 - "Community 50"
Cohesion: 1.0
Nodes (0): 

## Knowledge Gaps
- **181 isolated node(s):** `CollisionMasks`, `SpawnDirective`, `DynamicEvent`, `EventRuntime`, `DirectorSpawn` (+176 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **Thin community `Community 32`** (2 nodes): `intent.rs`, `PlayerIntent`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 33`** (2 nodes): `CCEffect`, `cc.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 34`** (2 nodes): `DamageType`, `damage.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 35`** (2 nodes): `EquipmentSlot`, `equipment.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 36`** (1 nodes): `lib.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 37`** (1 nodes): `physics_constants.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 38`** (1 nodes): `mod.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 39`** (1 nodes): `mod.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 40`** (1 nodes): `lib.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 41`** (1 nodes): `lib.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 42`** (1 nodes): `lib.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 43`** (1 nodes): `rls.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 44`** (1 nodes): `lib.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 45`** (1 nodes): `mod.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 46`** (1 nodes): `ai.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 47`** (1 nodes): `collectors.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 48`** (1 nodes): `controller.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 49`** (1 nodes): `finalization.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.
- **Thin community `Community 50`** (1 nodes): `skill_dispatch.rs`
  Too small to be a meaningful cluster - may be noise or needs more connections extracted.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `run()` connect `Community 2` to `Community 0`, `Community 1`, `Community 3`, `Community 4`, `Community 7`, `Community 12`?**
  _High betweenness centrality (0.093) - this node is a cross-community bridge._
- **Why does `TickId` connect `Community 4` to `Community 0`, `Community 1`, `Community 2`, `Community 3`, `Community 6`, `Community 7`, `Community 8`, `Community 9`, `Community 10`, `Community 12`?**
  _High betweenness centrality (0.078) - this node is a cross-community bridge._
- **Why does `Entity` connect `Community 3` to `Community 2`, `Community 5`?**
  _High betweenness centrality (0.063) - this node is a cross-community bridge._
- **Are the 74 inferred relationships involving `TickId` (e.g. with `event_fires_and_respects_cooldown()` and `spawn_and_activate()`) actually correct?**
  _`TickId` has 74 INFERRED edges - model-reasoned connections that need verification._
- **Are the 56 inferred relationships involving `EntityId` (e.g. with `count_players_per_region_filters_non_players()` and `eid()`) actually correct?**
  _`EntityId` has 56 INFERRED edges - model-reasoned connections that need verification._
- **Are the 33 inferred relationships involving `run()` (e.g. with `.new()` and `TickId`) actually correct?**
  _`run()` has 33 INFERRED edges - model-reasoned connections that need verification._
- **What connects `CollisionMasks`, `SpawnDirective`, `DynamicEvent` to the rest of the system?**
  _181 weakly-connected nodes found - possible documentation gaps or missing edges._