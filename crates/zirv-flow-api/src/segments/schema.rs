//! The OpenAPI schemas of the segment definition AST (architecture section 8).
//!
//! They are written here, not derived, so `core` and `segments` stay free of utoipa. The tests
//! compare them with the AST's own lists, so a node kind, operator, aggregate or issue code that
//! is added without a schema fails the build.

use std::collections::BTreeMap;

use utoipa::openapi::schema::{
    AdditionalProperties, ArrayBuilder, ObjectBuilder, OneOfBuilder, SchemaType, Type,
};
use utoipa::openapi::{Ref, RefOr, Schema};
use zirv_flow_core::segment::{AggFn, NODE_KEYS, Op, code};

pub(crate) const DEFINITION: &str = "SegmentDefinition";
pub(crate) const NODE: &str = "SegmentNode";
pub(crate) const OP: &str = "SegmentOp";
pub(crate) const AGG_FN: &str = "SegmentAggFn";
pub(crate) const ISSUE: &str = "SegmentIssue";
pub(crate) const PROPERTY_FILTER: &str = "SegmentPropertyFilter";
const AGGREGATE: &str = "SegmentAggregate";
const WINDOW: &str = "SegmentWindow";

/// The component name of the schema for one node kind, such as `SegmentTraitNode`.
pub(crate) fn node_schema_name(key: &str) -> String {
    let mut name = String::from("Segment");
    let mut characters = key.chars();
    if let Some(first) = characters.next() {
        name.extend(first.to_uppercase());
        name.push_str(characters.as_str());
    }
    name.push_str("Node");
    name
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

fn string_enum<'a>(values: impl IntoIterator<Item = &'a str>) -> Schema {
    string()
        .enum_values(Some(values.into_iter().map(str::to_owned)))
        .into()
}

fn any_value(description: &str) -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(SchemaType::AnyValue)
        .description(Some(description))
}

fn array_of(items: RefOr<Schema>) -> ArrayBuilder {
    ArrayBuilder::new().items(items)
}

fn node_object(key: &str, shape: impl Into<RefOr<Schema>>, description: &str) -> ObjectBuilder {
    ObjectBuilder::new()
        .description(Some(description))
        .property(key, shape)
        .required(key)
        .additional_properties(closed())
}

fn node_builders() -> Vec<(&'static str, ObjectBuilder)> {
    let mut builders = Vec::new();
    let mut put = |key: &'static str, schema: ObjectBuilder| builders.push((key, schema));

    put(
        "all",
        node_object(
            "all",
            array_of(reference(NODE)).build(),
            "True when every child is true; an empty list is true.",
        ),
    );
    put(
        "any",
        node_object(
            "any",
            array_of(reference(NODE)).build(),
            "True when some child is true; an empty list is false.",
        ),
    );
    put(
        "not",
        node_object(
            "not",
            reference(NODE),
            "Boolean negation of the child. A leaf's operator is never rewritten under it.",
        ),
    );
    put(
        "trait",
        node_object(
            "trait",
            string().build(),
            "A condition on a person's trait. Beside `trait` go `op` and, for most operators, `value`.",
        )
        .property("op", reference(OP))
        .required("op")
        .property("value", any_value("The operand; its type follows the trait's type.")),
    );
    put(
        "event",
        node_object(
            "event",
            string().build(),
            "The person has events of this name that match every `where` filter in the window, \
             and satisfy `agg` when it is given (otherwise at least one).",
        )
        .property("where", array_of(reference(PROPERTY_FILTER)))
        .property("agg", reference(AGGREGATE))
        .property("window", reference(WINDOW)),
    );
    put(
        "list",
        node_object(
            "list",
            string()
                .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                    utoipa::openapi::KnownFormat::Uuid,
                )))
                .build(),
            "The person is a member of the list.",
        ),
    );
    put(
        "segment",
        node_object(
            "segment",
            string()
                .format(Some(utoipa::openapi::SchemaFormat::KnownFormat(
                    utoipa::openapi::KnownFormat::Uuid,
                )))
                .build(),
            "The person is in the other segment. Cycles are rejected when the segment is saved.",
        ),
    );
    put(
        "consent",
        node_object(
            "consent",
            ObjectBuilder::new()
                .property("channel", string().build())
                .required("channel")
                .property("topic", string().build())
                .property(
                    "status",
                    string_enum([
                        "subscribed",
                        "unsubscribed",
                        "pending_confirmation",
                        "unknown",
                    ]),
                )
                .required("status")
                .additional_properties(closed())
                .build(),
            "The person's consent status for a channel, or one of its topics.",
        ),
    );
    put(
        "identifier",
        node_object(
            "identifier",
            string().build(),
            "The person has an identifier of this kind: `email`, `phone`, `anonymous_id` or \
             `external:<namespace>`.",
        ),
    );
    builders
}

/// Every component schema of the AST, by name.
pub(crate) fn schemas() -> BTreeMap<String, Schema> {
    let mut schemas: BTreeMap<String, Schema> = node_builders()
        .into_iter()
        .map(|(key, builder)| (node_schema_name(key), Schema::from(builder)))
        .collect();

    let mut node = OneOfBuilder::new().description(Some(
        "One node of a segment condition tree: an object with exactly one node key.",
    ));
    for key in NODE_KEYS {
        node = node.item(reference(&node_schema_name(key)));
    }
    schemas.insert(NODE.to_owned(), node.into());

    let mut definition = OneOfBuilder::new().description(Some(
        "A segment definition, version 1: one node with `v` added beside its key. Unknown keys are refused.",
    ));
    for (_, builder) in node_builders() {
        definition = definition.item(
            builder
                .property(
                    "v",
                    ObjectBuilder::new()
                        .schema_type(Type::Integer)
                        .enum_values(Some([1])),
                )
                .required("v"),
        );
    }
    schemas.insert(DEFINITION.to_owned(), definition.into());
    schemas.insert(OP.to_owned(), string_enum(Op::ALL.map(Op::as_str)));
    schemas.insert(
        AGG_FN.to_owned(),
        string_enum(AggFn::ALL.map(AggFn::as_str)),
    );
    schemas.insert(
        PROPERTY_FILTER.to_owned(),
        ObjectBuilder::new()
            .property("prop", string().build())
            .required("prop")
            .property("op", reference(OP))
            .required("op")
            .property(
                "value",
                any_value("The operand; its type follows the property's type."),
            )
            .additional_properties(closed())
            .into(),
    );
    schemas.insert(
        AGGREGATE.to_owned(),
        ObjectBuilder::new()
            .property("fn", reference(AGG_FN))
            .required("fn")
            .property("prop", string().build())
            .property("op", reference(OP))
            .required("op")
            .property(
                "value",
                any_value("A number, or a timestamp for `first_time` and `last_time`."),
            )
            .required("value")
            .additional_properties(closed())
            .into(),
    );
    schemas.insert(
        WINDOW.to_owned(),
        OneOfBuilder::new()
            .description(Some(
                "Which events count; with no window, all retained events.",
            ))
            .item(
                ObjectBuilder::new()
                    .property(
                        "last",
                        string()
                            .description(Some("ISO 8601 duration in days or hours, such as P90D.")),
                    )
                    .required("last")
                    .additional_properties(closed()),
            )
            .item(
                ObjectBuilder::new()
                    .property(
                        "between",
                        ArrayBuilder::new()
                            .items(string())
                            .min_items(Some(2))
                            .max_items(Some(2)),
                    )
                    .required("between")
                    .additional_properties(closed()),
            )
            .item(
                ObjectBuilder::new()
                    .property("since_trait", string())
                    .required("since_trait")
                    .additional_properties(closed()),
            )
            .into(),
    );
    schemas.insert(
        ISSUE.to_owned(),
        ObjectBuilder::new()
            .description(Some(
                "One problem with a definition. It carries a position and a fixed code, never a value from the definition.",
            ))
            .property(
                "pointer",
                string().description(Some("A JSON pointer into the definition.")),
            )
            .required("pointer")
            .property("code", string_enum(code::ALL))
            .required("code")
            .into(),
    );
    schemas
}

/// Adds the AST schemas to the document's components.
pub(crate) struct SegmentSchemas;

impl utoipa::Modify for SegmentSchemas {
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

    #[test]
    fn the_document_publishes_the_ast_schemas() {
        let document = document();
        for name in [
            DEFINITION,
            NODE,
            OP,
            AGG_FN,
            ISSUE,
            PROPERTY_FILTER,
            AGGREGATE,
            WINDOW,
        ] {
            component(&document, name);
        }
    }

    #[test]
    fn every_node_kind_has_a_schema_that_the_node_schema_refers_to() {
        let document = document();
        let refs: Vec<&str> = component(&document, NODE)["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["$ref"].as_str().unwrap())
            .collect();
        assert_eq!(refs.len(), NODE_KEYS.len());
        for key in NODE_KEYS {
            let name = node_schema_name(key);
            assert!(
                refs.contains(&format!("#/components/schemas/{name}").as_str()),
                "{name} is not one of the node kinds"
            );
            let schema = component(&document, &name);
            assert!(
                schema["properties"].get(key).is_some(),
                "{name} lacks {key}"
            );
            assert_eq!(schema["required"][0], key);
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    #[test]
    fn the_enums_match_the_ast() {
        let document = document();
        let names = |name: &str| -> Vec<String> {
            component(&document, name)["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names(OP), Op::ALL.map(|op| op.as_str().to_owned()));
        assert_eq!(
            names(AGG_FN),
            AggFn::ALL.map(|func| func.as_str().to_owned())
        );
        let issue = &component(&document, ISSUE)["properties"]["code"];
        assert_eq!(
            issue["enum"].as_array().unwrap().len(),
            code::ALL.len(),
            "an issue code is missing from the schema"
        );
    }

    #[test]
    fn each_root_variant_is_closed_and_lists_the_version_and_its_node_key() {
        let document = document();
        let variants = component(&document, DEFINITION)["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(variants.len(), NODE_KEYS.len());
        for (variant, key) in variants.iter().zip(NODE_KEYS) {
            assert_eq!(variant["additionalProperties"], false, "{key}");
            assert!(variant["properties"].get("v").is_some(), "{key} lacks v");
            assert!(
                variant["properties"].get(key).is_some(),
                "{key} lacks itself"
            );
            let required: Vec<&str> = variant["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap())
                .collect();
            assert!(required.contains(&"v") && required.contains(&key), "{key}");
        }
        let nested = component(&document, &node_schema_name("all"));
        assert!(nested["properties"].get("v").is_none());
    }

    #[test]
    fn operands_carry_no_type() {
        let document = document();
        let operands = [
            &component(&document, &node_schema_name("trait"))["properties"]["value"],
            &component(&document, PROPERTY_FILTER)["properties"]["value"],
            &component(&document, AGGREGATE)["properties"]["value"],
        ];
        for operand in operands {
            assert!(operand.get("type").is_none(), "{operand}");
            assert!(operand.get("description").is_some());
        }
    }

    #[test]
    fn a_node_schema_name_capitalizes_the_key() {
        assert_eq!(node_schema_name("trait"), "SegmentTraitNode");
    }
}
