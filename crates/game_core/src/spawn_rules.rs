//! Runtime registry of parsed `SpawnRule`s (Phase 7.5).
//!
//! Loaded once at worker start from `data/spawn_rules.ron`.  Provides lookups
//! the coordinator uses to populate `DirectorState`:
//!   - `for_open_world()` — all rules with `SpawnScope::OpenWorldCell` or
//!     `OpenWorldAny`, flattened into per-cell `(rx, rz, rule)` tuples.
//!   - `for_dungeon(template_id)` — all rules for a given dungeon template.
//!
//! This crate owns the registry type; the simulation worker is responsible
//! for translating rules into `DynamicEvent`s at the right lifecycle point.

pub use game_schema::spawn::*;

use crate::director::{DirectorTrigger, DynamicEvent, SpawnDirective};

/// Mirror of `server_module::reducers::synthesise_zone_id`.
///
/// Both the worker and the module key `zone_counter` and `world_phase` rows
/// by this id, so the two implementations must stay in sync.
///
/// Formula: `layer * 1_000_000 + (rx + 500) * 1_000 + (rz + 500)`.
pub fn synthesise_zone_id(layer: u32, rx: i32, rz: i32) -> u32 {
    layer * 1_000_000 + ((rx + 500) as u32) * 1_000 + (rz + 500) as u32
}

/// Translate a `SpawnRule` (+ resolved region) into a `DynamicEvent` suitable
/// for the Director.  Returns `None` for trigger variants the Director cannot
/// evaluate directly (e.g. `OnEvent` is reserved for future encounter scripts).
///
/// Zone keying convention: instance layers (>0) normalise to `(layer, 0, 0)`;
/// open-world uses the real cell with `layer = 0`.  This matches the
/// `push_death_zone_counter` convention in the server module.
pub fn rule_to_dynamic_event(
    rule: &SpawnRule,
    rx: i32,
    rz: i32,
    layer: u32,
) -> Option<DynamicEvent> {
    let zone_id = if layer == 0 {
        synthesise_zone_id(0, rx, rz)
    } else {
        synthesise_zone_id(layer, 0, 0)
    };

    let trigger = match &rule.trigger {
        SpawnTrigger::WorldPhase { phase_name } => DirectorTrigger::WorldPhase {
            zone_id,
            phase_name: phase_name.clone(),
        },
        SpawnTrigger::PlayerCountAtLeast { threshold } => DirectorTrigger::PlayerCountAtLeast {
            threshold: *threshold,
        },
        SpawnTrigger::OnEvent { .. } => return None,
    };

    let spawns = rule
        .spawns
        .iter()
        .map(|s| SpawnDirective {
            kind: s.kind,
            max_hp: s.max_hp,
            offset: s.offset,
        })
        .collect();

    Some(DynamicEvent {
        region: (rx, rz, layer),
        trigger,
        spawns,
        cooldown_ticks: rule.cooldown_ticks,
        max_activations: rule.max_activations,
        scaling: rule.scaling.clone(),
    })
}

/// Parsed registry of spawn rules, indexed for fast scope-based lookup.
#[derive(Debug, Default)]
pub struct SpawnRulesRegistry {
    rules: Vec<SpawnRule>,
}

impl SpawnRulesRegistry {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Parse a RON document (`SpawnFile`) into a registry.
    pub fn from_ron(src: &str) -> Result<Self, ron::error::SpannedError> {
        let file: SpawnFile = ron::from_str(src)?;
        Ok(Self { rules: file.rules })
    }

    /// Register a single rule.  Duplicate `rule_id`s are allowed — callers
    /// should validate at load time if uniqueness matters.
    pub fn register(&mut self, rule: SpawnRule) {
        self.rules.push(rule);
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// All rules, unfiltered (primarily for tests / diagnostics).
    pub fn all(&self) -> &[SpawnRule] {
        &self.rules
    }

    /// Yield `(rx, rz, &SpawnRule)` for every open-world cell covered by the
    /// rule set.  `OpenWorldAny` is expanded into its bounding box.  Callers
    /// (the coordinator) register one `DynamicEvent` per yielded tuple.
    pub fn for_open_world(&self) -> Vec<(i32, i32, &SpawnRule)> {
        let mut out = Vec::new();
        for rule in &self.rules {
            match &rule.scope {
                SpawnScope::OpenWorldCell { rx, rz } => {
                    out.push((*rx, *rz, rule));
                }
                SpawnScope::OpenWorldAny {
                    rx_min,
                    rx_max,
                    rz_min,
                    rz_max,
                } => {
                    for rx in *rx_min..=*rx_max {
                        for rz in *rz_min..=*rz_max {
                            out.push((rx, rz, rule));
                        }
                    }
                }
                SpawnScope::Dungeon { .. } => {}
            }
        }
        out
    }

    /// Yield all rules whose scope is `Dungeon { template_id == requested }`.
    pub fn for_dungeon<'a>(
        &'a self,
        template_id: &'a str,
    ) -> impl Iterator<Item = &'a SpawnRule> + 'a {
        self.rules.iter().filter(move |r| match &r.scope {
            SpawnScope::Dungeon { template_id: t } => t == template_id,
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_schema::EntityKind;

    const SAMPLE: &str = r#"(
        rules: [
            (
                rule_id: "open_world_boss_origin",
                scope: OpenWorldAny(rx_min: -1, rx_max: 1, rz_min: -1, rz_max: 1),
                trigger: WorldPhase(phase_name: "boss_ready"),
                spawns: [
                    (kind: Boss, max_hp: 500.0, offset: (0.0, 1.0, 0.0)),
                ],
                cooldown_ticks: 100,
                max_activations: 1,
            ),
            (
                rule_id: "arena_boss",
                scope: Dungeon(template_id: "arena_01"),
                trigger: WorldPhase(phase_name: "ready"),
                spawns: [
                    (kind: Boss, max_hp: 1000.0, offset: (0.0, 1.0, 0.0)),
                ],
                cooldown_ticks: 0,
                max_activations: 1,
                scaling: Some((
                    per_player_hp_mult: 0.35,
                    per_player_count: 0.0,
                    per_level_hp_mult: 0.0,
                    level_source: None,
                )),
            ),
        ],
    )"#;

    #[test]
    fn parses_sample_ron() {
        let reg = SpawnRulesRegistry::from_ron(SAMPLE).expect("parse ok");
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn expands_open_world_any_into_grid() {
        let reg = SpawnRulesRegistry::from_ron(SAMPLE).expect("parse ok");
        let cells = reg.for_open_world();
        // 3x3 grid from OpenWorldAny only; arena_boss is a Dungeon scope.
        assert_eq!(cells.len(), 9);
        assert!(cells.iter().any(|(rx, rz, _)| *rx == 0 && *rz == 0));
        assert!(cells.iter().any(|(rx, rz, _)| *rx == -1 && *rz == 1));
    }

    #[test]
    fn for_dungeon_filters_by_template() {
        let reg = SpawnRulesRegistry::from_ron(SAMPLE).expect("parse ok");
        let arena: Vec<_> = reg.for_dungeon("arena_01").collect();
        assert_eq!(arena.len(), 1);
        assert_eq!(arena[0].rule_id, "arena_boss");
        assert_eq!(reg.for_dungeon("does_not_exist").count(), 0);
    }

    #[test]
    fn boss_kind_round_trips() {
        let reg = SpawnRulesRegistry::from_ron(SAMPLE).expect("parse ok");
        assert_eq!(reg.all()[0].spawns[0].kind, EntityKind::Boss);
    }
}
