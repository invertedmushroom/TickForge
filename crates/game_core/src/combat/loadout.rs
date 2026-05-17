use serde::{Deserialize, Serialize};

/// Two weapon sets with an active index. Player-only component.
///
/// Each weapon set carries a list of ability IDs available while that set is active.
/// `active` is 0 or 1. Phase 2 validates `UseAbility` requests against the active
/// set's abilities. Weapon swap toggles the index and emits `WeaponSwapped`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeaponLoadout {
    /// Ability IDs available in weapon set 0.
    pub set_0: Vec<u32>,
    /// Ability IDs available in weapon set 1.
    pub set_1: Vec<u32>,
    /// Active weapon set index (0 or 1).
    pub active: u8,
}

impl WeaponLoadout {
    /// Create a new loadout with two ability sets.
    pub fn new(set_0: Vec<u32>, set_1: Vec<u32>) -> Self {
        Self {
            set_0,
            set_1,
            active: 0,
        }
    }

    /// Return the ability IDs for the currently active weapon set.
    pub fn active_abilities(&self) -> &[u32] {
        if self.active == 0 {
            &self.set_0
        } else {
            &self.set_1
        }
    }

    /// Return `true` if `ability_id` is in the currently active weapon set.
    pub fn is_ability_available(&self, ability_id: u32) -> bool {
        self.active_abilities().contains(&ability_id)
    }

    /// Toggle the active set (0→1 or 1→0). Returns the new active index.
    pub fn swap(&mut self) -> u8 {
        self.active = 1 - self.active;
        self.active
    }
}

impl Default for WeaponLoadout {
    fn default() -> Self {
        Self {
            set_0: Vec::new(),
            set_1: Vec::new(),
            active: 0,
        }
    }
}
