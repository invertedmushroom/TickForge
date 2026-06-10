use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use regex::Regex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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
const WEB_APP_DIR: &str = "apps/web";
const WEB_CONTRACT_OUT: &str = "target/web-contract";
const WEB_CONTRACT_BINDINGS_DIR: &str = "bindings";
const WEB_CONTRACT_PACKAGE_NAME: &str = "@dive/client-contract";
const WEB_CONTRACT_STDB_NPM_VERSION: &str = "2.4.1";
const WEB_BROWSER_POLICY_FILE: &str = "browser-policy.json";
const WEB_ABILITIES_FILE: &str = "abilities.json";
const WEB_PHYSICS_PREDICTION_FILE: &str = "physics-prediction.json";
const WEB_CONTENT_METADATA_FILE: &str = "content-metadata.json";
const WEB_CONTENT_LOOKUP_FILE: &str = "content-lookup.json";
const WEB_MAP_BUNDLES_DIR: &str = "map-bundles";
const WEB_MAP_BUNDLE_COLLIDER_FORMAT_VERSION: u32 = 1;
const WEB_CONTENT_SOURCES: &[&str] = &[
    "data/abilities.ron",
    "data/buffs.ron",
    "data/dungeons.ron",
    "data/items.ron",
    "data/layers.ron",
    "data/npc_archetypes.ron",
    "data/spawn_rules.ron",
];
const WEB_ALWAYS_ON_SUBSCRIPTIONS: &[&str] = &[
    "SELECT * FROM my_region",
    "SELECT * FROM nearby_transforms",
    "SELECT * FROM nearby_entities",
    "SELECT * FROM nearby_health",
    "SELECT * FROM client_sequence",
    "SELECT * FROM sim_tick",
    "SELECT * FROM module_config",
    "SELECT * FROM combat_event",
    "SELECT * FROM world_event",
    "SELECT * FROM entity_layer",
    "SELECT * FROM instance",
    "SELECT * FROM instance_membership",
    "SELECT * FROM death_state",
];
const WEB_FEATURE_SUBSCRIPTIONS: &[&str] = &[
    "SELECT * FROM player_inventory",
    "SELECT * FROM player_equipment",
    "SELECT * FROM bank",
    "SELECT * FROM party",
    "SELECT * FROM party_member",
    "SELECT * FROM party_invite",
    "SELECT * FROM boss_phase",
    "SELECT * FROM world_phase",
    "SELECT * FROM respawn_point",
    "SELECT * FROM interactable_config",
    "SELECT * FROM loot_pile",
    "SELECT * FROM loot_pile_item",
    "SELECT * FROM active_buff",
    "SELECT * FROM npc_state",
    "SELECT * FROM entity_team",
];
const WEB_FORBIDDEN_TABLES: &[&str] = &[
    "entity",
    "entity_transform",
    "entity_health",
    "entity_region",
    "stealthed_entity",
    "trusted_worker",
];
const WEB_ALLOWED_REDUCERS: &[&str] = &[
    "spawnPlayer",
    "submitIntent",
    "submitIntentsBatch",
    "respawnPlayer",
    "joinInstance",
    "leaveInstance",
    "equipItem",
    "unequipItem",
    "swapItem",
    "claimLoot",
    "createParty",
    "inviteToParty",
    "acceptPartyInvite",
    "declinePartyInvite",
    "leaveParty",
    "kickFromParty",
    "disbandParty",
];
const WEB_FORBIDDEN_REDUCERS: &[&str] = &[
    "registerWorker",
    "commitTickResults",
    "tickTrigger",
    "worldClock",
    "createInstance",
    "terrainSetUpsert",
    "terrainChunkUpsert",
    "terrainManifestUpsert",
    "spawnNpc",
    "addRespawnPoint",
    "removeRespawnPoint",
    "expireInstances",
    "incrementZoneCounter",
    "lootItem",
];

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
    /// Generate the browser-facing contract artifact package.
    WebContract(WebContractArgs),
    /// Validate checked-in RON content references.
    ContentCheck,
    /// Refresh the web contract and start the browser dev server.
    Web,
    Reset(ResetArgs),
    WorkerRegister(WorkerRegisterArgs),
    Worker(RunWorkerArgs),
    /// Capture a deterministic replay fixture to a JSON file.
    CaptureFixture(CaptureFixtureArgs),
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
    Web,
    Wasm,
    All(BuildProfileArgs),
}

#[derive(Subcommand)]
enum TestCmd {
    Fast,
    /// Run simulation_worker tests with the `connected` feature enabled.
    Worker(BuildProfileArgs),
    Cli(ClientTestArgs),
    /// Run multi-client integration tests (requires running server + worker)
    MultiClient(ClientTestArgs),
    /// Run the browser client CI test lane.
    Web,
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

#[derive(Args)]
struct WebContractArgs {
    /// Output directory for generated web-contract artifacts.
    #[arg(long, default_value = WEB_CONTRACT_OUT)]
    out: PathBuf,

    /// Skip schema publish/regeneration before TypeScript codegen.
    #[arg(long, default_value_t = false)]
    skip_schema: bool,

    /// Validate that output is up to date without writing files.
    #[arg(long, default_value_t = false)]
    check: bool,
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
        DevCmd::WebContract(args) => dev_web_contract(args),
        DevCmd::ContentCheck => dev_content_check(),
        DevCmd::Web => dev_web(),
        DevCmd::Reset(args) => dev_reset(args),
        DevCmd::WorkerRegister(args) => dev_worker_register(args),
        DevCmd::Worker(args) => dev_worker(args),
        DevCmd::CaptureFixture(args) => dev_capture_fixture(args),
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
        BuildCmd::Web => build_web(),
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
            }))?;
            run_build(BuildCmd::Web)
        }
    }
}

fn run_test(cmd: TestCmd) -> Result<()> {
    match cmd {
        TestCmd::Fast => {
            run_command(cargo_cmd(["test", "-p", "game_core"]))?;
            run_command(cargo_cmd(["test", "-p", "simulation_worker"]))
        }
        TestCmd::Worker(args) => run_command(cargo_test_command(
            ["-p", "simulation_worker", "--features", "connected"],
            args.release,
        )),
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
        TestCmd::Web => test_web(),
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

fn dev_web() -> Result<()> {
    refresh_web_contract()?;
    run_npm_in_web(["run", "dev"])
}

fn build_web() -> Result<()> {
    refresh_web_contract()?;
    run_npm_in_web(["run", "build"])
}

fn test_web() -> Result<()> {
    refresh_web_contract()?;
    run_npm_in_web(["run", "test:ci"])
}

fn refresh_web_contract() -> Result<()> {
    dev_web_contract(WebContractArgs {
        out: PathBuf::from(WEB_CONTRACT_OUT),
        skip_schema: true,
        check: false,
    })
}

fn run_npm_in_web<const N: usize>(args: [&str; N]) -> Result<()> {
    let mut command = Command::new(npm_program());
    command.args(args);
    command.current_dir(WEB_APP_DIR);
    run_command(command)
}

fn npm_program() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
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

#[derive(Serialize)]
struct WebContractManifest {
    package_name: String,
    contract_version: String,
    schema_hash: String,
    content_hash: String,
    metadata_hash: String,
    physics_hash: String,
    bindings_dir: String,
    browser_policy_file: String,
    abilities_file: String,
    physics_prediction_file: String,
    content_metadata_file: String,
    content_lookup_file: String,
    map_bundles_dir: String,
    layer_count: usize,
    dungeon_template_count: usize,
    source_files: Vec<String>,
}

#[derive(Serialize)]
struct WebContractPackageJson {
    name: String,
    version: String,
    private: bool,
    #[serde(rename = "type")]
    package_type: String,
    description: String,
    main: String,
    files: Vec<String>,
    exports: BTreeMap<String, String>,
    dependencies: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct WebContentMetadata {
    schema_version: u32,
    layers: Vec<LayerMetadata>,
    dungeon_templates: Vec<DungeonTemplateMetadata>,
}

#[derive(Serialize)]
struct WebContentLookup {
    layer_ids: Vec<u32>,
    layer_name_to_ids: BTreeMap<String, Vec<u32>>,
    dungeon_template_ids: Vec<String>,
    terrain_sets: Vec<String>,
}

#[derive(Serialize)]
struct BrowserPolicy {
    schema_version: u32,
    always_on_subscriptions: Vec<String>,
    feature_subscriptions: Vec<String>,
    forbidden_tables: Vec<String>,
    allowed_reducers: Vec<String>,
    forbidden_reducers: Vec<String>,
}

#[derive(Serialize)]
struct WebPhysicsPrediction {
    #[serde(flatten)]
    payload: WebPhysicsPredictionPayload,
    hash: String,
}

#[derive(Serialize)]
struct WebPhysicsPredictionPayload {
    schema_version: u32,
    tick_rate_hz: u32,
    fixed_dt_seconds: f32,
    player_capsule: WebCapsulePhysics,
    kcc: WebKccPhysics,
    collision_groups: WebKccCollisionGroups,
}

#[derive(Serialize)]
struct WebCapsulePhysics {
    half_height: f32,
    radius: f32,
}

#[derive(Serialize)]
struct WebKccPhysics {
    offset_relative: f32,
    normal_nudge_factor: f32,
    max_slope_climb_radians: f32,
    snap_to_ground_relative: f32,
    autostep_max_height_relative: f32,
    autostep_min_width_relative: f32,
    autostep_include_dynamic_bodies: bool,
    ground_pull_meters_per_second: f32,
    gravity_meters_per_second_squared: f32,
    move_shape_dt_seconds: f32,
}

#[derive(Serialize)]
struct WebKccCollisionGroups {
    movement_membership_bits: u32,
    movement_filter_bits: u32,
}

#[derive(Serialize)]
struct WebAbilityCatalog {
    schema_version: u32,
    tick_rate: u32,
    abilities: Vec<WebAbilityMetadata>,
}

#[derive(Serialize)]
struct WebAbilityMetadata {
    ability_id: u32,
    name: String,
    base_damage: f32,
    damage_type: String,
    targeting_mode: WebTargetingModeMetadata,
    cast_facing_policy: String,
    max_range: Option<f32>,
    projectile_speed: Option<f32>,
    cooldown_ticks: u32,
    linger_ticks: u32,
    damage_interval_ticks: u32,
    timeline_duration_ticks: u32,
    hitbox_spawn_tick: Option<u32>,
    damage_frame_tick: Option<u32>,
    hitbox_remove_tick: Option<u32>,
    lock_on_timeout_ticks: Option<u32>,
    charge_tiers: Vec<WebChargeTierMetadata>,
    preview_shape: WebPreviewShapeMetadata,
    offset: [f32; 3],
}

#[derive(Serialize)]
struct WebChargeTierMetadata {
    min_ticks: u32,
    damage_mult: f32,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WebTargetingModeMetadata {
    DirectionTarget,
    EntityTarget,
    GroundTarget,
    RaycastStrict,
    AimAssist,
    LockOn { max_targets: u32 },
    SelfOnly,
    CasterOffset,
}

#[derive(Serialize, Clone, Copy)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WebPreviewShapeMetadata {
    None,
    Sphere { radius: f32 },
    Capsule { radius: f32, half_height: f32 },
}

#[derive(Serialize, Clone)]
struct LayerMetadata {
    layer_id: u32,
    name: String,
    terrain_set: Option<String>,
    client_visual: Option<String>,
    collision_policy: LayerCollisionPolicyMetadata,
    spawn_points: Vec<[f32; 3]>,
    geometry: Vec<GeometryMetadata>,
}

#[derive(Serialize, Clone)]
struct DungeonTemplateMetadata {
    template_id: String,
    name: String,
    max_players: u32,
    terrain_set: Option<String>,
    collision_policy: LayerCollisionPolicyMetadata,
    spawn_points: Vec<[f32; 3]>,
    exit_points: Vec<[f32; 3]>,
    geometry: Vec<GeometryMetadata>,
    interactables: Vec<InteractableMetadata>,
}

#[derive(Serialize, Clone)]
struct LayerCollisionPolicyMetadata {
    player_collides_player: bool,
}

#[derive(Serialize, Clone)]
struct GeometryMetadata {
    position: [f32; 3],
    shape: ShapeMetadata,
}

#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ShapeMetadata {
    Cuboid {
        half_x: f32,
        half_y: f32,
        half_z: f32,
    },
    Cylinder {
        half_height: f32,
        radius: f32,
    },
    Heightfield {
        nrows: usize,
        ncols: usize,
        scale_x: f32,
        scale_y: f32,
        scale_z: f32,
        heights: Vec<f32>,
    },
    TriMesh {
        vertices: Vec<f32>,
        indices: Vec<u32>,
    },
}

#[derive(Serialize, Clone)]
struct InteractableMetadata {
    local_id: u32,
    script_id: Option<String>,
    tags: Vec<String>,
    kind: InteractableKindMetadata,
    position: [f32; 3],
    linked_to: Option<u32>,
    required_buff: Option<u32>,
    required_item: Option<u32>,
    interact_range: Option<f32>,
    puzzle_group: Option<String>,
    puzzle_required_count: Option<u32>,
    puzzle_window_ticks: Option<u32>,
    body_shape: Option<BodyShapeMetadata>,
}

#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum InteractableKindMetadata {
    Gate,
    Switch,
    Chest,
    BossSpawn {
        npc_name: String,
        encounter_name: Option<String>,
        archetype_id: Option<String>,
    },
    NpcSpawn {
        npc_name: String,
        archetype_id: Option<String>,
    },
}

#[derive(Serialize, Clone)]
struct BodyShapeMetadata {
    id: u8,
    name: String,
}

#[derive(Serialize)]
struct MapBundleIndex {
    schema_version: u32,
    bundles: Vec<MapBundleIndexEntry>,
}

#[derive(Serialize)]
struct MapBundleIndexEntry {
    bundle_id: String,
    source_kind: String,
    name: String,
    layer_id: Option<u32>,
    dungeon_template_id: Option<String>,
    terrain_set: Option<String>,
    manifest_path: String,
    collider_count: usize,
    content_hash: String,
}

#[derive(Serialize)]
struct MapBundleModule {
    schema_version: u32,
    bundles: Vec<MapBundleModuleEntry>,
}

#[derive(Serialize)]
struct MapBundleModuleEntry {
    bundle_id: String,
    manifest: MapBundleManifest,
    colliders: ColliderBundle,
}

#[derive(Serialize)]
struct MapBundleManifest {
    schema_version: u32,
    bundle_version: String,
    /// Physics determinism hash. Equal to `colliders.content_hash`.
    /// Changing this invalidates client prediction; must match the worker's bundle.
    content_hash: String,
    /// Hash over the visual asset payload (`render_meshes` + their referenced bytes).
    /// `None` when no visual bundle exists; clients must gate visual loading on this
    /// hash, not on `content_hash`, so visual revisions never invalidate prediction.
    visual_content_hash: Option<String>,
    bundle_id: String,
    source: MapBundleSource,
    render_meshes: Vec<AssetRef>,
    collider_json: Vec<AssetRef>,
    debug_markers: Vec<DebugMarker>,
}

#[derive(Serialize, Clone)]
struct MapBundleSource {
    kind: String,
    name: String,
    layer_id: Option<u32>,
    dungeon_template_id: Option<String>,
    terrain_set: Option<String>,
}

#[derive(Serialize, Clone)]
struct AssetRef {
    url: String,
    sha256: String,
    bytes: usize,
}

#[derive(Serialize, Clone)]
struct DebugMarker {
    kind: String,
    label: String,
    position: [f32; 3],
}

#[derive(Serialize, Clone)]
struct ColliderBundle {
    format_version: u32,
    content_hash: String,
    coordinate_system: String,
    source: MapBundleSource,
    colliders: Vec<BundleCollider>,
}

#[derive(Serialize, Clone)]
struct BundleCollider {
    collider_id: String,
    position: [f32; 3],
    rotation: [f32; 4],
    shape: ShapeMetadata,
}

fn build_content_metadata(
    world_layers: &game_schema::dungeon::WorldLayersFile,
    dungeons: &game_schema::dungeon::DungeonFile,
) -> WebContentMetadata {
    let layers = world_layers
        .layers
        .iter()
        .map(export_layer_metadata)
        .collect::<Vec<_>>();
    let dungeon_templates = dungeons
        .templates
        .iter()
        .map(export_dungeon_template_metadata)
        .collect::<Vec<_>>();

    WebContentMetadata {
        schema_version: 1,
        layers,
        dungeon_templates,
    }
}

fn build_content_lookup(metadata: &WebContentMetadata) -> WebContentLookup {
    let layer_ids = metadata
        .layers
        .iter()
        .map(|layer| layer.layer_id)
        .collect::<Vec<_>>();

    let mut layer_name_to_ids = BTreeMap::<String, Vec<u32>>::new();
    for layer in &metadata.layers {
        layer_name_to_ids
            .entry(layer.name.clone())
            .or_default()
            .push(layer.layer_id);
    }

    let dungeon_template_ids = metadata
        .dungeon_templates
        .iter()
        .map(|template| template.template_id.clone())
        .collect::<Vec<_>>();

    let mut terrain_sets = BTreeSet::new();
    for layer in &metadata.layers {
        if let Some(set) = &layer.terrain_set {
            terrain_sets.insert(set.clone());
        }
    }
    for template in &metadata.dungeon_templates {
        if let Some(set) = &template.terrain_set {
            terrain_sets.insert(set.clone());
        }
    }

    WebContentLookup {
        layer_ids,
        layer_name_to_ids,
        dungeon_template_ids,
        terrain_sets: terrain_sets.into_iter().collect(),
    }
}

fn build_browser_policy() -> BrowserPolicy {
    BrowserPolicy {
        schema_version: 1,
        always_on_subscriptions: WEB_ALWAYS_ON_SUBSCRIPTIONS
            .iter()
            .map(|value| value.to_string())
            .collect(),
        feature_subscriptions: WEB_FEATURE_SUBSCRIPTIONS
            .iter()
            .map(|value| value.to_string())
            .collect(),
        forbidden_tables: WEB_FORBIDDEN_TABLES
            .iter()
            .map(|value| value.to_string())
            .collect(),
        allowed_reducers: WEB_ALLOWED_REDUCERS
            .iter()
            .map(|value| value.to_string())
            .collect(),
        forbidden_reducers: WEB_FORBIDDEN_REDUCERS
            .iter()
            .map(|value| value.to_string())
            .collect(),
    }
}

fn build_physics_prediction() -> Result<WebPhysicsPrediction> {
    use game_core::collision_layers::CollisionMasks;
    use game_core::physics_constants::{
        CAPSULE_HALF_HEIGHT, CAPSULE_RADIUS, FALL_GRAVITY, GROUND_PULL,
        KCC_AUTOSTEP_INCLUDE_DYNAMIC_BODIES, KCC_AUTOSTEP_MAX_HEIGHT_RELATIVE,
        KCC_AUTOSTEP_MIN_WIDTH_RELATIVE, KCC_MAX_SLOPE_CLIMB_RADIANS, KCC_MOVE_SHAPE_DT_SECONDS,
        KCC_NORMAL_NUDGE_FACTOR, KCC_OFFSET_RELATIVE, KCC_SNAP_TO_GROUND_RELATIVE,
    };

    let tick_config = game_protocol::tick::TickConfig::default_20hz();
    let payload = WebPhysicsPredictionPayload {
        schema_version: 1,
        tick_rate_hz: tick_config.rate_hz,
        fixed_dt_seconds: tick_config.dt,
        player_capsule: WebCapsulePhysics {
            half_height: CAPSULE_HALF_HEIGHT,
            radius: CAPSULE_RADIUS,
        },
        kcc: WebKccPhysics {
            offset_relative: KCC_OFFSET_RELATIVE,
            normal_nudge_factor: KCC_NORMAL_NUDGE_FACTOR,
            max_slope_climb_radians: KCC_MAX_SLOPE_CLIMB_RADIANS,
            snap_to_ground_relative: KCC_SNAP_TO_GROUND_RELATIVE,
            autostep_max_height_relative: KCC_AUTOSTEP_MAX_HEIGHT_RELATIVE,
            autostep_min_width_relative: KCC_AUTOSTEP_MIN_WIDTH_RELATIVE,
            autostep_include_dynamic_bodies: KCC_AUTOSTEP_INCLUDE_DYNAMIC_BODIES,
            ground_pull_meters_per_second: GROUND_PULL,
            gravity_meters_per_second_squared: FALL_GRAVITY,
            move_shape_dt_seconds: KCC_MOVE_SHAPE_DT_SECONDS,
        },
        collision_groups: WebKccCollisionGroups {
            movement_membership_bits: CollisionMasks::KCC_MOVEMENT_MEMBERSHIP,
            movement_filter_bits: CollisionMasks::KCC_MOVEMENT_FILTER,
        },
    };
    let payload_json =
        serde_json::to_string_pretty(&payload).context("serialize physics prediction payload")?;
    let hash = hash_bytes(payload_json.as_bytes());
    Ok(WebPhysicsPrediction { payload, hash })
}

fn build_ability_catalog(file: &game_core::combat::skill::AbilityFile) -> WebAbilityCatalog {
    use game_core::combat::skill::AbilityAction;

    let abilities = file
        .abilities
        .iter()
        .map(|data| {
            let timeline = file
                .timelines
                .iter()
                .find(|timeline| timeline.ability_id == data.ability_id);

            let cooldown_ticks = timeline
                .and_then(|timeline| {
                    timeline
                        .actions
                        .iter()
                        .find_map(|scheduled| match &scheduled.action {
                            AbilityAction::CooldownStart { duration_ticks } => {
                                Some(*duration_ticks)
                            }
                            _ => None,
                        })
                })
                .unwrap_or(0);

            let timeline_duration_ticks = timeline
                .and_then(|timeline| {
                    timeline
                        .actions
                        .iter()
                        .map(|scheduled| scheduled.tick_offset.saturating_add(1))
                        .max()
                })
                .unwrap_or(0);

            let hitbox_spawn_tick = timeline.and_then(|timeline| {
                timeline
                    .actions
                    .iter()
                    .find_map(|scheduled| match &scheduled.action {
                        AbilityAction::SpawnHitbox { .. }
                        | AbilityAction::SpawnConfiguredHitbox { .. } => {
                            Some(scheduled.tick_offset)
                        }
                        _ => None,
                    })
            });

            let damage_frame_tick = timeline.and_then(|timeline| {
                timeline
                    .actions
                    .iter()
                    .find_map(|scheduled| match &scheduled.action {
                        AbilityAction::ApplyDamageFrame => Some(scheduled.tick_offset),
                        _ => None,
                    })
            });

            let hitbox_remove_tick = timeline.and_then(|timeline| {
                timeline
                    .actions
                    .iter()
                    .find_map(|scheduled| match &scheduled.action {
                        AbilityAction::RemoveHitbox => Some(scheduled.tick_offset),
                        _ => None,
                    })
            });

            let first_spawn = timeline.and_then(|timeline| {
                timeline
                    .actions
                    .iter()
                    .find_map(|scheduled| match &scheduled.action {
                        AbilityAction::SpawnHitbox { shape, offset } => {
                            Some((*shape, [offset.x, offset.y, offset.z], None))
                        }
                        AbilityAction::SpawnConfiguredHitbox {
                            shape,
                            offset,
                            rules,
                            ..
                        } => Some((*shape, [offset.x, offset.y, offset.z], *rules)),
                        _ => None,
                    })
            });

            let linger_ticks = timeline
                .map(|timeline| {
                    let spawn_tick = timeline
                        .actions
                        .iter()
                        .find_map(|scheduled| match &scheduled.action {
                            AbilityAction::SpawnHitbox { .. }
                            | AbilityAction::SpawnConfiguredHitbox { .. } => {
                                Some(scheduled.tick_offset)
                            }
                            _ => None,
                        })
                        .unwrap_or(0);
                    let remove_tick = timeline
                        .actions
                        .iter()
                        .find_map(|scheduled| match &scheduled.action {
                            AbilityAction::RemoveHitbox => Some(scheduled.tick_offset),
                            _ => None,
                        })
                        .unwrap_or(0);
                    remove_tick.saturating_sub(spawn_tick)
                })
                .unwrap_or(0);

            let (preview_shape, offset, hitbox_rules) = first_spawn
                .map(|(shape, offset, rules)| (preview_shape_for(shape), offset, rules))
                .unwrap_or_else(|| (preview_shape_for(data.shape), [0.0, 0.0, 0.0], None));

            WebAbilityMetadata {
                ability_id: data.ability_id,
                name: data.name.clone(),
                base_damage: data.base_damage,
                damage_type: damage_type_label(data.damage_type).to_string(),
                targeting_mode: targeting_mode_metadata(data.targeting_mode),
                cast_facing_policy: cast_facing_policy_label(data.cast_facing_policy).to_string(),
                max_range: data.max_range,
                projectile_speed: data.projectile_speed,
                cooldown_ticks,
                linger_ticks,
                damage_interval_ticks: hitbox_rules
                    .map(|rules| rules.damage_interval_ticks)
                    .unwrap_or(data.damage_interval_ticks),
                timeline_duration_ticks,
                hitbox_spawn_tick,
                damage_frame_tick,
                hitbox_remove_tick,
                lock_on_timeout_ticks: data.lock_on_timeout_ticks,
                charge_tiers: data
                    .charge_tiers
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|tier| WebChargeTierMetadata {
                        min_ticks: tier.min_ticks,
                        damage_mult: tier.damage_mult,
                    })
                    .collect(),
                preview_shape,
                offset,
            }
        })
        .collect();

    WebAbilityCatalog {
        schema_version: 1,
        tick_rate: 20,
        abilities,
    }
}

fn preview_shape_for(shape: game_core::combat::skill::SkillShape) -> WebPreviewShapeMetadata {
    use game_core::combat::skill::SkillShape;
    match shape {
        SkillShape::CapsuleSweep => WebPreviewShapeMetadata::Capsule {
            radius: 0.75,
            half_height: 1.0,
        },
        SkillShape::LineSweep => WebPreviewShapeMetadata::Capsule {
            radius: 0.5,
            half_height: 3.0,
        },
        SkillShape::Cone => WebPreviewShapeMetadata::Capsule {
            radius: 0.75,
            half_height: 1.0,
        },
        SkillShape::Sphere => WebPreviewShapeMetadata::Sphere { radius: 2.0 },
        SkillShape::HazardZone => WebPreviewShapeMetadata::Sphere { radius: 5.0 },
        SkillShape::Projectile => WebPreviewShapeMetadata::None,
    }
}

fn targeting_mode_metadata(
    mode: game_core::combat::skill::TargetingMode,
) -> WebTargetingModeMetadata {
    use game_core::combat::skill::TargetingMode;
    match mode {
        TargetingMode::DirectionTarget => WebTargetingModeMetadata::DirectionTarget,
        TargetingMode::EntityTarget => WebTargetingModeMetadata::EntityTarget,
        TargetingMode::GroundTarget => WebTargetingModeMetadata::GroundTarget,
        TargetingMode::RaycastStrict => WebTargetingModeMetadata::RaycastStrict,
        TargetingMode::AimAssist => WebTargetingModeMetadata::AimAssist,
        TargetingMode::LockOn { max_targets } => WebTargetingModeMetadata::LockOn { max_targets },
        TargetingMode::SelfOnly => WebTargetingModeMetadata::SelfOnly,
        TargetingMode::CasterOffset => WebTargetingModeMetadata::CasterOffset,
    }
}

fn damage_type_label(damage_type: game_schema::DamageType) -> &'static str {
    match damage_type {
        game_schema::DamageType::Physical => "physical",
        game_schema::DamageType::Magical => "magical",
        game_schema::DamageType::True => "true",
    }
}

fn cast_facing_policy_label(policy: game_core::combat::skill::CastFacingPolicy) -> &'static str {
    use game_core::combat::skill::CastFacingPolicy;
    match policy {
        CastFacingPolicy::PreserveBody => "preserve_body",
        CastFacingPolicy::FaceAimDirection => "face_aim_direction",
        CastFacingPolicy::FaceResolvedTarget => "face_resolved_target",
    }
}

fn build_map_bundle_outputs(
    metadata: &WebContentMetadata,
    content_hash: &str,
    repo_root: &Path,
) -> Result<(MapBundleIndex, MapBundleModule, Vec<MapBundleFile>)> {
    let mut index_entries = Vec::new();
    let mut module_entries = Vec::new();
    let mut files: Vec<MapBundleFile> = Vec::new();

    for layer in &metadata.layers {
        let slug = sanitize_bundle_part(&format!("layer-{}-{}", layer.layer_id, layer.name));
        let bundle_id = slug.clone();
        let source = MapBundleSource {
            kind: "layer".to_string(),
            name: layer.name.clone(),
            layer_id: Some(layer.layer_id),
            dungeon_template_id: None,
            terrain_set: layer.terrain_set.clone(),
        };
        let colliders = collider_bundle_for_geometries(
            content_hash,
            source.clone(),
            &layer.geometry,
            &format!("layer-{}", layer.layer_id),
        );
        let visual_stem = layer
            .client_visual
            .clone()
            .or_else(|| layer.terrain_set.clone());
        let visual = collect_visual_assets(repo_root, visual_stem.as_deref(), &bundle_id)?;
        let (manifest, collider_json) =
            build_single_map_bundle(&bundle_id, content_hash, source, &colliders, &visual)?;
        let manifest_path = format!("{bundle_id}/manifest.json");
        let collider_path = format!("{bundle_id}/colliders/static-colliders.json");
        index_entries.push(MapBundleIndexEntry {
            bundle_id: bundle_id.clone(),
            source_kind: "layer".to_string(),
            name: layer.name.clone(),
            layer_id: Some(layer.layer_id),
            dungeon_template_id: None,
            terrain_set: layer.terrain_set.clone(),
            manifest_path: manifest_path.clone(),
            collider_count: colliders.colliders.len(),
            content_hash: content_hash.to_string(),
        });
        module_entries.push(MapBundleModuleEntry {
            bundle_id,
            manifest,
            colliders,
        });
        files.push(MapBundleFile::text(
            manifest_path,
            serde_json::to_string_pretty(&module_entries.last().unwrap().manifest)?,
        ));
        files.push(MapBundleFile::text(collider_path, collider_json));
        if let Some(visual) = visual {
            files.extend(visual.files);
        }
    }

    for template in &metadata.dungeon_templates {
        let slug = sanitize_bundle_part(&format!("dungeon-{}", template.template_id));
        let bundle_id = slug.clone();
        let source = MapBundleSource {
            kind: "dungeon".to_string(),
            name: template.name.clone(),
            layer_id: None,
            dungeon_template_id: Some(template.template_id.clone()),
            terrain_set: template.terrain_set.clone(),
        };
        let colliders = collider_bundle_for_geometries(
            content_hash,
            source.clone(),
            &template.geometry,
            &format!("dungeon-{}", template.template_id),
        );
        // Dungeon templates only carry `terrain_set` today (no separate
        // `client_visual` override); reuse the same stem for both physics and
        // visual lookup.
        let visual = collect_visual_assets(repo_root, template.terrain_set.as_deref(), &bundle_id)?;
        let (manifest, collider_json) =
            build_single_map_bundle(&bundle_id, content_hash, source, &colliders, &visual)?;
        let manifest_path = format!("{bundle_id}/manifest.json");
        let collider_path = format!("{bundle_id}/colliders/static-colliders.json");
        index_entries.push(MapBundleIndexEntry {
            bundle_id: bundle_id.clone(),
            source_kind: "dungeon".to_string(),
            name: template.name.clone(),
            layer_id: None,
            dungeon_template_id: Some(template.template_id.clone()),
            terrain_set: template.terrain_set.clone(),
            manifest_path: manifest_path.clone(),
            collider_count: colliders.colliders.len(),
            content_hash: content_hash.to_string(),
        });
        module_entries.push(MapBundleModuleEntry {
            bundle_id,
            manifest,
            colliders,
        });
        files.push(MapBundleFile::text(
            manifest_path,
            serde_json::to_string_pretty(&module_entries.last().unwrap().manifest)?,
        ));
        files.push(MapBundleFile::text(collider_path, collider_json));
        if let Some(visual) = visual {
            files.extend(visual.files);
        }
    }

    Ok((
        MapBundleIndex {
            schema_version: 1,
            bundles: index_entries,
        },
        MapBundleModule {
            schema_version: 1,
            bundles: module_entries,
        },
        files,
    ))
}

fn build_single_map_bundle(
    bundle_id: &str,
    content_hash: &str,
    source: MapBundleSource,
    colliders: &ColliderBundle,
    visual: &Option<VisualBundle>,
) -> Result<(MapBundleManifest, String)> {
    let collider_json =
        serde_json::to_string_pretty(&colliders).context("serialize collider bundle")?;
    let collider_asset = AssetRef {
        url: "colliders/static-colliders.json".to_string(),
        sha256: hash_bytes(collider_json.as_bytes()),
        bytes: collider_json.len(),
    };
    let debug_markers = colliders
        .colliders
        .first()
        .map(|collider| DebugMarker {
            kind: "origin".to_string(),
            label: format!("{} origin", source.name),
            position: collider.position,
        })
        .into_iter()
        .collect();

    let (render_meshes, visual_content_hash) = match visual {
        Some(visual) => (
            visual.render_meshes.clone(),
            Some(visual.content_hash.clone()),
        ),
        None => (Vec::new(), None),
    };

    Ok((
        MapBundleManifest {
            schema_version: 1,
            bundle_version: "local-v1".to_string(),
            content_hash: content_hash.to_string(),
            visual_content_hash,
            bundle_id: bundle_id.to_string(),
            source,
            render_meshes,
            collider_json: vec![collider_asset],
            debug_markers,
        },
        collider_json,
    ))
}

fn collider_bundle_for_geometries(
    content_hash: &str,
    source: MapBundleSource,
    geometries: &[GeometryMetadata],
    id_prefix: &str,
) -> ColliderBundle {
    let colliders = geometries
        .iter()
        .enumerate()
        .map(|(idx, geometry)| BundleCollider {
            collider_id: format!("{id_prefix}-collider-{idx}"),
            position: geometry.position,
            rotation: [0.0, 0.0, 0.0, 1.0],
            shape: geometry.shape.clone(),
        })
        .collect();

    ColliderBundle {
        format_version: WEB_MAP_BUNDLE_COLLIDER_FORMAT_VERSION,
        content_hash: content_hash.to_string(),
        coordinate_system: "right_handed_y_up".to_string(),
        source,
        colliders,
    }
}

fn sanitize_bundle_part(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_dash = false;
    for ch in value.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            last_was_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if ch == '_' || ch == '-' {
            if last_was_dash {
                None
            } else {
                last_was_dash = true;
                Some('-')
            }
        } else if last_was_dash {
            None
        } else {
            last_was_dash = true;
            Some('-')
        };
        if let Some(ch) = next {
            out.push(ch);
        }
    }
    out.trim_matches('-').to_string()
}

/// File payload emitted by the map-bundle builder. Text payloads (JSON) and
/// binary payloads (glTF, .bin, textures) both flow through one channel so the
/// caller can write them in a single loop.
enum MapBundleFile {
    Text {
        relative_path: String,
        content: String,
    },
    Binary {
        relative_path: String,
        content: Vec<u8>,
    },
}

impl MapBundleFile {
    fn text(relative_path: String, content: String) -> Self {
        Self::Text {
            relative_path,
            content,
        }
    }

    fn binary(relative_path: String, content: Vec<u8>) -> Self {
        Self::Binary {
            relative_path,
            content,
        }
    }
}

/// One bundle's worth of resolved visual assets.
///
/// `render_meshes` lists every file that ships under `<bundle_id>/visual/...`
/// with a per-file SHA-256. `content_hash` is a stable hash over the URL list
/// + each file's SHA-256, so visual revisions surface as a single field on the
/// manifest without bloating it.
struct VisualBundle {
    render_meshes: Vec<AssetRef>,
    content_hash: String,
    files: Vec<MapBundleFile>,
}

const VISUAL_ASSET_ROOT: &str = "crates/game_client_bevy/assets/terrain";

/// Probe `crates/game_client_bevy/assets/terrain/{stem}/` for a glTF/glb +
/// siblings and return a `VisualBundle` ready to embed into a map bundle.
///
/// Returns `Ok(None)` when no `stem` is configured or the on-disk directory
/// does not contain an entry point (`{stem}.gltf` or `{stem}.glb`). This is
/// the common case today — most layers/dungeons have no imported visual mesh.
fn collect_visual_assets(
    repo_root: &Path,
    stem: Option<&str>,
    bundle_id: &str,
) -> Result<Option<VisualBundle>> {
    let Some(stem) = stem else {
        return Ok(None);
    };
    let stem_dir = repo_root.join(VISUAL_ASSET_ROOT).join(stem);
    if !stem_dir.is_dir() {
        return Ok(None);
    }

    let gltf_path = stem_dir.join(format!("{stem}.gltf"));
    let glb_path = stem_dir.join(format!("{stem}.glb"));
    let entry_point = if gltf_path.is_file() {
        gltf_path
    } else if glb_path.is_file() {
        glb_path
    } else {
        return Ok(None);
    };
    let entry_relative = entry_point
        .strip_prefix(&stem_dir)
        .with_context(|| format!("entry point not under {}", stem_dir.display()))?
        .to_path_buf();

    let mut render_meshes = Vec::new();
    let mut files = Vec::new();
    let mut hash_inputs: Vec<(String, String)> = Vec::new();

    for relative in collect_files_relative(&stem_dir)? {
        let absolute = stem_dir.join(&relative);
        let bytes = fs::read(&absolute)
            .with_context(|| format!("reading visual asset {}", absolute.display()))?;
        let sha256 = hash_bytes(&bytes);
        let url = format!("visual/{}", relative.to_string_lossy().replace('\\', "/"));
        let bundle_relative = format!("{bundle_id}/{url}");
        render_meshes.push(AssetRef {
            url: url.clone(),
            sha256: sha256.clone(),
            bytes: bytes.len(),
        });
        hash_inputs.push((url, sha256));
        files.push(MapBundleFile::binary(bundle_relative, bytes));
    }

    // Stable order: entry point first (`.gltf` / `.glb`), then siblings sorted
    // by URL so the manifest is deterministic across filesystems.
    let entry_url = format!(
        "visual/{}",
        entry_relative.to_string_lossy().replace('\\', "/")
    );
    render_meshes.sort_by(|a, b| {
        let a_is_entry = a.url == entry_url;
        let b_is_entry = b.url == entry_url;
        match (a_is_entry, b_is_entry) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.url.cmp(&b.url),
        }
    });
    hash_inputs.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"dive-visual-v1");
    hasher.update([0u8]);
    hasher.update(
        entry_relative
            .to_string_lossy()
            .replace('\\', "/")
            .as_bytes(),
    );
    hasher.update([0u8]);
    for (url, sha) in &hash_inputs {
        hasher.update(url.as_bytes());
        hasher.update([0u8]);
        hasher.update(sha.as_bytes());
        hasher.update([0u8]);
    }
    let content_hash = hex_digest(&hasher.finalize());

    Ok(Some(VisualBundle {
        render_meshes,
        content_hash,
        files,
    }))
}

fn export_layer_metadata(layer: &game_schema::dungeon::WorldLayerDef) -> LayerMetadata {
    LayerMetadata {
        layer_id: layer.layer_id,
        name: layer.name.clone(),
        terrain_set: layer.terrain_set.clone(),
        client_visual: layer.client_visual.clone(),
        collision_policy: LayerCollisionPolicyMetadata {
            player_collides_player: layer.collision_policy.player_collides_player,
        },
        spawn_points: layer.spawn_points.clone(),
        geometry: layer
            .geometry
            .iter()
            .map(export_geometry_metadata)
            .collect(),
    }
}

fn export_dungeon_template_metadata(
    template: &game_schema::dungeon::DungeonTemplate,
) -> DungeonTemplateMetadata {
    DungeonTemplateMetadata {
        template_id: template.template_id.clone(),
        name: template.name.clone(),
        max_players: template.max_players,
        terrain_set: template.terrain_set.clone(),
        collision_policy: LayerCollisionPolicyMetadata {
            player_collides_player: template.collision_policy.player_collides_player,
        },
        spawn_points: template.spawn_points.clone(),
        exit_points: template.exit_points.clone(),
        geometry: template
            .geometry
            .iter()
            .map(export_geometry_metadata)
            .collect(),
        interactables: template
            .interactables
            .iter()
            .map(export_interactable_metadata)
            .collect(),
    }
}

fn export_geometry_metadata(geometry: &game_schema::dungeon::GeometryDef) -> GeometryMetadata {
    GeometryMetadata {
        position: geometry.position,
        shape: export_shape_metadata(&geometry.shape),
    }
}

fn export_shape_metadata(shape: &game_schema::dungeon::ShapeDef) -> ShapeMetadata {
    match shape {
        game_schema::dungeon::ShapeDef::Cuboid {
            half_x,
            half_y,
            half_z,
        } => ShapeMetadata::Cuboid {
            half_x: *half_x,
            half_y: *half_y,
            half_z: *half_z,
        },
        game_schema::dungeon::ShapeDef::Cylinder {
            half_height,
            radius,
        } => ShapeMetadata::Cylinder {
            half_height: *half_height,
            radius: *radius,
        },
        game_schema::dungeon::ShapeDef::Heightfield {
            nrows,
            ncols,
            scale_x,
            scale_y,
            scale_z,
            heights,
        } => ShapeMetadata::Heightfield {
            nrows: *nrows,
            ncols: *ncols,
            scale_x: *scale_x,
            scale_y: *scale_y,
            scale_z: *scale_z,
            heights: heights.clone(),
        },
        game_schema::dungeon::ShapeDef::TriMesh { vertices, indices } => ShapeMetadata::TriMesh {
            vertices: vertices.clone(),
            indices: indices.clone(),
        },
    }
}

fn export_interactable_metadata(
    interactable: &game_schema::dungeon::InteractableDef,
) -> InteractableMetadata {
    InteractableMetadata {
        local_id: interactable.local_id,
        script_id: interactable.script_id.clone(),
        tags: interactable.tags.clone(),
        kind: export_interactable_kind_metadata(&interactable.kind),
        position: interactable.position,
        linked_to: interactable.linked_to,
        required_buff: interactable.required_buff,
        required_item: interactable.required_item,
        interact_range: interactable.interact_range,
        puzzle_group: interactable.puzzle_group.clone(),
        puzzle_required_count: interactable.puzzle_required_count,
        puzzle_window_ticks: interactable.puzzle_window_ticks,
        body_shape: interactable.body_shape.map(export_body_shape_metadata),
    }
}

fn export_interactable_kind_metadata(
    kind: &game_schema::dungeon::InteractKindDef,
) -> InteractableKindMetadata {
    match kind {
        game_schema::dungeon::InteractKindDef::Gate => InteractableKindMetadata::Gate,
        game_schema::dungeon::InteractKindDef::Switch => InteractableKindMetadata::Switch,
        game_schema::dungeon::InteractKindDef::Chest => InteractableKindMetadata::Chest,
        game_schema::dungeon::InteractKindDef::BossSpawn {
            npc_name,
            encounter_name,
            archetype_id,
        } => InteractableKindMetadata::BossSpawn {
            npc_name: npc_name.clone(),
            encounter_name: encounter_name.clone(),
            archetype_id: archetype_id.clone(),
        },
        game_schema::dungeon::InteractKindDef::NpcSpawn {
            npc_name,
            archetype_id,
        } => InteractableKindMetadata::NpcSpawn {
            npc_name: npc_name.clone(),
            archetype_id: archetype_id.clone(),
        },
    }
}

fn export_body_shape_metadata(shape: game_schema::dungeon::BodyShapeDef) -> BodyShapeMetadata {
    BodyShapeMetadata {
        id: shape.to_u8(),
        name: body_shape_name(shape).to_string(),
    }
}

fn body_shape_name(shape: game_schema::dungeon::BodyShapeDef) -> &'static str {
    match shape {
        game_schema::dungeon::BodyShapeDef::PlayerCapsule => "PlayerCapsule",
        game_schema::dungeon::BodyShapeDef::NpcCapsule => "NpcCapsule",
        game_schema::dungeon::BodyShapeDef::BossCapsule => "BossCapsule",
        game_schema::dungeon::BodyShapeDef::LargeBossCapsule => "LargeBossCapsule",
        game_schema::dungeon::BodyShapeDef::GateCuboid => "GateCuboid",
        game_schema::dungeon::BodyShapeDef::SwitchCuboid => "SwitchCuboid",
        game_schema::dungeon::BodyShapeDef::ChestCuboid => "ChestCuboid",
        game_schema::dungeon::BodyShapeDef::CrateCuboid => "CrateCuboid",
    }
}

fn dev_web_contract(args: WebContractArgs) -> Result<()> {
    if !args.skip_schema {
        println!("Refreshing schema before web-contract generation...");
        dev_schema()?;
    }

    let out_dir = args.out;
    let staging_dir = staging_dir_for(&out_dir);

    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)
            .with_context(|| format!("failed removing staging dir {}", staging_dir.display()))?;
    }
    fs::create_dir_all(&staging_dir)
        .with_context(|| format!("failed creating staging dir {}", staging_dir.display()))?;

    let bindings_dir = staging_dir.join(WEB_CONTRACT_BINDINGS_DIR);
    fs::create_dir_all(&bindings_dir)
        .with_context(|| format!("failed creating bindings dir {}", bindings_dir.display()))?;
    run_spacetime_generate_typescript(&bindings_dir)?;

    let schema_hash = hash_directory(&bindings_dir)?;
    let content_hash = hash_content_files(WEB_CONTENT_SOURCES)?;

    let world_layers: game_schema::dungeon::WorldLayersFile =
        load_ron_file(Path::new("data/layers.ron"))?;
    let dungeons: game_schema::dungeon::DungeonFile =
        load_ron_file(Path::new("data/dungeons.ron"))?;
    let ability_file: game_core::combat::skill::AbilityFile =
        load_ron_file(Path::new("data/abilities.ron"))?;
    let ability_catalog = build_ability_catalog(&ability_file);
    let content_metadata = build_content_metadata(&world_layers, &dungeons);
    let content_lookup = build_content_lookup(&content_metadata);
    let browser_policy = build_browser_policy();
    let physics_prediction = build_physics_prediction()?;
    let (map_bundle_index, map_bundle_module, map_bundle_files) =
        build_map_bundle_outputs(&content_metadata, &content_hash, Path::new("."))?;
    let abilities_json =
        serde_json::to_string_pretty(&ability_catalog).context("serialize ability catalog")?;
    let content_metadata_json =
        serde_json::to_string_pretty(&content_metadata).context("serialize content metadata")?;
    let content_lookup_json =
        serde_json::to_string_pretty(&content_lookup).context("serialize content lookup")?;
    let browser_policy_json =
        serde_json::to_string_pretty(&browser_policy).context("serialize browser policy")?;
    let physics_prediction_json = serde_json::to_string_pretty(&physics_prediction)
        .context("serialize physics prediction")?;
    let map_bundle_index_json =
        serde_json::to_string_pretty(&map_bundle_index).context("serialize map bundle index")?;
    let map_bundle_module_json =
        serde_json::to_string_pretty(&map_bundle_module).context("serialize map bundle module")?;
    let map_bundle_module_ts = format!(
        "export const mapBundles = {map_bundle_module_json} as const;\nexport default mapBundles;\n"
    );
    let metadata_hash = hash_bytes(format!("{abilities_json}\n{content_metadata_json}").as_bytes());

    let source_files = WEB_CONTENT_SOURCES
        .iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();

    let manifest = WebContractManifest {
        package_name: WEB_CONTRACT_PACKAGE_NAME.to_string(),
        contract_version: env!("CARGO_PKG_VERSION").to_string(),
        schema_hash,
        content_hash: content_hash.clone(),
        metadata_hash,
        physics_hash: physics_prediction.hash.clone(),
        bindings_dir: WEB_CONTRACT_BINDINGS_DIR.to_string(),
        browser_policy_file: WEB_BROWSER_POLICY_FILE.to_string(),
        abilities_file: WEB_ABILITIES_FILE.to_string(),
        physics_prediction_file: WEB_PHYSICS_PREDICTION_FILE.to_string(),
        content_metadata_file: WEB_CONTENT_METADATA_FILE.to_string(),
        content_lookup_file: WEB_CONTENT_LOOKUP_FILE.to_string(),
        map_bundles_dir: WEB_MAP_BUNDLES_DIR.to_string(),
        layer_count: world_layers.layers.len(),
        dungeon_template_count: dungeons.templates.len(),
        source_files,
    };

    let mut exports = BTreeMap::new();
    exports.insert("./bindings".to_string(), "./bindings/index.ts".to_string());
    exports.insert(
        "./bindings/types".to_string(),
        "./bindings/types.ts".to_string(),
    );
    exports.insert(
        "./bindings/types/*".to_string(),
        "./bindings/types/*".to_string(),
    );
    exports.insert(
        "./browser-policy.json".to_string(),
        "./browser-policy.json".to_string(),
    );
    exports.insert(
        "./abilities.json".to_string(),
        "./abilities.json".to_string(),
    );
    exports.insert(
        "./physics-prediction.json".to_string(),
        "./physics-prediction.json".to_string(),
    );
    exports.insert("./contract.json".to_string(), "./contract.json".to_string());
    exports.insert(
        "./content-metadata.json".to_string(),
        "./content-metadata.json".to_string(),
    );
    exports.insert(
        "./content-lookup.json".to_string(),
        "./content-lookup.json".to_string(),
    );
    exports.insert(
        "./map-bundles".to_string(),
        "./map-bundles/index.ts".to_string(),
    );
    exports.insert(
        "./map-bundles/index.json".to_string(),
        "./map-bundles/index.json".to_string(),
    );
    exports.insert("./map-bundles/*".to_string(), "./map-bundles/*".to_string());

    let mut dependencies = BTreeMap::new();
    dependencies.insert(
        "spacetimedb".to_string(),
        WEB_CONTRACT_STDB_NPM_VERSION.to_string(),
    );

    let package_json = WebContractPackageJson {
        name: WEB_CONTRACT_PACKAGE_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        private: true,
        package_type: "module".to_string(),
        description: "Generated Dive browser contract artifacts".to_string(),
        main: format!("{WEB_CONTRACT_BINDINGS_DIR}/index.ts"),
        files: vec![
            WEB_CONTRACT_BINDINGS_DIR.to_string(),
            WEB_MAP_BUNDLES_DIR.to_string(),
            WEB_BROWSER_POLICY_FILE.to_string(),
            WEB_ABILITIES_FILE.to_string(),
            WEB_PHYSICS_PREDICTION_FILE.to_string(),
            "contract.json".to_string(),
            WEB_CONTENT_METADATA_FILE.to_string(),
            WEB_CONTENT_LOOKUP_FILE.to_string(),
            "README.md".to_string(),
        ],
        exports,
        dependencies,
    };

    write_text_file(&staging_dir.join(WEB_ABILITIES_FILE), &abilities_json)?;
    write_text_file(
        &staging_dir.join(WEB_CONTENT_METADATA_FILE),
        &content_metadata_json,
    )?;
    write_text_file(
        &staging_dir.join(WEB_CONTENT_LOOKUP_FILE),
        &content_lookup_json,
    )?;
    write_text_file(
        &staging_dir.join(WEB_BROWSER_POLICY_FILE),
        &browser_policy_json,
    )?;
    write_text_file(
        &staging_dir.join(WEB_PHYSICS_PREDICTION_FILE),
        &physics_prediction_json,
    )?;
    let map_bundles_dir = staging_dir.join(WEB_MAP_BUNDLES_DIR);
    write_text_file(&map_bundles_dir.join("index.json"), &map_bundle_index_json)?;
    write_text_file(&map_bundles_dir.join("index.ts"), &map_bundle_module_ts)?;
    for file in map_bundle_files {
        match file {
            MapBundleFile::Text {
                relative_path,
                content,
            } => {
                write_text_file(&map_bundles_dir.join(relative_path), &content)?;
            }
            MapBundleFile::Binary {
                relative_path,
                content,
            } => {
                write_binary_file(&map_bundles_dir.join(relative_path), &content)?;
            }
        }
    }

    write_text_file(
        &staging_dir.join("contract.json"),
        &serde_json::to_string_pretty(&manifest).context("serialize contract manifest")?,
    )?;
    write_text_file(
        &staging_dir.join("package.json"),
        &serde_json::to_string_pretty(&package_json).context("serialize package.json")?,
    )?;
    write_text_file(
        &staging_dir.join("README.md"),
        "Generated by `cargo xtask dev web-contract`.\nContains TypeScript bindings, browser policy, deterministic contract metadata, exported ability/layer/dungeon content artifacts, Rapier prediction metadata, and local map-bundle fixtures.\n",
    )?;

    if args.check {
        if !out_dir.exists() {
            fs::remove_dir_all(&staging_dir).ok();
            bail!(
                "web-contract output not found at {}. Run `cargo xtask dev web-contract` first.",
                out_dir.display()
            );
        }

        let expected_hash = hash_directory(&staging_dir)?;
        let current_hash = hash_directory(&out_dir)?;
        fs::remove_dir_all(&staging_dir).ok();

        if expected_hash != current_hash {
            bail!(
                "web-contract output at {} is stale. Run `cargo xtask dev web-contract`.",
                out_dir.display()
            );
        }

        println!("Web contract output is up to date: {}", out_dir.display());
        return Ok(());
    }

    if out_dir.exists() {
        fs::remove_dir_all(&out_dir)
            .with_context(|| format!("failed removing output dir {}", out_dir.display()))?;
    }
    if let Some(parent) = out_dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating output parent {}", parent.display()))?;
    }
    fs::rename(&staging_dir, &out_dir).with_context(|| {
        format!(
            "failed moving staging output {} -> {}",
            staging_dir.display(),
            out_dir.display()
        )
    })?;

    println!("Generated web contract at {}", out_dir.display());
    Ok(())
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
    println!("terrain_set '{}' → id={}", args.set_name, resolved_set_id);

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
        bail!("reducer `{reducer}` HTTP call failed ({status_line}):\n{body}");
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

fn staging_dir_for(out_dir: &Path) -> PathBuf {
    let parent = out_dir.parent().unwrap_or_else(|| Path::new("."));
    let stem = out_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("web-contract");
    parent.join(format!("{stem}.staging"))
}

fn run_spacetime_generate_typescript(out_dir: &Path) -> Result<()> {
    let mut generate = Command::new("spacetime");
    generate.args(["generate", "--lang", "typescript", "--out-dir"]);
    generate.arg(out_dir);
    generate.args(["--module-path", MODULE_PATH]);
    run_command(generate)
}

fn hash_content_files(paths: &[&str]) -> Result<String> {
    let mut hasher = Sha256::new();
    for path in paths {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        let bytes = fs::read(path).with_context(|| format!("reading {path}"))?;
        hasher.update(&bytes);
        hasher.update([0u8]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(&hasher.finalize())
}

fn hash_directory(root: &Path) -> Result<String> {
    if !root.exists() {
        bail!("directory does not exist: {}", root.display());
    }

    let mut hasher = Sha256::new();
    for rel in collect_files_relative(root)? {
        let full = root.join(&rel);
        let rel_norm = rel.to_string_lossy().replace('\\', "/");
        hasher.update(rel_norm.as_bytes());
        hasher.update([0u8]);
        let bytes = fs::read(&full).with_context(|| format!("reading {}", full.display()))?;
        hasher.update(&bytes);
        hasher.update([0u8]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn collect_files_relative(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_recursive(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files_recursive(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, std::io::Error>>()
        .with_context(|| format!("collecting entries from {}", dir.display()))?;

    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(root, &path, out)?;
            continue;
        }
        if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .with_context(|| {
                    format!(
                        "failed computing relative path: {} from {}",
                        path.display(),
                        root.display()
                    )
                })?
                .to_path_buf();
            out.push(rel);
        }
    }
    Ok(())
}

fn load_ron_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&raw).with_context(|| format!("parsing RON {}", path.display()))
}

fn write_text_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating parent {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn write_binary_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating parent {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Deserialize)]
struct ItemFile {
    items: Vec<game_core::stats::ItemData>,
}

#[derive(Default)]
struct ContentCheck {
    errors: Vec<String>,
    warnings: Vec<String>,
    ability_ids: BTreeSet<u32>,
    buff_ids: BTreeSet<u32>,
    item_ids: BTreeSet<u32>,
    loot_table_ids: BTreeSet<String>,
    behavior_tree_ids: BTreeSet<String>,
    route_ids: BTreeSet<String>,
    archetype_kinds: BTreeMap<String, game_schema::EntityKind>,
    archetype_usage: BTreeMap<String, game_schema::ArchetypeUsage>,
    encounter_ids: BTreeSet<String>,
    dungeon_template_ids: BTreeSet<String>,
}

impl ContentCheck {
    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(message.into());
    }

    fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    fn require_u32(&mut self, known: &BTreeSet<u32>, id: u32, kind: &str, owner: &str) {
        if !known.contains(&id) {
            self.error(format!("{owner} references unknown {kind} {id}"));
        }
    }

    fn require_str(&mut self, known: &BTreeSet<String>, id: &str, kind: &str, owner: &str) {
        if !known.contains(id) {
            self.error(format!("{owner} references unknown {kind} '{id}'"));
        }
    }
}

fn dev_content_check() -> Result<()> {
    let ability_file: game_core::combat::skill::AbilityFile =
        load_ron_file(Path::new("data/abilities.ron"))?;
    let buff_file: game_core::combat::status::BuffFile =
        load_ron_file(Path::new("data/buffs.ron"))?;
    let item_file: ItemFile = load_ron_file(Path::new("data/items.ron"))?;
    let loot_file: game_core::loot::LootTablesFile =
        load_ron_file(Path::new("data/loot_tables.ron"))?;
    let behavior_tree_file: game_core::ai::behavior_tree::BehaviorTreeFile =
        load_ron_file(Path::new("data/behavior_trees.ron"))?;
    let route_file: game_core::ai::routes::RouteFile =
        load_ron_file(Path::new("data/npc_routes.ron"))?;
    let archetype_file: game_schema::NpcArchetypeFile =
        load_ron_file(Path::new("data/npc_archetypes.ron"))?;
    let encounter_file: game_core::encounter::EncounterFile =
        load_ron_file(Path::new("data/encounters.ron"))?;
    let dungeon_file: game_schema::dungeon::DungeonFile =
        load_ron_file(Path::new("data/dungeons.ron"))?;
    let spawn_file: game_schema::spawn::SpawnFile =
        load_ron_file(Path::new("data/spawn_rules.ron"))?;

    let mut check = ContentCheck::default();
    collect_content_ids(
        &mut check,
        &ability_file,
        &buff_file,
        &item_file,
        &loot_file,
        &behavior_tree_file,
        &route_file,
        &archetype_file,
        &encounter_file,
        &dungeon_file,
    );

    validate_abilities(&mut check, &ability_file);
    validate_loot_tables(&mut check, &loot_file);
    validate_behavior_trees(&mut check, &behavior_tree_file);
    validate_archetypes(&mut check, &archetype_file);
    validate_encounters(&mut check, &encounter_file);
    validate_dungeons(&mut check, &dungeon_file);
    validate_spawn_rules(&mut check, &spawn_file);

    if !check.warnings.is_empty() {
        eprintln!("content-check warnings:");
        for warning in &check.warnings {
            eprintln!("  - {warning}");
        }
    }

    if !check.errors.is_empty() {
        let mut message = format!("content-check failed with {} error(s):", check.errors.len());
        for error in &check.errors {
            message.push_str("\n  - ");
            message.push_str(error);
        }
        bail!("{message}");
    }

    println!(
        "content-check ok: {} abilities, {} buffs, {} items, {} loot tables, {} behavior trees, {} routes, {} archetypes, {} encounters, {} dungeons",
        check.ability_ids.len(),
        check.buff_ids.len(),
        check.item_ids.len(),
        check.loot_table_ids.len(),
        check.behavior_tree_ids.len(),
        check.route_ids.len(),
        check.archetype_kinds.len(),
        check.encounter_ids.len(),
        check.dungeon_template_ids.len(),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_content_ids(
    check: &mut ContentCheck,
    ability_file: &game_core::combat::skill::AbilityFile,
    buff_file: &game_core::combat::status::BuffFile,
    item_file: &ItemFile,
    loot_file: &game_core::loot::LootTablesFile,
    behavior_tree_file: &game_core::ai::behavior_tree::BehaviorTreeFile,
    route_file: &game_core::ai::routes::RouteFile,
    archetype_file: &game_schema::NpcArchetypeFile,
    encounter_file: &game_core::encounter::EncounterFile,
    dungeon_file: &game_schema::dungeon::DungeonFile,
) {
    for ability in &ability_file.abilities {
        if !check.ability_ids.insert(ability.ability_id) {
            check.error(format!("duplicate ability_id {}", ability.ability_id));
        }
    }
    for buff in &buff_file.buffs {
        if !check.buff_ids.insert(buff.buff_id) {
            check.error(format!("duplicate buff_id {}", buff.buff_id));
        }
    }
    for item in &item_file.items {
        if !check.item_ids.insert(item.item_id) {
            check.error(format!("duplicate item_id {}", item.item_id));
        }
    }
    for table_id in loot_file.tables.keys() {
        if !check.loot_table_ids.insert(table_id.clone()) {
            check.error(format!("duplicate loot table id '{table_id}'"));
        }
    }
    for tree in &behavior_tree_file.trees {
        if tree.id.trim().is_empty() {
            check.error("behavior tree id must not be empty");
        } else if !check.behavior_tree_ids.insert(tree.id.clone()) {
            check.error(format!("duplicate behavior tree id '{}'", tree.id));
        }
    }
    for route in &route_file.routes {
        if route.route_id.trim().is_empty() {
            check.error("route_id must not be empty");
        } else if !check.route_ids.insert(route.route_id.clone()) {
            check.error(format!("duplicate route_id '{}'", route.route_id));
        }
    }
    for (archetype_id, archetype) in &archetype_file.archetypes {
        if archetype_id.trim().is_empty() {
            check.error("npc archetype id must not be empty");
        } else if check
            .archetype_kinds
            .insert(archetype_id.clone(), archetype.kind)
            .is_some()
        {
            check.error(format!("duplicate npc archetype id '{archetype_id}'"));
        }
        check
            .archetype_usage
            .insert(archetype_id.clone(), archetype.usage);
    }
    for encounter in &encounter_file.encounters {
        if encounter.name.trim().is_empty() {
            check.error("encounter name must not be empty");
        } else if !check.encounter_ids.insert(encounter.name.clone()) {
            check.error(format!("duplicate encounter name '{}'", encounter.name));
        }
    }
    for template in &dungeon_file.templates {
        if template.template_id.trim().is_empty() {
            check.error("dungeon template_id must not be empty");
        } else if !check
            .dungeon_template_ids
            .insert(template.template_id.clone())
        {
            check.error(format!(
                "duplicate dungeon template_id '{}'",
                template.template_id
            ));
        }
    }
}

fn validate_abilities(
    check: &mut ContentCheck,
    ability_file: &game_core::combat::skill::AbilityFile,
) {
    let mut timeline_ids = BTreeSet::new();
    for ability in &ability_file.abilities {
        let owner = format!("ability {}", ability.ability_id);
        for buff_id in &ability.on_hit_buffs {
            check.require_u32(&check.buff_ids.clone(), *buff_id, "buff_id", &owner);
        }
    }

    for timeline in &ability_file.timelines {
        let owner = format!("ability timeline {}", timeline.ability_id);
        if !timeline_ids.insert(timeline.ability_id) {
            check.error(format!(
                "duplicate ability timeline {}",
                timeline.ability_id
            ));
        }
        check.require_u32(
            &check.ability_ids.clone(),
            timeline.ability_id,
            "ability_id",
            &owner,
        );
        for action in &timeline.actions {
            validate_ability_action(
                check,
                &format!("{owner} @ tick {}", action.tick_offset),
                &action.action,
            );
        }
    }
}

fn validate_ability_action(
    check: &mut ContentCheck,
    owner: &str,
    action: &game_core::combat::skill::AbilityAction,
) {
    use game_core::combat::skill::AbilityAction;
    match action {
        AbilityAction::SpawnHitbox { .. }
        | AbilityAction::ApplyDamageFrame
        | AbilityAction::RemoveHitbox
        | AbilityAction::CooldownStart { .. }
        | AbilityAction::StanceBegin { .. }
        | AbilityAction::StanceEnd
        | AbilityAction::RootForTicks { .. }
        | AbilityAction::SetMovement { .. }
        | AbilityAction::ArcMovement { .. }
        | AbilityAction::Telegraph { .. }
        | AbilityAction::Cleanse { .. }
        | AbilityAction::ClearCC { .. }
        | AbilityAction::Stunbreak
        | AbilityAction::TeleportBehindTarget { .. }
        | AbilityAction::TeleportForward { .. } => {}
        AbilityAction::SpawnConfiguredHitbox { effect, .. } => {
            if let Some(effect) = effect.as_deref() {
                validate_hit_effect(check, &format!("{owner} configured hitbox"), effect);
            }
        }
        AbilityAction::OpenFollowUpWindow {
            next_ability_id, ..
        } => {
            check.require_u32(
                &check.ability_ids.clone(),
                *next_ability_id,
                "ability_id",
                owner,
            );
        }
        AbilityAction::ApplyBuff { buff_id } => {
            check.require_u32(&check.buff_ids.clone(), *buff_id, "buff_id", owner);
        }
    }
}

fn validate_hit_effect(
    check: &mut ContentCheck,
    owner: &str,
    effect: &game_core::combat::skill::HitEffectSpec,
) {
    for buff_id in &effect.on_hit_buffs {
        check.require_u32(&check.buff_ids.clone(), *buff_id, "buff_id", owner);
    }
    for (index, contact) in effect.on_contact.iter().enumerate() {
        match contact {
            game_core::combat::skill::HitEffectAction::SpawnHitbox { effect, .. } => {
                if let Some(effect) = effect.as_deref() {
                    validate_hit_effect(check, &format!("{owner} contact effect {index}"), effect);
                }
            }
        }
    }
}

fn validate_loot_tables(check: &mut ContentCheck, loot_file: &game_core::loot::LootTablesFile) {
    for (table_id, table) in &loot_file.tables {
        if table.rolls == 0 {
            check.error(format!("loot table '{table_id}' has rolls=0"));
        }
        if table.entries.is_empty() {
            check.error(format!("loot table '{table_id}' has no entries"));
        }
        for entry in &table.entries {
            let owner = format!("loot table '{table_id}'");
            check.require_u32(&check.item_ids.clone(), entry.item_id, "item_id", &owner);
            if entry.weight == 0 {
                check.error(format!(
                    "loot table '{table_id}' entry item_id={} has weight=0",
                    entry.item_id
                ));
            }
            if entry.min == 0 || entry.max == 0 || entry.min > entry.max {
                check.error(format!(
                    "loot table '{table_id}' entry item_id={} has invalid quantity range {}..{}",
                    entry.item_id, entry.min, entry.max
                ));
            }
        }
    }
}

fn validate_behavior_trees(
    check: &mut ContentCheck,
    behavior_tree_file: &game_core::ai::behavior_tree::BehaviorTreeFile,
) {
    for tree in &behavior_tree_file.trees {
        validate_behavior_node(check, &format!("behavior tree '{}'", tree.id), &tree.root);
    }
}

fn validate_behavior_node(
    check: &mut ContentCheck,
    owner: &str,
    node: &game_core::ai::behavior_tree::BehaviorNode,
) {
    use game_core::ai::behavior_tree::{ActionNode, BehaviorNode};
    match node {
        BehaviorNode::Selector(children) | BehaviorNode::Sequence(children) => {
            if children.is_empty() {
                check.warn(format!("{owner} contains an empty composite node"));
            }
            for (index, child) in children.iter().enumerate() {
                validate_behavior_node(check, &format!("{owner} child {index}"), child);
            }
        }
        BehaviorNode::Condition(_) => {}
        BehaviorNode::Action(action) => match action {
            ActionNode::FollowRoute(route_id) => {
                check.require_str(
                    &check.route_ids.clone(),
                    route_id.as_str(),
                    "route_id",
                    owner,
                );
            }
            ActionNode::Emit(action) => {
                check.warn(format!(
                    "{owner} uses ActionNode::Emit; prefer named high-level BT leaves for shipped content"
                ));
                validate_desired_ai_action(check, owner, action);
            }
            _ => {}
        },
        BehaviorNode::Invert(child) => {
            validate_behavior_node(check, &format!("{owner} inverted child"), child);
        }
    }
}

fn validate_desired_ai_action(
    check: &mut ContentCheck,
    owner: &str,
    action: &game_core::ai::decision::DesiredAiAction,
) {
    use game_core::ai::decision::DesiredAiAction;
    match action {
        DesiredAiAction::FollowRoute { route_id } => {
            check.require_str(
                &check.route_ids.clone(),
                route_id.as_str(),
                "route_id",
                owner,
            );
        }
        DesiredAiAction::MoveToPoint(point)
        | DesiredAiAction::EvadeHome {
            home_position: point,
        } => {
            if !point.x.is_finite() || !point.y.is_finite() || !point.z.is_finite() {
                check.error(format!("{owner} emits a non-finite AI point"));
            }
        }
        DesiredAiAction::SetState { .. }
        | DesiredAiAction::StopMovement
        | DesiredAiAction::ClearThreat
        | DesiredAiAction::MoveTowardEntity(_)
        | DesiredAiAction::MoveAwayFromEntity(_)
        | DesiredAiAction::TryCastBestAbility { .. } => {}
    }
}

fn validate_archetypes(check: &mut ContentCheck, archetype_file: &game_schema::NpcArchetypeFile) {
    for (archetype_id, archetype) in &archetype_file.archetypes {
        let owner = format!("npc archetype '{archetype_id}'");
        // Shared structural validator: same code path the server reducer
        // (`load_npc_archetypes`) and the simulation worker
        // (`NpcArchetypeRegistry::from_ron`) run. Aggregates per-row instead
        // of bailing on the first error so the content-check report still
        // surfaces all archetype problems in one run.
        if let Err(err) = game_schema::npc_archetype::validate_archetype(archetype_id, archetype) {
            check.error(err);
        }
        for ability_id in &archetype.ability_ids {
            check.require_u32(
                &check.ability_ids.clone(),
                *ability_id,
                "ability_id",
                &owner,
            );
        }
        if let Some(tree_id) = archetype.behavior_tree_id.as_deref()
            && !tree_id.is_empty()
        {
            check.require_str(
                &check.behavior_tree_ids.clone(),
                tree_id,
                "behavior_tree_id",
                &owner,
            );
        }
        if let Some(route_id) = archetype.route_id.as_deref()
            && !route_id.is_empty()
        {
            check.require_str(&check.route_ids.clone(), route_id, "route_id", &owner);
        }
        if let Some(table_id) = archetype.loot_table_id.as_deref()
            && !table_id.is_empty()
        {
            check.require_str(
                &check.loot_table_ids.clone(),
                table_id,
                "loot_table_id",
                &owner,
            );
        }
    }
}

fn validate_encounters(
    check: &mut ContentCheck,
    encounter_file: &game_core::encounter::EncounterFile,
) {
    for encounter in &encounter_file.encounters {
        let owner = format!("encounter '{}'", encounter.name);
        if let Some(table_id) = encounter.loot_table_id.as_deref() {
            check.require_str(
                &check.loot_table_ids.clone(),
                table_id,
                "loot_table_id",
                &owner,
            );
        }
        let mut rule_ids = BTreeSet::new();
        for rule in &encounter.rules {
            if rule.id.trim().is_empty() {
                check.error(format!("{owner} has a rule with an empty id"));
            } else if !rule_ids.insert(rule.id.clone()) {
                check.error(format!("{owner} has duplicate rule id '{}'", rule.id));
            }
            let rule_owner = format!("{owner} rule '{}'", rule.id);
            validate_encounter_cond(check, &rule_owner, &rule.cond);
            for (index, effect) in rule.effects.iter().enumerate() {
                validate_encounter_effect(check, &format!("{rule_owner} effect {index}"), effect);
            }
        }
    }
}

fn validate_encounter_cond(
    check: &mut ContentCheck,
    owner: &str,
    cond: &game_core::encounter::Cond,
) {
    use game_core::encounter::Cond;
    match cond {
        Cond::All { conds } | Cond::Any { conds } => {
            for (index, cond) in conds.iter().enumerate() {
                validate_encounter_cond(check, &format!("{owner} cond {index}"), cond);
            }
        }
        Cond::Not { cond } => validate_encounter_cond(check, &format!("{owner} not cond"), cond),
        Cond::AllVolumeOccupantsHaveBuff { buff_id, .. } => {
            check.require_u32(&check.buff_ids.clone(), *buff_id, "buff_id", owner);
        }
        Cond::Always
        | Cond::PhaseIs { .. }
        | Cond::HpPctCmp { .. }
        | Cond::CounterCmp { .. }
        | Cond::OccupancyCmp { .. }
        | Cond::VolumeOccupantsExactlyOneOf { .. } => {}
    }
}

fn validate_encounter_effect(
    check: &mut ContentCheck,
    owner: &str,
    effect: &game_core::encounter::Effect,
) {
    use game_core::encounter::Effect;
    match effect {
        Effect::CastSkill { skill_id, .. } | Effect::Telegraph { skill_id, .. } => {
            check.require_u32(&check.ability_ids.clone(), *skill_id, "ability_id", owner);
        }
        Effect::ReplaceAbilityList { ability_ids } => {
            for ability_id in ability_ids {
                check.require_u32(&check.ability_ids.clone(), *ability_id, "ability_id", owner);
            }
        }
        Effect::SpawnAdds { archetype, .. } => {
            if !check.archetype_kinds.contains_key(archetype) {
                check.error(format!(
                    "{owner} references unknown archetype '{archetype}'"
                ));
            } else {
                if check.archetype_kinds.get(archetype) != Some(&game_schema::EntityKind::Npc) {
                    check.error(format!(
                        "{owner} spawns archetype '{archetype}' with kind {:?}; SpawnAdds requires kind Npc",
                        check.archetype_kinds.get(archetype)
                    ));
                }
                if let Some(usage) = check.archetype_usage.get(archetype).copied()
                    && !usage.allows_encounter_add()
                {
                    check.error(format!(
                        "{owner} spawns archetype '{archetype}' with usage {:?}; SpawnAdds requires usage Both or AddOnly",
                        usage
                    ));
                }
            }
        }
        Effect::ApplyBuff { buff_id, .. } => {
            check.require_u32(&check.buff_ids.clone(), *buff_id, "buff_id", owner);
        }
        Effect::RemoveBuffs { buff_ids, .. } => {
            for buff_id in buff_ids {
                check.require_u32(&check.buff_ids.clone(), *buff_id, "buff_id", owner);
            }
        }
        Effect::Sequence { steps } | Effect::Parallel { steps } => {
            for (index, step) in steps.iter().enumerate() {
                validate_encounter_effect(check, &format!("{owner} step {index}"), step);
            }
        }
        Effect::ChangePhase { .. }
        | Effect::StartMechanic { .. }
        | Effect::StopMechanic { .. }
        | Effect::EncounterCue { .. }
        | Effect::SetInteractableState { .. }
        | Effect::ToggleInteractable { .. }
        | Effect::IncrementCounter { .. }
        | Effect::IncrementZoneCounter { .. }
        | Effect::EmitEncounterEvent { .. }
        | Effect::SpawnVolume { .. }
        | Effect::DespawnVolume { .. }
        | Effect::Wait { .. } => {}
    }
}

fn validate_dungeons(check: &mut ContentCheck, dungeon_file: &game_schema::dungeon::DungeonFile) {
    for template in &dungeon_file.templates {
        let owner = format!("dungeon '{}'", template.template_id);
        let mut local_ids = BTreeSet::new();
        for interactable in &template.interactables {
            let interactable_owner =
                format!("{owner} interactable local_id={}", interactable.local_id);
            if !local_ids.insert(interactable.local_id) {
                check.error(format!(
                    "{owner} has duplicate interactable local_id {}",
                    interactable.local_id
                ));
            }
            if let Some(buff_id) = interactable.required_buff {
                check.require_u32(
                    &check.buff_ids.clone(),
                    buff_id,
                    "buff_id",
                    &interactable_owner,
                );
            }
            if let Some(item_id) = interactable.required_item {
                check.require_u32(
                    &check.item_ids.clone(),
                    item_id,
                    "item_id",
                    &interactable_owner,
                );
            }
            if let game_schema::dungeon::InteractKindDef::BossSpawn {
                npc_name,
                encounter_name,
                archetype_id,
            } = &interactable.kind
            {
                match encounter_name.as_deref() {
                    Some(encounter_name) => check.require_str(
                        &check.encounter_ids.clone(),
                        encounter_name,
                        "encounter",
                        &interactable_owner,
                    ),
                    None => check.warn(format!(
                        "{interactable_owner} BossSpawn has no encounter_name; runtime falls back to npc_name '{npc_name}'"
                    )),
                }
                validate_dungeon_actor_archetype(
                    check,
                    &interactable_owner,
                    archetype_id.as_deref(),
                    game_schema::EntityKind::Boss,
                );
            }
            if let game_schema::dungeon::InteractKindDef::NpcSpawn { archetype_id, .. } =
                &interactable.kind
            {
                validate_dungeon_actor_archetype(
                    check,
                    &interactable_owner,
                    archetype_id.as_deref(),
                    game_schema::EntityKind::Npc,
                );
            }
        }
        for interactable in &template.interactables {
            if let Some(linked_to) = interactable.linked_to
                && !local_ids.contains(&linked_to)
            {
                check.error(format!(
                    "{owner} interactable local_id={} links to missing local_id {}",
                    interactable.local_id, linked_to
                ));
            }
        }
    }
}

fn validate_dungeon_actor_archetype(
    check: &mut ContentCheck,
    owner: &str,
    archetype_id: Option<&str>,
    expected_kind: game_schema::EntityKind,
) {
    let Some(archetype_id) = archetype_id.filter(|id| !id.is_empty()) else {
        return;
    };
    match check.archetype_kinds.get(archetype_id).copied() {
        Some(kind) if kind == expected_kind => {}
        Some(kind) => check.error(format!(
            "{owner} references archetype_id '{archetype_id}' with kind {:?}; expected {:?}",
            kind, expected_kind
        )),
        None => {
            check.error(format!(
                "{owner} references unknown archetype_id '{archetype_id}'"
            ));
            return;
        }
    }
    if let Some(usage) = check.archetype_usage.get(archetype_id).copied()
        && !usage.allows_actor_spawn()
    {
        check.error(format!(
            "{owner} references archetype_id '{archetype_id}' with usage {:?}; dungeon actor spawns require usage Both or ActorOnly",
            usage
        ));
    }
}

fn validate_spawn_rules(check: &mut ContentCheck, spawn_file: &game_schema::spawn::SpawnFile) {
    let mut rule_ids = BTreeSet::new();
    for rule in &spawn_file.rules {
        let owner = format!("spawn rule '{}'", rule.rule_id);
        if rule.rule_id.trim().is_empty() {
            check.error("spawn rule_id must not be empty");
        } else if !rule_ids.insert(rule.rule_id.clone()) {
            check.error(format!("duplicate spawn rule_id '{}'", rule.rule_id));
        }
        if let game_schema::spawn::SpawnScope::Dungeon { template_id } = &rule.scope {
            check.require_str(
                &check.dungeon_template_ids.clone(),
                template_id,
                "dungeon template_id",
                &owner,
            );
        }
        match &rule.trigger {
            game_schema::spawn::SpawnTrigger::WorldActivityEventActive { tag, .. } => {
                if tag.trim().is_empty() {
                    check.error(format!(
                        "{owner} trigger WorldActivityEventActive tag must not be empty"
                    ));
                }
            }
            game_schema::spawn::SpawnTrigger::WorldPhase { phase_name } => {
                if phase_name.trim().is_empty() {
                    check.error(format!(
                        "{owner} trigger WorldPhase phase_name must not be empty"
                    ));
                }
            }
            game_schema::spawn::SpawnTrigger::OnEvent { event_name } => {
                if event_name.trim().is_empty() {
                    check.error(format!(
                        "{owner} trigger OnEvent event_name must not be empty"
                    ));
                }
            }
            game_schema::spawn::SpawnTrigger::PlayerCountAtLeast { .. } => {}
        }
        for (index, spawn) in rule.spawns.iter().enumerate() {
            if spawn.max_hp <= 0.0 && spawn.kind != game_schema::EntityKind::Prop {
                check.error(format!("{owner} spawn {index} max_hp must be positive"));
            }
            if spawn.offset.iter().any(|value| !value.is_finite()) {
                check.error(format!("{owner} spawn {index} has a non-finite offset"));
            }
        }
    }
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
