use game_schema::EntityKind;

/// What kind of zone a region represents.
///
/// Determines collision/repulsion rules for entities within the region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum RegionType {
    /// Open world: only player↔NPC repulsion (players pass through each other).
    #[default]
    OpenWorld,
    /// Dungeon/instance: both player↔player and player↔NPC repulsion active.
    Dungeon,
    /// Arena/PvP: no character repulsion at all.
    Arena,
}

/// Per-region rules for soft character-body repulsion.
///
/// Determines which entity-kind pairs push each other apart when overlapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RepulsionRules {
    pub player_player: bool,
    pub player_npc: bool,
    pub npc_npc: bool,
}

impl RepulsionRules {
    pub fn for_region(region_type: RegionType) -> Self {
        match region_type {
            RegionType::OpenWorld => Self {
                player_player: false,
                player_npc: true,
                npc_npc: false,
            },
            RegionType::Dungeon => Self {
                player_player: true,
                player_npc: true,
                npc_npc: false,
            },
            RegionType::Arena => Self {
                player_player: false,
                player_npc: false,
                npc_npc: false,
            },
        }
    }

    /// Returns true if entities of the given kinds should repulse each other.
    pub fn should_repulse(&self, kind_a: EntityKind, kind_b: EntityKind) -> bool {
        match (kind_a, kind_b) {
            (EntityKind::Player, EntityKind::Player) => self.player_player,
            (EntityKind::Player, EntityKind::Npc | EntityKind::Boss)
            | (EntityKind::Npc | EntityKind::Boss, EntityKind::Player) => self.player_npc,
            (EntityKind::Npc | EntityKind::Boss, EntityKind::Npc | EntityKind::Boss) => {
                self.npc_npc
            }
            _ => false, // Props and other kinds don't participate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_world_rules() {
        let rules = RepulsionRules::for_region(RegionType::OpenWorld);
        assert!(!rules.should_repulse(EntityKind::Player, EntityKind::Player));
        assert!(rules.should_repulse(EntityKind::Player, EntityKind::Npc));
        assert!(rules.should_repulse(EntityKind::Npc, EntityKind::Player));
        assert!(rules.should_repulse(EntityKind::Player, EntityKind::Boss));
        assert!(!rules.should_repulse(EntityKind::Npc, EntityKind::Npc));
    }

    #[test]
    fn dungeon_rules() {
        let rules = RepulsionRules::for_region(RegionType::Dungeon);
        assert!(rules.should_repulse(EntityKind::Player, EntityKind::Player));
        assert!(rules.should_repulse(EntityKind::Player, EntityKind::Npc));
        assert!(!rules.should_repulse(EntityKind::Npc, EntityKind::Npc));
    }

    #[test]
    fn arena_rules() {
        let rules = RepulsionRules::for_region(RegionType::Arena);
        assert!(!rules.should_repulse(EntityKind::Player, EntityKind::Player));
        assert!(!rules.should_repulse(EntityKind::Player, EntityKind::Npc));
        assert!(!rules.should_repulse(EntityKind::Npc, EntityKind::Npc));
    }

    #[test]
    fn props_never_repulse() {
        let rules = RepulsionRules::for_region(RegionType::Dungeon);
        assert!(!rules.should_repulse(EntityKind::Prop, EntityKind::Player));
        assert!(!rules.should_repulse(EntityKind::Player, EntityKind::Prop));
    }
}
