//! Proptest generators for the reference evaluator and the PostgreSQL comparison (architecture
//! section 18; rulings 3724f651, 887b9eef and e6659f67). Behind the `test-support` feature, and
//! in this crate's own tests.
//!
//! The generators draw from small pools on purpose: a definition and a person only agree often
//! enough to be interesting when they share the values, so the strings, numbers, instants and
//! dates a definition compares with are the ones a person stores, at the edges of every window.
//! The data reaches what the write path would refuse and the SQL must still read the same way:
//! missing and JSON-null values, wrong JSON types, time strings in the shapes PostgreSQL and
//! the evaluator could disagree on, fractions in integer properties, numbers just inside and
//! outside `1e300`, empty arrays, zero events, and a missing `since_trait` bound.
//!
//! Every number is exactly representable (small integers and dyadic fractions, plus the
//! `1e300` edges), and every instant sits on a whole second, so a sum or an average gives the
//! same bits whatever the order of the events.

use std::env;

use hashbrown::HashMap;
use proptest::collection::{btree_set, vec};
use proptest::option;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use serde_json::{Map, Value, json};
use time::macros::datetime;
use time::{Duration, OffsetDateTime, UtcOffset};
use uuid::Uuid;
use zirv_flow_core::consent::{Channel, ConsentStatus};
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::TraitType;
use zirv_flow_core::segment::{
    AggFn, Aggregate, ConsentCondition, Definition, EventCondition, Node, Op, PropertyFilter,
    TraitCondition, Window,
};

use crate::catalog::Catalog;
use crate::snapshot::{PersonSnapshot, SnapshotConsent, SnapshotEvent};
use crate::validate::{Kind, ops_for};

/// The time every case is compiled and evaluated at.
pub const NOW: OffsetDateTime = datetime!(2025-06-15 12:00:00 UTC);

/// The most people one case holds.
pub const MAX_PEOPLE: usize = 30;

/// The environment variable that sets the number of cases.
pub const CASES_VAR: &str = "ZIRV_FLOW_PROPTEST_CASES";
/// The environment variable that sets the seed, a `u64`. The nightly run sets it at random and
/// prints it.
pub const SEED_VAR: &str = "ZIRV_FLOW_PROPTEST_SEED";

const DEFAULT_CASES: u32 = 64;
const DEFAULT_SEED: u64 = 0x5eed_2025_0615;

const LISTS: [u128; 2] = [
    0x018f_0000_0000_7000_8000_0000_0000_0011,
    0x018f_0000_0000_7000_8000_0000_0000_0012,
];
const SEGMENTS: [u128; 2] = [
    0x018f_0000_0000_7000_8000_0000_0000_0021,
    0x018f_0000_0000_7000_8000_0000_0000_0022,
];
const TOPICS: [(&str, u128); 2] = [
    ("news", 0x018f_0000_0000_7000_8000_0000_0000_0031),
    ("offers", 0x018f_0000_0000_7000_8000_0000_0000_0032),
];

const TRAITS: [(&str, TraitType); 8] = [
    ("name", TraitType::String),
    ("country", TraitType::String),
    ("age", TraitType::Number),
    ("vip", TraitType::Boolean),
    ("signed_up", TraitType::Timestamp),
    ("renewed", TraitType::Timestamp),
    ("birthday", TraitType::Date),
    ("tags", TraitType::StringList),
];

const ORDER: [(&str, PropertyType); 9] = [
    ("total", PropertyType::Number),
    ("score", PropertyType::Number),
    ("items", PropertyType::Integer),
    ("coupon", PropertyType::String),
    ("gift", PropertyType::Boolean),
    ("shipped_at", PropertyType::Timestamp),
    ("due", PropertyType::Date),
    ("labels", PropertyType::StringList),
    ("extra", PropertyType::Object),
];

const VISIT: [(&str, PropertyType); 3] = [
    ("url", PropertyType::String),
    ("pages", PropertyType::Integer),
    ("seen_at", PropertyType::Timestamp),
];

/// The events and the properties of their current schema.
const EVENTS: [(&str, &[(&str, PropertyType)]); 2] = [("order", &ORDER), ("visit", &VISIT)];

/// The properties an aggregate may read. `score` is left out: it holds the `1e300` edges, which
/// would make a sum depend on the order of the events.
const AGGREGATED: [(&str, &[&str]); 2] = [("order", &["total", "items"]), ("visit", &["pages"])];

const IDENTIFIERS: [&str; 4] = ["email", "phone", "anonymous_id", "external:crm"];

const WORDS: [&str; 10] = ["", "a", "b", "ab", "abc", "A", "5", "true", "é", "日本"];
const DURATIONS: [&str; 5] = ["PT1H", "PT36H", "P1D", "P7D", "P30D"];
const DURATION_HOURS: [i64; 5] = [1, 36, 24, 168, 720];

/// Time strings the write path refuses and PostgreSQL may or may not take, and the other way
/// round (`notes.md` of #119). The data stores them in both trait and event-property position.
pub const TIMESTAMP_ODDITIES: [&str; 22] = [
    "infinity",
    "-infinity",
    "now",
    "epoch",
    "today",
    "10000-01-01T00:00:00Z",
    "2025-06-15T12:00:00Z\n",
    "0000-01-01T00:00:00Z",
    "2025-06-15T12:00:00+16:00",
    "2025-06-15T12:00:00-23:59",
    "2025-06-15T12:00:00+15:59",
    "2025-06-15T24:00:00Z",
    "2025-06-15T23:59:60Z",
    "2025-06-15T12:00:00.5Z",
    "2025-02-30T00:00:00Z",
    "2025-06-15T12:60:00Z",
    "2025-06-15",
    "2025-06-15T12:00:00",
    "2025-06-15T12:00Z",
    "2025-06-15 12:00:00 +02:00",
    "2025-06-15T12:00:00+0200",
    "yesterday",
];

/// The same for dates.
pub const DATE_ODDITIES: [&str; 12] = [
    "infinity",
    "now",
    "epoch",
    "today",
    "10000-01-01",
    "0000-01-01",
    "2025-06-15\n",
    "2025-02-30",
    "2025-13-01",
    "2025-6-5",
    "20250615",
    "2025-06-15T00:00:00Z",
];

/// The `1e300` bound and the doubles just past it.
pub const NUMBER_EDGES: [f64; 4] = [
    1e300,
    1.000_000_000_000_000_2e300,
    -1e300,
    -1.000_000_000_000_000_2e300,
];

const SMALL_NUMBERS: [f64; 15] = [
    -3.0, -1.0, -0.5, 0.0, 0.25, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 7.0, 8.0,
];

/// A workspace's worth of definitions: the traits, events, lists, segments and topics the
/// generators draw from.
pub fn catalog() -> Catalog {
    let mut catalog = Catalog::default();
    for (key, kind) in TRAITS {
        catalog.traits.insert(key.to_owned(), kind);
    }
    for (name, properties) in EVENTS {
        let schema: HashMap<String, PropertyType> = properties
            .iter()
            .map(|(key, kind)| ((*key).to_owned(), *kind))
            .collect();
        catalog.events.insert(name.to_owned(), schema);
    }
    for id in LISTS {
        catalog.lists.insert(Uuid::from_u128(id));
    }
    for id in SEGMENTS {
        catalog.segments.insert(Uuid::from_u128(id));
    }
    let topics: HashMap<String, Uuid> = TOPICS
        .iter()
        .map(|(key, id)| ((*key).to_owned(), Uuid::from_u128(*id)))
        .collect();
    catalog.topics.insert(Channel::Email, topics);
    catalog
}

/// The number of cases: [`CASES_VAR`], or 64.
pub fn cases() -> u32 {
    parse_cases(env::var(CASES_VAR).ok().as_deref())
}

/// The seed: [`SEED_VAR`], or a fixed one, so CI runs the same cases every time.
pub fn seed() -> u64 {
    parse_seed(env::var(SEED_VAR).ok().as_deref())
}

fn parse_cases(text: Option<&str>) -> u32 {
    text.and_then(|text| text.trim().parse().ok())
        .filter(|cases| *cases > 0)
        .unwrap_or(DEFAULT_CASES)
}

fn parse_seed(text: Option<&str>) -> u64 {
    text.and_then(|text| text.trim().parse().ok())
        .unwrap_or(DEFAULT_SEED)
}

/// A runner on [`seed`] with [`cases`] cases. Failure persistence is off: a failing case is
/// reproduced from its seed, and no regression file is written.
pub fn runner() -> TestRunner {
    runner_with(seed(), cases())
}

/// A runner on a given seed and number of cases.
pub fn runner_with(seed: u64, cases: u32) -> TestRunner {
    let config = Config {
        cases,
        failure_persistence: None,
        ..Config::default()
    };
    let mut bytes = [0_u8; 32];
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let word = seed
            .wrapping_add(index as u64)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &bytes))
}

/// `count` values of a strategy from the runner, without shrinking: for a test that runs them
/// somewhere a closure cannot go, such as an async database test.
pub fn sample<S: Strategy>(strategy: &S, runner: &mut TestRunner, count: u32) -> Vec<S::Value> {
    (0..count)
        .map(|_| {
            strategy
                .new_tree(runner)
                .expect("the strategy should always produce a value")
                .current()
        })
        .collect()
}

/// One comparison case: a definition and the people it is checked against.
#[derive(Clone, Debug)]
pub struct Case {
    pub definition: Definition,
    pub people: Vec<PersonSnapshot>,
}

/// A definition and up to [`MAX_PEOPLE`] people.
pub fn case() -> impl Strategy<Value = Case> {
    (definition(), vec(person(), 0..=MAX_PEOPLE))
        .prop_map(|(definition, people)| Case { definition, people })
}

/// A valid definition against [`catalog`]: every node, every operator of every value type, the
/// aggregate and window forms, and nested `not`, at most four levels deep.
pub fn definition() -> impl Strategy<Value = Definition> {
    node().prop_map(|root| Definition { v: 1, root })
}

pub fn node() -> BoxedStrategy<Node> {
    let leaf = prop_oneof![
        6 => trait_leaf(),
        6 => event_leaf(),
        1 => select(LISTS.to_vec()).prop_map(|id| Node::List(Uuid::from_u128(id))),
        1 => select(SEGMENTS.to_vec()).prop_map(|id| Node::Segment(Uuid::from_u128(id))),
        2 => consent_leaf(),
        1 => select(IDENTIFIERS.to_vec()).prop_map(|kind| Node::Identifier(kind.to_owned())),
    ];
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            3 => vec(inner.clone(), 0..=3).prop_map(Node::All),
            3 => vec(inner.clone(), 0..=3).prop_map(Node::Any),
            2 => inner.prop_map(|node| Node::Not(Box::new(node))),
        ]
    })
    .boxed()
}

fn trait_leaf() -> BoxedStrategy<Node> {
    select(TRAITS.to_vec())
        .prop_flat_map(|(key, kind)| {
            condition(Kind::from(kind), true).prop_map(move |(op, value)| {
                Node::Trait(TraitCondition {
                    key: key.to_owned(),
                    op,
                    value,
                })
            })
        })
        .boxed()
}

fn consent_leaf() -> BoxedStrategy<Node> {
    let topic = prop_oneof![
        2 => Just(None),
        1 => select(TOPICS.to_vec()).prop_map(|(key, _)| Some(key.to_owned())),
    ];
    let status = select(vec![
        ConsentStatus::Subscribed,
        ConsentStatus::Unsubscribed,
        ConsentStatus::PendingConfirmation,
        ConsentStatus::Unknown,
    ]);
    (topic, status)
        .prop_map(|(topic, status)| {
            Node::Consent(ConsentCondition {
                channel: Channel::Email.as_str().to_owned(),
                topic,
                status: status.as_str().to_owned(),
            })
        })
        .boxed()
}

fn event_leaf() -> BoxedStrategy<Node> {
    select(EVENTS.to_vec())
        .prop_flat_map(|(name, properties)| {
            let filters = vec(filter(properties), 0..=2);
            let agg = option::weighted(0.35, aggregate(name));
            let window = option::weighted(0.45, window());
            (filters, agg, window).prop_map(move |(filters, agg, window)| {
                Node::Event(EventCondition {
                    name: name.to_owned(),
                    filters,
                    agg,
                    window,
                })
            })
        })
        .boxed()
}

fn filter(properties: &'static [(&'static str, PropertyType)]) -> BoxedStrategy<PropertyFilter> {
    select(properties.to_vec())
        .prop_flat_map(|(prop, kind)| {
            condition(Kind::from(kind), true).prop_map(move |(op, value)| PropertyFilter {
                prop: prop.to_owned(),
                op,
                value,
            })
        })
        .boxed()
}

fn aggregate(event: &'static str) -> BoxedStrategy<Aggregate> {
    let props = AGGREGATED
        .iter()
        .find(|(name, _)| *name == event)
        .map_or(&[][..], |(_, props)| *props);
    select(AggFn::ALL.to_vec())
        .prop_flat_map(move |func| {
            let (prop, kind): (BoxedStrategy<Option<String>>, Kind) = match func {
                AggFn::Count => (Just(None).boxed(), Kind::Number),
                AggFn::FirstTime | AggFn::LastTime => (Just(None).boxed(), Kind::Timestamp),
                _ => (
                    select(props.to_vec())
                        .prop_map(|prop| Some(prop.to_owned()))
                        .boxed(),
                    Kind::Number,
                ),
            };
            let counts = func == AggFn::Count;
            (prop, condition_of(kind, false, counts)).prop_map(move |(prop, (op, value))| {
                Aggregate {
                    func,
                    prop,
                    op,
                    value,
                }
            })
        })
        .boxed()
}

fn window() -> BoxedStrategy<Window> {
    prop_oneof![
        3 => select(DURATIONS.to_vec()).prop_map(|text| Window::Last(text.to_owned())),
        2 => range_pair().prop_map(|(start, end)| Window::Between(start, end)),
        2 => select(vec!["signed_up", "renewed", "birthday"])
            .prop_map(|key| Window::SinceTrait(key.to_owned())),
    ]
    .boxed()
}

/// The operator and value of one condition on a value type.
fn condition(kind: Kind, presence: bool) -> BoxedStrategy<(Op, Option<Value>)> {
    condition_of(kind, presence, false)
}

fn condition_of(kind: Kind, presence: bool, counts: bool) -> BoxedStrategy<(Op, Option<Value>)> {
    let ops: Vec<Op> = ops_for(kind)
        .iter()
        .copied()
        .filter(|op| presence || !matches!(op, Op::Exists | Op::NotExists))
        .collect();
    select(ops)
        .prop_flat_map(move |op| operand(kind, op, counts).prop_map(move |value| (op, value)))
        .boxed()
}

fn operand(kind: Kind, op: Op, counts: bool) -> BoxedStrategy<Option<Value>> {
    match op {
        Op::Exists | Op::NotExists | Op::IsEmpty => Just(None).boxed(),
        Op::WithinLast | Op::MoreThanAgo | Op::WithinNext => select(DURATIONS.to_vec())
            .prop_map(|text| Some(json!(text)))
            .boxed(),
        Op::In | Op::NotIn => vec(scalar(kind, counts), 1..=3)
            .prop_map(|items| Some(Value::Array(items)))
            .boxed(),
        Op::Between => (scalar(kind, counts), scalar(kind, counts))
            .prop_map(move |(first, second)| Some(ordered(kind, first, second)))
            .boxed(),
        Op::Contains | Op::ContainsAll if kind == Kind::StringList => {
            vec(scalar(Kind::String, counts), 1..=3)
                .prop_map(|items| Some(Value::Array(items)))
                .boxed()
        }
        _ => scalar(kind, counts).prop_map(Some).boxed(),
    }
}

/// A range with its ends in order: strictly for instants and dates, which refuse an empty one.
fn ordered(kind: Kind, first: Value, second: Value) -> Value {
    let key = |value: &Value| -> (f64, String) {
        (
            value.as_f64().unwrap_or_default(),
            value.as_str().unwrap_or_default().to_owned(),
        )
    };
    let swap = key(&first) > key(&second);
    let (low, high) = if swap {
        (second, first)
    } else {
        (first, second)
    };
    if matches!(kind, Kind::Timestamp | Kind::Date) && low == high {
        let later = match kind {
            Kind::Date => day_string(NOW + Duration::days(40)),
            _ => instant_string(NOW + Duration::days(40)),
        };
        return json!([low, later]);
    }
    json!([low, high])
}

/// One value a definition compares with.
fn scalar(kind: Kind, counts: bool) -> BoxedStrategy<Value> {
    match kind {
        Kind::String => select(WORDS.to_vec()).prop_map(|word| json!(word)).boxed(),
        Kind::Number | Kind::Integer if counts => select(vec![0, 1, 2, 3, 5])
            .prop_map(|count| json!(count))
            .boxed(),
        Kind::Number => prop_oneof![
            9 => select(SMALL_NUMBERS.to_vec()).prop_map(|number| json!(number)),
            1 => select(NUMBER_EDGES.to_vec()).prop_map(|number| json!(number)),
        ]
        .boxed(),
        Kind::Integer => prop_oneof![
            9 => select(SMALL_NUMBERS.to_vec())
                .prop_filter("whole numbers only", |number| number.fract() == 0.0)
                .prop_map(|number| json!(number as i64)),
            1 => select(NUMBER_EDGES.to_vec()).prop_map(|number| json!(number)),
        ]
        .boxed(),
        Kind::Boolean => any::<bool>().prop_map(|flag| json!(flag)).boxed(),
        Kind::Timestamp => instant().prop_map(|at| json!(instant_string(at))).boxed(),
        Kind::Date => day().prop_map(|at| json!(day_string(at))).boxed(),
        Kind::StringList | Kind::Object => Just(Value::Null).boxed(),
    }
}

/// The anchors every instant is drawn around: now, the edges of each window, and the bounds of
/// the fixed ranges.
fn anchors() -> Vec<OffsetDateTime> {
    let mut anchors = vec![NOW];
    for hours in DURATION_HOURS {
        anchors.push(NOW - Duration::hours(hours));
        anchors.push(NOW + Duration::hours(hours));
    }
    for (start, end) in RANGES {
        anchors.push(start);
        anchors.push(end);
    }
    anchors
}

const RANGES: [(OffsetDateTime, OffsetDateTime); 3] = [
    (
        datetime!(2025-06-01 0:00 UTC),
        datetime!(2025-06-08 0:00 UTC),
    ),
    (
        datetime!(2025-06-13 12:00 UTC),
        datetime!(2025-06-14 12:00 UTC),
    ),
    (
        datetime!(2025-06-14 12:00 UTC),
        datetime!(2025-06-16 12:00 UTC),
    ),
];

/// An instant on a whole second: mostly exactly at, or one second either side of, an anchor.
pub fn instant() -> BoxedStrategy<OffsetDateTime> {
    prop_oneof![
        9 => (select(anchors()), select(vec![-1_i64, 0, 0, 1]))
            .prop_map(|(anchor, seconds)| anchor + Duration::seconds(seconds)),
        2 => (-300_000_i64..300_000).prop_map(|seconds| NOW + Duration::seconds(seconds)),
    ]
    .boxed()
}

/// A day at UTC midnight, around now.
fn day() -> BoxedStrategy<OffsetDateTime> {
    select(vec![-30_i64, -7, -2, -1, 0, 0, 1, 2, 7, 30])
        .prop_map(|days| (NOW + Duration::days(days)).replace_time(time::Time::MIDNIGHT))
        .boxed()
}

fn range_pair() -> BoxedStrategy<(String, String)> {
    select(RANGES.to_vec())
        .prop_map(|(start, end)| (instant_string(start), instant_string(end)))
        .boxed()
}

/// RFC 3339 in UTC, the canonical spelling a definition uses.
fn instant_string(at: OffsetDateTime) -> String {
    spell(at, UtcOffset::UTC, Spelling::Canonical, None)
}

fn day_string(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}",
        at.year(),
        u8::from(at.month()),
        at.day()
    )
}

#[derive(Clone, Copy, Debug)]
enum Spelling {
    Canonical,
    Space,
    Lower,
}

/// Writes an instant in an offset, with the separator and case of a spelling and an optional
/// fraction of a second, all of which the write path's shape accepts.
fn spell(
    at: OffsetDateTime,
    offset: UtcOffset,
    spelling: Spelling,
    fraction: Option<&str>,
) -> String {
    let local = at.to_offset(offset);
    let separator = match spelling {
        Spelling::Canonical => 'T',
        Spelling::Space => ' ',
        Spelling::Lower => 't',
    };
    let zone = if offset == UtcOffset::UTC {
        match spelling {
            Spelling::Lower => "z".to_owned(),
            _ => "Z".to_owned(),
        }
    } else {
        let (hours, minutes, _) = offset.as_hms();
        let sign = if offset.is_negative() { '-' } else { '+' };
        format!("{sign}{:02}:{:02}", hours.abs(), minutes.abs())
    };
    format!(
        "{:04}-{:02}-{:02}{separator}{:02}:{:02}:{:02}{}{zone}",
        local.year(),
        u8::from(local.month()),
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
        fraction.unwrap_or_default(),
    )
}

fn offset() -> BoxedStrategy<UtcOffset> {
    let offsets = [
        (0, 0),
        (0, 0),
        (2, 0),
        (-5, -30),
        (14, 0),
        (15, 0),
        (15, 59),
        (-12, 0),
    ];
    select(offsets.to_vec())
        .prop_map(|(hours, minutes)| {
            UtcOffset::from_hms(hours, minutes, 0).expect("a valid offset")
        })
        .boxed()
}

/// A time string the stored value could hold: any spelling the shape takes, or an oddity.
fn stored_timestamp() -> BoxedStrategy<Value> {
    prop_oneof![
        12 => (
            instant(),
            offset(),
            select(vec![Spelling::Canonical, Spelling::Canonical, Spelling::Space, Spelling::Lower]),
            select(vec![None, None, None, Some(".000"), Some(".5")]),
        )
            .prop_map(|(at, offset, spelling, fraction)| json!(spell(at, offset, spelling, fraction))),
        5 => select(TIMESTAMP_ODDITIES.to_vec()).prop_map(|text| json!(text)),
        3 => wrong_type(),
    ]
    .boxed()
}

fn stored_date() -> BoxedStrategy<Value> {
    prop_oneof![
        10 => day().prop_map(|at| json!(day_string(at))),
        4 => select(DATE_ODDITIES.to_vec()).prop_map(|text| json!(text)),
        2 => instant().prop_map(|at| json!(instant_string(at))),
        2 => wrong_type(),
    ]
    .boxed()
}

/// A JSON value of the wrong type for almost any property.
fn wrong_type() -> BoxedStrategy<Value> {
    select(vec![
        json!(5),
        json!(5.5),
        json!("5"),
        json!(true),
        json!("true"),
        Value::Null,
        json!([]),
        json!(["a"]),
        json!({}),
        json!({"a": "b"}),
    ])
    .boxed()
}

fn number_value(allow_edges: bool, whole: bool) -> BoxedStrategy<Value> {
    let small = select(SMALL_NUMBERS.to_vec()).prop_filter("whole numbers only", move |number| {
        !whole || number.fract() == 0.0
    });
    let plain = (small, any::<bool>()).prop_map(|(number, as_int)| {
        if as_int && number.fract() == 0.0 {
            json!(number as i64)
        } else {
            json!(number)
        }
    });
    let fractions = select(vec![0.5, 2.5, -1.5]).prop_map(|number| json!(number));
    let edges = select(NUMBER_EDGES.to_vec()).prop_map(|number| json!(number));
    if allow_edges {
        prop_oneof![14 => plain, 3 => edges, 3 => wrong_type()].boxed()
    } else if whole {
        prop_oneof![14 => plain, 4 => fractions, 3 => wrong_type()].boxed()
    } else {
        // Large positive values are fine in a sum; the negative edge would not be.
        let large = select(vec![
            1e300,
            1.000_000_000_000_000_2e300,
            -1.000_000_000_000_000_2e300,
        ])
        .prop_map(|number| json!(number));
        prop_oneof![14 => plain, 2 => large, 3 => wrong_type()].boxed()
    }
}

/// One stored value for a value type: usually the right one, sometimes an oddity or the wrong
/// JSON type.
fn stored(kind: Kind, property: &str) -> BoxedStrategy<Value> {
    match kind {
        Kind::String => prop_oneof![
            12 => select(WORDS.to_vec()).prop_map(|word| json!(word)),
            4 => wrong_type(),
        ]
        .boxed(),
        Kind::Number => number_value(property != "total", false),
        Kind::Integer => number_value(false, true),
        Kind::Boolean => prop_oneof![
            10 => any::<bool>().prop_map(|flag| json!(flag)),
            3 => wrong_type(),
        ]
        .boxed(),
        Kind::Timestamp => stored_timestamp(),
        Kind::Date => stored_date(),
        Kind::StringList => prop_oneof![
            10 => vec(
                prop_oneof![
                    6 => select(WORDS.to_vec()).prop_map(|word| json!(word)),
                    1 => wrong_type(),
                ],
                0..=3,
            )
            .prop_map(Value::Array),
            2 => Just(json!([])),
            3 => wrong_type(),
        ]
        .boxed(),
        Kind::Object => prop_oneof![
            4 => Just(json!({})),
            3 => Just(json!({"k": 1})),
            3 => wrong_type(),
        ]
        .boxed(),
    }
}

/// A map of values by key, each key present about three times in four.
fn properties(keys: Vec<(&'static str, Kind)>) -> BoxedStrategy<Map<String, Value>> {
    let slots: Vec<BoxedStrategy<Option<(String, Value)>>> = keys
        .into_iter()
        .map(|(key, kind)| {
            option::weighted(
                0.75,
                stored(kind, key).prop_map(move |value| (key.to_owned(), value)),
            )
            .boxed()
        })
        .collect();
    slots
        .prop_map(|entries| entries.into_iter().flatten().collect())
        .boxed()
}

fn snapshot_event() -> BoxedStrategy<SnapshotEvent> {
    select(EVENTS.to_vec())
        .prop_flat_map(|(name, schema)| {
            let keys = schema
                .iter()
                .map(|(key, kind)| (*key, Kind::from(*kind)))
                .collect();
            (instant(), properties(keys)).prop_map(move |(occurred_at, properties)| SnapshotEvent {
                name: name.to_owned(),
                occurred_at,
                properties,
            })
        })
        .boxed()
}

fn subset(ids: &[u128]) -> BoxedStrategy<std::collections::BTreeSet<Uuid>> {
    btree_set(
        select(ids.to_vec()).prop_map(Uuid::from_u128),
        0..=ids.len(),
    )
    .boxed()
}

fn consent_rows() -> BoxedStrategy<Vec<SnapshotConsent>> {
    let status = select(vec![
        ConsentStatus::Subscribed,
        ConsentStatus::Unsubscribed,
        ConsentStatus::PendingConfirmation,
        ConsentStatus::Unknown,
    ]);
    let topics: Vec<Option<Uuid>> = std::iter::once(None)
        .chain(TOPICS.iter().map(|(_, id)| Some(Uuid::from_u128(*id))))
        .collect();
    let rows: Vec<BoxedStrategy<Option<SnapshotConsent>>> = topics
        .into_iter()
        .map(|topic| {
            option::weighted(0.5, status.clone())
                .prop_map(move |status| {
                    status.map(|status| SnapshotConsent {
                        channel: Channel::Email,
                        topic,
                        status,
                    })
                })
                .boxed()
        })
        .collect();
    rows.prop_map(|rows| rows.into_iter().flatten().collect())
        .boxed()
}

/// One person's data, matched to [`catalog`].
pub fn person() -> impl Strategy<Value = PersonSnapshot> {
    let trait_keys = TRAITS
        .iter()
        .map(|(key, kind)| (*key, Kind::from(*kind)))
        .collect();
    // Zero events about a quarter of the time, so the zero-event aggregates are reached.
    let events = prop_oneof![
        1 => Just(Vec::new()),
        3 => vec(snapshot_event(), 1..=6),
    ];
    let kinds = btree_set(select(IDENTIFIERS.to_vec()).prop_map(str::to_owned), 0..=3);
    (
        properties(trait_keys),
        events,
        subset(&LISTS),
        consent_rows(),
        kinds,
        subset(&SEGMENTS),
    )
        .prop_map(
            |(traits, events, lists, consent, identifier_kinds, segments)| PersonSnapshot {
                traits,
                events,
                capped_events: Default::default(),
                events_complete_after: None,
                lists,
                consent,
                identifier_kinds,
                segments,
            },
        )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::evaluate::evaluate;
    use crate::normalize::normalize;
    use crate::storage::TraitStorage;
    use crate::validate::validate;

    fn depth(node: &Node) -> usize {
        match node {
            Node::All(children) | Node::Any(children) => {
                1 + children.iter().map(depth).max().unwrap_or(0)
            }
            Node::Not(child) => 1 + depth(child),
            _ => 1,
        }
    }

    fn definitions(count: u32) -> Vec<Definition> {
        sample(&definition(), &mut runner_with(7, count), count)
    }

    #[test]
    fn the_seed_and_the_case_count_come_from_the_environment_or_default() {
        assert_eq!(parse_cases(None), 64);
        assert_eq!(parse_cases(Some(" 1000 ")), 1000);
        assert_eq!(parse_cases(Some("0")), 64);
        assert_eq!(parse_cases(Some("many")), 64);
        assert_eq!(parse_seed(None), DEFAULT_SEED);
        assert_eq!(parse_seed(Some("42")), 42);
        assert_eq!(parse_seed(Some("-1")), DEFAULT_SEED);
    }

    #[test]
    fn a_seed_gives_the_same_cases_and_another_seed_other_ones() {
        let first = sample(&case(), &mut runner_with(11, 5), 5);
        let again = sample(&case(), &mut runner_with(11, 5), 5);
        let other = sample(&case(), &mut runner_with(12, 5), 5);
        let shown = |cases: &[Case]| format!("{cases:?}");
        assert_eq!(shown(&first), shown(&again));
        assert_ne!(shown(&first), shown(&other));
    }

    #[test]
    fn every_generated_definition_validates_compiles_and_stays_within_four_levels() {
        let catalog = catalog();
        for definition in definitions(1500) {
            validate(&definition, &catalog)
                .unwrap_or_else(|issues| panic!("{issues:?} in {}", definition.to_value()));
            crate::compile(&definition, &catalog, &TraitStorage::default(), NOW)
                .unwrap_or_else(|issues| panic!("{issues:?} in {}", definition.to_value()));
            assert!(depth(&definition.root) <= 4, "{}", definition.to_value());
        }
    }

    fn facts(node: &Node, catalog: &Catalog, facts: &mut BTreeSet<String>) {
        match node {
            Node::All(children) | Node::Any(children) => {
                facts.insert(format!("node:{}", node.key()));
                for child in children {
                    self::facts(child, catalog, facts);
                }
            }
            Node::Not(child) => {
                facts.insert("node:not".to_owned());
                if matches!(**child, Node::Not(_)) {
                    facts.insert("not:not".to_owned());
                }
                self::facts(child, catalog, facts);
            }
            Node::Trait(condition) => {
                facts.insert("node:trait".to_owned());
                let kind = Kind::from(catalog.traits[&condition.key]);
                facts.insert(format!("trait:{kind:?}:{}", condition.op.as_str()));
            }
            Node::Event(condition) => {
                facts.insert("node:event".to_owned());
                for filter in &condition.filters {
                    let kind = Kind::from(catalog.events[&condition.name][&filter.prop]);
                    facts.insert(format!("prop:{kind:?}:{}", filter.op.as_str()));
                }
                if let Some(agg) = &condition.agg {
                    facts.insert(format!("agg:{}:{}", agg.func.as_str(), agg.op.as_str()));
                }
                facts.insert(match &condition.window {
                    None => "window:none".to_owned(),
                    Some(Window::Last(_)) => "window:last".to_owned(),
                    Some(Window::Between(..)) => "window:between".to_owned(),
                    Some(Window::SinceTrait(_)) => "window:since_trait".to_owned(),
                });
            }
            other => {
                facts.insert(format!("node:{}", other.key()));
            }
        }
    }

    #[test]
    fn the_definitions_reach_every_node_operator_aggregate_and_window() {
        let catalog = catalog();
        let mut reached = BTreeSet::new();
        for definition in definitions(6000) {
            facts(&definition.root, &catalog, &mut reached);
        }
        let mut required = BTreeSet::new();
        for key in zirv_flow_core::segment::NODE_KEYS {
            required.insert(format!("node:{key}"));
        }
        required.insert("not:not".to_owned());
        for window in ["none", "last", "between", "since_trait"] {
            required.insert(format!("window:{window}"));
        }
        for (_, kind) in TRAITS {
            for op in ops_for(Kind::from(kind)) {
                required.insert(format!("trait:{:?}:{}", Kind::from(kind), op.as_str()));
            }
        }
        for (_, properties) in EVENTS {
            for (_, kind) in properties {
                for op in ops_for(Kind::from(*kind)) {
                    required.insert(format!("prop:{:?}:{}", Kind::from(*kind), op.as_str()));
                }
            }
        }
        for func in AggFn::ALL {
            let kind = if matches!(func, AggFn::FirstTime | AggFn::LastTime) {
                Kind::Timestamp
            } else {
                Kind::Number
            };
            for op in ops_for(kind) {
                if !matches!(op, Op::Exists | Op::NotExists) {
                    required.insert(format!("agg:{}:{}", func.as_str(), op.as_str()));
                }
            }
        }
        let missing: Vec<_> = required.difference(&reached).collect();
        assert!(missing.is_empty(), "never generated: {missing:?}");
    }

    fn strings_of(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::String(text) => {
                found.insert(text.clone());
            }
            Value::Array(items) => items.iter().for_each(|item| strings_of(item, found)),
            Value::Object(map) => map.values().for_each(|item| strings_of(item, found)),
            _ => {}
        }
    }

    #[test]
    fn the_people_reach_the_disagreement_cases_in_trait_and_property_position() {
        let people = sample(&person(), &mut runner_with(3, 4000), 4000);
        let mut in_traits = BTreeSet::new();
        let mut in_properties = BTreeSet::new();
        for person in &people {
            strings_of(&Value::Object(person.traits.clone()), &mut in_traits);
            for event in &person.events {
                strings_of(&Value::Object(event.properties.clone()), &mut in_properties);
            }
        }
        for (position, found) in [("trait", &in_traits), ("property", &in_properties)] {
            for text in TIMESTAMP_ODDITIES.iter().chain(DATE_ODDITIES.iter()) {
                assert!(
                    found.contains(*text),
                    "{text:?} never stored as a {position}"
                );
            }
        }
    }

    #[test]
    fn the_people_reach_the_value_edges() {
        let people = sample(&person(), &mut runner_with(5, 4000), 4000);
        let mut numbers = BTreeSet::new();
        let (mut nulls, mut empty, mut fractional, mut zero_events, mut no_trait) = (0, 0, 0, 0, 0);
        let mut edge_events = BTreeSet::new();
        for person in &people {
            if person.events.is_empty() {
                zero_events += 1;
            }
            if !person.traits.contains_key("signed_up") {
                no_trait += 1;
            }
            for value in person.traits.values().chain(
                person
                    .events
                    .iter()
                    .flat_map(|event| event.properties.values()),
            ) {
                match value {
                    Value::Null => nulls += 1,
                    Value::Array(items) if items.is_empty() => empty += 1,
                    Value::Number(number) => {
                        let number = number.as_f64().unwrap_or_default();
                        if number.abs() >= 1e300 {
                            numbers.insert(number.to_bits());
                        } else if number.fract() != 0.0 {
                            fractional += 1;
                        }
                    }
                    _ => {}
                }
            }
            for event in &person.events {
                for hours in DURATION_HOURS {
                    for delta in [-1, 0, 1] {
                        if event.occurred_at
                            == NOW - Duration::hours(hours) + Duration::seconds(delta)
                        {
                            edge_events.insert((hours, delta));
                        }
                    }
                }
            }
        }
        for edge in NUMBER_EDGES {
            assert!(numbers.contains(&edge.to_bits()), "{edge} never stored");
        }
        assert!(nulls > 0 && empty > 0 && fractional > 0 && zero_events > 0 && no_trait > 0);
        assert_eq!(
            edge_events.len(),
            DURATION_HOURS.len() * 3,
            "{edge_events:?}"
        );
        let exactly_at_now = people
            .iter()
            .flat_map(|person| &person.events)
            .any(|event| event.occurred_at == NOW);
        assert!(exactly_at_now);
        for (start, end) in RANGES {
            for bound in [start, end] {
                assert!(
                    people
                        .iter()
                        .flat_map(|person| &person.events)
                        .any(|event| event.occurred_at == bound),
                    "{bound}"
                );
            }
        }
    }

    #[test]
    fn evaluating_a_definition_gives_the_same_outcome_as_evaluating_its_normal_form() {
        let catalog = catalog();
        let mut runner = runner();
        let members = std::cell::Cell::new(0_usize);
        let checked = std::cell::Cell::new(0_usize);
        runner
            .run(&case(), |case| {
                let normal = normalize(case.definition.clone());
                for person in &case.people {
                    let direct = evaluate(&case.definition, &catalog, person, NOW);
                    let normalized = evaluate(&normal, &catalog, person, NOW);
                    prop_assert_eq!(direct.outcome, normalized.outcome);
                    checked.set(checked.get() + 1);
                    members
                        .set(members.get() + usize::from(direct.outcome == crate::Outcome::Pass));
                }
                Ok(())
            })
            .unwrap_or_else(|failure| panic!("seed {}: {failure}", seed()));
        assert!(
            checked.get() > 0 && members.get() > 0,
            "{} of {}",
            members.get(),
            checked.get()
        );
    }
}
