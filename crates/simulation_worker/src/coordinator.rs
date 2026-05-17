//! Simulation coordinator — connects to SpacetimeDB as a client and drives the tick pipeline.
//!
//! # Prerequisites
//!
//! 1. Generate module bindings:
//!    ```sh
//!    spacetime generate --lang rust \
//!        --out-dir crates/simulation_worker/src/module_bindings \
//!        --module-path crates/server_module
//!    ```
//!
//! 2. Build with the `connected` feature:
//!    ```sh
//!    cargo build -p simulation_worker --features connected
//!    ```
//!
//! # Architecture
//!
//! The coordinator:
//!   1. Connects to SpacetimeDB via WebSocket.
//!   2. Registers itself as a trusted worker (`register_worker` reducer).
//!   3. Subscribes to `sim_tick` and `player_intent` tables.
//!   4. On each `sim_tick` update, gathers pending intents, runs the tick pipeline,
//!      and calls `commit_tick_results` to push authoritative state.
//!
//! # Callback ownership
//!
//!   `subscription.on_applied` — Seed coordinator baselines only. No entity mutations.
//!   `entity.on_insert`        — Single spawn gate. `contains()` guard for idempotency.
//!   `entity.on_update`        — Mirror external lifecycle transitions only.
//!   `entity.on_delete`        — Force cleanup guard only.
//!   `sim_tick.on_insert`      — Single tick gate. Advance `last_processed_tick` only in the
//!                               async commit acknowledgement callback.  A `pending_commit_tick`
//!                               guard blocks new tick processing while a commit is in-flight.

use std::sync::{Arc, Mutex};
use std::path::Path;

use log::{error, info, warn};
use spacetimedb_sdk::{DbContext, Identity, Table, TableWithPrimaryKey};

use crate::commit_builder::{self, CommitPackage, CommitCombatEvent, CommitCombatEventKind, CommitWorldEventKind, CommitEntityStateKind, CommitBuff, CommitThreat, CommitNpcState};
use crate::entity_sync::EntitySync;
use crate::module_bindings::*;
use crate::physics::rapier_world::PhysicsWorld;
use crate::simulation_runner::SimulationRunner;
use game_core::combat::skill::{
    AbilityAction, AbilityData, AbilityRegistry, AbilityTimeline, ScheduledAbilityAction, SkillShape,
};
use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;

/// Configuration for connecting the worker to SpacetimeDB.
pub struct CoordinatorConfig {
    /// SpacetimeDB host URI (e.g. "http://localhost:3000").
    pub uri: String,
    /// Module name or Identity published to SpacetimeDB.
    pub module_name: String,
    /// Optional auth token for reconnecting with the same Identity.
    pub auth_token: Option<String>,
}

/// Shared mutable state accessed from subscription callbacks.
struct CoordinatorState {
    sim: SimulationRunner,
}

const TOKEN_FILE: &str = ".worker_token";

/// Load a previously-saved auth token from disk.
fn load_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_FILE).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Save the auth token so the worker keeps the same identity across restarts.
fn save_token(token: &str) {
    if let Err(e) = std::fs::write(TOKEN_FILE, token) {
        warn!("Failed to save auth token to {TOKEN_FILE}: {e}");
    } else {
        info!("Auth token saved to {TOKEN_FILE}");
    }
}

/// Start the coordinator loop. This blocks the current thread.
pub fn run(config: CoordinatorConfig) {
    let tick_dt = 1.0 / 20.0; // 20 Hz — must match server TickConfig
    let physics = PhysicsWorld::new(tick_dt);
    let abilities = load_abilities();

    let state = Arc::new(Mutex::new(CoordinatorState {
        sim: SimulationRunner::new(TickId(0), Box::new(physics), tick_dt, abilities),
    }));

    let state_for_connect = Arc::clone(&state);
    let state_for_tick = Arc::clone(&state);
    let state_for_entity = Arc::clone(&state);

    // Use persisted token if no explicit token provided.
    let auth_token = config.auth_token.or_else(load_token);
    if auth_token.is_some() && Path::new(TOKEN_FILE).exists() {
        info!("Using persisted auth token from {TOKEN_FILE}");
    }

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity: Identity, token: &str| {
            save_token(token);

            info!("========================================");
            info!("  Worker identity: {identity}");
            info!("========================================");
            info!("Register once with:");
            info!(r#"  spacetime call jump register_worker '{{"__identity__":"0x{identity}"}}' -s local"#);

            subscribe_to_tables(ctx, Arc::clone(&state_for_connect));
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(e) = err {
                error!("Disconnected with error: {e}");
            } else {
                info!("Disconnected gracefully");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    // Watch for sim_tick inserts — each new tick row triggers a pipeline run.
    //
    // The SpacetimeDB SDK docs explicitly state there is **no ordering guarantee**
    // between `on_insert` and `on_applied` callbacks for initial subscription rows.
    // Checking `ctx.event == Event::SubscribeApplied` is the documented pattern
    // (matching `ctx.Event is not Event<Reducer>.SubscribeApplied` in the C# tutorial)
    // to distinguish initial snapshot rows from live reducer-inserted rows.
    // Historical sim_tick rows must be skipped — they are already committed.
    conn.db.sim_tick().on_insert(move |ctx, new_tick| {
        // Initial subscription snapshot — skip, only process live tick reducer inserts.
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return;
        }
        let canonical_tick = new_tick.tick_id;

        // Gather intents targeting this tick from the client cache.
        // Collect (intent_id, pipeline_intent) together so we can delete consumed rows in commit.
        let (consumed_ids, intents): (Vec<u64>, Vec<game_protocol::intent::PlayerIntent>) = ctx
            .db
            .player_intent()
            .iter()
            .filter(|i| i.target_tick == canonical_tick)
            .map(|row| (row.intent_id, game_protocol::intent::PlayerIntent {
                entity_id: EntityId(row.entity_id),
                sequence_id: row.sequence_id,
                target_tick: TickId(row.target_tick),
                client_time_ms: row.client_time_ms,
                action: convert_intent_action(row.action.clone()),
            }))
            .unzip();

        // ── Begin locked section ────────────────────────────────────────
        // Acquire the lock for pipeline mutation (run_tick + marshal).
        // The lock is dropped *before* the async reducer call so that commit
        // acknowledgement, entity callbacks, and other SDK events are not
        // blocked while the server processes the commit.
        let pkg = {
            let mut guard = match state_for_tick.lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    error!("CoordinatorState lock poisoned — recovering");
                    poisoned.into_inner()
                }
            };

            // Delegate tick orchestration to SimulationRunner.
            let result = match guard.sim.run_tick(canonical_tick, &intents) {
                Ok(r) => r,
                Err(_) => return, // already logged by TickDriver
            };

            // Marshal TickResult into SDK-free CommitPackage.
            commit_builder::build(result, consumed_ids.clone())
        };
        // ── Lock released ───────────────────────────────────────────────

        // Convert CommitPackage into SDK wire types (1:1 mapping).
        let tick_id_raw = pkg.tick_id;
        let transforms = wire_transforms(&pkg);
        let health_updates = wire_health_updates(&pkg);
        let combat_events = wire_combat_events(&pkg);
        let world_events = wire_world_events(&pkg);
        let entity_state_updates = wire_entity_state_updates(&pkg);
        let buff_updates = wire_buff_updates(&pkg);
        let buff_cleared_entity_ids = pkg.buff_cleared_entity_ids.clone();
        let threat_updates = wire_threat_updates(&pkg);
        let threat_cleared_entity_ids = pkg.threat_cleared_entity_ids.clone();
        let npc_state_updates = wire_npc_state_updates(&pkg);

        // Commit results to SpacetimeDB via the acknowledgement-aware path.
        // `commit_tick_results_then` is fire-and-forget for the *send*; the
        // closure fires asynchronously when the server confirms (or rejects)
        // the reducer invocation.  `last_processed_tick` is advanced only
        // inside the success branch of the callback — never optimistically.
        let state_for_ack = Arc::clone(&state_for_tick);
        let send_result = ctx.reducers.commit_tick_results_then(
            tick_id_raw,
            transforms,
            health_updates,
            combat_events,
            world_events,
            pkg.consumed_intent_ids.clone(),
            entity_state_updates,
            wire_region_updates(&pkg),
            buff_updates,
            buff_cleared_entity_ids,
            threat_updates,
            threat_cleared_entity_ids,
            npc_state_updates,
            move |_rctx, outcome| {
                let mut guard = match state_for_ack.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in commit callback — recovering");
                        poisoned.into_inner()
                    }
                };

                match outcome {
                    Ok(Ok(())) => {
                        guard.sim.acknowledge_success(canonical_tick);
                    }
                    Ok(Err(reducer_err)) => {
                        guard.sim.acknowledge_failure(
                            canonical_tick,
                            &format!("reducer rejected: {reducer_err}"),
                        );
                    }
                    Err(internal_err) => {
                        guard.sim.acknowledge_failure(
                            canonical_tick,
                            &format!("internal error: {internal_err:?}"),
                        );
                    }
                }
            },
        );

        // Handle send failure (unable to enqueue the reducer call at all).
        if let Err(e) = send_result {
            let mut guard = match state_for_tick.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.sim.acknowledge_failure(
                canonical_tick,
                &format!("send failed: {e}"),
            );
        }

        // Periodically prune old combat/world event rows so the event tables don't
        // grow unbounded. Keep a 2-second window (40 ticks at 20 Hz) so clients
        // that are slightly behind still receive events before they are deleted.
        // Runs every 100 ticks (5 s) to keep the per-tick cost negligible.
        const EVENT_RETAIN_TICKS: u64 = 40;
        const EVENT_PRUNE_INTERVAL: u64 = 100;
        if canonical_tick % EVENT_PRUNE_INTERVAL == 0
            && canonical_tick > EVENT_RETAIN_TICKS
        {
            let before_tick = canonical_tick - EVENT_RETAIN_TICKS;
            if let Err(e) = ctx.reducers.clear_events(before_tick) {
                warn!("clear_events(before_tick={before_tick}) failed: {e}");
            }
        }
    });

    // entity.on_insert — thin adapter over EntitySync::sync_insert.
    // SDK cache reads happen before the lock; decision logic lives in EntitySync.
    conn.db.entity().on_insert(move |ctx, new_entity| {
        let eid = EntityId(new_entity.entity_id);
        let kind = convert_entity_kind(new_entity.kind);
        let state = convert_entity_state(new_entity.state);
        let tick = TickId(new_entity.spawned_at_tick);

        // Look up companion rows before acquiring the state lock — these reads
        // are from the SDK cache (no contention) and must not be done under the
        // lock to avoid holding it across I/O.
        let max_hp = ctx.db
            .entity_health()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|h| h.max_hp)
            .unwrap_or(100.0);

        let pos = ctx.db
            .entity_transform()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|t| game_protocol::types::Vec3f { x: t.pos_x, y: t.pos_y, z: t.pos_z })
            .unwrap_or(game_protocol::types::Vec3f { x: 0.0, y: 1.0, z: 0.0 });

        // Read runtime state from the SDK cache for restart continuity.
        let buffs: Vec<game_core::combat::status::ActiveBuff> = ctx.db
            .active_buff()
            .iter()
            .filter(|b| b.entity_id == new_entity.entity_id)
            .map(|b| game_core::combat::status::ActiveBuff {
                buff_id: b.buff_id,
                source: EntityId(b.source_entity),
                target: EntityId(b.entity_id),
                stacks: b.stacks,
                // max_stacks is static registry data not stored in the DB.
                // Use u32::MAX as an explicit "uncapped" sentinel — the correct
                // value is restored next time this buff is applied from combat.
                max_stacks: u32::MAX,
                expires_at: b.expires_at_tick.map(game_protocol::tick::TickId),
                modifiers: Default::default(),
            })
            .collect();

        let threats: Vec<game_core::combat::status::ThreatEntry> = ctx.db
            .threat_entry()
            .iter()
            .filter(|t| t.npc_entity == new_entity.entity_id)
            .map(|t| game_core::combat::status::ThreatEntry {
                source: EntityId(t.source_entity),
                threat: t.threat,
            })
            .collect();

        let npc_state: Option<(game_schema::NpcAiState, Option<EntityId>)> = ctx.db
            .npc_state()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|n| (convert_npc_ai_state(n.ai_state), n.target_entity.map(EntityId)));

        let mut guard = match state_for_entity.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                error!("CoordinatorState lock poisoned in entity.on_insert — recovering");
                poisoned.into_inner()
            }
        };
        EntitySync::sync_insert(&mut guard.sim, eid, kind, state, tick, max_hp, pos, buffs, threats, npc_state);
    });

    // entity.on_update — thin adapter over EntitySync::sync_update.
    let state_for_entity_update = Arc::clone(&state);
    conn.db.entity().on_update(move |_ctx, old_entity, new_entity| {
        let eid = EntityId(new_entity.entity_id);
        let old_state = convert_entity_state(old_entity.state);
        let new_state = convert_entity_state(new_entity.state);
        let mut guard = match state_for_entity_update.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                error!("CoordinatorState lock poisoned in entity.on_update — recovering");
                poisoned.into_inner()
            }
        };
        EntitySync::sync_update(&mut guard.sim, eid, old_state, new_state);
    });

    // entity.on_delete — thin adapter over EntitySync::sync_delete.
    let state_for_entity_delete = Arc::clone(&state);
    conn.db.entity().on_delete(move |_ctx, deleted_entity| {
        let eid = EntityId(deleted_entity.entity_id);
        let mut guard = match state_for_entity_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                error!("CoordinatorState lock poisoned in entity.on_delete — recovering");
                poisoned.into_inner()
            }
        };
        EntitySync::sync_delete(&mut guard.sim, eid);
    });

    // Block on the connection thread — the callbacks above drive the simulation.
    conn.run_threaded().join().expect("Connection thread panicked");
}

/// Subscribe to the tables the coordinator needs to observe.
///
/// Ordering note (SDK docs): The SpacetimeDB SDK makes **no ordering guarantee**
/// between `on_insert` (called for each initial row) and `on_applied`. To avoid
/// replaying historical sim_tick rows, `sim_tick.on_insert` checks
/// `ctx.event == Event::SubscribeApplied` and skips those rows entirely.
/// `on_applied` seeds `last_processed_tick` as defense-in-depth.
///
/// Entity spawning (`entity.on_insert`) processes initial rows normally — the
/// `contains()` guard in that callback provides idempotency against both
/// initial-subscription and live-insert events.
fn subscribe_to_tables(ctx: &DbConnection, state: Arc<Mutex<CoordinatorState>>) {
    ctx.subscription_builder()
        .on_applied(move |ctx| {
            // Seed last_processed_tick to the highest committed tick visible in the
            // subscription cache. This prevents historical sim_tick.on_insert callbacks
            // from replaying already-processed ticks after a worker restart.
            let max_tick = ctx.db.sim_tick().iter().map(|t| t.tick_id).max().unwrap_or(0);
            {
                let mut guard = match state.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in on_applied — recovering");
                        poisoned.into_inner()
                    }
                };
                guard.sim.seed(max_tick);
            }
            info!(
                "Subscription applied — {} sim_tick rows, {} intent rows, {} entity rows; seeding last_processed_tick={} pipeline_start_tick={}",
                ctx.db.sim_tick().count(),
                ctx.db.player_intent().count(),
                ctx.db.entity().count(),
                max_tick,
                max_tick + 1,
            );
            // Entity rows are materialised by entity.on_insert (idempotent via contains() guard).
            // sim_tick rows are skipped by the Event::SubscribeApplied guard in that callback.
        })
        .on_error(|_ctx, err| {
            error!("Subscription error: {err}");
        })
        .subscribe([
            "SELECT * FROM sim_tick",
            "SELECT * FROM player_intent",
            "SELECT * FROM entity",
            "SELECT * FROM entity_health",
            "SELECT * FROM entity_transform",
            "SELECT * FROM active_buff",
            "SELECT * FROM threat_entry",
            "SELECT * FROM npc_state",
        ]);
}

// ── Ability registry ───────────────────────────────────────────
//
// Hardcoded ability definitions for the initial vertical slice.
// All abilities use ability_id=1 by convention until a data loading
// pipeline (roadmap #4) replaces this with JSON/RON files.
//
// Timeline: 20 Hz ticks, so tick offsets map as follows:
//   offset 0  → SpawnHitbox (sensor created)
//   offset 0  → CooldownStart (20 ticks = 1 s cooldown)
//   offset 1  → ApplyDamageFrame (combat reads hitbox contacts)
//   offset 2  → RemoveHitbox (sensor freed)

/// On-disk serialization format for `data/abilities.ron`.
/// Both `AbilityData` and `AbilityTimeline` already derive `serde::Deserialize`.
#[derive(serde::Deserialize)]
struct AbilityFile {
    abilities: Vec<AbilityData>,
    timelines: Vec<AbilityTimeline>,
}

/// Load abilities from `data/abilities.ron`.
/// Falls back to the hardcoded registry if the file is missing or malformed.
fn load_abilities() -> AbilityRegistry {
    const PATH: &str = "data/abilities.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| ron::from_str::<AbilityFile>(&src).map_err(|e| format!("parse '{PATH}': {e}")));
    match result {
        Ok(file) => {
            let count = file.abilities.len();
            let mut reg = AbilityRegistry::new();
            for a in file.abilities { reg.register(a); }
            for t in file.timelines { reg.register_timeline(t); }
            info!("Loaded {count} ability/abilities from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using hardcoded fallback");
            build_ability_registry()
        }
    }
}

fn build_ability_registry() -> AbilityRegistry {
    let mut reg = AbilityRegistry::new();

    // Ability 1: Slash — fast melee capsule sweep.
    reg.register(AbilityData {
        ability_id: 1,
        name: "Slash".to_string(),
        base_damage: 25.0,
        damage_type: game_schema::DamageType::Physical,
        shape: SkillShape::CapsuleSweep,
        threat_multiplier: 1.0,
    });
    reg.register_timeline(AbilityTimeline {
        ability_id: 1,
        actions: vec![
            ScheduledAbilityAction {
                tick_offset: 0,
                action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::CapsuleSweep,
                    offset: game_schema::Vec3f { x: 0.0, y: 0.0, z: 0.0 },
                },
            },
            ScheduledAbilityAction {
                tick_offset: 0,
                action: AbilityAction::CooldownStart { duration_ticks: 20 },
            },
            ScheduledAbilityAction {
                tick_offset: 1,
                action: AbilityAction::ApplyDamageFrame,
            },
            ScheduledAbilityAction {
                tick_offset: 2,
                action: AbilityAction::RemoveHitbox,
            },
        ],
    });

    reg
}

// ── Binding conversions ─────────────────────────────────────────
//
// The generated bindings produce mirror types of game_schema types.
// We convert between them here since they are distinct Rust types.

/// Convert generated binding's IntentAction → game_protocol's IntentAction.
fn convert_intent_action(action: crate::module_bindings::IntentAction) -> game_protocol::intent::IntentAction {
    match action {
        crate::module_bindings::IntentAction::Move(d) => {
            game_protocol::intent::IntentAction::Move(game_schema::MoveDir {
                dir_x: d.dir_x,
                dir_y: d.dir_y,
                dir_z: d.dir_z,
            })
        }
        crate::module_bindings::IntentAction::UseAbility(u) => {
            game_protocol::intent::IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: u.ability_id,
                target: convert_ability_target(u.target),
            })
        }
        crate::module_bindings::IntentAction::Stop => game_protocol::intent::IntentAction::Stop,
        crate::module_bindings::IntentAction::FaceTo(d) => {
            game_protocol::intent::IntentAction::FaceTo(game_schema::MoveDir {
                dir_x: d.dir_x,
                dir_y: d.dir_y,
                dir_z: d.dir_z,
            })
        }
        crate::module_bindings::IntentAction::Interact(id) => {
            game_protocol::intent::IntentAction::Interact(id)
        }
    }
}

fn convert_ability_target(target: crate::module_bindings::AbilityTarget) -> game_schema::AbilityTarget {
    match target {
        crate::module_bindings::AbilityTarget::None => game_schema::AbilityTarget::None,
        crate::module_bindings::AbilityTarget::Entity(id) => game_schema::AbilityTarget::Entity(id),
        crate::module_bindings::AbilityTarget::Position(v) => {
            game_schema::AbilityTarget::Position(game_schema::Vec3f { x: v.x, y: v.y, z: v.z })
        }
        crate::module_bindings::AbilityTarget::Direction(v) => {
            game_schema::AbilityTarget::Direction(game_schema::Vec3f { x: v.x, y: v.y, z: v.z })
        }
    }
}

/// Convert generated binding's EntityKind → game_schema's EntityKind.
fn convert_entity_kind(kind: crate::module_bindings::EntityKind) -> game_schema::EntityKind {
    match kind {
        crate::module_bindings::EntityKind::Player => game_schema::EntityKind::Player,
        crate::module_bindings::EntityKind::Npc => game_schema::EntityKind::Npc,
        crate::module_bindings::EntityKind::Projectile => game_schema::EntityKind::Projectile,
        crate::module_bindings::EntityKind::Hazard => game_schema::EntityKind::Hazard,
        crate::module_bindings::EntityKind::Boss => game_schema::EntityKind::Boss,
    }
}

fn convert_entity_state(state: crate::module_bindings::EntityState) -> game_schema::EntityState {
    match state {
        crate::module_bindings::EntityState::Spawning => game_schema::EntityState::Spawning,
        crate::module_bindings::EntityState::Active => game_schema::EntityState::Active,
        crate::module_bindings::EntityState::DespawnPending => game_schema::EntityState::DespawnPending,
        crate::module_bindings::EntityState::Removed => game_schema::EntityState::Removed,
    }
}

fn convert_npc_ai_state(state: crate::module_bindings::NpcAiState) -> game_schema::NpcAiState {
    match state {
        crate::module_bindings::NpcAiState::Idle => game_schema::NpcAiState::Idle,
        crate::module_bindings::NpcAiState::Patrol => game_schema::NpcAiState::Patrol,
        crate::module_bindings::NpcAiState::Combat => game_schema::NpcAiState::Combat,
        crate::module_bindings::NpcAiState::Flee => game_schema::NpcAiState::Flee,
        crate::module_bindings::NpcAiState::Scripted => game_schema::NpcAiState::Scripted,
    }
}

// ── CommitPackage → SDK wire type conversions ───────────────────
//
// Each function does a 1:1 structural mapping from the SDK-free
// intermediate types in `commit_builder` to the generated SDK bindings.

fn wire_transforms(pkg: &CommitPackage) -> Vec<TransformUpdate> {
    pkg.transforms
        .iter()
        .map(|t| TransformUpdate {
            entity_id: t.entity_id,
            pos_x: t.pos_x, pos_y: t.pos_y, pos_z: t.pos_z,
            rot_x: t.rot_x, rot_y: t.rot_y, rot_z: t.rot_z, rot_w: t.rot_w,
            vel_x: t.vel_x, vel_y: t.vel_y, vel_z: t.vel_z,
            angvel_x: t.angvel_x, angvel_y: t.angvel_y, angvel_z: t.angvel_z,
        })
        .collect()
}

fn wire_health_updates(pkg: &CommitPackage) -> Vec<HealthUpdate> {
    pkg.health_updates
        .iter()
        .map(|h| HealthUpdate {
            entity_id: h.entity_id,
            hp: h.hp,
            max_hp: h.max_hp,
        })
        .collect()
}

fn wire_combat_events(pkg: &CommitPackage) -> Vec<CombatEventInput> {
    pkg.combat_events.iter().map(wire_combat_event).collect()
}

fn wire_combat_event(e: &CommitCombatEvent) -> CombatEventInput {
    CombatEventInput {
        source_entity: e.source_entity,
        target_entity: e.target_entity,
        event_sequence: e.event_sequence,
        event_kind: match &e.event_kind {
            CommitCombatEventKind::Damage(d) => CombatEventKind::Damage(DamageData {
                amount: d.amount,
                damage_type: wire_damage_type(d.damage_type),
            }),
            CommitCombatEventKind::SkillHit(id) => CombatEventKind::SkillHit(*id),
            CommitCombatEventKind::BuffApplied(b) => CombatEventKind::BuffApplied(BuffAppliedData {
                buff_id: b.buff_id,
                duration_ticks: b.duration_ticks,
            }),
            CommitCombatEventKind::BuffExpired(id) => CombatEventKind::BuffExpired(*id),
            CommitCombatEventKind::EntityDied(k) => CombatEventKind::EntityDied(*k),
        },
    }
}

fn wire_world_events(pkg: &CommitPackage) -> Vec<WorldEventInput> {
    pkg.world_events
        .iter()
        .map(|e| WorldEventInput {
            entity_id: e.entity_id,
            event_sequence: e.event_sequence,
            event_kind: match &e.event_kind {
                CommitWorldEventKind::EntityDespawned => WorldEventKind::EntityDespawned,
                CommitWorldEventKind::PickupCollected(id) => WorldEventKind::PickupCollected(*id),
                CommitWorldEventKind::InteractTriggered(target) => WorldEventKind::InteractTriggered(*target),
            },
        })
        .collect()
}

fn wire_entity_state_updates(pkg: &CommitPackage) -> Vec<EntityStateUpdate> {
    pkg.entity_state_updates
        .iter()
        .map(|u| EntityStateUpdate {
            entity_id: u.entity_id,
            new_state: match u.new_state {
                CommitEntityStateKind::Spawning => EntityState::Spawning,
                CommitEntityStateKind::Active => EntityState::Active,
                CommitEntityStateKind::DespawnPending => EntityState::DespawnPending,
                CommitEntityStateKind::Removed => EntityState::Removed,
            },
        })
        .collect()
}

fn wire_region_updates(pkg: &CommitPackage) -> Vec<RegionUpdate> {
    pkg.region_updates
        .iter()
        .map(|r| RegionUpdate {
            entity_id: r.entity_id,
            region_x: r.region_x,
            region_z: r.region_z,
            layer: r.layer,
        })
        .collect()
}

fn wire_buff_updates(pkg: &CommitPackage) -> Vec<BuffUpdate> {
    pkg.buff_updates
        .iter()
        .map(|b| BuffUpdate {
            entity_id: b.entity_id,
            buff_id: b.buff_id,
            source_entity: b.source_entity,
            stacks: b.stacks,
            expires_at_tick: b.expires_at_tick,
        })
        .collect()
}

fn wire_threat_updates(pkg: &CommitPackage) -> Vec<ThreatUpdate> {
    pkg.threat_updates
        .iter()
        .map(|t| ThreatUpdate {
            npc_entity: t.npc_entity,
            source_entity: t.source_entity,
            threat: t.threat,
        })
        .collect()
}

fn wire_npc_state_updates(pkg: &CommitPackage) -> Vec<NpcStateUpdate> {
    pkg.npc_state_updates
        .iter()
        .map(|n| NpcStateUpdate {
            entity_id: n.entity_id,
            ai_state: wire_npc_ai_state(n.ai_state),
            target_entity: n.target_entity,
        })
        .collect()
}

fn wire_npc_ai_state(state: game_schema::NpcAiState) -> NpcAiState {
    match state {
        game_schema::NpcAiState::Idle => NpcAiState::Idle,
        game_schema::NpcAiState::Patrol => NpcAiState::Patrol,
        game_schema::NpcAiState::Combat => NpcAiState::Combat,
        game_schema::NpcAiState::Flee => NpcAiState::Flee,
        game_schema::NpcAiState::Scripted => NpcAiState::Scripted,
    }
}

fn wire_damage_type(dt: game_schema::DamageType) -> DamageType {
    match dt {
        game_schema::DamageType::Physical => DamageType::Physical,
        game_schema::DamageType::Magical => DamageType::Magical,
        game_schema::DamageType::True => DamageType::True,
    }
}
