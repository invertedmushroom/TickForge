//! NPC archetype registry — name → spawn profile lookup used when an
//! encounter rule emits `EncounterOutput::SpawnAdds { archetype, count }`.
//!
//! The checked-in roster is authored in `data/npc_archetypes.ron`. Validation
//! lives in `game_schema::npc_archetype::NpcArchetypeFile::parse_and_validate`
//! so the server reducer and the simulation worker reject the same authoring
//! bugs from the same code path. This module intentionally keeps the schema
//! small and enum-backed so authoring changes can alter shipped adds without
//! adding a scripting layer.

use std::collections::HashMap;

use super::EntityKind;
use game_schema::npc_archetype::NpcArchetypeFile;

pub use game_schema::npc_archetype::NpcArchetype;

pub(crate) trait NpcArchetypeDirectorConfigExt {
    fn director_npc_config(&self) -> Option<game_core::director::DirectorNpcConfig>;
}

impl NpcArchetypeDirectorConfigExt for NpcArchetype {
    fn director_npc_config(&self) -> Option<game_core::director::DirectorNpcConfig> {
        if self.kind != EntityKind::Npc && self.kind != EntityKind::Boss {
            return None;
        }
        Some(game_core::director::DirectorNpcConfig {
            passive: false,
            no_chase: false,
            ability_ids: self.ability_ids.clone(),
            leash_radius: 30.0,
            aggro_radius: 15.0,
            body_shape: self.body_shape.map(|shape| shape.to_u8()),
        })
    }
}

/// Registry of named archetypes. Owned by `TickPipeline`; populated at
/// startup with `data/npc_archetypes.ron` and extensible via `register`.
#[derive(Debug)]
pub struct NpcArchetypeRegistry {
    archetypes: HashMap<String, NpcArchetype>,
}

impl NpcArchetypeRegistry {
    pub fn new() -> Self {
        Self {
            archetypes: HashMap::new(),
        }
    }

    /// Default roster loaded from `data/npc_archetypes.ron`.
    ///
    /// On parse/validation failure the registry is left **empty**, not
    /// populated with a hardcoded fallback. Server-side
    /// `load_npc_archetypes` errors out on the same input, so the two
    /// processes never silently disagree about which archetypes exist. The
    /// worker still tolerates the empty case — `register_encounter_add_with_archetype` and
    /// `apply_npc_archetype_to_entity` warn and skip when an archetype
    /// is missing — so a content bug degrades to "no add profile
    /// installed" rather than worker crash.
    pub fn with_builtins() -> Self {
        Self::from_ron(include_str!("../../../../data/npc_archetypes.ron")).unwrap_or_else(|err| {
            log::warn!(
                "data/npc_archetypes.ron rejected by shared validator: {err}; \
                 worker registry will be empty (server reducer fails the same way)"
            );
            Self::new()
        })
    }

    pub fn from_ron(src: &str) -> Result<Self, String> {
        let file = NpcArchetypeFile::parse_and_validate(src)?;
        let mut reg = Self::new();
        for (name, archetype) in file.archetypes {
            reg.register(&name, archetype);
        }
        Ok(reg)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shipped_npc_archetypes_ron() {
        let registry =
            NpcArchetypeRegistry::from_ron(include_str!("../../../../data/npc_archetypes.ron"))
                .expect("data/npc_archetypes.ron should parse");
        let minion = registry
            .lookup("warden_minion")
            .expect("warden_minion archetype should exist");
        assert_eq!(minion.kind, EntityKind::Npc);
        assert_eq!(minion.team_id, Some(2));
        assert_eq!(
            minion.behavior_tree_id.as_deref(),
            Some("scripted_goal_idle")
        );
        assert_eq!(minion.default_goal.as_deref(), Some("go_idle"));
    }

    #[test]
    fn worker_delegates_validation_to_shared_schema() {
        // Same authoring bug as `parse_and_validate` rejects centrally.
        // Worker `from_ron` must surface the same error string.
        let err = NpcArchetypeRegistry::from_ron(
            r#"(
                archetypes: {
                    "too_many": (
                        kind: Npc,
                        max_hp: 10.0,
                        ability_ids: [1, 2, 3, 4, 5],
                    ),
                },
            )"#,
        )
        .expect_err("worker should reject the same content as the schema validator");
        assert!(err.contains("max is 4"), "unexpected error: {err}");
    }
}
