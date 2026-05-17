//! Pipeline execution wrapper — SDK-free simulation lifecycle facade.
//!
//! `SimulationRunner` owns the three previously-extracted seams
//! (`TickPipeline`, `CommitAuthority`, `TickDriver`) and exposes a
//! unified API for the coordinator.  The coordinator becomes a thin
//! SpacetimeDB protocol adapter: intent gathering, wire marshalling
//! (via `CommitBuilder`), and reducer calls remain there (and will
//! move to `EntitySync` in a later seam extraction).
//!
//! This struct is not feature-gated and has no SpacetimeDB dependency,
//! so all simulation lifecycle logic is testable offline.

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
    /// Entities whose equipment changed between ticks. Drained before
    /// each `run_tick` call. The coordinator pushes entity IDs here
    /// when it observes `player_equipment` DB changes. Step 4 (Stats
    /// Pipeline) will use this set to recalculate cached stat blocks.
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

    // ── Tick lifecycle ──────────────────────────────────────────

    /// Attempt to process one simulation tick.
    ///
    /// Drains any pending stat recalculation requests (from equipment
    /// changes observed between ticks), then delegates to `TickDriver`.
    pub fn run_tick(
        &mut self,
        canonical_tick: u64,
        intents: &[game_protocol::intent::PlayerIntent],
    ) -> Result<TickResult, TickSkipped> {
        // Drain pending equipment-driven stat recalculations into the
        // pipeline's stats_dirty set for Phase 1.5 recalculation.
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
        self.pipeline.set_current_tick(TickId(max_committed_tick + 1));
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

    // ── Equipment bridge ────────────────────────────────────────

    /// Queue a stat recalculation for `entity_id`.
    ///
    /// Called by the coordinator when it observes a `player_equipment`
    /// row change (insert, update, or delete). The recalculation is
    /// applied at the start of the next `run_tick` call, before Phase 1.
    pub fn queue_stat_recalc(&mut self, entity_id: EntityId) {
        self.pending_stat_recalcs.insert(entity_id);
    }

    /// Update the aggregated equipment modifiers for an entity and
    /// mark its stats dirty for recalculation.
    pub fn update_equipment(&mut self, entity_id: EntityId, modifiers: game_core::stats::EquipmentModifiers) {
        self.pipeline.set_equipment_modifiers(entity_id, modifiers);
        self.pending_stat_recalcs.insert(entity_id);
    }

    // ── Entity lifecycle ────────────────────────────────────────

    /// Ingest an entity from a DB snapshot row into the simulation.
    pub fn spawn_entity_from_snapshot(
        &mut self,
        id: EntityId,
        kind: EntityKind,
        tick: TickId,
        max_hp: f32,
        position: Vec3f,
    ) {
        self.pipeline.spawn_entity_from_snapshot(id, kind, tick, max_hp, position);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit_authority::FailureAction;
    use std::collections::HashMap;
    use game_core::combat::skill::{
        AbilityAction, AbilityData, AbilityRegistry, AbilityTimeline,
        ScheduledAbilityAction, SkillShape,
    };
    use game_core::physics_backend::*;
    use game_protocol::types::{Transform, Quatf};

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
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 1,
            actions: vec![
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::CapsuleSweep,
                    offset: game_schema::Vec3f { x: 0.0, y: 0.0, z: 0.0 },
                }},
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::CooldownStart { duration_ticks: 20 }},
                ScheduledAbilityAction { tick_offset: 1, action: AbilityAction::ApplyDamageFrame },
                ScheduledAbilityAction { tick_offset: 2, action: AbilityAction::RemoveHitbox },
            ],
        });
        reg
    }

    struct MockPhysics {
        transforms: HashMap<EntityId, Transform>,
    }
    impl PhysicsBackend for MockPhysics {
        fn as_any(&self) -> &dyn std::any::Any { self }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }
        fn step(&mut self, _dt: f32) {}
        fn get_transform(&self, id: EntityId) -> Option<Transform> {
            self.transforms.get(&id).cloned()
        }
        fn get_all_transforms(&self) -> Vec<(EntityId, Transform)> {
            self.transforms.iter().map(|(id, t)| (*id, t.clone())).collect()
        }
        fn drain_collision_events(&mut self) -> Vec<CollisionEvent> { vec![] }
        fn remove_entity(&mut self, id: EntityId) -> bool {
            self.transforms.remove(&id).is_some()
        }
        fn set_kinematic_position(&mut self, id: EntityId, pos: Vec3f) -> bool {
            if let Some(t) = self.transforms.get_mut(&id) {
                t.position = pos;
                true
            } else { false }
        }
        fn set_kinematic_rotation(&mut self, _id: EntityId, _rot: Quatf) -> bool { true }
        fn set_linear_velocity(&mut self, _id: EntityId, _vel: Vec3f) -> bool { true }
        fn spawn_sensor(&mut self, _id: EntityId, _shape: SensorShape, _offset: Vec3f, _kind: ColliderKind) -> Option<u64> {
            Some(1)
        }
        fn spawn_world_sensor(&mut self, _position: Vec3f, _shape: SensorShape, _kind: ColliderKind, _owner: EntityId) -> u64 { 0 }
        fn set_sensor_position(&mut self, _handle: u64, _position: Vec3f) -> bool { false }
        fn remove_sensor(&mut self, _handle: u64) {}
        fn spawn_character_body(&mut self, id: EntityId, pos: Vec3f, _kind: EntityKind) -> bool {
            self.transforms.insert(id, Transform::at_position(pos.x, pos.y, pos.z));
            true
        }
        fn move_character(&mut self, id: EntityId, desired: Vec3f) -> Option<MoveResult> {
            let t = self.transforms.get_mut(&id)?;
            t.position.x += desired.x;
            t.position.y += desired.y;
            t.position.z += desired.z;
            Some(MoveResult { position: t.position, grounded: true })
        }
        fn raycast(&self, _origin: Vec3f, _direction: Vec3f, _max_distance: f32) -> Option<RayHit> { None }
    }

    fn make_runner() -> SimulationRunner {
        SimulationRunner::new(
            TickId(1),
            Box::new(MockPhysics { transforms: HashMap::new() }),
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

        // Commit is in-flight — second tick should be skipped.
        let result2 = runner.run_tick(2, &[]);
        assert!(matches!(result2, Err(TickSkipped::CommitPending(1))));

        // Acknowledge → next tick proceeds.
        runner.acknowledge_success(1);
        let result3 = runner.run_tick(2, &[]);
        assert!(result3.is_ok());
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
            Vec3f { x: 0.0, y: 1.0, z: 0.0 },
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
            Vec3f { x: 0.0, y: 1.0, z: 0.0 },
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

        let action = runner.acknowledge_failure(1, "reducer rejected");
        assert_eq!(action, FailureAction::Retry);

        // Pending is kept set — next tick is blocked until retry succeeds.
        let result = runner.run_tick(2, &[]);
        assert!(matches!(result, Err(TickSkipped::CommitPending(1))));

        // Retry succeeds — unblocks pipeline.
        runner.acknowledge_success(1);
        let result = runner.run_tick(2, &[]);
        assert!(result.is_ok());
    }
}
