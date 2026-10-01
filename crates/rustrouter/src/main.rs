//! rustrouter entry point: `serve`, `start`, `stop`, `update-check`.
//!
//! The launcher behaviours live here: kill whatever holds the port, restart on
//! crash, and poll readiness. `docs/RUNTIME.md` has the details.
//!
//! `start` also carries the launcher's interface menu: the server runs on a
//! background thread while the main thread shows `Choose Interface`, opens the
//! dashboard, or hands the terminal to the interactive terminal UI.

mod cli;
mod launcher;
mod mem_report;

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use router_db::{Db, Paths};
use router_server::state::{AppState, resolve_host, resolve_port};

/// mimalloc returns freed pages to the OS instead of holding them in per-thread
/// arenas, which keeps the Rust heap from ratcheting up under a burst. It backs
/// Rust allocations only: the `override` feature is off, so the C heap the
/// bundled SQLite uses stays with the platform allocator, which is why
/// `router_server::reclaim` trims that heap separately. The release matrix builds
/// all six targets, so one mimalloc cannot build fails there rather than
/// silently falling back.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Worker threads a server runtime uses: enough for the blocking SQLite calls
/// and the stream fan-out, without one thread per core on a 32-core box.
const MIN_WORKER_THREADS: usize = 4;
const MAX_WORKER_THREADS: usize = 8;
/// Blocking threads: the SQLite pool is 4, plus the checkpoint task and the
/// schedulers. 512 (the tokio default) is a thread-per-request ceiling nobody
/// wants on a small host.
const MAX_BLOCKING_THREADS: usize = 32;

/// The server runtime, sized to the box instead of tokio's defaults.
fn server_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MIN_WORKER_THREADS)
        .clamp(MIN_WORKER_THREADS, MAX_WORKER_THREADS);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .enable_all()
        .build()
}

#[derive(Parser)]
#[command(
    name = "rustrouter",
    version,
    about = "Local AI routing gateway plus dashboard, on the shared SQLite database"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run in the foreground until interrupted.
    Serve {
        /// Port to run the server (default: 20129).
        #[arg(short, long)]
        port: Option<u16>,
        /// Host to bind (default: 0.0.0.0).
        #[arg(short = 'H', long)]
        host: Option<String>,
    },
    /// Kill whatever holds the port, then run, restarting on crash.
    Start {
        /// Port to run the server (default: 20129).
        #[arg(short, long)]
        port: Option<u16>,
        /// Host to bind (default: 0.0.0.0).
        #[arg(short = 'H', long)]
        host: Option<String>,
    },
    /// Kill the process holding the port.
    Stop {
        /// Port the server runs on (default: 20129).
        #[arg(short, long)]
        port: Option<u16>,
    },
    /// Report the running version.
    UpdateCheck,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    mem_report::spawn_if_enabled();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustrouter: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command.unwrap_or(Command::Start {
        port: None,
        host: None,
    }) {
        Command::Serve { port, host } => {
            apply_overrides(port, host.clone());
            let runtime = server_runtime()?;
            runtime.block_on(serve_once(port, host))
        }
        Command::Start { port, host } => {
            let port = port.unwrap_or_else(resolve_port);
            apply_overrides(Some(port), host.clone());
            launcher::kill_port_holder(port);

            // The server owns its own thread so the main thread can drive the
            // interface menu and keep the terminal.
            let server = std::thread::spawn({
                let host = host.clone();
                move || {
                    launcher::supervise(move || {
                        let runtime = server_runtime()?;
                        runtime.block_on(serve_once(Some(port), host.clone()))
                    })
                }
            });

            if !cli::term::interactive() {
                return server.join().unwrap_or(Ok(()));
            }

            wait_ready(port);
            let token = cli_token()?;
            loop {
                match cli::interface::choose(port, router_server::APP_VERSION) {
                    cli::interface::Interface::Web => cli::interface::open_dashboard(port),
                    cli::interface::Interface::Terminal => cli::tui::start(port, token.clone())?,
                    // The server lives on a thread inside this process, so a
                    // normal return would leave it running; the launcher's Exit
                    // ends the process.
                    cli::interface::Interface::Exit => std::process::exit(0),
                }
            }
        }
        Command::Stop { port } => {
            let port = port.unwrap_or_else(resolve_port);
            if launcher::kill_port_holder(port) {
                println!("stopped the process on port {port}");
            } else {
                println!("nothing was listening on port {port}");
            }
            Ok(())
        }
        Command::UpdateCheck => {
            use router_server::services::update_check::{InstallMethod, install_method};
            println!("rustrouter {}", router_server::APP_VERSION);
            let method = install_method();
            println!("install: {}", method.as_str());
            match method {
                InstallMethod::Docker => {
                    println!("update:  docker pull ghcr.io/walujanle/rustrouter:latest");
                    println!("         docker rm -f rustrouter");
                    println!(
                        "         then re-run your original run command, or `docker compose up -d`"
                    );
                    println!("         (a pinned tag: substitute it for `latest`)");
                }
                InstallMethod::Npm => println!("update:  npm i -g rustrouter@latest"),
                InstallMethod::Binary => {
                    println!("update:  https://github.com/walujanle/rustrouter/releases/latest");
                }
            }
            Ok(())
        }
    }
}

/// Push the CLI overrides into `PORT`/`HOSTNAME` so code that resolves them
/// from the environment follows the flag too.
///
/// The bind already uses the threaded value, but `router-server` reads the port
/// back out of the environment in two places that build a loopback URL for
/// itself (`/api/models/test`, `/api/providers/{id}/test-models`). Setting the
/// environment once, before any thread starts, is what makes `--port 20128`
/// mean 20128 everywhere.
fn apply_overrides(port: Option<u16>, host: Option<String>) {
    // SAFETY: called on the main thread before the server thread or the tokio
    // runtime exists, so no other thread can be reading the environment
    // concurrently. The edition-2024 `set_var` soundness requirement is that
    // no other thread is calling `getenv` at the same time.
    unsafe {
        if let Some(port) = port {
            std::env::set_var("PORT", port.to_string());
        }
        if let Some(host) = host {
            std::env::set_var("HOSTNAME", host);
        }
    }
}

/// The `x-9r-cli-token` the TUI sends: the machine id under the CLI salt, the
/// same value `AppState::cli_token()` hands the guard. Derived in-process, so
/// the terminal UI never has to log in.
fn cli_token() -> anyhow::Result<String> {
    let paths = Paths::from_env();
    paths.ensure_dirs()?;
    Ok(router_db::identity::consistent_machine_id(
        &paths,
        Some(router_db::identity::CLI_AUTH_SALT),
    )?)
}

/// Poll until the server accepts a TCP connection, or `timeout` elapses.
fn wait_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Open the shared database, migrate, and serve.
///
/// The order is load-bearing and copied from `docs/RUNTIME.md`: the DB has to
/// be ready and migrated before any scheduler starts, because the schedulers
/// read connections the moment they come up. The checkpoint timer is here
/// because it is pure DB hygiene.
async fn serve_once(port: Option<u16>, host: Option<String>) -> anyhow::Result<()> {
    let paths = Paths::from_env();
    paths.ensure_dirs()?;

    let db = Db::open(&paths.data_file, router_db::driver::DEFAULT_POOL_SIZE)?;
    // `migrate_on_boot` also takes the pre-schema safety backup and prunes old
    // ones, so a schema change is always recoverable from `db/backups/`.
    db.with_conn(|conn| {
        router_db::migrations::migrate_on_boot(conn, &paths, router_server::APP_VERSION)
    })?;
    db.spawn_checkpoint_task();
    // Hand freed pages back to the OS once the request streams have been idle a
    // while. Paired with the pool caps, this is what lets a burst's footprint
    // fall back instead of staying resident.
    router_server::reclaim::spawn();

    // The stored outbound proxy is applied at boot and on every settings write.
    // A failure to read the settings must not stop the server: the proxy stays
    // off and the next settings write applies it.
    if let Ok(settings) = db.with_conn(router_db::repos::settings::get_settings) {
        router_sse::executors::http::apply_outbound_proxy_settings(&settings);
    }

    // Startup: drop null optional fields and empty `providerSpecificData` left
    // behind by older rows. Best-effort, like the proxy read above.
    if let Err(error) = db.with_conn(router_db::repos::connections::cleanup_provider_connections) {
        tracing::warn!("[startup] connection cleanup failed (continuing): {error}");
    }

    let state = AppState::new(db, paths)?;

    // The three schedulers, started once the DB is ready and before the
    // listener binds (`docs/RUNTIME.md`, startup order).
    router_server::services::schedulers::start_all(state.clone()).await;

    let port = port.unwrap_or_else(resolve_port);
    let host = host.unwrap_or_else(resolve_host);
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {host}:{port}: {e}"))?;

    router_server::serve(state, addr).await?;
    Ok(())
}

/// Install the Console Log page's capture layer.
///
/// The terminal `fmt` layer is deliberately absent: the server's stdout is
/// discarded and the app's logging surfaces on the dashboard's Console Log
/// page. Logs go to the in-memory buffer the SSE route streams, never to the
/// terminal the TUI is drawing on.
///
/// The capture filter is pinned to the app targets at info+ (including
/// `router_sse`, where the chat request lines are emitted) rather than derived
/// from `RUST_LOG`, so the page is never mysteriously blank.
fn init_tracing() {
    use router_server::services::console_log::ConsoleLogLayer;
    use tracing_subscriber::EnvFilter;
    // `with_filter` is a `Layer` method; the trait has to be in scope.
    use tracing_subscriber::layer::{Layer, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;

    const DEFAULT_TARGETS: &str =
        "rustrouter=info,router_server=info,router_db=info,router_sse=info";

    let capture_filter = EnvFilter::new(DEFAULT_TARGETS);

    let _ = tracing_subscriber::registry()
        .with(ConsoleLogLayer.with_filter(capture_filter))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_server::state::{resolve_host, resolve_port};

    /// The whole point of `apply_overrides`: after it runs, the env-only
    /// resolvers the server uses for its self-referential URLs report the flag
    /// value, not the default. Lives in this test binary, so the env mutation
    /// cannot race the router-server crate's own tests.
    #[test]
    fn overrides_reach_the_env_resolvers() {
        apply_overrides(Some(20128), Some("127.0.0.1".to_string()));
        assert_eq!(resolve_port(), 20128);
        assert_eq!(resolve_host(), "127.0.0.1");

        // A `None` leaves the resolved value alone (serve with no flag).
        apply_overrides(None, None);
        assert_eq!(resolve_port(), 20128);
    }
}
