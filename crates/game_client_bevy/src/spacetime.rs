use bevy::prelude::*;
use game_client::module_bindings::*;
use log::info;
use spacetimedb_sdk::{DbContext, Table};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct SpacetimePlugin;

impl Plugin for SpacetimePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, connect);
        app.add_systems(Update, pump_connection);
    }
}

const TOKEN_FILE: &str = ".client_token";

fn load_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_token(token: &str) {
    if let Err(e) = std::fs::write(TOKEN_FILE, token) {
        log::warn!("Failed to save client token: {e}");
    }
}

/// Bevy resource holding the SpacetimeDB connection.
#[derive(Resource)]
pub struct StdbConnection {
    pub conn: DbConnection,
    pub connected: Arc<AtomicBool>,
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
    let module_name = std::env::var("STDB_MODULE").unwrap_or_else(|_| "jump".into());
    let auth_token = std::env::var("STDB_TOKEN").ok().or_else(load_token);

    let connected = Arc::new(AtomicBool::new(false));
    let connected_flag = Arc::clone(&connected);

    info!("Connecting to {uri}/{module_name}");

    let conn = DbConnection::builder()
        .with_uri(&uri)
        .with_database_name(&module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, token: &str| {
            save_token(token);
            info!("Connected as {identity}");
            connected_flag.store(true, Ordering::SeqCst);

            // Subscribe to nearby transforms, entities, and health.
            ctx.subscription_builder()
                .on_applied(|ctx| {
                    info!(
                        "Subscription applied — nearby: {} entities: {}",
                        ctx.db.nearby_transforms().count(),
                        ctx.db.entity().count(),
                    );
                })
                .on_error(|_ctx, err| {
                    log::error!("Subscription error: {err}");
                })
                .subscribe([
                    "SELECT * FROM my_region",
                    "SELECT * FROM nearby_transforms",
                    "SELECT * FROM entity",
                    "SELECT * FROM entity_health",
                    "SELECT * FROM sim_tick",
                    "SELECT * FROM combat_event",
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

    commands.insert_resource(StdbConnection {
        conn,
        connected,
    });
    commands.insert_resource(LocalPlayerEntity::default());
    commands.insert_resource(TickCounter::default());
}

/// Pump the SpacetimeDB connection each frame (processes callbacks).
fn pump_connection(
    stdb: Option<Res<StdbConnection>>,
    mut tick_counter: ResMut<TickCounter>,
) {
    let Some(stdb) = stdb else { return };
    let _ = stdb.conn.frame_tick();

    // Track the latest sim tick for intent submission.
    for tick in stdb.conn.db.sim_tick().iter() {
        if tick.tick_id > tick_counter.last_tick {
            tick_counter.last_tick = tick.tick_id;
        }
    }
}
