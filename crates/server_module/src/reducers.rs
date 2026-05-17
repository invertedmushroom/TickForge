use spacetimedb::{reducer, ReducerContext, Table, TimeDuration};
use crate::tables::*;

// ── Lifecycle ───────────────────────────────────────────────────────

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    log::info!("Module initializing — seeding tick 0");

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

#[reducer]
pub fn tick_trigger(ctx: &ReducerContext, _schedule: TickSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.identity() {
        return Err("tick_trigger may only be invoked by the scheduler".into());
    }

    // Find current tick
    let current = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .ok_or("No tick found")?;

    let next_tick_id = current.tick_id + 1;

    ctx.db.sim_tick().insert(SimTick {
        tick_id: next_tick_id,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });

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

// ── Commit Tick Results ─────────────────────────────────────────────
// Called by the simulation worker (external process) to commit
// authoritative results back to SpacetimeDB.
//
// Per spec: commit reducers should be extremely small and deterministic.
// The simulation worker never writes tables directly — all mutations
// pass through reducers.
//
// Per spec: verify caller identity for trusted-only reducers.

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
) -> Result<(), String> {
    // Per spec (security_and_authority): trusted-only reducers must verify caller identity.
    // Accept the module itself (scheduler) or any registered simulation worker.
    if !is_trusted_caller(ctx) {
        return Err("commit_tick_results may only be invoked by a trusted worker".into());
    }

    // Apply transform updates
    for t in transforms {
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
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id: h.entity_id,
            hp: h.hp,
            max_hp: h.max_hp,
        });
    }

    // Insert combat events
    let mut seq: u32 = 0;
    for e in combat_events {
        ctx.db.combat_event().insert(CombatEvent {
            event_id: 0,
            tick_id,
            event_sequence: seq,
            source_entity: e.source_entity,
            target_entity: e.target_entity,
            event_kind: e.event_kind,
        });
        seq += 1;
    }

    // Insert world events
    let mut wseq: u32 = 0;
    for e in world_events {
        ctx.db.world_event().insert(WorldEvent {
            event_id: 0,
            tick_id,
            event_sequence: wseq,
            entity_id: e.entity_id,
            event_kind: e.event_kind,
        });
        wseq += 1;
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
    pub event_kind: CombatEventKind,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct WorldEventInput {
    pub entity_id: u64,
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

// ── Worker Registration ─────────────────────────────────────────────
// Register an external simulation worker's Identity so it can call
// commit_tick_results and clear_events. Only the module itself may
// register workers (called from init or via spacetime CLI).

#[reducer]
pub fn register_worker(ctx: &ReducerContext, worker_identity: spacetimedb::Identity) -> Result<(), String> {
    if ctx.sender() != ctx.identity() {
        return Err("register_worker may only be invoked by the module owner".into());
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
