use spacetimedb::{reducer, ReducerContext, Table, TimeDuration};
use crate::tables::*;

// ── Lifecycle ───────────────────────────────────────────────────────

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    log::info!("Module initializing — seeding tick 0");

    // Store the publishing identity as admin for owner-only reducers.
    ctx.db.module_config().insert(ModuleConfig {
        key: 0,
        admin: ctx.sender(),
        last_committed_tick: 0,
    });

    // Seed tick 0
    ctx.db.sim_tick().insert(SimTick {
        tick_id: 0,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });

    // Start the simulation tick loop at 20Hz (50ms interval).
    // Per spec: scheduled reducers are best-effort, may be delayed.
    let interval = TimeDuration::from_micros(50_000);
    ctx.db.tick_schedule().insert(TickSchedule {
        scheduled_id: 0,
        scheduled_at: interval.into(),
    });

    log::info!("Tick scheduler started at 20Hz");
}

#[reducer(client_connected)]
pub fn client_connected(ctx: &ReducerContext) {
    let caller = ctx.sender();
    log::info!("Client connected: {:?}", caller);
}

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    let caller = ctx.sender();
    log::info!("Client disconnected: {:?}", caller);
}

// ── Tick Trigger ────────────────────────────────────────────────────
// Called by the scheduler. Advances the canonical tick in the DB.
// Per spec: the DB commit timeline defines the canonical tick.
// The coordinator merely proposes the next tick.
//
// Per spec: scheduled reducer should verify caller identity to
// prevent client invocation.

/// Maximum number of unacknowledged sim_tick rows before tick_trigger pauses.
/// At 20 Hz a backlog of 5 means the worker is >250 ms behind — pause and let it catch up.
const BACKLOG_LIMIT: u64 = 5;

/// Number of sim_tick rows to retain for coordinator restart seeding (6 seconds at 20 Hz).
/// Older rows are pruned each tick to prevent unbounded table growth.
const SIM_TICK_RETAIN: u64 = 120;

#[reducer]
pub fn tick_trigger(ctx: &ReducerContext, _schedule: TickSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.identity() {
        return Err("tick_trigger may only be invoked by the scheduler".into());
    }

    // Find current tick
    let current = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .ok_or("No tick found")?;

    let current_max = current.tick_id;

    // Backpressure guard: skip inserting a new tick if the simulation worker
    // has not committed recent ticks. This prevents sim_tick from accumulating
    // faster than the worker can consume when it is slow or disconnected.
    //
    // Guard is only active once a worker has committed at least one tick
    // (last_committed_tick > 0).  Before that — clean deploy, hot-reload after a
    // schema migration that zero-initialises the field, or a fresh server with no
    // worker yet — we let ticks flow freely.  The sim_tick table is already bounded
    // by the SIM_TICK_RETAIN pruning below, so unbounded growth is not a concern.
    let last_committed = ctx.db.module_config().key().find(0)
        .map(|c| c.last_committed_tick)
        .unwrap_or(0);

    if last_committed > 0 {
        let backlog = current_max.saturating_sub(last_committed);
        if backlog > BACKLOG_LIMIT {
            log::warn!(
                "tick backpressure: backlog={} (limit={}) last_committed={} — skipping tick {}",
                backlog, BACKLOG_LIMIT, last_committed, current_max + 1
            );
            return Ok(());
        }
    }

    let next_tick_id = current_max + 1;

    ctx.db.sim_tick().insert(SimTick {
        tick_id: next_tick_id,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });

    // Prune old sim_tick rows to cap table size.
    // Keeps the last SIM_TICK_RETAIN rows so a restarting coordinator can still
    // seed last_processed_tick = MAX(sim_tick) without loading the full history.
    if next_tick_id > SIM_TICK_RETAIN {
        let cutoff = next_tick_id - SIM_TICK_RETAIN;
        let to_delete: Vec<u64> = ctx.db.sim_tick()
            .iter()
            .filter(|t| t.tick_id < cutoff)
            .map(|t| t.tick_id)
            .collect();
        for id in to_delete {
            ctx.db.sim_tick().tick_id().delete(&id);
        }
    }

    Ok(())
}

// ── Player Input ────────────────────────────────────────────────────
// Per spec: reducers validate + insert intent. Clients send intent only.
// Server validates all gameplay. Input must be sequenced.

#[reducer]
pub fn submit_intent(
    ctx: &ReducerContext,
    entity_id: u64,
    sequence_id: u64,
    action: IntentAction,
    client_time_ms: u64,
) -> Result<(), String> {
    let caller = ctx.sender();

    // Verify client owns this entity
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Client not registered")?;

    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    // Sequence validation: reject out-of-order / duplicate
    if sequence_id <= seq.last_processed_sequence {
        return Err(format!(
            "Stale sequence: got {}, last processed {}",
            sequence_id, seq.last_processed_sequence
        ));
    }

    // Update last processed sequence
    ctx.db.client_sequence().client_identity().update(ClientSequence {
        client_identity: caller,
        last_processed_sequence: sequence_id,
        entity_id: seq.entity_id,
    });

    // Rate limit: cap the number of pending (unprocessed) intents per entity.
    // At 20 Hz a queue depth of 5 = 250 ms of buffered input — enough for normal
    // gameplay but prevents a buggy or malicious client from flooding DB writes.
    const MAX_QUEUED_INTENTS: usize = 5;
    let queued = ctx.db.player_intent()
        .iter()
        .filter(|i| i.entity_id == entity_id)
        .count();
    if queued >= MAX_QUEUED_INTENTS {
        return Err("Intent queue full: reduce submission rate".into());
    }

    // Compute target tick: current_tick + 1
    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);
    let target_tick = current_tick + 1;

    // Insert intent
    ctx.db.player_intent().insert(PlayerIntent {
        intent_id: 0, // auto_inc
        client_identity: caller,
        entity_id,
        sequence_id,
        target_tick,
        client_time_ms,
        action,
    });

    Ok(())
}

// ── Entity Spawning ─────────────────────────────────────────────────
// Register a player entity for the connected client.

#[reducer]
pub fn spawn_player(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();

    // Check not already registered
    if ctx.db.client_sequence().client_identity().find(&caller).is_some() {
        return Err("Player already spawned".into());
    }

    // Get current tick
    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);

    // Create entity
    let entity = ctx.db.entity().insert(Entity {
        entity_id: 0, // auto_inc
        kind: EntityKind::Player,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: Some(caller),
    });

    let eid = entity.entity_id;

    // Create transform at origin
    ctx.db.entity_transform().insert(EntityTransform {
        entity_id: eid,
        pos_x: 0.0, pos_y: 1.0, pos_z: 0.0,
        rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
        vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
        angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
        last_tick: current_tick,
    });

    // Create health
    ctx.db.entity_health().insert(EntityHealth {
        entity_id: eid,
        hp: 100.0,
        max_hp: 100.0,
    });

    // Create region assignment
    ctx.db.entity_region().insert(EntityRegion {
        entity_id: eid,
        region_x: 0,
        region_z: 0,
    });

    // Register sequence tracking
    ctx.db.client_sequence().insert(ClientSequence {
        client_identity: caller,
        last_processed_sequence: 0,
        entity_id: eid,
    });

    log::info!("Player spawned: entity_id={}, identity={:?}", eid, caller);
    Ok(())
}
// ── NPC Spawning ────────────────────────────────────────────────
// Spawn a test NPC at an explicit position. Admin or trusted-worker only.
// Used for smoke-testing the combat path (player Slash → NPC damage → death).

#[reducer]
pub fn spawn_npc(
    ctx: &ReducerContext,
    pos_x: f32,
    pos_y: f32,
    pos_z: f32,
    max_hp: f32,
) -> Result<(), String> {
    if !is_trusted_caller(ctx) && !is_module_admin(ctx) {
        return Err("spawn_npc may only be called by admin or a trusted worker".into());
    }

    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);

    let entity = ctx.db.entity().insert(Entity {
        entity_id: 0,
        kind: EntityKind::Npc,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: None,
    });
    let eid = entity.entity_id;

    ctx.db.entity_transform().insert(EntityTransform {
        entity_id: eid,
        pos_x, pos_y, pos_z,
        rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
        vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
        angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
        last_tick: current_tick,
    });

    ctx.db.entity_health().insert(EntityHealth {
        entity_id: eid,
        hp: max_hp,
        max_hp,
    });

    ctx.db.entity_region().insert(EntityRegion {
        entity_id: eid,
        region_x: 0,
        region_z: 0,
    });

    log::info!("NPC spawned: entity_id={} pos=({},{},{}) max_hp={}", eid, pos_x, pos_y, pos_z, max_hp);
    Ok(())
}
// ── Commit Tick Results ─────────────────────────────────────────────
// Called by the simulation worker (external process) to commit
// authoritative results back to SpacetimeDB.
//
// Per spec: commit reducers should be extremely small and deterministic.
// The simulation worker never writes tables directly — all mutations
// pass through reducers.
//
// Per spec: verify caller identity for trusted-only reducers.

/// Check if the caller is the module admin (the identity that published the module).
fn is_module_admin(ctx: &ReducerContext) -> bool {
    if ctx.sender() == ctx.identity() {
        return true;
    }
    ctx.db.module_config().key().find(0)
        .is_some_and(|cfg| ctx.sender() == cfg.admin)
}

/// Check if the caller is the module itself or a registered simulation worker.
fn is_trusted_caller(ctx: &ReducerContext) -> bool {
    if ctx.sender() == ctx.identity() {
        return true;
    }
    ctx.db.trusted_worker().worker_identity().find(&ctx.sender()).is_some()
}

#[reducer]
pub fn commit_tick_results(
    ctx: &ReducerContext,
    tick_id: u64,
    transforms: Vec<TransformUpdate>,
    health_updates: Vec<HealthUpdate>,
    combat_events: Vec<CombatEventInput>,
    world_events: Vec<WorldEventInput>,
    consumed_intent_ids: Vec<u64>,
    entity_state_updates: Vec<EntityStateUpdate>,
    region_updates: Vec<RegionUpdate>,
    buff_updates: Vec<BuffUpdate>,
    buff_cleared_entity_ids: Vec<u64>,
    threat_updates: Vec<ThreatUpdate>,
    threat_cleared_entity_ids: Vec<u64>,
    npc_state_updates: Vec<NpcStateUpdate>,
) -> Result<(), String> {
    // Per spec (security_and_authority): trusted-only reducers must verify caller identity.
    // Accept the module itself (scheduler) or any registered simulation worker.
    if !is_trusted_caller(ctx) {
        return Err("commit_tick_results may only be invoked by a trusted worker".into());
    }

    // Apply transform updates
    for t in transforms {
        if ctx.db.entity_transform().entity_id().find(&t.entity_id).is_none() {
            log::warn!(
                "commit_tick_results tick={}: no entity_transform row for entity_id={} — update skipped",
                tick_id, t.entity_id
            );
        }
        ctx.db.entity_transform().entity_id().update(EntityTransform {
            entity_id: t.entity_id,
            pos_x: t.pos_x, pos_y: t.pos_y, pos_z: t.pos_z,
            rot_x: t.rot_x, rot_y: t.rot_y, rot_z: t.rot_z, rot_w: t.rot_w,
            vel_x: t.vel_x, vel_y: t.vel_y, vel_z: t.vel_z,
            angvel_x: t.angvel_x, angvel_y: t.angvel_y, angvel_z: t.angvel_z,
            last_tick: tick_id,
        });
    }

    // Apply health updates
    for h in health_updates {
        if ctx.db.entity_health().entity_id().find(&h.entity_id).is_none() {
            log::warn!(
                "commit_tick_results tick={}: no entity_health row for entity_id={} — update skipped",
                tick_id, h.entity_id
            );
        }
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id: h.entity_id,
            hp: h.hp,
            max_hp: h.max_hp,
        });
    }

    // Insert combat events (use worker-provided event_sequence to preserve total ordering)
    for e in combat_events {
        ctx.db.combat_event().insert(CombatEvent {
            event_id: 0,
            tick_id,
            event_sequence: e.event_sequence,
            source_entity: e.source_entity,
            target_entity: e.target_entity,
            event_kind: e.event_kind,
        });
    }

    // Insert world events (preserve worker-provided event_sequence)
    for e in world_events {
        ctx.db.world_event().insert(WorldEvent {
            event_id: 0,
            tick_id,
            event_sequence: e.event_sequence,
            entity_id: e.entity_id,
            event_kind: e.event_kind,
        });
    }

    // Delete consumed intents
    for id in consumed_intent_ids {
        ctx.db.player_intent().intent_id().delete(&id);
    }

    // Apply entity state updates
    for u in entity_state_updates {
        if let Some(existing) = ctx.db.entity().entity_id().find(&u.entity_id) {
            ctx.db.entity().entity_id().update(Entity {
                entity_id: u.entity_id,
                kind: existing.kind,
                state: u.new_state,
                spawned_at_tick: existing.spawned_at_tick,
                owner_identity: existing.owner_identity,
            });
        } else {
            log::warn!(
                "commit_tick_results tick={}: no entity row for entity_id={} — state update skipped",
                tick_id, u.entity_id
            );
        }
    }

    // Apply region updates
    for r in region_updates {
        ctx.db.entity_region().entity_id().update(EntityRegion {
            entity_id: r.entity_id,
            region_x: r.region_x,
            region_z: r.region_z,
        });
    }

    // Persist buffs: delete all rows for entities present in buff_cleared_entity_ids, then
    // re-insert. buff_cleared_entity_ids always includes every non-Removed entity (with an
    // empty vec for entities that currently have no buffs), so stale rows are
    // reliably cleared when all buffs on an entity expire in the same tick.
    {
        // buff_cleared_entity_ids drives deletes; buff_updates drives inserts.
        // Separating them ensures entities with zero buffs still clear stale DB rows.
        for entity_id in &buff_cleared_entity_ids {
            let to_delete: Vec<u64> = ctx.db.active_buff()
                .iter()
                .filter(|b| b.entity_id == *entity_id)
                .map(|b| b.buff_instance_id)
                .collect();
            for id in to_delete {
                ctx.db.active_buff().buff_instance_id().delete(&id);
            }
        }
        for b in buff_updates {
            ctx.db.active_buff().insert(ActiveBuff {
                buff_instance_id: 0, // auto_inc
                entity_id: b.entity_id,
                buff_id: b.buff_id,
                source_entity: b.source_entity,
                stacks: b.stacks,
                expires_at_tick: b.expires_at_tick,
            });
        }
    }

    // Persist threat: delete all rows for NPCs present in threat_cleared_entity_ids, then re-insert.
    // threat_cleared_entity_ids always includes every non-Removed NPC/Boss entity so stale rows
    // are reliably cleared when all threat decays to zero in a single tick.
    {
        // threat_cleared_entity_ids drives deletes; threat_updates drives inserts.
        for npc_entity in &threat_cleared_entity_ids {
            let to_delete: Vec<u64> = ctx.db.threat_entry()
                .iter()
                .filter(|t| t.npc_entity == *npc_entity)
                .map(|t| t.threat_id)
                .collect();
            for id in to_delete {
                ctx.db.threat_entry().threat_id().delete(&id);
            }
        }
        for t in threat_updates {
            ctx.db.threat_entry().insert(ThreatEntry {
                threat_id: 0, // auto_inc
                npc_entity: t.npc_entity,
                source_entity: t.source_entity,
                threat: t.threat,
            });
        }
    }

    // Persist NPC state: upsert by entity PK (1:1 per NPC entity).
    for n in npc_state_updates {
        if ctx.db.npc_state().entity_id().find(&n.entity_id).is_some() {
            ctx.db.npc_state().entity_id().update(NpcState {
                entity_id: n.entity_id,
                ai_state: n.ai_state,
                target_entity: n.target_entity,
            });
        } else {
            ctx.db.npc_state().insert(NpcState {
                entity_id: n.entity_id,
                ai_state: n.ai_state,
                target_entity: n.target_entity,
            });
        }
    }

    // Advance the last_committed_tick cursor used by tick_trigger's backpressure guard.
    // Only update if this commit is actually moving the cursor forward (guards against
    // out-of-order or replayed commits, though the trusted-worker check above makes
    // those unlikely).
    if let Some(mut cfg) = ctx.db.module_config().key().find(0) {
        if tick_id > cfg.last_committed_tick {
            cfg.last_committed_tick = tick_id;
            ctx.db.module_config().key().update(cfg);
        }
    }

    Ok(())
}

// ── Commit reducer input types ──────────────────────────────────────
// These are the wire types the simulation worker sends when calling
// commit_tick_results. They use SpacetimeType so they are serializable
// across the SpacetimeDB boundary.

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct TransformUpdate {
    pub entity_id: u64,
    pub pos_x: f32, pub pos_y: f32, pub pos_z: f32,
    pub rot_x: f32, pub rot_y: f32, pub rot_z: f32, pub rot_w: f32,
    pub vel_x: f32, pub vel_y: f32, pub vel_z: f32,
    pub angvel_x: f32, pub angvel_y: f32, pub angvel_z: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct HealthUpdate {
    pub entity_id: u64,
    pub hp: f32,
    pub max_hp: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct CombatEventInput {
    pub source_entity: u64,
    pub target_entity: u64,
    pub event_sequence: u32,
    pub event_kind: CombatEventKind,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct WorldEventInput {
    pub entity_id: u64,
    pub event_sequence: u32,
    pub event_kind: WorldEventKind,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct EntityStateUpdate {
    pub entity_id: u64,
    pub new_state: EntityState,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct RegionUpdate {
    pub entity_id: u64,
    pub region_x: i32,
    pub region_z: i32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct BuffUpdate {
    pub entity_id: u64,
    pub buff_id: u32,
    pub source_entity: u64,
    pub stacks: u32,
    pub expires_at_tick: Option<u64>,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct ThreatUpdate {
    pub npc_entity: u64,
    pub source_entity: u64,
    pub threat: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct NpcStateUpdate {
    pub entity_id: u64,
    pub ai_state: NpcAiState,
    pub target_entity: Option<u64>,
}

// ── Worker Registration ─────────────────────────────────────────────
// Register an external simulation worker's Identity so it can call
// commit_tick_results and clear_events. Only the module itself may
// register workers (called from init or via spacetime CLI).

#[reducer]
pub fn register_worker(ctx: &ReducerContext, worker_identity: spacetimedb::Identity) -> Result<(), String> {
    if !is_module_admin(ctx) {
        return Err("register_worker may only be invoked by the module admin".into());
    }
    if ctx.db.trusted_worker().worker_identity().find(&worker_identity).is_some() {
        return Err("Worker already registered".into());
    }
    ctx.db.trusted_worker().insert(TrustedWorker { worker_identity });
    log::info!("Registered trusted worker: {:?}", worker_identity);
    Ok(())
}

// ── Clear Events ────────────────────────────────────────────────────
// Delete old event rows. Called by the coordinator after clients
// have had time to receive them. Per spec: event tables hold transient
// rows that are broadcast then deleted.

#[reducer]
pub fn clear_events(ctx: &ReducerContext, before_tick: u64) -> Result<(), String> {
    // Per spec: only the simulation worker / scheduler may delete event rows.
    if !is_trusted_caller(ctx) {
        return Err("clear_events may only be invoked by a trusted worker".into());
    }

    // Delete combat events from old ticks
    let to_delete: Vec<u64> = ctx.db.combat_event().iter()
        .filter(|e| e.tick_id < before_tick)
        .map(|e| e.event_id)
        .collect();
    for id in to_delete {
        ctx.db.combat_event().event_id().delete(&id);
    }

    // Delete world events from old ticks
    let to_delete: Vec<u64> = ctx.db.world_event().iter()
        .filter(|e| e.tick_id < before_tick)
        .map(|e| e.event_id)
        .collect();
    for id in to_delete {
        ctx.db.world_event().event_id().delete(&id);
    }

    Ok(())
}
