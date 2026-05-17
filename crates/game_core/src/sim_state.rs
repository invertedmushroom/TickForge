use std::collections::{BTreeSet, HashMap};
use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use game_schema::{EntityKind, EntityState, NpcAiState};

use crate::combat::hitbox::HitboxStore;
use crate::combat::loadout::WeaponLoadout;
use crate::combat::skill::AbilityExecutionStore;
use crate::combat::status::{ActiveBuff, ThreatTable};
use crate::combat::tactical::TacticalState;
use crate::sparse_set::SparseSet;
use crate::entity::entity_index::EntityIndex;
use crate::entity::entity_store::EntityStore;
use crate::physics_backend::CollisionEvent;
use crate::stats::{EquipmentModifiers, StatBlock, StatsStore};

// ── Mutation audit (debug/test only) ────────────────────────────────────────

/// Subsystem tags for mutation audit records.
#[cfg(any(debug_assertions, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuditSubsystem {
    Controller,
    Physics,
    Combat,
    AbilityTimeline,
    Lifecycle,
    CooldownTracker,
    StatusEffects,
    AiDecisions,
}

/// Domain tags for mutation audit records.
#[cfg(any(debug_assertions, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuditDomain {
    Transform,
    Health,
    Lifecycle,
    Cooldown,
    Hitbox,
    Execution,
    Threat,
    Buff,
    Ai,
    /// Follow-up eligibility windows (`CombatState::active_windows`).
    Window,
    /// Per-tick blocking/dodge flags (`CombatState::tactical`).
    Tactical,
}

/// A single audit record for one mutation event.
#[cfg(any(debug_assertions, test))]
#[derive(Clone, Debug)]
pub struct AuditRecord {
    pub domain: AuditDomain,
    pub subsystem: AuditSubsystem,
    pub phase: u8,
    pub entity: Option<EntityId>,
    pub detail: &'static str,
}

/// Per-tick mutation counters and optional detailed records.
///
/// Embedded in SimState, zeroed at the start of each tick.
/// Provides counters per (domain, phase) pair for CI violation checks
/// and an optional record log for debugging.
#[cfg(any(debug_assertions, test))]
#[derive(Clone, Debug, Default)]
pub struct MutationAudit {
    pub transform_writes: u32,
    pub health_writes: u32,
    pub lifecycle_writes: u32,
    pub cooldown_writes: u32,
    pub hitbox_writes: u32,
    pub execution_writes: u32,
    pub threat_writes: u32,
    pub buff_writes: u32,
    pub ai_writes: u32,
    pub window_writes: u32,
    pub tactical_writes: u32,
    /// Detailed records for debugging. Only populated when `record_details` is true.
    pub records: Vec<AuditRecord>,
    pub record_details: bool,
}

#[cfg(any(debug_assertions, test))]
impl MutationAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset counters for a new tick.
    pub fn reset(&mut self) {
        self.transform_writes = 0;
        self.health_writes = 0;
        self.lifecycle_writes = 0;
        self.cooldown_writes = 0;
        self.hitbox_writes = 0;
        self.execution_writes = 0;
        self.threat_writes = 0;
        self.buff_writes = 0;
        self.ai_writes = 0;
        self.window_writes = 0;
        self.tactical_writes = 0;
        self.records.clear();
    }

    /// Record a mutation event. Increments the domain counter and optionally logs details.
    ///
    /// **Enforcement:** panics (via `debug_assert!`) if the (domain, subsystem, phase)
    /// triple violates the ownership matrix. Active in debug and test builds only.
    pub fn record(
        &mut self,
        domain: AuditDomain,
        subsystem: AuditSubsystem,
        phase: u8,
        entity: Option<EntityId>,
        detail: &'static str,
    ) {
        debug_assert!(
            is_ownership_allowed(domain, subsystem, phase),
            "Ownership violation: {:?} written by {:?} in phase {} ({})",
            domain, subsystem, phase, detail
        );
        match domain {
            AuditDomain::Transform => self.transform_writes += 1,
            AuditDomain::Health => self.health_writes += 1,
            AuditDomain::Lifecycle => self.lifecycle_writes += 1,
            AuditDomain::Cooldown => self.cooldown_writes += 1,
            AuditDomain::Hitbox => self.hitbox_writes += 1,
            AuditDomain::Execution => self.execution_writes += 1,
            AuditDomain::Threat => self.threat_writes += 1,
            AuditDomain::Buff => self.buff_writes += 1,
            AuditDomain::Ai => self.ai_writes += 1,
            AuditDomain::Window => self.window_writes += 1,
            AuditDomain::Tactical => self.tactical_writes += 1,
        }
        if self.record_details {
            self.records.push(AuditRecord { domain, subsystem, phase, entity, detail });
        }
    }

    /// Return total mutation count across all domains.
    pub fn total_writes(&self) -> u32 {
        self.transform_writes + self.health_writes + self.lifecycle_writes
            + self.cooldown_writes + self.hitbox_writes + self.execution_writes
            + self.threat_writes + self.buff_writes + self.ai_writes
            + self.window_writes + self.tactical_writes
    }

    /// Print a one-line summary of mutation counts.
    pub fn summary_line(&self) -> String {
        format!(
            "audit: transform={} health={} lifecycle={} cooldown={} hitbox={} exec={} threat={} buff={} ai={} window={} tactical={}",
            self.transform_writes, self.health_writes, self.lifecycle_writes,
            self.cooldown_writes, self.hitbox_writes, self.execution_writes,
            self.threat_writes, self.buff_writes, self.ai_writes,
            self.window_writes, self.tactical_writes,
        )
    }
}

/// Check whether a (domain, subsystem, phase) triple is a valid ownership combination.
///
/// Returns `true` if the subsystem is the documented owner for the given domain in the
/// given phase. The allowed triples match the in-memory ownership matrix from
/// `docs/architecture.md`. Enforcement is active in debug/test builds only —
/// `MutationAudit::record()` calls this and panics on violation.
///
/// **Documented exceptions wired into the rules:**
/// - `Execution`: writable by both `Controller` (phase 2, cast) and `AbilityTimeline`
///   (phase 3, remove/cull).
/// - `Cooldown`: writable by `AbilityTimeline` (phase 3, start) and `CooldownTracker`
///   (phase 8, expire).
/// - `Lifecycle` phase-8 cleanup through `force_remove_entity` mutates hitboxes,
///   executions, and cooldowns directly without audit calls — those paths are owned
///   by the lifecycle system and intentionally bypass per-record enforcement.
#[cfg(any(debug_assertions, test))]
pub fn is_ownership_allowed(domain: AuditDomain, subsystem: AuditSubsystem, phase: u8) -> bool {
    match domain {
        AuditDomain::Health => {
            (subsystem == AuditSubsystem::Combat && phase == 6)
                || (subsystem == AuditSubsystem::StatusEffects && phase == 8)
        }
        AuditDomain::Threat => {
            (subsystem == AuditSubsystem::Combat && phase == 6)
                || (subsystem == AuditSubsystem::AiDecisions && phase == 7)
                || (subsystem == AuditSubsystem::Lifecycle && phase == 8)
        }
        AuditDomain::Transform => {
            (subsystem == AuditSubsystem::Controller && phase == 2)
                || (subsystem == AuditSubsystem::AiDecisions && phase == 7)
        }
        AuditDomain::Lifecycle => subsystem == AuditSubsystem::Lifecycle && phase == 8,
        AuditDomain::Hitbox => subsystem == AuditSubsystem::AbilityTimeline && phase == 3,
        AuditDomain::Execution => {
            (subsystem == AuditSubsystem::Controller && phase == 2)
                || (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::AiDecisions && phase == 7)
        }
        AuditDomain::Cooldown => {
            (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::CooldownTracker && phase == 8)
        }
        AuditDomain::Buff => {
            (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::Combat && phase == 6)
                || (subsystem == AuditSubsystem::StatusEffects && phase == 8)
        }
        AuditDomain::Ai => subsystem == AuditSubsystem::AiDecisions && phase == 7,
        // Window: AbilityTimeline writes in phase 3, CooldownTracker drains in phase 8.
        AuditDomain::Window => {
            (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::CooldownTracker && phase == 8)
        }
        // Tactical: Controller sets blocking in phase 2, AbilityTimeline writes in phase 3,
        // Combat applies CC conditions in phase 6, CooldownTracker clears in phase 8,
        // StatusEffects clears CC flags on natural buff expiry in phase 8.
        AuditDomain::Tactical => {
            (subsystem == AuditSubsystem::Controller && phase == 2)
                || (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::Combat && phase == 6)
                || (subsystem == AuditSubsystem::CooldownTracker && phase == 8)
                || (subsystem == AuditSubsystem::StatusEffects && phase == 8)
        }
    }
}

/// SoA health storage — hp, max_hp, and damage source as parallel arrays
/// indexed by `EntityIndex`.
pub struct HealthStore {
    pub hp: Vec<f32>,
    pub max_hp: Vec<f32>,
    pub last_damage_source: Vec<Option<EntityId>>,
}

impl HealthStore {
    pub fn new() -> Self {
        Self {
            hp: Vec::new(),
            max_hp: Vec::new(),
            last_damage_source: Vec::new(),
        }
    }

    /// Push a new entity's health slot.
    pub fn push(&mut self, max_hp: f32) {
        self.hp.push(max_hp);
        self.max_hp.push(max_hp);
        self.last_damage_source.push(None);
    }

    /// Reset an existing slot for a reused entity.
    pub fn reset(&mut self, idx: EntityIndex, max_hp: f32) {
        let i = idx.as_usize();
        self.hp[i] = max_hp;
        self.max_hp[i] = max_hp;
        self.last_damage_source[i] = None;
    }

    #[inline]
    pub fn is_dead(&self, idx: EntityIndex) -> bool {
        self.hp[idx.as_usize()] <= 0.0
    }

    /// Apply damage, clamping to 0. Returns actual damage dealt.
    ///
    /// Non-finite or negative amounts are treated as zero — callers should not
    /// need to pre-validate, and bad data must never invert the effect.
    pub fn apply_damage(&mut self, idx: EntityIndex, amount: f32, source: Option<EntityId>) -> f32 {
        let i = idx.as_usize();
        let amount = if amount.is_finite() { amount.max(0.0) } else { 0.0 };
        let actual = amount.min(self.hp[i]);
        self.hp[i] -= actual;
        if actual > 0.0 && source.is_some() {
            self.last_damage_source[i] = source;
        }
        actual
    }

    /// Apply healing, clamping to max. Returns actual healing done.
    ///
    /// Non-finite or negative amounts are treated as zero.
    pub fn apply_healing(&mut self, idx: EntityIndex, amount: f32) -> f32 {
        let i = idx.as_usize();
        let amount = if amount.is_finite() { amount.max(0.0) } else { 0.0 };
        let actual = amount.min(self.max_hp[i] - self.hp[i]);
        self.hp[i] += actual;
        actual
    }
}

impl Default for HealthStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Physics-related runtime data.
pub struct PhysicsState {
    /// Collision events from the latest physics step.
    pub contacts: Vec<CollisionEvent>,
}

/// Hot combat data — iterated together during combat resolution.
pub struct CombatState {
    /// Dense health arrays (hp, max_hp, last_damage_source).
    pub health: HealthStore,
    /// Threat tables — only NPC/Boss entities carry one.
    pub threat_tables: SparseSet<ThreatTable>,
    /// Active hitbox colliders — tracks hit dedup, ownership, lifetime.
    pub hitboxes: HitboxStore,
    /// In-flight ability cast instances — one entry per accepted UseAbility cast,
    /// carrying resolved targeting and cast-time spatial snapshot.
    pub executions: AbilityExecutionStore,
    /// Follow-up / combo windows: maps `(EntityId, ability_id)` → `(next_ability_id, expiry)`.
    ///
    /// Written by `OpenFollowUpWindow` timeline actions in Phase 3.
    /// Phase 2 checks this map: if the player presses `ability_id` while a window
    /// is open, the cast is redirected to `next_ability_id` (a separate ability
    /// with its own timeline and stats).  Phase 8 drains expired entries.
    pub active_windows: HashMap<(EntityId, u32), (u32, TickId)>,
    /// Per-entity tactical flags for this tick (blocking, dodge).
    ///
    /// Written by `StanceBegin` timeline actions in Phase 3.
    /// Phase 6 checks flags before routing hits through the damage path.
    /// Phase 8 clears all flags so `StanceBegin` must re-assert each tick.
    pub tactical: Vec<TacticalState>,
    /// In-flight charge states: one entry per entity currently charging a hold-release ability.
    ///
    /// Written by Phase 2 when UseAbility targets a chargeable ability.
    /// Consumed by Phase 2 on ReleaseAbility or auto-release at max tier.
    /// Cleaned up in `force_remove_entity`.
    pub charging: HashMap<EntityId, crate::combat::skill::ChargingState>,
    /// Weapon loadouts — player-only. Maps entity → two weapon sets + active index.
    /// Written by Phase 2 `WeaponSwap` intent. Read by Phase 2 `UseAbility` to
    /// validate the ability is in the active weapon set.
    pub loadouts: SparseSet<WeaponLoadout>,
}

/// Buff/debuff status data.
pub struct StatusState {
    /// Active buffs/debuffs per entity (empty Vec for entities without buffs).
    buffs: Vec<Vec<ActiveBuff>>,
    /// Entity indices whose buff arrays were mutated this tick.
    /// BTreeSet ensures deterministic iteration order for reproducible commits.
    dirty_entities: BTreeSet<usize>,
}

impl StatusState {
    pub fn new() -> Self {
        Self {
            buffs: Vec::new(),
            dirty_entities: BTreeSet::new(),
        }
    }

    /// Push an empty buff slot for a newly spawned entity.
    pub fn push_empty(&mut self) {
        self.buffs.push(Vec::new());
    }

    /// Clear an existing buff slot for a reused entity.
    pub fn clear_at(&mut self, idx: EntityIndex) {
        self.buffs[idx.as_usize()].clear();
    }

    /// Number of buff slots (must equal entity count).
    pub fn len(&self) -> usize {
        self.buffs.len()
    }

    /// Read-only access to an entity's active buffs.
    pub fn get_buffs(&self, idx: EntityIndex) -> &[ActiveBuff] {
        &self.buffs[idx.as_usize()]
    }

    /// Clone an entity's active buffs (used by collect_buff_updates).
    pub fn clone_buffs(&self, i: usize) -> Vec<ActiveBuff> {
        self.buffs[i].clone()
    }

    /// Mutate an entity's buff array through a closure, automatically marking dirty.
    pub fn modify_buffs<F>(&mut self, idx: EntityIndex, f: F)
    where
        F: FnOnce(&mut Vec<ActiveBuff>),
    {
        f(&mut self.buffs[idx.as_usize()]);
        self.dirty_entities.insert(idx.as_usize());
    }

    /// Apply a buff with stacking: if a buff with the same `buff_id` already exists,
    /// increment stacks (capped at `max_stacks`) and refresh the expiration.
    /// Otherwise, push a new entry. Returns the resulting stack count.
    pub fn apply_or_stack_buff(&mut self, idx: EntityIndex, buff: ActiveBuff) -> u32 {
        let buffs = &mut self.buffs[idx.as_usize()];
        if let Some(existing) = buffs.iter_mut().find(|b| b.buff_id == buff.buff_id) {
            if existing.stacks < existing.max_stacks {
                existing.stacks += 1;
            }
            // Refresh duration to the new buff's expiration.
            existing.expires_at = buff.expires_at;
            self.dirty_entities.insert(idx.as_usize());
            existing.stacks
        } else {
            let stacks = buff.stacks;
            buffs.push(buff);
            self.dirty_entities.insert(idx.as_usize());
            stacks
        }
    }

    /// Consume one stack of a buff by `buff_id`. If the buff reaches 0 stacks,
    /// it is removed entirely and this returns `true`. Returns `false` if the
    /// buff still has stacks remaining. No-op if the buff is not found.
    pub fn consume_stack(&mut self, idx: EntityIndex, buff_id: u32) -> bool {
        let buffs = &mut self.buffs[idx.as_usize()];
        let pos = buffs.iter().position(|b| b.buff_id == buff_id);
        let Some(pos) = pos else { return false };
        self.dirty_entities.insert(idx.as_usize());
        if buffs[pos].stacks <= 1 {
            buffs.swap_remove(pos);
            true
        } else {
            buffs[pos].stacks -= 1;
            false
        }
    }

    /// Replace an entity's entire buff array, marking dirty.
    pub fn replace_buffs(&mut self, idx: EntityIndex, buffs: Vec<ActiveBuff>) {
        self.buffs[idx.as_usize()] = buffs;
        self.dirty_entities.insert(idx.as_usize());
    }

    /// Mark an entity's buff array as changed this tick.
    pub fn mark_dirty(&mut self, idx: EntityIndex) {
        self.dirty_entities.insert(idx.as_usize());
    }

    /// Drain and return the set of dirty entity indices, clearing it.
    /// Iteration order is deterministic (ascending) because BTreeSet is sorted.
    pub fn take_dirty(&mut self) -> BTreeSet<usize> {
        std::mem::take(&mut self.dirty_entities)
    }

    /// Expire buffs that have passed their expiration tick.
    /// Returns (entity_id, expired_buff) pairs so callers can inspect modifiers
    /// (e.g. `cc_effect`) for cleanup. Only marks entities dirty when at least
    /// one buff was actually removed.
    pub(crate) fn expire(&mut self, entities: &EntityStore, current_tick: TickId) -> Vec<(EntityId, ActiveBuff)> {
        let mut expired = Vec::new();
        for i in 0..entities.len() {
            if entities.states[i] == EntityState::Removed {
                continue;
            }
            let entity_id = entities.id_of(entities.index_at(i));
            let before = self.buffs[i].len();
            self.buffs[i].retain(|b| {
                if let Some(expires_at) = b.expires_at
                    && expires_at <= current_tick {
                        expired.push((entity_id, b.clone()));
                        return false;
                    }
                true
            });
            if self.buffs[i].len() != before {
                self.dirty_entities.insert(i);
            }
        }
        expired
    }

    /// Remove up to `count` Condition debuffs (oldest first), returning the removed buffs.
    pub fn cleanse_conditions(&mut self, idx: EntityIndex, count: u32) -> Vec<ActiveBuff> {
        let buffs = &mut self.buffs[idx.as_usize()];
        let mut removed = Vec::new();
        let mut i = 0;
        while i < buffs.len() && (removed.len() as u32) < count {
            if buffs[i].buff_kind == crate::combat::status::BuffKind::Condition {
                removed.push(buffs.swap_remove(i));
            } else {
                i += 1;
            }
        }
        if !removed.is_empty() {
            self.dirty_entities.insert(idx.as_usize());
        }
        removed
    }

    /// Remove the first debuff whose `cc_effect` matches, returning it if found.
    pub fn remove_cc_debuff(&mut self, idx: EntityIndex, cc_effect: game_schema::CCEffect) -> Option<ActiveBuff> {
        let buffs = &mut self.buffs[idx.as_usize()];
        let pos = buffs.iter().position(|b| b.modifiers.cc_effect == Some(cc_effect))?;
        self.dirty_entities.insert(idx.as_usize());
        Some(buffs.swap_remove(pos))
    }
}

/// NPC AI data.
pub struct AiState {
    /// NPC AI state — only NPC/Boss entities carry one.
    pub npc_ai: SparseSet<NpcAiState>,
    /// Spawn position used as the patrol home point for NPCs/bosses.
    pub home_positions: SparseSet<Vec3f>,
    /// Ability IDs this NPC can use in combat. Empty = no attacks.
    pub npc_ability_ids: SparseSet<Vec<u32>>,
    /// Passive NPCs never enter Combat state (training dummies).
    pub npc_passive: SparseSet<bool>,
    /// No-chase NPCs fight back but don’t move toward the target.
    pub npc_no_chase: SparseSet<bool>,
}

/// Runtime simulation state for one region.
///
/// Dense array storage indexed by `EntityIndex` for cache-friendly
/// iteration. The `EntityStore` manages the `EntityId ↔ EntityIndex`
/// mapping; all component arrays use the same index space.
///
/// Sub-structs group hot data by access pattern so that combat-tick
/// hot loops only touch `combat` / `physics`, keeping cold data
/// (`status`, `ai`) out of the cache lines.
pub struct SimState {
    /// Entity lifecycle — id mapping, kind, state, spawn tick.
    pub entities: EntityStore,
    /// Physics contacts.
    pub physics: PhysicsState,
    /// Health, threat, hitboxes.
    pub combat: CombatState,
    /// Buffs/debuffs.
    pub status: StatusState,
    /// NPC AI.
    pub ai: AiState,
    /// Cached per-entity derived stats (movement speed, damage multipliers, etc.).
    pub stats: StatsStore,
    /// Per-tick mutation counters (debug/test only).
    #[cfg(any(debug_assertions, test))]
    pub audit: MutationAudit,
}

impl SimState {
    pub fn new() -> Self {
        Self {
            entities: EntityStore::new(),
            physics: PhysicsState { contacts: Vec::new() },
            combat: CombatState {
                health: HealthStore::new(),
                threat_tables: SparseSet::new(),
                hitboxes: HitboxStore::new(),
                executions: AbilityExecutionStore::new(),
                active_windows: HashMap::new(),
                tactical: Vec::new(),
                charging: HashMap::new(),
                loadouts: SparseSet::new(),
            },
            status: StatusState::new(),
            ai: AiState { npc_ai: SparseSet::new(), home_positions: SparseSet::new(), npc_ability_ids: SparseSet::new(), npc_passive: SparseSet::new(), npc_no_chase: SparseSet::new() },
            stats: StatsStore::new(),
            #[cfg(any(debug_assertions, test))]
            audit: MutationAudit::new(),
        }
    }

    /// Debug-only check that all parallel component arrays have the same length
    /// as the entity store. Catches desync from missing push in spawn_entity.
    pub fn debug_assert_coherent(&self) {
        let n = self.entities.len();
        debug_assert_eq!(self.combat.health.hp.len(), n, "health.hp desync");
        debug_assert_eq!(self.combat.health.max_hp.len(), n, "health.max_hp desync");
        debug_assert_eq!(self.combat.health.last_damage_source.len(), n, "health.last_damage_source desync");
        debug_assert_eq!(self.combat.threat_tables.sparse_len(), n, "threat_tables desync");
        debug_assert_eq!(self.combat.tactical.len(), n, "tactical desync");
        debug_assert_eq!(self.status.len(), n, "buffs desync");
        debug_assert_eq!(self.ai.npc_ai.sparse_len(), n, "npc_ai desync");
        debug_assert_eq!(self.ai.home_positions.sparse_len(), n, "home_positions desync");
        debug_assert_eq!(self.ai.npc_ability_ids.sparse_len(), n, "npc_ability_ids desync");
        debug_assert_eq!(self.ai.npc_passive.sparse_len(), n, "npc_passive desync");
        debug_assert_eq!(self.ai.npc_no_chase.sparse_len(), n, "npc_no_chase desync");
        debug_assert_eq!(self.combat.loadouts.sparse_len(), n, "loadouts desync");
        debug_assert_eq!(self.stats.len(), n, "stats desync");
    }

    /// Register a new entity, returning its dense index.
    pub fn spawn_entity(
        &mut self,
        id: EntityId,
        kind: EntityKind,
        tick: TickId,
        max_hp: f32,
    ) -> EntityIndex {
        let (idx, reused) = self.entities.spawn(id, kind, tick);
        if reused {
            // Overwrite existing component slots at the recycled index.
            self.combat.health.reset(idx, max_hp);
            self.status.clear_at(idx);
            self.status.mark_dirty(idx);
            // SparseSet slots already exist; insert only for NPC/Boss.
            if kind == EntityKind::Npc || kind == EntityKind::Boss {
                self.combat.threat_tables.insert(idx, ThreatTable::default());
                self.ai.npc_ai.insert(idx, NpcAiState::Idle);
                self.ai.npc_ability_ids.insert(idx, vec![1]);
            }
            // Remove stale loadout on slot reuse (player will re-assign).
            self.combat.loadouts.remove(idx);
            self.combat.tactical[idx.as_usize()] = TacticalState::default();
            if kind == EntityKind::Boss {
                self.combat.tactical[idx.as_usize()].dr_immune = true;
            }
            self.stats.set(idx, StatBlock::compute(kind, max_hp, &[], &EquipmentModifiers::default()));
        } else {
            // Push new component slots in lockstep with the entity store.
            self.combat.health.push(max_hp);
            self.status.push_empty();
            // Mark the new entity's buff slot dirty so the first commit clears any
            // stale DB rows (e.g. after a worker restart where in-flight buffs were
            // never committed).
            self.status.mark_dirty(idx);
            self.combat.threat_tables.push_slot();
            self.combat.loadouts.push_slot();
            self.ai.npc_ai.push_slot();
            self.ai.home_positions.push_slot();
            self.ai.npc_ability_ids.push_slot();
            self.ai.npc_passive.push_slot();
            self.ai.npc_no_chase.push_slot();
            if kind == EntityKind::Npc || kind == EntityKind::Boss {
                self.combat.threat_tables.insert(idx, ThreatTable::default());
                self.ai.npc_ai.insert(idx, NpcAiState::Idle);
                // Default ability: Slash (ability_id 1).
                self.ai.npc_ability_ids.insert(idx, vec![1]);
            }
            self.combat.tactical.push(TacticalState::default());
            if kind == EntityKind::Boss {
                self.combat.tactical.last_mut().unwrap().dr_immune = true;
            }
            self.stats.push(StatBlock::compute(kind, max_hp, &[], &EquipmentModifiers::default()));
        }
        idx
    }

    /// Transition an entity to Active state.
    pub fn activate_entity(&mut self, id: EntityId) {
        if let Some(idx) = self.entities.lookup(id) {
            self.entities.activate(idx);
        }
    }

    /// Mark an entity for removal at the end of the tick.
    pub fn mark_despawn(&mut self, id: EntityId) {
        if let Some(idx) = self.entities.lookup(id) {
            self.entities.mark_despawn(idx);
        }
    }

    /// Tombstone-remove: mark Removed, drop id mapping, clean up sparse components.    
    /// Hitbox and execution cleanup is owned by `force_remove_entity`    
    /// (which has physics access for sensor teardown) — not duplicated here.    
    /// Returns true if the entity existed.
    pub fn remove_entity(&mut self, id: EntityId) -> bool {
        if let Some(idx) = self.entities.lookup(id) {
            self.entities.mark_removed(idx);
            // Clean up sparse components so their dense arrays stay compact.
            self.combat.threat_tables.remove(idx);
            self.combat.loadouts.remove(idx);
            self.ai.npc_ai.remove(idx);
            self.ai.home_positions.remove(idx);
            self.ai.npc_ability_ids.remove(idx);
            self.ai.npc_passive.remove(idx);
            self.ai.npc_no_chase.remove(idx);
            true
        } else {
            false
        }
    }

    /// Get all entity ids currently in DespawnPending state.
    pub fn despawn_pending(&self) -> Vec<EntityId> {
        (0..self.entities.len())
            .filter(|&i| self.entities.states[i] == EntityState::DespawnPending)
            .map(|i| self.entities.id_of(self.entities.index_at(i)))
            .collect()
    }

    /// Get indices of all active entities of a given kind.
    pub fn active_indices_of_kind(&self, kind: EntityKind) -> Vec<EntityIndex> {
        (0..self.entities.len())
            .filter(|&i| {
                self.entities.kinds[i] == kind
                    && self.entities.states[i] == EntityState::Active
            })
            .map(|i| self.entities.index_at(i))
            .collect()
    }

    /// Get all active entity ids of a given kind.
    pub fn active_entities_of_kind(&self, kind: EntityKind) -> Vec<EntityId> {
        self.active_indices_of_kind(kind)
            .iter()
            .map(|&idx| self.entities.id_of(idx))
            .collect()
    }

    /// Expire buffs that have passed their expiration tick.
    pub fn expire_buffs(&mut self, current_tick: TickId) -> Vec<(EntityId, crate::combat::status::ActiveBuff)> {
        self.status.expire(&self.entities, current_tick)
    }

    // ── Convenience accessors (EntityId → lookup → dense array) ─

    /// Check if an entity is in the Active state.
    pub fn is_active(&self, id: EntityId) -> bool {
        self.entities.lookup(id)
            .is_some_and(|idx| self.entities.is_active(idx))
    }

    /// Get current HP for an entity.
    pub fn hp_of(&self, id: EntityId) -> Option<f32> {
        self.entities.lookup(id).map(|idx| self.combat.health.hp[idx.as_usize()])
    }

    /// Get last damage source for an entity.
    pub fn last_damage_source_of(&self, id: EntityId) -> Option<EntityId> {
        self.entities.lookup(id)
            .and_then(|idx| self.combat.health.last_damage_source[idx.as_usize()])
    }

    /// Get threat table for an entity (if NPC/Boss).
    pub fn threat_table_of(&self, id: EntityId) -> Option<&ThreatTable> {
        self.entities.lookup(id)
            .and_then(|idx| self.combat.threat_tables.get(idx))
    }
}

impl Default for SimState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(n: u64) -> EntityId {
        EntityId(n)
    }

    #[test]
    fn spawn_and_activate() {
        let mut state = SimState::new();
        let idx = state.spawn_entity(eid(1), EntityKind::Player, TickId(0), 100.0);

        assert_eq!(state.entities.states[idx.as_usize()], EntityState::Spawning);
        assert_eq!(state.entities.kinds[idx.as_usize()], EntityKind::Player);

        state.activate_entity(eid(1));
        assert!(state.entities.is_active(idx));

        assert_eq!(state.combat.health.hp[idx.as_usize()], 100.0);
        // Players don't get threat tables
        assert!(!state.combat.threat_tables.contains(idx));
    }

    #[test]
    fn npc_gets_threat_table_and_ai() {
        let mut state = SimState::new();
        let idx = state.spawn_entity(eid(10), EntityKind::Npc, TickId(0), 500.0);

        assert!(state.combat.threat_tables.contains(idx));
        assert_eq!(state.ai.npc_ai.get(idx).copied(), Some(NpcAiState::Idle));
    }

    #[test]
    fn health_damage_and_healing() {
        let mut store = HealthStore::new();
        store.push(100.0);
        let idx = EntityIndex::dangling(0);

        assert_eq!(store.apply_damage(idx, 30.0, Some(eid(99))), 30.0);
        assert_eq!(store.hp[0], 70.0);
        assert_eq!(store.last_damage_source[0], Some(eid(99)));

        // Overkill clamps; source=None must NOT erase the recorded killer
        assert_eq!(store.apply_damage(idx, 200.0, None), 70.0);
        assert!(store.is_dead(idx));
        assert_eq!(store.last_damage_source[0], Some(eid(99)));

        // Healing on dead doesn't exceed max
        assert_eq!(store.apply_healing(idx, 50.0), 50.0);
        assert_eq!(store.hp[0], 50.0);
        assert_eq!(store.apply_healing(idx, 999.0), 50.0);
        assert_eq!(store.hp[0], 100.0);
    }

    #[test]
    fn damage_rejects_negative_and_non_finite() {
        let mut store = HealthStore::new();
        store.push(100.0);
        let idx = EntityIndex::dangling(0);

        // Negative damage must not heal
        assert_eq!(store.apply_damage(idx, -50.0, Some(eid(1))), 0.0);
        assert_eq!(store.hp[0], 100.0);

        // NaN damage must not corrupt
        assert_eq!(store.apply_damage(idx, f32::NAN, Some(eid(1))), 0.0);
        assert_eq!(store.hp[0], 100.0);

        // Positive infinity must not corrupt
        assert_eq!(store.apply_damage(idx, f32::INFINITY, Some(eid(1))), 0.0);
        assert_eq!(store.hp[0], 100.0);

        // Negative infinity must not corrupt
        assert_eq!(store.apply_damage(idx, f32::NEG_INFINITY, Some(eid(1))), 0.0);
        assert_eq!(store.hp[0], 100.0);

        // Confirm normal damage still works after rejections
        assert_eq!(store.apply_damage(idx, 10.0, Some(eid(1))), 10.0);
        assert_eq!(store.hp[0], 90.0);
    }

    #[test]
    fn healing_rejects_negative_and_non_finite() {
        let mut store = HealthStore::new();
        store.push(100.0);
        let idx = EntityIndex::dangling(0);
        store.apply_damage(idx, 50.0, None);
        assert_eq!(store.hp[0], 50.0);

        // Negative healing must not damage
        assert_eq!(store.apply_healing(idx, -30.0), 0.0);
        assert_eq!(store.hp[0], 50.0);

        // NaN healing must not corrupt
        assert_eq!(store.apply_healing(idx, f32::NAN), 0.0);
        assert_eq!(store.hp[0], 50.0);

        // Positive infinity must not corrupt
        assert_eq!(store.apply_healing(idx, f32::INFINITY), 0.0);
        assert_eq!(store.hp[0], 50.0);

        // Negative infinity must not corrupt
        assert_eq!(store.apply_healing(idx, f32::NEG_INFINITY), 0.0);
        assert_eq!(store.hp[0], 50.0);

        // Confirm normal healing still works after rejections
        assert_eq!(store.apply_healing(idx, 20.0), 20.0);
        assert_eq!(store.hp[0], 70.0);
    }

    #[test]
    fn despawn_lifecycle() {
        let mut state = SimState::new();
        state.spawn_entity(eid(1), EntityKind::Player, TickId(0), 100.0);
        state.activate_entity(eid(1));
        state.mark_despawn(eid(1));

        assert_eq!(state.despawn_pending(), vec![eid(1)]);

        state.remove_entity(eid(1));
        assert!(!state.entities.contains(eid(1)));
        assert_eq!(state.entities.states[0], EntityState::Removed);
    }

    #[test]
    fn remove_entity_clears_loadout_component() {
        let mut state = SimState::new();
        let idx = state.spawn_entity(eid(1), EntityKind::Player, TickId(0), 100.0);
        state.combat.loadouts.insert(idx, WeaponLoadout::new(vec![1, 2], vec![3, 4]));

        assert!(state.combat.loadouts.get(idx).is_some(), "loadout should exist before removal");
        assert!(state.remove_entity(eid(1)), "entity should be removable");
        assert!(state.combat.loadouts.get(idx).is_none(), "loadout should be cleared during removal");
    }

    #[test]
    fn expire_buffs_by_tick() {
        let mut state = SimState::new();
        let idx = state.spawn_entity(eid(1), EntityKind::Player, TickId(0), 100.0);
        state.status.modify_buffs(idx, |buffs| buffs.push(ActiveBuff {
            buff_id: 42,
            source: eid(99),
            target: eid(1),
            buff_kind: Default::default(),
            stacks: 1,
            max_stacks: 1,
            expires_at: Some(TickId(10)),
            modifiers: Default::default(),
            last_dot_tick: None,
        }));
        state.status.modify_buffs(idx, |buffs| buffs.push(ActiveBuff {
            buff_id: 43,
            source: eid(99),
            target: eid(1),
            buff_kind: Default::default(),
            stacks: 1,
            max_stacks: 1,
            expires_at: None, // permanent
            modifiers: Default::default(),
            last_dot_tick: None,
        }));

        let expired = state.expire_buffs(TickId(10));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, eid(1));
        assert_eq!(expired[0].1.buff_id, 42);
        // Permanent buff remains
        assert_eq!(state.status.get_buffs(idx).len(), 1);
        assert_eq!(state.status.get_buffs(idx)[0].buff_id, 43);
    }

    // ── Ownership enforcement guardrail tests ───────────────────

    #[test]
    fn ownership_allows_valid_triples() {
        // Verify that every documented ownership triple passes.
        assert!(is_ownership_allowed(AuditDomain::Health, AuditSubsystem::Combat, 6));
        assert!(is_ownership_allowed(AuditDomain::Threat, AuditSubsystem::Combat, 6));
        assert!(is_ownership_allowed(AuditDomain::Threat, AuditSubsystem::AiDecisions, 7));
        assert!(is_ownership_allowed(AuditDomain::Threat, AuditSubsystem::Lifecycle, 8));
        assert!(is_ownership_allowed(AuditDomain::Transform, AuditSubsystem::Controller, 2));
        assert!(is_ownership_allowed(AuditDomain::Transform, AuditSubsystem::AiDecisions, 7));
        assert!(is_ownership_allowed(AuditDomain::Lifecycle, AuditSubsystem::Lifecycle, 8));
        assert!(is_ownership_allowed(AuditDomain::Hitbox, AuditSubsystem::AbilityTimeline, 3));
        assert!(is_ownership_allowed(AuditDomain::Execution, AuditSubsystem::Controller, 2));
        assert!(is_ownership_allowed(AuditDomain::Execution, AuditSubsystem::AbilityTimeline, 3));
        assert!(is_ownership_allowed(AuditDomain::Cooldown, AuditSubsystem::AbilityTimeline, 3));
        assert!(is_ownership_allowed(AuditDomain::Cooldown, AuditSubsystem::CooldownTracker, 8));
        assert!(is_ownership_allowed(AuditDomain::Buff, AuditSubsystem::StatusEffects, 8));
        assert!(is_ownership_allowed(AuditDomain::Ai, AuditSubsystem::AiDecisions, 7));
        assert!(is_ownership_allowed(AuditDomain::Tactical, AuditSubsystem::Controller, 2));
        assert!(is_ownership_allowed(AuditDomain::Tactical, AuditSubsystem::Combat, 6));
        assert!(is_ownership_allowed(AuditDomain::Tactical, AuditSubsystem::StatusEffects, 8));
    }

    #[test]
    fn health_ownership_rejects_non_combat_writer() {
        // Health may only be written by Combat in phase 6.
        assert!(!is_ownership_allowed(AuditDomain::Health, AuditSubsystem::Lifecycle, 8));
        assert!(!is_ownership_allowed(AuditDomain::Health, AuditSubsystem::Controller, 2));
        assert!(!is_ownership_allowed(AuditDomain::Health, AuditSubsystem::Combat, 3));
        assert!(!is_ownership_allowed(AuditDomain::Health, AuditSubsystem::AiDecisions, 7));
    }

    #[test]
    #[should_panic(expected = "Ownership violation")]
    fn health_enforcement_panics_on_invalid_write() {
        let mut audit = MutationAudit::new();
        audit.record(AuditDomain::Health, AuditSubsystem::Lifecycle, 8, None, "invalid");
    }

    #[test]
    fn lifecycle_ownership_rejects_non_lifecycle_writer() {
        // Lifecycle may only be written by Lifecycle in phase 8.
        assert!(!is_ownership_allowed(AuditDomain::Lifecycle, AuditSubsystem::Combat, 6));
        assert!(!is_ownership_allowed(AuditDomain::Lifecycle, AuditSubsystem::Controller, 2));
        assert!(!is_ownership_allowed(AuditDomain::Lifecycle, AuditSubsystem::Lifecycle, 3));
        assert!(!is_ownership_allowed(AuditDomain::Lifecycle, AuditSubsystem::AiDecisions, 7));
    }

    #[test]
    #[should_panic(expected = "Ownership violation")]
    fn lifecycle_enforcement_panics_on_invalid_write() {
        let mut audit = MutationAudit::new();
        audit.record(AuditDomain::Lifecycle, AuditSubsystem::Combat, 6, None, "invalid");
    }

    #[test]
    fn transform_ownership_rejects_non_controller_writer() {
        // Transform may only be written by Controller in phase 2.
        assert!(!is_ownership_allowed(AuditDomain::Transform, AuditSubsystem::Combat, 6));
        assert!(!is_ownership_allowed(AuditDomain::Transform, AuditSubsystem::Physics, 4));
        assert!(!is_ownership_allowed(AuditDomain::Transform, AuditSubsystem::Controller, 8));
        assert!(!is_ownership_allowed(AuditDomain::Transform, AuditSubsystem::Lifecycle, 8));
    }

    #[test]
    #[should_panic(expected = "Ownership violation")]
    fn transform_enforcement_panics_on_invalid_write() {
        let mut audit = MutationAudit::new();
        audit.record(AuditDomain::Transform, AuditSubsystem::Physics, 4, None, "invalid");
    }

    #[test]
    fn cooldown_ownership_rejects_invalid_writer() {
        // Cooldown may only be written by AbilityTimeline (phase 3) or CooldownTracker (phase 8).
        assert!(!is_ownership_allowed(AuditDomain::Cooldown, AuditSubsystem::Combat, 6));
        assert!(!is_ownership_allowed(AuditDomain::Cooldown, AuditSubsystem::Controller, 2));
        assert!(!is_ownership_allowed(AuditDomain::Cooldown, AuditSubsystem::AbilityTimeline, 8));
        assert!(!is_ownership_allowed(AuditDomain::Cooldown, AuditSubsystem::CooldownTracker, 3));
    }

    #[test]
    #[should_panic(expected = "Ownership violation")]
    fn cooldown_enforcement_panics_on_invalid_write() {
        let mut audit = MutationAudit::new();
        audit.record(AuditDomain::Cooldown, AuditSubsystem::Combat, 6, None, "invalid");
    }
}
