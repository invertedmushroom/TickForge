//! SDK-free simulation lifecycle facade used by the coordinator.
//!
//! `SimulationRunner` groups `TickPipeline`, `CommitAuthority`, and
//! `TickDriver` behind one API so coordinator code can focus on DB I/O.

use std::collections::HashSet;

use game_core::combat::skill::AbilityRegistry;
use game_core::physics_backend::PhysicsBackend;
use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use game_schema::EntityKind;

use crate::commit_authority::{CommitAuthority, FailureAction};
use crate::tick_driver::{TickDriver, TickSkipped};
use crate::tick_pipeline::{TickPipeline, TickResult};

/// SDK-free simulation lifecycle facade.
///
/// Consolidates `TickPipeline`, `CommitAuthority`, and `TickDriver`
/// behind a single API surface.  The coordinator holds one
/// `SimulationRunner` instead of three independent fields.
pub struct SimulationRunner {
    pipeline: TickPipeline,
    commit: CommitAuthority,
    tick_driver: TickDriver,
    /// Entity IDs that need stat recomputation on the next tick.
    pending_stat_recalcs: HashSet<EntityId>,
    /// Team assignments received before the entity was spawned.
    pending_teams: std::collections::HashMap<EntityId, u32>,
}

impl SimulationRunner {
    /// Create a new runner with the given pipeline configuration.
    pub fn new(
        start_tick: TickId,
        physics: Box<dyn PhysicsBackend>,
        dt: f32,
        abilities: AbilityRegistry,
        buff_registry: game_core::combat::status::BuffRegistry,
        global_max_rewind_ticks: u32,
    ) -> Self {
        Self {
            pipeline: TickPipeline::new(
                start_tick,
                physics,
                dt,
                abilities,
                buff_registry,
                global_max_rewind_ticks,
            ),
            commit: CommitAuthority::new(),
            tick_driver: TickDriver::new(),
            pending_stat_recalcs: HashSet::new(),
            pending_teams: std::collections::HashMap::new(),
        }
    }

    /// Access the buff template registry for rehydration lookups.
    pub fn buff_registry(&self) -> &game_core::combat::status::BuffRegistry {
        &self.pipeline.buff_registry
    }

    /// Attempt to process one simulation tick.
    pub fn run_tick(
        &mut self,
        canonical_tick: u64,
        intents: &[game_protocol::intent::PlayerIntent],
    ) -> Result<TickResult, TickSkipped> {
        // Apply deferred equipment-driven stat recomputes before tick execution.
        if !self.pending_stat_recalcs.is_empty() {
            log::info!(
                "stat_recalc: draining {} pending equipment changes",
                self.pending_stat_recalcs.len()
            );
            for eid in self.pending_stat_recalcs.drain() {
                self.pipeline.mark_stats_dirty(eid);
            }
        }

        self.tick_driver.process_tick(
            canonical_tick,
            intents,
            &mut self.pipeline,
            &mut self.commit,
        )
    }

    /// Returns `true` when a catch-up tick can be processed: there is
    /// backlog (canonical ahead of next-expected) and the pipeline has room.
    pub fn can_catch_up(&self, canonical_tick: u64) -> bool {
        canonical_tick >= self.commit.next_expected_tick()
            && matches!(
                self.commit.can_process_tick(canonical_tick),
                crate::commit_authority::CanProcessResult::Proceed
            )
    }

    /// Seed the commit cursor and pipeline tick counter from a
    /// subscription snapshot.  Called once during `on_applied`.
    pub fn seed(&mut self, max_committed_tick: u64, global_max_rewind_ticks: u32) {
        self.commit.seed(max_committed_tick);
        self.pipeline
            .set_current_tick(TickId(max_committed_tick + 1));
        self.pipeline
            .set_global_max_rewind_ticks(global_max_rewind_ticks);
    }

    /// Record a successful commit acknowledgement.
    pub fn acknowledge_success(&mut self, tick: u64) {
        self.commit.acknowledge_success(tick);
    }

    /// Record a failed commit (does not advance cursor).
    ///
    /// Returns `Retry` if the commit should be re-sent, or `Exhausted`
    /// if retries are spent and backpressure should take over.
    pub fn acknowledge_failure(&mut self, tick: u64, reason: &str) -> FailureAction {
        self.commit.acknowledge_failure(tick, reason)
    }

    /// Queue a stat recalculation for `entity_id` on the next `run_tick`.
    pub fn queue_stat_recalc(&mut self, entity_id: EntityId) {
        self.pending_stat_recalcs.insert(entity_id);
    }

    /// Update the aggregated equipment modifiers for an entity and
    /// mark its stats dirty for recalculation.
    pub fn update_equipment(
        &mut self,
        entity_id: EntityId,
        modifiers: game_core::stats::EquipmentModifiers,
    ) {
        self.pipeline.set_equipment_modifiers(entity_id, modifiers);
        self.pending_stat_recalcs.insert(entity_id);
    }

    /// Ingest an entity from a DB snapshot row into the simulation.
    pub fn spawn_entity_from_snapshot(
        &mut self,
        id: EntityId,
        kind: EntityKind,
        tick: TickId,
        max_hp: f32,
        position: Vec3f,
        layer: u32,
    ) {
        self.pipeline
            .spawn_entity_from_snapshot(id, kind, tick, max_hp, position, layer);
        if let Some(team_id) = self.pending_teams.remove(&id) {
            self.set_entity_team(id, team_id);
        }
    }

    /// Restore buff, threat, and NPC AI state from DB rows after a worker restart.
    ///
    /// Call this once per entity *after* `spawn_entity_from_snapshot` for that entity.
    /// Rows for unknown entity IDs are silently ignored.
    pub fn seed_runtime_state(
        &mut self,
        buffs: &[(EntityId, Vec<game_core::combat::status::ActiveBuff>)],
        npc_states: &[(EntityId, game_schema::NpcAiState, Option<EntityId>)],
    ) {
        self.pipeline.seed_runtime_state(buffs, npc_states);
    }

    /// Hard teardown for an entity from all runtime stores.
    /// Returns `true` if the entity was present and removed.
    pub fn force_remove_entity(&mut self, id: EntityId) -> bool {
        self.pending_teams.remove(&id);
        self.pipeline.force_remove_entity(id)
    }

    /// Remove a mirrored encounter add registration from the worker when the
    /// reducer deletes the corresponding `encounter_add` row.
    pub fn unregister_encounter_add(&mut self, add_entity: EntityId) {
        self.pipeline.unregister_encounter_add(add_entity);
    }

    /// Apply NPC spawn configuration (passive, no_chase, ability list).
    pub fn configure_npc(&mut self, id: EntityId, cfg: crate::entity_sync::NpcSpawnConfig) {
        if let Some(idx) = self.pipeline.state.entities.lookup(id) {
            if cfg.passive {
                self.pipeline.state.ai.npc_passive.insert(idx, true);
            }
            if cfg.no_chase {
                self.pipeline.state.ai.npc_no_chase.insert(idx, true);
            }
            if cfg.leash_radius > 0.0 {
                self.pipeline
                    .state
                    .ai
                    .npc_leash_radius
                    .insert(idx, cfg.leash_radius);
            } else {
                self.pipeline.state.ai.npc_leash_radius.remove(idx);
            }
            if cfg.aggro_radius > 0.0 {
                self.pipeline
                    .state
                    .ai
                    .npc_aggro_radius
                    .insert(idx, cfg.aggro_radius);
            } else {
                self.pipeline.state.ai.npc_aggro_radius.remove(idx);
            }
            if !cfg.ability_ids.is_empty() {
                // Replace default NPC abilities assigned during spawn.
                self.pipeline.state.ai.npc_ability_ids.remove(idx);
                self.pipeline
                    .state
                    .ai
                    .npc_ability_ids
                    .insert(idx, cfg.ability_ids);
            }
        }
    }

    /// Insert or update an interactable entry in the sim state.
    pub fn set_interactable(&mut self, id: EntityId, info: game_core::sim_state::InteractableInfo) {
        self.pipeline.state.interactables.insert(id, info);
    }

    /// Remove an interactable entry from the sim state.
    pub fn remove_interactable(&mut self, id: EntityId) {
        self.pipeline.state.interactables.remove(&id);
    }

    pub fn add_inventory_item_count(&mut self, entity: EntityId, item_id: u32, count: u32) {
        self.pipeline
            .add_inventory_item_count(entity, item_id, count);
    }

    pub fn remove_inventory_item_count(&mut self, entity: EntityId, item_id: u32, count: u32) {
        self.pipeline
            .remove_inventory_item_count(entity, item_id, count);
    }

    pub fn clear_inventory_items(&mut self) {
        self.pipeline.inventory_items.clear();
    }

    /// Returns the visibility layer for an entity (0 = open world).
    pub fn entity_layer(&self, id: EntityId) -> u32 {
        self.pipeline.layer_of(id)
    }

    /// Update the visibility layer for an entity in the sim's region map and physics.
    /// Called when instance join/leave changes the entity's layer.
    pub fn set_entity_layer(&mut self, id: EntityId, layer: u32) {
        if let Some(cell) = self.pipeline.entity_regions.get_mut(&id) {
            cell.layer = layer;
        }
        // Update the dense layer cache for O(1) same_layer checks.
        if let Some(idx) = self.pipeline.state.entities.lookup(id) {
            let slot = idx.as_usize();
            if slot >= self.pipeline.entity_layer_cache.len() {
                self.pipeline.entity_layer_cache.resize(slot + 1, 0);
            }
            self.pipeline.entity_layer_cache[slot] = layer;
        }
        self.pipeline.physics.set_entity_layer(id, layer);
    }

    /// Reposition an entity after a layer change or boundary event.
    ///
    /// `advisory` is the position a reducer just wrote to DB
    /// (typically `DungeonTemplate.spawn_point`). This resolves the
    /// authoritative Y by raycasting down against terrain on `new_layer`
    /// via `raycast_surface`; if the ray hits, the character center snaps
    /// to `hit.y + CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS` so the capsule
    /// rests on the surface. If nothing is hit (hole in terrain, off the
    /// heightfield) the advisory Y is used unchanged.
    ///
    /// Callers must have already updated the entity's layer via
    /// `set_entity_layer` so the raycast filter sees the correct
    /// terrain. The physics body is teleported; no contact resolution.
    pub fn reconcile_entity_to_position(
        &mut self,
        id: EntityId,
        advisory: Vec3f,
        new_layer: u32,
    ) -> bool {
        let resolved = self.resolve_spawn_position(advisory, new_layer);
        self.pipeline.physics.teleport_entity(id, resolved)
    }

    /// Compute the authoritative ground-snapped position for `advisory` on
    /// `layer`. Exposed separately so `entity.on_insert` can resolve Y
    /// before the entity is spawned into physics.
    ///
    /// Currently assumes the standard character capsule
    /// (`CAPSULE_HALF_HEIGHT` + `CAPSULE_RADIUS`). When per-kind collider
    /// sizes are introduced (e.g. distinct boss capsules), callers must
    /// switch to a variant that takes the capsule dimensions, or the
    /// spawn Y will embed larger bodies into the terrain by the size
    /// difference and force the KCC to push them out on first tick.
    pub fn resolve_spawn_position(&self, advisory: Vec3f, layer: u32) -> Vec3f {
        self.resolve_spawn_position_with_capsule(
            advisory,
            layer,
            game_core::physics_constants::CAPSULE_HALF_HEIGHT,
            game_core::physics_constants::CAPSULE_RADIUS,
        )
    }

    /// Variant of [`Self::resolve_spawn_position`] that takes explicit
    /// capsule dimensions. Use this once per-kind collider sizes diverge
    /// from the standard character capsule.
    pub fn resolve_spawn_position_with_capsule(
        &self,
        advisory: Vec3f,
        layer: u32,
        capsule_half_height: f32,
        capsule_radius: f32,
    ) -> Vec3f {
        /// Lift the ray origin well above the tallest expected terrain to
        /// guarantee we're outside any volume before casting down.
        const SKY_LIFT: f32 = 200.0;
        const MAX_DROP: f32 = 400.0;
        /// Probe distance for the cave-detection upward cast. A ceiling
        /// within this range above `advisory` indicates the spawn point is
        /// inside an enclosed volume; we then cast down from `advisory`
        /// itself instead of the sky to bypass that ceiling.
        const CEILING_PROBE: f32 = 50.0;

        let down = Vec3f {
            x: 0.0,
            y: -1.0,
            z: 0.0,
        };
        let up = Vec3f {
            x: 0.0,
            y: 1.0,
            z: 0.0,
        };

        // Cave-aware origin selection (§4.8b Phase 6).
        //
        // The naive "lift to sky, cast down" snaps to the first surface
        // below the sky — which is the *cave ceiling* when `advisory` is
        // inside a cave. Probe upward from `advisory` first: if we hit a
        // surface within `CEILING_PROBE`, treat `advisory` as inside an
        // enclosed volume and cast down from `advisory` (bypassing the
        // ceiling). Otherwise fall back to the sky cast, which is more
        // forgiving to imprecise authored spawn-point Ys on open terrain.
        let inside_cave = self
            .pipeline
            .physics
            .raycast_surface(advisory, up, CEILING_PROBE, layer)
            .is_some();

        let ray_origin = if inside_cave {
            advisory
        } else {
            Vec3f {
                x: advisory.x,
                y: advisory.y + SKY_LIFT,
                z: advisory.z,
            }
        };

        match self
            .pipeline
            .physics
            .raycast_surface(ray_origin, down, MAX_DROP, layer)
        {
            Some(hit) => Vec3f {
                x: advisory.x,
                y: hit.y + capsule_half_height + capsule_radius,
                z: advisory.z,
            },
            None => advisory,
        }
    }

    /// Update an entity's team membership in the dense cache.
    pub fn set_entity_team(&mut self, id: EntityId, team_id: u32) {
        if let Some(idx) = self.pipeline.state.entities.lookup(id) {
            let slot = idx.as_usize();
            if slot >= self.pipeline.entity_team_cache.len() {
                self.pipeline.entity_team_cache.resize(slot + 1, 0);
            }
            self.pipeline.entity_team_cache[slot] = team_id;
        } else {
            self.pending_teams.insert(id, team_id);
        }
    }

    /// Whether the entity is in `Active` state.
    pub fn is_active(&self, id: EntityId) -> bool {
        self.pipeline.state.is_active(id)
    }

    /// Mark an active entity as `DespawnPending`.
    pub fn mark_despawn(&mut self, id: EntityId) {
        self.pipeline.state.mark_despawn(id);
    }

    /// Whether the entity store has a mapping for this id.
    pub fn contains(&self, id: EntityId) -> bool {
        self.pipeline.state.entities.contains(id)
    }

    /// Whether the entity has a live index mapping (not yet fully removed).
    pub fn entity_exists(&self, id: EntityId) -> bool {
        self.pipeline.state.entities.lookup(id).is_some()
    }

    /// Mutable access to the physics backend (for instance collider management).
    pub fn physics_mut(&mut self) -> &mut dyn game_core::physics_backend::PhysicsBackend {
        self.pipeline.physics_mut()
    }

    // ── Tier 2 projections ──────────────────────────────────────────

    /// Set or update a world_phase projection for a zone.
    pub fn set_world_phase(&mut self, zone_id: u32, phase_name: String) {
        self.pipeline.world_phases.insert(zone_id, phase_name);
    }

    /// Remove a world_phase projection.
    pub fn remove_world_phase(&mut self, zone_id: u32) {
        self.pipeline.world_phases.remove(&zone_id);
    }

    /// Set or update an NPC goal projection.
    pub fn set_npc_goal(&mut self, entity_id: EntityId, goal_kind: String, priority: u32) {
        self.pipeline
            .npc_goals
            .insert(entity_id, (goal_kind, priority));
    }

    /// Remove an NPC goal projection.
    pub fn remove_npc_goal(&mut self, entity_id: EntityId) {
        self.pipeline.npc_goals.remove(&entity_id);
    }

    // ── Encounter management ────────────────────────────────────────

    /// Register an encounter for a boss entity.
    pub fn register_encounter(
        &mut self,
        boss_entity: EntityId,
        encounter: game_core::encounter::EncounterState,
    ) {
        self.pipeline.encounters.insert(boss_entity, encounter);
    }

    /// Register an encounter add membership projection with optional tags.
    ///
    /// Mirrors `encounter_add` table inserts into the pipeline maps used by
    /// encounter `OnEntityDied { tag }` trigger fan-out.
    pub fn register_encounter_add_with_tags(
        &mut self,
        add_entity: EntityId,
        boss_entity: EntityId,
        tags: &[String],
    ) {
        self.pipeline
            .register_encounter_add_with_tags(add_entity, boss_entity, tags);
    }

    // ── Director management ────────────────────────────────────────

    /// Register a dynamic world event with the Director (Phase 7.5).
    ///
    /// Used by the coordinator at startup to wire open-world spawn rules,
    /// and by `instance.on_insert` to register dungeon-scoped rules.
    pub fn register_director_event(
        &mut self,
        def: game_core::director::DynamicEvent,
    ) -> game_core::director::EventId {
        self.pipeline.director_mut().register_event(def)
    }

    /// Bulk-remove all director events bound to a visibility layer.
    ///
    /// Called when a dungeon instance expires so stale triggers on the
    /// recycled layer cannot fire.  Returns the number of events removed.
    pub fn clear_director_for_layer(&mut self, layer: u32) -> usize {
        self.pipeline.director_mut().clear_for_layer(layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit_authority::FailureAction;
    use game_core::combat::skill::{
        AbilityAction, AbilityData, AbilityRegistry, AbilityTimeline, ScheduledAbilityAction,
        SkillShape, TargetingMode,
    };
    use game_core::physics_backend::*;
    use game_protocol::types::{Quatf, Transform};
    use std::collections::HashMap;

    fn test_registry() -> AbilityRegistry {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 1,
            name: "Slash".to_string(),
            base_damage: 25.0,
            damage_type: game_schema::DamageType::Physical,
            shape: SkillShape::CapsuleSweep,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
            pierce: false,
            charge_roots_while_charging: false,
            knockdown_ticks: 0,
            stun_ticks: 0,
            pull_force: 0.0,
            launch_lift: 0.0,
            launch_recovery_ticks: 0,
            usable_while_cc: false,
            require_grounded: true,
            heal_amount: 0.0,
            fear_ticks: 0,
            silence_ticks: 0,
            sleep_ticks: 0,
            targeting_mode: TargetingMode::DirectionTarget,
            cast_facing_policy: game_core::combat::skill::CastFacingPolicy::FaceAimDirection,
            projectile_speed: None,
            max_range: None,
            lock_on_timeout_ticks: None,
            max_rewind_ticks: None,
            target_filter: game_core::combat::skill::TargetFilter::Hostile,
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 1,
            actions: vec![
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::SpawnHitbox {
                        shape: SkillShape::CapsuleSweep,
                        offset: game_schema::Vec3f {
                            x: 0.0,
                            y: 0.0,
                            z: 0.0,
                        },
                    },
                },
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::CooldownStart { duration_ticks: 20 },
                },
                ScheduledAbilityAction {
                    tick_offset: 1,
                    action: AbilityAction::ApplyDamageFrame,
                },
                ScheduledAbilityAction {
                    tick_offset: 2,
                    action: AbilityAction::RemoveHitbox,
                },
            ],
        });
        reg
    }

    struct MockPhysics {
        transforms: HashMap<EntityId, Transform>,
    }
    impl PhysicsBackend for MockPhysics {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn step(&mut self, _dt: f32) {}
        fn get_transform(&self, id: EntityId) -> Option<Transform> {
            self.transforms.get(&id).cloned()
        }
        fn get_all_transforms(&self) -> Vec<(EntityId, Transform)> {
            self.transforms
                .iter()
                .map(|(id, t)| (*id, t.clone()))
                .collect()
        }
        fn drain_collision_events(&mut self) -> Vec<CollisionEvent> {
            vec![]
        }
        fn remove_entity(&mut self, id: EntityId) -> bool {
            self.transforms.remove(&id).is_some()
        }
        fn set_kinematic_position(&mut self, id: EntityId, pos: Vec3f) -> bool {
            if let Some(t) = self.transforms.get_mut(&id) {
                t.position = pos;
                true
            } else {
                false
            }
        }
        fn set_kinematic_rotation(&mut self, _id: EntityId, _rot: Quatf) -> bool {
            true
        }
        fn set_linear_velocity(&mut self, _id: EntityId, _vel: Vec3f) -> bool {
            true
        }
        fn spawn_sensor(
            &mut self,
            _id: EntityId,
            _shape: SensorShape,
            _offset: Vec3f,
            _kind: ColliderKind,
        ) -> Option<u64> {
            Some(1)
        }
        fn spawn_world_sensor(
            &mut self,
            _position: Vec3f,
            _shape: SensorShape,
            _kind: ColliderKind,
            _owner: EntityId,
        ) -> u64 {
            0
        }
        fn set_sensor_position(&mut self, _handle: u64, _position: Vec3f) -> bool {
            false
        }
        fn remove_sensor(&mut self, _handle: u64) {}
        fn spawn_character_body(&mut self, id: EntityId, pos: Vec3f, _kind: EntityKind) -> bool {
            self.transforms
                .insert(id, Transform::at_position(pos.x, pos.y, pos.z));
            true
        }
        fn spawn_prop_body(
            &mut self,
            id: EntityId,
            pos: Vec3f,
            _half_extents: Vec3f,
            _pushable: bool,
        ) -> bool {
            self.transforms
                .insert(id, Transform::at_position(pos.x, pos.y, pos.z));
            true
        }
        fn move_character(&mut self, id: EntityId, desired: Vec3f) -> Option<MoveResult> {
            let t = self.transforms.get_mut(&id)?;
            t.position.x += desired.x;
            t.position.y += desired.y;
            t.position.z += desired.z;
            Some(MoveResult {
                position: t.position,
                grounded: true,
            })
        }
        fn raycast(
            &self,
            _origin: Vec3f,
            _direction: Vec3f,
            _max_distance: f32,
            _ignore_entity: Option<EntityId>,
        ) -> Option<RayHit> {
            None
        }
        fn line_of_sight(&self, _from: Vec3f, _to: Vec3f) -> bool {
            true
        }
        fn cast_to_wall(&self, _from: Vec3f, to: Vec3f) -> Vec3f {
            to
        }
        fn teleport_entity(&mut self, id: EntityId, pos: Vec3f) -> bool {
            self.set_kinematic_position(id, pos)
        }
    }

    fn make_runner() -> SimulationRunner {
        SimulationRunner::new(
            TickId(1),
            Box::new(MockPhysics {
                transforms: HashMap::new(),
            }),
            0.05,
            test_registry(),
            game_core::combat::status::BuffRegistry::new(),
            crate::lag_compensation::MAX_REWIND_TICKS,
        )
    }

    #[test]
    fn run_tick_returns_result_and_tracks_commit() {
        let mut runner = make_runner();
        let result = runner.run_tick(1, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().tick_id, TickId(1));

        // Pipeline depth=2: tick 2 proceeds even with tick 1 in-flight.
        let result2 = runner.run_tick(2, &[]);
        assert!(result2.is_ok());
        assert_eq!(result2.unwrap().tick_id, TickId(2));

        // Pipeline full — tick 3 blocked until an ack arrives.
        let result3 = runner.run_tick(3, &[]);
        assert!(matches!(result3, Err(TickSkipped::CommitPending(1))));

        // Acknowledge tick 1 → tick 3 proceeds.
        runner.acknowledge_success(1);
        let result4 = runner.run_tick(3, &[]);
        assert!(result4.is_ok());
        assert_eq!(result4.unwrap().tick_id, TickId(3));
    }

    #[test]
    fn seed_aligns_commit_and_pipeline() {
        let mut runner = make_runner();
        runner.seed(50, 4);

        // Ticks up to 50 are already committed.
        let result = runner.run_tick(50, &[]);
        assert!(matches!(result, Err(TickSkipped::AlreadyProcessed)));

        // Tick 51 proceeds (pipeline was advanced to 51).
        let result = runner.run_tick(51, &[]);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().tick_id, TickId(51));
    }

    #[test]
    fn entity_spawn_and_remove_lifecycle() {
        let mut runner = make_runner();
        let eid = EntityId(42);

        assert!(!runner.contains(eid));
        runner.spawn_entity_from_snapshot(
            eid,
            EntityKind::Npc,
            TickId(1),
            100.0,
            Vec3f {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
            0,
        );
        assert!(runner.contains(eid));
        assert!(runner.entity_exists(eid));

        // Force removal clears all stores.
        let removed = runner.force_remove_entity(eid);
        assert!(removed);
        assert!(!runner.entity_exists(eid));
    }

    #[test]
    fn mark_despawn_transitions_entity() {
        let mut runner = make_runner();
        let eid = EntityId(7);

        runner.spawn_entity_from_snapshot(
            eid,
            EntityKind::Player,
            TickId(1),
            100.0,
            Vec3f {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
            0,
        );

        // Activate the entity so is_active returns true.
        // Entities start in Spawning state; run a tick to let Phase 8 activate it.
        // For this focused test, directly activate via state.
        runner.pipeline.state.activate_entity(eid);
        assert!(runner.is_active(eid));

        runner.mark_despawn(eid);
        assert!(!runner.is_active(eid));
    }

    #[test]
    fn acknowledge_failure_keeps_pending_for_retry() {
        let mut runner = make_runner();
        let _ = runner.run_tick(1, &[]);

        // Fill the pipeline so failure actually blocks.
        let _ = runner.run_tick(2, &[]);

        let action = runner.acknowledge_failure(1, "reducer rejected");
        assert_eq!(action, FailureAction::Retry);

        // Pipeline full and oldest failed — tick 3 is blocked.
        let result = runner.run_tick(3, &[]);
        assert!(matches!(result, Err(TickSkipped::CommitPending(1))));

        // Retry succeeds — frees a slot.
        runner.acknowledge_success(1);
        let result = runner.run_tick(3, &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn later_canonical_tick_catches_up_missing_tick_first() {
        let mut runner = make_runner();

        let intents = vec![
            game_protocol::intent::PlayerIntent {
                entity_id: EntityId(100),
                sequence_id: 1,
                target_tick: TickId(2),
                client_observed_tick: 0,
                action: game_protocol::intent::IntentAction::Jump,
            },
            game_protocol::intent::PlayerIntent {
                entity_id: EntityId(100),
                sequence_id: 2,
                target_tick: TickId(3),
                client_observed_tick: 0,
                action: game_protocol::intent::IntentAction::Jump,
            },
        ];

        // Tick 1 in-flight.
        let first = runner.run_tick(1, &[]).unwrap();
        assert_eq!(first.tick_id, TickId(1));

        // Pipeline depth=2: tick 2 proceeds (catches up contiguously).
        let second = runner.run_tick(3, &intents).unwrap();
        assert_eq!(second.tick_id, TickId(2));
        assert_eq!(second.summary.intents_processed, 1);

        // Pipeline full — ack tick 1 to free a slot.
        runner.acknowledge_success(1);

        let third = runner.run_tick(4, &intents).unwrap();
        assert_eq!(third.tick_id, TickId(3));
        assert_eq!(third.summary.intents_processed, 1);
    }

    /// Phase 2c coverage: `resolve_spawn_position` must raycast-snap the
    /// capsule center to `surface_y + CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS`
    /// on the requested layer's terrain, replacing whatever Y the reducer
    /// advisory carried.
    #[test]
    fn resolve_spawn_position_snaps_y_to_heightfield() {
        use crate::physics::rapier_world::PhysicsWorld;
        use game_core::physics_backend::{EnvironmentShape, PhysicsBackend};
        use game_core::physics_constants::{CAPSULE_HALF_HEIGHT, CAPSULE_RADIUS};

        let mut world = PhysicsWorld::new(1.0 / 60.0);
        // Flat heightfield at y=2.0 on the shared layer (0).
        world.add_environment_collider_on_layer(
            EnvironmentShape::Heightfield {
                nrows: 3,
                ncols: 3,
                scale_x: 10.0,
                scale_y: 1.0,
                scale_z: 10.0,
                heights: vec![2.0_f32; 9],
            },
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            0,
        );
        world.step();

        let runner = SimulationRunner::new(
            TickId(1),
            Box::new(world),
            1.0 / 60.0,
            test_registry(),
            game_core::combat::status::BuffRegistry::new(),
            crate::lag_compensation::MAX_REWIND_TICKS,
        );

        // Advisory Y is garbage (50 m in the air): should be replaced by
        // terrain surface + capsule offset.
        let advisory = Vec3f {
            x: 0.5,
            y: 50.0,
            z: -1.0,
        };
        let resolved = runner.resolve_spawn_position(advisory, 0);
        let expected_y = 2.0 + CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS;
        assert!(
            (resolved.y - expected_y).abs() < 0.1,
            "resolved y={} want≈{}",
            resolved.y,
            expected_y
        );
        // XZ passes through unchanged.
        assert_eq!(resolved.x, advisory.x);
        assert_eq!(resolved.z, advisory.z);
    }

    /// If the raycast misses (no terrain on `layer`), the advisory
    /// position must be returned verbatim so the reducer's hint is the
    /// authoritative fallback.
    #[test]
    fn resolve_spawn_position_passthrough_on_miss() {
        use crate::physics::rapier_world::PhysicsWorld;

        let mut world = PhysicsWorld::new(1.0 / 60.0);
        world.step();

        let runner = SimulationRunner::new(
            TickId(1),
            Box::new(world),
            1.0 / 60.0,
            test_registry(),
            game_core::combat::status::BuffRegistry::new(),
            crate::lag_compensation::MAX_REWIND_TICKS,
        );

        // No heightfield authored on layer 99: with strict same-layer
        // queries, the downward raycast cannot see the layer-0 placeholder
        // floor, so it misses regardless of XZ. XZ is kept at extreme
        // values for legacy parity with the prior shared-plane test.
        let advisory = Vec3f {
            x: 10_000.0,
            y: 7.5,
            z: -10_000.0,
        };
        let resolved = runner.resolve_spawn_position(advisory, 99);
        assert_eq!(resolved.x, advisory.x);
        assert_eq!(resolved.y, advisory.y);
        assert_eq!(resolved.z, advisory.z);
    }

    /// §4.8b Phase 6: a spawn point inside a cave (advisory below a
    /// ceiling) must snap to the cave floor, not the ceiling above it.
    /// The naive sky-down cast hit the ceiling first; the cave-aware
    /// path probes upward, detects the ceiling, and casts down from
    /// `advisory` itself instead.
    #[test]
    fn resolve_spawn_position_snaps_to_cave_floor_not_ceiling() {
        use crate::physics::rapier_world::PhysicsWorld;
        use game_core::physics_backend::{EnvironmentShape, PhysicsBackend};
        use game_core::physics_constants::{CAPSULE_HALF_HEIGHT, CAPSULE_RADIUS};

        let mut world = PhysicsWorld::new(1.0 / 60.0);
        // Cave floor at y=0 (cuboid top surface at y=0).
        world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 50.0,
                half_y: 0.5,
                half_z: 50.0,
            },
            Vec3f::new(0.0, -0.5, 0.0),
            5,
        );
        // Cave ceiling at y=10 (cuboid bottom surface at y=10).
        world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 50.0,
                half_y: 0.5,
                half_z: 50.0,
            },
            Vec3f::new(0.0, 10.5, 0.0),
            5,
        );
        world.step();

        let runner = SimulationRunner::new(
            TickId(1),
            Box::new(world),
            1.0 / 60.0,
            test_registry(),
            game_core::combat::status::BuffRegistry::new(),
            crate::lag_compensation::MAX_REWIND_TICKS,
        );

        // Advisory inside the cave: y=5 (between floor=0 and ceiling=10).
        let advisory = Vec3f {
            x: 1.0,
            y: 5.0,
            z: 2.0,
        };
        let resolved = runner.resolve_spawn_position(advisory, 5);
        let expected_y = 0.0 + CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS;
        assert!(
            (resolved.y - expected_y).abs() < 0.1,
            "cave spawn snapped to ceiling instead of floor: y={} want≈{}",
            resolved.y,
            expected_y
        );
    }

    /// §4.8b Phase 6: open-terrain spawns still benefit from sky-cast
    /// robustness — an advisory placed 100 m above the surface still
    /// snaps correctly because no ceiling is detected above it.
    #[test]
    fn resolve_spawn_position_snaps_open_terrain_from_far_above() {
        use crate::physics::rapier_world::PhysicsWorld;
        use game_core::physics_backend::{EnvironmentShape, PhysicsBackend};
        use game_core::physics_constants::{CAPSULE_HALF_HEIGHT, CAPSULE_RADIUS};

        let mut world = PhysicsWorld::new(1.0 / 60.0);
        // Single ground cuboid, no ceiling above.
        world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 50.0,
                half_y: 0.5,
                half_z: 50.0,
            },
            Vec3f::new(0.0, -0.5, 0.0),
            6,
        );
        world.step();

        let runner = SimulationRunner::new(
            TickId(1),
            Box::new(world),
            1.0 / 60.0,
            test_registry(),
            game_core::combat::status::BuffRegistry::new(),
            crate::lag_compensation::MAX_REWIND_TICKS,
        );

        // Advisory miles above terrain — sky-cast must still find ground.
        let advisory = Vec3f {
            x: 0.0,
            y: 150.0,
            z: 0.0,
        };
        let resolved = runner.resolve_spawn_position(advisory, 6);
        let expected_y = 0.0 + CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS;
        assert!(
            (resolved.y - expected_y).abs() < 0.1,
            "open-terrain spawn failed to snap from far above: y={} want≈{}",
            resolved.y,
            expected_y
        );
    }

    #[test]
    fn unregister_encounter_add_clears_pipeline_tracking() {
        use crate::physics::rapier_world::PhysicsWorld;

        let mut world = PhysicsWorld::new(1.0 / 60.0);
        world.step();

        let mut runner = SimulationRunner::new(
            TickId(1),
            Box::new(world),
            1.0 / 60.0,
            test_registry(),
            game_core::combat::status::BuffRegistry::new(),
            crate::lag_compensation::MAX_REWIND_TICKS,
        );

        let add = EntityId(200);
        let boss = EntityId(100);

        // Register with tags
        runner.register_encounter_add_with_tags(add, boss, &["my_tag".to_string()]);

        // Verify they are registered
        assert_eq!(runner.pipeline.add_to_boss.get(&add), Some(&boss));
        assert!(runner.pipeline.entity_tags.contains_key(&add));

        // Unregister
        runner.unregister_encounter_add(add);

        // Verify cleared
        assert!(!runner.pipeline.add_to_boss.contains_key(&add));
        assert!(!runner.pipeline.entity_tags.contains_key(&add));
    }
}
