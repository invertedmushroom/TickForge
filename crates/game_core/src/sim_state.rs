use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::{EntityKind, EntityState, NpcAiState};

use crate::combat::hitbox::HitboxStore;
use crate::combat::skill::AbilityExecutionStore;
use crate::combat::status::{ActiveBuff, ThreatTable};
use crate::entity::entity_index::EntityIndex;
use crate::entity::entity_store::EntityStore;
use crate::physics_backend::CollisionEvent;

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
    pub fn apply_damage(&mut self, idx: EntityIndex, amount: f32, source: Option<EntityId>) -> f32 {
        let i = idx.as_usize();
        let actual = amount.min(self.hp[i]);
        self.hp[i] -= actual;
        if actual > 0.0 && source.is_some() {
            self.last_damage_source[i] = source;
        }
        actual
    }

    /// Apply healing, clamping to max. Returns actual healing done.
    pub fn apply_healing(&mut self, idx: EntityIndex, amount: f32) -> f32 {
        let i = idx.as_usize();
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
            },
            status: StatusState { buffs: Vec::new() },
            ai: AiState { npc_ai: Vec::new() },
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
        debug_assert_eq!(self.status.buffs.len(), n, "buffs desync");
        debug_assert_eq!(self.ai.npc_ai.len(), n, "npc_ai desync");
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
        });
        state.status.buffs[idx.as_usize()].push(ActiveBuff {
            buff_id: 43,
            source: eid(99),
            target: eid(1),
            stacks: 1,
            max_stacks: 1,
            expires_at: None, // permanent
        });

        let expired = state.expire_buffs(TickId(10));
        assert_eq!(expired, vec![(eid(1), 42)]);
        // Permanent buff remains
        assert_eq!(state.status.buffs[idx.as_usize()].len(), 1);
        assert_eq!(state.status.buffs[idx.as_usize()][0].buff_id, 43);
    }
}
