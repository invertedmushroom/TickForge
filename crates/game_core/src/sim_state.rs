use std::collections::HashMap;
use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use game_schema::{EntityKind, EntityState, NpcAiState};

use crate::combat::hitbox::HitboxStore;
use crate::combat::skill::AbilityExecutionStore;
use crate::combat::status::{ActiveBuff, ThreatTable};
use crate::combat::tactical::TacticalState;
use crate::entity::entity_index::EntityIndex;
use crate::entity::entity_store::EntityStore;
use crate::physics_backend::CollisionEvent;

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
///    (phase 3, remove/cull).
/// - `Cooldown`: writable by `AbilityTimeline` (phase 3, start) and `CooldownTracker`
///    (phase 8, expire).
/// - `Lifecycle` phase-8 cleanup through `force_remove_entity` mutates hitboxes,
///    executions, and cooldowns directly without audit calls — those paths are owned
///    by the lifecycle system and intentionally bypass per-record enforcement.
#[cfg(any(debug_assertions, test))]
pub fn is_ownership_allowed(domain: AuditDomain, subsystem: AuditSubsystem, phase: u8) -> bool {
    match domain {
        AuditDomain::Health => subsystem == AuditSubsystem::Combat && phase == 6,
        AuditDomain::Threat => subsystem == AuditSubsystem::Combat && phase == 6,
        AuditDomain::Transform => {
            (subsystem == AuditSubsystem::Controller && phase == 2)
                || (subsystem == AuditSubsystem::AiDecisions && phase == 7)
        }
        AuditDomain::Lifecycle => subsystem == AuditSubsystem::Lifecycle && phase == 8,
        AuditDomain::Hitbox => subsystem == AuditSubsystem::AbilityTimeline && phase == 3,
        AuditDomain::Execution => {
            (subsystem == AuditSubsystem::Controller && phase == 2)
                || (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
        }
        AuditDomain::Cooldown => {
            (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::CooldownTracker && phase == 8)
        }
        AuditDomain::Buff => subsystem == AuditSubsystem::StatusEffects && phase == 8,
        AuditDomain::Ai => subsystem == AuditSubsystem::AiDecisions && phase == 7,
        // Window: AbilityTimeline writes in phase 3, CooldownTracker drains in phase 8.
        AuditDomain::Window => {
            (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::CooldownTracker && phase == 8)
        }
        // Tactical: AbilityTimeline writes in phase 3, CooldownTracker clears in phase 8.
        AuditDomain::Tactical => {
            (subsystem == AuditSubsystem::AbilityTimeline && phase == 3)
                || (subsystem == AuditSubsystem::CooldownTracker && phase == 8)
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
    /// Threat tables — `Some` for NPCs/bosses, `None` for other entity kinds.
    pub threat_tables: Vec<Option<ThreatTable>>,
    /// Active hitbox colliders — tracks hit dedup, ownership, lifetime.
    pub hitboxes: HitboxStore,
    /// In-flight ability cast instances — one entry per accepted UseAbility cast,
    /// carrying resolved targeting and cast-time spatial snapshot.
    pub executions: AbilityExecutionStore,
    /// Follow-up eligibility windows: maps (EntityId, ability_id) → expiry tick.
    ///
    /// Written by `OpenFollowUpWindow` timeline actions in Phase 3.
    /// Phase 2 checks this to set `AbilityParams::variant` for combo presses.
    /// Phase 8 drains entries whose expiry tick ≤ current tick.
    pub active_windows: HashMap<(EntityId, u32), TickId>,
    /// Per-entity tactical flags for this tick (blocking, dodge).
    ///
    /// Written by `StanceBegin` timeline actions in Phase 3.
    /// Phase 6 checks flags before routing hits through the damage path.
    /// Phase 8 clears all flags so `StanceBegin` must re-assert each tick.
    pub tactical: Vec<TacticalState>,
}

/// Buff/debuff status data.
pub struct StatusState {
    /// Active buffs/debuffs per entity (empty Vec for entities without buffs).
    pub buffs: Vec<Vec<ActiveBuff>>,
}

/// NPC AI data.
pub struct AiState {
    /// NPC AI state — `Some` for NPCs/bosses, `None` for other entity kinds.
    pub npc_ai: Vec<Option<NpcAiState>>,
    /// Spawn position used as the patrol home point for NPCs/bosses.
    /// Non-NPC slots hold `Vec3f::ZERO` and are never read.
    pub home_positions: Vec<Vec3f>,
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
                threat_tables: Vec::new(),
                hitboxes: HitboxStore::new(),
                executions: AbilityExecutionStore::new(),
                active_windows: HashMap::new(),
                tactical: Vec::new(),
            },
            status: StatusState { buffs: Vec::new() },
            ai: AiState { npc_ai: Vec::new(), home_positions: Vec::new() },
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
        debug_assert_eq!(self.combat.threat_tables.len(), n, "threat_tables desync");
        debug_assert_eq!(self.combat.tactical.len(), n, "tactical desync");
        debug_assert_eq!(self.status.buffs.len(), n, "buffs desync");
        debug_assert_eq!(self.ai.npc_ai.len(), n, "npc_ai desync");
        debug_assert_eq!(self.ai.home_positions.len(), n, "home_positions desync");
    }

    /// Register a new entity, returning its dense index.
    pub fn spawn_entity(
        &mut self,
        id: EntityId,
        kind: EntityKind,
        tick: TickId,
        max_hp: f32,
    ) -> EntityIndex {
        let idx = self.entities.spawn(id, kind, tick);
        // Push component slots in lockstep with the entity store.
        self.combat.health.push(max_hp);
        self.status.buffs.push(Vec::new());
        let is_npc = kind == EntityKind::Npc || kind == EntityKind::Boss;
        self.combat.threat_tables.push(if is_npc { Some(ThreatTable::default()) } else { None });
        self.ai.npc_ai.push(if is_npc { Some(NpcAiState::Idle) } else { None });
        self.ai.home_positions.push(Vec3f::ZERO);
        self.combat.tactical.push(TacticalState::default());
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

    /// Tombstone-remove: mark Removed, drop id mapping, clean up hitboxes.
    /// Returns true if the entity existed.
    pub fn remove_entity(&mut self, id: EntityId) -> bool {
        if let Some(idx) = self.entities.lookup(id) {
            self.entities.mark_removed(idx);
            self.combat.hitboxes.remove_all_for_entity(id);
            self.combat.executions.remove_all_for_caster(id);
            true
        } else {
            false
        }
    }

    /// Get all entity ids currently in DespawnPending state.
    pub fn despawn_pending(&self) -> Vec<EntityId> {
        (0..self.entities.len())
            .filter(|&i| self.entities.states[i] == EntityState::DespawnPending)
            .map(|i| self.entities.id_of(EntityIndex(i as u32)))
            .collect()
    }

    /// Get indices of all active entities of a given kind.
    pub fn active_indices_of_kind(&self, kind: EntityKind) -> Vec<EntityIndex> {
        (0..self.entities.len())
            .filter(|&i| {
                self.entities.kinds[i] == kind
                    && self.entities.states[i] == EntityState::Active
            })
            .map(|i| EntityIndex(i as u32))
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
    pub fn expire_buffs(&mut self, current_tick: TickId) -> Vec<(EntityId, u32)> {
        let mut expired = Vec::new();
        for i in 0..self.entities.len() {
            if self.entities.states[i] == EntityState::Removed {
                continue;
            }
            let entity_id = self.entities.id_of(EntityIndex(i as u32));
            self.status.buffs[i].retain(|b| {
                if let Some(expires_at) = b.expires_at {
                    if expires_at <= current_tick {
                        expired.push((entity_id, b.buff_id));
                        return false;
                    }
                }
                true
            });
        }
        expired
    }

    // ── Convenience accessors (EntityId → lookup → dense array) ─

    /// Check if an entity is in the Active state.
    pub fn is_active(&self, id: EntityId) -> bool {
        self.entities.lookup(id)
            .map_or(false, |idx| self.entities.is_active(idx))
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
            .and_then(|idx| self.combat.threat_tables[idx.as_usize()].as_ref())
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
        assert!(state.combat.threat_tables[idx.as_usize()].is_none());
    }

    #[test]
    fn npc_gets_threat_table_and_ai() {
        let mut state = SimState::new();
        let idx = state.spawn_entity(eid(10), EntityKind::Npc, TickId(0), 500.0);

        assert!(state.combat.threat_tables[idx.as_usize()].is_some());
        assert_eq!(state.ai.npc_ai[idx.as_usize()], Some(NpcAiState::Idle));
    }

    #[test]
    fn health_damage_and_healing() {
        let mut store = HealthStore::new();
        store.push(100.0);
        let idx = EntityIndex(0);

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
        let idx = EntityIndex(0);

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
        let idx = EntityIndex(0);
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
    fn expire_buffs_by_tick() {
        let mut state = SimState::new();
        let idx = state.spawn_entity(eid(1), EntityKind::Player, TickId(0), 100.0);
        state.status.buffs[idx.as_usize()].push(ActiveBuff {
            buff_id: 42,
            source: eid(99),
            target: eid(1),
            stacks: 1,
            max_stacks: 1,
            expires_at: Some(TickId(10)),
            modifiers: Default::default(),
        });
        state.status.buffs[idx.as_usize()].push(ActiveBuff {
            buff_id: 43,
            source: eid(99),
            target: eid(1),
            stacks: 1,
            max_stacks: 1,
            expires_at: None, // permanent
            modifiers: Default::default(),
        });

        let expired = state.expire_buffs(TickId(10));
        assert_eq!(expired, vec![(eid(1), 42)]);
        // Permanent buff remains
        assert_eq!(state.status.buffs[idx.as_usize()].len(), 1);
        assert_eq!(state.status.buffs[idx.as_usize()][0].buff_id, 43);
    }

    // ── Ownership enforcement guardrail tests ───────────────────

    #[test]
    fn ownership_allows_valid_triples() {
        // Verify that every documented ownership triple passes.
        assert!(is_ownership_allowed(AuditDomain::Health, AuditSubsystem::Combat, 6));
        assert!(is_ownership_allowed(AuditDomain::Threat, AuditSubsystem::Combat, 6));
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
