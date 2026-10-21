//! What the segment assistant tells the model (architecture 13.2, 13.4) and how a literal stays
//! out of it.
//!
//! The model sees the workspace's model of the world: trait keys, types and descriptions, event
//! names with their property names and types, list and segment names, consent topics and
//! identifier kinds. It never sees a trait or event value, a preview count or sample, a member, or
//! the explain result of any person.
//!
//! The one place a value can hide is the saved definition itself, which holds the literals a
//! marketer typed ("country is DK"). Unless the workspace allows personal data, every string
//! literal compared against a trait or event property that is not declared non-personal is
//! replaced by a numbered placeholder before the definition is sent, the way a manifest's header
//! values are, and the placeholders are put back into the model's draft. A placeholder has to come
//! back in the node it was sent in: one the model moves, or makes up, is an issue it must fix.

use hashbrown::HashSet;
use serde_json::Value;
use uuid::Uuid;
use zirv_flow_ai::{AiError, ChatRequest, Content, Privacy, RequestBuilder, ToolSpec};
use zirv_flow_core::segment::{MAX_NODES, NODE_KEYS, pointer_push};
use zirv_flow_store::CatalogDetails;

use super::prompt::{MAX_LISTS, MAX_OUTPUT_TOKENS, MAX_TRAITS, omitted};

/// The most events the context lists.
pub(crate) const MAX_EVENTS: usize = 200;
/// The most properties of one event the context lists.
pub(crate) const MAX_PROPERTIES: usize = 100;
/// The most other segments the context lists.
pub(crate) const MAX_SEGMENTS: usize = 200;
/// The most consent topics the context lists.
pub(crate) const MAX_TOPICS: usize = 200;
/// How deep the walks below descend. A definition deeper than the compiler allows is refused by
/// the validator; the walks only have to stop.
pub(crate) const WALK_DEPTH: usize = 24;

pub(crate) const SEGMENT_INSTRUCTIONS: &str = "\
You help a marketer write the rule that decides who is in a segment. Answer by calling the \
draft_segment tool exactly once, and by nothing else. Put the whole definition in the \
`definition` argument.

Use only what is listed below: trait keys, event names and their property names, list ids, \
segment ids, consent topics and identifier kinds. Never invent one, and never write a list or \
segment id that is not listed. If a saved definition is shown, change it as little as the request \
needs. A value shown as (kept:N) stands for a value you are not shown: keep it exactly, in the \
same place, to leave it as it is, and write a real value only for a condition you add or change.

If your answer is refused, the problems are listed with their JSON pointers: fix exactly those \
and call the tool again.";

pub(crate) const EXPLAIN_INSTRUCTIONS: &str = "\
You help a marketer understand a saved segment. Answer by calling the explain_segment tool \
exactly once, and by nothing else. Give a one or two sentence summary of who the segment \
holds, then one short plain-language sentence for every condition of the definition, each \
named by its JSON pointer. Cite every condition (a trait, event, list, segment, consent or \
identifier node) exactly once. You may also explain an all, any or not node by its pointer. \
Write plain text only: no markup. A value shown as (kept:N) stands for a value you are not \
shown; you may repeat it as written.

If your answer is refused, the problems are listed with their JSON pointers: fix exactly those \
and call the tool again.";

/// The definition format, as the model needs it.
pub(crate) const SEGMENT_REFERENCE: &str = "\
Segment definition v1 (JSON). The root is one node with \"v\": 1 added beside its key. A node is \
an object with exactly one node key:
- {\"all\": [nodes]} and {\"any\": [nodes]}: every or at least one child holds. {\"not\": node}.
- {\"trait\": key, \"op\": op, \"value\": value}: a trait of the person. The operators depend on \
the trait's type. string: eq, neq, in, not_in, contains, starts_with, exists, not_exists. number: \
eq, neq, gt, gte, lt, lte, between, in, not_in, exists, not_exists. boolean: eq, exists, \
not_exists. timestamp and date: before, after, between, within_last, more_than_ago, within_next \
(a duration such as P30D), exists, not_exists. string_list: contains, contains_all, is_empty, \
exists, not_exists. exists, not_exists and is_empty take no value. in and not_in take an array, \
between takes [low, high].
- {\"event\": name, \"where\": [{\"prop\": property, \"op\": op, \"value\": value}], \"agg\": \
{\"fn\": count|sum|min|max|avg|first_time|last_time, \"prop\": property, \"op\": op, \"value\": \
value}, \"window\": {\"last\": \"P90D\"} or {\"between\": [start, end]} or {\"since_trait\": \
key}}: people with events of that name. where, agg and window are optional; without agg the \
condition is true when at least one event matches. count takes no prop.
- {\"list\": id} and {\"segment\": id}: membership of a list or another segment.
- {\"consent\": {\"channel\": \"email\", \"topic\": key, \"status\": subscribed|unsubscribed|\
pending_confirmation}}: the person's consent state. The topic is optional.
- {\"identifier\": kind}: the person has an identifier of the kind: email, phone, anonymous_id or \
external:<namespace>.
A missing trait or property makes every operator false except not_exists. At most 200 nodes and \
10 levels.

Example:
{\"v\":1,\"all\":[{\"trait\":\"plan\",\"op\":\"eq\",\"value\":\"pro\"},\
{\"event\":\"order_placed\",\"where\":[{\"prop\":\"total\",\"op\":\"gte\",\"value\":50}],\
\"agg\":{\"fn\":\"count\",\"op\":\"gte\",\"value\":2},\"window\":{\"last\":\"P90D\"}},\
{\"not\":{\"list\":\"0190d0f2-0000-7000-8000-0000000000a1\"}}]}";

/// What a literal is compared against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Owner {
    /// A trait key.
    Trait(String),
    /// An event name and a property name; the property is empty for an aggregate with none.
    Property(String, String),
}

/// A string literal of a definition: where it is, and what it is compared against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Slot {
    /// The JSON pointer of the string itself.
    pub(crate) pointer: String,
    pub(crate) owner: Owner,
}

/// A node of a definition in its wire form.
#[derive(Clone, Debug)]
pub(crate) struct NodeRef<'a> {
    /// The JSON pointer of the node; empty for the root.
    pub(crate) pointer: String,
    /// The node key: one of the nine in `NODE_KEYS`.
    pub(crate) kind: &'static str,
    pub(crate) value: &'a Value,
}

impl NodeRef<'_> {
    /// Whether the node is a condition and not a composition.
    pub(crate) fn is_leaf(&self) -> bool {
        !matches!(self.kind, "all" | "any" | "not")
    }
}

/// The node keys, as `&'static str`.
fn key_of(map: &serde_json::Map<String, Value>) -> Option<&'static str> {
    NODE_KEYS.into_iter().find(|key| map.contains_key(*key))
}

/// Every node of a definition's wire form, parents before children, in document order. It is
/// tolerant: a value that is not a node is skipped, and the walk stops at a fixed depth and at
/// one node more than a definition may hold, so a hostile draft cannot make it expensive. What it
/// misses, the validator refuses.
pub(crate) fn nodes(root: &Value) -> Vec<NodeRef<'_>> {
    let mut found = Vec::new();
    let mut stack = vec![(root, String::new(), 1_usize)];
    while let Some((value, pointer, depth)) = stack.pop() {
        let Some(map) = value.as_object() else {
            continue;
        };
        let Some(kind) = key_of(map) else {
            continue;
        };
        if found.len() > MAX_NODES {
            break;
        }
        found.push(NodeRef {
            pointer: pointer.clone(),
            kind,
            value,
        });
        if depth >= WALK_DEPTH {
            continue;
        }
        match kind {
            "all" | "any" => {
                let base = pointer_push(&pointer, kind);
                if let Some(children) = map.get(kind).and_then(Value::as_array) {
                    for (index, child) in children.iter().enumerate().take(MAX_NODES + 1).rev() {
                        stack.push((child, pointer_push(&base, &index.to_string()), depth + 1));
                    }
                }
            }
            "not" => {
                if let Some(child) = map.get("not") {
                    stack.push((child, pointer_push(&pointer, "not"), depth + 1));
                }
            }
            _ => {}
        }
    }
    found
}

/// Whether a JSON value is a scalar literal: a string, a number or a boolean.
fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// The scalar literals of one `value` member, at `pointer`: the value itself, or each item of an
/// array (the items of `in`, `not_in` and `between`).
fn value_slots(value: Option<&Value>, pointer: &str, owner: &Owner, out: &mut Vec<Slot>) {
    match value {
        Some(scalar) if is_scalar(scalar) => out.push(Slot {
            pointer: pointer.to_owned(),
            owner: owner.clone(),
        }),
        Some(Value::Array(items)) => {
            for (index, _) in items.iter().enumerate().filter(|(_, item)| is_scalar(item)) {
                out.push(Slot {
                    pointer: pointer_push(pointer, &index.to_string()),
                    owner: owner.clone(),
                });
            }
        }
        _ => {}
    }
}

/// Every scalar literal a definition compares a trait, an event property or an aggregate over a
/// property against. A count with no property compares a number of events, not a value of any
/// trait or property, so it has no slot.
pub(crate) fn literal_slots(definition: &Value) -> Vec<Slot> {
    let mut slots = Vec::new();
    for node in nodes(definition) {
        let Some(map) = node.value.as_object() else {
            continue;
        };
        match node.kind {
            "trait" => {
                if let Some(key) = map.get("trait").and_then(Value::as_str) {
                    value_slots(
                        map.get("value"),
                        &pointer_push(&node.pointer, "value"),
                        &Owner::Trait(key.to_owned()),
                        &mut slots,
                    );
                }
            }
            "event" => {
                let Some(name) = map.get("event").and_then(Value::as_str) else {
                    continue;
                };
                let property = |filter: &Value| {
                    Owner::Property(
                        name.to_owned(),
                        filter
                            .get("prop")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    )
                };
                let filters = pointer_push(&node.pointer, "where");
                for (index, filter) in map
                    .get("where")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let at = pointer_push(&pointer_push(&filters, &index.to_string()), "value");
                    value_slots(filter.get("value"), &at, &property(filter), &mut slots);
                }
                if let Some(agg) = map
                    .get("agg")
                    .filter(|agg| agg.get("prop").is_some_and(Value::is_string))
                {
                    let at = pointer_push(&pointer_push(&node.pointer, "agg"), "value");
                    value_slots(agg.get("value"), &at, &property(agg), &mut slots);
                }
            }
            _ => {}
        }
    }
    slots
}

/// What the workspace declares non-personal: only these literals may be sent in the clear.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClearKeys {
    traits: HashSet<String>,
    properties: HashSet<(String, String)>,
}

impl ClearKeys {
    pub(crate) fn of(details: &CatalogDetails) -> Self {
        Self {
            traits: details
                .traits
                .iter()
                .filter(|entry| !entry.pii)
                .map(|entry| entry.key.clone())
                .collect(),
            properties: details
                .events
                .iter()
                .flat_map(|event| {
                    event
                        .properties
                        .iter()
                        .filter(|property| !property.pii)
                        .map(|property| (event.name.clone(), property.name.clone()))
                })
                .collect(),
        }
    }

    /// Whether a literal compared against `owner` may be personal. A trait or property the
    /// workspace does not define counts as personal.
    fn is_personal(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Trait(key) => !self.traits.contains(key),
            Owner::Property(event, property) => {
                !self.properties.contains(&(event.clone(), property.clone()))
            }
        }
    }
}

/// One literal that was replaced.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Placeholder {
    token: String,
    slot: Slot,
    /// The literal as it was, with its JSON type.
    original: Value,
}

/// The literals replaced in the definition a model was shown, to put back in its answer.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Placeholders {
    entries: Vec<Placeholder>,
}

const TOKEN_OPEN: &str = "(kept:";

/// Whether the text has the shape of a placeholder.
fn is_token(text: &str) -> bool {
    text.strip_prefix(TOKEN_OPEN)
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// The line for a placeholder that is not where it was sent.
pub(crate) const PLACEHOLDER_MOVED: &str = "placeholder_moved";
/// The line for a placeholder the model made up.
pub(crate) const PLACEHOLDER_INVENTED: &str = "placeholder_invented";

impl Placeholders {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Puts every placeholder that sits under the trait, or the event property, it was sent under
    /// back as its literal (with its JSON type), wherever the node moved to. Then names every other
    /// string that looks like one, as `code at <pointer>`. A placeholder under another owner and
    /// an invented one are both left in place.
    pub(crate) fn restore(&self, draft: &mut Value) -> Vec<String> {
        let mut restored: HashSet<String> = HashSet::new();
        for slot in literal_slots(draft) {
            let Some(Value::String(text)) = draft.pointer(&slot.pointer) else {
                continue;
            };
            let Some(entry) = self.entries.iter().find(|entry| entry.token == *text) else {
                continue;
            };
            if entry.slot.owner == slot.owner
                && let Some(target) = draft.pointer_mut(&slot.pointer)
            {
                *target = entry.original.clone();
                restored.insert(slot.pointer);
            }
        }
        let mut lines = Vec::new();
        self.leftovers(draft, "", &restored, 0, &mut lines);
        lines
    }

    fn leftovers(
        &self,
        value: &Value,
        pointer: &str,
        restored: &HashSet<String>,
        depth: usize,
        lines: &mut Vec<String>,
    ) {
        if depth > 2 * WALK_DEPTH + 8 {
            return;
        }
        match value {
            Value::String(text) if is_token(text) && !restored.contains(pointer) => {
                let known = self.entries.iter().any(|entry| entry.token == *text);
                let code = if known {
                    PLACEHOLDER_MOVED
                } else {
                    PLACEHOLDER_INVENTED
                };
                lines.push(format!("{code} at {pointer}"));
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    let at = pointer_push(pointer, &index.to_string());
                    self.leftovers(item, &at, restored, depth + 1, lines);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    let at = pointer_push(pointer, key);
                    self.leftovers(item, &at, restored, depth + 1, lines);
                }
            }
            _ => {}
        }
    }

    /// The text with every placeholder it repeats replaced by its literal. For an explanation,
    /// which goes to the admin who owns the definition and is never stored.
    pub(crate) fn restore_text(&self, text: &str) -> String {
        // One pass: replaced text is never scanned again, so a literal that reads like another
        // placeholder stays as it is.
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(TOKEN_OPEN) {
            out.push_str(&rest[..at]);
            rest = &rest[at..];
            let end = rest.find(')').map_or(rest.len(), |close| close + 1);
            let candidate = &rest[..end];
            match self.entries.iter().find(|entry| entry.token == candidate) {
                Some(entry) => out.push_str(&match &entry.original {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                }),
                None => out.push_str(candidate),
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }
}

/// The definition with each personal string literal replaced by a placeholder, and the table to
/// put them back.
pub(crate) fn mask(definition: &Value, clear: &ClearKeys) -> (Value, Placeholders) {
    let (mut masked, table) = mask_many(std::slice::from_ref(definition), clear);
    (masked.pop().unwrap_or(Value::Null), table)
}

/// [`mask`] for several definitions that share one table: the placeholders are numbered across all
/// of them, so a placeholder names one literal wherever it appears. A flow holds many conditions,
/// each one a definition of its own.
pub(crate) fn mask_many(definitions: &[Value], clear: &ClearKeys) -> (Vec<Value>, Placeholders) {
    let mut table = Placeholders::default();
    let mut masked_all = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let mut masked = definition.clone();
        for slot in literal_slots(definition) {
            if !clear.is_personal(&slot.owner) {
                continue;
            }
            let Some(original) = masked.pointer(&slot.pointer).cloned() else {
                continue;
            };
            let token = format!("{TOKEN_OPEN}{})", table.entries.len() + 1);
            if let Some(target) = masked.pointer_mut(&slot.pointer) {
                *target = Value::String(token.clone());
            }
            table.entries.push(Placeholder {
                token,
                slot,
                original,
            });
        }
        masked_all.push(masked);
    }
    (masked_all, table)
}

/// The catalog as request content: names, types and descriptions, each list capped with a note
/// of what was left out. `own` is the segment being drafted, which is left out of the segments a
/// definition may refer to.
pub(crate) fn render_catalog(details: &CatalogDetails, own: Option<Uuid>) -> Content {
    let mut text = String::new();
    let mut notes = Vec::new();
    text.push_str("Traits (key, type, description):\n");
    if details.traits.is_empty() {
        text.push_str("(none)\n");
    }
    for entry in details.traits.iter().take(MAX_TRAITS) {
        text.push_str(&format!("- {} ({})", entry.key, entry.trait_type.as_str()));
        if !entry.description.is_empty() {
            text.push_str(&format!(": {}", entry.description));
        }
        text.push('\n');
    }
    omitted(&mut notes, "traits", details.traits.len(), MAX_TRAITS);

    text.push_str("Events (name: property type, ...):\n");
    if details.events.is_empty() {
        text.push_str("(none)\n");
    }
    for event in details.events.iter().take(MAX_EVENTS) {
        let properties: Vec<String> = event
            .properties
            .iter()
            .take(MAX_PROPERTIES)
            .map(|property| format!("{} {}", property.name, property.property_type.as_str()))
            .collect();
        text.push_str(&format!("- {}: {}\n", event.name, properties.join(", ")));
        omitted(
            &mut notes,
            &format!("properties of {}", event.name),
            event.properties.len(),
            MAX_PROPERTIES,
        );
    }
    omitted(&mut notes, "events", details.events.len(), MAX_EVENTS);

    text.push_str("Lists (id, name):\n");
    if details.lists.is_empty() {
        text.push_str("(none)\n");
    }
    for (id, name) in details.lists.iter().take(MAX_LISTS) {
        text.push_str(&format!("- {id} {name}\n"));
    }
    omitted(&mut notes, "lists", details.lists.len(), MAX_LISTS);

    let others: Vec<&(Uuid, String)> = details
        .segments
        .iter()
        .filter(|(id, _)| Some(*id) != own)
        .collect();
    text.push_str("Other segments (id, name):\n");
    if others.is_empty() {
        text.push_str("(none)\n");
    }
    for (id, name) in others.iter().take(MAX_SEGMENTS) {
        text.push_str(&format!("- {id} {name}\n"));
    }
    omitted(&mut notes, "segments", others.len(), MAX_SEGMENTS);

    text.push_str("Consent topics (channel, key, name):\n");
    if details.topics.is_empty() {
        text.push_str("(none)\n");
    }
    for topic in details.topics.iter().take(MAX_TOPICS) {
        text.push_str(&format!(
            "- {} {} {}\n",
            topic.channel.as_str(),
            topic.key,
            topic.name
        ));
    }
    omitted(&mut notes, "topics", details.topics.len(), MAX_TOPICS);

    text.push_str("Consent channels: email.\n");
    text.push_str("Identifier kinds: email, phone, anonymous_id, external:<namespace>.\n");
    for note in notes {
        text.push_str(&note);
        text.push('\n');
    }
    Content::Schema(text)
}

/// The saved definition as request content. Under [`Privacy::SchemaOnly`] its personal literals
/// are placeholders and the content is plain schema; with the workspace's opt-in the definition is
/// sent as it is, as personal data, fenced like any other.
pub(crate) fn render_saved(
    definition: &Value,
    details: &CatalogDetails,
    privacy: Privacy,
) -> (Content, Placeholders) {
    match privacy {
        Privacy::SchemaOnly => {
            let (masked, table) = mask(definition, &ClearKeys::of(details));
            (
                Content::Schema(format!("Saved definition: {masked}\n")),
                table,
            )
        }
        Privacy::PersonalAllowed => (
            Content::Personal(format!("Saved definition: {definition}\n")),
            Placeholders::default(),
        ),
    }
}

/// The context of a segment draft: the catalog, and for an existing segment its saved definition.
pub(crate) fn draft_context(
    details: &CatalogDetails,
    own: Option<Uuid>,
    saved: Option<&Value>,
    privacy: Privacy,
) -> (Vec<Content>, Placeholders) {
    let (saved, table) = match saved {
        Some(definition) => render_saved(definition, details, privacy),
        None => (
            Content::Schema("No definition is saved yet.\n".to_owned()),
            Placeholders::default(),
        ),
    };
    (vec![render_catalog(details, own), saved], table)
}

/// The context of an explanation: the catalog, so a trait or list can be named, and the saved
/// definition.
pub(crate) fn explain_context(
    details: &CatalogDetails,
    own: Uuid,
    saved: &Value,
    privacy: Privacy,
) -> (Vec<Content>, Placeholders) {
    draft_context(details, Some(own), Some(saved), privacy)
}

/// The request for an explanation: the fixed instructions and the definition format, the context
/// and the one tool.
pub(crate) fn build_explain_request(
    privacy: Privacy,
    mut context: Vec<Content>,
    tool: ToolSpec,
) -> Result<ChatRequest, AiError> {
    context.push(Content::Schema("Explain this segment.".to_owned()));
    Ok(RequestBuilder::new(privacy)
        .system(Content::Instruction(EXPLAIN_INSTRUCTIONS.into()))?
        .system(Content::Instruction(SEGMENT_REFERENCE.into()))?
        .user(context)?
        .tool(tool)?
        .max_output_tokens(MAX_OUTPUT_TOKENS)
        .build())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_store::{EventDetail, PropertyDetail, TraitDetail};

    use super::*;

    fn details() -> CatalogDetails {
        CatalogDetails {
            traits: vec![
                TraitDetail {
                    key: "city".into(),
                    trait_type: TraitType::String,
                    description: "Where they live".into(),
                    pii: true,
                },
                TraitDetail {
                    key: "plan".into(),
                    trait_type: TraitType::String,
                    description: String::new(),
                    pii: false,
                },
            ],
            events: vec![EventDetail {
                name: "order".into(),
                properties: vec![
                    PropertyDetail {
                        name: "coupon".into(),
                        property_type: PropertyType::String,
                        pii: true,
                    },
                    PropertyDetail {
                        name: "channel".into(),
                        property_type: PropertyType::String,
                        pii: false,
                    },
                ],
            }],
            lists: vec![(Uuid::nil(), "VIPs".into())],
            ..CatalogDetails::default()
        }
    }

    fn definition() -> Value {
        json!({"v": 1, "all": [
            {"trait": "city", "op": "in", "value": ["Aarhus", "Odense"]},
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"event": "order",
             "where": [{"prop": "coupon", "op": "eq", "value": "SECRET10"},
                       {"prop": "channel", "op": "eq", "value": "web"}],
             "agg": {"fn": "count", "op": "gte", "value": 2}},
            {"not": {"trait": "ghost", "op": "eq", "value": "Hidden"}}
        ]})
    }

    #[test]
    fn nodes_come_in_document_order_with_their_pointers() {
        let definition = definition();

        let found: Vec<(String, &str)> = nodes(&definition)
            .into_iter()
            .map(|node| (node.pointer, node.kind))
            .collect();

        assert_eq!(
            found,
            [
                ("".to_owned(), "all"),
                ("/all/0".to_owned(), "trait"),
                ("/all/1".to_owned(), "trait"),
                ("/all/2".to_owned(), "event"),
                ("/all/3".to_owned(), "not"),
                ("/all/3/not".to_owned(), "trait"),
            ]
        );
        assert!(nodes(&json!({"v": 1, "list": "x"}))[0].is_leaf());
        assert!(nodes(&json!("text")).is_empty());
        // A pointer from the walk resolves in the definition.
        assert_eq!(
            definition.pointer("/all/3/not/trait"),
            Some(&json!("ghost"))
        );
    }

    #[test]
    fn a_hostile_nesting_is_cut_off() {
        let mut node = json!({"list": "x"});
        for _ in 0..200 {
            node = json!({"not": node});
        }

        assert!(nodes(&node).len() <= WALK_DEPTH);
    }

    #[test]
    fn personal_literals_become_placeholders_and_clear_ones_stay() {
        let (masked, table) = mask(&definition(), &ClearKeys::of(&details()));

        let text = masked.to_string();
        for hidden in ["Aarhus", "Odense", "SECRET10", "Hidden"] {
            assert!(!text.contains(hidden), "{hidden} in {text}");
        }
        for shown in ["pro", "web"] {
            assert!(text.contains(shown), "{shown} missing");
        }
        assert_eq!(masked.pointer("/all/0/value/0"), Some(&json!("(kept:1)")));
        assert_eq!(masked.pointer("/all/0/value/1"), Some(&json!("(kept:2)")));
        // A trait the workspace does not define is personal until declared otherwise.
        assert_eq!(masked.pointer("/all/3/not/value"), Some(&json!("(kept:4)")));
        // The aggregate's number is not a string literal.
        assert_eq!(masked.pointer("/all/2/agg/value"), Some(&json!(2)));
        assert_eq!(table.entries.len(), 4);
    }

    #[test]
    fn restoring_an_unchanged_draft_gives_back_the_saved_definition() {
        let saved = definition();
        let (mut draft, table) = mask(&saved, &ClearKeys::of(&details()));
        draft["all"][1]["value"] = json!("team");

        let lines = table.restore(&mut draft);

        assert_eq!(lines, Vec::<String>::new());
        let mut expected = saved;
        expected["all"][1]["value"] = json!("team");
        assert_eq!(draft, expected);
    }

    #[test]
    fn a_placeholder_put_under_another_trait_is_an_issue_and_stays_in_place() {
        let (mut draft, table) = mask(&definition(), &ClearKeys::of(&details()));
        // The model puts the city placeholder on the plan.
        draft["all"][1]["value"] = json!("(kept:1)");

        let lines = table.restore(&mut draft);

        assert_eq!(lines, ["placeholder_moved at /all/1/value"]);
        // The ones that stayed under their trait are restored.
        assert_eq!(draft.pointer("/all/0/value/0"), Some(&json!("Aarhus")));
        assert!(!lines.join("").contains("Aarhus"));
    }

    /// The masked definition, changed by `change`, restored: no issue, and the literals back.
    fn restored_after(change: impl FnOnce(&mut Vec<Value>)) -> Value {
        let (mut draft, table) = mask(&definition(), &ClearKeys::of(&details()));
        change(draft["all"].as_array_mut().unwrap());
        let lines = table.restore(&mut draft);
        assert_eq!(lines, Vec::<String>::new());
        draft
    }

    #[test]
    fn a_placeholder_is_restored_wherever_its_node_moved_under_the_same_owner() {
        let original = definition();
        let nodes = original["all"].as_array().unwrap().clone();

        // A sibling before it is removed: everything shifts up.
        let removed = restored_after(|all| {
            all.remove(1);
        });
        assert_eq!(removed["all"], json!([nodes[0], nodes[2], nodes[3]]));
        // The node that held a placeholder is removed, the others keep theirs.
        let without_city = restored_after(|all| {
            all.remove(0);
        });
        assert_eq!(without_city["all"], json!([nodes[1], nodes[2], nodes[3]]));
        // A sibling is inserted before the masked nodes.
        let inserted = restored_after(|all| {
            all.insert(0, json!({"trait": "plan", "op": "exists"}));
        });
        assert_eq!(inserted["all"][1], nodes[0]);
        assert_eq!(inserted["all"][3], nodes[2]);
        // The nodes are reordered.
        let reordered = restored_after(|all| all.reverse());
        assert_eq!(
            reordered["all"],
            json!([nodes[3], nodes[2], nodes[1], nodes[0]])
        );
        // The condition is moved into a group of its own.
        let nested = restored_after(|all| {
            let city = all.remove(0);
            all.push(json!({"any": [city]}));
        });
        assert_eq!(nested["all"][3]["any"][0], nodes[0]);
    }

    #[test]
    fn a_placeholder_moved_onto_another_property_of_the_event_is_refused() {
        let (mut draft, table) = mask(&definition(), &ClearKeys::of(&details()));
        draft["all"][2]["where"][0]["prop"] = json!("channel");

        let lines = table.restore(&mut draft);

        assert_eq!(lines, ["placeholder_moved at /all/2/where/0/value"]);
    }

    fn personal_numbers() -> Value {
        json!({"v": 1, "all": [
            {"trait": "salary", "op": "between", "value": [90000, 120000]},
            {"trait": "salary", "op": "gt", "value": 1.5},
            {"trait": "vip", "op": "eq", "value": true},
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"event": "order",
             "where": [{"prop": "coupon", "op": "eq", "value": 7}],
             "agg": {"fn": "sum", "prop": "coupon", "op": "gte", "value": 500}},
            {"event": "order", "agg": {"fn": "count", "op": "gte", "value": 2}},
            {"event": "order", "agg": {"fn": "sum", "prop": "total", "op": "gte", "value": 500}}
        ]})
    }

    #[test]
    fn numbers_and_booleans_on_personal_owners_are_masked_and_restored_with_their_type() {
        let saved = personal_numbers();
        let (masked, table) = mask(&saved, &ClearKeys::of(&details()));

        let text = masked.to_string();
        for hidden in ["90000", "120000", "1.5", "true", "500", "\"value\":7"] {
            assert!(!text.contains(hidden), "{hidden} in {text}");
        }
        assert_eq!(masked.pointer("/all/0/value/0"), Some(&json!("(kept:1)")));
        assert_eq!(masked.pointer("/all/0/value/1"), Some(&json!("(kept:2)")));
        // A clear string stays, and a plain count has nothing to mask.
        assert_eq!(masked.pointer("/all/3/value"), Some(&json!("pro")));
        assert_eq!(masked.pointer("/all/5/agg/value"), Some(&json!(2)));
        // The restore gives back the typed JSON values, before anything validates them.
        let mut draft = masked;
        let lines = table.restore(&mut draft);
        assert_eq!(lines, Vec::<String>::new());
        assert_eq!(draft, saved);
        assert_eq!(draft.pointer("/all/0/value/0"), Some(&json!(90000)));
        assert_eq!(draft.pointer("/all/2/value"), Some(&json!(true)));
    }

    #[test]
    fn a_clear_numeric_property_is_not_masked() {
        let mut details = details();
        details.events[0].properties.push(PropertyDetail {
            name: "total".into(),
            property_type: PropertyType::Number,
            pii: false,
        });
        let (masked, _) = mask(&personal_numbers(), &ClearKeys::of(&details));

        assert_eq!(masked.pointer("/all/6/agg/value"), Some(&json!(500)));
        assert_eq!(masked.pointer("/all/4/agg/value"), Some(&json!("(kept:6)")));
    }

    #[test]
    fn every_node_kind_of_the_ast_is_walked_and_only_value_carrying_ones_have_literals() {
        let samples: [(&str, Value, usize); 9] = [
            (
                "all",
                json!({"all": [{"trait": "t", "op": "eq", "value": "a"}]}),
                1,
            ),
            (
                "any",
                json!({"any": [{"trait": "t", "op": "eq", "value": "a"}]}),
                1,
            ),
            (
                "not",
                json!({"not": {"trait": "t", "op": "eq", "value": "a"}}),
                1,
            ),
            (
                "trait",
                json!({"trait": "t", "op": "in", "value": ["a", 2, false]}),
                3,
            ),
            (
                "event",
                json!({"event": "e", "where": [{"prop": "p", "op": "eq", "value": 1}],
                    "agg": {"fn": "max", "prop": "p", "op": "gt", "value": 2.5},
                    "window": {"last": "P7D"}}),
                2,
            ),
            (
                "list",
                json!({"list": "0190d0f2-0000-7000-8000-0000000000a2"}),
                0,
            ),
            (
                "segment",
                json!({"segment": "0190d0f2-0000-7000-8000-0000000000a2"}),
                0,
            ),
            (
                "consent",
                json!({"consent": {"channel": "email", "topic": "news", "status": "subscribed"}}),
                0,
            ),
            ("identifier", json!({"identifier": "email"}), 0),
        ];
        // A new node kind in the AST has to be added here, and so decided on, before this passes.
        let kinds: Vec<&str> = samples.iter().map(|(kind, _, _)| *kind).collect();
        assert_eq!(kinds, zirv_flow_core::segment::NODE_KEYS);
        for (kind, node, expected) in &samples {
            assert_eq!(literal_slots(node).len(), *expected, "{kind}");
            let (masked, table) = mask(node, &ClearKeys::default());
            assert_eq!(table.entries.len(), *expected, "{kind}");
            assert_eq!(
                masked.to_string().contains("(kept:"),
                *expected > 0,
                "{kind}"
            );
        }
    }

    #[test]
    fn an_explanation_restore_does_not_replace_a_literal_that_reads_like_a_placeholder() {
        // The first literal is the text of the second placeholder.
        let saved = json!({"v": 1, "all": [
            {"trait": "city", "op": "eq", "value": "(kept:2)"},
            {"trait": "city", "op": "eq", "value": "Odense"}
        ]});
        let (_, table) = mask(&saved, &ClearKeys::default());

        let text = table.restore_text("(kept:1) and (kept:2), (kept:3) and (kept:");

        assert_eq!(text, "(kept:2) and Odense, (kept:3) and (kept:");
    }

    #[test]
    fn a_placeholder_the_model_made_up_is_an_issue_wherever_it_is() {
        let (mut draft, table) = mask(&definition(), &ClearKeys::of(&details()));
        draft["all"][1]["value"] = json!("(kept:99)");
        draft["all"][2]["event"] = json!("(kept:1)");

        let lines = table.restore(&mut draft);

        assert_eq!(
            lines,
            [
                "placeholder_invented at /all/1/value",
                "placeholder_moved at /all/2/event",
                "placeholder_moved at /all/2/where/0/value"
            ]
        );
    }

    #[test]
    fn a_literal_that_looks_like_a_placeholder_is_not_one_unless_it_is() {
        for text in ["(kept:1)", "(kept:42)"] {
            assert!(is_token(text));
        }
        for text in ["(kept:)", "(kept:x)", "kept:1", "(kept:1) ", "(kept)"] {
            assert!(!is_token(text), "{text}");
        }
    }

    #[test]
    fn an_explanation_gets_the_literals_back() {
        let (_, table) = mask(&definition(), &ClearKeys::of(&details()));

        let text = table.restore_text("Lives in (kept:1) or (kept:2); coupon (kept:3).");

        assert_eq!(text, "Lives in Aarhus or Odense; coupon SECRET10.");
    }

    #[test]
    fn the_catalog_lists_names_and_types_and_never_a_value() {
        let content = render_catalog(&details(), None);

        let Content::Schema(text) = content else {
            panic!("the catalog is schema text");
        };
        assert!(text.contains("- city (string): Where they live"), "{text}");
        assert!(text.contains("- order: coupon string, channel string"));
        assert!(text.contains(&format!("- {} VIPs", Uuid::nil())));
        assert!(
            text.contains("Identifier kinds: email, phone, anonymous_id, external:<namespace>.")
        );
        assert!(text.contains("Other segments (id, name):\n(none)"));
    }

    #[test]
    fn the_segment_being_drafted_is_not_one_it_may_refer_to() {
        let own = Uuid::now_v7();
        let other = Uuid::now_v7();
        let mut details = details();
        details.segments = vec![(own, "Mine".into()), (other, "Theirs".into())];

        let Content::Schema(text) = render_catalog(&details, Some(own)) else {
            panic!("the catalog is schema text");
        };

        assert!(!text.contains("Mine"), "{text}");
        assert!(text.contains(&format!("- {other} Theirs")));
    }

    #[test]
    fn a_long_catalog_is_capped_with_a_note() {
        let mut details = details();
        details.lists = (0..MAX_LISTS + 3)
            .map(|index| (Uuid::now_v7(), format!("List {index}")))
            .collect();

        let Content::Schema(text) = render_catalog(&details, None) else {
            panic!("the catalog is schema text");
        };

        assert!(text.contains("3 lists were left out"), "{text}");
        assert!(!text.contains(&format!("List {}", MAX_LISTS + 2)));
    }

    #[test]
    fn the_saved_definition_is_masked_without_the_opt_in_and_fenced_with_it() {
        let (content, table) = render_saved(&definition(), &details(), Privacy::SchemaOnly);
        let Content::Schema(text) = content else {
            panic!("masked text is schema");
        };
        assert!(!text.contains("Aarhus") && text.contains("(kept:1)"));
        assert!(!table.is_empty());

        let (content, table) = render_saved(&definition(), &details(), Privacy::PersonalAllowed);
        assert!(content.is_personal());
        assert!(content.render().contains("Aarhus"));
        assert!(table.is_empty());
    }

    #[test]
    fn a_new_segment_has_no_saved_definition_to_show() {
        let (parts, table) = draft_context(&details(), None, None, Privacy::SchemaOnly);

        assert_eq!(parts.len(), 2);
        assert!(table.is_empty());
        assert_eq!(
            parts[1],
            Content::Schema("No definition is saved yet.\n".to_owned())
        );
    }

    #[test]
    fn the_reference_example_is_a_definition_that_parses() {
        let start = SEGMENT_REFERENCE.find("Example:\n").unwrap() + "Example:\n".len();
        let example: Value = serde_json::from_str(&SEGMENT_REFERENCE[start..]).unwrap();

        assert!(zirv_flow_core::segment::parse(&example).is_ok());
    }
}
