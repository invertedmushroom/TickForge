use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spacetimedb_sdk::{DbContext, Table};

use crate::client::{ClientConfig, load_token, save_token};
use crate::module_bindings::*;

fn pump(conn: &DbConnection, millis: u64) {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        let _ = conn.frame_tick();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for(conn: &DbConnection, timeout_ms: u64, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let _ = conn.frame_tick();
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn current_client_sequence(conn: &DbConnection) -> Option<ClientSequence> {
    let identity = conn.try_identity()?;
    conn.db()
        .client_sequence()
        .client_identity()
        .find(&identity)
}

fn facing_xz_from_transform(transform: &EntityTransform) -> (f32, f32) {
    let x = 2.0 * (transform.rot_x * transform.rot_z + transform.rot_w * transform.rot_y);
    let z = 1.0 - 2.0 * (transform.rot_x * transform.rot_x + transform.rot_y * transform.rot_y);
    let len_sq = x * x + z * z;
    if len_sq > 1e-6 {
        let inv_len = 1.0 / len_sq.sqrt();
        (x * inv_len, z * inv_len)
    } else {
        (0.0, 1.0)
    }
}

fn first_free_inventory_slot(conn: &DbConnection, owner_entity: u64) -> Option<u32> {
    (0..64).find(|slot| {
        !conn
            .db()
            .player_inventory()
            .iter()
            .any(|row| row.owner_entity == owner_entity && row.slot_index == *slot)
    })
}

pub fn claim_loot(config: ClientConfig, loot_pile_id: u64, item_id: u32, target_slot: u32) -> i32 {
    let auth_token = config.auth_token.or_else(load_token);

    let connected = Arc::new(AtomicBool::new(false));
    let spawn_done = Arc::new(AtomicBool::new(false));
    let subscribed = Arc::new(AtomicBool::new(false));
    let claim_done = Arc::new(AtomicBool::new(false));
    let claim_ok = Arc::new(AtomicBool::new(false));
    let claim_error = Arc::new(Mutex::new(None::<String>));

    let connected_flag = Arc::clone(&connected);
    let spawn_done_flag = Arc::clone(&spawn_done);

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, token: &str| {
            save_token(token);
            info!("Connected as {identity}");
            connected_flag.store(true, Ordering::SeqCst);

            let spawn_done_inner = Arc::clone(&spawn_done_flag);
            if let Err(err) = ctx.reducers().spawn_player_then(move |_ctx, result| {
                match result {
                    Ok(Ok(())) => info!("spawn_player succeeded"),
                    Ok(Err(err)) if err.contains("already spawned") => {
                        info!("Player already spawned (OK)")
                    }
                    Ok(Err(err)) => warn!("spawn_player returned: {err}"),
                    Err(err) => warn!("spawn_player internal error: {err}"),
                }
                spawn_done_inner.store(true, Ordering::SeqCst);
            }) {
                warn!("Failed to send spawn_player: {err}");
                spawn_done_flag.store(true, Ordering::SeqCst);
            }
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(err) = err {
                error!("Disconnected: {err}");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    if !wait_for(&conn, 5000, || connected.load(Ordering::SeqCst)) {
        error!("Timed out waiting for connection");
        return 1;
    }

    if !wait_for(&conn, 5000, || spawn_done.load(Ordering::SeqCst)) {
        error!("Timed out waiting for spawn_player callback");
        return 1;
    }

    let subscribed_flag = Arc::clone(&subscribed);
    conn.subscription_builder()
        .on_applied(move |_ctx| {
            subscribed_flag.store(true, Ordering::SeqCst);
        })
        .on_error(|_ctx, err| {
            error!("Subscription error: {err}");
        })
        .subscribe([
            "SELECT * FROM client_sequence",
            "SELECT * FROM entity_layer",
            "SELECT * FROM loot_pile",
            "SELECT * FROM loot_pile_item",
            "SELECT * FROM player_inventory",
        ]);

    if !wait_for(&conn, 5000, || subscribed.load(Ordering::SeqCst)) {
        error!("Timed out waiting for loot claim subscription");
        return 1;
    }
    pump(&conn, 100);

    let Some(seq) = current_client_sequence(&conn) else {
        error!("Connected identity has no client_sequence; run through spawn_player first");
        return 1;
    };
    info!("Claiming as entity {}", seq.entity_id);

    let Some(pile) = conn.db().loot_pile().loot_pile_id().find(&loot_pile_id) else {
        error!("loot_pile {loot_pile_id} is not visible; it may be expired or already claimed");
        return 1;
    };
    if !pile.eligible_claimants.contains(&seq.entity_id) {
        error!(
            "entity {} is not eligible for loot_pile {} (eligible={:?})",
            seq.entity_id, loot_pile_id, pile.eligible_claimants
        );
        return 1;
    }

    let claimant_layer = conn
        .db()
        .entity_layer()
        .entity_id()
        .find(&seq.entity_id)
        .map(|row| row.layer);
    if claimant_layer != Some(pile.layer) {
        error!(
            "entity {} layer {:?} does not match loot_pile layer {}",
            seq.entity_id, claimant_layer, pile.layer
        );
        return 1;
    }

    let Some(item) = conn
        .db()
        .loot_pile_item()
        .iter()
        .find(|row| row.loot_pile_id == loot_pile_id && row.item_id == item_id)
    else {
        error!("loot_pile {loot_pile_id} has no visible item_id {item_id}");
        return 1;
    };
    info!(
        "Claim target: loot_pile={} item_id={} quantity={} target_slot={}",
        loot_pile_id, item.item_id, item.quantity, target_slot
    );

    if conn
        .db()
        .player_inventory()
        .iter()
        .any(|row| row.owner_entity == seq.entity_id && row.slot_index == target_slot)
    {
        error!("target inventory slot {target_slot} is already occupied");
        return 1;
    }

    let claim_done_flag = Arc::clone(&claim_done);
    let claim_ok_flag = Arc::clone(&claim_ok);
    let claim_error_slot = Arc::clone(&claim_error);
    if let Err(err) =
        conn.reducers()
            .claim_loot_then(loot_pile_id, item_id, target_slot, move |_ctx, result| {
                match result {
                    Ok(Ok(())) => claim_ok_flag.store(true, Ordering::SeqCst),
                    Ok(Err(err)) => {
                        *claim_error_slot.lock().unwrap() = Some(err);
                    }
                    Err(err) => {
                        *claim_error_slot.lock().unwrap() = Some(err.to_string());
                    }
                }
                claim_done_flag.store(true, Ordering::SeqCst);
            })
    {
        error!("Failed to send claim_loot: {err}");
        return 1;
    }

    if !wait_for(&conn, 5000, || claim_done.load(Ordering::SeqCst)) {
        error!("Timed out waiting for claim_loot callback");
        return 1;
    }

    if !claim_ok.load(Ordering::SeqCst) {
        let err = claim_error
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "unknown claim_loot error".to_string());
        error!("claim_loot rejected: {err}");
        return 1;
    }

    if !wait_for(&conn, 5000, || {
        conn.db().player_inventory().iter().any(|row| {
            row.owner_entity == seq.entity_id
                && row.slot_index == target_slot
                && row.item_id == item_id
        })
    }) {
        error!("claim_loot succeeded but inventory row did not appear");
        return 1;
    }

    info!(
        "claim_loot succeeded: entity {} received item_id {} in slot {}",
        seq.entity_id, item_id, target_slot
    );
    0
}

pub fn run_loot_smoke(config: ClientConfig) -> i32 {
    let auth_token = config.auth_token.or_else(load_token);

    let connected = Arc::new(AtomicBool::new(false));
    let spawn_done = Arc::new(AtomicBool::new(false));
    let subscribed = Arc::new(AtomicBool::new(false));

    let connected_flag = Arc::clone(&connected);
    let spawn_done_flag = Arc::clone(&spawn_done);

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, token: &str| {
            save_token(token);
            info!("Connected as {identity}");
            connected_flag.store(true, Ordering::SeqCst);

            let spawn_done_inner = Arc::clone(&spawn_done_flag);
            if let Err(err) = ctx.reducers().spawn_player_then(move |_ctx, result| {
                match result {
                    Ok(Ok(())) => info!("spawn_player succeeded"),
                    Ok(Err(err)) if err.contains("already spawned") => {
                        info!("Player already spawned (OK)")
                    }
                    Ok(Err(err)) => warn!("spawn_player returned: {err}"),
                    Err(err) => warn!("spawn_player internal error: {err}"),
                }
                spawn_done_inner.store(true, Ordering::SeqCst);
            }) {
                warn!("Failed to send spawn_player: {err}");
                spawn_done_flag.store(true, Ordering::SeqCst);
            }
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(err) = err {
                error!("Disconnected: {err}");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    if !wait_for(&conn, 5000, || connected.load(Ordering::SeqCst)) {
        error!("Timed out waiting for connection");
        return 1;
    }
    if !wait_for(&conn, 5000, || spawn_done.load(Ordering::SeqCst)) {
        error!("Timed out waiting for spawn_player callback");
        return 1;
    }

    let subscribed_flag = Arc::clone(&subscribed);
    conn.subscription_builder()
        .on_applied(move |_ctx| {
            subscribed_flag.store(true, Ordering::SeqCst);
        })
        .on_error(|_ctx, err| {
            error!("Subscription error: {err}");
        })
        .subscribe([
            "SELECT * FROM client_sequence",
            "SELECT * FROM nearby_entities",
            "SELECT * FROM nearby_transforms",
            "SELECT * FROM nearby_health",
            "SELECT * FROM entity_layer",
            "SELECT * FROM combat_event",
            "SELECT * FROM loot_pile",
            "SELECT * FROM loot_pile_item",
            "SELECT * FROM player_inventory",
        ]);

    if !wait_for(&conn, 5000, || subscribed.load(Ordering::SeqCst)) {
        error!("Timed out waiting for loot smoke subscription");
        return 1;
    }
    pump(&conn, 200);

    let Some(seq) = current_client_sequence(&conn) else {
        error!("Connected identity has no client_sequence");
        return 1;
    };
    let Some(player_tf) = conn
        .db()
        .nearby_transforms()
        .iter()
        .find(|row| row.entity_id == seq.entity_id)
    else {
        error!(
            "No nearby_transforms row for player entity {}; AOI subscription may not be initialized",
            seq.entity_id
        );
        return 1;
    };
    let before_max_entity = conn
        .db()
        .nearby_entities()
        .iter()
        .map(|row| row.entity_id)
        .max()
        .unwrap_or(0);

    let (forward_x, forward_z) = facing_xz_from_transform(&player_tf);
    let spawn_done = Arc::new(AtomicBool::new(false));
    let spawn_ok = Arc::new(AtomicBool::new(false));
    let spawn_err = Arc::new(Mutex::new(None::<String>));
    let spawn_done_flag = Arc::clone(&spawn_done);
    let spawn_ok_flag = Arc::clone(&spawn_ok);
    let spawn_err_slot = Arc::clone(&spawn_err);

    if let Err(err) = conn.reducers().debug_spawn_encounter_boss_then(
        seq.entity_id,
        "crucible_warden".to_string(),
        forward_x * 0.75,
        0.0,
        forward_z * 0.75,
        20.0,
        move |_ctx, result| {
            match result {
                Ok(Ok(())) => spawn_ok_flag.store(true, Ordering::SeqCst),
                Ok(Err(err)) => *spawn_err_slot.lock().unwrap() = Some(err),
                Err(err) => *spawn_err_slot.lock().unwrap() = Some(err.to_string()),
            }
            spawn_done_flag.store(true, Ordering::SeqCst);
        },
    ) {
        error!("Failed to send debug_spawn_encounter_boss: {err}");
        return 1;
    }
    if !wait_for(&conn, 5000, || spawn_done.load(Ordering::SeqCst)) {
        error!("Timed out waiting for debug_spawn_encounter_boss callback");
        return 1;
    }
    if !spawn_ok.load(Ordering::SeqCst) {
        let err = spawn_err
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "unknown spawn error".to_string());
        error!("debug_spawn_encounter_boss rejected: {err}");
        return 1;
    }

    let boss_id = if wait_for(&conn, 5000, || {
        conn.db().nearby_entities().iter().any(|row| {
            row.entity_id > before_max_entity
                && row.kind == EntityKind::Boss
                && row.state == EntityState::Active
        })
    }) {
        conn.db()
            .nearby_entities()
            .iter()
            .filter(|row| row.entity_id > before_max_entity)
            .filter(|row| row.kind == EntityKind::Boss && row.state == EntityState::Active)
            .map(|row| row.entity_id)
            .max()
            .expect("boss found after wait")
    } else {
        error!("Spawned encounter boss did not become Active");
        return 1;
    };
    info!("Spawned crucible_warden boss entity {boss_id}");

    let seq_base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let last_processed = current_client_sequence(&conn)
        .map(|row| row.last_processed_sequence)
        .unwrap_or(0);
    let sequence_id = seq_base.max(last_processed + 1);
    if let Err(err) = conn.reducers().submit_intent(
        seq.entity_id,
        sequence_id,
        IntentAction::UseAbility(UseAbilityData {
            ability_id: 1,
            target: AbilityTarget::None,
            target_hint: None,
        }),
        0,
    ) {
        error!("Failed to send Slash intent: {err}");
        return 1;
    }
    info!("Sent Slash intent sequence_id={sequence_id}");

    if !wait_for(&conn, 8000, || {
        conn.db()
            .loot_pile()
            .iter()
            .any(|row| row.corpse_entity == boss_id)
    }) {
        let health = conn
            .db()
            .nearby_health()
            .iter()
            .find(|row| row.entity_id == boss_id);
        let state = conn
            .db()
            .nearby_entities()
            .iter()
            .find(|row| row.entity_id == boss_id);
        error!(
            "Timed out waiting for loot pile from boss {boss_id}; health={health:?} state={state:?}"
        );
        return 1;
    }

    let pile = conn
        .db()
        .loot_pile()
        .iter()
        .find(|row| row.corpse_entity == boss_id)
        .expect("pile found after wait");
    let Some(item) = conn
        .db()
        .loot_pile_item()
        .iter()
        .find(|row| row.loot_pile_id == pile.loot_pile_id)
    else {
        error!("Loot pile {} has no visible items", pile.loot_pile_id);
        return 1;
    };
    let Some(target_slot) = first_free_inventory_slot(&conn, seq.entity_id) else {
        error!("No free inventory slot for entity {}", seq.entity_id);
        return 1;
    };

    info!(
        "Loot pile ready: pile={} item={} qty={} slot={}",
        pile.loot_pile_id, item.item_id, item.quantity, target_slot
    );

    let claim_done = Arc::new(AtomicBool::new(false));
    let claim_ok = Arc::new(AtomicBool::new(false));
    let claim_err = Arc::new(Mutex::new(None::<String>));
    let claim_done_flag = Arc::clone(&claim_done);
    let claim_ok_flag = Arc::clone(&claim_ok);
    let claim_err_slot = Arc::clone(&claim_err);
    if let Err(err) = conn.reducers().claim_loot_then(
        pile.loot_pile_id,
        item.item_id,
        target_slot,
        move |_ctx, result| {
            match result {
                Ok(Ok(())) => claim_ok_flag.store(true, Ordering::SeqCst),
                Ok(Err(err)) => *claim_err_slot.lock().unwrap() = Some(err),
                Err(err) => *claim_err_slot.lock().unwrap() = Some(err.to_string()),
            }
            claim_done_flag.store(true, Ordering::SeqCst);
        },
    ) {
        error!("Failed to send claim_loot: {err}");
        return 1;
    }
    if !wait_for(&conn, 5000, || claim_done.load(Ordering::SeqCst)) {
        error!("Timed out waiting for claim_loot callback");
        return 1;
    }
    if !claim_ok.load(Ordering::SeqCst) {
        let err = claim_err
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "unknown claim error".to_string());
        error!("claim_loot rejected: {err}");
        return 1;
    }

    if !wait_for(&conn, 5000, || {
        conn.db().player_inventory().iter().any(|row| {
            row.owner_entity == seq.entity_id
                && row.slot_index == target_slot
                && row.item_id == item.item_id
        })
    }) {
        error!("claim_loot succeeded but inventory row did not appear");
        return 1;
    }

    info!(
        "loot smoke succeeded: boss={} pile={} item={} slot={}",
        boss_id, pile.loot_pile_id, item.item_id, target_slot
    );
    0
}
