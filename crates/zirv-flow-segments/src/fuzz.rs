//! The same parser and compiler invariants for proptest and libFuzzer.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use serde_json::Value;
use zirv_flow_core::{people, segment};

use crate::{Query, TraitStorage, compile, generate, parse_and_validate};

/// Parse, validate, round-trip and compile bounded, untrusted JSON against a fixed catalog.
pub fn check_segment_ast(data: &[u8]) {
    if data.len() > 256 * 1024 {
        return;
    }
    let started = Instant::now();
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        // Golden files wrap their input in `definition`.
        let value = value.get("definition").unwrap_or(&value);
        match segment::parse(value) {
            Ok(definition) => {
                // Without float_roundtrip, serde_json itself can change float precision.
                if json_round_trips(value) {
                    let encoded = serde_json::to_vec(&definition).unwrap();
                    let decoded = serde_json::from_slice(&encoded).unwrap();
                    assert_eq!(segment::parse(&decoded), Ok(definition));
                }
            }
            Err(issues) => assert!(!issues.is_empty() && issues.len() <= segment::MAX_ISSUES),
        }
        let catalog = generate::catalog();
        match parse_and_validate(value, &catalog) {
            Ok(definition) => {
                assert_eq!(
                    parse_and_validate(&definition.to_value(), &catalog),
                    Ok(definition.clone())
                );
                match compile(
                    &definition,
                    &catalog,
                    &TraitStorage::default(),
                    generate::NOW,
                ) {
                    Ok(compiled) => check_sql(&compiled.members_sql()),
                    Err(issues) => {
                        assert!(!issues.is_empty() && issues.len() <= segment::MAX_ISSUES)
                    }
                }
            }
            Err(issues) => assert!(!issues.is_empty() && issues.len() <= segment::MAX_ISSUES),
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "segment sample exceeded five seconds"
    );
}

fn json_round_trips(value: &Value) -> bool {
    let encoded = serde_json::to_string(value).unwrap();
    serde_json::from_str::<Value>(&encoded).unwrap() == *value
}

fn check_sql(query: &Query) {
    assert!(!query.sql.contains([';', '"']));
    assert!(!query.sql.contains("--") && !query.sql.contains("/*"));
    // The only SQL literals are compiler-owned type guards and range boundaries.
    let guards = format!(
        "{} {}",
        people::time_sql("j", "t", true),
        people::time_sql("j", "t", false)
    );
    let mut allowed: BTreeSet<&str> = guards.split('\'').skip(1).step_by(2).collect();
    allowed.extend(["", "number", "boolean", "array", "null", "[)", "(]", "[]"]);
    for literal in query.sql.split('\'').skip(1).step_by(2) {
        assert!(
            allowed.contains(literal),
            "input leaked into SQL: {literal:?}"
        );
    }
    let mut referenced = BTreeSet::new();
    for sql in query.sql.split('\'').step_by(2) {
        for token in
            sql.split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '$' | '.'))
        {
            if token.starts_with(|c: char| c.is_ascii_digit()) {
                assert!(
                    matches!(token, "0" | "1" | "1e300"),
                    "unbound SQL number: {token}"
                );
            }
        }
        for tail in sql.split('$').skip(1) {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            let index: usize = digits.parse().expect("a bind placeholder has an index");
            assert!((1..=query.params.len() + 1).contains(&index));
            referenced.insert(index);
        }
    }
    assert_eq!(referenced, (1..=query.params.len() + 1).collect());
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;
    use zirv_flow_core::fuzz::{config, with_fixtures};

    use super::*;

    #[test]
    fn imprecise_json_float_regression() {
        let data = include_bytes!("../tests/fixtures/fuzz/segment_ast-crash-0747684c.bin");
        let value: Value = serde_json::from_slice(data).unwrap();
        assert!(!json_round_trips(&value["definition"]));
        assert!(segment::parse(&value["definition"]).is_ok());
        check_segment_ast(data);
    }

    #[test]
    fn normal_float_keeps_the_model_round_trip_check() {
        let data = br#"{"v":1,"trait":"age","op":"eq","value":1.25}"#;
        let value: Value = serde_json::from_slice(data).unwrap();
        assert!(json_round_trips(&value));
        assert!(segment::parse(&value).is_ok());
        check_segment_ast(data);
    }

    fn inputs() -> BoxedStrategy<Value> {
        let fixtures = [
            include_str!("../tests/fixtures/golden/070-hostile-values.json"),
            include_str!("../tests/fixtures/golden/044-spec-example.json"),
            include_str!("../tests/fixtures/golden/013-trait-timestamp.json"),
            include_str!("../tests/fixtures/golden/030-agg-count.json"),
        ]
        .into_iter()
        .map(|text| serde_json::from_str::<Value>(text).unwrap()["definition"].clone())
        .collect();
        prop_oneof![
            with_fixtures(fixtures),
            generate::definition().prop_map(|definition| definition.to_value()),
            ".{0,64}".prop_map(|text| json!({"v": 1, "trait": "name", "op": "eq", "value": format!("fuzz'; -- {text}")})),
        ].boxed()
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn fuzz_segment_ast(value in inputs()) {
            check_segment_ast(&serde_json::to_vec(&value).unwrap());
        }

        #[test]
        fn fuzz_segment_limits(extra in 0usize..=1) {
            let value = json!({"v": 1, "all": vec![json!({"all": []}); segment::MAX_NODES - 1 + extra]});
            check_segment_ast(&serde_json::to_vec(&value).unwrap());
            prop_assert_eq!(segment::parse(&value).is_ok(), extra == 0);
            let mut value = json!({"all": []});
            for _ in 1..segment::MAX_DEPTH + extra {
                value = json!({"not": value});
            }
            value["v"] = json!(1);
            check_segment_ast(&serde_json::to_vec(&value).unwrap());
            prop_assert_eq!(segment::parse(&value).is_ok(), extra == 0);
        }
    }
}
