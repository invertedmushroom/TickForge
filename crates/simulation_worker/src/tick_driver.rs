//! SDK-free tick scheduling/execution orchestration.
//!
//! `TickDriver` gates tick eligibility through `CommitAuthority`, keeps
//! pipeline tick counters aligned, runs one tick, marks it in-flight, and
//! emits summary logs.

use std::time::Instant;

use log::{debug, info, warn};

use crate::commit_authority::{CanProcessResult, CommitAuthority};
use crate::tick_pipeline::{TickPipeline, TickResult, TickSummary};
use game_protocol::intent::PlayerIntent;
use game_protocol::tick::TickId;

/// Why `process_tick` declined to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickSkipped {
    /// Tick already committed — harmless duplicate.
    AlreadyProcessed,
    /// A prior commit is still in-flight (or pipeline depth is full).
    CommitPending(u64),
}

/// Tick intake and scheduling interaction seam.
///
/// Coordinates the pipeline execution cycle without any SpacetimeDB
/// dependency.  The coordinator owns `TickDriver` inside
/// `CoordinatorState` alongside `CommitAuthority`.
pub struct TickDriver {
    /// How often to emit summary logs even when nothing interesting happened.
    pub summary_interval: u64,
}

impl Default for TickDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl TickDriver {
    pub fn new() -> Self {
        Self {
            summary_interval: 20,
        }
    }

    /// Process one eligible tick and mark it in-flight.
    ///
    /// Returns `Err(TickSkipped)` for duplicates or backpressure conditions.
    pub fn process_tick(
        &self,
        canonical_tick: u64,
        intents: &[PlayerIntent],
        pipeline: &mut TickPipeline,
        commit: &mut CommitAuthority,
    ) -> Result<TickResult, TickSkipped> {
        match commit.can_process_tick(canonical_tick) {
            CanProcessResult::AlreadyProcessed => {
                debug!("Tick {canonical_tick} already processed, skipping");
                return Err(TickSkipped::AlreadyProcessed);
            }
            CanProcessResult::CommitPending(pending) => {
                warn!("tick={canonical_tick} skipped — commit for tick={pending} still in-flight");
                return Err(TickSkipped::CommitPending(pending));
            }
            CanProcessResult::PipelineFull(oldest) => {
                warn!(
                    "tick={canonical_tick} skipped — pipeline full, oldest in-flight tick={oldest}"
                );
                return Err(TickSkipped::CommitPending(oldest));
            }
            CanProcessResult::Proceed => {}
        }

        let tick_to_process = commit.next_expected_tick();
        let gap = canonical_tick.saturating_sub(tick_to_process);
        if gap > commit.pipeline_depth() as u64 {
            warn!(
                "tick backlog: canonical={canonical_tick} processing_contiguous_tick={tick_to_process} gap={gap}"
            );
        } else if gap > 0 {
            debug!(
                "tick pipelining: canonical={canonical_tick} processing_contiguous_tick={tick_to_process} gap={gap}"
            );
        }

        let pipeline_tick = pipeline.current_tick();
        if pipeline_tick != TickId(tick_to_process) {
            warn!(
                "tick desync: canonical={canonical_tick} executing={tick_to_process} pipeline={} — advancing pipeline to match",
                pipeline_tick.0,
            );
            pipeline.set_current_tick(TickId(tick_to_process));
        }

        let tick_start = Instant::now();
        let mut result = pipeline.run_tick(intents);
        result.summary.tick_duration_us = tick_start.elapsed().as_micros() as u64;
        result.summary.commit_retries = commit.retry_count();

        commit.mark_in_flight(tick_to_process);

        self.log_summary(tick_to_process, &result.summary);

        Ok(result)
    }

    /// Emit a structured tick summary when interesting or periodic.
    fn log_summary(&self, tick: u64, summary: &TickSummary) {
        if tick.is_multiple_of(self.summary_interval)
            || summary.damage_events > 0
            || summary.deaths > 0
            || summary.despawns > 0
            || summary.intents_processed > 0
        {
            info!(
                "tick={tick} intents={} contacts={} damage={} deaths={} despawns={} entities={} hitboxes={} transforms={} region_updates={} actions={} tick_us={} retries={}",
                summary.intents_processed,
                summary.contacts,
                summary.damage_events,
                summary.deaths,
                summary.despawns,
                summary.active_entities,
                summary.active_hitboxes,
                summary.transform_updates,
                summary.region_updates,
                summary.scheduled_actions_len,
                summary.tick_duration_us,
                summary.commit_retries,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit_authority::CommitAuthority;
    use crate::tick_pipeline::TickPipeline;
    use game_core::combat::skill::{
        AbilityAction, AbilityData, AbilityRegistry, AbilityTimeline, ScheduledAbilityAction,
        SkillShape, TargetingMode,
    };
    use game_protocol::tick::TickId;

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

    fn mock_pipeline() -> TickPipeline {
        use game_core::physics_backend::*;
        use game_protocol::entity_id::EntityId;
        use game_protocol::types::Transform;
        use std::collections::HashMap;

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
            fn set_kinematic_position(
                &mut self,
                id: EntityId,
                pos: game_protocol::types::Vec3f,
            ) -> bool {
                if let Some(t) = self.transforms.get_mut(&id) {
                    t.position = pos;
                    true
                } else {
                    false
                }
            }
            fn set_kinematic_rotation(
                &mut self,
                _id: EntityId,
                _rot: game_protocol::types::Quatf,
            ) -> bool {
                true
            }
            fn set_linear_velocity(
                &mut self,
                _id: EntityId,
                _vel: game_protocol::types::Vec3f,
            ) -> bool {
                true
            }
            fn spawn_sensor(
                &mut self,
                _id: EntityId,
                _shape: SensorShape,
                _offset: game_protocol::types::Vec3f,
                _kind: ColliderKind,
            ) -> Option<u64> {
                Some(1)
            }
            fn spawn_world_sensor(
                &mut self,
                _position: game_protocol::types::Vec3f,
                _shape: SensorShape,
                _kind: ColliderKind,
                _owner: EntityId,
            ) -> u64 {
                0
            }
            fn set_sensor_position(
                &mut self,
                _handle: u64,
                _position: game_protocol::types::Vec3f,
            ) -> bool {
                false
            }
            fn remove_sensor(&mut self, _handle: u64) {}
            fn spawn_character_body(
                &mut self,
                id: EntityId,
                pos: game_protocol::types::Vec3f,
                _kind: game_schema::EntityKind,
            ) -> bool {
                self.transforms
                    .insert(id, Transform::at_position(pos.x, pos.y, pos.z));
                true
            }
            fn spawn_prop_body(
                &mut self,
                id: EntityId,
                pos: game_protocol::types::Vec3f,
                _half_extents: game_protocol::types::Vec3f,
                _pushable: bool,
            ) -> bool {
                self.transforms
                    .insert(id, Transform::at_position(pos.x, pos.y, pos.z));
                true
            }
            fn move_character(
                &mut self,
                id: EntityId,
                desired: game_protocol::types::Vec3f,
            ) -> Option<MoveResult> {
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
                _origin: game_protocol::types::Vec3f,
                _direction: game_protocol::types::Vec3f,
                _max_distance: f32,
                _ignore_entity: Option<EntityId>,
            ) -> Option<RayHit> {
                None
            }
            fn line_of_sight(
                &self,
                _from: game_protocol::types::Vec3f,
                _to: game_protocol::types::Vec3f,
            ) -> bool {
                true
            }
            fn cast_to_wall(
                &self,
                _from: game_protocol::types::Vec3f,
                to: game_protocol::types::Vec3f,
            ) -> game_protocol::types::Vec3f {
                to
            }
            fn teleport_entity(&mut self, id: EntityId, pos: game_protocol::types::Vec3f) -> bool {
                self.set_kinematic_position(id, pos)
            }
        }

        TickPipeline::new(
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
    fn process_tick_runs_pipeline_and_marks_in_flight() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline();
        let mut commit = CommitAuthority::new();

        let result = driver.process_tick(1, &[], &mut pipeline, &mut commit);
        assert!(result.is_ok());
        let tick_result = result.unwrap();
        assert_eq!(tick_result.tick_id, TickId(1));
        assert_eq!(commit.pending_tick(), Some(1));
        assert_eq!(commit.last_processed_tick(), 0); // not advanced yet
    }

    #[test]
    fn already_processed_tick_is_skipped() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline();
        let mut commit = CommitAuthority::new();
        commit.seed(5);

        let result = driver.process_tick(3, &[], &mut pipeline, &mut commit);
        assert!(matches!(result, Err(TickSkipped::AlreadyProcessed)));
    }

    #[test]
    fn pipeline_full_skips_tick() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline();
        let mut commit = CommitAuthority::new();

        // Process tick 1 — marks it in-flight.
        let _ = driver.process_tick(1, &[], &mut pipeline, &mut commit);
        assert_eq!(commit.pending_tick(), Some(1));

        // Process tick 2 — second in-flight (depth=2 allows it).
        let r2 = driver.process_tick(2, &[], &mut pipeline, &mut commit);
        assert!(r2.is_ok());
        assert_eq!(commit.in_flight_count(), 2);

        // Tick 3 should be skipped — pipeline full at depth 2.
        let result = driver.process_tick(3, &[], &mut pipeline, &mut commit);
        assert!(matches!(result, Err(TickSkipped::CommitPending(1))));
    }

    #[test]
    fn desync_is_corrected() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline(); // starts at tick 1
        let mut commit = CommitAuthority::new();

        // Ask to process a later canonical tick while the next contiguous tick is 1.
        // TickDriver should keep the pipeline on the next expected tick.
        let result = driver.process_tick(5, &[], &mut pipeline, &mut commit);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().tick_id, TickId(1));
    }

    #[test]
    fn backlog_processes_next_expected_tick() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline();
        let mut commit = CommitAuthority::new();
        commit.seed(3);

        let result = driver.process_tick(5, &[], &mut pipeline, &mut commit);
        assert!(result.is_ok());
        let tick_result = result.unwrap();
        assert_eq!(tick_result.tick_id, TickId(4));
        assert_eq!(commit.pending_tick(), Some(4));
    }

    #[test]
    fn consecutive_ticks_after_ack() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline();
        let mut commit = CommitAuthority::new();

        let r1 = driver
            .process_tick(1, &[], &mut pipeline, &mut commit)
            .unwrap();
        assert_eq!(r1.tick_id, TickId(1));

        commit.acknowledge_success(1);

        let r2 = driver
            .process_tick(2, &[], &mut pipeline, &mut commit)
            .unwrap();
        assert_eq!(r2.tick_id, TickId(2));
        assert_eq!(commit.pending_tick(), Some(2));
    }

    #[test]
    fn pipelined_ticks_without_intermediate_ack() {
        let driver = TickDriver::new();
        let mut pipeline = mock_pipeline();
        let mut commit = CommitAuthority::new();

        // Process two ticks without any ack in between.
        let r1 = driver
            .process_tick(1, &[], &mut pipeline, &mut commit)
            .unwrap();
        assert_eq!(r1.tick_id, TickId(1));

        let r2 = driver
            .process_tick(2, &[], &mut pipeline, &mut commit)
            .unwrap();
        assert_eq!(r2.tick_id, TickId(2));
        assert_eq!(commit.in_flight_count(), 2);

        // Ack tick 1 — frees a slot.
        commit.acknowledge_success(1);
        assert_eq!(commit.in_flight_count(), 1);

        // Tick 3 can now proceed.
        let r3 = driver
            .process_tick(3, &[], &mut pipeline, &mut commit)
            .unwrap();
        assert_eq!(r3.tick_id, TickId(3));
    }
}
