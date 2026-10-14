//! Lowers a validated, normalized definition to a SQL predicate over `p`, a `people` row
//! (architecture 8.2). Every value, trait key and property name is a bind parameter; the only
//! text in the SQL is fixed. The caller has already validated, so a lookup that misses reports
//! a code and the compile fails rather than emit a wrong predicate.

use std::str::FromStr;

use serde_json::{Map, Value};
use time::{Duration, OffsetDateTime};
use zirv_flow_core::consent::{Channel, ConsentStatus};
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::TraitType;
use zirv_flow_core::segment::{
    AggFn, ChangeClasses, ConsentCondition, EventCondition, Issue, Node, Op, TraitCondition,
    Window, code,
};

use crate::catalog::Catalog;
use crate::deps::Deps;
use crate::query::Param;
use crate::storage::{Storage, TraitStorage};
use crate::time_value::{parse_date, parse_duration, parse_timestamp};
use crate::validate::Kind;

pub(crate) struct Lowered {
    pub predicate: String,
    pub params: Vec<Param>,
    pub deps: Deps,
    pub time_sensitive: bool,
}

pub(crate) fn lower(
    root: &Node,
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
) -> Result<Lowered, Vec<Issue>> {
    let mut lowerer = Lowerer {
        catalog,
        storage,
        now,
        params: Vec::new(),
        deps: Deps::default(),
        time_sensitive: false,
        issues: Vec::new(),
    };
    let predicate = lowerer.node(root);
    lowerer.need(ChangeClasses::MERGE);
    if lowerer.issues.is_empty() {
        Ok(Lowered {
            predicate,
            params: lowerer.params,
            deps: lowerer.deps,
            time_sensitive: lowerer.time_sensitive,
        })
    } else {
        Err(lowerer.issues)
    }
}

#[derive(Clone, Copy)]
enum Table {
    Person,
    Event,
}

/// A JSON object column and the bound key of one entry in it.
struct Field {
    column: &'static str,
    key: String,
    key_sql: String,
}

impl Field {
    fn json(&self) -> String {
        format!("({} -> {})", self.column, self.key_sql)
    }

    fn text(&self) -> String {
        format!("({} ->> {})", self.column, self.key_sql)
    }
}

struct Lowerer<'a> {
    catalog: &'a Catalog,
    storage: &'a TraitStorage,
    now: OffsetDateTime,
    params: Vec<Param>,
    deps: Deps,
    time_sensitive: bool,
    issues: Vec<Issue>,
}

impl Lowerer<'_> {
    fn fail(&mut self, reason: &'static str) -> String {
        self.issues.push(Issue::new("", reason));
        "false".to_owned()
    }

    fn need(&mut self, class: ChangeClasses) {
        self.deps.classes = self.deps.classes | class;
    }

    /// Binds a value, reusing the placeholder of an equal one, and returns it with its cast.
    fn bind(&mut self, param: Param, cast: &str) -> String {
        let index = match self.params.iter().position(|bound| *bound == param) {
            Some(index) => index,
            None => {
                self.params.push(param);
                self.params.len() - 1
            }
        };
        format!("${}::{cast}", index + 2)
    }

    fn text(&mut self, value: &str) -> String {
        self.bind(Param::Text(value.to_owned()), "text")
    }

    fn float(&mut self, value: f64) -> String {
        self.bind(Param::Float(value), "float8")
    }

    fn instant(&mut self, value: OffsetDateTime) -> String {
        self.bind(Param::Timestamp(value), "timestamptz")
    }

    fn node(&mut self, node: &Node) -> String {
        match node {
            Node::All(children) if children.is_empty() => "true".to_owned(),
            Node::Any(children) if children.is_empty() => "false".to_owned(),
            Node::All(children) => self.group(children, " AND "),
            Node::Any(children) => self.group(children, " OR "),
            Node::Not(child) => format!("NOT ({})", self.node(child)),
            Node::Trait(condition) => self.trait_condition(condition),
            Node::Event(condition) => self.event(condition),
            Node::List(id) => {
                self.deps.lists.insert(*id);
                self.need(ChangeClasses::LISTS);
                let list = self.bind(Param::Uuid(*id), "uuid");
                format!(
                    "EXISTS (SELECT 1 FROM list_members lm WHERE lm.workspace_id = p.workspace_id \
                     AND lm.person_id = p.id AND lm.list_id = {list})"
                )
            }
            Node::Segment(id) => {
                self.deps.segments.insert(*id);
                let segment = self.bind(Param::Uuid(*id), "uuid");
                format!(
                    "EXISTS (SELECT 1 FROM segment_members sm WHERE sm.workspace_id = p.workspace_id \
                     AND sm.person_id = p.id AND sm.segment_id = {segment} AND sm.exited_at IS NULL)"
                )
            }
            Node::Consent(condition) => self.consent(condition),
            Node::Identifier(kind) => {
                self.deps.identifiers.insert(kind.clone());
                self.need(ChangeClasses::IDENTIFIERS);
                let kind = self.text(kind);
                format!(
                    "EXISTS (SELECT 1 FROM identities i WHERE i.workspace_id = p.workspace_id \
                     AND i.person_id = p.id AND i.kind = {kind})"
                )
            }
        }
    }

    fn group(&mut self, children: &[Node], joiner: &str) -> String {
        let parts: Vec<String> = children.iter().map(|child| self.node(child)).collect();
        format!("({})", parts.join(joiner))
    }

    fn consent(&mut self, condition: &ConsentCondition) -> String {
        let Ok(channel) = Channel::from_str(&condition.channel) else {
            return self.fail(code::VALUE_NOT_ALLOWED);
        };
        let Ok(status) = ConsentStatus::from_str(&condition.status) else {
            return self.fail(code::VALUE_NOT_ALLOWED);
        };
        self.deps
            .consent
            .insert((condition.channel.clone(), condition.topic.clone()));
        self.need(ChangeClasses::CONSENT);
        let topic = match &condition.topic {
            None => "c.topic_id IS NULL".to_owned(),
            Some(key) => {
                let id = self
                    .catalog
                    .topics
                    .get(&channel)
                    .and_then(|topics| topics.get(key))
                    .copied();
                match id {
                    Some(id) => format!("c.topic_id = {}", self.bind(Param::Uuid(id), "uuid")),
                    None => return self.fail(code::UNKNOWN_TOPIC),
                }
            }
        };
        let channel = self.text(channel.as_str());
        let wanted = self.text(status.as_str());
        let row = format!(
            "FROM consent c WHERE c.workspace_id = p.workspace_id AND c.person_id = p.id \
             AND c.channel = {channel} AND {topic}"
        );
        if status == ConsentStatus::Unknown {
            format!("NOT EXISTS (SELECT 1 {row} AND c.status <> {wanted})")
        } else {
            format!("EXISTS (SELECT 1 {row} AND c.status = {wanted})")
        }
    }

    fn trait_condition(&mut self, condition: &TraitCondition) -> String {
        let Some(kind) = self.catalog.traits.get(&condition.key).copied() else {
            return self.fail(code::UNKNOWN_TRAIT);
        };
        match self.storage.of(&condition.key) {
            Storage::Jsonb => {}
        }
        self.deps.traits.insert(condition.key.clone());
        self.need(ChangeClasses::TRAITS);
        self.leaf(
            Table::Person,
            &condition.key,
            Kind::from(kind),
            condition.op,
            condition.value.as_ref(),
        )
    }

    fn field(&mut self, table: Table, key: &str) -> Field {
        let mut field = Self::unbound_field(table, key);
        field.key_sql = self.text(key);
        field
    }

    /// A field whose key is not bound, for a containment that carries the key in its own value.
    fn unbound_field(table: Table, key: &str) -> Field {
        Field {
            column: match table {
                Table::Person => "p.traits",
                Table::Event => "e.properties",
            },
            key: key.to_owned(),
            key_sql: String::new(),
        }
    }

    /// One two-valued leaf: a missing or wrongly typed value makes it false.
    fn leaf(
        &mut self,
        table: Table,
        key: &str,
        kind: Kind,
        op: Op,
        value: Option<&Value>,
    ) -> String {
        if op.is_relative() {
            self.time_sensitive = true;
        }
        let bare = op == Op::Eq && matches!(kind, Kind::String | Kind::Boolean);
        let field = if bare {
            Self::unbound_field(table, key)
        } else {
            self.field(table, key)
        };
        let inner = match op {
            Op::Exists => presence(&field),
            Op::NotExists => format!("NOT {}", presence(&field)),
            _ => self.typed(&field, kind, op, value),
        };
        if bare {
            return inner;
        }
        format!("COALESCE(({inner}), false)")
    }

    fn typed(&mut self, field: &Field, kind: Kind, op: Op, value: Option<&Value>) -> String {
        match kind {
            Kind::String => self.string(field, op, value),
            Kind::Number | Kind::Integer => {
                let expr = number_expr(&field.json(), kind == Kind::Integer);
                self.number(&expr, op, value)
            }
            Kind::Boolean => match (op, value) {
                (Op::Eq, Some(Value::Bool(_))) => self.contains(field, value),
                _ => self.fail(code::VALUE_TYPE_MISMATCH),
            },
            Kind::Timestamp | Kind::Date => self.time(&time_expr(field, kind), kind, op, value),
            Kind::StringList => self.list(field, op, value),
            Kind::Object => self.fail(code::OPERATOR_NOT_ALLOWED),
        }
    }

    /// JSON containment of `{key: value}`, which the `jsonb_path_ops` index serves.
    fn contains(&mut self, field: &Field, value: Option<&Value>) -> String {
        let Some(value) = value else {
            return self.fail(code::VALUE_MISSING);
        };
        let mut object = Map::new();
        object.insert(field.key.clone(), value.clone());
        let object = self.bind(Param::Jsonb(Value::Object(object)), "jsonb");
        format!("{} @> {object}", field.column)
    }

    fn string(&mut self, field: &Field, op: Op, value: Option<&Value>) -> String {
        if op == Op::Eq {
            return match value {
                Some(Value::String(_)) => self.contains(field, value),
                _ => self.fail(code::VALUE_TYPE_MISMATCH),
            };
        }
        let guard = format!("jsonb_typeof({}) = 'string'", field.json());
        let text = field.text();
        let comparison = match op {
            Op::Neq | Op::Contains | Op::StartsWith => {
                let Some(Value::String(wanted)) = value else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let wanted = self.text(wanted);
                match op {
                    Op::Neq => format!("{text} <> {wanted}"),
                    Op::Contains => format!("strpos({text}, {wanted}) > 0"),
                    _ => format!("left({text}, char_length({wanted})) = {wanted}"),
                }
            }
            Op::In | Op::NotIn => {
                let Some(items) = strings(value) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let items = self.bind(Param::TextArray(items), "text[]");
                if op == Op::In {
                    format!("{text} = ANY({items})")
                } else {
                    format!("{text} <> ALL({items})")
                }
            }
            _ => return self.fail(code::OPERATOR_NOT_ALLOWED),
        };
        format!("CASE WHEN {guard} THEN {comparison} END")
    }

    /// Compares a `float8` expression (a trait, a property or an aggregate).
    fn number(&mut self, expr: &str, op: Op, value: Option<&Value>) -> String {
        let symbol = match op {
            Op::Eq => "=",
            Op::Neq => "<>",
            Op::Gt => ">",
            Op::Gte => ">=",
            Op::Lt => "<",
            Op::Lte => "<=",
            Op::Between => {
                let Some([start, end]) = pair(value, number_of) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let start = self.float(start);
                let end = self.float(end);
                return format!("{expr} BETWEEN {start} AND {end}");
            }
            Op::In | Op::NotIn => {
                let Some(items) = numbers(value) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let items = self.bind(Param::FloatArray(items), "float8[]");
                return if op == Op::In {
                    format!("{expr} = ANY({items})")
                } else {
                    format!("{expr} <> ALL({items})")
                };
            }
            _ => return self.fail(code::OPERATOR_NOT_ALLOWED),
        };
        let Some(wanted) = value.and_then(number_of) else {
            return self.fail(code::VALUE_TYPE_MISMATCH);
        };
        let wanted = self.float(wanted);
        format!("{expr} {symbol} {wanted}")
    }

    /// Compares a `timestamptz` expression. A date is already at UTC midnight.
    fn time(&mut self, expr: &str, kind: Kind, op: Op, value: Option<&Value>) -> String {
        match op {
            Op::Before | Op::After => {
                let Some(at) = value.and_then(|value| absolute(kind, value)) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let at = self.instant(at);
                let symbol = if op == Op::Before { "<" } else { ">" };
                format!("{expr} {symbol} {at}")
            }
            Op::Between => {
                let Some([start, end]) = pair(value, |value| absolute(kind, value)) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let start = self.instant(start);
                let end = self.instant(end);
                format!("{expr} <@ tstzrange({start}, {end}, '[)')")
            }
            Op::WithinLast | Op::MoreThanAgo | Op::WithinNext => {
                let Some(span) = value.and_then(span_of) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let (Some(earlier), Some(later)) =
                    (self.now.checked_sub(span), self.now.checked_add(span))
                else {
                    return self.fail(code::INVALID_DURATION);
                };
                match op {
                    Op::WithinLast => {
                        let from = self.instant(earlier);
                        let now = self.instant(self.now);
                        format!("{expr} <@ tstzrange({from}, {now}, '(]')")
                    }
                    Op::MoreThanAgo => format!("{expr} <= {}", self.instant(earlier)),
                    _ => {
                        let now = self.instant(self.now);
                        let to = self.instant(later);
                        format!("{expr} <@ tstzrange({now}, {to}, '[)')")
                    }
                }
            }
            _ => self.fail(code::OPERATOR_NOT_ALLOWED),
        }
    }

    fn list(&mut self, field: &Field, op: Op, value: Option<&Value>) -> String {
        let json = field.json();
        let comparison = match op {
            Op::IsEmpty => format!("jsonb_array_length({json}) = 0"),
            Op::Contains | Op::ContainsAll => {
                let Some(items) = strings(value) else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let items = self.bind(Param::TextArray(items), "text[]");
                let operator = if op == Op::Contains { "?|" } else { "?&" };
                format!("{json} {operator} {items}")
            }
            _ => return self.fail(code::OPERATOR_NOT_ALLOWED),
        };
        format!("CASE WHEN jsonb_typeof({json}) = 'array' THEN {comparison} END")
    }

    /// An event condition. With a `since_trait` window, a person whose trait is missing or
    /// invalid fails the whole condition, aggregate included.
    fn event(&mut self, condition: &EventCondition) -> String {
        let body = self.event_rows(condition);
        let Some(Window::SinceTrait(key)) = &condition.window else {
            return body;
        };
        let kind = match self.catalog.traits.get(key) {
            Some(TraitType::Date) => Kind::Date,
            _ => Kind::Timestamp,
        };
        let field = self.field(Table::Person, key);
        format!("({} IS NOT NULL AND {body})", time_expr(&field, kind))
    }

    fn event_rows(&mut self, condition: &EventCondition) -> String {
        let catalog = self.catalog;
        let Some(schema) = catalog.events.get(&condition.name) else {
            return self.fail(code::UNKNOWN_EVENT);
        };
        self.deps.events.insert(condition.name.clone());
        self.need(ChangeClasses::EVENTS);
        let name = self.text(&condition.name);
        let mut filters = vec![
            "e.workspace_id = p.workspace_id".to_owned(),
            "e.person_id = p.id".to_owned(),
            format!("e.name = {name}"),
        ];
        if let Some(window) = &condition.window {
            filters.push(self.window(window));
        }
        for filter in &condition.filters {
            let Some(kind) = schema.get(&filter.prop).copied() else {
                return self.fail(code::UNKNOWN_PROPERTY);
            };
            let leaf = self.leaf(
                Table::Event,
                &filter.prop,
                Kind::from(kind),
                filter.op,
                filter.value.as_ref(),
            );
            filters.push(leaf);
        }
        let from = format!("FROM events e WHERE {}", filters.join(" AND "));
        let Some(agg) = &condition.agg else {
            return format!("EXISTS (SELECT 1 {from})");
        };
        if agg.op.is_relative() {
            self.time_sensitive = true;
        }
        let comparison = match agg.func {
            AggFn::Count => {
                let expr = format!("(SELECT count(*)::float8 {from})");
                self.number(&expr, agg.op, agg.value.as_ref())
            }
            AggFn::Sum | AggFn::Min | AggFn::Max | AggFn::Avg => {
                let Some(prop) = &agg.prop else {
                    return self.fail(code::AGGREGATE_NOT_ALLOWED);
                };
                let field = self.field(Table::Event, prop);
                let integral = schema.get(prop) == Some(&PropertyType::Integer);
                let argument = number_expr(&field.json(), integral);
                // PostgreSQL's `avg(float8)` also accumulates the sum of squares, which
                // overflows for values past about 1e154 and fails the whole query, though the bound allows
                // 1e300. The mean is the sum over the count, which is what `avg` returns without that.
                let aggregate = if agg.func == AggFn::Avg {
                    format!("sum({argument}) / NULLIF(count({argument}), 0)")
                } else {
                    format!("{}({argument})", agg.func.as_str())
                };
                let expr = format!("(SELECT {aggregate} {from})");
                self.number(&expr, agg.op, agg.value.as_ref())
            }
            AggFn::FirstTime | AggFn::LastTime => {
                let function = if agg.func == AggFn::FirstTime {
                    "min"
                } else {
                    "max"
                };
                let expr = format!("(SELECT {function}(e.occurred_at) {from})");
                self.time(&expr, Kind::Timestamp, agg.op, agg.value.as_ref())
            }
        };
        format!("COALESCE(({comparison}), false)")
    }

    /// The bounds are bound as timestamps, so the planner can prune partitions.
    fn window(&mut self, window: &Window) -> String {
        match window {
            Window::Last(text) => {
                self.time_sensitive = true;
                let Some(earlier) = span_of_text(text).and_then(|span| self.now.checked_sub(span))
                else {
                    return self.fail(code::INVALID_DURATION);
                };
                let from = self.instant(earlier);
                let now = self.instant(self.now);
                format!("e.occurred_at > {from} AND e.occurred_at <= {now}")
            }
            Window::Between(start, end) => {
                let (Some(start), Some(end)) = (parse_timestamp(start), parse_timestamp(end))
                else {
                    return self.fail(code::VALUE_TYPE_MISMATCH);
                };
                let start = self.instant(start);
                let end = self.instant(end);
                format!("e.occurred_at >= {start} AND e.occurred_at < {end}")
            }
            Window::SinceTrait(key) => {
                let kind = match self.catalog.traits.get(key) {
                    Some(TraitType::Timestamp) => Kind::Timestamp,
                    Some(TraitType::Date) => Kind::Date,
                    _ => return self.fail(code::UNKNOWN_TRAIT),
                };
                self.deps.traits.insert(key.clone());
                self.need(ChangeClasses::TRAITS);
                let field = self.field(Table::Person, key);
                format!("e.occurred_at >= {}", time_expr(&field, kind))
            }
        }
    }
}

/// A key that is present and not JSON null.
fn presence(field: &Field) -> String {
    let json = field.json();
    format!("(jsonb_typeof({json}) IS NOT NULL AND jsonb_typeof({json}) <> 'null')")
}

/// The value as `float8`, or NULL when it is not a number, its magnitude is past `1e300`, or
/// `integral` is set and it has a fractional part. The nesting keeps the cast from running on a
/// wrong type.
fn number_expr(json: &str, integral: bool) -> String {
    let whole = if integral {
        format!(" AND ({json})::numeric = trunc(({json})::numeric)")
    } else {
        String::new()
    };
    format!(
        "CASE WHEN jsonb_typeof({json}) = 'number' THEN \
         CASE WHEN abs(({json})::numeric) <= 1e300{whole} THEN ({json})::float8 END END"
    )
}

/// The RFC 3339 shape the write path accepts. PostgreSQL alone also takes `infinity`, `now` and
/// other spellings the write path refuses.
const TIMESTAMP_SHAPE: &str = "^[0-9]{4}-[0-9]{2}-[0-9]{2}[Tt ][0-9]{2}:[0-9]{2}:[0-9]{2}([.][0-9]+)?([Zz]|[+-][0-9]{2}:[0-9]{2})$";
const DATE_SHAPE: &str = "^[0-9]{4}-[0-9]{2}-[0-9]{2}$";

/// The value as `timestamptz`, or NULL when it is not a string in the write path's shape that
/// PostgreSQL also accepts. A date is taken at UTC midnight.
fn time_expr(field: &Field, kind: Kind) -> String {
    let text = field.text();
    let (cast, sql_type, shape) = if kind == Kind::Date {
        (
            format!("(({text})::date::timestamp AT TIME ZONE 'UTC')"),
            "date",
            DATE_SHAPE,
        )
    } else {
        (
            format!("({text})::timestamptz"),
            "timestamptz",
            TIMESTAMP_SHAPE,
        )
    };
    format!(
        "CASE WHEN jsonb_typeof({}) = 'string' THEN \
         CASE WHEN {text} ~ '{shape}' AND pg_input_is_valid({text}, '{sql_type}') THEN {cast} END END",
        field.json()
    )
}

fn number_of(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn numbers(value: Option<&Value>) -> Option<Vec<f64>> {
    value?.as_array()?.iter().map(number_of).collect()
}

fn strings(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

fn pair<T>(value: Option<&Value>, read: impl Fn(&Value) -> Option<T>) -> Option<[T; 2]> {
    match value?.as_array()?.as_slice() {
        [start, end] => Some([read(start)?, read(end)?]),
        _ => None,
    }
}

fn absolute(kind: Kind, value: &Value) -> Option<OffsetDateTime> {
    let text = value.as_str()?;
    if kind == Kind::Date {
        parse_date(text).map(|date| date.midnight().assume_utc())
    } else {
        parse_timestamp(text)
    }
}

fn span_of(value: &Value) -> Option<Duration> {
    span_of_text(value.as_str()?)
}

fn span_of_text(text: &str) -> Option<Duration> {
    let hours = parse_duration(text).ok()?;
    Some(Duration::hours(i64::try_from(hours).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_is_reused_for_an_equal_value() {
        let catalog = Catalog::default();
        let storage = TraitStorage::default();
        let mut lowerer = Lowerer {
            catalog: &catalog,
            storage: &storage,
            now: OffsetDateTime::UNIX_EPOCH,
            params: Vec::new(),
            deps: Deps::default(),
            time_sensitive: false,
            issues: Vec::new(),
        };
        assert_eq!(lowerer.text("a"), "$2::text");
        assert_eq!(lowerer.text("b"), "$3::text");
        assert_eq!(lowerer.text("a"), "$2::text");
        assert_eq!(lowerer.float(1.0), "$4::float8");
        assert_eq!(lowerer.params.len(), 3);
    }

    #[test]
    fn a_lookup_that_misses_fails_instead_of_emitting_a_predicate() {
        let node = Node::Trait(TraitCondition {
            key: "ghost".into(),
            op: Op::Exists,
            value: None,
        });
        let result = lower(
            &node,
            &Catalog::default(),
            &TraitStorage::default(),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(
            result.err(),
            Some(vec![Issue::new("", code::UNKNOWN_TRAIT)])
        );
    }

    #[test]
    fn the_number_guard_nests_the_cast_inside_the_type_check() {
        let expr = number_expr("(p.traits -> $2::text)", false);
        assert!(
            expr.starts_with("CASE WHEN jsonb_typeof((p.traits -> $2::text)) = 'number' THEN CASE")
        );
        assert!(expr.contains("<= 1e300"));
    }

    #[test]
    fn the_mean_is_the_sum_over_the_count_because_avg_squares_its_inputs() {
        let mut catalog = Catalog::default();
        let mut order = hashbrown::HashMap::new();
        order.insert("total".to_owned(), PropertyType::Number);
        catalog.events.insert("order".to_owned(), order);
        let node = Node::Event(EventCondition {
            name: "order".into(),
            filters: Vec::new(),
            agg: Some(zirv_flow_core::segment::Aggregate {
                func: AggFn::Avg,
                prop: Some("total".into()),
                op: Op::Gt,
                value: Some(serde_json::json!(1)),
            }),
            window: None,
        });
        let lowered = lower(
            &node,
            &catalog,
            &TraitStorage::default(),
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert!(!lowered.predicate.contains("avg("));
        assert!(lowered.predicate.contains("sum(CASE"));
        assert!(lowered.predicate.contains("/ NULLIF(count(CASE"));
    }
}
