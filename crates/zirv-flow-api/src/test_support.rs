//! Helpers the acceptance modules share.

use sqlx::PgPool;

/// The text of every row of every table, in both bytea encodings, holds no plaintext marker.
pub(crate) async fn tables_hold_no(pool: &PgPool, markers: &[&str]) {
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = current_schema()")
            .fetch_all(pool)
            .await
            .unwrap();
    let mut connection = pool.acquire().await.unwrap();
    for encoding in ["hex", "escape"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET bytea_output = '{encoding}'"
        )))
        .execute(&mut *connection)
        .await
        .unwrap();
        for table in &tables {
            let text: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT string_agg(t::text, E'\\n') FROM \"{table}\" t"
            )))
            .fetch_one(&mut *connection)
            .await
            .unwrap();
            let text = text.unwrap_or_default();
            for marker in markers {
                let hex: String = marker.bytes().map(|byte| format!("{byte:02x}")).collect();
                assert!(!text.contains(marker), "{marker} in {table} ({encoding})");
                assert!(
                    !text.contains(&hex),
                    "{marker} as hex in {table} ({encoding})"
                );
            }
        }
    }
}

/// Plants each marker in a table in turn and shows `tables_hold_no` reports it.
pub(crate) async fn the_scan_finds_a_planted(pool: &PgPool, markers: &[&str]) {
    sqlx::query("CREATE TABLE scan_control (note text)")
        .execute(pool)
        .await
        .unwrap();
    for marker in markers {
        sqlx::query("INSERT INTO scan_control (note) VALUES ($1)")
            .bind(marker)
            .execute(pool)
            .await
            .unwrap();
        let planted = [*marker];
        let caught = std::panic::AssertUnwindSafe(tables_hold_no(pool, &planted));
        assert!(
            futures_util::FutureExt::catch_unwind(caught).await.is_err(),
            "the scan must detect a marker"
        );
        sqlx::query("DELETE FROM scan_control")
            .execute(pool)
            .await
            .unwrap();
    }
}
