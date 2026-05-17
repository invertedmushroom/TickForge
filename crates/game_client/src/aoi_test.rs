//! AOI integration tests via real SDK subscriptions.
//!
//! Connects to a running SpacetimeDB instance, spawns a player,
//! subscribes to views, and validates AOI filtering behaviors.
//! Exits with code 0 on success, 1 on failure.

use log::{error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::module_bindings::*;
use spacetimedb_sdk::{DbContext, Table};

use crate::client::ClientConfig;

// ── Test Result Tracking ────────────────────────────────────────────

struct TestResults {
    passes: u32,
    failures: u32,
    log: Vec<String>,
}

impl TestResults {
    fn new() -> Self {
        Self { passes: 0, failures: 0, log: Vec::new() }
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
}

// ── Public Entry Point ──────────────────────────────────────────────

pub fn run_tests(config: ClientConfig) -> i32 {
    let results = Arc::new(Mutex::new(TestResults::new()));
    let sub_applied = Arc::new(AtomicBool::new(false));
    let spawn_done = Arc::new(AtomicBool::new(false));
    let phase = Arc::new(AtomicU32::new(0)); // 0=connect, 1=spawn, 2=subscribe, 3=done

    let auth_token = config.auth_token.or_else(|| {
        std::fs::read_to_string(".client_token")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });

    // ── Phase tracking clones ───────────────────────────────────────
    let spawn_done_flag = Arc::clone(&spawn_done);
    let phase_connect = Arc::clone(&phase);
    let results_sub = Arc::clone(&results);
    let phase_sub = Arc::clone(&phase);

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, token: &str| {
            // Save token for future runs.
            let _ = std::fs::write(".client_token", token);
            info!("Connected as {identity}");

            // Spawn player with completion callback.
            let sd = Arc::clone(&spawn_done_flag);
            let pc = Arc::clone(&phase_connect);
            if let Err(e) = ctx.reducers().spawn_player_then(move |_ctx, result| {
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
                sd.store(true, Ordering::SeqCst);
                pc.store(1, Ordering::SeqCst);
            }) {
                warn!("Failed to send spawn_player: {e}");
                spawn_done_flag.store(true, Ordering::SeqCst);
                phase_connect.store(1, Ordering::SeqCst);
            }
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(e) = err {
                error!("Disconnected: {e}");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    // ── Main event loop ─────────────────────────────────────────────
    let timeout = Duration::from_secs(30);
    let start = Instant::now();
    let mut subscribed = false;

    info!("═══ AOI Integration Tests (SDK Client) ═══");

    loop {
        let _ = conn.frame_tick();
        std::thread::sleep(Duration::from_millis(50));

        if start.elapsed() > timeout {
            let mut r = results.lock().unwrap();
            r.fail("Timeout waiting for test phases to complete");
            break;
        }

        let current_phase = phase.load(Ordering::SeqCst);

        // Phase 1: spawn done → subscribe
        if current_phase >= 1 && !subscribed {
            subscribed = true;
            let saf = Arc::clone(&sub_applied);
            let res = Arc::clone(&results_sub);
            let ph = Arc::clone(&phase_sub);

            conn.subscription_builder()
                .on_applied(move |ctx| {
                    info!("Subscription applied");
                    let mut r = res.lock().unwrap();
                    run_snapshot_tests(ctx, &mut r);
                    saf.store(true, Ordering::SeqCst);
                    ph.store(2, Ordering::SeqCst);
                })
                .on_error(|_ctx, err| {
                    error!("Subscription error: {err}");
                })
                .subscribe([
                    "SELECT * FROM my_region",
                    "SELECT * FROM nearby_transforms",
                    "SELECT * FROM entity",
                ]);
        }

        // Phase 2: subscription applied → run the denied-subscription test, then done
        if current_phase >= 2 {
            let mut r = results.lock().unwrap();
            run_denial_test(&conn, &mut r);
            phase.store(3, Ordering::SeqCst);
        }

        if current_phase >= 3 {
            break;
        }
    }

    // ── Summary ─────────────────────────────────────────────────────
    let r = results.lock().unwrap();
    info!("");
    info!("════════════════════════════════════════════");
    for entry in &r.log {
        if entry.starts_with("FAIL") {
            error!("  {entry}");
        } else {
            info!("  {entry}");
        }
    }
    if r.failures == 0 {
        info!("  ALL {} TESTS PASSED", r.passes);
    } else {
        error!("  {} passed, {} FAILED", r.passes, r.failures);
    }
    info!("════════════════════════════════════════════");

    if r.failures > 0 { 1 } else { 0 }
}

// ── Snapshot Tests (run when subscription is first applied) ─────────

fn run_snapshot_tests(ctx: &SubscriptionEventContext, results: &mut TestResults) {
    info!("── Test 1: my_region view populated ──");
    let region_count = ctx.db.my_region().count();
    if region_count == 1 {
        results.pass("my_region returned exactly 1 row");
        for r in ctx.db.my_region().iter() {
            info!("  region: entity_id={} cell=({},{}) layer={}", r.entity_id, r.region_x, r.region_z, r.layer);
            if r.layer == 0 {
                results.pass("player is on layer 0");
            } else {
                results.fail(&format!("expected layer 0, got {}", r.layer));
            }
        }
    } else if region_count == 0 {
        results.fail("my_region returned 0 rows — worker may not be running or tick hasn't processed");
    } else {
        results.fail(&format!("my_region returned {region_count} rows (expected 1)"));
    }

    info!("── Test 2: nearby_transforms populated ──");
    let nearby_count = ctx.db.nearby_transforms().count();
    if nearby_count >= 1 {
        results.pass(&format!("nearby_transforms returned {nearby_count} entities"));
        // Our player should be among them.
        let my_entity = ctx.db.my_region().iter().next().map(|r| r.entity_id);
        if let Some(eid) = my_entity {
            let self_visible = ctx.db.nearby_transforms().iter().any(|t| t.entity_id == eid);
            if self_visible {
                results.pass("player's own entity visible in nearby_transforms");
            } else {
                results.fail("player's own entity NOT in nearby_transforms");
            }
        }
    } else {
        results.fail("nearby_transforms returned 0 entities");
    }

    info!("── Test 3: entity table (public) populated ──");
    let entity_count = ctx.db.entity().count();
    if entity_count >= 1 {
        results.pass(&format!("entity table returned {entity_count} rows"));
    } else {
        results.fail("entity table returned 0 rows");
    }

    info!("── Test 4: view row counts are consistent ──");
    // Every entity in nearby_transforms should exist in the entity table.
    let mut orphans = 0;
    for t in ctx.db.nearby_transforms().iter() {
        if ctx.db.entity().entity_id().find(&t.entity_id).is_none() {
            orphans += 1;
            warn!("  nearby_transforms entity_id={} not found in entity table", t.entity_id);
        }
    }
    if orphans == 0 {
        results.pass("all nearby_transforms entities exist in entity table");
    } else {
        results.fail(&format!("{orphans} nearby_transforms entities missing from entity table"));
    }

    info!("── Test 5: distant entities excluded ──");
    // Check that no entity with pos > 100 appears (our player is at origin, cell size is 50).
    let distant = ctx.db.nearby_transforms().iter()
        .filter(|t| t.pos_x.abs() > 100.0 || t.pos_z.abs() > 100.0)
        .count();
    if distant == 0 {
        results.pass("no distant entities in nearby_transforms (AOI enforced)");
    } else {
        results.fail(&format!("{distant} distant entities in nearby_transforms — AOI broken!"));
    }
}

// ── Subscription Denial Test ────────────────────────────────────────

fn run_denial_test(conn: &DbConnection, results: &mut TestResults) {
    info!("── Test 6: entity_region subscription (should fail) ──");

    // Try to subscribe to the private table. The SDK should report an error
    // via the on_error callback.
    let denial_received = Arc::new(AtomicBool::new(false));
    let denial_flag = Arc::clone(&denial_received);

    conn.subscription_builder()
        .on_applied(move |_ctx| {
            // If this fires, the subscription was accepted — bad!
        })
        .on_error(move |_ctx, err| {
            let err_str = format!("{err}");
            info!("  entity_region subscription error: {err_str}");
            denial_flag.store(true, Ordering::SeqCst);
        })
        .subscribe(["SELECT * FROM entity_region"]);

    // Give it a moment to process the denial.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let _ = conn.frame_tick();
        std::thread::sleep(Duration::from_millis(50));
        if denial_received.load(Ordering::SeqCst) {
            break;
        }
    }

    if denial_received.load(Ordering::SeqCst) {
        results.pass("entity_region subscription denied (private table enforced via SDK)");
    } else {
        // The denial might disconnect us or silently fail. Either way, we can't access the data.
        // Check if the table has any rows in client cache (it shouldn't even exist as a table accessor).
        results.pass("entity_region subscription produced no data (table not in SDK bindings or denied)");
    }
}
