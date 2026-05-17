use game_protocol::event::SimEvent;
use game_protocol::intent::PlayerIntent;
use game_protocol::tick::TickId;
use game_protocol::entity_id::EntityId;
use game_protocol::types::Transform;
use game_core::physics_backend::PhysicsBackend;
use game_core::combat::skill::{
    AbilityAction, AbilityTimeline, ScheduledAction, ScheduledActionType,
};

/// Output of a single simulation tick — committed atomically to SpacetimeDB.
pub struct TickResult {
    pub tick_id: TickId,
    pub transforms: Vec<(EntityId, Transform)>,
    pub events: Vec<SimEvent>,
}

/// The canonical 10-phase simulation tick pipeline per spec.
///
/// Phases:
///  1. Input ingestion
///  2. Controller update
///  3. Skill scheduling
///  4. Physics integration
///  5. Contact collection
///  6. Combat resolution
///  7. AI decisions
///  8. State finalization
///  9. Event emission
/// 10. Commit
///
/// Every stage consumes the output of the previous stage.
/// No stage writes to the database until the commit stage.
pub struct TickPipeline {
    current_tick: TickId,
    event_sequence: u32,
    pending_events: Vec<SimEvent>,
    physics: Box<dyn PhysicsBackend>,
    dt: f32,
    /// Per-entity scheduled actions, sorted by tick_id ascending.
    scheduled_actions: Vec<ScheduledAction>,
}

impl TickPipeline {
    pub fn new(start_tick: TickId, physics: Box<dyn PhysicsBackend>, dt: f32) -> Self {
        Self {
            current_tick: start_tick,
            event_sequence: 0,
            pending_events: Vec::new(),
            physics,
            dt,
            scheduled_actions: Vec::new(),
        }
    }

    /// Get mutable access to the physics backend (for body creation, etc.).
    pub fn physics_mut(&mut self) -> &mut dyn PhysicsBackend {
        &mut *self.physics
    }

    /// Schedule all actions from an ability timeline, starting at the given tick.
    pub fn schedule_ability(
        &mut self,
        entity: EntityId,
        timeline: &AbilityTimeline,
        start_tick: TickId,
    ) {
        for scheduled in &timeline.actions {
            let tick_id = TickId(start_tick.0 + scheduled.tick_offset as u64);
            self.scheduled_actions.push(ScheduledAction {
                tick_id,
                entity,
                action_type: ScheduledActionType::AbilityFrame {
                    ability_id: timeline.ability_id,
                    action: scheduled.action.clone(),
                },
            });
        }
        // Keep sorted by tick for efficient drain.
        self.scheduled_actions.sort_by_key(|a| a.tick_id);
    }

    /// Schedule a single deferred action (buff expiry, cooldown, etc.).
    pub fn schedule_action(&mut self, action: ScheduledAction) {
        let tick = action.tick_id;
        self.scheduled_actions.push(action);
        // Maintain sort — binary search for insertion would be faster, but
        // the queue is small enough that a full sort is fine at 20 Hz.
        self.scheduled_actions.sort_by_key(|a| a.tick_id);
        let _ = tick; // suppress unused binding
    }

    /// Execute one full simulation tick, returning results for commit.
    pub fn run_tick(&mut self, intents: &[PlayerIntent]) -> TickResult {
        self.event_sequence = 0;
        self.pending_events.clear();

        // Phase 1: Input ingestion — filter intents for this tick
        let tick_intents: Vec<_> = intents
            .iter()
            .filter(|i| i.target_tick == self.current_tick)
            .collect();

        // Phase 2: Controller update
        self.phase_controller_update(&tick_intents);

        // Phase 3: Skill scheduling
        self.phase_skill_scheduling();

        // Phase 4: Physics integration
        self.phase_physics_step();

        // Phase 5: Contact collection
        self.phase_contact_collection();

        // Phase 6: Combat resolution
        self.phase_combat_resolution();

        // Phase 7: AI decisions
        self.phase_ai_decisions();

        // Phase 8: State finalization
        self.phase_state_finalization();

        // Phase 9: Event emission (collect pending events)
        // Events have been accumulated during phases above.

        // Phase 10: Commit — build result
        let result = TickResult {
            tick_id: self.current_tick,
            transforms: self.physics.get_all_transforms(),
            events: std::mem::take(&mut self.pending_events),
        };

        self.current_tick = self.current_tick.next();
        result
    }

    // ── Phase stubs ─────────────────────────────────────────────

    fn phase_controller_update(&mut self, _intents: &[&PlayerIntent]) {
        // Apply validated movement intents to physics bodies.
    }

    fn phase_skill_scheduling(&mut self) {
        // Drain all scheduled actions due this tick (queue is sorted by tick_id).
        let current = self.current_tick;
        let split_idx = self.scheduled_actions
            .partition_point(|a| a.tick_id <= current);
        let due_actions: Vec<_> = self.scheduled_actions.drain(..split_idx).collect();

        for scheduled in due_actions {
            let entity = scheduled.entity;
            match scheduled.action_type {
                ScheduledActionType::AbilityFrame { ability_id, ref action } => {
                    self.execute_ability_action(entity, ability_id, action);
                }
                ScheduledActionType::BuffExpire { buff_id } => {
                    self.emit_event(
                        entity,
                        game_protocol::event::EventPayload::BuffExpired {
                            buff_id,
                        },
                    );
                }
                ScheduledActionType::CooldownExpire { ability_id } => {
                    self.emit_event(
                        entity,
                        game_protocol::event::EventPayload::CooldownReady {
                            ability_id,
                        },
                    );
                }
            }
        }
    }

    fn execute_ability_action(
        &mut self,
        entity: EntityId,
        ability_id: u32,
        action: &AbilityAction,
    ) {
        match action {
            AbilityAction::SpawnHitbox { shape: _ } => {
                // TODO: Create a sensor collider via PhysicsWorld::add_sensor_to_entity
                // using the correct shape and collision groups for this ability.
                self.emit_event(
                    entity,
                    game_protocol::event::EventPayload::HitboxSpawned {
                        ability_id,
                    },
                );
            }
            AbilityAction::ApplyDamageFrame => {
                // TODO: Collect current sensor overlaps and apply damage in
                // phase_combat_resolution. For now, emit a marker event.
                self.emit_event(
                    entity,
                    game_protocol::event::EventPayload::DamageFrame {
                        ability_id,
                    },
                );
            }
            AbilityAction::RemoveHitbox => {
                // TODO: Remove the sensor collider via PhysicsWorld::remove_collider.
                self.emit_event(
                    entity,
                    game_protocol::event::EventPayload::HitboxRemoved {
                        ability_id,
                    },
                );
            }
            AbilityAction::CooldownStart { duration_ticks } => {
                let expire_tick = TickId(self.current_tick.0 + *duration_ticks as u64);
                self.scheduled_actions.push(ScheduledAction {
                    tick_id: expire_tick,
                    entity,
                    action_type: ScheduledActionType::CooldownExpire { ability_id },
                });
                // Re-sort after insertion.
                self.scheduled_actions.sort_by_key(|a| a.tick_id);
            }
        }
    }

    fn phase_physics_step(&mut self) {
        // Single Rapier timestep via the PhysicsBackend trait.
        self.physics.step(self.dt);
    }

    fn phase_contact_collection(&mut self) {
        // Read Rapier contact/sensor events.
        // Build contact pairs for combat resolution.
    }

    fn phase_combat_resolution(&mut self) {
        // Hit validation, damage calculation, buff/debuff application.
        // Emit combat events.
    }

    fn phase_ai_decisions(&mut self) {
        // NPC decision logic, boss mechanics, encounter rules.
    }

    fn phase_state_finalization(&mut self) {
        // Process DespawnPending entities, lifecycle transitions.
        // Update threat tables, decay buffs.
    }

    // ── Event helpers ───────────────────────────────────────────

    fn emit_event(&mut self, entity_id: EntityId, payload: game_protocol::event::EventPayload) {
        self.pending_events.push(SimEvent {
            tick_id: self.current_tick,
            event_sequence: self.event_sequence,
            entity_id,
            payload,
        });
        self.event_sequence += 1;
    }
}
