//! The privacy half of exit criterion 2 (#374), end to end: the default-level logs hold no
//! identifier values, and the destinations that declare erase receive the erase call.

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_api::AppState;
    use zirv_flow_api::admin::{self, Context};
    use zirv_flow_connectors::destinations::{
        FakeProvider, FakeProviderOptions, FakeProviderScript, fake_ledger_erased,
    };
    use zirv_flow_core::people::{Identifier, identifier_hash};
    use zirv_flow_core::{HmacPurpose, HmacRoot};
    use zirv_flow_store::idempotency_key;

    use crate::config::LogFormat;
    use crate::e2e::harness::{PUBLIC_URL, World, call};
    use crate::log::{self, Captured};

    const EMAIL: &str = "Zelda.Quillfeather@Privacy-Probe.example";
    const PHONE_TYPED: &str = "+45 27 18 49 36";
    const PHONE: &str = "+4527184936";
    const EXTERNAL: &str = "crm-9c41d7e2-zq";
    const NAME: &str = "Zelda Quillfeather";
    const SURNAME: &str = "Quillfeather";

    /// One seeded value, with the label a failure names it by.
    struct Seed {
        label: &'static str,
        value: &'static str,
        /// The identifier whose keyed hash stands in for the value, if it is one.
        identifier: Option<(&'static str, &'static str)>,
    }

    const SEEDS: [Seed; 6] = [
        Seed {
            label: "email",
            value: EMAIL,
            identifier: Some(("email", EMAIL)),
        },
        Seed {
            label: "phone as typed",
            value: PHONE_TYPED,
            identifier: None,
        },
        Seed {
            label: "phone",
            value: PHONE,
            identifier: Some(("phone", PHONE_TYPED)),
        },
        Seed {
            label: "external id",
            value: EXTERNAL,
            identifier: Some(("external:crm", EXTERNAL)),
        },
        Seed {
            label: "name",
            value: NAME,
            identifier: None,
        },
        Seed {
            label: "surname",
            value: SURNAME,
            identifier: None,
        },
    ];

    type Needles = Vec<(&'static str, &'static str, String)>;

    fn percent_encode(value: &str) -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(byte));
            } else {
                out.push_str(&format!("%{byte:02x}"));
            }
        }
        out
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The forms of every seeded value the scan looks for, lowercased: raw (which covers the
    /// lowercase form once the text is lowercased too), percent-encoded, form-encoded and the
    /// first 12 hex digits of the keyed identifier hash the product uses.
    fn needles(root: &HmacRoot) -> Needles {
        let key = root.hmac_key(HmacPurpose::Identifier);
        let mut out = Vec::new();
        for seed in &SEEDS {
            out.push((seed.label, "raw", seed.value.to_lowercase()));
            let encoded = percent_encode(seed.value).to_lowercase();
            out.push((seed.label, "form-encoded", encoded.replace("%20", "+")));
            out.push((seed.label, "percent-encoded", encoded));
            if let Some((kind, value)) = seed.identifier {
                let identifier = Identifier::parse(kind, value).expect("an identifier");
                let hash = hex(&identifier_hash(&key, &identifier));
                out.push((seed.label, "hash prefix", hash[..12].to_owned()));
            }
        }
        out
    }

    /// The seeded values found in `text`, by label and form, never by value.
    fn leaks(text: &str, needles: &Needles) -> Vec<String> {
        let text = text.to_lowercase();
        needles
            .iter()
            .filter(|(_, _, needle)| text.contains(needle.as_str()))
            .map(|(label, form, _)| format!("{label} ({form})"))
            .collect()
    }

    fn probe_root() -> HmacRoot {
        HmacRoot::bootstrap_from(
            &zirv_flow_store::MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=")
                .expect("key"),
        )
    }

    #[test]
    fn the_scan_finds_a_seeded_value_in_every_form_and_a_control_line_fails_it() {
        let root = probe_root();
        let needles = needles(&root);
        let captured = Captured::default();
        let dispatch = log::dispatch("info", LogFormat::Json, captured.clone());

        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(count = 3, "a clean line");
        });
        assert_eq!(leaks(&captured.text(), &needles), Vec::<String>::new());

        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(email = EMAIL, "control line");
        });
        let found = leaks(&captured.text(), &needles);
        assert!(found.contains(&"email (raw)".to_owned()), "{found:?}");

        let encoded = format!("q={}", percent_encode(NAME));
        assert!(leaks(&encoded, &needles).contains(&"name (percent-encoded)".to_owned()));
        let key = root.hmac_key(HmacPurpose::Identifier);
        let identifier = Identifier::parse("email", EMAIL).expect("an identifier");
        let hash = hex(&identifier_hash(&key, &identifier));
        assert!(leaks(&hash[..16], &needles).contains(&"email (hash prefix)".to_owned()));
    }

    fn html_doc(sender: &str) -> Value {
        json!({"schema": 1, "channel": "email", "mode": "code", "email": {
            "subject": "Your offer", "sender": sender, "lang": "en", "category": "marketing",
            "html": "<p>Hello</p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
            "text": "Hello {{ system.unsubscribe_url }}"
        }})
    }

    /// A message of the person through the destination, which is what makes the destination one
    /// that handled them.
    async fn messaged(world: &World, person: Uuid, destination: &str) {
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status, reason, completed_at) \
             VALUES (gen_random_uuid(), $1, $3, gen_random_uuid(), 'n', gen_random_uuid(), \
                $2::uuid, 'skipped', 'no_consent', now())",
        )
        .bind(world.ws.as_uuid())
        .bind(destination)
        .bind(person)
        .execute(&world.pool)
        .await
        .expect("a message");
    }

    async fn fake_destination(world: &World, name: &str, ledger: &str, mode: &str) -> String {
        let credential = world
            .api_ok(
                "POST",
                "/credentials",
                Some(json!({"kind": "custom", "name": name,
                            "schema": {"fields": [{"key": "token", "label": "token",
                                "type": "string", "required": true, "secret": true}]},
                            "secret": {"token": "ok"}})),
                StatusCode::CREATED,
            )
            .await;
        world
            .api_ok(
                "POST",
                "/destinations",
                Some(json!({"kind": "fake", "name": name,
                            "config": {"mode": mode, "ledger": ledger},
                            "credential_id": credential["id"], "rate": 100, "burst": 100})),
                StatusCode::CREATED,
            )
            .await["id"]
            .as_str()
            .expect("a destination id")
            .to_owned()
    }

    /// Identifies the person through ingest and subscribes them; returns their id.
    async fn identified(world: &World) -> Uuid {
        let reply = call(
            &world.router,
            "POST",
            "/api/v1/ingest/batch",
            Some(json!({"batch": [
                {"type": "identify", "messageId": "p-1",
                 "identifiers": {"email": EMAIL, "phone": PHONE_TYPED, "external:crm": EXTERNAL},
                 "traits": {"name": NAME}},
            ]})),
            Some(&world.ingest),
        )
        .await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text);
        world.settle().await;
        world.the_person().await
    }

    /// Requests the erasure; returns the ledger id.
    async fn erase(world: &World, person: Uuid) -> Uuid {
        let accepted = world
            .api_ok(
                "POST",
                "/privacy/erasures",
                Some(json!({"person_id": person})),
                StatusCode::ACCEPTED,
            )
            .await;
        accepted["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .expect("a ledger id")
    }

    async fn call_states(world: &World) -> Vec<(Uuid, String, Option<String>, i32)> {
        sqlx::query_as(
            "SELECT destination_id, outcome, error_code, attempts FROM erasure_destination_calls \
             ORDER BY destination_id",
        )
        .fetch_all(&world.pool)
        .await
        .expect("calls")
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn default_level_logs_hold_no_identifier_value_through_the_whole_privacy_scenario(
        pool: PgPool,
    ) {
        let captured = Captured::default();
        let dispatch = log::dispatch("info", LogFormat::Json, captured.clone());
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let started = std::time::Instant::now();
        let world = World::start(pool, PUBLIC_URL).await;
        let needles = needles(&world.root);

        let (sender, _) = world
            .verified_sender("example.com", "news@example.com")
            .await;
        let provider = world.provider_destination().await;
        let ledger = format!("privacy-{}", Uuid::now_v7());
        let erasing = fake_destination(&world, "Erasing", &ledger, "server_error").await;
        let offer = world.template("Offer", html_doc(&sender)).await;
        world.event_schema("order_placed").await;
        world
            .api_ok(
                "PUT",
                "/event-policy",
                Some(json!({"unplanned": "reject", "future_skew_seconds": 300,
                            "lateness_seconds": 86400, "redaction": {}})),
                StatusCode::OK,
            )
            .await;

        // Batch ingest: identify and track items, one the policy rejects at the drain.
        let batch = call(
            &world.router,
            "POST",
            "/api/v1/ingest/batch",
            Some(json!({"batch": [
                {"type": "identify", "messageId": "p-1",
                 "identifiers": {"email": EMAIL, "phone": PHONE_TYPED, "external:crm": EXTERNAL},
                 "traits": {"name": NAME}},
                {"type": "track", "messageId": "p-2", "event": "order_placed",
                 "identifiers": {"email": EMAIL}, "properties": {"customer": NAME}},
                {"type": "track", "messageId": "p-3", "event": "unplanned_probe",
                 "identifiers": {"email": EMAIL}, "properties": {"customer": NAME}},
            ]})),
            Some(&world.ingest),
        )
        .await;
        assert_eq!(batch.status, StatusCode::ACCEPTED, "{}", batch.text);
        // A malformed item refuses the whole batch.
        let malformed = call(
            &world.router,
            "POST",
            "/api/v1/ingest/batch",
            Some(json!({"batch": [
                {"type": "identify", "messageId": "p-4",
                 "identifiers": {"email": "Zelda.Quillfeather at Privacy-Probe.example"},
                 "traits": {"name": NAME}},
            ]})),
            Some(&world.ingest),
        )
        .await;
        assert_eq!(
            malformed.status,
            StatusCode::BAD_REQUEST,
            "{}",
            malformed.text
        );
        world.settle().await;
        let person = world.the_person().await;
        world
            .api_ok(
                "PUT",
                &format!("/people/{person}/consent"),
                Some(json!({"channel": "email", "status": "subscribed"})),
                StatusCode::OK,
            )
            .await;

        // A profile read, a 4xx and a 5xx.
        world
            .api_ok("GET", &format!("/people/{person}"), None, StatusCode::OK)
            .await;
        let missing = world
            .api("GET", &format!("/people/{}", Uuid::now_v7()), None)
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let bare = zirv_flow_api::router(AppState::new(world.pool.clone(), Arc::new(String::new)));
        let unavailable = call(
            &bare,
            "POST",
            &world.path("/privacy/erasures"),
            Some(json!({"person_id": person})),
            Some(&world.admin),
        )
        .await;
        assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);

        // A failing send: the provider refuses the message for good.
        world.provider.script(FakeProviderScript::Status {
            status: 422,
            body: json!({"error": "refused"}),
            retry_after: None,
        });
        let list = world
            .api_ok(
                "POST",
                "/lists",
                Some(json!({"name": "Probe"})),
                StatusCode::CREATED,
            )
            .await["id"]
            .as_str()
            .expect("a list id")
            .to_owned();
        world
            .api_ok(
                "POST",
                &format!("/lists/{list}/members"),
                Some(json!({"person_ids": [person]})),
                StatusCode::OK,
            )
            .await;
        let flow = world
            .flow(
                "Probe",
                json!({"v": 1, "nodes": [
                    {"id": "send", "type": "action.send", "template": offer,
                     "destination": provider},
                    {"id": "end", "type": "exit"}],
                    "edges": [{"id": "e", "from": "send", "port": "next", "to": "end"}]}),
                json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
            )
            .await;
        world
            .api_ok(
                "POST",
                &format!("/flows/{flow}/start"),
                Some(json!({"list_id": list})),
                StatusCode::ACCEPTED,
            )
            .await;
        world.settle().await;
        assert_eq!(world.provider.accepted().len(), 0, "the send failed");
        assert!(
            world.provider.requests() >= 1,
            "the send reached the provider"
        );
        assert_eq!(world.count("SELECT count(*) FROM ingest_rejects").await, 1);

        // An export and its download.
        let export = world
            .api_ok(
                "POST",
                "/privacy/exports",
                Some(json!({"person_id": person})),
                StatusCode::ACCEPTED,
            )
            .await["id"]
            .as_str()
            .expect("an export id")
            .to_owned();
        world.settle().await;
        let download = world
            .api("GET", &format!("/privacy/exports/{export}/download"), None)
            .await;
        assert_eq!(download.status, StatusCode::OK, "{}", download.status);

        // An erasure whose destination fails its call, and the replay.
        messaged(&world, person, &erasing).await;
        erase(&world, person).await;
        world.settle().await;
        // The first answer is retryable; the retry is terminal.
        world
            .api_ok(
                "PATCH",
                &format!("/destinations/{erasing}"),
                Some(json!({"config": {"mode": "rejected", "ledger": ledger}})),
                StatusCode::OK,
            )
            .await;
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'destination.erase'")
            .execute(&world.pool)
            .await
            .expect("the retry is due");
        world.settle().await;
        assert_eq!(world.count("SELECT count(*) FROM people").await, 0);
        admin::replay_erasures(&world.pool, &Context::system(), Some("acme"), &world.root)
            .await
            .expect("the replay");
        assert_eq!(fake_ledger_erased(&ledger).len(), 2);

        let text = captured.text();
        let lines = text.lines().count();
        assert!(lines >= 10, "the scenario logged only {lines} lines");
        assert!(
            text.contains("an erase call failed"),
            "the failing erase call logged nothing"
        );
        let found = leaks(&text, &needles);
        assert!(found.is_empty(), "the logs hold seeded values: {found:?}");

        // The control: the same scan fails on a seeded value written to a test-only line.
        tracing::info!(email = EMAIL, "control line");
        assert!(!leaks(&captured.text(), &needles).is_empty());
        eprintln!("privacy log scenario took {:?}", started.elapsed());
        world.finish().await;
    }

    /// The destinations a person went through: the test fake that declares erase, a provider
    /// with and one without an `erase_profile`, and SMTP, which declares none.
    struct Four {
        fake: String,
        ledger: String,
        with: String,
        with_provider: FakeProvider,
        without_provider: FakeProvider,
    }

    async fn four_destinations(world: &World, person: Uuid, fake_mode: &str) -> Four {
        let ledger = format!("privacy-{}", Uuid::now_v7());
        let fake = fake_destination(world, "Erasing", &ledger, fake_mode).await;
        let with_provider = FakeProvider::start(FakeProviderOptions::default()).await;
        let without_provider = FakeProvider::start(FakeProviderOptions::default()).await;
        let with = world
            .provider_destination_on("With erase", &with_provider, true)
            .await;
        let without = world
            .provider_destination_on("Without erase", &without_provider, false)
            .await;
        let smtp = world.smtp_destination().await;
        for destination in [&fake, &with, &without, &smtp] {
            messaged(world, person, destination).await;
        }
        Four {
            fake,
            ledger,
            with,
            with_provider,
            without_provider,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_the_destinations_that_declare_erase_receive_one_call_with_a_stable_key(
        pool: PgPool,
    ) {
        let world = World::start(pool, PUBLIC_URL).await;
        let person = identified(&world).await;
        let four = four_destinations(&world, person, "ok").await;

        let ledger = erase(&world, person).await;
        world.settle().await;

        assert_eq!(world.count("SELECT count(*) FROM people").await, 0);
        let erased = fake_ledger_erased(&four.ledger);
        assert_eq!(erased.len(), 1, "{erased:?}");
        assert_eq!(erased[0].subject, person.to_string());
        let fake: Uuid = four.fake.parse().expect("an id");
        let with: Uuid = four.with.parse().expect("an id");
        assert_eq!(erased[0].key, idempotency_key(ledger, fake));
        let accepted = four.with_provider.accepted();
        assert_eq!(accepted.len(), 1, "{accepted:?}");
        assert_eq!(accepted[0].key, idempotency_key(ledger, with));
        assert_eq!(accepted[0].path, "/v1/erasures");
        assert_eq!(accepted[0].body, json!({"subject": EMAIL.to_lowercase()}));
        assert_eq!(four.without_provider.requests(), 0);
        assert_eq!(world.sink.messages().len(), 0, "SMTP got no erase call");
        let calls = call_states(&world).await;
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls.iter().all(|call| call.1 == "done" && call.3 == 1));
        let listed = world
            .api_ok("GET", "/privacy/erasures", None, StatusCode::OK)
            .await;
        assert_eq!(
            listed["erasures"][0]["destination_calls"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        world.finish().await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retryable_then_a_terminal_failure_is_recorded_per_destination_and_blocks_nothing(
        pool: PgPool,
    ) {
        let world = World::start(pool, PUBLIC_URL).await;
        let person = identified(&world).await;
        let ledger_name = format!("privacy-{}", Uuid::now_v7());
        let fake = fake_destination(&world, "Erasing", &ledger_name, "server_error").await;
        messaged(&world, person, &fake).await;

        erase(&world, person).await;
        world.settle().await;

        let calls = call_states(&world).await;
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].1, "pending", "{calls:?}");
        assert_eq!(world.count("SELECT count(*) FROM people").await, 0);

        world
            .api_ok(
                "PATCH",
                &format!("/destinations/{fake}"),
                Some(json!({"config": {"mode": "rejected", "ledger": ledger_name}})),
                StatusCode::OK,
            )
            .await;
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'destination.erase'")
            .execute(&world.pool)
            .await
            .expect("the retry is due");
        world.settle().await;

        let calls = call_states(&world).await;
        assert_eq!(calls[0].1, "failed", "{calls:?}");
        assert_eq!(calls[0].2.as_deref(), Some("http_4xx"));
        assert_eq!(fake_ledger_erased(&ledger_name).len(), 2);
        let listed = world
            .api_ok("GET", "/privacy/erasures", None, StatusCode::OK)
            .await;
        let call = &listed["erasures"][0]["destination_calls"][0];
        assert_eq!(call["outcome"], "failed");
        assert_eq!(call["error_code"], "http_4xx");
        assert_eq!(listed["erasures"][0]["status"], "completed");
        assert_eq!(world.count("SELECT count(*) FROM people").await, 0);
        let errors: Vec<Option<String>> = sqlx::query_scalar("SELECT last_error FROM jobs")
            .fetch_all(&world.pool)
            .await
            .expect("jobs");
        let needles = needles(&world.root);
        let shown = format!("{listed} {errors:?} {calls:?}");
        assert!(
            leaks(&shown, &needles).is_empty(),
            "an identifier in a failure"
        );
        world.finish().await;
    }
}
