use std::collections::BTreeMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use serde::{Deserialize, Serialize};

use crate::stats::ItemRegistry;

pub const DEFAULT_CLAIM_WINDOW_TICKS: u64 = 20 * 60 * 5;
pub const MAX_ELIGIBLE_CLAIMANTS: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LootTablesFile {
    pub tables: BTreeMap<String, LootTableDef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LootTableDef {
    pub rolls: u32,
    pub entries: Vec<LootEntryDef>,
    #[serde(default = "default_claim_window_ticks")]
    pub claim_window_ticks: u64,
    #[serde(default)]
    pub public: bool,
}

fn default_claim_window_ticks() -> u64 {
    DEFAULT_CLAIM_WINDOW_TICKS
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LootEntryDef {
    pub item_id: u32,
    pub weight: u32,
    pub min: u32,
    pub max: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LootRollItem {
    pub item_id: u32,
    pub quantity: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LootRollOutput {
    pub corpse_entity: EntityId,
    pub layer: u32,
    pub position: Vec3f,
    pub rolled_items: Vec<LootRollItem>,
    pub eligible_claimants: Vec<EntityId>,
    pub claim_window_ticks: u64,
}

#[derive(Clone, Debug, Default)]
pub struct LootRegistry {
    tables: BTreeMap<String, LootTableDef>,
}

impl LootRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_ron(src: &str, items: &ItemRegistry) -> Result<Self, String> {
        let file: LootTablesFile = ron::from_str(src).map_err(|e| e.to_string())?;
        let mut registry = LootRegistry::new();
        for (table_id, table) in file.tables {
            validate_table(&table_id, &table, items)?;
            registry.tables.insert(table_id, table);
        }
        Ok(registry)
    }

    pub fn register_for_tests(&mut self, table_id: impl Into<String>, table: LootTableDef) {
        self.tables.insert(table_id.into(), table);
    }

    pub fn get(&self, table_id: &str) -> Option<&LootTableDef> {
        self.tables.get(table_id)
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

pub fn roll_loot_table(
    table_id: &str,
    table: &LootTableDef,
    tick_id: TickId,
    corpse_entity: EntityId,
) -> Vec<LootRollItem> {
    if table.rolls == 0 || table.entries.is_empty() {
        return Vec::new();
    }

    let total_weight: u64 = table.entries.iter().map(|entry| entry.weight as u64).sum();
    if total_weight == 0 {
        return Vec::new();
    }

    let seed = mix_seed(tick_id.0, corpse_entity.0, stable_hash(table_id));
    let mut rng = SplitMix64::new(seed);
    let mut out = Vec::with_capacity(table.rolls as usize);

    for _ in 0..table.rolls {
        let pick = rng.next_bounded(total_weight);
        let mut cursor = 0u64;
        let Some(entry) = table.entries.iter().find(|entry| {
            cursor = cursor.saturating_add(entry.weight as u64);
            pick < cursor
        }) else {
            continue;
        };

        let min = entry.min.min(entry.max);
        let max = entry.max.max(entry.min);
        let quantity = if min == max {
            min
        } else {
            min + rng.next_bounded((max - min + 1) as u64) as u32
        };
        if quantity > 0 {
            out.push(LootRollItem {
                item_id: entry.item_id,
                quantity,
            });
        }
    }

    coalesce_items(out)
}

pub fn freeze_eligible_claimants(
    contributors: impl IntoIterator<Item = (EntityId, f32)>,
    killer: Option<EntityId>,
) -> Vec<EntityId> {
    let mut entries: Vec<(EntityId, f32)> = contributors
        .into_iter()
        .filter(|(_, contribution)| *contribution > 0.0 && contribution.is_finite())
        .collect();

    entries.sort_by(|(entity_a, contribution_a), (entity_b, contribution_b)| {
        contribution_b
            .total_cmp(contribution_a)
            .then_with(|| entity_a.0.cmp(&entity_b.0))
    });
    entries.dedup_by_key(|(entity, _)| *entity);

    let mut claimants: Vec<EntityId> = entries
        .into_iter()
        .take(MAX_ELIGIBLE_CLAIMANTS)
        .map(|(entity, _)| entity)
        .collect();

    if claimants.is_empty() {
        if let Some(killer) = killer {
            claimants.push(killer);
        }
    }

    claimants
}

fn validate_table(
    table_id: &str,
    table: &LootTableDef,
    items: &ItemRegistry,
) -> Result<(), String> {
    if table.rolls == 0 {
        return Err(format!("loot table '{table_id}' must roll at least once"));
    }
    if table.entries.is_empty() {
        return Err(format!("loot table '{table_id}' must contain entries"));
    }
    if table.entries.iter().all(|entry| entry.weight == 0) {
        return Err(format!(
            "loot table '{table_id}' must contain a positive entry weight"
        ));
    }

    for entry in &table.entries {
        if entry.min == 0 || entry.max == 0 {
            return Err(format!(
                "loot table '{table_id}' entry item_id={} must use positive quantities",
                entry.item_id
            ));
        }
        if items.get(entry.item_id).is_none() {
            return Err(format!(
                "loot table '{table_id}' references unknown item_id={}",
                entry.item_id
            ));
        }
    }

    Ok(())
}

fn coalesce_items(items: Vec<LootRollItem>) -> Vec<LootRollItem> {
    let mut by_item: BTreeMap<u32, u32> = BTreeMap::new();
    for item in items {
        let quantity = by_item.entry(item.item_id).or_insert(0);
        *quantity = quantity.saturating_add(item.quantity);
    }
    by_item
        .into_iter()
        .map(|(item_id, quantity)| LootRollItem { item_id, quantity })
        .collect()
}

fn mix_seed(tick_id: u64, corpse_entity: u64, table_hash: u64) -> u64 {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    seed ^= tick_id.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= corpse_entity.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^= table_hash.rotate_left(17);
    seed
}

fn stable_hash(input: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn next_bounded(&mut self, upper: u64) -> u64 {
        debug_assert!(upper > 0);
        self.next() % upper
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{EquipmentModifiers, ItemData, ItemRegistry};

    fn items() -> ItemRegistry {
        let mut registry = ItemRegistry::new();
        registry.register(ItemData {
            item_id: 1,
            name: "Rusty Sword".to_string(),
            modifiers: EquipmentModifiers::default(),
        });
        registry.register(ItemData {
            item_id: 2,
            name: "Leather Vest".to_string(),
            modifiers: EquipmentModifiers::default(),
        });
        registry
    }

    #[test]
    fn parses_and_validates_loot_tables() {
        let registry = LootRegistry::from_ron(
            r#"
            (
                tables: {
                    "boss": (
                        rolls: 1,
                        entries: [(item_id: 1, weight: 10, min: 1, max: 1)],
                    ),
                },
            )
            "#,
            &items(),
        )
        .expect("valid loot table");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn rejects_unknown_item_ids() {
        let err = LootRegistry::from_ron(
            r#"
            (
                tables: {
                    "boss": (
                        rolls: 1,
                        entries: [(item_id: 99, weight: 10, min: 1, max: 1)],
                    ),
                },
            )
            "#,
            &items(),
        )
        .expect_err("unknown item should reject table");
        assert!(err.contains("unknown item_id=99"));
    }

    #[test]
    fn deterministic_rolls_are_stable() {
        let table = LootTableDef {
            rolls: 3,
            entries: vec![
                LootEntryDef {
                    item_id: 1,
                    weight: 5,
                    min: 1,
                    max: 1,
                },
                LootEntryDef {
                    item_id: 2,
                    weight: 95,
                    min: 1,
                    max: 3,
                },
            ],
            claim_window_ticks: DEFAULT_CLAIM_WINDOW_TICKS,
            public: false,
        };

        let first = roll_loot_table("boss", &table, TickId(42), EntityId(7));
        let second = roll_loot_table("boss", &table, TickId(42), EntityId(7));
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn freezes_contributors_by_descending_contribution_then_entity_id() {
        let claimants = freeze_eligible_claimants(
            [
                (EntityId(5), 10.0),
                (EntityId(2), 20.0),
                (EntityId(3), 20.0),
            ],
            Some(EntityId(99)),
        );
        assert_eq!(claimants, vec![EntityId(2), EntityId(3), EntityId(5)]);
    }

    #[test]
    fn falls_back_to_killer_when_contributors_are_empty() {
        let claimants = freeze_eligible_claimants([], Some(EntityId(99)));
        assert_eq!(claimants, vec![EntityId(99)]);
    }
}
