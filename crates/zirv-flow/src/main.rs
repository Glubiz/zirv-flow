// Makes the `browser-test` feature impossible to compile by accident: `--all-features`, an alias or
// a default list all fail here unless the builder deliberately sets the variable.
#[cfg(feature = "browser-test")]
const _: () = assert!(
    option_env!("ZIRV_FLOW_BROWSER_TEST_BUILD").is_some(),
    "browser-test is for the browser test server only: set ZIRV_FLOW_BROWSER_TEST_BUILD=1 to build it",
);

mod cli;
mod config;
#[cfg(test)]
mod e2e;
mod healthcheck;
mod log;
mod otlp;
mod secrets;

use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use zirv_flow_store::{PgPool, SchemaState};

use crate::cli::{AdminCommand, Cli, Command, ConfigCommand};
use crate::config::Config;
use crate::secrets::{Secrets, load_hmac_root, load_keyring, load_secrets};

/// Seconds; makes the exporter render request durations as histograms rather than summaries.
const HTTP_DURATION_BUCKETS: [f64; 10] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// Seconds; for job durations and claim lag, which run far longer than a request.
const JOB_BUCKETS: [f64; 13] = [
    0.005, 0.025, 0.1, 0.5, 1.0, 2.5, 5.0, 15.0, 30.0, 60.0, 300.0, 900.0, 3600.0,
];

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The render host is synchronous and single-threaded: it starts no async runtime at all.
    if let Some(Command::RenderHost { memory_limit_bytes }) = &cli.command {
        return ExitCode::from(run_render_host(*memory_limit_bytes));
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    ExitCode::from(runtime.block_on(async {
        run(
            cli,
            std::env::vars_os(),
            &mut io::stdin().lock(),
            &mut io::stdout(),
            &mut io::stderr(),
        )
        .await
    }))
}

/// The hidden `render-host` command: logs go to standard error, because standard output carries
/// the response frames and nothing else.
fn run_render_host(memory_limit_bytes: Option<u64>) -> u8 {
    let subscriber = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    zirv_flow_render::host::run(memory_limit_bytes)
}

async fn run(
    cli: Cli,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let command = cli.command.unwrap_or(Command::All);
    // Writing a key needs no configuration, so it works before any exists.
    if let Command::Admin(AdminCommand::GenerateMasterKey { out, if_missing }) = &command {
        return generate_master_key(out, *if_missing, stdout, stderr);
    }

    let config = match Config::load(cli.config.as_deref(), env) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(stderr, "error: {error}");
            return 2;
        }
    };

    if matches!(command, Command::Config(ConfigCommand::Check)) {
        match toml::to_string_pretty(&config) {
            Ok(output) => {
                let _ = write!(stdout, "{output}");
                return 0;
            }
            Err(_) => {
                let _ = writeln!(stderr, "error: config: could not serialise configuration");
                return 2;
            }
        }
    }

    if matches!(command, Command::Healthcheck) {
        return healthcheck::run(config.server_bind(), stderr).await;
    }

    if matches!(command, Command::Migrate) {
        return run_migrate(&config, stdout, stderr).await;
    }

    if let Command::Admin(admin_command) = &command {
        if let AdminCommand::ExportLedger { out } = admin_command {
            // Exporting reads the ledger table only: no key and no password.
            let pool = match open_database(&config).await {
                Ok(pool) => pool,
                Err(message) => return database_error(stderr, message),
            };
            return run_export_ledger(out, &pool, stdout, stderr).await;
        }
        if let AdminCommand::ReplayErasures { workspace, from } = admin_command {
            // Replaying needs the HMAC root to hash identifiers, and no password.
            let keyring = match load_keyring(&config) {
                Ok(keyring) => keyring,
                Err(message) => {
                    let _ = writeln!(stderr, "error: admin: {message}");
                    return 2;
                }
            };
            let pool = match open_database(&config).await {
                Ok(pool) => pool,
                Err(message) => return database_error(stderr, message),
            };
            let root = match load_hmac_root(&pool, &keyring).await {
                Ok(root) => root,
                Err(message) => {
                    let _ = writeln!(stderr, "error: admin: {message}");
                    return 2;
                }
            };
            return run_replay_erasures(
                workspace.as_deref(),
                from.as_deref(),
                &root,
                &pool,
                stdout,
                stderr,
            )
            .await;
        }
        if let AdminCommand::EvaluateAssistant { workspace, case } = admin_command {
            // Evaluating opens the provider key, so it needs the keyring, and no password.
            let keyring = match load_keyring(&config) {
                Ok(keyring) => keyring,
                Err(message) => {
                    let _ = writeln!(stderr, "error: admin: {message}");
                    return 2;
                }
            };
            let pool = match open_database(&config).await {
                Ok(pool) => pool,
                Err(message) => return database_error(stderr, message),
            };
            let secrets = match load_secrets(&pool, keyring).await {
                Ok(secrets) => secrets,
                Err(message) => {
                    let _ = writeln!(stderr, "error: admin: {message}");
                    return 2;
                }
            };
            return run_evaluate_assistant(
                &config,
                pool,
                &secrets,
                workspace,
                case.as_deref(),
                stdout,
                stderr,
            )
            .await;
        }
        if matches!(admin_command, AdminCommand::RotateMasterKey) {
            // Rotating needs the keyring and the database, and no password.
            let keyring = match load_keyring(&config) {
                Ok(keyring) => keyring,
                Err(message) => {
                    let _ = writeln!(stderr, "error: admin: {message}");
                    return 2;
                }
            };
            let pool = match open_database(&config).await {
                Ok(pool) => pool,
                Err(message) => return database_error(stderr, message),
            };
            return run_rotate_master_key(&keyring, &pool, stdout, stderr).await;
        }
        // The password is read first so a missing one fails without touching the database.
        let password = match read_password(stdin) {
            Ok(password) => password,
            Err(message) => {
                let _ = writeln!(stderr, "error: admin: {message}");
                return 1;
            }
        };
        let pool = match open_database(&config).await {
            Ok(pool) => pool,
            Err(message) => return database_error(stderr, message),
        };
        return run_admin(admin_command, &pool, &password, stdout, stderr).await;
    }

    if let Some(roles) = Roles::of(&command) {
        // The key is checked before the database is opened or anything is bound or claimed.
        let keyring = if roles.serve || roles.worker {
            match load_keyring(&config) {
                Ok(keyring) => Some(keyring),
                Err(message) => {
                    let _ = writeln!(stderr, "error: {message}");
                    return 2;
                }
            }
        } else {
            None
        };
        let export = log::install(&config);
        if let Some(keyring) = &keyring {
            info!(key_id = %keyring.current().key_id(), "master key loaded");
            if keyring.has_previous() {
                info!(
                    "a previous master key is configured; run `admin rotate-master-key`, then remove it"
                );
            }
        }
        let pool = match open_database(&config).await {
            Ok(pool) => pool,
            Err(message) => {
                error!(error = %message, "could not open the database");
                flush_traces(export).await;
                return 1;
            }
        };
        // The HMAC root lives in the database, so it is loaded once the schema is ready.
        let secrets = match keyring {
            Some(keyring) => match load_secrets(&pool, keyring).await {
                Ok(secrets) => Some(secrets),
                Err(message) => {
                    error!(error = %message, "the secrets could not be started");
                    flush_traces(export).await;
                    return 2;
                }
            },
            None => None,
        };
        let code = run_roles(roles, &config, pool, secrets, shutdown_signal()).await;
        flush_traces(export).await;
        return code;
    }

    let _ = writeln!(
        stderr,
        "error: the {} command is not implemented yet",
        command.name()
    );
    1
}

/// Creates `out` holding a new master key, never overwriting a file. With `if_missing`, an
/// existing file that holds a valid key is kept. Prints the key id, never the key.
fn generate_master_key(
    out: &Path,
    if_missing: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let mut fail = |message: String| {
        let _ = writeln!(stderr, "error: admin: {message}");
        1
    };
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty())
        && let Err(error) = fs::create_dir_all(parent)
    {
        return fail(format!("could not create {}: {error}", parent.display()));
    }
    let key = zirv_flow_store::MasterKey::generate();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(out) {
        Ok(mut file) => {
            if let Err(error) = writeln!(file, "{}", key.to_base64()) {
                drop(file);
                let _ = fs::remove_file(out);
                return fail(format!("could not write {}: {error}", out.display()));
            }
            let _ = writeln!(stdout, "master key written: {}", key.key_id());
            0
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !if_missing {
                return fail(format!(
                    "{} already exists; it is never overwritten",
                    out.display()
                ));
            }
            let existing = fs::read_to_string(out)
                .map_err(|_| "not readable".to_owned())
                .and_then(|text| {
                    zirv_flow_store::MasterKey::parse(&text).map_err(|error| error.to_string())
                });
            match existing {
                Ok(existing) => {
                    let _ = writeln!(stdout, "master key exists: {}", existing.key_id());
                    0
                }
                Err(reason) => fail(format!("{}: {reason}", out.display())),
            }
        }
        Err(error) => fail(format!("could not create {}: {error}", out.display())),
    }
}

/// Flushes the trace exporter, if there is one, before the process exits.
async fn flush_traces(export: Option<otlp::Otlp>) {
    if let Some(export) = export {
        export.shutdown(TRACE_FLUSH_TIMEOUT).await;
    }
}

/// How long the exit waits for queued spans to reach the collector; well inside
/// [`SHUTDOWN_GRACE`].
const TRACE_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// The long-running roles a command starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Roles {
    serve: bool,
    worker: bool,
    scheduler: bool,
}

impl Roles {
    /// The roles of a long-running command, or none for a one-off one.
    fn of(command: &Command) -> Option<Self> {
        let (serve, worker, scheduler) = match command {
            Command::All => (true, true, true),
            Command::Serve => (true, false, false),
            Command::Worker => (false, true, false),
            Command::Scheduler => (false, false, true),
            _ => return None,
        };
        Some(Self {
            serve,
            worker,
            scheduler,
        })
    }
}

/// Runs the roles in one process until `shutdown` completes, then drains them: the server
/// finishes its requests, workers stop claiming and finish running jobs, and the scheduler
/// gives up its lock. Every role stops at once and shares one deadline, [`SHUTDOWN_GRACE`]
/// after the shutdown began; whatever is still running then is left to its lease to expire.
async fn run_roles(
    roles: Roles,
    config: &Config,
    pool: PgPool,
    secrets: Option<Secrets>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> u8 {
    run_roles_reporting(roles, config, pool, secrets, shutdown, None).await
}

/// [`run_roles`], sending the address the ops listener bound to `ops_bound` when it starts.
async fn run_roles_reporting(
    roles: Roles,
    config: &Config,
    pool: PgPool,
    secrets: Option<Secrets>,
    shutdown: impl Future<Output = ()> + Send + 'static,
    ops_bound: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> u8 {
    let hmac_root = secrets
        .as_ref()
        .map(|secrets| Arc::clone(&secrets.hmac_root));
    let ops_bind = ops_bind(roles, config);
    if roles.worker
        && let Err(code) = check_ledger_dir(config)
    {
        return code;
    }
    // A bad server configuration or a taken ops address stops the process before any role starts.
    let render_pool = if roles.serve || roles.worker {
        match start_render_pool(config).await {
            Ok(pool) => Some(pool),
            Err(code) => return code,
        }
    } else {
        None
    };
    let render = if roles.serve || ops_bind.is_some() {
        match install_recorder() {
            Ok(render) => Some(render),
            Err(code) => return code,
        }
    } else {
        None
    };
    let state = match (roles.serve, &render) {
        (true, Some(render)) => {
            match http_state(config, pool.clone(), Arc::clone(render), secrets.as_ref()) {
                Ok(state) => Some(match &render_pool {
                    Some(render_pool) => state.with_render_pool(render_pool.clone()),
                    None => state,
                }),
                Err(code) => return code,
            }
        }
        _ => None,
    };
    if let Some(state) = &state
        && let Err(code) = announce_setup(state, config).await
    {
        return code;
    }

    let (stop, stopped) = tokio::sync::watch::channel(false);
    let liveness = Arc::new(RoleLiveness::default());
    let ops = match (ops_bind, render) {
        (Some(addr), Some(render)) => {
            let state = zirv_flow_api::OpsState::new(
                pool.clone(),
                render,
                role_readiness(stopped.clone(), Arc::clone(&liveness)),
            );
            match start_ops(addr, state).await {
                Ok(ops) => {
                    if let Some(report) = ops_bound {
                        let _ = report.send(ops.address);
                    }
                    Some(ops)
                }
                Err(code) => return code,
            }
        }
        _ => None,
    };
    let stop = Arc::new(stop);
    let deadline = Arc::new(ShutdownDeadline::default());
    tokio::spawn({
        let stop = Arc::clone(&stop);
        let deadline = Arc::clone(&deadline);
        async move {
            shutdown.await;
            deadline.start();
            let _ = stop.send(true);
        }
    });

    let mut background = tokio::task::JoinSet::new();
    if roles.worker {
        let mut worker_config =
            zirv_flow_jobs::WorkerConfig::new(config.worker_concurrency() as usize);
        worker_config.lease = Duration::from_secs(u64::from(config.worker_lease_seconds()));
        worker_config.max_running_per_workspace = config.worker_max_running_per_workspace();
        worker_config.event_retention = config.events_retention();
        worker_config.redaction_key = hmac_root
            .as_deref()
            .map(|root| Arc::new(root.hmac_key(zirv_flow_store::HmacPurpose::Redaction)));
        worker_config.identifier_key = hmac_root
            .as_deref()
            .map(|root| Arc::new(root.hmac_key(zirv_flow_store::HmacPurpose::Identifier)));
        worker_config.ledger_dir = config.ledger_dir().map(|dir| Arc::new(dir.to_path_buf()));
        worker_config.public_url = config.public_url();
        worker_config.tracking_key = hmac_root
            .as_deref()
            .map(|root| Arc::new(root.hmac_key(zirv_flow_store::HmacPurpose::Tracking)));
        worker_config.keyring = secrets.as_ref().map(|secrets| Arc::clone(&secrets.keyring));
        worker_config.ingest_drain_batch_size = config.ingest_drain_batch_size();
        worker_config.ingest_drain_shards = config.ingest_drain_shards();
        worker_config.egress = config.egress_policy();
        worker_config.render_pool = render_pool.clone();
        worker_config.dns = system_resolver();
        share_rate_limits(state.as_ref(), &mut worker_config);
        let worker = zirv_flow_jobs::Worker::new(
            pool.clone(),
            zirv_flow_jobs::Registry::built_in(),
            worker_config,
        );
        background.spawn(liveness.track(Role::Worker, worker.run(stopped.clone())));
        info!(
            role = "worker",
            concurrency = config.worker_concurrency(),
            "worker started"
        );
    }
    if roles.scheduler {
        let scheduler = zirv_flow_jobs::Scheduler::new(
            pool.clone(),
            zirv_flow_jobs::SchedulerConfig {
                sending_domain_recheck: config.sending_domain_recheck(),
                ..zirv_flow_jobs::SchedulerConfig::default()
            },
        );
        background.spawn(liveness.track(Role::Scheduler, scheduler.run(stopped.clone())));
        info!(role = "scheduler", "scheduler standing for leadership");
    }

    let code = if let Some(state) = state {
        let mut draining = stopped.clone();
        let code = serve_http(state, config.server_bind(), &deadline, async move {
            let _ = draining.wait_for(|stopped| *stopped).await;
        })
        .await;
        // A server that failed to start or stopped on its own takes the other roles down.
        deadline.start();
        let _ = stop.send(true);
        code
    } else {
        let mut waiting = stopped.clone();
        let _ = waiting.wait_for(|stopped| *stopped).await;
        0
    };

    drain(background, deadline.start()).await;
    if let Some(render_pool) = &render_pool {
        render_pool.shutdown().await;
    }
    if let Some(ops) = ops {
        ops.stop().await;
    }
    code
}

/// What each pool child runs: the hidden `render-host` command of this executable.
#[cfg(not(test))]
fn render_host_command(
    program: std::path::PathBuf,
    memory_limit_bytes: u64,
) -> zirv_flow_render::ChildCommand {
    zirv_flow_render::ChildCommand::render_host(program, memory_limit_bytes)
}

/// Under test the executable is the test binary, which has no `render-host` command, so a child
/// is the test binary re-run on `tests::render_host_child`, which serves as the host does.
#[cfg(test)]
fn render_host_command(
    program: std::path::PathBuf,
    _memory_limit_bytes: u64,
) -> zirv_flow_render::ChildCommand {
    zirv_flow_render::ChildCommand::new(program)
        .args(["--exact", "tests::render_host_child", "--nocapture"])
        .args(["--test-threads", "1", "-q"])
        .env("ZIRV_FLOW_TEST_RENDER_HOST", "1")
        .skip_stdout_until("running 1 test\n")
}

/// The resolver sending-domain checks read DNS through, from the system's name servers. Without
/// one the checks fail until the host is fixed and the process restarted; nothing else needs it.
fn system_resolver() -> Option<Arc<dyn zirv_flow_jobs::DnsResolver>> {
    match zirv_flow_jobs::SystemResolver::new() {
        Ok(resolver) => Some(Arc::new(resolver)),
        Err(error) => {
            warn!(%error, "no DNS resolver could be built; sending domain checks will fail");
            None
        }
    }
}

/// Starts the render-host children this process renders through: copies of this executable
/// running the hidden `render-host` command. On failure it reports the problem and returns the
/// exit code.
/// With `privacy.ledger_dir` set, proves the directory takes a durable write and rename before any
/// worker starts; refuses to start otherwise. Nothing is checked when the key is unset.
fn check_ledger_dir(config: &Config) -> Result<(), u8> {
    let Some(dir) = config.ledger_dir() else {
        return Ok(());
    };
    zirv_flow_store::check_ledger_dir(dir).map_err(|error| {
        error!(
            key = "privacy.ledger_dir",
            path = %dir.display(),
            %error,
            "privacy.ledger_dir cannot be written; fix the directory or unset the key"
        );
        1_u8
    })
}

async fn start_render_pool(config: &Config) -> Result<zirv_flow_render::RenderPool, u8> {
    let program = std::env::current_exe().map_err(|error| {
        error!(%error, "could not find this executable to start the render host");
        1_u8
    })?;
    let (pool_config, memory_limit_bytes) = config.render_pool();
    let command = render_host_command(program, memory_limit_bytes);
    let size = pool_config.size;
    let pool = zirv_flow_render::RenderPool::start(command, pool_config)
        .await
        .map_err(|error| {
            error!(%error, "could not start the render host");
            1_u8
        })?;
    info!(children = size, memory_limit_bytes, "render host started");
    Ok(pool)
}

/// The address of the ops listener, when this process should run one: only without the
/// `serve` role, whose server already answers the same endpoints.
fn ops_bind(roles: Roles, config: &Config) -> Option<SocketAddr> {
    let addr = config.ops_bind()?;
    if roles.serve {
        warn!(
            address = %addr,
            server = %config.server_bind(),
            "ops.bind is ignored because the serve role is running; /metrics, /healthz and /readyz are on server.bind"
        );
        return None;
    }
    Some(addr)
}

/// What `/readyz` asks of the roles: shutdown has begun, or a started role loop has returned.
fn role_readiness(
    stopped: tokio::sync::watch::Receiver<bool>,
    liveness: Arc<RoleLiveness>,
) -> Arc<dyn Fn() -> Option<&'static str> + Send + Sync> {
    Arc::new(move || {
        if *stopped.borrow() {
            Some("shutting down")
        } else {
            liveness.not_ready()
        }
    })
}

#[derive(Clone, Copy)]
enum Role {
    Worker,
    Scheduler,
}

/// Which role loops of the process have returned. A role the process did not start never does.
#[derive(Default)]
struct RoleLiveness {
    worker_stopped: std::sync::atomic::AtomicBool,
    scheduler_stopped: std::sync::atomic::AtomicBool,
}

impl RoleLiveness {
    /// Runs `role` and records that it returned.
    fn track<F: Future<Output = ()>>(
        self: &Arc<Self>,
        role: Role,
        future: F,
    ) -> impl Future<Output = ()> + use<F> {
        let liveness = Arc::clone(self);
        async move {
            future.await;
            let flag = match role {
                Role::Worker => &liveness.worker_stopped,
                Role::Scheduler => &liveness.scheduler_stopped,
            };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The reason the process is not ready, or `None` while every started role is running.
    fn not_ready(&self) -> Option<&'static str> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.worker_stopped.load(SeqCst) {
            Some("the worker loop has stopped")
        } else if self.scheduler_stopped.load(SeqCst) {
            Some("the scheduler loop has stopped")
        } else {
            None
        }
    }
}

/// While no owner exists, mints the first-run setup token and logs it, once. This line is
/// the only place the token appears.
async fn announce_setup(state: &zirv_flow_api::AppState, config: &Config) -> Result<(), u8> {
    match state.mint_setup_token().await {
        Ok(Some(token)) => {
            warn!(
                "no owner exists; finish setup at {}/setup with setup token {token}, or run zirv-flow admin create-owner",
                config.base_url()
            );
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(error) => {
            error!(%error, "could not check whether an owner exists");
            Err(1)
        }
    }
}

/// The running ops listener.
struct OpsListener {
    address: SocketAddr,
    drain: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl OpsListener {
    /// Stops accepting and gives open connections a moment before cutting them.
    async fn stop(self) {
        let _ = self.drain.send(true);
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(1), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}

/// Binds `addr` and serves the ops endpoints in the background. On failure it reports the
/// problem and returns the exit code.
async fn start_ops(addr: SocketAddr, state: zirv_flow_api::OpsState) -> Result<OpsListener, u8> {
    let listener = TcpListener::bind(addr).await.map_err(|error| {
        error!(address = %addr, %error, "could not bind the ops address");
        1_u8
    })?;
    let address = listener.local_addr().unwrap_or(addr);
    info!(address = %address, "ops listener on {address}");
    let (drain, draining) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let mut draining = draining;
            let _ = draining.wait_for(|draining| *draining).await;
        };
        if let Err(error) = zirv_flow_api::serve_ops(listener, state, shutdown).await {
            error!(%error, "the ops listener failed");
        }
    });
    Ok(OpsListener {
        address,
        drain,
        task,
    })
}

/// Makes the worker use the API state's rate-limit buckets, so a process that runs both
/// `serve` and a worker keeps one bucket per source across them. A worker-only process keeps
/// its own.
fn share_rate_limits(
    state: Option<&zirv_flow_api::AppState>,
    worker: &mut zirv_flow_jobs::WorkerConfig,
) {
    if let Some(state) = state {
        worker.rate_limits = Arc::clone(state.rate_limits());
    }
}

type Render = Arc<dyn Fn() -> String + Send + Sync>;

/// Installs the process-wide Prometheus recorder and returns what renders it. On failure it
/// reports the problem and returns the exit code.
fn install_recorder() -> Result<Render, u8> {
    let handle = PrometheusBuilder::new()
        .set_buckets(&HTTP_DURATION_BUCKETS)
        .and_then(|builder| {
            builder.set_buckets_for_metric(Matcher::Prefix("zirv_flow_job_".into()), &JOB_BUCKETS)
        })
        .and_then(|builder| builder.install_recorder())
        .map_err(|error| {
            error!(%error, "could not install the metrics recorder");
            1
        })?;
    Ok(Arc::new(move || handle.render()))
}

/// The API state for the `serve` role: the metrics renderer, cookie and sign-in settings.
/// On failure it reports the problem and returns the exit code.
fn http_state(
    config: &Config,
    pool: PgPool,
    render: Render,
    secrets: Option<&Secrets>,
) -> Result<zirv_flow_api::AppState, u8> {
    let mut state = zirv_flow_api::AppState::new(pool, render)
        .with_cookie_secure(config.server_cookie_secure())
        .with_local_sign_in(config.auth_local_enabled())
        .with_event_retention(config.events_retention())
        .with_ingest_limits(config.ingest_limits())
        .with_import_limits(config.import_limits())
        .with_asset_limits(config.asset_limits())
        .with_explorer_limits(config.explorer_limits())
        .with_egress_policy(config.egress_policy())
        .with_public_url(config.public_url())
        .with_prefetch_user_agents(config.prefetch_user_agents())
        .with_ai_config(config.ai_config());
    if let Some(secrets) = secrets {
        state = state
            .with_hmac_root(&secrets.hmac_root)
            .with_keyring(Arc::clone(&secrets.keyring));
    }

    match config.oidc() {
        Some((settings, auto_create_users)) => state
            .with_oidc(settings, auto_create_users)
            .map_err(|error| {
                error!(%error, "auth.oidc is not usable");
                2
            }),
        None => Ok(state),
    }
}

/// The one deadline of a shutdown, shared by every role: it starts when the shutdown does.
#[derive(Default)]
struct ShutdownDeadline(std::sync::OnceLock<tokio::time::Instant>);

impl ShutdownDeadline {
    /// Starts the clock if it is not running yet, and returns the instant it ends.
    fn start(&self) -> tokio::time::Instant {
        *self
            .0
            .get_or_init(|| tokio::time::Instant::now() + SHUTDOWN_GRACE)
    }
}

/// Waits for the background roles to finish, until `deadline`.
async fn drain(mut background: tokio::task::JoinSet<()>, deadline: tokio::time::Instant) {
    let drained = async { while background.join_next().await.is_some() {} };
    tokio::select! {
        result = tokio::time::timeout_at(deadline, drained) => {
            if result.is_err() {
                warn!(
                    "jobs still running after {}s; leaving them to lease expiry",
                    SHUTDOWN_GRACE.as_secs()
                );
            }
        }
        _ = tokio::signal::ctrl_c() => {
            warn!("interrupted while draining jobs");
        }
    }
}

/// Binds `addr` and serves the API until `shutdown` completes.
async fn serve_http(
    state: zirv_flow_api::AppState,
    addr: SocketAddr,
    deadline: &ShutdownDeadline,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> u8 {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            error!(address = %addr, %error, "could not bind the server address");
            return 1;
        }
    };
    let bound = listener.local_addr().unwrap_or(addr);
    info!(address = %bound, "listening on {bound}");

    let (drain, draining) = tokio::sync::watch::channel(false);
    let server = zirv_flow_api::serve(listener, state, async move {
        let mut draining = draining;
        let _ = draining.wait_for(|draining| *draining).await;
    });
    tokio::pin!(server);

    let result = tokio::select! {
        result = &mut server => result,
        () = shutdown => {
            let _ = drain.send(true);
            // In-flight requests get a grace period; a second Ctrl-C or the
            // deadline ends the process without waiting for them.
            tokio::select! {
                result = tokio::time::timeout_at(deadline.start(), &mut server) => match result {
                    Ok(result) => result,
                    Err(_) => {
                        error!("requests still running after {}s; exiting", SHUTDOWN_GRACE.as_secs());
                        return 1;
                    }
                },
                _ = tokio::signal::ctrl_c() => {
                    error!("interrupted while draining requests");
                    return 1;
                }
            }
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            error!(%error, "the server failed");
            1
        }
    }
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Completes on Ctrl-C, or SIGTERM on Unix.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    () = ctrl_c => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Connects, then migrates when `database.migrate_on_start` is set or refuses to go on
/// when the schema is not ready. Errors are the message to report.
async fn open_database(config: &Config) -> Result<PgPool, String> {
    let pool = connect(config).await.map_err(|error| error.to_string())?;
    if config.database_migrate_on_start() {
        zirv_flow_store::migrate(&pool)
            .await
            .map_err(|error| error.to_string())?;
    } else {
        let state = zirv_flow_store::schema_state(&pool)
            .await
            .map_err(|error| error.to_string())?;
        if let StartupAction::Refuse(message) = startup_action(state) {
            return Err(message);
        }
    }
    Ok(pool)
}

/// One line from standard input, without its line ending. Nothing else is trimmed, since
/// a password may begin or end with spaces.
fn read_password(stdin: &mut impl BufRead) -> Result<String, &'static str> {
    let mut line = String::new();
    match stdin.read_line(&mut line) {
        Ok(0) => return Err("no password on standard input"),
        Ok(_) => {}
        Err(_) => return Err("could not read the password from standard input"),
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

async fn run_admin(
    command: &AdminCommand,
    pool: &PgPool,
    password: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let context = zirv_flow_api::admin::Context::system();
    let result = match command {
        AdminCommand::CreateOwner { email } => {
            zirv_flow_api::admin::create_owner(pool, &context, email, password)
                .await
                .map(|_| "owner created")
        }
        AdminCommand::ResetPassword { email } => {
            zirv_flow_api::admin::reset_password(pool, &context, email, password)
                .await
                .map(|()| "password reset; the user's sessions were ended")
        }
        AdminCommand::GenerateMasterKey { .. } => {
            unreachable!("generate-master-key is handled before the database is opened")
        }
        AdminCommand::ReplayErasures { .. } => {
            unreachable!("replay-erasures is handled before the password is read")
        }
        AdminCommand::ExportLedger { .. } => {
            unreachable!("export-ledger is handled before the password is read")
        }
        AdminCommand::RotateMasterKey => {
            unreachable!("rotate-master-key is handled before the password is read")
        }
        AdminCommand::EvaluateAssistant { .. } => {
            unreachable!("evaluate-assistant is handled before the password is read")
        }
    };
    match result {
        Ok(message) => {
            let _ = writeln!(stdout, "{message}");
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "error: admin: {error}");
            1
        }
    }
}

/// Re-wraps every data key with the current master key and prints counts only.
async fn run_rotate_master_key(
    keyring: &zirv_flow_store::Keyring,
    pool: &PgPool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let context = zirv_flow_api::admin::Context::system();
    match zirv_flow_api::admin::rotate_master_key(pool, &context, keyring).await {
        Ok(report) => {
            let _ = writeln!(
                stdout,
                "re-wrapped {} data key(s) with master key {}",
                report.rows, report.key_id
            );
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "error: admin: {error}");
            1
        }
    }
}

/// Re-applies the erasure ledger after a restore and prints counts only.
/// Runs the assistant's golden prompts against the workspace's model and prints one JSON line per
/// case. The exit code is 0 whatever the scores; only a run that could not happen fails.
async fn run_evaluate_assistant(
    config: &Config,
    pool: PgPool,
    secrets: &Secrets,
    workspace: &str,
    case: Option<&str>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let mut fail = |message: String| {
        let _ = writeln!(stderr, "error: admin: {message}");
        1
    };
    let found = match zirv_flow_api::admin::workspace_by_slug(&pool, workspace).await {
        Ok(found) => found,
        Err(error) => return fail(error.to_string()),
    };
    let state = match http_state(config, pool, Arc::new(String::new), Some(secrets)) {
        Ok(state) => state,
        Err(code) => return code,
    };
    match zirv_flow_api::evaluate_assistant(&state, found, case).await {
        Ok(reports) => {
            for report in reports {
                let _ = writeln!(stdout, "{}", report.to_json_line());
            }
            0
        }
        Err(error) => fail(error.to_string()),
    }
}

async fn run_export_ledger(
    out: &Path,
    pool: &PgPool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    match zirv_flow_api::admin::export_ledger(pool, out).await {
        Ok(report) => {
            let _ = writeln!(
                stdout,
                "exported {} ledger file(s) written, {} already there",
                report.written, report.unchanged
            );
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "error: admin: {error}");
            1
        }
    }
}

async fn run_replay_erasures(
    workspace: Option<&str>,
    from: Option<&Path>,
    root: &zirv_flow_store::HmacRoot,
    pool: &PgPool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let context = zirv_flow_api::admin::Context::system();
    match zirv_flow_api::admin::replay_erasures(pool, &context, workspace, root, from).await {
        Ok(report) => {
            let _ = write!(
                stdout,
                "replayed {} erasure request(s): {} suppression(s) added, {} person(s) erased, \
                 {} pending inbox row(s) removed",
                report.requests, report.suppressions, report.people, report.inbox_rows
            );
            if from.is_some() {
                let _ = write!(
                    stdout,
                    "; {} ledger entr(ies) merged, {} skipped for an unknown workspace",
                    report.merged, report.skipped_workspaces
                );
            }
            let _ = writeln!(stdout);
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "error: admin: {error}");
            1
        }
    }
}

async fn connect(config: &Config) -> Result<PgPool, zirv_flow_store::StoreError> {
    zirv_flow_store::connect(config.database_url(), config.database_max_connections()).await
}

async fn run_migrate(config: &Config, stdout: &mut impl Write, stderr: &mut impl Write) -> u8 {
    let pool = match connect(config).await {
        Ok(pool) => pool,
        Err(error) => return database_error(stderr, error),
    };
    match zirv_flow_store::migrate(&pool).await {
        Ok(versions) if versions.is_empty() => {
            let _ = writeln!(stdout, "database schema is up to date");
            0
        }
        Ok(versions) => {
            for version in versions {
                let _ = writeln!(stdout, "applied migration {version}");
            }
            0
        }
        Err(error) => database_error(stderr, error),
    }
}

fn database_error(stderr: &mut impl Write, error: impl std::fmt::Display) -> u8 {
    let _ = writeln!(stderr, "error: database: {error}");
    1
}

#[derive(Debug, Eq, PartialEq)]
enum StartupAction {
    Proceed,
    Refuse(String),
}

/// Decides startup when migrate-on-start is off and the schema was inspected without a lock.
fn startup_action(state: SchemaState) -> StartupAction {
    match state {
        SchemaState::UpToDate => StartupAction::Proceed,
        SchemaState::Pending { count } => StartupAction::Refuse(format!(
            "{count} pending migration(s); run `zirv-flow migrate` or set database.migrate_on_start"
        )),
        SchemaState::Newer { version } => StartupAction::Refuse(format!(
            "database schema version {version} is newer than this binary supports; upgrade zirv-flow"
        )),
        SchemaState::Dirty { version } => StartupAction::Refuse(format!(
            "migration {version} is partially applied; repair the database and rerun `zirv-flow migrate`"
        )),
        SchemaState::Modified { version } => StartupAction::Refuse(format!(
            "migration {version} was changed after it was applied; restore the original migration file"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database_env(url: &str) -> Vec<(OsString, OsString)> {
        vec![("ZIRV_FLOW_DATABASE__URL".into(), url.into())]
    }

    #[tokio::test]
    async fn config_check_redacts_the_database_password() {
        let cli = Cli {
            config: None,
            command: Some(Command::Config(ConfigCommand::Check)),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(
            cli,
            database_env("postgres://user:hunter2@localhost/database"),
            &mut io::empty(),
            &mut stdout,
            &mut stderr,
        )
        .await;
        let output = String::from_utf8(stdout).unwrap();

        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        assert!(!output.contains("hunter2"));
        assert!(output.contains("***"));
    }

    #[tokio::test]
    async fn config_check_redacts_the_oidc_client_secret() {
        let cli = Cli {
            config: None,
            command: Some(Command::Config(ConfigCommand::Check)),
        };
        let mut env = database_env("postgres://localhost/database");
        env.extend([
            (
                "ZIRV_FLOW_SERVER__PUBLIC_URL".into(),
                "https://flow.example.com".into(),
            ),
            (
                "ZIRV_FLOW_AUTH__OIDC__ISSUER_URL".into(),
                "https://idp.example.com".into(),
            ),
            ("ZIRV_FLOW_AUTH__OIDC__CLIENT_ID".into(), "zirv-flow".into()),
            (
                "ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET".into(),
                "oidc-secret-value".into(),
            ),
        ]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut stdout, &mut stderr).await;
        let output = String::from_utf8(stdout).unwrap();

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert!(!output.contains("oidc-secret-value"));
        assert!(output.contains("client_secret = \"***\""));
        assert!(output.contains("public_url = \"https://flow.example.com\""));
    }

    #[tokio::test]
    async fn healthcheck_probes_the_configured_bind_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
            let _ =
                tokio::io::AsyncWriteExt::write_all(&mut stream, b"HTTP/1.1 200 OK\r\n\r\n").await;
        });
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_SERVER__BIND".into(), addr.to_string().into()));
        let cli = Cli {
            config: None,
            command: Some(Command::Healthcheck),
        };
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut Vec::new(), &mut stderr).await;

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
    }

    #[tokio::test]
    async fn config_check_shows_the_log_section_with_its_overrides() {
        let cli = Cli {
            config: None,
            command: Some(Command::Config(ConfigCommand::Check)),
        };
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_LOG__FILTER".into(), "warn".into()));
        env.push(("ZIRV_FLOW_LOG__FORMAT".into(), "text".into()));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut stdout, &mut stderr).await;
        let output = String::from_utf8(stdout).unwrap();

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert!(
            output.contains("[log]\nfilter = \"warn\"\nformat = \"text\""),
            "{output}"
        );
    }

    #[tokio::test]
    async fn config_check_shows_the_otlp_endpoint_and_a_bad_one_exits_with_code_two() {
        let check = |endpoint: &str| {
            let cli = Cli {
                config: None,
                command: Some(Command::Config(ConfigCommand::Check)),
            };
            let mut env = database_env("postgres://localhost/database");
            env.push(("ZIRV_FLOW_LOG__OTLP__ENDPOINT".into(), endpoint.into()));
            (cli, env)
        };
        let (cli, env) = check("http://collector:4318");
        let mut stdout = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut stdout, &mut Vec::new()).await;

        assert_eq!(code, 0);
        assert!(String::from_utf8(stdout).unwrap().contains(
            "[log.otlp]
endpoint = \"http://collector:4318/\""
        ));
        let (cli, env) = check("ftp://collector");
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut Vec::new(), &mut stderr).await;

        assert_eq!(code, 2);
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("log.otlp.endpoint")
        );
    }

    #[tokio::test]
    async fn flushing_without_an_exporter_returns_at_once() {
        tokio::time::timeout(Duration::from_secs(1), flush_traces(None))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_invalid_log_filter_exits_with_code_two() {
        let cli = Cli {
            config: None,
            command: Some(Command::Serve),
        };
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_LOG__FILTER".into(), "zirv=notalevel".into()));
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut Vec::new(), &mut stderr).await;

        assert_eq!(code, 2);
        assert!(String::from_utf8(stderr).unwrap().contains("log.filter"));
    }

    #[tokio::test]
    async fn a_configuration_error_exits_with_code_two() {
        let cli = Cli {
            config: None,
            command: Some(Command::Serve),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(cli, [], &mut io::empty(), &mut stdout, &mut stderr).await;

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("database.url"));
    }

    #[test]
    fn the_password_is_one_line_without_its_line_ending() {
        let read = |input: &str| read_password(&mut input.as_bytes());

        assert_eq!(read("secret phrase\n").unwrap(), "secret phrase");
        assert_eq!(read("secret phrase\r\n").unwrap(), "secret phrase");
        assert_eq!(read("secret phrase").unwrap(), "secret phrase");
        assert_eq!(read(" padded \nnext line\n").unwrap(), " padded ");
        assert_eq!(read("\n").unwrap(), "");
    }

    #[test]
    fn a_missing_password_is_an_error() {
        assert_eq!(
            read_password(&mut io::empty()),
            Err("no password on standard input")
        );
        assert_eq!(
            read_password(&mut &[0xff, 0xfe, b'\n'][..]),
            Err("could not read the password from standard input")
        );
    }

    /// A fresh directory under the system temp directory, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("zirv-flow-key-{}-{id}", std::process::id())))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn generate(out: &Path, if_missing: bool) -> (u8, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = generate_master_key(out, if_missing, &mut stdout, &mut stderr);
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn the_test_key_is_a_valid_master_key() {
        assert!(zirv_flow_store::MasterKey::parse(TEST_MASTER_KEY).is_ok());
    }

    #[test]
    fn generate_master_key_creates_the_file_and_prints_only_the_key_id() {
        let dir = TempDir::new();
        let out = dir.0.join("nested").join("master.key");

        let (code, stdout, stderr) = generate(&out, false);

        assert_eq!(code, 0, "{stderr}");
        let contents = fs::read_to_string(&out).unwrap();
        assert!(contents.ends_with('\n'));
        let key = zirv_flow_store::MasterKey::parse(&contents).unwrap();
        assert_eq!(stdout, format!("master key written: {}\n", key.key_id()));
        assert!(!stdout.contains(contents.trim()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&out).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn generate_master_key_refuses_to_overwrite_without_if_missing() {
        let dir = TempDir::new();
        let out = dir.0.join("master.key");
        generate(&out, false);
        let before = fs::read_to_string(&out).unwrap();

        let (code, stdout, stderr) = generate(&out, false);

        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        assert!(stderr.contains("already exists"), "{stderr}");
        assert_eq!(fs::read_to_string(&out).unwrap(), before);
    }

    #[test]
    fn if_missing_keeps_a_valid_key_and_rejects_an_invalid_file() {
        let dir = TempDir::new();
        let out = dir.0.join("master.key");
        let (_, written, _) = generate(&out, true);
        let id = written.trim().strip_prefix("master key written: ").unwrap();
        let before = fs::read_to_string(&out).unwrap();

        let (code, stdout, _) = generate(&out, true);

        assert_eq!(code, 0);
        assert_eq!(stdout, format!("master key exists: {id}\n"));
        assert_eq!(fs::read_to_string(&out).unwrap(), before);

        fs::write(&out, "hunter2 is not a key\n").unwrap();
        let (code, stdout, stderr) = generate(&out, true);

        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        assert!(stderr.contains("not a base64 32-byte key"), "{stderr}");
        assert!(!stderr.contains("hunter2"));
    }

    #[tokio::test]
    async fn generate_master_key_runs_without_any_configuration() {
        let dir = TempDir::new();
        let out = dir.0.join("master.key");
        let cli = Cli {
            config: None,
            command: Some(Command::Admin(AdminCommand::GenerateMasterKey {
                out: out.clone(),
                if_missing: false,
            })),
        };
        let mut stdout = Vec::new();

        let code = run(cli, [], &mut io::empty(), &mut stdout, &mut Vec::new()).await;

        assert_eq!(code, 0);
        assert!(out.exists());
    }

    async fn start_without_database(command: Command, key: Option<(&str, &str)>) -> (u8, String) {
        let cli = Cli {
            config: None,
            command: Some(command),
        };
        let mut env = database_env("postgres://postgres@127.0.0.1:1/zirv_flow");
        env.extend(key.map(|(name, value)| (name.into(), value.into())));
        let (logs, _guard) = capture_logs();
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut io::sink(), &mut stderr).await;

        (
            code,
            format!("{}{}", logs.text(), String::from_utf8(stderr).unwrap()),
        )
    }

    #[tokio::test]
    async fn serve_worker_and_all_refuse_to_start_without_a_master_key() {
        for command in [Command::Serve, Command::Worker, Command::All] {
            let (code, output) = start_without_database(command, None).await;

            assert_eq!(code, 2, "{output}");
            assert!(
                output.contains(
                    "the master key is required for serve and worker: set secrets.master_key_file or ZIRV_FLOW_SECRETS__MASTER_KEY; create one with zirv-flow admin generate-master-key --out <path>"
                ),
                "{output}"
            );
        }
    }

    #[tokio::test]
    async fn replay_erasures_needs_the_master_key_and_asks_for_no_password() {
        let (code, output) = start_without_database(
            Command::Admin(AdminCommand::ReplayErasures {
                workspace: None,
                from: None,
            }),
            None,
        )
        .await;

        assert_eq!(code, 2, "{output}");
        assert!(
            output.contains("error: admin: the master key is required"),
            "{output}"
        );
        assert!(!output.contains("no password"), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_erasures_prints_counts_only_and_refuses_an_unknown_workspace(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let key = zirv_flow_store::MasterKey::generate();
        let context = zirv_flow_api::admin::Context::system();
        let ws = zirv_flow_api::admin::create_workspace(&pool, &context, "ours", None)
            .await
            .unwrap()
            .id;
        // A ledger entry for a person who is back after a restore.
        let person = sqlx::types::Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO erasure_requests \
                (id, workspace_id, person_id, identifier_hashes, identifier_kinds, status, \
                 completed_at, trace_id) \
             VALUES (gen_random_uuid(), $1, $2, $3, ARRAY['email'], 'completed', now(), \
                     gen_random_uuid())",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(vec![vec![7_u8; 32]])
        .execute(&pool)
        .await
        .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let key = zirv_flow_store::HmacRoot::bootstrap_from(&key);

        let code =
            run_replay_erasures(Some("ours"), None, &key, &pool, &mut stdout, &mut stderr).await;
        let unknown =
            run_replay_erasures(Some("nope"), None, &key, &pool, &mut stdout, &mut stderr).await;

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "replayed 1 erasure request(s): 1 suppression(s) added, 1 person(s) erased, \
             0 pending inbox row(s) removed\n"
        );
        assert_eq!(unknown, 1);
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "error: admin: no such workspace\n"
        );
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 0);
        let audited: Vec<String> = zirv_flow_store::list_audit(&pool, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.action)
            .collect();
        assert!(audited.contains(&"privacy.erasures_replayed".to_owned()));
    }

    struct LedgerDir(std::path::PathBuf);

    impl LedgerDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("zirv-flow-ledger-{}", uuid::Uuid::now_v7()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for LedgerDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn ledger_config(dir: Option<&Path>) -> Config {
        let mut env = database_env("postgres://localhost/database");
        if let Some(dir) = dir {
            env.push((
                "ZIRV_FLOW_PRIVACY__LEDGER_DIR".into(),
                format!("{:?}", dir.display().to_string()).into(),
            ));
        }
        Config::load(None, env).unwrap()
    }

    #[test]
    fn the_startup_check_passes_when_the_key_is_unset_or_the_directory_is_writable() {
        let dir = LedgerDir::new();

        assert_eq!(check_ledger_dir(&ledger_config(None)), Ok(()));
        assert_eq!(check_ledger_dir(&ledger_config(Some(&dir.0))), Ok(()));
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn the_startup_check_refuses_an_unwritable_directory_naming_the_key_and_the_path() {
        let dir = LedgerDir::new();
        let not_a_directory = dir.0.join("plain-file");
        fs::write(&not_a_directory, b"x").unwrap();
        let (logs, _guard) = capture_logs();

        for bad in [not_a_directory, dir.0.join("absent")] {
            assert_eq!(check_ledger_dir(&ledger_config(Some(&bad))), Err(1));
            let lines = logs.json_lines();
            let last = lines.last().unwrap();
            assert_eq!(last["level"], "ERROR");
            assert_eq!(last["fields"]["key"], "privacy.ledger_dir");
            assert_eq!(last["fields"]["path"], bad.display().to_string());
        }
    }

    /// A scratch database on the server of `pool`, migrated, with the workspace when one is given.
    async fn scratch_database(
        pool: &PgPool,
        workspace: zirv_flow_core::WorkspaceId,
    ) -> (PgPool, String) {
        let name = format!("zirv_flow_restore_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(pool)
            .await
            .unwrap();
        let options = pool.connect_options().as_ref().clone().database(&name);
        let scratch = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        zirv_flow_store::migrate(&scratch).await.unwrap();
        sqlx::query("INSERT INTO workspaces (id, name, slug) VALUES ($1, 'ours', 'ours')")
            .bind(workspace.as_uuid())
            .execute(&scratch)
            .await
            .unwrap();
        (scratch, name)
    }

    async fn drop_scratch(pool: &PgPool, scratch: PgPool, name: &str) {
        scratch.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
        )))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_ada(pool: &PgPool, ws: zirv_flow_core::WorkspaceId) -> uuid::Uuid {
        let outcomes = zirv_flow_store::apply_profile_batch(
            pool,
            ws,
            &[zirv_flow_store::ProfileRecord {
                source: "crm".into(),
                stream: "s".into(),
                record_key: "1".into(),
                identifiers: vec![("email".into(), "ada@example.com".into())],
                traits: serde_json::Map::new(),
                observed_at: time::OffsetDateTime::now_utc(),
                trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
            }],
        )
        .await
        .unwrap();
        let zirv_flow_store::ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        person_id
    }

    async fn ours(pool: &PgPool) -> zirv_flow_core::WorkspaceId {
        zirv_flow_store::create_workspace(
            pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replaying_from_a_directory_re_erases_a_person_a_restored_backup_brings_back(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let root =
            zirv_flow_store::HmacRoot::bootstrap_from(&zirv_flow_store::MasterKey::generate());
        let key = root.hmac_key(zirv_flow_store::HmacPurpose::Identifier);
        let ws = ours(&pool).await;
        let person = seed_ada(&pool, ws).await;
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t"));
        // The live database erases the person and copies the ledger entry to the directory.
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let request = zirv_flow_store::request_erasure(&mut tx, ws, person, &key, None, &trace)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::erase_person(&mut tx, ws, request.id, &key, &[], &trace)
            .await
            .unwrap();
        let entry = zirv_flow_store::find_ledger_entry(&mut tx, ws, request.id)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let dir = LedgerDir::new();
        zirv_flow_store::write_ledger_file(&dir.0, &entry).unwrap();
        // An older backup: the same workspace and person, and no ledger row.
        let (restored, name) = scratch_database(&pool, ws).await;
        // The restore brings back the same person id.
        let back = person;
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(back)
            .bind(ws.as_uuid())
            .execute(&restored)
            .await
            .unwrap();
        let hash = zirv_flow_core::people::identifier_hash(
            &key,
            &zirv_flow_core::people::Identifier::parse("email", "ada@example.com").unwrap(),
        );
        assert!(
            zirv_flow_store::find_person(&restored, ws, back)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            !zirv_flow_store::is_suppressed(&restored, ws, &hash)
                .await
                .unwrap()
        );
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_replay_erasures(
            None,
            Some(&dir.0),
            &root,
            &restored,
            &mut stdout,
            &mut stderr,
        )
        .await;

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert!(
            zirv_flow_store::find_person(&restored, ws, back)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            zirv_flow_store::is_suppressed(&restored, ws, &hash)
                .await
                .unwrap()
        );
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "replayed 1 erasure request(s): 1 suppression(s) added, 1 person(s) erased, \
             0 pending inbox row(s) removed; 1 ledger entr(ies) merged, \
             0 skipped for an unknown workspace\n"
        );
        drop_scratch(&pool, restored, &name).await;
    }

    fn ledger_entry(workspace: uuid::Uuid) -> zirv_flow_store::LedgerEntry {
        zirv_flow_store::LedgerEntry {
            format: zirv_flow_store::LEDGER_FORMAT.to_owned(),
            id: uuid::Uuid::now_v7(),
            workspace_id: workspace,
            person_id: uuid::Uuid::now_v7(),
            person_ids: Vec::new(),
            identifier_hashes: vec!["07".repeat(32)],
            identifier_kinds: vec!["email".to_owned()],
            requested_by: None,
            requested_at: time::OffsetDateTime::now_utc(),
            completed_at: time::OffsetDateTime::now_utc(),
            trace_id: uuid::Uuid::now_v7(),
            restore_note: String::new(),
        }
    }

    async fn ledger_rows(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM erasure_requests")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replay_from_an_empty_a_good_and_a_corrupt_directory(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let root =
            zirv_flow_store::HmacRoot::bootstrap_from(&zirv_flow_store::MasterKey::generate());
        let ws = ours(&pool).await;
        let empty = LedgerDir::new();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code =
            run_replay_erasures(None, Some(&empty.0), &root, &pool, &mut stdout, &mut stderr).await;

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert!(
            String::from_utf8(stdout)
                .unwrap()
                .ends_with("; 0 ledger entr(ies) merged, 0 skipped for an unknown workspace\n")
        );

        // A good file for this workspace and one for a workspace this database lacks.
        let good = LedgerDir::new();
        zirv_flow_store::write_ledger_file(&good.0, &ledger_entry(ws.as_uuid())).unwrap();
        zirv_flow_store::write_ledger_file(&good.0, &ledger_entry(uuid::Uuid::now_v7())).unwrap();
        let mut stdout = Vec::new();
        let code =
            run_replay_erasures(None, Some(&good.0), &root, &pool, &mut stdout, &mut stderr).await;
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert!(
            String::from_utf8(stdout)
                .unwrap()
                .ends_with("; 1 ledger entr(ies) merged, 1 skipped for an unknown workspace\n")
        );
        assert_eq!(ledger_rows(&pool).await, 1);

        // One corrupt file stops the command, naming it; the good file beside it is not merged.
        let mixed = LedgerDir::new();
        zirv_flow_store::write_ledger_file(&mixed.0, &ledger_entry(ws.as_uuid())).unwrap();
        fs::write(mixed.0.join("broken.json"), b"{ not json").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code =
            run_replay_erasures(None, Some(&mixed.0), &root, &pool, &mut stdout, &mut stderr).await;
        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "error: admin: broken.json is not a valid ledger file\n"
        );
        assert_eq!(ledger_rows(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn exporting_the_ledger_round_trips_into_a_fresh_database(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let root =
            zirv_flow_store::HmacRoot::bootstrap_from(&zirv_flow_store::MasterKey::generate());
        let ws = ours(&pool).await;
        for status in ["completed", "pending"] {
            sqlx::query(
                "INSERT INTO erasure_requests \
                    (id, workspace_id, person_id, identifier_hashes, identifier_kinds, status, \
                     completed_at, trace_id) \
                 VALUES (gen_random_uuid(), $1, gen_random_uuid(), $2, ARRAY['email'], $3, now(), \
                         gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(vec![vec![7_u8; 32]])
            .bind(status)
            .execute(&pool)
            .await
            .unwrap();
        }
        let dir = LedgerDir::new();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_export_ledger(&dir.0, &pool, &mut stdout, &mut stderr).await;

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "exported 1 ledger file(s) written, 0 already there\n"
        );
        let (fresh, name) = scratch_database(&pool, ws).await;
        let code = run_replay_erasures(
            None,
            Some(&dir.0),
            &root,
            &fresh,
            &mut Vec::new(),
            &mut stderr,
        )
        .await;
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        let completed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM erasure_requests WHERE status = 'completed'")
                .fetch_one(&fresh)
                .await
                .unwrap();
        assert_eq!(completed, 1);
        let mut stdout = Vec::new();
        let again = run_export_ledger(&dir.0, &pool, &mut stdout, &mut stderr).await;
        assert_eq!(again, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "exported 0 ledger file(s) written, 1 already there\n"
        );
        let missing =
            run_export_ledger(&dir.0.join("absent"), &pool, &mut Vec::new(), &mut stderr).await;
        assert_eq!(missing, 1);
        drop_scratch(&pool, fresh, &name).await;
    }

    #[tokio::test]
    async fn the_scheduler_needs_no_master_key() {
        let (code, output) = start_without_database(Command::Scheduler, None).await;

        // It got past the key check and failed on the unreachable database instead.
        assert_eq!(code, 1, "{output}");
        assert!(!output.contains("master key"), "{output}");
    }

    #[tokio::test]
    async fn an_invalid_master_key_is_refused_without_echoing_it() {
        let (code, output) = start_without_database(
            Command::Serve,
            Some(("ZIRV_FLOW_SECRETS__MASTER_KEY", "hunter2-not-a-key")),
        )
        .await;

        assert_eq!(code, 2, "{output}");
        assert!(output.contains("secrets.master_key: not a base64 32-byte key"));
        assert!(!output.contains("hunter2"));
    }

    #[tokio::test]
    async fn an_unreadable_master_key_file_is_refused_with_its_path() {
        let missing = TempDir::new().0.join("absent.key");
        let (code, output) = start_without_database(
            Command::Worker,
            Some((
                "ZIRV_FLOW_SECRETS__MASTER_KEY_FILE",
                missing.to_str().unwrap(),
            )),
        )
        .await;

        assert_eq!(code, 2, "{output}");
        assert!(
            output.contains(&format!(
                "secrets.master_key_file {}: not readable",
                missing.display()
            )),
            "{output}"
        );
    }

    #[tokio::test]
    async fn a_valid_master_key_is_logged_by_id_only_and_the_start_goes_on() {
        let dir = TempDir::new();
        let out = dir.0.join("master.key");
        generate(&out, false);
        let text = fs::read_to_string(&out).unwrap();
        let id = zirv_flow_store::MasterKey::parse(&text).unwrap().key_id();

        let (code, output) = start_without_database(
            Command::Serve,
            Some(("ZIRV_FLOW_SECRETS__MASTER_KEY_FILE", out.to_str().unwrap())),
        )
        .await;

        // Past the key check; the database is unreachable.
        assert_eq!(code, 1, "{output}");
        assert!(output.contains("master key loaded"), "{output}");
        assert!(output.contains(&id), "{output}");
        assert!(!output.contains(text.trim()), "{output}");
    }

    #[tokio::test]
    async fn config_check_redacts_the_inline_master_key() {
        let cli = Cli {
            config: None,
            command: Some(Command::Config(ConfigCommand::Check)),
        };
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_SECRETS__MASTER_KEY".into(),
            TEST_MASTER_KEY.into(),
        ));
        let mut stdout = Vec::new();
        let code = run(cli, env, &mut io::empty(), &mut stdout, &mut Vec::new()).await;
        let output = String::from_utf8(stdout).unwrap();

        assert_eq!(code, 0);
        assert!(!output.contains(TEST_MASTER_KEY), "{output}");
        assert!(output.contains("master_key = \"<redacted>\""), "{output}");
    }

    #[tokio::test]
    async fn admin_commands_fail_without_a_password_before_touching_the_database() {
        let cli = Cli {
            config: None,
            command: Some(Command::Admin(AdminCommand::CreateOwner {
                email: "owner@example.com".into(),
            })),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run(
            cli,
            database_env("postgres://postgres@127.0.0.1:1/zirv_flow"),
            &mut io::empty(),
            &mut stdout,
            &mut stderr,
        )
        .await;

        assert_eq!(code, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "error: admin: no password on standard input\n"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn create_owner_reads_the_password_and_a_second_owner_is_refused(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let create = |email: &str| AdminCommand::CreateOwner {
            email: email.to_owned(),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let first = run_admin(
            &create("Owner@Example.com"),
            &pool,
            "correct horse battery",
            &mut stdout,
            &mut stderr,
        )
        .await;
        let second = run_admin(
            &create("other@example.com"),
            &pool,
            "correct horse battery",
            &mut stdout,
            &mut stderr,
        )
        .await;

        assert_eq!(first, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "owner created\n");
        assert_eq!(second, 1);
        let error = String::from_utf8(stderr).unwrap();
        assert_eq!(error, "error: admin: an owner already exists\n");
        assert!(!error.contains("correct horse battery"));
        let users = zirv_flow_store::list_users(&pool).await.unwrap();
        assert_eq!(users.len(), 1);
        assert!(users[0].is_owner);
        assert_eq!(users[0].email, "owner@example.com");
        assert!(
            !users[0]
                .password_hash
                .as_deref()
                .unwrap()
                .contains("correct horse battery")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn create_owner_rejects_a_short_password_and_reset_needs_an_existing_user(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let short = run_admin(
            &AdminCommand::CreateOwner {
                email: "owner@example.com".into(),
            },
            &pool,
            "short",
            &mut stdout,
            &mut stderr,
        )
        .await;
        let reset = run_admin(
            &AdminCommand::ResetPassword {
                email: "nobody@example.com".into(),
            },
            &pool,
            "correct horse battery",
            &mut stdout,
            &mut stderr,
        )
        .await;

        assert_eq!((short, reset), (1, 1));
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.contains("password: must be 12 to 1024 characters"));
        assert!(error.contains("no such user"));
        assert!(!error.contains("short\n"));
        assert!(zirv_flow_store::list_users(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reset_password_changes_the_password_of_an_existing_user(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_admin(
            &AdminCommand::CreateOwner {
                email: "owner@example.com".into(),
            },
            &pool,
            "correct horse battery",
            &mut stdout,
            &mut stderr,
        )
        .await;

        let before = zirv_flow_store::list_users(&pool).await.unwrap()[0]
            .password_hash
            .clone();

        let code = run_admin(
            &AdminCommand::ResetPassword {
                email: "owner@example.com".into(),
            },
            &pool,
            "another long password",
            &mut stdout,
            &mut stderr,
        )
        .await;

        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        let after = zirv_flow_store::list_users(&pool).await.unwrap()[0]
            .password_hash
            .clone();
        assert_ne!(before, after);
        let after = after.unwrap();
        assert!(after.starts_with("$argon2id$"));
        assert!(!after.contains("another long password"));
    }

    #[test]
    fn an_up_to_date_schema_proceeds() {
        assert_eq!(
            startup_action(SchemaState::UpToDate),
            StartupAction::Proceed
        );
    }

    #[test]
    fn pending_migrations_are_refused_when_disabled() {
        assert_eq!(
            startup_action(SchemaState::Pending { count: 2 }),
            StartupAction::Refuse(
                "2 pending migration(s); run `zirv-flow migrate` or set database.migrate_on_start"
                    .into()
            )
        );
    }

    #[test]
    fn a_newer_schema_is_refused() {
        assert_eq!(
            startup_action(SchemaState::Newer { version: 12 }),
            StartupAction::Refuse(
                "database schema version 12 is newer than this binary supports; upgrade zirv-flow"
                    .into()
            )
        );
    }

    #[test]
    fn a_dirty_migration_is_refused() {
        assert_eq!(
            startup_action(SchemaState::Dirty { version: 7 }),
            StartupAction::Refuse(
                "migration 7 is partially applied; repair the database and rerun `zirv-flow migrate`"
                    .into()
            )
        );
    }

    #[test]
    fn a_modified_migration_is_refused() {
        assert_eq!(
            startup_action(SchemaState::Modified { version: 3 }),
            StartupAction::Refuse(
                "migration 3 was changed after it was applied; restore the original migration file"
                    .into()
            )
        );
    }

    /// 32 bytes of 0x07, as base64.
    const TEST_MASTER_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap()
    }

    #[tokio::test]
    async fn the_server_binary_has_no_test_routes() {
        use tower::ServiceExt;

        let app = zirv_flow_api::router(zirv_flow_api::AppState::new(
            lazy_pool(),
            Arc::new(String::new),
        ));
        let send = |method: &'static str, uri: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(
                    axum::http::Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        let read = send("GET", "/__test/sink").await;
        let clear = send("DELETE", "/__test/sink").await;
        let shutdown = send("POST", "/__test/shutdown").await;

        // Only the web app's history fallback answers a GET, and it answers a DELETE with 405.
        let content_type = read.headers()[axum::http::header::CONTENT_TYPE]
            .to_str()
            .unwrap();
        assert!(content_type.starts_with("text/html"), "{content_type}");
        assert_eq!(clear.status(), axum::http::StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            shutdown.status(),
            axum::http::StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn the_worker_and_the_api_share_one_set_of_rate_limit_buckets() {
        let state = state();
        let mut worker = zirv_flow_jobs::WorkerConfig::new(1);
        let own = Arc::clone(&worker.rate_limits);

        share_rate_limits(None, &mut worker);
        assert!(
            Arc::ptr_eq(&worker.rate_limits, &own),
            "a worker-only process keeps its own"
        );

        share_rate_limits(Some(&state), &mut worker);
        assert!(Arc::ptr_eq(&worker.rate_limits, state.rate_limits()));
        assert!(!Arc::ptr_eq(&worker.rate_limits, &own));
    }

    fn state() -> zirv_flow_api::AppState {
        zirv_flow_api::AppState::new(lazy_pool(), Arc::new(String::new))
    }

    /// Routes the process's logs into the returned capture until the guard drops.
    fn capture_logs() -> (log::Captured, tracing::subscriber::DefaultGuard) {
        let captured = log::Captured::default();
        let dispatch = log::dispatch("info", config::LogFormat::Json, captured.clone());
        (captured, tracing::dispatcher::set_default(&dispatch))
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_an_owner_the_setup_token_is_logged_once_with_the_setup_address(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let state = zirv_flow_api::AppState::new(pool, Arc::new(String::new));
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let (logs, _guard) = capture_logs();

        announce_setup(&state, &config).await.unwrap();

        let lines = logs.json_lines();
        assert_eq!(lines.len(), 1, "{}", logs.text());
        assert_eq!(lines[0]["level"], "WARN");
        let message = lines[0]["fields"]["message"].as_str().unwrap();
        let token = message
            .split("setup token ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap();
        assert_eq!(token.len(), 43, "{message}");
        assert!(message.starts_with(
            "no owner exists; finish setup at http://127.0.0.1:8080/setup with setup token "
        ));
        assert!(message.ends_with(", or run zirv-flow admin create-owner"));
        assert_eq!(logs.text().matches(token).count(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn with_an_owner_nothing_is_logged_and_no_token_is_minted(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        zirv_flow_api::admin::create_owner(
            &pool,
            &zirv_flow_api::admin::Context::system(),
            "owner@example.com",
            "correct horse battery",
        )
        .await
        .unwrap();
        let state = zirv_flow_api::AppState::new(pool, Arc::new(String::new));
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let (logs, _guard) = capture_logs();

        announce_setup(&state, &config).await.unwrap();

        assert!(logs.text().is_empty(), "{}", logs.text());
    }

    /// The URL of the database `sqlx::test` created for `pool`, on the server `DATABASE_URL` names.
    fn test_database_url(pool: &PgPool) -> String {
        let base = std::env::var("DATABASE_URL").expect("DATABASE_URL is set for PostgreSQL tests");
        let (server, _) = base.rsplit_once('/').unwrap();
        let database = pool.connect_options().get_database().unwrap().to_owned();
        format!("{server}/{database}")
    }

    async fn serve_against(pool: &PgPool, migrate_on_start: bool) -> (u8, String) {
        let mut env = database_env(&test_database_url(pool));
        env.push((
            "ZIRV_FLOW_SECRETS__MASTER_KEY".into(),
            TEST_MASTER_KEY.into(),
        ));
        env.push((
            "ZIRV_FLOW_DATABASE__MIGRATE_ON_START".into(),
            migrate_on_start.to_string().into(),
        ));
        let cli = Cli {
            config: None,
            command: Some(Command::Serve),
        };
        let (logs, _guard) = capture_logs();
        let mut stderr = Vec::new();

        let code = run(cli, env, &mut io::empty(), &mut io::sink(), &mut stderr).await;

        let output = format!("{}{}", logs.text(), String::from_utf8(stderr).unwrap());
        (code, output)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn serve_refuses_a_schema_newer_than_the_binary(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (999999, 'future', true, '\\x00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (code, output) = serve_against(&pool, false).await;

        assert_eq!(code, 1, "{output}");
        assert!(
            output.contains(
                "database schema version 999999 is newer than this binary supports; upgrade zirv-flow"
            ),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn serve_refuses_pending_migrations_when_migrating_on_start_is_off(pool: PgPool) {
        let (code, output) = serve_against(&pool, false).await;

        assert_eq!(code, 1, "{output}");
        assert!(output.contains("pending migration(s)"), "{output}");
        assert!(output.contains("zirv-flow migrate"), "{output}");
    }

    #[tokio::test]
    async fn an_unreachable_database_stops_the_announcement_with_exit_code_one() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let state = zirv_flow_api::AppState::new(pool, Arc::new(String::new));
        let (logs, _guard) = capture_logs();

        let result = announce_setup(&state, &config).await;

        assert_eq!(result, Err(1));
        assert_eq!(logs.json_lines()[0]["level"], "ERROR");
    }

    #[tokio::test]
    async fn a_bind_failure_is_logged_as_an_error_with_exit_code_one() {
        let (logs, _guard) = capture_logs();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();

        let code = serve_http(
            state(),
            addr,
            &ShutdownDeadline::default(),
            std::future::ready(()),
        )
        .await;

        assert_eq!(code, 1);
        let lines = logs.json_lines();
        assert_eq!(lines.len(), 1, "{}", logs.text());
        assert_eq!(lines[0]["level"], "ERROR");
        assert_eq!(
            lines[0]["fields"]["message"],
            "could not bind the server address"
        );
        assert_eq!(lines[0]["fields"]["address"], addr.to_string());
    }

    #[tokio::test]
    async fn the_server_binds_and_exits_cleanly_on_shutdown() {
        let (logs, _guard) = capture_logs();

        let code = serve_http(
            state(),
            "127.0.0.1:0".parse().unwrap(),
            &ShutdownDeadline::default(),
            std::future::ready(()),
        )
        .await;

        assert_eq!(code, 0);
        let lines = logs.json_lines();
        assert_eq!(lines[0]["level"], "INFO");
        assert!(
            lines[0]["fields"]["address"]
                .as_str()
                .unwrap()
                .starts_with("127.0.0.1:")
        );
    }

    /// The child of the pool under test: a no-op unless the pool started it.
    #[test]
    fn render_host_child() {
        if std::env::var_os("ZIRV_FLOW_TEST_RENDER_HOST").is_some() {
            std::process::exit(i32::from(zirv_flow_render::host::run(None)));
        }
    }

    #[tokio::test]
    async fn a_started_worker_and_scheduler_each_log_one_json_startup_line() {
        let (logs, _guard) = capture_logs();
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let roles = Roles {
            serve: false,
            worker: true,
            scheduler: true,
        };

        let code = run_roles(roles, &config, lazy_pool(), None, std::future::ready(())).await;

        assert_eq!(code, 0);
        let lines = logs.json_lines();
        let started = |message: &str| {
            lines
                .iter()
                .find(|line| line["fields"]["message"] == message)
                .unwrap_or_else(|| panic!("no {message:?} line in {}", logs.text()))
                .clone()
        };
        let worker = started("worker started");
        assert_eq!(worker["level"], "INFO");
        assert_eq!(worker["fields"]["role"], "worker");
        assert_eq!(worker["fields"]["concurrency"], 4);
        assert!(worker["timestamp"].is_string());
        assert_eq!(
            started("scheduler standing for leadership")["fields"]["role"],
            "scheduler"
        );
    }

    #[tokio::test]
    async fn draining_the_background_roles_stops_at_the_shared_deadline() {
        let deadline = ShutdownDeadline::default();
        // The shutdown began; the server used up most of the grace before this point.
        let ends = deadline.start();
        assert_eq!(deadline.start(), ends, "the deadline starts only once");
        let (logs, _guard) = capture_logs();
        let mut background = tokio::task::JoinSet::new();
        background.spawn(std::future::pending());

        let soon = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
        tokio::time::timeout(std::time::Duration::from_secs(10), drain(background, soon))
            .await
            .expect("the drain ends at its deadline, not a fresh grace period");

        let lines = logs.json_lines();
        assert_eq!(lines[0]["level"], "WARN");
        assert!(
            lines[0]["fields"]["message"]
                .as_str()
                .unwrap()
                .contains("jobs still running after 30s"),
            "{}",
            logs.text()
        );
    }

    #[test]
    fn long_running_commands_map_to_their_roles() {
        let roles = |command| Roles::of(&command).map(|r| (r.serve, r.worker, r.scheduler));

        assert_eq!(roles(Command::All), Some((true, true, true)));
        assert_eq!(roles(Command::Serve), Some((true, false, false)));
        assert_eq!(roles(Command::Worker), Some((false, true, false)));
        assert_eq!(roles(Command::Scheduler), Some((false, false, true)));
        assert_eq!(roles(Command::Migrate), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_worker_and_scheduler_roles_do_their_work_and_drain_on_shutdown(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, kind, trace_id, cause_kind, cause_id) \
             VALUES (gen_random_uuid(), 'maintenance.noop', gen_random_uuid(), 'manual', '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (logs, _guard) = capture_logs();
        // A schedule that is due, so the scheduler has a slot to fire.
        zirv_flow_store::create_schedule(
            &pool,
            &zirv_flow_store::NewSchedule {
                workspace_id: None,
                kind: "maintenance.noop".into(),
                cron: "0 0 1 1 *".into(),
                timezone: "UTC".into(),
                next_run_at: zirv_flow_store::database_now(&pool).await.unwrap()
                    - time::Duration::hours(1),
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        let (shutdown, shut_down) = tokio::sync::oneshot::channel::<()>();
        let roles = Roles {
            serve: false,
            worker: true,
            scheduler: true,
        };
        let run = tokio::spawn({
            let pool = pool.clone();
            async move {
                run_roles(roles, &config, pool, None, async move {
                    let _ = shut_down.await;
                })
                .await
            }
        });

        // The scheduler fires the due slot (moving the schedule into the future) and the
        // worker runs both jobs.
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            assert!(
                std::time::Instant::now() < give_up,
                "timed out after 60s waiting for the scheduler and worker to finish"
            );
            let done: bool = sqlx::query_scalar(
                "SELECT (SELECT next_run_at > now() FROM schedules WHERE kind = 'maintenance.noop') \
                    AND NOT EXISTS (SELECT 1 FROM jobs)",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        shutdown.send(()).unwrap();
        let code = run.await.unwrap();

        assert_eq!(code, 0, "{}", logs.text());
    }

    fn ops_config(bind: Option<&str>) -> Config {
        let mut env = database_env("postgres://localhost/database");
        if let Some(bind) = bind {
            env.push(("ZIRV_FLOW_OPS__BIND".into(), bind.into()));
        }
        Config::load(None, env).unwrap()
    }

    const WORKER_ONLY: Roles = Roles {
        serve: false,
        worker: true,
        scheduler: false,
    };

    #[tokio::test]
    async fn a_wrapped_role_that_returns_makes_the_process_not_ready() {
        let liveness = Arc::new(RoleLiveness::default());
        let (_keep, pending) = tokio::sync::oneshot::channel::<()>();
        let still_running = tokio::spawn(liveness.track(Role::Scheduler, async move {
            let _ = pending.await;
        }));
        tokio::task::yield_now().await;
        assert_eq!(liveness.not_ready(), None);

        liveness.track(Role::Worker, async {}).await;

        assert_eq!(liveness.not_ready(), Some("the worker loop has stopped"));
        still_running.abort();
    }

    #[tokio::test]
    async fn readyz_through_the_real_wiring_reports_a_stopped_loop_then_shutdown() {
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let liveness = Arc::new(RoleLiveness::default());
        liveness.track(Role::Worker, async {}).await;
        let state = zirv_flow_api::OpsState::new(
            lazy_pool(),
            Arc::new(String::new),
            role_readiness(stopped, liveness),
        );
        let ops = start_ops("127.0.0.1:0".parse().unwrap(), state)
            .await
            .unwrap();

        let (status, body) = fetch(ops.address, "/readyz").await.unwrap();
        assert!(status.contains("503"), "{status}");
        assert!(body.contains("the worker loop has stopped"), "{body}");
        stop.send(true).unwrap();
        let (status, body) = fetch(ops.address, "/readyz").await.unwrap();
        assert!(status.contains("503"), "{status}");
        assert!(body.contains("shutting down"), "{body}");
        ops.stop().await;
    }

    #[tokio::test]
    async fn a_stopped_scheduler_loop_is_named() {
        let liveness = Arc::new(RoleLiveness::default());

        liveness.track(Role::Scheduler, async {}).await;

        assert_eq!(liveness.not_ready(), Some("the scheduler loop has stopped"));
    }

    #[test]
    fn the_ops_listener_is_off_unless_bound_and_never_runs_beside_serve() {
        let (logs, _guard) = capture_logs();
        let serving = Roles {
            serve: true,
            worker: true,
            scheduler: true,
        };

        assert_eq!(ops_bind(WORKER_ONLY, &ops_config(None)), None);
        assert_eq!(
            ops_bind(WORKER_ONLY, &ops_config(Some("127.0.0.1:9100"))),
            Some("127.0.0.1:9100".parse().unwrap())
        );
        assert!(logs.json_lines().is_empty(), "{}", logs.text());
        assert_eq!(ops_bind(serving, &ops_config(Some("127.0.0.1:9100"))), None);

        let lines = logs.json_lines();
        assert_eq!(lines.len(), 1, "{}", logs.text());
        assert_eq!(lines[0]["level"], "WARN");
        assert!(
            lines[0]["fields"]["message"]
                .as_str()
                .unwrap()
                .contains("server.bind")
        );
    }

    #[tokio::test]
    async fn a_taken_ops_address_is_logged_as_an_error_with_exit_code_one() {
        let (logs, _guard) = capture_logs();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();
        let state =
            zirv_flow_api::OpsState::new(lazy_pool(), Arc::new(String::new), Arc::new(|| None));

        let result = start_ops(addr, state).await;

        assert_eq!(result.err(), Some(1));
        let lines = logs.json_lines();
        assert_eq!(lines[0]["level"], "ERROR");
        assert_eq!(
            lines[0]["fields"]["message"],
            "could not bind the ops address"
        );
    }

    /// One `GET` over a fresh connection: the status line and the body, or `None` when the
    /// connection could not be made or yielded nothing.
    async fn fetch(addr: SocketAddr, path: &str) -> Option<(String, String)> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
        let request = format!("GET {path} HTTP/1.1\r\nHost: ops\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).await.ok()?;
        let (head, body) = response.split_once("\r\n\r\n")?;
        Some((head.lines().next()?.to_owned(), body.to_owned()))
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_only_process_serves_metrics_and_readiness_on_the_ops_listener(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, kind, trace_id, cause_kind, cause_id) \
             VALUES (gen_random_uuid(), 'maintenance.noop', gen_random_uuid(), 'manual', '')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let config = ops_config(Some("127.0.0.1:0"));
        let (shutdown, shut_down) = tokio::sync::oneshot::channel::<()>();
        let (bound, bound_to) = tokio::sync::oneshot::channel();
        let run = tokio::spawn({
            let pool = pool.clone();
            async move {
                run_roles_reporting(
                    WORKER_ONLY,
                    &config,
                    pool,
                    None,
                    async move {
                        let _ = shut_down.await;
                    },
                    Some(bound),
                )
                .await
            }
        });
        let addr = bound_to.await.expect("the ops listener started");

        let give_up = std::time::Instant::now() + Duration::from_secs(60);
        let metrics = loop {
            let (status, body) = fetch(addr, "/metrics").await.expect("/metrics answers");
            assert!(status.contains("200"), "{status}");
            if body.contains("zirv_flow_jobs_total") {
                break body;
            }
            assert!(
                std::time::Instant::now() < give_up,
                "timed out waiting for a job metric"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(
            metrics.contains("zirv_flow_db_pool_connections"),
            "{metrics}"
        );
        let (status, body) = fetch(addr, "/readyz").await.unwrap();
        assert!(status.contains("200"), "{status} {body}");
        assert_eq!(body.trim(), r#"{"status":"ready"}"#);
        let (status, _) = fetch(addr, "/healthz").await.unwrap();
        assert!(status.contains("200"), "{status}");

        shutdown.send(()).unwrap();
        if let Some((status, body)) = fetch(addr, "/readyz").await {
            assert!(status.contains("503"), "{status}");
            assert!(body.contains("shutting down"), "{body}");
        }
        assert_eq!(run.await.unwrap(), 0);
        assert!(
            tokio::net::TcpStream::connect(addr).await.is_err(),
            "the listener stops when the drain ends"
        );
    }

    fn keyring_of(
        current: &zirv_flow_store::MasterKey,
        previous: Option<&zirv_flow_store::MasterKey>,
    ) -> zirv_flow_store::Keyring {
        let copy = |key: &zirv_flow_store::MasterKey| {
            zirv_flow_store::MasterKey::parse(&key.to_base64()).unwrap()
        };
        zirv_flow_store::Keyring::new(copy(current), previous.map(copy)).unwrap()
    }

    fn hashes(root: &zirv_flow_store::HmacRoot) -> [[u8; 32]; 2] {
        [
            zirv_flow_store::HmacPurpose::Identifier,
            zirv_flow_store::HmacPurpose::Redaction,
        ]
        .map(|purpose| root.hmac_key(purpose).digest(b"person@example.com"))
    }

    #[tokio::test]
    async fn rotate_master_key_needs_the_master_key_and_asks_for_no_password() {
        let (code, output) =
            start_without_database(Command::Admin(AdminCommand::RotateMasterKey), None).await;

        assert_eq!(code, 2, "{output}");
        assert!(
            output.contains("error: admin: the master key is required"),
            "{output}"
        );
        assert!(!output.contains("no password"), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_keeps_every_secret_and_every_hash(pool: PgPool) {
        use zirv_flow_store::{
            MasterKey, SecretBytes, SecretPurpose, open_secret, secret_aad, secrets,
        };

        zirv_flow_store::migrate(&pool).await.unwrap();
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        let context = zirv_flow_api::admin::Context::system();
        let workspace = zirv_flow_api::admin::create_workspace(&pool, &context, "ours", None)
            .await
            .unwrap()
            .id;

        // 1. A secret and the hashes, under key A.
        let before = load_secrets(&pool, keyring_of(&a, None)).await.unwrap();
        let id = zirv_flow_api::admin::create_secret(
            &pool,
            &context,
            &before.keyring,
            workspace,
            SecretPurpose::ConnectorCredential,
            &SecretBytes::new(b"hunter2".to_vec()),
        )
        .await
        .unwrap();
        let hashes_before = hashes(&before.hmac_root);

        // 2. B is current and A is previous; rotate.
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let rotating = keyring_of(&b, Some(&a));
        let restarted = load_secrets(&pool, keyring_of(&b, Some(&a))).await.unwrap();
        let code = run_rotate_master_key(&rotating, &pool, &mut stdout, &mut stderr).await;
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("re-wrapped 2 data key(s) with master key {}\n", b.key_id())
        );

        // 3. A is dropped; the process restarts with B alone.
        let after = load_secrets(&pool, keyring_of(&b, None)).await.unwrap();

        // 4. The secret opens, the hashes are identical, and a second rotation does nothing.
        let stored = secrets::get(&pool, workspace, id).await.unwrap().unwrap();
        let aad = secret_aad(workspace, id, stored.purpose);
        let opened = open_secret(&after.keyring, &stored.sealed, &aad).unwrap();
        assert_eq!(opened.expose(), b"hunter2");
        assert_eq!(hashes(&after.hmac_root), hashes_before);
        assert_eq!(hashes(&restarted.hmac_root), hashes_before);
        let mut stdout = Vec::new();
        let code = run_rotate_master_key(&rotating, &pool, &mut stdout, &mut Vec::new()).await;
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("re-wrapped 0 data key(s) with master key {}\n", b.key_id())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_start_with_a_previous_key_and_no_root_is_refused_before_anything_is_stored(
        pool: PgPool,
    ) {
        use zirv_flow_store::{MasterKey, secrets};

        zirv_flow_store::migrate(&pool).await.unwrap();
        let (a, b) = (MasterKey::generate(), MasterKey::generate());

        let error = load_secrets(&pool, keyring_of(&b, Some(&a)))
            .await
            .err()
            .expect("the start is refused");

        assert!(
            error.contains("no HMAC root exists yet; start once with only the master key"),
            "{error}"
        );
        assert!(secrets::hmac_root(&pool).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_with_a_row_the_keyring_cannot_open_changes_nothing(pool: PgPool) {
        use zirv_flow_store::{MasterKey, SecretBytes, SecretPurpose, secrets};

        zirv_flow_store::migrate(&pool).await.unwrap();
        let (a, b, c) = (
            MasterKey::generate(),
            MasterKey::generate(),
            MasterKey::generate(),
        );
        let context = zirv_flow_api::admin::Context::system();
        let workspace = zirv_flow_api::admin::create_workspace(&pool, &context, "ours", None)
            .await
            .unwrap()
            .id;
        let under_a = load_secrets(&pool, keyring_of(&a, None)).await.unwrap();
        let id = zirv_flow_api::admin::create_secret(
            &pool,
            &context,
            &under_a.keyring,
            workspace,
            SecretPurpose::OAuthToken,
            &SecretBytes::new(b"token".to_vec()),
        )
        .await
        .unwrap();
        let mut stderr = Vec::new();

        // The keyring holds C and B, but the rows are wrapped by A.
        let code = run_rotate_master_key(
            &keyring_of(&c, Some(&b)),
            &pool,
            &mut Vec::new(),
            &mut stderr,
        )
        .await;

        assert_eq!(code, 1);
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.starts_with("error: admin: "), "{stderr}");
        assert!(stderr.contains(&a.key_id()), "{stderr}");
        let stored = secrets::get(&pool, workspace, id).await.unwrap().unwrap();
        assert_eq!(stored.sealed.key_id, a.key_id());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_with_no_hmac_root_exits_one_and_changes_nothing(pool: PgPool) {
        use zirv_flow_store::{MasterKey, secrets};

        zirv_flow_store::migrate(&pool).await.unwrap();
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        let mut stderr = Vec::new();

        let code = run_rotate_master_key(
            &keyring_of(&b, Some(&a)),
            &pool,
            &mut Vec::new(),
            &mut stderr,
        )
        .await;

        assert_eq!(code, 1);
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains(secrets::NO_HMAC_ROOT), "{stderr}");
        assert!(secrets::hmac_root(&pool).await.unwrap().is_none());
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(entries.is_empty(), "{entries:?}");
    }
}
