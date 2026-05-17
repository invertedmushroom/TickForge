use serde::{Deserialize, Serialize};

/// Equipment slot classification for player gear.
///
/// Each player entity has at most one item per slot. The `player_equipment`
/// DB table uses this enum as the slot discriminator. Used by both the
/// SpacetimeDB WASM module and native crates (simulation worker, client).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum EquipmentSlot {
    Weapon,
    OffHand,
    Helmet,
    Chest,
    Legs,
    Boots,
    Gloves,
    Ring1,
    Ring2,
    Amulet,
}
