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
    now: OffsetDateTime,
}

const SELECT: &str = "SELECT m.id, m.enrollment_id, m.created_at, m.completed_at, m.template_id, \
        t.name AS template_name, m.template_version, m.category, m.flow_id, f.name AS flow_name, \
        m.flow_version, m.node_id, \
        (SELECT jsonb_path_query_first(v.graph, '$.nodes[*] ? (@.id == $id).name', \
                jsonb_build_object('id', m.node_id)) #>> '{}' \
           FROM flow_versions v \
          WHERE v.workspace_id = m.workspace_id AND v.flow_id = m.flow_id \
            AND v.version = m.flow_version) AS node_name, \
        m.destination_id, d.name AS destination_name, m.status, m.reason, m.deciding_rule, \
        m.gate_results, now() AS now \
     FROM messages m \
     LEFT JOIN templates t ON t.workspace_id = m.workspace_id AND t.id = m.template_id \
     LEFT JOIN flows f ON f.workspace_id = m.workspace_id AND f.id = m.flow_id \
     LEFT JOIN destinations d ON d.workspace_id = m.workspace_id AND d.id = m.destination_id \
     WHERE m.workspace_id = ";

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

/// The latest `deferred` entry of a gate log: the gate and the time it held the message to.
fn latest_deferral(gates: &Value) -> Option<(WaitReason, OffsetDateTime)> {
    let entry = gates
        .as_array()?
        .iter()
        .rev()
        .find(|entry| entry["result"] == "deferred")?;
    let until = entry["run_at"].as_str().and_then(|text| {
        OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
    })?;
    let why = match entry["gate"].as_str()? {
        "quiet_hours" => WaitReason::QuietHours,
        "frequency_cap" => WaitReason::FrequencyCap,
        _ => return None,
    };
    Some((why, until))
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

/// The waiting send job of each message: when it runs and whether it waits for a rate-limit token
/// (a gate's hold clears `first_wait`; a wait for a token sets it).
async fn waiting_jobs(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    messages: &[String],
) -> Result<HashMap<String, (OffsetDateTime, bool)>, StoreError> {
    let rows: Vec<(String, OffsetDateTime, bool)> = sqlx::query_as(
        "SELECT payload ->> 'message_id', run_at, \
                COALESCE(jsonb_typeof(payload -> 'first_wait') = 'string', false) \
         FROM jobs \
         WHERE workspace_id = $1 AND kind = $2 AND state = 'ready' \
           AND payload ->> 'message_id' = ANY($3)",
    )
    .bind(workspace.as_uuid())
    .bind(SEND_JOB_KIND)
    .bind(messages)
    .fetch_all(connection)
    .await
    .map_err(StoreError::bounded)?;
    Ok(rows
        .into_iter()
        .map(|(message, run_at, rate)| (message, (run_at, rate)))
        .collect())
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

    let waiting: Vec<String> = rows
        .iter()
        .filter(|row| row.status == "pending")
        .map(|row| row.id.to_string())
        .collect();
    let jobs = if waiting.is_empty() {
        HashMap::new()
    } else {
        waiting_jobs(&mut tx, workspace, &waiting).await?
    };
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
            let (until, why) = wait_of(&row, jobs.get(&row.id.to_string()));
            let rule_id = consent_rule(&row.gate_results);
            LoggedMessage {
                trace_id: traces.get(&row.id).copied(),
                policy_id: rule_id.and(policy_id),
                rule_id,
                deferred_until: until,
                deferred_why: why,
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

/// When a pending message runs again and why: the waiting job says, and without one the last gate
/// hold does while it is still in the future.
fn wait_of(
    row: &Row,
    job: Option<&(OffsetDateTime, bool)>,
) -> (Option<OffsetDateTime>, Option<WaitReason>) {
    if row.status != "pending" {
        return (None, None);
    }
    let held = latest_deferral(&row.gate_results);
    match (job, held) {
        (Some((run_at, true)), _) => (Some(*run_at), Some(WaitReason::RateLimit)),
        (Some((run_at, false)), Some((why, _))) => (Some(*run_at), Some(why)),
        (Some((run_at, false)), None) => (Some(*run_at), Some(WaitReason::RateLimit)),
        (None, Some((why, until))) if until > row.now => (Some(until), Some(why)),
        (None, _) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    fn row(status: &str, gates: Value) -> Row {
        Row {
            id: Uuid::from_u128(1),
            enrollment_id: Uuid::from_u128(2),
            created_at: datetime!(2026-03-01 12:00 UTC),
            completed_at: None,
            template_id: Uuid::from_u128(3),
            template_name: None,
            template_version: None,
            category: None,
            flow_id: None,
            flow_name: None,
            flow_version: None,
            node_id: "mail".to_owned(),
            node_name: None,
            destination_id: Uuid::from_u128(4),
            destination_name: None,
            status: status.to_owned(),
            reason: None,
            deciding_rule: None,
            gate_results: gates,
            now: datetime!(2026-03-01 12:00 UTC),
        }
    }

    #[test]
    fn the_latest_gate_hold_names_its_gate_and_time() {
        let gates = json!([
            {"gate": "quiet_hours", "result": "deferred", "run_at": "2026-03-01T06:00:00Z"},
            {"gate": "frequency_cap", "result": "deferred", "run_at": "2026-03-01T13:00:00Z"},
        ]);

        assert_eq!(
            latest_deferral(&gates),
            Some((WaitReason::FrequencyCap, datetime!(2026-03-01 13:00 UTC)))
        );
        assert_eq!(
            latest_deferral(&json!([{"gate": "consent", "result": "passed"}])),
            None
        );
    }

    #[test]
    fn a_pending_message_waits_for_its_job_and_a_gate_hold_in_the_past_is_not_a_wait() {
        let held = json!([
            {"gate": "quiet_hours", "result": "deferred", "run_at": "2026-03-01T06:00:00Z"}
        ]);
        let future = json!([
            {"gate": "quiet_hours", "result": "deferred", "run_at": "2026-03-01T18:00:00Z"}
        ]);
        let job = (datetime!(2026-03-01 15:00 UTC), true);

        assert_eq!(
            wait_of(&row("pending", held.clone()), Some(&job)),
            (Some(job.0), Some(WaitReason::RateLimit))
        );
        assert_eq!(wait_of(&row("pending", held), None), (None, None));
        assert_eq!(
            wait_of(&row("pending", future), None),
            (
                Some(datetime!(2026-03-01 18:00 UTC)),
                Some(WaitReason::QuietHours)
            )
        );
        assert_eq!(wait_of(&row("sent", json!([])), Some(&job)), (None, None));
    }

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
        };
        let skipped = load_person_message_log(&pool, ws, survivor, &skipped_only, 10, None)
            .await
            .unwrap()
            .unwrap();
        let one_template = MessageLogFilter {
            statuses: None,
            template: Some(Uuid::from_u128(2)),
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
}
