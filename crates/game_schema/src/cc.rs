use serde::{Deserialize, Serialize};

/// Crowd-control effect type associated with a Condition debuff.
///
/// Hard CC (Stun, Knockdown, Sleep) disables all actions.
/// Silence blocks abilities but allows movement.
/// Fear forces movement away from the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum CCEffect {
    Stun,
    Knockdown,
    Sleep,
    Silence,
    Fear,
    Knockback,
}
