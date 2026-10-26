//! Boot, seed, serve and clean up: the life of the browser test server.

use std::io::Write;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;
use zirv_flow_api::admin::{self, Context};
use zirv_flow_jobs::test_support::SmtpCaptured;
use zirv_flow_store::Role;

use crate::harness::World;
use crate::mail;

/// The owner every run seeds. The password is the run's own, from the environment.
const OWNER_EMAIL: &str = "owner@browser-test.example";
/// The one address the seeded test-send allowlist holds.
const ALLOWLIST_ADDRESS: &str = "qa@browser-test.example";
const DEFAULT_PORT: u16 = 47700;
/// How long open connections get to finish after a stop signal.
const DRAIN: Duration = Duration::from_secs(5);

fn required(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(format!("{name} must be set")),
    }
}

/// The port from the value of `ZIRV_FLOW_BROWSER_TEST_PORT`, or the default when it is not set.
fn parse_port(value: Option<&str>) -> Result<u16, String> {
    let port = match value {
        Some(value) => value
            .parse()
            .map_err(|_| "ZIRV_FLOW_BROWSER_TEST_PORT must be a port number".to_owned())?,
        None => DEFAULT_PORT,
    };
    if port == 0 {
        return Err(
            "ZIRV_FLOW_BROWSER_TEST_PORT must not be 0: the tests need to know the port".to_owned(),
        );
    }
    Ok(port)
}

fn port() -> Result<u16, String> {
    parse_port(std::env::var("ZIRV_FLOW_BROWSER_TEST_PORT").ok().as_deref())
}

/// The name of this run's database: the process and 12 hex digits of `id`.
fn scratch_name(process: u32, id: Uuid) -> String {
    format!("zf_browser_{process}_{}", &id.simple().to_string()[..12])
}

/// `name` as a quoted SQL identifier.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn create_sql(name: &str) -> String {
    format!("CREATE DATABASE {}", quoted(name))
}

fn drop_sql(name: &str) -> String {
    format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", quoted(name))
}

/// The request to stop: a signal (SIGINT, SIGTERM, Ctrl-C) or `POST /__test/shutdown`.
#[derive(Clone)]
struct Stop(Arc<watch::Sender<bool>>);

impl Stop {
    /// Starts listening for the stop signals. Called first, so a signal at any later point is
    /// seen.
    fn install() -> Self {
        let stop = Self(Arc::new(watch::channel(false).0));
        let signalled = stop.clone();
        tokio::spawn(async move {
            stop_signal().await;
            signalled.request();
        });
        stop
    }

    fn request(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once a stop was requested.
    async fn requested(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|requested| *requested).await;
    }
}

/// A database made for this run on the server `DATABASE_URL` names, dropped by
/// [`Self::drop_database`].
struct Scratch {
    maintenance: PgConnectOptions,
    name: String,
    pool: PgPool,
}

impl Scratch {
    async fn create(database_url: &str) -> Result<Self, String> {
        let maintenance = PgConnectOptions::from_str(database_url)
            .map_err(|_| "DATABASE_URL is not a PostgreSQL URL".to_owned())?;
        let name = scratch_name(std::process::id(), Uuid::new_v4());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(maintenance.clone())
            .await
            .map_err(|error| format!("could not connect to PostgreSQL: {error}"))?;
        sqlx::query(AssertSqlSafe(create_sql(&name)))
            .execute(&admin)
            .await
            .map_err(|error| format!("could not create the scratch database: {error}"))?;
        admin.close().await;
        let connected = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(maintenance.clone().database(&name))
            .await;
        match connected {
            Ok(pool) => Ok(Self {
                maintenance,
                name,
                pool,
            }),
            Err(error) => {
                let _ = drop_database(maintenance, &name).await;
                Err(format!(
                    "could not connect to the scratch database: {error}"
                ))
            }
        }
    }

    async fn drop_database(self) -> Result<(), String> {
        // A connection some task still holds must not keep the drop waiting: `FORCE` cuts it.
        let _ = tokio::time::timeout(DRAIN, self.pool.close()).await;
        drop_database(self.maintenance, &self.name).await
    }
}

/// Drops the database `name`, cutting any connection still open. Only this run's own name is
/// ever passed.
async fn drop_database(maintenance: PgConnectOptions, name: &str) -> Result<(), String> {
    let dropped = async {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(maintenance)
            .await?;
        sqlx::query(AssertSqlSafe(drop_sql(name)))
            .execute(&admin)
            .await?;
        admin.close().await;
        Ok::<(), sqlx::Error>(())
    }
    .await;
    dropped.map_err(|error| format!("could not drop the scratch database {name}: {error}"))
}

/// What the run holds that must be released: the world and the scratch database. Whoever gets
/// here first (the shutdown route, or the end of `run`) does the cleanup, once.
struct Shared {
    world: Mutex<Option<World>>,
    scratch: Mutex<Option<Scratch>>,
    /// Held for the whole cleanup, so a second caller waits for the first to finish.
    cleaning: tokio::sync::Mutex<()>,
    stop: Stop,
}

fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

impl Shared {
    /// Stops the worker and the render host, closes the pool and drops the scratch database.
    /// Calling it again does nothing.
    async fn clean_up(&self) -> Result<(), String> {
        let _cleaning = self.cleaning.lock().await;
        if let Some(world) = take(&self.world) {
            world.finish().await;
        }
        match take(&self.scratch) {
            Some(scratch) => scratch.drop_database().await,
            None => Ok(()),
        }
    }
}

pub(crate) async fn run() -> Result<(), String> {
    let stop = Stop::install();
    let password = required("ZIRV_FLOW_BROWSER_TEST_PASSWORD")?;
    let database_url = required("DATABASE_URL")?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port()?))
        .await
        .map_err(|error| format!("could not listen on 127.0.0.1: {error}"))?;
    let scratch = Scratch::create(&database_url).await?;
    let pool = scratch.pool.clone();
    let shared = Arc::new(Shared {
        world: Mutex::new(None),
        scratch: Mutex::new(Some(scratch)),
        cleaning: tokio::sync::Mutex::new(()),
        stop,
    });
    let served = tokio::spawn(serve(Arc::clone(&shared), pool, listener, password)).await;
    let cleaned = shared.clean_up().await;
    served.unwrap_or_else(|error| Err(format!("the server stopped unexpectedly: {error}")))?;
    cleaned
}

async fn serve(
    shared: Arc<Shared>,
    pool: PgPool,
    listener: TcpListener,
    password: String,
) -> Result<(), String> {
    let address = listener
        .local_addr()
        .map_err(|error| format!("no local address: {error}"))?;
    let base = format!("http://{address}");
    let stop = shared.stop.clone();
    // A stop while the world starts abandons it: nothing it started outlives the future yet.
    let world = tokio::select! {
        world = World::start(pool, &base) => world,
        () = stop.requested() => return Ok(()),
    };
    let router = world.router.clone();
    // From here on the world is finished on every path: a stop or a failure while seeding
    // finishes it here, and a seeded world goes into `shared` for the shutdown route and `run`.
    let seeded = tokio::select! {
        seeded = seed(&world, &password) => Some(seeded),
        () = stop.requested() => None,
    };
    if !matches!(seeded, Some(Ok(()))) {
        world.finish().await;
        return seeded.unwrap_or(Ok(()));
    }
    *shared.world.lock().unwrap_or_else(PoisonError::into_inner) = Some(world);

    let app = Router::new()
        .route("/__test/sink", get(sink_list).delete(sink_clear))
        .route("/__test/shutdown", post(shutdown))
        .with_state(Arc::clone(&shared))
        .merge(router);
    println!("READY {base}");
    let _ = std::io::stdout().flush();

    let (stopping, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = stopped.await;
    });
    let mut server = tokio::spawn(async move { server.await });
    let outcome = tokio::select! {
        done = &mut server => done,
        () = stop.requested() => {
            let _ = stopping.send(());
            match tokio::time::timeout(DRAIN, &mut server).await {
                Ok(done) => done,
                Err(_) => {
                    server.abort();
                    Ok(Ok(()))
                }
            }
        }
    };
    outcome
        .map_err(|error| format!("the server task failed: {error}"))?
        .map_err(|error| format!("the server failed: {error}"))
}

/// Seeds the workspace content every test starts from: an owner who administers the workspace, a
/// verified sending domain with a sender, an SMTP destination on the sink and one allowlisted
/// test-send address.
async fn seed(world: &World, password: &str) -> Result<(), String> {
    let context = Context::system();
    let owner = admin::create_owner(&world.pool, &context, OWNER_EMAIL, password)
        .await
        .map_err(|error| format!("could not create the owner: {error:?}"))?;
    admin::set_member(&world.pool, &context, world.ws, owner.id, Role::Admin)
        .await
        .map_err(|error| format!("could not add the owner to the workspace: {error:?}"))?;
    world
        .verified_sender("example.com", "news@example.com")
        .await;
    world.smtp_destination().await;
    world
        .api_ok(
            "POST",
            "/test-send-allowlist",
            Some(json!({"address": ALLOWLIST_ADDRESS})),
            StatusCode::CREATED,
        )
        .await;
    Ok(())
}

/// One captured message as the sink route shows it: the envelope recipients, every header, and
/// the HTML and text parts decoded.
fn message_json(message: &SmtpCaptured) -> Value {
    let headers: Vec<Value> = mail::headers(&message.data)
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    json!({
        "recipients": message.recipients,
        "headers": headers,
        "html": mail::part(&message.data, "text/html"),
        "text": mail::part(&message.data, "text/plain"),
    })
}

async fn sink_list(State(shared): State<Arc<Shared>>) -> Result<Json<Value>, StatusCode> {
    let guard = shared.world.lock().unwrap_or_else(PoisonError::into_inner);
    let world = guard.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let messages: Vec<Value> = world.sink.messages().iter().map(message_json).collect();
    Ok(Json(Value::Array(messages)))
}

async fn sink_clear(State(shared): State<Arc<Shared>>) -> StatusCode {
    let guard = shared.world.lock().unwrap_or_else(PoisonError::into_inner);
    match guard.as_ref() {
        Some(world) => {
            world.sink.clear();
            StatusCode::NO_CONTENT
        }
        None => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// The answer to a shutdown request: 200 once the cleanup succeeded, else 500 with the reason.
fn shutdown_reply(cleaned: &Result<(), String>) -> (StatusCode, String) {
    match cleaned {
        Ok(()) => (StatusCode::OK, String::new()),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.clone()),
    }
}

/// Cleans up while the server still serves, answers 200 once the scratch database is dropped (500
/// with the reason when it could not be), and only then stops the server. A forced stop of the
/// process, as on Windows, can then no longer leave the database behind.
async fn shutdown(State(shared): State<Arc<Shared>>) -> (StatusCode, String) {
    let cleaned = shared.clean_up().await;
    if cleaned.is_ok() {
        shared.stop.request();
    }
    shutdown_reply(&cleaned)
}

/// Resolves on SIGINT, on SIGTERM (Unix), or on Ctrl-C (Windows).
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_defaults_and_can_be_overridden() {
        assert_eq!(parse_port(None), Ok(47700));
        assert_eq!(parse_port(Some("48123")), Ok(48123));
    }

    #[test]
    fn port_zero_and_junk_are_refused() {
        for value in ["0", "", "http", "-1", "65536", " 80"] {
            assert!(parse_port(Some(value)).is_err(), "{value:?}");
        }
        assert!(parse_port(Some("0")).unwrap_err().contains("must not be 0"));
    }

    #[test]
    fn a_scratch_name_is_the_process_and_twelve_hex_digits() {
        let name = scratch_name(
            4242,
            Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
        );

        assert_eq!(name, "zf_browser_4242_0123456789ab");
    }

    #[test]
    fn the_sql_names_only_the_quoted_database() {
        let name = scratch_name(1, Uuid::new_v4());

        assert_eq!(create_sql(&name), format!("CREATE DATABASE \"{name}\""));
        assert_eq!(
            drop_sql(&name),
            format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)")
        );
    }

    #[test]
    fn a_quote_in_a_name_cannot_end_the_identifier() {
        assert_eq!(
            quoted("a\"; DROP DATABASE postgres; --"),
            "\"a\"\"; DROP DATABASE postgres; --\""
        );
    }

    #[test]
    fn a_captured_message_shows_its_recipients_headers_and_parts() {
        let data = b"Subject: Hi\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nplain\r\n--b1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>html</p>\r\n--b1--\r\n";
        let message = SmtpCaptured {
            mail_from: "from@example.com".to_owned(),
            recipients: vec!["to@example.com".to_owned()],
            data: data.to_vec(),
        };

        let shown = message_json(&message);

        assert_eq!(shown["recipients"], json!(["to@example.com"]));
        assert_eq!(
            shown["headers"][0],
            json!({"name": "Subject", "value": "Hi"})
        );
        assert_eq!(shown["html"], "<p>html</p>");
        assert_eq!(shown["text"], "plain");
    }

    #[test]
    fn shutdown_answers_200_after_a_cleanup_and_500_with_the_reason_after_a_failure() {
        assert_eq!(shutdown_reply(&Ok(())), (StatusCode::OK, String::new()));
        assert_eq!(
            shutdown_reply(&Err("could not drop".to_owned())),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not drop".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn a_stop_request_resolves_every_waiter_now_and_later() {
        let stop = Stop(Arc::new(watch::channel(false).0));
        let early = stop.clone();
        let waiting = tokio::spawn(async move { early.requested().await });

        stop.request();

        waiting.await.unwrap();
        stop.requested().await;
    }

    #[tokio::test]
    async fn cleaning_up_twice_does_nothing_the_second_time() {
        let shared = Shared {
            world: Mutex::new(None),
            scratch: Mutex::new(None),
            cleaning: tokio::sync::Mutex::new(()),
            stop: Stop(Arc::new(watch::channel(false).0)),
        };

        assert_eq!(shared.clean_up().await, Ok(()));
        assert_eq!(shared.clean_up().await, Ok(()));
    }
}
