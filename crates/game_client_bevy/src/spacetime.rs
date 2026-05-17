use bevy::prelude::*;
use game_client::module_bindings::*;
use log::info;
use spacetimedb_sdk::{DbContext, EventTable, Table};

use crossbeam_channel::{Receiver, unbounded};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct SpacetimePlugin;

impl Plugin for SpacetimePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, connect);
        app.add_systems(Update, pump_connection);
    }
}

const DEFAULT_TOKEN_FILE: &str = ".client_token";

fn token_file() -> String {
    std::env::var("STDB_TOKEN_FILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_TOKEN_FILE.to_string())
}

fn load_token() -> Option<String> {
    std::fs::read_to_string(token_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_token(token: &str) {
    let token_file = token_file();
    if let Err(e) = std::fs::write(&token_file, token) {
        log::warn!("Failed to save client token to {token_file}: {e}");
    }
}

/// Bevy resource holding the SpacetimeDB connection.
#[derive(Resource)]
pub struct StdbConnection {
    pub conn: DbConnection,
    pub connected: Arc<AtomicBool>,
}

/// Push queues for transient SpacetimeDB event tables.
#[derive(Resource)]
pub struct SpacetimeEvents {
    pub combat_event_rx: Receiver<CombatEvent>,
    pub world_event_rx: Receiver<WorldEvent>,
}
#[allow(dead_code)]
/// Bevy resource tracking the local player's entity ID once spawned.
#[derive(Resource, Default)]
pub struct LocalPlayerEntity {
    pub entity_id: Option<u64>,
    pub spawned: bool,
}

/// Bevy resource tracking the latest observed tick (for intent submission).
#[derive(Resource, Default)]
pub struct TickCounter {
    pub last_tick: u64,
    pub intent_seq: u64,
}

fn connect(mut commands: Commands) {
    let uri = std::env::var("STDB_URI").unwrap_or_else(|_| "http://localhost:3000".into());
    let module_name = std::env::var("STDB_MODULE").unwrap_or_else(|_| "tickforge".into());
    let auth_token = std::env::var("STDB_TOKEN").ok().or_else(load_token);

    let (combat_tx, combat_rx) = unbounded();
    let (world_tx, world_rx) = unbounded();

    let connected = Arc::new(AtomicBool::new(false));
    let connected_flag = Arc::clone(&connected);

    info!("Connecting to {uri}/{module_name}");

    let conn = DbConnection::builder()
        .with_uri(&uri)
        .with_database_name(&module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, token: &str| {
            let combat_tx_clone = combat_tx.clone();
            ctx.db.combat_event().on_insert(move |_, row| {
                let _ = combat_tx_clone.send(row.clone());
            });
            let world_tx_clone = world_tx.clone();
            ctx.db.world_event().on_insert(move |_, row| {
                let _ = world_tx_clone.send(row.clone());
            });

            save_token(token);
            info!("Connected as {identity}");
            connected_flag.store(true, Ordering::SeqCst);

            // Subscribe to AOI-filtered transforms, entities, and health, plus
            // the small global tables the UI currently needs.
            ctx.subscription_builder()
                .on_applied(|ctx| {
                    info!(
                        "Subscription applied — nearby_transforms: {} nearby_entities: {} nearby_health: {}",
                        ctx.db.nearby_transforms().count(),
                        ctx.db.nearby_entities().count(),
                        ctx.db.nearby_health().count(),
                    );
                })
                .on_error(|_ctx, err| {
                    log::error!("Subscription error: {err}");
                })
                .subscribe([
                    "SELECT * FROM my_region",
                    "SELECT * FROM nearby_transforms",
                    "SELECT * FROM nearby_entities",
                    "SELECT * FROM client_sequence",
                    "SELECT * FROM nearby_health",
                    "SELECT * FROM sim_tick",
                    "SELECT * FROM combat_event",
                    "SELECT * FROM active_buff",
                    "SELECT * FROM npc_state",
                    "SELECT * FROM world_event",
                    "SELECT * FROM player_inventory",
                    "SELECT * FROM player_equipment",
                    "SELECT * FROM module_config",
                    "SELECT * FROM interactable_config",
                    "SELECT * FROM boss_phase",
                    "SELECT * FROM world_phase",
                    "SELECT * FROM zone_counter",
                    "SELECT * FROM instance",
                    "SELECT * FROM instance_membership",
                    "SELECT * FROM death_state",
                    "SELECT * FROM entity_layer",
                ]);

            // Spawn the player.
            if let Err(e) = ctx.reducers.spawn_player() {
                log::error!("Failed to call spawn_player: {e}");
            } else {
                info!("spawn_player reducer called");
            }
        })
        .on_connect_error(|_ctx, err| {
            log::error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(e) = err {
                log::error!("Disconnected: {e}");
            } else {
                info!("Disconnected");
            }
        })
        .build()
        .expect("Failed to build SpacetimeDB connection");

    commands.insert_resource(StdbConnection { conn, connected });
    commands.insert_resource(SpacetimeEvents {
        combat_event_rx: combat_rx,
        world_event_rx: world_rx,
    });
    commands.insert_resource(LocalPlayerEntity::default());
    commands.insert_resource(TickCounter::default());
}

/// Pump the SpacetimeDB connection each frame (processes callbacks).
fn pump_connection(
    stdb: Option<Res<StdbConnection>>,
    mut tick_counter: ResMut<TickCounter>,
    intent_ack: Option<Res<crate::input::IntentAckStats>>,
) {
    let Some(stdb) = stdb else { return };
    let _ = stdb.conn.frame_tick();

    // Track the latest sim tick for intent submission.
    for tick in stdb.conn.db.sim_tick().iter() {
        if tick.tick_id > tick_counter.last_tick {
            tick_counter.last_tick = tick.tick_id;
        }
    }

    // Keep local intent sequence aligned with authoritative server sequence.
    let identity = stdb.conn.identity();
    if let Some(seq) = stdb
        .conn
        .db
        .client_sequence()
        .client_identity()
        .find(&identity)
    {
        if tick_counter.intent_seq < seq.last_processed_sequence {
            log::warn!(
                "Resyncing intent sequence from {} -> {}",
                tick_counter.intent_seq,
                seq.last_processed_sequence
            );
            tick_counter.intent_seq = seq.last_processed_sequence;
        }
        // Authoritative ack signal for the redundancy ring buffer. The
        // `submit_intents_batch` reducer returns `Ok(())` even when tail
        // entries were silently dropped to the server queue cap, so the
        // batch callback alone cannot prune the ring safely. The cursor
        // table is the only source of truth for what the server actually
        // committed. `mark_acked` is monotonic-max, so calling it every
        // frame is cheap and idempotent.
        if let Some(ack) = intent_ack.as_ref() {
            ack.ack_up_to(seq.last_processed_sequence);
        }
    }
}
