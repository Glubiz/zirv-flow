//! Boot, seed, serve and clean up: the life of the browser test server.

use std::io::Write;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};
use tokio::net::TcpListener;
use uuid::Uuid;
use zirv_flow_api::admin::{self, Context};
use zirv_flow_core::Role;

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

fn port() -> Result<u16, String> {
    match std::env::var("ZIRV_FLOW_BROWSER_TEST_PORT") {
        Ok(value) => value
            .parse()
            .map_err(|_| "ZIRV_FLOW_BROWSER_TEST_PORT must be a port number".to_owned()),
        Err(_) => Ok(DEFAULT_PORT),
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
        let random = Uuid::new_v4().simple().to_string();
        let name = format!("zf_browser_{}_{}", std::process::id(), &random[..12]);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(maintenance.clone())
            .await
            .map_err(|error| format!("could not connect to PostgreSQL: {error}"))?;
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .map_err(|error| format!("could not create the scratch database: {error}"))?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(maintenance.clone().database(&name))
            .await
            .map_err(|error| format!("could not connect to the scratch database: {error}"))?;
        Ok(Self {
            maintenance,
            name,
            pool,
        })
    }

    /// Drops the database, cutting any connection still open. Only this run's own name is ever
    /// dropped.
    async fn drop_database(self) {
        self.pool.close().await;
        let dropped = async {
            let admin = PgPoolOptions::new()
                .max_connections(1)
                .connect_with(self.maintenance)
                .await?;
            sqlx::query(AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            )))
            .execute(&admin)
            .await?;
            admin.close().await;
            Ok::<(), sqlx::Error>(())
        }
        .await;
        if let Err(error) = dropped {
            eprintln!(
                "warning: could not drop the scratch database {}: {error}",
                self.name
            );
        }
    }
}

pub(crate) async fn run() -> Result<(), String> {
    let password = required("ZIRV_FLOW_BROWSER_TEST_PASSWORD")?;
    let database_url = required("DATABASE_URL")?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port()?))
        .await
        .map_err(|error| format!("could not listen on 127.0.0.1: {error}"))?;
    let scratch = Scratch::create(&database_url).await?;
    let served = tokio::spawn(serve(scratch.pool.clone(), listener, password)).await;
    scratch.drop_database().await;
    served.unwrap_or_else(|error| Err(format!("the server stopped unexpectedly: {error}")))
}

async fn serve(pool: PgPool, listener: TcpListener, password: String) -> Result<(), String> {
    let address = listener
        .local_addr()
        .map_err(|error| format!("no local address: {error}"))?;
    let base = format!("http://{address}");
    let world = Arc::new(World::start(pool, &base).await);
    seed(&world, &password).await?;

    let app = Router::new()
        .route("/__test/sink", get(sink_list).delete(sink_clear))
        .with_state(Arc::clone(&world))
        .merge(world.router.clone());
    println!("READY {base}");
    let _ = std::io::stdout().flush();

    let (stopping, stop) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async {
        let _ = stop.await;
    });
    let mut server = tokio::spawn(async move { server.await });
    let outcome = tokio::select! {
        done = &mut server => done,
        () = stop_signal() => {
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
    if let Some(world) = Arc::into_inner(world) {
        world.finish().await;
    }
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

/// The messages the sink accepted: the envelope recipients, every header, and the HTML and text
/// parts decoded.
async fn sink_list(State(world): State<Arc<World>>) -> Json<Value> {
    let messages: Vec<Value> = world
        .sink
        .messages()
        .iter()
        .map(|message| {
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
        })
        .collect();
    Json(Value::Array(messages))
}

async fn sink_clear(State(world): State<Arc<World>>) -> StatusCode {
    world.sink.clear();
    StatusCode::NO_CONTENT
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
