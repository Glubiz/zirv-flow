//! The per-person message log read (architecture 12.3 and 15): a person's messages, newest first,
//! with the names a reader needs, the gate results as stored, when a waiting message runs again
//! and the trace its decisions sit on. No address and no identifier hash is read.

use hashbrown::HashMap;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;
use crate::bounded::begin_bounded;
use crate::flows::SEND_JOB_KIND;

/// What a message list may be narrowed to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MessageLogFilter {
    /// Only messages in one of these stored statuses.
    pub statuses: Option<Vec<&'static str>>,
    /// Only messages of this template.
    pub template: Option<Uuid>,
    /// Only pending messages that wait (or do not), or those still in progress.
    pub wait: Option<WaitFilter>,
}

/// A narrowing by whether a message is waiting for a known time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitFilter {
    /// Pending, with a rate-limit wait or a gate hold that is still in force.
    Deferred,
    /// `sending`, or pending without such a wait: the send job has not got to it yet, or is
    /// retrying it.
    InProgress,
}

/// Where a page ended: the next page starts after this message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageLogCursor {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
}

/// Why a pending message waits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitReason {
    /// The quiet hours of the recipient's time zone.
    QuietHours,
    /// A frequency cap in `defer` mode.
    FrequencyCap,
    /// The destination's rate limit has no token yet.
    RateLimit,
}

impl WaitReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QuietHours => "quiet_hours",
            Self::FrequencyCap => "frequency_cap",
            Self::RateLimit => "rate_limit",
        }
    }
}

/// A message as the log shows it. Ids, names and codes only.
#[derive(Clone, Debug, PartialEq)]
pub struct LoggedMessage {
    pub id: Uuid,
    pub created_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
    pub template_id: Uuid,
    pub template_name: Option<String>,
    pub template_version: Option<i32>,
    pub category: Option<String>,
    pub flow_id: Option<Uuid>,
    pub flow_name: Option<String>,
    pub flow_version: Option<i32>,
    pub node_id: String,
    pub node_name: Option<String>,
    pub destination_id: Uuid,
    pub destination_name: Option<String>,
    pub status: String,
    pub reason: Option<String>,
    pub deciding_rule: Option<String>,
    /// The id of the consent policy rule that denied the send, when a rule of the policy did.
    pub rule_id: Option<Uuid>,
    /// The workspace's consent policy, when `rule_id` is set and the workspace has one.
    pub policy_id: Option<Uuid>,
    /// The gate entries in the order they were written.
    pub gate_results: Value,
    /// When a pending message runs again, and why.
    pub deferred_until: Option<OffsetDateTime>,
    pub deferred_why: Option<WaitReason>,
    /// The trace of the decisions about this message; none when none was recorded.
    pub trace_id: Option<Uuid>,
}

/// A page of the log.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageLogPage {
    pub messages: Vec<LoggedMessage>,
    /// Pass as `before` for the next page; absent on the last.
    pub next: Option<MessageLogCursor>,
}

#[derive(FromRow)]
struct Row {
    id: Uuid,
    enrollment_id: Uuid,
    created_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
    template_id: Uuid,
    template_name: Option<String>,
    template_version: Option<i32>,
    category: Option<String>,
    flow_id: Option<Uuid>,
    flow_name: Option<String>,
    flow_version: Option<i32>,
    node_id: String,
    node_name: Option<String>,
    destination_id: Uuid,
    destination_name: Option<String>,
    status: String,
    reason: Option<String>,
    deciding_rule: Option<String>,
    gate_results: Value,
    wait_why: Option<String>,
    wait_until: Option<OffsetDateTime>,
}

/// The columns and the joins up to the send job's kind, which is bound next.
const SELECT: &str = "SELECT m.id, m.enrollment_id, m.created_at, m.completed_at, m.template_id,         t.name AS template_name, m.template_version, m.category, m.flow_id, f.name AS flow_name,         m.flow_version, m.node_id,         (SELECT jsonb_path_query_first(v.graph, '$.nodes[*] ? (@.id == $id).name',                 jsonb_build_object('id', m.node_id)) #>> '{}'            FROM flow_versions v           WHERE v.workspace_id = m.workspace_id AND v.flow_id = m.flow_id             AND v.version = m.flow_version) AS node_name,         m.destination_id, d.name AS destination_name, m.status, m.reason, m.deciding_rule,         m.gate_results,         CASE WHEN j.rate THEN 'rate_limit' WHEN k.hold THEN h.gate END AS wait_why,         CASE WHEN j.rate THEN j.run_at WHEN k.hold THEN COALESCE(j.run_at, h.until) END             AS wait_until      FROM messages m      LEFT JOIN templates t ON t.workspace_id = m.workspace_id AND t.id = m.template_id      LEFT JOIN flows f ON f.workspace_id = m.workspace_id AND f.id = m.flow_id      LEFT JOIN destinations d ON d.workspace_id = m.workspace_id AND d.id = m.destination_id      LEFT JOIN LATERAL (         SELECT jb.run_at,                COALESCE(jsonb_typeof(jb.payload -> 'first_wait') = 'string'                    OR (jb.payload -> 'first_wait' IS NULL                        AND jsonb_typeof(jb.payload -> 'deferrals') = 'number'                        AND (jb.payload ->> 'deferrals')::numeric > 0), false) AS rate           FROM jobs jb          WHERE m.status = 'pending' AND jb.workspace_id = m.workspace_id AND jb.kind = ";

/// The waiting-time part of the query: the pending message's live send job, its latest gate
/// hold, and whether that hold is in force. A rate-limit wait needs the job's `first_wait`, or, in the older payload that has none, a deferral (as `SendJob::parse` reads it); a
/// gate hold needs the job to be queued for the hold's time (a job that is due, or backing off
/// from a failure, is not held), or, without a ready job, the hold to lie in the future.
const WAIT_JOINS: &str = " AND jb.state = 'ready' AND jb.payload ->> 'message_id' = m.id::text          ORDER BY jb.run_at LIMIT 1) j ON true      LEFT JOIN LATERAL (         SELECT e ->> 'gate' AS gate, (e ->> 'run_at')::timestamptz AS until           FROM jsonb_array_elements(CASE WHEN jsonb_typeof(m.gate_results) = 'array'                                          THEN m.gate_results ELSE '[]'::jsonb END)                WITH ORDINALITY AS a(e, n)          WHERE m.status = 'pending' AND e ->> 'result' = 'deferred'          ORDER BY n DESC LIMIT 1) h ON true      LEFT JOIN LATERAL (         SELECT h.gate IN ('quiet_hours', 'frequency_cap') AND h.until IS NOT NULL AND                CASE WHEN j.run_at IS NULL THEN h.until > now()                     ELSE j.run_at > now() AND abs(extract(epoch FROM j.run_at - h.until)) < 1                END AS hold) k ON true      WHERE m.workspace_id = ";

/// A pending message that waits for a known time, as `wait_why` reads it.
const DEFERRED: &str = "(m.status = 'pending' AND (COALESCE(j.rate, false)                         OR COALESCE(k.hold, false)))";

/// The people whose messages read as `person`: the survivor it was merged into (or itself) and
/// everyone merged into that survivor, through `person_merges`. `None` when the workspace has no
/// such person (an erased person has no row left).
async fn who(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Vec<Uuid>>, StoreError> {
    let survivor: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let Some(survivor) = survivor else {
        return Ok(None);
    };
    let people = sqlx::query_scalar(
        "WITH RECURSIVE who(id) AS ( \
            SELECT $2::uuid \
            UNION \
            SELECT m.merged_person_id FROM person_merges m JOIN who ON m.survivor_person_id = who.id \
            WHERE m.workspace_id = $1) \
         SELECT id FROM who",
    )
    .bind(workspace.as_uuid())
    .bind(survivor)
    .fetch_all(connection)
    .await
    .map_err(StoreError::bounded)?;
    Ok(Some(people))
}

/// The id of the consent rule the gate log names.
fn consent_rule(gates: &Value) -> Option<Uuid> {
    gates
        .as_array()?
        .iter()
        .find(|entry| entry["gate"] == "consent" && entry["result"] == "blocked")?["rule"]
        .as_str()?
        .parse()
        .ok()
}

/// The first trace each message has decisions in. The enrollment key is the indexed one; every
/// send decision carries both.
async fn traces(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    rows: &[Row],
) -> Result<HashMap<Uuid, Uuid>, StoreError> {
    let enrollments: Vec<String> = rows
        .iter()
        .map(|row| row.enrollment_id.to_string())
        .collect();
    let messages: Vec<String> = rows.iter().map(|row| row.id.to_string()).collect();
    let found: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT ON (subject_ids ->> 'message') subject_ids ->> 'message', trace_id \
         FROM decision_records \
         WHERE workspace_id = $1 AND subject_ids ? 'enrollment' \
           AND subject_ids ->> 'enrollment' = ANY($2) AND subject_ids ->> 'message' = ANY($3) \
         ORDER BY subject_ids ->> 'message', recorded_at",
    )
    .bind(workspace.as_uuid())
    .bind(&enrollments)
    .bind(&messages)
    .fetch_all(connection)
    .await
    .map_err(StoreError::bounded)?;
    Ok(found
        .into_iter()
        .filter_map(|(message, trace)| Some((message.parse().ok()?, trace)))
        .collect())
}

/// A page of the message log of `person` and the people merged into them, newest first by
/// (`created_at`, `id`). `None` when the workspace has no such person.
pub async fn load_person_message_log(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    filter: &MessageLogFilter,
    limit: u32,
    before: Option<MessageLogCursor>,
) -> Result<Option<MessageLogPage>, StoreError> {
    let limit = i64::from(limit.max(1));
    let mut tx = begin_bounded(pool).await?;
    let Some(people) = who(&mut tx, workspace, person).await? else {
        return Ok(None);
    };
    let mut query = QueryBuilder::<Postgres>::new(SELECT);
    query.push_bind(SEND_JOB_KIND).push(WAIT_JOINS);
    query.push_bind(workspace.as_uuid());
    query
        .push(" AND m.person_id = ANY(")
        .push_bind(&people)
        .push(")");
    if let Some(statuses) = &filter.statuses {
        query
            .push(" AND m.status = ANY(")
            .push_bind(statuses.clone())
            .push(")");
    }
    if let Some(template) = filter.template {
        query.push(" AND m.template_id = ").push_bind(template);
    }
    match filter.wait {
        Some(WaitFilter::Deferred) => {
            query.push(" AND ").push(DEFERRED);
        }
        Some(WaitFilter::InProgress) => {
            query
                .push(" AND (m.status = 'sending' OR (m.status = 'pending' AND NOT ")
                .push(DEFERRED)
                .push("))");
        }
        None => {}
    }
    if let Some(cursor) = before {
        query
            .push(" AND (m.created_at, m.id) < (")
            .push_bind(cursor.created_at)
            .push(", ")
            .push_bind(cursor.id)
            .push(")");
    }
    query
        .push(" ORDER BY m.created_at DESC, m.id DESC LIMIT ")
        .push_bind(limit + 1);
    let mut rows: Vec<Row> = query
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::bounded)?;
    let more = i64::try_from(rows.len()).is_ok_and(|len| len > limit);
    rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));

    let traces = if rows.is_empty() {
        HashMap::new()
    } else {
        traces(&mut tx, workspace, &rows).await?
    };
    let policy_id: Option<Uuid> = if rows
        .iter()
        .any(|row| consent_rule(&row.gate_results).is_some())
    {
        sqlx::query_scalar("SELECT id FROM consent_policies WHERE workspace_id = $1")
            .bind(workspace.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::bounded)?
    } else {
        None
    };
    drop(tx);

    let next = if more {
        rows.last().map(|last| MessageLogCursor {
            created_at: last.created_at,
            id: last.id,
        })
    } else {
        None
    };
    let messages = rows
        .into_iter()
        .map(|row| {
            let why = match row.wait_why.as_deref() {
                Some("rate_limit") => Some(WaitReason::RateLimit),
                Some("quiet_hours") => Some(WaitReason::QuietHours),
                Some("frequency_cap") => Some(WaitReason::FrequencyCap),
                _ => None,
            };
            let until = why.and(row.wait_until);
            let rule_id = consent_rule(&row.gate_results);
            LoggedMessage {
                trace_id: traces.get(&row.id).copied(),
                policy_id: rule_id.and(policy_id),
                rule_id,
                deferred_until: until,
                deferred_why: why.filter(|_| until.is_some()),
                id: row.id,
                created_at: row.created_at,
                completed_at: row.completed_at,
                template_id: row.template_id,
                template_name: row.template_name,
                template_version: row.template_version,
                category: row.category,
                flow_id: row.flow_id,
                flow_name: row.flow_name,
                flow_version: row.flow_version,
                node_id: row.node_id,
                node_name: row.node_name,
                destination_id: row.destination_id,
                destination_name: row.destination_name,
                status: row.status,
                reason: row.reason,
                deciding_rule: row.deciding_rule,
                gate_results: row.gate_results,
            }
        })
        .collect();
    Ok(Some(MessageLogPage { messages, next }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_consent_rule_is_the_blocked_consent_entrys() {
        let rule = Uuid::from_u128(9);
        let gates = json!([
            {"gate": "consent", "result": "blocked", "reason": "no_consent", "rule": rule},
        ]);

        assert_eq!(consent_rule(&gates), Some(rule));
        assert_eq!(
            consent_rule(&json!([{"gate": "consent", "result": "passed"}])),
            None
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_log_pages_newest_first_filters_and_reads_through_merges(pool: PgPool) {
        use zirv_flow_core::consent::Dimensions;
        use zirv_flow_core::{Slug, WorkspaceName};

        use crate::{MessageStatus, NewMessage, insert_message};

        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let survivor = Uuid::now_v7();
        let merged = Uuid::now_v7();
        for id in [survivor, merged] {
            sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
                .bind(id)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(merged)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO person_merges \
                (id, workspace_id, survivor_person_id, merged_person_id, trace_id) \
             VALUES ($1, $2, $3, $4, gen_random_uuid())",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(survivor)
        .bind(merged)
        .execute(&pool)
        .await
        .unwrap();
        let dimensions = Dimensions::new();
        let base = OffsetDateTime::now_utc();
        let mut ids = Vec::new();
        for index in 0..5_u8 {
            let id = Uuid::now_v7();
            ids.push(id);
            let skipped = index % 2 == 1;
            let new = NewMessage {
                id,
                workspace: ws,
                person: if index == 0 { merged } else { survivor },
                enrollment: Uuid::now_v7(),
                flow: None,
                flow_version: None,
                node: "mail",
                template: Uuid::from_u128(1 + u128::from(index == 4)),
                template_version: Some(1),
                destination: Uuid::from_u128(9),
                sender_identity: None,
                category: Some("marketing"),
                topic: None,
                dimensions: &dimensions,
                status: if skipped {
                    MessageStatus::Skipped
                } else {
                    MessageStatus::Sent
                },
                reason: skipped.then_some("no_consent"),
                deciding_rule: None,
                gates: &[],
                attempts: 1,
                now: base + time::Duration::seconds(i64::from(index)),
            };
            assert!(insert_message(&pool, &new).await.unwrap());
        }
        let all = MessageLogFilter::default();

        let first = load_person_message_log(&pool, ws, survivor, &all, 3, None)
            .await
            .unwrap()
            .unwrap();
        let second = load_person_message_log(&pool, ws, survivor, &all, 3, first.next)
            .await
            .unwrap()
            .unwrap();
        let from_merged = load_person_message_log(&pool, ws, merged, &all, 10, None)
            .await
            .unwrap()
            .unwrap();
        let skipped_only = MessageLogFilter {
            statuses: Some(vec!["skipped"]),
            template: None,
            wait: None,
        };
        let skipped = load_person_message_log(&pool, ws, survivor, &skipped_only, 10, None)
            .await
            .unwrap()
            .unwrap();
        let one_template = MessageLogFilter {
            statuses: None,
            template: Some(Uuid::from_u128(2)),
            wait: None,
        };
        let by_template = load_person_message_log(&pool, ws, survivor, &one_template, 10, None)
            .await
            .unwrap()
            .unwrap();
        let stranger = load_person_message_log(&pool, ws, Uuid::now_v7(), &all, 10, None)
            .await
            .unwrap();

        let page_ids =
            |page: &MessageLogPage| page.messages.iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(page_ids(&first), [ids[4], ids[3], ids[2]]);
        assert_eq!(
            page_ids(&second),
            [ids[1], ids[0]],
            "no overlap, merged person's included"
        );
        assert!(second.next.is_none());
        assert_eq!(page_ids(&from_merged).len(), 5);
        assert_eq!(page_ids(&skipped), [ids[3], ids[1]]);
        assert_eq!(page_ids(&by_template), [ids[4]]);
        assert!(stranger.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_a_wait_that_is_one_is_deferred_and_the_rest_is_in_progress(pool: PgPool) {
        use zirv_flow_core::consent::Dimensions;
        use zirv_flow_core::{Slug, WorkspaceName};

        use crate::{MessageStatus, NewMessage, insert_message, record_message_deferral};

        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let dimensions = Dimensions::new();
        let now = OffsetDateTime::now_utc();
        let whole = |offset: time::Duration| (now + offset).replace_nanosecond(0).unwrap();
        let mut ids = Vec::new();
        for index in 0..6_i64 {
            let id = Uuid::now_v7();
            ids.push(id);
            let new = NewMessage {
                id,
                workspace: ws,
                person,
                enrollment: Uuid::now_v7(),
                flow: None,
                flow_version: None,
                node: "mail",
                template: Uuid::from_u128(1),
                template_version: Some(1),
                destination: Uuid::from_u128(9),
                sender_identity: None,
                category: None,
                topic: None,
                dimensions: &dimensions,
                status: if index == 4 {
                    MessageStatus::Sending
                } else {
                    MessageStatus::Pending
                },
                reason: None,
                deciding_rule: None,
                gates: &[],
                attempts: 1,
                now: now - time::Duration::seconds(100 - index),
            };
            assert!(insert_message(&pool, &new).await.unwrap());
        }
        let job = |message: Uuid, run_at: OffsetDateTime, first_wait: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO jobs (id, workspace_id, kind, payload, state, run_at, trace_id, cause_kind, cause_id)                      VALUES ($1, $2, $3, jsonb_build_object('message_id', $4::text,                              'first_wait', $5::text), 'ready', $6, gen_random_uuid(), 'api', 't')",
                )
                .bind(Uuid::now_v7())
                .bind(ws.as_uuid())
                .bind(SEND_JOB_KIND)
                .bind(message)
                .bind(first_wait)
                .bind(run_at)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        // 0: a job that is due and not leased yet.
        job(ids[0], whole(-time::Duration::minutes(1)), None).await;
        // 1: backing off after a failure; its only gate hold is long past.
        let past = whole(-time::Duration::hours(1));
        record_message_deferral(&pool, ws, ids[1], "quiet_hours", past, None, false)
            .await
            .unwrap();
        job(ids[1], whole(time::Duration::minutes(5)), None).await;
        // 2: waits for a rate-limit token.
        let token = whole(time::Duration::minutes(1));
        job(ids[2], token, Some("2026-01-01T00:00:00Z")).await;
        // 3: held by quiet hours, the job queued for the hold's time.
        let hold = whole(time::Duration::hours(2));
        record_message_deferral(&pool, ws, ids[3], "quiet_hours", hold, None, false)
            .await
            .unwrap();
        job(ids[3], hold, None).await;
        // 4: sending.
        // 5: waits for a token in the older payload shape, which has a deferral and no `first_wait`.
        let older = whole(time::Duration::minutes(2));
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, state, run_at, trace_id, cause_kind, cause_id) \n             VALUES ($1, $2, $3, jsonb_build_object('message_id', $4::text, 'deferrals', 1), 'ready', $5,                      gen_random_uuid(), 'api', 't')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(SEND_JOB_KIND)
        .bind(ids[5])
        .bind(older)
        .execute(&pool)
        .await
        .unwrap();
        let with = |wait| MessageLogFilter {
            statuses: None,
            template: None,
            wait: Some(wait),
        };
        let load = |filter: MessageLogFilter, limit: u32, before| {
            let pool = pool.clone();
            async move {
                load_person_message_log(&pool, ws, person, &filter, limit, before)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };

        let all = load(MessageLogFilter::default(), 10, None).await;
        let deferred = load(with(WaitFilter::Deferred), 10, None).await;
        let in_progress = load(with(WaitFilter::InProgress), 10, None).await;
        let first = load(with(WaitFilter::InProgress), 2, None).await;
        let second = load(with(WaitFilter::InProgress), 2, first.next).await;

        let ids_of = |page: &MessageLogPage| page.messages.iter().map(|m| m.id).collect::<Vec<_>>();
        let wait_of = |id: Uuid| {
            let m = all.messages.iter().find(|m| m.id == id).unwrap();
            (m.deferred_why, m.deferred_until)
        };
        assert_eq!(wait_of(ids[0]), (None, None), "not leased yet");
        assert_eq!(wait_of(ids[1]), (None, None), "a retry backoff");
        assert_eq!(wait_of(ids[2]), (Some(WaitReason::RateLimit), Some(token)));
        assert_eq!(wait_of(ids[3]), (Some(WaitReason::QuietHours), Some(hold)));
        assert_eq!(wait_of(ids[4]), (None, None));
        assert_eq!(wait_of(ids[5]), (Some(WaitReason::RateLimit), Some(older)));
        assert_eq!(ids_of(&deferred), [ids[5], ids[3], ids[2]]);
        assert_eq!(ids_of(&in_progress), [ids[4], ids[1], ids[0]]);
        assert_eq!(ids_of(&first), [ids[4], ids[1]]);
        assert_eq!(ids_of(&second), [ids[0]], "paging keeps its place");
    }
}
