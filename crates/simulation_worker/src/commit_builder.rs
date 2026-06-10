//! TickResult -> commit payload marshalling.
//!
//! Converts simulation output into an SDK-free `CommitPackage` that the
//! coordinator maps 1:1 into generated reducer binding types.
#[cfg(test)]
use game_protocol::entity_id::EntityId;
use game_protocol::event::{AbilityCancelReason, EventPayload, SimEvent};
use game_schema::DamageType;

use crate::tick_pipeline::TickResult;

// SDK-free mirrors of reducer wire types from `server_module/src/reducers.rs`.

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
    CastStart {
        ability_id: u32,
        cast_duration_ticks: u32,
        /// See `EventPayload::CastStart::effective_cooldown_ticks`.
        effective_cooldown_ticks: u32,
    },
    ChargeStart {
        ability_id: u32,
        max_ticks: u32,
    },
    ChargeTierReached {
        ability_id: u32,
        tier: u8,
    },
    BlockStart,
    BlockEnd,
    Damage(CommitDamageData),
    Healed {
        amount: f32,
    },
    SkillHit(u32),
    BuffApplied(CommitBuffAppliedData),
    BuffExpired(u32),
    EntityDied(Option<u64>),
    Dodged(u32),
    Blocked {
        ability_id: u32,
        damage_taken: f32,
        perfect: bool,
    },
    Covered {
        blocker: u64,
        ability_id: u32,
        damage_taken: f32,
    },
    TelegraphWarning {
        target: u64,
        impact_tick: u64,
    },
    AreaTelegraph {
        ability_id: u32,
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        radius: f32,
        shape: String,
        impact_tick: u64,
    },
    EncounterCue {
        cue_id: String,
        anchor_entity: Option<u64>,
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        shape: String,
        inner_radius: f32,
        outer_radius: f32,
        half_height: f32,
        starts_at_tick: u64,
        expires_at_tick: u64,
    },
    LockOnAcquired,
    LockOnSessionStarted {
        ability_id: u32,
    },
    LockOnCanceled {
        target: u64,
    },
    LockOnFired {
        targets: Vec<u64>,
    },
    ProjectileLaunched {
        execution_id: u64,
        ability_id: u32,
        origin_x: f32,
        origin_y: f32,
        origin_z: f32,
        direction_x: f32,
        direction_y: f32,
        direction_z: f32,
        speed: f32,
        max_range: f32,
    },
    HazardSpawned {
        execution_id: u64,
        ability_id: u32,
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        radius: f32,
    },
    ContactHitboxSpawned {
        execution_id: u64,
        parent_execution_id: u64,
        ability_id: u32,
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        radius: f32,
        duration_ticks: u32,
    },
    SkillObjectRemoved {
        execution_id: u64,
    },
    Teleported {
        from_x: f32,
        from_y: f32,
        from_z: f32,
        to_x: f32,
        to_y: f32,
        to_z: f32,
    },
    Knockback {
        force: f32,
    },
    Launched,
    Stunned {
        duration_ticks: u32,
    },
    KnockedDown {
        duration_ticks: u32,
    },
    Pulled,
    Slept {
        duration_ticks: u32,
    },
    Silenced {
        duration_ticks: u32,
    },
    Feared {
        duration_ticks: u32,
    },
    StabilityConsumed {
        buff_id: u32,
    },
    WeaponSwapped {
        new_set: u8,
    },
    CCCleared {
        cc_effect: game_schema::CCEffect,
        source: u64,
    },
    Cleansed {
        count: u32,
        source: u64,
    },
    Stunbreak,
    CCImmune {
        cc_effect: game_schema::CCEffect,
        source: u64,
    },
    /// An in-flight cast or charge was cancelled.
    /// Appended at the tail — variant order is matched 1:1 with
    /// `CombatEventKind::AbilityCancelled` in `server_module`.
    AbilityCancelled {
        ability_id: u32,
        reason: AbilityCancelReason,
    },
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

/// Per-instance buff state for persistence. Only runtime-varying fields;
/// static template data is reconstructed from `BuffRegistry` at rehydration.
#[derive(Clone, Debug)]
pub struct CommitBuff {
    pub entity_id: u64,
    pub buff_id: u32,
    pub source_entity: u64,
    pub stacks: u32,
    pub expires_at_tick: Option<u64>,
    pub mod_ai_override_kind: Option<u8>,
    pub mod_ai_override_target: Option<u64>,
    pub mod_stealth: Option<bool>,
    pub last_dot_tick: Option<u64>,
}

/// NPC AI state for persistence. Mirrors the `npc_state` DB table.
#[derive(Clone, Debug)]
pub struct CommitNpcState {
    pub entity_id: u64,
    pub ai_state: game_schema::NpcAiState,
    pub target_entity: Option<u64>,
}

/// Director spawn request — entity to be created in the DB by the coordinator.
#[derive(Clone, Debug)]
pub struct CommitDirectorSpawn {
    pub kind: game_schema::EntityKind,
    pub max_hp: f32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub layer: u32,
    pub npc_config: Option<CommitDirectorNpcConfig>,
    pub team_id: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct CommitDirectorNpcConfig {
    pub passive: bool,
    pub no_chase: bool,
    pub ability_ids: Vec<u32>,
    pub leash_radius: f32,
    pub aggro_radius: f32,
    pub body_shape: Option<u8>,
}

/// Interactable state change for commit.
#[derive(Clone, Debug)]
pub struct CommitInteractableUpdate {
    pub entity_id: u64,
    pub new_state: game_core::sim_state::SimInteractState,
}

/// Authoritative `death_state` row insert produced by Phase 8b.
#[derive(Clone, Debug)]
pub struct CommitDeathStateInsert {
    pub entity_id: u64,
    pub killer_entity: Option<u64>,
    pub layer: u32,
    pub death_pos_x: f32,
    pub death_pos_y: f32,
    pub death_pos_z: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitLootRollItem {
    pub item_id: u32,
    pub quantity: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommitLootRoll {
    pub corpse_entity: u64,
    pub layer: u32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub eligible_claimants: Vec<u64>,
    pub claim_window_ticks: u64,
    pub items: Vec<CommitLootRollItem>,
}

/// Encounter-add membership entry paired with a `CommitDirectorSpawn` by
/// `spawn_index`. Mirrors `game_core::director::PendingAddMembership` in
/// SDK-free form. See `docs/contracts/spawn_add_membership_contract.md`.
#[derive(Clone, Debug)]
pub struct CommitEncounterAddMembership {
    pub spawn_index: u32,
    pub boss_entity: u64,
    pub archetype: String,
    pub tags: Vec<String>,
}

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
    /// NPC AI state snapshot — upsert by entity PK in the reducer.
    pub npc_state_updates: Vec<CommitNpcState>,
    /// Entities spawned by the world director that need DB rows created.
    pub director_spawns: Vec<CommitDirectorSpawn>,
    /// Encounter-add memberships paired with `director_spawns` by
    /// `spawn_index`. Inserted as `encounter_add` rows in the same
    /// transaction as the entity inserts.
    pub encounter_memberships: Vec<CommitEncounterAddMembership>,
    /// Interactable state changes committed inline via `commit_tick_results`.
    pub interactable_updates: Vec<CommitInteractableUpdate>,
    /// Boss phase transitions from encounter executor.
    /// Each entry is (boss_entity_id, phase_number, entered_at_tick).
    pub boss_phase_updates: Vec<(u64, u32, u64)>,
    /// Zone counter increments from encounter executor / combat system.
    /// Each entry is (layer, region_x, region_z, counter_name, delta).
    pub zone_counter_deltas: Vec<(u32, i32, i32, String, f64)>,
    /// Authoritative player-death rows. Inserted verbatim by the reducer
    /// (no derivation from observed lifecycle transitions).
    pub death_state_inserts: Vec<CommitDeathStateInsert>,
    /// Worker-produced loot rolls. Inserted verbatim as loot piles/items by
    /// the reducer in the same commit as the matching death event.
    pub loot_rolls: Vec<CommitLootRoll>,
    /// Simulation-pipeline diagnostic messages for this tick.
    /// Forwarded to the `sim_log` event table via `commit_tick_results`.
    /// Populated from `TickResult::sim_warnings`; level is always Warn (2).
    pub sim_logs: Vec<String>,
}

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
            buffs.iter().map(move |b| {
                use game_core::combat::status::AiOverride;
                let (ai_kind, ai_target) = match b.modifiers.ai_override {
                    None => (None, None),
                    Some(AiOverride::ForceFlee) => (Some(0u8), None),
                    Some(AiOverride::ForceIdle) => (Some(1u8), None),
                    Some(AiOverride::ForceFocus { target }) => (Some(2u8), Some(target.0)),
                };
                CommitBuff {
                    entity_id: eid.0,
                    buff_id: b.buff_id,
                    source_entity: b.source.0,
                    stacks: b.stacks,
                    expires_at_tick: b.expires_at.map(|t| t.0),
                    mod_ai_override_kind: ai_kind,
                    mod_ai_override_target: ai_target,
                    mod_stealth: b.modifiers.stealth,
                    last_dot_tick: b.last_dot_tick.map(|t| t.0),
                }
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

    let mut buff_cleared_entity_ids: Vec<u64> =
        result.buff_updates.iter().map(|(eid, _)| eid.0).collect();

    // Removed entities are excluded from buff update snapshots, but their
    // stale DB rows still need to be deleted. Include them in the cleared list.
    for (eid, state) in &result.entity_state_updates {
        if *state == game_schema::EntityState::Removed {
            buff_cleared_entity_ids.push(eid.0);
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

    let director_spawns = result
        .director_spawns
        .iter()
        .map(|s| CommitDirectorSpawn {
            kind: s.kind,
            max_hp: s.max_hp,
            pos_x: s.position.x,
            pos_y: s.position.y,
            pos_z: s.position.z,
            layer: s.layer,
            npc_config: s.npc_config.as_ref().map(|cfg| CommitDirectorNpcConfig {
                passive: cfg.passive,
                no_chase: cfg.no_chase,
                ability_ids: cfg.ability_ids.clone(),
                leash_radius: cfg.leash_radius,
                aggro_radius: cfg.aggro_radius,
                body_shape: cfg.body_shape,
            }),
            team_id: s.team_id,
        })
        .collect();

    let encounter_memberships = result
        .encounter_memberships
        .iter()
        .map(|m| CommitEncounterAddMembership {
            spawn_index: m.spawn_index,
            boss_entity: m.boss_entity.0,
            archetype: m.archetype.clone(),
            tags: m.tags.clone(),
        })
        .collect();

    let interactable_updates = result
        .interactable_updates
        .iter()
        .map(|(eid, new_state)| CommitInteractableUpdate {
            entity_id: eid.0,
            new_state: *new_state,
        })
        .collect();

    let death_state_inserts = result
        .death_state_inserts
        .iter()
        .map(|d| CommitDeathStateInsert {
            entity_id: d.entity_id.0,
            killer_entity: d.killer_entity.map(|k| k.0),
            layer: d.layer,
            death_pos_x: d.death_pos_x,
            death_pos_y: d.death_pos_y,
            death_pos_z: d.death_pos_z,
        })
        .collect();

    let loot_rolls = result
        .loot_rolls
        .iter()
        .map(|roll| CommitLootRoll {
            corpse_entity: roll.corpse_entity.0,
            layer: roll.layer,
            pos_x: roll.position.x,
            pos_y: roll.position.y,
            pos_z: roll.position.z,
            eligible_claimants: roll
                .eligible_claimants
                .iter()
                .map(|entity| entity.0)
                .collect(),
            claim_window_ticks: roll.claim_window_ticks,
            items: roll
                .rolled_items
                .iter()
                .map(|item| CommitLootRollItem {
                    item_id: item.item_id,
                    quantity: item.quantity,
                })
                .collect(),
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
        npc_state_updates,
        director_spawns,
        encounter_memberships,
        interactable_updates,
        boss_phase_updates: result.boss_phase_updates,
        zone_counter_deltas: result.zone_counter_deltas,
        death_state_inserts,
        loot_rolls,
        sim_logs: result.sim_warnings,
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
            EventPayload::Dodged { source, ability_id } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Dodged(*ability_id),
                });
            }
            EventPayload::Blocked {
                source,
                ability_id,
                damage_taken,
                perfect,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Blocked {
                        ability_id: *ability_id,
                        damage_taken: *damage_taken,
                        perfect: *perfect,
                    },
                });
            }
            EventPayload::Covered {
                blocker,
                ability_id,
                damage_taken,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: blocker.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Covered {
                        blocker: blocker.0,
                        ability_id: *ability_id,
                        damage_taken: *damage_taken,
                    },
                });
            }
            EventPayload::TelegraphWarning {
                source,
                target,
                impact_tick,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: target.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::TelegraphWarning {
                        target: target.0,
                        impact_tick: *impact_tick,
                    },
                });
            }
            EventPayload::AreaTelegraph {
                source,
                ability_id,
                position,
                radius,
                shape,
                impact_tick,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::AreaTelegraph {
                        ability_id: *ability_id,
                        pos_x: position.x,
                        pos_y: position.y,
                        pos_z: position.z,
                        radius: *radius,
                        shape: shape.clone(),
                        impact_tick: *impact_tick,
                    },
                });
            }
            EventPayload::EncounterCue {
                source,
                target,
                cue_id,
                anchor_entity,
                position,
                shape,
                inner_radius,
                outer_radius,
                half_height,
                starts_at_tick,
                expires_at_tick,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: target.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::EncounterCue {
                        cue_id: cue_id.clone(),
                        anchor_entity: anchor_entity.map(|id| id.0),
                        pos_x: position.x,
                        pos_y: position.y,
                        pos_z: position.z,
                        shape: shape.clone(),
                        inner_radius: *inner_radius,
                        outer_radius: *outer_radius,
                        half_height: *half_height,
                        starts_at_tick: *starts_at_tick,
                        expires_at_tick: *expires_at_tick,
                    },
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
            EventPayload::CastStart {
                ability_id,
                cast_duration_ticks,
                effective_cooldown_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::CastStart {
                        ability_id: *ability_id,
                        cast_duration_ticks: *cast_duration_ticks,
                        effective_cooldown_ticks: *effective_cooldown_ticks,
                    },
                });
            }
            EventPayload::ChargeStart {
                ability_id,
                max_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::ChargeStart {
                        ability_id: *ability_id,
                        max_ticks: *max_ticks,
                    },
                });
            }
            EventPayload::ChargeTierReached { ability_id, tier } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::ChargeTierReached {
                        ability_id: *ability_id,
                        tier: *tier,
                    },
                });
            }
            EventPayload::BlockStart => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::BlockStart,
                });
            }
            EventPayload::BlockEnd => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::BlockEnd,
                });
            }
            EventPayload::FallDamage { damage, .. } => {
                // Fall damage is a self-inflicted hit — source and target are the same entity.
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Damage(CommitDamageData {
                        amount: *damage,
                        damage_type: DamageType::Physical,
                    }),
                });
            }
            EventPayload::Healed { amount, source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Healed { amount: *amount },
                });
            }
            EventPayload::LockOnSessionStarted { source, ability_id } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::LockOnSessionStarted {
                        ability_id: *ability_id,
                    },
                });
            }
            EventPayload::LockOnCanceled { source, target } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: target.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::LockOnCanceled { target: target.0 },
                });
            }
            EventPayload::LockOnFired { source, targets } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::LockOnFired {
                        targets: targets.iter().map(|target| target.0).collect(),
                    },
                });
            }
            EventPayload::Teleported { entity, from, to } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: entity.0,
                    target_entity: entity.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Teleported {
                        from_x: from.x,
                        from_y: from.y,
                        from_z: from.z,
                        to_x: to.x,
                        to_y: to.y,
                        to_z: to.z,
                    },
                });
            }
            // Internal pipeline events — not committed to DB.
            EventPayload::Jumped
            | EventPayload::EntitySpawned
            | EventPayload::HitboxSpawned { .. }
            | EventPayload::DamageFrame { .. }
            | EventPayload::HitboxRemoved { .. }
            | EventPayload::CooldownReady { .. }
            | EventPayload::TickBoundary
            | EventPayload::CompensationApplied { .. }
            | EventPayload::VolumeEnter { .. }
            | EventPayload::VolumeExit { .. } => {}
            EventPayload::LockOnWarning { source, target } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: target.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::LockOnAcquired,
                });
            }
            // Weapon swap events — committed as combat events.
            EventPayload::WeaponSwapped { new_set } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::WeaponSwapped { new_set: *new_set },
                });
            }
            // CC events — committed as combat events.
            EventPayload::Knockback { source, force } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Knockback { force: *force },
                });
            }
            EventPayload::Launched { source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Launched,
                });
            }
            EventPayload::Stunned {
                source,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Stunned {
                        duration_ticks: *duration_ticks,
                    },
                });
            }
            EventPayload::KnockedDown {
                source,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::KnockedDown {
                        duration_ticks: *duration_ticks,
                    },
                });
            }
            EventPayload::Pulled { source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Pulled,
                });
            }
            EventPayload::Slept {
                source,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Slept {
                        duration_ticks: *duration_ticks,
                    },
                });
            }
            EventPayload::Silenced {
                source,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Silenced {
                        duration_ticks: *duration_ticks,
                    },
                });
            }
            EventPayload::Feared {
                source,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Feared {
                        duration_ticks: *duration_ticks,
                    },
                });
            }
            EventPayload::StabilityConsumed { buff_id } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::StabilityConsumed { buff_id: *buff_id },
                });
            }
            EventPayload::CCCleared { cc_effect, source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::CCCleared {
                        cc_effect: *cc_effect,
                        source: source.0,
                    },
                });
            }
            EventPayload::Cleansed { count, source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Cleansed {
                        count: *count,
                        source: source.0,
                    },
                });
            }
            EventPayload::Stunbreak => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::Stunbreak,
                });
            }
            EventPayload::CCImmune { cc_effect, source } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::CCImmune {
                        cc_effect: *cc_effect,
                        source: source.0,
                    },
                });
            }
            EventPayload::ProjectileLaunched {
                execution_id,
                ability_id,
                origin,
                direction,
                speed,
                max_range,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::ProjectileLaunched {
                        execution_id: *execution_id,
                        ability_id: *ability_id,
                        origin_x: origin.x,
                        origin_y: origin.y,
                        origin_z: origin.z,
                        direction_x: direction.x,
                        direction_y: direction.y,
                        direction_z: direction.z,
                        speed: *speed,
                        max_range: *max_range,
                    },
                });
            }
            EventPayload::HazardSpawned {
                execution_id,
                ability_id,
                position,
                radius,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::HazardSpawned {
                        execution_id: *execution_id,
                        ability_id: *ability_id,
                        pos_x: position.x,
                        pos_y: position.y,
                        pos_z: position.z,
                        radius: *radius,
                    },
                });
            }
            EventPayload::ContactHitboxSpawned {
                execution_id,
                parent_execution_id,
                ability_id,
                position,
                radius,
                duration_ticks,
            } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::ContactHitboxSpawned {
                        execution_id: *execution_id,
                        parent_execution_id: *parent_execution_id,
                        ability_id: *ability_id,
                        pos_x: position.x,
                        pos_y: position.y,
                        pos_z: position.z,
                        radius: *radius,
                        duration_ticks: *duration_ticks,
                    },
                });
            }
            EventPayload::SkillObjectRemoved { execution_id } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::SkillObjectRemoved {
                        execution_id: *execution_id,
                    },
                });
            }
            EventPayload::AbilityCancelled { ability_id, reason } => {
                combat_events.push(CommitCombatEvent {
                    source_entity: e.entity_id.0,
                    target_entity: 0,
                    event_sequence: e.event_sequence,
                    event_kind: CommitCombatEventKind::AbilityCancelled {
                        ability_id: *ability_id,
                        reason: *reason,
                    },
                });
            }
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
            npc_state_updates: Vec::new(),
            region_updates: Vec::new(),
            director_spawns: Vec::new(),
            encounter_memberships: Vec::new(),
            interactable_updates: Vec::new(),
            boss_phase_updates: Vec::new(),
            zone_counter_deltas: Vec::new(),
            death_state_inserts: Vec::new(),
            loot_rolls: Vec::new(),
            sim_warnings: Vec::new(),
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
    fn build_marshals_encounter_memberships() {
        let mut result = make_tick_result(77);
        result.encounter_memberships = vec![
            game_core::director::PendingAddMembership {
                spawn_index: 2,
                boss_entity: EntityId(9001),
                archetype: "skeleton".to_string(),
                tags: vec!["add".to_string(), "melee".to_string()],
            },
            game_core::director::PendingAddMembership {
                spawn_index: 3,
                boss_entity: EntityId(9002),
                archetype: "caster".to_string(),
                tags: vec!["add".to_string(), "ranged".to_string()],
            },
        ];

        let pkg = build(result, vec![]);

        assert_eq!(pkg.encounter_memberships.len(), 2);
        assert_eq!(pkg.encounter_memberships[0].spawn_index, 2);
        assert_eq!(pkg.encounter_memberships[0].boss_entity, 9001);
        assert_eq!(pkg.encounter_memberships[0].archetype, "skeleton");
        assert_eq!(
            pkg.encounter_memberships[0].tags,
            vec!["add".to_string(), "melee".to_string()]
        );
        assert_eq!(pkg.encounter_memberships[1].spawn_index, 3);
        assert_eq!(pkg.encounter_memberships[1].boss_entity, 9002);
        assert_eq!(pkg.encounter_memberships[1].archetype, "caster");
        assert_eq!(
            pkg.encounter_memberships[1].tags,
            vec!["add".to_string(), "ranged".to_string()]
        );
    }

    #[test]
    fn build_preserves_shifted_spawn_index_offsets() {
        let mut result = make_tick_result(88);
        // TickPipeline shifts membership spawn_index by the number of
        // pre-existing director spawns before these are marshaled.
        result.encounter_memberships = vec![game_core::director::PendingAddMembership {
            spawn_index: 5,
            boss_entity: EntityId(7000),
            archetype: "brute".to_string(),
            tags: vec!["elite".to_string()],
        }];

        let pkg = build(result, vec![]);

        assert_eq!(pkg.encounter_memberships.len(), 1);
        assert_eq!(pkg.encounter_memberships[0].spawn_index, 5);
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
            npc_state_updates: Vec::new(),
            region_updates: Vec::new(),
            director_spawns: Vec::new(),
            encounter_memberships: Vec::new(),
            interactable_updates: Vec::new(),
            boss_phase_updates: Vec::new(),
            zone_counter_deltas: Vec::new(),
            death_state_inserts: Vec::new(),
            loot_rolls: Vec::new(),
            sim_warnings: Vec::new(),
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

    #[test]
    fn build_marshals_loot_rolls() {
        let mut result = make_tick_result(12);
        result.loot_rolls.push(game_core::loot::LootRollOutput {
            corpse_entity: EntityId(500),
            layer: 101,
            position: Vec3f {
                x: 4.0,
                y: 1.5,
                z: -2.0,
            },
            rolled_items: vec![game_core::loot::LootRollItem {
                item_id: 5,
                quantity: 2,
            }],
            eligible_claimants: vec![EntityId(10), EntityId(11)],
            claim_window_ticks: 6000,
        });

        let pkg = build(result, Vec::new());

        assert_eq!(pkg.loot_rolls.len(), 1);
        let roll = &pkg.loot_rolls[0];
        assert_eq!(roll.corpse_entity, 500);
        assert_eq!(roll.layer, 101);
        assert_eq!(roll.pos_x, 4.0);
        assert_eq!(roll.pos_y, 1.5);
        assert_eq!(roll.pos_z, -2.0);
        assert_eq!(roll.eligible_claimants, vec![10, 11]);
        assert_eq!(roll.claim_window_ticks, 6000);
        assert_eq!(roll.items.len(), 1);
        assert_eq!(roll.items[0].item_id, 5);
        assert_eq!(roll.items[0].quantity, 2);
    }

    #[test]
    fn build_marshals_director_spawn_with_layer() {
        use game_core::director::DirectorSpawn;

        let result = TickResult {
            tick_id: TickId(50),
            transforms: Vec::new(),
            events: Vec::new(),
            summary: Default::default(),
            entity_state_updates: Vec::new(),
            health_updates: Vec::new(),
            buff_updates: Vec::new(),
            npc_state_updates: Vec::new(),
            region_updates: Vec::new(),
            director_spawns: vec![
                DirectorSpawn {
                    kind: game_schema::EntityKind::Npc,
                    max_hp: 500.0,
                    position: Vec3f {
                        x: 10.0,
                        y: 1.0,
                        z: -5.0,
                    },
                    layer: 105,
                    npc_config: Some(game_core::director::DirectorNpcConfig {
                        passive: false,
                        no_chase: true,
                        ability_ids: vec![1, 124],
                        leash_radius: 20.0,
                        aggro_radius: 8.0,
                        body_shape: Some(game_schema::dungeon::BodyShapeDef::NpcCapsule.to_u8()),
                    }),
                    team_id: Some(2),
                },
                DirectorSpawn {
                    kind: game_schema::EntityKind::Npc,
                    max_hp: 100.0,
                    position: Vec3f {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    },
                    layer: 0,
                    npc_config: None,
                    team_id: None,
                },
            ],
            encounter_memberships: Vec::new(),
            interactable_updates: Vec::new(),
            boss_phase_updates: Vec::new(),
            zone_counter_deltas: Vec::new(),
            death_state_inserts: Vec::new(),
            loot_rolls: Vec::new(),
            sim_warnings: Vec::new(),
        };
        let pkg = build(result, Vec::new());

        assert_eq!(pkg.director_spawns.len(), 2);

        let s0 = &pkg.director_spawns[0];
        assert_eq!(s0.kind, game_schema::EntityKind::Npc);
        assert_eq!(s0.max_hp, 500.0);
        assert_eq!(s0.pos_x, 10.0);
        assert_eq!(s0.pos_y, 1.0);
        assert_eq!(s0.pos_z, -5.0);
        assert_eq!(s0.layer, 105);
        assert_eq!(s0.team_id, Some(2));
        let cfg = s0.npc_config.as_ref().expect("npc config should marshal");
        assert_eq!(cfg.ability_ids, vec![1, 124]);
        assert!(cfg.no_chase);
        assert_eq!(
            cfg.body_shape,
            Some(game_schema::dungeon::BodyShapeDef::NpcCapsule.to_u8())
        );

        let s1 = &pkg.director_spawns[1];
        assert_eq!(s1.layer, 0);
        assert!(s1.npc_config.is_none());
        assert_eq!(s1.team_id, None);
    }
}
