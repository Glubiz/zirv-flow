//! Bounded inputs shared by the parser properties. Available only for tests and fuzzing.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use serde_json::Value;

/// A fixed floor on every platform; the nightly job raises the case count.
pub fn config() -> Config {
    Config {
        cases: std::env::var("ZIRV_FLOW_FUZZ_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|cases| *cases > 0)
            .unwrap_or(64),
        rng_seed: RngSeed::Fixed(387),
        failure_persistence: None,
        ..Config::default()
    }
}

/// All JSON kinds, with at most four recursive levels, 64 leaves and short strings.
pub fn json() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        (-4096i32..4096).prop_map(|n| Value::from(f64::from(n) / 4.0)),
        ".{0,64}".prop_map(Value::String),
    ];
    leaf.prop_recursive(4, 64, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(".{0,24}", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
    .boxed()
}

/// Arbitrary JSON, unchanged seeds, and seeds with a randomly selected subtree replaced.
pub fn with_fixtures(fixtures: Vec<Value>) -> BoxedStrategy<Value> {
    let seeds = prop::sample::select(fixtures);
    let mutated =
        (seeds.clone(), any::<usize>(), json()).prop_map(|(mut value, index, replacement)| {
            let mut paths = Vec::new();
            paths_of(&value, String::new(), &mut paths);
            *value.pointer_mut(&paths[index % paths.len()]).unwrap() = replacement;
            value
        });
    prop_oneof![1 => json(), 1 => seeds, 4 => mutated].boxed()
}

fn paths_of(value: &Value, path: String, paths: &mut Vec<String>) {
    paths.push(path.clone());
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                paths_of(item, format!("{path}/{index}"), paths);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let key = key.replace('~', "~0").replace('/', "~1");
                paths_of(item, format!("{path}/{key}"), paths);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[test]
    fn every_fuzz_target_calls_a_checker_exercised_by_a_property() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = fs::read_to_string(root.join("fuzz/Cargo.toml")).unwrap();
        let mut targets = 0;
        for bin in manifest.split("[[bin]]").skip(1) {
            let field = |name: &str| {
                bin.lines()
                    .find_map(|line| line.strip_prefix(&format!("{name} = \"")))
                    .unwrap()
                    .trim_end_matches('"')
            };
            let name = field("name");
            let target = fs::read_to_string(root.join("fuzz").join(field("path"))).unwrap();
            let (crate_name, file) = match name {
                "segment_ast" => ("segments", "fuzz.rs"),
                "manifest" => ("connectors", "manifest/fuzz.rs"),
                "template_document" => ("templates", "fuzz.rs"),
                "sanitize_source" => ("templates", "content/blocks/sanitize.rs"),
                _ => panic!("add a normal-suite property for fuzz target {name}"),
            };
            let checker = format!("check_{name}(");
            assert!(
                target.contains(&checker),
                "{name} does not call its checker"
            );
            let source =
                fs::read_to_string(root.join(format!("crates/zirv-flow-{crate_name}/src/{file}")))
                    .unwrap();
            let tests = source.rsplit_once("#[cfg(test)]").unwrap().1;
            let property = tests
                .split_once(&format!("fn fuzz_{name}("))
                .unwrap_or_else(|| panic!("{name} has no property"))
                .1
                .split("\n        }")
                .next()
                .unwrap();
            assert!(tests.contains("proptest!"));
            assert!(
                property.contains(&checker),
                "{name}'s property bypasses its checker"
            );
            targets += 1;
        }
        assert_eq!(targets, 4);
    }
}
