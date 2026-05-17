use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use regex::Regex;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MODULE_NAME: &str = "jump";
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
}

#[derive(Subcommand)]
enum BuildCmd {
    Worker(BuildProfileArgs),
    Client(BuildProfileArgs),
    Wasm,
    All(BuildProfileArgs),
}

#[derive(Subcommand)]
enum TestCmd {
    Fast,
    Worker,
    Cli,
    /// Run multi-client integration tests (requires running server + worker)
    MultiClient,
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
    #[arg(long, default_value = "crates/simulation_worker/tests/fixtures/combat_lifecycle_v1.generated.json")]
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
    }
}

fn run_build(cmd: BuildCmd) -> Result<()> {
    match cmd {
        BuildCmd::Worker(args) => run_command(cargo_build_command(
            ["-p", "simulation_worker", "--features", "connected"],
            args.release,
        )),
        BuildCmd::Client(args) => run_command(cargo_build_command(
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
        TestCmd::Cli => dev_client_test(ClientTestArgs {
            release: false,
            args: Vec::new(),
        }),
        TestCmd::MultiClient => run_command(cargo_cmd([
            "run",
            "-p",
            "game_client",
            "--features",
            "connected",
            "--",
            "--test-multi",
        ])),
        TestCmd::Workspace => run_command(cargo_cmd([
            "test",
            "--workspace",
            "--exclude",
            "server_module",
        ])),
        TestCmd::Replay(args) => run_command(cargo_test_command([
            "-p",
            "simulation_worker",
            "--test",
            "deterministic_replay",
        ], args.release)),
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

fn dev_capture_fixture(args: CaptureFixtureArgs) -> Result<()> {
    let mut command = cargo_test_command([
        "-p",
        "simulation_worker",
        "--test",
        "deterministic_replay",
    ],
    args.release);
    command.args(["capture_combat_lifecycle_fixture_to_json", "--", "--ignored", "--exact", "--nocapture"]);

    command.env("REPLAY_FIXTURE_OUT", args.out);
    run_command(command)
}

fn capture_worker_identity(release: bool) -> Result<String> {
    let mut child = cargo_run_command(["-p", "simulation_worker", "--features", "connected"], release)
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
