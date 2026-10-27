//! The end-to-end test of the send path and of trace completeness (#232).
//!
//! The scenario runs the real thing in one process: the API router, a worker pool on a
//! `ManualClock`, the render host, PostgreSQL, the local SMTP sink, the fixture DNS resolver and
//! the fake email provider. See [`tests::the_send_path_and_its_trace_hold_end_to_end`].

mod harness;
mod mail;
mod trace;

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::{Duration, OffsetDateTime};
    use zirv_flow_connectors::destinations::{
        provider_feedback_of, provider_feedback_scheme, sign_provider_feedback,
        verify_dkim_signature,
    };

    use super::harness::{PROVIDER_SECRET, PUBLIC_URL, Reply, World, call, call_form};
    use super::{mail, trace};

    /// A synthetic address. It must appear in no decision, no trace and, once the person is
    /// erased, in no table.
    const ADDRESS: &str = "ada@example.com";
    /// The allowlisted address of the test send. It too must be gone after the erasure.
    const TEST_ADDRESS: &str = "qa@example.com";
    const OFFER_URL: &str = "https://shop.example/offer?x=1";

    fn html_doc(sender: &str) -> Value {
        json!({"schema": 1, "channel": "email", "mode": "code", "email": {
            "subject": "Your offer", "sender": sender, "lang": "en", "category": "marketing",
            "html": format!(
                "<p>Hello</p><a href=\"{OFFER_URL}\">Offer</a> \
                 <a href=\"{{{{ system.unsubscribe_url }}}}\">Unsubscribe</a>"
            ),
            "text": "Hello {{ system.unsubscribe_url }}"
        }})
    }

    /// A marketing block email from the v1 set: a heading, a button and a text block with the
    /// unsubscribe link.
    fn blocks_doc(sender: &str) -> Value {
        json!({"schema": 1, "channel": "email", "mode": "blocks", "blocks": {
        "subject": "Blocks", "sender": sender, "lang": "en", "category": "marketing",
        "sections": [
            {"id": "intro", "columns": [[
                {"type": "heading", "id": "title", "level": 1, "text": "Hello"},
                {"type": "button", "id": "cta", "label": "Open",
                 "url": "https://shop.example/blocks?b=1&c=2"}
            ]]},
            {"id": "foot", "columns": [[
                {"type": "text", "id": "legal", "content":
                    "<p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></p>"}
            ]]}
        ]}})
    }

    fn send_node(id: &str, template: &str, destination: &str) -> Value {
        json!({"id": id, "type": "action.send", "template": template, "destination": destination})
    }

    fn edge(from: &str, to: &str) -> Value {
        json!({"id": format!("{from}-{to}"), "from": from, "port": "next", "to": to})
    }

    /// The path of a link in a message: the part after the public URL.
    fn path_of(url: &str) -> &str {
        url.strip_prefix(PUBLIC_URL)
            .unwrap_or_else(|| panic!("a link on the public URL: {url}"))
    }

    /// Replaces every tracking URL, and the open pixel, so a sent message and its preview can be
    /// compared: a send signs a token into each link, a preview points them at an inert one.
    fn without_tracking(html: &str) -> String {
        let html = html.replace("&#x2f;", "/");
        let mut out = html.clone();
        for url in mail::urls_containing(&html, "/t/")
            .into_iter()
            .chain(mail::urls_containing(&html, "/u/"))
            .chain(mail::urls_containing(&html, ".invalid"))
        {
            out = out.replace(&url, "TRACKED");
        }
        // The open pixel exists in a send only.
        while let Some(start) = out.find("<img") {
            let Some(length) = out[start..].find('>') else {
                break;
            };
            if !out[start..start + length].contains("TRACKED") {
                break;
            }
            out.replace_range(start..=start + length, "");
        }
        out.replace("\r\n", "\n").trim_end().to_owned()
    }

    /// The preview of `template` for `person`, as the editor shows it.
    async fn preview_html(world: &World, template: &str, person: uuid::Uuid) -> String {
        let preview = world
            .api_ok(
                "POST",
                &format!("/templates/{template}/preview"),
                Some(json!({"person_id": person})),
                StatusCode::OK,
            )
            .await;
        assert_eq!(preview["rendered"], true, "{preview}");
        preview["html"]
            .as_str()
            .expect("an html preview")
            .to_owned()
    }

    /// Posts a signed delivery event of the fake provider for `message`.
    async fn deliver_feedback(world: &World, destination: &str, message: uuid::Uuid) -> Reply {
        let config = world.provider_config();
        let (_, verification) = provider_feedback_of(&config).expect("a feedback scheme");
        let body = json!({"event": "delivered", "id": "evt-1", "meta": {"message_id": message}})
            .to_string()
            .into_bytes();
        let headers = sign_provider_feedback(
            &provider_feedback_scheme(&verification),
            PROVIDER_SECRET,
            world.clock.now().unix_timestamp(),
            &body,
        );
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/destinations/{destination}/feedback"));
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = tower::ServiceExt::oneshot(
            world.router.clone(),
            request
                .body(axum::body::Body::from(body))
                .expect("a feedback request"),
        )
        .await
        .expect("the router answers");
        Reply {
            status: response.status(),
            headers: response.headers().clone(),
            body: Value::Null,
            text: String::new(),
        }
    }

    /// The listed tables, or all of them, as text the way a dump would show it, in both bytea
    /// encodings, except the erasure ledger, which keeps the keyed hashes on purpose.
    async fn dump_text(pool: &PgPool, only: Option<&[&str]>) -> String {
        let all: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = current_schema()",
        )
        .fetch_all(pool)
        .await
        .expect("tables");
        let mut connection = pool.acquire().await.expect("a connection");
        let mut text = String::new();
        for encoding in ["hex", "escape"] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "SET bytea_output = '{encoding}'"
            )))
            .execute(&mut *connection)
            .await
            .expect("the encoding is set");
            for table in &all {
                let wanted = only.is_none_or(|only| only.contains(&table.as_str()));
                if table == "erasure_requests" || !wanted {
                    continue;
                }
                let rows: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT string_agg(t::text, E'\\n') FROM \"{table}\" t"
                )))
                .fetch_one(&mut *connection)
                .await
                .expect("a table");
                text.push_str(&rows.unwrap_or_default());
            }
        }
        text
    }

    /// Whether `text` holds `word` other than inside a run of hex digits, so a uuid, hash or hex
    /// bytea that happens to spell it does not count.
    fn names(text: &str, word: &str) -> bool {
        text.match_indices(word).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            !before.is_some_and(|c| c.is_ascii_hexdigit())
                && !after.is_some_and(|c| c.is_ascii_hexdigit())
        })
    }

    #[test]
    fn a_name_inside_hex_is_not_a_name() {
        assert!(names("to ada@example.com", "ada"));
        assert!(names("ada%40example.com", "ada"));
        assert!(names("ada", "ada"));
        assert!(!names("4f1c-ada7-9e2b", "ada"));
        assert!(!names("\\x0ada", "ada"));
        assert!(!names("3fadab", "ada"));
    }

    /// Criterion 1 (track call to an unsubscribed second send) and criterion 9 (the trace and its
    /// API tree), end to end.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_send_path_and_its_trace_hold_end_to_end(pool: PgPool) {
        let world = World::start(pool, PUBLIC_URL).await;

        // A verified sender (its records resolve in the fixture DNS), two destinations, a
        // marketing template and a flow: send over SMTP, send through the provider, wait a day,
        // send over SMTP again. A person who is in the segment enters the flow.
        let (sender, dkim_record) = world
            .verified_sender("example.com", "news@example.com")
            .await;
        let smtp = world.smtp_destination().await;
        let provider = world.provider_destination().await;
        let offer = world.template("Offer", html_doc(&sender)).await;
        world.event_schema("order_placed").await;
        let segment = world
            .segment("Buyers", json!({"v": 1, "event": "order_placed"}))
            .await;
        let graph = json!({"v": 1, "nodes": [
            send_node("welcome", &offer, &smtp),
            send_node("receipt", &offer, &provider),
            {"id": "pause", "type": "wait.duration", "seconds": 86400},
            send_node("reminder", &offer, &smtp),
            {"id": "end", "type": "exit"}],
            "edges": [edge("welcome", "receipt"), edge("receipt", "pause"),
                      edge("pause", "reminder"), edge("reminder", "end")]});
        let flow = world
            .flow(
                "Offer flow",
                graph,
                json!({"trigger": {"type": "segment", "segment_id": segment, "on": "entered"},
                       "reentry": {"mode": "once"}}),
            )
            .await;

        // The person exists and subscribed.
        let person = world.subscribed_person(ADDRESS).await;

        // Step 1 and 2: a track call moves the person into the segment, which starts the flow.
        let track_trace = world.track_order(ADDRESS).await;
        // The flow reaches its wait on this instant of the manual clock.
        let waited_at = world.clock.now();
        world.settle().await;
        assert_eq!(
            world
                .count("SELECT count(*) FROM segment_members WHERE exited_at IS NULL")
                .await,
            1,
            "the event moved the person into the segment"
        );

        // Step 3: the sink captured the SMTP message, with the one-click headers and a DKIM
        // signature that verifies against the record the domain publishes.
        let captured = world.sink.messages();
        assert_eq!(
            captured.len(),
            1,
            "the first SMTP send; the second waits a day"
        );
        let welcome = &captured[0];
        assert_eq!(welcome.recipients, [ADDRESS]);
        assert_eq!(welcome.mail_from, "news@example.com");
        let data = &welcome.data;
        let list_unsubscribe = mail::header(data, "List-Unsubscribe").expect("List-Unsubscribe");
        assert!(
            list_unsubscribe.starts_with(&format!("<{PUBLIC_URL}/u/")),
            "{list_unsubscribe}"
        );
        assert_eq!(
            mail::header(data, "List-Unsubscribe-Post").as_deref(),
            Some("List-Unsubscribe=One-Click")
        );
        let signature = mail::header(data, "DKIM-Signature").expect("a DKIM signature");
        assert!(signature.contains("d=example.com;"), "{signature}");
        assert_eq!(verify_dkim_signature(data, &dkim_record), Ok(()));
        assert_eq!(
            world.provider.accepted().len(),
            1,
            "the provider got the second send"
        );

        // The HTML that went out is the preview, apart from the tracking tokens.
        let html = mail::part(data, "text/html").expect("an html part");
        assert_eq!(
            without_tracking(&html),
            without_tracking(&preview_html(&world, &offer, person).await)
        );

        // Step 4: the click is recorded and redirects to the URL recorded for the link.
        world.clock.advance(Duration::minutes(1));
        let links = mail::urls_containing(&html, "/t/c/");
        assert_eq!(links.len(), 1, "{links:?}");
        let click = call(&world.router, "GET", path_of(&links[0]), None, None).await;
        assert_eq!(click.status, StatusCode::FOUND);
        assert_eq!(click.headers["location"], OFFER_URL);
        assert_eq!(
            world
                .count("SELECT count(*) FROM events WHERE name = 'email.clicked'")
                .await,
            1
        );

        // Step 5: the one-click unsubscribe updates consent.
        world.clock.advance(Duration::minutes(1));
        let unsubscribe_url = list_unsubscribe
            .trim_start_matches('<')
            .split('>')
            .next()
            .expect("a URL")
            .to_owned();
        let one_click = call_form(
            &world.router,
            "POST",
            path_of(&unsubscribe_url),
            "List-Unsubscribe=One-Click",
        )
        .await;
        assert_eq!(one_click.status, StatusCode::OK, "{}", one_click.text);
        let consent = world
            .api_ok(
                "GET",
                &format!("/people/{person}/consent"),
                None,
                StatusCode::OK,
            )
            .await;
        assert_eq!(consent["states"][0]["channel"], "email");
        assert_eq!(consent["states"][0]["status"], "unsubscribed", "{consent}");

        // The provider reports the receipt delivered, signed as it signs.
        world.clock.advance(Duration::minutes(1));
        let receipt: uuid::Uuid =
            sqlx::query_scalar("SELECT id FROM messages WHERE node_id = 'receipt'")
                .fetch_one(&world.pool)
                .await
                .expect("the receipt message");
        let feedback = deliver_feedback(&world, &provider, receipt).await;
        assert_eq!(feedback.status, StatusCode::ACCEPTED);
        assert_eq!(
            world
                .count("SELECT count(*) FROM events WHERE name = 'email.delivered'")
                .await,
            1
        );

        // Step 6: a day later the flow runs its second send, which is skipped for no consent.
        // The wake was scheduled from the manual clock: a day after the wait began.
        let scheduled: OffsetDateTime = sqlx::query_scalar(
            "SELECT run_at FROM jobs WHERE kind = 'flow.step' AND state = 'ready'",
        )
        .fetch_one(&world.pool)
        .await
        .expect("exactly one step is waiting");
        assert!(
            (scheduled - (waited_at + Duration::days(1))).abs() < Duration::milliseconds(1),
            "the wake is due a day after {waited_at}, not at {scheduled}"
        );
        world.clock.advance(Duration::days(1));
        // The queue claims by database time, so the due wake is moved there.
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'flow.step'")
            .execute(&world.pool)
            .await
            .expect("the wake is due");
        world.settle().await;
        assert_eq!(world.sink.messages().len(), 1, "nothing more went out");
        let (status, reason): (String, Option<String>) =
            sqlx::query_as("SELECT status, reason FROM messages WHERE node_id = 'reminder'")
                .fetch_one(&world.pool)
                .await
                .expect("the reminder message");
        assert_eq!(
            (status.as_str(), reason.as_deref()),
            ("skipped", Some("no_consent"))
        );

        // Criterion 9, API: one trace, the track call's, holds the event, the segment entry, the
        // enrollment, the steps, the messages, the unsubscribe, the delivery feedback and the
        // later skipped send. The enrollment continues the trace of the segment entry.
        let tree = world
            .api_ok(
                "GET",
                &format!("/traces/{track_trace}"),
                None,
                StatusCode::OK,
            )
            .await;
        let enrollments = world
            .api_ok(
                "GET",
                &format!("/flows/{flow}/enrollments"),
                None,
                StatusCode::OK,
            )
            .await;
        assert_eq!(
            enrollments["enrollments"][0]["trace_id"], track_trace,
            "the enrollment continues the trace of the track call"
        );
        assert_eq!(tree["truncated"], false);
        let records = trace::records_of(&tree);
        if let Err(gap) = trace::complete(&records) {
            panic!("{gap}");
        }

        // No address value in the trace response or in any stored decision.
        let shown = serde_json::to_string(&records).expect("text");
        assert!(!shown.contains('@'), "an address is in the trace");
        let body = tree.to_string();
        for marker in ["ada@", ADDRESS, "qa@", TEST_ADDRESS] {
            assert!(!body.contains(marker), "{marker} is in the trace response");
        }

        // Criterion 9, UI: the trace, normalised, is the fixture the web test renders.
        trace::bless_or_compare(&trace::normalised(&track_trace, records));

        // The message log of the person agrees with the trace on the later send.
        let log = world
            .api_ok(
                "GET",
                &format!("/people/{person}/messages?result=skipped"),
                None,
                StatusCode::OK,
            )
            .await;
        assert_eq!(log["messages"].as_array().map(Vec::len), Some(1), "{log}");
        assert_eq!(log["messages"][0]["reason"], "no_consent");

        // A test send to an allowlisted address is logged, so the erasure scan has one to find.
        world
            .api_ok(
                "POST",
                "/test-send-allowlist",
                Some(json!({"address": TEST_ADDRESS})),
                StatusCode::CREATED,
            )
            .await;
        world
            .api_ok(
                "POST",
                &format!("/templates/{offer}/test-send"),
                Some(json!({"destination_id": smtp, "sender_identity_id": sender,
                            "to": {"address": TEST_ADDRESS}})),
                StatusCode::ACCEPTED,
            )
            .await;
        world.settle().await;
        assert_eq!(world.count("SELECT count(*) FROM test_sends").await, 1);
        // No address in any stored decision or job, the test send's included.
        let decisions = dump_text(&world.pool, Some(&["decision_records", "jobs"])).await;
        for marker in ["ada@", ADDRESS, "qa@", TEST_ADDRESS] {
            assert!(
                !decisions.contains(marker),
                "{marker} is in a stored decision"
            );
        }

        // The erasure scan, once, at the end: the person is erased, and nothing of them is left
        // in `messages`, `message_links` or `test_sends`, nor anywhere else.
        assert_eq!(world.count("SELECT count(*) FROM messages").await, 3);
        assert!(world.count("SELECT count(*) FROM message_links").await >= 1);
        world
            .api_ok(
                "POST",
                "/privacy/erasures",
                Some(json!({"person_id": person})),
                StatusCode::ACCEPTED,
            )
            .await;
        world.settle().await;
        for table in ["messages", "message_links", "people", "identities"] {
            assert_eq!(
                world.count(&format!("SELECT count(*) FROM {table}")).await,
                0,
                "{table} still holds rows of the erased person"
            );
        }
        let leaked = dump_text(
            &world.pool,
            Some(&["messages", "message_links", "test_sends"]),
        )
        .await;
        assert!(!names(&leaked, "ada"), "the erased person is still named");
        let everything = dump_text(&world.pool, None).await;
        for marker in [ADDRESS, TEST_ADDRESS] {
            assert!(
                !everything.contains(marker),
                "{marker} survives the erasure"
            );
        }
        world.finish().await;
    }

    /// The blocks step of the scenario: a flow sends a block document built from the v1 set, and
    /// the HTML the sink captures is the preview, apart from the tracking tokens.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_document_email_is_the_preview_render_apart_from_tracking(pool: PgPool) {
        let world = World::start(pool, PUBLIC_URL).await;
        let (sender, _) = world
            .verified_sender("example.com", "news@example.com")
            .await;
        let smtp = world.smtp_destination().await;
        let blocks = world.template("Blocks", blocks_doc(&sender)).await;
        world.event_schema("order_placed").await;
        let segment = world
            .segment("Buyers", json!({"v": 1, "event": "order_placed"}))
            .await;
        world
            .flow(
                "Blocks flow",
                json!({"v": 1, "nodes": [send_node("blocks", &blocks, &smtp),
                                         {"id": "end", "type": "exit"}],
                       "edges": [edge("blocks", "end")]}),
                json!({"trigger": {"type": "segment", "segment_id": segment, "on": "entered"},
                       "reentry": {"mode": "once"}}),
            )
            .await;
        let person = world.subscribed_person(ADDRESS).await;
        world.track_order(ADDRESS).await;
        world.settle().await;

        let captured = world.sink.messages();
        assert_eq!(captured.len(), 1, "the block email was sent");
        let html = mail::part(&captured[0].data, "text/html").expect("an html part");
        assert_eq!(
            without_tracking(&html),
            without_tracking(&preview_html(&world, &blocks, person).await)
        );
        world.finish().await;
    }

    /// Creates and publishes a snippet or a theme through the API; returns its id.
    async fn published(world: &World, kind: &str, name: &str, content: Value) -> String {
        let (path, field) = match kind {
            "snippet" => ("snippets", "content"),
            _ => ("themes", "tokens"),
        };
        let created = world
            .api_ok(
                "POST",
                &format!("/{path}"),
                Some(json!({"name": name, field: content})),
                StatusCode::CREATED,
            )
            .await;
        let id = created["id"].as_str().expect("an id").to_owned();
        world
            .api_ok(
                "POST",
                &format!("/{path}/{id}/publish"),
                None,
                StatusCode::CREATED,
            )
            .await;
        id
    }

    /// Preview equals send for the inputs of #307: a named theme with a footer snippet, a snippet
    /// block, and blocks whose `visible_if` is true and false for the person. The preview route
    /// and the send job resolve and render through the same loader, conversion and pool call, so
    /// the HTML the sink captures is the preview apart from the tracking tokens.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_themed_email_with_snippets_and_conditions_is_the_preview_apart_from_tracking(
        pool: PgPool,
    ) {
        let world = World::start(pool, PUBLIC_URL).await;
        let (sender, _) = world
            .verified_sender("example.com", "news@example.com")
            .await;
        let smtp = world.smtp_destination().await;
        let legal = published(
            &world,
            "snippet",
            "Legal",
            json!({"blocks": [
                {"type": "heading", "id": "legal_title", "level": 3, "text": "FOOTER SNIPPET TEXT"},
                {"type": "text", "id": "legal_link", "content":
                    "<p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></p>"}
            ]}),
        )
        .await;
        let promo = published(
            &world,
            "snippet",
            "Promo",
            json!({"blocks": [
                {"type": "heading", "id": "promo_title", "level": 2, "text": "PROMO SNIPPET TEXT"}
            ]}),
        )
        .await;
        let theme = published(
            &world,
            "theme",
            "Brand",
            json!({"primary": "#aa0000", "footer_snippet": legal}),
        )
        .await;
        let document = json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
            "subject": "Themed", "sender": sender, "lang": "en", "category": "marketing",
            "theme": {"id": theme},
            "sections": [{"id": "main", "columns": [[
                {"type": "snippet", "id": "promo", "snippet_id": promo},
                {"type": "text", "id": "shown", "content": "<p>CONDITION MET TEXT</p>",
                 "visible_if": {"identifier": "email"}},
                {"type": "text", "id": "hidden", "content": "<p>CONDITION UNMET TEXT</p>",
                 "visible_if": {"identifier": "phone"}}
            ]]}]
        }});
        let template = world.template("Themed", document).await;
        world.event_schema("order_placed").await;
        let segment = world
            .segment("Buyers", json!({"v": 1, "event": "order_placed"}))
            .await;
        world
            .flow(
                "Themed flow",
                json!({"v": 1, "nodes": [send_node("themed", &template, &smtp),
                                         {"id": "end", "type": "exit"}],
                       "edges": [edge("themed", "end")]}),
                json!({"trigger": {"type": "segment", "segment_id": segment, "on": "entered"},
                       "reentry": {"mode": "once"}}),
            )
            .await;
        let person = world.subscribed_person(ADDRESS).await;
        world.track_order(ADDRESS).await;
        world.settle().await;

        let captured = world.sink.messages();
        assert_eq!(captured.len(), 1, "the themed email was sent");
        let sent = without_tracking(&mail::part(&captured[0].data, "text/html").expect("html"));
        let previewed = without_tracking(&preview_html(&world, &template, person).await);

        assert_eq!(sent, previewed);
        for present in [
            "PROMO SNIPPET TEXT",
            "FOOTER SNIPPET TEXT",
            "CONDITION MET TEXT",
            "#aa0000",
        ] {
            assert!(sent.contains(present), "{present} is in the sent HTML");
        }
        assert!(!sent.contains("CONDITION UNMET TEXT"));
        world.finish().await;
    }

    /// The smoke run of the send benchmark scenario (#233): the same shape the bench scripts
    /// seed (a verified domain, one SMTP destination, a published block template and a published
    /// HTML template, a manual flow with an `action.send` node per template), small N, through
    /// the in-process sink. People are identified through ingest, subscribed through the
    /// consent endpoint and enrolled by starting the flow for a list.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_send_bench_scenario_delivers_one_message_per_person_per_template(pool: PgPool) {
        const PEOPLE: usize = 12;
        let world = World::start(pool, PUBLIC_URL).await;
        let (sender, _) = world
            .verified_sender("example.com", "news@example.com")
            .await;
        let smtp = world.smtp_destination().await;
        let blocks = world.template("Bench blocks", blocks_doc(&sender)).await;
        let html = world.template("Bench html", html_doc(&sender)).await;

        let items: Vec<Value> = (0..PEOPLE)
            .map(|n| {
                json!({"type": "identify",
                       "identifiers": {"email": format!("sb-{n}@bench.example")},
                       "messageId": format!("sb-{n}")})
            })
            .collect();
        let identify = call(
            &world.router,
            "POST",
            "/api/v1/ingest/batch",
            Some(json!({ "batch": items })),
            Some(&world.ingest),
        )
        .await;
        assert_eq!(identify.status, StatusCode::ACCEPTED, "{}", identify.text);
        world.settle().await;
        let people: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM people")
            .fetch_all(&world.pool)
            .await
            .expect("people");
        assert_eq!(people.len(), PEOPLE);
        for person in &people {
            world
                .api_ok(
                    "PUT",
                    &format!("/people/{person}/consent"),
                    Some(json!({"channel": "email", "status": "subscribed"})),
                    StatusCode::OK,
                )
                .await;
        }

        let mut expected = 0;
        for (name, template) in [("blocks", &blocks), ("html", &html)] {
            let list = world
                .api_ok(
                    "POST",
                    "/lists",
                    Some(json!({"name": format!("bench {name}")})),
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
                    Some(json!({"person_ids": people})),
                    StatusCode::OK,
                )
                .await;
            let flow = world
                .flow(
                    &format!("Bench {name}"),
                    json!({"v": 1, "nodes": [send_node("send", template, &smtp),
                                             {"id": "end", "type": "exit"}],
                           "edges": [edge("send", "end")]}),
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
            expected += PEOPLE;
            assert_eq!(
                world.sink.messages().len(),
                expected,
                "after the {name} flow"
            );
        }

        let mut recipients: Vec<String> = world
            .sink
            .messages()
            .iter()
            .flat_map(|message| message.recipients.clone())
            .collect();
        recipients.sort();
        recipients.dedup();
        assert_eq!(recipients.len(), PEOPLE, "each person got mail");
        world.finish().await;
    }
}
