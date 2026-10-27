//! Helpers the acceptance modules share.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

static CREDENTIALS_MADE: AtomicU64 = AtomicU64::new(0);

/// Test setup sugar for the acceptance modules written before sources and destinations took a
/// workspace credential. A `POST` to `/sources` or `/destinations` that carries an inline
/// `"credential": {name: value}` first creates a `custom` workspace credential holding exactly
/// those fields (all required secrets) and sends `credential_id` instead. A `PATCH` that carries
/// one rotates the credential the source or destination already uses (or creates and links one
/// when it has none), which is what replacing the whole credential meant. An empty inline
/// credential is dropped, so the request is refused as it always was. Anything else is untouched.
/// `send` issues a request with the caller's own authentication and returns the status and body.
/// `Err` is the refusal of a credential call, to be shown as the answer to the original one.
pub(crate) async fn expand_inline_credential<F, Fut>(
    send: F,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Result<Option<Value>, (StatusCode, Value)>
where
    F: Fn(String, String, Option<Value>) -> Fut,
    Fut: Future<Output = (StatusCode, Value)>,
{
    let Some(Value::Object(mut map)) = body.clone() else {
        return Ok(body);
    };
    let Some(Value::Object(inline)) = map.get("credential").cloned() else {
        return Ok(body);
    };
    let cut = ["/sources", "/destinations"]
        .iter()
        .filter_map(|marker| uri.find(marker))
        .min();
    let Some(cut) = cut else {
        return Ok(body);
    };
    let base = &uri[..cut];
    map.remove("credential");
    if inline.is_empty() {
        return Ok(Some(Value::Object(map)));
    }
    let collection = uri.ends_with("/sources") || uri.ends_with("/destinations");
    let existing = if method == "PATCH" && !collection {
        let (status, current) = send("GET".into(), uri.to_owned(), None).await;
        if !status.is_success() {
            // Not ours to read: the request goes on without a credential and is refused as before.
            return Ok(Some(Value::Object(map)));
        }
        current["credential_id"].as_str().map(str::to_owned)
    } else {
        None
    };
    let id = match existing {
        Some(id) => {
            let (status, answer) = send(
                "PUT".into(),
                format!("{base}/credentials/{id}/secret"),
                Some(json!({"secret": inline})),
            )
            .await;
            if !status.is_success() {
                return Err((status, answer));
            }
            id
        }
        None => {
            let number = CREDENTIALS_MADE.fetch_add(1, Ordering::Relaxed);
            let fields: Vec<Value> = inline
                .keys()
                .map(|key| {
                    json!({"key": key, "label": key, "type": "string",
                           "required": true, "secret": true})
                })
                .collect();
            let (status, answer) = send(
                "POST".into(),
                format!("{base}/credentials"),
                Some(
                    json!({"kind": "custom", "name": format!("test credential {number}"),
                            "schema": {"fields": fields}, "secret": inline}),
                ),
            )
            .await;
            if status != StatusCode::CREATED {
                return Err((status, answer));
            }
            answer["id"].as_str().unwrap_or_default().to_owned()
        }
    };
    map.insert("credential_id".into(), Value::String(id));
    Ok(Some(Value::Object(map)))
}

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

/// Waits until some other backend of this database is blocked on a lock, polling for up to five
/// seconds, and fails with a clear message when none ever is. A test uses it to know that a
/// request really waited for the transaction it holds open, instead of guessing with a sleep.
pub(crate) async fn wait_until_a_backend_waits_on_a_lock(pool: &PgPool) {
    for _ in 0..100 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
               AND pid <> pg_backend_pid()",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no backend ever waited on a lock: the request did not block on the held transaction");
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

/// The value of the series `series` (its name and labels as rendered, such as
/// `zirv_flow_explorer_timeouts_total{kind="count"}`) in the exposition text, if it is there.
pub(crate) fn metric_sample(output: &str, series: &str) -> Option<f64> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
        .find_map(|value| value.parse().ok())
}

/// Fails when a label of any series in the rendered `/metrics` text holds one of the `seeded`
/// values (an email, a phone number, a name, a property value) or an `@`: labels carry closed
/// sets and ids only.
pub(crate) fn labels_hold_no(output: &str, seeded: &[&str]) {
    for line in output.lines().filter(|line| !line.starts_with('#')) {
        let Some((_, rest)) = line.split_once('{') else {
            continue;
        };
        let labels = rest.rsplit_once('}').map_or(rest, |(labels, _)| labels);
        assert!(
            !labels.contains('@'),
            "an address-like label value in {line}"
        );
        for value in seeded {
            assert!(
                !labels.contains(value),
                "the seeded value {value} is a label in {line}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "zz-scan-marker-3141";

    #[test]
    fn the_label_scan_passes_clean_labels_and_flags_a_seeded_value() {
        let clean = "zirv_flow_jobs{kind=\"a\",state=\"ready\"} 1
zirv_flow_x 2
# TYPE x gauge";
        labels_hold_no(clean, &["pat"]);
        assert_eq!(metric_sample(clean, "zirv_flow_x"), Some(2.0));
        assert_eq!(
            metric_sample(clean, "zirv_flow_jobs{kind=\"a\",state=\"ready\"}"),
            Some(1.0)
        );
        assert_eq!(metric_sample(clean, "zirv_flow"), None);

        let dirty = "zirv_flow_x{plan=\"pat\"} 1";
        let caught = std::panic::catch_unwind(|| labels_hold_no(dirty, &["pat"]));
        assert!(caught.is_err());
        let address = "zirv_flow_x{who=\"a@b\"} 1";
        assert!(std::panic::catch_unwind(|| labels_hold_no(address, &[])).is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_finds_a_marker_stored_as_bytea(pool: PgPool) {
        sqlx::query("CREATE TABLE scanned (note text, blob bytea)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO scanned (note, blob) VALUES ('clean', $1)")
            .bind(vec![0u8])
            .execute(&pool)
            .await
            .unwrap();
        tables_hold_no(&pool, &[MARKER]).await;

        sqlx::query("UPDATE scanned SET blob = $1")
            .bind(MARKER.as_bytes())
            .execute(&pool)
            .await
            .unwrap();
        let caught = std::panic::AssertUnwindSafe(tables_hold_no(&pool, &[MARKER]));
        assert!(
            futures_util::FutureExt::catch_unwind(caught).await.is_err(),
            "the scan must detect a marker stored as bytea"
        );
    }
}
