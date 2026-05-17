//! Simulation coordinator — connects to SpacetimeDB as a client and drives the tick pipeline.
//!
//! # Prerequisites
//!
//! 1. Generate module bindings:
//!    ```sh
//!    spacetime generate --lang rust \
//!        --out-dir crates/simulation_worker/src/module_bindings \
//!        --project-path crates/server_module
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

use std::sync::{Arc, Mutex};

use log::{error, info, warn};
use spacetimedb_sdk::{DbContext, Event, Identity, Table, TableWithPrimaryKey};

use crate::module_bindings::*;
use crate::physics::rapier_world::PhysicsWorld;
use crate::tick_pipeline::TickPipeline;
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
    pipeline: TickPipeline,
    last_processed_tick: u64,
}

/// Start the coordinator loop. This blocks the current thread.
pub fn run(config: CoordinatorConfig) {
    let tick_dt = 1.0 / 20.0; // 20 Hz — must match server TickConfig
    let physics = PhysicsWorld::new(tick_dt);
    let pipeline = TickPipeline::new(TickId(0), Box::new(physics), tick_dt);

    let state = Arc::new(Mutex::new(CoordinatorState {
        pipeline,
        last_processed_tick: 0,
    }));

    let state_for_tick = Arc::clone(&state);

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_module_name(&config.module_name)
        .with_token(config.auth_token.as_deref())
        .on_connect(move |ctx, identity, token| {
            info!(
                "Connected to SpacetimeDB as {:?}, token: {}...",
                identity,
                &token[..token.len().min(8)]
            );

            // Register ourselves as a trusted worker.
            // This will succeed only if the module owner has called `register_worker`
            // with our identity, or if we are the module identity itself.
            subscribe_to_tables(ctx);
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

    // Watch for sim_tick updates — each update triggers a pipeline run.
    conn.db.sim_tick().on_update(move |ctx, _old, new_tick| {
        let canonical_tick = new_tick.current_tick;

        // Gather intents targeting this tick from the client cache.
        let intents: Vec<game_protocol::intent::PlayerIntent> = ctx
            .db
            .player_intent()
            .iter()
            .filter(|i| i.target_tick == canonical_tick)
            .map(|row| game_protocol::intent::PlayerIntent {
                player_id: EntityId(row.player_id),
                sequence: row.sequence,
                target_tick: TickId(row.target_tick),
                action: deserialize_intent_action(&row.action_json),
            })
            .collect();

        let mut guard = state_for_tick.lock().unwrap();

        // Skip if we already processed this tick (idempotency).
        if canonical_tick <= guard.last_processed_tick {
            warn!("Tick {canonical_tick} already processed, skipping");
            return;
        }

        info!("Processing tick {canonical_tick} with {} intents", intents.len());
        let result = guard.pipeline.run_tick(&intents);
        guard.last_processed_tick = canonical_tick;

        // Marshal transforms: Vec<(entity_id, x, y, z, qx, qy, qz, qw)>
        let transforms: Vec<(u64, f32, f32, f32, f32, f32, f32, f32)> = result
            .transforms
            .iter()
            .map(|(eid, t)| {
                (
                    eid.0,
                    t.position.x,
                    t.position.y,
                    t.position.z,
                    t.rotation.x,
                    t.rotation.y,
                    t.rotation.z,
                    t.rotation.w,
                )
            })
            .collect();

        // Marshal events: Vec<(entity_id, sequence, tick, payload_json)>
        let events: Vec<(u64, u32, u64, String)> = result
            .events
            .iter()
            .map(|e| {
                (
                    e.entity_id.0,
                    e.event_sequence,
                    e.tick_id.0,
                    serialize_event_payload(&e.payload),
                )
            })
            .collect();

        // Commit results to SpacetimeDB.
        ctx.reducers.commit_tick_results(
            result.tick_id.0,
            transforms,
            Vec::new(), // health_updates — filled once combat resolution is implemented
            events,
            Vec::new(), // state_updates
            Vec::new(), // region_updates
        );
    });

    // Block on the connection thread — the callbacks above drive the simulation.
    conn.run_threaded().join().expect("Connection thread panicked");
}

/// Subscribe to the tables the coordinator needs to observe.
fn subscribe_to_tables(ctx: &DbConnection) {
    ctx.subscription_builder()
        .on_applied(|ctx| {
            info!(
                "Subscription applied — {} sim_tick rows, {} intent rows",
                ctx.db.sim_tick().count(),
                ctx.db.player_intent().count(),
            );
        })
        .on_error(|_ctx, err| {
            error!("Subscription error: {err}");
        })
        .subscribe([
            "SELECT * FROM sim_tick",
            "SELECT * FROM player_intent",
        ]);
}

// ── Serialization helpers ───────────────────────────────────────
//
// The server_module stores some fields as JSON strings because the WASM
// boundary cannot share game_protocol types directly.  These helpers
// bridge between the generated binding types and game_protocol types.

fn deserialize_intent_action(json: &str) -> game_protocol::intent::IntentAction {
    // TODO: Replace with proper serde deserialization once the action_json
    // column is finalized.  For now, default to Idle if parsing fails.
    serde_json::from_str(json).unwrap_or(game_protocol::intent::IntentAction::Stop)
}

fn serialize_event_payload(payload: &game_protocol::event::EventPayload) -> String {
    serde_json::to_string(payload).unwrap_or_default()
}
