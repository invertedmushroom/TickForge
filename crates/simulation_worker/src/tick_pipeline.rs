use std::collections::HashMap;
use game_protocol::event::{EventPayload, SimEvent};
use game_protocol::intent::{IntentAction, MoveDir, PlayerIntent};
use game_protocol::tick::TickId;
use game_protocol::entity_id::EntityId;
use game_protocol::types::Transform;
use game_core::physics_backend::{ColliderKind, PhysicsBackend, SensorShape};
use game_core::combat::skill::{
    AbilityAction, AbilityTimeline, AbilityRegistry, ScheduledAction, ScheduledActionType, SkillShape,
};
use game_core::entity::entity_index::EntityIndex;
use game_core::sim_state::SimState;
use game_protocol::types::Vec3f;
use game_schema::EntityKind;

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
    /// Opaque sensor handles for active hitbox colliders: (entity, ability_id) → physics handle.
    sensor_handles: HashMap<(EntityId, u32), u64>,
    /// Running stats for the current tick — incremented inline, returned in TickResult.
    summary: TickSummary,
}

impl TickPipeline {
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
            sensor_handles: HashMap::new(),
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

    /// Downcast the physics backend to a concrete type.
    /// Returns `None` if the backend is not of type `T`.
    pub fn physics_as<T: 'static>(&mut self) -> Option<&mut T> {
        self.physics.as_any_mut().downcast_mut::<T>()
    }

    /// Schedule all actions from an ability timeline, starting at the given tick.
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
    ) {
        for scheduled in &timeline.actions {
            let tick_id = TickId(start_tick.0 + scheduled.tick_offset as u64);
            debug_assert!(
                tick_id >= self.current_tick,
                "scheduled action for past tick {tick_id} (current: {})",
                self.current_tick
            );
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
    /// Same rules as `schedule_ability` — future ticks only, not idempotent.
    pub fn schedule_action(&mut self, action: ScheduledAction) {
        debug_assert!(
            action.tick_id >= self.current_tick,
            "scheduled action for past tick {} (current: {})",
            action.tick_id, self.current_tick
        );
        self.scheduled_actions.push(action);
        // Maintain sort — binary search for insertion would be faster, but
        // the queue is small enough that a full sort is fine at 20 Hz.
        self.scheduled_actions.sort_by_key(|a| a.tick_id);
    }

    /// Returns true if a CooldownExpire action for this entity+ability is pending.
    /// Called by phase 2 to reject UseAbility intents for abilities still recovering.
    fn is_on_cooldown(&self, entity: EntityId, ability_id: u32) -> bool {
        self.scheduled_actions.iter().any(|a| {
            a.entity == entity
                && matches!(&a.action_type, ScheduledActionType::CooldownExpire { ability_id: id } if *id == ability_id)
        })
    }

    /// Execute one full simulation tick, returning results for commit.
    pub fn run_tick(&mut self, intents: &[PlayerIntent]) -> TickResult {
        self.state.debug_assert_coherent();
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
        let entity_state_updates = self.phase_state_finalization();

        // Phase 9: Event emission (collect pending events)
        // Events have been accumulated during phases above.

        // Invariant checks — only active in debug builds (debug_assert! is a no-op in release).
        // Catches parallel-structure divergence between sensor_handles, HitboxStore, and
        // entity component arrays. Called here (post-all-phases) to catch mid-tick corruption.
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
        let result = TickResult {
            tick_id: self.current_tick,
            transforms: self.physics.get_all_transforms(),
            events: std::mem::take(&mut self.pending_events),
            summary: self.summary,
            entity_state_updates,
            health_updates,
        };

        self.current_tick = self.current_tick.next();
        result
    }

    // ── Invariant verification (debug builds only) ──────────────

    /// Cross-structure invariant checks for parallel state systems.
    ///
    /// The key invariant: `sensor_handles`, `HitboxStore`, and Rapier sensors are three
    /// separate representations of the same set of active hitbox colliders. Silent divergence
    /// between them causes ghost hits or missing hit registration that are very hard to reproduce.
    ///
    /// Also re-asserts that all SoA component arrays have the same length as the entity store.
    ///
    /// Called at end of each tick in debug builds; uses `debug_assert!` so it compiles away
    /// in release.
    fn verify_invariants(&self) {
        // All parallel component arrays must agree on entity count.
        self.state.debug_assert_coherent();

        // Pipeline-level sensor_handles must mirror HitboxStore one-for-one.
        // Any mismatch means a SpawnHitbox or RemoveHitbox did not propagate to both.
        debug_assert_eq!(
            self.sensor_handles.len(),
            self.state.combat.hitboxes.len(),
            "sensor_handles ({}) and HitboxStore ({}) diverged — hitbox lifecycle bug",
            self.sensor_handles.len(),
            self.state.combat.hitboxes.len(),
        );
    }

    // ── Phase 2: Controller update ──────────────────────────────

    fn phase_controller_update(&mut self, intents: &[&PlayerIntent]) {
        for intent in intents {
            let entity_id = intent.client_id;

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
                IntentAction::FaceTo(_dir) => {
                    // TODO: Apply rotation to physics body.
                }
                IntentAction::UseAbility(data) => {
                    let ability_id = data.ability_id;
                    // Validate cooldown: skip if a CooldownExpire is queued for this entity+ability.
                    if self.is_on_cooldown(entity_id, ability_id) {
                        continue;
                    }
                    // Clone to release the shared borrow before calling schedule_ability (&mut self).
                    if let Some(timeline) = self.abilities.get_timeline(ability_id).cloned() {
                        self.schedule_ability(entity_id, &timeline, self.current_tick);
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
        let len_sq = dir.dir_x * dir.dir_x + dir.dir_y * dir.dir_y + dir.dir_z * dir.dir_z;
        if len_sq < 1e-6 {
            return;
        }
        let inv_len = 1.0 / len_sq.sqrt();
        let vx = dir.dir_x * inv_len * BASE_SPEED;
        let vy = dir.dir_y * inv_len * BASE_SPEED;
        let vz = dir.dir_z * inv_len * BASE_SPEED;

        // Character bodies are kinematic_position_based — velocity calls have no effect.
        // Integrate one timestep of desired velocity and set the explicit next position.
        if let Some(t) = self.physics.get_transform(entity_id) {
            self.physics.set_kinematic_position(entity_id, Vec3f {
                x: t.position.x + vx * self.dt,
                y: t.position.y + vy * self.dt,
                z: t.position.z + vz * self.dt,
            });
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
            match scheduled.action_type {
                ScheduledActionType::AbilityFrame { ability_id, ref action } => {
                    self.execute_ability_action(entity, ability_id, action);
                }
                ScheduledActionType::BuffExpire { buff_id } => {
                    self.emit_event(
                        entity,
                        EventPayload::BuffExpired { buff_id },
                    );
                }
                ScheduledActionType::CooldownExpire { ability_id } => {
                    self.emit_event(
                        entity,
                        EventPayload::CooldownReady { ability_id },
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
            AbilityAction::SpawnHitbox { shape, offset } => {
                self.state.combat.hitboxes.spawn(entity, ability_id, self.current_tick);
                let sensor_shape = skill_shape_to_sensor(*shape);
                if let Some(handle) = self.physics.spawn_sensor(
                    entity,
                    sensor_shape,
                    *offset,
                    ColliderKind::Hitbox(ability_id),
                ) {
                    self.sensor_handles.insert((entity, ability_id), handle);
                }
                self.emit_event(entity, EventPayload::HitboxSpawned { ability_id });
            }
            AbilityAction::ApplyDamageFrame => {
                // Marker: combat resolution reads the hitbox store + contacts.
                self.emit_event(entity, EventPayload::DamageFrame { ability_id });
            }
            AbilityAction::RemoveHitbox => {
                self.state.combat.hitboxes.remove(entity, ability_id);
                if let Some(handle) = self.sensor_handles.remove(&(entity, ability_id)) {
                    self.physics.remove_sensor(handle);
                }
                self.emit_event(entity, EventPayload::HitboxRemoved { ability_id });
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
            // Determine attacker/target using collider kinds.
            let (attacker, ability_id, target) = match (contact.kind1, contact.kind2) {
                (ColliderKind::Hitbox(aid), ColliderKind::Body | ColliderKind::Hurtbox) => {
                    (contact.entity1, aid, contact.entity2)
                }
                (ColliderKind::Body | ColliderKind::Hurtbox, ColliderKind::Hitbox(aid)) => {
                    (contact.entity2, aid, contact.entity1)
                }
                _ => continue, // Not a hitbox-vs-target contact.
            };

            // Skip self-hits.
            if attacker == target {
                continue;
            }

            // Dedup: skip if this hitbox already hit this target.
            if !self.state.combat.hitboxes.record_hit(attacker, ability_id, target) {
                continue;
            }

            // Look up ability data for damage values.
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
            self.summary.damage_events += 1;

            // Generate threat on NPC targets.
            if let Some(table) = self.state.combat.threat_tables[target_idx.as_usize()].as_mut() {
                table.add_threat(attacker, actual * threat_mult);
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
        use std::collections::HashSet;
        let damaged: HashSet<EntityId> = self.pending_events.iter()
            .filter_map(|e| if matches!(&e.payload, EventPayload::Damage { .. }) { Some(e.entity_id) } else { None })
            .collect();
        damaged.iter().filter_map(|&eid| {
            let idx = self.state.entities.lookup(eid)?;
            let i = idx.as_usize();
            Some((eid, self.state.combat.health.hp[i], self.state.combat.health.max_hp[i]))
        }).collect()
    }

    // ── Phase 8: State finalization ─────────────────────────────

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
            // Clear our sensor tracking before body removal (body removal cascades to colliders).
            let hitbox_aids: Vec<u32> = self.state.combat.hitboxes
                .hitboxes_for(id)
                .iter()
                .map(|hb| hb.ability_id)
                .collect();
            for aid in hitbox_aids {
                self.sensor_handles.remove(&(id, aid));
            }
            self.state.remove_entity(id);
            self.physics.remove_entity(id);
            state_updates.push((id, EntityState::Removed));
        }

        // Expire buffs.
        let expired_buffs = self.state.expire_buffs(self.current_tick);
        for (entity_id, buff_id) in expired_buffs {
            self.emit_event(entity_id, EventPayload::BuffExpired { buff_id });
        }

        // Decay threat tables.
        const THREAT_DECAY_PER_TICK: f32 = 0.5;
        for table in self.state.combat.threat_tables.iter_mut().flatten() {
            table.decay(THREAT_DECAY_PER_TICK);
        }

        // Activate any Spawning entities (they've had one tick to set up physics).
        let spawning: Vec<EntityIndex> = (0..self.state.entities.len())
            .filter(|&i| self.state.entities.states[i] == EntityState::Spawning)
            .map(|i| EntityIndex(i as u32))
            .collect();
        for idx in spawning {
            let id = self.state.entities.id_of(idx);
            self.state.entities.activate(idx);
            state_updates.push((id, EntityState::Active));
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
        // Use spawn_sensor (the trait method) so PhysicsWorld.sensor_handles and
        // TickPipeline.sensor_handles are both populated — matching what execute_ability_action does.
        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(1),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.sensor_handles.insert((attacker_id, 1), sensor);
        pipeline.state.combat.hitboxes.spawn(attacker_id, 1, TickId(1));

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
        // Use spawn_sensor so all three state structures stay in sync (invariant).
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(10),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.sensor_handles.insert((attacker, 10), sensor);
        pipeline.state.combat.hitboxes.spawn(attacker, 10, TickId(1));

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
        // Phase 2 → schedule_ability → Phase 3 spawns hitbox in physics → Phase 4 steps.
        let cast_intent = PlayerIntent {
            client_id: attacker,
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
            client_id: attacker,
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
        // Use spawn_sensor so all three state structures stay in sync (invariant).
        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(1),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.sensor_handles.insert((attacker_id, 1), sensor);
        pipeline.state.combat.hitboxes.spawn(attacker_id, 1, TickId(1));

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

        // Use spawn_sensor so all three state structures stay in sync (invariant).
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(10),
        ).expect("attacker has a body — spawn_sensor must succeed");
        pipeline.sensor_handles.insert((attacker, 10), sensor);
        pipeline.state.combat.hitboxes.spawn(attacker, 10, TickId(1));

        let result = pipeline.run_tick(&[]);

        // The victim was removed from the EntityStore in phase_state_finalization
        // (same tick as the hit), but health_updates must still carry hp=0 for it
        // so the DB row is updated atomically with the EntityDied event.
        let victim_update = result.health_updates.iter().find(|(eid, _, _)| *eid == victim);
        assert!(victim_update.is_some(), "Killed entity must appear in health_updates (hp=0) so DB reflects death");
        let (_, hp, _) = victim_update.unwrap();
        assert_eq!(*hp, 0.0, "Killed entity hp must be 0 in health update");
    }

    /// Regression test for kinematic-kinematic collision detection.
    ///
    /// In the live server, both player and NPC bodies are `kinematic_position_based`.
    /// Rapier's default `ActiveCollisionTypes` excludes KINEMATIC_KINEMATIC, so without
    /// explicitly enabling it, hitbox sensors attached to the player's kinematic body
    /// would never detect contacts against the NPC's kinematic body or hurtbox sensor.
    ///
    /// This test uses `spawn_character_body` (which calls `add_kinematic_capsule`) for
    /// both entities — exactly mirroring the live server path — and verifies that a
    /// UseAbility intent produces damage.
    #[test]
    fn hitbox_damages_kinematic_npc_through_intent() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        let npc = EntityId(2);

        // Spawn both entities using the same path as the live server.
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

        // Tick 0: warm-up — Spawning → Active for both entities.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player), "player must be Active after warm-up");
        assert!(pipeline.state.is_active(npc), "npc must be Active after warm-up");

        // Tick 1: UseAbility(1) intent → Phase 2 schedules Slash → Phase 3 spawns hitbox
        // → Phase 4 physics step → Phase 5 drains contacts → Phase 6 applies damage.
        let slash_intent = PlayerIntent {
            client_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_time_ms: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let result = pipeline.run_tick(&[slash_intent]);

        // Verify damage was applied — NPC hp must be below max.
        let npc_hp = pipeline.state.hp_of(npc).unwrap();
        assert!(
            npc_hp < 100.0,
            "Kinematic NPC must take damage from Slash via UseAbility intent; hp={npc_hp}"
        );

        // Verify Damage event was emitted.
        let has_damage = result.events.iter().any(|e| {
            matches!(&e.payload, EventPayload::Damage { source, .. } if *source == player)
        });
        assert!(has_damage, "Damage event must be emitted for kinematic NPC hit");
    }
}
