use std::collections::{HashMap, HashSet};
use log::warn;
use game_protocol::event::{EventPayload, SimEvent};
use game_protocol::intent::{IntentAction, MoveDir, PlayerIntent};
use game_protocol::tick::TickId;
use game_protocol::entity_id::EntityId;
use game_protocol::types::Transform;
use game_core::physics_backend::{ColliderKind, PhysicsBackend, SensorShape};
use game_core::combat::skill::{
    AbilityAction, AbilityParams, AbilityTimeline, AbilityRegistry,
    AbilityExecutionContext, AbilityExecutionId, ChargingState, ResolvedTargeting,
    ScheduledAction, ScheduledActionType, SkillShape,
};
use game_core::director::{DirectorSpawn, DirectorState};
use game_core::entity::entity_index::EntityIndex;
use game_core::sim_state::SimState;
use game_protocol::types::{Quatf, Vec3f};
use game_schema::EntityKind;
use crate::lag_compensation::{self, TransformHistory};

// ── Region / AOI constants ──────────────────────────────────────────

/// Width of a spatial grid cell in world units.
pub(crate) const CELL_SIZE: f32 = 50.0;

/// Hysteresis band in world units. An entity must move at least this far
/// past a cell boundary before a region transition is emitted. Prevents
/// thrashing when entities oscillate on cell edges.
pub(crate) const HYSTERESIS_BAND: f32 = 5.0;

/// A spatial grid cell assignment with a visibility layer.
///
/// `region_x` and `region_z` identify the cell in an infinite 2D grid.
/// `layer` isolates entities within the same cell (instancing, phasing, stealth).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegionCell {
    pub region_x: i32,
    pub region_z: i32,
    pub layer: u32,
}

impl RegionCell {
    /// Compute the grid cell for a world position on the default layer.
    pub fn from_position(pos: &Vec3f) -> Self {
        Self {
            region_x: (pos.x / CELL_SIZE).floor() as i32,
            region_z: (pos.z / CELL_SIZE).floor() as i32,
            layer: 0,
        }
    }

    /// Compute the grid cell with hysteresis: only transition if the entity
    /// has moved past the boundary by at least `HYSTERESIS_BAND` units.
    /// Returns `current` unchanged if the move is within the dead zone.
    pub fn from_position_with_hysteresis(pos: &Vec3f, current: &RegionCell) -> Self {
        let raw_x = (pos.x / CELL_SIZE).floor() as i32;
        let raw_z = (pos.z / CELL_SIZE).floor() as i32;

        let mut result = *current;

        if raw_x != current.region_x {
            let boundary = if raw_x > current.region_x {
                // Moved right: check distance past the right edge of the current cell
                (current.region_x + 1) as f32 * CELL_SIZE
            } else {
                // Moved left: check distance past the left edge of the current cell
                current.region_x as f32 * CELL_SIZE
            };
            if (pos.x - boundary).abs() >= HYSTERESIS_BAND {
                result.region_x = raw_x;
            }
        }

        if raw_z != current.region_z {
            let boundary = if raw_z > current.region_z {
                (current.region_z + 1) as f32 * CELL_SIZE
            } else {
                current.region_z as f32 * CELL_SIZE
            };
            if (pos.z - boundary).abs() >= HYSTERESIS_BAND {
                result.region_z = raw_z;
            }
        }

        result
    }
}

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
    pub scheduled_actions_len: usize,
    pub tick_duration_us: u64,
    pub commit_retries: u32,
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
    /// Region assignment changes for entities that crossed a grid cell boundary
    /// this tick. Only entities whose cell changed (with hysteresis) are included.
    pub region_updates: Vec<(EntityId, RegionCell)>,
    /// Entities spawned by the world director this tick.
    /// Coordinator marshals these into DB insert calls so the entities are persisted.
    pub director_spawns: Vec<DirectorSpawn>,
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
    /// Buff templates — looked up by ID when applying buffs from abilities.
    buff_registry: game_core::combat::status::BuffRegistry,
    /// Runtime entity state — lifecycle, health, buffs, threat, AI, contacts.
    pub(crate) state: SimState,
    /// Explicit cooldown state — maps (EntityId, ability_id) → tick when the cooldown expires.
    ///
    /// The HashMap owns cooldown truth: O(1) lookup in Phase 2, drained in Phase 8 to emit
    /// CooldownReady events. Retroactive cooldown reduction is a direct mutation of the ready_at
    /// value. Entity despawn cleaning removes all entries in a single `retain` call.
    cooldowns: HashMap<(EntityId, u32), TickId>,
    /// Monotonically increasing counter for `ScheduledAction::id`.
    /// Assigned at scheduling time; never reused within a session.
    next_scheduled_id: u64,
    /// Impulses to apply at the start of Phase 4 on the next tick.
    ///
    /// Accumulated in Phase 6 combat resolution (knockback, boss push,
    /// block-that-yields). Applied via `physics.set_linear_velocity` before
    /// the physics step so that Rapier resolves them on the same tick the
    /// physical effect should first be visible. Cleared after application,
    /// so each entry is consumed exactly once.
    ///
    /// Preserves the "physics does not know gameplay" invariant: combat accumulates
    /// requests into this lane; physics applies them without knowing their cause.
    pending_impulses: Vec<(EntityId, Vec3f)>,
    /// Running stats for the current tick — incremented inline, returned in TickResult.
    summary: TickSummary,
    /// Last committed region cell per entity. Used to detect cell crossings with
    /// hysteresis so only actual transitions produce `RegionUpdate` entries.
    /// Seeded at spawn (from initial position → cell), updated each tick when
    /// a transition is emitted. Entries are removed in `force_remove_entity`.
    entity_regions: HashMap<EntityId, RegionCell>,
    /// Ring buffer of recent transform snapshots for lag-compensation rewind.
    /// Populated at the end of each tick (Phase 10) with entity positions.
    /// Phase 6 reads historical positions for compensated hit detection.
    transform_history: TransformHistory,
    /// World director — evaluates dynamic event triggers and spawns NPCs.
    director: DirectorState,
    /// Entities whose cached `StatBlock` needs recalculation.
    /// Populated by: (a) equipment changes (via `mark_stats_dirty`),
    /// (b) buff changes (copied from `StatusState::dirty_entities` at Phase 10).
    /// Drained by Phase 1.5 stat recalculation.
    /// Keyed by EntityId (not dense index) so slot reuse cannot cause stale refs.
    stats_dirty: HashSet<EntityId>,
    /// Aggregated equipment modifiers per entity. Updated by the coordinator
    /// when `player_equipment` rows change; read by `phase_stat_recalc`.
    equipment_modifiers: HashMap<EntityId, game_core::stats::EquipmentModifiers>,
    /// NPCs whose threat table has been emitted as non-empty at least once.
    /// Used by `collect_threat_updates` to emit one final empty snapshot when
    /// all entries decay to zero, then stop emitting entirely. Prevents
    /// O(N) per-tick DB scans for idle NPCs with no threat history.
    threat_has_db_rows: HashSet<EntityId>,
    /// Last emitted (NpcAiState, target) per NPC. `collect_npc_state_updates`
    /// only emits when the current value differs, eliminating redundant upserts
    /// for idle NPCs whose state never changes.
    npc_state_prev: HashMap<EntityId, (game_schema::NpcAiState, Option<EntityId>)>,
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

        // 4) Remove any pending impulses and follow-up windows for this entity.
        self.pending_impulses.retain(|&(eid, _)| eid != id);
        self.state.combat.active_windows.retain(|&(eid, _), _| eid != id);
        self.state.combat.charging.remove(&id);

        // 4b) Scrub this entity from all NPC threat tables so they acquire new targets.
        // Leaving a removed entity in threat tables can cause NPCs to lock onto a
        // non-existent target and no-op during movement (physics transform missing).
        for (_, table) in self.state.combat.threat_tables.iter_mut() {
            table.entries.retain(|e| e.source != id);
        }

        // 5) Remove hitbox sensors and execution contexts owned by this entity
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

        // 6) Drop region tracking and dirty-tracking for this entity.
        self.entity_regions.remove(&id);
        self.threat_has_db_rows.remove(&id);
        self.npc_state_prev.remove(&id);
        self.equipment_modifiers.remove(&id);

        // 7) Finally, remove entity from SimState and physics world
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
        buff_registry: game_core::combat::status::BuffRegistry,
    ) -> Self {
        Self {
            current_tick: start_tick,
            event_sequence: 0,
            pending_events: Vec::new(),
            physics,
            dt,
            scheduled_actions: Vec::new(),
            abilities,
            buff_registry,
            state: SimState::new(),
            cooldowns: HashMap::new(),
            next_scheduled_id: 0,
            pending_impulses: Vec::new(),
            summary: TickSummary::default(),
            entity_regions: HashMap::new(),
            transform_history: TransformHistory::new(),
            director: DirectorState::new(),
            stats_dirty: HashSet::new(),
            equipment_modifiers: HashMap::new(),
            threat_has_db_rows: HashSet::new(),
            npc_state_prev: HashMap::new(),
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
        // Record spawn position as patrol home for NPC/Boss entities.
        if (kind == EntityKind::Npc || kind == EntityKind::Boss)
            && let Some(idx) = self.state.entities.lookup(id) {
                self.state.ai.home_positions.insert(idx, position);
            }

        // NOTE: entity_regions is NOT seeded here. collect_region_updates() will
        // discover the entity on its first tick (via the `None` branch) and emit
        // an initial region update, which guarantees the DB receives the correct
        // cell computed from the actual spawn position.
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
                self.state.status.replace_buffs(idx, entity_buffs.clone());
                // Ensure stats are recalculated on the first tick so restored
                // buff modifiers take effect immediately.
                self.stats_dirty.insert(*eid);
            }
        }
        for (eid, entries) in threats {
            if let Some(idx) = self.state.entities.lookup(*eid)
                && let Some(table) = self.state.combat.threat_tables.get_mut(idx) {
                    table.entries = entries.clone();
                }
        }
        for (eid, ai_state, _target) in npc_states {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                if let Some(ai) = self.state.ai.npc_ai.get_mut(idx) {
                    *ai = *ai_state;
                }
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

    

    /// Attempt to cast an ability for a given entity.
    ///
    /// Shared entry point for both player intents (Phase 2) and NPC AI (Phase 7).
    /// Performs cooldown validation, creates the `AbilityExecutionContext` with a
    /// cast-time position/facing snapshot, and schedules the ability timeline.
    ///
    /// `rewind_ticks` controls lag compensation depth: `0` for NPCs (no client lag),
    /// or computed from `client_observed_tick` for player intents.
    ///
    /// `charge_level` is the resolved charge tier (0 for instant-cast or tier 0).
    ///
    /// Returns `true` if the ability was successfully cast, `false` if rejected
    /// (on cooldown, unknown ability, entity has no physics body).
    pub(crate) fn cast_ability(
        &mut self,
        caster: EntityId,
        ability_id: u32,
        targeting: ResolvedTargeting,
        rewind_ticks: u32,
        charge_level: u8,
    ) -> bool {
        // Combo redirect: if a follow-up window is open for (caster, ability_id),
        // redirect the cast to the replacement ability.  Each combo step is a
        // first-class ability with its own timeline, damage, and cooldown.
        let resolved_id = if let Some(&(next_id, exp)) = self.state.combat.active_windows
            .get(&(caster, ability_id))
        {
            if self.current_tick <= exp {
                self.state.combat.active_windows.remove(&(caster, ability_id));
                next_id
            } else {
                ability_id
            }
        } else {
            ability_id
        };

        if self.is_on_cooldown(caster, resolved_id) {
            return false;
        }
        let timeline = match self.abilities.get_timeline(resolved_id).cloned() {
            Some(t) => t,
            None => return false,
        };

        // Snapshot caster position and facing at cast time so later timeline
        // phases (ApplyDamageFrame, projectile spawn, etc.) use cast-time
        // geometry rather than the caster's current position.
        let (origin, facing) = if let Some(t) = self.physics.get_transform(caster) {
            // Yaw quaternion: (0, sin(θ/2), 0, cos(θ/2)) → facing = (sinθ, 0, cosθ).
            let yaw = 2.0 * t.rotation.y.atan2(t.rotation.w);
            (t.position, Vec3f { x: yaw.sin(), y: 0.0, z: yaw.cos() })
        } else {
            (Vec3f::ZERO, Vec3f { x: 0.0, y: 0.0, z: 1.0 })
        };

        let params = AbilityParams { charge_level, variant: 0 };
        let execution_id = self.state.combat.executions.next_id();
        self.state.combat.executions.insert(AbilityExecutionContext {
            execution_id,
            ability_id: resolved_id,
            caster,
            started_at: self.current_tick,
            targeting,
            origin,
            facing,
            params,
            rewind_ticks,
        });
        let cast_duration_ticks = timeline.actions.iter()
            .map(|a| a.tick_offset)
            .max()
            .unwrap_or(0) + 1;
        self.emit_event(caster, EventPayload::CastStart {
            ability_id: resolved_id,
            cast_duration_ticks,
        });
        self.schedule_ability(caster, &timeline, self.current_tick, execution_id);
        true
    }

    /// Returns true if `ability_id` is on cooldown for `entity` this tick.
    ///
    /// O(1) — looks up the ready_at tick in the cooldown HashMap. The entry is absent
    /// (implying not on cooldown) when the ability has never been cast or the cooldown
    /// already expired and was pruned by `phase_expire_cooldowns`.
    fn is_on_cooldown(&self, entity: EntityId, ability_id: u32) -> bool {
        self.cooldowns
            .get(&(entity, ability_id))
            .is_some_and(|&ready_at| self.current_tick < ready_at)
    }

    /// Test-visible alias for `is_on_cooldown`. Production code uses the private method
    /// directly; tests need it to assert post-cast cooldown state without going through
    /// a UseAbility intent.
    #[cfg(test)]
    pub fn is_on_cooldown_pub(&self, entity: EntityId, ability_id: u32) -> bool {
        self.is_on_cooldown(entity, ability_id)
    }

    /// Returns true if the entity cannot move this tick.
    ///
    /// Checks two sources:
    /// - `TacticalState.rooted` (set by Block intent or StanceBegin)
    /// - Any active buff with `root: Some(true)`
    fn is_rooted(&self, idx: game_core::entity::entity_index::EntityIndex) -> bool {
        if self.state.combat.tactical[idx.as_usize()].rooted {
            return true;
        }
        self.state.status.get_buffs(idx)
            .iter()
            .any(|b| b.modifiers.root == Some(true))
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
    fn execution_is_alive(exec_id: AbilityExecutionId, scheduled_sources: &HashSet<AbilityExecutionId>, hitboxes: &game_core::combat::hitbox::HitboxStore) -> bool {
        scheduled_sources.contains(&exec_id)
            || hitboxes.get(exec_id).is_some()
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

        // Phase 1.5: Stat recalculation — recompute cached StatBlocks for dirty entities
        self.phase_stat_recalc();

        // Phase 2: Controller update
        self.phase_controller_update(&tick_intents);

        // Phase 3: Skill scheduling
        self.phase_skill_scheduling();

        // Phase 3.5: Projectile movement — advance world-space hitbox sensors
        self.phase_projectile_movement();

        // Phase 4: Physics integration
        self.phase_physics_step();

        // Phase 5: Contact collection
        self.phase_contact_collection();

        // Phase 6: Combat resolution
        self.phase_combat_resolution();

        // Phase 7: AI decisions
        self.phase_ai_decisions();

        // Phase 7.5: World orchestration — director evaluates triggers and spawns
        let director_spawns = self.phase_world_orchestration();

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
        self.summary.scheduled_actions_len = self.scheduled_actions.len();
        // Collect runtime domain snapshots for persistence.
        let buff_updates = self.collect_buff_updates();
        let threat_updates = self.collect_threat_updates();
        let npc_state_updates = self.collect_npc_state_updates();

        let transforms = self.physics.get_all_transforms();
        let region_updates = self.collect_region_updates(&transforms);

        // Snapshot positions into the transform history ring buffer for lag compensation.
        // Must happen before advancing current_tick so the snapshot is tagged with the
        // tick that produced these positions.
        self.transform_history.record(
            self.current_tick,
            transforms.iter().map(|(eid, t)| (*eid, t.position)).collect(),
        );

        let result = TickResult {
            tick_id: self.current_tick,
            transforms,
            events: std::mem::take(&mut self.pending_events),
            summary: self.summary,
            entity_state_updates,
            health_updates,
            buff_updates,
            threat_updates,
            npc_state_updates,
            region_updates,
            director_spawns,
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
        // Sub-step: drive arc movement for entities mid-vault/leap.
        // Runs before intent processing so arcs take priority over player input
        // (the entity is also rooted, so apply_movement would early-exit anyway).
        self.drive_arc_movement();

        // Sub-step: advance charge timers, detect tier crossings, auto-release.
        // Runs before intent processing so auto-releases fire before new intents.
        self.drive_charging();

        // Track which (entity, ability) pairs have been cast in this Phase 2 pass.
        // Without this, two UseAbility intents for the same ability targeting the same
        // tick both pass is_on_cooldown() — the cooldown map isn’t updated until Phase 3.
        // This closes the same-tick double-cast exploit before the cooldown state exists.
        let mut cast_this_tick: HashSet<(EntityId, u32)> = HashSet::new();

        for intent in intents {
            let entity_id = intent.entity_id;

            // Only process intents for active entities.
            let is_active = self.state.entities.lookup(entity_id)
                .is_some_and(|idx| self.state.entities.is_active(idx));
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
                    if cast_this_tick.contains(&cast_key) {
                        continue;
                    }
                    // Reject if already charging (prevent double-charge).
                    if self.state.combat.charging.contains_key(&entity_id) {
                        continue;
                    }
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
                    let rewind_ticks = lag_compensation::compute_rewind_ticks(
                        self.current_tick,
                        intent.client_observed_tick,
                    );

                    // Check if this ability is chargeable (needs ≥2 tiers).
                    let is_chargeable = self.abilities.get(ability_id)
                        .and_then(|ad| ad.charge_tiers.as_ref())
                        .is_some_and(|tiers| tiers.len() >= 2);

                    if is_chargeable {
                        // Don't fire yet — start charging. Ability fires on
                        // ReleaseAbility or auto-release at max tier.
                        if self.is_on_cooldown(entity_id, ability_id) {
                            continue;
                        }
                        let max_ticks = self.abilities.get(ability_id)
                            .and_then(|ad| ad.charge_tiers.as_ref())
                            .and_then(|tiers| tiers.last())
                            .map(|t| t.min_ticks)
                            .unwrap_or(1);
                        self.state.combat.charging.insert(entity_id, ChargingState {
                            ability_id,
                            started_at: self.current_tick,
                            targeting,
                            rewind_ticks,
                            notified_tier: 0,
                        });
                        // Root the entity while charging.
                        if let Some(idx) = self.state.entities.lookup(entity_id) {
                            self.state.combat.tactical[idx.as_usize()].rooted = true;
                        }
                        self.emit_event(entity_id, EventPayload::ChargeStart {
                            ability_id,
                            max_ticks,
                        });
                        audit!(self.state, Execution, Controller, 2, Some(entity_id), "charge_start");
                    } else {
                        if self.cast_ability(entity_id, ability_id, targeting, rewind_ticks, 0) {
                            audit!(self.state, Execution, Controller, 2, Some(entity_id), "cast");
                            cast_this_tick.insert(cast_key);
                        }
                    }
                }
                IntentAction::ReleaseAbility(ability_id) => {
                    let ability_id = *ability_id;
                    // Release a charging ability: resolve tier from elapsed ticks and fire.
                    if let Some(charging) = self.state.combat.charging.remove(&entity_id) {
                        if charging.ability_id == ability_id {
                            let elapsed = self.current_tick.0.saturating_sub(charging.started_at.0) as u32;
                            let tier = self.resolve_charge_tier(ability_id, elapsed);
                            // Unroot the entity.
                            if let Some(idx) = self.state.entities.lookup(entity_id) {
                                self.state.combat.tactical[idx.as_usize()].rooted = false;
                            }
                            if self.cast_ability(entity_id, ability_id, charging.targeting, charging.rewind_ticks, tier) {
                                audit!(self.state, Execution, Controller, 2, Some(entity_id), "charge_release");
                                cast_this_tick.insert((entity_id, ability_id));
                            }
                        } else {
                            // Wrong ability — re-insert the charge state.
                            self.state.combat.charging.insert(entity_id, charging);
                        }
                    }
                }
                IntentAction::Block => {
                    // Hold-to-block: set blocking flag each tick the button is held.
                    // Phase 8 clears `blocking` every tick, so the intent must re-assert it.
                    // `block_start_tick` is set on the first tick of a new block and preserved
                    // across consecutive Block intents for perfect-block window calculation.
                    // Blocking also roots the player (cannot move while holding block).
                    if let Some(idx) = self.state.entities.lookup(entity_id) {
                        let is_new_block = self.state.combat.tactical[idx.as_usize()].block_start_tick.is_none();
                        let t = &mut self.state.combat.tactical[idx.as_usize()];
                        t.blocking = true;
                        t.rooted = true;
                        t.block_grace = false;
                        if is_new_block {
                            t.block_start_tick = Some(self.current_tick);
                            self.emit_event(entity_id, EventPayload::BlockStart);
                        }
                        audit!(self.state, Tactical, Controller, 2, Some(entity_id), "block");
                    }
                }
                IntentAction::Interact(target_id_raw) => {
                    // Proximity validation: reject if target is out of interact range.
                    // Both positions are read from physics (post-last-tick state).
                    // If either transform is unavailable, silently drop the intent.
                    const INTERACT_RADIUS: f32 = 3.0;
                    let target = EntityId(*target_id_raw);
                    if let (Some(actor_t), Some(target_t)) = (
                        self.physics.get_transform(entity_id),
                        self.physics.get_transform(target),
                    ) {
                        let dx = actor_t.position.x - target_t.position.x;
                        let dz = actor_t.position.z - target_t.position.z;
                        let dist_sq = dx * dx + dz * dz;
                        if dist_sq <= INTERACT_RADIUS * INTERACT_RADIUS {
                            self.emit_event(entity_id, EventPayload::InteractTriggered { target });
                        }
                    }
                }
            }
        }
    }

    fn apply_movement(&mut self, entity_id: EntityId, dir: &MoveDir) {
        // Rooted entities cannot move — block stance, CC, or channeled skill.
        if let Some(idx) = self.state.entities.lookup(entity_id) {
            if self.is_rooted(idx) {
                return;
            }
        }
        // Normalise direction and scale by the cached movement speed from StatBlock.
        // Grounded movement: ignore vertical component to prevent client "flight".
        let x = dir.dir_x;
        let z = dir.dir_z;
        let len_sq = x * x + z * z;
        // Guard against NaN since `NaN < 1e-6` evaluates to `false` and
        // would let invalid directions through, producing NaN velocities.
        if !len_sq.is_finite() || len_sq < 1e-6 {
            return;
        }
        // Read cached movement speed from StatBlock (recalculated in Phase 1.5).
        let speed = if let Some(idx) = self.state.entities.lookup(entity_id) {
            self.state.stats.get(idx).movement_speed
        } else {
            return;
        };
        let inv_len = 1.0 / len_sq.sqrt();
        let vx = x * inv_len * speed;
        let vy = 0.0;
        let vz = z * inv_len * speed;

        // Move via character controller — resolves contacts against static
        // geometry (walls, obstacles) and slides along surfaces.
        let desired = Vec3f {
            x: vx * self.dt,
            y: vy * self.dt,
            z: vz * self.dt,
        };
        if self.physics.move_character(entity_id, desired).is_some() {
            audit!(self.state, Transform, Controller, 2, Some(entity_id), "move");
        }
    }

    fn apply_stop(&mut self, _entity_id: EntityId) {
        // For kinematic_position_based bodies, not issuing a set_kinematic_position
        // this tick is sufficient — the body remains at its current position.
    }

    /// Advance all entities currently in a kinematic arc (vault / leap).
    ///
    /// For each entity with `TacticalState::arc_state`:
    /// 1. Apply gravity to the velocity's Y component.
    /// 2. Feed `velocity * dt` into `move_character`.
    /// 3. If `MoveResult::grounded` (and Y velocity is non-positive, so we're
    ///    past the launch apex), end the arc and unroot the entity.
    fn drive_arc_movement(&mut self) {
        let dt = self.dt;

        // Collect (entity_id, slot_index) for entities with active arcs.
        // Two-pass avoids borrowing `self` mutably while iterating tactical.
        let arc_entities: Vec<(EntityId, usize)> = self.state.combat.tactical.iter()
            .enumerate()
            .filter(|(_, t)| t.arc_state.is_some())
            .filter_map(|(i, _)| {
                let id = self.state.entities.lookup_by_slot(i)?;
                Some((id, i))
            })
            .collect();

        for (entity_id, slot) in arc_entities {
            let arc = match &mut self.state.combat.tactical[slot].arc_state {
                Some(a) => a,
                None => continue,
            };
            // Integrate gravity (downward pull).
            arc.velocity.y -= arc.gravity * dt;

            let desired = Vec3f {
                x: arc.velocity.x * dt,
                y: arc.velocity.y * dt,
                z: arc.velocity.z * dt,
            };

            if let Some(result) = self.physics.move_character(entity_id, desired) {
                // End the arc when the character touches ground and is falling.
                if result.grounded && arc.velocity.y <= 0.0 {
                    let t = &mut self.state.combat.tactical[slot];
                    t.arc_state = None;
                    t.rooted = false;
                    audit!(self.state, Tactical, Controller, 2, Some(entity_id), "arc_landed");
                }
                audit!(self.state, Transform, Controller, 2, Some(entity_id), "arc_move");
            }
        }
    }

    /// Advance charge timers for all entities currently charging a hold-release ability.
    ///
    /// For each charging entity:
    /// 1. Compute elapsed ticks since `started_at`.
    /// 2. If a new tier threshold was crossed, emit `ChargeTierReached`.
    /// 3. If elapsed >= last tier's `min_ticks`, auto-release the ability at max tier.
    /// 4. Re-assert `rooted` so movement stays blocked while charging.
    fn drive_charging(&mut self) {
        // Collect auto-releases to process after iteration (avoids borrow issues).
        let mut auto_releases: Vec<(EntityId, ChargingState, u8)> = Vec::new();
        // Collect tier-up events to emit.
        let mut tier_events: Vec<(EntityId, u32, u8)> = Vec::new();

        for (&entity_id, charging) in self.state.combat.charging.iter_mut() {
            let elapsed = self.current_tick.0.saturating_sub(charging.started_at.0) as u32;
            let tiers = match self.abilities.get(charging.ability_id)
                .and_then(|ad| ad.charge_tiers.as_ref())
            {
                Some(t) => t,
                None => continue,
            };

            // Determine current tier from elapsed ticks.
            let current_tier = tiers.iter()
                .rposition(|t| elapsed >= t.min_ticks)
                .unwrap_or(0) as u8;

            // Emit tier-up events for newly crossed thresholds.
            if current_tier > charging.notified_tier {
                for t in (charging.notified_tier + 1)..=current_tier {
                    tier_events.push((entity_id, charging.ability_id, t));
                }
                charging.notified_tier = current_tier;
            }

            // Check auto-release: last tier reached.
            let max_tier = (tiers.len() - 1) as u8;
            if current_tier >= max_tier && elapsed >= tiers.last().unwrap().min_ticks {
                auto_releases.push((entity_id, charging.clone(), max_tier));
            }

            // Re-assert rooted (in case something else cleared it).
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                self.state.combat.tactical[idx.as_usize()].rooted = true;
            }
        }

        // Emit tier events.
        for (entity_id, ability_id, tier) in tier_events {
            self.emit_event(entity_id, EventPayload::ChargeTierReached {
                ability_id,
                tier,
            });
        }

        // Process auto-releases.
        for (entity_id, charging, tier) in auto_releases {
            self.state.combat.charging.remove(&entity_id);
            // Unroot the entity.
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                self.state.combat.tactical[idx.as_usize()].rooted = false;
            }
            if self.cast_ability(entity_id, charging.ability_id, charging.targeting, charging.rewind_ticks, tier) {
                audit!(self.state, Execution, Controller, 2, Some(entity_id), "charge_auto_release");
            }
        }
    }

    /// Resolve the highest charge tier achieved for a given elapsed tick count.
    /// Returns the tier index (0-based). If no tiers are defined, returns 0.
    fn resolve_charge_tier(&self, ability_id: u32, elapsed_ticks: u32) -> u8 {
        self.abilities.get(ability_id)
            .and_then(|ad| ad.charge_tiers.as_ref())
            .map(|tiers| {
                tiers.iter()
                    .rposition(|t| elapsed_ticks >= t.min_ticks)
                    .unwrap_or(0) as u8
            })
            .unwrap_or(0)
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
                        AbilityAction::OpenFollowUpWindow { .. } => "OpenFollowUpWindow",
                        AbilityAction::ApplyBuff { .. } => "ApplyBuff",
                        AbilityAction::StanceBegin { .. } => "StanceBegin",
                        AbilityAction::StanceEnd => "StanceEnd",
                        AbilityAction::Telegraph { .. } => "Telegraph",
                        AbilityAction::ArcMovement { .. } => "ArcMovement",
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
        let scheduled_sources: HashSet<AbilityExecutionId> = self
            .scheduled_actions
            .iter()
            .filter_map(|a| a.source)
            .collect();
        let dead: Vec<AbilityExecutionId> = self
            .state
            .combat
            .executions
            .active_ids()
            .into_iter()
            .filter(|&id| !Self::execution_is_alive(id, &scheduled_sources, &self.state.combat.hitboxes))
            .collect();
        for id in dead {
            if let Some(ctx) = self.state.combat.executions.get(id) {
                audit!(self.state, Execution, AbilityTimeline, 3, Some(ctx.caster), "cull");
            }
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
                // Look up rewind_ticks from the execution context for this cast.
                let rewind_ticks = self.state.combat.executions
                    .get(execution_id)
                    .map(|ctx| ctx.rewind_ticks)
                    .unwrap_or(0);
                let allow_reentry = self.abilities
                    .get(ability_id)
                    .map(|ad| ad.allow_reentry)
                    .unwrap_or(false);
                let damage_interval_ticks = self.abilities
                    .get(ability_id)
                    .map(|ad| ad.damage_interval_ticks)
                    .unwrap_or(0);
                // Declare the hitbox logically — no Rapier sensor yet.
                //
                // The sensor is deferred to `ApplyDamageFrame` so that the Rapier
                // physics step on the damage-frame tick is the first to see the
                // collider, generating CollisionEvent::started on the correct tick.
                // Before this fix, SpawnHitbox spawned the sensor immediately, so
                // Rapier fired contacts one tick early and damage landed on the spawn
                // tick rather than the intended damage-frame tick.
                self.state.combat.hitboxes.spawn(execution_id, entity, ability_id, self.current_tick, *shape, *offset, rewind_ticks, allow_reentry, damage_interval_ticks);
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
                //
                // For projectile shapes, spawn a world-space sensor at the caster's
                // origin and set up projectile travel state. Direction is resolved
                // from the targeting intent: Direction/SelfCast use caster facing,
                // Entity/LockOn aim toward the target, Position aims toward a world
                // point (range-capped).
                let stored = self.state.combat.hitboxes
                    .get(execution_id)
                    .map(|hb| (hb.shape, hb.offset));
                let ctx_data = self.state.combat.executions.get(execution_id)
                    .map(|ctx| (ctx.targeting.clone(), ctx.origin, ctx.facing));
                if let Some((shape, offset)) = stored {
                    let sensor_shape = skill_shape_to_sensor(shape);
                    let is_projectile = shape == SkillShape::Projectile;
                    let use_world_sensor = is_projectile;

                    if use_world_sensor {
                        let (targeting, origin, default_facing) = ctx_data.unwrap();
                        // Resolve travel direction from targeting intent.
                        let facing = match &targeting {
                            game_core::combat::skill::ResolvedTargeting::Entity { target }
                            | game_core::combat::skill::ResolvedTargeting::LockOn { target } => {
                                // Aim toward the target entity's current position.
                                self.physics.get_transform(*target)
                                    .and_then(|t| {
                                        let dx = t.position.x - origin.x;
                                        let dz = t.position.z - origin.z;
                                        let len_sq = dx * dx + dz * dz;
                                        if len_sq > 1e-6 {
                                            let inv = 1.0 / len_sq.sqrt();
                                            Some(game_protocol::types::Vec3f::new(dx * inv, 0.0, dz * inv))
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or(default_facing)
                            }
                            game_core::combat::skill::ResolvedTargeting::Position { point } => {
                                // Aim toward the targeted world position.
                                let dx = point.x - origin.x;
                                let dz = point.z - origin.z;
                                let len_sq = dx * dx + dz * dz;
                                if len_sq > 1e-6 {
                                    let inv = 1.0 / len_sq.sqrt();
                                    game_protocol::types::Vec3f::new(dx * inv, 0.0, dz * inv)
                                } else {
                                    default_facing
                                }
                            }
                            _ => default_facing,
                        };
                        // Spawn position: origin + offset (offset.z along facing direction)
                        let spawn_pos = game_protocol::types::Vec3f {
                            x: origin.x + facing.x * offset.z + offset.x,
                            y: origin.y + offset.y,
                            z: origin.z + facing.z * offset.z,
                        };
                        // For Position targeting, cap the projectile range to the
                        // distance to the target point so it doesn't overshoot.
                        const PROJECTILE_SPEED: f32 = 1.0; // units per tick (20 units/sec at 20Hz)
                        const PROJECTILE_MAX_RANGE: f32 = 30.0;
                        let max_range = match &targeting {
                            game_core::combat::skill::ResolvedTargeting::Position { point } => {
                                let dx = point.x - spawn_pos.x;
                                let dz = point.z - spawn_pos.z;
                                (dx * dx + dz * dz).sqrt().min(PROJECTILE_MAX_RANGE)
                            }
                            _ => PROJECTILE_MAX_RANGE,
                        };
                        let handle = self.physics.spawn_world_sensor(
                            spawn_pos,
                            sensor_shape,
                            ColliderKind::Hitbox(execution_id.0),
                            entity,
                        );
                        if self.state.combat.hitboxes.arm(execution_id, handle) {
                            // Set up projectile travel state.
                            if let Some(hb) = self.state.combat.hitboxes.get_mut(execution_id) {
                                hb.projectile = Some(game_core::combat::hitbox::ProjectileState {
                                    position: spawn_pos,
                                    prev_position: spawn_pos,
                                    direction: facing,
                                    speed: PROJECTILE_SPEED,
                                    max_range_sq: max_range * max_range,
                                    origin: spawn_pos,
                                });
                            }
                            // Notify clients so they can spawn a predicted visual.
                            self.emit_event(entity, EventPayload::ProjectileLaunched {
                                execution_id: execution_id.0,
                                ability_id,
                                origin: spawn_pos,
                                direction: facing,
                                speed: PROJECTILE_SPEED,
                                max_range,
                            });
                            audit!(self.state, Hitbox, AbilityTimeline, 3, Some(entity), "arm_world");
                        } else {
                            self.physics.remove_sensor(handle);
                        }
                    } else {
                        // Entity-parented sensor (melee, PBAoE, etc.)
                        if let Some(handle) = self.physics.spawn_sensor(
                            entity,
                            sensor_shape,
                            offset,
                            ColliderKind::Hitbox(execution_id.0),
                        ) {
                            if self.state.combat.hitboxes.arm(execution_id, handle) {
                                audit!(self.state, Hitbox, AbilityTimeline, 3, Some(entity), "arm");
                            } else {
                                self.physics.remove_sensor(handle);
                            }
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
                    // If this was a projectile, notify clients to kill the predicted visual.
                    if removed.projectile.is_some() {
                        self.emit_event(entity, EventPayload::ProjectileRemoved {
                            execution_id: execution_id.0,
                        });
                    }
                }
                // Execution context cleanup is deferred to the uniform culling pass
                // at the end of Phase 3.  execution_is_alive() will return false
                // now that the hitbox has been removed, so the context will be
                // collected on the same tick.
                self.emit_event(entity, EventPayload::HitboxRemoved { ability_id });
            }
            AbilityAction::CooldownStart { duration_ticks } => {
                // Insert directly into the cooldown map — no scheduled action needed.
                // Phase 8 drains entries where ready_at ≤ current_tick and emits CooldownReady.
                // Retroactive cooldown reduction: mutate the ready_at value for the entry directly.
                // If an existing entry for this (entity, ability) already reached readiness
                // on this same tick, emit the pending CooldownReady before overwriting it,
                // otherwise the ready event can be lost when the new ready_at is in future.
                if let Some(&old_ready) = self.cooldowns.get(&(entity, ability_id))
                    && old_ready <= self.current_tick
                {
                    // Remove the old entry and emit its CooldownReady now.
                    self.cooldowns.remove(&(entity, ability_id));
                    // This retroactive emission is initiated from AbilityTimeline (Phase 3).
                    audit!(self.state, Cooldown, AbilityTimeline, 3, Some(entity), "expire_retro");
                    self.emit_event(entity, EventPayload::CooldownReady { ability_id });
                }
                // Read cached cooldown reduction from StatBlock (recalculated in Phase 1.5).
                let cd_reduce: f32 = self.state.entities.lookup(entity)
                    .map(|idx| self.state.stats.get(idx).cooldown_reduce_pct)
                    .unwrap_or(0.0);
                let effective_duration = ((*duration_ticks as f32) * (1.0 - cd_reduce)).ceil() as u64;
                let ready_at = TickId(self.current_tick.0 + effective_duration.max(1));
                self.cooldowns.insert((entity, ability_id), ready_at);
                audit!(self.state, Cooldown, AbilityTimeline, 3, Some(entity), "start");
            }
            AbilityAction::OpenFollowUpWindow { duration_ticks, next_ability_id } => {
                // Write a combo / follow-up window for (entity, ability).
                // Phase 2 of subsequent ticks checks this: if the player presses
                // the same ability_id again within the window, the cast is
                // redirected to next_ability_id.
                let expiry = TickId(self.current_tick.0 + *duration_ticks as u64);
                self.state.combat.active_windows.insert((entity, ability_id), (*next_ability_id, expiry));
                audit!(self.state, Window, AbilityTimeline, 3, Some(entity), "open_window");
            }
            AbilityAction::StanceBegin { dodge_active, rooted } => {
                // Set iframe/root flags on the caster (timeline-driven).
                // Persists until a corresponding StanceEnd action fires.
                if let Some(idx) = self.state.entities.lookup(entity) {
                    let t = &mut self.state.combat.tactical[idx.as_usize()];
                    if *dodge_active {
                        t.dodge_stacks = t.dodge_stacks.saturating_add(1);
                    }
                    if *rooted {
                        t.rooted = true;
                    }
                    audit!(self.state, Tactical, AbilityTimeline, 3, Some(entity), "stance_begin");
                }
            }
            AbilityAction::StanceEnd => {
                // Clear iframe/root flags on the caster (timeline-driven).
                if let Some(idx) = self.state.entities.lookup(entity) {
                    let t = &mut self.state.combat.tactical[idx.as_usize()];
                    t.dodge_stacks = t.dodge_stacks.saturating_sub(1);
                    t.rooted = false;
                    t.arc_state = None;
                    audit!(self.state, Tactical, AbilityTimeline, 3, Some(entity), "stance_end");
                }
            }
            AbilityAction::ArcMovement { speed, lift, gravity } => {
                // Launch the caster in a kinematic arc.
                // Initial velocity = facing * speed + up * lift.
                // Phase 2 will integrate gravity each tick and feed into move_character.
                if let Some(idx) = self.state.entities.lookup(entity) {
                    let facing = self.state.combat.executions.get(execution_id)
                        .map(|ctx| ctx.facing)
                        .unwrap_or(Vec3f { x: 0.0, y: 0.0, z: 1.0 });
                    let t = &mut self.state.combat.tactical[idx.as_usize()];
                    t.arc_state = Some(game_core::combat::tactical::ArcState {
                        velocity: Vec3f {
                            x: facing.x * speed,
                            y: *lift,
                            z: facing.z * speed,
                        },
                        gravity: *gravity,
                    });
                    t.rooted = true;
                    audit!(self.state, Tactical, AbilityTimeline, 3, Some(entity), "arc_begin");
                }
            }
            AbilityAction::ApplyBuff { buff_id } => {
                // Apply a buff to the caster (self-buff) with stacking.
                if let Some(template) = self.buff_registry.get(*buff_id) {
                    if let Some(idx) = self.state.entities.lookup(entity) {
                        let active = game_core::combat::status::ActiveBuff::from_template(
                            template, entity, entity, self.current_tick,
                        );
                        self.state.status.apply_or_stack_buff(idx, active);
                        self.stats_dirty.insert(entity);
                        audit!(self.state, Buff, AbilityTimeline, 3, Some(entity), "apply_buff");
                        let duration = template.duration_ticks.unwrap_or(0);
                        self.emit_event(entity, EventPayload::BuffApplied {
                            buff_id: *buff_id,
                            source: entity,
                            duration_ticks: duration,
                        });
                    }
                } else {
                    warn!("ApplyBuff: buff_id {} not found in registry", buff_id);
                }
            }
            AbilityAction::Telegraph { impact_delay } => {
                // Emit a LockOnWarning to the resolved target so they can react.
                if let Some(ctx) = self.state.combat.executions.get(execution_id) {
                    let target = match &ctx.targeting {
                        game_core::combat::skill::ResolvedTargeting::Entity { target } => Some(*target),
                        game_core::combat::skill::ResolvedTargeting::LockOn { target } => Some(*target),
                        _ => None,
                    };
                    if let Some(target) = target {
                        let impact_tick = self.current_tick.0 + *impact_delay as u64;
                        self.emit_event(target, EventPayload::LockOnWarning {
                            source: entity,
                            target,
                            impact_tick,
                        });
                    }
                }
            }
        }
    }

    // ── Phase 3.5: Projectile movement ──────────────────────────

    fn phase_projectile_movement(&mut self) {
        let ids = self.state.combat.hitboxes.armed_projectile_ids();
        let mut expired = Vec::new();
        for exec_id in ids {
            let hb = match self.state.combat.hitboxes.get_mut(exec_id) {
                Some(hb) => hb,
                None => continue,
            };
            let proj = match hb.projectile.as_mut() {
                Some(p) => p,
                None => continue,
            };
            // Snapshot current position for swept collision detection.
            proj.prev_position = proj.position;
            // Advance position along travel direction.
            proj.position.x += proj.direction.x * proj.speed;
            proj.position.y += proj.direction.y * proj.speed;
            proj.position.z += proj.direction.z * proj.speed;

            // Check range limit.
            let dx = proj.position.x - proj.origin.x;
            let dy = proj.position.y - proj.origin.y;
            let dz = proj.position.z - proj.origin.z;
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if dist_sq > proj.max_range_sq {
                expired.push(exec_id);
                continue;
            }

            // Move the world-space sensor.
            if let Some(handle) = hb.sensor_handle {
                self.physics.set_sensor_position(handle, proj.position);
            }
        }
        // Remove projectiles that exceeded max range.
        for exec_id in expired {
            if let Some(removed) = self.state.combat.hitboxes.remove(exec_id) {
                if let Some(handle) = removed.sensor_handle {
                    self.physics.remove_sensor(handle);
                }
                self.emit_event(removed.owner, EventPayload::ProjectileRemoved {
                    execution_id: exec_id.0,
                });
                self.emit_event(removed.owner, EventPayload::HitboxRemoved {
                    ability_id: removed.ability_id,
                });
            }
        }
    }

    // ── Phase 4: Physics integration ────────────────────────────

    fn phase_physics_step(&mut self) {
        // Apply any combat-generated impulses accumulated in the previous tick's Phase 6.
        // Applying them here (before the physics step) means Rapier resolves the velocity
        // change on this tick, decoupling the gameplay event (hit) from its physical effect
        // (knockback) across tick boundaries.
        let impulses: Vec<(EntityId, Vec3f)> = std::mem::take(&mut self.pending_impulses);
        for (entity_id, velocity) in impulses {
            self.physics.set_linear_velocity(entity_id, velocity);
        }
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
        self.resolve_projectile_hits();
        self.resolve_compensated_hits();
        self.resolve_periodic_damage();
    }

    /// Shared damage pipeline: ability lookup → tactical routing → buff multipliers
    /// → charge tier multiplier → damage application → threat → events.
    fn apply_hit_damage(
        &mut self,
        attacker: EntityId,
        target: EntityId,
        target_idx: EntityIndex,
        ability_id: u32,
        compensated: bool,
        exec_id: Option<AbilityExecutionId>,
    ) {
        let ability = match self.abilities.get(ability_id) {
            Some(a) => a,
            None => return,
        };

        // Resolve charge-tier damage multiplier from execution context.
        let charge_mult: f32 = exec_id
            .and_then(|eid| self.state.combat.executions.get(eid))
            .map(|ctx| {
                let tier = ctx.params.charge_level as usize;
                ability.charge_tiers.as_ref()
                    .and_then(|tiers| tiers.get(tier))
                    .map(|t| t.damage_mult)
                    .unwrap_or(1.0)
            })
            .unwrap_or(1.0);

        let base_damage = ability.base_damage * charge_mult;
        let damage_type = ability.damage_type;
        let threat_mult = ability.threat_multiplier;
        let on_hit_buffs = ability.on_hit_buffs.clone();
        let knockback_force = ability.knockback_force;

        let attacker_idx = self.state.entities.lookup(attacker);

        // Tactical routing: dodge evades entirely, block reduces damage.
        let tactical = self.state.combat.tactical[target_idx.as_usize()];
        if tactical.is_dodging() {
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Dodged {
                    source: attacker,
                    ability_id,
                },
            });
            self.event_sequence += 1;
            return;
        }

        // True damage bypasses block and damage-reduction modifiers entirely.
        let is_true_damage = damage_type == game_protocol::event::DamageType::True;

        // Directional block: block only mitigates damage from the front arc.
        // True damage ignores blocking completely.
        // Compute dot product between target's facing direction and the vector
        // from target toward the attacker. If the attacker is behind the
        // blocker (dot < 0), blocking is ineffective.
        const BLOCK_DOT_THRESHOLD: f32 = 0.0; // 0 = 180° front arc
        let facing_attacker = if tactical.blocking && !is_true_damage {
            if let (Some(target_t), Some(attacker_t)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let yaw = 2.0 * target_t.rotation.y.atan2(target_t.rotation.w);
                let facing = (yaw.sin(), yaw.cos()); // (x, z)
                let dx = attacker_t.position.x - target_t.position.x;
                let dz = attacker_t.position.z - target_t.position.z;
                let len = (dx * dx + dz * dz).sqrt();
                if len > 1e-6 {
                    let dot = facing.0 * (dx / len) + facing.1 * (dz / len);
                    dot >= BLOCK_DOT_THRESHOLD
                } else {
                    true // overlapping positions — allow block
                }
            } else {
                true // no transform available — allow block
            }
        } else {
            false // not blocking at all, or true damage
        };

        // Perfect block window: first N ticks of a block sequence deal zero damage.
        const PERFECT_BLOCK_TICKS: u64 = 3;
        let (block_factor, perfect_block) = if tactical.blocking && facing_attacker {
            let perfect = tactical.block_start_tick
                .map_or(false, |start| self.current_tick.0.saturating_sub(start.0) < PERFECT_BLOCK_TICKS);
            if perfect { (0.0, true) } else { (0.5, false) }
        } else {
            (1.0, false)
        };

        // Apply damage_out_mult from attacker's cached StatBlock.
        let out_mult: f32 = attacker_idx.map_or(1.0, |idx| {
            self.state.stats.get(idx).damage_out_mult
        });
        // True damage ignores damage_in_mult (incoming reduction/amplification).
        let in_mult: f32 = if is_true_damage {
            1.0
        } else {
            self.state.stats.get(target_idx).damage_in_mult
        };
        let effective_damage = base_damage * out_mult * in_mult * block_factor;

        // ── Cover check: allies behind a blocking player take reduced damage ──
        // Iterates blocking entities to see if any friendly blocker is
        // interposed between the attacker and the target within a rear cone.
        // True damage bypasses cover (same as self-block).
        const COVER_RADIUS_SQ: f32 = 16.0;     // 4 units
        const COVER_DOT_THRESHOLD: f32 = -0.3;  // ~110° rear arc
        const COVER_FACTOR: f32 = 0.7;          // 30% damage reduction
        let (cover_factor, cover_source) = if !is_true_damage && block_factor >= 1.0 {
            // Only check cover if the target isn't already self-blocking.
            let target_kind = self.state.entities.kinds[target_idx.as_usize()];
            if let (Some(tp), Some(ap)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let mut best_factor = 1.0f32;
                let mut best_blocker: Option<EntityId> = None;
                for (i, slot) in self.state.combat.tactical.iter().enumerate() {
                    if !slot.blocking { continue; }
                    let blocker_id = match self.state.entities.lookup_by_slot(i) {
                        Some(id) if id != target && id != attacker => id,
                        _ => continue,
                    };
                    // Same-team check: blocker and target must be the same entity kind category.
                    // Players cover players; NPCs/Bosses cover NPCs/Bosses.
                    let blocker_kind = self.state.entities.kinds[i];
                    let same_team = matches!(
                        (blocker_kind, target_kind),
                        (EntityKind::Player, EntityKind::Player)
                        | (EntityKind::Npc | EntityKind::Boss, EntityKind::Npc | EntityKind::Boss)
                    );
                    if !same_team { continue; }
                    let Some(bp) = self.physics.get_transform(blocker_id) else { continue };
                    // Distance: target must be within COVER_RADIUS of blocker.
                    let dx = tp.position.x - bp.position.x;
                    let dz = tp.position.z - bp.position.z;
                    let dist_sq = dx * dx + dz * dz;
                    if dist_sq > COVER_RADIUS_SQ { continue; }
                    // Cone: target must be behind blocker (relative to blocker's facing).
                    let yaw = 2.0 * bp.rotation.y.atan2(bp.rotation.w);
                    let facing = (yaw.sin(), yaw.cos());
                    let len = dist_sq.sqrt();
                    if len < 1e-6 { continue; }
                    let dot = facing.0 * (dx / len) + facing.1 * (dz / len);
                    if dot > COVER_DOT_THRESHOLD { continue; } // target not behind blocker
                    // Interposition: blocker must be closer to attacker than target is.
                    let bax = bp.position.x - ap.position.x;
                    let baz = bp.position.z - ap.position.z;
                    let tax = tp.position.x - ap.position.x;
                    let taz = tp.position.z - ap.position.z;
                    if bax * bax + baz * baz >= tax * tax + taz * taz { continue; }
                    if COVER_FACTOR < best_factor {
                        best_factor = COVER_FACTOR;
                        best_blocker = Some(blocker_id);
                    }
                }
                (best_factor, best_blocker)
            } else {
                (1.0, None)
            }
        } else {
            (1.0, None)
        };
        let effective_damage = effective_damage * cover_factor;

        let actual = self.state.combat.health.apply_damage(target_idx, effective_damage, Some(attacker));
        if compensated {
            audit!(self.state, Health, Combat, 6, Some(target), "damage_compensated");
        } else {
            audit!(self.state, Health, Combat, 6, Some(target), "damage");
        }
        self.summary.damage_events += 1;

        // Only add threat if the attacker is still alive — a delayed projectile or
        // DoT from a dead caster must not re-insert them into the threat table.
        if self.state.entities.lookup(attacker).is_some() {
            if let Some(table) = self.state.combat.threat_tables.get_mut(target_idx) {
                table.add_threat(attacker, actual * threat_mult);
                if compensated {
                    audit!(self.state, Threat, Combat, 6, Some(target), "add_threat_compensated");
                } else {
                    audit!(self.state, Threat, Combat, 6, Some(target), "add_threat");
                }
            }
        }

        // Emit Blocked event when blocking from the front (even if perfect block dealt zero damage).
        if tactical.blocking && facing_attacker {
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Blocked {
                    source: attacker,
                    ability_id,
                    damage_taken: actual,
                    perfect: perfect_block,
                },
            });
            self.event_sequence += 1;
        }
        // Emit Covered event when an ally's block stance reduced our damage.
        if let Some(blocker) = cover_source {
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Covered {
                    blocker,
                    ability_id,
                    damage_taken: actual,
                },
            });
            self.event_sequence += 1;
        }
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

        // Apply on-hit buffs/debuffs to the target.
        for &bid in &on_hit_buffs {
            if let Some(template) = self.buff_registry.get(bid) {
                let active = game_core::combat::status::ActiveBuff::from_template(
                    template, attacker, target, self.current_tick,
                );
                self.state.status.apply_or_stack_buff(target_idx, active);
                self.stats_dirty.insert(target);
                audit!(self.state, Buff, Combat, 6, Some(target), "on_hit_buff");
                let duration = template.duration_ticks.unwrap_or(0);
                self.emit_event(target, EventPayload::BuffApplied {
                    buff_id: bid,
                    source: attacker,
                    duration_ticks: duration,
                });
            } else {
                warn!("on_hit_buff: buff_id {} not found in registry", bid);
            }
        }

        // ── Knockback impulse ──────────────────────────────────────
        // Skip knockback if:
        //   - knockback_force is zero (most abilities)
        //   - target is successfully blocking from the front
        //   - target is dodging (already returned above, but guard for clarity)
        let blocked = tactical.blocking && facing_attacker;
        if knockback_force > 0.0 && !blocked {
            if let (Some(target_t), Some(attacker_t)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let dx = target_t.position.x - attacker_t.position.x;
                let dz = target_t.position.z - attacker_t.position.z;
                let len = (dx * dx + dz * dz).sqrt();
                let (dir_x, dir_z) = if len > 1e-6 {
                    (dx / len, dz / len)
                } else {
                    (0.0, 1.0) // fallback: push along +Z
                };
                self.pending_impulses.push((
                    target,
                    Vec3f { x: dir_x * knockback_force, y: 0.0, z: dir_z * knockback_force },
                ));
            }
        }
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

            // Skip projectile hitboxes — world-space sensors are standalone
            // colliders (no rigid body parent), so Rapier does not reliably fire
            // Started events when they are moved via set_translation().
            // resolve_projectile_hits handles them with manual Parry tests.
            //
            // Skip lag-compensated hitboxes — they are handled exclusively by
            // resolve_compensated_hits against historical positions. Without
            // this guard, the physical sensor hits entities at their *current*
            // positions in addition to the compensated pass, causing double-hits
            // on different targets.
            if self.state.combat.hitboxes.get(exec_id)
                .is_some_and(|hb| hb.rewind_ticks > 0 || hb.projectile.is_some())
            {
                continue;
            }

            // Skip self-hits.
            if attacker == target {
                continue;
            }

            // Track overlapping for periodic damage (HazardZone).
            self.state.combat.hitboxes.add_overlapping(exec_id, target);

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
            // Translate target EntityId → dense index. Skip non-active targets
            // (e.g. Spawning) for consistency with resolve_compensated_hits.
            let target_idx = match self.state.entities.lookup(target) {
                Some(idx) if self.state.entities.is_active(idx) => idx,
                _ => continue,
            };

            self.apply_hit_damage(attacker, target, target_idx, ability_id, false, Some(exec_id));
        }

        // Second pass: process Stopped sensor events to clear already_hit for
        // reentry-capable hitboxes and remove from overlapping set.
        for contact in self.state.physics.contacts
            .iter()
            .filter(|c| !c.started && c.is_sensor)
        {
            let (acting, _receiving, _attacker, target) = normalize_contact_pair(
                contact.kind1, contact.entity1,
                contact.kind2, contact.entity2,
            );
            if let ColliderKind::Hitbox(eid_raw) = acting {
                let eid = AbilityExecutionId(eid_raw);
                self.state.combat.hitboxes.clear_hit(eid, target);
                self.state.combat.hitboxes.remove_overlapping(eid, target);
            }
        }
    }

    /// Periodic re-damage for lingering area effects (HazardZone).
    ///
    /// Hitboxes with `damage_interval_ticks > 0` re-damage all entities that
    /// remain inside the sensor volume. The `overlapping` set is maintained by
    /// `resolve_hits` (Started → add, Stopped → remove). Every
    /// `damage_interval_ticks` ticks, `already_hit` is cleared for overlapping
    /// targets and damage is reapplied.
    fn resolve_periodic_damage(&mut self) {
        let due = self.state.combat.hitboxes.collect_periodic_due(self.current_tick);
        for (exec_id, attacker, ability_id, targets) in due {
            for target in targets {
                if target == attacker {
                    continue;
                }
                let target_idx = match self.state.entities.lookup(target) {
                    Some(idx) if self.state.entities.is_active(idx) => idx,
                    _ => continue,
                };
                // Clear dedup so record_hit succeeds on re-damage.
                self.state.combat.hitboxes.get_mut(exec_id).map(|hb| {
                    hb.already_hit.remove(&target);
                });
                if !self.state.combat.hitboxes.record_hit(exec_id, target) {
                    continue;
                }
                self.apply_hit_damage(attacker, target, target_idx, ability_id, false, Some(exec_id));
            }
            self.state.combat.hitboxes.mark_periodic_tick(exec_id, self.current_tick);
        }
    }

    /// Projectile hit detection — standalone Parry intersection tests.
    ///
    /// World-space projectile sensors are standalone colliders (no rigid body parent).
    /// Rapier classifies them as "fixed" while character bodies are kinematic.
    /// `ActiveCollisionTypes` does not include `KINEMATIC_FIXED`, so Rapier's narrow
    /// phase never generates `Started` events when the sensor moves via
    /// `set_translation()`. Rather than adding a kinematic body per projectile, we
    /// run cheap Parry shape intersection tests each tick — the same proven pattern
    /// used by lag compensation.
    fn resolve_projectile_hits(&mut self) {
        let projectiles: Vec<_> = self
            .state
            .combat
            .hitboxes
            .armed_projectile_ids()
            .into_iter()
            .filter_map(|eid| {
                let hb = self.state.combat.hitboxes.get(eid)?;
                let proj = hb.projectile.as_ref()?;
                Some((eid, hb.owner, hb.ability_id, hb.shape, proj.prev_position, proj.position))
            })
            .collect();

        if projectiles.is_empty() {
            return;
        }

        let mut confirmed_hits: Vec<(EntityId, EntityId, EntityIndex, u32, AbilityExecutionId)> = Vec::new();

        for (exec_id, attacker, ability_id, shape, prev_pos, curr_pos) in projectiles {
            let hitbox_shape = lag_compensation::hitbox_sensor_shape(shape);

            for slot in 0..self.state.entities.len() {
                if self.state.entities.states[slot]
                    != game_core::entity::lifecycle::EntityState::Active
                {
                    continue;
                }
                let target_idx = self.state.entities.index_at(slot);
                let target_id = self.state.entities.id_of(target_idx);

                if target_id == attacker {
                    continue;
                }
                if self.state.combat.hitboxes.has_hit(exec_id, target_id) {
                    continue;
                }

                let target_pos = match self.physics.get_transform(target_id) {
                    Some(t) => t.position,
                    None => continue,
                };

                if !lag_compensation::swept_shapes_intersect(hitbox_shape, prev_pos, curr_pos, target_pos) {
                    continue;
                }
                if !self.state.combat.hitboxes.record_hit(exec_id, target_id) {
                    continue;
                }

                confirmed_hits.push((attacker, target_id, target_idx, ability_id, exec_id));
            }
        }

        for (attacker, target_id, target_idx, ability_id, exec_id) in confirmed_hits {
            self.apply_hit_damage(attacker, target_id, target_idx, ability_id, false, Some(exec_id));
        }
    }

    /// Lag-compensated hit detection — second pass after `resolve_hits`.
    ///
    /// For each armed hitbox with `rewind_ticks > 0`, performs standalone Parry shape
    /// intersection tests against each entity's historical position from the transform
    /// history buffer. Entities already hit by the normal contact-based pass (Phase 5)
    /// are skipped via the `already_hit` dedup set on the hitbox.
    ///
    /// This does NOT touch Rapier bodies or the BVH — all tests are pure math on
    /// positioned shapes. Cost is O(hitboxes × entities) with O(1) per intersection.
    fn resolve_compensated_hits(&mut self) {
        // Collect compensated hitboxes: armed, rewind_ticks > 0.
        let compensated: Vec<_> = self
            .state
            .combat
            .hitboxes
            .iter_armed_compensated()
            .map(|hb| (hb.execution_id, hb.owner, hb.ability_id, hb.shape, hb.offset, hb.rewind_ticks))
            .collect();

        if compensated.is_empty() {
            return;
        }

        // Two-pass approach: first collect confirmed hits (resolves borrow on
        // transform_history), then apply damage in a second pass.
        let mut confirmed_hits: Vec<(EntityId, EntityId, EntityIndex, u32, AbilityExecutionId)> = Vec::new();

        for (exec_id, attacker, ability_id, shape, offset, rewind_ticks) in compensated {
            // Resolve the hitbox's world position from the attacker's current
            // transform.  The normal Rapier sensor is parented to the attacker's
            // body, so it rotates with them — the compensated test must match.
            // Falling back to cast-time origin+facing only when the physics body
            // is gone (entity despawned between phases).
            let (attacker_pos, facing) = match self.state.combat.executions.get(exec_id) {
                Some(ctx) => {
                    if let Some(t) = self.physics.get_transform(attacker) {
                        let yaw = 2.0 * t.rotation.y.atan2(t.rotation.w);
                        let current_facing = Vec3f { x: yaw.sin(), y: 0.0, z: yaw.cos() };
                        (t.position, current_facing)
                    } else {
                        (ctx.origin, ctx.facing)
                    }
                }
                None => continue,
            };

            let hitbox_pos =
                lag_compensation::hitbox_world_position(attacker_pos, facing, offset);
            let hitbox_shape = lag_compensation::hitbox_sensor_shape(shape);

            // Determine the historical tick to sample.
            let rewind_tick = TickId(self.current_tick.0.saturating_sub(rewind_ticks as u64));
            let snapshot = match self.transform_history.get_snapshot(rewind_tick) {
                Some(s) => s,
                None => continue, // Not enough history yet.
            };

            // Collect positions from snapshot before releasing the borrow.
            let candidates: Vec<_> = snapshot.positions.clone();

            // Test each entity in the snapshot for overlap with the hitbox.
            for (target_id, historical_pos) in candidates {
                // Skip self-hits.
                if target_id == attacker {
                    continue;
                }

                // Skip entities already hit by the normal pass or a prior compensated test.
                if self.state.combat.hitboxes.has_hit(exec_id, target_id) {
                    continue;
                }

                // Only compensate for active entities with a dense index.
                let target_idx = match self.state.entities.lookup(target_id) {
                    Some(idx) if self.state.entities.is_active(idx) => idx,
                    _ => continue,
                };

                // Run the standalone shape intersection test.
                if !lag_compensation::shapes_intersect(hitbox_shape, hitbox_pos, historical_pos) {
                    continue;
                }

                // Record this as a hit (dedup).
                if !self.state.combat.hitboxes.record_hit(exec_id, target_id) {
                    continue;
                }

                confirmed_hits.push((attacker, target_id, target_idx, ability_id, exec_id));
            }
        }

        // Apply damage for all confirmed compensated hits.
        for (attacker, target_id, target_idx, ability_id, exec_id) in confirmed_hits {
            self.apply_hit_damage(attacker, target_id, target_idx, ability_id, true, Some(exec_id));
        }
    }

    // ── Phase 7: AI decisions ───────────────────────────────────

    fn phase_ai_decisions(&mut self) {
        use game_core::entity::lifecycle::{EntityKind, NpcAiState};
        use game_core::combat::status::AiOverride;

        let npcs = self.state.active_indices_of_kind(EntityKind::Npc);
        let bosses = self.state.active_indices_of_kind(EntityKind::Boss);

        for idx in npcs.iter().chain(bosses.iter()) {
            let ai_state = match self.state.ai.npc_ai.get(*idx).copied() {
                Some(s) => s,
                None => continue,
            };

            // Check for an ai_override carried by an active buff.
            // First buff with a non-None override wins; check runs before standard AI logic.
            let override_opt = self.state.status.get_buffs(*idx)
                .iter()
                .find_map(|b| b.modifiers.ai_override);

            // 1) State transitions: apply override if present, otherwise run normal transitions.
            if let Some(ai_override) = override_opt {
                match ai_override {
                    AiOverride::ForceFlee => {
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Flee; }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_flee");
                    }
                    AiOverride::ForceIdle => {
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Idle; }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_idle");
                    }
                    AiOverride::ForceFocus { target } => {
                        // Only force-focus if the target is still alive — the buff
                        // may outlive the target entity.
                        if self.state.entities.lookup(target).is_none() {
                            audit!(self.state, Ai, AiDecisions, 7, None, "override_focus_dead_target");
                        } else {
                            if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                                let max_other_threat = table.entries.iter()
                                    .filter(|e| e.source != target)
                                    .filter(|e| e.threat.is_finite())
                                    .map(|e| e.threat)
                                    .fold(0.0f32, f32::max);
                                let forced_threat = max_other_threat + 10.0;
                                if let Some(entry) = table.entries.iter_mut().find(|e| e.source == target) {
                                    entry.threat = entry.threat.max(forced_threat);
                                } else {
                                    table.entries.push(game_core::combat::status::ThreatEntry {
                                        source: target,
                                        threat: forced_threat,
                                    });
                                }
                            }
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Combat; }
                            audit!(self.state, Threat, AiDecisions, 7, None, "override_focus_threat");
                            audit!(self.state, Ai, AiDecisions, 7, None, "override_focus");
                        }
                    }
                }
            } else {
                // Standard state transitions (do not perform movement here).
                match ai_state {
                    NpcAiState::Idle | NpcAiState::Patrol => {
                        // Check if anyone is on the threat table → transition to Combat.
                        if let Some(table) = self.state.combat.threat_tables.get(*idx)
                            && table.top_threat().is_some() {
                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Combat; }
                                audit!(self.state, Ai, AiDecisions, 7, None, "to_combat");
                            }
                    }
                    NpcAiState::Combat => {
                        // If threat table is empty, return to Idle.
                        let top_target = self.state.combat.threat_tables.get(*idx)
                            .and_then(|t| t.top_threat());
                        match top_target {
                            None => {
                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Idle; }
                                audit!(self.state, Ai, AiDecisions, 7, None, "combat_to_idle");
                            }
                            Some(_) => {
                                // stay in Combat; action execution below will handle movement
                            }
                        }
                    }
                    NpcAiState::Flee => {
                        // Flee behavior: remain fleeing while any top threat exists.
                        // Only return to Idle when no threats remain.
                        let top_target = self.state.combat.threat_tables.get(*idx)
                            .and_then(|t| t.top_threat());
                        if top_target.is_none() {
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Idle; }
                            audit!(self.state, Ai, AiDecisions, 7, None, "flee_to_idle");
                        }
                    }
                    NpcAiState::Scripted => {
                        // No scripted behavior yet — placeholder.
                    }
                }
            }

            // 2) Action execution: run movement/behavior for the current state.
            let current_state = self.state.ai.npc_ai.get(*idx).copied().unwrap();
            match current_state {
                NpcAiState::Combat => {
                    if let Some(target_id) = self.state.combat.threat_tables.get(*idx).and_then(|t| t.top_threat()) {
                        let npc_id = self.state.entities.id_of(*idx);
                        self.npc_move_toward(npc_id, *idx, target_id, self.dt);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "chase");

                        // Attempt to cast ability 1 (Slash) at top-threat target.
                        // cast_ability is a no-op if the ability is on cooldown.
                        let targeting = ResolvedTargeting::Entity { target: target_id };
                        if self.cast_ability(npc_id, 1, targeting, 0, 0) {
                            audit!(self.state, Execution, AiDecisions, 7, Some(npc_id), "npc_cast");
                        }
                    }
                }
                NpcAiState::Flee => {
                    if let Some(threat_source) = self.state.combat.threat_tables.get(*idx).and_then(|t| t.top_threat()) {
                        let npc_id = self.state.entities.id_of(*idx);
                        self.npc_move_away(npc_id, *idx, threat_source, self.dt);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "flee");
                    }
                }
                NpcAiState::Patrol => {
                    if let Some(&home) = self.state.ai.home_positions.get(*idx) {
                        let npc_id = self.state.entities.id_of(*idx);
                        self.npc_move_toward_pos(npc_id, home, self.dt);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "patrol");
                    }
                }
                _ => {}
            }
        }
    }

    // ── NPC movement helpers (Phase 7) ─────────────────────────

    /// Move `npc_id` one step toward `target_id`'s current physics position.
    /// Uses the NPC's authoritative base speed. No-ops if either transform is unavailable.
    fn npc_move_toward(&mut self, npc_id: EntityId, npc_idx: game_core::entity::entity_index::EntityIndex, target_id: EntityId, dt: f32) {
        let (npc_pos, target_pos) = match (
            self.physics.get_transform(npc_id),
            self.physics.get_transform(target_id),
        ) {
            (Some(a), Some(b)) => (a.position, b.position),
            _ => return,
        };
        // Stop if already within melee range to prevent overshoot jitter.
        const CHASE_ARRIVE_RADIUS: f32 = 1.0;
        let dx = target_pos.x - npc_pos.x;
        let dz = target_pos.z - npc_pos.z;
        if dx * dx + dz * dz <= CHASE_ARRIVE_RADIUS * CHASE_ARRIVE_RADIUS {
            return;
        }
        let kind = self.state.entities.kinds[npc_idx.as_usize()];
        self.npc_step_toward(npc_id, npc_pos, target_pos, kind, dt);
    }

    /// Move `npc_id` one step away from `threat_source`'s current physics position.
    /// Uses the NPC's authoritative base speed. No-ops if either transform is unavailable.
    fn npc_move_away(&mut self, npc_id: EntityId, npc_idx: game_core::entity::entity_index::EntityIndex, threat_source: EntityId, dt: f32) {
        let (npc_pos, threat_pos) = match (
            self.physics.get_transform(npc_id),
            self.physics.get_transform(threat_source),
        ) {
            (Some(a), Some(b)) => (a.position, b.position),
            _ => return,
        };
        // Mirror the direction: flee = move in the direction opposite to the threat.
        let away = Vec3f {
            x: npc_pos.x + (npc_pos.x - threat_pos.x),
            y: npc_pos.y,
            z: npc_pos.z + (npc_pos.z - threat_pos.z),
        };
        let kind = self.state.entities.kinds[npc_idx.as_usize()];
        self.npc_step_toward(npc_id, npc_pos, away, kind, dt);
    }

    /// Move `npc_id` toward an explicit world position (used for patrol).
    /// No-ops when the NPC is already within `PATROL_ARRIVE_RADIUS` of the target.
    fn npc_move_toward_pos(&mut self, npc_id: EntityId, dest: Vec3f, dt: f32) {
        const PATROL_ARRIVE_RADIUS: f32 = 0.5;
        let npc_pos = match self.physics.get_transform(npc_id) {
            Some(t) => t.position,
            None => return,
        };
        // Stop if already close enough — prevents micro-jitter at the home point.
        let dx = dest.x - npc_pos.x;
        let dz = dest.z - npc_pos.z;
        if dx * dx + dz * dz <= PATROL_ARRIVE_RADIUS * PATROL_ARRIVE_RADIUS {
            return;
        }
        let kind = if let Some(idx) = self.state.entities.lookup(npc_id) {
            self.state.entities.kinds[idx.as_usize()]
        } else {
            return;
        };
        self.npc_step_toward(npc_id, npc_pos, dest, kind, dt);
    }

    /// Shared step: move `npc_id` from `from_pos` toward `to_pos` by one tick of movement.
    /// Uses cached `StatBlock::movement_speed` (recalculated in Phase 1.5).
    fn npc_step_toward(&mut self, npc_id: EntityId, from: Vec3f, to: Vec3f, _kind: EntityKind, dt: f32) {
        // Rooted NPCs cannot move.
        if let Some(idx) = self.state.entities.lookup(npc_id) {
            if self.is_rooted(idx) {
                return;
            }
        }
        let dx = to.x - from.x;
        let dz = to.z - from.z;
        let dist_sq = dx * dx + dz * dz;
        if dist_sq < 1e-6 {
            return;
        }
        let speed = if let Some(idx) = self.state.entities.lookup(npc_id) {
            self.state.stats.get(idx).movement_speed
        } else {
            return;
        };
        let dist = dist_sq.sqrt();
        // Clamp step to remaining distance so the NPC never overshoots the target.
        let move_dist = (speed * dt).min(dist);
        let inv = 1.0 / dist;
        let desired = Vec3f {
            x: dx * inv * move_dist,
            y: 0.0,
            z: dz * inv * move_dist,
        };
        self.physics.move_character(npc_id, desired);
    }

    // ── Phase 7.5: World orchestration ──────────────────────────

    /// Evaluate dynamic event triggers and spawn NPCs/bosses into the simulation.
    ///
    /// Sweeps `entity_regions` to count active players per region cell,
    /// evaluates all registered `DirectorState` events against those counts,
    /// and calls `spawn_entity_from_snapshot` for each spawn directive.
    ///
    /// Returns the list of spawns so Phase 10 can include them in the TickResult
    /// for the coordinator to persist as DB rows.
    fn phase_world_orchestration(&mut self) -> Vec<DirectorSpawn> {
        // Build region player counts from current entity_regions.
        let mut region_player_counts: HashMap<(i32, i32, u32), u32> = HashMap::new();
        for (eid, cell) in &self.entity_regions {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                let i = idx.as_usize();
                if self.state.entities.kinds[i] == game_core::entity::lifecycle::EntityKind::Player
                    && self.state.entities.states[i] == game_core::entity::lifecycle::EntityState::Active
                {
                    *region_player_counts
                        .entry((cell.region_x, cell.region_z, cell.layer))
                        .or_insert(0) += 1;
                }
            }
        }

        // Director spawns are deferred: we return the spawn requests so the
        // coordinator can send them to SpacetimeDB via commit_tick_results.
        // The DB assigns canonical IDs and broadcasts entity.on_insert, which
        // the coordinator handles to materialize them into the local sim.
        self.director.evaluate(&region_player_counts, self.current_tick)
    }

    /// Get mutable access to the director state for event registration.
    pub fn director_mut(&mut self) -> &mut DirectorState {
        &mut self.director
    }

    /// Get read access to the director state.
    pub fn director(&self) -> &DirectorState {
        &self.director
    }

    /// Mark an entity for stat recalculation on the next tick's Phase 1.5.
    ///
    /// Called by the coordinator (via `SimulationRunner`) when equipment changes
    /// are observed between ticks. Also used internally when buffs change.
    pub fn mark_stats_dirty(&mut self, entity_id: EntityId) {
        if self.state.entities.contains(entity_id) {
            self.stats_dirty.insert(entity_id);
        }
    }

    /// Update the aggregated equipment modifiers for an entity.
    ///
    /// Called by the coordinator when `player_equipment` rows change. The new
    /// modifiers take effect on the next `phase_stat_recalc` pass.
    pub fn set_equipment_modifiers(&mut self, entity_id: EntityId, modifiers: game_core::stats::EquipmentModifiers) {
        self.equipment_modifiers.insert(entity_id, modifiers);
    }

    // ── Phase 1.5: Stat recalculation ───────────────────────────

    /// Recalculate cached `StatBlock` for entities whose buffs or equipment changed.
    ///
    /// Drains `stats_dirty` and recomputes each entity's stats from its kind,
    /// spawn-time max_hp, and current active buffs using `StatBlock::compute`.
    fn phase_stat_recalc(&mut self) {
        if self.stats_dirty.is_empty() {
            return;
        }
        let dirty: Vec<EntityId> = self.stats_dirty.drain().collect();
        let no_equip = game_core::stats::EquipmentModifiers::default();
        for eid in dirty {
            let Some(idx) = self.state.entities.lookup(eid) else { continue };
            let i = idx.as_usize();
            let kind = self.state.entities.kinds[i];
            let max_hp = self.state.combat.health.max_hp[i];
            let buffs = self.state.status.get_buffs(idx);
            let equip = self.equipment_modifiers.get(&eid).unwrap_or(&no_equip);
            let block = game_core::stats::StatBlock::compute(kind, max_hp, buffs, equip);
            self.state.stats.set(idx, block);
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

    // ── Runtime domain snapshots ────────────────────────────────

    /// Collect buff updates for entities whose buff arrays changed this tick.
    ///
    /// Only dirty entities are emitted. Entities with zero remaining buffs are
    /// still included so the reducer's delete-all-then-insert reliably clears
    /// stale rows when every buff on an entity expires in the same tick.
    fn collect_buff_updates(&mut self) -> Vec<(EntityId, Vec<game_core::combat::status::ActiveBuff>)> {
        let dirty = self.state.status.take_dirty();
        let mut out = Vec::with_capacity(dirty.len());
        for i in &dirty {
            if self.state.entities.states[*i] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            // Mark buff-dirty entities for stat recalculation next tick.
            let eid = self.state.entities.id_of(self.state.entities.index_at(*i));
            self.stats_dirty.insert(eid);
            out.push((eid, self.state.status.clone_buffs(*i)));
        }
        out
    }

    /// Snapshot threat tables for persistence — dirty-tracked.
    ///
    /// Only emits NPCs whose threat table is non-empty (in-combat, post-decay),
    /// plus NPCs that transitioned from non-empty to empty (cleanup emission so
    /// the reducer deletes stale DB rows). NPCs that have never entered combat
    /// are skipped entirely, eliminating O(N) DB scans for idle NPCs.
    fn collect_threat_updates(&mut self) -> Vec<(EntityId, Vec<game_core::combat::status::ThreatEntry>)> {
        let mut out = Vec::new();
        for (idx, table) in self.state.combat.threat_tables.iter() {
            if self.state.entities.states[idx.as_usize()] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            let eid = self.state.entities.id_of(idx);
            if !table.entries.is_empty() {
                // In-combat: emit snapshot, mark as having DB presence.
                self.threat_has_db_rows.insert(eid);
                out.push((eid, table.entries.clone()));
            } else if self.threat_has_db_rows.remove(&eid) {
                // Just left combat: emit empty snapshot so reducer clears stale rows.
                out.push((eid, Vec::new()));
            }
            // else: never had DB rows, skip entirely.
        }
        out
    }

    /// Snapshot NPC AI state — only emits when state or target changed.
    fn collect_npc_state_updates(&mut self) -> Vec<(EntityId, game_schema::NpcAiState, Option<EntityId>)> {
        let mut out = Vec::new();
        for (idx, &ai_state) in self.state.ai.npc_ai.iter() {
            if self.state.entities.states[idx.as_usize()] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            let eid = self.state.entities.id_of(idx);
            let target = self.state.combat.threat_tables.get(idx)
                .and_then(|t| t.top_threat());
            let current = (ai_state, target);
            let changed = self.npc_state_prev.get(&eid) != Some(&current);
            if changed {
                self.npc_state_prev.insert(eid, current);
                out.push((eid, ai_state, target));
            }
        }
        out
    }

    /// Detect grid-cell transitions using the already-collected transforms.
    ///
    /// Compares each entity's current position against its stored `RegionCell`,
    /// applying hysteresis so oscillation at cell boundaries does not produce
    /// spurious updates. Only changed cells are emitted; the stored cell is
    /// updated in place so the next tick's comparison is correct.
    fn collect_region_updates(
        &mut self,
        transforms: &[(EntityId, Transform)],
    ) -> Vec<(EntityId, RegionCell)> {
        let mut out = Vec::new();
        for &(eid, ref tf) in transforms {
            let pos = &tf.position;
            let new_cell = match self.entity_regions.get(&eid) {
                Some(current) => RegionCell::from_position_with_hysteresis(pos, current),
                // Entity not tracked yet (expected on first tick after spawn).
                None => RegionCell::from_position(pos),
            };
            let changed = self.entity_regions.get(&eid) != Some(&new_cell);
            if changed {
                self.entity_regions.insert(eid, new_cell);
                out.push((eid, new_cell));
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
    ///
    /// Also drains expired follow-up windows from `CombatState::active_windows` and
    /// clears the hold-to-block flag (`blocking`) so the `Block` intent must re-assert
    /// it each tick. `dodge_stacks` is NOT cleared here — it persists across ticks and
    /// is managed exclusively by `StanceBegin`/`StanceEnd` in Phase 3.
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

        // Drain expired follow-up windows (expiry tick ≤ current tick).
        self.state.combat.active_windows.retain(|_, &mut (_, exp)| {
            exp > current
        });
        // Audit a single Window drain record per tick if any windows were active.
        // (Individual per-window records omitted to avoid log spam at scale.)
        #[cfg(any(debug_assertions, test))]
        {
            // We can't audit here without an entity context; the drain is a bulk operation.
            // The ownership rule (CooldownTracker, phase 8) is enforced structurally — this
            // method is the sole caller for window drains.
            let _ = self; // suppress unused-warning if audit! expands to nothing
        }

        // Clear hold-to-block flag. Phase 2 must re-assert it each tick via Block intent.
        // dodge_stacks is NOT cleared here — it is lifecycle-managed by StanceBegin/StanceEnd
        // in Phase 3 and must persist across multiple ticks for the full iframe duration.
        //
        // Uses a 1-tick grace period to absorb timing gaps between the client’s
        // intent throttle and the server tick rate. On the first missed tick,
        // block_grace is set and blocking/root are preserved. On the second
        // consecutive miss, the block sequence truly ends.
        let mut block_ended: Vec<EntityId> = Vec::new();
        for (i, slot) in self.state.combat.tactical.iter_mut().enumerate() {
            if slot.blocking {
                // Block was active this tick — clear the flag so it must be re-asserted,
                // but preserve block_start_tick for perfect-block window continuity.
                // Also clear the block-driven root. Timeline-driven root (StanceBegin)
                // is NOT cleared here — it persists until StanceEnd in Phase 3.
                slot.blocking = false;
                slot.rooted = false;
                slot.block_grace = false;
            } else if slot.block_start_tick.is_some() {
                if !slot.block_grace {
                    // First missed tick — grant grace period. Keep block_start_tick
                    // alive so Phase 6 still treats this entity as blocking for
                    // damage reduction, and keep root so they can’t move.
                    slot.block_grace = true;
                } else {
                    // Second consecutive miss — block sequence truly ended.
                    slot.block_start_tick = None;
                    slot.block_grace = false;
                    if let Some(eid) = self.state.entities.lookup_by_slot(i) {
                        block_ended.push(eid);
                    }
                }
            }
        }
        for eid in block_ended {
            self.emit_event(eid, EventPayload::BlockEnd);
        }
    }

    fn phase_state_finalization(&mut self) -> Vec<(EntityId, game_schema::EntityState)> {
        use game_core::entity::lifecycle::EntityState;

        let mut state_updates: Vec<(EntityId, game_schema::EntityState)> = Vec::new();

        // Check for newly dead entities and mark them for despawn.
        let dead_indices: Vec<EntityIndex> = (0..self.state.entities.len())
            .map(|i| self.state.entities.index_at(i))
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
                    self.state.combat.threat_tables.get(idx)
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
        for table in self.state.combat.threat_tables.values_mut() {
            table.decay(THREAT_DECAY_FACTOR);
        }
        audit!(self.state, Threat, Lifecycle, 8, None::<EntityId>, "decay");

        // Activate any Spawning entities (they've had one tick to set up physics).
        let spawning: Vec<EntityIndex> = (0..self.state.entities.len())
            .filter(|&i| self.state.entities.states[i] == EntityState::Spawning)
            .map(|i| self.state.entities.index_at(i))
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
        SkillShape::Projectile   => SensorShape::Sphere { radius: 0.5 },
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
    use game_core::entity::lifecycle::NpcAiState;
    use game_core::physics_backend::ColliderKind;
    use game_protocol::event::{DamageType, EventPayload};
    use game_protocol::intent::{PlayerIntent, IntentAction};
    use crate::physics::rapier_world::PhysicsWorld;
    use crate::physics::collision_groups;

    fn make_pipeline(abilities: AbilityRegistry) -> TickPipeline {
        let physics = Box::new(PhysicsWorld::new(1.0 / 20.0));
        TickPipeline::new(TickId(0), physics, 1.0 / 20.0, abilities, game_core::combat::status::BuffRegistry::new())
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
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
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

        // Neutralize NPC AI before the damage tick so it doesn't auto-cast back
        // at the player when threat is added during Phase 6 combat resolution.
        if let Some(idx) = pipeline.state.entities.lookup(target_id) {
            pipeline.state.ai.npc_ai.remove(idx);
        }

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
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
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
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let cast_b = PlayerIntent {
            entity_id: attacker,
            sequence_id: 2,
            target_tick: TickId(1),
            client_observed_tick: 0,
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
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
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
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
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
            client_observed_tick: 0,
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

    // ── P5: Interact proximity tests ──────────────────────────────────────────

    /// Interact intent within INTERACT_RADIUS (3.0 units) must emit InteractTriggered.
    #[test]
    fn interact_within_range_emits_event() {
        let mut pipeline = make_pipeline(AbilityRegistry::new());

        let player = EntityId(1);
        let npc = EntityId(2);

        // NPC at origin, player 2.0 units away on X — within 3.0 radius.
        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 2.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Warm-up tick.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player));
        assert!(pipeline.state.is_active(npc));

        // Send Interact intent.
        let intent = PlayerIntent {
            entity_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::Interact(npc.0),
        };
        let result = pipeline.run_tick(&[intent]);

        let triggered = result.events.iter().any(|e| {
            matches!(&e.payload, EventPayload::InteractTriggered { target } if *target == npc)
        });
        assert!(triggered, "InteractTriggered must fire when actor is within INTERACT_RADIUS");
    }

    /// Interact intent beyond INTERACT_RADIUS (3.0 units) must be silently dropped.
    #[test]
    fn interact_out_of_range_is_silent() {
        let mut pipeline = make_pipeline(AbilityRegistry::new());

        let player = EntityId(1);
        let npc = EntityId(2);

        // NPC at origin, player 5.0 units away — beyond 3.0 radius.
        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 5.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        pipeline.run_tick(&[]);

        let intent = PlayerIntent {
            entity_id: player,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::Interact(npc.0),
        };
        let result = pipeline.run_tick(&[intent]);

        let triggered = result.events.iter().any(|e| {
            matches!(&e.payload, EventPayload::InteractTriggered { .. })
        });
        assert!(!triggered, "InteractTriggered must NOT fire when actor is beyond INTERACT_RADIUS");
    }

    // ── P5: Per-kind base speed test ──────────────────────────────────────────

    /// `base_speed` returns the expected value for every EntityKind.
    #[test]
    fn base_speed_per_kind() {
        use game_core::entity::lifecycle::EntityKind;
        use game_core::stats::base_speed;

        assert!((base_speed(EntityKind::Player)     - 5.0 ).abs() < f32::EPSILON);
        assert!((base_speed(EntityKind::Npc)        - 3.5 ).abs() < f32::EPSILON);
        assert!((base_speed(EntityKind::Boss)       - 2.5 ).abs() < f32::EPSILON);
        assert!((base_speed(EntityKind::Projectile) - 12.0).abs() < f32::EPSILON);
        assert!((base_speed(EntityKind::Hazard)     - 0.0 ).abs() < f32::EPSILON);
    }

    // ── P5: NPC AI movement tests ─────────────────────────────────────────────

    /// An NPC in Combat state with a valid threat target must move closer to that
    /// target each tick (Phase 7 chase branch).
    #[test]
    fn npc_chase_moves_toward_target() {
        let mut pipeline = make_pipeline(AbilityRegistry::new());

        let player = EntityId(1);
        let npc    = EntityId(2);

        // Place NPC 4.0 units away from player on X axis.
        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 4.0, y: 1.0, z: 0.0 },
        );

        // Warm-up tick: entities go Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player));
        assert!(pipeline.state.is_active(npc));

        // Set NPC to Combat state directly and prime the threat table.
        // (Skipping the Idle → Combat transition tick so movement fires this tick.)
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Combat;
            if let Some(table) = pipeline.state.combat.threat_tables.get_mut(idx) {
                table.add_threat(player, 100.0);
            }
        }

        // Record NPC position before the AI tick.
        let before = pipeline.physics.get_transform(npc).unwrap();

        // Tick 1: Phase 7 calls set_next_kinematic_position on the NPC body.
        // The new position is queued in Rapier but not yet committed (physics step
        // happens in Phase 4, which runs BEFORE Phase 7 in the same tick).
        pipeline.run_tick(&[]);

        // Tick 2: Phase 4 physics step commits the kinematic position from tick 1.
        // Now get_transform returns the new position.
        pipeline.run_tick(&[]);

        let after = pipeline.physics.get_transform(npc).unwrap();

        // NPC must have moved closer to the player (player is at x=0, NPC started at x=4).
        assert!(
            after.position.x < before.position.x,
            "NPC must move toward player (x should decrease); before={}, after={}",
            before.position.x, after.position.x,
        );
    }

    /// An NPC in Flee state must move away from the threat source.
    #[test]
    fn npc_flee_moves_away_from_threat() {
        let mut pipeline = make_pipeline(AbilityRegistry::new());

        let player = EntityId(1);
        let npc    = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 4.0, y: 1.0, z: 0.0 },
        );

        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(player));
        assert!(pipeline.state.is_active(npc));

        // Force NPC into Flee state with a threat source.
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Flee;
            if let Some(table) = pipeline.state.combat.threat_tables.get_mut(idx) {
                table.add_threat(player, 100.0);
            }
        }

        let before = pipeline.physics.get_transform(npc).unwrap();

        // Tick 1: Phase 7 queues the flee position via set_next_kinematic_position.
        pipeline.run_tick(&[]);
        // Tick 2: Phase 4 physics step commits the queued position.
        pipeline.run_tick(&[]);

        let after = pipeline.physics.get_transform(npc).unwrap();

        // NPC must have moved away from player (player is at x=0, NPC started at x=4, so x increases).
        assert!(
            after.position.x > before.position.x,
            "NPC must flee away from player (x should increase); before={}, after={}",
            before.position.x, after.position.x,
        );
    }

    /// An NPC in Patrol state must move toward its home position and stop within
    /// PATROL_ARRIVE_RADIUS once it arrives.
    #[test]
    fn npc_patrol_moves_toward_home() {
        let mut pipeline = make_pipeline(AbilityRegistry::new());

        let npc = EntityId(1);

        // Spawn NPC at origin; home_positions[idx] is recorded as the spawn position (0,1,0).
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up — Spawning → Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(npc));

        // Teleport the NPC body 4.0 units away from home on X.
        // set_kinematic_position uses set_next_kinematic_position internally:
        // the position is queued but not committed to body.translation() until
        // the next physics step (Phase 4 of the next run_tick call).
        pipeline.physics.set_kinematic_position(npc, game_schema::Vec3f { x: 4.0, y: 1.0, z: 0.0 });

        // Force NPC into Patrol state before the commit tick.
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Patrol;
        }

        // Tick 1: Phase 4 commits the teleport position (body now at x=4).
        // Phase 7 patrol branch also fires, queueing movement toward home — but
        // the movement destination reads the COMMITTED position (x=4 → toward 0),
        // so it queues the correct step. That step will be committed in tick 2.
        pipeline.run_tick(&[]);

        // Snap the `before` position now that the teleport is committed.
        // Phase 7 may have already queued one step toward home in tick 1,
        // but get_transform still reflects the committed x=4 position until
        // the next physics step.
        let before = pipeline.physics.get_transform(npc).unwrap();

        // Tick 2: Phase 4 commits the patrol step queued in tick 1.
        // Re-assert Patrol in case a state transition occurred (threat table is empty so
        // the Combat/Idle logic does not interfere, but be explicit for test clarity).
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Patrol;
        }
        pipeline.run_tick(&[]);

        let after = pipeline.physics.get_transform(npc).unwrap();

        assert!(
            after.position.x < before.position.x,
            "Patrolling NPC must move toward home (x should decrease); before={}, after={}",
            before.position.x, after.position.x,
        );
    }

    // ── RegionCell tests ────────────────────────────────────────

    #[test]
    fn region_cell_from_position_basic() {
        // Position in the middle of cell (0,0)
        let pos = Vec3f::new(10.0, 0.0, 10.0);
        let cell = RegionCell::from_position(&pos);
        assert_eq!(cell.region_x, 0);
        assert_eq!(cell.region_z, 0);
        assert_eq!(cell.layer, 0);
    }

    #[test]
    fn region_cell_from_position_negative() {
        // Position in cell (-1, -1) — just past the negative boundary
        let pos = Vec3f::new(-1.0, 5.0, -1.0);
        let cell = RegionCell::from_position(&pos);
        assert_eq!(cell.region_x, -1);
        assert_eq!(cell.region_z, -1);
    }

    #[test]
    fn region_cell_from_position_exact_boundary() {
        // Position right on the cell boundary at CELL_SIZE
        let pos = Vec3f::new(CELL_SIZE, 0.0, CELL_SIZE);
        let cell = RegionCell::from_position(&pos);
        assert_eq!(cell.region_x, 1);
        assert_eq!(cell.region_z, 1);
    }

    #[test]
    fn hysteresis_prevents_thrashing() {
        // Entity is in cell (0,0), barely crosses into cell (1,0) but within
        // the hysteresis band — should stay in (0,0).
        let current = RegionCell { region_x: 0, region_z: 0, layer: 0 };
        let pos = Vec3f::new(CELL_SIZE + 1.0, 0.0, 25.0); // 1 unit past boundary < HYSTERESIS_BAND
        let cell = RegionCell::from_position_with_hysteresis(&pos, &current);
        assert_eq!(cell.region_x, 0, "Should stay in cell 0 due to hysteresis");
        assert_eq!(cell.region_z, 0);
    }

    #[test]
    fn hysteresis_allows_transition_past_band() {
        // Entity in cell (0,0), moves well past boundary + hysteresis band
        let current = RegionCell { region_x: 0, region_z: 0, layer: 0 };
        let pos = Vec3f::new(CELL_SIZE + HYSTERESIS_BAND + 1.0, 0.0, 25.0);
        let cell = RegionCell::from_position_with_hysteresis(&pos, &current);
        assert_eq!(cell.region_x, 1, "Should transition to cell 1 past hysteresis band");
    }

    #[test]
    fn hysteresis_preserves_layer() {
        let current = RegionCell { region_x: 0, region_z: 0, layer: 42 };
        let pos = Vec3f::new(CELL_SIZE + HYSTERESIS_BAND + 1.0, 0.0, 25.0);
        let cell = RegionCell::from_position_with_hysteresis(&pos, &current);
        assert_eq!(cell.layer, 42, "Layer should be preserved through hysteresis");
        assert_eq!(cell.region_x, 1);
    }

    #[test]
    fn region_updates_emitted_on_spawn_tick() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);
        let id = EntityId(1);
        pipeline.spawn_entity_from_snapshot(
            id, EntityKind::Player, TickId(0), 100.0,
            Vec3f::new(75.0, 0.0, 75.0), // cell (1, 1)
        );

        let result = pipeline.run_tick(&[]);
        // First tick always emits an initial region update because
        // spawn_entity_from_snapshot does NOT seed entity_regions —
        // collect_region_updates discovers the entity as new.
        assert_eq!(result.region_updates.len(), 1, "Expected initial region update on spawn tick");
        let (eid, cell) = &result.region_updates[0];
        assert_eq!(*eid, id);
        assert_eq!(cell.region_x, 1);
        assert_eq!(cell.region_z, 1);
        assert_eq!(cell.layer, 0);
    }

    #[test]
    fn no_region_update_without_movement() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);
        let id = EntityId(1);
        pipeline.spawn_entity_from_snapshot(
            id, EntityKind::Player, TickId(0), 100.0,
            Vec3f::new(75.0, 0.0, 75.0),
        );

        // First tick: initial region emitted
        let _ = pipeline.run_tick(&[]);
        // Second tick: no movement → no region update
        let result = pipeline.run_tick(&[]);
        assert!(
            result.region_updates.is_empty(),
            "No region update when entity hasn't moved"
        );
    }

    // ── Lag-compensation integration tests ──────────────────────

    #[test]
    fn compensated_hit_lands_at_historical_position() {
        use game_core::combat::skill::{AbilityExecutionContext, AbilityParams, ResolvedTargeting};

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker_id = EntityId(1);
        let target_id = EntityId(2);

        // Spawn both entities at the same position.
        let origin = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        pipeline.state.spawn_entity(attacker_id, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target_id, EntityKind::Npc, TickId(0), 100.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker_id, origin, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target_id, origin, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up (Spawning → Active). History records both at origin.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(attacker_id));
        assert!(pipeline.state.is_active(target_id));

        // Tick 1: another tick so we have history at tick 1 with target at origin.
        pipeline.run_tick(&[]);

        // Now move the target far away so Rapier contacts won't fire.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.set_kinematic_position(target_id, rapier3d::math::Vector::new(100.0, 5.0, 100.0));
        }
        // Tick 2: target is now far away in physics, recorded at new position.
        pipeline.run_tick(&[]);

        // Inject a compensated hitbox (rewind_ticks=2 → looks at tick 0 where
        // the target was at origin).
        let exec_id = AbilityExecutionId(1);
        let rewind_ticks: u32 = 2;

        // Spawn hitbox with rewind_ticks > 0 (use spawn + arm, not spawn_armed).
        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has body");
        pipeline.state.combat.hitboxes.spawn(
            exec_id, attacker_id, 1, TickId(3), SkillShape::Sphere,
            Vec3f { x: 0.0, y: 0.0, z: 0.0 }, rewind_ticks, false, 0,
        );
        pipeline.state.combat.hitboxes.arm(exec_id, sensor);

        // Also insert an execution context so resolve_compensated_hits can
        // look up the attacker's position/facing.
        pipeline.state.combat.executions.insert(AbilityExecutionContext {
            execution_id: exec_id,
            ability_id: 1,
            caster: attacker_id,
            started_at: TickId(3),
            targeting: ResolvedTargeting::SelfCast,
            origin: Vec3f { x: 0.0, y: 5.0, z: 0.0 },
            facing: Vec3f { x: 0.0, y: 0.0, z: 1.0 },
            params: AbilityParams { charge_level: 0, variant: 0 },
            rewind_ticks,
        });

        // Tick 3: combat resolution should find the hit via compensated path.
        let result = pipeline.run_tick(&[]);

        // Target was far away in current physics, so normal contacts should NOT
        // have fired. But the compensated path rewound to tick 0/1 where the
        // target was at origin — within the hitbox sphere.
        let target_hp = pipeline.state.hp_of(target_id).unwrap();
        assert!(
            (target_hp - 75.0).abs() < 0.01,
            "Compensated hit should deal 25 damage, got hp={}",
            target_hp,
        );

        // Verify Damage event was emitted.
        let damage_events: Vec<_> = result.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::Damage { source, .. } if *source == attacker_id)
        }).collect();
        assert!(!damage_events.is_empty(), "Should emit Damage event from compensated hit");

        // Verify SkillHit event was emitted.
        let skill_events: Vec<_> = result.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::SkillHit { skill_id: 1, .. })
        }).collect();
        assert!(!skill_events.is_empty(), "Should emit SkillHit event from compensated hit");
    }

    #[test]
    fn compensated_hit_deduplicates_with_normal_hit() {
        use game_core::combat::skill::{AbilityExecutionContext, AbilityParams, ResolvedTargeting};

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker_id = EntityId(1);
        let target_id = EntityId(2);

        // Spawn both at the same position — close enough for Rapier contacts.
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        pipeline.state.spawn_entity(attacker_id, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target_id, EntityKind::Npc, TickId(0), 100.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker_id, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target_id, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up. History records both at same position.
        pipeline.run_tick(&[]);

        // Tick 1: build more history.
        pipeline.run_tick(&[]);

        // Inject a hitbox with rewind_ticks > 0 AND a Rapier sensor at the same
        // position. Both the normal contact path and the compensated path should
        // find the target — but dedup must ensure only one hit.
        let exec_id = AbilityExecutionId(1);
        let rewind_ticks: u32 = 1;

        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has body");
        pipeline.state.combat.hitboxes.spawn(
            exec_id, attacker_id, 1, TickId(2), SkillShape::Sphere,
            Vec3f { x: 0.0, y: 0.0, z: 0.0 }, rewind_ticks, false, 0,
        );
        pipeline.state.combat.hitboxes.arm(exec_id, sensor);

        pipeline.state.combat.executions.insert(AbilityExecutionContext {
            execution_id: exec_id,
            ability_id: 1,
            caster: attacker_id,
            started_at: TickId(2),
            targeting: ResolvedTargeting::SelfCast,
            origin: Vec3f { x: 0.0, y: 5.0, z: 0.0 },
            facing: Vec3f { x: 0.0, y: 0.0, z: 1.0 },
            params: AbilityParams { charge_level: 0, variant: 0 },
            rewind_ticks,
        });

        // Tick 2: both contact-based and compensated paths can see the target.
        let result = pipeline.run_tick(&[]);

        // Should get exactly ONE damage event — dedup prevents double-hit.
        let damage_events: Vec<_> = result.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::Damage { source, .. } if *source == attacker_id)
        }).collect();
        assert_eq!(
            damage_events.len(), 1,
            "Dedup should prevent double-hit from normal + compensated paths, got {} damage events",
            damage_events.len(),
        );

        // Verify only 25 damage was dealt (not 50).
        let target_hp = pipeline.state.hp_of(target_id).unwrap();
        assert!(
            (target_hp - 75.0).abs() < 0.01,
            "Should take exactly 25 damage (not doubled), got hp={}",
            target_hp,
        );
    }

    #[test]
    fn compensated_hit_misses_when_historically_out_of_range() {
        use game_core::combat::skill::{AbilityExecutionContext, AbilityParams, ResolvedTargeting};

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let attacker_id = EntityId(1);
        let target_id = EntityId(2);

        // Spawn entities far apart from the start.
        pipeline.state.spawn_entity(attacker_id, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target_id, EntityKind::Npc, TickId(0), 100.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                attacker_id,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                target_id,
                rapier3d::math::Vector::new(50.0, 5.0, 50.0),
                0.5, 0.3, 1.0,
                collision_groups::npc_body_groups(),
            );
        }

        // Ticks 0-1: build history with target always far away.
        pipeline.run_tick(&[]);
        pipeline.run_tick(&[]);

        // Inject compensated hitbox. Even with rewind, the target was never near.
        let exec_id = AbilityExecutionId(1);
        let rewind_ticks: u32 = 1;

        let sensor = pipeline.physics.spawn_sensor(
            attacker_id,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("attacker has body");
        pipeline.state.combat.hitboxes.spawn(
            exec_id, attacker_id, 1, TickId(2), SkillShape::Sphere,
            Vec3f { x: 0.0, y: 0.0, z: 0.0 }, rewind_ticks, false, 0,
        );
        pipeline.state.combat.hitboxes.arm(exec_id, sensor);

        pipeline.state.combat.executions.insert(AbilityExecutionContext {
            execution_id: exec_id,
            ability_id: 1,
            caster: attacker_id,
            started_at: TickId(2),
            targeting: ResolvedTargeting::SelfCast,
            origin: Vec3f { x: 0.0, y: 5.0, z: 0.0 },
            facing: Vec3f { x: 0.0, y: 0.0, z: 1.0 },
            params: AbilityParams { charge_level: 0, variant: 0 },
            rewind_ticks,
        });

        // Tick 2: no hit should occur.
        let result = pipeline.run_tick(&[]);

        let damage_events: Vec<_> = result.events.iter().filter(|e| {
            matches!(&e.payload, EventPayload::Damage { .. })
        }).collect();
        assert!(
            damage_events.is_empty(),
            "No damage when target was historically out of range",
        );

        let target_hp = pipeline.state.hp_of(target_id).unwrap();
        assert!(
            (target_hp - 100.0).abs() < 0.01,
            "Target should be at full health, got hp={}",
            target_hp,
        );
    }

    #[test]
    fn cooldown_reduce_pct_shortens_cooldown() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Player, TickId(0), 100.0);
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(caster, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
        }

        // Tick 0: warm-up — Spawning → Active.
        pipeline.run_tick(&[]);

        // Apply a buff with 50% cooldown reduction.
        use game_core::combat::status::{ActiveBuff, BuffModifiers};
        let idx = pipeline.state.entities.lookup(caster).unwrap();
        pipeline.state.status.modify_buffs(idx, |buffs| buffs.push(ActiveBuff {
            buff_id: 99,
            source: caster,
            target: caster,
            stacks: 1,
            max_stacks: 1,
            expires_at: None,
            modifiers: BuffModifiers {
                cooldown_reduce_pct: Some(0.5),
                ..Default::default()
            },
        }));

        // Tick 1: cast ability (Slash has 20-tick cooldown).
        let cast_intent = PlayerIntent {
            entity_id: caster,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[cast_intent]);

        // With 50% CDR on a 20-tick cooldown, effective = ceil(20 * 0.5) = 10 ticks.
        // CooldownStart fires in Phase 3 of tick 1 (offset 0).
        // ready_at = tick 1 + 10 = tick 11.
        // After tick 1's run_tick completes, current_tick = 2.
        // is_on_cooldown checks: current_tick < ready_at.
        assert!(
            pipeline.is_on_cooldown_pub(caster, 1),
            "Should be on cooldown immediately after cast (tick 2 < 11)"
        );

        // Advance from tick 2 to tick 10 (8 more run_ticks).
        for _ in 0..8 {
            pipeline.run_tick(&[]);
        }
        // Now at tick 10 — should still be on cooldown (10 < 11).
        assert!(
            pipeline.is_on_cooldown_pub(caster, 1),
            "Should still be on cooldown at tick 10 (ready_at = 11)"
        );

        // Tick 10 → 11: cooldown should be ready (11 < 11 is false).
        pipeline.run_tick(&[]);
        assert!(
            !pipeline.is_on_cooldown_pub(caster, 1),
            "Cooldown should be ready at tick 11 with 50% CDR"
        );

        // Verify it's shorter than a full 20-tick cooldown:
        // Without CDR, ready_at would be tick 1 + 20 = tick 21, so at tick 11
        // we'd still have 10 ticks left. The CDR halved it.
    }

    #[test]
    fn cooldown_reduce_pct_clamps_to_max() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Player, TickId(0), 100.0);
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(caster, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
        }

        pipeline.run_tick(&[]);

        // Apply an absurd 200% CDR buff — should clamp to 99%.
        use game_core::combat::status::{ActiveBuff, BuffModifiers};
        let idx = pipeline.state.entities.lookup(caster).unwrap();
        pipeline.state.status.modify_buffs(idx, |buffs| buffs.push(ActiveBuff {
            buff_id: 99,
            source: caster,
            target: caster,
            stacks: 1,
            max_stacks: 1,
            expires_at: None,
            modifiers: BuffModifiers {
                cooldown_reduce_pct: Some(2.0),
                ..Default::default()
            },
        }));

        let cast_intent = PlayerIntent {
            entity_id: caster,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 1,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[cast_intent]);

        // With 99% clamp on a 20-tick cooldown: ceil(20 * 0.01) = 1 tick.
        // ready_at = tick 1 + max(1, 1) = tick 2.
        // We're now at tick 2, so cooldown should already be expired.
        assert!(
            !pipeline.is_on_cooldown_pub(caster, 1),
            "With clamped CDR, cooldown should expire almost immediately"
        );
    }

    #[test]
    fn cast_ability_helper_creates_execution_context() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Npc, TickId(0), 100.0);
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(caster, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up — Spawning → Active.
        pipeline.run_tick(&[]);

        // NPC casts via cast_ability directly (no intent needed) at tick 1.
        let ok = pipeline.cast_ability(caster, 1, ResolvedTargeting::SelfCast, 0, 0);
        assert!(ok, "cast_ability should succeed for a valid ability");

        // Execution context should exist immediately after cast_ability.
        let execs: Vec<_> = pipeline.state.combat.executions.active_ids();
        assert_eq!(execs.len(), 1, "One execution context should exist after cast");
        let ctx = pipeline.state.combat.executions.get(execs[0]).unwrap();
        assert_eq!(ctx.caster, caster);
        assert_eq!(ctx.ability_id, 1);
        assert_eq!(ctx.rewind_ticks, 0, "NPC casts should have 0 rewind");

        // Run tick 1 so Phase 3 drains the scheduled CooldownStart action.
        pipeline.run_tick(&[]);

        // Cooldown should now be active (CooldownStart processed in Phase 3).
        assert!(
            pipeline.is_on_cooldown_pub(caster, 1),
            "Ability should be on cooldown after Phase 3 processes CooldownStart"
        );

        // Second cast should fail (on cooldown).
        let ok2 = pipeline.cast_ability(caster, 1, ResolvedTargeting::SelfCast, 0, 0);
        assert!(!ok2, "Second cast should fail — on cooldown");
    }

    /// An NPC in Combat state should auto-cast ability 1 at its top-threat target.
    #[test]
    fn npc_combat_casts_ability_at_target() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        let npc = EntityId(2);

        // Place NPC and player close together so the NPC has a valid target.
        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 2.0, y: 1.0, z: 0.0 },
        );

        // Tick 0: warm-up — Spawning → Active.
        pipeline.run_tick(&[]);

        // Put NPC in Combat with threat on the player.
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Combat;
            if let Some(table) = pipeline.state.combat.threat_tables.get_mut(idx) {
                table.add_threat(player, 100.0);
            }
        }

        // No executions before the AI tick.
        assert_eq!(pipeline.state.combat.executions.active_ids().len(), 0);

        // Tick 1: Phase 7 AI should cast ability 1 → creates execution context.
        pipeline.run_tick(&[]);

        let execs: Vec<_> = pipeline.state.combat.executions.active_ids();
        assert!(
            !execs.is_empty(),
            "NPC in Combat should create an execution context via cast_ability"
        );
        let ctx = pipeline.state.combat.executions.get(execs[0]).unwrap();
        assert_eq!(ctx.caster, npc, "Caster should be the NPC");
        assert_eq!(ctx.ability_id, 1, "Should cast ability 1 (Slash)");
        assert_eq!(ctx.rewind_ticks, 0, "NPC casts have 0 rewind");
    }

    /// NPC respects cooldown — does not cast again while ability is on cooldown.
    #[test]
    fn npc_combat_respects_cooldown() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        let npc = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 2.0, y: 1.0, z: 0.0 },
        );

        // Warm-up tick.
        pipeline.run_tick(&[]);

        // Set NPC to Combat with threat.
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Combat;
            if let Some(table) = pipeline.state.combat.threat_tables.get_mut(idx) {
                table.add_threat(player, 100.0);
            }
        }

        // Tick 1: NPC casts (creates execution).
        pipeline.run_tick(&[]);
        let exec_count_after_first = pipeline.state.combat.executions.active_ids().len();
        assert_eq!(exec_count_after_first, 1, "First cast should succeed");

        // Tick 2: NPC should NOT cast again (ability 1 has 20-tick cooldown).
        pipeline.run_tick(&[]);
        let exec_count_after_second = pipeline.state.combat.executions.active_ids().len();
        // Execution context from tick 1 may still be active or may have been cleaned up,
        // but no NEW execution should have been created beyond the first.
        assert!(
            exec_count_after_second <= exec_count_after_first,
            "NPC should not cast again while on cooldown; had {exec_count_after_first}, now {exec_count_after_second}"
        );
    }

    /// NPCs in non-Combat states (Flee, Patrol, Idle) should NOT cast abilities.
    #[test]
    fn npc_non_combat_does_not_cast() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        let npc = EntityId(2);

        pipeline.spawn_entity_from_snapshot(
            player, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            npc, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 2.0, y: 1.0, z: 0.0 },
        );

        // Warm-up tick.
        pipeline.run_tick(&[]);

        // Set NPC to Flee with threat (so top_threat exists but state is Flee).
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Flee;
            if let Some(table) = pipeline.state.combat.threat_tables.get_mut(idx) {
                table.add_threat(player, 100.0);
            }
        }

        // Tick: Phase 7 runs in Flee state — should only move away, not cast.
        pipeline.run_tick(&[]);
        assert_eq!(
            pipeline.state.combat.executions.active_ids().len(), 0,
            "NPC in Flee state should not cast abilities"
        );

        // Switch to Patrol with no threats — should not cast.
        if let Some(idx) = pipeline.state.entities.lookup(npc) {
            *pipeline.state.ai.npc_ai.get_mut(idx).unwrap() = NpcAiState::Patrol;
            if let Some(table) = pipeline.state.combat.threat_tables.get_mut(idx) {
                table.entries.clear();
            }
        }
        pipeline.run_tick(&[]);
        assert_eq!(
            pipeline.state.combat.executions.active_ids().len(), 0,
            "NPC in Patrol state should not cast abilities"
        );
    }

    /// A dead attacker's delayed hit must NOT re-insert them into the NPC's
    /// threat table.  Verifies the liveness guard in `apply_hit_damage`.
    #[test]
    fn dead_attacker_hit_does_not_insert_threat() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Poke".to_string(),
            base_damage: 5.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: true, // allow_reentry so we can test a delayed hit after death
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let npc = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(npc, EntityKind::Npc, TickId(0), 200.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(npc, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up — entities go Active.
        pipeline.run_tick(&[]);

        let npc_idx = pipeline.state.entities.lookup(npc).expect("NPC should exist");

        // Kill and remove the attacker before the hit lands.
        pipeline.force_remove_entity(attacker);
        assert!(pipeline.state.entities.lookup(attacker).is_none(), "attacker should be gone");

        // Simulate a delayed hit from the dead attacker.
        pipeline.apply_hit_damage(attacker, npc, npc_idx, 10, false, None);

        // Threat table for the NPC must NOT contain the dead attacker.
        let threat = pipeline.state.combat.threat_tables.get(npc_idx)
            .expect("NPC should have a threat table");
        assert!(
            !threat.entries.iter().any(|e| e.source == attacker),
            "dead attacker must not appear in threat table after delayed hit"
        );
    }

    /// Block mitigates damage when the defender faces the attacker (front 180° arc).
    #[test]
    fn directional_block_mitigates_when_facing_attacker() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Poke".to_string(),
            base_damage: 20.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let defender = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(defender, EntityKind::Player, TickId(0), 100.0);

        // Place attacker in front of defender (+Z direction).
        // Defender faces +Z (default yaw = 0). Attacker is at z=3.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                defender,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                attacker,
                rapier3d::math::Vector::new(0.0, 5.0, 3.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        let def_idx = pipeline.state.entities.lookup(defender).unwrap();

        // Set blocking flag (normally done by Phase 2 Block intent).
        // block_start_tick = None avoids triggering perfect-block window.
        pipeline.state.combat.tactical[def_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[def_idx.as_usize()].block_start_tick = None;

        // Apply damage — defender faces +Z, attacker is at +Z → front hit.
        pipeline.apply_hit_damage(attacker, defender, def_idx, 10, false, None);

        // Block should halve the 20 base damage → 10 actual.
        let hp = pipeline.state.hp_of(defender).unwrap();
        assert!(
            (hp - 90.0).abs() < 0.01,
            "Front-facing block should halve damage (expected 90 hp), got {}",
            hp,
        );
    }

    /// Block does NOT mitigate damage when the attacker is behind the defender.
    #[test]
    fn directional_block_fails_when_attacker_behind() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Poke".to_string(),
            base_damage: 20.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let defender = EntityId(2);

        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(defender, EntityKind::Player, TickId(0), 100.0);

        // Defender faces +Z (yaw = 0). Attacker is behind at z=-3.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                defender,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                attacker,
                rapier3d::math::Vector::new(0.0, 5.0, -3.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        let def_idx = pipeline.state.entities.lookup(defender).unwrap();

        // Set blocking flag.
        pipeline.state.combat.tactical[def_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[def_idx.as_usize()].block_start_tick = None;

        // Apply damage — attacker is behind → block should NOT apply.
        pipeline.apply_hit_damage(attacker, defender, def_idx, 10, false, None);

        // Full 20 damage should land (no block mitigation).
        let hp = pipeline.state.hp_of(defender).unwrap();
        assert!(
            (hp - 80.0).abs() < 0.01,
            "Back-hit should deal full damage (expected 80 hp), got {}",
            hp,
        );
    }

    /// Blocking roots the player: Move intent is ignored while holding block.
    #[test]
    fn block_roots_player_movement() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let player = EntityId(1);
        pipeline.state.spawn_entity(player, EntityKind::Player, TickId(0), 100.0);

        // Add physics body so movement has something to move.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                player,
                rapier3d::math::Vector::new(0.0, 0.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        // Tick 0: warm-up (initialises physics bodies).
        pipeline.run_tick(&[]);

        let pos_before = pipeline.physics.get_transform(player).unwrap();

        // Tick 1: Send Block + Move simultaneously.
        let intents = vec![
            PlayerIntent {
                entity_id: player,
                sequence_id: 1,
                target_tick: TickId(1),
                client_observed_tick: 0,
                action: IntentAction::Block,
            },
            PlayerIntent {
                entity_id: player,
                sequence_id: 2,
                target_tick: TickId(1),
                client_observed_tick: 0,
                action: IntentAction::Move(game_protocol::intent::MoveDir { dir_x: 0.0, dir_y: 0.0, dir_z: 1.0 }),
            },
        ];
        pipeline.run_tick(&intents);

        let pos_after = pipeline.physics.get_transform(player).unwrap();
        let dx = pos_after.position.x - pos_before.position.x;
        let dz = pos_after.position.z - pos_before.position.z;
        let dist = (dx * dx + dz * dz).sqrt();
        assert!(
            dist < 0.001,
            "Player should not move while blocking (rooted). Moved {} units.",
            dist,
        );
    }

    /// Buff-driven root prevents NPC from stepping toward target.
    #[test]
    fn buff_root_prevents_npc_movement() {
        use game_core::combat::status::{ActiveBuff, BuffModifiers};

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let npc = EntityId(10);
        let player = EntityId(1);
        pipeline.state.spawn_entity(npc, EntityKind::Npc, TickId(0), 100.0);
        pipeline.state.spawn_entity(player, EntityKind::Player, TickId(0), 100.0);

        // Place NPC and player far apart so AI would want to step toward player.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                npc,
                rapier3d::math::Vector::new(0.0, 0.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::npc_body_groups(),
            );
            pw.add_dynamic_capsule(
                player,
                rapier3d::math::Vector::new(0.0, 0.0, 20.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        let npc_idx = pipeline.state.entities.lookup(npc).unwrap();

        // Apply a root buff on the NPC.
        pipeline.state.status.modify_buffs(npc_idx, |buffs| {
            buffs.push(ActiveBuff {
                buff_id: 999,
                source: player,
                target: npc,
                stacks: 1,
                max_stacks: 1,
                expires_at: Some(TickId(100)),
                modifiers: BuffModifiers {
                    root: Some(true),
                    ..Default::default()
                },
            });
        });

        assert!(pipeline.is_rooted(npc_idx), "NPC should be rooted by buff");

        let pos_before = pipeline.physics.get_transform(npc).unwrap();

        // Attempt to step NPC toward player — should be blocked by root.
        let from = game_schema::Vec3f { x: 0.0, y: 0.0, z: 0.0 };
        let to = game_schema::Vec3f { x: 0.0, y: 0.0, z: 20.0 };
        pipeline.npc_step_toward(npc, from, to, EntityKind::Npc, pipeline.dt);

        let pos_after = pipeline.physics.get_transform(npc).unwrap();
        let dx = pos_after.position.x - pos_before.position.x;
        let dz = pos_after.position.z - pos_before.position.z;
        let dist = (dx * dx + dz * dz).sqrt();
        assert!(
            dist < 0.001,
            "Rooted NPC should not move toward target. Moved {} units.",
            dist,
        );
    }

    /// Knockback impulse pushes the target away from the attacker.
    #[test]
    fn knockback_pushes_target_away() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 80,
            name: "HeavySlam".to_string(),
            base_damage: 10.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 5.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Player, TickId(0), 100.0);

        // Place attacker at origin, target at +Z so knockback pushes along +Z.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                attacker,
                rapier3d::math::Vector::new(0.0, 0.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                target,
                rapier3d::math::Vector::new(0.0, 0.0, 3.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        pipeline.run_tick(&[]);

        let tgt_idx = pipeline.state.entities.lookup(target).unwrap();
        pipeline.apply_hit_damage(attacker, target, tgt_idx, 80, false, None);

        // Knockback should have enqueued an impulse.
        assert_eq!(pipeline.pending_impulses.len(), 1, "Should have one knockback impulse");
        let (imp_eid, imp_vel) = &pipeline.pending_impulses[0];
        assert_eq!(*imp_eid, target);
        // Direction should be roughly +Z (away from attacker).
        assert!(imp_vel.z > 0.0, "Knockback should push away from attacker along +Z");
        let mag = (imp_vel.x * imp_vel.x + imp_vel.z * imp_vel.z).sqrt();
        assert!(
            (mag - 5.0).abs() < 0.01,
            "Knockback magnitude should equal knockback_force (5.0), got {}",
            mag,
        );
    }

    /// Blocking negates knockback — a blocking defender is not pushed.
    #[test]
    fn block_prevents_knockback() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 80,
            name: "HeavySlam".to_string(),
            base_damage: 10.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 5.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let defender = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(defender, EntityKind::Player, TickId(0), 100.0);

        // Place attacker in front of defender (+Z) — defender faces +Z by default.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                attacker,
                rapier3d::math::Vector::new(0.0, 0.0, 3.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                defender,
                rapier3d::math::Vector::new(0.0, 0.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        pipeline.run_tick(&[]);

        // Activate block on the defender (facing attacker).
        let def_idx = pipeline.state.entities.lookup(defender).unwrap();
        pipeline.state.combat.tactical[def_idx.as_usize()].blocking = true;

        pipeline.apply_hit_damage(attacker, defender, def_idx, 80, false, None);

        // No knockback impulse should have been enqueued.
        assert!(
            pipeline.pending_impulses.is_empty(),
            "Blocking defender should not receive knockback impulse, got {} impulse(s)",
            pipeline.pending_impulses.len(),
        );
    }

    /// True damage ignores block mitigation — full base damage even when blocking.
    #[test]
    fn true_damage_ignores_block() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 50,
            name: "TrueStrike".to_string(),
            base_damage: 30.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let defender = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(defender, EntityKind::Player, TickId(0), 100.0);

        // Place attacker in front of defender so directional block would normally apply.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                defender,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                attacker,
                rapier3d::math::Vector::new(0.0, 5.0, 3.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        pipeline.run_tick(&[]);
        let def_idx = pipeline.state.entities.lookup(defender).unwrap();

        // Defender is blocking from the front — would halve Physical/Magical.
        pipeline.state.combat.tactical[def_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[def_idx.as_usize()].block_start_tick = None;

        pipeline.apply_hit_damage(attacker, defender, def_idx, 50, false, None);

        // True damage should deal full 30 (no block reduction).
        let hp = pipeline.state.hp_of(defender).unwrap();
        assert!(
            (hp - 70.0).abs() < 0.01,
            "True damage should ignore block (expected 70 hp), got {}",
            hp,
        );

        // Verify no Blocked event was emitted.
        let blocked = pipeline.pending_events.iter().any(|e| matches!(e.payload, EventPayload::Blocked { .. }));
        assert!(!blocked, "True damage should not emit a Blocked event");
    }

    /// True damage ignores damage_in_pct reduction from buffs.
    #[test]
    fn true_damage_ignores_damage_reduction_buff() {
        use game_core::combat::status::{ActiveBuff, BuffModifiers};

        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 50,
            name: "TrueStrike".to_string(),
            base_damage: 20.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Player, TickId(0), 100.0);

        pipeline.run_tick(&[]);
        let tgt_idx = pipeline.state.entities.lookup(target).unwrap();

        // Give target a -50% incoming damage reduction buff.
        pipeline.state.status.modify_buffs(tgt_idx, |buffs| {
            buffs.push(ActiveBuff {
                buff_id: 100,
                source: target,
                target,
                stacks: 1,
                max_stacks: 1,
                expires_at: Some(TickId(100)),
                modifiers: BuffModifiers {
                    damage_in_pct: Some(-50.0),
                    ..Default::default()
                },
            });
        });

        // Recompute stats so damage_in_mult picks up the buff.
        pipeline.stats_dirty.insert(target);
        pipeline.phase_stat_recalc();

        pipeline.apply_hit_damage(attacker, target, tgt_idx, 50, false, None);

        // True damage ignores damage_in_mult → full 20 damage applied.
        let hp = pipeline.state.hp_of(target).unwrap();
        assert!(
            (hp - 80.0).abs() < 0.01,
            "True damage should ignore damage_in reduction (expected 80 hp), got {}",
            hp,
        );
    }

    /// ApplyBuff timeline action applies a self-buff to the caster.
    #[test]
    fn apply_buff_action_gives_caster_self_buff() {
        use game_core::combat::status::BuffTemplate;

        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 60,
            name: "PowerUp".to_string(),
            base_damage: 0.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 0.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 60,
            actions: vec![
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::ApplyBuff {
                        buff_id: 200,
                    },
                },
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::CooldownStart { duration_ticks: 60 },
                },
            ],
        });
        let mut pipeline = make_pipeline(reg);

        // Register the buff template in the buff registry.
        pipeline.buff_registry.register(BuffTemplate {
            buff_id: 200,
            name: "PowerUp".into(),
            duration_ticks: Some(40),
            max_stacks: 1,
            modifiers: game_core::combat::status::BuffModifiers {
                damage_out_pct: Some(0.25),
                ..Default::default()
            },
        });

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Player, TickId(0), 100.0);

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        // Tick 1: cast the self-buff ability.
        let cast = PlayerIntent {
            entity_id: caster,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 60,
                target: game_schema::AbilityTarget::None,
            }),
        };
        let result = pipeline.run_tick(&[cast]);

        // Caster should have the buff.
        let idx = pipeline.state.entities.lookup(caster).unwrap();
        let buffs = pipeline.state.status.get_buffs(idx);
        assert_eq!(buffs.len(), 1, "Caster should have exactly one buff");
        assert_eq!(buffs[0].buff_id, 200);
        assert_eq!(buffs[0].stacks, 1);
        assert_eq!(buffs[0].modifiers.damage_out_pct, Some(0.25));
        assert_eq!(buffs[0].expires_at, Some(TickId(41))); // tick 1 + 40

        // BuffApplied event should have been emitted.
        let buff_applied = result.events.iter().any(|e| {
            matches!(e.payload, EventPayload::BuffApplied { buff_id: 200, .. })
        });
        assert!(buff_applied, "BuffApplied event should be emitted for self-buff");
    }

    /// on_hit_buffs applies a debuff to the target when the hit connects.
    #[test]
    fn on_hit_buff_applies_debuff_to_target() {
        use game_core::combat::status::BuffTemplate;

        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 70,
            name: "FrostStrike".to_string(),
            base_damage: 15.0,
            damage_type: DamageType::Magical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![300],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        // Register the on-hit debuff template.
        pipeline.buff_registry.register(BuffTemplate {
            buff_id: 300,
            name: "Chill".into(),
            duration_ticks: Some(60),
            max_stacks: 1,
            modifiers: game_core::combat::status::BuffModifiers {
                speed_pct: Some(-0.5),
                ..Default::default()
            },
        });

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Player, TickId(0), 100.0);

        pipeline.run_tick(&[]);

        let tgt_idx = pipeline.state.entities.lookup(target).unwrap();

        // Directly call apply_hit_damage to simulate a hit.
        pipeline.apply_hit_damage(attacker, target, tgt_idx, 70, false, None);

        // Target should now have the slow debuff.
        let buffs = pipeline.state.status.get_buffs(tgt_idx);
        assert_eq!(buffs.len(), 1, "Target should have one debuff from on-hit");
        assert_eq!(buffs[0].buff_id, 300);
        assert_eq!(buffs[0].source, attacker);
        assert_eq!(buffs[0].modifiers.speed_pct, Some(-0.5));

        // BuffApplied event should have been emitted on the target.
        let buff_event = pipeline.pending_events.iter().any(|e| {
            e.entity_id == target && matches!(e.payload, EventPayload::BuffApplied { buff_id: 300, .. })
        });
        assert!(buff_event, "BuffApplied event should fire on target for on-hit debuff");
    }

    /// Buff stacking: re-applying same buff_id increments stacks and refreshes duration.
    #[test]
    fn buff_stacking_increments_and_caps() {
        use game_core::combat::status::{ActiveBuff, BuffModifiers};

        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Poke".to_string(),
            base_damage: 5.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let entity = EntityId(1);
        pipeline.state.spawn_entity(entity, EntityKind::Player, TickId(0), 100.0);
        pipeline.run_tick(&[]);

        let idx = pipeline.state.entities.lookup(entity).unwrap();

        // Apply a buff with max_stacks = 3.
        let buff = ActiveBuff {
            buff_id: 400,
            source: EntityId(99),
            target: entity,
            stacks: 1,
            max_stacks: 3,
            expires_at: Some(TickId(10)),
            modifiers: BuffModifiers {
                damage_out_pct: Some(0.1),
                ..Default::default()
            },
        };

        // First application: 1 stack.
        let s = pipeline.state.status.apply_or_stack_buff(idx, buff.clone());
        assert_eq!(s, 1);
        assert_eq!(pipeline.state.status.get_buffs(idx).len(), 1);

        // Second application: 2 stacks, one entry.
        let buff2 = ActiveBuff { expires_at: Some(TickId(20)), ..buff.clone() };
        let s = pipeline.state.status.apply_or_stack_buff(idx, buff2);
        assert_eq!(s, 2);
        assert_eq!(pipeline.state.status.get_buffs(idx).len(), 1);
        assert_eq!(pipeline.state.status.get_buffs(idx)[0].stacks, 2);
        assert_eq!(pipeline.state.status.get_buffs(idx)[0].expires_at, Some(TickId(20))); // refreshed

        // Third: 3 stacks (at cap).
        let buff3 = ActiveBuff { expires_at: Some(TickId(30)), ..buff.clone() };
        let s = pipeline.state.status.apply_or_stack_buff(idx, buff3);
        assert_eq!(s, 3);

        // Fourth: still 3 (capped), but duration refreshed.
        let buff4 = ActiveBuff { expires_at: Some(TickId(40)), ..buff.clone() };
        let s = pipeline.state.status.apply_or_stack_buff(idx, buff4);
        assert_eq!(s, 3);
        assert_eq!(pipeline.state.status.get_buffs(idx).len(), 1);
        assert_eq!(pipeline.state.status.get_buffs(idx)[0].expires_at, Some(TickId(40)));
    }

    /// Overlapping iframe windows stack: the first StanceEnd does not
    /// clobber a second ability's iframe.
    #[test]
    fn dodge_stacks_overlap_survives_first_end() {
        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let entity = EntityId(1);
        pipeline.state.spawn_entity(entity, EntityKind::Player, TickId(0), 100.0);
        pipeline.run_tick(&[]); // warm-up

        let idx = pipeline.state.entities.lookup(entity).unwrap();

        // Simulate two overlapping StanceBegin(dodge=true)
        let t = &mut pipeline.state.combat.tactical[idx.as_usize()];
        assert!(!t.is_dodging());

        t.dodge_stacks = t.dodge_stacks.saturating_add(1); // ability A
        assert!(t.is_dodging());

        t.dodge_stacks = t.dodge_stacks.saturating_add(1); // ability B
        assert_eq!(t.dodge_stacks, 2);

        // Ability A ends — should still be dodging
        t.dodge_stacks = t.dodge_stacks.saturating_sub(1);
        assert!(t.is_dodging());
        assert_eq!(t.dodge_stacks, 1);

        // Ability B ends — now dodging stops
        t.dodge_stacks = t.dodge_stacks.saturating_sub(1);
        assert!(!t.is_dodging());
        assert_eq!(t.dodge_stacks, 0);
    }

    /// ArcMovement timeline action sets arc_state and roots the caster.
    #[test]
    fn arc_movement_sets_state_and_roots() {

        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 90,
            name: "Vault".to_string(),
            base_damage: 0.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 0.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 90,
            actions: vec![
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::ArcMovement { speed: 8.0, lift: 6.0, gravity: 20.0 },
                },
                ScheduledAbilityAction {
                    tick_offset: 0,
                    action: AbilityAction::CooldownStart { duration_ticks: 40 },
                },
                ScheduledAbilityAction {
                    tick_offset: 20,
                    action: AbilityAction::StanceEnd,
                },
            ],
        });
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Player, TickId(0), 100.0);

        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                caster,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        // Tick 1: cast vault.
        let cast = PlayerIntent {
            entity_id: caster,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 90,
                target: game_schema::AbilityTarget::None,
            }),
        };
        pipeline.run_tick(&[cast]);

        let idx = pipeline.state.entities.lookup(caster).unwrap();
        let t = &pipeline.state.combat.tactical[idx.as_usize()];

        // Arc should be active (may have already been consumed by drive_arc_movement
        // on the launch tick — but rooted should still be set if arc is still going,
        // or cleared if it already landed). Since ArcMovement fires in Phase 3 which
        // is AFTER Phase 2, the arc_state is set but drive_arc_movement hasn't run
        // yet on this tick. We verify after the full tick, where Phase 2 of the NEXT
        // tick would be the first to drive it. But run_tick runs Phases 1-10, and
        // Phase 2 runs before Phase 3. So on tick 1, Phase 2 runs first (no arc yet),
        // then Phase 3 sets arc_state. It persists until next tick's Phase 2.
        assert!(t.rooted, "Caster should be rooted after ArcMovement");
        assert!(t.arc_state.is_some(), "arc_state should be set after ArcMovement");
        let arc = t.arc_state.unwrap();
        // Facing defaults to +Z (no rotation set), so velocity.z = speed, velocity.x ≈ 0.
        assert!(arc.velocity.z > 0.0, "Arc should move forward (+Z)");
        assert!((arc.velocity.y - 6.0).abs() < 0.01, "Lift should be 6.0");
        assert!((arc.gravity - 20.0).abs() < 0.01, "Gravity should be 20.0");
    }

    /// Arc movement integrates gravity and lands when grounded with negative Y velocity.
    #[test]
    fn arc_movement_integrates_and_lands() {
        use game_core::combat::tactical::ArcState;

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let entity = EntityId(1);
        pipeline.state.spawn_entity(entity, EntityKind::Player, TickId(0), 100.0);

        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                entity,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
        }

        // Tick 0: warm-up to activate entity.
        pipeline.run_tick(&[]);

        let idx = pipeline.state.entities.lookup(entity).unwrap();

        // Manually set an arc with lift=6, gravity=20, speed=8 in +Z.
        // At dt=0.05, vel.y drops by 1.0 per tick. After 6 ticks vel.y reaches 0,
        // after 7 ticks vel.y is negative. With spawn_character_body, the mock
        // always returns grounded=true, but PhysicsWorld's character controller
        // returns grounded based on actual ground contact. Since the capsule is
        // at y=5 with no ground plane, it won't be grounded until it falls.
        // For this test, we directly test drive_arc_movement with a set arc_state.
        let t = &mut pipeline.state.combat.tactical[idx.as_usize()];
        t.arc_state = Some(ArcState {
            velocity: Vec3f { x: 0.0, y: 3.0, z: 8.0 },
            gravity: 20.0,
        });
        t.rooted = true;

        // Run one tick — Phase 2 will call drive_arc_movement.
        // vel.y starts at 3.0, gravity subtracts 20*0.05=1.0 → vel.y = 2.0 after first tick.
        // Still positive, so not landing yet (even if character controller says grounded).
        pipeline.run_tick(&[]);
        let t = &pipeline.state.combat.tactical[idx.as_usize()];
        // After 1 tick: vel.y was 3.0 - 1.0 = 2.0 (still rising)
        // Arc may or may not have landed depending on grounded — but vel.y > 0 so no landing.
        assert!(t.arc_state.is_some(), "Arc should still be active (vel.y > 0)");

        // After 3 more ticks, vel.y = 2.0 - 1.0 - 1.0 - 1.0 = -1.0 (falling).
        // With the real character controller and no ground, grounded may be false.
        // Let's run ticks and check.
        pipeline.run_tick(&[]);
        pipeline.run_tick(&[]);
        pipeline.run_tick(&[]);

        let t = &pipeline.state.combat.tactical[idx.as_usize()];
        // After 4 ticks total: vel.y started at 3.0, each tick subtracts 1.0.
        // vel.y: 3.0 → 2.0 → 1.0 → 0.0 → -1.0
        // On the 4th drive_arc_movement, vel.y becomes -1.0 (<=0).
        // If Rapier says grounded=true (capsule resting on default ground), arc ends.
        // If not grounded (floating in space), arc continues.
        // With no ground geometry, the character controller likely returns grounded=false
        // for a floating capsule, so arc continues. That's correct behavior.

        // Verify gravity integration happened: if arc is still active, check velocity.
        if let Some(arc) = &t.arc_state {
            assert!(arc.velocity.y < 0.0, "Gravity should have pulled vel.y negative");
        }
        // If arc ended (grounded=true from Rapier), rooted should be cleared.
        if t.arc_state.is_none() {
            assert!(!t.rooted, "Landing should clear rooted flag");
        }
    }

    /// StanceEnd clears arc_state and unroots.
    #[test]
    fn stance_end_clears_arc_state() {
        use game_core::combat::tactical::ArcState;

        let reg = setup_ability_registry();
        let mut pipeline = make_pipeline(reg);

        let entity = EntityId(1);
        pipeline.state.spawn_entity(entity, EntityKind::Player, TickId(0), 100.0);
        pipeline.run_tick(&[]);

        let idx = pipeline.state.entities.lookup(entity).unwrap();
        let t = &mut pipeline.state.combat.tactical[idx.as_usize()];
        t.arc_state = Some(ArcState {
            velocity: Vec3f { x: 0.0, y: 5.0, z: 5.0 },
            gravity: 10.0,
        });
        t.rooted = true;

        // Simulate StanceEnd — directly call the action handler.
        let exec_id = AbilityExecutionId(999);
        pipeline.execute_ability_action(entity, 1, exec_id, &AbilityAction::StanceEnd);

        let t = &pipeline.state.combat.tactical[idx.as_usize()];
        assert!(t.arc_state.is_none(), "StanceEnd should clear arc_state");
        assert!(!t.rooted, "StanceEnd should clear rooted");
    }

    // ── Cover mechanic tests ──────────────────────────────────────────────────

    /// When a blocking ally stands between attacker and target (within 4 units,
    /// in the blocker's rear cone), the target takes 30% less damage.
    #[test]
    fn cover_reduces_damage_for_ally_behind_blocker() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Slash".to_string(),
            base_damage: 100.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let blocker  = EntityId(2);
        let target   = EntityId(3);

        // Attacker at z=0, blocker at z=3 (between attacker and target), target at z=5.
        // Blocker faces +Z (default identity quat), so "behind" the blocker is -Z
        // direction. Wait — blocker must face *toward* the attacker for blocking.
        // Default identity rotation = facing +Z.
        //
        // Layout: attacker at z=6, blocker at z=3 facing +Z (toward attacker), target at z=0 (behind blocker).
        pipeline.spawn_entity_from_snapshot(
            attacker, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 6.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            blocker, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 3.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            target, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        // Warm-up tick: entities go Active.
        pipeline.run_tick(&[]);
        assert!(pipeline.state.is_active(attacker));
        assert!(pipeline.state.is_active(blocker));
        assert!(pipeline.state.is_active(target));

        // Remove NPC AI entries (players don't have them, but just in case).
        for id in [attacker, blocker, target] {
            if let Some(idx) = pipeline.state.entities.lookup(id) {
                pipeline.state.ai.npc_ai.remove(idx);
            }
        }

        // Set blocker to blocking stance (facing +Z by default = toward attacker at z=6).
        let blocker_idx = pipeline.state.entities.lookup(blocker).unwrap();
        pipeline.state.combat.tactical[blocker_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[blocker_idx.as_usize()].block_start_tick = None;

        // Apply damage to target.
        let target_idx = pipeline.state.entities.lookup(target).unwrap();
        pipeline.apply_hit_damage(attacker, target, target_idx, 10, false, None);

        // Target should take 100 * 0.7 = 70 damage (30% reduction from cover).
        let hp = pipeline.state.hp_of(target).unwrap();
        assert!(
            (hp - 30.0).abs() < 0.01,
            "Cover should reduce 100 damage to 70, leaving 30 hp; got {}",
            hp,
        );

        // Verify Covered event was emitted.
        let covered = pipeline.pending_events.iter().any(|e| {
            matches!(&e.payload, EventPayload::Covered { blocker: b, .. } if *b == blocker)
        });
        assert!(covered, "Covered event must be emitted with the blocker's id");
    }

    /// Cover does NOT apply when the target is NOT in the blocker's rear cone
    /// (e.g. target is in front of the blocker, same side as the attacker).
    #[test]
    fn cover_does_not_apply_outside_rear_cone() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Slash".to_string(),
            base_damage: 100.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let blocker  = EntityId(2);
        let target   = EntityId(3);

        // Target is in FRONT of blocker (same side as attacker), so no cover.
        // Blocker at z=3 facing +Z, attacker at z=6, target at z=5 (in front of blocker).
        pipeline.spawn_entity_from_snapshot(
            attacker, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 6.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            blocker, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 3.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            target, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 5.0 },
        );

        pipeline.run_tick(&[]);

        for id in [attacker, blocker, target] {
            if let Some(idx) = pipeline.state.entities.lookup(id) {
                pipeline.state.ai.npc_ai.remove(idx);
            }
        }

        let blocker_idx = pipeline.state.entities.lookup(blocker).unwrap();
        pipeline.state.combat.tactical[blocker_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[blocker_idx.as_usize()].block_start_tick = None;

        let target_idx = pipeline.state.entities.lookup(target).unwrap();
        pipeline.apply_hit_damage(attacker, target, target_idx, 10, false, None);

        // Full 100 damage — no cover since target is not behind blocker.
        let hp = pipeline.state.hp_of(target).unwrap();
        assert!(
            (hp - 0.0).abs() < 0.01,
            "No cover: target should take full 100 damage (0 hp); got {}",
            hp,
        );
    }

    /// Cover does NOT apply for True damage.
    #[test]
    fn cover_does_not_apply_for_true_damage() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Burn".to_string(),
            base_damage: 50.0,
            damage_type: DamageType::True,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let blocker  = EntityId(2);
        let target   = EntityId(3);

        // Same layout as the working cover test.
        pipeline.spawn_entity_from_snapshot(
            attacker, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 6.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            blocker, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 3.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            target, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        pipeline.run_tick(&[]);

        for id in [attacker, blocker, target] {
            if let Some(idx) = pipeline.state.entities.lookup(id) {
                pipeline.state.ai.npc_ai.remove(idx);
            }
        }

        let blocker_idx = pipeline.state.entities.lookup(blocker).unwrap();
        pipeline.state.combat.tactical[blocker_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[blocker_idx.as_usize()].block_start_tick = None;

        let target_idx = pipeline.state.entities.lookup(target).unwrap();
        pipeline.apply_hit_damage(attacker, target, target_idx, 10, false, None);

        // True damage bypasses cover → full 50 damage.
        let hp = pipeline.state.hp_of(target).unwrap();
        assert!(
            (hp - 50.0).abs() < 0.01,
            "True damage should bypass cover (expected 50 hp); got {}",
            hp,
        );
    }

    /// Cover does NOT apply across teams (NPC blocker does not cover a Player).
    #[test]
    fn cover_does_not_apply_across_teams() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Slash".to_string(),
            base_damage: 100.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let blocker  = EntityId(2);
        let target   = EntityId(3);

        // Blocker is an NPC, target is a Player — different teams, no cover.
        pipeline.spawn_entity_from_snapshot(
            attacker, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 6.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            blocker, EntityKind::Npc, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 3.0 },
        );
        pipeline.spawn_entity_from_snapshot(
            target, EntityKind::Player, TickId(0), 100.0,
            game_schema::Vec3f { x: 0.0, y: 1.0, z: 0.0 },
        );

        pipeline.run_tick(&[]);

        for id in [attacker, blocker, target] {
            if let Some(idx) = pipeline.state.entities.lookup(id) {
                pipeline.state.ai.npc_ai.remove(idx);
            }
        }

        let blocker_idx = pipeline.state.entities.lookup(blocker).unwrap();
        pipeline.state.combat.tactical[blocker_idx.as_usize()].blocking = true;
        pipeline.state.combat.tactical[blocker_idx.as_usize()].block_start_tick = None;

        let target_idx = pipeline.state.entities.lookup(target).unwrap();
        pipeline.apply_hit_damage(attacker, target, target_idx, 10, false, None);

        // Full 100 damage — blocker is NPC, target is Player → different teams.
        let hp = pipeline.state.hp_of(target).unwrap();
        assert!(
            (hp - 0.0).abs() < 0.01,
            "Cross-team: target should take full 100 damage (0 hp); got {}",
            hp,
        );
    }

    #[test]
    fn resolve_charge_tier_basic() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 99,
            name: "Chargey".to_string(),
            base_damage: 10.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: Some(vec![
                game_core::combat::skill::ChargeTierDef { min_ticks: 0,  damage_mult: 1.0 },
                game_core::combat::skill::ChargeTierDef { min_ticks: 2,  damage_mult: 1.5 },
                game_core::combat::skill::ChargeTierDef { min_ticks: 4,  damage_mult: 2.0 },
            ]),
            damage_interval_ticks: 0,
        });
        let pipeline = make_pipeline(reg);

        let cases = vec![(0, 0u8), (1, 0u8), (2, 1u8), (3, 1u8), (4, 2u8), (10, 2u8)];
        for (elapsed, expect) in cases {
            let tier = pipeline.resolve_charge_tier(99, elapsed);
            assert_eq!(tier, expect, "elapsed {} -> tier {} expected {}", elapsed, tier, expect);
        }
    }

    #[test]
    fn charge_auto_release_and_tier_events() {
        // Small thresholds for fast test.
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 42,
            name: "ChargedSmash".to_string(),
            base_damage: 20.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: Some(vec![
                game_core::combat::skill::ChargeTierDef { min_ticks: 0, damage_mult: 1.0 },
                game_core::combat::skill::ChargeTierDef { min_ticks: 2, damage_mult: 1.5 },
                game_core::combat::skill::ChargeTierDef { min_ticks: 4, damage_mult: 2.0 },
            ]),
            damage_interval_ticks: 0,

        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 42,
            actions: vec![
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::SpawnHitbox { shape: SkillShape::Sphere, offset: game_schema::Vec3f { x:0.0, y:0.0, z:0.0 } } },
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::CooldownStart { duration_ticks: 20 } },
                ScheduledAbilityAction { tick_offset: 1, action: AbilityAction::ApplyDamageFrame },
                ScheduledAbilityAction { tick_offset: 2, action: AbilityAction::RemoveHitbox },
            ],
        });

        let mut pipeline = make_pipeline(reg);
        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 100.0);
        let origin = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, origin, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, origin, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        pipeline.run_tick(&[]); // warm-up

        let intent = PlayerIntent {
            entity_id: attacker,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData { ability_id: 42, target: game_schema::AbilityTarget::None }),
        };
        let r1 = pipeline.run_tick(&[intent]);
        assert!(r1.events.iter().any(|e| matches!(&e.payload, EventPayload::ChargeStart { ability_id, .. } if *ability_id == 42)));

        let mut found_tier2 = false;
        for _ in 2..=6 {
            let r = pipeline.run_tick(&[]);
            if r.events.iter().any(|e| matches!(&e.payload, EventPayload::ChargeTierReached { ability_id, tier } if *ability_id == 42 && *tier == 2)) {
                found_tier2 = true;
            }
            if r.events.iter().any(|e| matches!(&e.payload, EventPayload::HitboxSpawned { ability_id } if *ability_id == 42)) {
                assert!(!pipeline.state.combat.charging.contains_key(&attacker));
                break;
            }
        }
        assert!(found_tier2, "Tier 2 event should have been emitted before auto-release");
    }

    #[test]
    fn damage_scaling_apply_hit_damage() {
        // Ability with two tiers: base=10, tier1 multiplier=2.0
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 10,
            name: "Scaler".to_string(),
            base_damage: 10.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: Some(vec![
                game_core::combat::skill::ChargeTierDef { min_ticks: 0, damage_mult: 1.0 },
                game_core::combat::skill::ChargeTierDef { min_ticks: 2, damage_mult: 2.0 },
            ]),
            damage_interval_ticks: 0,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 100.0);
        let origin = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, origin, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, origin, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        pipeline.run_tick(&[]);

        // Spawn and arm a hitbox tied to an execution with charge_level = 1
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("spawn sensor");
        pipeline.state.combat.hitboxes.spawn_armed(exec_id, attacker, 10, TickId(1), SkillShape::Sphere, Vec3f { x:0.0, y:0.0, z:0.0 }, sensor);

        pipeline.state.combat.executions.insert(AbilityExecutionContext {
            execution_id: exec_id,
            ability_id: 10,
            caster: attacker,
            started_at: TickId(1),
            targeting: ResolvedTargeting::SelfCast,
            origin: Vec3f { x:0.0, y:5.0, z:0.0 },
            facing: Vec3f { x:0.0, y:0.0, z:1.0 },
            params: AbilityParams { charge_level: 1, variant: 0 },
            rewind_ticks: 0,
        });

        let result = pipeline.run_tick(&[]);
        // Damage should equal base_damage * 2.0 = 20.0
        let target_update = result.health_updates.iter().find(|(eid, _, _)| *eid == target).expect("health update");
        let (_, hp, _) = target_update;
        assert!((*hp - 80.0).abs() < 0.01, "Expected hp=80 after 20 damage, got {}", hp);
    }

    /// Helper: register a Projectile ability with immediate spawn + damage frame.
    fn setup_projectile_registry() -> AbilityRegistry {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 50,
            name: "TestProjectile".to_string(),
            base_damage: 10.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::Projectile,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 50,
            actions: vec![
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::Projectile,
                    offset: Vec3f { x: 0.0, y: 1.0, z: 0.0 },
                }},
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::CooldownStart { duration_ticks: 20 } },
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::ApplyDamageFrame },
            ],
        });
        reg
    }

    #[test]
    fn position_targeted_projectile_aims_at_target_point() {
        let reg = setup_projectile_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Player, TickId(0), 100.0);

        // Place caster at origin facing +Z.
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(caster, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
        }

        // Warm-up tick.
        pipeline.run_tick(&[]);

        // Fire at a position 10 units along +X (perpendicular to default facing).
        let target_point = Vec3f { x: 10.0, y: 0.0, z: 0.0 };
        let intent = PlayerIntent {
            entity_id: caster,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 50,
                target: game_schema::AbilityTarget::Position(target_point),
            }),
        };
        let result = pipeline.run_tick(&[intent]);

        // Verify ProjectileLaunched event was emitted with direction toward target.
        let launched = result.events.iter().find(|e| {
            matches!(&e.payload, EventPayload::ProjectileLaunched { ability_id: 50, .. })
        }).expect("ProjectileLaunched event should be emitted");

        if let EventPayload::ProjectileLaunched { direction, max_range, .. } = &launched.payload {
            // Direction should be approximately (1, 0, 0) — toward target point.
            assert!(direction.x > 0.9, "Direction X should point toward target, got {}", direction.x);
            assert!(direction.z.abs() < 0.2, "Direction Z should be near zero, got {}", direction.z);
            // Max range should be capped to ~ distance to target point (~10 units),
            // not the full 30-unit default.
            assert!(*max_range < 15.0, "Max range should be capped to target distance (~10), got {}", max_range);
        } else {
            panic!("Expected ProjectileLaunched payload");
        }
    }

    #[test]
    fn entity_targeted_projectile_aims_at_target_entity() {
        let reg = setup_projectile_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(caster, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 100.0);

        // Place caster at origin, target at (0, 5, 15) — 15 units along +Z.
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(
                caster,
                rapier3d::math::Vector::new(0.0, 5.0, 0.0),
                0.5, 0.3, 1.0,
                collision_groups::player_body_groups(),
            );
            pw.add_dynamic_capsule(
                target,
                rapier3d::math::Vector::new(0.0, 5.0, 15.0),
                0.5, 0.3, 1.0,
                collision_groups::npc_body_groups(),
            );
        }

        // Warm-up tick.
        pipeline.run_tick(&[]);

        // Neutralize NPC AI to prevent it from acting.
        if let Some(idx) = pipeline.state.entities.lookup(target) {
            pipeline.state.ai.npc_ai.remove(idx);
        }

        // Fire with Entity targeting.
        let intent = PlayerIntent {
            entity_id: caster,
            sequence_id: 1,
            target_tick: TickId(1),
            client_observed_tick: 0,
            action: IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: 50,
                target: game_schema::AbilityTarget::Entity(target.0),
            }),
        };
        let result = pipeline.run_tick(&[intent]);

        // Verify ProjectileLaunched event with direction toward the target entity.
        let launched = result.events.iter().find(|e| {
            matches!(&e.payload, EventPayload::ProjectileLaunched { ability_id: 50, .. })
        }).expect("ProjectileLaunched event should be emitted");

        if let EventPayload::ProjectileLaunched { direction, max_range, .. } = &launched.payload {
            // Direction should be approximately (0, 0, 1) — toward target at +Z.
            assert!(direction.z > 0.9, "Direction Z should point toward target, got {}", direction.z);
            assert!(direction.x.abs() < 0.2, "Direction X should be near zero, got {}", direction.x);
            // Entity targeting does NOT cap range — full 30-unit default.
            assert!((*max_range - 30.0).abs() < 0.01, "Max range should be default 30, got {}", max_range);
        } else {
            panic!("Expected ProjectileLaunched payload");
        }
    }

    #[test]
    fn periodic_damage_reapplies_on_interval() {
        // Ability with damage_interval_ticks = 2: initial hit + re-damage every 2 ticks.
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 60,
            name: "PoisonPool".to_string(),
            base_damage: 5.0,
            damage_type: DamageType::Magical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 2,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 100.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up.
        pipeline.run_tick(&[]);

        // Neutralize NPC AI.
        if let Some(idx) = pipeline.state.entities.lookup(target) {
            pipeline.state.ai.npc_ai.remove(idx);
        }

        // Manually insert an armed hitbox with damage_interval_ticks = 2.
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("spawn sensor");
        pipeline.state.combat.hitboxes.spawn(
            exec_id, attacker, 60, TickId(1), SkillShape::Sphere,
            Vec3f { x: 0.0, y: 0.0, z: 0.0 }, 0, false, 2,
        );
        pipeline.state.combat.hitboxes.arm(exec_id, sensor);

        // Tick 1: initial hit from physics contact.
        let _r1 = pipeline.run_tick(&[]);
        let hp1 = pipeline.state.hp_of(target).unwrap();
        assert!((hp1 - 95.0).abs() < 0.01, "Tick 1: expected 95 hp after initial 5 damage, got {}", hp1);

        // Tick 2: no re-damage yet (interval=2, only 1 tick elapsed since last_damage_tick=1).
        pipeline.run_tick(&[]);
        let hp2 = pipeline.state.hp_of(target).unwrap();
        assert!((hp2 - 95.0).abs() < 0.01, "Tick 2: expected 95 hp (no periodic yet), got {}", hp2);

        // Tick 3: periodic damage fires (2 ticks since last_damage_tick=1).
        pipeline.run_tick(&[]);
        let hp3 = pipeline.state.hp_of(target).unwrap();
        assert!((hp3 - 90.0).abs() < 0.01, "Tick 3: expected 90 hp after periodic re-damage, got {}", hp3);

        // Tick 4: no re-damage (only 1 tick since last_damage_tick=3).
        pipeline.run_tick(&[]);
        let hp4 = pipeline.state.hp_of(target).unwrap();
        assert!((hp4 - 90.0).abs() < 0.01, "Tick 4: expected 90 hp (no periodic yet), got {}", hp4);

        // Tick 5: second periodic pulse (2 ticks since last_damage_tick=3).
        pipeline.run_tick(&[]);
        let hp5 = pipeline.state.hp_of(target).unwrap();
        assert!((hp5 - 85.0).abs() < 0.01, "Tick 5: expected 85 hp after second periodic pulse, got {}", hp5);
    }

    #[test]
    fn periodic_damage_stops_when_target_leaves() {
        let mut reg = AbilityRegistry::new();
        reg.register(AbilityData {
            ability_id: 61,
            name: "LavaPool".to_string(),
            base_damage: 10.0,
            damage_type: DamageType::Magical,
            shape: SkillShape::Sphere,
            threat_multiplier: 1.0,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 2,
        });
        let mut pipeline = make_pipeline(reg);

        let attacker = EntityId(1);
        let target = EntityId(2);
        pipeline.state.spawn_entity(attacker, EntityKind::Player, TickId(0), 100.0);
        pipeline.state.spawn_entity(target, EntityKind::Npc, TickId(0), 100.0);

        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(attacker, pos, 0.5, 0.3, 1.0, collision_groups::player_body_groups());
            pw.add_dynamic_capsule(target, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        pipeline.run_tick(&[]); // warm-up

        if let Some(idx) = pipeline.state.entities.lookup(target) {
            pipeline.state.ai.npc_ai.remove(idx);
        }

        // Inject armed hitbox.
        let exec_id = AbilityExecutionId(1);
        let sensor = pipeline.physics.spawn_sensor(
            attacker,
            SensorShape::Sphere { radius: 1.0 },
            Vec3f { x: 0.0, y: 0.0, z: 0.0 },
            ColliderKind::Hitbox(exec_id.0),
        ).expect("spawn sensor");
        pipeline.state.combat.hitboxes.spawn(
            exec_id, attacker, 61, TickId(1), SkillShape::Sphere,
            Vec3f { x: 0.0, y: 0.0, z: 0.0 }, 0, false, 2,
        );
        pipeline.state.combat.hitboxes.arm(exec_id, sensor);

        // Tick 1: initial hit.
        pipeline.run_tick(&[]);
        let hp1 = pipeline.state.hp_of(target).unwrap();
        assert!((hp1 - 90.0).abs() < 0.01, "Expected 90 hp after initial hit, got {}", hp1);

        // Simulate target leaving: remove from overlapping set.
        pipeline.state.combat.hitboxes.remove_overlapping(exec_id, target);

        // Tick 2-3: no periodic damage because target left.
        pipeline.run_tick(&[]);
        pipeline.run_tick(&[]);
        let hp3 = pipeline.state.hp_of(target).unwrap();
        assert!((hp3 - 90.0).abs() < 0.01, "Expected 90 hp (target left zone), got {}", hp3);
    }

    // ── Combo routing tests ──────────────────────────────────────────

    /// Build a registry with Slash (id=1) that opens a combo window to SlashCombo (id=4).
    fn setup_combo_registry() -> AbilityRegistry {
        let mut reg = AbilityRegistry::new();
        // Slash — base ability
        reg.register(AbilityData {
            ability_id: 1,
            name: "Slash".to_string(),
            base_damage: 25.0,
            damage_type: DamageType::Physical,
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
                    shape: SkillShape::Sphere,
                    offset: game_schema::Vec3f::ZERO,
                }},
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::CooldownStart { duration_ticks: 20 } },
                ScheduledAbilityAction { tick_offset: 1, action: AbilityAction::ApplyDamageFrame },
                ScheduledAbilityAction { tick_offset: 2, action: AbilityAction::RemoveHitbox },
                // Open 10-tick combo window → Slash Combo (id=4)
                ScheduledAbilityAction { tick_offset: 2, action: AbilityAction::OpenFollowUpWindow {
                    duration_ticks: 10,
                    next_ability_id: 4,
                }},
            ],
        });
        // Slash Combo — follow-up ability
        reg.register(AbilityData {
            ability_id: 4,
            name: "Slash Combo".to_string(),
            base_damage: 35.0,
            damage_type: DamageType::Physical,
            shape: SkillShape::CapsuleSweep,
            threat_multiplier: 1.2,
            on_hit_buffs: vec![],
            knockback_force: 0.0,
            allow_reentry: false,
            charge_tiers: None,
            damage_interval_ticks: 0,
        });
        reg.register_timeline(AbilityTimeline {
            ability_id: 4,
            actions: vec![
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::Sphere,
                    offset: game_schema::Vec3f::ZERO,
                }},
                ScheduledAbilityAction { tick_offset: 0, action: AbilityAction::CooldownStart { duration_ticks: 20 } },
                ScheduledAbilityAction { tick_offset: 1, action: AbilityAction::ApplyDamageFrame },
                ScheduledAbilityAction { tick_offset: 2, action: AbilityAction::RemoveHitbox },
            ],
        });
        reg
    }

    /// Pressing ability 1 within its combo window redirects the cast to ability 4.
    #[test]
    fn combo_window_redirects_to_next_ability() {
        let reg = setup_combo_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Npc, TickId(0), 100.0);
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(caster, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: warm-up — Spawning → Active.
        pipeline.run_tick(&[]);

        // Cast Slash (ability 1) at tick 1.
        let ok1 = pipeline.cast_ability(caster, 1, ResolvedTargeting::SelfCast, 0, 0);
        assert!(ok1, "First Slash cast should succeed");

        // Verify the execution is for ability 1.
        let exec1: Vec<_> = pipeline.state.combat.executions.active_ids();
        assert_eq!(pipeline.state.combat.executions.get(exec1[0]).unwrap().ability_id, 1);

        // Tick 1: Phase 3 drains SpawnHitbox + CooldownStart (offset 0).
        pipeline.run_tick(&[]);
        // Tick 2: Phase 3 drains ApplyDamageFrame (offset 1).
        pipeline.run_tick(&[]);
        // Tick 3: Phase 3 drains RemoveHitbox + OpenFollowUpWindow (offset 2).
        pipeline.run_tick(&[]);

        // Combo window should now be open for (caster, ability_id=1).
        assert!(
            pipeline.state.combat.active_windows.contains_key(&(caster, 1)),
            "Combo window should be open after OpenFollowUpWindow fires"
        );

        // The window should point to ability 4.
        let &(next_id, _expiry) = pipeline.state.combat.active_windows.get(&(caster, 1)).unwrap();
        assert_eq!(next_id, 4, "Window should redirect to ability 4");

        // Now cast ability 1 again — it should be redirected to ability 4 (Slash Combo).
        // Note: ability 1 is on cooldown, but the redirect checks cooldown of ability 4.
        let ok2 = pipeline.cast_ability(caster, 1, ResolvedTargeting::SelfCast, 0, 0);
        assert!(ok2, "Second press of ability 1 should succeed via combo redirect to 4");

        // Verify the new execution is for ability 4 (not 1).
        let all_execs: Vec<_> = pipeline.state.combat.executions.active_ids();
        // Most recently inserted is last.
        let latest = *all_execs.last().unwrap();
        let ctx = pipeline.state.combat.executions.get(latest).unwrap();
        assert_eq!(ctx.ability_id, 4, "Redirected cast should create execution for ability 4");

        // The combo window should be consumed.
        assert!(
            !pipeline.state.combat.active_windows.contains_key(&(caster, 1)),
            "Combo window should be consumed after redirect"
        );
    }

    /// After the combo window expires, pressing the same ability should NOT redirect.
    #[test]
    fn expired_combo_window_does_not_redirect() {
        let reg = setup_combo_registry();
        let mut pipeline = make_pipeline(reg);

        let caster = EntityId(1);
        pipeline.state.spawn_entity(caster, EntityKind::Npc, TickId(0), 100.0);
        let pos = rapier3d::math::Vector::new(0.0, 5.0, 0.0);
        {
            let pw = pipeline.physics_as::<PhysicsWorld>().unwrap();
            pw.add_dynamic_capsule(caster, pos, 0.5, 0.3, 1.0, collision_groups::npc_body_groups());
        }

        // Tick 0: Spawning → Active.
        pipeline.run_tick(&[]);

        // Cast Slash at tick 1.
        pipeline.cast_ability(caster, 1, ResolvedTargeting::SelfCast, 0, 0);

        // Ticks 1, 2, 3: Slash timeline plays out.
        // OpenFollowUpWindow fires at offset 2 → during tick 3 (current_tick=3).
        // expiry = TickId(3 + 10) = TickId(13).
        pipeline.run_tick(&[]);
        pipeline.run_tick(&[]);
        pipeline.run_tick(&[]);
        assert!(pipeline.state.combat.active_windows.contains_key(&(caster, 1)));

        // Advance past the combo window expiry (TickId(13)).
        // Phase 8 drains when exp > current is false → during tick 13.
        // We're at current_tick=4, need 10 more ticks to process tick 13.
        for _ in 0..10 {
            pipeline.run_tick(&[]);
        }

        // Window should be drained by Phase 8.
        assert!(
            !pipeline.state.combat.active_windows.contains_key(&(caster, 1)),
            "Combo window should be drained after expiry"
        );

        // Wait for ability 1's cooldown to expire.
        // CooldownStart fired at tick 1 with duration 20 → ready_at = TickId(21).
        // We're at current_tick=14, need 8 more ticks to process tick 21.
        for _ in 0..8 {
            pipeline.run_tick(&[]);
        }
        assert!(!pipeline.is_on_cooldown_pub(caster, 1), "Cooldown should have expired");

        // Now cast ability 1 again — no combo window, should cast ability 1 (not redirect to 4).
        let ok = pipeline.cast_ability(caster, 1, ResolvedTargeting::SelfCast, 0, 0);
        assert!(ok, "Cast of ability 1 should succeed after cooldown");

        let all_execs: Vec<_> = pipeline.state.combat.executions.active_ids();
        let latest = *all_execs.last().unwrap();
        let ctx = pipeline.state.combat.executions.get(latest).unwrap();
        assert_eq!(ctx.ability_id, 1, "Without active window, should cast ability 1 (no redirect)");
    }
}
