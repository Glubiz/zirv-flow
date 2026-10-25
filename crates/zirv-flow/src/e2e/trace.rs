//! The trace of the scenario as data: the decisions a complete trace holds, the check that names
//! the one that is missing, and the normal form of the trace the web test reads as a fixture.

use std::path::PathBuf;

use hashbrown::HashMap;
use serde_json::{Value, json};

/// The decisions the trace of the send path must hold: action, outcome and, when it matters, the
/// reason. The send gates are listed one by one, in the order the pipeline runs them.
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
    // The gates of a message that went out, and the message.
    ("send.gate", "passed", Some("enrollment")),
    ("send.gate", "passed", Some("template")),
    ("send.gate", "passed", Some("sender")),
    ("send.gate", "passed", Some("address")),
    ("send.gate", "passed", Some("consent")),
    ("send.gate", "passed", Some("suppression")),
    ("send.gate", "passed", Some("quiet_hours")),
    ("send.gate", "passed", Some("frequency_cap")),
    ("send.gate", "passed", Some("render")),
    ("send.gate", "passed", Some("rate_limit")),
    ("send.gate", "passed", Some("delivery")),
    ("message.send", "sent", None),
    // The unsubscribe and the delivery feedback.
    ("consent.change", "changed", Some("unsubscribe_link")),
    ("message.feedback", "recorded", Some("delivered")),
    // The later send, stopped at the consent gate with its exact reason.
    ("send.gate", "blocked", Some("consent")),
    ("message.send", "skipped", Some("no_consent")),
];

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

/// Names the first decision of [`REQUIRED`] that no record describes.
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
    Ok(())
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
    if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
        let path = fixture_path();
        let parent = path.parent().expect("the fixture has a directory");
        std::fs::create_dir_all(parent).expect("the fixture directory is made");
        let mut text = serde_json::to_string_pretty(trace).expect("the trace serializes");
        text.push('\n');
        std::fs::write(&path, text).expect("the fixture is written");
        return;
    }
    assert_eq!(
        &committed(),
        trace,
        "the trace differs from the committed fixture; regenerate it with ZIRV_FLOW_BLESS=1 \
         cargo nextest run -p zirv-flow e2e and review the diff"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
