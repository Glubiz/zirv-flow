//! Rebuilds `template1` on a disposable PostgreSQL server with every migration applied, so each
//! `#[sqlx::test]` database (a plain `CREATE DATABASE`, which clones `template1`) starts migrated.
//!
//! `DATABASE_URL` names any database on the server. The old `template1` is dropped and rebuilt
//! from `template0`, so a stale one cannot survive. Any error exits non-zero.

use std::process::ExitCode;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[tokio::main]
async fn main() -> ExitCode {
    match prepare().await {
        Ok(applied) => {
            println!("template1 migrated: {applied} migrations applied");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("prepare_test_template failed: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn prepare() -> Result<usize, String> {
    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".to_string())?;
    let options: PgConnectOptions = url.parse().map_err(|e| format!("DATABASE_URL: {e}"))?;
    if options.get_database() == Some("template1") {
        return Err("DATABASE_URL must not name template1".into());
    }

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.clone())
        .await
        .map_err(|e| format!("connect: {e}"))?;
    for statement in [
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = 'template1' AND pid <> pg_backend_pid()",
        "UPDATE pg_database SET datistemplate = false WHERE datname = 'template1'",
        "DROP DATABASE template1",
        "CREATE DATABASE template1 TEMPLATE template0 IS_TEMPLATE true",
    ] {
        sqlx::query(statement)
            .execute(&admin)
            .await
            .map_err(|e| format!("{statement}: {e}"))?;
    }
    admin.close().await;

    let template = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.database("template1"))
        .await
        .map_err(|e| format!("connect to template1: {e}"))?;
    let result = zirv_flow_store::migrate(&template).await;
    // A connected template blocks CREATE DATABASE, so close before reporting anything.
    template.close().await;
    result
        .map(|applied| applied.len())
        .map_err(|e| format!("migrate: {e}"))
}
