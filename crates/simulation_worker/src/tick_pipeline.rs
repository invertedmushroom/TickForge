use std::collections::{HashMap, HashSet};
use game_protocol::event::{EventPayload, SimEvent};
use game_protocol::intent::{IntentAction, MoveDir, PlayerIntent};
use game_protocol::tick::TickId;
use game_protocol::entity_id::EntityId;
use game_protocol::types::Transform;
use game_core::physics_backend::{ColliderKind, PhysicsBackend, SensorShape};
use game_core::combat::skill::{
    AbilityAction, AbilityTimeline, AbilityRegistry,
    AbilityExecutionContext, AbilityExecutionId, ResolvedTargeting,
    ScheduledAction, ScheduledActionType, SkillShape,
};
use game_core::entity::entity_index::EntityIndex;
use game_core::sim_state::SimState;
use game_protocol::types::{Quatf, Vec3f};
use game_schema::EntityKind;

/// Record a mutation in the audit log (no-op in release builds).
macro_rules! audit {
    ($state:expr, $domain:ident, $subsystem:ident, $phase:expr, $entity:expr, $detail:expr) => {
        #[cfg(any(debug_assertions, test))]
        {
            use game_core::sim_state::{AuditDomain, AuditSubsystem};
            $state.audit.record(
                AuditDomain::$domain,
                AuditSubsystem::$subsystem,
                $phase,
                $entity,
                $detail,
            );
        }
    };
}

/// Per-tick statistics for observability. One summary emitted per tick in the coordinator.
/// Counters are incremented inline during pipeline phases — no post-hoc scanning.
#[derive(Clone, Copy, Debug, Default)]
pub struct TickSummary {
    pub intents_processed: usize,
    pub contacts: usize,
    pub damage_events: usize,
    pub deaths: usize,
    pub despawns: usize,
    pub active_entities: usize,
    pub active_hitboxes: usize,
}

/// Output of a single simulation tick — committed atomically to SpacetimeDB.
pub struct TickResult {
    pub tick_id: TickId,
    pub transforms: Vec<(EntityId, Transform)>,
    pub events: Vec<SimEvent>,
    pub summary: TickSummary,
    /// Entity lifecycle transitions that occurred this tick.
    /// Each entry is (entity_id, new_state). Coordinator marshals these into
    /// `EntityStateUpdate` wire types so the DB stays a faithful lifecycle projection.
    pub entity_state_updates: Vec<(EntityId, game_schema::EntityState)>,
    /// Health snapshots for entities whose hp changed this tick (took damage).
    /// Each entry is (entity_id, current_hp, max_hp). Coordinator marshals these into
    /// `HealthUpdate` wire types. Collected after all combat phases, before entity removal,
    /// so killed entities (hp=0) are included in the same commit as their death event.
    pub health_updates: Vec<(EntityId, f32, f32)>,
    /// Full buff snapshot for all active entities. Each entry is
    /// (entity_id, Vec<ActiveBuff>). Written as delete-all-then-insert
    /// per entity in the reducer.
    pub buff_updates: Vec<(EntityId, Vec<game_core::combat::status::ActiveBuff>)>,
    /// Full threat table snapshot for NPCs/bosses with non-empty threat.
    /// Each entry is (npc_entity_id, Vec<ThreatEntry>).
    pub threat_updates: Vec<(EntityId, Vec<game_core::combat::status::ThreatEntry>)>,
    /// NPC AI state snapshot for all active NPCs/bosses.
    /// Each entry is (entity_id, NpcAiState, top_threat target).
    pub npc_state_updates: Vec<(EntityId, game_schema::NpcAiState, Option<EntityId>)>,
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
    /// Static ability definitions — looked up during combat resolution.
    abilities: AbilityRegistry,
    /// Runtime entity state — lifecycle, health, buffs, threat, AI, contacts.
    pub state: SimState,
    /// Explicit cooldown state — maps (EntityId, ability_id) → tick when the cooldown expires.
    ///
    /// The HashMap owns cooldown truth: O(1) lookup in Phase 2, drained in Phase 8 to emit
    /// CooldownReady events. Retroactive cooldown reduction is a direct mutation of the ready_at
    /// value. Entity despawn cleaning removes all entries in a single `retain` call.
    cooldowns: HashMap<(EntityId, u32), TickId>,
    /// Monotonically increasing counter for `ScheduledAction::id`.
    /// Assigned at scheduling time; never reused within a session.
    next_scheduled_id: u64,
    /// Running stats for the current tick — incremented inline, returned in TickResult.
    summary: TickSummary,
}

impl TickPipeline {
    /// Hard teardown for an entity from *all* runtime stores and the physics backend.
    /// Returns `true` if an entity was actually removed from `SimState`, `false` if it
    /// was not present.
    pub fn force_remove_entity(&mut self, id: EntityId) -> bool {
        use std::collections::HashSet;

        // 1) Determine all execution IDs owned by this caster so we can fully
        //    remove any dependent runtime objects (scheduled actions, sensors, hitboxes).
        let execs_to_remove: HashSet<AbilityExecutionId> = self
            .state
            .combat
            .executions
            .active_ids()
            .into_iter()
            .filter(|&eid| {
                self.state
                    .combat
                    .executions
                    .get(eid)
                    .map(|ctx| ctx.caster == id)
                    .unwrap_or(false)
            })
            .collect();

        // 2) Remove any future actions that target this entity OR are sourced from
        //    an execution owned by this entity (prevents orphaned actions referencing
        //    dead executions). Use retain to keep only actions that are unrelated.
        self.scheduled_actions.retain(|a| {
            if a.entity == id {
                return false;
            }
            match a.source {
                Some(src) => !execs_to_remove.contains(&src),
                None => true,
            }
        });

        // 3) Remove cooldown entries for this entity
        self.cooldowns.retain(|&(eid, _), _| eid != id);

        // 4) Remove hitbox sensors and execution contexts owned by this entity
        for exec_id in execs_to_remove.iter().copied() {
            if let Some(handle) = self
                .state
                .combat
                .hitboxes
                .get(exec_id)
                .and_then(|hb| hb.sensor_handle)
            {
                self.physics.remove_sensor(handle);
            }
            self.state.combat.hitboxes.remove(exec_id);
            self.state.combat.executions.remove(exec_id);
        }

        // 5) Finally, remove entity from SimState and physics world
        let removed = self.state.remove_entity(id);
        // Always attempt to remove physics body; remove_entity is idempotent there.
        self.physics.remove_entity(id);
        removed
    }
    pub fn new(
        start_tick: TickId,
        physics: Box<dyn PhysicsBackend>,
        dt: f32,
        abilities: AbilityRegistry,
    ) -> Self {
        Self {
            current_tick: start_tick,
            event_sequence: 0,
            pending_events: Vec::new(),
            physics,
            dt,
            scheduled_actions: Vec::new(),
            abilities,
            state: SimState::new(),
            cooldowns: HashMap::new(),
            next_scheduled_id: 0,
            summary: TickSummary::default(),
}
    }

    /// Get mutable access to the physics backend (for body creation, etc.).
    pub fn physics_mut(&mut self) -> &mut dyn PhysicsBackend {
        &mut *self.physics
    }

    /// Override the pipeline's internal tick counter.
    ///
    /// Must be called in `on_applied` after the subscription snapshot is loaded so
    /// that `pipeline.current_tick` matches the first real tick the coordinator will
    /// deliver via `sim_tick.on_insert`.  Without this sync, the inner
    /// `i.target_tick == self.current_tick` filter in `run_tick` never matches intents
    /// targeted at the actual live tick (e.g. tick 170 when the pipeline starts at 0).
    pub fn set_current_tick(&mut self, tick: TickId) {
        self.current_tick = tick;
    }

    /// Read the pipeline's current tick (for sync diagnostics in the coordinator).
    pub fn current_tick(&self) -> TickId {
        self.current_tick
    }

    /// Ingest an entity from a DB snapshot row into the simulation.
    ///
    /// Registers the entity in SimState and creates the appropriate physics body
    /// based on its kind. Safe to call multiple times for the same entity — the
    /// physics body creation is idempotent.
    pub fn spawn_entity_from_snapshot(
        &mut self,
        id: EntityId,
        kind: EntityKind,
        tick: TickId,
        max_hp: f32,
        position: Vec3f,
    ) {
        // Idempotency guard — the coordinator's on_insert callback already checks
        // contains() before calling here, but this defence-in-depth prevents a double-
        // spawn if the call sequence ever regresses (e.g. a second on_insert for the
        // same entity after a subscription re-apply).
        if self.state.entities.contains(id) {
            return;
        }
        self.state.spawn_entity(id, kind, tick, max_hp);
        match kind {
            EntityKind::Player | EntityKind::Npc | EntityKind::Boss => {
                self.physics.spawn_character_body(id, position, kind);
            }
            // Projectile and Hazard bodies are spawned by the ability/encounter system;
            // the DB row alone is not enough to reconstruct the full physics state.
            EntityKind::Projectile | EntityKind::Hazard => {}
        }
    }

    /// Seed runtime state from DB rows recovered on worker restart.
    ///
    /// Must be called after `spawn_entity_from_snapshot` so the entity already
    /// has an `EntityIndex` mapping.  Silently ignores rows whose entity is not
    /// present (e.g. entities that became Removed between the commit and the restart).
    pub fn seed_runtime_state(
        &mut self,
        buffs: &[(EntityId, Vec<game_core::combat::status::ActiveBuff>)],
        threats: &[(EntityId, Vec<game_core::combat::status::ThreatEntry>)],
        npc_states: &[(EntityId, game_schema::NpcAiState, Option<EntityId>)],
    ) {
        for (eid, entity_buffs) in buffs {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                self.state.status.buffs[idx.as_usize()] = entity_buffs.clone();
            }
        }
        for (eid, entries) in threats {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                if let Some(ref mut table) = self.state.combat.threat_tables[idx.as_usize()] {
                    table.entries = entries.clone();
                }
            }
        }
        for (eid, ai_state, _target) in npc_states {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                self.state.ai.npc_ai[idx.as_usize()] = Some(*ai_state);
                // target_entity is re-derived each tick from the threat table in the
                // AI decisions phase — no need to persist it in NpcAiState.
            }
        }
    }

    /// Downcast the physics backend to a concrete type.
    /// Returns `None` if the backend is not of type `T`.
    pub fn physics_as<T: 'static>(&mut self) -> Option<&mut T> {
        self.physics.as_any_mut().downcast_mut::<T>()
    }

    /// Schedule all actions from an ability timeline, starting at the given tick.
    ///
    /// Each `AbilityFrame` action carries `execution_id` so later phases can look up
    /// the cast-time context (targeting, origin, facing) from `SimState::combat.executions`.
    ///
    /// Scheduling rules:
    /// - Actions are always scheduled for future ticks (start_tick + offset).
    /// - Multiple actions may land on the same tick; they execute in insertion order.
    /// - Scheduling into the past is treated as a bug (debug_assert).
    /// - Scheduling is not idempotent — callers must not double-schedule.
    pub fn schedule_ability(
        &mut self,
        entity: EntityId,
        timeline: &AbilityTimeline,
        start_tick: TickId,
        execution_id: AbilityExecutionId,
    ) {
        for scheduled in &timeline.actions {
            let tick_id = TickId(start_tick.0 + scheduled.tick_offset as u64);
            debug_assert!(
                tick_id >= self.current_tick,
                "scheduled action for past tick {tick_id} (current: {})",
                self.current_tick
            );
            self.scheduled_actions.push(ScheduledAction {
                id: { let id = self.next_scheduled_id; self.next_scheduled_id += 1; id },
                tick_id,
                entity,
                source: Some(execution_id),
                action_type: ScheduledActionType::AbilityFrame {
                    execution_id,
                    ability_id: timeline.ability_id,
                    action: scheduled.action.clone(),
                },
            });
        }
        // Keep sorted by tick for efficient drain.
        self.scheduled_actions.sort_by_key(|a| a.tick_id);
    }

    /// Schedule a single deferred action (buff expiry, etc.).
    /// Same rules as `schedule_ability` — future ticks only, not idempotent.
    /// The action's `id` is assigned by the pipeline (caller-provided value is overwritten).
    pub fn schedule_action(&mut self, mut action: ScheduledAction) {
        debug_assert!(
            action.tick_id >= self.current_tick,
            "scheduled action for past tick {} (current: {})",
            action.tick_id, self.current_tick
        );
        action.id = self.next_scheduled_id;
        self.next_scheduled_id += 1;
        self.scheduled_actions.push(action);
        // Maintain sort — binary search for insertion would be faster, but
        // the queue is small enough that a full sort is fine at 20 Hz.
        self.scheduled_actions.sort_by_key(|a| a.tick_id);
    }

    /// Returns true if `ability_id` is on cooldown for `entity` this tick.
    ///
    /// O(1) — looks up the ready_at tick in the cooldown HashMap. The entry is absent
    /// (implying not on cooldown) when the ability has never been cast or the cooldown
    /// already expired and was pruned by `phase_expire_cooldowns`.
    fn is_on_cooldown(&self, entity: EntityId, ability_id: u32) -> bool {
        self.cooldowns
            .get(&(entity, ability_id))
            .map_or(false, |&ready_at| self.current_tick < ready_at)
    }

    /// Test-visible alias for `is_on_cooldown`. Production code uses the private method
    /// directly; tests need it to assert post-cast cooldown state without going through
    /// a UseAbility intent.
    #[cfg(test)]
    pub fn is_on_cooldown_pub(&self, entity: EntityId, ability_id: u32) -> bool {
        self.is_on_cooldown(entity, ability_id)
    }

    /// Returns true if this execution still owns any **runtime effect** that must be
    /// resolved before the context can be freed.
    ///
    /// Right now a "runtime effect" is either:
    ///   - a pending scheduled action that cites this execution as its source, or
    ///   - a live hitbox entry (armed or not yet on its damage frame).
    ///
    /// When new effect types are added, extend the predicate here so that culling
    /// remains correct without touching `phase_skill_scheduling`:
    ///
    /// ```text
    ///   || self.state.combat.projectiles.contains(exec_id)
    ///   || self.state.combat.buffs.contains(exec_id)
    /// ```
    fn execution_is_alive(&self, exec_id: AbilityExecutionId) -> bool {
        self.scheduled_actions.iter().any(|a| a.source == Some(exec_id))
            || self.state.combat.hitboxes.get(exec_id).is_some()
    }

    /// Execute one full simulation tick, returning results for commit.
    pub fn run_tick(&mut self, intents: &[PlayerIntent]) -> TickResult {
        self.state.debug_assert_coherent();
        #[cfg(any(debug_assertions, test))]
        self.state.audit.reset();
        self.event_sequence = 0;
        self.pending_events.clear();
        self.summary = TickSummary::default();

        // Phase 1: Input ingestion — filter intents for this tick
        let tick_intents: Vec<_> = intents
            .iter()
            .filter(|i| i.target_tick == self.current_tick)
            .collect();
        self.summary.intents_processed = tick_intents.len();

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

        // Snapshot health for entities that took damage this tick — must happen BEFORE
        // phase_state_finalization removes dead entity slots from the EntityStore.
        let health_updates = self.collect_health_updates();

        // Phase 8: State finalization
        // 8a: Drain ready cooldowns — emit CooldownReady events for abilities that became
        //     available this tick. Must run before 8b so the events land in pending_events
        //     and are included in this tick’s TickResult.
        self.phase_expire_cooldowns();
        // 8b: Entity lifecycle transitions, buff expiry, threat decay, spawning → active.
        let entity_state_updates = self.phase_state_finalization();

        // Phase 9: Event emission (collect pending events)
        // Events have been accumulated during phases above.

        // Invariant checks — only active in debug builds (debug_assert! is a no-op in release).
        // Catches parallel-structure divergence between hitbox arming and sensor-handle
        // ownership plus entity component arrays. Called here (post-all-phases) to catch
        // mid-tick corruption.
        #[cfg(debug_assertions)]
        self.verify_invariants();

        // Phase 10: Commit — build result
        self.summary.active_entities = (0..self.state.entities.len())
            .filter(|&i| matches!(
                self.state.entities.states[i],
                game_core::entity::lifecycle::EntityState::Active
            ))
            .count();
        self.summary.active_hitboxes = self.state.combat.hitboxes.len();
        // Collect runtime domain snapshots for persistence.
        let buff_updates = self.collect_buff_updates();
        let threat_updates = self.collect_threat_updates();
        let npc_state_updates = self.collect_npc_state_updates();

        let result = TickResult {
            tick_id: self.current_tick,
            transforms: self.physics.get_all_transforms(),
            events: std::mem::take(&mut self.pending_events),
            summary: self.summary,
            entity_state_updates,
            health_updates,
            buff_updates,
            threat_updates,
            npc_state_updates,
        };

        self.current_tick = self.current_tick.next();

        #[cfg(any(debug_assertions, test))]
        if self.state.audit.total_writes() > 0 {
            log::trace!("tick {} {}", result.tick_id.0, self.state.audit.summary_line());
        }

        result
    }

    // ── Invariant verification (debug builds only) ──────────────

    /// Cross-structure invariant checks for parallel state systems.
    ///
    /// The key invariant: for each active hitbox, `armed` and `sensor_handle.is_some()`
    /// must describe the same lifecycle state. Silent divergence causes ghost hits or
    /// missing hit registration that are very hard to reproduce.
    ///
    /// Also re-asserts that all SoA component arrays have the same length as the entity store.
    ///
    /// Called at end of each tick in debug builds; uses `debug_assert!` so it compiles away
    /// in release.
    fn verify_invariants(&self) {
        // All parallel component arrays must agree on entity count.
        self.state.debug_assert_coherent();

        // HitboxStore must agree internally on which execution IDs are armed and which
        // execution IDs own a live sensor handle. A count-only check hides identity bugs;
        // key-set diff catches silent divergence.
        let sensor_keys = self.state.combat.hitboxes.sensor_backed_execution_ids();
        let armed_keys = self.state.combat.hitboxes.armed_execution_ids();
        debug_assert_eq!(
            sensor_keys,
            armed_keys,
            "sensor-backed key-set and armed HitboxStore key-set diverged — hitbox lifecycle bug\n  sensor-backed: {:?}\n  armed hitboxes: {:?}",
            sensor_keys,
            armed_keys,
        );
    }

    // ── Phase 2: Controller update ──────────────────────────────

    fn phase_controller_update(&mut self, intents: &[&PlayerIntent]) {
        // Track which (entity, ability) pairs have been cast in this Phase 2 pass.
        // Without this, two UseAbility intents for the same ability targeting the same
        // tick both pass is_on_cooldown() — the cooldown map isn’t updated until Phase 3.
        // This closes the same-tick double-cast exploit before the cooldown state exists.
        let mut cast_this_tick: HashSet<(EntityId, u32)> = HashSet::new();

        for intent in intents {
            let entity_id = intent.entity_id;

            // Only process intents for active entities.
            let is_active = self.state.entities.lookup(entity_id)
                .map_or(false, |idx| self.state.entities.is_active(idx));
            if !is_active {
                continue;
            }

            match &intent.action {
                IntentAction::Move(dir) => {
                    self.apply_movement(entity_id, dir);
                }
                IntentAction::Stop => {
                    self.apply_stop(entity_id);
                }
                IntentAction::FaceTo(dir) => {
                    // Project direction onto the XZ plane for yaw-only rotation.
                    // Characters don't pitch or roll, so only the Y-axis angle matters.
                    // Zero-length XZ component means no meaningful facing — skip.
                    let xz_sq = dir.dir_x * dir.dir_x + dir.dir_z * dir.dir_z;
                    if xz_sq > 1e-6 {
                        // atan2(x, z): angle from +Z (forward) toward +X (right).
                        let yaw = dir.dir_x.atan2(dir.dir_z);
                        let half_yaw = yaw * 0.5;
                        let rotation = Quatf {
                            x: 0.0,
                            y: half_yaw.sin(),
                            z: 0.0,
                            w: half_yaw.cos(),
                        };
                        self.physics.set_kinematic_rotation(entity_id, rotation);
                        audit!(self.state, Transform, Controller, 2, Some(entity_id), "face_to");
                    }
                }
                IntentAction::UseAbility(data) => {
                    let ability_id = data.ability_id;
                    let cast_key = (entity_id, ability_id);
                    // Reject if already cast this Phase 2 pass (same-tick double-cast guard).
                    // The cooldown map insert happens in Phase 3 (CooldownStart action), so
                    // is_on_cooldown alone cannot catch two UseAbility intents for the same tick.
                    if cast_this_tick.contains(&cast_key) {
                        continue;
                    }
                    // Validate cooldown: skip if a CooldownExpire is queued for this entity+ability.
                    if self.is_on_cooldown(entity_id, ability_id) {
                        continue;
                    }
                    // Clone to release the shared borrow before calling schedule_ability (&mut self).
                    if let Some(timeline) = self.abilities.get_timeline(ability_id).cloned() {
                        // Resolve wire-format targeting to the runtime variant.
                        let targeting = match &data.target {
                            game_schema::AbilityTarget::None => ResolvedTargeting::SelfCast,
                            game_schema::AbilityTarget::Entity(id) => {
                                ResolvedTargeting::Entity { target: EntityId(*id) }
                            }
                            game_schema::AbilityTarget::Position(p) => {
                                ResolvedTargeting::Position { point: *p }
                            }
                            game_schema::AbilityTarget::Direction(d) => {
                                ResolvedTargeting::Direction { dir: *d }
                            }
                        };
                        // Snapshot caster position and facing at cast time so later timeline
                        // phases (ApplyDamageFrame, projectile spawn, etc.) use cast-time
                        // geometry rather than the caster's current position.
                        let (origin, facing) = if let Some(t) = self.physics.get_transform(entity_id) {
                            // Yaw quaternion: (0, sin(θ/2), 0, cos(θ/2)) → facing = (sinθ, 0, cosθ).
                            let yaw = 2.0 * t.rotation.y.atan2(t.rotation.w);
                            (t.position, Vec3f { x: yaw.sin(), y: 0.0, z: yaw.cos() })
                        } else {
                            (Vec3f::ZERO, Vec3f { x: 0.0, y: 0.0, z: 1.0 })
                        };
                        let execution_id = self.state.combat.executions.next_id();
                        self.state.combat.executions.insert(AbilityExecutionContext {
                            execution_id,
                            ability_id,
                            caster: entity_id,
                            started_at: self.current_tick,
                            targeting,
                            origin,
                            facing,
                        });
                        audit!(self.state, Execution, Controller, 2, Some(entity_id), "cast");
                        self.schedule_ability(entity_id, &timeline, self.current_tick, execution_id);
                        cast_this_tick.insert(cast_key);
                    }
                }
                IntentAction::Interact(_target) => {
                    // TODO: Validate proximity, trigger interaction.
                }
            }
        }
    }

    fn apply_movement(&mut self, entity_id: EntityId, dir: &MoveDir) {
        // Normalize direction and scale by a base movement speed.
        // TODO: Per-entity movement speed from a stats component.
        const BASE_SPEED: f32 = 5.0;
        // Grounded movement: ignore vertical component to prevent client "flight".
        let x = dir.dir_x;
        let z = dir.dir_z;
        let len_sq = x * x + z * z;
        if len_sq < 1e-6 {
            return;
        }
        let inv_len = 1.0 / len_sq.sqrt();
        let vx = x * inv_len * BASE_SPEED;
        let vy = 0.0;
        let vz = z * inv_len * BASE_SPEED;

        // Character bodies are kinematic_position_based — velocity calls have no effect.
        // Integrate one timestep of desired velocity and set the explicit next position.
        if let Some(t) = self.physics.get_transform(entity_id) {
            self.physics.set_kinematic_position(entity_id, Vec3f {
                x: t.position.x + vx * self.dt,
                y: t.position.y + vy * self.dt,
                z: t.position.z + vz * self.dt,
            });
            audit!(self.state, Transform, Controller, 2, Some(entity_id), "move");
        }
    }

    fn apply_stop(&mut self, _entity_id: EntityId) {
        // For kinematic_position_based bodies, not issuing a set_kinematic_position
        // this tick is sufficient — the body remains at its current position.
    }

    // ── Phase 3: Skill scheduling ───────────────────────────────

    fn phase_skill_scheduling(&mut self) {
        // Drain all scheduled actions due this tick (queue is sorted by tick_id).
        let current = self.current_tick;
        let split_idx = self.scheduled_actions
            .partition_point(|a| a.tick_id <= current);
        let due_actions: Vec<_> = self.scheduled_actions.drain(..split_idx).collect();

        for scheduled in due_actions {
            let entity = scheduled.entity;
            log::debug!(
                "dispatch sched={} tick={} entity={} source={:?} action={}",
                scheduled.id,
                scheduled.tick_id.0,
                entity.0,
                scheduled.source,
                match &scheduled.action_type {
                    ScheduledActionType::AbilityFrame { action, .. } => match action {
                        AbilityAction::SpawnHitbox { .. } => "SpawnHitbox",
                        AbilityAction::ApplyDamageFrame => "ApplyDamageFrame",
                        AbilityAction::RemoveHitbox => "RemoveHitbox",
                        AbilityAction::CooldownStart { .. } => "CooldownStart",
                    },
                    ScheduledActionType::BuffExpire { .. } => "BuffExpire",
                },
            );
            match scheduled.action_type {
                ScheduledActionType::AbilityFrame { execution_id, ability_id, ref action } => {
                    self.execute_ability_action(entity, ability_id, execution_id, action);
                }
                ScheduledActionType::BuffExpire { buff_id } => {
                    self.emit_event(
                        entity,
                        EventPayload::BuffExpired { buff_id },
                    );
                }
            }
        }

        // Cull execution contexts for casts that no longer own any runtime effect.
        //
        // This covers abilities whose timeline has no RemoveHitbox (pure cooldown,
        // buff-apply) — without this pass their AbilityExecutionContext would leak
        // for the lifetime of the session.
        //
        // The `execution_is_alive` predicate defines what "still owning an effect"
        // means.  Extend that method when new effect types (projectiles, buffs) are
        // introduced — no changes here are needed.
        let dead: Vec<AbilityExecutionId> = self
            .state
            .combat
            .executions
            .active_ids()
            .into_iter()
            .filter(|&id| !self.execution_is_alive(id))
            .collect();
        for id in dead {
            self.state.combat.executions.remove(id);
        }
    }

    fn execute_ability_action(
        &mut self,
        entity: EntityId,
        ability_id: u32,
        execution_id: AbilityExecutionId,
        action: &AbilityAction,
    ) {
        match action {
            AbilityAction::SpawnHitbox { shape, offset } => {
                // Declare the hitbox logically — no Rapier sensor yet.
                //
                // The sensor is deferred to `ApplyDamageFrame` so that the Rapier
                // physics step on the damage-frame tick is the first to see the
                // collider, generating CollisionEvent::started on the correct tick.
                // Before this fix, SpawnHitbox spawned the sensor immediately, so
                // Rapier fired contacts one tick early and damage landed on the spawn
                // tick rather than the intended damage-frame tick.
                self.state.combat.hitboxes.spawn(execution_id, entity, ability_id, self.current_tick, *shape, *offset);
                audit!(self.state, Hitbox, AbilityTimeline, 3, Some(entity), "spawn");
                self.emit_event(entity, EventPayload::HitboxSpawned { ability_id });
            }
            AbilityAction::ApplyDamageFrame => {
                // Materialise the Rapier sensor for this hitbox.
                //
                // Phase 3 runs before Phase 4 (physics step), so inserting the collider
                // here means Rapier generates CollisionEvent::started on this same tick.
                // Phase 6 then resolves the contacts — damage fires exactly when the
                // timeline says the damage frame is open.
                let stored = self.state.combat.hitboxes
                    .get(execution_id)
                    .map(|hb| (hb.shape, hb.offset));
                if let Some((shape, offset)) = stored {
                    let sensor_shape = skill_shape_to_sensor(shape);
                    if let Some(handle) = self.physics.spawn_sensor(
                        entity,
                        sensor_shape,
                        offset,
                        ColliderKind::Hitbox(execution_id.0),
                    ) {
                        // Only mark armed if the hitbox state transitions successfully.
                        if self.state.combat.hitboxes.arm(execution_id, handle) {
                            audit!(self.state, Hitbox, AbilityTimeline, 3, Some(entity), "arm");
                        } else {
                            // If arm failed, remove the sensor we just created to avoid leaks.
                            self.physics.remove_sensor(handle);
                        }
                    }
                }
                self.emit_event(entity, EventPayload::DamageFrame { ability_id });
            }
            AbilityAction::RemoveHitbox => {
                if let Some(removed) = self.state.combat.hitboxes.remove(execution_id) {
                    audit!(self.state, Hitbox, AbilityTimeline, 3, Some(entity), "remove");
                    if let Some(handle) = removed.sensor_handle {
                        self.physics.remove_sensor(handle);
                    }
                }
                // Cast complete — remove the execution context now that its last
                // physics action has run and no dependent runtime object remains.
                self.state.combat.executions.remove(execution_id);
                audit!(self.state, Execution, AbilityTimeline, 3, Some(entity), "remove");
                self.emit_event(entity, EventPayload::HitboxRemoved { ability_id });
            }
            AbilityAction::CooldownStart { duration_ticks } => {
                // Insert directly into the cooldown map — no scheduled action needed.
                // Phase 8 drains entries where ready_at ≤ current_tick and emits CooldownReady.
                // Retroactive cooldown reduction: mutate the ready_at value for the entry directly.
                let ready_at = TickId(self.current_tick.0 + *duration_ticks as u64);
                self.cooldowns.insert((entity, ability_id), ready_at);
                audit!(self.state, Cooldown, AbilityTimeline, 3, Some(entity), "start");
            }
        }
    }

    // ── Phase 4: Physics integration ────────────────────────────

    fn phase_physics_step(&mut self) {
        self.physics.step(self.dt);
    }

    // ── Phase 5: Contact collection ─────────────────────────────

    fn phase_contact_collection(&mut self) {
        self.state.physics.contacts = self.physics.drain_collision_events();
        self.summary.contacts = self.state.physics.contacts.len();
    }

    // ── Phase 6: Combat resolution ──────────────────────────────

    fn phase_combat_resolution(&mut self) {
        self.resolve_hits();
    }

    /// Hit resolution chain: contacts → dedup → ability lookup → damage → threat → events.
    /// Owner: combat system. Only called from phase 6.
    fn resolve_hits(&mut self) {
        use game_core::physics_backend::ColliderKind;

        // Process sensor contacts — hitbox vs hurtbox overlaps.
        let contacts: Vec<_> = self.state.physics.contacts
            .iter()
            .filter(|c| c.started && c.is_sensor)
            .cloned()
            .collect();

        for contact in &contacts {
            // Normalise the pair: acting collider (Hitbox, future: Projectile) first.
            // Eliminates duplicate match arms — each interaction rule is stated once.
            let (acting, receiving, attacker, target) = normalize_contact_pair(
                contact.kind1, contact.entity1,
                contact.kind2, contact.entity2,
            );

            // Dispatch on interaction type. Today only (Hitbox → Hurtbox/Body) deals damage.
            // Future slots have a clean insertion point:
            //   (Hitbox(_), Shield(_))  => apply_block(...)
            //   (Hitbox(_), Hitbox(_))  => apply_clash(...)
            //   (Projectile(_), Hazard) => destroy_projectile(...)
            let (exec_id, attacker, target) = match (acting, receiving) {
                (ColliderKind::Hitbox(eid_raw), ColliderKind::Body | ColliderKind::Hurtbox) => {
                    (AbilityExecutionId(eid_raw), attacker, target)
                }
                _ => continue,
            };

            // Skip self-hits.
            if attacker == target {
                continue;
            }

            // Dedup: skip if this hitbox already hit this target.
            if !self.state.combat.hitboxes.record_hit(exec_id, target) {
                continue;
            }

            // Look up ability data for damage values — get ability_id from the hitbox record.
            // The hitbox must still exist after record_hit succeeds.
            let ability_id = match self.state.combat.hitboxes.get(exec_id) {
                Some(hb) => hb.ability_id,
                None => continue,
            };
            let ability = match self.abilities.get(ability_id) {
                Some(a) => a,
                None => continue, // Unknown ability — skip.
            };

            let base_damage = ability.base_damage;
            let damage_type = ability.damage_type;
            let threat_mult = ability.threat_multiplier;

            // Translate target EntityId → dense index.
            let target_idx = match self.state.entities.lookup(target) {
                Some(idx) => idx,
                None => continue,
            };

            // Apply damage via dense health arrays.
            let actual = self.state.combat.health.apply_damage(target_idx, base_damage, Some(attacker));
            audit!(self.state, Health, Combat, 6, Some(target), "damage");
            self.summary.damage_events += 1;

            // Generate threat on NPC targets.
            if let Some(table) = self.state.combat.threat_tables[target_idx.as_usize()].as_mut() {
                table.add_threat(attacker, actual * threat_mult);
                audit!(self.state, Threat, Combat, 6, Some(target), "add_threat");
            }

            // Emit damage event on the target.
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Damage {
                    source: attacker,
                    amount: actual,
                    damage_type,
                },
            });
            self.event_sequence += 1;

            // Emit skill-hit event on the target.
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::SkillHit {
                    skill_id: ability_id,
                    source: attacker,
                },
            });
            self.event_sequence += 1;
        }
    }

    // ── Phase 7: AI decisions ───────────────────────────────────

    fn phase_ai_decisions(&mut self) {
        use game_core::entity::lifecycle::{EntityKind, NpcAiState};

        let npcs = self.state.active_indices_of_kind(EntityKind::Npc);
        let bosses = self.state.active_indices_of_kind(EntityKind::Boss);

        for idx in npcs.iter().chain(bosses.iter()) {
            let i = idx.as_usize();
            let ai_state = match self.state.ai.npc_ai[i] {
                Some(s) => s,
                None => continue,
            };

            match ai_state {
                NpcAiState::Idle => {
                    // Check if anyone is on the threat table → transition to Combat.
                    if let Some(ref table) = self.state.combat.threat_tables[i] {
                        if table.top_threat().is_some() {
                            self.state.ai.npc_ai[i] = Some(NpcAiState::Combat);
                            audit!(self.state, Ai, AiDecisions, 7, None, "idle_to_combat");
                        }
                    }
                }
                NpcAiState::Combat => {
                    // If threat table is empty, return to Idle.
                    let has_threat = self.state.combat.threat_tables[i]
                        .as_ref()
                        .and_then(|t| t.top_threat())
                        .is_some();
                    if !has_threat {
                        self.state.ai.npc_ai[i] = Some(NpcAiState::Idle);
                        audit!(self.state, Ai, AiDecisions, 7, None, "combat_to_idle");
                    }
                    // TODO: Chase top-threat target, use abilities.
                }
                NpcAiState::Flee => {
                    // TODO: Move away from threat source.
                }
                NpcAiState::Patrol | NpcAiState::Scripted => {
                    // TODO: Follow patrol path / scripted behavior.
                }
            }
        }
    }

    // ── Health delta collection ─────────────────────────────────

    /// Snapshot current hp/max_hp for every entity that received a Damage event this tick.
    ///
    /// Must be called AFTER all combat phases (1-7) and BEFORE `phase_state_finalization`,
    /// because finalization removes dead entities from the EntityStore. Dead entities (hp=0)
    /// are intentionally included so the DB commit reflects their final health atomically
    /// with the EntityDied event in the same reducer call.
    fn collect_health_updates(&self) -> Vec<(EntityId, f32, f32)> {
        let damaged: HashSet<EntityId> = self.pending_events.iter()
            .filter_map(|e| if matches!(&e.payload, EventPayload::Damage { .. }) { Some(e.entity_id) } else { None })
            .collect();
        damaged.iter().filter_map(|&eid| {
            let idx = self.state.entities.lookup(eid)?;
            let i = idx.as_usize();
            Some((eid, self.state.combat.health.hp[i], self.state.combat.health.max_hp[i]))
        }).collect()
    }

    // ── Runtime domain snapshots (full snapshot every tick) ──────

    /// Snapshot all active buffs for persistence.
    ///
    /// Emits ALL non-Removed entities (including those with empty buff arrays) so
    /// the reducer's delete-all-then-insert reliably clears stale rows when every
    /// buff on an entity expires in the same tick.
    fn collect_buff_updates(&self) -> Vec<(EntityId, Vec<game_core::combat::status::ActiveBuff>)> {
        let mut out = Vec::new();
        for i in 0..self.state.entities.len() {
            if self.state.entities.states[i] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            let eid = self.state.entities.id_of(EntityIndex(i as u32));
            out.push((eid, self.state.status.buffs[i].clone()));
        }
        out
    }

    /// Snapshot all threat tables for persistence.
    ///
    /// Emits ALL non-Removed NPC/Boss entities (including those with an empty threat
    /// table) so the reducer reliably clears stale rows when all threat decays to zero.
    fn collect_threat_updates(&self) -> Vec<(EntityId, Vec<game_core::combat::status::ThreatEntry>)> {
        let mut out = Vec::new();
        for i in 0..self.state.entities.len() {
            if self.state.entities.states[i] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            if let Some(ref table) = self.state.combat.threat_tables[i] {
                // Emit even when entries are empty — the reducer uses the entity ID
                // to delete stale rows, so zero entries must still clear the DB.
                let eid = self.state.entities.id_of(EntityIndex(i as u32));
                out.push((eid, table.entries.clone()));
            }
        }
        out
    }

    /// Snapshot NPC AI state for all active NPCs/bosses.
    fn collect_npc_state_updates(&self) -> Vec<(EntityId, game_schema::NpcAiState, Option<EntityId>)> {
        let mut out = Vec::new();
        for i in 0..self.state.entities.len() {
            if self.state.entities.states[i] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            if let Some(ai_state) = self.state.ai.npc_ai[i] {
                let eid = self.state.entities.id_of(EntityIndex(i as u32));
                let target = self.state.combat.threat_tables[i]
                    .as_ref()
                    .and_then(|t| t.top_threat());
                out.push((eid, ai_state, target));
            }
        }
        out
    }

    // ── Phase 8: State finalization ─────────────────────────────

    /// Phase 8a: Drain the cooldown map of entries that have become ready this tick.
    ///
    /// An entry is ready when `ready_at ≤ current_tick`. At that point the ability is
    /// castable again; we emit a `CooldownReady` event and remove the entry so the map
    /// only holds in-flight cooldowns. CooldownReady uniqueness is guaranteed by the
    /// HashMap key — duplicate entries for the same (entity, ability) are impossible.
    fn phase_expire_cooldowns(&mut self) {
        let current = self.current_tick;
        // Collect first so the borrow on `self.cooldowns` ends before `emit_event` borrows `self`.
        let expired: Vec<(EntityId, u32)> = self
            .cooldowns
            .iter()
            .filter(|&(_, &ready_at)| ready_at <= current)
            .map(|(&k, _)| k)
            .collect();
        for (entity, ability_id) in expired {
            self.cooldowns.remove(&(entity, ability_id));
            audit!(self.state, Cooldown, CooldownTracker, 8, Some(entity), "expire");
            self.emit_event(entity, EventPayload::CooldownReady { ability_id });
        }
    }

    fn phase_state_finalization(&mut self) -> Vec<(EntityId, game_schema::EntityState)> {
        use game_core::entity::lifecycle::EntityState;

        let mut state_updates: Vec<(EntityId, game_schema::EntityState)> = Vec::new();

        // Check for newly dead entities and mark them for despawn.
        let dead_indices: Vec<EntityIndex> = (0..self.state.entities.len())
            .map(|i| EntityIndex(i as u32))
            .filter(|&idx| {
                self.state.entities.is_active(idx) && self.state.combat.health.is_dead(idx)
            })
            .collect();

        for idx in dead_indices {
            let id = self.state.entities.id_of(idx);
            self.state.entities.mark_despawn(idx);
            state_updates.push((id, EntityState::DespawnPending));
            audit!(self.state, Lifecycle, Lifecycle, 8, Some(id), "death_despawn");
            self.summary.deaths += 1;
            // Determine killer: prefer last_damage_source, fall back to top-threat.
            let killer = self.state.combat.health.last_damage_source[idx.as_usize()]
                .or_else(|| {
                    self.state.combat.threat_tables[idx.as_usize()]
                        .as_ref()
                        .and_then(|t| t.top_threat())
                });
            self.emit_event(id, EventPayload::EntityDied { killer });
        }

        // Clean up DespawnPending entities.
        let despawning = self.state.despawn_pending();
        for id in despawning {
            self.summary.despawns += 1;
            self.emit_event(id, EventPayload::EntityDespawned);
            // Hard teardown for this entity from all runtime stores and the physics backend.
            // Centralised here so external removal paths can call the same behaviour.
            self.force_remove_entity(id);
            audit!(self.state, Lifecycle, Lifecycle, 8, Some(id), "remove");
            state_updates.push((id, EntityState::Removed));
        }

        // Expire buffs.
        let expired_buffs = self.state.expire_buffs(self.current_tick);
        for (entity_id, buff_id) in expired_buffs {
            audit!(self.state, Buff, StatusEffects, 8, Some(entity_id), "expire");
            self.emit_event(entity_id, EventPayload::BuffExpired { buff_id });
        }

        // Decay threat tables multiplicatively — all values scale by factor each tick.
        // Multiplicative decay prevents runaway target switching when entries are near-equal,
        // and allows large accumulated threat to bleed down gracefully.
        const THREAT_DECAY_FACTOR: f32 = 0.98;
        for table in self.state.combat.threat_tables.iter_mut().flatten() {
            table.decay(THREAT_DECAY_FACTOR);
        }

        // Activate any Spawning entities (they've had one tick to set up physics).
        let spawning: Vec<EntityIndex> = (0..self.state.entities.len())
            .filter(|&i| self.state.entities.states[i] == EntityState::Spawning)
            .map(|i| EntityIndex(i as u32))
            .collect();
        for idx in spawning {
            let id = self.state.entities.id_of(idx);
            self.state.entities.activate(idx);
            audit!(self.state, Lifecycle, Lifecycle, 8, Some(id), "activate");
            state_updates.push((id, EntityState::Active));
        }

        // Dedup: if an entity transitions DespawnPending → Removed within the same tick
        // (the normal combat-death path), drop the intermediate DespawnPending entry.
        // Sending both causes two entity.on_update callbacks on the coordinator and two
        // DB writes, but the final state is the same. Pruning here reduces churn.
        {
            let removed_ids: HashSet<EntityId> = state_updates
                .iter()
                .filter(|(_, s)| *s == game_schema::EntityState::Removed)
                .map(|(id, _)| *id)
                .collect();
            state_updates.retain(|(id, s)| {
                !(*s == game_schema::EntityState::DespawnPending && removed_ids.contains(id))
            });
        }

        state_updates
    }

    // ── Event helpers ───────────────────────────────────────────

    fn emit_event(&mut self, entity_id: EntityId, payload: EventPayload) {
        self.pending_events.push(SimEvent {
            tick_id: self.current_tick,
            event_sequence: self.event_sequence,
            entity_id,
            payload,
        });
        self.event_sequence += 1;
    }
}

/// Map a game-level SkillShape to a physics-level SensorShape.
/// Sizes here are gameplay defaults — move into AbilityData when balancing requires per-ability tuning.
fn skill_shape_to_sensor(shape: SkillShape) -> SensorShape {
    match shape {
        SkillShape::Sphere       => SensorShape::Sphere { radius: 2.0 },
        SkillShape::Cone         => SensorShape::Capsule { half_height: 1.5, radius: 1.0 },
        SkillShape::CapsuleSweep => SensorShape::Capsule { half_height: 1.0, radius: 0.75 },
        SkillShape::Projectile   => SensorShape::Sphere { radius: 0.3 },
        SkillShape::LineSweep    => SensorShape::Capsule { half_height: 3.0, radius: 0.5 },
        SkillShape::HazardZone   => SensorShape::Sphere { radius: 5.0 },
    }
}

/// Returns true if this collider kind is the **initiating** side of an interaction —
/// i.e. the thing that acts on something else, rather than receiving the action.
///
/// This is the single extension point for normalization precedence.
/// When `Projectile`, `BlockCone`, or `Aura` arrive, add them here.
/// `normalize_contact_pair` and `resolve_hits` need no changes.
fn is_acting_collider(kind: ColliderKind) -> bool {
    use game_core::physics_backend::ColliderKind::*;
    matches!(kind, Hitbox(_))
    // future: | Projectile(_) | BlockCone(_) | Aura(_)
}

/// Normalise a contact pair so that the **acting** collider is always in position 0.
///
/// Reduces `(Hitbox, Hurtbox)` and `(Hurtbox, Hitbox)` to the same canonical form,
/// eliminating duplicated match arms in `resolve_hits`.  When more collider types
/// are added (6–8 variants), every interaction rule is expressed once, not twice.
///
/// To extend: add new acting kinds to `is_acting_collider` — do not touch this function.
///
/// Returns `(acting_kind, receiving_kind, acting_entity, receiving_entity)`.
fn normalize_contact_pair(
    kind1: ColliderKind,
    entity1: EntityId,
    kind2: ColliderKind,
    entity2: EntityId,
) -> (ColliderKind, ColliderKind, EntityId, EntityId) {
    if is_acting_collider(kind1) {
        (kind1, kind2, entity1, entity2)
    } else if is_acting_collider(kind2) {
        (kind2, kind1, entity2, entity1)
    } else {
        (kind1, kind2, entity1, entity2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_core::combat::skill::{
        AbilityAction, AbilityData, AbilityRegistry, AbilityTimeline, ScheduledAbilityAction, SkillShape,
    };
    use game_core::entity::lifecycle::EntityKind;
    use game_core::physics_backend::ColliderKind;
    use game_protocol::event::{DamageType, EventPayload};
    use game_protocol::intent::{PlayerIntent, IntentAction};
    use crate::physics::rapier_world::PhysicsWorld;
    use crate::physics::collision_groups;

    fn make_pipeline(abilities: AbilityRegistry) -> TickPipeline {
        let physics = Box::new(PhysicsWorld::new(1.0 / 20.0));
        TickPipeline::new(TickId(0), physics, 1.0 / 20.0, abilities)
    }

    fn setup_ability_registry() -> AbilityRegistry {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 1,
            name: "Slash".to_string(),
            base_damage: 25.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::CapsuleSweep,
            threat_multiplier: 1.0,
        });
        // Timeline: hitbox + cooldown fire immediately (offset 0), damage frame on next tick.
        reg.register_timeline(AbilityTimeline {
            ability_id: 1,
            actions: vec![
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::Sphere,
                    offset: game_schema::Vec3f { x: 0.0, y: 0.0, z: 0.0 },
                }},
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::CooldownStart { duration_ticks: 20 } },
                ScheduledAbilityAction { tick_offset: 1, action: AbilityAction::ApplyDamageFrame },
                ScheduledAbilityAction { tick_offset: 2, action: AbilityAction::RemoveHitbox },
            ],
        });
        reg
    }

    #[test]
    fn hitbox_damages_target_through_pipeline() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker_id = EntityId(1);
        let target_id = EntityId(2);

        // Spawn entities in SimState.
        pipeline.state.spawn_entity(attacker_id, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target_id, EntityKind::Npc, TickId(0), 100.0);

        // Create physics bodies at the same position.
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                attacker_id, pos, 0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                target_id, pos, 0.5, 0.3, 1.0,
                collision_groups::npc_body_groups(),
            );
        }

        // Tick 0: warm-up — activates Spawning → Active, no hitbox yet.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(attacker_id));
        assert!(pipeline.state.is_active(target_id));

        // NOW add the hitbox sensor (after entities are Active).
        // Use spawn_sensor (the trait method) and store the returned handle on
        // ActiveHitbox so lifecycle ownership matches the production path.
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.state.combat.hitboxes.spawn_armed(exec_id, attacker_id, 1, TickId(1), SkillShape::Sphere, Vec3f { x: 0.0, y: 0.0, z: 0.0 }, sensor);

        // Tick 1: physics step detects hitbox↔hurtbox overlap,
        // combat resolution applies damage.
        let result1 = pipeline.run_tick(&[]);

        // Verify damage was applied to target.
        let target_hp = pipeline.state.hp_of(target_id).unwrap();
        assert!(
            (target_hp - 75.0).abs() < 0.01,
            "Expected 75 hp after 25 damage, got {}",
            target_hp
        );

        // Verify last_damage_source is the attacker.
        assert_eq!(
            pipeline.state.last_damage_source_of(target_id),
            Some(attacker_id),
        );

        // Verify threat was generated on the NPC target.
        let threat = pipeline.state.threat_table_of(target_id)
            .and_then(|t| t.top_threat());
        assert_eq!(threat, Some(attacker_id), "Attacker should be top threat");

        // Verify Damage event was emitted.
        let damage_events: Vec<_> = result1.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::Damage { source, .. } if *source == attacker_id)
        }).collect();
        assert!(
            !damage_events.is_empty(),
            "Should have emitted Damage event, events: {:?}",
            result1.events.iter().map(|e| format!("{:?}", e.payload)).collect::<Vec<_>>()
        );

        // Verify SkillHit event was emitted.
        let skill_events: Vec<_> = result1.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::SkillHit { skill_id: 1, .. })
        }).collect();
        assert!(!skill_events.is_empty(), "Should have emitted SkillHit event");

        // Verify dedup — running another tick should NOT produce duplicate damage
        // from the same hitbox instance.
        let result2 = pipeline.run_tick(&[]);
        let damage_events2: Vec<_> = result2.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::Damage { .. })
        }).collect();
        assert!(
            damage_events2.is_empty(),
            "Hitbox should not damage same target twice",
        );
    }

    #[test]
    fn damage_causes_death_and_despawn() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "OneShot".to_string(),
            base_damage: 999.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let victim = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(victim, EntityKind::Npc, TickId(0), 50.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(victim, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up — activates entities.
        pipeline.run_tick(&[]);

        // Add lethal hitbox after entities are Active.
        // Use spawn_sensor and attach the returned handle to ActiveHitbox so
        // hitbox lifecycle ownership matches production.
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.state.combat.hitboxes.spawn_armed(exec_id, attacker, 10, TickId(1), SkillShape::Sphere, Vec3f { x: 0.0, y: 0.0, z: 0.0 }, sensor);

        // Tick 1: damage kills victim in same tick (entities already Active).
        let result1 = pipeline.run_tick(&[]);

        // Victim should have died.
        let died_events: Vec<_> = result1.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::EntityDied { .. })
        }).collect();
        assert!(!died_events.is_empty(), "Victim should have died");

        // Killer should be identified via last_damage_source.
        if let EventPayload::EntityDied { killer } = &died_events[0].payload {
            assert_eq!(*killer, Some(attacker));
        }

        // Victim should have been despawned and removed from state.
        assert!(!pipeline.state.entities.contains(victim));
    }

    #[test]
    fn use_ability_intent_spawns_hitbox_and_cooldown_prevents_double_cast() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 100.0);

        // Place both entities at the same position so the hitbox overlaps the hurtbox.
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up — entities transition Spawning → Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(attacker), "attacker should be Active after tick 0");
        assert!(pipeline.state.is_active(target), "target should be Active after tick 0");

        // Tick 1: send UseAbility intent.
        // Phase 2 creates AbilityExecutionContext + schedules timeline actions.
        // Phase 3 fires SpawnHitbox (offset 0) — registers the hitbox logically only,
        // no Rapier sensor yet. CooldownStart also queued at offset 0.
        // The Rapier sensor will be spawned in Phase 3 of tick 2 (ApplyDamageFrame, offset 1).
        let cast_intent = PlayerIntent {
            entity_id: attacker,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let result1 = pipeline.run_tick(&[cast_intent]);

        // Hitbox should have been spawned this tick (offset 0 → fires in phase 3 of tick 1).
        let hitbox_spawned = result1.events.iter().any(|e| {
            matches!(&e.payload, EventPayload::HitboxSpawned { ability_id: 1 })
        });
        assert!(hitbox_spawned, "HitboxSpawned event should fire on the cast tick");

        // Verify that the cooldown is now active — is_on_cooldown is a private helper,
        // so we confirm indirectly: sending the same ability again should NOT spawn a
        // second hitbox (the intent is silently dropped by the cooldown gate).
        let double_cast_intent = PlayerIntent {
            entity_id: attacker,
            sequence_id: 2,
            target_tick: TickId(2),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let result2 = pipeline.run_tick(&[double_cast_intent]);

        let second_hitbox = result2.events.iter().any(|e| {
            matches!(&e.payload, EventPayload::HitboxSpawned { ability_id: 1 })
        });
        assert!(!second_hitbox, "Cooldown should prevent a second hitbox from spawning on tick 2");
    }

    #[test]
    fn health_updates_populated_after_damage() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker_id = EntityId(1);
        let target_id = EntityId(2);

        pipeline.state.spawn_entity(attacker_id, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target_id, EntityKind::Npc, TickId(0), 100.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker_id, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target_id, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up — no damage, health_updates must be empty.
        let result0 = pipeline.run_tick(&[]);
        assert!(result0.health_updates.is_empty(), "No damage this tick — health_updates should be empty");

        // Add hitbox sensor at the same position as target so they overlap.
        // Use spawn_sensor and attach the returned handle to ActiveHitbox so
        // hitbox lifecycle ownership matches production.
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.state.combat.hitboxes.spawn_armed(exec_id, attacker_id, 1, TickId(1), SkillShape::Sphere, Vec3f { x: 0.0, y: 0.0, z: 0.0 }, sensor);

        // Tick 1: hitbox overlaps hurtbox — damage fires, health_updates should be populated.
        let result1 = pipeline.run_tick(&[]);

        let target_update = result1.health_updates.iter().find(|(eid, _, _)| *eid == target_id);
        assert!(target_update.is_some(), "Target entity should have a HealthUpdate entry after being hit");
        let (_, hp, max_hp) = target_update.unwrap();
        assert!((*hp - 75.0).abs() < 0.01, "Expected hp=75 after 25 damage, got {}", hp);
        assert!((*max_hp - 100.0).abs() < 0.01, "Expected max_hp=100, got {}", max_hp);

        // Attacker took no damage — should have no health update.
        let attacker_update = result1.health_updates.iter().find(|(eid, _, _)| *eid == attacker_id);
        assert!(attacker_update.is_none(), "Attacker took no damage — should have no HealthUpdate");
    }

    #[test]
    fn health_updates_include_killed_entity_with_zero_hp() {
        // Use a one-shot ability so the victim dies in the same tick it is hit.
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "OneShot".to_string(),
            base_damage: 999.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let victim = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(victim, EntityKind::Npc, TickId(0), 50.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(victim, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        pipeline.run_tick(&[]); // warm-up — entities go Active

        // Use spawn_sensor and attach the returned handle to ActiveHitbox so
        // hitbox lifecycle ownership matches production.
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.state.combat.hitboxes.spawn_armed(exec_id, attacker, 10, TickId(1), SkillShape::Sphere, Vec3f { x: 0.0, y: 0.0, z: 0.0 }, sensor);

        let result = pipeline.run_tick(&[]);

        // The victim was removed from the EntityStore in phase_state_finalization
        // (same tick as the hit), but health_updates must still carry hp=0 for it
        // so the DB row is updated atomically with the EntityDied event.
        let victim_update = result.health_updates.iter().find(|(eid, _, _)| *eid == victim);
        assert!(victim_update.is_some(), "Killed entity must appear in health_updates (hp=0) so DB reflects death");
        let (_, hp, _) = victim_update.unwrap();
        assert_eq!(*hp, 0.0, "Killed entity hp must be 0 in health update");
    }

    /// SpawnHitbox declares the hitbox logically but must NOT deal damage before the
    /// ApplyDamageFrame action materialises the Rapier sensor on the damage-frame tick.
    ///
    /// Uses the full live-server path (kinematic bodies, UseAbility intent) so this
    /// test directly validates the timeline semantics and physics event model together:
    ///   SpawnHitbox (tick 1) = declare intent  →  no collision, no damage
    ///   ApplyDamageFrame (tick 2) = materialise sensor  →  Rapier fires contact → damage
    #[test]
    fn hitbox_does_not_damage_before_damage_frame() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        let npc = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            player,
            EntityKind::Player,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc,
            EntityKind::Npc,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up — Spawning → Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player));
        assert!(pipeline.state.is_active(npc));

        // Tick 1: UseAbility → Phase 2 schedules Slash timeline → Phase 3 fires
        // SpawnHitbox (offset 0): logical hitbox only, no Rapier sensor inserted.
        // No sensor → no collision event → no damage this tick.
        let slash_intent = PlayerIntent {
            entity_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let result1 = pipeline.run_tick(&[slash_intent]);

        // HitboxSpawned event must fire (logical registration happened).
        assert!(
            result1.events.iter().any(|e| matches!(&e.payload, EventPayload::HitboxSpawned { ability_id: 1 })),
            "HitboxSpawned must emit on cast tick (logical declaration)"
        );
        // But no damage — the Rapier sensor is not yet live.
        assert!(
            !result1.events.iter().any(|e| matches!(&e.payload, EventPayload::Damage { .. })),
            "No damage must occur on the SpawnHitbox tick — sensor is not yet materialised"
        );
        assert_eq!(
            pipeline.state.hp_of(npc).unwrap(), 100.0,
            "NPC hp must be unchanged on the cast/spawn tick"
        );
    }

    /// Full-path test: UseAbility → SpawnHitbox (logical, tick 1) →
    /// ApplyDamageFrame (sensor materialised, tick 2) → damage on the damage-frame tick.
    ///
    /// This is the canonical test encoding the intended timeline semantics:
    ///   SpawnHitbox  = declare hitbox intent (no collision yet)
    ///   ApplyDamageFrame = bring sensor live → Rapier fires contact → Phase 6 damages
    ///
    /// Uses `spawn_character_body` (`add_kinematic_capsule`) for both entities,
    /// mirroring the live server path and verifying KINEMATIC_KINEMATIC contacts.
    #[test]
    fn hitbox_damages_on_damage_frame_tick() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        let npc = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            player,
            EntityKind::Player,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc,
            EntityKind::Npc,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        // Tick 1: UseAbility → SpawnHitbox registered logically. No damage.
        let slash_intent = PlayerIntent {
            entity_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[slash_intent]);

        // Tick 2: ApplyDamageFrame (offset 1 in the Slash timeline) → Phase 3 arms the
        // hitbox and inserts the Rapier sensor → Phase 4 physics step detects the
        // overlap → Phase 5 drains CollisionEvent::started → Phase 6 resolves damage.
        let result2 = pipeline.run_tick(&[]);

        let npc_hp = pipeline.state.hp_of(npc).unwrap();
        assert!(
            npc_hp < 100.0,
            "NPC must take damage on the ApplyDamageFrame tick; hp={npc_hp}"
        );

        let has_damage = result2.events.iter().any(|e| {
            matches!(&e.payload, EventPayload::Damage { source, .. } if *source == player)
        });
        assert!(has_damage, "Damage event must be emitted on the ApplyDamageFrame tick");
    }

    #[test]
    fn face_to_intent_rotates_kinematic_body() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);

        // Spawn via the live server path so there's a kinematic body in the physics world.
        pipeline.spawn_entity_from_snapshot(
            player,
            EntityKind::Player,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up — entity transitions Spawning → Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player), "player must be Active after warm-up");

        // Snapshot initial rotation (should be identity: w=1).
        let initial = pipeline.physics.get_transform(player).unwrap();

        // Tick 1: FaceTo intent pointing right (+X direction = 90° yaw from +Z forward).
        let face_right = PlayerIntent {
            entity_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::FaceTo(game_schema::MoveDir {
                dir_x: 1.0,
                dir_y: 0.0,
                dir_z: 0.0,
            }),
        };
        pipeline.run_tick(&[face_right]);

        let after = pipeline.physics.get_transform(player).unwrap();

        // Rotation must have changed from identity (w went from 1.0 to ~0.707).
        assert!(
            (after.rotation.w - initial.rotation.w).abs() > 0.01,
            "FaceTo must rotate the body: before w={}, after w={}",
            initial.rotation.w, after.rotation.w,
        );

        // For a pure right (+X) direction: yaw = atan2(1, 0) = π/2.
        // Resulting quaternion: (x=0, y=sin(π/4)≈0.707, z=0, w=cos(π/4)≈0.707).
        assert!(
            (after.rotation.y - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "FaceTo right: expected y≈0.707, got y={}",
            after.rotation.y,
        );
        assert!(
            (after.rotation.w - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "FaceTo right: expected w≈0.707, got w={}",
            after.rotation.w,
        );

        // Position must be preserved.
        assert!(
            (after.position.y - 1.0).abs() < 0.1,
            "FaceTo must not change entity position; y={}",
            after.position.y,
        );

        // Zero-length direction must be a no-op (no panic, rotation unchanged).
        let face_zero = PlayerIntent {
            entity_id: player,
            sequence_id: 2,
            target_tick: TickId(2),
            client_time_ms: 0,
            action: IntentAction::FaceTo(game_schema::MoveDir {
                dir_x: 0.0,
                dir_y: 0.0,
                dir_z: 0.0,
            }),
        };
        let result_zero = pipeline.run_tick(&[face_zero]);
        // No crash is sufficient; verify the tick still produced output.
        assert_eq!(result_zero.tick_id.0, 2, "Pipeline must not stall on zero-direction FaceTo");
    }

    /// Regression test for the same-tick double-cast exploit (BUG 2).
    ///
    /// Two `UseAbility` intents for the same `(entity, ability)` pair passed to a single
    /// `run_tick` call must only schedule the ability once. Without the `cast_this_tick`
    /// HashSet guard, both intents pass `is_on_cooldown` because `CooldownExpire` isn't
    /// inserted until Phase 3 — after Phase 2 has already processed both.
    #[test]
    fn same_tick_double_cast_is_rejected() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            attacker,
            EntityKind::Player,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            target,
            EntityKind::Npc,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(attacker));
        assert!(pipeline.state.is_active(target));

        // Tick 1: submit two identical UseAbility intents in the same tick slice.
        // They differ only in sequence_id (as a client might if it sent two queued intents).
        let cast_a = PlayerIntent {
            entity_id: attacker,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let cast_b = PlayerIntent {
            entity_id: attacker,
            sequence_id: 2,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let result = pipeline.run_tick(&[cast_a, cast_b]);

        // Exactly one HitboxSpawned event must fire — not two.
        let hitbox_spawned_count = result.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::HitboxSpawned { ability_id: 1 })
        }).count();
        assert_eq!(
            hitbox_spawned_count, 1,
            "Same-tick double-cast must produce exactly one HitboxSpawned event, got {hitbox_spawned_count}"
        );
    }

    /// Regression test for the state-update dedup patch (BUG 3).
    ///
    /// When an entity dies and is despawned in the same tick (the normal combat-death path),
    /// `entity_state_updates` must contain only the final `Removed` entry — not both
    /// `DespawnPending` and `Removed`. Sending both causes two `entity.on_update` callbacks
    /// in the coordinator and two DB writes for the same field.
    #[test]
    fn death_in_single_tick_emits_only_removed_state_update() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "OneShot".to_string(),
            base_damage: 999.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let victim = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            attacker,
            EntityKind::Player,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            victim,
            EntityKind::Npc,
            TickId(0),
            50.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up — both entities become Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(attacker));
        assert!(pipeline.state.is_active(victim));

        // Add a lethal hitbox sensor directly (bypasses the intent path so the one-shot
        // hits in the very next tick without any cooldown scheduling complexity).
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has a body");
        pipeline.state.combat.hitboxes.spawn_armed(exec_id, attacker, 10, TickId(1), SkillShape::Sphere, Vec3f { x: 0.0, y: 0.0, z: 0.0 }, sensor);

        // Tick 1: one-shot kills victim.
        let result = pipeline.run_tick(&[]);

        // The victim must be gone from SimState.
        assert!(
            !pipeline.state.entities.contains(victim),
            "Victim should be fully removed from SimState after one-shot kill"
        );

        // entity_state_updates for the victim must contain ONLY Removed, not DespawnPending.
        let victim_updates: Vec<_> = result.entity_state_updates.iter()
            .filter(|(id, _)| *id == victim)
            .collect();

        assert_eq!(
            victim_updates.len(), 1,
            "Victim should appear exactly once in entity_state_updates, got: {:?}", victim_updates
        );
        assert_eq!(
            victim_updates[0].1,
            game_schema::EntityState::Removed,
            "Single victim state update must be Removed, not DespawnPending"
        );
    }

    /// Regression test for the ExecutionContext leak fix (#27).
    ///
    /// An ability whose timeline contains only `CooldownStart` (no hitbox — no
    /// `SpawnHitbox`, no `ApplyDamageFrame`, no `RemoveHitbox`) previously leaked its
    /// `AbilityExecutionContext` for the lifetime of the session because the context
    /// was only removed in `execute_ability_action` on the `RemoveHitbox` arm.
    ///
    /// After the fix, `phase_skill_scheduling` runs a culling pass at the end of each
    /// tick: any execution whose `execution_is_alive` predicate returns false is
    /// removed from `executions`. A cooldown-only cast has no pending scheduled actions
    /// and no live hitbox immediately after its `CooldownStart` fires — so it must be
    /// culled in the same tick it was cast.
    #[test]
    fn cooldown_only_ability_does_not_leak_execution_context() {
        let mut reg = AbilityRegistry::new();
        // Ability with only a CooldownStart action — no hitbox at all.
        reg.register(AbilityData {
            ability_id: 99,
            name: "Taunt".to_string(),
            base_damage: 0.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 0.0,
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 99,
            actions: vec![
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::CooldownStart { duration_ticks: 10 },
                },
            ],
        });

        let mut pipeline = make_pipeline(reg);
        let player = EntityId(1);

        pipeline.spawn_entity_from_snapshot(
            player,
            EntityKind::Player,
            TickId(0),
            100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up — entity transitions Spawning → Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player));
        assert!(
            pipeline.state.combat.executions.is_empty(),
            "No casts yet — executions must be empty after warm-up"
        );

        // Tick 1: cast the cooldown-only ability.
        // Phase 2 creates an AbilityExecutionContext and schedules CooldownStart (offset 0).
        // Phase 3 fires CooldownStart — inserts into cooldowns map. No hitbox is touched.
        // Culling pass at end of Phase 3: execution_is_alive() returns false (no scheduled
        // actions remain, no hitbox exists) → context is removed.
        let cast_intent = PlayerIntent {
            entity_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 99,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[cast_intent]);

        assert!(
            pipeline.state.combat.executions.is_empty(),
            "Cooldown-only ability must not leak its AbilityExecutionContext — \
             executions should be empty after the cast tick"
        );

        // Cooldown must still be active (the map entry outlives the context).
        assert!(
            pipeline.is_on_cooldown_pub(player, 99),
            "Cooldown must be registered even though the execution context was culled"
        );
    }

    /// Regression test for the Interaction Resolution Layer (#29).
    ///
    /// Two hitbox sensors at the same position produce a `(Hitbox, Hitbox)` contact.
    /// Before the explicit dispatch table in `resolve_hits`, this pair fell through an
    /// implicit path that could produce unintended behaviour in future. After the
    /// refactor, only `(Hitbox, Body|Hurtbox)` deals damage; `(Hitbox, Hitbox)` falls
    /// through to `_ => continue` and must produce no damage events.
    ///
    /// Unit tests for the contact-pair normalizer and acting-collider predicate (#29).
    ///
    /// Why unit not integration:
    ///   The `SKILL_HITBOX` collision group filter does not include `SKILL_HITBOX`, so
    ///   Rapier's broadphase will never generate a `(Hitbox, Hitbox)` contact pair in
    ///   the physics simulation. The collision-group tests in
    ///   `physics::collision_groups::tests` already verify this at the layer level.
    ///
    ///   The correct way to pin down the dispatch logic for pairs that can't naturally
    ///   occur in physics is to test `is_acting_collider` and `normalize_contact_pair`
    ///   directly — both are visible here via `use super::*`.
    ///
    /// What is covered:
    ///   1. `is_acting_collider` classification for every relevant variant
    ///   2. `normalize_contact_pair` reorders `(Hurtbox, Hitbox)` to `(Hitbox, Hurtbox)`
    ///   3. `(Hitbox, Hitbox)` normalizes with hitbox first — and the first element does
    ///      NOT match `Body | Hurtbox` in the dispatch arm, proving the `_ => continue`
    ///      path for intra-role contacts is explicit, not accidental.
    #[test]
    fn contact_normalization_and_dispatch_classification() {
        let e1 = EntityId(1);
        let e2 = EntityId(2);

        // is_acting_collider
        assert!( is_acting_collider(ColliderKind::Hitbox(99)));
        assert!(!is_acting_collider(ColliderKind::Hurtbox));
        assert!(!is_acting_collider(ColliderKind::Body));
        assert!(!is_acting_collider(ColliderKind::Aura(1)));
        assert!(!is_acting_collider(ColliderKind::BlockCone(1)));
        assert!(!is_acting_collider(ColliderKind::Hazard(1)));

        // (Hitbox, Hurtbox) — already in canonical order
        let (a, b, ea, eb) = normalize_contact_pair(
            ColliderKind::Hitbox(1), e1, ColliderKind::Hurtbox, e2,
        );
        assert!(matches!(a, ColliderKind::Hitbox(1)));
        assert_eq!(b, ColliderKind::Hurtbox);
        assert_eq!(ea, e1);
        assert_eq!(eb, e2);

        // (Hurtbox, Hitbox) — reversed; must be swapped to canonical
        let (a, b, ea, eb) = normalize_contact_pair(
            ColliderKind::Hurtbox, e1, ColliderKind::Hitbox(1), e2,
        );
        assert!(matches!(a, ColliderKind::Hitbox(1)));
        assert_eq!(b, ColliderKind::Hurtbox);
        assert_eq!(ea, e2); // entity that owned the Hitbox
        assert_eq!(eb, e1);

        // (Hitbox, Hitbox) — intra-role: first stays first (both acting; no swap)
        let (a, b, ea, eb) = normalize_contact_pair(
            ColliderKind::Hitbox(1), e1, ColliderKind::Hitbox(2), e2,
        );
        assert!(matches!(a, ColliderKind::Hitbox(1)));
        assert!(matches!(b, ColliderKind::Hitbox(2)));
        assert_eq!(ea, e1);
        assert_eq!(eb, e2);

        // Verify the receiving side of a (Hitbox, Hitbox) pair does NOT match the
        // damage-dispatch arm `Body | Hurtbox`. This is the explicit proof that the
        // dispatch table falls through to `_ => continue` for intra-role contacts.
        let does_match_damage_arm = matches!(b, ColliderKind::Body | ColliderKind::Hurtbox);
        assert!(
            !does_match_damage_arm,
            "(Hitbox, Hitbox): receiving side must not match the damage dispatch arm"
        );

        // (Body, Hurtbox) — neither side is acting: unchanged order, both pass through dispatch
        let (a, b, ea, eb) = normalize_contact_pair(
            ColliderKind::Body, e1, ColliderKind::Hurtbox, e2,
        );
        assert_eq!(a, ColliderKind::Body);
        assert_eq!(b, ColliderKind::Hurtbox);
        assert_eq!(ea, e1);
        assert_eq!(eb, e2);
    }

    #[test]
    fn audit_counters_track_combat_lifecycle() {
        // Run the standard combat lifecycle: spawn → activate → cast → damage → death → despawn.
        // Verify that the audit counters reflect the expected mutation pattern.
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 50.0);

        // Create physics bodies at the same position for collision.
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warmup (Spawning → Active)
        pipeline.run_tick(&[]);
        assert!(pipeline.state.audit.lifecycle_writes >= 2, "two entities should activate");

        // Tick 1: cast slash (Phase 2: execution insert, Phase 3: hitbox spawn + cooldown start)
        let cast = PlayerIntent {
            entity_id: attacker,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[cast]);
        assert!(pipeline.state.audit.execution_writes >= 1, "cast should register execution");
        assert!(pipeline.state.audit.hitbox_writes >= 1, "hitbox should spawn");
        assert!(pipeline.state.audit.cooldown_writes >= 1, "cooldown should start");

        // Tick 2: damage frame → damage + death + despawn
        pipeline.run_tick(&[]);
        // Hitbox arm fires on this tick (ApplyDamageFrame), so hitbox_writes should be > 0.
        assert!(pipeline.state.audit.hitbox_writes >= 1, "hitbox should arm on damage frame");
        assert!(pipeline.state.audit.health_writes >= 1, "damage should apply");

        // Verify summary_line produces valid output
        let summary = pipeline.state.audit.summary_line();
        assert!(summary.contains("health="), "summary should contain health counter");
        assert!(summary.contains("lifecycle="), "summary should contain lifecycle counter");
    }

    #[test]
    fn audit_ownership_violations_are_detectable() {
        // Enable detailed recording and verify that every mutation record
        // has the expected (domain, subsystem, phase) triple.
        // This is the test CI can run to detect ownership violations.

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);
        pipeline.state.audit.record_details = true;

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 50.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Run 3 ticks: warmup, cast, damage
        pipeline.run_tick(&[]);
        let cast = PlayerIntent {
            entity_id: attacker,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[cast]);
        pipeline.state.audit.record_details = true; // reset cleared it, re-enable
        pipeline.run_tick(&[]);

        // Validate ownership rules on the detailed records from the last tick.
        // Now uses the shared enforcement function — same rules as the debug_assert!
        // in MutationAudit::record().
        let violations: Vec<String> = pipeline.state.audit.records.iter().filter_map(|r| {
            if game_core::sim_state::is_ownership_allowed(r.domain, r.subsystem, r.phase) {
                None
            } else {
                Some(format!("{:?} written by {:?} in phase {} ({})", r.domain, r.subsystem, r.phase, r.detail))
            }
        }).collect();

        assert!(violations.is_empty(), "Ownership violations detected:\n{}", violations.join("\n"));
    }
}
