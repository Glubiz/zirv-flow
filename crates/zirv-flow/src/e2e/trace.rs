//! The trace of the scenario as data: the decisions a complete trace holds, the check that names
//! the one that is missing, and the normal form of the trace the web test reads as a fixture.

use std::path::PathBuf;

use hashbrown::{HashMap, HashSet};
use serde_json::{Value, json};

/// The decisions the trace of the send path must hold once: action, outcome and, when it
/// matters, the reason. What each message must hold is [`GATES`].
pub(super) const REQUIRED: &[(&str, &str, Option<&str>)] = &[
    // The track call and what it caused.
    ("event.accepted", "valid", None),
    ("segment.entered", "entered", None),
    ("flow.enroll", "enrolled", None),
    // The steps: queued, a wait, its wake and the end of the flow.
    ("flow.step", "enqueued", None),
    ("flow.step", "waiting", None),
    ("flow.wake", "timer", None),
    ("flow.step", "done", None),
    ("flow.enrollment", "completed", None),
    // The unsubscribe and the delivery feedback.
    ("consent.change", "changed", Some("unsubscribe_link")),
    ("message.feedback", "recorded", Some("delivered")),
];

/// The send gates a message that went out passed, in the order the pipeline runs them.
pub(super) const GATES: &[&str] = &[
    "enrollment",
    "template",
    "sender",
    "address",
    "consent",
    "suppression",
    "quiet_hours",
    "frequency_cap",
    "render",
    "rate_limit",
    "delivery",
];

/// A transient send retries twice, fails on its last attempt, and a signed hard-bounce
/// webhook suppresses the address on the same trace.
const FAILING: &[(&str, &str, Option<&str>)] = &[
    ("event.accepted", "valid", None),
    ("segment.entered", "entered", Some("data_change")),
    ("flow.enroll", "enrolled", None),
    ("flow.step", "enqueued", Some("inputs_fresh")),
    ("flow.step", "waiting", Some("wait.duration")),
    ("flow.wait", "waiting", Some("duration")),
    ("job.attempt", "failed", Some("job_error")),
    ("job.attempt", "failed", Some("job_error")),
    ("send.gate", "passed", Some("enrollment")),
    ("send.gate", "passed", Some("template")),
    ("send.gate", "passed", Some("sender")),
    ("send.gate", "passed", Some("address")),
    ("send.gate", "passed", Some("render")),
    ("send.gate", "passed", Some("rate_limit")),
    ("send.gate", "blocked", Some("delivery")),
    ("message.send", "failed", Some("http_5xx")),
    ("message.feedback", "recorded", Some("bounced_hard")),
    (
        "suppression.feedback",
        "added",
        Some("organization_hard_bounce"),
    ),
];

pub(super) fn failing_complete(records: &[Value]) -> Result<(), String> {
    let actual: Vec<_> = records
        .iter()
        .map(|record| {
            (
                text(record, "action").unwrap_or_default(),
                text(record, "outcome").unwrap_or_default(),
                text(record, "reason"),
            )
        })
        .collect();
    if actual != FAILING {
        return Err(format!("the failing send decisions differ: {actual:?}"));
    }
    for (index, record) in records
        .iter()
        .filter(|record| text(record, "action") == Some("job.attempt"))
        .enumerate()
    {
        if record["attempt"].as_u64() != Some(index as u64 + 1)
            || record["job_id"] != records[6]["job_id"]
        {
            return Err("the failing send retries are not consecutive attempts of one job".into());
        }
    }
    let send = records
        .iter()
        .find(|record| text(record, "action") == Some("message.send"))
        .unwrap();
    if send["attempt"] != 3 || send["job_id"] != records[6]["job_id"] {
        return Err("the failed send is not the last attempt of the retrying job".into());
    }
    one_tree(records)
}

/// How many messages the scenario sends, and how many it stops at the consent gate.
const SENT: usize = 2;
const SKIPPED: usize = 1;

fn text<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

fn describes(record: &Value, wanted: &(&str, &str, Option<&str>)) -> bool {
    text(record, "action") == Some(wanted.0)
        && text(record, "outcome") == Some(wanted.1)
        && wanted
            .2
            .is_none_or(|reason| text(record, "reason") == Some(reason))
}

/// The cause a record names: the parent link the trace response carries.
fn cause_id(record: &Value) -> Option<&str> {
    record.get("cause").and_then(|cause| text(cause, "id"))
}

/// The ids a record answers to as a parent: its own and those of what it is about.
fn ids_of(record: &Value) -> Vec<&str> {
    let subjects = record.get("subject_ids").and_then(Value::as_object);
    text(record, "id")
        .into_iter()
        .chain(
            subjects
                .into_iter()
                .flat_map(|s| s.values().filter_map(Value::as_str)),
        )
        .collect()
}

/// Whether the records are one tree. A record's cause names the parent: the id of a record or of
/// a subject (an event, a segment, an enrollment) that another record is about. The first record
/// is the root, started by a cause outside the response; every other record must be reached from
/// it by following those links.
fn one_tree(records: &[Value]) -> Result<(), String> {
    let Some(root) = records.first() else {
        return Err("the trace is not one tree: it has no records".to_owned());
    };
    let mut known: HashSet<&str> = ids_of(root).into_iter().collect();
    let mut reached = vec![false; records.len()];
    reached[0] = true;
    loop {
        let mut grew = false;
        for (index, record) in records.iter().enumerate() {
            if !reached[index] && cause_id(record).is_some_and(|id| known.contains(id)) {
                reached[index] = true;
                known.extend(ids_of(record));
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let cut_off: Vec<usize> = (0..records.len())
        .filter(|index| !reached[*index])
        .collect();
    if cut_off.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the trace is not one tree: records {cut_off:?} cannot be reached from the root"
        ))
    }
}

/// The message a record is about.
fn message_of(record: &Value) -> Option<&str> {
    record
        .get("subject_ids")
        .and_then(|subjects| text(subjects, "message"))
}

/// Checks the gates message by message, so a gate missing from one message is not covered by the
/// same gate of another: a message that went out passed every gate of [`GATES`], and one that
/// did not was blocked at the consent gate.
fn messages_complete(records: &[Value]) -> Result<(), String> {
    let (mut sent, mut skipped) = (0, 0);
    for send in records
        .iter()
        .filter(|r| text(r, "action") == Some("message.send"))
    {
        let message = message_of(send).unwrap_or("(no message)");
        let outcome = text(send, "outcome").unwrap_or_default();
        let wanted: Vec<(&str, Option<&str>)> = match outcome {
            "sent" => {
                sent += 1;
                GATES.iter().map(|gate| ("passed", Some(*gate))).collect()
            }
            "skipped" => {
                skipped += 1;
                vec![("blocked", Some("consent"))]
            }
            other => return Err(format!("message {message} ended `{other}`")),
        };
        for (gate_outcome, reason) in wanted {
            let held = records.iter().any(|record| {
                message_of(record) == Some(message)
                    && describes(record, &("send.gate", gate_outcome, reason))
            });
            if !held {
                return Err(format!(
                    "message {message} has no `send.gate` decision with outcome `{gate_outcome}`                      with reason `{}`",
                    reason.unwrap_or_default()
                ));
            }
        }
    }
    if (sent, skipped) != (SENT, SKIPPED) {
        return Err(format!(
            "the trace has {sent} sent and {skipped} skipped `message.send` decisions, not              {SENT} and {SKIPPED}"
        ));
    }
    Ok(())
}

/// Names the first decision of [`REQUIRED`] that no record describes, the first gate a message
/// lacks, or says the records are not one tree.
pub(super) fn complete(records: &[Value]) -> Result<(), String> {
    for wanted in REQUIRED {
        if !records.iter().any(|record| describes(record, wanted)) {
            let reason = wanted
                .2
                .map_or_else(String::new, |reason| format!(" with reason `{reason}`"));
            return Err(format!(
                "the trace has no `{}` decision with outcome `{}`{reason}",
                wanted.0, wanted.1
            ));
        }
    }
    messages_complete(records)?;
    one_tree(records)
}

/// The records of a trace body.
pub(super) fn records_of(body: &Value) -> Vec<Value> {
    body["records"].as_array().cloned().unwrap_or_default()
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(index, c)| match index {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_trace_id(text: &str) -> bool {
    text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// Gives every id a number in the order it is first met, and keeps the shape of the id.
#[derive(Default)]
struct Ids {
    seen: HashMap<String, usize>,
}

impl Ids {
    fn number(&mut self, id: &str) -> usize {
        let next = self.seen.len() + 1;
        *self.seen.entry(id.to_owned()).or_insert(next)
    }

    fn walk(&mut self, value: &mut Value) {
        match value {
            Value::String(text) if is_uuid(text) => {
                *text = format!("00000000-0000-7000-8000-{:012}", self.number(text));
            }
            Value::String(text) if is_trace_id(text) => {
                *text = format!("{:032x}", self.number(text));
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.walk(item)),
            Value::Object(map) => map.values_mut().for_each(|item| self.walk(item)),
            _ => {}
        }
    }
}

/// The trace as the fixture keeps it: ids numbered in the order they appear, and each record's
/// time a second after the one before, so the same scenario always gives the same bytes.
pub(super) fn normalised(trace_id: &str, mut records: Vec<Value>) -> Value {
    let mut ids = Ids::default();
    let mut shown_trace = Value::String(trace_id.to_owned());
    ids.walk(&mut shown_trace);
    for (index, record) in records.iter_mut().enumerate() {
        if let Some(map) = record.as_object_mut() {
            let at = format!("2031-01-01T00:{:02}:{:02}Z", index / 60, index % 60);
            map.insert("recorded_at".to_owned(), Value::String(at));
        }
        ids.walk(record);
    }
    json!({"trace_id": shown_trace, "records": records, "truncated": false})
}

/// Where the web test reads the trace from.
pub(super) fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/features/traces/fixtures/send-path-trace.json")
}

/// The fixture as committed.
pub(super) fn committed() -> Value {
    let text = std::fs::read_to_string(fixture_path())
        .expect("the trace fixture exists; regenerate it with ZIRV_FLOW_BLESS=1");
    serde_json::from_str(&text).expect("the trace fixture is JSON")
}

/// With `ZIRV_FLOW_BLESS` set, writes `trace` as the fixture; otherwise it must equal the
/// committed one.
pub(super) fn bless_or_compare(trace: &Value) {
    bless_or_compare_at(trace, fixture_path());
}

fn failing_fixture_path() -> PathBuf {
    fixture_path().with_file_name("failing-send-path-trace.json")
}

pub(super) fn bless_failing_or_compare(trace: &Value) {
    bless_or_compare_at(trace, failing_fixture_path());
}

fn bless_or_compare_at(trace: &Value, path: PathBuf) {
    if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
        let parent = path.parent().expect("the fixture has a directory");
        std::fs::create_dir_all(parent).expect("the fixture directory is made");
        let mut text = serde_json::to_string_pretty(trace).expect("the trace serializes");
        text.push('\n');
        std::fs::write(&path, text).expect("the fixture is written");
        return;
    }
    let text = std::fs::read_to_string(path).expect("the trace fixture exists");
    let committed: Value = serde_json::from_str(&text).expect("the trace fixture is JSON");
    assert_eq!(
        &committed, trace,
        "the trace differs from the committed fixture; regenerate it with ZIRV_FLOW_BLESS=1 \
         cargo nextest run -p zirv-flow e2e and review the diff"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_failing_tree_requires_every_attempt_the_failed_send_and_the_feedback() {
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(failing_fixture_path()).unwrap())
                .unwrap();
        let records = records_of(&fixture);
        assert_eq!(failing_complete(&records), Ok(()));
        for index in 0..records.len() {
            let mut missing = records.clone();
            missing.remove(index);
            assert!(
                failing_complete(&missing).is_err(),
                "missing record {index}"
            );
        }
        let mut broken = records;
        broken[17]["cause"]["id"] = json!("unreachable");
        assert!(
            failing_complete(&broken)
                .unwrap_err()
                .contains("not one tree")
        );
    }

    #[test]
    fn a_trace_with_any_one_required_decision_removed_fails_naming_its_action() {
        let trace = committed();
        let records = records_of(&trace);
        assert_eq!(complete(&records), Ok(()));
        for wanted in REQUIRED {
            let without: Vec<Value> = records
                .iter()
                .filter(|record| !describes(record, wanted))
                .cloned()
                .collect();
            let failure = complete(&without).expect_err("a gap is reported");
            assert!(
                failure.contains(&format!("`{}`", wanted.0))
                    && failure.contains(&format!("`{}`", wanted.1)),
                "{failure}"
            );
            if let Some(reason) = wanted.2 {
                assert!(failure.contains(&format!("`{reason}`")), "{failure}");
            }
        }
    }

    #[test]
    fn a_single_gate_missing_from_one_message_fails_naming_the_message_and_the_gate() {
        let records = records_of(&committed());
        let sends: Vec<&Value> = records
            .iter()
            .filter(|r| text(r, "action") == Some("message.send"))
            .collect();
        assert_eq!(sends.len(), SENT + SKIPPED);
        for send in sends {
            let message = message_of(send).expect("a message");
            let skipped = text(send, "outcome") == Some("skipped");
            // A message that went out needs every gate passed; a skipped one, its blocking gate.
            let required: Vec<usize> = records
                .iter()
                .enumerate()
                .filter(|(_, r)| text(r, "action") == Some("send.gate"))
                .filter(|(_, r)| message_of(r) == Some(message))
                .filter(|(_, r)| (text(r, "outcome") == Some("blocked")) == skipped)
                .map(|(index, _)| index)
                .collect();
            assert!(!required.is_empty());
            for index in required {
                let mut without = records.clone();
                let removed = without.remove(index);
                let failure = complete(&without).expect_err("a gap is reported");
                let reason = text(&removed, "reason").unwrap_or_default();
                assert!(
                    failure.contains(message) && failure.contains(&format!("`{reason}`")),
                    "{failure}"
                );
            }
        }
    }

    #[test]
    fn a_trace_that_is_not_one_tree_fails() {
        let records = records_of(&committed());
        assert_eq!(one_tree(&records), Ok(()));

        let mut broken_link = records.clone();
        broken_link[3]["cause"]["id"] = json!("0192b1a4-7c3e-7000-8000-00000000ffff");
        let failure = complete(&broken_link).expect_err("a dangling parent is reported");
        assert!(
            failure.contains("not one tree") && failure.contains("[3]"),
            "{failure}"
        );

        let mut no_root = records.clone();
        no_root.remove(0);
        assert!(one_tree(&no_root).is_err(), "no root record is left");
        assert!(one_tree(&[]).is_err(), "an empty response is no tree");

        // Two records whose causes name each other, and nothing else, are cut off from the root.
        let mut island = records.clone();
        island.push(json!({"id": "a", "cause": {"kind": "event", "id": "b"}, "subject_ids": {}}));
        island.push(json!({"id": "b", "cause": {"kind": "event", "id": "a"}, "subject_ids": {}}));
        let failure = one_tree(&island).expect_err("an island is reported");
        let (first, second) = (records.len(), records.len() + 1);
        assert!(
            failure.contains(&format!("[{first}, {second}]")),
            "{failure}"
        );
    }

    #[test]
    fn the_fixture_carries_no_address_and_every_record_has_ids_only() {
        let text = serde_json::to_string(&committed()).expect("text");
        assert!(
            !text.contains('@'),
            "an address-like value is in the fixture"
        );
        for record in records_of(&committed()) {
            assert!(record["subject_ids"].is_object());
        }
    }

    #[test]
    fn ids_are_numbered_by_first_appearance_and_times_step_by_the_second() {
        let one = "0192b1a4-7c3e-7000-8000-00000000a001";
        let two = "0192b1a4-7c3e-7000-8000-00000000a002";
        let records = vec![
            json!({"id": one, "recorded_at": "2026-01-01T00:00:00Z", "subject_ids": {"x": two}}),
            json!({"id": two, "recorded_at": "2026-01-01T00:00:09Z", "subject_ids": {"x": one}}),
        ];

        let first = normalised("0123456789abcdef0123456789abcdef", records.clone());
        let second = normalised("0123456789abcdef0123456789abcdef", records);

        assert_eq!(first, second);
        assert_eq!(first["trace_id"], "00000000000000000000000000000001");
        assert_eq!(
            first["records"][0]["id"],
            "00000000-0000-7000-8000-000000000002"
        );
        assert_eq!(
            first["records"][1]["subject_ids"]["x"],
            first["records"][0]["id"]
        );
        assert_eq!(first["records"][1]["recorded_at"], "2031-01-01T00:00:01Z");
    }

    async fn event_fixture(pool: sqlx::PgPool, outcome: &str, replayed: bool) {
        use zirv_flow_core::events::{EventMode, UnplannedPolicy};
        use zirv_flow_core::{
            Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
        };
        use zirv_flow_store::{EventOutcome, EventPolicy, EventRecord};

        let ws = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("Events").unwrap(),
            &Slug::parse("events").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let mut input = EventRecord {
            source_record_id: None,
            source: "fixture".into(),
            identifiers: vec![(
                "email".into(),
                zirv_flow_store::decision_testing::PERSONAL_DATA
                    .email
                    .into(),
            )],
            name: "seed".into(),
            properties: Default::default(),
            timestamp: None,
            received_at: time::OffsetDateTime::now_utc(),
            message_id: None,
            trace: TraceContext::root(Cause::api("seed")),
        };
        let seeded = zirv_flow_store::apply_event_batch(&pool, ws, &key, &[input.clone()])
            .await
            .unwrap();
        let EventOutcome::Accepted {
            person_id: person, ..
        } = seeded[0]
        else {
            panic!("seeded person");
        };
        input.name = "fixture".into();
        input.trace = TraceContext::root(Cause::api("fixture"));
        if outcome == "flagged" {
            zirv_flow_store::create_event_schema(
                &pool,
                ws,
                &zirv_flow_store::NewEventSchema {
                    name: input.name.clone(),
                    mode: if replayed {
                        EventMode::Block
                    } else {
                        EventMode::Warn
                    },
                    description: String::new(),
                    properties: serde_json::from_value(json!([
                        {"name": "total", "type": "number", "required": true}
                    ]))
                    .unwrap(),
                },
            )
            .await
            .unwrap();
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            zirv_flow_store::publish_schema_draft(&mut tx, ws, &input.name, None)
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        } else if replayed {
            zirv_flow_store::set_event_policy(
                &pool,
                ws,
                &EventPolicy {
                    unplanned: UnplannedPolicy::Reject,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let applied = zirv_flow_store::apply_event_batch(&pool, ws, &key, &[input.clone()])
            .await
            .unwrap();
        let (trace, triple, subjects) = if replayed {
            let EventOutcome::Rejected {
                reject_id: reject, ..
            } = applied[0]
            else {
                panic!("rejected event");
            };
            if outcome == "flagged" {
                let mut conn = pool.acquire().await.unwrap();
                zirv_flow_store::update_event_schema(
                    &mut conn,
                    ws,
                    &input.name,
                    &zirv_flow_store::EventSchemaUpdate {
                        mode: Some(EventMode::Warn),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            } else {
                zirv_flow_store::set_event_policy(&pool, ws, &EventPolicy::default())
                    .await
                    .unwrap();
            }
            let trace = TraceContext::root(Cause::definition_change(uuid::Uuid::now_v7()));
            let result = zirv_flow_store::replay_rejects(&pool, ws, &key, &[reject], &trace, false)
                .await
                .unwrap();
            assert_eq!(result.stored(), 1);
            let event: uuid::Uuid =
                sqlx::query_scalar("SELECT replay_event_id FROM ingest_rejects WHERE id = $1")
                    .bind(reject)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            (
                trace,
                ("event.replayed", outcome, None),
                json!({"reject": reject, "event": event, "person": person}),
            )
        } else {
            let EventOutcome::Accepted {
                event_id: event, ..
            } = applied[0]
            else {
                panic!("accepted event");
            };
            (
                input.trace,
                ("event.accepted", outcome, None),
                json!({"event": event, "person": person}),
            )
        };
        zirv_flow_store::decision_testing::assert_fixture(&pool, ws, &trace, triple, subjects)
            .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn event_accepted_flagged(pool: sqlx::PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            event_fixture(pool, "flagged", false).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn event_accepted_unplanned(pool: sqlx::PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            event_fixture(pool, "unplanned", false).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn event_replayed_flagged(pool: sqlx::PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            event_fixture(pool, "flagged", true).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn event_replayed_unplanned(pool: sqlx::PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            event_fixture(pool, "unplanned", true).await;
        })
        .await;
    }
}
