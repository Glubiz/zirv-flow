//! Manifest invariants shared by the normal suite and libFuzzer.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::{MAX_MANIFEST_BYTES, issue, validate};

/// Validate bounded JSON, including inputs just above the product's byte limit.
pub fn check_manifest(data: &[u8]) {
    if data.len() > MAX_MANIFEST_BYTES * 2 {
        return;
    }
    let started = Instant::now();
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        match validate(&value) {
            Ok(_) => {
                // Manifest is a runtime model, not a wire serializer. Its accepted wire value
                // must survive JSON serialization and remain accepted by the same validator.
                let encoded = serde_json::to_vec(&value).unwrap();
                let decoded: Value = serde_json::from_slice(&encoded).unwrap();
                assert_eq!(value, decoded);
                assert!(validate(&decoded).is_ok());
            }
            Err(issues) => assert!(!issues.is_empty() && issues.len() <= issue::MAX_ISSUES),
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "manifest sample exceeded five seconds"
    );
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;
    use zirv_flow_core::fuzz::{config, with_fixtures};

    use super::*;
    use crate::manifest::{
        MAX_DEPTH, code,
        parse::{MAX_FIELDS, MAX_STREAMS},
    };

    fn fixtures() -> Vec<Value> {
        [
            include_str!("../../tests/fixtures/manifests/cursor_oauth2_code.json"),
            include_str!("../../tests/fixtures/manifests/offset_api_key.json"),
            include_str!("../../tests/fixtures/manifests/page_basic_soft_delete.json"),
            include_str!("../../tests/fixtures/manifests/link_header_bearer.json"),
            include_str!("../../tests/fixtures/providers/bearer_timestamped.json"),
        ]
        .into_iter()
        .map(|text| serde_json::from_str(text).unwrap())
        .collect()
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn fuzz_manifest(value in with_fixtures(fixtures())) {
            check_manifest(&serde_json::to_vec(&value).unwrap());
        }

        #[test]
        fn fuzz_manifest_limits(extra in 0usize..=1) {
            for (value, limit_code) in [
                (json!({"streams": vec![json!({}); MAX_STREAMS + extra]}), code::TOO_MANY_STREAMS),
                (json!({"streams": [{"fields": vec![json!({}); MAX_FIELDS + extra]}]}), code::TOO_MANY_FIELDS),
                (json!("x".repeat(MAX_MANIFEST_BYTES - 2 + extra)), code::MANIFEST_TOO_LARGE),
            ] {
                check_manifest(&serde_json::to_vec(&value).unwrap());
                let issues = validate(&value).unwrap_err();
                prop_assert_eq!(issues.iter().any(|issue| issue.code == limit_code), extra == 1);
            }
            let mut value = json!({});
            for _ in 1..MAX_DEPTH + extra {
                value = json!([value]);
            }
            check_manifest(&serde_json::to_vec(&value).unwrap());
            prop_assert_eq!(validate(&value).unwrap_err().iter().any(|issue| issue.code == code::TOO_DEEP), extra == 1);
        }
    }
}
