//! Lowers a validated, normalized definition to a SQL predicate over `p`, a `people` row
//! (architecture 8.2). Every value, trait key and property name is a bind parameter; the only
//! text in the SQL is fixed. The caller has already validated, so a lookup that misses reports
//! a code and the compile fails rather than emit a wrong predicate.

use std::str::FromStr;

use serde_json::{Map, Value};
use time::{Duration, OffsetDateTime};
use zirv_flow_core::consent::{Channel, ConsentStatus};
use zirv_flow_core::events::PropertyType;
use zirv_flow_core::people::{SlotColumn, SlotKind, TraitType, number_sql, time_sql};
use zirv_flow_core::segment::{
    AggFn, Aggregate, ChangeClasses, ConsentCondition, EventCondition, Issue, Node, Op,
    TraitCondition, Window, code,
};

use crate::catalog::Catalog;
use crate::deps::Deps;
use crate::leaf::empty_aggregate_satisfies;
use crate::operand::{absolute, number_of, numbers, strings, timestamp};
use crate::query::Param;
use crate::storage::{Storage, TraitStorage};
use crate::time_value::parse_duration;
use crate::validate::Kind;

/// Where `CompiledSegment::scoped` renders the batch restriction inside a grouped event set. The
/// marker becomes `AND e.person_id = ANY($n::uuid[])` for a batch query and nothing otherwise.
/// The lowered text holds only fixed text, column names and placeholders, so no bound value or
/// key can collide with it.
pub(crate) const SCOPE: &str = "/*scope*/";

pub(crate) struct Lowered {
    /// The `LEFT JOIN` clauses of the aggregates that need a joined value.
    pub joins: Vec<String>,
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
        explicit_ranges: false,
        joins: Vec::new(),
    };
    let predicate = lowerer.node(root, true);
    lowerer.need(ChangeClasses::MERGE);
    if lowerer.issues.is_empty() {
        Ok(Lowered {
            predicate,
            joins: lowerer.joins,
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
    /// Lower a time range as two comparisons, which a btree on a slot serves, rather than `<@`.
    explicit_ranges: bool,
    /// The `LEFT JOIN` clauses of the aggregates that need a joined value, numbered by position.
    joins: Vec<String>,
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

    /// `positive` is whether the node is the root or reached from it only through `all` nodes, so
    /// that its falsehood alone makes the person a non-member.
    fn node(&mut self, node: &Node, positive: bool) -> String {
        match node {
            Node::All(children) if children.is_empty() => "true".to_owned(),
            Node::Any(children) if children.is_empty() => "false".to_owned(),
            Node::All(children) => self.group(children, " AND ", positive),
            Node::Any(children) => self.group(children, " OR ", false),
            Node::Not(child) => format!("NOT ({})", self.node(child, false)),
            Node::Trait(condition) => self.trait_condition(condition),
            Node::Event(condition) => self.event(condition, positive),
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

    fn group(&mut self, children: &[Node], joiner: &str, positive: bool) -> String {
        let parts: Vec<String> = children
            .iter()
            .map(|child| self.node(child, positive))
            .collect();
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
        self.deps.traits.insert(condition.key.clone());
        self.need(ChangeClasses::TRAITS);
        if let Some(slot) = self.slot_of(&condition.key, kind)
            && !matches!(condition.op, Op::Exists | Op::NotExists)
        {
            return self.slot_leaf(
                slot,
                Kind::from(kind),
                condition.op,
                condition.value.as_ref(),
            );
        }
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

    /// The slot a `ready` trait is read from. A slot of another kind than the trait is ignored
    /// and the trait is read from `jsonb`, which cannot happen for a slot assigned by type.
    fn slot_of(&self, key: &str, kind: TraitType) -> Option<SlotColumn> {
        match self.storage.of(key) {
            Storage::Slot(slot) if Some(slot.kind()) == SlotKind::of(kind) => Some(slot),
            Storage::Slot(_) | Storage::Jsonb => None,
        }
    }

    /// One two-valued leaf over a slot column, where NULL is a missing, wrongly typed or invalid
    /// value. `exists` and `not_exists` are not lowered here: a slot cannot tell a missing value
    /// from a present one of the wrong type, so they read `jsonb`.
    fn slot_leaf(&mut self, slot: SlotColumn, kind: Kind, op: Op, value: Option<&Value>) -> String {
        if op.is_relative() {
            self.time_sensitive = true;
        }
        let column = format!("p.{}", slot.as_str());
        let inner = match kind {
            Kind::String => match (op, value) {
                (Op::Eq, Some(Value::String(wanted))) => {
                    format!("{column} = {}", self.text(wanted))
                }
                (Op::Eq, _) => self.fail(code::VALUE_TYPE_MISMATCH),
                _ => self.string_comparison(&column, op, value),
            },
            Kind::Number | Kind::Integer => self.number(&column, op, value),
            Kind::Boolean => match (op, value) {
                (Op::Eq, Some(Value::Bool(true))) => format!("{column} IS TRUE"),
                (Op::Eq, Some(Value::Bool(false))) => format!("{column} IS FALSE"),
                _ => self.fail(code::VALUE_TYPE_MISMATCH),
            },
            Kind::Timestamp | Kind::Date => {
                self.explicit_ranges = true;
                let inner = self.time(&column, kind, op, value);
                self.explicit_ranges = false;
                inner
            }
            Kind::StringList | Kind::Object => self.fail(code::OPERATOR_NOT_ALLOWED),
        };
        // `AND IS NOT NULL` makes the leaf two-valued (NULL AND false is false) while the
        // comparison stays a plain column predicate the partial index serves.
        format!("({inner} AND {column} IS NOT NULL)")
    }

    /// A trait as `timestamptz`: its slot when `ready`, else the `jsonb` expression.
    fn trait_time(&mut self, key: &str, kind: Kind) -> String {
        let trait_type = match kind {
            Kind::Date => TraitType::Date,
            _ => TraitType::Timestamp,
        };
        if let Some(slot) = self.slot_of(key, trait_type) {
            return format!("p.{}", slot.as_str());
        }
        let field = self.field(Table::Person, key);
        time_expr(&field, kind)
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
        let comparison = self.string_comparison(&field.text(), op, value);
        format!("CASE WHEN {guard} THEN {comparison} END")
    }

    /// A string comparison other than equality over a text expression.
    fn string_comparison(&mut self, text: &str, op: Op, value: Option<&Value>) -> String {
        match op {
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
            _ => self.fail(code::OPERATOR_NOT_ALLOWED),
        }
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

    /// `expr` inside the range from `low` to `high`. Over a slot it is two comparisons, which a
    /// btree serves; over `jsonb` it is the range containment the golden corpus pins.
    fn within(&self, expr: &str, low: &str, high: &str, bounds: &str) -> String {
        if !self.explicit_ranges {
            return format!("{expr} <@ tstzrange({low}, {high}, '{bounds}')");
        }
        let (open, close) = match bounds {
            "(]" => (">", "<="),
            _ => (">=", "<"),
        };
        format!("({expr} {open} {low} AND {expr} {close} {high})")
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
                self.within(expr, &start, &end, "[)")
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
                        self.within(expr, &from, &now, "(]")
                    }
                    Op::MoreThanAgo => format!("{expr} <= {}", self.instant(earlier)),
                    _ => {
                        let now = self.instant(self.now);
                        let to = self.instant(later);
                        self.within(expr, &now, &to, "[)")
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
    fn event(&mut self, condition: &EventCondition, positive: bool) -> String {
        let body = self.event_rows(condition, positive);
        let Some(Window::SinceTrait(key)) = &condition.window else {
            return body;
        };
        let kind = match self.catalog.traits.get(key) {
            Some(TraitType::Date) => Kind::Date,
            _ => Kind::Timestamp,
        };
        let since = self.trait_time(key, kind);
        format!("({since} IS NOT NULL AND {body})")
    }

    /// The rows of an event condition. A condition without an aggregate is an `EXISTS`, and so
    /// is one with a `since_trait` window, whose bound reads the person's own trait. Any other
    /// aggregate is one grouped set over `events` (architecture 8.2), as a semi-join when the
    /// condition is a positive conjunct that no event-less person satisfies, and as a `LEFT JOIN`
    /// otherwise.
    fn event_rows(&mut self, condition: &EventCondition, positive: bool) -> String {
        let catalog = self.catalog;
        let Some(schema) = catalog.events.get(&condition.name) else {
            return self.fail(code::UNKNOWN_EVENT);
        };
        self.deps.events.insert(condition.name.clone());
        self.need(ChangeClasses::EVENTS);
        let grouped =
            condition.agg.is_some() && !matches!(condition.window, Some(Window::SinceTrait(_)));
        let name = self.text(&condition.name);
        let mut filters = if grouped {
            vec!["e.workspace_id = $1".to_owned()]
        } else {
            vec![
                "e.workspace_id = p.workspace_id".to_owned(),
                "e.person_id = p.id".to_owned(),
            ]
        };
        filters.push(format!("e.name = {name}"));
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
        let scope = if grouped { SCOPE } else { "" };
        let from = format!("FROM events e WHERE {}{scope}", filters.join(" AND "));
        let Some(agg) = &condition.agg else {
            return format!("EXISTS (SELECT 1 {from})");
        };
        if agg.op.is_relative() {
            self.time_sensitive = true;
        }
        let (aggregate, is_time) = match agg.func {
            AggFn::Count => ("count(*)::float8".to_owned(), false),
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
                (aggregate, false)
            }
            AggFn::FirstTime | AggFn::LastTime => {
                let function = if agg.func == AggFn::FirstTime {
                    "min"
                } else {
                    "max"
                };
                (format!("{function}(e.occurred_at)"), true)
            }
        };
        let expr = if !grouped {
            format!("(SELECT {aggregate} {from})")
        } else if positive && !empty_aggregate_satisfies(agg) {
            // A person without a matching event has no group and the empty aggregate fails the
            // comparison, so the set of persons whose group passes is exactly the members.
            let having = self.aggregate_comparison(agg, &aggregate, is_time);
            return format!(
                "p.id IN (SELECT e.person_id {from} GROUP BY e.person_id HAVING {having})"
            );
        } else {
            let alias = format!("a{}", self.joins.len());
            self.joins.push(format!(
                "LEFT JOIN (SELECT e.person_id, {aggregate} AS v {from} GROUP BY e.person_id) \
                 {alias} ON {alias}.person_id = p.id"
            ));
            // A count of no events is 0 and any other aggregate of none is NULL, the value the
            // correlated subquery gave.
            if agg.func == AggFn::Count {
                format!("COALESCE({alias}.v, 0::float8)")
            } else {
                format!("{alias}.v")
            }
        };
        let comparison = self.aggregate_comparison(agg, &expr, is_time);
        format!("COALESCE(({comparison}), false)")
    }

    fn aggregate_comparison(&mut self, agg: &Aggregate, expr: &str, is_time: bool) -> String {
        if is_time {
            self.time(expr, Kind::Timestamp, agg.op, agg.value.as_ref())
        } else {
            self.number(expr, agg.op, agg.value.as_ref())
        }
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
                let (Some(start), Some(end)) = (timestamp(start), timestamp(end)) else {
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
                let since = self.trait_time(key, kind);
                format!("e.occurred_at >= {since}")
            }
        }
    }
}

/// A key that is present and not JSON null.
fn presence(field: &Field) -> String {
    let json = field.json();
    format!("(jsonb_typeof({json}) IS NOT NULL AND jsonb_typeof({json}) <> 'null')")
}

/// The value as `float8`, shared with the write path (see [`number_sql`]).
fn number_expr(json: &str, integral: bool) -> String {
    number_sql(json, integral)
}

/// The value as `timestamptz`, shared with the write path (see [`time_sql`]).
fn time_expr(field: &Field, kind: Kind) -> String {
    time_sql(&field.json(), &field.text(), kind == Kind::Date)
}

fn pair<T>(value: Option<&Value>, read: impl Fn(&Value) -> Option<T>) -> Option<[T; 2]> {
    match value?.as_array()?.as_slice() {
        [start, end] => Some([read(start)?, read(end)?]),
        _ => None,
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
            explicit_ranges: false,
            joins: Vec::new(),
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
