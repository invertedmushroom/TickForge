pub(super) use crate::lag_compensation::{self, TransformHistory};
pub(super) use game_core::combat::skill::{
    AbilityAction, AbilityExecutionContext, AbilityExecutionId, AbilityParams, AbilityRegistry,
    AbilityTimeline, CastFacingPolicy, ChargingState, ContactSpawnHitboxPayload, HitEffectAction,
    HitEffectSpec, HitboxRules, ResolvedTargeting, ScheduledAction, ScheduledActionType,
    SkillShape, TargetFilter, TargetingMode,
};
pub(super) use game_core::director::{DirectorSpawn, DirectorState};
pub(super) use game_core::entity::entity_index::EntityIndex;
pub(super) use game_core::physics_backend::{ColliderKind, PhysicsBackend, SensorShape};
pub(super) use game_core::sim_state::SimState;
pub(super) use game_protocol::entity_id::EntityId;
pub(super) use game_protocol::event::{EventPayload, SimEvent};
pub(super) use game_protocol::intent::{IntentAction, MoveDir, PlayerIntent};
pub(super) use game_protocol::tick::TickId;
pub(super) use game_protocol::types::Transform;
pub(super) use game_protocol::types::{Quatf, Vec3f};
pub(super) use game_schema::EntityKind;
#[allow(unused_imports)]
pub(super) use log::warn;
pub(super) use std::collections::{HashMap, HashSet};

mod ai;
mod collectors;
mod combat;
mod controller;
mod finalization;
mod skill_dispatch;
#[cfg(test)]
mod tests;

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

    /// Compute the grid cell for a world position on a specific visibility layer.
    pub fn from_position_on_layer(pos: &Vec3f, layer: u32) -> Self {
        Self {
            region_x: (pos.x / CELL_SIZE).floor() as i32,
            region_z: (pos.z / CELL_SIZE).floor() as i32,
            layer,
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

// Re-export the macro for sub-modules.
pub(super) use audit;

/// Emit a simulation-level warning: logs via `log::warn!` AND buffers
/// the message into `TickSummary::sim_warnings` so it is shipped to the
/// DB as a `SimLog` event row by the coordinator. Use this instead of a
/// bare `warn!` for any condition inside the tick pipeline that operators
/// should be able to observe live with:
///   spacetime subscribe <module> "SELECT * FROM sim_log"
macro_rules! sim_warn {
    ($pipeline:expr, $($arg:tt)*) => {{
        let _msg = format!($($arg)*);
        log::warn!("{}", _msg);
        $pipeline.summary.sim_warnings.push(_msg);
    }};
}
pub(super) use sim_warn;

/// Per-tick statistics for observability. One summary emitted per tick in the coordinator.
/// Counters are incremented inline during pipeline phases — no post-hoc scanning.
#[derive(Clone, Debug, Default)]
pub struct TickSummary {
    pub intents_processed: usize,
    pub contacts: usize,
    pub damage_events: usize,
    pub deaths: usize,
    pub despawns: usize,
    pub active_entities: usize,
    pub active_hitboxes: usize,
    pub transform_updates: usize,
    pub region_updates: usize,
    pub scheduled_actions_len: usize,
    pub tick_duration_us: u64,
    pub commit_retries: u32,
    /// Diagnostic messages emitted by the pipeline this tick (e.g. missing
    /// registry entries, physics fallbacks). Shipped to DB as SimLog event rows.
    pub sim_warnings: Vec<String>,
}

/// Authoritative death-state row emitted by the worker for player deaths.
///
/// Produced in Phase 8b at the moment the worker decides an entity transitions
/// to `DespawnPending` due to combat death (HP-based or DoT). The reducer just
/// inserts the row — it must not derive death state from observed lifecycle
/// transitions, since that loses killer attribution and silently fabricates
/// fallbacks for missing position/layer data.
#[derive(Clone, Debug)]
pub struct DeathStateInsertEntry {
    pub entity_id: EntityId,
    pub killer_entity: Option<EntityId>,
    pub layer: u32,
    pub death_pos_x: f32,
    pub death_pos_y: f32,
    pub death_pos_z: f32,
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
    /// NPC AI state snapshot for all active NPCs/bosses.
    /// Each entry is (entity_id, NpcAiState, top_threat target).
    pub npc_state_updates: Vec<(EntityId, game_schema::NpcAiState, Option<EntityId>)>,
    /// Region assignment changes for entities that crossed a grid cell boundary
    /// this tick. Only entities whose cell changed (with hysteresis) are included.
    pub region_updates: Vec<(EntityId, RegionCell)>,
    /// Entities spawned by the world director this tick.
    /// Coordinator marshals these into DB insert calls so the entities are persisted.
    pub director_spawns: Vec<DirectorSpawn>,
    /// Interactable state changes this tick (entity_id, new SimInteractState).
    pub interactable_updates: Vec<(EntityId, game_core::sim_state::SimInteractState)>,
    /// Boss phase transitions from the encounter executor (boss_entity_id, phase, tick).
    pub boss_phase_updates: Vec<(u64, u32, u64)>,
    /// Zone counter increments from the encounter executor and combat system.
    pub zone_counter_deltas: Vec<(u32, i32, i32, String, f64)>,
    /// Player death rows produced this tick by Phase 8b. The reducer inserts
    /// these into `death_state` verbatim (it does not derive them).
    pub death_state_inserts: Vec<DeathStateInsertEntry>,
    /// Diagnostic warnings from the pipeline this tick. Each message is shipped
    /// to the DB as a `SimLog` event row (level=Warn) by the coordinator.
    pub sim_warnings: Vec<String>,
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

/// In-flight lock-on selection session for TERA-style multi-target lock-on skills.
///
/// Created when a player activates a `TargetingMode::LockOn` ability for the first
/// time. Consumed when they re-activate the same ability (fire) or cancel with
/// `ReleaseAbility`. Auto-cancelled after `timeout_at` ticks (no cooldown).
pub(super) struct LockOnSession {
    pub ability_id: u32,
    pub tagged: Vec<EntityId>,
    pub max_targets: u32,
    /// Tick on which the session auto-expires if not fired or cancelled.
    pub timeout_at: TickId,
}

/// Default lock-on session timeout in ticks (20 s at 20 Hz).
const LOCK_ON_SESSION_TIMEOUT_TICKS: u32 = 400;

pub struct TickPipeline {
    pub(super) current_tick: TickId,
    pub(super) event_sequence: u32,
    pub(super) pending_events: Vec<SimEvent>,
    pub(super) physics: Box<dyn PhysicsBackend>,
    pub(super) dt: f32,
    /// Per-entity scheduled actions, sorted by tick_id ascending.
    pub(super) scheduled_actions: Vec<ScheduledAction>,
    /// Static ability definitions — looked up during combat resolution.
    pub(super) abilities: AbilityRegistry,
    /// Buff templates — looked up by ID when applying buffs from abilities.
    pub(super) buff_registry: game_core::combat::status::BuffRegistry,
    /// Runtime entity state — lifecycle, health, buffs, threat, AI, contacts.
    pub(crate) state: SimState,
    /// Explicit cooldown state — maps (EntityId, ability_id) → tick when the cooldown expires.
    ///
    /// The HashMap owns cooldown truth: O(1) lookup in Phase 2, drained in Phase 8 to emit
    /// CooldownReady events. Retroactive cooldown reduction is a direct mutation of the ready_at
    /// value. Entity despawn cleaning removes all entries in a single `retain` call.
    pub(super) cooldowns: HashMap<(EntityId, u32), TickId>,
    /// Monotonically increasing counter for `ScheduledAction::id`.
    /// Assigned at scheduling time; never reused within a session.
    pub(super) next_scheduled_id: u64,
    /// Running stats for the current tick — incremented inline, returned in TickResult.
    pub(super) summary: TickSummary,
    /// Last committed region cell per entity. Used to detect cell crossings with
    /// hysteresis so only actual transitions produce `RegionUpdate` entries.
    /// Seeded at spawn (from initial position → cell), updated each tick when
    /// a transition is emitted. Entries are removed in `force_remove_entity`.
    pub(super) entity_regions: HashMap<EntityId, RegionCell>,
    /// Dense layer cache indexed by entity slot (EntityIndex.as_usize()).
    /// Mirrors `entity_regions[id].layer` for O(1) access in hot paths.
    /// Updated in `spawn_entity_from_snapshot` and `set_entity_layer`.
    pub(super) entity_layer_cache: Vec<u32>,
    /// Dense team cache indexed by entity slot (EntityIndex.as_usize()).
    /// Mirrors the `entity_team` DB table for O(1) access in combat checks.
    /// Updated in `set_entity_team`; 0 = unassigned/no team.
    pub(super) entity_team_cache: Vec<u32>,
    /// Last transform snapshot sent to the commit reducer per entity.
    ///
    /// Used to emit transform deltas instead of a full-world snapshot every tick.
    /// This keeps commit cost proportional to movement activity rather than total
    /// entity count. Entries are removed when an entity is force-removed.
    pub(super) last_committed_transforms: HashMap<EntityId, Transform>,
    /// Ring buffer of recent transform snapshots for lag-compensation rewind.
    /// Populated at the end of each tick (Phase 10) with entity positions.
    /// Phase 6 reads historical positions for compensated hit detection.
    pub(super) transform_history: TransformHistory,
    /// World director — evaluates dynamic event triggers and spawns NPCs.
    pub(super) director: DirectorState,
    /// World phase projections from the DB — keyed by zone_id → phase_name.
    /// Updated by coordinator callbacks when world_phase rows change.
    pub(super) world_phases: HashMap<u32, String>,
    /// NPC goal directives from the DB (Tier 2 world_clock output).
    /// Keyed by entity_id → (goal_kind, priority). Phase 7 AI reads before decisions.
    pub(super) npc_goals: HashMap<EntityId, (String, u32)>,
    /// Active boss encounters — keyed by boss entity ID.
    /// Phase 7.5 evaluates encounter rules after director spawns.
    pub(crate) encounters: HashMap<EntityId, game_core::encounter::EncounterState>,
    /// Entities whose cached `StatBlock` needs recalculation.
    /// Populated by: (a) equipment changes (via `mark_stats_dirty`),
    /// (b) buff changes (copied from `StatusState::dirty_entities` at Phase 10).
    /// Drained by Phase 1.5 stat recalculation.
    /// Keyed by EntityId (not dense index) so slot reuse cannot cause stale refs.
    pub(super) stats_dirty: HashSet<EntityId>,
    /// Aggregated equipment modifiers per entity. Updated by the coordinator
    /// when `player_equipment` rows change; read by `phase_stat_recalc`.
    pub(super) equipment_modifiers: HashMap<EntityId, game_core::stats::EquipmentModifiers>,
    /// Last emitted (NpcAiState, target) per NPC. `collect_npc_state_updates`
    /// only emits when the current value differs, eliminating redundant upserts
    /// for idle NPCs whose state never changes.
    pub(super) npc_state_prev: HashMap<EntityId, (game_schema::NpcAiState, Option<EntityId>)>,
    /// Per-entity weapon-swap cooldown: maps EntityId → tick when next swap is allowed.
    /// Prevents rapid toggling. Cleaned up in `force_remove_entity`.
    pub(super) weapon_swap_cooldowns: HashMap<EntityId, TickId>,
    /// Region type overrides. If a RegionCell has an entry here, its
    /// `RegionType` determines repulsion rules. Cells without an entry
    /// default to `RegionType::OpenWorld`.
    pub(super) region_types: HashMap<RegionCell, game_core::region::RegionType>,
    /// Active TERA-style lock-on sessions, keyed by caster EntityId.
    /// Created by `UseAbility` on a `LockOn`-mode skill; consumed on re-activation (fire)
    /// or `ReleaseAbility` (cancel). Auto-expired in Phase 8 if timeout_at is reached.
    pub(super) active_lock_on_sessions: HashMap<EntityId, LockOnSession>,
    /// Blocking entities for the current tick: (slot_index, EntityId) pairs where
    /// blocking || block_grace is true. Built once at the start of Phase 6 so that
    /// the cover check iterates only active blockers instead of all tactical slots.
    pub(super) cover_blockers: Vec<(usize, EntityId)>,
    /// Global maximum lag compensation rewind depth (ticks).
    /// Sourced from `TickConfig::global_max_rewind_ticks` at startup.
    pub(super) global_max_rewind_ticks: u32,
    /// Interactable state changes accumulated this tick.
    /// Each entry is (entity_id, new_state). Drained in Phase 10 commit.
    pub(super) pending_interactable_updates:
        Vec<(EntityId, game_core::sim_state::SimInteractState)>,
    /// Deferred heals queued during Phase 7 (AI decisions) and drained in Phase 8b.
    /// Keeps Health mutations centralised in combat/finalization phases.
    /// Each entry is (entity_id, heal_amount, heal_source).
    pub(super) pending_heals: Vec<(EntityId, f32, EntityId)>,
    /// Zone counter deltas emitted by Phase 8b on entity death, indexed by
    /// the dying entity's region cell and kind:
    ///   - `Npc`  → ("kills", 1.0)
    ///   - `Boss` → ("kills", 1.0) and ("boss_killed", 1.0)
    ///   - `Player` / other → no delta
    /// Drained into `TickResult.zone_counter_deltas` alongside encounter-
    /// emitted deltas before the commit stage. Matches the hardcoded
    /// `world_clock` threshold rules in `server_module::reducers`.
    pub(super) pending_zone_counter_deltas: Vec<(u32, i32, i32, String, f64)>,
    /// Authoritative `death_state` rows produced by Phase 8b for player deaths.
    /// Drained into `TickResult.death_state_inserts` at commit assembly time.
    pub(super) pending_death_state_inserts: Vec<DeathStateInsertEntry>,
}

impl TickPipeline {
    /// Hard teardown for an entity from *all* runtime stores and the physics backend.
    /// Returns `true` if an entity was actually removed from `SimState`, `false` if it
    /// was not present.
    pub fn force_remove_entity(&mut self, id: EntityId) -> bool {
        self.force_remove_entities(&[id]) > 0
    }

    /// Batch hard teardown for multiple entities. Performs each cleanup pass once
    /// against a `HashSet` of removed IDs instead of per-entity, converting
    /// O(N × world) into O(world) for threat tables, fear references, cooldowns, etc.
    ///
    /// Returns the number of entities actually removed from `SimState`.
    pub fn force_remove_entities(&mut self, ids: &[EntityId]) -> usize {
        if ids.is_empty() {
            return 0;
        }

        let removed_set: HashSet<EntityId> = ids.iter().copied().collect();

        // 1) Determine all execution IDs owned by ANY removed caster so we can
        //    fully remove dependent runtime objects (scheduled actions, sensors, hitboxes).
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
                    .map(|ctx| removed_set.contains(&ctx.caster))
                    .unwrap_or(false)
            })
            .collect();

        // 2) Remove any future actions that target a removed entity OR are sourced
        //    from an execution owned by a removed entity.
        self.scheduled_actions.retain(|a| {
            if removed_set.contains(&a.entity) {
                return false;
            }
            match a.source {
                Some(src) => !execs_to_remove.contains(&src),
                None => true,
            }
        });

        // 3) Single-pass cooldown cleanup.
        self.cooldowns
            .retain(|&(eid, _), _| !removed_set.contains(&eid));

        // 4) Single-pass follow-up windows + charging cleanup.
        self.state
            .combat
            .active_windows
            .retain(|&(eid, _), _| !removed_set.contains(&eid));
        for id in &removed_set {
            self.state.combat.charging.remove(id);
        }

        // 4b) Single-pass threat table scrub — one iteration of all NPC threat
        //     tables, retaining only entries whose source is not in the removed set.
        for (_, table) in self.state.combat.threat_tables.iter_mut() {
            table.entries.retain(|e| !removed_set.contains(&e.source));
        }

        // 4c) Single-pass fear cleanup — one scan of all tactical slots.
        // Collect affected slot indices first, then remove debuffs in a second pass
        // (tactical and status live in separate fields, so we can't do both in one loop).
        let mut fear_cleared_slots: Vec<usize> = Vec::new();
        for (i, t) in self.state.combat.tactical.iter_mut().enumerate() {
            if let Some(src) = t.fear_source {
                if removed_set.contains(&src) {
                    t.fear_source = None;
                    t.movement_conditions
                        .remove(game_core::combat::tactical::MovementConditions::FEARED);
                    fear_cleared_slots.push(i);
                }
            }
        }
        for slot in fear_cleared_slots {
            let idx = self.state.entities.index_at(slot);
            self.state
                .status
                .remove_cc_debuff(idx, game_schema::CCEffect::Fear);
        }

        // 5) Remove hitbox sensors and execution contexts owned by removed entities.
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

        // 6) Drop region tracking and dirty-tracking for each removed entity.
        //    Also zero out dense cache slots so stale layer/team values are never
        //    inherited when the slot is recycled by a future spawn.
        for id in &removed_set {
            if let Some(idx) = self.state.entities.lookup(*id) {
                let slot = idx.as_usize();
                if slot < self.entity_layer_cache.len() {
                    self.entity_layer_cache[slot] = 0;
                }
                if slot < self.entity_team_cache.len() {
                    self.entity_team_cache[slot] = 0;
                }
            }
            self.entity_regions.remove(id);
            self.last_committed_transforms.remove(id);
            self.npc_state_prev.remove(id);
            self.equipment_modifiers.remove(id);
            self.weapon_swap_cooldowns.remove(id);
        }

        // 6b) Clean up lock-on sessions. Self-removal and target scrubbing.
        let mut cancellations: Vec<(EntityId, EntityId)> = Vec::new();
        self.active_lock_on_sessions.retain(|&caster, session| {
            if removed_set.contains(&caster) {
                // If the caster itself is removed, cancel the entire session.
                for target in &session.tagged {
                    cancellations.push((caster, *target));
                }
                false
            } else {
                // If the caster survives, scrub only the targets that were removed.
                session.tagged.retain(|t| {
                    if removed_set.contains(t) {
                        cancellations.push((caster, *t));
                        false
                    } else {
                        true
                    }
                });
                true
            }
        });
        for (caster, target) in cancellations {
            self.emit_event(
                caster,
                EventPayload::LockOnCanceled {
                    source: caster,
                    target,
                },
            );
        }

        // 7) Remove entities from SimState and physics world.
        //    Character bodies (Player/NPC/Boss) are pooled via disable_entity
        //    for cheap reuse on respawn. Other kinds are fully removed.
        let mut count = 0usize;
        for &id in &removed_set {
            let kind = self
                .state
                .entities
                .lookup(id)
                .map(|idx| self.state.entities.kinds[idx.as_usize()]);
            if self.state.remove_entity(id) {
                count += 1;
            }
            match kind {
                Some(EntityKind::Player | EntityKind::Npc | EntityKind::Boss) => {
                    self.physics.disable_entity(id, kind.unwrap());
                }
                _ => {
                    self.physics.remove_entity(id);
                }
            }
        }
        count
    }

    pub fn new(
        start_tick: TickId,
        physics: Box<dyn PhysicsBackend>,
        dt: f32,
        abilities: AbilityRegistry,
        buff_registry: game_core::combat::status::BuffRegistry,
        global_max_rewind_ticks: u32,
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
            summary: TickSummary::default(),
            entity_regions: HashMap::new(),
            entity_layer_cache: Vec::new(),
            entity_team_cache: Vec::new(),
            last_committed_transforms: HashMap::new(),
            transform_history: TransformHistory::new(),
            director: DirectorState::new(),
            world_phases: HashMap::new(),
            npc_goals: HashMap::new(),
            encounters: HashMap::new(),
            stats_dirty: HashSet::new(),
            equipment_modifiers: HashMap::new(),
            npc_state_prev: HashMap::new(),
            weapon_swap_cooldowns: HashMap::new(),
            region_types: HashMap::new(),
            active_lock_on_sessions: HashMap::new(),
            cover_blockers: Vec::new(),
            global_max_rewind_ticks,
            pending_interactable_updates: Vec::new(),
            pending_heals: Vec::new(),
            pending_zone_counter_deltas: Vec::new(),
            pending_death_state_inserts: Vec::new(),
        }
    }

    pub fn physics_mut(&mut self) -> &mut dyn PhysicsBackend {
        &mut *self.physics
    }

    // ── Test support accessors ──────────────────────────────────
    //
    // Expose internals that integration tests need to set up encounter
    // scenarios. Not part of the public API contract.

    /// Direct access to `SimState` for test setup (e.g. manipulating HP).
    #[doc(hidden)]
    pub fn state_mut(&mut self) -> &mut SimState {
        &mut self.state
    }

    /// Direct access to the encounter map for test registration.
    #[doc(hidden)]
    pub fn encounters_mut(
        &mut self,
    ) -> &mut HashMap<EntityId, game_core::encounter::EncounterState> {
        &mut self.encounters
    }

    /// Read-only access to encounters for assertions.
    #[doc(hidden)]
    pub fn encounters(&self) -> &HashMap<EntityId, game_core::encounter::EncounterState> {
        &self.encounters
    }

    /// Direct access to the world_phase projection map for test setup
    /// (normally written by the coordinator on world_phase DB inserts).
    #[doc(hidden)]
    pub fn world_phases_mut(&mut self) -> &mut HashMap<u32, String> {
        &mut self.world_phases
    }

    pub fn set_current_tick(&mut self, tick: TickId) {
        self.current_tick = tick;
    }

    pub fn set_global_max_rewind_ticks(&mut self, ticks: u32) {
        self.global_max_rewind_ticks = ticks;
    }

    pub fn current_tick(&self) -> TickId {
        self.current_tick
    }

    /// Set the region type for a spatial cell. Determines which repulsion
    /// rules apply to entities within that cell.
    pub fn set_region_type(
        &mut self,
        cell: RegionCell,
        region_type: game_core::region::RegionType,
    ) {
        self.region_types.insert(cell, region_type);
    }

    /// Look up the repulsion rules for an entity based on its current region.
    pub(super) fn repulsion_rules_for(
        &self,
        entity: EntityId,
    ) -> game_core::region::RepulsionRules {
        let region_type = self
            .entity_regions
            .get(&entity)
            .and_then(|cell| self.region_types.get(cell))
            .copied()
            .unwrap_or_default();
        game_core::region::RepulsionRules::for_region(region_type)
    }

    /// Returns the visibility layer for an entity, defaulting to 0 (open world).
    /// Uses the dense layer cache for O(1) access when an EntityIndex is available.
    pub(super) fn layer_of(&self, entity: EntityId) -> u32 {
        if let Some(idx) = self.state.entities.lookup(entity) {
            let slot = idx.as_usize();
            if slot < self.entity_layer_cache.len() {
                return self.entity_layer_cache[slot];
            }
        }
        // Fallback to HashMap for entities not yet in the cache.
        self.entity_regions.get(&entity).map_or(0, |c| c.layer)
    }

    /// O(1) layer lookup by dense index — preferred in tight loops where the
    /// EntityIndex is already known.
    #[inline(always)]
    pub(super) fn layer_of_idx(&self, idx: EntityIndex) -> u32 {
        let slot = idx.as_usize();
        if slot < self.entity_layer_cache.len() {
            self.entity_layer_cache[slot]
        } else {
            0
        }
    }

    /// Check whether two entities share the same visibility layer.
    pub(super) fn same_layer(&self, a: EntityId, b: EntityId) -> bool {
        self.layer_of(a) == self.layer_of(b)
    }

    /// O(1) team lookup by dense index — used in combat target filtering.
    #[inline(always)]
    pub(super) fn team_of_idx(&self, idx: EntityIndex) -> u32 {
        let slot = idx.as_usize();
        if slot < self.entity_team_cache.len() {
            self.entity_team_cache[slot]
        } else {
            0
        }
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
        layer: u32,
    ) {
        // Idempotency guard — the coordinator's on_insert callback already checks
        // contains() before calling here, but this defence-in-depth prevents a double-
        // spawn if the call sequence ever regresses (e.g. a second on_insert for the
        // same entity after a subscription re-apply).
        if self.state.entities.contains(id) {
            return;
        }
        self.state.spawn_entity(id, kind, tick, max_hp);

        // Grow or set the dense layer cache.
        if let Some(idx) = self.state.entities.lookup(id) {
            let slot = idx.as_usize();
            if slot >= self.entity_layer_cache.len() {
                self.entity_layer_cache.resize(slot + 1, 0);
            }
            self.entity_layer_cache[slot] = layer;
            // Grow team cache in parallel and reset to 0 (unassigned) so slot
            // reuse never inherits the previous occupant's team.
            if slot >= self.entity_team_cache.len() {
                self.entity_team_cache.resize(slot + 1, 0);
            }
            self.entity_team_cache[slot] = 0;
        }

        match kind {
            EntityKind::Player | EntityKind::Npc | EntityKind::Boss => {
                self.physics.reuse_or_spawn_character(id, position, kind);
            }
            EntityKind::Prop => {
                // Props get a dynamic box body; default half-extents 0.5m cube.
                self.physics.spawn_prop_body(
                    id,
                    position,
                    game_protocol::types::Vec3f {
                        x: 0.5,
                        y: 0.5,
                        z: 0.5,
                    },
                    true,
                );
            }
            // Projectile and Hazard bodies are spawned by the ability/encounter system;
            // the DB row alone is not enough to reconstruct the full physics state.
            EntityKind::Projectile | EntityKind::Hazard => {}
        }
        // Record spawn position as patrol home for NPC/Boss entities.
        if (kind == EntityKind::Npc || kind == EntityKind::Boss)
            && let Some(idx) = self.state.entities.lookup(id)
        {
            self.state.ai.home_positions.insert(idx, position);
        }

        // Seed the entity's region so collect_region_updates uses the correct
        // visibility layer from the first tick onward.
        self.entity_regions
            .insert(id, RegionCell::from_position_on_layer(&position, layer));
        // Mirror the layer into the physics runtime so scene-query predicates
        // can filter cross-layer interactions.
        self.physics.set_entity_layer(id, layer);
    }

    /// Seed runtime state from DB rows recovered on worker restart.
    ///
    /// Must be called after `spawn_entity_from_snapshot` so the entity already
    /// has an `EntityIndex` mapping.  Silently ignores rows whose entity is not
    /// present (e.g. entities that became Removed between the commit and the restart).
    pub fn seed_runtime_state(
        &mut self,
        buffs: &[(EntityId, Vec<game_core::combat::status::ActiveBuff>)],
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
        for (eid, ai_state, target) in npc_states {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                if let Some(ai) = self.state.ai.npc_ai.get_mut(idx) {
                    *ai = *ai_state;
                }
                // Reconstruct a minimal in-memory threat table from npc_state.target_entity
                // so restart recovery preserves the current aggro holder.
                if let Some(target_id) = target {
                    if let Some(table) = self.state.combat.threat_tables.get_mut(idx) {
                        if table.entries.is_empty() {
                            table.add_threat(*target_id, 1.0);
                        }
                    }
                }
            }
        }
    }

    /// Downcast the physics backend to a concrete type.
    /// Returns `None` if the backend is not of type `T`.
    pub fn physics_as<T: 'static>(&mut self) -> Option<&mut T> {
        self.physics.as_any_mut().downcast_mut::<T>()
    }

    /// Schedule all actions from an ability timeline starting at `start_tick`.
    ///
    /// Each action receives a unique monotonic ID so scheduled actions can be tracked
    /// and correlated in logs. The scheduled action queue is kept sorted by `tick_id`.
    ///
    /// # Invariants
    /// - All scheduled ticks must be ≥ `current_tick` (debug-asserted).
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
            let pos = self
                .scheduled_actions
                .partition_point(|a| a.tick_id <= tick_id);
            self.scheduled_actions.insert(
                pos,
                ScheduledAction {
                    id: {
                        let id = self.next_scheduled_id;
                        self.next_scheduled_id += 1;
                        id
                    },
                    tick_id,
                    entity,
                    source: Some(execution_id),
                    action_type: ScheduledActionType::AbilityFrame {
                        execution_id,
                        ability_id: timeline.ability_id,
                        action: scheduled.action.clone(),
                    },
                },
            );
        }
        debug_assert!(
            self.scheduled_actions
                .windows(2)
                .all(|w| w[0].tick_id <= w[1].tick_id),
            "scheduled_actions sort invariant violated after schedule_ability",
        );
    }

    fn forward_from_rotation(rotation: Quatf) -> Vec3f {
        // Full 3-D quaternion forward vector: q * (0,0,1) * q^-1
        //   f_x = 2(qx·qz + qw·qy)
        //   f_y = 2(qy·qz − qw·qx)
        //   f_z = 1 − 2(qx² + qy²)
        let qx = rotation.x;
        let qy = rotation.y;
        let qz = rotation.z;
        let qw = rotation.w;
        Vec3f {
            x: 2.0 * (qx * qz + qw * qy),
            y: 2.0 * (qy * qz - qw * qx),
            z: 1.0 - 2.0 * (qx * qx + qy * qy),
        }
    }

    fn normalize_direction(dir: Vec3f) -> Option<Vec3f> {
        let len_sq = dir.x * dir.x + dir.y * dir.y + dir.z * dir.z;
        if !len_sq.is_finite() || len_sq < 1e-6 {
            return None;
        }
        let inv_len = 1.0 / len_sq.sqrt();
        Some(Vec3f {
            x: dir.x * inv_len,
            y: dir.y * inv_len,
            z: dir.z * inv_len,
        })
    }

    /// Project a vector onto the XZ plane and normalize.
    ///
    /// Used **only** for character *body yaw* (torso orientation). Abilities
    /// and projectiles must use [`Self::normalize_direction`] so pitch is
    /// preserved end-to-end (see execution-facing pipeline in
    /// `resolve_execution_facing`). Returning a vector with `y = 0.0` is
    /// intentional, not a 3D → 2D dropout.
    fn normalize_horizontal_direction(dir: Vec3f) -> Option<Vec3f> {
        let len_sq = dir.x * dir.x + dir.z * dir.z;
        if !len_sq.is_finite() || len_sq < 1e-6 {
            return None;
        }
        let inv_len = 1.0 / len_sq.sqrt();
        let out = Vec3f {
            x: dir.x * inv_len,
            y: 0.0,
            z: dir.z * inv_len,
        };
        debug_assert!(out.y.abs() < 1e-4, "body facing must be yaw-only");
        Some(out)
    }

    fn horizontal_direction_to(origin: Vec3f, point: Vec3f) -> Option<Vec3f> {
        Self::normalize_horizontal_direction(Vec3f {
            x: point.x - origin.x,
            y: 0.0,
            z: point.z - origin.z,
        })
    }

    fn direction_to(origin: Vec3f, point: Vec3f) -> Option<Vec3f> {
        Self::normalize_direction(Vec3f {
            x: point.x - origin.x,
            y: point.y - origin.y,
            z: point.z - origin.z,
        })
    }

    fn set_entity_body_facing(
        &mut self,
        entity_id: EntityId,
        dir: Vec3f,
        _audit_detail: &'static str,
    ) -> Option<Vec3f> {
        let facing = Self::normalize_horizontal_direction(dir)?;
        let yaw = facing.x.atan2(facing.z);
        let half_yaw = yaw * 0.5;
        let rotation = Quatf {
            x: 0.0,
            y: half_yaw.sin(),
            z: 0.0,
            w: half_yaw.cos(),
        };
        self.physics.set_kinematic_rotation(entity_id, rotation);
        audit!(
            self.state,
            Transform,
            Controller,
            2,
            Some(entity_id),
            _audit_detail
        );
        Some(facing)
    }

    fn resolve_target_facing(&self, origin: Vec3f, targeting: &ResolvedTargeting) -> Option<Vec3f> {
        match targeting {
            ResolvedTargeting::Entity { target } => self
                .physics
                .get_transform(*target)
                .and_then(|t| Self::horizontal_direction_to(origin, t.position)),
            ResolvedTargeting::Position { point } => Self::horizontal_direction_to(origin, *point),
            ResolvedTargeting::MultiLockOn { targets } => targets
                .first()
                .copied()
                .and_then(|target| self.physics.get_transform(target))
                .and_then(|t| Self::horizontal_direction_to(origin, t.position)),
            _ => None,
        }
    }

    fn resolve_target_direction(
        &self,
        origin: Vec3f,
        targeting: &ResolvedTargeting,
    ) -> Option<Vec3f> {
        match targeting {
            ResolvedTargeting::Entity { target } => self
                .physics
                .get_transform(*target)
                .and_then(|t| Self::direction_to(origin, t.position)),
            ResolvedTargeting::Position { point } => Self::direction_to(origin, *point),
            ResolvedTargeting::MultiLockOn { targets } => targets
                .first()
                .copied()
                .and_then(|target| self.physics.get_transform(target))
                .and_then(|t| Self::direction_to(origin, t.position)),
            _ => None,
        }
    }

    fn resolve_execution_facing(
        &self,
        origin: Vec3f,
        targeting: &ResolvedTargeting,
        body_facing: Vec3f,
    ) -> Vec3f {
        match targeting {
            ResolvedTargeting::Direction { dir } => {
                Self::normalize_direction(*dir).unwrap_or(body_facing)
            }
            ResolvedTargeting::Entity { .. }
            | ResolvedTargeting::Position { .. }
            | ResolvedTargeting::MultiLockOn { .. } => self
                .resolve_target_direction(origin, targeting)
                .unwrap_or(body_facing),
            ResolvedTargeting::SelfCast | ResolvedTargeting::CasterOffset => body_facing,
        }
    }

    fn resolve_cast_snapshot(
        &mut self,
        caster: EntityId,
        ability_id: u32,
        targeting: &ResolvedTargeting,
    ) -> Option<(Vec3f, Vec3f)> {
        let transform = self.physics.get_transform(caster)?;
        let origin = transform.position;
        let body_facing = Self::normalize_direction(Self::forward_from_rotation(
            transform.rotation,
        ))
        .unwrap_or(Vec3f {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        });
        let facing = self.resolve_execution_facing(origin, targeting, body_facing);
        let target_facing = self.resolve_target_facing(origin, targeting);
        let facing_policy = self.ability_prop(
            ability_id,
            |ad| ad.cast_facing_policy,
            CastFacingPolicy::PreserveBody,
        );

        match facing_policy {
            CastFacingPolicy::PreserveBody => {}
            CastFacingPolicy::FaceAimDirection => {
                self.set_entity_body_facing(caster, facing, "cast_face");
            }
            CastFacingPolicy::FaceResolvedTarget => {
                if let Some(dir) = target_facing {
                    self.set_entity_body_facing(caster, dir, "cast_face");
                }
            }
        }

        Some((origin, facing))
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
        // The window is consumed only after all validation passes (cooldown,
        // timeline lookup, physics body) to avoid losing the combo opportunity
        // on a rejected cast.
        let combo_key = (caster, ability_id);
        let resolved_id =
            if let Some(&(next_id, exp)) = self.state.combat.active_windows.get(&combo_key) {
                if self.current_tick <= exp {
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
        let requirements = match self.abilities.get(resolved_id) {
            Some(ability) => ability.cast_requirements(),
            None => return false,
        };
        if requirements.require_grounded {
            let grounded = self
                .state
                .entities
                .lookup(caster)
                .map(|idx| {
                    let kind = self.state.entities.kinds[idx.as_usize()];
                    // NPC locomotion can transiently report airborne while standing on
                    // spawn snapshots; keep the player input gate strict and leave AI
                    // cast gating to AI state/range/cooldown checks.
                    kind != EntityKind::Player
                        || self.state.combat.tactical[idx.as_usize()].is_grounded
                })
                .unwrap_or(false);
            if !grounded {
                return false;
            }
        }
        let timeline = match self.abilities.get_timeline(resolved_id).cloned() {
            Some(t) => t,
            None => return false,
        };

        // Snapshot caster position and facing at cast time so later timeline
        // phases (ApplyDamageFrame, projectile spawn, etc.) use cast-time
        // geometry rather than the caster's current position.
        let Some((origin, facing)) = self.resolve_cast_snapshot(caster, resolved_id, &targeting)
        else {
            return false; // entity has no physics body — reject cast
        };

        // All validation passed — consume the combo window now.
        if resolved_id != ability_id {
            self.state.combat.active_windows.remove(&combo_key);
        }

        let params = AbilityParams {
            charge_level,
            variant: 0,
        };
        let execution_id = self.state.combat.executions.next_id();
        self.state
            .combat
            .executions
            .insert(AbilityExecutionContext {
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
        let cast_duration_ticks = timeline
            .actions
            .iter()
            .map(|a| a.tick_offset)
            .max()
            .unwrap_or(0)
            + 1;
        self.emit_event(
            caster,
            EventPayload::CastStart {
                ability_id: resolved_id,
                cast_duration_ticks,
            },
        );
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
    /// Derives the composite root state from all independent sources:
    /// - `blocking` (hold-to-block, Phase 2 / Phase 8)
    /// - `movement_conditions` (bitflags: ROOTED, INPUT_LOCK, STUNNED, KNOCKED_DOWN, FLOATING, SLEEPING, FEARED all block movement)
    /// - `arc_state` where `!gravity_only` (deliberate ArcMovement roots; gravity-only jump/fall arcs do not)
    /// - Any active buff with `root: Some(true)`
    pub(super) fn is_rooted(&self, idx: game_core::entity::entity_index::EntityIndex) -> bool {
        let t = &self.state.combat.tactical[idx.as_usize()];
        // Any movement-blocking condition flag immediately roots the entity.
        // CC bitflags (STUNNED, KNOCKED_DOWN, FLOATING, SLEEPING, FEARED) are the
        // single source of truth — set on CC application, cleared by buff expiry.
        if t.movement_conditions.intersects(
            game_core::combat::tactical::MovementConditions::ROOTED
                | game_core::combat::tactical::MovementConditions::INPUT_LOCK
                | game_core::combat::tactical::MovementConditions::STUNNED
                | game_core::combat::tactical::MovementConditions::KNOCKED_DOWN
                | game_core::combat::tactical::MovementConditions::FLOATING
                | game_core::combat::tactical::MovementConditions::SLEEPING
                | game_core::combat::tactical::MovementConditions::FEARED,
        ) {
            return true;
        }
        let rooted_by_timer = t
            .rooted_until_tick
            .is_some_and(|until| self.current_tick < until);
        // Deliberate arc-movement (vault, leap) roots the entity.
        // Gravity-only arcs (jump, fall-from-root) do NOT root — WASD keeps working mid-air.
        let arc_roots = t.arc_state.map_or(false, |a| !a.gravity_only);
        if t.blocking || t.block_grace || rooted_by_timer || arc_roots {
            return true;
        }
        self.state
            .status
            .get_buffs(idx)
            .iter()
            .any(|b| b.modifiers.root == Some(true))
    }

    /// Returns `true` when the entity is under a CC effect that prevents all actions
    /// (casting, blocking, jumping, interacting). Checked at the top of Phase 2
    /// intent dispatch to reject non-movement intents for CC'd entities.
    ///
    /// CC-disabled conditions (single source of truth — bitflags only):
    /// - STUNNED
    /// - KNOCKED_DOWN
    /// - FLOATING (airborne from launch CC)
    /// - SLEEPING
    /// - FEARED
    pub(super) fn is_cc_disabled(&self, idx: game_core::entity::entity_index::EntityIndex) -> bool {
        let t = &self.state.combat.tactical[idx.as_usize()];
        t.movement_conditions
            .intersects(game_core::combat::tactical::MovementConditions::CC_DISABLED)
    }

    /// Returns `true` when the entity is silenced (cannot cast abilities but can
    /// move, jump, and block). Separate from `is_cc_disabled` because SILENCED
    /// does not prevent movement intents.
    pub(super) fn is_silenced(&self, idx: game_core::entity::entity_index::EntityIndex) -> bool {
        let t = &self.state.combat.tactical[idx.as_usize()];
        t.movement_conditions
            .contains(game_core::combat::tactical::MovementConditions::SILENCED)
    }

    /// Clear a specific CC effect's timer and movement-condition bitflag.
    ///
    /// Called by `Cleanse` (when a removed debuff carried a `cc_effect`) and
    /// `ClearCC` (explicit CC removal).
    pub(super) fn clear_cc_by_effect(
        &mut self,
        _entity: EntityId,
        idx: game_core::entity::entity_index::EntityIndex,
        cc_effect: game_schema::CCEffect,
    ) {
        let t = &mut self.state.combat.tactical[idx.as_usize()];
        match cc_effect {
            game_schema::CCEffect::Stun => {
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::STUNNED);
            }
            game_schema::CCEffect::Knockdown => {
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::KNOCKED_DOWN);
            }
            game_schema::CCEffect::Sleep => {
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::SLEEPING);
            }
            game_schema::CCEffect::Silence => {
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::SILENCED);
            }
            game_schema::CCEffect::Fear => {
                t.fear_source = None;
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::FEARED);
            }
            game_schema::CCEffect::Knockback => {
                // Knockback CC is the arc displacement itself — clear it.
                // Also clear FLOATING since all displacement arcs set it.
                t.arc_state = None;
                t.arc_recovery_ticks = 0;
                t.arc_recovery_effect = None;
                t.arc_attacker = None;
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::FLOATING);
            }
        }
    }

    /// Returns the expiry tick of an existing CC debuff on the target, if any.
    /// Used as the "don't shorten" guard when applying timed CC — only extends
    /// CC duration, never shortens it.
    fn cc_debuff_expiry(
        &self,
        target_idx: game_core::entity::entity_index::EntityIndex,
        buff_id: u32,
    ) -> Option<TickId> {
        self.state
            .status
            .get_buffs(target_idx)
            .iter()
            .find(|b| b.buff_id == buff_id)
            .and_then(|b| b.expires_at)
    }

    /// Mutable reference to the `TacticalState` for `entity`, or `None` if the entity
    /// is not currently active. Replaces the repeated 2-line lookup+index pattern.
    #[inline]
    pub(super) fn tactical_mut(
        &mut self,
        entity: EntityId,
    ) -> Option<&mut game_core::combat::tactical::TacticalState> {
        let idx = self.state.entities.lookup(entity)?;
        Some(&mut self.state.combat.tactical[idx.as_usize()])
    }

    /// Read a field from `AbilityData`, returning `default` when the ability is unknown.
    /// Eliminates the repeated `self.abilities.get(id).map(|ad| ad.field).unwrap_or(v)` pattern.
    #[inline]
    pub(super) fn ability_prop<T>(
        &self,
        id: u32,
        f: fn(&game_core::combat::skill::AbilityData) -> T,
        default: T,
    ) -> T {
        self.abilities.get(id).map(f).unwrap_or(default)
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
    fn execution_is_alive(
        exec_id: AbilityExecutionId,
        scheduled_sources: &HashSet<AbilityExecutionId>,
        hitboxes: &game_core::combat::hitbox::HitboxStore,
    ) -> bool {
        scheduled_sources.contains(&exec_id) || hitboxes.get(exec_id).is_some()
    }

    /// Execute one full simulation tick, returning results for commit.
    pub fn run_tick(&mut self, intents: &[PlayerIntent]) -> TickResult {
        self.state.debug_assert_coherent();
        #[cfg(any(debug_assertions, test))]
        self.state.audit.reset();
        self.event_sequence = 0;
        self.pending_events.clear();
        self.pending_heals.clear();
        // Defensive symmetry with `pending_heals.clear()`: the buffer is
        // normally drained via `mem::take` in commit assembly, but an
        // early-return path between Phase 8b and result assembly would
        // otherwise leak prior-tick rows into the next commit.
        self.pending_death_state_inserts.clear();
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

        // Phase 7.5b: Encounter execution — evaluate boss encounter rules
        let encounter_outputs = self.phase_encounter_execution();
        let mut boss_phase_updates = Vec::new();
        let mut zone_counter_deltas = Vec::new();
        for output in encounter_outputs {
            match output {
                game_core::encounter::EncounterOutput::ChangeBossPhase {
                    boss_entity_id,
                    new_phase,
                    entered_at_tick,
                } => {
                    boss_phase_updates.push((boss_entity_id.0, new_phase, entered_at_tick));
                }
                game_core::encounter::EncounterOutput::IncrementZoneCounter {
                    layer,
                    region_x,
                    region_z,
                    counter_name,
                    delta,
                } => {
                    zone_counter_deltas.push((layer, region_x, region_z, counter_name, delta));
                }
            }
        }

        // Snapshot health for entities that took damage this tick — must happen BEFORE
        // phase_state_finalization removes dead entity slots from the EntityStore.
        let mut health_updates = self.collect_health_updates();

        // Phase 8: State finalization
        // 8a: Drain ready cooldowns — emit CooldownReady events for abilities that became
        //     available this tick. Must run before 8b so the events land in pending_events
        //     and are included in this tick's TickResult.
        self.phase_expire_cooldowns();
        // 8b: Entity lifecycle transitions, buff expiry, DoT damage, threat decay,
        //     spawning → active. Returns DoT health snapshots captured before DoT-killed
        //     entities are removed.
        let (entity_state_updates, dot_health) = self.phase_state_finalization();
        // Merge DoT health snapshots — DoT entries overwrite Phase 6 entries for the
        // same entity (DoT is the later, more current snapshot).
        for dot in dot_health {
            if let Some(existing) = health_updates.iter_mut().find(|(eid, _, _)| *eid == dot.0) {
                *existing = dot;
            } else {
                health_updates.push(dot);
            }
        }
        // Merge zone counter deltas emitted by Phase 8b death detection into the
        // tick's outgoing delta vec. Encounter-driven deltas (collected above) and
        // death-driven deltas are both associative increments, so merge order is
        // irrelevant.
        zone_counter_deltas.extend(self.pending_zone_counter_deltas.drain(..));

        // Aggregate by (layer, region_x, region_z, counter_name) so multiple
        // kills in the same region cell collapse into a single secondary
        // reducer call. The coordinator dispatches one `increment_zone_counter`
        // reducer per entry after the main commit, so an unaggregated N-death
        // wave produced N sequential round-trips; on 10k clustered NPC deaths
        // that serial fan-out was the dominant commit-latency tail. Counters
        // are additive, so summing deltas per key preserves the final row
        // value exactly. Also improves retry semantics: a failing write
        // contributes a single entry to `pending_secondary` instead of N.
        if zone_counter_deltas.len() > 1 {
            let mut agg: HashMap<(u32, i32, i32, String), f64> =
                HashMap::with_capacity(zone_counter_deltas.len());
            for (layer, rx, rz, name, delta) in zone_counter_deltas.drain(..) {
                *agg.entry((layer, rx, rz, name)).or_insert(0.0) += delta;
            }
            zone_counter_deltas = agg
                .into_iter()
                .map(|((layer, rx, rz, name), delta)| (layer, rx, rz, name, delta))
                .collect();
        }

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
            .filter(|&i| {
                matches!(
                    self.state.entities.states[i],
                    game_core::entity::lifecycle::EntityState::Active
                )
            })
            .count();
        self.summary.active_hitboxes = self.state.combat.hitboxes.len();
        self.summary.scheduled_actions_len = self.scheduled_actions.len();
        // Collect runtime domain snapshots for persistence.
        let buff_updates = self.collect_buff_updates();
        let npc_state_updates = self.collect_npc_state_updates();

        let all_transforms = self.physics.get_all_transforms();
        let region_updates = self.collect_region_updates(&all_transforms);
        let transforms = self.collect_transform_updates(&all_transforms);
        self.summary.transform_updates = transforms.len();
        self.summary.region_updates = region_updates.len();

        // Snapshot positions into the transform history ring buffer for lag compensation.
        // Must happen before advancing current_tick so the snapshot is tagged with the
        // tick that produced these positions.
        self.transform_history.record(
            self.current_tick,
            all_transforms
                .iter()
                .map(|(eid, t)| (*eid, t.position))
                .collect(),
        );

        let sim_warnings = std::mem::take(&mut self.summary.sim_warnings);

        let result = TickResult {
            tick_id: self.current_tick,
            transforms,
            events: std::mem::take(&mut self.pending_events),
            summary: self.summary.clone(),
            entity_state_updates,
            health_updates,
            buff_updates,
            npc_state_updates,
            region_updates,
            director_spawns,
            interactable_updates: std::mem::take(&mut self.pending_interactable_updates),
            boss_phase_updates,
            zone_counter_deltas,
            death_state_inserts: std::mem::take(&mut self.pending_death_state_inserts),
            sim_warnings,
        };

        self.current_tick = self.current_tick.next();

        #[cfg(any(debug_assertions, test))]
        if self.state.audit.total_writes() > 0 {
            log::trace!(
                "tick {} {}",
                result.tick_id.0,
                self.state.audit.summary_line()
            );
        }

        result
    }

    // ── Invariant verification (debug builds only) ──────────────

    /// Cross-structure invariant checks for parallel state systems.
    fn verify_invariants(&self) {
        self.state.debug_assert_coherent();

        let sensor_keys = self.state.combat.hitboxes.sensor_backed_execution_ids();
        let armed_keys = self.state.combat.hitboxes.armed_execution_ids();
        debug_assert_eq!(
            sensor_keys, armed_keys,
            "sensor-backed key-set and armed HitboxStore key-set diverged — hitbox lifecycle bug\n  sensor-backed: {:?}\n  armed hitboxes: {:?}",
            sensor_keys, armed_keys,
        );
    }

    // ── Event helpers ───────────────────────────────────────────

    pub(super) fn emit_event(&mut self, entity_id: EntityId, payload: EventPayload) {
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
pub(super) fn skill_shape_to_sensor(shape: SkillShape) -> SensorShape {
    match shape {
        SkillShape::Sphere => SensorShape::Sphere { radius: 2.0 },
        SkillShape::Cone => SensorShape::Capsule {
            half_height: 1.5,
            radius: 1.0,
        },
        SkillShape::CapsuleSweep => SensorShape::Capsule {
            half_height: 1.0,
            radius: 0.75,
        },
        SkillShape::Projectile => SensorShape::Sphere { radius: 0.5 },
        SkillShape::LineSweep => SensorShape::Capsule {
            half_height: 3.0,
            radius: 0.5,
        },
        SkillShape::HazardZone => SensorShape::Sphere { radius: 2.0 },
    }
}

/// Returns true if this collider kind is the **initiating** side of an interaction —
/// i.e. the thing that acts on something else, rather than receiving the action.
///
/// This is the single extension point for normalization precedence.
/// When `BlockCone` arrives, add it here.
/// `normalize_contact_pair` and `resolve_hits` need no changes.
pub(super) fn is_acting_collider(kind: ColliderKind) -> bool {
    use game_core::physics_backend::ColliderKind::*;
    matches!(kind, Hitbox(_))
    // future: | BlockCone(_)
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
pub(super) fn normalize_contact_pair(
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
