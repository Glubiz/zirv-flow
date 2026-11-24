//! The personal merge fields of a webhook template, for the rule that a caller without access to
//! personal values cannot send them out (#593).
//!
//! A merge field is found by validating the document twice: against the workspace's field catalog,
//! and against the same catalog with the personal traits, the personal event properties and the
//! reserved `person.email` removed. A path the second pass calls `unknown_merge_field` and the
//! first accepts is a personal field. The template crate reports an absent field only under that
//! one code (`validate::lookup` is the single place that decides a path is unknown, and
//! `check_template_in` is the single place that turns it into an issue), so no second parser is
//! needed. Only the `webhook` channel is judged: an email goes to the person it describes.

use std::collections::BTreeSet;

use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{ClearFields, PgConnection, TemplateFieldCatalog};
use zirv_flow_templates::{Channel, FieldCatalog, TemplateDocument, code, migrate_to_current};

use super::{field_catalog, stored_document};
use crate::problem::ApiError;
use crate::segments::TemplateSend;

/// The merge fields of `doc` that read personal data, sorted. Empty for a template that is not a
/// webhook template.
pub(crate) fn personal_fields(
    doc: &TemplateDocument,
    read: &TemplateFieldCatalog,
    clear: &ClearFields,
) -> Vec<String> {
    if doc.channel != Channel::Webhook {
        return Vec::new();
    }
    let full = field_catalog(read.clone());
    let mut clear_only = full.clone();
    clear_only.person.retain(|key, _| !clear.is_trait_pii(key));
    clear_only.event.retain(|property, _| {
        // A property no event schema declares counts as personal, as an unknown key does in the
        // segments guard.
        read.event_property_events
            .get(property)
            .is_some_and(|events| {
                events
                    .iter()
                    .all(|event| !clear.is_property_pii(event, property))
            })
    });
    let known = unknown_fields(doc, &full);
    unknown_fields(doc, &clear_only)
        .into_iter()
        .filter(|field| !known.contains(field))
        .collect()
}

/// The merge fields `doc` names that `catalog` does not have.
fn unknown_fields(doc: &TemplateDocument, catalog: &FieldCatalog) -> BTreeSet<String> {
    zirv_flow_templates::validate(doc, catalog)
        .into_iter()
        .filter(|issue| issue.code == code::UNKNOWN_MERGE_FIELD)
        .filter_map(|issue| issue.field)
        .collect()
}

/// Refuses `doc` when it is a webhook template that reads a personal field: 403
/// `pii_not_sendable`, naming `node` when the template is judged as part of a flow.
pub(crate) async fn refuse_personal_document(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    node: Option<&str>,
    doc: &TemplateDocument,
    clear: &ClearFields,
) -> Result<(), ApiError> {
    if doc.channel != Channel::Webhook {
        return Ok(());
    }
    let read = zirv_flow_store::read_template_field_catalog(connection, workspace).await?;
    match personal_fields(doc, &read, clear).first() {
        Some(field) => Err(ApiError::pii_not_sendable(node, field)),
        None => Ok(()),
    }
}

/// Refuses a flow whose webhook nodes name a published webhook template that reads a personal
/// field. The templates are read once for all nodes; one that is gone, archived, unpublished or
/// not a webhook template is already an issue of the check and is not judged here.
pub(crate) async fn refuse_personal_templates(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    sends: &[TemplateSend],
    clear: &ClearFields,
) -> Result<(), ApiError> {
    if sends.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = sends
        .iter()
        .map(|send| send.template)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let templates = zirv_flow_store::find_templates(&mut *connection, workspace, &ids).await?;
    let read = zirv_flow_store::read_template_field_catalog(connection, workspace).await?;
    for send in sends {
        let Some(template) = templates
            .iter()
            .find(|template| template.id == send.template)
            .filter(|template| template.channel == "webhook" && template.archived_at.is_none())
        else {
            continue;
        };
        let Some(number) = template.current_version else {
            continue;
        };
        let Some(version) = zirv_flow_store::find_template_version(
            &mut *connection,
            workspace,
            template.id,
            number,
        )
        .await?
        else {
            continue;
        };
        // A document that no longer parses cannot render either, so it sends nothing.
        let Ok(doc) = migrate_to_current(version.document)
            .map_err(|_| ())
            .and_then(|value| stored_document(&value).map_err(|_| ()))
        else {
            continue;
        };
        if let Some(field) = personal_fields(&doc, &read, clear).first() {
            return Err(ApiError::pii_not_sendable(Some(&send.node), field));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::events::PropertyType;
    use zirv_flow_core::people::TraitType;

    use super::*;

    fn read() -> TemplateFieldCatalog {
        let mut read = TemplateFieldCatalog::default();
        for (key, trait_type) in [("ssn", TraitType::String), ("plan", TraitType::String)] {
            read.traits.insert(key.to_owned(), trait_type);
        }
        for (property, property_type, event) in [
            ("total", PropertyType::Number, "order"),
            ("sku", PropertyType::String, "order"),
            // Clear on `order`, personal on `view`.
            ("shared", PropertyType::String, "order"),
            ("shared", PropertyType::String, "view"),
        ] {
            read.event_properties
                .insert(property.to_owned(), property_type);
            read.event_property_events
                .entry(property.to_owned())
                .or_default()
                .insert(event.to_owned());
        }
        read
    }

    fn clear() -> ClearFields {
        ClearFields::new(["plan"], [("order", "sku"), ("order", "shared")])
    }

    fn webhook(headers: serde_json::Value, body: &str) -> TemplateDocument {
        stored_document(&json!({"schema": 2, "channel": "webhook", "mode": "code",
            "webhook": {"headers": headers, "body": body}}))
        .expect("a webhook document")
    }

    #[test]
    fn a_personal_trait_event_property_and_the_reserved_email_are_found_in_headers_and_body() {
        let doc = webhook(
            json!({"X-Who": "{{ person.ssn }}"}),
            r#"{"a": "{{ event.total }}", "b": ["{{ person.email }}"], "c": "{{ person.plan }}",
               "d": "{{ event.sku }}"}"#,
        );

        assert_eq!(
            personal_fields(&doc, &read(), &clear()),
            ["event.total", "person.email", "person.ssn"]
        );
    }

    #[test]
    fn a_template_of_clear_fields_has_none() {
        let doc = webhook(
            json!({"X-Plan": "{{ person.plan }}"}),
            r#"{"s": "{{ event.sku }}", "run": "{{ system.run_id }}"}"#,
        );

        assert!(personal_fields(&doc, &read(), &clear()).is_empty());
    }

    #[test]
    fn a_field_the_workspace_does_not_have_is_not_a_personal_field() {
        let doc = webhook(json!({}), r#"{"x": "{{ person.nope }}"}"#);

        assert!(personal_fields(&doc, &read(), &clear()).is_empty());
    }

    #[test]
    fn a_property_is_personal_when_any_event_that_declares_it_flags_it() {
        let doc = webhook(json!({}), r#"{"x": "{{ event.shared }}"}"#);
        // `shared` is clear on `order`, but `view` does not list it as clear.
        assert_eq!(personal_fields(&doc, &read(), &clear()), ["event.shared"]);
        let clear_everywhere = ClearFields::new([], [("order", "shared"), ("view", "shared")]);
        assert!(personal_fields(&doc, &read(), &clear_everywhere).is_empty());
    }

    #[test]
    fn an_admin_declaring_email_not_personal_unblocks_it() {
        let doc = webhook(json!({}), r#"{"x": "{{ person.email }}"}"#);
        let declared = ClearFields::new(["email"], []);

        assert!(personal_fields(&doc, &read(), &declared).is_empty());
    }

    #[test]
    fn a_path_below_a_personal_object_property_is_personal() {
        let mut read = read();
        read.event_properties
            .insert("address".to_owned(), PropertyType::Object);
        read.event_property_events
            .entry("address".to_owned())
            .or_default()
            .insert("order".to_owned());
        let doc = webhook(json!({}), r#"{"x": "{{ event.address.city }}"}"#);

        assert_eq!(
            personal_fields(&doc, &read, &clear()),
            ["event.address.city"]
        );
    }

    #[test]
    fn an_email_template_is_never_judged() {
        let doc = stored_document(&json!({"schema": 2, "channel": "email", "mode": "code",
            "email": {"subject": "{{ person.ssn }}", "html": "<p>{{ person.ssn }}</p>"}}))
        .expect("an email document");

        assert!(personal_fields(&doc, &read(), &clear()).is_empty());
    }
}
