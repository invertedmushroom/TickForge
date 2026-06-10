//! Shared on-disk NPC archetype schema.
//!
//! `data/npc_archetypes.ron` is consumed by both the server reducer path
//! (durable actor rows for `BossSpawn`/`NpcSpawn`) and the simulation worker
//! (BT/route/goal/loot replay for encounter `SpawnAdds`).
//!
//! The `usage` tag on each row makes the authoring surface explicit:
//! `ActorOnly` rows are only valid for dungeon actor spawns,
//! `AddOnly` rows are only valid for encounter adds, and `Both`
//! (the default) is reachable from either surface.

use std::collections::HashMap;

use serde::Deserialize;

use crate::{dungeon::BodyShapeDef, entity::EntityKind};

/// On-disk format for `data/npc_archetypes.ron`.
#[derive(Clone, Debug, Deserialize)]
pub struct NpcArchetypeFile {
    pub archetypes: HashMap<String, NpcArchetype>,
}

/// Which authoring surfaces are allowed to reference an archetype id.
///
/// The default is `Both` so legacy RON rows continue to work. Restrict to
/// `ActorOnly` or `AddOnly` whenever a row is unsuitable for the other
/// surface (e.g. an actor-only `Boss` archetype that should never be a
/// `SpawnAdds` target, or an encounter-only minion).
#[derive(Clone, Copy, Debug, Deserialize, Default, PartialEq, Eq)]
pub enum ArchetypeUsage {
    /// Reachable from both dungeon `BossSpawn`/`NpcSpawn` and encounter
    /// `SpawnAdds`. Default for backward compatibility.
    #[default]
    Both,
    /// Dungeon `BossSpawn`/`NpcSpawn` only. Encounter `SpawnAdds` rejects.
    ActorOnly,
    /// Encounter `SpawnAdds` only. Dungeon actor spawns reject.
    AddOnly,
}

impl ArchetypeUsage {
    /// True when this archetype may be used as a dungeon actor spawn target
    /// (`BossSpawn` / `NpcSpawn` `archetype_id`).
    pub fn allows_actor_spawn(self) -> bool {
        matches!(self, ArchetypeUsage::Both | ArchetypeUsage::ActorOnly)
    }

    /// True when this archetype may be used as an encounter `SpawnAdds`
    /// target.
    pub fn allows_encounter_add(self) -> bool {
        matches!(self, ArchetypeUsage::Both | ArchetypeUsage::AddOnly)
    }
}

/// Spawn profile for an authored NPC-like actor.
#[derive(Clone, Debug, Deserialize)]
pub struct NpcArchetype {
    pub kind: EntityKind,
    pub max_hp: f32,
    #[serde(default)]
    pub body_shape: Option<BodyShapeDef>,
    #[serde(default)]
    pub team_id: Option<u32>,
    /// AI ability list copied onto the spawned actor. Empty = default loadout.
    #[serde(default)]
    pub ability_ids: Vec<u32>,
    /// Optional Behavior Tree policy id from `data/behavior_trees.ron`.
    #[serde(default)]
    pub behavior_tree_id: Option<String>,
    /// Optional authored route id from `data/npc_routes.ron`.
    #[serde(default)]
    pub route_id: Option<String>,
    /// Optional initial goal directive. This is a spawn-time content default,
    /// not a DB-owned `npc_goal` directive.
    #[serde(default)]
    pub default_goal: Option<String>,
    /// Optional loot table id from `data/loot_tables.ron`.
    #[serde(default)]
    pub loot_table_id: Option<String>,
    /// Which authoring surfaces are allowed to reference this archetype id.
    #[serde(default)]
    pub usage: ArchetypeUsage,
}

impl NpcArchetypeFile {
    /// Parse the RON source and run the shared structural validation
    /// (`validate_archetype`) on every row. Both the server reducer
    /// (durable actor row inserts) and the simulation worker
    /// (BT/route/goal/loot replay) must use this entry point so they
    /// agree on which content rows are valid before either side acts on
    /// them. Returns the parsed file with trimmed ids on success, or the
    /// first validation error encountered.
    pub fn parse_and_validate(src: &str) -> Result<Self, String> {
        let file: NpcArchetypeFile =
            ron::from_str(src).map_err(|err| format!("npc_archetypes.ron parse error: {err}"))?;
        let mut validated = HashMap::with_capacity(file.archetypes.len());
        for (name, archetype) in file.archetypes {
            let key = name.trim();
            if key.is_empty() {
                return Err("npc archetype id must not be empty".to_string());
            }
            validate_archetype(key, &archetype)?;
            validated.insert(key.to_string(), archetype);
        }
        Ok(NpcArchetypeFile {
            archetypes: validated,
        })
    }
}

/// Shared structural validator for a single NPC archetype row. Enforces
/// kind/hp/ability-cap and the non-empty-when-set rule for string id
/// references. Cross-file id existence checks (BT id, route id, loot id,
/// ability id) are not done here — that is the job of
/// `cargo xtask dev content-check`. Both the server reducer and the
/// simulation worker call this so they reject the same authoring bugs.
pub fn validate_archetype(key: &str, archetype: &NpcArchetype) -> Result<(), String> {
    if archetype.kind != EntityKind::Npc && archetype.kind != EntityKind::Boss {
        return Err(format!(
            "npc archetype '{key}' kind must be Npc or Boss; got {:?}",
            archetype.kind
        ));
    }
    if !archetype.max_hp.is_finite() || archetype.max_hp <= 0.0 {
        return Err(format!("npc archetype '{key}' max_hp must be positive"));
    }
    if archetype.ability_ids.len() > 4 {
        return Err(format!(
            "npc archetype '{key}' has {} ability_ids; max is 4",
            archetype.ability_ids.len()
        ));
    }
    if archetype
        .behavior_tree_id
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Err(format!(
            "npc archetype '{key}' behavior_tree_id must not be empty"
        ));
    }
    if archetype.route_id.as_deref().is_some_and(str::is_empty) {
        return Err(format!("npc archetype '{key}' route_id must not be empty"));
    }
    if archetype.default_goal.as_deref().is_some_and(str::is_empty) {
        return Err(format!(
            "npc archetype '{key}' default_goal must not be empty"
        ));
    }
    if archetype
        .loot_table_id
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Err(format!(
            "npc archetype '{key}' loot_table_id must not be empty"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_more_than_four_ability_ids() {
        let err = NpcArchetypeFile::parse_and_validate(
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
        .expect_err("ability id cap should be enforced");
        assert!(err.contains("max is 4"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_non_npc_non_boss_kind() {
        let err = NpcArchetypeFile::parse_and_validate(
            r#"(
                archetypes: {
                    "propish": (
                        kind: Prop,
                        max_hp: 1.0,
                    ),
                },
            )"#,
        )
        .expect_err("only Npc/Boss are valid archetype kinds");
        assert!(
            err.contains("kind must be Npc or Boss"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_non_positive_max_hp() {
        let err = NpcArchetypeFile::parse_and_validate(
            r#"(
                archetypes: {
                    "ghost": (
                        kind: Npc,
                        max_hp: 0.0,
                    ),
                },
            )"#,
        )
        .expect_err("max_hp must be positive");
        assert!(
            err.contains("max_hp must be positive"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn accepts_well_formed_file() {
        let file = NpcArchetypeFile::parse_and_validate(
            r#"(
                archetypes: {
                    "ok": (
                        kind: Npc,
                        max_hp: 100.0,
                        ability_ids: [1, 2],
                        behavior_tree_id: Some("basic_melee_combat"),
                        usage: AddOnly,
                    ),
                },
            )"#,
        )
        .expect("well-formed RON should parse");
        let archetype = file.archetypes.get("ok").expect("'ok' archetype present");
        assert_eq!(archetype.kind, EntityKind::Npc);
        assert_eq!(archetype.usage, ArchetypeUsage::AddOnly);
    }
}
