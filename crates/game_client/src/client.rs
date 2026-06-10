use log::{error, info, warn};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::module_bindings::*;
use spacetimedb_sdk::{DbContext, Table, TableWithPrimaryKey};

// ── Configuration ───────────────────────────────────────────────────

pub struct ClientConfig {
    pub uri: String,
    pub module_name: String,
    pub auth_token: Option<String>,
}

const TOKEN_FILE: &str = ".client_token";

pub(crate) fn load_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn save_token(token: &str) {
    if let Err(e) = std::fs::write(TOKEN_FILE, token) {
        warn!("Failed to save client token to {TOKEN_FILE}: {e}");
    } else {
        info!("Client token saved to {TOKEN_FILE}");
    }
}

// ── Connection ──────────────────────────────────────────────────────

pub fn run(config: ClientConfig) {
    let auth_token = config.auth_token.or_else(load_token);
    if auth_token.is_some() && Path::new(TOKEN_FILE).exists() {
        info!("Using persisted client token from {TOKEN_FILE}");
    }

    let connected = Arc::new(AtomicBool::new(false));
    let connected_flag = Arc::clone(&connected);

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, token: &str| {
            save_token(token);
            info!("Connected as {identity}");
            connected_flag.store(true, Ordering::SeqCst);
            subscribe(ctx);
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(e) = err {
                error!("Disconnected: {e}");
            } else {
                info!("Disconnected");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    // Register callbacks before the subscription snapshot arrives.
    register_callbacks(&conn);

    // Pump the connection until Ctrl-C.
    info!("Client running — press Ctrl-C to stop");
    loop {
        let _ = conn.frame_tick();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// ── Subscriptions ───────────────────────────────────────────────────

fn subscribe(ctx: &DbConnection) {
    ctx.subscription_builder()
        .on_applied(|ctx| {
            info!(
                "Subscription applied — my_region: {} rows, nearby_transforms: {} rows",
                ctx.db.my_region().count(),
                ctx.db.nearby_transforms().count(),
            );

            // Show what the server thinks our region is.
            for r in ctx.db.my_region().iter() {
                info!(
                    "  My region: entity_id={} cell=({},{}) layer={}",
                    r.entity_id, r.region_x, r.region_z, r.layer
                );
            }

            // Show nearby entities.
            for t in ctx.db.nearby_transforms().iter() {
                info!(
                    "  Nearby: entity_id={} pos=({:.1},{:.1},{:.1})",
                    t.entity_id, t.pos_x, t.pos_y, t.pos_z
                );
            }
        })
        .on_error(|_ctx, err| {
            error!("Subscription error: {err}");
        })
        .subscribe([
            "SELECT * FROM my_region",
            "SELECT * FROM nearby_transforms",
            "SELECT * FROM nearby_health",
            "SELECT * FROM nearby_entities",
        ]);
}

// ── Callbacks ───────────────────────────────────────────────────────

fn register_callbacks(conn: &DbConnection) {
    conn.db.my_region().on_insert(|_ctx, row| {
        info!(
            "Region entered: entity_id={} cell=({},{}) layer={}",
            row.entity_id, row.region_x, row.region_z, row.layer
        );
    });

    conn.db.my_region().on_update(|_ctx, old, row| {
        info!(
            "Region update: entity_id={} cell=({},{}) layer={} -> cell=({},{}) layer={}",
            row.entity_id,
            old.region_x,
            old.region_z,
            old.layer,
            row.region_x,
            row.region_z,
            row.layer
        );
    });

    conn.db.nearby_transforms().on_insert(|_ctx, row| {
        info!(
            "Entity entered AOI: entity_id={} pos=({:.1},{:.1},{:.1})",
            row.entity_id, row.pos_x, row.pos_y, row.pos_z
        );
    });

    conn.db.nearby_transforms().on_update(|_ctx, old, row| {
        info!(
            "Entity moved in AOI: entity_id={} pos=({:.1},{:.1},{:.1}) -> ({:.1},{:.1},{:.1}) tick={}",
            row.entity_id,
            old.pos_x,
            old.pos_y,
            old.pos_z,
            row.pos_x,
            row.pos_y,
            row.pos_z,
            row.last_tick
        );
    });

    conn.db.nearby_transforms().on_delete(|_ctx, row| {
        info!(
            "Entity left AOI: entity_id={} pos=({:.1},{:.1},{:.1})",
            row.entity_id, row.pos_x, row.pos_y, row.pos_z
        );
    });
}
