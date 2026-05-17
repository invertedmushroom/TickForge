use serde::{Deserialize, Serialize};

/// Damage classification for combat events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum DamageType {
    Physical,
    Magical,
    True,
}
