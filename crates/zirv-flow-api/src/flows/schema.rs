//! The OpenAPI schemas of the flow graph, the flow settings and the publish issues
//! (architecture sections 9.1 and 9.4).
//!
//! They are written here, not derived, so `core` stays free of utoipa. The tests compare them
//! with the vocabulary's own lists, so a node kind, trigger or issue code that is added without a
//! schema fails the build.

use std::collections::BTreeMap;

use utoipa::openapi::schema::{
    AdditionalProperties, ArrayBuilder, ObjectBuilder, OneOfBuilder, SchemaType, Type,
};
use utoipa::openapi::{KnownFormat, Ref, RefOr, Schema, SchemaFormat};
use zirv_flow_core::flow::{
    MAX_ARMS, MAX_CONDITIONS, MAX_DURATION_SECONDS, MAX_ID_CHARS, MAX_NODES, MAX_TRAIT_KEYS,
    MIN_ARMS, NODE_TYPES, TRIGGER_TYPES, code,
};

use super::validation;
use crate::segments::{SEGMENT_FILTER_SCHEMA, SEGMENT_NODE_SCHEMA};

pub(crate) const GRAPH: &str = "FlowGraph";
pub(crate) const NODE: &str = "FlowNode";
pub(crate) const EDGE: &str = "FlowEdge";
pub(crate) const SETTINGS: &str = "FlowSettings";
pub(crate) const ISSUE: &str = "FlowIssue";
const UNTIL_TIME: &str = "FlowUntilTime";
const TRAIT_CHANGE: &str = "FlowTraitChange";
const EVENT_MATCH_FIELDS: [&str; 2] = ["event", "filters"];
const TRIGGER: &str = "FlowTrigger";
const AUDIENCE: &str = "FlowAudience";
const REENTRY: &str = "FlowReentry";
const GOAL: &str = "FlowGoal";
const EXIT_CRITERION: &str = "FlowExitCriterion";

/// The component name of the schema for one node kind, such as `FlowWaitDurationNode`.
pub(crate) fn node_schema_name(node_type: &str) -> String {
    format!("Flow{}Node", pascal(node_type))
}

/// The component name of the schema for one trigger type, such as `FlowSegmentTrigger`.
pub(crate) fn trigger_schema_name(trigger_type: &str) -> String {
    format!("Flow{}Trigger", pascal(trigger_type))
}

/// `wait.until_event` as `WaitUntilEvent`.
fn pascal(text: &str) -> String {
    text.split(['.', '_'])
        .flat_map(|word| {
            let mut characters = word.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().chain(characters).collect::<String>())
        })
        .collect()
}

fn closed() -> Option<AdditionalProperties<Schema>> {
    Some(AdditionalProperties::FreeForm(false))
}

fn reference(name: &str) -> RefOr<Schema> {
    RefOr::Ref(Ref::from_schema_name(name))
}

fn string() -> ObjectBuilder {
    ObjectBuilder::new().schema_type(Type::String)
}

fn uuid() -> ObjectBuilder {
    string().format(Some(SchemaFormat::KnownFormat(KnownFormat::Uuid)))
}

fn integer() -> ObjectBuilder {
    ObjectBuilder::new().schema_type(Type::Integer)
}

fn seconds(description: &str) -> ObjectBuilder {
    integer()
        .minimum(Some(0))
        .maximum(Some(MAX_DURATION_SECONDS as f64))
        .description(Some(description))
}

fn boolean() -> ObjectBuilder {
    ObjectBuilder::new().schema_type(Type::Boolean)
}

fn string_enum<'a>(values: impl IntoIterator<Item = &'a str>) -> ObjectBuilder {
    string().enum_values(Some(values.into_iter().map(str::to_owned)))
}

/// A closed object whose `type` (or `tag`) is the fixed `value`.
fn tagged(tag: &str, value: &str, description: &str) -> ObjectBuilder {
    ObjectBuilder::new()
        .description(Some(description))
        .property(tag, string_enum([value]))
        .required(tag)
        .additional_properties(closed())
}

fn filters() -> ArrayBuilder {
    ArrayBuilder::new().items(reference(SEGMENT_FILTER_SCHEMA))
}

/// The `event` and `filters` fields of an event match, added to `object`.
fn with_event_match(object: ObjectBuilder) -> ObjectBuilder {
    let [name, filters_field] = EVENT_MATCH_FIELDS;
    object
        .property(
            name,
            string().description(Some(
                "The event name; it must have a schema in the workspace.",
            )),
        )
        .required(name)
        .property(
            filters_field,
            filters().description(Some(
                "Property filters, as the `where` of a segment event condition.",
            )),
        )
}

fn node_builders() -> Vec<(&'static str, ObjectBuilder)> {
    let node = |node_type: &'static str, description: &str| {
        tagged("type", node_type, description)
            .property(
                "id",
                string()
                    .min_length(Some(1))
                    .max_length(Some(MAX_ID_CHARS))
                    .description(Some("A stable id, unique in the graph.")),
            )
            .required("id")
    };
    vec![
        (
            "wait.duration",
            node("wait.duration", "Waits a fixed time. Port `next`.")
                .property("seconds", seconds("How long to wait."))
                .required("seconds"),
        ),
        (
            "wait.until_time",
            node(
                "wait.until_time",
                "Waits for a local time of day or date. Port `next`.",
            )
            .property("when", reference(UNTIL_TIME))
            .required("when"),
        ),
        (
            "wait.until_event",
            with_event_match(node(
                "wait.until_event",
                "Waits for an event, or the timeout. Ports `event` and `timeout`.",
            ))
            .property(
                "timeout_seconds",
                seconds("How long to wait for the event before taking `timeout`."),
            )
            .required("timeout_seconds"),
        ),
        (
            "branch",
            node(
                "branch",
                "The first matching condition wins. Ports `case_0`, `case_1`, ... and `else`.",
            )
            .property(
                "conditions",
                ArrayBuilder::new()
                    .items(reference(SEGMENT_NODE_SCHEMA))
                    .min_items(Some(1))
                    .max_items(Some(MAX_CONDITIONS))
                    .description(Some(
                        "Segment condition nodes without `v`, evaluated in order for this person.",
                    )),
            )
            .required("conditions"),
        ),
        (
            "split.random",
            node(
                "split.random",
                "Sends a person down one arm by weight. Ports `arm_0`, `arm_1`, ...",
            )
            .property(
                "arms",
                ArrayBuilder::new()
                    .items(
                        ObjectBuilder::new()
                            .property("weight", integer().minimum(Some(1)).maximum(Some(100)))
                            .required("weight")
                            .additional_properties(closed()),
                    )
                    .min_items(Some(MIN_ARMS))
                    .max_items(Some(MAX_ARMS))
                    .description(Some("The weights of all arms sum to 100.")),
            )
            .required("arms"),
        ),
        (
            "action.webhook",
            node(
                "action.webhook",
                "Calls a URL with a signed JSON body: the fixed envelope plus the listed trait keys, or, \
                 with a `template`, the headers and body that published webhook template renders. \
                 Port `next`.",
            )
            .property(
                "url",
                string().description(Some(
                    "An http or https URL. A literal address the egress policy refuses is an issue.",
                )),
            )
            .required("url")
            .property(
                "trait_keys",
                ArrayBuilder::new()
                    .items(string())
                    .max_items(Some(MAX_TRAIT_KEYS)),
            )
            .property("template", uuid()),
        ),
        (
            "action.update_trait",
            node(
                "action.update_trait",
                "Changes one trait as the flow's own contribution. Port `next`.",
            )
            .property("key", string())
            .required("key")
            .property("change", reference(TRAIT_CHANGE))
            .required("change"),
        ),
        (
            "action.list",
            node("action.list", "Adds a person to a list or removes them. Port `next`.")
                .property("list_id", uuid())
                .required("list_id")
                .property("change", string_enum(["add", "remove"]))
                .required("change"),
        ),
        (
            "goal",
            node(
                "goal",
                "Records a conversion. With `exit` it also ends the enrollment and has no port; \
                 otherwise port `next`.",
            )
            .property("exit", boolean()),
        ),
        ("exit", node("exit", "Ends the enrollment. No port.")),
    ]
}

fn trigger_builders() -> Vec<(&'static str, ObjectBuilder)> {
    let trigger =
        |trigger_type: &'static str, description: &str| tagged("type", trigger_type, description);
    vec![
        (
            "segment",
            trigger("segment", "A person enters or leaves a segment.")
                .property("segment_id", uuid())
                .required("segment_id")
                .property("on", string_enum(["entered", "exited"]))
                .required("on")
                .property(
                    "enroll_current",
                    boolean().description(Some("Also enroll the current members at publish.")),
                ),
        ),
        (
            "event",
            with_event_match(trigger("event", "A named event with property filters.")),
        ),
        (
            "schedule",
            trigger("schedule", "A cron slot in its own time zone.")
                .property("cron", string())
                .required("cron")
                .property(
                    "timezone",
                    string()
                        .description(Some("An IANA time zone name such as `Europe/Copenhagen`.")),
                )
                .required("timezone")
                .property("audience", reference(AUDIENCE))
                .required("audience"),
        ),
        (
            "manual",
            trigger("manual", "An operator enrolls people by hand."),
        ),
        (
            "api",
            trigger("api", "`POST .../flows/{id}/enrollments` enrolls people."),
        ),
    ]
}

/// The builder named `name`. The lists the schemas follow are the vocabulary's own, so a name
/// without a builder is a vocabulary change that reached this module without a schema.
fn builder_of(
    builders: &mut Vec<(&'static str, ObjectBuilder)>,
    name: &str,
    what: &str,
) -> ObjectBuilder {
    let position = builders
        .iter()
        .position(|(held, _)| *held == name)
        .unwrap_or_else(|| panic!("the {what} `{name}` has no OpenAPI schema"));
    builders.remove(position).1
}

fn issue_codes() -> Vec<&'static str> {
    code::ALL.into_iter().chain(validation::code::ALL).collect()
}

/// Every component schema of the flow document, by name.
pub(crate) fn schemas() -> BTreeMap<String, Schema> {
    let mut schemas: BTreeMap<String, Schema> = BTreeMap::new();

    let mut nodes = node_builders();
    let mut node = OneOfBuilder::new().description(Some(
        "One node of a flow graph: an object whose `type` selects its fields. Unknown keys are \
         refused.",
    ));
    for node_type in NODE_TYPES {
        let name = node_schema_name(node_type);
        schemas.insert(
            name.clone(),
            builder_of(&mut nodes, node_type, "node type").into(),
        );
        node = node.item(reference(&name));
    }
    assert!(
        nodes.is_empty(),
        "a node schema is for a type that does not exist"
    );
    schemas.insert(NODE.to_owned(), node.into());

    schemas.insert(
        EDGE.to_owned(),
        ObjectBuilder::new()
            .description(Some(
                "The output port `port` of node `from` leads to node `to`. Every port of every \
                 node is wired exactly once.",
            ))
            .property(
                "id",
                string().min_length(Some(1)).max_length(Some(MAX_ID_CHARS)),
            )
            .required("id")
            .property("from", string())
            .required("from")
            .property(
                "port",
                string().description(Some(
                    "`next`, `event`, `timeout`, `else`, `case_<i>` or `arm_<i>`, as the node's \
                     type defines.",
                )),
            )
            .required("port")
            .property("to", string())
            .required("to")
            .additional_properties(closed())
            .into(),
    );
    schemas.insert(
        GRAPH.to_owned(),
        ObjectBuilder::new()
            .description(Some(
                "A flow graph, version 1: a directed acyclic graph with one entry node.",
            ))
            .property("v", integer().enum_values(Some([1])))
            .required("v")
            .property(
                "nodes",
                ArrayBuilder::new()
                    .items(reference(NODE))
                    .max_items(Some(MAX_NODES)),
            )
            .required("nodes")
            .property("edges", ArrayBuilder::new().items(reference(EDGE)))
            .required("edges")
            .additional_properties(closed())
            .into(),
    );

    schemas.insert(
        UNTIL_TIME.to_owned(),
        OneOfBuilder::new()
            .description(Some(
                "A local time, in the person's time zone and else the workspace's.",
            ))
            .item(
                tagged(
                    "kind",
                    "time_of_day",
                    "The next `HH:MM`, on any of `weekdays`.",
                )
                .property("time", string().description(Some("`HH:MM`, 24 hours.")))
                .required("time")
                .property(
                    "weekdays",
                    ArrayBuilder::new().items(string_enum([
                        "mon", "tue", "wed", "thu", "fri", "sat", "sun",
                    ])),
                ),
            )
            .item(
                tagged("kind", "date_time", "A local date and time with no offset.")
                    .property(
                        "at",
                        string().description(Some("`YYYY-MM-DDTHH:MM:SS`, with no offset.")),
                    )
                    .required("at"),
            )
            .into(),
    );
    schemas.insert(
        TRAIT_CHANGE.to_owned(),
        OneOfBuilder::new()
            .item(
                tagged("op", "set", "Sets the trait.")
                    .property(
                        "value",
                        ObjectBuilder::new()
                            .schema_type(SchemaType::AnyValue)
                            .description(Some("The value; its type follows the trait's type.")),
                    )
                    .required("value"),
            )
            .item(
                tagged("op", "increment", "Adds to a number trait.")
                    .property("by", ObjectBuilder::new().schema_type(Type::Number))
                    .required("by"),
            )
            .item(tagged("op", "unset", "Removes the flow's contribution."))
            .into(),
    );

    let mut triggers = trigger_builders();
    let mut trigger = OneOfBuilder::new().description(Some("How a person comes into the flow."));
    for trigger_type in TRIGGER_TYPES {
        let name = trigger_schema_name(trigger_type);
        schemas.insert(
            name.clone(),
            builder_of(&mut triggers, trigger_type, "trigger type").into(),
        );
        trigger = trigger.item(reference(&name));
    }
    assert!(
        triggers.is_empty(),
        "a trigger schema is for a type that does not exist"
    );
    schemas.insert(TRIGGER.to_owned(), trigger.into());
    schemas.insert(
        AUDIENCE.to_owned(),
        OneOfBuilder::new()
            .description(Some("Who a schedule trigger enrolls."))
            .item(tagged("type", "everyone", "Every person."))
            .item(
                tagged("type", "segment", "The members of a segment.")
                    .property("segment_id", uuid())
                    .required("segment_id"),
            )
            .into(),
    );
    schemas.insert(
        REENTRY.to_owned(),
        OneOfBuilder::new()
            .description(Some("Whether a person may enter again."))
            .item(tagged("mode", "once", "Never again after the first entry."))
            .item(tagged(
                "mode",
                "once_at_a_time",
                "Again once the active enrollment has ended.",
            ))
            .item(
                tagged(
                    "mode",
                    "always",
                    "Again, no sooner than `min_interval_seconds` after the last entry.",
                )
                .property("min_interval_seconds", seconds("0 for no wait.")),
            )
            .into(),
    );
    schemas.insert(
        GOAL.to_owned(),
        OneOfBuilder::new()
            .description(Some("A conversion."))
            .item(with_event_match(tagged(
                "type",
                "event",
                "An event with the filters of an event trigger.",
            )))
            .item(
                tagged("type", "segment", "Entry into a segment.")
                    .property("segment_id", uuid())
                    .required("segment_id"),
            )
            .into(),
    );
    schemas.insert(
        EXIT_CRITERION.to_owned(),
        OneOfBuilder::new()
            .description(Some("A reason to end an active enrollment."))
            .item(tagged(
                "type",
                "leaves_segment",
                "The person leaves the trigger segment (a segment trigger only).",
            ))
            .item(with_event_match(tagged(
                "type",
                "event",
                "The event occurs.",
            )))
            .item(tagged(
                "type",
                "goal_reached",
                "The flow's goal is reached (needs a goal).",
            ))
            .into(),
    );
    schemas.insert(
        SETTINGS.to_owned(),
        ObjectBuilder::new()
            .description(Some(
                "The settings beside the graph: the trigger, re-entry, the optional goal and the \
                 exit criteria.",
            ))
            .property("trigger", reference(TRIGGER))
            .required("trigger")
            .property("reentry", reference(REENTRY))
            .required("reentry")
            .property("goal", reference(GOAL))
            .property(
                "exit_on_goal",
                boolean().description(Some("End the enrollment when the goal is reached.")),
            )
            .property(
                "exit_criteria",
                ArrayBuilder::new().items(reference(EXIT_CRITERION)),
            )
            .additional_properties(closed())
            .into(),
    );
    schemas.insert(
        ISSUE.to_owned(),
        ObjectBuilder::new()
            .description(Some(
                "One problem with a flow. It names a fixed code and where it is, never a value \
                 from the flow.",
            ))
            .property("code", string_enum(issue_codes()))
            .required("code")
            .property(
                "node",
                string().description(Some("The id of the node it concerns, when it does.")),
            )
            .property(
                "edge",
                string().description(Some("The id of the edge it concerns, when it does.")),
            )
            .property(
                "at",
                string().description(Some(
                    "A JSON pointer into the node, the edge or the settings object. Inside a \
                     condition or a filter it continues with the segment pointer.",
                )),
            )
            .into(),
    );
    schemas
}

/// Adds the flow schemas to the document's components.
pub(crate) struct FlowSchemas;

impl utoipa::Modify for FlowSchemas {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        for (name, schema) in schemas() {
            components.schemas.insert(name, RefOr::T(schema));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn document() -> Value {
        serde_json::to_value(crate::openapi::openapi()).unwrap()
    }

    fn component<'a>(document: &'a Value, name: &str) -> &'a Value {
        document["components"]["schemas"]
            .get(name)
            .unwrap_or_else(|| panic!("schema {name} is not in the document"))
    }

    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_document_publishes_the_flow_schemas() {
        let document = document();
        for name in [GRAPH, NODE, EDGE, SETTINGS, ISSUE, TRIGGER, REENTRY, GOAL] {
            component(&document, name);
        }
    }

    #[test]
    fn every_node_type_has_a_closed_schema_that_the_node_schema_refers_to() {
        let document = document();
        let refs: Vec<&str> = component(&document, NODE)["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["$ref"].as_str().unwrap())
            .collect();
        assert_eq!(refs.len(), NODE_TYPES.len());
        for node_type in NODE_TYPES {
            let name = node_schema_name(node_type);
            assert!(
                refs.contains(&format!("#/components/schemas/{name}").as_str()),
                "{name} is not one of the node kinds"
            );
            let schema = component(&document, &name);
            assert_eq!(
                strings(&schema["properties"]["type"]["enum"]),
                [node_type],
                "{name}"
            );
            let required = strings(&schema["required"]);
            assert!(required.contains(&"id".to_owned()) && required.contains(&"type".to_owned()));
            assert_eq!(schema["additionalProperties"], false, "{name}");
        }
    }

    #[test]
    fn every_trigger_type_has_a_closed_schema_that_the_trigger_schema_refers_to() {
        let document = document();
        let refs: Vec<&str> = component(&document, TRIGGER)["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["$ref"].as_str().unwrap())
            .collect();
        assert_eq!(refs.len(), TRIGGER_TYPES.len());
        for trigger_type in TRIGGER_TYPES {
            let name = trigger_schema_name(trigger_type);
            assert!(
                refs.contains(&format!("#/components/schemas/{name}").as_str()),
                "{name} is not one of the triggers"
            );
            let schema = component(&document, &name);
            assert_eq!(
                strings(&schema["properties"]["type"]["enum"]),
                [trigger_type],
                "{name}"
            );
            assert_eq!(schema["additionalProperties"], false, "{name}");
        }
    }

    #[test]
    fn the_issue_codes_are_the_structural_ones_and_the_added_ones() {
        let document = document();
        let published = strings(&component(&document, ISSUE)["properties"]["code"]["enum"]);
        let mut expected: Vec<String> = code::ALL.iter().map(|text| (*text).to_owned()).collect();
        expected.extend(validation::code::ALL.iter().map(|text| (*text).to_owned()));
        assert_eq!(published, expected);
        let unique: std::collections::BTreeSet<&String> = published.iter().collect();
        assert_eq!(unique.len(), published.len(), "a code is listed twice");
    }

    #[test]
    fn the_branch_refers_to_the_segment_node_schema_and_the_filters_to_the_segment_filter() {
        let document = document();
        let branch = component(&document, &node_schema_name("branch"));
        assert_eq!(
            branch["properties"]["conditions"]["items"]["$ref"],
            format!("#/components/schemas/{SEGMENT_NODE_SCHEMA}")
        );
        let event = component(&document, &trigger_schema_name("event"));
        assert_eq!(
            event["properties"]["filters"]["items"]["$ref"],
            format!("#/components/schemas/{SEGMENT_FILTER_SCHEMA}")
        );
        component(&document, SEGMENT_NODE_SCHEMA);
        component(&document, SEGMENT_FILTER_SCHEMA);
    }

    #[test]
    fn the_limits_in_the_schema_are_the_validators() {
        let document = document();
        let graph = component(&document, GRAPH);
        assert_eq!(graph["properties"]["nodes"]["maxItems"], MAX_NODES);
        let split = component(&document, &node_schema_name("split.random"));
        assert_eq!(split["properties"]["arms"]["minItems"], MIN_ARMS);
        assert_eq!(split["properties"]["arms"]["maxItems"], MAX_ARMS);
        let branch = component(&document, &node_schema_name("branch"));
        assert_eq!(
            branch["properties"]["conditions"]["maxItems"],
            MAX_CONDITIONS
        );
        let wait = component(&document, &node_schema_name("wait.duration"));
        assert_eq!(
            wait["properties"]["seconds"]["maximum"],
            MAX_DURATION_SECONDS
        );
    }

    #[test]
    fn a_name_is_made_of_the_words_of_the_type() {
        assert_eq!(
            node_schema_name("wait.until_event"),
            "FlowWaitUntilEventNode"
        );
        assert_eq!(node_schema_name("exit"), "FlowExitNode");
        assert_eq!(trigger_schema_name("api"), "FlowApiTrigger");
    }

    #[test]
    fn every_example_node_and_setting_of_the_core_vocabulary_is_accepted_by_its_schema() {
        // The schemas are closed; a sample of each shape the parser reads must use only the
        // properties they list.
        let document = document();
        let samples = [
            (
                "wait.duration",
                serde_json::json!({"id": "a", "type": "wait.duration", "seconds": 5}),
            ),
            (
                "wait.until_event",
                serde_json::json!({"id": "a", "type": "wait.until_event", "event": "e",
                    "filters": [], "timeout_seconds": 5}),
            ),
            (
                "goal",
                serde_json::json!({"id": "a", "type": "goal", "exit": true}),
            ),
            ("exit", serde_json::json!({"id": "a", "type": "exit"})),
        ];
        for (node_type, sample) in samples {
            let schema = component(&document, &node_schema_name(node_type));
            let properties = schema["properties"].as_object().unwrap();
            for key in sample.as_object().unwrap().keys() {
                assert!(properties.contains_key(key), "{node_type} lacks {key}");
            }
            // The core parser reads the same sample.
            zirv_flow_core::flow::FlowNode::try_from(sample).unwrap();
        }
    }
}
