//! The compiler entry point: validate, normalize, lower (architecture 8.2). The output is a
//! boolean SQL predicate over a `people` row and the values to bind; running it is the
//! caller's job.

use time::OffsetDateTime;
use zirv_flow_core::segment::{Definition, Issue};

use crate::catalog::Catalog;
use crate::deps::Deps;
use crate::lower::lower;
use crate::normalize::normalize;
use crate::query::{Param, Query};
use crate::storage::TraitStorage;
use crate::validate::validate;

/// A definition lowered to SQL. `now` was fixed at compile time, so the same inputs always give
/// the same text and values.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSegment {
    predicate: String,
    params: Vec<Param>,
    pub deps: Deps,
    /// Whether membership can change as time passes with no new data.
    pub time_sensitive: bool,
}

/// Validates, normalizes and lowers a definition at the explicit time `now`.
pub fn compile(
    definition: &Definition,
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
) -> Result<CompiledSegment, Vec<Issue>> {
    validate(definition, catalog)?;
    let normalized = normalize(definition.clone());
    let lowered = lower(&normalized.root, catalog, storage, now)?;
    Ok(CompiledSegment {
        predicate: lowered.predicate,
        params: lowered.params,
        deps: lowered.deps,
        time_sensitive: lowered.time_sensitive,
    })
}

impl CompiledSegment {
    /// `SELECT count(*)` over the live members.
    pub fn count_sql(&self) -> Query {
        self.query("count(*)", "")
    }

    /// The first `limit` member ids in id order.
    pub fn sample_sql(&self, limit: u32) -> Query {
        let at = self.params.len() + 2;
        let mut query = self.query("p.id", &format!(" ORDER BY p.id LIMIT ${at}::bigint"));
        query.params.push(Param::Int(i64::from(limit)));
        query
    }

    /// Every member id.
    pub fn members_sql(&self) -> Query {
        self.query("p.id", "")
    }

    /// The members among a batch of people. The caller binds the batch last, as a `uuid[]`.
    pub fn batch_sql(&self) -> Query {
        let at = self.params.len() + 2;
        self.query("p.id", &format!(" AND p.id = ANY(${at}::uuid[])"))
    }

    fn query(&self, select: &str, tail: &str) -> Query {
        Query {
            sql: format!(
                "SELECT {select} FROM people p WHERE p.workspace_id = $1 \
                 AND p.merged_into IS NULL AND ({}){tail}",
                self.predicate
            ),
            params: self.params.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    use hashbrown::HashMap;
    use serde_json::{Value, json};
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::consent::Channel;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::segment::{AggFn, ChangeClasses, NODE_KEYS, Node, Op, Window, code};

    use super::*;
    use crate::validate::{Kind, ops_for};

    const NOW: OffsetDateTime = datetime!(2025-06-15 12:00:00 UTC);
    const LIST: &str = "018f0000-0000-7000-8000-000000000001";
    const SEGMENT: &str = "018f0000-0000-7000-8000-000000000002";
    const NEWS: &str = "018f0000-0000-7000-8000-000000000004";

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        for (key, kind) in [
            ("name", TraitType::String),
            ("age", TraitType::Number),
            ("vip", TraitType::Boolean),
            ("signed_up", TraitType::Timestamp),
            ("birthday", TraitType::Date),
            ("tags", TraitType::StringList),
        ] {
            catalog.traits.insert(key.to_owned(), kind);
        }
        let mut order = HashMap::new();
        for (key, kind) in [
            ("total", PropertyType::Number),
            ("items", PropertyType::Integer),
            ("coupon", PropertyType::String),
            ("gift", PropertyType::Boolean),
            ("shipped_at", PropertyType::Timestamp),
            ("due", PropertyType::Date),
            ("labels", PropertyType::StringList),
            ("extra", PropertyType::Object),
        ] {
            order.insert(key.to_owned(), kind);
        }
        catalog.events.insert("order".to_owned(), order);
        catalog.lists.insert(Uuid::parse_str(LIST).unwrap());
        catalog.segments.insert(Uuid::parse_str(SEGMENT).unwrap());
        let mut topics = HashMap::new();
        topics.insert("news".to_owned(), Uuid::parse_str(NEWS).unwrap());
        catalog.topics.insert(Channel::Email, topics);
        catalog
    }

    fn definition(mut value: Value) -> Definition {
        value["v"] = json!(1);
        Definition::from_value(&value).expect("the fixture should parse")
    }

    fn compiled(value: Value) -> CompiledSegment {
        compile(
            &definition(value),
            &catalog(),
            &TraitStorage::default(),
            NOW,
        )
        .expect("the definition should compile")
    }

    fn fixtures() -> Vec<(PathBuf, Value)> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden");
        let mut paths: Vec<PathBuf> = fs::read_dir(dir)
            .expect("the golden directory should exist")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let text = fs::read_to_string(&path).unwrap();
                let value = serde_json::from_str(&text).unwrap();
                (path, value)
            })
            .collect()
    }

    fn compile_fixture(fixture: &Value) -> Result<CompiledSegment, Vec<Issue>> {
        let definition = Definition::from_value(&fixture["definition"])?;
        compile(&definition, &catalog(), &TraitStorage::default(), NOW)
    }

    fn actual(fixture: &Value, compiled: &CompiledSegment) -> Value {
        let query = compiled.members_sql();
        json!({
            "definition": fixture["definition"],
            "sql": query.sql,
            "params": query.params.iter().map(Param::to_value).collect::<Vec<_>>(),
            "deps": compiled.deps.to_value(),
            "time_sensitive": compiled.time_sensitive,
        })
    }

    #[test]
    fn the_golden_corpus_matches_the_compiler() {
        let bless = std::env::var_os("ZIRV_FLOW_BLESS").is_some();
        let mut mismatches = Vec::new();
        for (path, fixture) in fixtures() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let compiled = compile_fixture(&fixture)
                .unwrap_or_else(|issues| panic!("{name} does not compile: {issues:?}"));
            let actual = actual(&fixture, &compiled);
            if bless {
                let mut text = serde_json::to_string_pretty(&actual).unwrap();
                text.push('\n');
                fs::write(&path, text).unwrap();
            } else if fixture != actual {
                mismatches.push(format!(
                    "{name}\n--- expected\n{}\n--- actual\n{}",
                    serde_json::to_string_pretty(&fixture).unwrap(),
                    serde_json::to_string_pretty(&actual).unwrap()
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "run with ZIRV_FLOW_BLESS=1 to rewrite, then review the diff:\n{}",
            mismatches.join("\n\n")
        );
    }

    fn kind_name(kind: Kind) -> String {
        format!("{kind:?}").to_lowercase()
    }

    fn visit(node: &Node, catalog: &Catalog, facts: &mut BTreeSet<String>) {
        match node {
            Node::All(children) | Node::Any(children) if children.is_empty() => {
                facts.insert(format!("const:{}", matches!(node, Node::All(_))));
            }
            Node::All(children) | Node::Any(children) => {
                facts.insert(format!("node:{}", node.key()));
                for child in children {
                    visit(child, catalog, facts);
                }
            }
            Node::Not(child) => {
                facts.insert("node:not".to_owned());
                facts.insert(format!("not:{}", child.key()));
                visit(child, catalog, facts);
            }
            Node::Trait(condition) => {
                facts.insert("node:trait".to_owned());
                let kind = Kind::from(catalog.traits[&condition.key]);
                facts.insert(format!(
                    "trait:{}:{}",
                    kind_name(kind),
                    condition.op.as_str()
                ));
            }
            Node::Event(condition) => {
                facts.insert("node:event".to_owned());
                let schema = &catalog.events[&condition.name];
                for filter in &condition.filters {
                    let kind = Kind::from(schema[&filter.prop]);
                    facts.insert(format!("prop:{}:{}", kind_name(kind), filter.op.as_str()));
                }
                if let Some(agg) = &condition.agg {
                    facts.insert(format!("agg:{}", agg.func.as_str()));
                    let kind = match agg.func {
                        AggFn::FirstTime | AggFn::LastTime => "timestamp",
                        _ => "number",
                    };
                    facts.insert(format!("aggop:{kind}:{}", agg.op.as_str()));
                }
                let window = match &condition.window {
                    None => "none",
                    Some(Window::Last(_)) => "last",
                    Some(Window::Between(..)) => "between",
                    Some(Window::SinceTrait(_)) => "since_trait",
                };
                facts.insert(format!("window:{window}"));
            }
            other => {
                facts.insert(format!("node:{}", other.key()));
            }
        }
    }

    fn required() -> BTreeSet<String> {
        let mut required = BTreeSet::new();
        for key in NODE_KEYS {
            required.insert(format!("node:{key}"));
        }
        for key in ["trait", "event", "list", "segment", "consent", "identifier"] {
            required.insert(format!("not:{key}"));
        }
        for value in [true, false] {
            required.insert(format!("const:{value}"));
        }
        for window in ["none", "last", "between", "since_trait"] {
            required.insert(format!("window:{window}"));
        }
        for func in AggFn::ALL {
            required.insert(format!("agg:{}", func.as_str()));
        }
        let trait_kinds = [
            Kind::String,
            Kind::Number,
            Kind::Boolean,
            Kind::Timestamp,
            Kind::Date,
            Kind::StringList,
        ];
        let property_kinds = [
            Kind::String,
            Kind::Number,
            Kind::Integer,
            Kind::Boolean,
            Kind::Timestamp,
            Kind::Date,
            Kind::StringList,
            Kind::Object,
        ];
        for (prefix, kinds) in [("trait", &trait_kinds[..]), ("prop", &property_kinds[..])] {
            for kind in kinds {
                for op in ops_for(*kind) {
                    required.insert(format!("{prefix}:{}:{}", kind_name(*kind), op.as_str()));
                }
            }
        }
        for (name, kind) in [("number", Kind::Number), ("timestamp", Kind::Timestamp)] {
            for op in ops_for(kind) {
                if !matches!(op, Op::Exists | Op::NotExists) {
                    required.insert(format!("aggop:{name}:{}", op.as_str()));
                }
            }
        }
        required
    }

    #[test]
    fn the_golden_corpus_covers_every_node_operator_aggregate_and_window() {
        let catalog = catalog();
        let mut covered = BTreeSet::new();
        for (path, fixture) in fixtures() {
            let parsed = Definition::from_value(&fixture["definition"])
                .unwrap_or_else(|issues| panic!("{path:?} does not parse: {issues:?}"));
            visit(&normalize(parsed).root, &catalog, &mut covered);
        }
        let missing: Vec<String> = required().difference(&covered).cloned().collect();
        assert!(
            missing.is_empty(),
            "the golden corpus does not cover: {missing:?}"
        );
    }

    #[test]
    fn every_fixture_carries_a_blessed_result() {
        for (path, fixture) in fixtures() {
            for key in ["definition", "sql", "params", "deps", "time_sensitive"] {
                assert!(fixture.get(key).is_some(), "{path:?} has no {key}");
            }
        }
    }

    #[test]
    fn compiling_twice_gives_identical_output() {
        for (path, fixture) in fixtures() {
            let first = compile_fixture(&fixture).unwrap();
            let second = compile_fixture(&fixture).unwrap();
            assert_eq!(first, second, "{path:?}");
            assert_eq!(first.members_sql(), second.members_sql());
        }
    }

    #[test]
    fn a_hostile_key_or_value_is_only_ever_a_parameter() {
        let key = "k'; DROP TABLE x; -- $1 \" \\";
        let value = "v'); DELETE FROM people; -- $2";
        let mut catalog = catalog();
        catalog.traits.insert(key.to_owned(), TraitType::String);
        catalog
            .events
            .get_mut("order")
            .unwrap()
            .insert(key.to_owned(), PropertyType::String);
        let definition = definition(json!({"all": [
            {"trait": key, "op": "eq", "value": value},
            {"trait": key, "op": "contains", "value": value},
            {"trait": key, "op": "in", "value": [value, "b"]},
            {"event": "order", "where": [{"prop": key, "op": "starts_with", "value": value}]},
        ]}));
        let compiled = compile(&definition, &catalog, &TraitStorage::default(), NOW).unwrap();
        let query = compiled.members_sql();
        for fragment in ["DROP", "DELETE", "--", "\"", "\\", "k'", "v'"] {
            assert!(
                !query.sql.contains(fragment),
                "{fragment:?} reached the SQL: {}",
                query.sql
            );
        }
        let list = Param::TextArray(vec!["b".to_owned(), value.to_owned()]);
        assert!(query.params.contains(&Param::Text(key.to_owned())));
        assert!(query.params.contains(&Param::Text(value.to_owned())));
        assert!(query.params.contains(&list));
        assert!(query.params.contains(&Param::Jsonb(json!({ key: value }))));
    }

    fn deps_of(value: Value) -> (Deps, bool) {
        let compiled = compiled(value);
        (compiled.deps, compiled.time_sensitive)
    }

    #[test]
    fn every_compiled_segment_depends_on_merges_and_only_the_classes_it_reads() {
        let cases = [
            (json!({"all": []}), ChangeClasses::MERGE),
            (
                json!({"trait": "age", "op": "exists"}),
                ChangeClasses::TRAITS | ChangeClasses::MERGE,
            ),
            (
                json!({"event": "order"}),
                ChangeClasses::EVENTS | ChangeClasses::MERGE,
            ),
            (
                json!({"event": "order", "window": {"since_trait": "signed_up"}}),
                ChangeClasses::EVENTS | ChangeClasses::TRAITS | ChangeClasses::MERGE,
            ),
            (
                json!({"list": LIST}),
                ChangeClasses::LISTS | ChangeClasses::MERGE,
            ),
            (json!({"segment": SEGMENT}), ChangeClasses::MERGE),
            (
                json!({"consent": {"channel": "email", "status": "subscribed"}}),
                ChangeClasses::CONSENT | ChangeClasses::MERGE,
            ),
            (
                json!({"identifier": "email"}),
                ChangeClasses::IDENTIFIERS | ChangeClasses::MERGE,
            ),
        ];
        for (value, expected) in cases {
            let (deps, _) = deps_of(value.clone());
            assert_eq!(deps.classes, expected, "{value}");
        }
    }

    #[test]
    fn deps_list_what_the_definition_reads() {
        let (deps, _) = deps_of(json!({"all": [
            {"trait": "age", "op": "gt", "value": 3},
            {"event": "order", "window": {"since_trait": "signed_up"}},
            {"list": LIST},
            {"not": {"segment": SEGMENT}},
            {"consent": {"channel": "email", "topic": "news", "status": "subscribed"}},
            {"consent": {"channel": "email", "status": "unknown"}},
            {"identifier": "external:crm"},
        ]}));
        let strings = |items: &[&str]| -> BTreeSet<String> {
            items.iter().map(|item| (*item).to_owned()).collect()
        };
        assert_eq!(deps.traits, strings(&["age", "signed_up"]));
        assert_eq!(deps.events, strings(&["order"]));
        assert_eq!(deps.lists, BTreeSet::from([Uuid::parse_str(LIST).unwrap()]));
        assert_eq!(
            deps.segments,
            BTreeSet::from([Uuid::parse_str(SEGMENT).unwrap()])
        );
        assert_eq!(
            deps.consent,
            BTreeSet::from([
                ("email".to_owned(), None),
                ("email".to_owned(), Some("news".to_owned())),
            ])
        );
        assert_eq!(deps.identifiers, strings(&["external:crm"]));
    }

    #[test]
    fn only_a_last_window_or_a_relative_operator_is_time_sensitive() {
        let sensitive = [
            json!({"event": "order", "window": {"last": "P7D"}}),
            json!({"trait": "signed_up", "op": "within_last", "value": "P7D"}),
            json!({"trait": "birthday", "op": "more_than_ago", "value": "P1D"}),
            json!({"trait": "signed_up", "op": "within_next", "value": "P7D"}),
            json!({"event": "order", "where": [
                {"prop": "shipped_at", "op": "within_last", "value": "P1D"}]}),
            json!({"event": "order",
                "agg": {"fn": "first_time", "op": "within_last", "value": "P1D"}}),
            json!({"event": "order",
                "agg": {"fn": "last_time", "op": "more_than_ago", "value": "P1D"}}),
            json!({"not": {"event": "order", "window": {"last": "P7D"}}}),
        ];
        for value in sensitive {
            assert!(deps_of(value.clone()).1, "{value} should be time sensitive");
        }
        let steady = [
            json!({"event": "order", "window": {"between": [
                "2025-01-01T00:00:00Z", "2025-02-01T00:00:00Z"]}}),
            json!({"event": "order", "window": {"since_trait": "signed_up"}}),
            json!({"trait": "signed_up", "op": "before", "value": "2025-01-01T00:00:00Z"}),
            json!({"event": "order",
                "agg": {"fn": "last_time", "op": "after", "value": "2025-01-01T00:00:00Z"}}),
            json!({"list": LIST}),
            json!({"all": []}),
        ];
        for value in steady {
            assert!(
                !deps_of(value.clone()).1,
                "{value} should not be time sensitive"
            );
        }
    }

    fn references(sql: &str, index: usize) -> usize {
        let needle = format!("${index}");
        sql.match_indices(&needle)
            .filter(|(at, _)| {
                !sql[at + needle.len()..]
                    .chars()
                    .next()
                    .is_some_and(|next| next.is_ascii_digit())
            })
            .count()
    }

    #[test]
    fn the_workspace_is_bound_once_and_the_batch_comes_last() {
        let compiled = compiled(json!({"all": [
            {"trait": "name", "op": "in", "value": ["a", "b"]},
            {"trait": "age", "op": "between", "value": [1, 2]},
            {"trait": "name", "op": "contains", "value": "x"},
            {"trait": "age", "op": "gt", "value": 3},
            {"trait": "signed_up", "op": "before", "value": "2025-01-01T00:00:00Z"},
            {"list": LIST},
            {"segment": SEGMENT},
            {"identifier": "email"},
            {"event": "order", "window": {"last": "P7D"}},
            {"consent": {"channel": "email", "topic": "news", "status": "subscribed"}},
        ]}));
        let count = compiled.params.len();
        assert!(count > 10, "the fixture should pass $10");
        let members = compiled.members_sql();
        assert_eq!(members.params.len(), count);
        assert_eq!(references(&members.sql, 1), 1);
        for index in 2..count + 2 {
            assert!(references(&members.sql, index) >= 1, "${index} is unused");
        }
        assert_eq!(references(&members.sql, count + 2), 0);

        let batch = compiled.batch_sql();
        assert_eq!(batch.params.len(), count);
        assert_eq!(references(&batch.sql, 1), 1);
        assert_eq!(references(&batch.sql, count + 2), 1);
        let tail = format!("AND p.id = ANY(${}::uuid[])", count + 2);
        assert!(batch.sql.ends_with(&tail));
    }

    #[test]
    fn every_query_starts_from_the_live_people_of_the_workspace() {
        let compiled = compiled(json!({"trait": "age", "op": "gt", "value": 3}));
        let prefix = "FROM people p WHERE p.workspace_id = $1 AND p.merged_into IS NULL AND (";
        let count = compiled.count_sql();
        assert!(count.sql.starts_with("SELECT count(*) "));
        assert!(count.sql.contains(prefix));
        let members = compiled.members_sql();
        assert!(members.sql.starts_with("SELECT p.id "));
        assert!(members.sql.contains(prefix));
        let sample = compiled.sample_sql(25);
        assert!(sample.sql.contains(prefix));
        let limit = format!("ORDER BY p.id LIMIT ${}::bigint", compiled.params.len() + 2);
        assert!(sample.sql.ends_with(&limit));
        assert_eq!(sample.params.last(), Some(&Param::Int(25)));
        assert_eq!(sample.params.len(), compiled.params.len() + 1);
        assert!(compiled.batch_sql().sql.contains(prefix));
    }

    #[test]
    fn the_constants_lower_to_true_and_false() {
        let all = compiled(json!({"all": []})).members_sql().sql;
        let any = compiled(json!({"any": []})).members_sql().sql;
        assert!(all.ends_with("AND (true)"));
        assert!(any.ends_with("AND (false)"));
    }

    #[test]
    fn relative_bounds_come_from_the_explicit_now() {
        let window = json!({"event": "order", "window": {"last": "P1D"}});
        let at_noon = compiled(window.clone());
        let later = compile(
            &definition(window),
            &catalog(),
            &TraitStorage::default(),
            datetime!(2025-06-16 12:00:00 UTC),
        )
        .unwrap();
        assert_ne!(at_noon.members_sql().params, later.members_sql().params);
        assert_eq!(at_noon.members_sql().sql, later.members_sql().sql);
        assert!(!at_noon.members_sql().sql.contains("now()"));
        let from = Param::Timestamp(datetime!(2025-06-14 12:00:00 UTC));
        assert!(at_noon.members_sql().params.contains(&from));
    }

    #[test]
    fn an_invalid_definition_is_refused_with_its_issues() {
        let issues = compile(
            &definition(json!({"trait": "ghost", "op": "exists"})),
            &catalog(),
            &TraitStorage::default(),
            NOW,
        )
        .unwrap_err();
        assert_eq!(issues, vec![Issue::new("/trait", code::UNKNOWN_TRAIT)]);
        let unsupported = Definition {
            v: 2,
            root: Node::All(vec![]),
        };
        assert!(compile(&unsupported, &catalog(), &TraitStorage::default(), NOW).is_err());
    }

    #[test]
    fn a_consent_topic_resolves_to_its_id_and_the_channel_has_no_topic_filter() {
        let topic = compiled(
            json!({"consent": {"channel": "email", "topic": "news", "status": "subscribed"}}),
        );
        let id = Param::Uuid(Uuid::parse_str(NEWS).unwrap());
        assert!(topic.members_sql().params.contains(&id));
        let channel = compiled(json!({"consent": {"channel": "email", "status": "subscribed"}}));
        let query = channel.members_sql();
        assert!(query.sql.contains("c.topic_id IS NULL"));
        assert!(!query.params.contains(&id));
    }

    fn sql_of(value: Value) -> String {
        compiled(value).members_sql().sql
    }

    #[test]
    fn every_temporal_cast_is_guarded_by_a_shape_and_validity_check() {
        let timestamp = "^[0-9]{4}-[0-9]{2}-[0-9]{2}[Tt ][0-9]{2}:[0-9]{2}:[0-9]{2}([.][0-9]+)?([Zz]|[+-][0-9]{2}:[0-9]{2})$";
        let date = "^[0-9]{4}-[0-9]{2}-[0-9]{2}$";
        let cases = [
            (
                json!({"trait": "signed_up", "op": "before", "value": "2025-01-01T00:00:00Z"}),
                "(p.traits ->> $2::text)",
                "timestamptz",
            ),
            (
                json!({"trait": "birthday", "op": "within_last", "value": "P7D"}),
                "(p.traits ->> $2::text)",
                "date",
            ),
            (
                json!({"event": "order", "where": [
                    {"prop": "shipped_at", "op": "before", "value": "2025-01-01T00:00:00Z"}]}),
                "(e.properties ->> $3::text)",
                "timestamptz",
            ),
            (
                json!({"event": "order", "where": [
                    {"prop": "due", "op": "after", "value": "2025-01-01"}]}),
                "(e.properties ->> $3::text)",
                "date",
            ),
            (
                json!({"event": "order", "window": {"since_trait": "birthday"}}),
                "(p.traits ->> $3::text)",
                "date",
            ),
            (
                json!({"event": "order", "window": {"since_trait": "signed_up"}}),
                "(p.traits ->> $3::text)",
                "timestamptz",
            ),
        ];
        for (value, text, sql_type) in cases {
            let shape = if sql_type == "date" { date } else { timestamp };
            let guard = format!(
                "= 'string' THEN CASE WHEN {text} ~ '{shape}' \
                 AND pg_input_is_valid({text}, '{sql_type}') THEN "
            );
            assert!(
                sql_of(value.clone()).contains(&guard),
                "{value}: no guarded cast"
            );
        }
    }

    #[test]
    fn an_integer_property_treats_a_fractional_value_as_missing() {
        let filter = sql_of(json!({"event": "order", "where": [
            {"prop": "items", "op": "gt", "value": 1}]}));
        assert!(filter.contains("= trunc("), "{filter}");
        let sum = sql_of(json!({"event": "order",
            "agg": {"fn": "sum", "prop": "items", "op": "gt", "value": 1}}));
        assert!(sum.contains("= trunc("), "{sum}");
        let number = sql_of(json!({"event": "order", "where": [
            {"prop": "total", "op": "gt", "value": 1}]}));
        assert!(!number.contains("trunc("), "{number}");
        let total = sql_of(json!({"event": "order",
            "agg": {"fn": "sum", "prop": "total", "op": "gt", "value": 1}}));
        assert!(!total.contains("trunc("), "{total}");
    }

    #[test]
    fn a_missing_since_trait_bound_fails_the_whole_event_condition() {
        let count = sql_of(
            json!({"event": "order", "window": {"since_trait": "signed_up"},
            "agg": {"fn": "count", "op": "eq", "value": 0}}),
        );
        assert!(
            count.contains(" END END IS NOT NULL AND COALESCE(((SELECT count(*)::float8"),
            "{count}"
        );
        let exists = sql_of(json!({"event": "order", "window": {"since_trait": "signed_up"}}));
        assert!(
            exists.contains(" END END IS NOT NULL AND EXISTS (SELECT 1 FROM events e"),
            "{exists}"
        );
        let plain = sql_of(json!({"event": "order",
            "agg": {"fn": "count", "op": "eq", "value": 0}}));
        assert!(!plain.contains("IS NOT NULL"), "{plain}");
    }

    #[test]
    fn string_and_boolean_equality_is_a_bare_containment() {
        for value in [
            json!({"trait": "name", "op": "eq", "value": "Ada"}),
            json!({"trait": "vip", "op": "eq", "value": true}),
            json!({"event": "order", "where": [
                {"prop": "coupon", "op": "eq", "value": "X"}]}),
        ] {
            let sql = sql_of(value.clone());
            assert!(sql.contains(" @> $"), "{sql}");
            assert!(!sql.contains("COALESCE((p.traits @>"), "{sql}");
            assert!(!sql.contains("COALESCE((e.properties @>"), "{sql}");
        }
        let sql = sql_of(json!({"trait": "name", "op": "eq", "value": "Ada"}));
        assert!(sql.ends_with("AND (p.traits @> $2::jsonb)"), "{sql}");
    }

    #[test]
    fn every_golden_parameter_is_referenced_by_the_sql() {
        for (path, fixture) in fixtures() {
            let compiled = compile_fixture(&fixture).unwrap();
            let query = compiled.members_sql();
            for index in 2..query.params.len() + 2 {
                assert!(
                    references(&query.sql, index) >= 1,
                    "{path:?}: ${index} is unused"
                );
            }
        }
    }
}
