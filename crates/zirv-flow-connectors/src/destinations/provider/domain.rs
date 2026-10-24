//! The provider's view of a sending domain: the reply of the manifest's `domain_status` request,
//! mapped to one status per record. Read-only: nothing here marks a domain verified.

use serde_json::Value;

use super::classify::literal_of;
use crate::manifest::DomainStatusSpec;

/// The most records read from one reply.
const MAX_RECORDS: usize = 50;
/// The longest record name or type kept, in characters.
const MAX_TEXT_CHARS: usize = 255;

/// What the provider says of one record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRecordStatus {
    Verified,
    Pending,
    Failed,
    /// The provider's status is not one the manifest maps.
    Unknown,
}

impl ProviderRecordStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Pending => "pending",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// One DNS record of the domain, as the provider reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRecord {
    pub name: String,
    pub record_type: String,
    pub status: ProviderRecordStatus,
}

fn short(text: String) -> Option<String> {
    (!text.is_empty()
        && text.chars().count() <= MAX_TEXT_CHARS
        && !text.chars().any(char::is_control))
    .then_some(text)
}

/// Maps the reply `body` to its records. A record without a usable name or type is left out, and
/// a reply whose records are not where the manifest says is an empty list.
pub fn map(spec: &DomainStatusSpec, body: &Value) -> Vec<ProviderRecord> {
    let records = if spec.records_path.is_empty() {
        Some(body)
    } else {
        body.pointer(&spec.records_path)
    };
    let Some(items) = records.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .take(MAX_RECORDS)
        .filter_map(|item| {
            let name = item
                .pointer(&spec.name)
                .and_then(literal_of)
                .and_then(short)?;
            let record_type = item
                .pointer(&spec.record_type)
                .and_then(literal_of)
                .and_then(short)?;
            let status = item.pointer(&spec.status).and_then(literal_of);
            let status = match status {
                Some(found) if spec.verified.contains(&found) => ProviderRecordStatus::Verified,
                Some(found) if spec.pending.contains(&found) => ProviderRecordStatus::Pending,
                Some(found) if spec.failed.contains(&found) => ProviderRecordStatus::Failed,
                _ => ProviderRecordStatus::Unknown,
            };
            Some(ProviderRecord {
                name,
                record_type,
                status,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn spec(records_path: &str) -> DomainStatusSpec {
        DomainStatusSpec {
            path: "/domains".to_owned(),
            domain_param: "domain".to_owned(),
            records_path: records_path.to_owned(),
            name: "/host".to_owned(),
            record_type: "/kind".to_owned(),
            status: "/state".to_owned(),
            verified: vec!["ok".to_owned()],
            pending: vec!["waiting".to_owned()],
            failed: vec!["bad".to_owned(), "7".to_owned()],
        }
    }

    #[test]
    fn each_record_gets_the_status_its_providers_value_maps_to() {
        let body = json!({"records": [
            {"host": "s1._domainkey.example.test", "kind": "TXT", "state": "ok"},
            {"host": "example.test", "kind": "MX", "state": "waiting"},
            {"host": "x.example.test", "kind": "CNAME", "state": "bad"},
            {"host": "y.example.test", "kind": "CNAME", "state": 7},
            {"host": "z.example.test", "kind": "CNAME", "state": "mystery"},
            {"host": "w.example.test", "kind": "CNAME"}
        ]});

        let records = map(&spec("/records"), &body);

        let statuses: Vec<_> = records
            .iter()
            .map(|record| record.status.as_str())
            .collect();
        assert_eq!(
            statuses,
            [
                "verified", "pending", "failed", "failed", "unknown", "unknown"
            ]
        );
        assert_eq!(records[0].name, "s1._domainkey.example.test");
        assert_eq!(records[1].record_type, "MX");
    }

    #[test]
    fn a_top_level_array_a_wrong_shape_and_unusable_records_are_handled() {
        let top = json!([{"host": "a.test", "kind": "TXT", "state": "ok"}]);

        assert_eq!(map(&spec(""), &top).len(), 1);
        assert!(map(&spec("/records"), &top).is_empty());
        assert!(map(&spec("/records"), &json!({"records": "nope"})).is_empty());
        let unusable = json!({"records": [{"host": "", "kind": "TXT"}, {"kind": "TXT"}, 5]});
        assert!(map(&spec("/records"), &unusable).is_empty());
    }

    #[test]
    fn at_most_fifty_records_are_read() {
        let many: Vec<Value> = (0..80)
            .map(|n| json!({"host": format!("h{n}.test"), "kind": "TXT", "state": "ok"}))
            .collect();

        assert_eq!(map(&spec(""), &Value::Array(many)).len(), MAX_RECORDS);
    }
}
