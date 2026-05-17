use std::collections::HashMap;
use game_schema::EntityKind;
use serde::{Deserialize, Serialize};

use crate::combat::status::ActiveBuff;
use crate::entity::entity_index::EntityIndex;

/// Authoritative per-entity-kind base movement speed (units/second).
///
/// These are the canonical speeds before any buff modifier is applied.
/// `apply_movement` in the tick pipeline multiplies this by `speed_pct`
/// from active `BuffModifiers` to produce the effective speed.
///
/// | Kind       | Speed | Rationale                                             |
/// |------------|-------|-------------------------------------------------------|
/// | Player     | 5.0   | Baseline feel; matches previous hardcoded value        |
/// | Npc        | 3.5   | Slower than player to allow kiting                    |
/// | Boss       | 2.5   | Bosses are slow but hit hard                          |
/// | Projectile | 12.0  | Projectiles move fast; kinematics override per ability |
/// | Hazard     | 0.0   | Hazards are stationary by default                     |
pub fn base_speed(kind: EntityKind) -> f32 {
    match kind {
        EntityKind::Player    => 5.0,
        EntityKind::Npc       => 3.5,
        EntityKind::Boss      => 2.5,
        EntityKind::Projectile => 12.0,
        EntityKind::Hazard    => 0.0,
    }
}

/// Base attack power per entity kind.
pub fn base_attack_power(kind: EntityKind) -> f32 {
    match kind {
        EntityKind::Player     => 1.0,
        EntityKind::Npc        => 1.0,
        EntityKind::Boss       => 1.5,
        EntityKind::Projectile => 1.0,
        EntityKind::Hazard     => 1.0,
    }
}

/// Base max HP per entity kind. Used as a fallback when no explicit max_hp is
/// provided at spawn — the spawn-time `max_hp` parameter takes precedence.
pub fn base_max_hp(kind: EntityKind) -> f32 {
    match kind {
        EntityKind::Player     => 100.0,
        EntityKind::Npc        => 80.0,
        EntityKind::Boss       => 500.0,
        EntityKind::Projectile => 1.0,
        EntityKind::Hazard     => 1.0,
    }
}

// ── Equipment ───────────────────────────────────────────────────

/// Flat stat bonuses contributed by a single equipped item.
///
/// Loaded from item definitions (data/items.ron) and aggregated per entity.
/// Applied additively to base stats *before* multiplicative buff scaling:
/// `(base + gear_flat) * (1.0 + buff_pct)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EquipmentModifiers {
    pub max_hp: f32,
    pub attack_power: f32,
    pub speed: f32,
    pub damage_out: f32,
    pub damage_in: f32,
    pub cooldown_reduce: f32,
}

impl EquipmentModifiers {
    /// Combine modifiers from multiple equipped items.
    pub fn aggregate(items: impl Iterator<Item = EquipmentModifiers>) -> Self {
        let mut result = Self::default();
        for m in items {
            result.max_hp += m.max_hp;
            result.attack_power += m.attack_power;
            result.speed += m.speed;
            result.damage_out += m.damage_out;
            result.damage_in += m.damage_in;
            result.cooldown_reduce += m.cooldown_reduce;
        }
        result
    }
}

/// Static item definition loaded from data/items.ron.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemData {
    pub item_id: u32,
    pub name: String,
    pub modifiers: EquipmentModifiers,
}

/// Registry mapping item_id → static item data.
#[derive(Clone, Debug, Default)]
pub struct ItemRegistry {
    items: HashMap<u32, ItemData>,
}

impl ItemRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, data: ItemData) {
        self.items.insert(data.item_id, data);
    }

    pub fn get(&self, item_id: u32) -> Option<&ItemData> {
        self.items.get(&item_id)
    }

    /// Look up an item and return its modifiers, or `Default` if unknown.
    pub fn modifiers(&self, item_id: u32) -> EquipmentModifiers {
        self.items.get(&item_id).map_or(EquipmentModifiers::default(), |d| d.modifiers)
    }
}

// ── StatBlock ───────────────────────────────────────────────────

/// Cached per-entity derived stats.
///
/// Flattened from: `(Base + GearFlat) * (1.0 + BuffPct)`.
/// Recalculated when buffs or equipment change (dirty flag).
/// Pipeline phases read these instead of iterating buffs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatBlock {
    /// Effective max HP after all modifiers.
    pub max_hp: f32,
    /// Effective movement speed (world units / second).
    pub movement_speed: f32,
    /// Outgoing damage multiplier (1.0 = baseline, 1.5 = +50%).
    pub damage_out_mult: f32,
    /// Incoming damage multiplier (1.0 = baseline, 0.8 = −20% taken).
    pub damage_in_mult: f32,
    /// Cooldown reduction fraction in [0, 0.99]. 0.2 = 20% shorter cooldowns.
    pub cooldown_reduce_pct: f32,
    /// Attack power multiplier applied to ability base_damage.
    pub attack_power: f32,
}

impl Default for StatBlock {
    fn default() -> Self {
        Self {
            max_hp: 100.0,
            movement_speed: 5.0,
            damage_out_mult: 1.0,
            damage_in_mult: 1.0,
            cooldown_reduce_pct: 0.0,
            attack_power: 1.0,
        }
    }
}

impl StatBlock {
    /// Compute a stat block from entity kind, spawn-time max_hp, active buffs, and equipment.
    ///
    /// Formula: `(base + gear_flat) * (1.0 + sum(buff_pct_modifiers))`, clamped to sane ranges.
    pub fn compute(kind: EntityKind, spawn_max_hp: f32, buffs: &[ActiveBuff], equip: &EquipmentModifiers) -> Self {
        let speed_pct: f32 = buffs.iter().filter_map(|b| b.modifiers.speed_pct).sum();
        let dmg_out: f32 = buffs.iter().filter_map(|b| b.modifiers.damage_out_pct).sum();
        let dmg_in: f32 = buffs.iter().filter_map(|b| b.modifiers.damage_in_pct).sum();
        let cd_reduce: f32 = (buffs
            .iter()
            .filter_map(|b| b.modifiers.cooldown_reduce_pct)
            .sum::<f32>()
            + equip.cooldown_reduce)
            .clamp(0.0, 0.99);

        Self {
            max_hp: spawn_max_hp + equip.max_hp,
            movement_speed: (base_speed(kind) + equip.speed) * (1.0 + speed_pct).max(0.0),
            damage_out_mult: (1.0 + equip.damage_out + dmg_out).max(0.0),
            damage_in_mult: (1.0 + equip.damage_in + dmg_in).max(0.0),
            cooldown_reduce_pct: cd_reduce,
            attack_power: base_attack_power(kind) + equip.attack_power,
        }
    }
}

// ── StatsStore ──────────────────────────────────────────────────

/// Dense array of cached `StatBlock` values, indexed by `EntityIndex`.
///
/// Parallel to `EntityStore` — one slot per entity. Recalculated in
/// Phase 1.5 for entities whose buffs or equipment changed.
pub struct StatsStore {
    blocks: Vec<StatBlock>,
}

impl StatsStore {
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Push a default stat block for a newly spawned entity.
    pub fn push(&mut self, block: StatBlock) {
        self.blocks.push(block);
    }

    /// Read a cached stat block.
    pub fn get(&self, idx: EntityIndex) -> &StatBlock {
        &self.blocks[idx.as_usize()]
    }

    /// Write a recalculated stat block.
    pub fn set(&mut self, idx: EntityIndex, block: StatBlock) {
        self.blocks[idx.as_usize()] = block;
    }

    /// Number of slots (must equal EntityStore.len()).
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combat::status::{ActiveBuff, BuffModifiers};
    use game_protocol::entity_id::EntityId;

    const NO_EQUIP: EquipmentModifiers = EquipmentModifiers {
        max_hp: 0.0, attack_power: 0.0, speed: 0.0,
        damage_out: 0.0, damage_in: 0.0, cooldown_reduce: 0.0,
    };

    fn make_buff(speed: Option<f32>, dmg_out: Option<f32>, dmg_in: Option<f32>, cd: Option<f32>) -> ActiveBuff {
        ActiveBuff {
            buff_id: 1,
            source: EntityId(0),
            target: EntityId(0),
            stacks: 1,
            max_stacks: 1,
            expires_at: None,
            modifiers: BuffModifiers {
                damage_out_pct: dmg_out,
                damage_in_pct: dmg_in,
                cooldown_reduce_pct: cd,
                speed_pct: speed,
                ai_override: None,
                root: None,
            },
        }
    }

    #[test]
    fn default_stat_block() {
        let sb = StatBlock::default();
        assert_eq!(sb.max_hp, 100.0);
        assert_eq!(sb.movement_speed, 5.0);
        assert_eq!(sb.damage_out_mult, 1.0);
        assert_eq!(sb.damage_in_mult, 1.0);
        assert_eq!(sb.attack_power, 1.0);
    }

    #[test]
    fn compute_no_buffs() {
        let sb = StatBlock::compute(EntityKind::Player, 100.0, &[], &NO_EQUIP);
        assert_eq!(sb.movement_speed, 5.0);
        assert_eq!(sb.damage_out_mult, 1.0);
        assert_eq!(sb.damage_in_mult, 1.0);
        assert_eq!(sb.cooldown_reduce_pct, 0.0);
        assert_eq!(sb.attack_power, 1.0);
    }

    #[test]
    fn compute_with_speed_buff() {
        let buffs = vec![make_buff(Some(0.2), None, None, None)];
        let sb = StatBlock::compute(EntityKind::Player, 100.0, &buffs, &NO_EQUIP);
        assert!((sb.movement_speed - 6.0).abs() < 0.001); // 5.0 * 1.2
    }

    #[test]
    fn compute_with_damage_buffs() {
        let buffs = vec![
            make_buff(None, Some(0.1), None, None),
            make_buff(None, Some(0.15), None, None),
        ];
        let sb = StatBlock::compute(EntityKind::Player, 100.0, &buffs, &NO_EQUIP);
        assert!((sb.damage_out_mult - 1.25).abs() < 0.001);
    }

    #[test]
    fn compute_damage_in_debuff() {
        let buffs = vec![make_buff(None, None, Some(-0.3), None)];
        let sb = StatBlock::compute(EntityKind::Player, 100.0, &buffs, &NO_EQUIP);
        assert!((sb.damage_in_mult - 0.7).abs() < 0.001);
    }

    #[test]
    fn compute_cooldown_reduce_clamped() {
        let buffs = vec![
            make_buff(None, None, None, Some(0.8)),
            make_buff(None, None, None, Some(0.5)),
        ];
        let sb = StatBlock::compute(EntityKind::Player, 100.0, &buffs, &NO_EQUIP);
        assert_eq!(sb.cooldown_reduce_pct, 0.99);
    }

    #[test]
    fn compute_npc_base_speed() {
        let sb = StatBlock::compute(EntityKind::Npc, 80.0, &[], &NO_EQUIP);
        assert_eq!(sb.movement_speed, 3.5);
        assert_eq!(sb.max_hp, 80.0);
    }

    #[test]
    fn store_push_get_set() {
        let mut store = StatsStore::new();
        store.push(StatBlock::default());
        assert_eq!(store.len(), 1);

        let sb = store.get(EntityIndex::dangling(0));
        assert_eq!(sb.movement_speed, 5.0);

        store.set(EntityIndex::dangling(0), StatBlock { movement_speed: 10.0, ..StatBlock::default() });
        assert_eq!(store.get(EntityIndex::dangling(0)).movement_speed, 10.0);
    }

    #[test]
    fn speed_cannot_go_negative() {
        let buffs = vec![make_buff(Some(-2.0), None, None, None)];
        let sb = StatBlock::compute(EntityKind::Player, 100.0, &buffs, &NO_EQUIP);
        assert_eq!(sb.movement_speed, 0.0);
    }
}
