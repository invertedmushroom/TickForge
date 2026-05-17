//! World Director — dynamic event and scaling system (Phase 7.5).
//!
//! The Director tracks player distribution across spatial regions and evaluates
//! trigger conditions each tick to spawn dynamic encounters, scale NPC density,
//! and manage world events.

use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::EntityKind;
use game_schema::spawn::SpawnScaling;
use serde::{Deserialize, Serialize};

/// Width of a spatial grid cell in world units — must match `tick_pipeline::CELL_SIZE`.
pub const REGION_CELL_SIZE: f32 = 50.0;

/// Unique identifier for a dynamic event definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub u32);

/// Per-region scaling factor derived from player density.
#[derive(Clone, Copy, Debug)]
pub struct ScalingFactor {
    /// Number of active players in this region.
    pub player_count: u32,
    /// Multiplicative spawn rate scaling (1.0 = baseline).
    pub spawn_rate: f32,
}

impl ScalingFactor {
    /// Compute scaling from raw player count.
    ///
    /// Scaling curve: 1.0 for 0-2 players, +0.25 per additional player, capped at 3.0.
    pub fn from_player_count(count: u32) -> Self {
        let spawn_rate = if count <= 2 {
            1.0
        } else {
            (1.0 + (count - 2) as f32 * 0.25).min(3.0)
        };
        Self {
            player_count: count,
            spawn_rate,
        }
    }
}

/// Condition that must be met before a dynamic event fires.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DirectorTrigger {
    /// Fires when the region player count meets or exceeds the threshold.
    PlayerCountAtLeast { threshold: u32 },
    /// Fires after a specific tick.
    AfterTick { tick: u64 },
    /// Fires when a zone's world_phase matches `phase_name`.
    WorldPhase { zone_id: u32, phase_name: String },
    /// Both conditions must be true.
    And(Box<DirectorTrigger>, Box<DirectorTrigger>),
}

impl DirectorTrigger {
    /// Evaluate the trigger against current region state.
    pub fn evaluate(
        &self,
        player_count: u32,
        current_tick: TickId,
        world_phases: &HashMap<u32, String>,
    ) -> bool {
        match self {
            Self::PlayerCountAtLeast { threshold } => player_count >= *threshold,
            Self::AfterTick { tick } => current_tick.0 >= *tick,
            Self::WorldPhase {
                zone_id,
                phase_name,
            } => world_phases.get(zone_id).map_or(false, |p| p == phase_name),
            Self::And(a, b) => {
                a.evaluate(player_count, current_tick, world_phases)
                    && b.evaluate(player_count, current_tick, world_phases)
            }
        }
    }
}

/// What spawns when a dynamic event fires.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpawnDirective {
    pub kind: EntityKind,
    pub max_hp: f32,
    /// Offset from the region center where the entity spawns.
    pub offset: [f32; 3],
}

/// A dynamic world event definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DynamicEvent {
    /// Which region cell (as `(region_x, region_z, layer)`) this event targets.
    pub region: (i32, i32, u32),
    /// Condition for spawning.
    pub trigger: DirectorTrigger,
    /// Entities to spawn when triggered.
    pub spawns: Vec<SpawnDirective>,
    /// Minimum ticks between activations (cooldown).
    pub cooldown_ticks: u64,
    /// Maximum number of activations (0 = unlimited).
    pub max_activations: u32,
    /// Optional per-player / per-level scaling applied at resolution time.
    ///
    /// When `None`, falls back to the built-in `ScalingFactor::from_player_count`
    /// curve for the spawn-count multiplier and leaves `max_hp` unchanged.
    #[serde(default)]
    pub scaling: Option<SpawnScaling>,
}

/// Runtime tracking for a registered event.
#[derive(Clone, Debug)]
pub struct EventRuntime {
    pub def: DynamicEvent,
    /// Tick when this event last fired.
    pub last_fired: Option<TickId>,
    /// How many times this event has activated.
    pub activation_count: u32,
}

/// Spawn request emitted by the director for the tick pipeline to execute.
#[derive(Clone, Debug)]
pub struct DirectorSpawn {
    pub kind: EntityKind,
    pub max_hp: f32,
    pub position: game_protocol::types::Vec3f,
    /// Target visibility layer (0 = open world, 100+ = dynamic instance).
    pub layer: u32,
}

/// World director state — owns event definitions and per-region player counts.
#[derive(Debug)]
pub struct DirectorState {
    events: HashMap<EventId, EventRuntime>,
    next_event_id: u32,
}

impl DirectorState {
    pub fn new() -> Self {
        Self {
            events: HashMap::new(),
            next_event_id: 1,
        }
    }

    /// Register a new dynamic event. Returns its `EventId`.
    pub fn register_event(&mut self, def: DynamicEvent) -> EventId {
        let id = EventId(self.next_event_id);
        self.next_event_id += 1;
        self.events.insert(
            id,
            EventRuntime {
                def,
                last_fired: None,
                activation_count: 0,
            },
        );
        id
    }

    /// Remove a dynamic event registration.
    pub fn remove_event(&mut self, id: EventId) -> bool {
        self.events.remove(&id).is_some()
    }

    /// Remove every registered event whose region layer matches.
    ///
    /// Used when a dungeon instance expires: all rules registered under that
    /// instance's layer are cleared in bulk so stale triggers don't fire
    /// against a recycled layer.
    pub fn clear_for_layer(&mut self, layer: u32) -> usize {
        let before = self.events.len();
        self.events.retain(|_, rt| rt.def.region.2 != layer);
        before - self.events.len()
    }

    /// Evaluate all registered events against current region player counts.
    ///
    /// Returns spawn directives for the tick pipeline to execute. Updates
    /// internal cooldown / activation tracking for events that fire.
    pub fn evaluate(
        &mut self,
        region_player_counts: &HashMap<(i32, i32, u32), u32>,
        current_tick: TickId,
        world_phases: &HashMap<u32, String>,
    ) -> Vec<DirectorSpawn> {
        let mut spawns = Vec::new();

        for (_id, rt) in self.events.iter_mut() {
            // Check max activations.
            if rt.def.max_activations > 0 && rt.activation_count >= rt.def.max_activations {
                continue;
            }
            // Check cooldown.
            if let Some(last) = rt.last_fired {
                if current_tick.0 < last.0 + rt.def.cooldown_ticks {
                    continue;
                }
            }

            let player_count = region_player_counts
                .get(&rt.def.region)
                .copied()
                .unwrap_or(0);

            if rt
                .def
                .trigger
                .evaluate(player_count, current_tick, world_phases)
            {
                let (rx, rz, layer) = rt.def.region;
                let center_x = (rx as f32 + 0.5) * REGION_CELL_SIZE;
                let center_z = (rz as f32 + 0.5) * REGION_CELL_SIZE;

                // Resolve count & hp multipliers from the rule's optional
                // scaling, falling back to the built-in density curve.
                let (count, hp_mult) = match &rt.def.scaling {
                    Some(s) => {
                        let extras = player_count.saturating_sub(1) as f32;
                        let count = (1.0 + s.per_player_count * extras).round().max(1.0) as u32;
                        let hp_mult = 1.0 + s.per_player_hp_mult * extras;
                        // Note: per_level_hp_mult / level_source are accepted
                        // in the schema but not yet applied — player levels
                        // are not tracked in the sim state yet.
                        (count, hp_mult)
                    }
                    None => {
                        let sf = ScalingFactor::from_player_count(player_count);
                        (sf.spawn_rate.round().max(1.0) as u32, 1.0)
                    }
                };

                for directive in &rt.def.spawns {
                    // Props are indestructible by contract — the tick
                    // pipeline excludes them from the death sweep. Any
                    // finite max_hp authored in a spawn rule would leave
                    // a damaged prop stuck at 0 HP (no despawn, no
                    // cleanup). Force the indestructible sentinel so
                    // `apply_damage` cannot drive them below 1.0.
                    let max_hp = if directive.kind == EntityKind::Prop {
                        f32::MAX
                    } else {
                        directive.max_hp * hp_mult
                    };
                    for _ in 0..count {
                        spawns.push(DirectorSpawn {
                            kind: directive.kind,
                            max_hp,
                            position: game_protocol::types::Vec3f {
                                x: center_x + directive.offset[0],
                                y: directive.offset[1],
                                z: center_z + directive.offset[2],
                            },
                            layer,
                        });
                    }
                }

                rt.last_fired = Some(current_tick);
                rt.activation_count += 1;
            }
        }

        spawns
    }

    /// Number of registered events.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Iterate over registered event IDs.
    pub fn event_ids(&self) -> impl Iterator<Item = EventId> + '_ {
        self.events.keys().copied()
    }
}

/// Count active players per region cell.
///
/// `entity_ids` is an iterator of (EntityId, EntityKind) for all active entities.
/// `entity_regions` maps entity IDs to their current region cell.
/// Returns a map of (region_x, region_z, layer) → player count.
pub fn count_players_per_region<'a>(
    entities: impl Iterator<Item = (EntityId, EntityKind)>,
    entity_regions: &HashMap<EntityId, (i32, i32, u32)>,
) -> HashMap<(i32, i32, u32), u32> {
    let mut counts: HashMap<(i32, i32, u32), u32> = HashMap::new();
    for (eid, kind) in entities {
        if kind == EntityKind::Player {
            if let Some(&region) = entity_regions.get(&eid) {
                *counts.entry(region).or_insert(0) += 1;
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_protocol::tick::TickId;

    #[test]
    fn scaling_factor_baseline() {
        let sf = ScalingFactor::from_player_count(0);
        assert_eq!(sf.spawn_rate, 1.0);
        let sf = ScalingFactor::from_player_count(2);
        assert_eq!(sf.spawn_rate, 1.0);
    }

    #[test]
    fn scaling_factor_ramps() {
        let sf = ScalingFactor::from_player_count(4);
        assert_eq!(sf.spawn_rate, 1.5);
        let sf = ScalingFactor::from_player_count(10);
        assert_eq!(sf.spawn_rate, 3.0);
    }

    #[test]
    fn scaling_factor_capped() {
        let sf = ScalingFactor::from_player_count(100);
        assert_eq!(sf.spawn_rate, 3.0);
    }

    #[test]
    fn trigger_player_count() {
        let t = DirectorTrigger::PlayerCountAtLeast { threshold: 3 };
        let wp = HashMap::new();
        assert!(!t.evaluate(2, TickId(100), &wp));
        assert!(t.evaluate(3, TickId(100), &wp));
        assert!(t.evaluate(5, TickId(100), &wp));
    }

    #[test]
    fn trigger_after_tick() {
        let t = DirectorTrigger::AfterTick { tick: 50 };
        let wp = HashMap::new();
        assert!(!t.evaluate(0, TickId(49), &wp));
        assert!(t.evaluate(0, TickId(50), &wp));
        assert!(t.evaluate(0, TickId(100), &wp));
    }

    #[test]
    fn trigger_and_both_must_pass() {
        let t = DirectorTrigger::And(
            Box::new(DirectorTrigger::PlayerCountAtLeast { threshold: 2 }),
            Box::new(DirectorTrigger::AfterTick { tick: 100 }),
        );
        let wp = HashMap::new();
        assert!(!t.evaluate(2, TickId(99), &wp));
        assert!(!t.evaluate(1, TickId(100), &wp));
        assert!(t.evaluate(2, TickId(100), &wp));
    }

    #[test]
    fn event_fires_and_respects_cooldown() {
        let mut director = DirectorState::new();
        let eid = director.register_event(DynamicEvent {
            region: (0, 0, 0),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![SpawnDirective {
                kind: EntityKind::Npc,
                max_hp: 100.0,
                offset: [0.0, 0.0, 0.0],
            }],
            cooldown_ticks: 10,
            max_activations: 0,
            scaling: None,
        });

        let mut counts = HashMap::new();
        counts.insert((0, 0, 0), 1u32);

        // First evaluation fires.
        let wp = HashMap::new();
        let spawns = director.evaluate(&counts, TickId(1), &wp);
        assert!(!spawns.is_empty());

        // Still on cooldown.
        let spawns = director.evaluate(&counts, TickId(5), &wp);
        assert!(spawns.is_empty());

        // Cooldown expired.
        let spawns = director.evaluate(&counts, TickId(11), &wp);
        assert!(!spawns.is_empty());

        assert_eq!(eid, EventId(1));
    }

    #[test]
    fn event_max_activations() {
        let mut director = DirectorState::new();
        director.register_event(DynamicEvent {
            region: (1, 2, 0),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![SpawnDirective {
                kind: EntityKind::Npc,
                max_hp: 50.0,
                offset: [5.0, 0.0, 5.0],
            }],
            cooldown_ticks: 0,
            max_activations: 2,
            scaling: None,
        });

        let mut counts = HashMap::new();
        counts.insert((1, 2, 0), 3u32);

        let wp = HashMap::new();
        assert!(!director.evaluate(&counts, TickId(1), &wp).is_empty());
        assert!(!director.evaluate(&counts, TickId(2), &wp).is_empty());
        // Max activations hit.
        assert!(director.evaluate(&counts, TickId(3), &wp).is_empty());
    }

    #[test]
    fn count_players_per_region_filters_non_players() {
        let entities = vec![
            (EntityId(1), EntityKind::Player),
            (EntityId(2), EntityKind::Npc),
            (EntityId(3), EntityKind::Player),
        ];
        let mut regions = HashMap::new();
        regions.insert(EntityId(1), (0, 0, 0));
        regions.insert(EntityId(2), (0, 0, 0));
        regions.insert(EntityId(3), (0, 0, 0));

        let counts = count_players_per_region(entities.into_iter(), &regions);
        assert_eq!(counts.get(&(0, 0, 0)), Some(&2));
    }

    #[test]
    fn remove_event() {
        let mut director = DirectorState::new();
        let eid = director.register_event(DynamicEvent {
            region: (0, 0, 0),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![],
            cooldown_ticks: 0,
            max_activations: 0,
            scaling: None,
        });
        assert_eq!(director.event_count(), 1);
        assert!(director.remove_event(eid));
        assert_eq!(director.event_count(), 0);
        assert!(!director.remove_event(eid));
    }

    #[test]
    fn clear_for_layer_removes_matching_events() {
        let mut director = DirectorState::new();
        // Two events on the same dungeon layer, one on open world.
        director.register_event(DynamicEvent {
            region: (0, 0, 200),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![],
            cooldown_ticks: 0,
            max_activations: 0,
            scaling: None,
        });
        director.register_event(DynamicEvent {
            region: (3, 4, 200),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![],
            cooldown_ticks: 0,
            max_activations: 0,
            scaling: None,
        });
        director.register_event(DynamicEvent {
            region: (0, 0, 0),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![],
            cooldown_ticks: 0,
            max_activations: 0,
            scaling: None,
        });
        assert_eq!(director.event_count(), 3);
        let removed = director.clear_for_layer(200);
        assert_eq!(removed, 2);
        assert_eq!(director.event_count(), 1);
        // Open-world event still present.
        assert_eq!(director.clear_for_layer(200), 0);
    }

    #[test]
    fn scaling_applies_per_player_hp_and_count() {
        use game_schema::spawn::SpawnScaling;
        let mut director = DirectorState::new();
        director.register_event(DynamicEvent {
            region: (0, 0, 0),
            trigger: DirectorTrigger::PlayerCountAtLeast { threshold: 1 },
            spawns: vec![SpawnDirective {
                kind: EntityKind::Boss,
                max_hp: 100.0,
                offset: [0.0, 0.0, 0.0],
            }],
            cooldown_ticks: 0,
            max_activations: 0,
            scaling: Some(SpawnScaling {
                per_player_hp_mult: 0.5, // +50% per extra player
                per_player_count: 1.0,   // +1 copy per extra player
                ..Default::default()
            }),
        });

        let mut counts = HashMap::new();
        counts.insert((0, 0, 0), 3u32); // 3 players → 2 extras
        let wp = HashMap::new();
        let spawns = director.evaluate(&counts, TickId(1), &wp);

        // count = 1 + 1.0 * 2 = 3
        assert_eq!(spawns.len(), 3);
        // hp = 100 * (1 + 0.5 * 2) = 200
        for s in &spawns {
            assert!(
                (s.max_hp - 200.0).abs() < 1e-4,
                "unexpected hp {}",
                s.max_hp
            );
        }
    }
}
