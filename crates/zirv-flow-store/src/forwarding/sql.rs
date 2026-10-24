//! The SQL of one rule's outbox insert: the event-name match, the property filter and the
//! mapping, evaluated over the events of a batch in the transaction that wrote them.
//!
//! Property names, pointers, keys and values are always bound, never spliced into the text.

use serde_json::Value;
use zirv_flow_core::forwarding::{EventField, Mapping, Source};
use zirv_flow_core::segment::{Op, PropertyFilter};

/// One bound value; the SQL casts each placeholder.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Bind {
    Text(String),
    TextArray(Vec<String>),
    Jsonb(Value),
}

/// The binds that follow the fixed ones.
#[derive(Default)]
struct Binds {
    first: usize,
    values: Vec<Bind>,
}

impl Binds {
    fn push(&mut self, bind: Bind) -> usize {
        self.values.push(bind);
        self.first + self.values.len() - 1
    }
}

/// `$1` workspace, `$2` rule, `$3` destination, `$4` event ids, `$5` their `occurred_at`, `$6`
/// the event name. Dynamic binds start at `$7`.
const FIXED: usize = 6;

/// The insert of one rule over `unnest($4, $5)` and the dynamic binds it needs.
pub(super) fn insert_for(mapping: &Mapping, filters: &[PropertyFilter]) -> (String, Vec<Bind>) {
    let mut binds = Binds {
        first: FIXED + 1,
        values: Vec::new(),
    };
    let mut pairs = Vec::with_capacity(mapping.len());
    for (key, source) in mapping.entries() {
        let key = binds.push(Bind::Text(key.to_owned()));
        pairs.push(format!("${key}::text, {}", source_sql(source, &mut binds)));
    }
    let mut conditions = vec!["e.name = $6".to_owned()];
    conditions.extend(filters.iter().map(|filter| filter_sql(filter, &mut binds)));
    let sql = format!(
        "INSERT INTO forwarding_outbox \
            (id, workspace_id, destination_id, rule_id, event_id, person_id, idempotency_key, \
             payload) \
         SELECT gen_random_uuid(), $1, $3, $2, e.id, e.person_id, \
                encode(sha256(convert_to(e.id::text || ':' || $3::text, 'UTF8')), 'hex'), \
                jsonb_build_object({pairs}) \
         FROM unnest($4::uuid[], $5::timestamptz[]) AS r(id, at) \
         JOIN events e ON e.occurred_at = r.at AND e.id = r.id AND e.workspace_id = $1 \
         WHERE {conditions} \
         ON CONFLICT (destination_id, event_id) DO NOTHING",
        pairs = pairs.join(", "),
        conditions = conditions.join(" AND "),
    );
    (sql, binds.values)
}

fn source_sql(source: &Source, binds: &mut Binds) -> String {
    match source {
        Source::Event(EventField::Id) => "to_jsonb(e.id::text)".to_owned(),
        Source::Event(EventField::Name) => "to_jsonb(e.name)".to_owned(),
        Source::Event(EventField::Time) => timestamp_sql("e.occurred_at"),
        Source::Event(EventField::ReceivedAt) => timestamp_sql("e.received_at"),
        Source::PersonId => "to_jsonb(e.person_id::text)".to_owned(),
        Source::Property(tokens) => {
            let path = binds.push(Bind::TextArray(tokens.clone()));
            format!("(e.properties #> ${path}::text[])")
        }
        Source::Static(value) => {
            let at = binds.push(Bind::Jsonb(value.clone()));
            format!("${at}::jsonb")
        }
    }
}

fn timestamp_sql(column: &str) -> String {
    format!(
        "to_jsonb(to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'))"
    )
}

/// A `LIKE` pattern that matches `text` literally, with `prefix` and `suffix` wildcards.
fn like_pattern(text: &str, leading: bool, trailing: bool) -> String {
    let mut pattern = String::new();
    if leading {
        pattern.push('%');
    }
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    if trailing {
        pattern.push('%');
    }
    pattern
}

/// A property predicate. A comparison needs the property to be present and of the value's type;
/// anything else does not match. A predicate the validator would have refused (a value of the
/// wrong shape) matches nothing.
fn filter_sql(filter: &PropertyFilter, binds: &mut Binds) -> String {
    const NEVER: &str = "false";
    let prop = binds.push(Bind::Text(filter.prop.clone()));
    let held = format!("(e.properties -> ${prop}::text)");
    let value = filter.value.as_ref();
    match filter.op {
        Op::Exists => format!("jsonb_exists(e.properties, ${prop}::text)"),
        Op::NotExists => format!("NOT jsonb_exists(e.properties, ${prop}::text)"),
        Op::Eq => match value {
            Some(value) if !value.is_null() => {
                let at = binds.push(Bind::Jsonb(value.clone()));
                format!("{held} = ${at}::jsonb")
            }
            _ => NEVER.to_owned(),
        },
        Op::Neq => match value {
            Some(value) if !value.is_null() => {
                let at = binds.push(Bind::Jsonb(value.clone()));
                format!(
                    "(jsonb_typeof({held}) = jsonb_typeof(${at}::jsonb) AND {held} <> ${at}::jsonb)"
                )
            }
            _ => NEVER.to_owned(),
        },
        Op::In | Op::NotIn => match value {
            Some(Value::Array(items)) if !items.is_empty() => {
                let at = binds.push(Bind::Jsonb(Value::Array(items.clone())));
                let members = format!("(SELECT jsonb_array_elements(${at}::jsonb))");
                if filter.op == Op::In {
                    format!("{held} IN {members}")
                } else {
                    format!(
                        "(jsonb_typeof({held}) IN (SELECT jsonb_typeof(m) FROM \
                         jsonb_array_elements(${at}::jsonb) m) AND {held} NOT IN {members})"
                    )
                }
            }
            _ => NEVER.to_owned(),
        },
        Op::Contains | Op::StartsWith => match value.and_then(Value::as_str) {
            Some(text) => {
                let pattern = like_pattern(text, filter.op == Op::Contains, true);
                let at = binds.push(Bind::Text(pattern));
                format!(
                    "(jsonb_typeof({held}) = 'string' AND \
                     (e.properties ->> ${prop}::text) LIKE ${at}::text ESCAPE '\\')"
                )
            }
            None => NEVER.to_owned(),
        },
        Op::Gt | Op::Gte | Op::Lt | Op::Lte => match value {
            Some(Value::Number(number)) => {
                let at = binds.push(Bind::Text(number.to_string()));
                let operator = match filter.op {
                    Op::Gt => ">",
                    Op::Gte => ">=",
                    Op::Lt => "<",
                    _ => "<=",
                };
                format!(
                    "CASE WHEN jsonb_typeof({held}) = 'number' \
                     THEN ({held})::numeric {operator} ${at}::text::numeric ELSE false END"
                )
            }
            _ => NEVER.to_owned(),
        },
        Op::Between => match value {
            Some(Value::Array(bounds)) if bounds.len() == 2 => {
                match (bounds[0].as_number(), bounds[1].as_number()) {
                    (Some(low), Some(high)) => {
                        let low = binds.push(Bind::Text(low.to_string()));
                        let high = binds.push(Bind::Text(high.to_string()));
                        format!(
                            "CASE WHEN jsonb_typeof({held}) = 'number' \
                             THEN ({held})::numeric BETWEEN ${low}::text::numeric \
                                  AND ${high}::text::numeric ELSE false END"
                        )
                    }
                    _ => NEVER.to_owned(),
                }
            }
            _ => NEVER.to_owned(),
        },
        _ => NEVER.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::forwarding::{Mapping, parse_filter};

    use super::*;

    fn mapping() -> Mapping {
        Mapping::parse(&json!({
            "id": {"from": "event.id"},
            "plan": {"from": "event.properties", "pointer": "/plan"},
            "src": {"value": "shop"},
        }))
        .unwrap()
    }

    #[test]
    fn the_insert_is_one_set_based_statement_with_everything_bound() {
        let filters = parse_filter(
            "order",
            &json!([{"prop": "plan", "op": "eq", "value": "pro'; DROP TABLE x; --"}]),
        )
        .unwrap();

        let (sql, binds) = insert_for(&mapping(), &filters);

        assert!(sql.contains("unnest($4::uuid[], $5::timestamptz[])"));
        assert!(sql.contains("ON CONFLICT (destination_id, event_id) DO NOTHING"));
        assert!(sql.contains("jsonb_build_object($7::text, to_jsonb(e.id::text), $8::text"));
        assert!(!sql.contains("DROP TABLE"), "values are bound, not spliced: {sql}");
        assert!(binds.contains(&Bind::Jsonb(json!("pro'; DROP TABLE x; --"))));
        assert!(binds.contains(&Bind::TextArray(vec!["plan".into()])));
        // Every placeholder past the fixed ones has a bind.
        let highest = FIXED + binds.len();
        assert!(sql.contains(&format!("${highest}")), "{sql}");
        assert!(!sql.contains(&format!("${}", highest + 1)), "{sql}");
    }

    #[test]
    fn like_patterns_escape_their_wildcards() {
        assert_eq!(like_pattern("50%_off\\", true, true), "%50\\%\\_off\\\\%");
        assert_eq!(like_pattern("ab", false, true), "ab%");
    }

    #[test]
    fn a_predicate_with_a_value_of_the_wrong_shape_matches_nothing() {
        let mut binds = Binds {
            first: 7,
            values: Vec::new(),
        };
        let bad = PropertyFilter {
            prop: "n".into(),
            op: Op::Gt,
            value: Some(json!("text")),
        };

        assert_eq!(filter_sql(&bad, &mut binds), "false");
    }
}
