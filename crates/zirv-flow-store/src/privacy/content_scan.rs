//! The erasure content scan (architecture section 16.3, exit criterion 2): after an erasure, no
//! table holds a value of the erased person, whatever column it sits in. The test seeds distinctive
//! values in every data class, erases, and then reads every `text`, `varchar`, `jsonb`, `json`,
//! `bytea` and array column of every base table for them, case-insensitively and in the spellings
//! a log line or a URL gives them. It reports the table, the column and the row count, never a
//! value.
//!
//! A drift guard classifies every table of the catalog, so a migration that adds one fails here
//! until the new table is placed in exactly one reviewed class below.

/// Tables that may hold a seeded value after an erasure, each with the reason. Only the
/// suppression hash, the ledger entry and id-only audit rows survive (ruling e3c37d2b).
const SURVIVORS: &[(&str, &str)] = &[
    (
        "suppressions",
        "the hash of the address under HMAC, never the address",
    ),
    (
        "erasure_requests",
        "the ledger: request id, person ids and identifier hashes",
    ),
    (
        "erasure_destination_calls",
        "keyed to the ledger: destination id, outcome and a fixed error code",
    ),
    (
        "audit_log",
        "id-only by ruling e3c37d2b; the validator in #370 enforces it",
    ),
];

/// Tables that hold a person's value but have no `person_id` the scan could follow, each with
/// the delete that closes it. The content scan covers them like every other table.
const BY_VALUE: &[(&str, &str)] = &[
    (
        "ai_transcripts",
        "erase_ai_transcripts: workspace filter, substring match",
    ),
    (
        "ingest_rejects",
        "erase_unattributed_rejects for a null person; DELETIONS for the rest",
    ),
    (
        "import_chunks",
        "erase_import_values: workspace index, 7 day retention",
    ),
    (
        "import_rejects",
        "erase_import_values: workspace index, 30 day retention",
    ),
    (
        "contact_idempotency",
        "erase_contact_idempotency: primary key prefix, 24 hour expiry",
    ),
    (
        "test_sends",
        "keyed by recipient hash: delete_test_sends_by_hash",
    ),
    (
        "test_send_allowlist",
        "keyed by address hash, address sealed: delete_allowlist_by_hash",
    ),
    (
        "ingest_inbox",
        "keyed by identifier hash in the payload: delete_inbox_rows",
    ),
    (
        "debugger_feed",
        "no person column: DELETIONS drops the rows of the person's events and rejects",
    ),
    (
        "message_links",
        "no person column: DELETIONS drops the links of the person's messages",
    ),
    (
        "step_executions",
        "no person column: DELETIONS drops the steps of the person's enrollments",
    ),
    (
        "jobs",
        "purge_jobs by person and enrollment id inside delete_person",
    ),
];

/// Tables that are deliberately not classed above and hold no person's value, each group with the
/// reason. They are still scanned: this list is the reviewer's statement of why a hit there would
/// be a defect, not an exemption.
const VALUE_FREE: &[(&[&str], &str)] = &[
    (
        &[
            "workspaces",
            "users",
            "user_identities",
            "sessions",
            "oidc_logins",
            "api_keys",
            "memberships",
            "deployment_keys",
            "credentials",
            "workspace_credentials",
            "encrypted_secrets",
            "webhook_signing_keys",
        ],
        "operators and credentials: staff, never the workspace's people; secrets are sealed",
    ),
    (
        &[
            "flows",
            "flow_versions",
            "segments",
            "segment_versions",
            "lists",
            "topics",
            "templates",
            "template_drafts",
            "template_versions",
            "snippets",
            "snippet_drafts",
            "snippet_versions",
            "themes",
            "theme_drafts",
            "theme_versions",
            "workspace_default_theme",
            "assets",
            "asset_references",
            "trait_definitions",
            "contact_definitions",
            "event_schemas",
            "event_schema_versions",
            "event_policies",
        ],
        "workspace configuration an operator writes: definitions, never a person's values",
    ),
    (
        &[
            "consent_dimensions",
            "consent_dimension_values",
            "consent_policies",
            "consent_policy_versions",
            "send_settings",
            "quiet_hours",
            "frequency_caps",
            "destination_rate_buckets",
            "schedules",
        ],
        "consent policy and send limits: configuration and counters",
    ),
    (
        &[
            "destinations",
            "forwarding_rules",
            "webhooks",
            "sources",
            "source_oauth_states",
            "streams",
            "stream_offsets",
            "sync_runs",
            "sending_domains",
            "sending_domain_records",
            "sender_identities",
            "delivery_attempts",
        ],
        "integrations: configuration, cursors and delivery verdicts as codes",
    ),
    (
        &["ai_calls", "ai_drafts", "ai_settings"],
        "AI use: counters and mapping drafts; the free text is in ai_transcripts",
    ),
    (
        &["debugger_watch", "_sqlx_migrations", "imports"],
        "a watch timestamp, the migration ledger and an import's name, header and counters (cells are in import_rejects)",
    ),
];

/// The data types the scan reads as text.
const TEXT_TYPES: &[&str] = &[
    "text",
    "character varying",
    "character",
    "jsonb",
    "json",
    "bytea",
    "ARRAY",
];

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
    use zirv_flow_core::{HmacPurpose, HmacRoot, WorkspaceId};

    use super::*;
    use crate::ai::{CallOutcome, NewAiCall, insert_ai_transcript, record_ai_call};
    use crate::people::{apply_profile_batch, person_reference_places};
    use crate::privacy::testing::{
        BYSTANDER_EMAIL, EMAIL, Seeded, identifier_key, master_key, person_of, profile,
        seed_everything, trace,
    };
    use crate::privacy::{EraseOutcome, erase_person, request_erasure};
    use crate::{
        ConsentChange, EventRecord, ImportUpload, UploadScan, UploadStart, apply_event_batch,
        begin, commit, set_consent,
    };

    const PHONE: &str = "+4520304050";
    const EXTERNAL: &str = "ext-zq7-4471";
    const NAME: &str = "Zephyrine Quillfeather";
    const COUPON: &str = "ZQ-QUILL-777";
    const EVIDENCE: &str = "quillfeather-signup";
    const SKETCH: &str = "Please email ada@example.com about the unpaid invoice";
    /// A sign-up form id only the survivor's merged record carried.
    const ANON: &str = "anon-1";

    /// `value` as a log line, a URL or a form gives it: lowercased, trimmed, percent-encoded and
    /// form-encoded, and a phone number without its plus sign.
    fn forms(value: &str) -> Vec<String> {
        let lower = value.trim().to_lowercase();
        let percent: String = lower
            .bytes()
            .map(|byte| match byte {
                b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(byte).to_string()
                }
                _ => format!("%{byte:02x}"),
            })
            .collect();
        let mut all = vec![lower.clone(), percent.clone(), percent.replace("%20", "+")];
        if let Some(digits) = lower.strip_prefix('+') {
            all.push(digits.to_owned());
        }
        all.sort();
        all.dedup();
        all
    }

    fn all_forms(values: &[&str]) -> Vec<String> {
        let mut all: Vec<String> = values.iter().flat_map(|value| forms(value)).collect();
        all.sort();
        all.dedup();
        all
    }

    /// Every base table and partitioned table of the schema, partitions counted through their
    /// parent, sorted.
    async fn base_tables(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_class c \
             WHERE c.relnamespace = current_schema()::regnamespace \
               AND c.relkind IN ('r', 'p') AND NOT c.relispartition ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// `(column, data type)` of the table's text-like columns.
    async fn text_columns(pool: &PgPool, table: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT column_name::text, data_type::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
               AND data_type = ANY($2) ORDER BY ordinal_position",
        )
        .bind(table)
        .bind(TEXT_TYPES)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// `table.column: count rows` for every column outside [`SURVIVORS`] that holds any of
    /// `needles` (already lowercased). Never a value.
    async fn leftovers(pool: &PgPool, needles: &[String]) -> Vec<String> {
        let mut found = Vec::new();
        for table in base_tables(pool).await {
            if SURVIVORS.iter().any(|(name, _)| *name == table) {
                continue;
            }
            for (column, kind) in text_columns(pool, &table).await {
                let text = if kind == "bytea" {
                    format!("encode({column}, 'escape')")
                } else {
                    format!("{column}::text")
                };
                // Names come from the catalog and this module, never from input.
                let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {table} WHERE EXISTS ( \
                        SELECT 1 FROM unnest($1::text[]) AS needle \
                        WHERE position(needle IN lower({text})) > 0)"
                )))
                .bind(needles)
                .fetch_one(pool)
                .await
                .unwrap();
                if count > 0 {
                    found.push(format!("{table}.{column}: {count} rows"));
                }
            }
        }
        found
    }

    /// The tables that are in no class: not person-scoped (found by the shared discovery the
    /// export reads), not a survivor, not closed by value and not declared value-free.
    async fn unclassified(pool: &PgPool) -> Vec<String> {
        let scoped: Vec<String> = person_reference_places(pool)
            .await
            .into_iter()
            .map(|(table, _)| table)
            .collect();
        base_tables(pool)
            .await
            .into_iter()
            .filter(|table| {
                !scoped.contains(table)
                    && !SURVIVORS.iter().any(|(name, _)| name == table)
                    && !BY_VALUE.iter().any(|(name, _)| name == table)
                    && !VALUE_FREE
                        .iter()
                        .any(|(names, _)| names.contains(&table.as_str()))
            })
            .collect()
    }

    /// What the seed planted, so the test can look for it afterwards.
    struct Planted {
        /// The survivor's values, in the forms a scan looks for.
        needles: Vec<String>,
        /// The bystander's values.
        kept: Vec<String>,
    }

    async fn upload(pool: &PgPool, ws: WorkspaceId, name: &str, bytes: &[u8]) -> Uuid {
        let header = vec!["email".to_owned()];
        let trace = trace();
        ImportUpload::store(
            pool,
            ws,
            &UploadStart {
                name,
                source: "csv",
                stream: name,
                file_name: "people.csv",
                created_by: None,
                trace: &trace,
            },
            bytes,
            &UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: 1,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn import_reject(pool: &PgPool, ws: WorkspaceId, import: Uuid, raw: serde_json::Value) {
        sqlx::query(
            "INSERT INTO import_rejects (import_id, row_number, workspace_id, phase, errors, raw) \
             VALUES ($1, 2, $2, 'run', '[]', $3)",
        )
        .bind(import)
        .bind(ws.as_uuid())
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn null_person_reject(pool: &PgPool, ws: WorkspaceId, payload: serde_json::Value) {
        sqlx::query(
            "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, reason, \
                payload, trace_id) \
             VALUES (gen_random_uuid(), $1, now(), 'web', 'event', 'schema', $2, gen_random_uuid())",
        )
        .bind(ws.as_uuid())
        .bind(payload)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn idempotency_key(pool: &PgPool, ws: WorkspaceId, key: &str) {
        sqlx::query(
            "INSERT INTO contact_idempotency (workspace_id, key, request_hash, status) \
             VALUES ($1, $2, decode(repeat('00', 32), 'hex'), 200)",
        )
        .bind(ws.as_uuid())
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn transcript(pool: &PgPool, ws: WorkspaceId, text: &str) {
        let call = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        record_ai_call(
            &mut connection,
            &NewAiCall {
                id: call,
                workspace_id: ws,
                user_id: None,
                feature: "mapping",
                provider_id: "p",
                model: "m",
                input_tokens: 1,
                output_tokens: 1,
                estimated_cost_micros: 1,
                latency_ms: 1,
                outcome: CallOutcome::Ok,
                draft_id: None,
            },
        )
        .await
        .unwrap();
        insert_ai_transcript(
            &mut connection,
            ws,
            call,
            None,
            &json!({"turns": [{"role": "user", "text": text}]}),
            OffsetDateTime::now_utc() + time::Duration::days(1),
        )
        .await
        .unwrap();
    }

    async fn test_send(pool: &PgPool, ws: WorkspaceId, address: &str) {
        let hash = zirv_flow_core::people::identifier_hash(
            &identifier_key(),
            &zirv_flow_core::people::Identifier::parse("email", address).unwrap(),
        );
        sqlx::query(
            "INSERT INTO test_sends (id, workspace_id, template_id, destination_id, \
                sender_identity_id, recipient_hash, render_context) \
             VALUES ($1, $2, $1, $1, $1, $3, 'sample')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(hash.as_slice())
        .execute(pool)
        .await
        .unwrap();
    }

    /// Adds, to what [`seed_everything`] built, a distinctive value of the survivor in every data
    /// class and a different value of the bystander in each class the erasure finds by value.
    /// The bystander's values hold no `1`: the survivor has the external id `1`, which the
    /// by-value deletes match as a substring.
    async fn plant(pool: &PgPool, seeded: &Seeded) -> Planted {
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = *seeded;
        let identity = [
            ("email", EMAIL),
            ("phone", PHONE),
            ("external:hub", EXTERNAL),
        ];
        let outcomes = apply_profile_batch(
            pool,
            ws,
            &[profile("hub", "7", &identity, json!({"display": NAME}))],
        )
        .await
        .unwrap();
        assert_eq!(
            person_of(&outcomes[0]),
            person,
            "the record joins the survivor"
        );

        // The seed left the policy rejecting unplanned events; this one is to be stored.
        crate::set_event_policy(pool, ws, &crate::EventPolicy::default())
            .await
            .unwrap();
        let key = HmacRoot::bootstrap_from(&master_key()).hmac_key(HmacPurpose::Redaction);
        apply_event_batch(
            pool,
            ws,
            &key,
            &[EventRecord {
                source_record_id: None,
                source: "web".into(),
                identifiers: vec![("email".into(), EMAIL.into())],
                name: "seen".into(),
                properties: json!({"coupon": COUPON}).as_object().cloned().unwrap(),
                timestamp: None,
                received_at: OffsetDateTime::now_utc(),
                message_id: Some("m-coupon".into()),
                trace: trace(),
            }],
        )
        .await
        .unwrap();

        let mut tx = begin(pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                dimensions: Default::default(),
                person,
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Subscribed,
                source: ChangeSource::Api,
                evidence: json!({"form": EVIDENCE}),
                occurred_at: datetime!(2026-03-01 00:00 UTC),
                actor: None,
            },
            &trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();

        // The survivor: a transcript, an import file and a refused row of it, a reject that
        // never found a person, a test send, and an idempotency key.
        transcript(pool, ws, SKETCH).await;
        upload(pool, ws, "survivor", format!("email\n{EMAIL}\n").as_bytes()).await;
        let refused = upload(pool, ws, "refused", b"email\n").await;
        import_reject(pool, ws, refused, json!({"email": EMAIL, "phone": PHONE})).await;
        null_person_reject(pool, ws, json!({"contact": EXTERNAL, "device": ANON})).await;
        test_send(pool, ws, EMAIL).await;
        idempotency_key(pool, ws, &format!("retry-{EMAIL}-ext")).await;

        // The bystander's, in the same classes.
        let grace = "grace@example.com";
        assert_eq!(BYSTANDER_EMAIL, grace);
        transcript(pool, ws, "Draft a welcome for grace@example.com").await;
        upload(pool, ws, "bystander", b"email\ngrace@example.com\n").await;
        let kept_import = upload(pool, ws, "bystander-refused", b"email\n").await;
        import_reject(pool, ws, kept_import, json!({"email": grace})).await;
        null_person_reject(pool, ws, json!({"contact": grace})).await;
        test_send(pool, ws, grace).await;
        idempotency_key(pool, ws, "retry-grace@example.com-ext").await;
        let _ = bystander;

        Planted {
            needles: all_forms(&[
                EMAIL,
                PHONE,
                EXTERNAL,
                NAME,
                COUPON,
                EVIDENCE,
                SKETCH,
                ANON,
                &person.to_string(),
            ]),
            kept: all_forms(&[grace]),
        }
    }

    async fn erase(pool: &PgPool, seeded: &Seeded) -> EraseOutcome {
        let mut tx = begin(pool).await.unwrap();
        let request = request_erasure(
            &mut tx,
            seeded.ws,
            seeded.person,
            &identifier_key(),
            None,
            &trace(),
        )
        .await
        .unwrap()
        .unwrap();
        commit(tx).await.unwrap();
        let mut tx = begin(pool).await.unwrap();
        let outcome = erase_person(
            &mut tx,
            seeded.ws,
            request.id,
            &identifier_key(),
            &[],
            &trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    #[test]
    fn a_value_is_looked_for_in_the_forms_a_log_or_a_url_gives_it() {
        let forms = forms(" Ada@Example.com ");
        assert_eq!(forms, ["ada%40example.com", "ada@example.com"]);
        let phone = super::tests::forms("+45 20 30");
        assert!(phone.contains(&"45 20 30".to_owned()));
        assert!(phone.contains(&"%2b45%2020%2030".to_owned()));
        assert!(phone.contains(&"%2b45+20+30".to_owned()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_table_holds_a_value_of_an_erased_person_outside_the_allowlist(pool: PgPool) {
        let seeded = seed_everything(&pool).await;
        let planted = plant(&pool, &seeded).await;
        let before = leftovers(&pool, &planted.needles).await;
        let tables_before: Vec<&str> = before
            .iter()
            .map(|hit| hit.split('.').next().unwrap())
            .collect();
        for table in [
            "people",
            "events",
            "consent_changes",
            "ai_transcripts",
            "import_chunks",
            "import_rejects",
            "ingest_rejects",
            "contact_idempotency",
        ] {
            assert!(
                tables_before.contains(&table),
                "{table} not seeded: {before:?}"
            );
        }

        let outcome = erase(&pool, &seeded).await;

        assert!(matches!(
            outcome,
            EraseOutcome::Completed {
                person_found: true,
                ..
            }
        ));
        assert_eq!(
            leftovers(&pool, &planted.needles).await,
            Vec::<String>::new()
        );
        // The merged person's ids are gone as well.
        let merged = leftovers(&pool, &all_forms(&[&seeded.merged.to_string()])).await;
        assert_eq!(merged, Vec::<String>::new());
        // The bystander's values remain in every class that is found by value.
        let kept = leftovers(&pool, &planted.kept).await;
        for table in [
            "identities",
            "ai_transcripts",
            "import_chunks",
            "import_rejects",
            "ingest_rejects",
            "contact_idempotency",
        ] {
            assert!(
                kept.iter().any(|hit| hit.starts_with(&format!("{table}."))),
                "{table} lost the bystander's value: {kept:?}"
            );
        }
        let sends: i64 = sqlx::query_scalar("SELECT count(*) FROM test_sends")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sends, 1, "the bystander's test send stays");
        let _ = seeded.bystander;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_reports_table_column_and_count_and_never_a_value(pool: PgPool) {
        let seeded = seed_everything(&pool).await;
        let planted = plant(&pool, &seeded).await;

        let report = leftovers(&pool, &planted.needles).await.join("\n");

        assert!(
            report.contains("ai_transcripts.content: 1 rows"),
            "{report}"
        );
        let lowered = report.to_lowercase();
        for needle in &planted.needles {
            assert!(
                !lowered.contains(needle.as_str()),
                "the report holds {needle}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_table_of_the_schema_is_classified(pool: PgPool) {
        assert_eq!(unclassified(&pool).await, Vec::<String>::new());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_table_fails_the_guard_until_it_is_classified(pool: PgPool) {
        sqlx::query("CREATE TABLE zz_new_feature_notes (id uuid PRIMARY KEY, note text)")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(unclassified(&pool).await, ["zz_new_feature_notes"]);
    }

    #[test]
    fn the_classes_do_not_overlap_and_every_entry_has_a_reason() {
        let mut names: Vec<&str> = SURVIVORS.iter().map(|(name, _)| *name).collect();
        names.extend(BY_VALUE.iter().map(|(name, _)| *name));
        names.extend(
            VALUE_FREE
                .iter()
                .flat_map(|(names, _)| names.iter().copied()),
        );
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "a table sits in two classes");
        assert!(SURVIVORS.iter().all(|(_, reason)| !reason.is_empty()));
        assert!(BY_VALUE.iter().all(|(_, how)| !how.is_empty()));
        assert!(VALUE_FREE.iter().all(|(_, reason)| !reason.is_empty()));
    }
}
