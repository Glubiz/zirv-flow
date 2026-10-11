//! The event explorer (architecture section 12.3): counts over time, a breakdown of one
//! property, and rows, over the events of one workspace in a time range.
//!
//! Every query carries `workspace_id` and the time bounds, so only the partitions of the
//! range are read, and runs in a read-only transaction under the caller's statement timeout
//! (`begin_bounded_for`): a slow scan is cancelled and surfaces as a timeout, never runs
//! unbounded. The count, the breakdown, the list and the export share one `WHERE`, so they
//! agree on what matches.
//!
//! A viewer without access to values may filter and break down only by properties the
//! published schema declares non-PII, plus `eq` and `exists` on the rest, as the people
//! search does for PII traits. Masking of the rows themselves is the caller's, from the
//! schema each row carries.
//!
//! Nothing here logs or returns a name, an identifier or a value in an error.

mod filter;

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use sqlx::prelude::FromRow;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::events::{PropertyType, SchemaProperty, validate_event_name};

pub use filter::{ExploreFilter, FilterOp};
use filter::{Planned, Test, check_key, plan};

use crate::StoreError;
use crate::bounded::begin_bounded_for;
use crate::people::person_group;

/// The widest time range a query may cover.
pub const MAX_RANGE_DAYS: i64 = 400;

/// The values a breakdown names; the rest is counted as `other`.
pub const BREAKDOWN_VALUES: i64 = 50;

/// The most filters in one query.
pub const MAX_FILTERS: usize = 20;

/// How long a query may run and how many rows it may return.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExploreLimits {
    pub timeout: Duration,
    pub row_limit: u32,
}

/// The width of a count bucket, in UTC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bucket {
    Hour,
    Day,
    Week,
}

impl Bucket {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
        }
    }
}

impl FromStr for Bucket {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text {
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            _ => Err(()),
        }
    }
}

/// What a query looks at: events in `[from, to)`, optionally of one name and one person
/// (and the people merged into them), that pass every filter.
#[derive(Clone, Debug)]
pub struct ExploreScope<'a> {
    pub name: Option<&'a str>,
    pub filters: &'a [ExploreFilter],
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
    pub person: Option<Uuid>,
}

/// Why a query was refused or failed. `Invalid` names the field and the rule, never a value.
#[derive(Debug)]
pub enum ExploreError {
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    /// The property is PII-flagged or undeclared and the caller may not see values.
    PiiPropertyNotExplorable,
    /// A timeout is [`StoreError::is_timeout`].
    Store(StoreError),
}

impl From<StoreError> for ExploreError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for ExploreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field, reason } => write!(formatter, "{field}: {reason}"),
            Self::PiiPropertyNotExplorable => formatter.write_str(
                "property: only eq and exists apply to a PII or undeclared property \
                 without access to values, and it cannot be broken down",
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExploreError {}

fn invalid(field: &'static str, reason: &'static str) -> ExploreError {
    ExploreError::Invalid { field, reason }
}

fn bounded(error: sqlx::Error) -> ExploreError {
    ExploreError::Store(StoreError::bounded(error))
}

/// Counts per bucket, oldest first; buckets without events are left out.
#[derive(Clone, Debug, PartialEq)]
pub struct CountResult {
    pub buckets: Vec<(OffsetDateTime, i64)>,
    /// More buckets matched than the row limit; the newest were left out.
    pub truncated: bool,
}

/// The most common values of a property.
#[derive(Clone, PartialEq)]
pub struct Breakdown {
    /// Up to [`BREAKDOWN_VALUES`] values with their counts, most common first. A value is
    /// the property's text; a list or an object is its JSON.
    pub values: Vec<(String, i64)>,
    /// Matching events that have the property with a value not in `values`.
    pub other: i64,
    /// Matching events without the property.
    pub missing: i64,
}

// The values are personal data.
impl fmt::Debug for Breakdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Breakdown")
            .field("values", &self.values.len())
            .field("other", &self.other)
            .field("missing", &self.missing)
            .finish()
    }
}

/// Where a page of rows ended: the next page starts after this event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RowCursor {
    pub at: OffsetDateTime,
    pub id: Uuid,
}

/// An event as the explorer shows it. Its properties are as stored, redacted at ingest; the
/// viewer's masking is applied by the caller from `schema`.
#[derive(Clone, PartialEq)]
pub struct ExploreRow {
    pub id: Uuid,
    pub occurred_at: OffsetDateTime,
    pub name: String,
    pub source: String,
    pub person_id: Uuid,
    pub validation_state: String,
    pub flags: Vec<String>,
    pub trace_id: Uuid,
    pub properties: Map<String, Value>,
    /// The published schema version the event was validated against; absent for an event
    /// without one.
    pub schema: Option<Arc<Vec<SchemaProperty>>>,
}

// Event properties are personal data.
impl fmt::Debug for ExploreRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExploreRow")
            .field("id", &self.id)
            .field("occurred_at", &self.occurred_at)
            .finish_non_exhaustive()
    }
}

/// A page of rows, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct RowsPage {
    pub rows: Vec<ExploreRow>,
    /// Pass as `after` for the next page; absent on the last.
    pub next: Option<RowCursor>,
}

/// A scope after its checks: filters typed, the person group resolved.
struct Prepared {
    planned: Vec<Planned>,
    /// The person and the people merged with them; absent without a person filter.
    people: Option<Vec<Uuid>>,
    /// The properties of the published schema of the name, when there is a name and one.
    schema: Option<Vec<SchemaProperty>>,
}

impl Prepared {
    /// The declared type of `key`; `None` when undeclared or there is no published schema.
    fn declared(&self, key: &str) -> Option<&SchemaProperty> {
        self.schema
            .as_ref()
            .and_then(|schema| schema.iter().find(|property| property.name == key))
    }
}

async fn prepare(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    scope: &ExploreScope<'_>,
    reveal: bool,
) -> Result<Prepared, ExploreError> {
    if scope.from >= scope.to {
        return Err(invalid("from", "must be before to"));
    }
    if scope.to - scope.from > time::Duration::days(MAX_RANGE_DAYS) {
        return Err(invalid("to", "must be at most 400 days after from"));
    }
    if let Some(name) = scope.name
        && validate_event_name(name).is_err()
    {
        return Err(invalid("name", "is not a valid event name"));
    }
    if scope.filters.len() > MAX_FILTERS {
        return Err(invalid("filters", "must hold at most 20 filters"));
    }
    let schema: Option<Vec<SchemaProperty>> = match scope.name {
        Some(name) => sqlx::query_scalar::<_, Json<Vec<SchemaProperty>>>(
            "SELECT v.properties FROM event_schemas s \
             JOIN event_schema_versions v \
               ON v.workspace_id = s.workspace_id AND v.name = s.name \
              AND v.version = s.current_version \
             WHERE s.workspace_id = $1 AND s.name = $2",
        )
        .bind(workspace.as_uuid())
        .bind(name)
        .fetch_optional(&mut *connection)
        .await
        .map_err(bounded)?
        .map(|properties| properties.0),
        None => None,
    };
    let mut prepared = Prepared {
        planned: Vec::new(),
        people: None,
        schema,
    };
    for filter in scope.filters {
        let declared = prepared.declared(&filter.key);
        let kind = declared.map(|property| property.property_type);
        let plain = declared.is_some_and(|property| !property.pii);
        if !reveal && !plain && !filter.op.is_exact() {
            check_key("filters.key", &filter.key).map_err(|(f, r)| invalid(f, r))?;
            return Err(ExploreError::PiiPropertyNotExplorable);
        }
        let planned = plan(filter, kind).map_err(|(field, reason)| invalid(field, reason))?;
        prepared.planned.push(planned);
    }
    if let Some(person) = scope.person {
        // A person that does not exist matches no event.
        let group = person_group(connection, workspace, person)
            .await
            .map_err(ExploreError::Store)?;
        prepared.people = Some(group.unwrap_or_default());
    }
    Ok(prepared)
}

type Builder = QueryBuilder<Postgres>;

/// `(e.properties -> $key)`
fn push_json(qb: &mut Builder, key: &str) {
    qb.push("(e.properties -> ")
        .push_bind(key.to_owned())
        .push("::text)");
}

/// `(e.properties ->> $key)`
fn push_text(qb: &mut Builder, key: &str) {
    qb.push("(e.properties ->> ")
        .push_bind(key.to_owned())
        .push("::text)");
}

fn push_test(qb: &mut Builder, planned: &Planned) {
    let key = planned.key.as_str();
    if planned.negate {
        qb.push("((");
    }
    match &planned.test {
        Test::Exists(present) => {
            qb.push("COALESCE(jsonb_typeof(");
            push_json(qb, key);
            qb.push("), 'null') ")
                .push(if *present { "<>" } else { "=" })
                .push(" 'null'");
        }
        Test::Text(compare, value) => {
            push_text(qb, key);
            if *compare != filter::Compare::Eq {
                qb.push(" COLLATE \"C\"");
            }
            qb.push(" ")
                .push(compare.sql())
                .push(" ")
                .push_bind(value.clone())
                .push("::text");
        }
        Test::TextIn(values) => {
            push_text(qb, key);
            qb.push(" = ANY(")
                .push_bind(values.clone())
                .push("::text[])");
        }
        Test::Prefix(prefix) => {
            qb.push("starts_with(");
            push_text(qb, key);
            qb.push(", ").push_bind(prefix.clone()).push("::text)");
        }
        Test::Numeric(compare, value) => {
            qb.push("CASE WHEN jsonb_typeof(");
            push_json(qb, key);
            qb.push(") = 'number' THEN ");
            push_text(qb, key);
            qb.push("::numeric ")
                .push(compare.sql())
                .push(" ")
                .push_bind(value.clone())
                .push("::numeric END");
        }
        Test::NumericIn(values) => {
            qb.push("CASE WHEN jsonb_typeof(");
            push_json(qb, key);
            qb.push(") = 'number' THEN ");
            push_text(qb, key);
            qb.push("::numeric = ANY((")
                .push_bind(values.clone())
                .push("::text[])::numeric[]) END");
        }
        Test::Bool(flag) => {
            push_json(qb, key);
            qb.push(" = to_jsonb(").push_bind(*flag).push("::boolean)");
        }
        Test::Timestamp(compare, at) => {
            qb.push("zf_try_timestamptz(CASE WHEN jsonb_typeof(");
            push_json(qb, key);
            qb.push(") = 'string' THEN ");
            push_text(qb, key);
            qb.push(" END) ")
                .push(compare.sql())
                .push(" ")
                .push_bind(*at)
                .push("::timestamptz");
        }
        Test::Contains(item) => {
            push_json(qb, key);
            qb.push(" @> jsonb_build_array(")
                .push_bind(item.clone())
                .push("::text)");
        }
    }
    if planned.negate {
        qb.push(") IS NOT TRUE)");
    }
}

/// ` FROM events e WHERE ...`: the one `WHERE` every query shares.
fn push_scope(
    qb: &mut Builder,
    workspace: WorkspaceId,
    scope: &ExploreScope<'_>,
    prepared: &Prepared,
) {
    qb.push(" FROM events e WHERE e.workspace_id = ")
        .push_bind(workspace.as_uuid())
        .push(" AND e.occurred_at >= ")
        .push_bind(scope.from)
        .push(" AND e.occurred_at < ")
        .push_bind(scope.to);
    if let Some(name) = scope.name {
        qb.push(" AND e.name = ").push_bind(name.to_owned());
    }
    if let Some(people) = &prepared.people {
        qb.push(" AND e.person_id = ANY(")
            .push_bind(people.clone())
            .push(")");
    }
    for planned in &prepared.planned {
        qb.push(" AND ");
        push_test(qb, planned);
    }
}

fn row_limit(limits: ExploreLimits) -> i64 {
    i64::from(limits.row_limit.max(1))
}

/// Event counts per `bucket` over the scope.
pub async fn explore_count(
    pool: &PgPool,
    workspace: WorkspaceId,
    scope: &ExploreScope<'_>,
    bucket: Bucket,
    limits: ExploreLimits,
    reveal: bool,
) -> Result<CountResult, ExploreError> {
    let mut tx = begin_bounded_for(pool, limits.timeout).await?;
    let prepared = prepare(&mut tx, workspace, scope, reveal).await?;
    let limit = row_limit(limits);
    let mut qb: Builder = QueryBuilder::new(format!(
        "SELECT date_trunc('{}', e.occurred_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' AS bucket, \
                count(*) AS n",
        bucket.as_str()
    ));
    push_scope(&mut qb, workspace, scope, &prepared);
    qb.push(" GROUP BY 1 ORDER BY 1 LIMIT ")
        .push_bind(limit + 1);
    let mut buckets: Vec<(OffsetDateTime, i64)> = qb
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(bounded)?;
    drop(tx);
    let truncated = i64::try_from(buckets.len()).is_ok_and(|rows| rows > limit);
    if truncated {
        buckets.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    Ok(CountResult { buckets, truncated })
}

/// How many events match, for the export's audit entry and its row count.
pub async fn explore_total(
    pool: &PgPool,
    workspace: WorkspaceId,
    scope: &ExploreScope<'_>,
    limits: ExploreLimits,
    reveal: bool,
) -> Result<i64, ExploreError> {
    let mut tx = begin_bounded_for(pool, limits.timeout).await?;
    let prepared = prepare(&mut tx, workspace, scope, reveal).await?;
    let mut qb: Builder = QueryBuilder::new("SELECT count(*)");
    push_scope(&mut qb, workspace, scope, &prepared);
    let (total,): (i64,) = qb
        .build_query_as()
        .fetch_one(&mut *tx)
        .await
        .map_err(bounded)?;
    Ok(total)
}

/// The most common values of `property` over the scope. A caller without access to values
/// may break down only a property the published schema of `scope.name` declares non-PII.
pub async fn explore_breakdown(
    pool: &PgPool,
    workspace: WorkspaceId,
    scope: &ExploreScope<'_>,
    property: &str,
    limits: ExploreLimits,
    reveal: bool,
) -> Result<Breakdown, ExploreError> {
    check_key("property", property).map_err(|(field, reason)| invalid(field, reason))?;
    let mut tx = begin_bounded_for(pool, limits.timeout).await?;
    let prepared = prepare(&mut tx, workspace, scope, reveal).await?;
    if !reveal
        && !prepared
            .declared(property)
            .is_some_and(|declared| !declared.pii && declared.property_type != PropertyType::Object)
    {
        return Err(ExploreError::PiiPropertyNotExplorable);
    }
    let present = |qb: &mut Builder| {
        qb.push(" AND COALESCE(jsonb_typeof(");
        push_json(qb, property);
        qb.push("), 'null') <> 'null'");
    };

    let mut qb: Builder = QueryBuilder::new("SELECT ");
    push_text(&mut qb, property);
    qb.push(" AS v, count(*) AS n");
    push_scope(&mut qb, workspace, scope, &prepared);
    present(&mut qb);
    qb.push(" GROUP BY 1 ORDER BY n DESC, v LIMIT ")
        .push_bind(BREAKDOWN_VALUES);
    let values: Vec<(String, i64)> = qb
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(bounded)?;

    let mut qb: Builder = QueryBuilder::new(
        "SELECT count(*) AS total, count(*) FILTER (WHERE COALESCE(jsonb_typeof(",
    );
    push_json(&mut qb, property);
    qb.push("), 'null') <> 'null') AS present");
    push_scope(&mut qb, workspace, scope, &prepared);
    let (total, present_count): (i64, i64) = qb
        .build_query_as()
        .fetch_one(&mut *tx)
        .await
        .map_err(bounded)?;
    drop(tx);

    let named: i64 = values.iter().map(|(_, count)| count).sum();
    Ok(Breakdown {
        values,
        other: present_count - named,
        missing: total - present_count,
    })
}

#[derive(FromRow)]
struct EventRow {
    id: Uuid,
    occurred_at: OffsetDateTime,
    name: String,
    source: String,
    person_id: Uuid,
    validation_state: String,
    flags: Vec<String>,
    trace_id: Uuid,
    properties: Value,
    schema_version: Option<i32>,
}

/// Events matching the scope, newest first, at most `limit` after `after`. Rows carry the
/// published schema they were validated against, for the caller's masking.
pub async fn explore_rows(
    pool: &PgPool,
    workspace: WorkspaceId,
    scope: &ExploreScope<'_>,
    after: Option<RowCursor>,
    limit: u32,
    limits: ExploreLimits,
    reveal: bool,
) -> Result<RowsPage, ExploreError> {
    let limit = i64::from(limit.clamp(1, limits.row_limit.max(1)));
    let mut tx = begin_bounded_for(pool, limits.timeout).await?;
    let prepared = prepare(&mut tx, workspace, scope, reveal).await?;
    let mut qb: Builder = QueryBuilder::new(
        "SELECT e.id, e.occurred_at, e.name, e.source, e.person_id, e.validation_state, \
                e.flags, e.trace_id, e.properties, e.schema_version",
    );
    push_scope(&mut qb, workspace, scope, &prepared);
    if let Some(cursor) = after {
        qb.push(" AND (e.occurred_at, e.id) < (")
            .push_bind(cursor.at)
            .push(", ")
            .push_bind(cursor.id)
            .push(")");
    }
    qb.push(" ORDER BY e.occurred_at DESC, e.id DESC LIMIT ")
        .push_bind(limit + 1);
    let mut rows: Vec<EventRow> = qb
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(bounded)?;
    let more = i64::try_from(rows.len()).is_ok_and(|count| count > limit);
    if more {
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }

    let wanted: BTreeSet<(String, i32)> = rows
        .iter()
        .filter_map(|row| Some((row.name.clone(), row.schema_version?)))
        .collect();
    let mut schemas: HashMap<(String, i32), Arc<Vec<SchemaProperty>>> = HashMap::new();
    for (name, version) in wanted {
        let found: Option<Json<Vec<SchemaProperty>>> = sqlx::query_scalar(
            "SELECT properties FROM event_schema_versions \
             WHERE workspace_id = $1 AND name = $2 AND version = $3",
        )
        .bind(workspace.as_uuid())
        .bind(&name)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await
        .map_err(bounded)?;
        if let Some(properties) = found {
            schemas.insert((name, version), Arc::new(properties.0));
        }
    }
    drop(tx);

    let next = if more {
        rows.last().map(|row| RowCursor {
            at: row.occurred_at,
            id: row.id,
        })
    } else {
        None
    };
    let rows = rows
        .into_iter()
        .map(|row| ExploreRow {
            schema: row
                .schema_version
                .and_then(|version| schemas.get(&(row.name.clone(), version)).cloned()),
            id: row.id,
            occurred_at: row.occurred_at,
            name: row.name,
            source: row.source,
            person_id: row.person_id,
            validation_state: row.validation_state,
            flags: row.flags,
            trace_id: row.trace_id,
            properties: match row.properties {
                Value::Object(map) => map,
                _ => Map::new(),
            },
        })
        .collect();
    Ok(RowsPage { rows, next })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration as TimeDuration;
    use time::macros::datetime;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::{Cause, HmacPurpose, MasterKey, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::events::{
        EventRecord, NewEventSchema, apply_event_batch, create_event_schema, publish_schema_draft,
    };
    use crate::{begin, commit, create_workspace};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);
    const LIMITS: ExploreLimits = ExploreLimits {
        timeout: Duration::from_secs(10),
        row_limit: 10_000,
    };

    fn range() -> (OffsetDateTime, OffsetDateTime) {
        (NOW - TimeDuration::days(10), NOW + TimeDuration::days(1))
    }

    fn scope<'a>(name: Option<&'a str>, filters: &'a [ExploreFilter]) -> ExploreScope<'a> {
        let (from, to) = range();
        ExploreScope {
            name,
            filters,
            from,
            to,
            person: None,
        }
    }

    fn eq(key: &str, value: Value) -> ExploreFilter {
        ExploreFilter {
            key: key.to_owned(),
            op: FilterOp::Eq,
            value: Some(value),
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn property(name: &str, property_type: PropertyType, pii: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required: false,
            enum_values: None,
            description: String::new(),
            pii,
        }
    }

    async fn publish(pool: &PgPool, ws: WorkspaceId, name: &str, properties: Vec<SchemaProperty>) {
        create_event_schema(
            pool,
            ws,
            &NewEventSchema {
                name: name.to_owned(),
                mode: EventMode::Warn,
                description: String::new(),
                properties,
            },
        )
        .await
        .unwrap();
        let mut tx = begin(pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, name, None).await.unwrap();
        commit(tx).await.unwrap();
    }

    async fn send(
        pool: &PgPool,
        ws: WorkspaceId,
        who: &str,
        name: &str,
        hours_ago: i64,
        properties: Value,
    ) {
        let record = EventRecord {
            source: "web".to_owned(),
            identifiers: vec![("email".to_owned(), who.to_owned())],
            name: name.to_owned(),
            properties: properties.as_object().cloned().unwrap(),
            timestamp: Some(NOW - TimeDuration::hours(hours_ago)),
            received_at: NOW,
            message_id: None,
            trace: TraceContext::root(Cause::api("test")),
        };
        let key = MasterKey::generate().hmac_key(HmacPurpose::Redaction);
        apply_event_batch(pool, ws, &key, &[record]).await.unwrap();
    }

    /// Seven events of two names over three days, from three people.
    async fn fixture(pool: &PgPool) -> WorkspaceId {
        let ws = workspace(pool, "one").await;
        publish(
            pool,
            ws,
            "order",
            vec![
                property("total", PropertyType::Number, false),
                property("plan", PropertyType::String, false),
                property("email", PropertyType::String, true),
                property("vip", PropertyType::Boolean, false),
                property("tags", PropertyType::StringList, false),
                property("paid_at", PropertyType::Timestamp, false),
            ],
        )
        .await;
        let rows = [
            (
                "a@example.com",
                1,
                json!({"total": 10, "plan": "pro", "email": "a@example.com", "vip": true, "tags": ["x"], "paid_at": "2026-05-31T10:00:00Z"}),
            ),
            (
                "a@example.com",
                2,
                json!({"total": 20, "plan": "pro", "vip": false, "tags": ["x", "y"], "paid_at": "2026-05-31T12:00:00+02:00"}),
            ),
            (
                "b@example.com",
                25,
                json!({"total": 30, "plan": "free", "email": "b@example.com", "paid_at": "2026-05-30T10:00:00Z"}),
            ),
            ("b@example.com", 26, json!({"total": 40, "plan": "free"})),
            ("c@example.com", 49, json!({"total": 50, "plan": "team"})),
        ];
        for (who, ago, properties) in rows {
            send(pool, ws, who, "order", ago, properties).await;
        }
        send(
            pool,
            ws,
            "a@example.com",
            "page",
            3,
            json!({"path": "/home"}),
        )
        .await;
        send(
            pool,
            ws,
            "c@example.com",
            "page",
            50,
            json!({"path": "/pricing"}),
        )
        .await;
        ws
    }

    #[test]
    fn buckets_and_operators_have_stable_names() {
        for bucket in [Bucket::Hour, Bucket::Day, Bucket::Week] {
            assert_eq!(bucket.as_str().parse(), Ok(bucket));
        }
        assert_eq!("month".parse::<Bucket>(), Err(()));
    }

    #[test]
    fn debug_output_shows_no_event_value() {
        let row = ExploreRow {
            id: Uuid::nil(),
            occurred_at: NOW,
            name: "secret-name".into(),
            source: "web".into(),
            person_id: Uuid::nil(),
            validation_state: "valid".into(),
            flags: Vec::new(),
            trace_id: Uuid::nil(),
            properties: json!({"email": "a@example.com"})
                .as_object()
                .cloned()
                .unwrap(),
            schema: None,
        };
        let breakdown = Breakdown {
            values: vec![("a@example.com".into(), 1)],
            other: 0,
            missing: 0,
        };

        let shown = format!("{row:?}{breakdown:?}");

        assert!(!shown.contains("a@example.com") && !shown.contains("secret-name"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn count_breakdown_list_and_total_agree_on_the_fixture(pool: PgPool) {
        let ws = fixture(&pool).await;
        let filters = [ExploreFilter {
            key: "total".into(),
            op: FilterOp::Gte,
            value: Some(json!(20)),
        }];
        let scope = scope(Some("order"), &filters);

        let count = explore_count(&pool, ws, &scope, Bucket::Day, LIMITS, true)
            .await
            .unwrap();
        let breakdown = explore_breakdown(&pool, ws, &scope, "plan", LIMITS, true)
            .await
            .unwrap();
        let rows = explore_rows(&pool, ws, &scope, None, 100, LIMITS, true)
            .await
            .unwrap();
        let total = explore_total(&pool, ws, &scope, LIMITS, true)
            .await
            .unwrap();

        let counted: i64 = count.buckets.iter().map(|(_, n)| n).sum();
        let grouped: i64 = breakdown.values.iter().map(|(_, n)| n).sum();
        assert_eq!(counted, 4);
        assert_eq!(total, 4);
        assert_eq!(rows.rows.len(), 4);
        assert_eq!(grouped + breakdown.other + breakdown.missing, 4);
        assert_eq!(
            breakdown.values,
            [
                ("free".to_owned(), 2),
                ("pro".to_owned(), 1),
                ("team".to_owned(), 1)
            ]
        );
        assert!(!count.truncated && rows.next.is_none());
        assert!(count.buckets.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rows_come_newest_first_in_keyset_pages_with_their_schema(pool: PgPool) {
        let ws = fixture(&pool).await;
        let scope = scope(None, &[]);

        let first = explore_rows(&pool, ws, &scope, None, 3, LIMITS, true)
            .await
            .unwrap();
        let second = explore_rows(&pool, ws, &scope, first.next, 3, LIMITS, true)
            .await
            .unwrap();
        let third = explore_rows(&pool, ws, &scope, second.next, 3, LIMITS, true)
            .await
            .unwrap();

        let all: Vec<&ExploreRow> = first
            .rows
            .iter()
            .chain(&second.rows)
            .chain(&third.rows)
            .collect();
        assert_eq!(all.len(), 7);
        assert!(
            all.windows(2)
                .all(|pair| pair[0].occurred_at >= pair[1].occurred_at)
        );
        assert!(third.next.is_none());
        assert_eq!(all[0].name, "order");
        assert!(
            all[0]
                .schema
                .as_ref()
                .is_some_and(|schema| schema.len() == 6)
        );
        assert!(
            all.iter()
                .any(|row| row.name == "page" && row.schema.is_none())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_typed_operators_select_the_right_events(pool: PgPool) {
        let ws = fixture(&pool).await;
        let cases: Vec<(ExploreFilter, i64)> = vec![
            (eq("plan", json!("pro")), 2),
            (
                ExploreFilter {
                    key: "plan".into(),
                    op: FilterOp::Ne,
                    value: Some(json!("pro")),
                },
                3,
            ),
            (
                ExploreFilter {
                    key: "plan".into(),
                    op: FilterOp::In,
                    value: Some(json!(["free", "team"])),
                },
                3,
            ),
            (
                ExploreFilter {
                    key: "plan".into(),
                    op: FilterOp::Prefix,
                    value: Some(json!("fr")),
                },
                2,
            ),
            (
                ExploreFilter {
                    key: "total".into(),
                    op: FilterOp::Lt,
                    value: Some(json!(30)),
                },
                2,
            ),
            (eq("vip", json!(true)), 1),
            (eq("tags", json!("y")), 1),
            (
                ExploreFilter {
                    key: "vip".into(),
                    op: FilterOp::Exists,
                    value: None,
                },
                2,
            ),
            (
                ExploreFilter {
                    key: "vip".into(),
                    op: FilterOp::Exists,
                    value: Some(json!(false)),
                },
                3,
            ),
            // 12:00+02:00 is 10:00Z, so only the 2026-05-30 event and the two at 10:00Z.
            (
                ExploreFilter {
                    key: "paid_at".into(),
                    op: FilterOp::Lte,
                    value: Some(json!("2026-05-31T10:00:00Z")),
                },
                3,
            ),
        ];
        for (filter, expected) in cases {
            let filters = [filter.clone()];
            let total = explore_total(&pool, ws, &scope(Some("order"), &filters), LIMITS, true)
                .await
                .unwrap();
            assert_eq!(total, expected, "{filter:?}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_rfc_3339_timestamps_parse_in_a_timestamp_filter(pool: PgPool) {
        let parsed: Vec<(String, Option<OffsetDateTime>)> = sqlx::query_as(
            "SELECT v, zf_try_timestamptz(v) FROM unnest(ARRAY[                'now', 'today', 'infinity', 'epoch', '2026-06-01 12:00:00',                 '2026-06-01T12:00:00', '2026-06-01T12:00:00+0200', '2026-13-01T12:00:00Z',                 '2026-06-01T12:00:00Z', '2026-06-01T12:00:00.25+02:00']) AS v",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let accepted: Vec<&str> = parsed
            .iter()
            .filter(|(_, at)| at.is_some())
            .map(|(text, _)| text.as_str())
            .collect();
        assert_eq!(
            accepted,
            ["2026-06-01T12:00:00Z", "2026-06-01T12:00:00.25+02:00"]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_undeclared_property_is_compared_as_text_across_names(pool: PgPool) {
        let ws = fixture(&pool).await;
        let filters = [eq("path", json!("/home"))];

        let total = explore_total(&pool, ws, &scope(None, &filters), LIMITS, true)
            .await
            .unwrap();

        assert_eq!(total, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_filter_follows_merges_and_the_name_narrows(pool: PgPool) {
        let ws = fixture(&pool).await;
        let person: Uuid = sqlx::query_scalar(
            "SELECT person_id FROM identities WHERE workspace_id = $1 AND value = 'a@example.com'",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut one = scope(None, &[]);
        one.person = Some(person);
        let mut other = scope(Some("order"), &[]);
        other.person = Some(Uuid::now_v7());

        let mine = explore_total(&pool, ws, &one, LIMITS, true).await.unwrap();
        let nobody = explore_total(&pool, ws, &other, LIMITS, true)
            .await
            .unwrap();

        assert_eq!(mine, 3);
        assert_eq!(nobody, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_viewer_without_access_cannot_probe_or_break_down_a_pii_property(pool: PgPool) {
        let ws = fixture(&pool).await;
        let prefix = [ExploreFilter {
            key: "email".into(),
            op: FilterOp::Prefix,
            value: Some(json!("a")),
        }];
        let exact = [eq("email", json!("a@example.com"))];
        let undeclared = [ExploreFilter {
            key: "path".into(),
            op: FilterOp::Prefix,
            value: Some(json!("/")),
        }];

        let probe = explore_total(&pool, ws, &scope(Some("order"), &prefix), LIMITS, false).await;
        let on_unplanned = explore_total(&pool, ws, &scope(None, &undeclared), LIMITS, false).await;
        let by_value = explore_total(&pool, ws, &scope(Some("order"), &exact), LIMITS, false)
            .await
            .unwrap();
        let breakdown = explore_breakdown(
            &pool,
            ws,
            &scope(Some("order"), &[]),
            "email",
            LIMITS,
            false,
        )
        .await;
        let plain = explore_breakdown(&pool, ws, &scope(Some("order"), &[]), "plan", LIMITS, false)
            .await
            .unwrap();
        let revealed =
            explore_breakdown(&pool, ws, &scope(Some("order"), &[]), "email", LIMITS, true)
                .await
                .unwrap();

        assert!(matches!(probe, Err(ExploreError::PiiPropertyNotExplorable)));
        assert!(matches!(
            on_unplanned,
            Err(ExploreError::PiiPropertyNotExplorable)
        ));
        assert!(matches!(
            breakdown,
            Err(ExploreError::PiiPropertyNotExplorable)
        ));
        assert_eq!(by_value, 1);
        assert_eq!(plain.values.len(), 3);
        assert_eq!(revealed.values.len(), 2);
        assert_eq!(revealed.missing, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_range_and_the_name_are_checked(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (from, to) = range();
        let backwards = ExploreScope {
            from: to,
            to: from,
            ..scope(None, &[])
        };
        let too_wide = ExploreScope {
            from: NOW - TimeDuration::days(401),
            to: NOW,
            ..scope(None, &[])
        };
        let bad_name = scope(Some(""), &[]);

        for bad in [backwards, too_wide, bad_name] {
            let error = explore_total(&pool, ws, &bad, LIMITS, true)
                .await
                .unwrap_err();
            assert!(matches!(error, ExploreError::Invalid { .. }), "{error}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_row_limit_truncates_the_buckets_and_the_range_is_pruned_to_the_workspace(
        pool: PgPool,
    ) {
        let ws = fixture(&pool).await;
        let other = workspace(&pool, "two").await;
        send(
            &pool,
            other,
            "z@example.com",
            "order",
            1,
            json!({"total": 1}),
        )
        .await;
        let limits = ExploreLimits {
            row_limit: 2,
            ..LIMITS
        };

        let count = explore_count(&pool, ws, &scope(None, &[]), Bucket::Hour, limits, true)
            .await
            .unwrap();
        let theirs = explore_total(&pool, other, &scope(None, &[]), LIMITS, true)
            .await
            .unwrap();

        assert!(count.truncated);
        assert_eq!(count.buckets.len(), 2);
        assert_eq!(theirs, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_query_past_its_timeout_is_cancelled_as_a_timeout(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person: Uuid = {
            send(&pool, ws, "a@example.com", "seen", 1, json!({})).await;
            sqlx::query_scalar("SELECT person_id FROM events LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        sqlx::query(
            "INSERT INTO events \
                (id, workspace_id, person_id, name, occurred_at, received_at, properties, \
                 source, trace_id, validation_state) \
             SELECT gen_random_uuid(), $1, $2, 'bulk', $3 - make_interval(secs => n), $3, \
                    jsonb_build_object('n', n), 'web', gen_random_uuid(), 'unplanned' \
             FROM generate_series(1, 300000) AS n",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(NOW)
        .execute(&pool)
        .await
        .unwrap();
        let hurried = ExploreLimits {
            timeout: Duration::from_millis(1),
            row_limit: 10_000,
        };
        let filters = [ExploreFilter {
            key: "n".into(),
            op: FilterOp::Prefix,
            value: Some(json!("99")),
        }];

        let error = explore_count(
            &pool,
            ws,
            &scope(None, &filters),
            Bucket::Hour,
            hurried,
            true,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&error, ExploreError::Store(inner) if inner.is_timeout()),
            "{error}"
        );
    }
}
