//! Multi-client integration tests for a running Jump server.
//!
//! Validates that multiple simultaneous SDK connections work correctly,
//! including the critical scenario where one client disconnecting does NOT
//! break subscriptions for other clients (SpacetimeDB #4648, fixed in 2.1.0).
//!
//! Run via: `cargo run -p game_client --features connected -- --test-multi`
//!
//! Prerequisites:
//!   - SpacetimeDB 2.1.0+ server running (`spacetime start`)
//!   - Module published and simulation worker running

use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::module_bindings::*;
use spacetimedb_sdk::{DbContext, Table};

use crate::client::ClientConfig;

// ── Test Result Tracking ────────────────────────────────────────────

struct TestResults {
    passes: u32,
    failures: u32,
    warnings: u32,
    log: Vec<String>,
}

impl TestResults {
    fn new() -> Self {
        Self { passes: 0, failures: 0, warnings: 0, log: Vec::new() }
    }
    fn pass(&mut self, label: &str) {
        info!("  PASS  {label}");
        self.log.push(format!("PASS: {label}"));
        self.passes += 1;
    }
    fn fail(&mut self, label: &str) {
        error!("  FAIL  {label}");
        self.log.push(format!("FAIL: {label}"));
        self.failures += 1;
    }
    fn warn_msg(&mut self, label: &str) {
        warn!("  WARN  {label}");
        self.log.push(format!("WARN: {label}"));
        self.warnings += 1;
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

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

/// Pump two connections simultaneously.
fn pump_both(a: &DbConnection, b: &DbConnection, millis: u64) {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        let _ = a.frame_tick();
        let _ = b.frame_tick();
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_both(
    a: &DbConnection,
    b: &DbConnection,
    timeout_ms: u64,
    condition: impl Fn() -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let _ = a.frame_tick();
        let _ = b.frame_tick();
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

// ── Connection Builder ──────────────────────────────────────────────

struct ClientHandle {
    conn: DbConnection,
    sub_applied: Arc<AtomicBool>,
    entity_id: Arc<AtomicU64>,
}

fn connect_fresh_client(uri: &str, module_name: &str) -> ClientHandle {
    let sub_applied = Arc::new(AtomicBool::new(false));
    let entity_id = Arc::new(AtomicU64::new(0));
    let connected = Arc::new(AtomicBool::new(false));

    let sa = Arc::clone(&sub_applied);
    let eid = Arc::clone(&entity_id);
    let cf = Arc::clone(&connected);

    // No token — each call creates a fresh anonymous identity.
    let conn = DbConnection::builder()
        .with_uri(uri)
        .with_database_name(module_name)
        .on_connect(move |ctx: &DbConnection, identity, _token: &str| {
            info!("Client connected as {identity}");
            cf.store(true, Ordering::SeqCst);

            let sa2 = Arc::clone(&sa);
            let eid2 = Arc::clone(&eid);

            // Spawn player first.
            let identity = ctx.identity();
            let _ = ctx.reducers().spawn_player_then(move |ctx, result| {
                match result {
                    Ok(Ok(())) => info!("spawn_player succeeded"),
                    Ok(Err(e)) => {
                        if e.contains("already spawned") {
                            info!("Player already spawned (OK)");
                        } else {
                            warn!("spawn_player error: {e}");
                        }
                    }
                    Err(e) => warn!("spawn_player internal error: {e}"),
                }

                let my_identity = identity;
                // Subscribe after spawn.
                ctx.subscription_builder()
                    .on_applied(move |ctx| {
                        // Resolve entity_id from client_sequence using OUR identity.
                        let id = ctx.db.client_sequence()
                            .client_identity()
                            .find(&my_identity)
                            .map(|cs| cs.entity_id)
                            .unwrap_or(0);
                        eid2.store(id, Ordering::SeqCst);
                        sa2.store(true, Ordering::SeqCst);
                        info!("Subscription applied (entity_id={id})");
                    })
                    .on_error(|_ctx, err| {
                        error!("Subscription error: {err}");
                    })
                    .subscribe([
                        "SELECT * FROM my_region",
                        "SELECT * FROM nearby_transforms",
                        "SELECT * FROM nearby_entities",
                        "SELECT * FROM nearby_health",
                        "SELECT * FROM client_sequence",
                        "SELECT * FROM sim_tick",
                        "SELECT * FROM entity",
                        "SELECT * FROM module_config",
                    ]);
            });
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(e) = err {
                error!("Disconnected: {e}");
            } else {
                info!("Disconnected cleanly");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    ClientHandle { conn, sub_applied, entity_id }
}

// ── Public Entry Points ─────────────────────────────────────────────

pub fn run_tests(config: ClientConfig) -> i32 {
    let results = Arc::new(Mutex::new(TestResults::new()));

    info!("═══ Jump Multi-Client Integration Tests ═══");
    info!("Connecting to {}:{}", config.uri, config.module_name);

    // ── M1–M3: Two clients coexist ────────────────────────────────
    run_coexistence_tests(&config, &results);

    // ── M4: Disconnect survival (SpacetimeDB #4648) ───────────────
    run_disconnect_survival_test(&config, &results);

    // ── Summary ─────────────────────────────────────────────────────
    let r = results.lock().unwrap();
    info!("");
    info!("════════════════════════════════════════════════════════════════");
    for entry in &r.log {
        if entry.starts_with("FAIL") {
            error!("  {entry}");
        } else if entry.starts_with("WARN") {
            warn!("  {entry}");
        } else {
            info!("  {entry}");
        }
    }
    if r.failures == 0 {
        if r.warnings > 0 {
            info!("  ALL {} TESTS PASSED ({} warnings)", r.passes, r.warnings);
        } else {
            info!("  ALL {} TESTS PASSED", r.passes);
        }
    } else {
        error!("  {} passed, {} FAILED, {} warnings", r.passes, r.failures, r.warnings);
    }
    info!("════════════════════════════════════════════════════════════════");

    if r.failures > 0 { 1 } else { 0 }
}

// ═══════════════════════════════════════════════════════════════════════
// M1–M3: Two-Client Coexistence
// ═══════════════════════════════════════════════════════════════════════

fn run_coexistence_tests(config: &ClientConfig, results: &Arc<Mutex<TestResults>>) {
    info!("");
    info!("── Two-Client Coexistence Tests ──────────────────────────────");

    let client_a = connect_fresh_client(&config.uri, &config.module_name);
    let client_b = connect_fresh_client(&config.uri, &config.module_name);

    // Wait for both subscriptions to apply.
    let sa_a = Arc::clone(&client_a.sub_applied);
    let sa_b = Arc::clone(&client_b.sub_applied);
    let both_ready = wait_for_both(
        &client_a.conn,
        &client_b.conn,
        15000,
        || sa_a.load(Ordering::SeqCst) && sa_b.load(Ordering::SeqCst),
    );

    let mut r = results.lock().unwrap();
    if !both_ready {
        r.fail("M0  timed out waiting for both clients to subscribe");
        return;
    }
    r.pass("M0  both clients connected and subscribed");

    let eid_a = client_a.entity_id.load(Ordering::SeqCst);
    let eid_b = client_b.entity_id.load(Ordering::SeqCst);

    // M1: Both clients have distinct entity IDs.
    if eid_a != 0 && eid_b != 0 && eid_a != eid_b {
        r.pass(&format!("M1  distinct entity IDs (A={eid_a}, B={eid_b})"));
    } else {
        r.fail(&format!("M1  entity ID issue (A={eid_a}, B={eid_b})"));
    }

    // M2: Client A sees client B in nearby_transforms (both spawn at origin → same cell).
    drop(r);
    pump_both(&client_a.conn, &client_b.conn, 600);
    let mut r = results.lock().unwrap();

    let a_sees_b = client_a.conn.db().nearby_transforms().iter()
        .any(|t| t.entity_id == eid_b);
    let b_sees_a = client_b.conn.db().nearby_transforms().iter()
        .any(|t| t.entity_id == eid_a);

    if a_sees_b {
        r.pass("M2a  client A sees client B in nearby_transforms");
    } else {
        r.fail("M2a  client A does NOT see client B");
    }
    if b_sees_a {
        r.pass("M2b  client B sees client A in nearby_transforms");
    } else {
        r.fail("M2b  client B does NOT see client A");
    }

    // M3: Intent from A causes transform update visible to B.
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let _ = client_a.conn.reducers().submit_intent(
        eid_a,
        seq,
        IntentAction::Move(MoveDir { dir_x: 1.0, dir_y: 0.0, dir_z: 0.0 }),
        0,
    );
    let _ = client_a.conn.reducers().submit_intent(
        eid_a,
        seq + 1,
        IntentAction::Stop,
        0,
    );

    drop(r);
    pump_both(&client_a.conn, &client_b.conn, 800);
    let mut r = results.lock().unwrap();

    // Check that B's view of A's transform has a nonzero pos_x (moved in +X).
    let b_view_of_a = client_b.conn.db().nearby_transforms().iter()
        .find(|t| t.entity_id == eid_a);
    match b_view_of_a {
        Some(t) if t.pos_x > 0.01 => {
            r.pass(&format!("M3  client B observed A's movement (pos_x={:.2})", t.pos_x));
        }
        Some(t) => {
            r.warn_msg(&format!("M3  client B sees A but pos_x={:.3} (may need more ticks)", t.pos_x));
        }
        None => {
            r.fail("M3  client B lost sight of A after movement");
        }
    }

    // Cleanup: remove both test entities.
    let _ = client_a.conn.reducers().debug_remove_entity(eid_a);
    let _ = client_b.conn.reducers().debug_remove_entity(eid_b);
    drop(r);
    pump_both(&client_a.conn, &client_b.conn, 300);
}

// ═══════════════════════════════════════════════════════════════════════
// M4: Disconnect Survival (SpacetimeDB #4648)
//
// With 2.0.x, disconnecting client A could drop subscriptions for
// client B. This test verifies the 2.1.0 fix.
// ═══════════════════════════════════════════════════════════════════════

fn run_disconnect_survival_test(config: &ClientConfig, results: &Arc<Mutex<TestResults>>) {
    info!("");
    info!("── Disconnect Survival Test (#4648) ─────────────────────────");

    let client_a = connect_fresh_client(&config.uri, &config.module_name);
    let client_b = connect_fresh_client(&config.uri, &config.module_name);

    let sa_a = Arc::clone(&client_a.sub_applied);
    let sa_b = Arc::clone(&client_b.sub_applied);
    let both_ready = wait_for_both(
        &client_a.conn,
        &client_b.conn,
        15000,
        || sa_a.load(Ordering::SeqCst) && sa_b.load(Ordering::SeqCst),
    );

    let mut r = results.lock().unwrap();
    if !both_ready {
        r.fail("M4-setup  timed out waiting for both clients");
        return;
    }

    let eid_a = client_a.entity_id.load(Ordering::SeqCst);
    let eid_b = client_b.entity_id.load(Ordering::SeqCst);

    // Verify B has a working subscription before the disconnect.
    let b_has_region_before = client_b.conn.db().my_region().count() >= 1;
    let b_nearby_before = client_b.conn.db().nearby_transforms().count();
    if !b_has_region_before {
        r.fail("M4-pre  client B has no my_region before disconnect");
        drop(r);
        return;
    }
    r.pass(&format!(
        "M4-pre  client B subscription healthy (region=1, nearby={b_nearby_before})"
    ));
    drop(r);

    // ── Disconnect client A ──
    info!("  M4: disconnecting client A (entity_id={eid_a})...");
    drop(client_a);

    // Give the server time to process the disconnect.
    pump(&client_b.conn, 1000);

    let mut r = results.lock().unwrap();

    // M4a: Client B's subscription still works — my_region still present.
    let b_has_region_after = client_b.conn.db().my_region().count() >= 1;
    if b_has_region_after {
        r.pass("M4a  client B my_region survived A's disconnect");
    } else {
        r.fail("M4a  client B my_region LOST after A disconnected (#4648 regression!)");
    }

    // M4b: Client B can still submit intents successfully.
    let seq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let intent_done = Arc::new(AtomicBool::new(false));
    let intent_ok = Arc::new(AtomicBool::new(false));
    let id = Arc::clone(&intent_done);
    let io = Arc::clone(&intent_ok);
    let _ = client_b.conn.reducers().submit_intent_then(
        eid_b,
        seq + 1000,
        IntentAction::Stop,
        seq,
        move |_ctx, result| {
            if let Ok(Ok(())) = result { io.store(true, Ordering::SeqCst); }
            id.store(true, Ordering::SeqCst);
        },
    );
    drop(r);
    wait_for(&client_b.conn, 5000, || intent_done.load(Ordering::SeqCst));
    let mut r = results.lock().unwrap();

    if intent_ok.load(Ordering::SeqCst) {
        r.pass("M4b  client B can still submit intents after A disconnected");
    } else {
        r.fail("M4b  client B intent FAILED after A disconnected");
    }

    // M4c: Client B's view still receives updates (tick advancement).
    let tick_before = client_b.conn.db().sim_tick().iter()
        .map(|t| t.tick_id)
        .max()
        .unwrap_or(0);

    drop(r);
    pump(&client_b.conn, 600);
    let mut r = results.lock().unwrap();

    let tick_after = client_b.conn.db().sim_tick().iter()
        .map(|t| t.tick_id)
        .max()
        .unwrap_or(0);

    if tick_after > tick_before {
        r.pass(&format!(
            "M4c  client B still receiving tick updates ({tick_before} → {tick_after})"
        ));
    } else {
        r.fail(&format!(
            "M4c  client B tick updates STALLED after A disconnected ({tick_before} → {tick_after})"
        ));
    }

    // Cleanup.
    let _ = client_b.conn.reducers().debug_remove_entity(eid_b);
    // A's entity was dropped with the connection; server handles via client_disconnected.
    let _ = client_b.conn.reducers().debug_remove_entity(eid_a);
    drop(r);
    pump(&client_b.conn, 300);
}
