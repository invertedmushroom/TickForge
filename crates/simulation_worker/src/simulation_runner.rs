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
}

impl SimulationRunner {
    /// Create a new runner with the given pipeline configuration.
    pub fn new(
        start_tick: TickId,
        physics: Box<dyn PhysicsBackend>,
        dt: f32,
        abilities: AbilityRegistry,
        buff_registry: game_core::combat::status::BuffRegistry,
    ) -> Self {
        Self {
            pipeline: TickPipeline::new(start_tick, physics, dt, abilities, buff_registry),
            commit: CommitAuthority::new(),
            tick_driver: TickDriver::new(),
            pending_stat_recalcs: HashSet::new(),
        }
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

    /// Seed the commit cursor and pipeline tick counter from a
    /// subscription snapshot.  Called once during `on_applied`.
    pub fn seed(&mut self, max_committed_tick: u64) {
        self.commit.seed(max_committed_tick);
        self.pipeline
            .set_current_tick(TickId(max_committed_tick + 1));
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
    ) {
        self.pipeline
            .spawn_entity_from_snapshot(id, kind, tick, max_hp, position);
    }

    /// Restore buff, threat, and NPC AI state from DB rows after a worker restart.
    ///
    /// Call this once per entity *after* `spawn_entity_from_snapshot` for that entity.
    /// Rows for unknown entity IDs are silently ignored.
    pub fn seed_runtime_state(
        &mut self,
        buffs: &[(EntityId, Vec<game_core::combat::status::ActiveBuff>)],
        threats: &[(EntityId, Vec<game_core::combat::status::ThreatEntry>)],
        npc_states: &[(EntityId, game_schema::NpcAiState, Option<EntityId>)],
    ) {
        self.pipeline.seed_runtime_state(buffs, threats, npc_states);
    }

    /// Hard teardown for an entity from all runtime stores.
    /// Returns `true` if the entity was present and removed.
    pub fn force_remove_entity(&mut self, id: EntityId) -> bool {
        self.pipeline.force_remove_entity(id)
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
                self.pipeline.state.ai.npc_leash_radius.insert(idx, cfg.leash_radius);
            }
            if cfg.aggro_radius > 0.0 {
                self.pipeline.state.ai.npc_aggro_radius.insert(idx, cfg.aggro_radius);
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
        self.pipeline.npc_goals.insert(entity_id, (goal_kind, priority));
    }

    /// Remove an NPC goal projection.
    pub fn remove_npc_goal(&mut self, entity_id: EntityId) {
        self.pipeline.npc_goals.remove(&entity_id);
    }

    // ── Encounter management ────────────────────────────────────────

    /// Register an encounter for a boss entity.
    pub fn register_encounter(&mut self, boss_entity: EntityId, encounter: game_core::encounter::EncounterState) {
        self.pipeline.encounters.insert(boss_entity, encounter);
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
            fear_ticks: 0,
            silence_ticks: 0,
            sleep_ticks: 0,
            targeting_mode: TargetingMode::DirectionTarget,
            cast_facing_policy: game_core::combat::skill::CastFacingPolicy::FaceAimDirection,
            projectile_speed: None,
            max_range: None,
            lock_on_timeout_ticks: None,
            max_rewind_ticks: None,
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
        runner.seed(50);

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
}
