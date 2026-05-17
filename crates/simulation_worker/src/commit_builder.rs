//! Tick result → commit payload marshalling.
//!
//! `CommitBuilder` converts a `TickResult` into an SDK-free `CommitPackage`
//! that is ready for the coordinator to forward to the `commit_tick_results`
//! reducer.  All event classification, transform flattening, and entity
//! state conversion happen here — the coordinator just maps the intermediate
//! types 1:1 into generated binding types.
//!
//! This module is not feature-gated: it depends only on `game_protocol` and
//! `game_schema`, so all marshalling logic is testable without the SDK.
#[cfg(test)]
use game_protocol::entity_id::EntityId;
use game_protocol::event::{EventPayload, SimEvent};
use game_schema::DamageType;

use crate::tick_pipeline::TickResult;

// ── SDK-free intermediate types ─────────────────────────────────
//
// Mirror the wire types from `server_module/src/reducers.rs` without
// requiring the SpacetimeDB `SpacetimeType` derive.  The coordinator
// maps these 1:1 into the generated binding types.

/// Flattened transform snapshot for one entity.
#[derive(Clone, Debug)]
pub struct CommitTransform {
    pub entity_id: u64,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub rot_x: f32,
    pub rot_y: f32,
    pub rot_z: f32,
    pub rot_w: f32,
    pub vel_x: f32,
    pub vel_y: f32,
    pub vel_z: f32,
    pub angvel_x: f32,
    pub angvel_y: f32,
    pub angvel_z: f32,
}

/// Health snapshot for one entity.
#[derive(Clone, Debug)]
pub struct CommitHealth {
    pub entity_id: u64,
    pub hp: f32,
    pub max_hp: f32,
}

/// Entity lifecycle state transition.
#[derive(Clone, Debug)]
pub struct CommitEntityState {
    pub entity_id: u64,
    pub new_state: CommitEntityStateKind,
}

/// SDK-free mirror of `EntityState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitEntityStateKind {
    Spawning,
    Active,
    DespawnPending,
    Removed,
}

/// Region assignment change for one entity.
#[derive(Clone, Debug)]
pub struct CommitRegion {
    pub entity_id: u64,
    pub region_x: i32,
    pub region_z: i32,
    pub layer: u32,
}

/// Damage data embedded in a combat event.
#[derive(Clone, Debug)]
pub struct CommitDamageData {
    pub amount: f32,
    pub damage_type: DamageType,
}

/// Buff application data embedded in a combat event.
#[derive(Clone, Debug)]
pub struct CommitBuffAppliedData {
    pub buff_id: u32,
    pub duration_ticks: u32,
}

/// Combat event kind — mirrors `CombatEventKind` from `server_module`.
#[derive(Clone, Debug)]
pub enum CommitCombatEventKind {
    Damage(CommitDamageData),
    SkillHit(u32),
    BuffApplied(CommitBuffAppliedData),
    BuffExpired(u32),
    EntityDied(Option<u64>),
}

/// A single combat event ready for commit.
#[derive(Clone, Debug)]
pub struct CommitCombatEvent {
    pub source_entity: u64,
    pub target_entity: u64,
    pub event_sequence: u32,
    pub event_kind: CommitCombatEventKind,
}

/// World event kind — mirrors `WorldEventKind` from `server_module`.
#[derive(Clone, Debug)]
pub enum CommitWorldEventKind {
    EntityDespawned,
    PickupCollected(u32),
    /// Player entity interacted with a world object within proximity range.
    /// Payload is the target entity_id.
    InteractTriggered(u64),
}

/// A single world event ready for commit.
#[derive(Clone, Debug)]
pub struct CommitWorldEvent {
    pub entity_id: u64,
    pub event_sequence: u32,
    pub event_kind: CommitWorldEventKind,
}

/// One buff instance for persistence. Mirrors the `active_buff` DB table.
#[derive(Clone, Debug)]
pub struct CommitBuff {
    pub entity_id: u64,
    pub buff_id: u32,
    pub source_entity: u64,
    pub stacks: u32,
    pub expires_at_tick: Option<u64>,
}

/// One threat entry for persistence. Mirrors the `threat_entry` DB table.
#[derive(Clone, Debug)]
pub struct CommitThreat {
    pub npc_entity: u64,
    pub source_entity: u64,
    pub threat: f32,
}

/// NPC AI state for persistence. Mirrors the `npc_state` DB table.
#[derive(Clone, Debug)]
pub struct CommitNpcState {
    pub entity_id: u64,
    pub ai_state: game_schema::NpcAiState,
    pub target_entity: Option<u64>,
}

// ── CommitPackage ───────────────────────────────────────────────

/// Complete marshalled payload for one tick commit.
///
/// Contains all vectors the `commit_tick_results` reducer expects,
/// in SDK-free intermediate form.  The coordinator maps each field
/// 1:1 into the generated binding types before calling the reducer.
#[derive(Clone, Debug)]
pub struct CommitPackage {
    pub tick_id: u64,
    pub transforms: Vec<CommitTransform>,
    pub health_updates: Vec<CommitHealth>,
    pub combat_events: Vec<CommitCombatEvent>,
    pub world_events: Vec<CommitWorldEvent>,
    pub consumed_intent_ids: Vec<u64>,
    pub entity_state_updates: Vec<CommitEntityState>,
    pub region_updates: Vec<CommitRegion>,
    /// Full buff snapshot — delete-all-then-insert per entity in the reducer.
    pub buff_updates: Vec<CommitBuff>,
    /// Entity IDs whose buff rows should be fully replaced this tick.
    /// Includes entities with zero buffs so stale rows are cleared when all buffs expire.
    pub buff_cleared_entity_ids: Vec<u64>,
    /// Full threat snapshot — delete-all-then-insert per NPC in the reducer.
    pub threat_updates: Vec<CommitThreat>,
    /// NPC/Boss entity IDs whose threat rows should be fully replaced this tick.
    /// Includes entities with zero threat so stale rows are cleared when threat decays out.
    pub threat_cleared_entity_ids: Vec<u64>,
    /// NPC AI state snapshot — upsert by entity PK in the reducer.
    pub npc_state_updates: Vec<CommitNpcState>,
}

// ── Builder ─────────────────────────────────────────────────────

/// Build a `CommitPackage` from a `TickResult` and consumed intent IDs.
///
/// This is a pure function — no side effects, no SDK dependency.
pub fn build(result: TickResult, consumed_intent_ids: Vec<u64>) -> CommitPackage {
    let tick_id = result.tick_id.0;

    let transforms = result
        .transforms
        .iter()
        .map(|(eid, t)| CommitTransform {
            entity_id: eid.0,
            pos_x: t.position.x,
            pos_y: t.position.y,
            pos_z: t.position.z,
            rot_x: t.rotation.x,
            rot_y: t.rotation.y,
            rot_z: t.rotation.z,
            rot_w: t.rotation.w,
            vel_x: t.linear_velocity.x,
            vel_y: t.linear_velocity.y,
            vel_z: t.linear_velocity.z,
            angvel_x: t.angular_velocity.x,
            angvel_y: t.angular_velocity.y,
            angvel_z: t.angular_velocity.z,
        })
        .collect();

    let health_updates = result
        .health_updates
        .iter()
        .map(|(eid, hp, max_hp)| CommitHealth {
            entity_id: eid.0,
            hp: *hp,
            max_hp: *max_hp,
        })
        .collect();

    let entity_state_updates = result
        .entity_state_updates
        .iter()
        .map(|(eid, state)| CommitEntityState {
            entity_id: eid.0,
            new_state: convert_entity_state(*state),
        })
        .collect();

    let (combat_events, world_events) = classify_events(&result.events);

    let buff_updates = result
        .buff_updates
        .iter()
        .flat_map(|(eid, buffs)| {
            buffs.iter().map(move |b| CommitBuff {
                entity_id: eid.0,
                buff_id: b.buff_id,
                source_entity: b.source.0,
                stacks: b.stacks,
                expires_at_tick: b.expires_at.map(|t| t.0),
            })
        })
        .collect();

    let threat_updates = result
        .threat_updates
        .iter()
        .flat_map(|(eid, entries)| {
            entries.iter().map(move |e| CommitThreat {
                npc_entity: eid.0,
                source_entity: e.source.0,
                threat: e.threat,
            })
        })
        .collect();

    let npc_state_updates = result
        .npc_state_updates
        .iter()
        .map(|(eid, ai_state, target)| CommitNpcState {
            entity_id: eid.0,
            ai_state: *ai_state,
            target_entity: target.map(|t| t.0),
        })
        .collect();

    let mut buff_cleared_entity_ids: Vec<u64> = result.buff_updates.iter().map(|(eid, _)| eid.0).collect();
    let mut threat_cleared_entity_ids: Vec<u64> = result.threat_updates.iter().map(|(eid, _)| eid.0).collect();

    // Removed entities are excluded from buff/threat update snapshots, but their
    // stale DB rows still need to be deleted. Include them in the cleared lists.
    for (eid, state) in &result.entity_state_updates {
        if *state == game_schema::EntityState::Removed {
            buff_cleared_entity_ids.push(eid.0);
            threat_cleared_entity_ids.push(eid.0);
        }
    }

    let region_updates = result
        .region_updates
        .iter()
        .map(|(eid, cell)| CommitRegion {
            entity_id: eid.0,
            region_x: cell.region_x,
            region_z: cell.region_z,
            layer: cell.layer,
        })
        .collect();

    CommitPackage {
        tick_id,
        transforms,
        health_updates,
        combat_events,
        world_events,
        consumed_intent_ids,
        entity_state_updates,
        region_updates,
        buff_updates,
        buff_cleared_entity_ids,
        threat_updates,
        threat_cleared_entity_ids,
        npc_state_updates,
    }
}

// ── Internal helpers ────────────────────────────────────────────

fn convert_entity_state(state: game_schema::EntityState) -> CommitEntityStateKind {
    match state {
        game_schema::EntityState::Spawning => CommitEntityStateKind::Spawning,
        game_schema::EntityState::Active => CommitEntityStateKind::Active,
        game_schema::EntityState::DespawnPending => CommitEntityStateKind::DespawnPending,
        game_schema::EntityState::Removed => CommitEntityStateKind::Removed,
    }
}

/// Classify pipeline `SimEvent`s into commit-ready combat and world events.
///
/// Internal pipeline events (hitbox lifecycle, cooldown ready, tick boundary)
/// are filtered out — they are not persisted to the DB.
fn classify_events(events: &[SimEvent]) -> (Vec<CommitCombatEvent>, Vec<CommitWorldEvent>) {
    let mut combat_events: Vec<CommitCombatEvent> = Vec::new();
    let mut world_events: Vec<CommitWorldEvent> = Vec::new();

    for e in events {
        match &e.payload {
            EventPayload::Damage {
                source,
                amount,
                damage_type,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Damage(CommitDamageData {
                        amount: *amount,
                        damage_type: *damage_type,
                    }),
                });
            }
            EventPayload::SkillHit { skill_id, source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::SkillHit(*skill_id),
                });
            }
            EventPayload::BuffApplied {
                buff_id,
                source,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::BuffApplied(CommitBuffAppliedData {
                        buff_id: *buff_id,
                        duration_ticks: *duration_ticks,
                    }),
                });
            }
            EventPayload::BuffExpired { buff_id } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::BuffExpired(*buff_id),
                });
            }
            EventPayload::EntityDied { killer } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: killer.map(|k| k.0).unwrap_or(0),
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::EntityDied(killer.map(|k| k.0)),
                });
            }
            EventPayload::EntityDespawned => {
                world_events.push(CommitWorldEvent {
                    entity_id: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitWorldEventKind::EntityDespawned,
                });
            }
            EventPayload::PickupCollected { item_id } => {
                world_events.push(CommitWorldEvent {
                    entity_id: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitWorldEventKind::PickupCollected(*item_id),
                });
            }
            EventPayload::InteractTriggered { target } => {
                world_events.push(CommitWorldEvent {
                    entity_id: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitWorldEventKind::InteractTriggered(target.0),
                });
            }
            // Internal pipeline events — not committed to DB.
            EventPayload::EntitySpawned
            | EventPayload::HitboxSpawned { .. }
            | EventPayload::DamageFrame { .. }
            | EventPayload::HitboxRemoved { .. }
            | EventPayload::CooldownReady { .. }
            | EventPayload::TickBoundary => {}
        }
    }

    (combat_events, world_events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_protocol::event::SimEvent;
    use game_protocol::tick::TickId;
    use game_protocol::types::{Quatf, Transform, Vec3f};

    fn make_transform(eid: u64) -> (EntityId, Transform) {
        (
            EntityId(eid),
            Transform {
                position: Vec3f {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
                rotation: Quatf {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                    w: 1.0,
                },
                linear_velocity: Vec3f {
                    x: 0.5,
                    y: 0.0,
                    z: -0.5,
                },
                angular_velocity: Vec3f {
                    x: 0.0,
                    y: 0.1,
                    z: 0.0,
                },
            },
        )
    }

    fn make_tick_result(tick: u64) -> TickResult {
        TickResult {
            tick_id: TickId(tick),
            transforms: vec![make_transform(100), make_transform(200)],
            events: Vec::new(),
            summary: Default::default(),
            entity_state_updates: vec![
                (EntityId(100), game_schema::EntityState::Active),
                (EntityId(200), game_schema::EntityState::DespawnPending),
            ],
            health_updates: vec![(EntityId(100), 75.0, 100.0)],
            buff_updates: Vec::new(),
            threat_updates: Vec::new(),
            npc_state_updates: Vec::new(),
            region_updates: Vec::new(),
        }
    }

    #[test]
    fn build_marshals_transforms() {
        let result = make_tick_result(42);
        let pkg = build(result, vec![1, 2, 3]);

        assert_eq!(pkg.tick_id, 42);
        assert_eq!(pkg.transforms.len(), 2);
        assert_eq!(pkg.transforms[0].entity_id, 100);
        assert_eq!(pkg.transforms[0].pos_x, 1.0);
        assert_eq!(pkg.transforms[0].rot_w, 1.0);
        assert_eq!(pkg.transforms[0].vel_x, 0.5);
        assert_eq!(pkg.transforms[0].angvel_y, 0.1);
        assert_eq!(pkg.transforms[1].entity_id, 200);
    }

    #[test]
    fn build_marshals_health_updates() {
        let result = make_tick_result(10);
        let pkg = build(result, vec![]);

        assert_eq!(pkg.health_updates.len(), 1);
        assert_eq!(pkg.health_updates[0].entity_id, 100);
        assert_eq!(pkg.health_updates[0].hp, 75.0);
        assert_eq!(pkg.health_updates[0].max_hp, 100.0);
    }

    #[test]
    fn build_marshals_entity_state_updates() {
        let result = make_tick_result(10);
        let pkg = build(result, vec![]);

        assert_eq!(pkg.entity_state_updates.len(), 2);
        assert_eq!(pkg.entity_state_updates[0].entity_id, 100);
        assert_eq!(
            pkg.entity_state_updates[0].new_state,
            CommitEntityStateKind::Active
        );
        assert_eq!(pkg.entity_state_updates[1].entity_id, 200);
        assert_eq!(
            pkg.entity_state_updates[1].new_state,
            CommitEntityStateKind::DespawnPending
        );
    }

    #[test]
    fn build_passes_consumed_intent_ids() {
        let result = make_tick_result(5);
        let pkg = build(result, vec![10, 20, 30]);

        assert_eq!(pkg.consumed_intent_ids, vec![10, 20, 30]);
    }

    #[test]
    fn classify_events_damage() {
        let events = vec![SimEvent {
            tick_id: TickId(1),
            event_sequence: 0,
            entity_id: EntityId(200),
            payload: EventPayload::Damage {
                source: EntityId(100),
                amount: 25.0,
                damage_type: game_schema::DamageType::Physical,
            },
        }];

        let (combat, world) = classify_events(&events);
        assert_eq!(combat.len(), 1);
        assert!(world.is_empty());
        assert_eq!(combat[0].source_entity, 100);
        assert_eq!(combat[0].target_entity, 200);
        assert_eq!(combat[0].event_sequence, 0);
        match &combat[0].event_kind {
            CommitCombatEventKind::Damage(d) => {
                assert_eq!(d.amount, 25.0);
                assert_eq!(d.damage_type, DamageType::Physical);
            }
            other => panic!("Expected Damage, got {other:?}"),
        }
    }

    #[test]
    fn classify_events_skill_hit() {
        let events = vec![SimEvent {
            tick_id: TickId(1),
            event_sequence: 1,
            entity_id: EntityId(200),
            payload: EventPayload::SkillHit {
                skill_id: 1,
                source: EntityId(100),
            },
        }];

        let (combat, _) = classify_events(&events);
        assert_eq!(combat.len(), 1);
        assert_eq!(combat[0].source_entity, 100);
        match &combat[0].event_kind {
            CommitCombatEventKind::SkillHit(id) => assert_eq!(*id, 1),
            other => panic!("Expected SkillHit, got {other:?}"),
        }
    }

    #[test]
    fn classify_events_buff_lifecycle() {
        let events = vec![
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 0,
                entity_id: EntityId(200),
                payload: EventPayload::BuffApplied {
                    buff_id: 5,
                    source: EntityId(100),
                    duration_ticks: 60,
                },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 1,
                entity_id: EntityId(200),
                payload: EventPayload::BuffExpired { buff_id: 5 },
            },
        ];

        let (combat, _) = classify_events(&events);
        assert_eq!(combat.len(), 2);
        match &combat[0].event_kind {
            CommitCombatEventKind::BuffApplied(b) => {
                assert_eq!(b.buff_id, 5);
                assert_eq!(b.duration_ticks, 60);
            }
            other => panic!("Expected BuffApplied, got {other:?}"),
        }
        match &combat[1].event_kind {
            CommitCombatEventKind::BuffExpired(id) => assert_eq!(*id, 5),
            other => panic!("Expected BuffExpired, got {other:?}"),
        }
    }

    #[test]
    fn classify_events_entity_died() {
        let events = vec![SimEvent {
            tick_id: TickId(1),
            event_sequence: 3,
            entity_id: EntityId(200),
            payload: EventPayload::EntityDied {
                killer: Some(EntityId(100)),
            },
        }];

        let (combat, _) = classify_events(&events);
        assert_eq!(combat.len(), 1);
        assert_eq!(combat[0].source_entity, 100);
        assert_eq!(combat[0].target_entity, 200);
        match &combat[0].event_kind {
            CommitCombatEventKind::EntityDied(k) => assert_eq!(*k, Some(100)),
            other => panic!("Expected EntityDied, got {other:?}"),
        }
    }

    #[test]
    fn classify_events_entity_died_no_killer() {
        let events = vec![SimEvent {
            tick_id: TickId(1),
            event_sequence: 0,
            entity_id: EntityId(300),
            payload: EventPayload::EntityDied { killer: None },
        }];

        let (combat, _) = classify_events(&events);
        assert_eq!(combat[0].source_entity, 0);
        match &combat[0].event_kind {
            CommitCombatEventKind::EntityDied(k) => assert_eq!(*k, None),
            other => panic!("Expected EntityDied, got {other:?}"),
        }
    }

    #[test]
    fn classify_events_world_events() {
        let events = vec![
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 0,
                entity_id: EntityId(50),
                payload: EventPayload::EntityDespawned,
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 1,
                entity_id: EntityId(60),
                payload: EventPayload::PickupCollected { item_id: 42 },
            },
        ];

        let (combat, world) = classify_events(&events);
        assert!(combat.is_empty());
        assert_eq!(world.len(), 2);
        assert_eq!(world[0].entity_id, 50);
        match &world[0].event_kind {
            CommitWorldEventKind::EntityDespawned => {}
            other => panic!("Expected EntityDespawned, got {other:?}"),
        }
        assert_eq!(world[1].entity_id, 60);
        match &world[1].event_kind {
            CommitWorldEventKind::PickupCollected(id) => assert_eq!(*id, 42),
            other => panic!("Expected PickupCollected, got {other:?}"),
        }
    }

    #[test]
    fn classify_events_skips_internal_events() {
        let events = vec![
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 0,
                entity_id: EntityId(100),
                payload: EventPayload::EntitySpawned,
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 1,
                entity_id: EntityId(100),
                payload: EventPayload::HitboxSpawned { ability_id: 1 },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 2,
                entity_id: EntityId(100),
                payload: EventPayload::DamageFrame { ability_id: 1 },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 3,
                entity_id: EntityId(100),
                payload: EventPayload::HitboxRemoved { ability_id: 1 },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 4,
                entity_id: EntityId(100),
                payload: EventPayload::CooldownReady { ability_id: 1 },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 5,
                entity_id: EntityId(100),
                payload: EventPayload::TickBoundary,
            },
        ];

        let (combat, world) = classify_events(&events);
        assert!(combat.is_empty());
        assert!(world.is_empty());
    }

    #[test]
    fn classify_events_preserves_event_sequence() {
        let events = vec![
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 0,
                entity_id: EntityId(200),
                payload: EventPayload::Damage {
                    source: EntityId(100),
                    amount: 10.0,
                    damage_type: game_schema::DamageType::Physical,
                },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 1,
                entity_id: EntityId(200),
                payload: EventPayload::EntityDied {
                    killer: Some(EntityId(100)),
                },
            },
            SimEvent {
                tick_id: TickId(1),
                event_sequence: 2,
                entity_id: EntityId(200),
                payload: EventPayload::EntityDespawned,
            },
        ];

        let (combat, world) = classify_events(&events);
        assert_eq!(combat[0].event_sequence, 0);
        assert_eq!(combat[1].event_sequence, 1);
        assert_eq!(world[0].event_sequence, 2);
    }

    #[test]
    fn build_empty_tick_result() {
        let result = TickResult {
            tick_id: TickId(99),
            transforms: Vec::new(),
            events: Vec::new(),
            summary: Default::default(),
            entity_state_updates: Vec::new(),
            health_updates: Vec::new(),
            buff_updates: Vec::new(),
            threat_updates: Vec::new(),
            npc_state_updates: Vec::new(),
            region_updates: Vec::new(),
        };
        let pkg = build(result, Vec::new());

        assert_eq!(pkg.tick_id, 99);
        assert!(pkg.transforms.is_empty());
        assert!(pkg.health_updates.is_empty());
        assert!(pkg.combat_events.is_empty());
        assert!(pkg.world_events.is_empty());
        assert!(pkg.consumed_intent_ids.is_empty());
        assert!(pkg.entity_state_updates.is_empty());
        assert!(pkg.region_updates.is_empty());
    }
}
