//! SpacetimeDB-facing coordinator for the simulation worker.
//!
//! Owns connection/subscription callbacks, gathers intents for each canonical
//! tick, delegates simulation to `SimulationRunner`, and forwards commits via
//! reducers with retry/ack handling.

use std::path::Path;
use std::sync::{Arc, Mutex};

use log::{debug, error, info, warn};
use spacetimedb_sdk::{DbContext, Identity, Table, TableWithPrimaryKey};

use crate::commit_authority::FailureAction;
use crate::commit_builder::{
    self, CommitCombatEvent, CommitCombatEventKind, CommitEntityStateKind, CommitPackage,
    CommitWorldEventKind,
};
use crate::entity_sync::EntitySync;
use crate::module_bindings::*;
use crate::physics::rapier_world::PhysicsWorld;
use crate::simulation_runner::SimulationRunner;
use game_core::combat::skill::{
    AbilityAction, AbilityData, AbilityFile, AbilityRegistry, AbilityTimeline, ScheduledAbilityAction,
    SkillShape,
};
use game_core::combat::status::BuffRegistry;
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
    items: game_core::stats::ItemRegistry,
    dungeons: game_core::dungeon::DungeonRegistry,
}

/// Send (or re-send) a commit payload to SpacetimeDB.
///
/// On transient failure the function retries immediately from the ack
/// callback — no dependency on `sim_tick.on_insert`, so it cannot
/// deadlock with server-side backpressure.
///
/// On `FailureAction::Exhausted` (all retries spent) the process exits
/// so the supervisor can restart the worker with a clean reseed.
fn send_commit(reducers: &RemoteReducers, pkg: CommitPackage, state: Arc<Mutex<CoordinatorState>>) {
    let tick_id = pkg.tick_id;
    let transforms = wire_transforms(&pkg);
    let health_updates = wire_health_updates(&pkg);
    let combat_events = wire_combat_events(&pkg);
    let world_events = wire_world_events(&pkg);
    let consumed_intent_ids = pkg.consumed_intent_ids.clone();
    let entity_state_updates = wire_entity_state_updates(&pkg);
    let region_updates = wire_region_updates(&pkg);
    let buff_updates = wire_buff_updates(&pkg);
    let buff_cleared_entity_ids = pkg.buff_cleared_entity_ids.clone();
    let threat_updates = wire_threat_updates(&pkg);
    let threat_cleared_entity_ids = pkg.threat_cleared_entity_ids.clone();
    let npc_state_updates = wire_npc_state_updates(&pkg);
    let director_spawns = wire_director_spawns(&pkg);
    let interactable_updates = wire_interactable_updates(&pkg);

    let state_for_ack = Arc::clone(&state);

    let send_result = reducers.commit_tick_results_then(
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
        director_spawns,
        interactable_updates,
        move |rctx, outcome| {
            let reason = match &outcome {
                Ok(Ok(())) => {
                    let mut guard = state_for_ack.lock().unwrap_or_else(|p| p.into_inner());
                    guard.sim.acknowledge_success(tick_id);
                    return;
                }
                Ok(Err(reducer_err)) => format!("reducer rejected: {reducer_err}"),
                Err(internal_err) => format!("internal error: {internal_err:?}"),
            };

            let action = {
                let mut guard = state_for_ack.lock().unwrap_or_else(|p| p.into_inner());
                guard.sim.acknowledge_failure(tick_id, &reason)
            };

            match action {
                FailureAction::Retry => {
                    send_commit(&rctx.reducers, pkg, state_for_ack);
                }
                FailureAction::Exhausted => {
                    error!(
                        "tick={tick_id} commit permanently failed — \
                         crashing for clean reseed"
                    );
                    std::process::exit(1);
                }
            }
        },
    );

    // Handle send failure (unable to enqueue the reducer call at all).
    if let Err(e) = send_result {
        let action = {
            let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .sim
                .acknowledge_failure(tick_id, &format!("send failed: {e}"))
        };

        match action {
            FailureAction::Retry => {
                warn!("tick={tick_id} send failed, retrying immediately");
                // Cannot retry send without a reducers reference (the
                // original borrow is from the on_insert ctx which we
                // can't capture here).  Crash for reseed — the send
                // path failing means the connection is likely broken.
                error!(
                    "tick={tick_id} send failed and cannot retry in-band — \
                     crashing for clean reseed"
                );
                std::process::exit(1);
            }
            FailureAction::Exhausted => {
                error!(
                    "tick={tick_id} send permanently failed — \
                     crashing for clean reseed"
                );
                std::process::exit(1);
            }
        }
    }
}

const TOKEN_FILE: &str = ".worker_token";

/// Load a previously-saved auth token from disk.
fn load_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    let items = load_items();
    let buffs = load_buffs();
    let dungeons = load_dungeons();

    let state = Arc::new(Mutex::new(CoordinatorState {
        sim: SimulationRunner::new(TickId(0), Box::new(physics), tick_dt, abilities, buffs),
        items,
        dungeons,
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

        // Gather intents up to the triggering canonical tick from the client cache.
        // If a later sim_tick callback arrives after a skipped insert, the runner will
        // process the next contiguous tick and ignore future-targeted intents for now.
        let pending_intents: Vec<(u64, game_protocol::intent::PlayerIntent)> = ctx
            .db
            .player_intent()
            .iter()
            .filter(|i| i.target_tick <= canonical_tick)
            .map(|row| {
                (
                    row.intent_id,
                    game_protocol::intent::PlayerIntent {
                        entity_id: EntityId(row.entity_id),
                        sequence_id: row.sequence_id,
                        target_tick: TickId(row.target_tick),
                        client_observed_tick: row.client_observed_tick,
                        action: convert_intent_action(row.action.clone()),
                    },
                )
            })
            .collect();
        let intents: Vec<_> = pending_intents
            .iter()
            .map(|(_, intent)| intent.clone())
            .collect();

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
            let consumed_ids: Vec<u64> = pending_intents
                .iter()
                .filter(|(_, intent)| intent.target_tick <= result.tick_id)
                .map(|(intent_id, _)| *intent_id)
                .collect();

            commit_builder::build(result, consumed_ids)
        };
        // ── Lock released ───────────────────────────────────────────────

        // Send the commit via the retry-aware path.  On transient failure
        // the ack callback re-sends immediately; on exhaustion the process
        // exits for a clean supervisor reseed.
        send_commit(&ctx.reducers, pkg, Arc::clone(&state_for_tick));

        // Periodically prune old combat/world event rows so the event tables don't
        // grow unbounded. Keep a 2-second window (40 ticks at 20 Hz) so clients
        // that are slightly behind still receive events before they are deleted.
        // Runs every 100 ticks (5 s) to keep the per-tick cost negligible.
        const EVENT_RETAIN_TICKS: u64 = 40;
        const EVENT_PRUNE_INTERVAL: u64 = 100;
        if canonical_tick % EVENT_PRUNE_INTERVAL == 0 && canonical_tick > EVENT_RETAIN_TICKS {
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

        // Skip terminal entities early — on restart the subscription snapshot
        // includes Removed rows whose companion tables have already been cleaned
        // up.  Warn-and-default below would be noisy for these.
        if matches!(state, game_schema::EntityState::Removed | game_schema::EntityState::DespawnPending) {
            debug!("entity {} is {:?} in snapshot — skipping", new_entity.entity_id, state);
            return;
        }

        // Look up companion rows before acquiring the state lock — these reads
        // are from the SDK cache (no contention) and must not be done under the
        // lock to avoid holding it across I/O.
        let max_hp = ctx.db
            .entity_health()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|h| h.max_hp)
            .unwrap_or_else(|| {
                warn!("entity {} has no entity_health companion row at spawn — defaulting to 100 hp", new_entity.entity_id);
                100.0
            });

        let pos = ctx.db
            .entity_transform()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|t| game_protocol::types::Vec3f { x: t.pos_x, y: t.pos_y, z: t.pos_z })
            .unwrap_or_else(|| {
                warn!("entity {} has no entity_transform companion row at spawn — defaulting to origin", new_entity.entity_id);
                game_protocol::types::Vec3f { x: 0.0, y: 1.0, z: 0.0 }
            });

        // Read runtime state from the SDK cache for restart continuity.
        let buffs: Vec<game_core::combat::status::ActiveBuff> = ctx.db
            .active_buff()
            .iter()
            .filter(|b| b.entity_id == new_entity.entity_id)
            .map(|b| {
                use game_core::combat::status::{AiOverride, BuffModifiers};
                let ai_override = match b.mod_ai_override_kind {
                    Some(0) => Some(AiOverride::ForceFlee),
                    Some(1) => Some(AiOverride::ForceIdle),
                    Some(2) => Some(AiOverride::ForceFocus {
                        target: EntityId(b.mod_ai_override_target.unwrap_or(0)),
                    }),
                    _ => None,
                };
                game_core::combat::status::ActiveBuff {
                    buff_id: b.buff_id,
                    source: EntityId(b.source_entity),
                    target: EntityId(b.entity_id),
                    buff_kind: Default::default(),
                    stacks: b.stacks,
                    // max_stacks is static registry data not stored in the DB.
                    // Use u32::MAX as an explicit "uncapped" sentinel — the correct
                    // value is restored next time this buff is applied from combat.
                    max_stacks: u32::MAX,
                    expires_at: b.expires_at_tick.map(game_protocol::tick::TickId),
                    modifiers: BuffModifiers {
                        damage_out_pct: b.mod_damage_out_pct,
                        damage_in_pct: b.mod_damage_in_pct,
                        cooldown_reduce_pct: b.mod_cooldown_reduce_pct,
                        speed_pct: b.mod_speed_pct,
                        ai_override,
                        root: b.mod_root,
                        stealth: b.mod_stealth,
                        ..Default::default()
                    },
                    last_dot_tick: None,
                }
            })
            .collect();

        let npc_state: Option<(game_schema::NpcAiState, Option<EntityId>)> = ctx.db
            .npc_state()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|n| (convert_npc_ai_state(n.ai_state), n.target_entity.map(EntityId)));

        let npc_config: Option<crate::entity_sync::NpcSpawnConfig> = ctx.db
            .npc_config()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|c| {
                let mut ability_ids: Vec<u32> = Vec::new();
                if let Some(id) = c.ability_id_1 { ability_ids.push(id); }
                if let Some(id) = c.ability_id_2 { ability_ids.push(id); }
                if let Some(id) = c.ability_id_3 { ability_ids.push(id); }
                if let Some(id) = c.ability_id_4 { ability_ids.push(id); }
                crate::entity_sync::NpcSpawnConfig {
                    passive: c.passive,
                    no_chase: c.no_chase,
                    ability_ids,
                    leash_radius: c.leash_radius,
                    aggro_radius: c.aggro_radius,
                }
            });

        let mut guard = match state_for_entity.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                error!("CoordinatorState lock poisoned in entity.on_insert — recovering");
                poisoned.into_inner()
            }
        };
        EntitySync::sync_insert(
            &mut guard.sim,
            eid,
            kind,
            state,
            tick,
            max_hp,
            pos,
            crate::entity_sync::RuntimeSnapshot {
                buffs,
                npc_state,
                npc_config,
                ..Default::default()
            },
        );
    });

    // entity.on_update — thin adapter over EntitySync::sync_update.
    // On Respawn result, reads companion rows and delegates to sync_insert.
    let state_for_entity_update = Arc::clone(&state);
    conn.db
        .entity()
        .on_update(move |ctx, old_entity, new_entity| {
            let eid = EntityId(new_entity.entity_id);
            let old_state = convert_entity_state(old_entity.state);
            let new_state = convert_entity_state(new_entity.state);

            // First pass: sync_update under the lock to get the result.
            let result = {
                let mut guard = match state_for_entity_update.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in entity.on_update — recovering");
                        poisoned.into_inner()
                    }
                };
                EntitySync::sync_update(&mut guard.sim, eid, old_state, new_state)
            };

            if result == crate::entity_sync::SyncUpdateResult::Respawn {
                // Read companion rows from SDK cache *before* reacquiring the
                // lock — same pattern as on_insert to keep the critical section
                // minimal.
                let kind = convert_entity_kind(new_entity.kind);
                let tick = TickId(new_entity.spawned_at_tick);

                let max_hp = ctx
                    .db
                    .entity_health()
                    .entity_id()
                    .find(&new_entity.entity_id)
                    .map(|h| h.max_hp)
                    .unwrap_or(100.0);

                let pos = ctx
                    .db
                    .entity_transform()
                    .entity_id()
                    .find(&new_entity.entity_id)
                    .map(|t| game_protocol::types::Vec3f {
                        x: t.pos_x,
                        y: t.pos_y,
                        z: t.pos_z,
                    })
                    .unwrap_or(game_protocol::types::Vec3f {
                        x: 0.0,
                        y: 1.0,
                        z: 0.0,
                    });

                let snapshot = crate::entity_sync::RuntimeSnapshot::default();

                // Reacquire lock only for the sync_insert mutation.
                let mut guard = match state_for_entity_update.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in entity.on_update (respawn) — recovering");
                        poisoned.into_inner()
                    }
                };
                EntitySync::sync_insert(
                    &mut guard.sim,
                    eid,
                    kind,
                    new_state,
                    tick,
                    max_hp,
                    pos,
                    snapshot,
                );
                info!(
                    "Entity {} respawned via sync_insert after on_update Respawn signal",
                    eid.0
                );
            }
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

    // ── Instance lifecycle — spawn/despawn environment colliders ─────
    // When the server creates an instance, the worker spawns parentless
    // environment colliders (Pass 1) from the dungeon template. Prop
    // entities for interactables are already created server-side (Pass 2).
    let state_for_instance_insert = Arc::clone(&state);
    conn.db.instance().on_insert(move |ctx, inst| {
        // Skip initial subscription snapshot — existing instances are already
        // running (or expired). Only react to live inserts.
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return;
        }
        if inst.state != crate::module_bindings::InstanceState::Active
            && inst.state != crate::module_bindings::InstanceState::Pending
        {
            return;
        }
        let mut guard = match state_for_instance_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(template) = guard.dungeons.get(&inst.template_id) {
            let template = template.clone();
            let layer = inst.layer;
            for geo in &template.geometry {
                use game_core::physics_backend::EnvironmentShape;
                let shape = match &geo.shape {
                    game_schema::dungeon::ShapeDef::Cuboid { half_x, half_y, half_z } => {
                        EnvironmentShape::Cuboid { half_x: *half_x, half_y: *half_y, half_z: *half_z }
                    }
                    game_schema::dungeon::ShapeDef::Cylinder { half_height, radius } => {
                        EnvironmentShape::Cylinder { half_height: *half_height, radius: *radius }
                    }
                };
                let pos = game_protocol::types::Vec3f {
                    x: geo.position[0],
                    y: geo.position[1],
                    z: geo.position[2],
                };
                guard.sim.physics_mut().add_environment_collider_on_layer(shape, pos, layer);
            }
            info!(
                "Instance {} (template={}): spawned {} environment colliders on layer {}",
                inst.instance_id, inst.template_id, template.geometry.len(), layer
            );
        } else {
            warn!(
                "Instance {} references unknown template '{}' — no geometry spawned",
                inst.instance_id, inst.template_id
            );
        }
    });

    // On instance update → Expired: remove environment colliders for that layer.
    let state_for_instance_update = Arc::clone(&state);
    conn.db.instance().on_update(move |_ctx, old_inst, new_inst| {
        // Only act when state transitions to Expired.
        if old_inst.state == new_inst.state {
            return;
        }
        if new_inst.state != crate::module_bindings::InstanceState::Expired {
            return;
        }
        let mut guard = match state_for_instance_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.physics_mut().remove_environment_colliders_by_layer(new_inst.layer);
        info!(
            "Instance {} expired: removed environment colliders from layer {}",
            new_inst.instance_id, new_inst.layer
        );
    });

    // ── Equipment bridge ────────────────────────────────────────────
    // Observe player_equipment changes to trigger stat recalculation.
    // These callbacks fire when a client calls equip_item / unequip_item
    // reducers. The coordinator queues a stat recalc on SimulationRunner,
    // which applies it before the next tick's Phase 1.

    // ── Interactable config bridge ──────────────────────────────────
    // Populate the sim-side interactable map from DB subscription so
    // handle_interact can branch by kind and toggle gate colliders.

    let state_for_interact_insert = Arc::clone(&state);
    conn.db.interactable_config().on_insert(move |_ctx, row| {
        let mut guard = match state_for_interact_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let info = convert_interactable_info(&row);
        guard.sim.pipeline.state.interactables.insert(EntityId(row.entity_id), info);
    });

    let state_for_interact_update = Arc::clone(&state);
    conn.db.interactable_config().on_update(move |_ctx, _old, row| {
        let mut guard = match state_for_interact_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let info = convert_interactable_info(&row);
        guard.sim.pipeline.state.interactables.insert(EntityId(row.entity_id), info);
    });

    // TODO: add on_delete callback to remove stale entries from
    // sim.pipeline.state.interactables when interactable_config rows
    // are deleted at runtime (e.g. instance teardown).

    let state_for_equip_insert = Arc::clone(&state);
    conn.db.player_equipment().on_insert(move |ctx, row| {
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return; // Initial snapshot — stats will be seeded from full state
        }
        let eid = EntityId(row.owner_entity);
        info!(
            "player_equipment.on_insert: entity={} item={} slot={:?}",
            row.owner_entity, row.item_id, row.slot
        );
        let mut guard = match state_for_equip_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        recompute_equipment(&ctx.db, &mut guard, eid);
    });

    let state_for_equip_update = Arc::clone(&state);
    conn.db
        .player_equipment()
        .on_update(move |ctx, _old, new_row| {
            let eid = EntityId(new_row.owner_entity);
            info!(
                "player_equipment.on_update: entity={} item={} slot={:?}",
                new_row.owner_entity, new_row.item_id, new_row.slot
            );
            let mut guard = match state_for_equip_update.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            recompute_equipment(&ctx.db, &mut guard, eid);
        });

    let state_for_equip_delete = Arc::clone(&state);
    conn.db.player_equipment().on_delete(move |ctx, old_row| {
        let eid = EntityId(old_row.owner_entity);
        info!(
            "player_equipment.on_delete: entity={} item={} slot={:?}",
            old_row.owner_entity, old_row.item_id, old_row.slot
        );
        let mut guard = match state_for_equip_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        recompute_equipment(&ctx.db, &mut guard, eid);
    });

    // Block on the connection thread — the callbacks above drive the simulation.
    conn.run_threaded()
        .join()
        .expect("Connection thread panicked");
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
            "SELECT * FROM npc_state",
            "SELECT * FROM player_equipment",
            "SELECT * FROM instance",
            "SELECT * FROM interactable_config",
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

/// Load abilities from `data/abilities.ron`.
/// Falls back to the hardcoded registry if the file is missing or malformed.
fn load_abilities() -> AbilityRegistry {
    const PATH: &str = "data/abilities.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<AbilityFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.abilities.len();
            let mut reg = AbilityRegistry::new();
            for a in file.abilities {
                reg.register(a);
            }
            for t in file.timelines {
                reg.register_timeline(t);
            }
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
        on_hit_buffs: vec![],
        knockback_force: 0.0,
        allow_reentry: false,
        charge_tiers: None,
        charge_roots_while_charging: true,
        damage_interval_ticks: 0,
        pierce: false,
        knockdown_ticks: 0,
        stun_ticks: 0,
        pull_force: 0.0,
        launch_lift: 0.0,
        launch_recovery_ticks: 0,
        usable_while_cc: false,
        fear_ticks: 0,
        silence_ticks: 0,
        sleep_ticks: 0,
        max_range: Option::None,
        projectile_speed: Option::None,
        targeting_mode: game_core::combat::skill::TargetingMode::DirectionTarget,
        cast_facing_policy: game_core::combat::skill::CastFacingPolicy::FaceAimDirection,
        lock_on_timeout_ticks: None,
        max_rewind_ticks: None,
    });
    reg.register_timeline(AbilityTimeline {
        ability_id: 1,
        actions: vec![
            ScheduledAbilityAction {
                tick_offset: 0,
                action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::CapsuleSweep,
                    offset: game_schema::Vec3f {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    },
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

// ── Item registry ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ItemFile {
    items: Vec<game_core::stats::ItemData>,
}

/// Load item definitions from `data/items.ron`.
/// Falls back to the hardcoded registry if the file is missing or malformed.
fn load_items() -> game_core::stats::ItemRegistry {
    const PATH: &str = "data/items.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<ItemFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.items.len();
            let mut reg = game_core::stats::ItemRegistry::new();
            for item in file.items {
                reg.register(item);
            }
            info!("Loaded {count} item(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using hardcoded fallback");
            build_item_registry()
        }
    }
}

fn build_item_registry() -> game_core::stats::ItemRegistry {
    use game_core::stats::{EquipmentModifiers, ItemData, ItemRegistry};
    let mut reg = ItemRegistry::new();
    reg.register(ItemData {
        item_id: 1,
        name: "Rusty Sword".into(),
        modifiers: EquipmentModifiers {
            attack_power: 0.2,
            ..EquipmentModifiers::default()
        },
    });
    reg.register(ItemData {
        item_id: 2,
        name: "Leather Vest".into(),
        modifiers: EquipmentModifiers {
            max_hp: 20.0,
            damage_in: -0.05,
            ..EquipmentModifiers::default()
        },
    });
    reg
}

// ── Buff registry ───────────────────────────────────────────────

/// Load buff definitions from `data/buffs.ron`.
/// Falls back to an empty registry if the file is missing or malformed.
fn load_buffs() -> BuffRegistry {
    use game_core::combat::status::BuffFile;
    const PATH: &str = "data/buffs.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<BuffFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.buffs.len();
            let mut reg = BuffRegistry::new();
            for b in file.buffs {
                reg.register(b);
            }
            info!("Loaded {count} buff(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using empty buff registry");
            BuffRegistry::new()
        }
    }
}

// ── Dungeon registry ────────────────────────────────────────────

/// Load dungeon templates from `data/dungeons.ron`.
/// Falls back to an empty registry if the file is missing or malformed.
fn load_dungeons() -> game_core::dungeon::DungeonRegistry {
    use game_core::dungeon::{DungeonFile, DungeonRegistry};
    const PATH: &str = "data/dungeons.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<DungeonFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.templates.len();
            let mut reg = DungeonRegistry::new();
            for t in file.templates {
                reg.register(t);
            }
            info!("Loaded {count} dungeon template(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using empty dungeon registry");
            DungeonRegistry::new()
        }
    }
}

/// Aggregate equipment modifiers for `entity_id` from the SDK cache and push
/// the result to the simulation runner.
fn recompute_equipment(
    db: &crate::module_bindings::RemoteTables,
    state: &mut CoordinatorState,
    entity_id: EntityId,
) {
    let modifiers = game_core::stats::EquipmentModifiers::aggregate(
        db.player_equipment()
            .iter()
            .filter(|row| row.owner_entity == entity_id.0)
            .map(|row| state.items.modifiers(row.item_id)),
    );
    state.sim.update_equipment(entity_id, modifiers);
}

// ── Binding conversions ─────────────────────────────────────────
//
// The generated bindings produce mirror types of game_schema types.
// We convert between them here since they are distinct Rust types.

/// Convert generated binding's IntentAction → game_protocol's IntentAction.
fn convert_intent_action(
    action: crate::module_bindings::IntentAction,
) -> game_protocol::intent::IntentAction {
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
                target_hint: u.target_hint,
            })
        }
        crate::module_bindings::IntentAction::ReleaseAbility(id) => {
            game_protocol::intent::IntentAction::ReleaseAbility(id)
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
        crate::module_bindings::IntentAction::Block(d) => {
            game_protocol::intent::IntentAction::Block(game_schema::BlockData {
                look_dir: game_schema::MoveDir {
                    dir_x: d.look_dir.dir_x,
                    dir_y: d.look_dir.dir_y,
                    dir_z: d.look_dir.dir_z,
                },
            })
        }
        crate::module_bindings::IntentAction::Jump => game_protocol::intent::IntentAction::Jump,
        crate::module_bindings::IntentAction::WeaponSwap => {
            game_protocol::intent::IntentAction::WeaponSwap
        }
        crate::module_bindings::IntentAction::TagTarget(id) => {
            game_protocol::intent::IntentAction::TagTarget(id)
        }
    }
}

fn convert_ability_target(
    target: crate::module_bindings::AbilityTarget,
) -> game_schema::AbilityTarget {
    match target {
        crate::module_bindings::AbilityTarget::None => game_schema::AbilityTarget::None,
        crate::module_bindings::AbilityTarget::Entity(id) => game_schema::AbilityTarget::Entity(id),
        crate::module_bindings::AbilityTarget::Position(v) => {
            game_schema::AbilityTarget::Position(game_schema::Vec3f {
                x: v.x,
                y: v.y,
                z: v.z,
            })
        }
        crate::module_bindings::AbilityTarget::Direction(v) => {
            game_schema::AbilityTarget::Direction(game_schema::Vec3f {
                x: v.x,
                y: v.y,
                z: v.z,
            })
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
        crate::module_bindings::EntityKind::Prop => game_schema::EntityKind::Prop,
    }
}

fn convert_entity_state(state: crate::module_bindings::EntityState) -> game_schema::EntityState {
    match state {
        crate::module_bindings::EntityState::Spawning => game_schema::EntityState::Spawning,
        crate::module_bindings::EntityState::Active => game_schema::EntityState::Active,
        crate::module_bindings::EntityState::DespawnPending => {
            game_schema::EntityState::DespawnPending
        }
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
        crate::module_bindings::NpcAiState::Evade => game_schema::NpcAiState::Evade,
    }
}

fn convert_interactable_info(row: &crate::module_bindings::InteractableConfig) -> game_core::sim_state::InteractableInfo {
    use game_core::sim_state::{SimInteractKind, SimInteractState, InteractableInfo};
    let kind = match row.interact_kind {
        crate::module_bindings::InteractKind::Switch => SimInteractKind::Switch,
        crate::module_bindings::InteractKind::Gate => SimInteractKind::Gate,
        crate::module_bindings::InteractKind::Grab => SimInteractKind::Grab,
        crate::module_bindings::InteractKind::Chest => SimInteractKind::Chest,
    };
    let state = match row.state {
        crate::module_bindings::InteractState::Idle => SimInteractState::Idle,
        crate::module_bindings::InteractState::Active => SimInteractState::Active,
        crate::module_bindings::InteractState::Cooldown => SimInteractState::Cooldown,
    };
    InteractableInfo {
        kind,
        linked_entity: row.linked_entity.map(EntityId),
        state,
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
            pos_x: t.pos_x,
            pos_y: t.pos_y,
            pos_z: t.pos_z,
            rot_x: t.rot_x,
            rot_y: t.rot_y,
            rot_z: t.rot_z,
            rot_w: t.rot_w,
            vel_x: t.vel_x,
            vel_y: t.vel_y,
            vel_z: t.vel_z,
            angvel_x: t.angvel_x,
            angvel_y: t.angvel_y,
            angvel_z: t.angvel_z,
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
            CommitCombatEventKind::CastStart {
                ability_id,
                cast_duration_ticks,
            } => CombatEventKind::CastStart(CastStartData {
                ability_id: *ability_id,
                cast_duration_ticks: *cast_duration_ticks,
            }),
            CommitCombatEventKind::ChargeStart {
                ability_id,
                max_ticks,
            } => CombatEventKind::ChargeStart(ChargeStartData {
                ability_id: *ability_id,
                max_ticks: *max_ticks,
            }),
            CommitCombatEventKind::ChargeTierReached { ability_id, tier } => {
                CombatEventKind::ChargeTierReached(ChargeTierReachedData {
                    ability_id: *ability_id,
                    tier: *tier,
                })
            }
            CommitCombatEventKind::BlockStart => CombatEventKind::BlockStart,
            CommitCombatEventKind::BlockEnd => CombatEventKind::BlockEnd,
            CommitCombatEventKind::Damage(d) => CombatEventKind::Damage(DamageData {
                amount: d.amount,
                damage_type: wire_damage_type(d.damage_type),
            }),
            CommitCombatEventKind::SkillHit(id) => CombatEventKind::SkillHit(*id),
            CommitCombatEventKind::BuffApplied(b) => {
                CombatEventKind::BuffApplied(BuffAppliedData {
                    buff_id: b.buff_id,
                    duration_ticks: b.duration_ticks,
                })
            }
            CommitCombatEventKind::BuffExpired(id) => CombatEventKind::BuffExpired(*id),
            CommitCombatEventKind::EntityDied(k) => CombatEventKind::EntityDied(*k),
            CommitCombatEventKind::Dodged(ability_id) => CombatEventKind::Dodged(*ability_id),
            CommitCombatEventKind::Blocked {
                ability_id,
                damage_taken,
                perfect,
            } => CombatEventKind::Blocked(BlockedData {
                ability_id: *ability_id,
                damage_taken: *damage_taken,
                perfect: *perfect,
            }),
            CommitCombatEventKind::Covered {
                blocker,
                ability_id,
                damage_taken,
            } => CombatEventKind::Covered(CoveredData {
                blocker: *blocker,
                ability_id: *ability_id,
                damage_taken: *damage_taken,
            }),
            CommitCombatEventKind::TelegraphWarning {
                target,
                impact_tick,
            } => CombatEventKind::TelegraphWarning(TelegraphWarningData {
                target: *target,
                impact_tick: *impact_tick,
            }),
            CommitCombatEventKind::LockOnAcquired => CombatEventKind::LockOnAcquired,
            CommitCombatEventKind::LockOnSessionStarted { ability_id } => {
                CombatEventKind::LockOnSessionStarted(LockOnSessionStartedData {
                    ability_id: *ability_id,
                })
            }
            CommitCombatEventKind::LockOnCanceled { target } => {
                CombatEventKind::LockOnCanceled(LockOnCanceledData {
                    target: *target,
                })
            }
            CommitCombatEventKind::LockOnFired { targets } => {
                CombatEventKind::LockOnFired(LockOnFiredData {
                    targets: targets.clone(),
                })
            }
            CommitCombatEventKind::ProjectileLaunched {
                execution_id,
                ability_id,
                origin_x,
                origin_y,
                origin_z,
                direction_x,
                direction_y,
                direction_z,
                speed,
                max_range,
            } => CombatEventKind::ProjectileLaunched(ProjectileLaunchedData {
                execution_id: *execution_id,
                ability_id: *ability_id,
                origin_x: *origin_x,
                origin_y: *origin_y,
                origin_z: *origin_z,
                direction_x: *direction_x,
                direction_y: *direction_y,
                direction_z: *direction_z,
                speed: *speed,
                max_range: *max_range,
            }),
            CommitCombatEventKind::HazardSpawned {
                execution_id,
                ability_id,
                pos_x,
                pos_y,
                pos_z,
                radius,
            } => CombatEventKind::HazardSpawned(HazardSpawnedData {
                execution_id: *execution_id,
                ability_id: *ability_id,
                pos_x: *pos_x,
                pos_y: *pos_y,
                pos_z: *pos_z,
                radius: *radius,
            }),
            CommitCombatEventKind::SkillObjectRemoved { execution_id } => {
                CombatEventKind::SkillObjectRemoved(*execution_id)
            }
            CommitCombatEventKind::Teleported {
                from_x,
                from_y,
                from_z,
                to_x,
                to_y,
                to_z,
            } => CombatEventKind::Teleported(TeleportedData {
                from_x: *from_x,
                from_y: *from_y,
                from_z: *from_z,
                to_x: *to_x,
                to_y: *to_y,
                to_z: *to_z,
            }),
            CommitCombatEventKind::Knockback { force } => {
                CombatEventKind::Knockback(KnockbackData { force: *force })
            }
            CommitCombatEventKind::Launched => CombatEventKind::Launched,
            CommitCombatEventKind::Stunned { duration_ticks } => {
                CombatEventKind::Stunned(StunnedData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::KnockedDown { duration_ticks } => {
                CombatEventKind::KnockedDown(KnockedDownData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::Pulled => CombatEventKind::Pulled,
            CommitCombatEventKind::Slept { duration_ticks } => CombatEventKind::Slept(SleptData {
                duration_ticks: *duration_ticks,
            }),
            CommitCombatEventKind::Silenced { duration_ticks } => {
                CombatEventKind::Silenced(SilencedData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::Feared { duration_ticks } => {
                CombatEventKind::Feared(FearedData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::StabilityConsumed { buff_id } => {
                CombatEventKind::StabilityConsumed(*buff_id)
            }
            CommitCombatEventKind::WeaponSwapped { new_set } => {
                CombatEventKind::WeaponSwapped(*new_set)
            }
            CommitCombatEventKind::CCCleared { cc_effect, source } => {
                CombatEventKind::CcCleared(CcClearedData {
                    cc_effect: wire_cc_effect(*cc_effect),
                    source: *source,
                })
            }
            CommitCombatEventKind::Cleansed { count, source } => {
                CombatEventKind::Cleansed(CleansedData {
                    count: *count,
                    source: *source,
                })
            }
            CommitCombatEventKind::Stunbreak => CombatEventKind::Stunbreak,
            CommitCombatEventKind::CCImmune { cc_effect, source } => {
                CombatEventKind::CcImmune(CcImmuneData {
                    cc_effect: wire_cc_effect(*cc_effect),
                    source: *source,
                })
            }
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
                CommitWorldEventKind::InteractTriggered(target) => {
                    WorldEventKind::InteractTriggered(*target)
                }
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
            mod_damage_out_pct: b.mod_damage_out_pct,
            mod_damage_in_pct: b.mod_damage_in_pct,
            mod_cooldown_reduce_pct: b.mod_cooldown_reduce_pct,
            mod_speed_pct: b.mod_speed_pct,
            mod_ai_override_kind: b.mod_ai_override_kind,
            mod_ai_override_target: b.mod_ai_override_target,
            mod_root: b.mod_root,
            mod_stealth: b.mod_stealth,
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

fn wire_director_spawns(pkg: &CommitPackage) -> Vec<DirectorSpawnInput> {
    pkg.director_spawns
        .iter()
        .map(|s| DirectorSpawnInput {
            kind: convert_entity_kind_to_wire(s.kind),
            max_hp: s.max_hp,
            pos_x: s.pos_x,
            pos_y: s.pos_y,
            pos_z: s.pos_z,
            layer: s.layer,
        })
        .collect()
}

fn convert_entity_kind_to_wire(
    kind: game_schema::EntityKind,
) -> crate::module_bindings::EntityKind {
    match kind {
        game_schema::EntityKind::Player => crate::module_bindings::EntityKind::Player,
        game_schema::EntityKind::Npc => crate::module_bindings::EntityKind::Npc,
        game_schema::EntityKind::Projectile => crate::module_bindings::EntityKind::Projectile,
        game_schema::EntityKind::Hazard => crate::module_bindings::EntityKind::Hazard,
        game_schema::EntityKind::Boss => crate::module_bindings::EntityKind::Boss,
        game_schema::EntityKind::Prop => crate::module_bindings::EntityKind::Prop,
    }
}

fn wire_interactable_updates(pkg: &CommitPackage) -> Vec<InteractableUpdate> {
    pkg.interactable_updates
        .iter()
        .map(|u| {
            let new_state = match u.new_state {
                game_core::sim_state::SimInteractState::Idle => crate::module_bindings::InteractState::Idle,
                game_core::sim_state::SimInteractState::Active => crate::module_bindings::InteractState::Active,
                game_core::sim_state::SimInteractState::Cooldown => crate::module_bindings::InteractState::Cooldown,
            };
            InteractableUpdate {
                entity_id: u.entity_id,
                state: new_state,
            }
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
        game_schema::NpcAiState::Evade => NpcAiState::Evade,
    }
}

fn wire_damage_type(dt: game_schema::DamageType) -> DamageType {
    match dt {
        game_schema::DamageType::Physical => DamageType::Physical,
        game_schema::DamageType::Magical => DamageType::Magical,
        game_schema::DamageType::True => DamageType::True,
    }
}

fn wire_cc_effect(cc: game_schema::CCEffect) -> CcEffect {
    match cc {
        game_schema::CCEffect::Stun => CcEffect::Stun,
        game_schema::CCEffect::Knockdown => CcEffect::Knockdown,
        game_schema::CCEffect::Sleep => CcEffect::Sleep,
        game_schema::CCEffect::Silence => CcEffect::Silence,
        game_schema::CCEffect::Fear => CcEffect::Fear,
        game_schema::CCEffect::Knockback => CcEffect::Knockback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_intent_action_preserves_target_hint() {
        let action = crate::module_bindings::IntentAction::UseAbility(
            crate::module_bindings::UseAbilityData {
                ability_id: 7,
                target: crate::module_bindings::AbilityTarget::Direction(
                    crate::module_bindings::Vec3F {
                        x: 1.0,
                        y: 0.0,
                        z: 0.0,
                    },
                ),
                target_hint: Some(42),
            },
        );

        let converted = convert_intent_action(action);
        match converted {
            game_protocol::intent::IntentAction::UseAbility(data) => {
                assert_eq!(data.target_hint, Some(42));
                match data.target {
                    game_schema::AbilityTarget::Direction(dir) => {
                        assert_eq!(
                            dir,
                            game_schema::Vec3f {
                                x: 1.0,
                                y: 0.0,
                                z: 0.0
                            }
                        );
                    }
                    other => panic!("expected direction target, got {other:?}"),
                }
            }
            other => panic!("expected UseAbility action, got {other:?}"),
        }
    }

    #[test]
    fn load_abilities_parses_fireball_as_aim_assist() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("abilities.ron");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let file: AbilityFile = ron::from_str(&source)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let fireball = file
            .abilities
            .into_iter()
            .find(|ability| ability.ability_id == 2)
            .expect("Fireball ability should be present in data/abilities.ron");

        assert_eq!(
            fireball.targeting_mode,
            game_core::combat::skill::TargetingMode::AimAssist
        );
    }

    #[test]
    fn load_abilities_parses_backstab_as_entity_target() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("abilities.ron");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let file: AbilityFile = ron::from_str(&source)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let backstab = file
            .abilities
            .into_iter()
            .find(|ability| ability.ability_id == 21)
            .expect("Backstab ability should be present in data/abilities.ron");

        assert_eq!(
            backstab.targeting_mode,
            game_core::combat::skill::TargetingMode::EntityTarget
        );
    }
}
