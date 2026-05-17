use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use regex::Regex;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MODULE_NAME: &str = "tickforge";
const SERVER_ALIAS: &str = "local";
const SERVER_HOST: &str = "127.0.0.1:3000";
const MODULE_PATH: &str = "crates/server_module";
const WORKER_BINDINGS_OUT: &str = "crates/simulation_worker/src/module_bindings";
const CLIENT_BINDINGS_OUT: &str = "crates/game_client/src/module_bindings";

#[derive(Parser)]
#[command(name = "cargo xtask")]
struct Cli {
    #[command(subcommand)]
    command: TopLevel,
}

#[derive(Subcommand)]
enum TopLevel {
    Dev {
        #[command(subcommand)]
        command: DevCmd,
    },
    Build {
        #[command(subcommand)]
        command: BuildCmd,
    },
    Test {
        #[command(subcommand)]
        command: TestCmd,
    },
}

#[derive(Subcommand)]
enum DevCmd {
    Server,
    /// Publish schema and regenerate bindings.
    /// The WASM module is always built with Cargo's release profile.
    Schema,
    Reset(ResetArgs),
    WorkerRegister(WorkerRegisterArgs),
    Worker(RunWorkerArgs),
    /// Capture a deterministic replay fixture to a JSON file.
    CaptureFixture(CaptureFixtureArgs),
    ClientTest(ClientTestArgs),
    Client(RunClientArgs),
    Clients(RunClientsArgs),
    /// Seed a synthetic terrain set + chunk via admin reducers (§4.8b smoke).
    /// Pushes one `terrain_set`, one flat `terrain_chunk` (a 20×20 m quad
    /// at y=0 made of 4 verts + 2 triangles), and a matching
    /// `terrain_manifest` row, then exits. Bind a `WorldLayerDef` /
    /// `DungeonTemplate` to the same `terrain_set` name and restart the
    /// worker to verify the deferred queue end-to-end. Requires the
    /// `debug` server feature (default on).
    SeedTerrain(SeedTerrainArgs),
    /// Import a real terrain mesh (glTF / .glb) into SpacetimeDB by
    /// chunking it on an XZ grid and pushing one `terrain_chunk` row
    /// per non-empty cell via the admin upsert reducers. Triangulates,
    /// applies node transforms, welds duplicate vertices per chunk,
    /// then calls `terrain_set_upsert` + `terrain_chunk_upsert` (per
    /// cell) + `terrain_manifest_upsert` (per cell). Authoring guide:
    /// export from Blender as triangulated glTF 2.0; CCW winding for
    /// upward-facing floors. Requires the `debug` server feature.
    ImportTerrain(ImportTerrainArgs),
}

#[derive(Subcommand)]
enum BuildCmd {
    Worker(BuildProfileArgs),
    Client(BuildProfileArgs),
    Cli(BuildProfileArgs),
    Wasm,
    All(BuildProfileArgs),
}

#[derive(Subcommand)]
enum TestCmd {
    Fast,
    Worker,
    Cli(ClientTestArgs),
    /// Run multi-client integration tests (requires running server + worker)
    MultiClient(ClientTestArgs),
    Workspace,
    /// Run the deterministic replay test suite (uses fixtures in crates/simulation_worker/tests/fixtures)
    Replay(BuildProfileArgs),
}

#[derive(Args)]
struct BuildProfileArgs {
    #[arg(long)]
    release: bool,
}

#[derive(Args)]
struct ResetArgs {
    #[arg(long)]
    db: bool,
    #[arg(long)]
    tokens: bool,
    #[arg(long)]
    all: bool,
}

#[derive(Args)]
struct WorkerRegisterArgs {
    #[arg(long)]
    seed_npc: bool,

    #[arg(long)]
    release: bool,
}

#[derive(Args)]
struct RunWorkerArgs {
    #[arg(long)]
    release: bool,
    #[arg(long, default_value = "info")]
    log: String,
}

#[derive(Args)]
struct CaptureFixtureArgs {
    #[arg(
        long,
        default_value = "crates/simulation_worker/tests/fixtures/combat_lifecycle_v1.generated.json"
    )]
    out: String,

    #[arg(long)]
    release: bool,
}

#[derive(Args)]
struct ClientTestArgs {
    #[arg(long)]
    release: bool,

    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct RunClientArgs {
    #[arg(long)]
    release: bool,

    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct RunClientsArgs {
    #[arg(long)]
    release: bool,

    #[arg(long, default_value = ".client_token_a")]
    token_a: String,

    #[arg(long, default_value = ".client_token_b")]
    token_b: String,

    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Args)]
struct SeedTerrainArgs {
    /// Name of the `terrain_set` row to upsert. Bind a layer to this
    /// name (in `data/layers.ron` or a `DungeonTemplate`) to see the
    /// chunk applied.
    #[arg(long, default_value = "smoke_floor")]
    set_name: String,

    /// `terrain_set_id` to use for the chunk + manifest rows. Must
    /// match the auto-incremented id assigned by the first upsert call;
    /// for a fresh DB this is typically `1`.
    #[arg(long, default_value_t = 1)]
    set_id: u32,

    /// Half-extent of the synthetic flat quad on the X/Z axes (meters).
    #[arg(long, default_value_t = 10.0)]
    half_extent: f32,

    /// Y elevation of the synthetic floor (meters).
    #[arg(long, default_value_t = 0.0)]
    elevation: f32,
}

#[derive(Args)]
struct ImportTerrainArgs {
    /// Path to a glTF 2.0 file (`.gltf` or `.glb`). All meshes in all
    /// scenes are flattened with their node transforms applied.
    #[arg(long)]
    gltf: PathBuf,

    /// Name of the `terrain_set` row to upsert.
    #[arg(long)]
    set_name: String,

    /// `terrain_set_id` fallback used only when `--skip-set` is passed and
    /// the DB cannot be queried. Normally the importer resolves the id
    /// automatically after calling `terrain_set_upsert`.
    #[arg(long, default_value_t = 0)]
    set_id: u32,

    /// XZ chunk size in meters (Y is single-chunk on the cy=0 plane).
    /// Triangles are bucketed by the chunk that owns their centroid.
    #[arg(long, default_value_t = 32.0)]
    chunk_size: f32,

    /// LOD value to write on each chunk row (0 = collision LOD).
    #[arg(long, default_value_t = 0u8)]
    lod: u8,

    /// World-space translation applied to all imported vertices before
    /// chunking (meters). Useful when the source mesh is centered.
    #[arg(long, default_value_t = 0.0)]
    offset_x: f32,
    #[arg(long, default_value_t = 0.0)]
    offset_y: f32,
    #[arg(long, default_value_t = 0.0)]
    offset_z: f32,

    /// Uniform scale applied to imported vertices (e.g. cm → m = 0.01).
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// Flip triangle winding (use if your floors come out facing down).
    #[arg(long, default_value_t = false)]
    flip_winding: bool,

    /// Skip the `terrain_set_upsert` call (set already exists).
    #[arg(long, default_value_t = false)]
    skip_set: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        TopLevel::Dev { command } => run_dev(command),
        TopLevel::Build { command } => run_build(command),
        TopLevel::Test { command } => run_test(command),
    }
}

fn run_dev(cmd: DevCmd) -> Result<()> {
    match cmd {
        DevCmd::Server => dev_server(),
        DevCmd::Schema => dev_schema(),
        DevCmd::Reset(args) => dev_reset(args),
        DevCmd::WorkerRegister(args) => dev_worker_register(args),
        DevCmd::Worker(args) => dev_worker(args),
        DevCmd::CaptureFixture(args) => dev_capture_fixture(args),
        DevCmd::ClientTest(args) => dev_client_test(args),
        DevCmd::Client(args) => dev_client(args),
        DevCmd::Clients(args) => dev_clients(args),
        DevCmd::SeedTerrain(args) => dev_seed_terrain(args),
        DevCmd::ImportTerrain(args) => dev_import_terrain(args),
    }
}

fn run_build(cmd: BuildCmd) -> Result<()> {
    match cmd {
        BuildCmd::Worker(args) => run_command(cargo_build_command(
            ["-p", "simulation_worker", "--features", "connected"],
            args.release,
        )),
        BuildCmd::Client(args) => run_command(cargo_build_command(
            ["-p", "game_client_bevy", "--features", "connected"],
            args.release,
        )),
        BuildCmd::Cli(args) => run_command(cargo_build_command(
            ["-p", "game_client", "--features", "connected"],
            args.release,
        )),
        BuildCmd::Wasm => run_command(cargo_cmd([
            "build",
            "-p",
            "server_module",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])),
        BuildCmd::All(args) => {
            run_build(BuildCmd::Wasm)?;
            run_build(BuildCmd::Worker(BuildProfileArgs {
                release: args.release,
            }))?;
            run_build(BuildCmd::Client(BuildProfileArgs {
                release: args.release,
            }))?;
            run_build(BuildCmd::Cli(BuildProfileArgs {
                release: args.release,
            }))
        }
    }
}

fn run_test(cmd: TestCmd) -> Result<()> {
    match cmd {
        TestCmd::Fast => {
            run_command(cargo_cmd(["test", "-p", "game_core"]))?;
            run_command(cargo_cmd(["test", "-p", "simulation_worker"]))
        }
        TestCmd::Worker => run_command(cargo_cmd([
            "test",
            "-p",
            "simulation_worker",
            "--features",
            "connected",
        ])),
        TestCmd::Cli(args) => dev_client_test(args),
        TestCmd::MultiClient(args) => {
            let mut command = cargo_run_command(
                ["-p", "game_client", "--features", "connected"],
                args.release,
            );
            command.args(["--", "--test-multi"]);
            command.args(args.args);
            run_command(command)
        }
        TestCmd::Workspace => run_command(cargo_cmd([
            "test",
            "--workspace",
            "--exclude",
            "server_module",
        ])),
        TestCmd::Replay(args) => run_command(cargo_test_command(
            ["-p", "simulation_worker", "--test", "deterministic_replay"],
            args.release,
        )),
    }
}

fn dev_server() -> Result<()> {
    if is_server_up() {
        println!("SpacetimeDB already running at http://{SERVER_HOST}");
        return Ok(());
    }

    println!("SpacetimeDB not running; starting server");
    run_command(command("spacetime", ["start"]))
}

fn dev_schema() -> Result<()> {
    run_build(BuildCmd::Wasm)?;
    run_command(command(
        "spacetime",
        [
            "publish",
            MODULE_NAME,
            "-p",
            MODULE_PATH,
            "-s",
            SERVER_ALIAS,
        ],
    ))?;
    run_command(command(
        "spacetime",
        [
            "generate",
            "--lang",
            "rust",
            "--out-dir",
            WORKER_BINDINGS_OUT,
            "--module-path",
            MODULE_PATH,
        ],
    ))?;
    run_command(command(
        "spacetime",
        [
            "generate",
            "--lang",
            "rust",
            "--out-dir",
            CLIENT_BINDINGS_OUT,
            "--module-path",
            MODULE_PATH,
        ],
    ))
}

fn dev_reset(args: ResetArgs) -> Result<()> {
    let default_all = !args.db && !args.tokens && !args.all;
    let reset_db = args.all || args.db || default_all;
    let reset_tokens = args.all || args.tokens || default_all;

    if reset_db {
        run_command(command(
            "spacetime",
            ["delete", MODULE_NAME, "-s", SERVER_ALIAS],
        ))?;
    }

    if reset_tokens {
        remove_if_exists(".worker_token")?;
        remove_if_exists(".client_token")?;
        remove_if_exists(".client_token_a")?;
        remove_if_exists(".client_token_b")?;
    }

    Ok(())
}

fn dev_worker_register(args: WorkerRegisterArgs) -> Result<()> {
    if !is_server_up() {
        bail!("SpacetimeDB is not running. Start it with `cargo xtask dev server`.");
    }

    run_build(BuildCmd::Worker(BuildProfileArgs {
        release: args.release,
    }))?;
    let identity = capture_worker_identity(args.release)?;
    println!("Captured worker identity: {identity}");

    let id_json = format!(r#"{{"__identity__":"0x{identity}"}}"#);
    let status = command(
        "spacetime",
        [
            "call",
            MODULE_NAME,
            "register_worker",
            id_json.as_str(),
            "-s",
            SERVER_ALIAS,
        ],
    )
    .status()
    .context("failed to run spacetime register_worker")?;

    if status.success() {
        println!("Worker registered successfully");
    } else {
        println!("register_worker returned non-zero; worker may already be registered");
    }

    if args.seed_npc {
        run_command(command(
            "spacetime",
            [
                "call",
                MODULE_NAME,
                "spawn_npc",
                "0.0",
                "1.0",
                "0.0",
                "100.0",
                "-s",
                SERVER_ALIAS,
            ],
        ))?;
    }

    Ok(())
}

fn dev_worker(args: RunWorkerArgs) -> Result<()> {
    let mut command = cargo_cmd(["run", "-p", "simulation_worker", "--features", "connected"]);

    if args.release {
        command.arg("--release");
    }

    command.env("RUST_LOG", args.log);
    run_command(command)
}

fn dev_client_test(args: ClientTestArgs) -> Result<()> {
    let mut command = cargo_run_command(
        ["-p", "game_client", "--features", "connected"],
        args.release,
    );
    command.args(["--", "--test"]);
    command.args(args.args);
    run_command(command)
}

fn dev_client(args: RunClientArgs) -> Result<()> {
    let mut command = cargo_run_command(
        ["-p", "game_client_bevy", "--features", "connected"],
        args.release,
    );
    command.arg("--");
    command.args(args.args);
    run_command(command)
}

fn dev_clients(args: RunClientsArgs) -> Result<()> {
    run_build(BuildCmd::Client(BuildProfileArgs {
        release: args.release,
    }))?;

    let client_bin = client_binary_path(args.release).ok_or_else(|| {
        anyhow!(
            "Bevy client binary not found in target/{} (expected one of: tickforge_client, game_client_bevy)",
            if args.release { "release" } else { "debug" }
        )
    })?;

    println!(
        "Launching two Bevy clients with token files '{}' and '{}'",
        args.token_a, args.token_b
    );
    println!("Tip: remove these with `cargo xtask dev reset --tokens` when done.");

    let mut client_a = Command::new(&client_bin);
    client_a.env("STDB_TOKEN_FILE", &args.token_a);
    client_a.args(&args.args);

    let mut client_b = Command::new(&client_bin);
    client_b.env("STDB_TOKEN_FILE", &args.token_b);
    client_b.args(&args.args);

    let mut child_a = client_a
        .spawn()
        .with_context(|| format!("failed to launch client A ({})", client_bin.display()))?;
    // Tiny stagger keeps first-launch logs readable and avoids startup races.
    thread::sleep(Duration::from_millis(250));
    let mut child_b = client_b
        .spawn()
        .with_context(|| format!("failed to launch client B ({})", client_bin.display()))?;

    let status_a = child_a.wait().context("client A process failed")?;
    let status_b = child_b.wait().context("client B process failed")?;
    if !status_a.success() || !status_b.success() {
        bail!("one or more Bevy clients exited with an error status");
    }

    Ok(())
}

/// Push one synthetic terrain set + one flat chunk + one manifest row
/// into SpacetimeDB via the admin upsert reducers. Useful as an end-to-
/// end smoke for §4.8b Phase 5: bind a layer to the same `set_name` and
/// the worker should hydrate the chunk at the next tick boundary.
///
/// Vertices form a flat 2×half_extent square at `elevation` on the
/// XZ plane, two CCW triangles, suitable for `SharedShape::trimesh`.
fn dev_seed_terrain(args: SeedTerrainArgs) -> Result<()> {
    if !is_server_up() {
        bail!("SpacetimeDB is not running. Start it with `cargo xtask dev server`.");
    }

    let h = args.half_extent;
    let y = args.elevation;
    // 4 verts (xz-quad), CCW from above.
    let vertices: Vec<f32> = vec![
        -h, y, -h, // 0
        h, y, -h, // 1
        h, y, h, // 2
        -h, y, h, // 3
    ];
    let indices: Vec<u32> = vec![0, 1, 2, 0, 2, 3];

    let verts_json = floats_to_json(&vertices);
    let idx_json = u32s_to_json(&indices);
    let chunk_morton: u64 = 0; // single chunk at world origin
    let lod: u8 = 0;
    let content_hash = format!("smoke-{:x}-h{:.3}-y{:.3}", chunk_morton, h, y);
    let version: u32 = 1;

    println!(
        "Seeding terrain set='{}' (id={}) with one {}×{} m flat chunk at y={}",
        args.set_name,
        args.set_id,
        h * 2.0,
        h * 2.0,
        y,
    );

    // 1) terrain_set_upsert(name, content_hash, version)
    run_command(command(
        "spacetime",
        [
            "call",
            MODULE_NAME,
            "terrain_set_upsert",
            args.set_name.as_str(),
            content_hash.as_str(),
            "1",
            "-s",
            SERVER_ALIAS,
        ],
    ))?;

    // 2) terrain_chunk_upsert(set_id, morton, vertices, indices, lod)
    let set_id_str = args.set_id.to_string();
    let morton_str = chunk_morton.to_string();
    let lod_str = lod.to_string();
    run_command(command(
        "spacetime",
        [
            "call",
            MODULE_NAME,
            "terrain_chunk_upsert",
            set_id_str.as_str(),
            morton_str.as_str(),
            verts_json.as_str(),
            idx_json.as_str(),
            lod_str.as_str(),
            "-s",
            SERVER_ALIAS,
        ],
    ))?;

    // 3) terrain_manifest_upsert(set_id, morton, content_hash, version)
    let version_str = version.to_string();
    run_command(command(
        "spacetime",
        [
            "call",
            MODULE_NAME,
            "terrain_manifest_upsert",
            set_id_str.as_str(),
            morton_str.as_str(),
            content_hash.as_str(),
            version_str.as_str(),
            "-s",
            SERVER_ALIAS,
        ],
    ))?;

    println!(
        "Done. Bind a layer to terrain_set=\"{}\" (data/layers.ron or DungeonTemplate) \
         and restart the worker; look for \"TerrainState: applied N insert(s)\" in the \
         worker log.",
        args.set_name,
    );
    Ok(())
}

/// Format a `&[f32]` as a SpacetimeDB CLI JSON array argument.
fn floats_to_json(xs: &[f32]) -> String {
    let mut s = String::with_capacity(xs.len() * 6 + 2);
    s.push('[');
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // Always emit a decimal point so STDB parses as f32, not int.
        if x.fract() == 0.0 {
            s.push_str(&format!("{:.1}", x));
        } else {
            s.push_str(&format!("{}", x));
        }
    }
    s.push(']');
    s
}

/// Format a `&[u32]` as a SpacetimeDB CLI JSON array argument.
fn u32s_to_json(xs: &[u32]) -> String {
    let mut s = String::with_capacity(xs.len() * 4 + 2);
    s.push('[');
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s.push(']');
    s
}

/// Import a glTF/glb mesh, chunk it on an XZ grid, and push one
/// `terrain_chunk` row per non-empty cell via the admin reducers.
///
/// Pipeline:
///   1. `gltf::import` (binary, validates the file).
///   2. Walk all default-scene roots; recurse with accumulated 4×4
///      transforms; for each `Mesh::primitive` read positions +
///      indices and push transformed triangles into a global pool.
///   3. Bucket each triangle by the chunk that owns its centroid.
///   4. Per chunk: weld duplicate vertices (exact f32 equality after
///      transform/scale; rely on the server's `MERGE_DUPLICATE_VERTICES`
///      for fuzzy welding), build a flat `vertices: Vec<f32>` +
///      `indices: Vec<u32>`, encode `chunk_morton`, call the three
///      reducers.
fn dev_import_terrain(args: ImportTerrainArgs) -> Result<()> {
    if !is_server_up() {
        bail!("SpacetimeDB is not running. Start it with `cargo xtask dev server`.");
    }

    let path = &args.gltf;
    if !path.exists() {
        bail!("glTF file not found: {}", path.display());
    }
    if args.chunk_size <= 0.0 {
        bail!("--chunk-size must be > 0");
    }

    println!("Loading glTF: {}", path.display());
    let (doc, buffers, _images) =
        gltf::import(path).with_context(|| format!("loading {}", path.display()))?;

    // Collect all triangles into a flat (v0,v1,v2) world-space pool.
    let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
    for scene in doc.scenes() {
        for node in scene.nodes() {
            collect_node_triangles(&node, &buffers, identity4(), &mut tris);
        }
    }

    if tris.is_empty() {
        bail!(
            "No triangles found in {}. Ensure the mesh is triangulated and uses POSITION + indices.",
            path.display()
        );
    }

    // Apply user offset/scale and optional flip.
    let s = args.scale;
    let (ox, oy, oz) = (args.offset_x, args.offset_y, args.offset_z);
    for t in tris.iter_mut() {
        for v in t.iter_mut() {
            v[0] = v[0] * s + ox;
            v[1] = v[1] * s + oy;
            v[2] = v[2] * s + oz;
        }
        if args.flip_winding {
            t.swap(1, 2);
        }
    }

    println!("Imported {} triangles. Bucketing...", tris.len());

    // Bucket triangles by (cx, cz) chunk via centroid; cy=0 (single y-row).
    use std::collections::HashMap;
    let cs = args.chunk_size;
    let mut buckets: HashMap<(i32, i32), Vec<[[f32; 3]; 3]>> = HashMap::new();
    for t in tris.iter() {
        let cx = ((t[0][0] + t[1][0] + t[2][0]) / 3.0 / cs).floor() as i32;
        let cz = ((t[0][2] + t[1][2] + t[2][2]) / 3.0 / cs).floor() as i32;
        buckets.entry((cx, cz)).or_default().push(*t);
    }

    println!(
        "{} non-empty chunks at {} m grid.",
        buckets.len(),
        args.chunk_size
    );

    // Obtain auth token once (needed for HTTP reducer calls).
    let token = get_spacetime_token()?;

    // 1) terrain_set_upsert (once), then resolve the DB-assigned id.
    let set_hash = format!("gltf-{}-cs{}", path_stem(path), cs);
    if !args.skip_set {
        let body = format!(
            "[\"{}\", \"{}\", 1]",
            json_escape(&args.set_name),
            json_escape(&set_hash)
        );
        call_reducer_http("terrain_set_upsert", &body, &token)?;
    }

    // Resolve actual terrain_set_id from DB (auto-incremented by the server;
    // the --set-id arg is now a fallback only used when --skip-set is given
    // without a running server that can be queried).
    let resolved_set_id = query_terrain_set_id(&args.set_name).unwrap_or_else(|e| {
        eprintln!(
            "Warning: could not resolve terrain_set_id for '{}' via SQL ({e}); \
             falling back to --set-id {}",
            args.set_name, args.set_id
        );
        args.set_id
    });
    println!(
        "terrain_set '{}' → id={}",
        args.set_name, resolved_set_id
    );

    // 2) per-chunk: weld + upsert chunk + upsert manifest.
    let mut sorted_keys: Vec<(i32, i32)> = buckets.keys().copied().collect();
    sorted_keys.sort();
    for (i, key) in sorted_keys.iter().enumerate() {
        let bucket = &buckets[key];
        let (cx, cz) = *key;
        let (verts, idx) = weld_triangles(bucket);
        let morton = game_schema::morton::encode_chunk_morton(cx, 0, cz);
        let verts_json = floats_to_json(&verts);
        let idx_json = u32s_to_json(&idx);
        let chunk_hash = format!(
            "{}-c{}-{}-v{}-i{}",
            set_hash,
            cx,
            cz,
            verts.len() / 3,
            idx.len() / 3
        );

        println!(
            "  [{:>4}/{}] chunk ({:+}, {:+}) morton={} verts={} tris={}",
            i + 1,
            sorted_keys.len(),
            cx,
            cz,
            morton,
            verts.len() / 3,
            idx.len() / 3
        );

        // Use HTTP POST to avoid Windows command-line length limit (os error 206)
        // that hits `spacetime call` when vertex/index arrays are large.
        let chunk_body = format!(
            "[{}, {}, {}, {}, {}]",
            resolved_set_id, morton, verts_json, idx_json, args.lod
        );
        // SpacetimeDB's HTTP server enforces a request body limit (~2 MB by
        // default). Bail early with actionable advice instead of letting the
        // server return an opaque 413 mid-import.
        const MAX_REDUCER_BODY: usize = 1_900_000;
        if chunk_body.len() > MAX_REDUCER_BODY {
            bail!(
                "chunk ({cx}, {cz}) JSON body is {} bytes (>{} byte limit): \
                 {} verts / {} tris is too dense for one chunk.\n\
                 Hints:\n  \
                 - Re-run with a smaller `--chunk-size` (e.g. {} or {})\n  \
                 - Or decimate the source mesh in Blender before importing \
                 (collision geometry rarely needs >50k tris per chunk).",
                chunk_body.len(),
                MAX_REDUCER_BODY,
                verts.len() / 3,
                idx.len() / 3,
                (cs * 0.5).max(1.0),
                (cs * 0.25).max(1.0),
            );
        }
        call_reducer_http("terrain_chunk_upsert", &chunk_body, &token)?;

        let manifest_body = format!(
            "[{}, {}, \"{}\", 1]",
            resolved_set_id,
            morton,
            json_escape(&chunk_hash)
        );
        call_reducer_http("terrain_manifest_upsert", &manifest_body, &token)?;
    }

    println!(
        "Done. Bind a layer to terrain_set=\"{}\" and restart the worker.",
        args.set_name
    );
    Ok(())
}

fn path_stem(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("terrain")
        .to_string()
}

fn identity4() -> [[f32; 4]; 4] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// Column-major 4x4 multiply (matches `gltf::scene::Transform::matrix`).
fn mat_mul(a: [[f32; 4]; 4], b: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut out = [[0.0f32; 4]; 4];
    for c in 0..4 {
        for r in 0..4 {
            let mut s = 0.0f32;
            for k in 0..4 {
                s += a[k][r] * b[c][k];
            }
            out[c][r] = s;
        }
    }
    out
}

fn transform_point(m: [[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    let x = m[0][0] * p[0] + m[1][0] * p[1] + m[2][0] * p[2] + m[3][0];
    let y = m[0][1] * p[0] + m[1][1] * p[1] + m[2][1] * p[2] + m[3][1];
    let z = m[0][2] * p[0] + m[1][2] * p[1] + m[2][2] * p[2] + m[3][2];
    [x, y, z]
}

fn collect_node_triangles(
    node: &gltf::Node,
    buffers: &[gltf::buffer::Data],
    parent: [[f32; 4]; 4],
    out: &mut Vec<[[f32; 3]; 3]>,
) {
    let local = node.transform().matrix();
    let world = mat_mul(parent, local);
    if let Some(mesh) = node.mesh() {
        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                eprintln!(
                    "  skipping primitive on mesh '{}': non-triangle mode {:?}",
                    mesh.name().unwrap_or("<unnamed>"),
                    prim.mode()
                );
                continue;
            }
            let reader = prim.reader(|b| Some(&buffers[b.index()]));
            let positions: Vec<[f32; 3]> = match reader.read_positions() {
                Some(it) => it.collect(),
                None => continue,
            };
            let indices: Vec<u32> = match reader.read_indices() {
                Some(idx) => idx.into_u32().collect(),
                None => (0..positions.len() as u32).collect(),
            };
            for tri in indices.chunks_exact(3) {
                let a = transform_point(world, positions[tri[0] as usize]);
                let b = transform_point(world, positions[tri[1] as usize]);
                let c = transform_point(world, positions[tri[2] as usize]);
                out.push([a, b, c]);
            }
        }
    }
    for child in node.children() {
        collect_node_triangles(&child, buffers, world, out);
    }
}

/// Weld a triangle list into a flat (vertices, indices) pair using exact
/// bit-pattern equality on f32 coords (sufficient because all transforms
/// are deterministic float ops and duplicates from glTF index reuse will
/// hit the same bit pattern). Parry's `MERGE_DUPLICATE_VERTICES` will
/// further weld near-duplicates server-side.
fn weld_triangles(tris: &[[[f32; 3]; 3]]) -> (Vec<f32>, Vec<u32>) {
    use std::collections::HashMap;
    let mut map: HashMap<[u32; 3], u32> = HashMap::new();
    let mut verts: Vec<f32> = Vec::with_capacity(tris.len() * 3);
    let mut indices: Vec<u32> = Vec::with_capacity(tris.len() * 3);
    for t in tris {
        for v in t {
            let key = [v[0].to_bits(), v[1].to_bits(), v[2].to_bits()];
            let id = *map.entry(key).or_insert_with(|| {
                let id = (verts.len() / 3) as u32;
                verts.extend_from_slice(v);
                id
            });
            indices.push(id);
        }
    }
    (verts, indices)
}

/// Get the local SpacetimeDB auth token by running `spacetime token show`.
/// The token is a JWT that begins with `ey`; warning/info lines on stdout
/// are filtered out.
/// Locate the SpacetimeDB CLI config file (cli.toml).
fn find_stdb_cli_config() -> Result<PathBuf> {
    // Windows: %LOCALAPPDATA%\SpacetimeDB\config\cli.toml
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local)
            .join("SpacetimeDB")
            .join("config")
            .join("cli.toml");
        if p.exists() {
            return Ok(p);
        }
    }
    // macOS: ~/Library/Application Support/spacetimedb/config/cli.toml
    // Linux: ~/.local/share/spacetimedb/config/cli.toml
    if let Ok(home) = std::env::var("HOME") {
        let candidates = [
            PathBuf::from(&home)
                .join("Library")
                .join("Application Support")
                .join("spacetimedb")
                .join("config")
                .join("cli.toml"),
            PathBuf::from(&home)
                .join(".local")
                .join("share")
                .join("spacetimedb")
                .join("config")
                .join("cli.toml"),
        ];
        for p in &candidates {
            if p.exists() {
                return Ok(p.clone());
            }
        }
    }
    bail!(
        "SpacetimeDB CLI config not found. \
         Ensure `spacetime` is installed and you are logged in (`spacetime login`)."
    )
}

/// Get the SpacetimeDB auth token by reading `spacetimedb_token` from the
/// CLI config file (`cli.toml`).  This is the JWT the CLI uses for all
/// server calls (local and remote).
fn get_spacetime_token() -> Result<String> {
    let config_path = find_stdb_cli_config()?;
    let content = fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;

    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("spacetimedb_token") {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix('=') {
                let val = rest.trim();
                if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
                    return Ok(val[1..val.len() - 1].to_string());
                }
            }
        }
    }
    bail!(
        "spacetimedb_token not found in {}. \
         Run `spacetime login` to authenticate.",
        config_path.display()
    )
}

/// Call a SpacetimeDB reducer via HTTP POST.  Avoids the Windows
/// command-line length limit (os error 206) that hits `spacetime call`
/// when vertex/index JSON is large.
///
/// `json_body` must be a valid JSON array of reducer args, e.g.
/// `[1, 12345, [1.0, 2.0], [0, 1, 2], 0]`.  u64 values are passed as
/// plain JSON numbers; SpacetimeDB's serde uses arbitrary-precision
/// integers so large u64 values are handled correctly.
fn call_reducer_http(reducer: &str, json_body: &str, token: &str) -> Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let path = format!("/v1/database/{MODULE_NAME}/call/{reducer}");
    let body_bytes = json_body.as_bytes();
    let request = format!(
        "POST {path} HTTP/1.0\r\nHost: {SERVER_HOST}\r\nContent-Type: application/json\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n",
        body_bytes.len()
    );

    let mut stream = TcpStream::connect(SERVER_HOST)
        .with_context(|| format!("cannot connect to SpacetimeDB at {SERVER_HOST}"))?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(body_bytes)?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("reading HTTP response")?;

    let status_line = response.lines().next().unwrap_or("");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    if !(200..300).contains(&status_code) {
        let body = response.split("\r\n\r\n").nth(1).unwrap_or("").trim();
        bail!(
            "reducer `{reducer}` HTTP call failed ({status_line}):\n{body}"
        );
    }
    Ok(())
}

/// Escape a Rust string for safe embedding as a JSON string value
/// (handles `"`, `\`, `\n`, `\r`, `\t`).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Query the `terrain_set` table to resolve a set name → numeric id.
/// Calls `spacetime sql` and parses the single-column result.
fn query_terrain_set_id(set_name: &str) -> Result<u32> {
    let sql = format!(
        "SELECT terrain_set_id FROM terrain_set WHERE name = '{}'",
        set_name.replace('\'', "''")
    );
    let output = Command::new("spacetime")
        .args(["sql", MODULE_NAME, &sql, "-s", SERVER_ALIAS])
        .output()
        .context("failed to run `spacetime sql`")?;
    if !output.status.success() {
        bail!(
            "spacetime sql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format:  terrain_set_id\n──────────────────\n 1 \n
    for line in stdout.lines() {
        let trimmed = line.trim().trim_matches('-');
        let trimmed = trimmed.trim();
        if let Ok(id) = trimmed.parse::<u32>() {
            return Ok(id);
        }
    }
    bail!(
        "terrain_set '{}' not found in DB after upsert. \
         Check that the server is running and the set was created.",
        set_name
    )
}

fn dev_capture_fixture(args: CaptureFixtureArgs) -> Result<()> {
    let mut command = cargo_test_command(
        ["-p", "simulation_worker", "--test", "deterministic_replay"],
        args.release,
    );
    command.args([
        "capture_combat_lifecycle_fixture_to_json",
        "--",
        "--ignored",
        "--exact",
        "--nocapture",
    ]);

    command.env("REPLAY_FIXTURE_OUT", args.out);
    run_command(command)
}

fn capture_worker_identity(release: bool) -> Result<String> {
    let mut child = cargo_run_command(
        ["-p", "simulation_worker", "--features", "connected"],
        release,
    )
    .env("RUST_LOG", "simulation_worker=info")
    .stderr(Stdio::piped())
    .stdout(Stdio::null())
    .spawn()
    .context("failed to launch simulation worker")?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("failed to capture worker stderr"))?;
    let mut reader = BufReader::new(stderr);
    let regex = Regex::new(r"Worker identity: ([0-9a-f]{64})")?;

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut line = String::new();
    let identity = loop {
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            bail!("Timed out waiting for worker identity");
        }

        line.clear();
        if reader.read_line(&mut line)? == 0 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        if let Some(captures) = regex.captures(line.trim()) {
            let id = captures
                .get(1)
                .ok_or_else(|| anyhow!("identity capture group missing"))?
                .as_str()
                .to_string();
            child.kill().ok();
            child.wait().ok();
            break id;
        }
    };

    Ok(identity)
}

fn remove_if_exists(path: &str) -> Result<()> {
    if Path::new(path).exists() {
        fs::remove_file(path).with_context(|| format!("failed removing {path}"))?;
        println!("Removed {path}");
    }
    Ok(())
}

fn is_server_up() -> bool {
    TcpStream::connect_timeout(
        &SERVER_HOST.parse().expect("valid server host"),
        Duration::from_millis(250),
    )
    .is_ok()
}

fn cargo_cmd<const N: usize>(args: [&str; N]) -> Command {
    command("cargo", args)
}

fn cargo_build_command<const N: usize>(args: [&str; N], release: bool) -> Command {
    let mut command = cargo_cmd(["build"]);
    command.args(args);
    if release {
        command.arg("--release");
    }
    command
}

fn cargo_run_command<const N: usize>(args: [&str; N], release: bool) -> Command {
    let mut command = cargo_cmd(["run"]);
    command.args(args);
    if release {
        command.arg("--release");
    }
    command
}

fn cargo_test_command<const N: usize>(args: [&str; N], release: bool) -> Command {
    let mut command = cargo_cmd(["test"]);
    command.args(args);
    if release {
        command.arg("--release");
    }
    command
}

fn client_binary_path(release: bool) -> Option<PathBuf> {
    let mut base = PathBuf::from("target");
    base.push(if release { "release" } else { "debug" });

    let candidates: &[&str] = if cfg!(windows) {
        &["tickforge_client.exe", "game_client_bevy.exe"]
    } else {
        &["tickforge_client", "game_client_bevy"]
    };

    candidates
        .iter()
        .map(|name| base.join(name))
        .find(|path| path.exists())
}

fn command<const N: usize>(program: &str, args: [&str; N]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    command
}

fn run_command(mut command: Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run command: {command:?}"))?;
    if !status.success() {
        bail!("command failed with status {status}: {command:?}");
    }
    Ok(())
}
