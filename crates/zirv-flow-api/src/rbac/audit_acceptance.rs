//! The audit acceptance test (architecture 16.2, exit criterion 2, audit part): every route of the
//! table called as an allowed principal over the seeded workspace of the RBAC walk.
//!
//! For each `Audited` row the call leaves exactly one `audit_log` row of that action, with the
//! actor kind of the caller, the request id the caller sent and the workspace; for each
//! `NotAudited` row it leaves none. The summaries of every row written are then held to the
//! redaction rules, and no seeded personal value appears in any of them.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::rbac::acceptance::tests::{
        Auth, Operations, PASSWORD, PERSONAL, Seeded, Who, World, found, rendering_world, seed,
        sign_in,
    };
    use crate::rbac::{Audit, ROUTES};

    /// What one call left behind.
    /// The `secret.*` rows a route that seals or drops a secret writes beside its own row. Any
    /// other route writing one is a defect.
    fn secret_companion(route_action: &str, written: &str) -> bool {
        let allowed: &[&str] = match route_action {
            "credential.created"
            | "webhook.created"
            | "sending_domain.created"
            | "webhook_signing.created" => &["secret.created"],
            "credential.rotated" | "webhook.secret_rotated" | "webhook_signing.rotated" => {
                &["secret.created", "secret.deleted"]
            }
            "webhook.deleted"
            | "sending_domain.deleted"
            | "source.deleted"
            | "source.oauth_disconnected" => &["secret.deleted"],
            "source.oauth_connected" | "ai.settings.updated" => {
                &["secret.created", "secret.replaced"]
            }
            _ => &[],
        };
        allowed.contains(&written)
    }

    struct Reply {
        status: u16,
        body: Value,
        text: String,
    }

    /// One audit row, as the walk reads it.
    #[derive(Debug, sqlx::FromRow)]
    struct Row {
        action: String,
        actor_kind: String,
        workspace_id: Option<Uuid>,
        object_id: Option<Uuid>,
        request_id: Option<String>,
    }

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<(String, Vec<u8>)>,
        auth: Auth<'_>,
        request_id: &str,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-request-id", request_id);
        request = match auth {
            Auth::Anonymous => request,
            Auth::Key(key) => request.header(header::AUTHORIZATION, format!("Bearer {key}")),
            Auth::Session(login) => request
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", &login.csrf),
        };
        let body = match body {
            Some((content_type, bytes)) => {
                request = request.header(header::CONTENT_TYPE, content_type);
                Body::from(bytes)
            }
            None => Body::empty(),
        };
        let response = router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let streaming = response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|kind| kind == "text/event-stream");
        if streaming {
            return Reply {
                status,
                body: Value::Null,
                text: String::new(),
            };
        }
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            response.into_body().collect(),
        )
        .await
        .unwrap_or_else(|_| panic!("{method} {uri} did not finish"))
        .unwrap()
        .to_bytes();
        Reply {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn rows_after(pool: &PgPool, after: i64) -> Vec<Row> {
        sqlx::query_as(
            "SELECT action, actor_kind, workspace_id, object_id, request_id \n             FROM audit_log WHERE id > $1 ORDER BY id",
        )
        .bind(after)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn last_id(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COALESCE(max(id), 0) FROM audit_log")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The route key a step covers.
    fn key(method: &str, template: &str) -> String {
        format!("{} {template}", method.to_lowercase())
    }

    /// What a call sends.
    enum Payload {
        Json(Value),
        Raw(&'static str, Vec<u8>),
    }

    fn j(value: Value) -> Option<Payload> {
        Some(Payload::Json(value))
    }

    fn csv(text: &str) -> Option<Payload> {
        Some(Payload::Raw("text/csv", text.as_bytes().to_vec()))
    }

    const WS: &str = "/api/v1/workspaces/{ws}";
    const SENDER: &str = "0192b1a4-7c3e-7000-8000-00000000a001";

    /// The walk: its world, what it covered and what went wrong.
    struct Walk<'a> {
        world: &'a World,
        operations: Operations,
        calls: u32,
        covered: BTreeSet<String>,
        failures: Vec<String>,
    }

    fn u(id: Uuid) -> String {
        id.to_string()
    }

    fn id_of(body: &Value) -> Uuid {
        body["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .unwrap_or_else(Uuid::nil)
    }

    impl<'a> Walk<'a> {
        fn new(world: &'a World) -> Self {
            Self {
                world,
                operations: Operations::new(),
                calls: 0,
                covered: BTreeSet::new(),
                failures: Vec::new(),
            }
        }

        fn fail(&mut self, what: String) {
            self.failures.push(what);
        }

        fn owner(&self) -> Auth<'a> {
            self.world.auth(Who::Owner)
        }

        /// The concrete uri of `template`: the workspace, then `ids` for the other placeholders.
        fn uri(&self, template: &str, ids: &[String]) -> String {
            let mut out = String::new();
            let mut rest = template;
            let mut next = ids.iter();
            while let Some(start) = rest.find('{') {
                out.push_str(&rest[..start]);
                let end = rest[start..].find('}').unwrap() + start;
                if &rest[start + 1..end] == "ws" {
                    out.push_str(&self.world.ws.as_uuid().to_string());
                } else {
                    out.push_str(next.next().map_or("ghost", String::as_str));
                }
                rest = &rest[end + 1..];
            }
            out.push_str(rest);
            out
        }

        /// The table row of a call, by method and template.
        fn row(method: &str, template: &str) -> Option<&'static crate::rbac::Route> {
            let wanted = key(method, template);
            ROUTES
                .iter()
                .find(|row| wanted == format!("{} {}", row.method.name(), row.path))
        }

        /// Calls the route as `auth` and returns the reply and the audit rows it left.
        async fn call(
            &mut self,
            auth: Auth<'_>,
            method: &str,
            template: &str,
            ids: &[String],
            body: Option<Payload>,
        ) -> (Reply, Vec<Row>, String) {
            self.calls += 1;
            let request_id = format!("audit-walk-{}", self.calls);
            let (sampled, query) = Self::row(method, template)
                .map_or((None, String::new()), |row| self.operations.request(row));
            let body = match body {
                Some(Payload::Json(body)) => {
                    Some(("application/json".to_owned(), body.to_string().into_bytes()))
                }
                Some(Payload::Raw(kind, bytes)) => Some((kind.to_owned(), bytes)),
                None => sampled,
            };
            let uri = format!("{}{query}", self.uri(template, ids));
            let before = last_id(&self.world.pool).await;
            let reply = send(
                &self.world.router,
                &method.to_uppercase(),
                &uri,
                body,
                auth,
                &request_id,
            )
            .await;
            let rows = rows_after(&self.world.pool, before).await;
            (reply, rows, request_id)
        }

        /// Calls an `Audited` route and holds the result to the audit rules: one row of the
        /// row's action, the caller's actor kind, the request id and the object. Returns the body.
        async fn audited(
            &mut self,
            auth: Auth<'_>,
            method: &str,
            template: &str,
            ids: &[String],
            body: Option<Payload>,
            object: Option<Uuid>,
        ) -> Value {
            let name = key(method, template);
            let actor = match auth {
                Auth::Key(_) => "api_key",
                Auth::Anonymous | Auth::Session(_) => "user",
            };
            let Some(Audit::Audited(action)) =
                Self::row(method, template).and_then(|row| row.audit)
            else {
                self.fail(format!("{name}: not an Audited row"));
                return Value::Null;
            };
            let (reply, rows, request_id) = self.call(auth, method, template, ids, body).await;
            if !(200..400).contains(&reply.status) {
                self.fail(format!("{name}: status {} {}", reply.status, reply.text));
                return reply.body;
            }
            let ours: Vec<&Row> = rows.iter().filter(|row| row.action == action).collect();
            if ours.len() != 1 {
                let actions: Vec<&str> = rows.iter().map(|row| row.action.as_str()).collect();
                self.fail(format!(
                    "{name}: {} rows of {action}, wrote {actions:?}",
                    ours.len()
                ));
                return reply.body;
            }
            let row = ours[0];
            if row.actor_kind != actor {
                self.fail(format!("{name}: actor {} not {actor}", row.actor_kind));
            }
            if row.request_id.as_deref() != Some(request_id.as_str()) {
                self.fail(format!("{name}: request id {:?}", row.request_id));
            }
            if template.contains("{ws}") && row.workspace_id != Some(self.world.ws.as_uuid()) {
                self.fail(format!("{name}: workspace {:?}", row.workspace_id));
            }
            if let Some(object) = object
                && row.object_id != Some(object)
            {
                self.fail(format!("{name}: object {:?} not {object}", row.object_id));
            }
            for other in rows.iter().filter(|row| row.action != action) {
                let companion = secret_companion(action, &other.action)
                    || (matches!(action, "credential.tested" | "forwarding.rule_created")
                        && other.action == "destination.checked");
                if !companion {
                    self.fail(format!("{name}: also wrote {}", other.action));
                }
            }
            self.covered.insert(name);
            reply.body
        }

        /// An `Audited` workspace route, called by the instance owner (a user).
        async fn a(
            &mut self,
            method: &str,
            tail: &str,
            ids: &[String],
            body: Option<Payload>,
            object: Option<Uuid>,
        ) -> Value {
            let owner = self.owner();
            self.audited(owner, method, &format!("{WS}{tail}"), ids, body, object)
                .await
        }

        /// Calls a `NotAudited` route and checks it wrote no audit row, whatever it answered.
        async fn silent(
            &mut self,
            auth: Auth<'_>,
            method: &str,
            template: &str,
            ids: &[String],
            body: Option<Payload>,
        ) {
            let name = key(method, template);
            if !Self::row(method, template)
                .is_some_and(|row| matches!(row.audit, Some(Audit::NotAudited(_))))
            {
                self.fail(format!("{name}: not a NotAudited row"));
                return;
            }
            let (reply, rows, _) = self.call(auth, method, template, ids, body).await;
            if reply.status >= 500 {
                self.fail(format!("{name}: status {} {}", reply.status, reply.text));
            }
            if !rows.is_empty() {
                let actions: Vec<&str> = rows.iter().map(|row| row.action.as_str()).collect();
                self.fail(format!("{name}: wrote {actions:?}"));
            }
            self.covered.insert(name);
        }

        /// A `NotAudited` workspace route, called by the instance owner.
        async fn s(&mut self, method: &str, tail: &str, ids: &[String], body: Option<Payload>) {
            let owner = self.owner();
            self.silent(owner, method, &format!("{WS}{tail}"), ids, body)
                .await;
        }

        /// A read of the workspace, for the value a later step needs.
        async fn get(&mut self, tail: &str) -> Value {
            let owner = self.owner();
            let uri = self.uri(&format!("{WS}{tail}"), &[]);
            self.calls += 1;
            send(
                &self.world.router,
                "GET",
                &uri,
                None,
                owner,
                &format!("audit-walk-{}", self.calls),
            )
            .await
            .body
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_audited_route_leaves_one_row_and_every_not_audited_route_none(pool: PgPool) {
        let world = rendering_world(&pool).await;
        let seeded = seed(&world).await;
        let mut walk = Walk::new(&world);

        instance(&mut walk).await;
        people(&mut walk, &seeded).await;
        flows(&mut walk, &seeded).await;
        content(&mut walk, &seeded).await;
        events_and_consent(&mut walk, &seeded).await;
        integrations(&mut walk, &seeded).await;
        sending(&mut walk).await;
        lists_privacy_imports(&mut walk, &seeded).await;
        public(&mut walk).await;
        keys(&mut walk).await;

        let wanted: BTreeSet<String> = ROUTES
            .iter()
            .filter(|row| row.audit.is_some())
            .map(|row| format!("{} {}", row.method.name(), row.path))
            .collect();
        let elsewhere: BTreeSet<String> = ELSEWHERE
            .iter()
            .map(|(route, _)| (*route).to_owned())
            .collect();
        let missed: Vec<&String> = wanted
            .iter()
            .filter(|route| !walk.covered.contains(*route) && !elsewhere.contains(*route))
            .collect();
        let stale: Vec<&String> = elsewhere
            .iter()
            .filter(|route| !wanted.contains(*route) || walk.covered.contains(*route))
            .collect();
        assert!(walk.failures.is_empty(), "{}", walk.failures.join("\n"));
        assert!(missed.is_empty(), "routes never called:\n{missed:#?}");
        assert!(stale.is_empty(), "{stale:?}");

        crate::test_support::assert_audit_log_holds_ids_only(&pool).await;
        let log: String = sqlx::query_scalar(
            "SELECT COALESCE(string_agg(summary::text || ' ' || COALESCE(request_id, ''), ' '), '') \
             FROM audit_log",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            found(&log),
            Vec::<&str>::new(),
            "seeded personal values reached the audit log"
        );
        assert!(PERSONAL.len() >= 8);
    }

    /// Audited routes whose fixture needs a live provider, a finished job or a configured
    /// deployment, each with the test that calls it and asserts its audit row.
    const ELSEWHERE: &[(&str, &str)] = &[
        (
            "post /api/v1/setup",
            "acceptance::tests::the_right_token_creates_the_owner_signs_them_in_clears_the_token_and_audits",
        ),
        (
            "post /api/v1/workspaces/{ws}/ai/drafts/{id}/accept",
            "assistant::acceptance::tests::accepting_returns_the_draft_and_publishes_nothing",
        ),
        (
            "post /api/v1/workspaces/{ws}/sources/{id}/oauth/start",
            "sources::http_acceptance::tests::start_returns_the_authorization_url_and_keeps_only_the_hash_of_the_state",
        ),
    ];

    #[test]
    fn every_route_called_elsewhere_has_that_test_and_it_names_its_action() {
        let sources = [
            ("acceptance", include_str!("../acceptance.rs")),
            (
                "assistant::acceptance",
                include_str!("../assistant/acceptance.rs"),
            ),
            (
                "sources::http_acceptance",
                include_str!("../sources/http_acceptance.rs"),
            ),
        ];
        for (route, test) in ELSEWHERE {
            let (module, name) = test.rsplit_once("::tests::").unwrap();
            let source = sources
                .iter()
                .find(|(path, _)| *path == module)
                .map(|(_, source)| *source)
                .unwrap_or_else(|| panic!("{test}: no source for {module}"));
            assert!(source.contains(&format!("fn {name}(")), "{test} is gone");
            let action = match ROUTES
                .iter()
                .find(|row| *route == format!("{} {}", row.method.name(), row.path))
                .and_then(|row| row.audit)
            {
                Some(Audit::Audited(action)) => action,
                _ => panic!("{route} is not an Audited row"),
            };
            let tail = source.split(&format!("fn {name}(")).nth(1).unwrap();
            let body = tail
                .split(
                    "
    #[sqlx::test]",
                )
                .next()
                .unwrap();
            assert!(
                body.contains(action) || body.contains("list_audit"),
                "{test} does not look at {action}"
            );
        }
    }

    async fn instance(w: &mut Walk<'_>) {
        let world = w.world;
        w.audited(
            Auth::Anonymous,
            "POST",
            "/api/v1/session",
            &[],
            j(json!({"email": "owner@example.com", "password": PASSWORD})),
            None,
        )
        .await;
        let login = sign_in(&world.router, "owner@example.com").await;
        w.audited(
            Auth::Session(&login),
            "DELETE",
            "/api/v1/session",
            &[],
            None,
            None,
        )
        .await;
        let owner = w.owner();
        let user = w
            .audited(
                owner,
                "POST",
                "/api/v1/users",
                &[],
                j(json!({"email": "walk.user@example.com", "password": PASSWORD})),
                None,
            )
            .await;
        let user_id = id_of(&user);
        let owner = w.owner();
        w.audited(
            owner,
            "POST",
            "/api/v1/workspaces",
            &[],
            j(json!({"name": "Walk"})),
            None,
        )
        .await;
        let job = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, state, attempts, max_attempts, last_error, \
                trace_id, cause_kind, cause_id, dead_at) \
             VALUES ($1, 'walk.kind', '{}', 'dead', 5, 5, 'boom', $2, 'schedule', 's', now())",
        )
        .bind(job)
        .bind(Uuid::new_v4())
        .execute(&world.pool)
        .await
        .unwrap();
        let owner = w.owner();
        w.audited(
            owner,
            "POST",
            "/api/v1/admin/jobs/{id}/retry",
            &[u(job)],
            None,
            Some(job),
        )
        .await;

        w.a(
            "PATCH",
            "",
            &[],
            j(json!({"timezone": "Europe/Copenhagen"})),
            Some(world.ws.as_uuid()),
        )
        .await;
        w.a(
            "PUT",
            "/members/{user_id}",
            &[u(user_id)],
            j(json!({"role": "viewer"})),
            Some(user_id),
        )
        .await;
        w.a(
            "DELETE",
            "/members/{user_id}",
            &[u(user_id)],
            None,
            Some(user_id),
        )
        .await;
        let key = w
            .a(
                "POST",
                "/api-keys",
                &[],
                j(json!({"name": "walk", "scopes": ["read"]})),
                None,
            )
            .await;
        let key_id = id_of(&key);
        w.a("DELETE", "/api-keys/{id}", &[u(key_id)], None, Some(key_id))
            .await;
        w.a(
            "PUT",
            "/ai/settings",
            &[],
            j(json!({"enabled": false, "monthly_budget_micros": 1000, "pii_opt_in": false})),
            None,
        )
        .await;
        for (tail, ids) in [
            ("/ai/drafts", vec![]),
            ("/segments/{id}/ai/explain", vec![u(Uuid::new_v4())]),
            ("/flows/{id}/ai/explain", vec![u(Uuid::new_v4())]),
            ("/traces/{trace_id}/ai/explain", vec!["0".repeat(32)]),
        ] {
            w.s(
                "POST",
                tail,
                &ids,
                j(json!({"kind": "segment", "prompt": "who"})),
            )
            .await;
        }
    }

    async fn people(w: &mut Walk<'_>, s: &Seeded) {
        w.a(
            "POST",
            "/traits",
            &[],
            j(json!({"key": "walk_trait", "type": "string"})),
            None,
        )
        .await;
        w.a(
            "PATCH",
            "/traits/{key}",
            &["walk_trait".to_owned()],
            j(json!({"description": "walk"})),
            None,
        )
        .await;
        w.a(
            "POST",
            "/traits/{key}/index",
            &["walk_trait".to_owned()],
            None,
            None,
        )
        .await;
        w.a(
            "DELETE",
            "/traits/{key}/index",
            &["walk_trait".to_owned()],
            None,
            None,
        )
        .await;
        w.a(
            "POST",
            "/identity-conflicts/{id}/resolve",
            &[u(s.conflict)],
            j(json!({"outcome": "keep_separate"})),
            Some(s.conflict),
        )
        .await;
        w.s("POST", "/people/search", &[], None).await;
    }

    fn graph() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.duration", "seconds": 60},
            {"id": "end", "type": "exit"}
        ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]})
    }

    async fn failed_enrollment(w: &Walk<'_>, s: &Seeded, person: Uuid) -> Uuid {
        let enrollment = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id, error, failed_node, failure_placed) \
             VALUES ($1, $2, $3, 1, $4, 'failed', 'wait', $1, $1, 'api', 't', 'send_failed', \
                     'wait', true)",
        )
        .bind(enrollment)
        .bind(w.world.ws.as_uuid())
        .bind(s.flow)
        .bind(person)
        .execute(&w.world.pool)
        .await
        .unwrap();
        enrollment
    }

    async fn flows(w: &mut Walk<'_>, s: &Seeded) {
        let flow = w
            .a(
                "POST",
                "/flows",
                &[],
                j(json!({"name": "Walk flow", "graph": graph(),
                        "settings": {"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}})),
                None,
            )
            .await;
        let id = id_of(&flow);
        w.a(
            "PATCH",
            "/flows/{id}",
            &[u(id)],
            j(json!({"name": "Walk flow 2"})),
            Some(id),
        )
        .await;
        w.s("POST", "/flows/{id}/validate", &[u(id)], None).await;
        w.a(
            "POST",
            "/flows/{id}/test",
            &[u(id)],
            j(json!({"snapshot": {}})),
            Some(id),
        )
        .await;
        w.a("POST", "/flows/{id}/publish", &[u(id)], None, Some(id))
            .await;
        w.a(
            "PATCH",
            "/flows/{id}",
            &[u(id)],
            j(json!({"name": "Walk flow 3"})),
            Some(id),
        )
        .await;
        w.a("POST", "/flows/{id}/publish", &[u(id)], None, Some(id))
            .await;
        w.a("POST", "/flows/{id}/pause", &[u(id)], None, Some(id))
            .await;
        w.a("POST", "/flows/{id}/resume", &[u(id)], None, Some(id))
            .await;
        w.a(
            "PUT",
            "/flows/{id}/warmup",
            &[u(id)],
            j(json!({"enabled": true, "seconds": 600})),
            Some(id),
        )
        .await;
        w.a("POST", "/flows/{id}/warmup/open", &[u(id)], None, Some(id))
            .await;
        w.a(
            "POST",
            "/flows/{id}/start",
            &[u(s.flow)],
            j(json!({"list_id": s.list})),
            Some(s.flow),
        )
        .await;
        w.s(
            "POST",
            "/flows/{id}/enrollments",
            &[u(id)],
            j(json!({"person_id": s.person})),
        )
        .await;
        let retrier = throwaway_person(w, "walk-retry").await;
        let retry = failed_enrollment(w, s, retrier).await;
        w.a(
            "POST",
            "/flows/{id}/enrollments/{enrollment_id}/retry",
            &[u(s.flow), u(retry)],
            None,
            None,
        )
        .await;
        let skipper = throwaway_person(w, "walk-skip").await;
        let skip = failed_enrollment(w, s, skipper).await;
        w.a(
            "POST",
            "/flows/{id}/enrollments/{enrollment_id}/skip",
            &[u(s.flow), u(skip)],
            None,
            None,
        )
        .await;
        w.a(
            "POST",
            "/flows/{id}/versions/{n}/exit-enrollments",
            &[u(id), "1".to_owned()],
            None,
            Some(id),
        )
        .await;
        w.a("POST", "/flows/{id}/archive", &[u(id)], None, Some(id))
            .await;
        w.a("POST", "/webhook-signing-secret", &[], None, None)
            .await;
        w.a("POST", "/webhook-signing-secret/rotate", &[], None, None)
            .await;

        let segment = w
            .a(
                "POST",
                "/segments",
                &[],
                j(json!({"name": "Walk segment",
                        "definition": {"v": 1, "trait": "plan", "op": "eq", "value": "pro"}})),
                None,
            )
            .await;
        let segment = id_of(&segment);
        w.a(
            "PATCH",
            "/segments/{id}",
            &[u(segment)],
            j(json!({"description": "walk"})),
            Some(segment),
        )
        .await;
        w.a(
            "POST",
            "/segments/{id}/refresh",
            &[u(segment)],
            None,
            Some(segment),
        )
        .await;
        w.a(
            "POST",
            "/segments/{id}/archive",
            &[u(segment)],
            None,
            Some(segment),
        )
        .await;
        w.s(
            "POST",
            "/segments/preview",
            &[],
            j(json!({"definition": {"v": 1, "trait": "plan", "op": "eq", "value": "pro"}})),
        )
        .await;
    }

    async fn content(w: &mut Walk<'_>, s: &Seeded) {
        let snippet = id_of(
            &w.a(
                "POST",
                "/snippets",
                &[],
                j(json!({"name": "Walk snippet"})),
                None,
            )
            .await,
        );
        w.a(
            "PUT",
            "/snippets/{id}",
            &[u(snippet)],
            j(json!({"name": "Walk snippet 2"})),
            Some(snippet),
        )
        .await;
        w.a(
            "PUT",
            "/snippets/{id}/draft",
            &[u(snippet)],
            j(json!({"content": {"blocks": [{"type": "heading", "id": "h", "level": 2, "text": "Hi"}]}})),
            Some(snippet),
        )
        .await;
        w.s(
            "POST",
            "/snippets/{id}/preview",
            &[u(snippet)],
            j(json!({})),
        )
        .await;
        w.a(
            "POST",
            "/snippets/{id}/publish",
            &[u(snippet)],
            None,
            Some(snippet),
        )
        .await;
        w.a(
            "DELETE",
            "/snippets/{id}",
            &[u(snippet)],
            None,
            Some(snippet),
        )
        .await;

        let theme = id_of(
            &w.a(
                "POST",
                "/themes",
                &[],
                j(json!({"name": "Walk theme"})),
                None,
            )
            .await,
        );
        w.a(
            "PUT",
            "/themes/{id}",
            &[u(theme)],
            j(json!({"name": "Walk theme 2"})),
            Some(theme),
        )
        .await;
        w.a(
            "PUT",
            "/themes/{id}/draft",
            &[u(theme)],
            j(json!({"tokens": {"primary": "#112233"}})),
            Some(theme),
        )
        .await;
        w.s("POST", "/themes/{id}/preview", &[u(theme)], j(json!({})))
            .await;
        w.a(
            "POST",
            "/themes/{id}/publish",
            &[u(theme)],
            None,
            Some(theme),
        )
        .await;
        w.a(
            "PUT",
            "/themes/default",
            &[],
            j(json!({"theme_id": theme})),
            Some(theme),
        )
        .await;
        let spare = id_of(
            &w.a(
                "POST",
                "/themes",
                &[],
                j(json!({"name": "Spare theme"})),
                None,
            )
            .await,
        );
        w.a("DELETE", "/themes/{id}", &[u(spare)], None, Some(spare))
            .await;

        zirv_flow_store::sending_fixtures::plant_verified_sender_as(
            &w.world.pool,
            w.world.ws,
            "walk-verified.example.com",
            SENDER.parse().unwrap(),
        )
        .await
        .unwrap();
        let template = w
            .a(
                "POST",
                "/templates",
                &[],
                j(json!({"name": "Walk template", "channel": "email",
                    "document": {"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
                        "subject": "Hi", "sender": SENDER,
                        "lang": "en", "category": "marketing", "sections": [{"id": "foot", "columns": [[{"type": "text", "id": "legal", "content": "<p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></p>"}]]}]}}})),
                None,
            )
            .await;
        let template_id = id_of(&template);
        w.a(
            "PUT",
            "/templates/{id}",
            &[u(template_id)],
            j(json!({"name": "Walk template 2"})),
            Some(template_id),
        )
        .await;
        let draft = template["draft"]["document"].clone();
        w.a(
            "PUT",
            "/templates/{id}/draft",
            &[u(template_id)],
            j(json!({"document": draft})),
            Some(template_id),
        )
        .await;
        w.a(
            "POST",
            "/templates/{id}/preview-people",
            &[u(template_id)],
            j(json!({"person_id": s.person})),
            None,
        )
        .await;
        w.a(
            "DELETE",
            "/templates/{id}/preview-people/{person_id}",
            &[u(template_id), u(s.person)],
            None,
            None,
        )
        .await;
        w.s(
            "POST",
            "/templates/{id}/preview",
            &[u(template_id)],
            j(json!({})),
        )
        .await;
        w.a(
            "POST",
            "/templates/{id}/publish",
            &[u(template_id)],
            j(json!({})),
            Some(template_id),
        )
        .await;
        let entry = w
            .a(
                "POST",
                "/test-send-allowlist",
                &[],
                j(json!({"address": "walk.allow@example.com"})),
                None,
            )
            .await;
        let entry = id_of(&entry);
        let destination = zirv_flow_store::create_destination(
            &w.world.pool,
            w.world.ws,
            &zirv_flow_store::NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "Walk test mail",
                config: &json!({"mode": "ok"}),
                credential_id: None,
                rate: 100,
                burst: 100,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        w.a(
            "POST",
            "/templates/{id}/test-send",
            &[u(template_id)],
            j(
                json!({"destination_id": destination, "sender_identity_id": SENDER,
                    "to": {"allowlist_entry": entry}}),
            ),
            Some(template_id),
        )
        .await;
        w.a(
            "POST",
            "/templates/{id}/draft/convert-to-mjml",
            &[u(template_id)],
            j(json!({})),
            Some(template_id),
        )
        .await;
        w.a(
            "DELETE",
            "/templates/{id}",
            &[u(template_id)],
            None,
            Some(template_id),
        )
        .await;
        w.a(
            "DELETE",
            "/test-send-allowlist/{id}",
            &[u(entry)],
            None,
            Some(entry),
        )
        .await;
    }

    async fn events_and_consent(w: &mut Walk<'_>, s: &Seeded) {
        w.a(
            "POST",
            "/event-schemas",
            &[],
            j(json!({"name": "walk_event", "properties": [{"name": "total", "type": "number"}]})),
            None,
        )
        .await;
        w.a(
            "PATCH",
            "/event-schemas/{name}",
            &["walk_event".to_owned()],
            j(json!({"description": "walk"})),
            None,
        )
        .await;
        w.a(
            "PUT",
            "/event-schemas/{name}/draft",
            &["walk_event".to_owned()],
            j(json!({"properties": [{"name": "total", "type": "number"}]})),
            None,
        )
        .await;
        w.a(
            "POST",
            "/event-schemas/{name}/versions",
            &["walk_event".to_owned()],
            None,
            None,
        )
        .await;
        w.a(
            "PUT",
            "/event-policy",
            &[],
            j(json!({"unplanned": "reject", "future_skew_seconds": 300,
                    "lateness_seconds": 86400, "redaction": {}})),
            None,
        )
        .await;
        let rejects: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM ingest_rejects WHERE workspace_id = $1 LIMIT 1")
                .bind(w.world.ws.as_uuid())
                .fetch_all(&w.world.pool)
                .await
                .unwrap();
        w.a(
            "POST",
            "/ingest-rejects/replay",
            &[],
            j(json!({"cause": "manual", "ids": rejects})),
            None,
        )
        .await;
        for tail in [
            "/events/explore/count",
            "/events/explore/breakdown",
            "/events/explore/list",
        ] {
            let body = crate::rbac::acceptance::tests::read_post_body(&format!("{WS}{tail}"), s);
            w.s("POST", tail, &[], body.map(Payload::Json)).await;
        }
        let body = crate::rbac::acceptance::tests::read_post_body("/events/explore/export", s);
        w.a(
            "POST",
            "/events/explore/export",
            &[],
            body.map(Payload::Json),
            None,
        )
        .await;

        let topic = id_of(
            &w.a(
                "POST",
                "/topics",
                &[],
                j(json!({"channel": "email", "key": "walk", "name": "Walk", "policy": "opt_out"})),
                None,
            )
            .await,
        );
        w.a(
            "PATCH",
            "/topics/{id}",
            &[u(topic)],
            j(json!({"description": "walk"})),
            Some(topic),
        )
        .await;
        w.a(
            "POST",
            "/consent/dimensions",
            &[],
            j(json!({"key": "walkdim", "name": "Walk", "values": [{"key": "a", "name": "A"}]})),
            None,
        )
        .await;
        w.a(
            "PATCH",
            "/consent/dimensions/{key}",
            &["walkdim".to_owned()],
            j(json!({"name": "Walk 2"})),
            None,
        )
        .await;
        w.a(
            "POST",
            "/consent/dimensions/{key}/values",
            &["walkdim".to_owned()],
            j(json!({"key": "b", "name": "B"})),
            None,
        )
        .await;
        w.a(
            "PATCH",
            "/consent/dimensions/{key}/values/{value}",
            &["walkdim".to_owned(), "b".to_owned()],
            j(json!({"name": "B 2"})),
            None,
        )
        .await;
        let draft = w.get("/consent/policy/draft").await;
        let revision = draft["revision"].as_i64().unwrap_or(0);
        w.a(
            "PUT",
            "/consent/policy/draft",
            &[],
            j(json!({"revision": revision, "rules": draft["rules"]})),
            None,
        )
        .await;
        let revision = w.get("/consent/policy/draft").await["revision"]
            .as_i64()
            .unwrap_or(revision);
        w.a(
            "POST",
            "/consent/policy/publish",
            &[],
            j(json!({"revision": revision})),
            None,
        )
        .await;
        w.s("POST", "/consent/policy/evaluate", &[], None).await;
        w.a(
            "POST",
            "/consent/import",
            &[],
            csv(&format!(
                "person_id,channel,topic,status\n{},email,newsletter,subscribed\n",
                s.person
            )),
            None,
        )
        .await;
        w.a(
            "PUT",
            "/people/{id}/consent",
            &[u(s.person)],
            j(json!({"channel": "email", "topic_key": "newsletter", "status": "subscribed"})),
            Some(s.person),
        )
        .await;
        w.a(
            "POST",
            "/suppressions",
            &[],
            j(json!({"kind": "email", "value": "walk.suppressed@example.com"})),
            None,
        )
        .await;
        w.s(
            "POST",
            "/suppressions/check",
            &[],
            j(json!({"kind": "email", "value": "walk.suppressed@example.com"})),
        )
        .await;
        w.a(
            "POST",
            "/suppressions/remove",
            &[],
            j(json!({"kind": "email", "value": "walk.suppressed@example.com"})),
            None,
        )
        .await;
        w.a(
            "POST",
            "/suppressions/import",
            &[],
            csv("kind,value\nemail,walk.imported@example.com\n"),
            None,
        )
        .await;

        w.a(
            "PUT",
            "/contact-definition",
            &[],
            j(json!({"fields": [{"key": "plan"}]})),
            None,
        )
        .await;
        w.a(
            "POST",
            "/contacts",
            &[],
            j(json!({"key": "walk-1", "fields": {"plan": "pro"}})),
            None,
        )
        .await;
        w.a(
            "POST",
            "/contacts/bulk",
            &[],
            j(json!({"contacts": [{"key": "walk-2", "fields": {"plan": "pro"}}]})),
            None,
        )
        .await;
        w.s("POST", "/contacts/search", &[], j(json!({"filters": []})))
            .await;
        w.a(
            "PUT",
            "/contacts/{key}",
            &["walk-1".to_owned()],
            j(json!({"fields": {"plan": "free"}})),
            None,
        )
        .await;
        w.a(
            "PATCH",
            "/contacts/{key}",
            &["walk-1".to_owned()],
            j(json!({"fields": {"plan": "pro"}})),
            None,
        )
        .await;
        w.a(
            "DELETE",
            "/contacts/{key}",
            &["walk-1".to_owned()],
            None,
            None,
        )
        .await;
    }

    async fn integrations(w: &mut Walk<'_>, s: &Seeded) {
        let credential = id_of(
            &w.a(
                "POST",
                "/credentials",
                &[],
                j(json!({"kind": "custom", "name": "Walk token",
                        "schema": {"fields": [{"key": "token", "label": "Token",
                                               "type": "string", "required": true, "secret": true}]},
                        "secret": {"token": "walk-secret-value"}})),
                None,
            )
            .await,
        );
        let destination = id_of(
            &w.a(
                "POST",
                "/destinations",
                &[],
                j(
                    json!({"kind": "fake", "name": "Walk mail", "config": {"mode": "ok"},
                        "credential_id": credential}),
                ),
                None,
            )
            .await,
        );
        w.a(
            "PATCH",
            "/destinations/{id}",
            &[u(destination)],
            j(json!({"name": "Walk mail 2"})),
            Some(destination),
        )
        .await;
        w.a(
            "POST",
            "/destinations/{id}/check",
            &[u(destination)],
            None,
            Some(destination),
        )
        .await;
        w.a(
            "PATCH",
            "/credentials/{id}",
            &[u(credential)],
            j(json!({"name": "Walk token 2"})),
            Some(credential),
        )
        .await;
        w.a(
            "PUT",
            "/credentials/{id}/secret",
            &[u(credential)],
            j(json!({"secret": {"token": "walk-secret-two"}})),
            Some(credential),
        )
        .await;
        w.a(
            "POST",
            "/credentials/{id}/test",
            &[u(credential)],
            j(json!({"destination_id": destination})),
            Some(credential),
        )
        .await;
        let rule = id_of(
            &w.a(
                "POST",
                "/forwarding/rules",
                &[],
                j(json!({"destination_id": destination, "name": "Walk rule",
                        "event_name": "order", "mapping": {"id": {"from": "event.id"}}})),
                None,
            )
            .await,
        );
        w.a(
            "PATCH",
            "/forwarding/rules/{id}",
            &[u(rule)],
            j(json!({"active": false})),
            Some(rule),
        )
        .await;
        w.a(
            "POST",
            "/forwarding/re-drive",
            &[],
            j(json!({"destination_id": destination})),
            None,
        )
        .await;
        w.a(
            "DELETE",
            "/forwarding/rules/{id}",
            &[u(rule)],
            None,
            Some(rule),
        )
        .await;
        w.a(
            "DELETE",
            "/destinations/{id}",
            &[u(destination)],
            None,
            Some(destination),
        )
        .await;

        let source = id_of(
            &w.a(
                "POST",
                "/sources",
                &[],
                j(
                    json!({"kind": "fake", "name": "Walk CRM", "config": {"streams": 2},
                        "credential_id": credential}),
                ),
                None,
            )
            .await,
        );
        w.a(
            "PATCH",
            "/sources/{id}",
            &[u(source)],
            j(json!({"name": "Walk CRM 2"})),
            Some(source),
        )
        .await;
        w.a(
            "POST",
            "/sources/{id}/test",
            &[u(source)],
            None,
            Some(source),
        )
        .await;
        w.a(
            "POST",
            "/sources/{id}/discover",
            &[u(source)],
            None,
            Some(source),
        )
        .await;
        w.a(
            "PUT",
            "/sources/{id}/streams/{name}",
            &[u(source), "stream_1".to_owned()],
            j(json!({"enabled": true, "sync_mode": "full_refresh",
                    "mapping": {"key_column": "id", "columns": [
                        {"column": "email", "target": {"type": "identifier", "kind": "email"}},
                        {"column": "name", "target": {"type": "trait", "key": "plan"}}]}})),
            None,
        )
        .await;
        w.s(
            "POST",
            "/sources/{id}/streams/{name}/preview",
            &[u(source), "stream_1".to_owned()],
            None,
        )
        .await;
        w.s("POST", "/source-manifests/validate", &[], None).await;
        w.s("POST", "/sources/{id}/manifest/sample", &[u(source)], None)
            .await;
        let runs = w
            .a(
                "POST",
                "/sources/{id}/runs",
                &[u(source)],
                j(json!({"kind": "sync"})),
                Some(source),
            )
            .await;
        let run = runs
            .as_array()
            .and_then(|runs| runs.first())
            .map_or_else(Uuid::nil, id_of);
        w.a(
            "POST",
            "/sources/{id}/runs/{run_id}/cancel",
            &[u(source), u(run)],
            None,
            None,
        )
        .await;
        w.a("DELETE", "/sources/{id}", &[u(source)], None, Some(source))
            .await;
        w.a(
            "DELETE",
            "/credentials/{id}",
            &[u(credential)],
            None,
            Some(credential),
        )
        .await;

        let hook = w
            .a(
                "POST",
                "/webhooks",
                &[],
                j(json!({"name": "Walk hook", "dedupe_field": "id"})),
                None,
            )
            .await;
        let hook = id_of(&hook);
        w.a(
            "PATCH",
            "/webhooks/{id}",
            &[u(hook)],
            j(json!({"tolerance_seconds": 60})),
            Some(hook),
        )
        .await;
        w.a(
            "POST",
            "/webhooks/{id}/rotate-secret",
            &[u(hook)],
            None,
            Some(hook),
        )
        .await;
        w.a("DELETE", "/webhooks/{id}", &[u(hook)], None, Some(hook))
            .await;
        let _ = s;
    }

    async fn sending(w: &mut Walk<'_>) {
        let domain = w
            .a(
                "POST",
                "/sending-domains",
                &[],
                j(json!({"domain": "walk-mail.example.com"})),
                None,
            )
            .await;
        let domain = id_of(&domain);
        w.a(
            "POST",
            "/sending-domains/{id}/verify",
            &[u(domain)],
            None,
            Some(domain),
        )
        .await;
        let identity = id_of(
            &w.a(
                "POST",
                "/sender-identities",
                &[],
                j(
                    json!({"domain_id": domain, "from_address": "news@walk-mail.example.com",
                        "from_name": "Walk"}),
                ),
                None,
            )
            .await,
        );
        w.a(
            "PATCH",
            "/sender-identities/{id}",
            &[u(identity)],
            j(json!({"from_name": "Walk 2"})),
            Some(identity),
        )
        .await;
        w.a(
            "DELETE",
            "/sender-identities/{id}",
            &[u(identity)],
            None,
            Some(identity),
        )
        .await;
        w.a(
            "DELETE",
            "/sending-domains/{id}",
            &[u(domain)],
            None,
            Some(domain),
        )
        .await;
        let settings = w.get("/sending/settings").await;
        let revision = settings["revision"].as_i64().unwrap_or(0);
        w.a(
            "PUT",
            "/sending/settings",
            &[],
            j(json!({"revision": revision, "frequency_caps": [], "quiet_hours": null})),
            None,
        )
        .await;
    }

    async fn lists_privacy_imports(w: &mut Walk<'_>, s: &Seeded) {
        let list = id_of(
            &w.a("POST", "/lists", &[], j(json!({"name": "Walk list"})), None)
                .await,
        );
        w.a(
            "PATCH",
            "/lists/{id}",
            &[u(list)],
            j(json!({"description": "walk"})),
            Some(list),
        )
        .await;
        w.a(
            "POST",
            "/lists/{id}/members",
            &[u(list)],
            j(json!({"person_ids": [s.person]})),
            Some(list),
        )
        .await;
        w.a(
            "POST",
            "/lists/{id}/members/remove",
            &[u(list)],
            j(json!({"person_ids": [s.person]})),
            Some(list),
        )
        .await;
        w.a(
            "POST",
            "/lists/{id}/members/import",
            &[u(list)],
            csv(&format!(
                "kind,value\nemail,{}\n",
                crate::rbac::acceptance::tests::EMAIL
            )),
            Some(list),
        )
        .await;
        w.a("DELETE", "/lists/{id}", &[u(list)], None, Some(list))
            .await;

        let doomed = throwaway_person(w, "walk-doomed").await;
        let erased = throwaway_person(w, "walk-erased").await;
        w.a("DELETE", "/people/{id}", &[u(doomed)], None, Some(doomed))
            .await;
        w.a(
            "POST",
            "/privacy/erasures",
            &[],
            j(json!({"person_id": erased})),
            None,
        )
        .await;
        let export = w
            .a(
                "POST",
                "/privacy/exports",
                &[],
                j(json!({"person_id": s.person})),
                None,
            )
            .await;
        let export = id_of(&export);
        sqlx::query(
            "UPDATE privacy_exports SET status = 'ready', content = $2, ready_at = now(), \
                 expires_at = now() + interval '1 day' WHERE id = $1",
        )
        .bind(export)
        .bind(vec![0x1f_u8, 0x8b])
        .execute(&w.world.pool)
        .await
        .unwrap();
        w.a(
            "GET",
            "/privacy/exports/{id}/download",
            &[u(export)],
            None,
            Some(export),
        )
        .await;
        w.a(
            "GET",
            "/imports/{id}/rejects.csv",
            &[u(s.import)],
            None,
            Some(s.import),
        )
        .await;
        w.a("GET", "/audit/export", &[], None, None).await;
        let owner = w.owner();
        w.audited(owner, "GET", "/api/v1/audit/export", &[], None, None)
            .await;
        w.a(
            "PUT",
            "/privacy/settings",
            &[],
            j(json!({"orphan_policy": "keep"})),
            None,
        )
        .await;
        w.a(
            "PUT",
            "/retention",
            &[],
            j(json!({"messages_days": 90})),
            None,
        )
        .await;

        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        )
        .unwrap();
        let asset = id_of(
            &w.a(
                "POST",
                "/assets",
                &[],
                Some(Payload::Raw("image/png", png)),
                None,
            )
            .await,
        );
        w.a("DELETE", "/assets/{id}", &[u(asset)], None, Some(asset))
            .await;

        let import = id_of(
            &w.a(
                "POST",
                "/imports",
                &[],
                csv(&format!(
                    "Email,Plan\n{},pro\n",
                    crate::rbac::acceptance::tests::EMAIL
                )),
                None,
            )
            .await,
        );
        w.s(
            "PUT",
            "/imports/{id}/mapping",
            &[u(import)],
            j(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]})),
        )
        .await;
        w.s("POST", "/imports/{id}/dry-run", &[u(import)], None)
            .await;
        sqlx::query("UPDATE imports SET status = 'dry_run_done' WHERE id = $1")
            .bind(import)
            .execute(&w.world.pool)
            .await
            .unwrap();
        w.a(
            "POST",
            "/imports/{id}/run",
            &[u(import)],
            None,
            Some(import),
        )
        .await;
        w.a(
            "POST",
            "/imports/{id}/cancel",
            &[u(import)],
            None,
            Some(import),
        )
        .await;
        let _ = s.import;
    }

    async fn throwaway_person(w: &Walk<'_>, key: &str) -> Uuid {
        let outcome = zirv_flow_store::apply_profile_batch(
            &w.world.pool,
            w.world.ws,
            &[zirv_flow_store::ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: key.into(),
                identifiers: vec![("external:crm".into(), format!("{key}-ext"))],
                traits: serde_json::Map::new(),
                observed_at: time::macros::datetime!(2026-01-01 00:00 UTC),
                trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("walk")),
            }],
        )
        .await
        .unwrap();
        match &outcome[0] {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. } => *person_id,
            other => panic!("not applied: {other:?}"),
        }
    }

    /// A key is the actor of what it writes: the audit row says `api_key`, not a user.
    async fn keys(w: &mut Walk<'_>) {
        let world = w.world;
        let writer = &world.keys[&Who::KeyWrite];
        let list = w
            .audited(
                Auth::Key(writer),
                "POST",
                &format!("{WS}/lists"),
                &[],
                j(json!({"name": "Key list"})),
                None,
            )
            .await;
        let list = id_of(&list);
        w.audited(
            Auth::Key(writer),
            "DELETE",
            &format!("{WS}/lists/{{id}}"),
            &[u(list)],
            None,
            Some(list),
        )
        .await;
        let admin = &world.keys[&Who::KeyAdmin];
        let hook = w
            .audited(
                Auth::Key(admin),
                "POST",
                &format!("{WS}/webhooks"),
                &[],
                j(json!({"name": "Key hook", "dedupe_field": "id"})),
                None,
            )
            .await;
        let hook = id_of(&hook);
        let rows: Vec<(String, Option<Uuid>)> = sqlx::query_as(
            "SELECT actor_kind, actor_id FROM audit_log WHERE action IN ('list.created', 'webhook.created') \n             AND request_id LIKE 'audit-walk-%' AND actor_kind = 'api_key'",
        )
        .fetch_all(&world.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|(_, actor)| actor.is_some()));
        let _ = hook;
    }

    async fn public(w: &mut Walk<'_>) {
        let world = w.world;
        let ghost = Uuid::new_v4().to_string();
        w.silent(
            Auth::Anonymous,
            "POST",
            "/hooks/{id}",
            std::slice::from_ref(&ghost),
            j(json!({"id": "1"})),
        )
        .await;
        w.silent(
            Auth::Anonymous,
            "POST",
            "/api/v1/destinations/{id}/feedback",
            std::slice::from_ref(&ghost),
            j(json!({})),
        )
        .await;
        for template in ["/u/{token}", "/u/{token}/preferences"] {
            w.silent(
                Auth::Anonymous,
                "POST",
                template,
                &["bad-token".to_owned()],
                Some(Payload::Raw(
                    "application/x-www-form-urlencoded",
                    b"a=b".to_vec(),
                )),
            )
            .await;
        }
        let ingest = &world.keys[&Who::KeyIngest];
        w.silent(
            Auth::Key(ingest),
            "POST",
            "/api/v1/ingest/identify",
            &[],
            j(json!({"identifiers": {"email": "walk.ingest@example.com"}})),
        )
        .await;
        w.silent(
            Auth::Key(ingest),
            "POST",
            "/api/v1/ingest/track",
            &[],
            j(
                json!({"identifiers": {"email": "walk.ingest@example.com"}, "event": "order",
                    "properties": {"total": 1}}),
            ),
        )
        .await;
        w.silent(
            Auth::Key(ingest),
            "POST",
            "/api/v1/ingest/batch",
            &[],
            j(json!({"events": []})),
        )
        .await;
    }
}
