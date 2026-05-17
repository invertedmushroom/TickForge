//! NPC archetype registry — name → spawn profile lookup used when an
//! encounter rule emits `EncounterOutput::SpawnAdds { archetype, count }`.
//!
//! Step 2 only needs a small fixed roster of built-ins so existing scripts
//! can call `SpawnAdds(archetype: "trash"/"elite"/"boss", count: N)`. Future
//! work can drive this from `data/spawn_rules.ron` or a dedicated
//! `archetypes.ron`.

use std::collections::HashMap;

use super::EntityKind;

/// Spawn profile for a scripted add.
#[derive(Clone, Debug)]
pub struct NpcArchetype {
    pub kind: EntityKind,
    pub max_hp: f32,
    /// AI ability list copied onto the spawned NPC. Empty = inherits default.
    pub ability_ids: Vec<u32>,
}

/// Registry of named archetypes. Owned by `TickPipeline`; populated at
/// startup with `with_builtins()` and extensible via `register`.
pub struct NpcArchetypeRegistry {
    archetypes: HashMap<String, NpcArchetype>,
}

impl NpcArchetypeRegistry {
    pub fn new() -> Self {
        Self {
            archetypes: HashMap::new(),
        }
    }

    /// Default roster used until data-driven archetypes land.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(
            "trash",
            NpcArchetype {
                kind: EntityKind::Npc,
                max_hp: 100.0,
                ability_ids: Vec::new(),
            },
        );
        reg.register(
            "elite",
            NpcArchetype {
                kind: EntityKind::Npc,
                max_hp: 350.0,
                ability_ids: Vec::new(),
            },
        );
        reg.register(
            "boss",
            NpcArchetype {
                kind: EntityKind::Boss,
                max_hp: 1200.0,
                ability_ids: Vec::new(),
            },
        );
        reg.register(
            "prop",
            NpcArchetype {
                kind: EntityKind::Prop,
                max_hp: 1.0,
                ability_ids: Vec::new(),
            },
        );
        // Encounter-specific roster used by the shipped scripts in
        // `data/encounters.ron`. Keeping these here avoids a "missing
        // archetype" runtime warning until a dedicated archetypes RON
        // pipeline lands.
        reg.register(
            "warden_minion",
            NpcArchetype {
                kind: EntityKind::Npc,
                max_hp: 80.0,
                ability_ids: Vec::new(),
            },
        );
        reg
    }

    pub fn register(&mut self, name: &str, archetype: NpcArchetype) {
        self.archetypes.insert(name.to_string(), archetype);
    }

    pub fn lookup(&self, name: &str) -> Option<&NpcArchetype> {
        self.archetypes.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.archetypes.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.archetypes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.archetypes.is_empty()
    }
}

impl Default for NpcArchetypeRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}
