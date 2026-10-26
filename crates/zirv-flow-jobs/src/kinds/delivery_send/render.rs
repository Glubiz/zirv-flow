//! Gate 7 of the send pipeline: the template renders for the person in the render host pool,
//! never in this process (architecture 10.1 and 11.9).
//!
//! Every registered email content type (`html`, `mjml`, `blocks`) goes through the pool's one
//! `render_email` call, which dispatches on the document. Any failure of a render, the renderer's
//! own refusal or the host's (`memory_limit`, `deadline`, a crashed child) ends this message as
//! `failed: render_error` and nothing else: the pool replaces a child it killed, and the worker
//! carries on with the next message.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use sqlx::PgExecutor;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_core::HmacKey;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::tracking::{TokenKind, sign};
use zirv_flow_render::{Hooks, RenderContext, RenderFailure, RenderPool, Rendered, Tracking};
use zirv_flow_store::{StoreError, Template, VerifiedSender, find_workspace};
use zirv_flow_templates::{
    RenderExtras, TemplateDocument, UtmPlan, UtmValues, parse_template_document,
};

use super::payload::SendJob;

/// The reason a message that could not be rendered fails with.
pub(crate) const RENDER_ERROR: &str = "render_error";

/// The host the unsubscribe and preference links of a transactional message are on when the worker
/// has no public URL. `.invalid` never resolves (RFC 6761), so a link built here cannot reach
/// anyone's server. A marketing message is not sent without the real links.
const PLACEHOLDER_HOST: &str = "https://links.zirv-flow.invalid";

/// What builds and signs the tracking links of a message: the public URL, without a trailing
/// slash, and the key that signs the tokens.
pub(crate) struct Tracker {
    pub(crate) base: String,
    pub(crate) key: Arc<HmacKey>,
}

impl Tracker {
    fn token(&self, kind: TokenKind, message: Uuid, link: u16) -> String {
        sign(&self.key, kind, message, link)
    }

    /// The page that unsubscribes the person from this message's topic.
    pub(crate) fn unsubscribe_url(&self, message: Uuid) -> String {
        format!(
            "{}/u/{}",
            self.base,
            self.token(TokenKind::Unsubscribe, message, 0)
        )
    }

    /// The preference center page.
    pub(crate) fn preferences_url(&self, message: Uuid) -> String {
        format!("{}/preferences", self.unsubscribe_url(message))
    }

    /// The open pixel as an `<img>`.
    fn pixel(&self, message: Uuid) -> String {
        format!(
            "<img src=\"{}/t/o/{}\" width=\"1\" height=\"1\" alt=\"\" style=\"display:none\">",
            self.base,
            self.token(TokenKind::Open, message, 0)
        )
    }
}

/// A rendered message and the links click tracking rewrote in it, as `(index, URL)`.
#[derive(Debug)]
pub(crate) struct Tracked {
    pub(crate) rendered: Rendered,
    pub(crate) links: Vec<(u16, String)>,
}

/// `html` with `pixel` just before the last `</body>`, else at the end.
fn with_pixel(html: &str, pixel: &str) -> String {
    let at = html
        .to_ascii_lowercase()
        .rfind("</body")
        .unwrap_or(html.len());
    let mut out = String::with_capacity(html.len() + pixel.len());
    out.push_str(&html[..at]);
    out.push_str(pixel);
    out.push_str(&html[at..]);
    out
}

/// What the template reads for this message. `traits` holds the person's value for each registry
/// trait the template may read (null when they have none); `address` is the one the message goes
/// to, as `person.email`.
pub(super) fn context_of(
    job: &SendJob,
    traits: &Map<String, Value>,
    address: &str,
    tracker: Option<&Tracker>,
) -> RenderContext {
    let mut context = RenderContext::default();
    for (key, value) in traits.iter().filter(|(_, value)| !value.is_null()) {
        context.person.insert(key.clone(), value.clone());
    }
    context
        .person
        .insert("email".to_owned(), Value::from(address));
    address_links(&mut context, job.message, tracker);
    context
}

/// Sets the message id and the unsubscribe and preference URLs the message carries: signed when
/// there is a `tracker`, and on a host that never resolves when there is none.
pub(crate) fn address_links(context: &mut RenderContext, message: Uuid, tracker: Option<&Tracker>) {
    context.message.id = message.to_string();
    match tracker {
        Some(tracker) => {
            context.system.unsubscribe_url = tracker.unsubscribe_url(message);
            context.system.preferences_url = tracker.preferences_url(message);
        }
        None => {
            context.system.unsubscribe_url = format!("{PLACEHOLDER_HOST}/u/{message}");
            context.system.preferences_url = format!("{PLACEHOLDER_HOST}/p/{message}");
        }
    }
}

/// The flow a message is sent from, for the UTM placeholders. A preview or a test send has none.
pub(crate) struct UtmFlow<'a> {
    pub(crate) name: &'a str,
    pub(crate) version: i32,
    pub(crate) step: &'a str,
}

/// The UTM parameters of one message (#240): `None` when the workspace has the setting off or the
/// template opts out, so the output is exactly what it was without the step. The values come from
/// the workspace, the flow, the template and its category (the message's consent category).
pub(crate) async fn utm_plan<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    template: &Template,
    template_version: Option<i32>,
    flow: Option<UtmFlow<'_>>,
) -> Result<Option<UtmPlan>, StoreError> {
    if template.utm == "off" {
        return Ok(None);
    }
    let Some(found) = find_workspace(executor, workspace).await? else {
        return Ok(None);
    };
    let utm = &found.utm;
    if !utm.enabled {
        return Ok(None);
    }
    let flow_version = flow.as_ref().map(|flow| flow.version.to_string());
    let template_version = template_version.map(|version| version.to_string());
    let values = UtmValues {
        workspace: &found.slug,
        flow: flow.as_ref().map_or("", |flow| flow.name),
        flow_version: flow_version.as_deref().unwrap_or(""),
        step: flow.as_ref().map_or("", |flow| flow.step),
        template: &template.name,
        template_version: template_version.as_deref().unwrap_or(""),
        category: &template.category,
    };
    Ok(Some(UtmPlan::new(
        [&utm.source, &utm.medium, &utm.campaign, &utm.content],
        &utm.domains,
        &values,
    )))
}

/// The stored document as the renderer reads it, or the reason it cannot be rendered.
pub(crate) fn document_of(document: &Value) -> Result<TemplateDocument, &'static str> {
    parse_template_document(document).map_err(|_| {
        warn!("email not rendered: the stored document is not a template document");
        RENDER_ERROR
    })
}

/// Renders `document` for `context`, or says why it could not. With a `tracker` the links are
/// rewritten to the click route and the open pixel is added; the links come back to be recorded.
/// Without one the message is left as rendered. `recorded` is what the message's earlier attempts
/// recorded: a URL keeps its recorded index, so a token never leads anywhere but to its own link.
pub(crate) async fn render(
    pool: Option<&RenderPool>,
    document: &TemplateDocument,
    context: &RenderContext,
    extras: &RenderExtras,
    tracker: Option<&Tracker>,
    recorded: &[(u16, String)],
    utm: Option<&UtmPlan>,
) -> Result<Tracked, &'static str> {
    let Some(pool) = pool else {
        warn!("email not rendered: the worker has no render pool");
        return Err(RENDER_ERROR);
    };
    let Some(tracker) = tracker else {
        let rendered = pool
            .render_email_with(
                document,
                context,
                &Hooks {
                    utm: utm.map(UtmPlan::tags),
                    ..Hooks::untracked()
                },
                extras,
            )
            .await
            .map_err(failed)?;
        return Ok(Tracked {
            rendered,
            links: Vec::new(),
        });
    };
    let message = context.message.id.parse::<Uuid>().map_err(|_| {
        warn!("email not rendered: the context holds no message id");
        RENDER_ERROR
    })?;
    let assigner = Mutex::new(LinkIndexes::new(recorded));
    let base = format!("{}/t/c", tracker.base);
    let token = |_position: usize, url: &str| {
        let index = assigner
            .lock()
            .map_or(u16::MAX, |mut assigner| assigner.index_of(url));
        tracker.token(TokenKind::Click, message, index)
    };
    let hooks = Hooks {
        tracking: Tracking::Rewrite {
            base_url: &base,
            token: &token,
        },
        utm: utm.map(UtmPlan::tags),
    };
    let mut rendered = pool
        .render_email_with(document, context, &hooks, extras)
        .await
        .map_err(failed)?;
    let Ok(assigner) = assigner.into_inner() else {
        warn!("email not rendered: the link indexes could not be assigned");
        return Err(RENDER_ERROR);
    };
    let Some(links) = assigner.finish() else {
        warn!("email not rendered: the message has more links than a token can name");
        return Err(RENDER_ERROR);
    };
    rendered.html = with_pixel(&rendered.html, &tracker.pixel(message));
    Ok(Tracked { rendered, links })
}

/// Gives each URL of a message its link index. A URL already recorded for the message keeps its
/// index (rows are immutable, and the token of an earlier attempt must keep leading where that
/// attempt pointed), a URL seen again in this render reuses the one it got, and any other URL gets
/// the next free index: one past the highest recorded or assigned.
struct LinkIndexes {
    by_url: BTreeMap<String, u16>,
    next: u32,
    added: Vec<(u16, String)>,
}

impl LinkIndexes {
    fn new(recorded: &[(u16, String)]) -> Self {
        let mut by_url = BTreeMap::new();
        let mut next = 0;
        for (index, url) in recorded {
            by_url.entry(url.clone()).or_insert(*index);
            next = next.max(u32::from(*index) + 1);
        }
        Self {
            by_url,
            next,
            added: Vec::new(),
        }
    }

    /// The index of `url`. Past `u16::MAX` it answers `u16::MAX` and [`Self::finish`] refuses.
    fn index_of(&mut self, url: &str) -> u16 {
        if let Some(index) = self.by_url.get(url) {
            return *index;
        }
        let Ok(index) = u16::try_from(self.next) else {
            self.next = self.next.saturating_add(1);
            return u16::MAX;
        };
        self.next += 1;
        self.by_url.insert(url.to_owned(), index);
        self.added.push((index, url.to_owned()));
        index
    }

    /// The pairs this render adds to the recorded ones, or none when there were too many URLs.
    fn finish(self) -> Option<Vec<(u16, String)>> {
        (self.next <= u32::from(u16::MAX) + 1).then_some(self.added)
    }
}

/// What a failed render comes to for the message: always [`RENDER_ERROR`], whether the renderer
/// refused the document or the host failed (`memory_limit`, `deadline`, a crashed child). Only the
/// failure's fixed reason is logged; its text can quote the template.
pub(super) fn failed(failure: RenderFailure) -> &'static str {
    warn!(reason = failure.reason(), "email not rendered");
    RENDER_ERROR
}

/// The payload an email destination is given: the sender, the rendered parts and the extra
/// headers (`List-Unsubscribe` for marketing mail). The address travels beside it as the
/// message's `to`.
pub(crate) fn email_payload(sender: &VerifiedSender, rendered: &Rendered) -> Value {
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

        let context = context_of(&job(), &traits, "ada@example.com", None);

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
            hidden: Vec::new(),
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

    fn tracker() -> Tracker {
        let master =
            zirv_flow_core::MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=")
                .unwrap();
        Tracker {
            base: "https://flow.example.test".to_owned(),
            key: Arc::new(
                zirv_flow_core::HmacRoot::bootstrap_from(&master)
                    .hmac_key(zirv_flow_core::HmacPurpose::Tracking),
            ),
        }
    }

    #[test]
    fn with_a_tracker_the_context_holds_signed_unsubscribe_and_preference_links() {
        let tracker = tracker();

        let context = context_of(&job(), &Map::new(), "ada@example.com", Some(&tracker));

        let unsubscribe = &context.system.unsubscribe_url;
        assert!(
            unsubscribe.starts_with("https://flow.example.test/u/"),
            "{unsubscribe}"
        );
        assert_eq!(
            context.system.preferences_url,
            format!("{unsubscribe}/preferences")
        );
        assert!(!unsubscribe.contains(".invalid"));
    }

    #[test]
    fn a_recorded_url_keeps_its_index_and_a_new_one_gets_the_next_free() {
        let recorded = [
            (0, "https://a.test/".to_owned()),
            (1, "https://b.test/".to_owned()),
        ];
        let mut indexes = LinkIndexes::new(&recorded);

        let got: Vec<u16> = ["https://b.test/", "https://c.test/", "https://b.test/"]
            .into_iter()
            .map(|url| indexes.index_of(url))
            .collect();

        assert_eq!(got, [1, 2, 1]);
        assert_eq!(
            indexes.finish(),
            Some(vec![(2, "https://c.test/".to_owned())])
        );
    }

    #[test]
    fn more_distinct_urls_than_a_token_can_name_are_refused() {
        let mut indexes = LinkIndexes::new(&[]);
        for n in 0..=u32::from(u16::MAX) {
            indexes.index_of(&format!("https://a.test/{n}"));
        }
        assert!(indexes.finish().is_some(), "65536 URLs fit 0..=65535");

        let mut indexes = LinkIndexes::new(&[]);
        for n in 0..=u32::from(u16::MAX) + 1 {
            indexes.index_of(&format!("https://a.test/{n}"));
        }
        assert_eq!(indexes.finish(), None);
    }

    #[test]
    fn the_pixel_goes_before_the_closing_body_tag_or_at_the_end() {
        assert_eq!(with_pixel("<body>x</BODY>", "<i>"), "<body>x<i></BODY>");
        assert_eq!(with_pixel("<p>x</p>", "<i>"), "<p>x</p><i>");
        let tracker = tracker();
        let pixel = tracker.pixel(Uuid::from_u128(2));
        assert!(
            pixel.starts_with("<img src=\"https://flow.example.test/t/o/"),
            "{pixel}"
        );
        assert!(pixel.contains("width=\"1\" height=\"1\""));
    }

    #[tokio::test]
    async fn without_a_pool_or_a_document_the_message_fails_as_a_render_error() {
        let context = RenderContext::default();
        let document =
            document_of(&json!({"schema": 1, "channel": "email", "mode": "code"})).unwrap();

        assert_eq!(
            render(
                None,
                &document,
                &context,
                &RenderExtras::default(),
                None,
                &[],
                None
            )
            .await
            .unwrap_err(),
            RENDER_ERROR
        );
        assert_eq!(document_of(&json!({})).unwrap_err(), RENDER_ERROR);
    }
}
