//! Gate 7 of the send pipeline: the template renders for the person in the render host pool,
//! never in this process (architecture 10.1 and 11.9).
//!
//! Every registered email content type (`html`, `mjml`, `blocks`) goes through the pool's one
//! `render_email` call, which dispatches on the document. Any failure of a render, the renderer's
//! own refusal or the host's (`memory_limit`, `deadline`, a crashed child) ends this message as
//! `failed: render_error` and nothing else: the pool replaces a child it killed, and the worker
//! carries on with the next message.

use serde_json::{Map, Value, json};
use tracing::warn;
use zirv_flow_render::{Hooks, RenderContext, RenderPool, Rendered, TemplateDocument};
use zirv_flow_store::VerifiedSender;

use super::payload::SendJob;

/// The reason a message that could not be rendered fails with.
pub(super) const RENDER_ERROR: &str = "render_error";

/// The host the placeholder links of a message are on. `.invalid` never resolves (RFC 6761), so a
/// link built here cannot reach anyone's server. Signed unsubscribe and preference links, and the
/// click and open tracking, replace these when the tracking endpoints exist.
const PLACEHOLDER_HOST: &str = "https://links.zirv-flow.invalid";

/// What the template reads for this message. `traits` holds the person's value for each registry
/// trait the template may read (null when they have none); `address` is the one the message goes
/// to, as `person.email`.
pub(super) fn context_of(
    job: &SendJob,
    traits: &Map<String, Value>,
    address: &str,
) -> RenderContext {
    let mut context = RenderContext::default();
    for (key, value) in traits.iter().filter(|(_, value)| !value.is_null()) {
        context.person.insert(key.clone(), value.clone());
    }
    context
        .person
        .insert("email".to_owned(), Value::from(address));
    let message = job.message;
    context.message.id = message.to_string();
    context.system.unsubscribe_url = format!("{PLACEHOLDER_HOST}/u/{message}");
    context.system.preferences_url = format!("{PLACEHOLDER_HOST}/p/{message}");
    context
}

/// Renders `document` for `context`, or says why it could not.
pub(super) async fn render(
    pool: Option<&RenderPool>,
    document: Value,
    context: &RenderContext,
) -> Result<Rendered, &'static str> {
    let Some(pool) = pool else {
        warn!("email not rendered: the worker has no render pool");
        return Err(RENDER_ERROR);
    };
    let Ok(document) = serde_json::from_value::<TemplateDocument>(document) else {
        warn!("email not rendered: the stored document is not a template document");
        return Err(RENDER_ERROR);
    };
    match pool
        .render_email(&document, context, &Hooks::untracked())
        .await
    {
        Ok(rendered) => Ok(rendered),
        Err(failure) => {
            // The reason is a fixed code; the failure's text can quote the template.
            warn!(reason = failure.reason(), "email not rendered");
            Err(RENDER_ERROR)
        }
    }
}

/// The payload an email destination is given: the sender, the rendered parts and the extra
/// headers (`List-Unsubscribe` for marketing mail). The address travels beside it as the
/// message's `to`.
pub(super) fn email_payload(sender: &VerifiedSender, rendered: &Rendered) -> Value {
    json!({
        "from": {"name": sender.from_name, "address": sender.from_address},
        "reply_to": sender.reply_to,
        "subject": rendered.subject,
        "html": rendered.html,
        "text": rendered.text,
        "headers": rendered
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect::<Map<String, Value>>(),
    })
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::WorkspaceId;
    use zirv_flow_core::consent::Dimensions;

    use super::super::payload::Queued;
    use super::*;

    fn job() -> SendJob {
        let dimensions = Dimensions::new();
        let payload = Queued {
            workspace: WorkspaceId::from_uuid(Uuid::from_u128(1)),
            message: Uuid::from_u128(2),
            key: "k",
            flow: Uuid::from_u128(3),
            version: 1,
            node: "mail",
            enrollment: Uuid::from_u128(4),
            person: Uuid::from_u128(5),
            template: Uuid::from_u128(6),
            destination: Uuid::from_u128(7),
            topic: None,
            dimensions: &dimensions,
            queued_at: datetime!(2031-05-06 07:08:09 UTC),
        }
        .payload()
        .unwrap();
        SendJob::parse(&payload).unwrap()
    }

    #[test]
    fn the_context_holds_the_traits_the_address_the_message_and_inert_links() {
        let mut traits = Map::new();
        traits.insert("plan".to_owned(), json!("pro"));
        traits.insert("age".to_owned(), Value::Null);

        let context = context_of(&job(), &traits, "ada@example.com");

        assert_eq!(context.person.get("plan"), Some(&json!("pro")));
        assert_eq!(context.person.get("age"), None, "a null trait is absent");
        assert_eq!(context.person.get("email"), Some(&json!("ada@example.com")));
        assert_eq!(context.message.id, Uuid::from_u128(2).to_string());
        for url in [
            &context.system.unsubscribe_url,
            &context.system.preferences_url,
        ] {
            assert!(url.starts_with("https://links.zirv-flow.invalid/"), "{url}");
            assert!(url.ends_with(&Uuid::from_u128(2).to_string()), "{url}");
        }
    }

    #[test]
    fn the_payload_carries_the_sender_the_parts_and_the_headers() {
        let sender = VerifiedSender {
            identity_id: Uuid::from_u128(1),
            domain_id: Uuid::from_u128(2),
            domain: "example.com".to_owned(),
            from_name: "News".to_owned(),
            from_address: "news@example.com".to_owned(),
            reply_to: None,
        };
        let rendered = Rendered {
            subject: "Hello".to_owned(),
            html: "<p>Hi</p>".to_owned(),
            text: "Hi".to_owned(),
            headers: vec![(
                "List-Unsubscribe".to_owned(),
                "<https://x.invalid/u>".to_owned(),
            )],
        };

        let payload = email_payload(&sender, &rendered);

        assert_eq!(
            payload,
            json!({
                "from": {"name": "News", "address": "news@example.com"},
                "reply_to": null,
                "subject": "Hello",
                "html": "<p>Hi</p>",
                "text": "Hi",
                "headers": {"List-Unsubscribe": "<https://x.invalid/u>"},
            })
        );
    }

    #[tokio::test]
    async fn without_a_pool_or_a_document_the_message_fails_as_a_render_error() {
        let context = RenderContext::default();

        assert_eq!(
            render(None, json!({}), &context).await.unwrap_err(),
            RENDER_ERROR
        );
    }
}
