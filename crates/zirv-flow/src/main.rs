mod cli;
mod config;

use std::ffi::OsString;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::net::TcpListener;
use zirv_flow_store::{PgPool, SchemaState};

use crate::cli::{AdminCommand, Cli, Command, ConfigCommand};
use crate::config::Config;

/// Seconds; makes the exporter render request durations as histograms rather than summaries.
const HTTP_DURATION_BUCKETS: [f64; 10] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    ExitCode::from(
        run(
            cli,
            std::env::vars_os(),
            &mut io::stdin().lock(),
            &mut io::stdout(),
            &mut io::stderr(),
        )
        .await,
    )
}

async fn run(
    cli: Cli,
    env: impl IntoIterator<Item = (OsString, OsString)>,
    stdin: &mut impl BufRead,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    let config = match Config::load(cli.config.as_deref(), env) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(stderr, "error: {error}");
            return 2;
        }
    };

    let command = cli.command.unwrap_or(Command::All);
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

    if matches!(command, Command::Migrate) {
        return run_migrate(&config, stdout, stderr).await;
    }

    if let Command::Admin(admin_command) = &command {
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

    if matches!(
        command,
        Command::All | Command::Serve | Command::Worker | Command::Scheduler
    ) {
        let pool = match open_database(&config).await {
            Ok(pool) => pool,
            Err(message) => return database_error(stderr, message),
        };

        // `all` also serves HTTP until the worker and scheduler roles exist (#7).
        if matches!(command, Command::All | Command::Serve) {
            let handle = match PrometheusBuilder::new()
                .set_buckets(&HTTP_DURATION_BUCKETS)
                .and_then(|builder| builder.install_recorder())
            {
                Ok(handle) => handle,
                Err(error) => {
                    let _ = writeln!(stderr, "error: metrics: {error}");
                    return 1;
                }
            };
            let render: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || handle.render());
            let mut state = zirv_flow_api::AppState::new(pool, render)
                .with_cookie_secure(config.server_cookie_secure())
                .with_local_sign_in(config.auth_local_enabled());
            if let Some((settings, auto_create_users)) = config.oidc() {
                state = match state.with_oidc(settings, auto_create_users) {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = writeln!(stderr, "error: auth.oidc: {error}");
                        return 2;
                    }
                };
            }
            return serve_http(state, config.server_bind(), stderr, shutdown_signal()).await;
        }
    }

    let _ = writeln!(
        stderr,
        "error: the {} command is not implemented yet",
        command.name()
    );
    1
}

/// Binds `addr` and serves the API until `shutdown` completes.
async fn serve_http(
    state: zirv_flow_api::AppState,
    addr: SocketAddr,
    stderr: &mut impl Write,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> u8 {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = writeln!(stderr, "error: server: could not bind {addr}: {error}");
            return 1;
        }
    };
    let bound = listener.local_addr().unwrap_or(addr);
    let _ = writeln!(stderr, "listening on {bound}");

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
                result = tokio::time::timeout(SHUTDOWN_GRACE, &mut server) => match result {
                    Ok(result) => result,
                    Err(_) => {
                        let _ = writeln!(stderr, "error: server: requests still running after {}s; exiting", SHUTDOWN_GRACE.as_secs());
                        return 1;
                    }
                },
                _ = tokio::signal::ctrl_c() => {
                    let _ = writeln!(stderr, "error: server: interrupted while draining requests");
                    return 1;
                }
            }
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "error: server: {error}");
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

    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap()
    }

    fn state() -> zirv_flow_api::AppState {
        zirv_flow_api::AppState::new(lazy_pool(), Arc::new(String::new))
    }

    #[tokio::test]
    async fn a_bind_failure_is_reported_with_exit_code_one() {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();
        let mut stderr = Vec::new();

        let code = serve_http(state(), addr, &mut stderr, std::future::ready(())).await;

        assert_eq!(code, 1);
        let message = String::from_utf8(stderr).unwrap();
        assert!(
            message.starts_with(&format!("error: server: could not bind {addr}: ")),
            "{message}"
        );
    }

    #[tokio::test]
    async fn the_server_binds_and_exits_cleanly_on_shutdown() {
        let mut stderr = Vec::new();

        let code = serve_http(
            state(),
            "127.0.0.1:0".parse().unwrap(),
            &mut stderr,
            std::future::ready(()),
        )
        .await;

        assert_eq!(code, 0);
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("listening on 127.0.0.1:")
        );
    }
}
