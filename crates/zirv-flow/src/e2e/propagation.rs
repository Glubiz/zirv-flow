//! Trace and request causes across the real API, job and decision boundaries (#385).

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_api::admin::{self, Context};
    use zirv_flow_connectors::destinations::{
        FakeProviderScript, provider_feedback_of, provider_feedback_scheme, sign_provider_feedback,
    };
    use zirv_flow_connectors::http::HttpLimits;
    use zirv_flow_connectors::sources::{Credential, Registry, SourceContext};
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{Cause, Role, TraceContext};
    use zirv_flow_jobs::{Scheduler, SchedulerConfig, Tick};
    use zirv_flow_store::decision_testing::PERSONAL_DATA;
    use zirv_flow_store::{
        DiscoveredStream, NewDestination, NewRule, NewSource, NewTraitDefinition, ProfileRecord,
        RuleWrite, RunKind, RunTrigger, StreamSettings,
    };

    use super::super::harness::{PROVIDER_SECRET, PUBLIC_URL, Reply, World, call};
    use super::super::trace::{one_tree, records_of};
    use crate::config::LogFormat;
    use crate::log::{self, Captured};

    fn header_of(reply: &Reply, name: &str) -> String {
        reply.headers[name].to_str().unwrap().to_owned()
    }

    async fn records(world: &World, trace: &str) -> Vec<Value> {
        let body = world
            .api_ok("GET", &format!("/traces/{trace}"), None, StatusCode::OK)
            .await;
        assert_eq!(body["trace_id"], trace);
        assert_eq!(body["truncated"], false);
        let records = records_of(&body);
        assert!(!records.is_empty(), "the trace has decisions");
        records
    }

    /// Requests, users, sync runs and schedules are roots outside the decision log. Include
    /// their independently known id so the same checker also covers trees rooted in those rows.
    fn rooted_tree(records: &[Value], root: &str) {
        assert!(!records.is_empty());
        let mut tree = vec![json!({"id": root, "subject_ids": {}})];
        tree.extend_from_slice(records);
        assert_eq!(one_tree(&tree), Ok(()), "{records:#?}");
    }

    fn cause_at<'a>(
        records: &'a [Value],
        action: &str,
        outcome: &str,
        kind: &str,
        id: &str,
    ) -> &'a Value {
        let found: Vec<_> = records
            .iter()
            .filter(|r| r["action"] == action && r["outcome"] == outcome)
            .collect();
        assert!(
            !found.is_empty(),
            "missing {action}/{outcome}: {records:#?}"
        );
        for record in &found {
            assert_eq!(record["cause"], json!({"kind": kind, "id": id}));
        }
        found[0]
    }

    fn capture() -> (Captured, tracing::dispatcher::DefaultGuard) {
        let captured = Captured::default();
        let dispatch = log::dispatch("info", LogFormat::Json, captured.clone());
        let guard = tracing::dispatcher::set_default(&dispatch);
        (captured, guard)
    }

    /// Completion deletes the queue row; the worker span retains the claimed job's context.
    fn completed_jobs(captured: &Captured, trace: &str, kind: &str, id: &str) -> Vec<Value> {
        let lines = captured.json_lines();
        let jobs: Vec<_> = lines
            .iter()
            .filter(|line| {
                line["fields"]["message"] == "job completed" && line["span"]["trace_id"] == trace
            })
            .map(|line| line["span"].clone())
            .collect();
        assert!(!jobs.is_empty(), "{lines:#?}");
        for job in &jobs {
            assert_eq!(job["cause_kind"], kind, "{job}");
            assert_eq!(job["cause_id"], id, "{job}");
        }
        jobs
    }

    fn exit_graph() -> Value {
        json!({"v": 1, "nodes": [{"id": "end", "type": "exit"}], "edges": []})
    }

    fn settings(trigger: Value) -> Value {
        json!({"trigger": trigger, "reentry": {"mode": "once"}})
    }

    async fn email(world: &World) -> (String, String) {
        let (sender, _) = world
            .verified_sender("example.com", "news@example.com")
            .await;
        let template = world.template("Propagation", json!({
            "schema": 1, "channel": "email", "mode": "code", "email": {
                "subject": "Propagation", "sender": sender, "lang": "en", "category": "marketing",
                "html": "<p>Hello</p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
                "text": "Hello {{ system.unsubscribe_url }}"
            }
        })).await;
        (sender, template)
    }

    fn send_graph(template: &str, destination: &str) -> Value {
        json!({"v": 1, "nodes": [
            {"id": "send", "type": "action.send", "template": template, "destination": destination},
            {"id": "end", "type": "exit"}
        ], "edges": [{"id": "sent", "from": "send", "port": "next", "to": "end"}]})
    }

    async fn trait_flow(world: &World) {
        zirv_flow_store::create_trait_definition(
            &world.pool,
            world.ws,
            &NewTraitDefinition {
                key: "plan".into(),
                trait_type: TraitType::String,
                precedence: Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
        let segment = world
            .segment(
                PERSONAL_DATA.segment_name,
                json!({"v": 1, "trait": "plan", "op": "eq", "value": PERSONAL_DATA.property}),
            )
            .await;
        world
            .flow(
                "Trait entry",
                exit_graph(),
                settings(json!({
                    "type": "segment", "segment_id": segment, "on": "entered"
                })),
            )
            .await;
        world.settle().await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sync_run_keeps_its_cause_through_traits_segment_entry_and_enrollment(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            trait_flow(&world).await;
            let config = json!({"records": 1, "note": PERSONAL_DATA.property});
            let source = zirv_flow_store::create_source(
                &world.pool,
                world.ws,
                &NewSource {
                    id: Uuid::now_v7(),
                    kind: "fake",
                    name: "Sync propagation",
                    config: &config,
                    credential_id: None,
                    schedule: None,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            let context = SourceContext::new(
                Arc::default(),
                HttpLimits::default(),
                "fake",
                source.id.to_string(),
                Arc::default(),
            )
            .unwrap();
            let found = Registry::built_in()
                .get("fake")
                .unwrap()
                .discover(&context, &config, &Credential::new())
                .await
                .unwrap();
            let descriptor = serde_json::to_value(&found[0]).unwrap();
            let mut tx = zirv_flow_store::begin(&world.pool).await.unwrap();
            zirv_flow_store::upsert_discovered_streams(
                &mut tx,
                world.ws,
                source.id,
                &[DiscoveredStream {
                    name: "stream_1",
                    sync_mode: "full_refresh",
                    cursor_field: None,
                    primary_key: &["id".to_owned()],
                    descriptor: &descriptor,
                    suggested_mapping: None,
                }],
            )
            .await
            .unwrap()
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            let stream = zirv_flow_store::configure_stream(
                &mut world.pool.acquire().await.unwrap(),
                world.ws,
                source.id,
                "stream_1",
                &StreamSettings {
                    enabled: true,
                    sync_mode: "full_refresh",
                    cursor_field: None,
                    primary_key: &["id".to_owned()],
                    staleness_threshold_seconds: None,
                    reconcile_schedule: None,
                    sweep_limit: 0.5,
                },
            )
            .await
            .unwrap()
            .unwrap();
            zirv_flow_store::set_stream_mapping(
                &world.pool,
                world.ws,
                source.id,
                "stream_1",
                &json!({
                    "key_column": "id", "columns": [
                        {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                        {"column": "note", "target": {"type": "trait", "key": "plan"}}
                    ]
                }),
            )
            .await
            .unwrap()
            .unwrap();
            let run = zirv_flow_store::queue_run(
                &mut world.pool.acquire().await.unwrap(),
                world.ws,
                source.id,
                stream.id,
                RunKind::Sync,
                RunTrigger::Manual,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            world.settle().await;

            let trace = run.trace_id.simple().to_string();
            let records = records(&world, &trace).await;
            let id = run.id.to_string();
            rooted_tree(&records, &id);
            for (action, outcome) in [
                ("sync.record", "applied"),
                ("trait.change", "changed"),
                ("segment.entered", "entered"),
                ("flow.enroll", "enrolled"),
                ("flow.step", "done"),
            ] {
                cause_at(&records, action, outcome, "sync_run", &id);
            }
            let jobs = completed_jobs(&captured, &trace, "sync_run", &id);
            for kind in ["sync.run", "flow.enroll", "flow.step"] {
                assert!(jobs.iter().any(|job| job["kind"] == kind), "{jobs:#?}");
            }
            world.finish().await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_manual_audience_start_keeps_the_operator_cause_in_step_decisions(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            let person = world.subscribed_person(PERSONAL_DATA.email).await;
            let segment = world
                .segment(
                    PERSONAL_DATA.segment_name,
                    json!({"v": 1, "trait": "first_name", "op": "exists"}),
                )
                .await;
            let flow = world
                .flow(
                    "Manual entry",
                    exit_graph(),
                    settings(json!({"type": "manual"})),
                )
                .await;
            world.settle().await;
            let user = admin::create_user(
                &world.pool,
                &Context::system(),
                "operator@example.com",
                "correct horse battery",
            )
            .await
            .unwrap();
            admin::set_member(
                &world.pool,
                &Context::system(),
                world.ws,
                user.id,
                Role::Admin,
            )
            .await
            .unwrap();
            let login = call(
                &world.router,
                "POST",
                "/api/v1/session",
                Some(json!({"email": "operator@example.com", "password": "correct horse battery"})),
                None,
            )
            .await;
            assert_eq!(login.status, StatusCode::OK, "{}", login.text);
            let cookie = header_of(&login, "set-cookie")
                .split(';')
                .next()
                .unwrap()
                .to_owned();
            let response = world
                .router
                .clone()
                .oneshot(
                    Request::post(world.path(&format!("/flows/{flow}/start")))
                        .header(header::COOKIE, cookie)
                        .header("x-csrf-token", login.body["csrf_token"].as_str().unwrap())
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(json!({"segment_id": segment}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            world.settle().await;
            // The manual start mints its own action trace. The occurrence in the response
            // identifies that action independently of the request middleware's trace.
            let trace: Uuid = sqlx::query_scalar(
                "SELECT trace_id FROM enrollments WHERE flow_id = $1 AND occurrence_id = $2",
            )
            .bind(Uuid::parse_str(&flow).unwrap())
            .bind(Uuid::parse_str(body["occurrence_id"].as_str().unwrap()).unwrap())
            .fetch_one(&world.pool)
            .await
            .unwrap();
            let trace = trace.simple().to_string();
            let records = records(&world, &trace).await;
            let operator = user.id.as_uuid().to_string();
            rooted_tree(&records, &operator);
            let enrolled = cause_at(&records, "flow.enroll", "enrolled", "manual", &operator);
            assert_eq!(enrolled["subject_ids"]["person"], person.to_string());
            cause_at(&records, "flow.step", "done", "manual", &operator);
            cause_at(
                &records,
                "flow.enrollment",
                "completed",
                "manual",
                &operator,
            );
            let jobs = completed_jobs(&captured, &trace, "manual", &operator);
            assert!(jobs.iter().any(|job| job["kind"] == "flow.enroll"));
            assert!(jobs.iter().any(|job| job["kind"] == "flow.step"));
            world.finish().await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_slot_keeps_its_cause_through_enrollment_steps_and_send(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            let (_, template) = email(&world).await;
            let destination = world.smtp_destination().await;
            world.subscribed_person(PERSONAL_DATA.email).await;
            let flow = world.flow("Scheduled send", send_graph(&template, &destination), settings(json!({
                "type": "schedule", "cron": "* * * * *", "timezone": "UTC", "audience": {"type": "everyone"}
            }))).await;
            world.settle().await;
            let schedule: Uuid = sqlx::query_scalar(
                "UPDATE schedules SET next_run_at = now() - interval '1 minute' WHERE flow_id = $1 RETURNING id")
                .bind(Uuid::parse_str(&flow).unwrap()).fetch_one(&world.pool).await.unwrap();
            let mut scheduler = Scheduler::new(world.pool.clone(), SchedulerConfig::default());
            assert!(matches!(scheduler.tick().await.unwrap(), Tick::Led(_)));
            world.settle().await;
            let trace: Uuid = sqlx::query_scalar(
                "SELECT trace_id FROM enrollments WHERE flow_id = $1")
                .bind(Uuid::parse_str(&flow).unwrap()).fetch_one(&world.pool).await.unwrap();
            let trace = trace.simple().to_string();
            let records = records(&world, &trace).await;
            let id = schedule.to_string();
            rooted_tree(&records, &id);
            for (action, outcome) in [("flow.enroll", "enrolled"), ("flow.step", "enqueued"),
                ("message.send", "sent"), ("flow.enrollment", "completed")] {
                cause_at(&records, action, outcome, "schedule", &id);
            }
            let jobs = completed_jobs(&captured, &trace, "schedule", &id);
            for kind in ["flow.enroll", "flow.step", "delivery.send"] {
                assert!(jobs.iter().any(|job| job["kind"] == kind));
            }
            assert_eq!(world.sink.messages().len(), 1);
            drop(scheduler);
            world.finish().await;
        }).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_crosses_the_outbox_to_a_forwarding_decision_on_its_own_trace(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            world.subscribed_person(PERSONAL_DATA.email).await;
            world.event_schema("order_placed").await;
            let destination = zirv_flow_store::create_destination(&world.pool, world.ws, &NewDestination {
                id: Uuid::now_v7(), kind: "fake", name: "Forwarding", config: &json!({"mode": "rejected"}),
                credential_id: None, rate: 100, burst: 100, created_by: None,
            }).await.unwrap();
            assert!(matches!(zirv_flow_store::create_rule(&mut world.pool.acquire().await.unwrap(), world.ws,
                &NewRule { id: Uuid::now_v7(), destination_id: destination.id, name: "Orders", event_name: "order_placed",
                    filter: &json!([]), mapping: &json!({"note": {"from": "event.properties", "pointer": "/private_note"}}),
                    active: true, created_by: None }).await.unwrap(), RuleWrite::Saved(_)));
            let reply = call(&world.router, "POST", "/api/v1/ingest/track", Some(json!({
                "identifiers": {"email": PERSONAL_DATA.email}, "event": "order_placed",
                "messageId": "forward-propagation", "properties": PERSONAL_DATA.properties()
            })), Some(&world.ingest)).await;
            assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text);
            world.settle().await;
            let trace = header_of(&reply, "x-trace-id");
            let (event, outbox, held, state): (Uuid, Uuid, Option<Uuid>, String) = sqlx::query_as(
                "SELECT event_id, id, trace_id, state FROM forwarding_outbox")
                .fetch_one(&world.pool).await.unwrap();
            assert_eq!(held, Some(Uuid::parse_str(&trace).unwrap()));
            assert_eq!(state, "dead");
            let records = records(&world, &trace).await;
            assert_eq!(one_tree(&records), Ok(()), "{records:#?}");
            cause_at(&records, "event.accepted", "valid", "api", &header_of(&reply, "x-request-id"));
            let forwarded = cause_at(&records, "event.forward", "dead", "event", &event.to_string());
            assert_eq!(forwarded["subject_ids"]["outbox"], outbox.to_string());
            let lines = captured.json_lines();
            let job = lines.iter().find(|line| line["fields"]["message"] == "job completed"
                && line["span"]["job_id"] == forwarded["job_id"]).unwrap();
            assert_ne!(job["span"]["trace_id"], trace);
            assert_eq!(job["span"]["cause_kind"], "schedule");
            assert_eq!(job["span"]["cause_id"], destination.id.to_string());
            world.finish().await;
        }).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn feedback_keeps_the_messages_trace_and_the_received_request_id(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            let (_, template) = email(&world).await;
            let destination = world.provider_destination().await;
            world.subscribed_person(PERSONAL_DATA.email).await;
            world.event_schema("order_placed").await;
            world.flow("Feedback", send_graph(&template, &destination), settings(json!({
                "type": "event", "event": "order_placed"
            }))).await;
            let trace = world.track_order(PERSONAL_DATA.email).await;
            world.settle().await;
            let message: Uuid = sqlx::query_scalar("SELECT id FROM messages WHERE status = 'sent'")
                .fetch_one(&world.pool).await.unwrap();
            let event: Uuid = sqlx::query_scalar("SELECT id FROM events WHERE name = 'order_placed'")
                .fetch_one(&world.pool).await.unwrap();
            let (_, verification) = provider_feedback_of(&world.provider_config()).unwrap();
            let body = json!({"event": "delivered", "id": "feedback-propagation", "meta": {"message_id": message}})
                .to_string().into_bytes();
            let signed = sign_provider_feedback(&provider_feedback_scheme(&verification), PROVIDER_SECRET,
                world.clock.now().unix_timestamp(), &body);
            let mut request = Request::post(format!("/api/v1/destinations/{destination}/feedback"))
                .header("x-request-id", "received");
            for (name, value) in signed { request = request.header(name, value); }
            let response = world.router.clone().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(response.headers()["x-request-id"], "received");
            let received_trace = response.headers()["x-trace-id"].to_str().unwrap().to_owned();
            assert_ne!(received_trace, trace);
            world.settle().await;
            let records = records(&world, &trace).await;
            assert_eq!(one_tree(&records), Ok(()), "{records:#?}");
            for (action, outcome) in [("flow.enroll", "enrolled"), ("message.send", "sent"), ("message.feedback", "recorded")] {
                cause_at(&records, action, outcome, "event", &event.to_string());
            }
            let feedback = cause_at(&records, "message.feedback", "recorded", "event", &event.to_string());
            assert_eq!(feedback["subject_ids"]["message"], message.to_string());
            assert_eq!(world.api("GET", &format!("/traces/{received_trace}"), None).await.status, StatusCode::NOT_FOUND);
            let lines = captured.json_lines();
            assert!(lines.iter().any(|line| line["fields"]["message"] == "request completed"
                && line["span"]["request_id"] == "received" && line["span"]["trace_id"] == received_trace), "{lines:#?}");
            world.finish().await;
        }).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_test_send_retry_keeps_the_request_trace_in_its_attempt_decision(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            let (sender, template) = email(&world).await;
            let destination = world.provider_destination().await;
            world.api_ok("POST", "/test-send-allowlist", Some(json!({"address": PERSONAL_DATA.email})), StatusCode::CREATED).await;
            world.provider.script(FakeProviderScript::Status { status: 503, body: json!({}), retry_after: Some(60) });
            let reply = world.api("POST", &format!("/templates/{template}/test-send"), Some(json!({
                "destination_id": destination, "sender_identity_id": sender, "to": {"address": PERSONAL_DATA.email}
            }))).await;
            assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text);
            world.settle().await;
            let trace = header_of(&reply, "x-trace-id");
            let request = header_of(&reply, "x-request-id");
            let records = records(&world, &trace).await;
            rooted_tree(&records, &request);
            let failed = cause_at(&records, "job.attempt", "failed", "api", &request);
            assert_eq!(failed["attempt"], 1);
            sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'template.test_send' AND state = 'ready'")
                .execute(&world.pool).await.unwrap();
            world.settle().await;
            let jobs = completed_jobs(&captured, &trace, "api", &request);
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0]["job_id"], failed["job_id"]);
            assert_eq!(jobs[0]["attempt"], 2);
            assert_eq!(world.provider.accepted().len(), 1);
            assert_eq!(world.count("SELECT count(*) FROM test_sends WHERE outcome = 'sent'").await, 1);
            assert_eq!(world.count("SELECT count(*) FROM messages").await, 0);
            world.finish().await;
        }).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn privacy_erase_keeps_the_request_trace_after_removing_the_person(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            let person = world.subscribed_person(PERSONAL_DATA.email).await;
            let reply = world
                .api(
                    "POST",
                    "/privacy/erasures",
                    Some(json!({"person_id": person})),
                )
                .await;
            assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text);
            world.settle().await;
            let trace = header_of(&reply, "x-trace-id");
            let request = header_of(&reply, "x-request-id");
            let records = records(&world, &trace).await;
            rooted_tree(&records, &request);
            let erased = cause_at(&records, "person.deleted", "deleted", "api", &request);
            assert_eq!(erased["reason"], "erasure");
            assert_eq!(erased["subject_ids"], json!({}));
            let jobs = completed_jobs(&captured, &trace, "api", &request);
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0]["kind"], "privacy.erase");
            let (held, status): (Uuid, String) =
                sqlx::query_as("SELECT trace_id, status FROM erasure_requests")
                    .fetch_one(&world.pool)
                    .await
                    .unwrap();
            assert_eq!(held, Uuid::parse_str(&trace).unwrap());
            assert_eq!(status, "completed");
            assert_eq!(world.count("SELECT count(*) FROM people").await, 0);
            world.finish().await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn coalesced_marks_keep_old_decisions_but_attach_entry_to_the_latest_trace(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let world = World::start(pool, PUBLIC_URL).await;
            trait_flow(&world).await;
            let older = TraceContext::root(Cause::api("older-mark"));
            let later = TraceContext::root(Cause::api("later-mark"));
            let record = |trace: &TraceContext, plan: &str, observed_at| ProfileRecord {
                source: "coalescing".into(),
                stream: "people".into(),
                record_key: PERSONAL_DATA.external_id.into(),
                identifiers: PERSONAL_DATA.identifiers(),
                traits: json!({"plan": plan, "first_name": PERSONAL_DATA.first_name})
                    .as_object()
                    .unwrap()
                    .clone(),
                observed_at,
                trace: trace.clone(),
            };
            // Both writes commit together: reevaluation cannot claim the first mark before the
            // second overwrites its trace. Earlier decisions stay; the dirty set has one winner.
            zirv_flow_store::apply_profile_batch(
                &world.pool,
                world.ws,
                &[
                    record(&older, "before", world.clock.now()),
                    record(
                        &later,
                        PERSONAL_DATA.property,
                        world.clock.now() + time::Duration::seconds(1),
                    ),
                ],
            )
            .await
            .unwrap();
            world.settle().await;
            let old_records = records(&world, &older.trace_id.to_string()).await;
            rooted_tree(&old_records, &older.cause.id);
            cause_at(
                &old_records,
                "trait.change",
                "changed",
                "api",
                &older.cause.id,
            );
            assert!(
                old_records
                    .iter()
                    .all(|r| r["action"] != "segment.entered" && r["action"] != "flow.enroll")
            );
            let new_records = records(&world, &later.trace_id.to_string()).await;
            rooted_tree(&new_records, &later.cause.id);
            for (action, outcome) in [
                ("trait.change", "changed"),
                ("segment.entered", "entered"),
                ("flow.enroll", "enrolled"),
            ] {
                cause_at(&new_records, action, outcome, "api", &later.cause.id);
            }
            let traces: Vec<Uuid> = sqlx::query_scalar(
                "SELECT trace_id FROM decision_records WHERE action = 'segment.entered'",
            )
            .fetch_all(&world.pool)
            .await
            .unwrap();
            assert_eq!(traces, [later.trace_id.as_uuid()]);
            world.finish().await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_request_id_reaches_child_jobs_their_spans_and_the_step_decision(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let (captured, _logging) = capture();
            let world = World::start(pool, PUBLIC_URL).await;
            let person = world.subscribed_person(PERSONAL_DATA.email).await;
            let flow = world
                .flow("API entry", exit_graph(), settings(json!({"type": "api"})))
                .await;
            let response = world
                .router
                .clone()
                .oneshot(
                    Request::post(world.path(&format!("/flows/{flow}/enrollments")))
                        .header(header::AUTHORIZATION, format!("Bearer {}", world.admin))
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("x-request-id", "request-job-child")
                        .body(Body::from(json!({"person_id": person}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let trace = response.headers()["x-trace-id"]
                .to_str()
                .unwrap()
                .to_owned();
            assert_eq!(response.headers()["x-request-id"], "request-job-child");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(status, StatusCode::ACCEPTED, "{body:?}");
            world.settle().await;
            let records = records(&world, &trace).await;
            rooted_tree(&records, "request-job-child");
            cause_at(
                &records,
                "flow.enroll",
                "enrolled",
                "api",
                "request-job-child",
            );
            let step = cause_at(&records, "flow.step", "done", "api", "request-job-child");
            let jobs = completed_jobs(&captured, &trace, "api", "request-job-child");
            let lines = captured.json_lines();
            for kind in ["flow.enroll", "flow.step"] {
                let job = jobs.iter().find(|job| job["kind"] == kind).unwrap();
                if kind == "flow.step" {
                    assert_eq!(step["job_id"], job["job_id"]);
                }
                assert!(
                    lines
                        .iter()
                        .any(|line| line["fields"]["message"] == "job completed"
                            && line["span"]["kind"] == kind
                            && line["span"]["job_id"] == job["job_id"]
                            && line["span"]["trace_id"] == trace
                            && line["span"]["cause_kind"] == "api"
                            && line["span"]["cause_id"] == "request-job-child"),
                    "{lines:#?}"
                );
            }
            assert!(
                lines
                    .iter()
                    .any(|line| line["fields"]["message"] == "request completed"
                        && line["span"]["trace_id"] == trace
                        && line["span"]["request_id"] == "request-job-child")
            );
            world.finish().await;
        })
        .await;
    }
}
