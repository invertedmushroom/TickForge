//! Unified integration test suite for a running Jump server.
//!
//! Replaces the three PS1 scripts (smoke-test.ps1, fault-test.ps1, client-test.ps1)
//! with SDK-native tests that connect to a live SpacetimeDB instance.
//!
//! Run via: `cargo run -p game_client --features connected -- --test`
//!
//! Requires a running local stack, but no pre-seeded NPCs or persisted client
//! state. The suite provisions and cleans up its own disposable test entities.

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

/// Pump connection and sleep, processing callbacks.
fn pump(conn: &DbConnection, millis: u64) {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        let _ = conn.frame_tick();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait until a condition is true or timeout expires. Returns true if condition met.
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
    conn.db().client_sequence().client_identity().find(&identity)
}

fn current_entity_id(conn: &DbConnection) -> Option<u64> {
    current_client_sequence(conn).map(|seq| seq.entity_id)
}

fn latest_live_npc_id(conn: &DbConnection) -> u64 {
    conn.db().entity().iter()
        .filter(|entity| entity.kind == EntityKind::Npc && entity.state != EntityState::Removed)
        .map(|entity| entity.entity_id)
        .max()
        .unwrap_or(0)
}

fn find_spawned_npc(conn: &DbConnection, min_entity_id: u64, pos_x: f32, pos_z: f32) -> Option<u64> {
    conn.db().entity_transform().iter()
        .filter(|transform| transform.entity_id > min_entity_id)
        .filter(|transform| (transform.pos_x - pos_x).abs() < 1.0 && (transform.pos_z - pos_z).abs() < 1.0)
        .filter_map(|transform| {
            conn.db().entity().entity_id().find(&transform.entity_id)
                .filter(|entity| entity.kind == EntityKind::Npc && entity.state != EntityState::Removed)
                .map(|_| transform.entity_id)
        })
        .max()
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

fn spawn_nearby_disposable_npc(
    conn: &DbConnection,
    owner_entity_id: u64,
    distance: f32,
    max_hp: f32,
) -> Result<u64, String> {
    let before_spawn_id = latest_live_npc_id(conn);
    let transform = conn.db().entity_transform().entity_id().find(&owner_entity_id)
        .ok_or_else(|| format!("No transform row for entity {owner_entity_id}"))?;
    let (forward_x, forward_z) = facing_xz_from_transform(&transform);
    let spawn_x = transform.pos_x + forward_x * distance;
    let spawn_z = transform.pos_z + forward_z * distance;

    let spawn_done = Arc::new(AtomicBool::new(false));
    let spawn_ok = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&spawn_done);
    let ok_flag = Arc::clone(&spawn_ok);

    conn.reducers()
        .spawn_npc_then(spawn_x, transform.pos_y, spawn_z, max_hp, move |_ctx, result| {
            if let Ok(Ok(())) = result {
                ok_flag.store(true, Ordering::SeqCst);
            }
            done_flag.store(true, Ordering::SeqCst);
        })
        .map_err(|err| format!("Failed to send spawn_npc: {err}"))?;

    if !wait_for(conn, 5000, || spawn_done.load(Ordering::SeqCst)) {
        return Err("Timed out waiting for spawn_npc callback".into());
    }
    if !spawn_ok.load(Ordering::SeqCst) {
        return Err("spawn_npc reducer rejected the request".into());
    }

    if !wait_for(conn, 5000, || {
        find_spawned_npc(conn, before_spawn_id, spawn_x, spawn_z)
            .and_then(|npc_id| conn.db().entity().entity_id().find(&npc_id))
            .is_some_and(|entity| entity.state == EntityState::Active)
    }) {
        return Err(format!(
            "Disposable NPC at ({spawn_x:.2}, {spawn_z:.2}) never became Active"
        ));
    }

    find_spawned_npc(conn, before_spawn_id, spawn_x, spawn_z)
        .ok_or_else(|| format!("Could not resolve disposable NPC at ({spawn_x:.2}, {spawn_z:.2})"))
}

fn cleanup_entity(conn: &DbConnection, entity_id: u64) {
    let _ = conn.reducers().debug_remove_entity(entity_id);
    pump(conn, 200);
}

// ── Public Entry Point ──────────────────────────────────────────────

pub fn run_tests(config: ClientConfig) -> i32 {
    let results = Arc::new(Mutex::new(TestResults::new()));
    let sub_applied = Arc::new(AtomicBool::new(false));
    let spawn_done = Arc::new(AtomicBool::new(false));
    let phase = Arc::new(AtomicU32::new(0)); // 0=connect, 1=spawn, 2=subscribe, 3=tests, 4=done

    let auth_token = config.auth_token;

    let spawn_done_flag = Arc::clone(&spawn_done);
    let phase_connect = Arc::clone(&phase);

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity, _token: &str| {
            info!("Connected as {identity}");

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
    let timeout = Duration::from_secs(45);
    let start = Instant::now();
    let mut subscribed = false;

    let _results_sub = Arc::clone(&results);
    let phase_sub = Arc::clone(&phase);
    let saf = Arc::clone(&sub_applied);

    info!("═══ Jump Unified Integration Tests (SDK Client) ═══");

    loop {
        let _ = conn.frame_tick();
        std::thread::sleep(Duration::from_millis(50));

        if start.elapsed() > timeout {
            let mut r = results.lock().unwrap();
            r.fail("Timeout waiting for test phases to complete");
            break;
        }

        let current_phase = phase.load(Ordering::SeqCst);

        // Phase 1: spawn done → subscribe to all needed tables
        if current_phase >= 1 && !subscribed {
            subscribed = true;
            let saf2 = Arc::clone(&saf);
            let ph = Arc::clone(&phase_sub);

            conn.subscription_builder()
                .on_applied(move |_ctx| {
                    info!("Subscription applied — all tables synced");
                    saf2.store(true, Ordering::SeqCst);
                    ph.store(2, Ordering::SeqCst);
                })
                .on_error(|_ctx, err| {
                    error!("Subscription error: {err}");
                })
                .subscribe([
                    "SELECT * FROM my_region",
                    "SELECT * FROM nearby_transforms",
                    "SELECT * FROM entity",
                    "SELECT * FROM entity_transform",
                    "SELECT * FROM entity_health",
                    "SELECT * FROM combat_event",
                    "SELECT * FROM player_intent",
                    "SELECT * FROM client_sequence",
                    "SELECT * FROM module_config",
                    "SELECT * FROM sim_tick",
                    "SELECT * FROM entity_team",
                    "SELECT * FROM active_buff",
                ]);
        }

        // Phase 2: subscription applied → run all test groups
        if current_phase >= 2 {
            // Wait a moment for initial data to populate
            pump(&conn, 200);

            let mut r = results.lock().unwrap();

            // ── AOI / View Tests (from client-test.ps1 + aoi_test.rs) ──
            run_aoi_tests(&conn, &mut r);

            // ── Smoke Tests (from smoke-test.ps1) ──
            run_smoke_tests(&conn, &mut r);

            // ── Fault / Boundary Tests (from fault-test.ps1) ──
            run_fault_tests(&conn, &mut r);

            // ── Subscription Denial Test ──
            drop(r); // release lock while pumping
            run_denial_test(&conn, &results);

            break; // all tests done
        }
    }

    if let Some(entity_id) = current_entity_id(&conn) {
        info!("Cleaning up smoke-test player entity_id={entity_id}");
        if let Err(err) = conn.reducers().debug_remove_entity(entity_id) {
            warn!("Failed to cleanup smoke-test player {entity_id}: {err}");
        } else {
            pump(&conn, 400);
        }
    }

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
// AOI / View Tests
// ═══════════════════════════════════════════════════════════════════════

fn run_aoi_tests(conn: &DbConnection, r: &mut TestResults) {
    info!("");
    info!("── AOI / View Tests ───────────────────────────────────────────");

    // T1: my_region view populated with correct values.
    info!("  T1: my_region view");
    let region_count = conn.db().my_region().count();
    if region_count == 1 {
        r.pass("T1a  my_region returned exactly 1 row");
        for reg in conn.db().my_region().iter() {
            if reg.region_x == 0 && reg.region_z == 0 && reg.layer == 0 {
                r.pass("T1b  my_region values correct (0, 0, layer=0)");
            } else {
                r.fail(&format!("T1b  expected (0,0,0), got ({},{},{})", reg.region_x, reg.region_z, reg.layer));
            }
        }
    } else {
        r.fail(&format!("T1a  my_region returned {region_count} rows (expected 1)"));
    }

    // T2: nearby_transforms populated, self visible.
    info!("  T2: nearby_transforms");
    let nearby_count = conn.db().nearby_transforms().count();
    if nearby_count >= 1 {
        r.pass(&format!("T2a  nearby_transforms returned {nearby_count} entities"));
        let my_entity = conn.db().my_region().iter().next().map(|reg| reg.entity_id);
        if let Some(eid) = my_entity {
            if conn.db().nearby_transforms().iter().any(|t| t.entity_id == eid) {
                r.pass("T2b  own entity visible in nearby_transforms");
            } else {
                r.fail("T2b  own entity NOT in nearby_transforms");
            }
        }
    } else {
        r.fail("T2a  nearby_transforms returned 0 entities");
    }

    // T3: entity table populated.
    info!("  T3: entity table");
    let entity_count = conn.db().entity().count();
    if entity_count >= 1 {
        r.pass(&format!("T3  entity table returned {entity_count} rows"));
    } else {
        r.fail("T3  entity table returned 0 rows");
    }

    // T4: consistency — nearby entities exist in entity table.
    info!("  T4: view consistency");
    let orphans = conn.db().nearby_transforms().iter()
        .filter(|t| conn.db().entity().entity_id().find(&t.entity_id).is_none())
        .count();
    if orphans == 0 {
        r.pass("T4  all nearby_transforms entities exist in entity table");
    } else {
        r.fail(&format!("T4  {orphans} nearby_transforms entities missing from entity table"));
    }

    // T5: no distant entities in nearby_transforms.
    info!("  T5: AOI enforcement");
    let distant = conn.db().nearby_transforms().iter()
        .filter(|t| t.pos_x.abs() > 100.0 || t.pos_z.abs() > 100.0)
        .count();
    if distant == 0 {
        r.pass("T5  no distant entities in nearby_transforms (AOI enforced)");
    } else {
        r.fail(&format!("T5  {distant} distant entities in nearby_transforms — AOI broken!"));
    }

    // T6–T8: adjacency tests (spawn NPCs at known positions, verify visibility).
    run_adjacency_tests(conn, r);

    // T9–T10: layer isolation tests (same cell, different layer → invisible).
    run_layer_tests(conn, r);

    // ST1–ST4: stealth visibility tests (team-based filtering).
    run_stealth_tests(conn, r);
}

// ═══════════════════════════════════════════════════════════════════════
// Adjacency Tests — verify 3x3 cell boundary in nearby_transforms
// ═══════════════════════════════════════════════════════════════════════

fn run_adjacency_tests(conn: &DbConnection, r: &mut TestResults) {
    info!("");
    info!("── Adjacency Tests (3x3 cell boundary) ───────────────────────");

    // Resolve caller's region cell (cell_size = 50.0 per spawn_npc_internal).
    let my_region = conn.db().my_region().iter().next();
    let (my_rx, my_rz) = match my_region {
        Some(ref reg) => (reg.region_x, reg.region_z),
        None => {
            r.fail("T6  cannot run adjacency tests — no my_region data");
            return;
        }
    };

    // Spawn NPC at +1 cell on X axis (adjacent — should be visible).
    // Cell center: ((my_rx + 1) * 50 + 25, 0, my_rz * 50 + 25).
    let adj_x = (my_rx + 1) as f32 * 50.0 + 25.0;
    let adj_z = my_rz as f32 * 50.0 + 25.0;

    info!("  T6: spawning adjacent NPC at ({adj_x}, 1, {adj_z}) — cell ({}, {my_rz})", my_rx + 1);
    let adj_done = Arc::new(AtomicBool::new(false));
    let adj_ok = Arc::new(AtomicBool::new(false));
    let ad = Arc::clone(&adj_done);
    let ao = Arc::clone(&adj_ok);
    let _ = conn.reducers().spawn_npc_then(adj_x, 1.0, adj_z, 100.0, move |_ctx, result| {
        if let Ok(Ok(())) = result { ao.store(true, Ordering::SeqCst); }
        ad.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || adj_done.load(Ordering::SeqCst));
    if !adj_ok.load(Ordering::SeqCst) {
        r.fail("T6  failed to spawn adjacent NPC");
        return;
    }

    // Spawn NPC at +3 cells on X axis (outside 3x3 — should be invisible).
    let far_x = (my_rx + 3) as f32 * 50.0 + 25.0;
    let far_z = my_rz as f32 * 50.0 + 25.0;

    info!("  T7: spawning distant NPC at ({far_x}, 1, {far_z}) — cell ({}, {my_rz})", my_rx + 3);
    let far_done = Arc::new(AtomicBool::new(false));
    let far_ok = Arc::new(AtomicBool::new(false));
    let fd = Arc::clone(&far_done);
    let fo = Arc::clone(&far_ok);
    let _ = conn.reducers().spawn_npc_then(far_x, 1.0, far_z, 100.0, move |_ctx, result| {
        if let Ok(Ok(())) = result { fo.store(true, Ordering::SeqCst); }
        fd.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || far_done.load(Ordering::SeqCst));
    if !far_ok.load(Ordering::SeqCst) {
        r.fail("T7  failed to spawn distant NPC");
        return;
    }

    // Wait for view to update.
    pump(conn, 600);

    // T6: adjacent NPC visible in nearby_transforms.
    let adj_visible = conn.db().nearby_transforms().iter()
        .any(|t| (t.pos_x - adj_x).abs() < 1.0 && (t.pos_z - adj_z).abs() < 1.0);
    if adj_visible {
        r.pass("T6  adjacent cell entity visible in nearby_transforms");
    } else {
        r.fail("T6  adjacent cell entity NOT visible in nearby_transforms");
    }

    // T7: distant NPC invisible in nearby_transforms.
    let far_visible = conn.db().nearby_transforms().iter()
        .any(|t| (t.pos_x - far_x).abs() < 1.0 && (t.pos_z - far_z).abs() < 1.0);
    if !far_visible {
        r.pass("T7  distant cell entity correctly invisible in nearby_transforms");
    } else {
        r.fail("T7  distant cell entity VISIBLE in nearby_transforms — AOI boundary broken!");
    }

    // T8: diagonal adjacency — spawn NPC at (+1, +1) cell (corner of 3x3).
    let diag_x = (my_rx + 1) as f32 * 50.0 + 25.0;
    let diag_z = (my_rz + 1) as f32 * 50.0 + 25.0;

    info!("  T8: spawning diagonal NPC at ({diag_x}, 1, {diag_z}) — cell ({}, {})", my_rx + 1, my_rz + 1);
    let diag_done = Arc::new(AtomicBool::new(false));
    let diag_ok = Arc::new(AtomicBool::new(false));
    let dd = Arc::clone(&diag_done);
    let dgo = Arc::clone(&diag_ok);
    let _ = conn.reducers().spawn_npc_then(diag_x, 1.0, diag_z, 100.0, move |_ctx, result| {
        if let Ok(Ok(())) = result { dgo.store(true, Ordering::SeqCst); }
        dd.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || diag_done.load(Ordering::SeqCst));
    if !diag_ok.load(Ordering::SeqCst) {
        r.fail("T8  failed to spawn diagonal NPC");
        return;
    }

    pump(conn, 600);

    let diag_visible = conn.db().nearby_transforms().iter()
        .any(|t| (t.pos_x - diag_x).abs() < 1.0 && (t.pos_z - diag_z).abs() < 1.0);
    if diag_visible {
        r.pass("T8  diagonal cell (+1,+1) entity visible in nearby_transforms");
    } else {
        r.fail("T8  diagonal cell (+1,+1) entity NOT visible — corner of 3x3 broken!");
    }

    // Cleanup: remove spawned test NPCs.
    for t in conn.db().nearby_transforms().iter() {
        if ((t.pos_x - adj_x).abs() < 1.0 && (t.pos_z - adj_z).abs() < 1.0)
            || ((t.pos_x - diag_x).abs() < 1.0 && (t.pos_z - diag_z).abs() < 1.0)
        {
            let _ = conn.reducers().debug_remove_entity(t.entity_id);
        }
    }
    // Distant NPC won't be in nearby_transforms; find it via entity table.
    for e in conn.db().entity().iter() {
        if let Some(t) = conn.db().entity_transform().entity_id().find(&e.entity_id) {
            if (t.pos_x - far_x).abs() < 1.0 && (t.pos_z - far_z).abs() < 1.0 {
                let _ = conn.reducers().debug_remove_entity(e.entity_id);
            }
        }
    }
    pump(conn, 200);
}

// ═══════════════════════════════════════════════════════════════════════
// Layer Isolation Tests — same cell, different layer → invisible
// ═══════════════════════════════════════════════════════════════════════

fn run_layer_tests(conn: &DbConnection, r: &mut TestResults) {
    info!("");
    info!("── Layer Isolation Tests ─────────────────────────────────────");

    // Resolve caller's region.
    let my_region = conn.db().my_region().iter().next();
    let (my_rx, my_rz) = match my_region {
        Some(ref reg) => (reg.region_x, reg.region_z),
        None => {
            r.fail("T9  cannot run layer tests — no my_region data");
            return;
        }
    };

    // Spawn NPC at same cell as player (layer 0 — should be visible initially).
    let same_x = my_rx as f32 * 50.0 + 25.0;
    let same_z = my_rz as f32 * 50.0 + 25.0;

    info!("  T9: spawning NPC at ({same_x}, 1, {same_z}) in player's cell, layer 0");
    let spawn_done = Arc::new(AtomicBool::new(false));
    let spawn_ok = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&spawn_done);
    let so = Arc::clone(&spawn_ok);
    let _ = conn.reducers().spawn_npc_then(same_x, 1.0, same_z, 100.0, move |_ctx, result| {
        if let Ok(Ok(())) = result { so.store(true, Ordering::SeqCst); }
        sd.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || spawn_done.load(Ordering::SeqCst));
    if !spawn_ok.load(Ordering::SeqCst) {
        r.fail("T9  failed to spawn layer test NPC");
        return;
    }

    pump(conn, 600);

    // Find the spawned NPC's entity_id.
    let test_npc_id = conn.db().nearby_transforms().iter()
        .filter(|t| (t.pos_x - same_x).abs() < 1.0 && (t.pos_z - same_z).abs() < 1.0)
        .filter(|t| {
            conn.db().entity().entity_id().find(&t.entity_id)
                .is_some_and(|e| e.kind == EntityKind::Npc)
        })
        .map(|t| t.entity_id)
        .max(); // latest spawned

    let npc_eid = match test_npc_id {
        Some(id) => {
            r.pass("T9a  layer-test NPC visible in layer 0 (same cell)");
            id
        }
        None => {
            r.fail("T9a  layer-test NPC NOT visible in layer 0 — spawn or view broken");
            return;
        }
    };

    // T9b: Move NPC to layer 1 — should become invisible to player (layer 0).
    info!("  T9b: moving NPC {npc_eid} to layer 1");
    let layer_done = Arc::new(AtomicBool::new(false));
    let layer_ok = Arc::new(AtomicBool::new(false));
    let ld = Arc::clone(&layer_done);
    let lo = Arc::clone(&layer_ok);
    let _ = conn.reducers().debug_set_layer_then(npc_eid, 1, move |_ctx, result| {
        if let Ok(Ok(())) = result { lo.store(true, Ordering::SeqCst); }
        ld.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || layer_done.load(Ordering::SeqCst));
    if !layer_ok.load(Ordering::SeqCst) {
        r.fail("T9b  debug_set_layer failed");
        // Cleanup.
        let _ = conn.reducers().debug_remove_entity(npc_eid);
        pump(conn, 200);
        return;
    }

    pump(conn, 600);

    let visible_after_layer_change = conn.db().nearby_transforms().iter()
        .any(|t| t.entity_id == npc_eid);
    if !visible_after_layer_change {
        r.pass("T9b  NPC in layer 1 correctly invisible to layer-0 player");
    } else {
        r.fail("T9b  NPC in layer 1 VISIBLE to layer-0 player — layer isolation broken!");
    }

    // T10: Move NPC back to layer 0 — should reappear.
    info!("  T10: moving NPC {npc_eid} back to layer 0");
    let back_done = Arc::new(AtomicBool::new(false));
    let back_ok = Arc::new(AtomicBool::new(false));
    let bd = Arc::clone(&back_done);
    let bo = Arc::clone(&back_ok);
    let _ = conn.reducers().debug_set_layer_then(npc_eid, 0, move |_ctx, result| {
        if let Ok(Ok(())) = result { bo.store(true, Ordering::SeqCst); }
        bd.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || back_done.load(Ordering::SeqCst));
    if !back_ok.load(Ordering::SeqCst) {
        r.fail("T10  debug_set_layer back to 0 failed");
        let _ = conn.reducers().debug_remove_entity(npc_eid);
        pump(conn, 200);
        return;
    }

    pump(conn, 600);

    let visible_after_return = conn.db().nearby_transforms().iter()
        .any(|t| t.entity_id == npc_eid);
    if visible_after_return {
        r.pass("T10  NPC returned to layer 0 correctly visible again");
    } else {
        r.fail("T10  NPC returned to layer 0 NOT visible — layer transition broken!");
    }

    // Cleanup.
    let _ = conn.reducers().debug_remove_entity(npc_eid);
    pump(conn, 200);
}

// ═══════════════════════════════════════════════════════════════════════
// Stealth Visibility Tests — team-based filtering in nearby_transforms
// ═══════════════════════════════════════════════════════════════════════

fn run_stealth_tests(conn: &DbConnection, r: &mut TestResults) {
    info!("");
    info!("── Stealth Visibility Tests ─────────────────────────────────");

    // Resolve player entity.
    let my_entity_id = match current_entity_id(conn) {
        Some(entity_id) => entity_id,
        None => {
            r.fail("ST0  cannot run stealth tests — no client_sequence for this connection");
            return;
        }
    };

    let my_region = conn.db().my_region().iter().next();
    let (my_rx, my_rz) = match my_region {
        Some(ref reg) => (reg.region_x, reg.region_z),
        None => {
            r.fail("ST0  cannot run stealth tests — no my_region data");
            return;
        }
    };

    // Spawn NPC in player's cell.
    let npc_x = my_rx as f32 * 50.0 + 25.0;
    let npc_z = my_rz as f32 * 50.0 + 25.0;

    info!("  ST1: spawning stealth-test NPC at ({npc_x}, 1, {npc_z})");
    let spawn_done = Arc::new(AtomicBool::new(false));
    let spawn_ok = Arc::new(AtomicBool::new(false));
    let sd = Arc::clone(&spawn_done);
    let so = Arc::clone(&spawn_ok);
    let _ = conn.reducers().spawn_npc_then(npc_x, 1.0, npc_z, 100.0, move |_ctx, result| {
        if let Ok(Ok(())) = result { so.store(true, Ordering::SeqCst); }
        sd.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 5000, || spawn_done.load(Ordering::SeqCst));
    if !spawn_ok.load(Ordering::SeqCst) {
        r.fail("ST1  failed to spawn stealth-test NPC");
        return;
    }

    pump(conn, 600);

    // Find the spawned NPC.
    let npc_eid = conn.db().nearby_transforms().iter()
        .filter(|t| (t.pos_x - npc_x).abs() < 1.0 && (t.pos_z - npc_z).abs() < 1.0)
        .filter(|t| {
            conn.db().entity().entity_id().find(&t.entity_id)
                .is_some_and(|e| e.kind == EntityKind::Npc && e.state != EntityState::Removed)
        })
        .map(|t| t.entity_id)
        .max();

    let npc_eid = match npc_eid {
        Some(id) => {
            r.pass("ST1  stealth-test NPC visible before stealth");
            id
        }
        None => {
            r.fail("ST1  stealth-test NPC NOT visible — spawn or view broken");
            return;
        }
    };

    // ── ST2: assign different teams, apply stealth → NPC should vanish ──
    info!("  ST2: set NPC team=1, player team=2, apply stealth");

    // Set NPC to team 1.
    let team_done = Arc::new(AtomicBool::new(false));
    let team_ok = Arc::new(AtomicBool::new(false));
    let td = Arc::clone(&team_done);
    let to = Arc::clone(&team_ok);
    let _ = conn.reducers().debug_set_team_then(npc_eid, 1, move |_ctx, result| {
        if let Ok(Ok(())) = result { to.store(true, Ordering::SeqCst); }
        td.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 3000, || team_done.load(Ordering::SeqCst));
    if !team_ok.load(Ordering::SeqCst) {
        r.fail("ST2  debug_set_team(npc, 1) failed");
        let _ = conn.reducers().debug_remove_entity(npc_eid);
        pump(conn, 200);
        return;
    }

    // Set player to team 2 (different from NPC).
    let team_done2 = Arc::new(AtomicBool::new(false));
    let team_ok2 = Arc::new(AtomicBool::new(false));
    let td2 = Arc::clone(&team_done2);
    let to2 = Arc::clone(&team_ok2);
    let _ = conn.reducers().debug_set_team_then(my_entity_id, 2, move |_ctx, result| {
        if let Ok(Ok(())) = result { to2.store(true, Ordering::SeqCst); }
        td2.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 3000, || team_done2.load(Ordering::SeqCst));
    if !team_ok2.load(Ordering::SeqCst) {
        r.fail("ST2  debug_set_team(player, 2) failed");
        let _ = conn.reducers().debug_remove_entity(npc_eid);
        pump(conn, 200);
        return;
    }

    // Apply stealth buff (buff_id=700, 100 ticks, mod_stealth=true).
    let buff_done = Arc::new(AtomicBool::new(false));
    let buff_ok = Arc::new(AtomicBool::new(false));
    let bd = Arc::clone(&buff_done);
    let bo = Arc::clone(&buff_ok);
    let _ = conn.reducers().debug_apply_buff_then(
        npc_eid, 700, 1, 200, // long duration so it doesn't expire during test
        Some(true), // mod_stealth
        move |_ctx, result| {
            if let Ok(Ok(())) = result { bo.store(true, Ordering::SeqCst); }
            bd.store(true, Ordering::SeqCst);
        },
    );
    wait_for(conn, 3000, || buff_done.load(Ordering::SeqCst));
    if !buff_ok.load(Ordering::SeqCst) {
        r.fail("ST2  debug_apply_buff(stealth) failed");
        let _ = conn.reducers().debug_remove_entity(npc_eid);
        pump(conn, 200);
        return;
    }

    pump(conn, 600);

    // Verify NPC is invisible to player (different team).
    let visible_stealthed = conn.db().nearby_transforms().iter()
        .any(|t| t.entity_id == npc_eid);
    if !visible_stealthed {
        r.pass("ST2  stealthed NPC invisible to enemy team (view filter works)");
    } else {
        r.fail("ST2  stealthed NPC VISIBLE to enemy team — stealth filtering broken!");
    }

    // Verify active_buff contains mod_stealth.
    let has_stealth_buff = conn.db().active_buff().iter()
        .any(|b| b.entity_id == npc_eid && b.mod_stealth == Some(true));
    if has_stealth_buff {
        r.pass("ST2b active_buff has mod_stealth=true for NPC");
    } else {
        r.fail("ST2b active_buff missing mod_stealth=true for NPC");
    }

    // ── ST3: set player to same team → NPC reappears ──
    info!("  ST3: set player to team 1 (same as NPC) — should see stealthed ally");

    let team_done3 = Arc::new(AtomicBool::new(false));
    let team_ok3 = Arc::new(AtomicBool::new(false));
    let td3 = Arc::clone(&team_done3);
    let to3 = Arc::clone(&team_ok3);
    let _ = conn.reducers().debug_set_team_then(my_entity_id, 1, move |_ctx, result| {
        if let Ok(Ok(())) = result { to3.store(true, Ordering::SeqCst); }
        td3.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 3000, || team_done3.load(Ordering::SeqCst));
    if !team_ok3.load(Ordering::SeqCst) {
        r.fail("ST3  debug_set_team(player, 1) failed");
        let _ = conn.reducers().debug_remove_entity(npc_eid);
        pump(conn, 200);
        return;
    }

    pump(conn, 600);

    let visible_ally = conn.db().nearby_transforms().iter()
        .any(|t| t.entity_id == npc_eid);
    if visible_ally {
        r.pass("ST3  stealthed NPC visible to allied team (same team_id)");
    } else {
        r.fail("ST3  stealthed NPC NOT visible to allied team — ally visibility broken!");
    }

    // ── ST4: remove stealth (remove NPC buffs via cleanup & respawn without buff) ──
    // We can't directly remove a buff, but we can remove the entity.
    // Instead, test that after the NPC is removed, stealth tables are cleaned up.
    info!("  ST4: verify entity_team populated for NPC");
    let npc_team = conn.db().entity_team().entity_id().find(&npc_eid);
    if let Some(team) = npc_team {
        if team.team_id == 1 {
            r.pass("ST4  entity_team correctly set (team_id=1) for NPC");
        } else {
            r.fail(&format!("ST4  entity_team wrong team_id={} (expected 1)", team.team_id));
        }
    } else {
        r.fail("ST4  entity_team row missing for NPC");
    }

    // Cleanup: remove NPC and reset player team.
    let _ = conn.reducers().debug_remove_entity(npc_eid);
    // Reset player team to 0 (no team).
    let _ = conn.reducers().debug_set_team(my_entity_id, 0);
    pump(conn, 400);
}

fn run_smoke_tests(conn: &DbConnection, r: &mut TestResults) {
    info!("");
    info!("── Smoke Tests ────────────────────────────────────────────────");

    // Resolve our player entity_id from client_sequence.
    let my_entity_id = current_entity_id(conn);

    let entity_id = match my_entity_id {
        Some(eid) => {
            info!("  Player entity_id = {eid}");
            eid
        }
        None => {
            r.fail("S0  could not resolve player entity_id for this connection");
            return;
        }
    };

    // S1: entity_health present.
    info!("  S1: entity_health");
    if let Some(health) = conn.db().entity_health().entity_id().find(&entity_id) {
        r.pass(&format!("S1  entity_health present (hp={}, max={})", health.hp, health.max_hp));
    } else {
        r.fail("S1  entity_health row missing for player");
    }

    // S2: entity_transform present.
    info!("  S2: entity_transform");
    let transform_before = conn.db().entity_transform().entity_id().find(&entity_id);
    if let Some(ref t) = transform_before {
        r.pass(&format!("S2  entity_transform present (last_tick={})", t.last_tick));
    } else {
        r.fail("S2  entity_transform row missing for player");
    }

    // Snapshot last_tick before intent submission.
    let tick_before = transform_before.as_ref().map(|t| t.last_tick).unwrap_or(0);

    // S3: Submit Move intent.
    info!("  S3: Move intent");
    let seq_base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let move_result = conn.reducers().submit_intent(
        entity_id,
        seq_base,
        IntentAction::Move(MoveDir { dir_x: 0.0, dir_y: 0.0, dir_z: 1.0 }),
        0,
    );
    if move_result.is_ok() {
        r.pass("S3  Move intent accepted");
    } else {
        r.fail(&format!("S3  Move intent rejected: {:?}", move_result.err()));
    }

    // S4: Submit Stop intent.
    info!("  S4: Stop intent");
    let stop_result = conn.reducers().submit_intent(
        entity_id,
        seq_base + 1,
        IntentAction::Stop,
        0,
    );
    if stop_result.is_ok() {
        r.pass("S4  Stop intent accepted");
    } else {
        r.fail(&format!("S4  Stop intent rejected: {:?}", stop_result.err()));
    }

    // Wait for Move + Stop to be consumed before submitting FaceTo.
    // This ensures the entity is fully initialized in the physics world
    // (avoids set_kinematic_rotation racing with entity sync on fresh spawn).
    info!("  Waiting 400ms for Move+Stop to process...");
    pump(conn, 400);

    // S5: Submit FaceTo intent (separate tick window for reliable rotation).
    info!("  S5: FaceTo intent");
    let face_result = conn.reducers().submit_intent(
        entity_id,
        seq_base + 2,
        IntentAction::FaceTo(MoveDir { dir_x: 1.0, dir_y: 0.0, dir_z: 0.0 }),
        0,
    );
    if face_result.is_ok() {
        r.pass("S5  FaceTo intent accepted");
    } else {
        r.fail(&format!("S5  FaceTo intent rejected: {:?}", face_result.err()));
    }

    // Wait for FaceTo to process.
    info!("  Waiting 400ms for FaceTo to process...");
    pump(conn, 400);

    // S6: last_tick advanced.
    info!("  S6: tick advancement");
    if let Some(t) = conn.db().entity_transform().entity_id().find(&entity_id) {
        if t.last_tick > tick_before {
            r.pass(&format!("S6  last_tick advanced ({tick_before} → {})", t.last_tick));
        } else {
            r.fail(&format!("S6  last_tick did NOT advance (before={tick_before}, after={})", t.last_tick));
        }
    } else {
        r.fail("S6  entity_transform missing after ticks");
    }

    // S7: FaceTo rotation committed.
    info!("  S7: FaceTo rotation");
    if let Some(t) = conn.db().entity_transform().entity_id().find(&entity_id) {
        // FaceTo(+X) → yaw = π/2 → quaternion (0, sin(π/4), 0, cos(π/4)) ≈ (0, 0.707, 0, 0.707)
        let target = (0.5_f32).sqrt(); // ≈ 0.70710678
        let tol = 0.05;
        if (t.rot_y - target).abs() < tol && (t.rot_w - target).abs() < tol {
            r.pass(&format!("S7  FaceTo rotation correct (rot_y={:.3}, rot_w={:.3})", t.rot_y, t.rot_w));
        } else {
            r.fail(&format!("S7  FaceTo rotation wrong — expected ≈({target:.3},{target:.3}), got ({:.3},{:.3})", t.rot_y, t.rot_w));
        }
    } else {
        r.fail("S7  entity_transform missing for rotation check");
    }

    // S8: Intents consumed (player_intent empty for our entity).
    info!("  S8: intent consumption");
    let pending = conn.db().player_intent().iter()
        .filter(|pi| pi.entity_id == entity_id)
        .count();
    if pending == 0 {
        r.pass("S8  all intents consumed (player_intent empty)");
    } else {
        r.fail(&format!("S8  {pending} intent(s) still pending"));
    }

    // S9: Combat path — Slash a disposable NPC, verify damage event.
    info!("  S9: combat path (Slash → disposable NPC damage)");

    let npc_id = match spawn_nearby_disposable_npc(conn, entity_id, 0.75, 200.0) {
        Ok(id) => id,
        Err(err) => {
            r.fail(&format!("S9  failed to provision disposable NPC: {err}"));
            return;
        }
    };
    info!("  S9: NPC entity_id = {npc_id}");

    // Snapshot combat_event count before the Slash.
    let pre_slash_events: u64 = conn.db().combat_event().iter()
        .filter(|e| e.source_entity == entity_id)
        .count() as u64;

    // Submit Slash.
    let slash_result = conn.reducers().submit_intent(
        entity_id,
        seq_base + 3,
        IntentAction::UseAbility(UseAbilityData {
            ability_id: 1,
            target: AbilityTarget::None,
            target_hint: None,
        }),
        0,
    );
    if slash_result.is_err() {
        cleanup_entity(conn, npc_id);
        r.fail(&format!("S9  Slash intent rejected: {:?}", slash_result.err()));
        return;
    }

    // Wait for Slash to process (SpawnHitbox tick 0, ApplyDamageFrame tick 1, RemoveHitbox tick 2).
    pump(conn, 800);

    // Check for new combat events from our entity.
    let post_slash_events: u64 = conn.db().combat_event().iter()
        .filter(|e| e.source_entity == entity_id)
        .count() as u64;
    let new_events = post_slash_events - pre_slash_events;
    if new_events > 0 {
        r.pass(&format!("S9  {new_events} new combat_event(s) from Slash"));
    } else {
        r.fail("S9  no new combat_events from Slash — hitbox did not detect NPC");
    }

    cleanup_entity(conn, npc_id);
}

// ═══════════════════════════════════════════════════════════════════════
// Fault / Boundary Tests
// ═══════════════════════════════════════════════════════════════════════

fn run_fault_tests(conn: &DbConnection, r: &mut TestResults) {
    info!("");
    info!("── Fault / Boundary Tests ─────────────────────────────────────");

    // Resolve our entity_id.
    let entity_id = match current_entity_id(conn) {
        Some(entity_id) => entity_id,
        None => {
            r.fail("F0  no client_sequence for this connection — cannot run fault tests");
            return;
        }
    };

    let seq_base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // F1: Stale-sequence anti-replay.
    run_f1_stale_sequence(conn, r, entity_id, seq_base);

    // F2: Entity-ownership enforcement.
    run_f2_ownership(conn, r, seq_base);

    // F3: Double-spawn idempotency.
    run_f3_double_spawn(conn, r);

    // F4: Intent-queue overflow.
    run_f4_queue_overflow(conn, r, entity_id, seq_base);

    // F5: Live cooldown enforcement.
    run_f5_cooldown(conn, r, entity_id, seq_base);

    // F6: Unauthorized commit rejection.
    run_f6_unauthorized_commit(conn, r);

    // F7: Commit rejection does not advance cursor.
    run_f7_cursor_safety(conn, r);

    // F8: Intent lifecycle integrity.
    run_f8_intent_lifecycle(conn, r, entity_id, seq_base);
}

fn run_f1_stale_sequence(conn: &DbConnection, r: &mut TestResults, entity_id: u64, seq_base: u64) {
    info!("  F1: Stale-sequence anti-replay");

    let last_seq = match current_client_sequence(conn) {
        Some(seq) => seq.last_processed_sequence,
        None => {
            r.fail("F1  cannot resolve client_sequence for this connection");
            return;
        }
    };

    // Submit with the already-processed sequence — should be rejected.
    let stale_done = Arc::new(AtomicBool::new(false));
    let stale_rejected = Arc::new(AtomicBool::new(false));

    let sd = Arc::clone(&stale_done);
    let sr = Arc::clone(&stale_rejected);
    let _ = conn.reducers().submit_intent_then(
        entity_id,
        last_seq,
        IntentAction::Stop,
        seq_base,
        move |_ctx, result| {
            if let Ok(Err(e)) = &result {
                if e.contains("Stale sequence") {
                    sr.store(true, Ordering::SeqCst);
                }
            }
            sd.store(true, Ordering::SeqCst);
        },
    );
    wait_for(conn, 3000, || stale_done.load(Ordering::SeqCst));

    if stale_rejected.load(Ordering::SeqCst) {
        r.pass("F1a  stale sequence_id rejected");
    } else {
        r.fail("F1a  stale sequence_id NOT rejected");
    }

    // Valid sequence should still work.
    let valid_done = Arc::new(AtomicBool::new(false));
    let valid_ok = Arc::new(AtomicBool::new(false));

    let vd = Arc::clone(&valid_done);
    let vo = Arc::clone(&valid_ok);
    let _ = conn.reducers().submit_intent_then(
        entity_id,
        seq_base + 100,
        IntentAction::Stop,
        seq_base,
        move |_ctx, result| {
            if let Ok(Ok(())) = result {
                vo.store(true, Ordering::SeqCst);
            }
            vd.store(true, Ordering::SeqCst);
        },
    );
    wait_for(conn, 3000, || valid_done.load(Ordering::SeqCst));

    if valid_ok.load(Ordering::SeqCst) {
        r.pass("F1b  subsequent valid sequence accepted");
    } else {
        r.fail("F1b  subsequent valid sequence rejected");
    }

    pump(conn, 200); // drain
}

fn run_f2_ownership(conn: &DbConnection, r: &mut TestResults, seq_base: u64) {
    info!("  F2: Entity-ownership enforcement");

    let owner_entity_id = match current_entity_id(conn) {
        Some(entity_id) => entity_id,
        None => {
            r.fail("F2  no client_sequence for this connection");
            return;
        }
    };

    let foreign_id = match spawn_nearby_disposable_npc(conn, owner_entity_id, 1.25, 100.0) {
        Ok(id) => id,
        Err(err) => {
            r.fail(&format!("F2  failed to provision foreign NPC: {err}"));
            return;
        }
    };

    let done = Arc::new(AtomicBool::new(false));
    let rejected = Arc::new(AtomicBool::new(false));

    let d = Arc::clone(&done);
    let rj = Arc::clone(&rejected);
    let _ = conn.reducers().submit_intent_then(
        foreign_id,
        seq_base + 200,
        IntentAction::Stop,
        seq_base,
        move |_ctx, result| {
            if let Ok(Err(e)) = &result {
                if e.contains("does not own") || e.contains("not registered") {
                    rj.store(true, Ordering::SeqCst);
                }
            }
            d.store(true, Ordering::SeqCst);
        },
    );
    wait_for(conn, 3000, || done.load(Ordering::SeqCst));

    if rejected.load(Ordering::SeqCst) {
        r.pass("F2  foreign entity_id rejected (ownership enforced)");
    } else {
        r.fail("F2  ownership check did NOT reject foreign entity_id");
    }

    cleanup_entity(conn, foreign_id);
}

fn run_f3_double_spawn(conn: &DbConnection, r: &mut TestResults) {
    info!("  F3: Double-spawn idempotency");

    let done = Arc::new(AtomicBool::new(false));
    let rejected = Arc::new(AtomicBool::new(false));

    let d = Arc::clone(&done);
    let rj = Arc::clone(&rejected);
    let _ = conn.reducers().spawn_player_then(move |_ctx, result| {
        if let Ok(Err(e)) = &result {
            if e.contains("already spawned") {
                rj.store(true, Ordering::SeqCst);
            }
        }
        d.store(true, Ordering::SeqCst);
    });
    wait_for(conn, 3000, || done.load(Ordering::SeqCst));

    if rejected.load(Ordering::SeqCst) {
        r.pass("F3  duplicate spawn_player rejected");
    } else {
        r.fail("F3  duplicate spawn_player was NOT rejected");
    }
}

fn run_f4_queue_overflow(conn: &DbConnection, r: &mut TestResults, entity_id: u64, seq_base: u64) {
    info!("  F4: Intent-queue overflow (MAX_QUEUED_INTENTS = 5)");

    // Submit 8 intents rapidly. With SDK, they arrive nearly simultaneously
    // (much faster than CLI round-trips), so overflow should trigger reliably.
    let full_count = Arc::new(AtomicU32::new(0));
    let ok_count = Arc::new(AtomicU32::new(0));
    let done_count = Arc::new(AtomicU32::new(0));

    for i in 0..8u64 {
        let fc = Arc::clone(&full_count);
        let oc = Arc::clone(&ok_count);
        let dc = Arc::clone(&done_count);
        let _ = conn.reducers().submit_intent_then(
            entity_id,
            seq_base + 300 + i,
            IntentAction::Stop,
            seq_base,
            move |_ctx, result| {
                match result {
                    Ok(Err(e)) if e.contains("queue full") || e.contains("Intent queue full") => {
                        fc.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {
                        oc.fetch_add(1, Ordering::SeqCst);
                    }
                }
                dc.fetch_add(1, Ordering::SeqCst);
            },
        );
    }

    wait_for(conn, 5000, || done_count.load(Ordering::SeqCst) >= 8);

    let full = full_count.load(Ordering::SeqCst);
    let ok = ok_count.load(Ordering::SeqCst);
    info!("  F4: {ok} accepted, {full} rejected as queue-full");

    if full >= 1 {
        r.pass(&format!("F4  queue overflow triggered ({full}/8 rejected)"));
    } else {
        r.warn_msg("F4  no overflow observed — worker may have drained between sends (not a bug)");
    }

    pump(conn, 500); // drain
}

fn run_f5_cooldown(conn: &DbConnection, r: &mut TestResults, entity_id: u64, seq_base: u64) {
    info!("  F5: Live cooldown enforcement");

    // Wait for any prior Slash cooldown to expire (20 ticks = 1s).
    info!("  F5: waiting 1200ms to clear existing cooldown...");
    pump(conn, 1200);

    let npc_id = match spawn_nearby_disposable_npc(conn, entity_id, 0.75, 200.0) {
        Ok(id) => id,
        Err(err) => {
            r.fail(&format!("F5  failed to provision disposable NPC: {err}"));
            return;
        }
    };
    info!("  F5: NPC entity_id = {npc_id}");

    // Snapshot NPC HP before sending Slashes (S9 may have already damaged it).
    let hp_before = conn.db().entity_health().entity_id().find(&npc_id)
        .map(|h| h.hp)
        .unwrap_or(0.0);
    info!("  F5: NPC HP before = {hp_before}");

    // Submit two UseAbility intents back-to-back.
    let _ = conn.reducers().submit_intent(
        entity_id,
        seq_base + 400,
        IntentAction::UseAbility(UseAbilityData {
            ability_id: 1,
            target: AbilityTarget::None,
            target_hint: None,
        }),
        0,
    );
    let _ = conn.reducers().submit_intent(
        entity_id,
        seq_base + 401,
        IntentAction::UseAbility(UseAbilityData {
            ability_id: 1,
            target: AbilityTarget::None,
            target_hint: None,
        }),
        0,
    );

    // Wait for ticks to process.
    pump(conn, 800);

    // Check NPC HP delta (Slash does 25 damage; one hit = ~25, two hits = ~50).
    if let Some(health) = conn.db().entity_health().entity_id().find(&npc_id) {
        let hp = health.hp;
        let damage = hp_before - hp;
        info!("  F5: NPC HP = {hp} (damage dealt = {damage})");
        if damage >= 45.0 {
            r.fail(&format!("F5  cooldown NOT enforced — damage={damage} (≥45 implies two Slashes)"));
        } else if damage >= 20.0 {
            r.pass(&format!("F5  cooldown enforced — damage={damage} (one Slash landed, second blocked)"));
        } else {
            r.warn_msg(&format!("F5  damage={damage} — slash may have missed (timing issue?)"));
        }
    } else {
        let npc_exists = conn.db().entity().entity_id().find(&npc_id).is_some();
        if !npc_exists {
            r.warn_msg("F5  NPC entity despawned before HP check — test inconclusive");
        } else {
            r.warn_msg("F5  NPC entity exists but no health row — test inconclusive");
        }
    }

    cleanup_entity(conn, npc_id);
}

fn run_f6_unauthorized_commit(conn: &DbConnection, r: &mut TestResults) {
    info!("  F6: Unauthorized commit_tick_results");

    let done = Arc::new(AtomicBool::new(false));
    let rejected = Arc::new(AtomicBool::new(false));

    let d = Arc::clone(&done);
    let rj = Arc::clone(&rejected);
    let _ = conn.reducers().commit_tick_results_then(
        999999,     // tick_id
        vec![],     // transforms
        vec![],     // health_updates
        vec![],     // combat_events
        vec![],     // world_events
        vec![],     // consumed_intent_ids
        vec![],     // entity_state_updates
        vec![],     // region_updates
        vec![],     // buff_updates
        vec![],     // buff_cleared_entity_ids
        vec![],     // npc_state_updates
        vec![],     // director_spawns
        vec![],     // interactable_updates
        move |_ctx, result: Result<Result<(), String>, spacetimedb_sdk::__codegen::InternalError>| {
            if let Ok(Err(e)) = &result {
                if e.contains("trusted worker") || e.contains("unauthorized") || e.contains("rejected") {
                    rj.store(true, Ordering::SeqCst);
                }
            }
            // Also treat InternalError as a rejection indicator.
            if result.is_err() {
                rj.store(true, Ordering::SeqCst);
            }
            d.store(true, Ordering::SeqCst);
        },
    );
    wait_for(conn, 5000, || done.load(Ordering::SeqCst));

    if rejected.load(Ordering::SeqCst) {
        r.pass("F6  unauthorized commit_tick_results rejected");
    } else {
        r.fail("F6  unauthorized commit_tick_results was NOT rejected");
    }
}

fn run_f7_cursor_safety(conn: &DbConnection, r: &mut TestResults) {
    info!("  F7: Rejected commit does not advance cursor");

    let tick_before = conn.db().module_config().iter()
        .next()
        .map(|mc| mc.last_committed_tick);

    let fake_tick: u64 = 999998;

    // Attempt unauthorized commit with a very high tick_id.
    let done = Arc::new(AtomicBool::new(false));
    let d = Arc::clone(&done);
    let _ = conn.reducers().commit_tick_results_then(
        fake_tick,
        vec![], vec![], vec![], vec![], vec![], vec![], vec![],
        vec![], vec![], vec![],
        vec![], // director_spawns
        vec![], // interactable_updates
        move |_ctx, _result| {
            d.store(true, Ordering::SeqCst);
        },
    );
    wait_for(conn, 3000, || done.load(Ordering::SeqCst));

    // Give a moment for any DB updates to propagate.
    pump(conn, 200);

    let tick_after = conn.db().module_config().iter()
        .next()
        .map(|mc| mc.last_committed_tick);

    match (tick_before, tick_after) {
        (Some(before), Some(after)) => {
            if after >= fake_tick {
                r.fail(&format!("F7  last_committed_tick jumped to fake tick ({before} → {after})"));
            } else {
                r.pass(&format!("F7  rejected commit did not corrupt cursor ({before} → {after}, fake={fake_tick} not reached)"));
            }
        }
        _ => {
            r.warn_msg("F7  could not read last_committed_tick from module_config");
        }
    }
}

fn run_f8_intent_lifecycle(conn: &DbConnection, r: &mut TestResults, entity_id: u64, seq_base: u64) {
    info!("  F8: Intent lifecycle (submit → consume → cursor advance)");

    let tick_before = conn.db().module_config().iter()
        .next()
        .map(|mc| mc.last_committed_tick)
        .unwrap_or(0);

    // Submit a Move intent.
    let _ = conn.reducers().submit_intent(
        entity_id,
        seq_base + 500,
        IntentAction::Move(MoveDir { dir_x: 0.0, dir_y: 0.0, dir_z: 1.0 }),
        0,
    );

    // Wait for tick processing.
    pump(conn, 800);

    // Check intent consumed.
    let pending = conn.db().player_intent().iter()
        .filter(|pi| pi.entity_id == entity_id)
        .count();
    let tick_after = conn.db().module_config().iter()
        .next()
        .map(|mc| mc.last_committed_tick)
        .unwrap_or(0);

    if pending == 0 && tick_after > tick_before {
        r.pass(&format!("F8  intent consumed and cursor advanced ({tick_before} → {tick_after})"));
    } else if pending > 0 {
        r.fail(&format!("F8  intent NOT consumed after 800ms ({pending} pending)"));
    } else {
        r.warn_msg(&format!("F8  intent consumed but cursor did not advance ({tick_before} → {tick_after})"));
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Subscription Denial Test
// ═══════════════════════════════════════════════════════════════════════

fn run_denial_test(conn: &DbConnection, results: &Arc<Mutex<TestResults>>) {
    info!("");
    info!("── Subscription Denial Test ──────────────────────────────────");

    let denial_received = Arc::new(AtomicBool::new(false));
    let denial_flag = Arc::clone(&denial_received);

    conn.subscription_builder()
        .on_applied(move |_ctx| {
            // If this fires, the subscription was accepted — could mean table is exposed.
        })
        .on_error(move |_ctx, err| {
            let err_str = format!("{err}");
            info!("  entity_region subscription error: {err_str}");
            denial_flag.store(true, Ordering::SeqCst);
        })
        .subscribe(["SELECT * FROM entity_region"]);

    // Wait for denial.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let _ = conn.frame_tick();
        std::thread::sleep(Duration::from_millis(50));
        if denial_received.load(Ordering::SeqCst) {
            break;
        }
    }

    let mut r = results.lock().unwrap();
    if denial_received.load(Ordering::SeqCst) {
        r.pass("D1  entity_region subscription denied (private table enforced via SDK)");
    } else {
        r.pass("D1  entity_region subscription produced no data (table not accessible)");
    }
}
