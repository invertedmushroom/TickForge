//! Data-driven spawn rules schema (Phase 7.5).
//!
//! A `SpawnRule` declares what to spawn, where, when, and how it scales with
//! player count / level.  Rules are authored in `data/spawn_rules.ron`, loaded
//! by `game_core::spawn_rules::SpawnRulesRegistry`, and translated into
//! `game_core::director::DynamicEvent`s by the simulation worker at startup
//! (open world) or on `instance.on_insert` (dungeons).
//!
//! This module defines only the on-disk schema — no runtime logic.  Pure
//! serde; no `SpacetimeType` derives because rules never cross the WASM
//! boundary.

use serde::{Deserialize, Serialize};

use crate::entity::EntityKind;

/// On-disk format for `data/spawn_rules.ron`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpawnFile {
    pub rules: Vec<SpawnRule>,
}

/// A single data-driven spawn rule.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpawnRule {
    /// Stable identifier used for logging and test assertions.
    pub rule_id: String,
    /// Where in the world this rule applies.
    pub scope: SpawnScope,
    /// Condition under which spawns fire.
    pub trigger: SpawnTrigger,
    /// What to spawn when the trigger evaluates true.
    pub spawns: Vec<SpawnEntityDef>,
    /// Minimum ticks between activations (cooldown).
    #[serde(default)]
    pub cooldown_ticks: u64,
    /// Maximum number of activations; `0` means unlimited.
    #[serde(default)]
    pub max_activations: u32,
    /// Optional per-player / per-level scaling applied at resolution time.
    #[serde(default)]
    pub scaling: Option<SpawnScaling>,
}

/// Where a `SpawnRule` applies.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SpawnScope {
    /// Open-world region cell at `layer = 0`.
    OpenWorldCell { rx: i32, rz: i32 },
    /// Every open-world cell within the inclusive bounding box at `layer = 0`.
    OpenWorldAny {
        rx_min: i32,
        rx_max: i32,
        rz_min: i32,
        rz_max: i32,
    },
    /// Dungeon instance of the given template.  Bound to the instance's
    /// layer and to region `(0, 0)` (instance-normalized zone key).
    Dungeon { template_id: String },
}

/// Condition under which a `SpawnRule` fires.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum SpawnTrigger {
    /// Fires when the zone's `world_phase` matches `phase_name`.
    WorldPhase { phase_name: String },
    /// Fires when the region's active player count meets the threshold.
    PlayerCountAtLeast { threshold: u32 },
    /// Fires when a `world_activity_event` row tagged `tag` is in state
    /// `Active` in the rule's region scope AND at least `min_players`
    /// active (non-disconnected) players are in the region. Preferred
    /// over bare `WorldPhase` for actor-spawning rules, because the
    /// `world_activity_event` row carries the authored
    /// `required_players` value and the worker's effective region count
    /// already excludes reconnect-grace players. This avoids
    /// `WorldPhase`-driven actor spawns in offscreen cells.
    WorldActivityEventActive { tag: String, min_players: u32 },
    /// Fires when a named event is raised (reserved for future use; e.g. on
    /// instance creation or an encounter-script callback).
    OnEvent { event_name: String },
}

/// A single entity to spawn when a rule fires.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SpawnEntityDef {
    pub kind: EntityKind,
    pub max_hp: f32,
    /// Offset from the region-cell center (or dungeon anchor) in world units.
    pub offset: [f32; 3],
}

/// How spawns scale with player count and/or player level.
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct SpawnScaling {
    /// HP multiplier per extra player beyond the first.
    /// Final `max_hp = base_hp * (1 + per_player_hp_mult * (n - 1))`.
    #[serde(default)]
    pub per_player_hp_mult: f32,
    /// Additional spawn copies per extra player beyond the first.
    /// Final `count = 1 + per_player_count * (n - 1)`, floored at 1.
    #[serde(default)]
    pub per_player_count: f32,
    /// HP multiplier per level above baseline, using `level_source`.
    #[serde(default)]
    pub per_level_hp_mult: f32,
    /// Which player level drives `per_level_hp_mult`.
    #[serde(default)]
    pub level_source: LevelSource,
}

/// Which level value drives level-based scaling.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Default, PartialEq, Eq)]
pub enum LevelSource {
    /// No level scaling (default).
    #[default]
    None,
    /// Highest level among active players in the region.
    HighestPlayer,
    /// Lowest level among active players in the region.
    LowestPlayer,
    /// Arithmetic mean of active player levels in the region.
    AveragePlayer,
}
