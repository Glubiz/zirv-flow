//! The RBAC matrix acceptance test (architecture 16.2, exit criterion 1): every row of the route
//! table called as every principal, and a walk of the read endpoints over a workspace seeded with
//! distinctive personal values.
//!
//! The expectation comes from a row's class and need alone, written out here independently of the
//! handlers' own checks. The walk iterates `ROUTES`, so a new row is covered without editing it,
//! and builds each request from the OpenAPI document so a body or query the extractor would refuse
//! before `authorize` ran is never what a denied caller sees.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Instant;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use tower::ServiceExt;
    use uuid::Uuid;
    use zirv_flow_core::{HmacRoot, MasterKey, Role, Scope, WorkspaceId};
    use zirv_flow_store::{Clock, ManualClock};

    use crate::admin::{self, Context};
    use crate::auth::Need;
    use crate::rbac::{Class, Method, ROUTES, Route};
    use crate::{AppState, router};

    const PASSWORD: &str = "correct horse battery";
    const GHOST: &str = "ghost";
    const UNAUTHENTICATED: &str = "authentication required";
    const NOT_FOUND: &str = "no such resource";
    const NEEDS_SCOPE: &str = "the API key lacks the required scope";
    const OWNER_ONLY: &str = "only the instance owner can do this";
    const NO_KEYS: &str = "API keys cannot use instance-level endpoints";
    const SESSION_INGEST: &str = "ingestion needs an API key with the ingest scope";
    const ASSISTANT_NEEDS_USER: &str =
        "the assistant needs a signed-in user; an API key cannot use it";
    const OAUTH_NEEDS_USER: &str = "connecting an account needs a signed-in person, not an API key";

    /// Everyone the walk calls as.
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
    enum Who {
        Owner,
        Admin,
        Publisher,
        Editor,
        EditorWithPii,
        Viewer,
        ViewerWithPii,
        NonMember,
        ForeignMember,
        KeyIngest,
        KeyRead,
        KeyWrite,
        KeyWriteAndPii,
        KeyReadAndPii,
        KeyPublish,
        KeyAdmin,
        KeyPii,
        ForeignKey,
        Anonymous,
    }

    const EVERYONE: [Who; 19] = [
        Who::Owner,
        Who::Admin,
        Who::Publisher,
        Who::Editor,
        Who::EditorWithPii,
        Who::Viewer,
        Who::ViewerWithPii,
        Who::NonMember,
        Who::ForeignMember,
        Who::KeyIngest,
        Who::KeyRead,
        Who::KeyWrite,
        Who::KeyWriteAndPii,
        Who::KeyReadAndPii,
        Who::KeyPublish,
        Who::KeyAdmin,
        Who::KeyPii,
        Who::ForeignKey,
        Who::Anonymous,
    ];

    /// What a principal is, as far as the expectation goes.
    enum Kind {
        Anonymous,
        Owner,
        /// A member of the walked workspace: the rank of their role (viewer 0 to admin 3) and
        /// whether they hold `view_pii`.
        Member(u8, bool),
        /// A signed-in user with no access to the walked workspace.
        Stranger,
        /// A key of the walked workspace.
        Key(&'static [Scope]),
        /// A key of another workspace, holding every scope.
        ForeignKey,
    }

    impl Who {
        fn kind(self) -> Kind {
            match self {
                Who::Anonymous => Kind::Anonymous,
                Who::Owner => Kind::Owner,
                Who::Viewer => Kind::Member(0, false),
                Who::ViewerWithPii => Kind::Member(0, true),
                Who::Editor => Kind::Member(1, false),
                Who::EditorWithPii => Kind::Member(1, true),
                Who::Publisher => Kind::Member(2, false),
                Who::Admin => Kind::Member(3, false),
                Who::NonMember | Who::ForeignMember => Kind::Stranger,
                Who::KeyIngest => Kind::Key(&[Scope::Ingest]),
                Who::KeyRead => Kind::Key(&[Scope::Read]),
                Who::KeyWrite => Kind::Key(&[Scope::Write]),
                Who::KeyWriteAndPii => Kind::Key(&[Scope::Write, Scope::Pii]),
                Who::KeyReadAndPii => Kind::Key(&[Scope::Read, Scope::Pii]),
                Who::KeyPublish => Kind::Key(&[Scope::Publish]),
                Who::KeyAdmin => Kind::Key(&[Scope::Admin]),
                Who::KeyPii => Kind::Key(&[Scope::Pii]),
                Who::ForeignKey => Kind::ForeignKey,
            }
        }

        fn is_session(self) -> bool {
            matches!(self.kind(), Kind::Owner | Kind::Member(..) | Kind::Stranger)
        }
    }

    /// What the caller must see.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Expect {
        Unauthenticated,
        /// 403 with this detail.
        Forbidden(String),
        /// 403 with this stable problem code.
        Refused(&'static str),
        NotFound,
        /// A browser redirect, whoever asks.
        Redirect,
        /// Past the checks: anything but 401, 403 or a server error.
        Through,
    }

    /// The rank a role needs, or none when no role can.
    fn needed_rank(need: Need) -> Option<u8> {
        match need {
            Need::Read | Need::ReadMembers => Some(0),
            Need::Edit => Some(1),
            Need::Publish => Some(2),
            Need::Admin => Some(3),
            Need::Ingest => None,
        }
    }

    fn role_word(need: Need) -> &'static str {
        match need {
            Need::Publish => "publisher",
            Need::Edit => "editor",
            Need::Admin => "admin",
            Need::Read | Need::ReadMembers | Need::Ingest => "member",
        }
    }

    fn scope_allows(scopes: &[Scope], need: Need) -> bool {
        let has = |scope: Scope| scopes.contains(&scope);
        match need {
            Need::Read => has(Scope::Read),
            Need::ReadMembers => has(Scope::Read) || has(Scope::Admin),
            Need::Edit => has(Scope::Write),
            Need::Publish => has(Scope::Publish),
            Need::Admin => has(Scope::Admin),
            Need::Ingest => has(Scope::Ingest),
        }
    }

    const EVERY_SCOPE: [Scope; 6] = [
        Scope::Ingest,
        Scope::Read,
        Scope::Write,
        Scope::Publish,
        Scope::Admin,
        Scope::Pii,
    ];

    /// How an instance-level row treats a caller. The class carries no need, so the row's note and
    /// path say.
    #[derive(Debug, Eq, PartialEq)]
    enum Instance {
        /// No principal is read: nobody is refused for who they are.
        Open,
        /// A signed-in user; a key is refused.
        Session,
        /// Any principal, a key included.
        Principal,
    }

    fn instance_rule(row: &Route) -> Instance {
        if row.note.starts_with("no principal") {
            Instance::Open
        } else if matches!(
            (row.method, row.path),
            (Method::Get | Method::Delete, "/api/v1/session") | (Method::Get, "/api/v1/workspaces")
        ) {
            Instance::Session
        } else {
            Instance::Principal
        }
    }

    /// Where a request's outcome comes from when the row's class alone does not say.
    #[derive(Clone, Copy, Debug)]
    enum Rule {
        /// The row's class.
        Row,
        /// A workspace need, whatever the row's strictest class says.
        Need(Need),
        /// A request naming a real person: `Edit`, and also the `view_pii` grant, refused with
        /// `code`. A workspace admin passes at once when `admin_passes`.
        PersonValues {
            code: &'static str,
            admin_passes: bool,
        },
        /// A session needs `session`; a key needs `admin` and is then refused with `refusal`,
        /// because the route acts for a person.
        NoKeys {
            session: Need,
            then: Option<Need>,
            refusal: &'static str,
        },
        /// The route answers every caller with a redirect: its authority is a signed state token.
        Redirect,
    }

    /// One way to call a row: an optional patch of the sample body and the rule it falls under.
    #[derive(Clone, Debug)]
    struct Case {
        patch: Option<Value>,
        rule: Rule,
    }

    /// The ways to call `row`. Most rows have one; a row whose need depends on the request, as its
    /// note says, has one per branch.
    fn cases(row: &Route) -> Vec<Case> {
        let plain = |rule| Case { patch: None, rule };
        let post = row.method == Method::Post;
        if post && row.path.ends_with("/ai/drafts") {
            let rule = |then| Rule::NoKeys {
                session: Need::Edit,
                then,
                refusal: ASSISTANT_NEEDS_USER,
            };
            // A mapping or manifest draft reads a source; the other kinds are the editor's own.
            return vec![
                plain(rule(Some(Need::Admin))),
                Case {
                    patch: Some(json!({"kind": "segment"})),
                    rule: rule(None),
                },
            ];
        }
        if post && row.path.ends_with("/ai/drafts/{id}/accept") {
            return vec![plain(Rule::NoKeys {
                session: Need::Edit,
                then: None,
                refusal: ASSISTANT_NEEDS_USER,
            })];
        }
        if post && row.path.ends_with("/ai/explain") {
            return vec![plain(Rule::NoKeys {
                session: Need::Admin,
                then: None,
                refusal: ASSISTANT_NEEDS_USER,
            })];
        }
        if post && row.path.ends_with("/oauth/start") {
            return vec![plain(Rule::NoKeys {
                session: Need::Admin,
                then: None,
                refusal: OAUTH_NEEDS_USER,
            })];
        }
        if row.path == "/api/v1/oauth/callback" {
            return vec![plain(Rule::Redirect)];
        }
        if row.note.starts_with("Edit; a request naming a real person") {
            let (code, admin_passes) = if row.path.ends_with("/flows/{id}/test") {
                ("flow_test_values_forbidden", false)
            } else {
                ("template_preview_values_forbidden", true)
            };
            return vec![
                plain(Rule::Need(Need::Edit)),
                Case {
                    patch: Some(json!({"person_id": Uuid::now_v7()})),
                    rule: Rule::PersonValues { code, admin_passes },
                },
            ];
        }
        vec![plain(Rule::Row)]
    }

    /// What `who` must see on a workspace route that takes `need`.
    fn workspace_outcome(need: Need, kind: &Kind) -> Expect {
        match kind {
            Kind::Anonymous => Expect::Unauthenticated,
            Kind::Owner => Expect::Through,
            Kind::Stranger | Kind::ForeignKey => Expect::NotFound,
            Kind::Member(rank, _) => match needed_rank(need) {
                Some(needed) if *rank >= needed => Expect::Through,
                _ => Expect::Forbidden(format!(
                    "this needs the {} role in the workspace",
                    role_word(need)
                )),
            },
            Kind::Key(scopes) if scope_allows(scopes, need) => Expect::Through,
            Kind::Key(_) => Expect::Forbidden(NEEDS_SCOPE.into()),
        }
    }

    /// What `who` must see on `row` under `rule`, from the class and need alone.
    fn expected(row: &Route, rule: Rule, who: Who) -> Expect {
        let kind = who.kind();
        match rule {
            Rule::Need(need) => return workspace_outcome(need, &kind),
            Rule::Redirect => return Expect::Redirect,
            Rule::NoKeys {
                session,
                then,
                refusal,
            } => {
                return match &kind {
                    Kind::Key(scopes) if scope_allows(scopes, Need::Admin) => {
                        Expect::Forbidden(refusal.into())
                    }
                    Kind::Key(_) => Expect::Forbidden(NEEDS_SCOPE.into()),
                    _ => match (workspace_outcome(session, &kind), then) {
                        (Expect::Through, Some(then)) => workspace_outcome(then, &kind),
                        (outcome, _) => outcome,
                    },
                };
            }
            Rule::PersonValues { code, admin_passes } => {
                let (revealed, admin) = match &kind {
                    Kind::Member(rank, pii) => (*rank == 3 || *pii, *rank == 3),
                    Kind::Key(scopes) => (
                        scopes.contains(&Scope::Admin) || scopes.contains(&Scope::Pii),
                        scopes.contains(&Scope::Admin),
                    ),
                    _ => (true, false),
                };
                return match (workspace_outcome(Need::Edit, &kind), revealed) {
                    (Expect::Through, true) => Expect::Through,
                    (Expect::Through, false) => Expect::Refused(code),
                    // Short of Edit: an admin-grade caller passes the render routes at once, and
                    // they answer every other refusal with the values code.
                    (Expect::Forbidden(_), _) if admin_passes && admin => Expect::Through,
                    (Expect::Forbidden(_), _) if admin_passes => Expect::Refused(code),
                    (other, _) => other,
                };
            }
            Rule::Row => {}
        }
        match row.class {
            Class::Public => Expect::Through,
            Class::Unchecked => panic!("{} is unchecked", row_name(row)),
            Class::Instance => match (instance_rule(row), kind) {
                (Instance::Open, _) => Expect::Through,
                (_, Kind::Anonymous) => Expect::Unauthenticated,
                (Instance::Session, Kind::Key(_) | Kind::ForeignKey) => {
                    Expect::Forbidden(NO_KEYS.into())
                }
                _ => Expect::Through,
            },
            Class::Owner => match kind {
                Kind::Anonymous => Expect::Unauthenticated,
                Kind::Owner => Expect::Through,
                Kind::Key(_) | Kind::ForeignKey => Expect::Forbidden(NO_KEYS.into()),
                Kind::Member(..) | Kind::Stranger => Expect::Forbidden(OWNER_ONLY.into()),
            },
            Class::Ingest => match kind {
                Kind::Anonymous => Expect::Unauthenticated,
                Kind::Key(scopes) if scopes.contains(&Scope::Ingest) => Expect::Through,
                Kind::ForeignKey => Expect::Through,
                Kind::Key(_) => Expect::Forbidden(NEEDS_SCOPE.into()),
                Kind::Owner | Kind::Member(..) | Kind::Stranger => {
                    Expect::Forbidden(SESSION_INGEST.into())
                }
            },
            Class::Need(need) => workspace_outcome(need, &kind),
        }
    }

    /// What came back.
    #[derive(Clone, Debug, Default)]
    struct Seen {
        status: u16,
        detail: String,
        code: String,
        location: String,
    }

    /// Why `seen` is not what `expect` asks for.
    fn mismatch(expect: &Expect, seen: &Seen) -> Option<String> {
        let ok = match expect {
            Expect::Unauthenticated => seen.status == 401 && seen.detail == UNAUTHENTICATED,
            Expect::Forbidden(detail) => seen.status == 403 && &seen.detail == detail,
            Expect::Refused(code) => seen.status == 403 && seen.code == *code,
            Expect::NotFound => seen.status == 404 && seen.detail == NOT_FOUND,
            Expect::Redirect => seen.status == 303 && seen.location.contains("oauth=failed"),
            // No render host runs under test, which the handlers say after their checks.
            Expect::Through => {
                !matches!(seen.status, 401 | 403)
                    && (seen.status < 500 || seen.code == "render_unavailable")
            }
        };
        (!ok).then(|| {
            format!(
                "expected {expect:?}, got {} {:?} {:?}",
                seen.status, seen.detail, seen.code
            )
        })
    }

    /// What a request would see if the handlers followed `rule` to the letter.
    fn obedient(row: &Route, rule: Rule, who: Who) -> Seen {
        let seen = |status, detail: &str, code: &str| Seen {
            status,
            detail: detail.into(),
            code: code.into(),
            location: String::new(),
        };
        match expected(row, rule, who) {
            Expect::Unauthenticated => seen(401, UNAUTHENTICATED, ""),
            Expect::Forbidden(detail) => seen(403, &detail, ""),
            Expect::Refused(code) => seen(403, "", code),
            Expect::NotFound => seen(404, NOT_FOUND, ""),
            Expect::Redirect => Seen {
                status: 303,
                location: "/?oauth=failed".into(),
                ..Seen::default()
            },
            Expect::Through => seen(404, NOT_FOUND, ""),
        }
    }

    fn row_name(row: &Route) -> String {
        format!("{:?} {}", row.method, row.path)
    }

    /// Every principal for which `rule` on `row` and the handlers disagree, given what `seen`
    /// returns.
    fn disagreements(row: &Route, rule: Rule, seen: impl Fn(Who) -> Seen) -> Vec<(Who, String)> {
        EVERYONE
            .into_iter()
            .filter(|who| row.class != Class::Public || *who == Who::Anonymous)
            .filter_map(|who| mismatch(&expected(row, rule, who), &seen(who)).map(|why| (who, why)))
            .collect()
    }

    // ---- requests built from the OpenAPI document ----

    struct Operations {
        document: Value,
    }

    impl Operations {
        fn new() -> Self {
            Self {
                document: serde_json::to_value(crate::openapi()).unwrap(),
            }
        }

        fn operation(&self, row: &Route) -> Option<&Value> {
            let method = format!("{:?}", row.method).to_lowercase();
            self.document["paths"].get(row.path)?.get(method)
        }

        fn resolve<'a>(&'a self, schema: &'a Value) -> &'a Value {
            match schema.get("$ref").and_then(Value::as_str) {
                Some(reference) => {
                    let name = reference.rsplit('/').next().unwrap_or_default();
                    &self.document["components"]["schemas"][name]
                }
                None => schema,
            }
        }

        /// The smallest value that deserializes as `schema`: required fields only, the first
        /// variant of a choice.
        fn sample(&self, schema: &Value, depth: u8) -> Value {
            let schema = self.resolve(schema);
            if depth == 0 {
                return Value::Null;
            }
            if let Some(values) = schema.get("enum").and_then(Value::as_array) {
                return values.first().cloned().unwrap_or(Value::Null);
            }
            if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
                let mut merged = serde_json::Map::new();
                for part in parts {
                    if let Value::Object(map) = self.sample(part, depth - 1) {
                        merged.extend(map);
                    }
                }
                return Value::Object(merged);
            }
            for choice in ["oneOf", "anyOf"] {
                if let Some(parts) = schema.get(choice).and_then(Value::as_array)
                    && let Some(first) = parts
                        .iter()
                        .find(|part| part.get("type").and_then(Value::as_str) != Some("null"))
                {
                    return self.sample(first, depth - 1);
                }
            }
            let kind = match schema.get("type") {
                Some(Value::String(kind)) => Some(kind.as_str()),
                Some(Value::Array(kinds)) => kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|kind| *kind != "null"),
                _ => None,
            };
            match kind {
                Some("string") => match schema.get("format").and_then(Value::as_str) {
                    Some("uuid") => json!(Uuid::now_v7()),
                    Some("date-time") => json!("2030-01-01T00:00:00Z"),
                    Some("date") => json!("2030-01-01"),
                    Some("uri") => json!("https://example.com"),
                    _ => json!("x"),
                },
                Some("integer") => json!(
                    schema
                        .get("minimum")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .max(0)
                ),
                Some("number") => json!(
                    schema
                        .get("minimum")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                        .max(0.0)
                ),
                Some("boolean") => json!(false),
                Some("array") => match (schema.get("items"), schema.get("minItems")) {
                    (Some(items), Some(min)) if min.as_u64().unwrap_or(0) > 0 => {
                        json!([self.sample(items, depth - 1)])
                    }
                    _ => json!([]),
                },
                Some("object") | None => {
                    let mut map = serde_json::Map::new();
                    let required: Vec<&str> = schema
                        .get("required")
                        .and_then(Value::as_array)
                        .map(|names| names.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    for name in required {
                        if let Some(property) = schema.get("properties").and_then(|p| p.get(name)) {
                            map.insert(name.to_owned(), self.sample(property, depth - 1));
                        }
                    }
                    Value::Object(map)
                }
                Some(_) => Value::Null,
            }
        }

        /// The content type and body of the request, and the query it needs.
        fn request(&self, row: &Route) -> (Option<(String, Vec<u8>)>, String) {
            let Some(operation) = self.operation(row) else {
                return (None, String::new());
            };
            let mut query = Vec::new();
            for parameter in operation
                .get("parameters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let parameter = self.resolve(parameter);
                if parameter["in"] == "query" && parameter["required"] == true {
                    let value = match self.sample(&parameter["schema"], 6) {
                        Value::String(text) => text,
                        other => other.to_string(),
                    };
                    query.push(format!(
                        "{}={value}",
                        parameter["name"].as_str().unwrap_or("")
                    ));
                }
            }
            let query = if query.is_empty() {
                String::new()
            } else {
                format!("?{}", query.join("&"))
            };
            let body = operation.get("requestBody").and_then(|body| {
                let content = self.resolve(body)["content"].as_object()?;
                match content.get("application/json") {
                    Some(json) => {
                        let body = self.sample(&json["schema"], 8);
                        Some(("application/json".to_owned(), body.to_string().into_bytes()))
                    }
                    None => content
                        .keys()
                        .next()
                        .map(|kind| (kind.clone(), b"a,b\n1,2\n".to_vec())),
                }
            });
            (body, query)
        }
    }

    /// The row's path with the workspace and a ghost for every other placeholder.
    fn concrete(path: &str, workspace: WorkspaceId) -> String {
        let ghost = Uuid::now_v7().to_string();
        let mut out = String::new();
        let mut rest = path;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let end = rest[start..].find('}').unwrap() + start;
            let name = &rest[start + 1..end];
            out.push_str(&match name {
                "ws" => workspace.as_uuid().to_string(),
                "id" | "user_id" | "enrollment_id" | "run_id" | "person_id" => ghost.clone(),
                "n" => "1".to_owned(),
                "trace_id" => "0".repeat(32),
                _ => GHOST.to_owned(),
            });
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        out
    }

    // ---- fixtures ----

    struct Login {
        cookie: String,
        csrf: String,
    }

    enum Auth<'a> {
        Anonymous,
        Key(&'a str),
        Session(&'a Login),
    }

    struct Reply {
        status: u16,
        body: Value,
        text: String,
        location: String,
    }

    impl Reply {
        fn seen(&self) -> Seen {
            let text = |name: &str| self.body[name].as_str().unwrap_or_default().to_owned();
            Seen {
                status: self.status,
                detail: text("detail"),
                code: text("code"),
                location: self.location.clone(),
            }
        }
    }

    async fn send(
        router: &Router,
        method: &str,
        uri: &str,
        body: Option<(String, Vec<u8>)>,
        auth: Auth<'_>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
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
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let streaming = response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|kind| kind == "text/event-stream");
        if streaming {
            return Reply {
                status,
                body: Value::Null,
                text: String::new(),
                location,
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
            location,
        }
    }

    async fn sign_in(router: &Router, email: &str) -> Login {
        let body = json!({"email": email, "password": PASSWORD}).to_string();
        let response = router
            .clone()
            .oneshot(
                Request::post("/api/v1/session")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "sign-in of {email}");
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        Login {
            cookie,
            csrf: body["csrf_token"].as_str().unwrap().to_owned(),
        }
    }

    /// The workspaces, their people and every principal of the walk.
    struct World {
        pool: PgPool,
        router: Router,
        ws: WorkspaceId,
        sessions: BTreeMap<Who, Login>,
        keys: BTreeMap<Who, String>,
    }

    impl World {
        fn auth(&self, who: Who) -> Auth<'_> {
            match who {
                Who::Anonymous => Auth::Anonymous,
                _ if who.is_session() => Auth::Session(&self.sessions[&who]),
                _ => Auth::Key(&self.keys[&who]),
            }
        }

        fn url(&self, tail: &str) -> String {
            format!("/api/v1/workspaces/{}{tail}", self.ws.as_uuid())
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse(slug).unwrap();
        let slug = zirv_flow_core::Slug::parse(slug).unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn world(pool: &PgPool) -> World {
        zirv_flow_store::migrate(pool).await.unwrap();
        let master = MasterKey::generate();
        let root = HmacRoot::bootstrap_from(&master);
        let keyring = Arc::new(zirv_flow_core::Keyring::new(master, None).unwrap());
        zirv_flow_store::secrets::insert_hmac_root_if_absent(
            pool,
            &zirv_flow_core::seal(&keyring, b"root", b"aad"),
        )
        .await
        .unwrap();
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_hmac_root(&root)
            .with_keyring(keyring)
            .with_clock(Clock::Manual(ManualClock::new(
                datetime!(2031-05-06 07:00 UTC),
            )));
        let router = router(state);
        let ws = workspace(pool, "ours").await;
        let other = workspace(pool, "theirs").await;
        let system = Context::system();

        admin::create_owner(pool, &system, "owner@example.com", PASSWORD)
            .await
            .unwrap();
        let mut sessions = BTreeMap::new();
        sessions.insert(Who::Owner, sign_in(&router, "owner@example.com").await);
        for (who, email, home, role, grant) in [
            (
                Who::Admin,
                "admin@example.com",
                ws,
                Some(Role::Admin),
                false,
            ),
            (
                Who::Publisher,
                "publisher@example.com",
                ws,
                Some(Role::Publisher),
                false,
            ),
            (
                Who::Editor,
                "editor@example.com",
                ws,
                Some(Role::Editor),
                false,
            ),
            (
                Who::EditorWithPii,
                "editor-pii@example.com",
                ws,
                Some(Role::Editor),
                true,
            ),
            (
                Who::Viewer,
                "viewer@example.com",
                ws,
                Some(Role::Viewer),
                false,
            ),
            (
                Who::ViewerWithPii,
                "pii@example.com",
                ws,
                Some(Role::Viewer),
                true,
            ),
            (Who::NonMember, "stranger@example.com", ws, None, false),
            (
                Who::ForeignMember,
                "foreign@example.com",
                other,
                Some(Role::Admin),
                true,
            ),
        ] {
            let user = admin::create_user(pool, &system, email, PASSWORD)
                .await
                .unwrap();
            if let Some(role) = role {
                admin::set_member_grant(pool, &system, home, user.id, role, Some(grant))
                    .await
                    .unwrap();
            }
            sessions.insert(who, sign_in(&router, email).await);
        }
        let mut keys = BTreeMap::new();
        for who in EVERYONE {
            if let Kind::Key(scopes) = who.kind() {
                let created = admin::create_api_key(pool, &system, ws, "walk", scopes)
                    .await
                    .unwrap();
                keys.insert(who, created.plaintext);
            }
        }
        let foreign = admin::create_api_key(pool, &system, other, "foreign", &EVERY_SCOPE)
            .await
            .unwrap();
        keys.insert(Who::ForeignKey, foreign.plaintext);
        World {
            pool: pool.clone(),
            router,
            ws,
            sessions,
            keys,
        }
    }

    // ---- the walk ----

    struct Call {
        row: Route,
        rule: Rule,
        who: Who,
        seen: Seen,
    }

    /// Calls every row, in every way it has, as every principal it applies to. Signing out goes
    /// last so the sessions last the whole walk.
    async fn walk(world: &World) -> Vec<Call> {
        let operations = Operations::new();
        let mut rows: Vec<&Route> = ROUTES.iter().collect();
        rows.sort_by_key(|row| row.method == Method::Delete && row.path == "/api/v1/session");
        let mut calls = Vec::new();
        for row in rows {
            let (sample, query) = operations.request(row);
            let uri = format!("{}{query}", concrete(row.path, world.ws));
            let method = format!("{:?}", row.method).to_uppercase();
            for case in cases(row) {
                let body = match row.method {
                    Method::Get | Method::Delete => None,
                    _ => {
                        let (kind, bytes) = sample
                            .clone()
                            .unwrap_or_else(|| ("application/json".into(), b"{}".to_vec()));
                        Some((kind, patched(bytes, &case.patch)))
                    }
                };
                for who in EVERYONE {
                    if row.class == Class::Public && who != Who::Anonymous {
                        continue;
                    }
                    let reply =
                        send(&world.router, &method, &uri, body.clone(), world.auth(who)).await;
                    calls.push(Call {
                        row: *row,
                        rule: case.rule,
                        who,
                        seen: reply.seen(),
                    });
                }
            }
        }
        calls
    }

    /// `bytes` with the keys of `patch` set, when it is a JSON object.
    fn patched(bytes: Vec<u8>, patch: &Option<Value>) -> Vec<u8> {
        let Some(Value::Object(patch)) = patch else {
            return bytes;
        };
        let mut body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
        if let Some(object) = body.as_object_mut() {
            object.extend(patch.clone());
        }
        body.to_string().into_bytes()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rbac_matrix_holds_for_every_row_and_principal(pool: PgPool) {
        let world = world(&pool).await;
        let started = Instant::now();

        let calls = walk(&world).await;

        eprintln!(
            "rbac walk: {} rows, {} principals, {} calls in {:?}",
            ROUTES.len(),
            EVERYONE.len(),
            calls.len(),
            started.elapsed()
        );
        let wrong: Vec<String> = calls
            .iter()
            .filter_map(|call| {
                mismatch(&expected(&call.row, call.rule, call.who), &call.seen)
                    .map(|why| format!("{} as {:?}: {why}", row_name(&call.row), call.who))
            })
            .collect();
        assert!(
            wrong.is_empty(),
            "{} mismatches:\n{}",
            wrong.len(),
            wrong.join("\n")
        );

        // A key and a member of another workspace get 404 on every row of this one.
        let foreign: Vec<&Call> = calls
            .iter()
            .filter(|call| matches!(call.who, Who::ForeignKey | Who::ForeignMember))
            .filter(|call| call.row.path.contains("/workspaces/{ws}"))
            .collect();
        assert!(foreign.len() > 400, "{}", foreign.len());
        let leaked: Vec<String> = foreign
            .iter()
            .filter(|call| call.seen.status != 404 || call.seen.detail != NOT_FOUND)
            .map(|call| {
                format!(
                    "{} as {:?}: {}",
                    row_name(&call.row),
                    call.who,
                    call.seen.status
                )
            })
            .collect();
        assert!(leaked.is_empty(), "{}", leaked.join("\n"));
    }

    #[test]
    fn a_weakened_need_makes_the_comparison_report_a_mismatch() {
        let row = ROUTES
            .iter()
            .find(|row| {
                row.path == "/api/v1/workspaces/{ws}/api-keys" && row.method == Method::Post
            })
            .copied()
            .unwrap();
        assert_eq!(row.class, Class::Need(Need::Admin));
        let mut weakened = row;
        weakened.class = Class::Need(Need::Read);

        // Handlers that follow the real row, judged against the real row and the weakened one.
        let seen = |who| obedient(&row, Rule::Row, who);
        assert_eq!(disagreements(&row, Rule::Row, seen), Vec::new());
        let against_weakened = disagreements(&weakened, Rule::Row, seen);

        let caught: Vec<Who> = against_weakened.iter().map(|(who, _)| *who).collect();
        assert!(caught.contains(&Who::Viewer), "{against_weakened:?}");
        assert!(caught.contains(&Who::Editor));
        assert!(caught.contains(&Who::KeyRead));
        assert!(!caught.contains(&Who::Admin));
    }

    #[test]
    fn every_row_has_an_expectation_for_every_principal() {
        for row in ROUTES {
            for case in cases(row) {
                for who in EVERYONE {
                    let _ = expected(row, case.rule, who);
                }
            }
        }
    }

    #[test]
    fn a_request_dependent_row_is_walked_once_per_branch() {
        let branches = |path: &str| {
            let row = ROUTES.iter().find(|row| row.path == path).unwrap();
            cases(row).len()
        };
        assert_eq!(
            branches("/api/v1/workspaces/{ws}/templates/{id}/preview"),
            2
        );
        assert_eq!(
            branches("/api/v1/workspaces/{ws}/templates/{id}/test-send"),
            2
        );
        assert_eq!(branches("/api/v1/workspaces/{ws}/flows/{id}/test"), 2);
        assert_eq!(branches("/api/v1/workspaces/{ws}/ai/drafts"), 2);
        assert_eq!(branches("/api/v1/workspaces/{ws}/flows"), 1);
    }

    #[test]
    fn a_real_person_needs_the_grant_on_top_of_edit() {
        let row = ROUTES
            .iter()
            .find(|row| row.path == "/api/v1/workspaces/{ws}/flows/{id}/test")
            .unwrap();
        let rule = Rule::PersonValues {
            code: "flow_test_values_forbidden",
            admin_passes: false,
        };
        let as_who = |who| expected(row, rule, who);
        assert_eq!(
            as_who(Who::Editor),
            Expect::Refused("flow_test_values_forbidden")
        );
        assert_eq!(as_who(Who::EditorWithPii), Expect::Through);
        assert_eq!(as_who(Who::Admin), Expect::Through);
        assert_eq!(
            as_who(Who::KeyWrite),
            Expect::Refused("flow_test_values_forbidden")
        );
        assert_eq!(as_who(Who::KeyWriteAndPii), Expect::Through);
        assert!(matches!(as_who(Who::ViewerWithPii), Expect::Forbidden(_)));
        assert_eq!(as_who(Who::NonMember), Expect::NotFound);
    }

    #[test]
    fn a_path_is_filled_with_the_workspace_and_ghosts() {
        let ws = WorkspaceId::new();
        let path = concrete(
            "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/x/{name}",
            ws,
        );
        assert!(path.starts_with(&format!("/api/v1/workspaces/{}/flows/", ws.as_uuid())));
        assert!(path.ends_with("/versions/1/x/ghost"));
        assert!(!path.contains('{'));
    }

    #[test]
    fn a_sample_body_holds_the_required_fields_only() {
        let operations = Operations::new();
        let row = ROUTES
            .iter()
            .find(|row| {
                row.path == "/api/v1/workspaces/{ws}/api-keys" && row.method == Method::Post
            })
            .unwrap();
        let (body, query) = operations.request(row);
        let (kind, bytes) = body.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(kind, "application/json");
        assert!(query.is_empty());
        assert!(
            body["name"].is_string() && body["scopes"].is_array(),
            "{body}"
        );
        let patched: Value =
            serde_json::from_slice(&patched(bytes, &Some(json!({"name": "y"})))).unwrap();
        assert_eq!(patched["name"], "y");
    }

    // ---- the seeded-personal-data walk ----

    const EMAIL: &str = "zq.ada.7731@seed-pii.test";
    const PHONE_DIGITS: &str = "70123499";
    const NAME: &str = "Zqfirstname";
    const PROPERTY: &str = "ZQ-PROP-31337";
    const EXTERNAL: &str = "ZQ-EXT-9912";
    const EVIDENCE: &str = "ZQ-EVIDENCE-8814";
    const PAYLOAD: &str = "ZQ-PAYLOAD-5521";
    const REJECTED_CELL: &str = "zq.reject.4410@seed-pii.test";
    const UNPLANNED: &str = "ZQ-UNPLANNED-6603";
    /// A list name is workspace configuration, not a person's data: every reader sees it. It is
    /// seeded to show the walk reads real data.
    const LIST_NAME: &str = "ZQ-List-8842";

    /// The seeded personal values, by what they are.
    const PERSONAL: [(&str, &str); 8] = [
        ("an email", EMAIL),
        ("a phone number", PHONE_DIGITS),
        ("a name trait", NAME),
        ("an event property", PROPERTY),
        ("an external id", EXTERNAL),
        ("consent evidence", EVIDENCE),
        ("a rejected import cell", REJECTED_CELL),
        ("an unplanned event property", UNPLANNED),
    ];

    /// Surfaces that must show a seeded value to a reader with the grant, and none without it.
    /// A `{}` stands for any one id.
    const REVEALS: [&str; 6] = [
        "/people/{id}",
        "/people/{id}/timeline",
        "/lists/{id}/members",
        "/events/explore/list",
        "/imports/{id}/rejects.csv",
        "/people/{id}/consent/history",
    ];

    /// Surfaces that keep a seeded value out of their response even for an admin, who sees values
    /// elsewhere: a response field the table behind it fills with personal data. `(path end, what, value, a marker
    /// the row's own fields put in the response)`.
    const HIDDEN_ON: [(&str, &str, &str, &str); 6] = [
        (
            "/flows/{id}/enrollments/{enrollment_id}",
            "an email",
            EMAIL,
            "send_failed",
        ),
        (
            "/flows/{id}/enrollments/{enrollment_id}",
            "a name trait",
            NAME,
            "send_failed",
        ),
        ("/flows/{id}/enrollments", "an email", EMAIL, "send_failed"),
        ("/forwarding/dead-letters", "a payload", PAYLOAD, "http_4xx"),
        ("/forwarding/dead-letters", "an email", EMAIL, "http_4xx"),
        (
            "/imports/{id}/rejects",
            "a rejected cell",
            REJECTED_CELL,
            "invalid_identifier",
        ),
    ];

    /// Surfaces that hold personal data in a column the response must leave out; a leak is an
    /// entry here and the issue it waits on. Empty: the walk found none that was not fixed.
    /// The one personal value no response ever shows: an unplanned event is rejected and the list
    /// keeps its name, not its properties.
    const NEVER_SHOWN: &str = "an unplanned event property";

    const PENDING: [(&str, &str); 0] = [];

    /// The smallest valid body of each read-style `POST`, by the end of its path.
    fn read_post_body(path: &str) -> Option<Value> {
        let range = || {
            let now = time::OffsetDateTime::now_utc();
            let format = time::format_description::well_known::Rfc3339;
            json!({
                "from": (now - time::Duration::days(3)).format(&format).unwrap(),
                "to": (now + time::Duration::hours(1)).format(&format).unwrap(),
            })
        };
        let explorer = |extra: Value| {
            let mut body = range();
            body["name"] = json!("order");
            if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
                body.extend(extra.clone());
            }
            Some(body)
        };
        match path {
            p if p.ends_with("/events/explore/count") => explorer(json!({"bucket": "day"})),
            p if p.ends_with("/events/explore/breakdown") => explorer(json!({"by": "plan"})),
            p if p.ends_with("/events/explore/list") || p.ends_with("/events/explore/export") => {
                explorer(json!({}))
            }
            p if p.ends_with("/segments/preview") => {
                Some(json!({"definition": {"v": 1, "trait": "plan", "op": "eq", "value": "pro"}}))
            }
            p if p.ends_with("/people/search") => Some(json!({"query": "zq"})),
            _ => None,
        }
    }

    /// Whether the walk reads this row: any `GET`, and a `POST` that searches, previews, explains,
    /// tests or explores.
    fn reads(row: &Route) -> bool {
        match row.method {
            Method::Get => true,
            Method::Post => {
                [
                    "/search",
                    "/preview",
                    "/explain",
                    "/check",
                    "/evaluate",
                    "/events/explore/",
                ]
                .iter()
                .any(|part| row.path.contains(part))
                    || row.path.ends_with("/flows/{id}/test")
            }
            _ => false,
        }
    }

    /// The ids of the seeded rows, by the kind of path they stand in.
    struct Seeded {
        person: Uuid,
        conflict: Uuid,
        list: Uuid,
        segment: Uuid,
        flow: Uuid,
        enrollment: Uuid,
        import: Uuid,
        rule: Uuid,
    }

    impl Seeded {
        /// `path` with the seeded row for each placeholder, a ghost for the rest.
        fn path(&self, path: &str, workspace: WorkspaceId) -> String {
            let mut out = String::new();
            let mut rest = path;
            while let Some(start) = rest.find('{') {
                out.push_str(&rest[..start]);
                let end = rest[start..].find('}').unwrap() + start;
                let name = &rest[start + 1..end];
                let before = out.rsplit('/').nth(1).unwrap_or_default().to_owned();
                out.push_str(&match (name, before.as_str()) {
                    ("ws", _) => workspace.as_uuid().to_string(),
                    ("id", "people") => self.person.to_string(),
                    ("id", "lists") => self.list.to_string(),
                    ("id", "segments") => self.segment.to_string(),
                    ("id", "flows") => self.flow.to_string(),
                    ("id", "imports") => self.import.to_string(),
                    ("id", "identity-conflicts") => self.conflict.to_string(),
                    ("id", "rules") => self.rule.to_string(),
                    ("enrollment_id", _) => self.enrollment.to_string(),
                    ("trace_id", _) => self.enrollment.simple().to_string(),
                    ("n", _) => "1".to_owned(),
                    ("name", "event-schemas") => "order".to_owned(),
                    ("key", "traits") => "first_name".to_owned(),
                    ("id" | "user_id" | "run_id" | "person_id", _) => Uuid::now_v7().to_string(),
                    _ => GHOST.to_owned(),
                });
                rest = &rest[end + 1..];
            }
            out.push_str(rest);
            out
        }
    }

    async fn api(world: &World, method: &str, tail: &str, body: Option<Value>, who: Who) -> Reply {
        let body = body.map(|body| ("application/json".to_owned(), body.to_string().into_bytes()));
        send(
            &world.router,
            method,
            &world.url(tail),
            body,
            world.auth(who),
        )
        .await
    }

    async fn created(world: &World, tail: &str, body: Value) -> Value {
        let reply = api(world, "POST", tail, Some(body), Who::Owner).await;
        assert!(
            reply.status == 201 || reply.status == 200,
            "{tail}: {}",
            reply.text
        );
        reply.body
    }

    fn profile(
        key: &str,
        identifiers: &[(&str, &str)],
        traits: Value,
    ) -> zirv_flow_store::ProfileRecord {
        zirv_flow_store::ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: traits.as_object().cloned().unwrap_or_default(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("seed")),
        }
    }

    /// Fills the workspace with the seeded values on every surface that could echo one.
    async fn seed(world: &World) -> Seeded {
        let pool = &world.pool;
        let ws = world.ws;
        for (key, pii) in [("first_name", true), ("plan", false)] {
            zirv_flow_store::create_trait_definition(
                pool,
                ws,
                &zirv_flow_store::NewTraitDefinition {
                    key: key.into(),
                    trait_type: zirv_flow_core::people::TraitType::String,
                    precedence: zirv_flow_core::people::Precedence::Latest,
                    source_order: Vec::new(),
                    pii,
                    channel_address: false,
                    inferred: false,
                    description: String::new(),
                },
            )
            .await
            .unwrap();
        }
        // Ada, and a second record that collides with her email: an identity conflict.
        let outcomes = zirv_flow_store::apply_profile_batch(
            pool,
            ws,
            &[
                profile(
                    "1",
                    &[
                        ("email", EMAIL),
                        ("phone", &format!("+45{PHONE_DIGITS}")),
                        ("external:crm", EXTERNAL),
                    ],
                    json!({"first_name": NAME, "plan": "pro"}),
                ),
                profile("2", &[("external:crm", "bob-ext")], json!({})),
                profile(
                    "3",
                    &[("external:crm", "bob-ext"), ("email", EMAIL)],
                    json!({}),
                ),
            ],
        )
        .await
        .unwrap();
        let person = match &outcomes[0] {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. } => *person_id,
            other => panic!("not applied: {other:?}"),
        };
        let conflict: Uuid = sqlx::query_scalar(
            "SELECT id FROM identity_conflicts WHERE workspace_id = $1 AND resolved_at IS NULL",
        )
        .bind(ws.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap();

        // Events: a planned one with a personal property, and an unplanned one that is rejected.
        created(
            world,
            "/event-schemas",
            json!({"name": "order", "mode": "warn", "properties": [
                {"name": "total", "type": "number", "required": true},
                {"name": "coupon", "type": "string", "pii": true},
                {"name": "plan", "type": "string"}
            ]}),
        )
        .await;
        created(world, "/event-schemas/order/versions", json!({})).await;
        let policy = api(
            world,
            "PUT",
            "/event-policy",
            Some(json!({"unplanned": "reject", "future_skew_seconds": 300,
                        "lateness_seconds": 86400, "redaction": {}})),
            Who::Owner,
        )
        .await;
        assert_eq!(policy.status, 200, "{}", policy.text);
        let now = time::OffsetDateTime::now_utc();
        let event = |name: &str, properties: Value| zirv_flow_store::EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), EMAIL.into())],
            name: name.into(),
            properties: properties.as_object().cloned().unwrap(),
            timestamp: Some(now - time::Duration::minutes(5)),
            received_at: now,
            message_id: None,
            trace: zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("seed")),
        };
        let redaction = HmacRoot::bootstrap_from(&MasterKey::generate())
            .hmac_key(zirv_flow_core::HmacPurpose::Redaction);
        zirv_flow_store::apply_event_batch(
            pool,
            ws,
            &redaction,
            &[
                event(
                    "order",
                    json!({"total": 10, "plan": "pro", "coupon": PROPERTY}),
                ),
                event("unplanned_thing", json!({"card": UNPLANNED})),
            ],
        )
        .await
        .unwrap();

        // A list, a segment, consent with evidence.
        let list = created(world, "/lists", json!({"name": LIST_NAME})).await["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        let added = api(
            world,
            "POST",
            &format!("/lists/{list}/members"),
            Some(json!({"person_ids": [person]})),
            Who::Owner,
        )
        .await;
        assert!(added.status < 300, "{}", added.text);
        let segment = created(
            world,
            "/segments",
            json!({"name": "Pros", "definition": {"v": 1, "trait": "plan", "op": "eq", "value": "pro"}}),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        created(
            world,
            "/topics",
            json!({"key": "newsletter", "name": "Newsletter", "channel": "email", "policy": "opt_out"}),
        )
        .await;
        let consent = api(
            world,
            "PUT",
            &format!("/people/{person}/consent"),
            Some(
                json!({"channel": "email", "topic_key": "newsletter", "status": "unsubscribed",
                        "evidence": {"text": EVIDENCE}}),
            ),
            Who::Owner,
        )
        .await;
        assert!(consent.status < 300, "{}", consent.text);

        // A flow with an enrollment of Ada, a step and a resume input holding her values.
        let flow = created(
            world,
            "/flows",
            json!({"name": "Welcome",
                   "graph": {"v": 1, "nodes": [
                       {"id": "wait", "type": "wait.duration", "seconds": 60},
                       {"id": "end", "type": "exit"}
                   ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]},
                   "settings": {"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}}),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        created(world, &format!("/flows/{flow}/publish"), json!({})).await;
        let enrollment = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id, error, failed_node, failure_placed, resume_input) \
             VALUES ($1, $2, $3, 1, $4, 'failed', 'wait', $1, $1, 'api', 't', 'send_failed', \
                     'wait', true, $5)",
        )
        .bind(enrollment)
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(person)
        .bind(json!({"input": "start", "email": EMAIL, "name": NAME}))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome, error) \
             VALUES ($1, $2, $3, 'wait', 'failed', 'send_failed')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(enrollment)
        .execute(pool)
        .await
        .unwrap();

        // A forwarding rule and a dead letter whose payload carries her values.
        let destination = zirv_flow_store::create_destination(
            pool,
            ws,
            &zirv_flow_store::NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "Mail",
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
        let rule = created(
            world,
            "/forwarding/rules",
            json!({"destination_id": destination, "name": "Orders", "event_name": "order",
                   "mapping": {"id": {"from": "event.id"}}}),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        sqlx::query(
            "INSERT INTO forwarding_outbox (id, workspace_id, destination_id, rule_id, event_id, \
                person_id, idempotency_key, payload, state, attempts, last_error, updated_at) \
             VALUES ($1, $2, $3, $4, gen_random_uuid(), $5, 'k', $6, 'dead', 3, 'http_4xx', now())",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(rule)
        .bind(person)
        .bind(json!({"email": EMAIL, "secret": PAYLOAD}))
        .execute(pool)
        .await
        .unwrap();

        // An import with its sample rows and rejected cells, and a test-send log row.
        let upload = send(
            &world.router,
            "POST",
            &world.url("/imports?name=Customers&file_name=c.csv"),
            Some((
                "text/csv".into(),
                format!("Email,Plan\n{EMAIL},pro\nnot-an-email,free\n").into_bytes(),
            )),
            world.auth(Who::Owner),
        )
        .await;
        assert_eq!(upload.status, 201, "{}", upload.text);
        let import = upload.body["id"].as_str().unwrap().parse::<Uuid>().unwrap();
        let mapping = api(
            world,
            "PUT",
            &format!("/imports/{import}/mapping"),
            Some(json!({"columns": [
                {"column": "Email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}, {"op": "lower"}]}
            ]})),
            Who::Owner,
        )
        .await;
        assert_eq!(mapping.status, 200, "{}", mapping.text);
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        zirv_flow_store::start_import_run(&mut tx, ws, import)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let rejected = [zirv_flow_store::RejectedRow {
            row_number: 3,
            reasons: vec![zirv_flow_store::RejectReason {
                column: "Email".into(),
                reason: "invalid_identifier",
            }],
            raw: vec![REJECTED_CELL.into(), "free".into()],
        }];
        zirv_flow_store::commit_import_batch(
            pool,
            &zirv_flow_store::ImportBatch {
                import_id: import,
                workspace: ws,
                expected_done: 0,
                records: &[],
                details: &[],
                rejected: &rejected,
                trace: &zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("seed")),
            },
            false,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO test_sends (id, workspace_id, template_id, destination_id, \
                sender_identity_id, recipient_hash, render_context, outcome, reason, completed_at) \
             VALUES ($1, $2, gen_random_uuid(), gen_random_uuid(), gen_random_uuid(), \
                     decode(repeat('ab', 32), 'hex'), 'person', 'failed', 'destination_error', now())",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .execute(pool)
        .await
        .unwrap();

        Seeded {
            person,
            conflict,
            list,
            segment,
            flow,
            enrollment,
            import,
            rule,
        }
    }

    /// The needles of `PERSONAL` that `text` holds, whatever the case.
    fn found(text: &str) -> Vec<&'static str> {
        let text = text.to_lowercase();
        PERSONAL
            .iter()
            .filter(|(_, needle)| text.contains(&needle.to_lowercase()))
            .map(|(what, _)| *what)
            .collect()
    }

    /// One read of a seeded workspace: the row, who read it and what came back.
    struct Read {
        row: Route,
        path: String,
        who: Who,
        status: u16,
        text: String,
    }

    /// Calls every reading row as `who`, with the seeded rows in the paths.
    async fn read_all(world: &World, seeded: &Seeded, who: Who) -> Vec<Read> {
        let operations = Operations::new();
        let mut done = Vec::new();
        for row in ROUTES
            .iter()
            .filter(|row| row.path.contains("{ws}") && reads(row))
        {
            let (sample, query) = operations.request(row);
            let path = seeded.path(row.path, world.ws);
            let uri = format!("{path}{query}");
            let method = format!("{:?}", row.method).to_uppercase();
            let body = match row.method {
                Method::Get => None,
                _ => Some(match read_post_body(row.path) {
                    Some(body) => ("application/json".to_owned(), body.to_string().into_bytes()),
                    None => sample
                        .clone()
                        .unwrap_or_else(|| ("application/json".into(), b"{}".to_vec())),
                }),
            };
            let reply = send(&world.router, &method, &uri, body, world.auth(who)).await;
            done.push(Read {
                row: *row,
                path: uri,
                who,
                status: reply.status,
                text: reply.text,
            });
        }
        done
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_seeded_personal_value_reaches_a_reader_without_the_grant(pool: PgPool) {
        let world = world(&pool).await;
        let seeded = seed(&world).await;
        let started = Instant::now();

        let mut leaks = Vec::new();
        let mut visited = 0;
        for who in [Who::Viewer, Who::KeyRead] {
            let reads = read_all(&world, &seeded, who).await;
            visited += reads.len();
            for read in &reads {
                for what in found(&read.text) {
                    let pending = PENDING.iter().any(|(row, _)| *row == row_name(&read.row));
                    if !pending {
                        leaks.push(format!("{} as {:?} shows {what}", read.path, read.who));
                    }
                }
            }
            // The walk reads real data: the list name, which is no secret, is there.
            for suffix in REVEALS {
                let read = reads
                    .iter()
                    .find(|read| read.row.path.ends_with(suffix))
                    .unwrap();
                assert_eq!(read.status, 200, "{who:?} {}: {}", read.path, read.text);
            }
            let lists = reads
                .iter()
                .find(|read| read.row.path == "/api/v1/workspaces/{ws}/lists")
                .unwrap();
            assert!(lists.text.contains(LIST_NAME), "{who:?}: {}", lists.text);
            assert_eq!(lists.status, 200);
            // Nothing the walk hit was a server error, and the data surfaces answered.
            let broken: Vec<String> = reads
                .iter()
                .filter(|read| read.status >= 500 && !read.text.contains("render_unavailable"))
                .map(|read| format!("{} {}", read.path, read.status))
                .collect();
            assert!(broken.is_empty(), "{who:?}: {broken:?}");
        }
        eprintln!(
            "pii walk (withheld): {visited} reads in {:?}",
            started.elapsed()
        );

        assert!(
            leaks.is_empty(),
            "{} leaks:\n{}",
            leaks.len(),
            leaks.join("\n")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_seeded_values_appear_for_a_reader_with_the_grant(pool: PgPool) {
        let world = world(&pool).await;
        let seeded = seed(&world).await;
        let started = Instant::now();

        let mut seen: Vec<&'static str> = Vec::new();
        let mut rejects_listed = false;
        for who in [Who::ViewerWithPii, Who::KeyReadAndPii, Who::Admin] {
            let reads = read_all(&world, &seeded, who).await;
            for suffix in REVEALS {
                let read = reads
                    .iter()
                    .find(|read| read.row.path.ends_with(suffix))
                    .unwrap_or_else(|| panic!("no row ends with {suffix}"));
                assert_eq!(read.status, 200, "{who:?} {}: {}", read.path, read.text);
                assert!(
                    !found(&read.text).is_empty(),
                    "{who:?} sees no seeded value on {}: {}",
                    read.path,
                    read.text
                );
            }
            for read in &reads {
                seen.extend(found(&read.text));
            }
            if who == Who::Admin {
                for (suffix, what, needle, marker) in HIDDEN_ON {
                    let read = reads
                        .iter()
                        .find(|read| read.row.path.ends_with(suffix))
                        .unwrap();
                    assert_eq!(read.status, 200, "{}: {}", read.path, read.text);
                    assert!(
                        read.text.contains(marker),
                        "{} holds no seeded row",
                        read.path
                    );
                    assert!(
                        !read.text.to_lowercase().contains(&needle.to_lowercase()),
                        "{} shows {what}",
                        read.path
                    );
                }
            }
            rejects_listed |= reads.iter().any(|read| {
                read.row.path.ends_with("/ingest-rejects") && read.text.contains("unplanned_thing")
            });
        }
        eprintln!("pii walk (granted): in {:?}", started.elapsed());

        // The rejects list never shows an event's properties, to anyone: its value stays hidden
        // even from the owner, which the walk records as a value that must never appear.
        for (what, _) in PERSONAL {
            assert_eq!(
                seen.contains(&what),
                what != NEVER_SHOWN,
                "{what} with the grant"
            );
        }
        assert!(
            rejects_listed,
            "the rejected event is not on the rejects list"
        );
    }
}
