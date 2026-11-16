//! Ingest rejects: what was refused and why, with a masked sample. The stored payload is
//! for replay and is never part of a response.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use hashbrown::HashMap;
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::events::{SchemaProperty, mask_sample};
use zirv_flow_store::{IngestReject, RejectCursor, RejectFilter};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct ReasonBody {
    /// The property that failed; never its value.
    property: String,
    /// `missing_required`, `wrong_type`, `not_in_enum` or `unplanned_property`; a `traits_dropped`
    /// reject also carries `invalid_key`, `archived_key`, `too_long` and `not_inferable`.
    reason: String,
}

impl ReasonBody {
    pub(crate) fn new(property: String, reason: &str) -> Self {
        Self {
            property,
            reason: reason.to_owned(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IngestRejectBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    received_at: OffsetDateTime,
    source: String,
    /// `event` today.
    kind: String,
    /// Null when the name itself was invalid.
    name: Option<String>,
    /// `validation_failed`, `unplanned_event`, `no_identifier` or another static code.
    reason: String,
    reasons: Vec<ReasonBody>,
    /// `{properties, masked}`: the values of PII-flagged properties, of unplanned properties
    /// and of every property of an unplanned event are null and listed in `masked`. An event
    /// reject is masked again against the schema published when it is read, so a property
    /// flagged PII after the reject was stored is null here too.
    #[schema(value_type = Object)]
    sample: serde_json::Value,
    person_id: Option<Uuid>,
    message_id: Option<String>,
    /// The trace of the ingest call: 32 lowercase hex characters.
    trace_id: String,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    replayed_at: Option<OffsetDateTime>,
}

impl IngestRejectBody {
    /// The reject as it is shown. `published` holds the properties of the schema each event
    /// name has now; a name without one has every value masked.
    fn shown(reject: IngestReject, published: &HashMap<String, Vec<SchemaProperty>>) -> Self {
        let mut sample = reject.sample;
        if reject.kind == "event" {
            let schema = reject
                .name
                .as_ref()
                .and_then(|name| published.get(name))
                .map(Vec::as_slice);
            let masked = sample
                .get("properties")
                .and_then(Value::as_object)
                .map(|properties| mask_sample(properties, schema));
            if let (Some(masked), Some(sample)) = (masked, sample.as_object_mut()) {
                sample.insert("properties".to_owned(), Value::Object(masked.properties));
                sample.insert("masked".to_owned(), json!(masked.masked));
            }
        }
        Self {
            id: reject.id,
            received_at: reject.received_at,
            source: reject.source,
            kind: reject.kind,
            name: reject.name,
            reason: reject.reason,
            reasons: reject
                .reasons
                .into_iter()
                .map(|reason| ReasonBody {
                    property: reason.property,
                    reason: reason.reason.as_str().to_owned(),
                })
                .collect(),
            sample,
            person_id: reject.person_id,
            message_id: reject.message_id,
            trace_id: zirv_flow_core::TraceId::from_uuid(reject.trace_id).to_string(),
            replayed_at: reject.replayed_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IngestRejectsBody {
    /// Newest first.
    rejects: Vec<IngestRejectBody>,
    /// Pass as `cursor` for the next page; null when there is none.
    next_cursor: Option<String>,
}

/// The `name` and `reason` filters of the query string.
fn parse_filters(query: &str) -> (Option<String>, Option<String>) {
    let (mut name, mut reason) = (None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "name" => name = Some(value.into_owned()),
            "reason" => reason = Some(value.into_owned()),
            _ => {}
        }
    }
    (name, reason)
}

/// Ingest rejects, newest first, with their masked samples and reasons. The payload is never
/// returned.
#[utoipa::path(
    get,
    operation_id = "list_ingest_rejects",
    path = "/api/v1/workspaces/{ws}/ingest-rejects",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Rejects per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page"),
        ("name" = Option<String>, Query, description = "Only rejects of this event name"),
        ("reason" = Option<String>, Query, description = "Only rejects with this reason code")
    ),
    responses(
        (status = 200, description = "A page of rejects", body = IngestRejectsBody),
        (status = 400, description = "limit is out of range or cursor is malformed; the removed before and after answer paging_parameter_renamed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<IngestRejectsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let query = query.unwrap_or_default();
    let (limit, before) = parse_page(&query)?;
    let (name, reason) = parse_filters(&query);
    let filter = RejectFilter {
        name: name.as_deref(),
        reason: reason.as_deref(),
    };
    let cursor = before.map(|(received_at, id)| RejectCursor { received_at, id });
    let (rejects, more) =
        zirv_flow_store::list_ingest_rejects(&state.pool, workspace.id, filter, cursor, limit)
            .await?;
    let next_cursor = match rejects.last() {
        Some(last) if more => Some(encode_cursor((last.received_at, last.id))),
        _ => None,
    };
    let names: Vec<String> = rejects
        .iter()
        .filter(|reject| reject.kind == "event")
        .filter_map(|reject| reject.name.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let published = if names.is_empty() {
        HashMap::new()
    } else {
        zirv_flow_store::published_event_properties(&state.pool, workspace.id, &names).await?
    };
    Ok(Json(IngestRejectsBody {
        rejects: rejects
            .into_iter()
            .map(|reject| IngestRejectBody::shown(reject, &published))
            .collect(),
        next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::events::{Reason, ReasonCode};

    use super::*;

    #[test]
    fn filters_are_read_from_the_query_string() {
        assert_eq!(parse_filters(""), (None, None));
        assert_eq!(
            parse_filters("limit=5&name=Order%20Completed&reason=validation_failed"),
            (
                Some("Order Completed".to_owned()),
                Some("validation_failed".to_owned())
            )
        );
    }

    #[test]
    fn a_reject_is_shown_with_its_sample_and_no_payload() {
        let reject = IngestReject {
            id: Uuid::nil(),
            received_at: OffsetDateTime::UNIX_EPOCH,
            source: "web".into(),
            kind: "event".into(),
            name: Some("order".into()),
            reason: "validation_failed".into(),
            reasons: vec![Reason {
                property: "total".into(),
                reason: ReasonCode::WrongType,
            }],
            sample: json!({"properties": {"total": null}, "masked": ["total"]}),
            person_id: None,
            message_id: Some("m-1".into()),
            trace_id: Uuid::nil(),
            replayed_at: None,
        };

        let shown = serde_json::to_value(IngestRejectBody::shown(reject, &HashMap::new())).unwrap();

        assert_eq!(
            shown["reasons"],
            json!([{"property": "total", "reason": "wrong_type"}])
        );
        assert_eq!(shown["received_at"], "1970-01-01T00:00:00Z");
        assert_eq!(shown["trace_id"].as_str().unwrap().len(), 32);
        assert!(shown.get("payload").is_none());
    }

    /// A workspace with the event `signup` published with `plan` non-PII, a reader key, and
    /// one `event` reject holding a clear `plan`.
    async fn rejected_signup(
        pool: &sqlx::PgPool,
    ) -> (
        AppState,
        zirv_flow_core::WorkspaceId,
        zirv_flow_store::ApiKey,
    ) {
        use zirv_flow_core::events::{EventMode, PropertyType, SchemaProperty};
        use zirv_flow_core::{Scope, Slug, WorkspaceName};

        zirv_flow_store::migrate(pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let plan = |pii: bool| SchemaProperty {
            name: "plan".into(),
            property_type: PropertyType::String,
            required: false,
            enum_values: None,
            description: String::new(),
            pii,
        };
        zirv_flow_store::create_event_schema(
            pool,
            ws,
            &zirv_flow_store::NewEventSchema {
                name: "signup".into(),
                mode: EventMode::Warn,
                description: String::new(),
                properties: vec![plan(false)],
            },
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        zirv_flow_store::publish_schema_draft(&mut connection, ws, "signup", None)
            .await
            .unwrap();
        for (kind, name) in [
            ("event", Some("signup")),
            ("record", Some("signup")),
            ("event", Some("gone")),
        ] {
            sqlx::query(
                "INSERT INTO ingest_rejects \
                    (id, workspace_id, received_at, source, kind, name, reason, reasons, sample, \
                     payload, trace_id) \
                 VALUES (gen_random_uuid(), $1, now(), 'web', $2, $3, 'validation_failed', '[]', \
                         $4, '{}', gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(kind)
            .bind(name)
            .bind(json!({"properties": {"plan": "x@example.com"}, "masked": []}))
            .execute(pool)
            .await
            .unwrap();
        }
        let key = zirv_flow_store::ApiKey {
            id: zirv_flow_core::ApiKeyId::new(),
            workspace_id: ws,
            name: "reader".into(),
            scopes: vec![Scope::Read],
            created_by: None,
            created_at: OffsetDateTime::now_utc(),
            last_used_at: None,
            revoked_at: None,
        };
        let state = AppState::new(pool.clone(), std::sync::Arc::new(String::new));
        (state, ws, key)
    }

    async fn listed(
        state: &AppState,
        ws: zirv_flow_core::WorkspaceId,
        key: &zirv_flow_store::ApiKey,
    ) -> Vec<serde_json::Value> {
        let Json(body) = list(
            State(state.clone()),
            Principal::Key(crate::auth::KeyPrincipal { key: key.clone() }),
            Path(ws.as_uuid().to_string()),
            RawQuery(None),
        )
        .await
        .unwrap();
        serde_json::to_value(body).unwrap()["rejects"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn sample_of<'a>(
        rejects: &'a [serde_json::Value],
        kind: &str,
        name: &str,
    ) -> &'a serde_json::Value {
        &rejects
            .iter()
            .find(|reject| reject["kind"] == kind && reject["name"] == name)
            .unwrap()["sample"]
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reject_sample_is_masked_against_the_schema_flags_in_force_at_read_time(
        pool: sqlx::PgPool,
    ) {
        let (state, ws, key) = rejected_signup(&pool).await;

        let before = listed(&state, ws, &key).await;
        assert_eq!(
            sample_of(&before, "event", "signup")["properties"]["plan"],
            "x@example.com",
            "a property still non-PII and correctly typed stays visible"
        );

        // The property is flagged PII after the reject was stored.
        zirv_flow_store::save_schema_draft(
            &pool,
            ws,
            "signup",
            &[zirv_flow_core::events::SchemaProperty {
                name: "plan".into(),
                property_type: zirv_flow_core::events::PropertyType::String,
                required: false,
                enum_values: None,
                description: String::new(),
                pii: true,
            }],
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        zirv_flow_store::publish_schema_draft(&mut connection, ws, "signup", None)
            .await
            .unwrap();
        drop(connection);
        let after = listed(&state, ws, &key).await;

        let shown = sample_of(&after, "event", "signup");
        assert_eq!(
            shown["properties"]["plan"],
            serde_json::Value::Null,
            "sample as listed: {shown}"
        );
        assert_eq!(shown["masked"], json!(["plan"]));
        // An event with no published schema shows every value as null; a record reject is as
        // stored.
        assert_eq!(
            sample_of(&after, "event", "gone")["properties"]["plan"],
            serde_json::Value::Null
        );
        assert_eq!(
            sample_of(&after, "record", "signup")["properties"]["plan"],
            "x@example.com"
        );
    }
}
