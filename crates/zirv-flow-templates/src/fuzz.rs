//! Document invariants shared by proptest and libFuzzer.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{
    FieldCatalog, MAX_DOCUMENT_BYTES, MAX_HTML_BYTES, MAX_TEXT_BYTES, RenderContext,
    TemplateDocument, blocks_to_mjml, migrate_to_current, sandbox, validate,
};

/// Deserialize, migrate, validate and render bounded documents in the MiniJinja sandbox.
pub fn check_template_document(data: &[u8]) {
    if data.len() > MAX_DOCUMENT_BYTES * 2 {
        return;
    }
    let started = Instant::now();
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        let current = match migrate_to_current(value.clone()) {
            Ok(current) => {
                assert_eq!(migrate_to_current(current.clone()), Ok(current.clone()));
                current
            }
            Err(_) => value,
        };
        if let Ok(doc) = serde_json::from_value::<TemplateDocument>(current) {
            let encoded = serde_json::to_vec(&doc).unwrap();
            assert_eq!(
                serde_json::from_slice::<TemplateDocument>(&encoded).unwrap(),
                doc
            );
            let catalog = FieldCatalog::default();
            let _issues = validate(&doc, &catalog);
            let context = minijinja::Value::from_serialize(RenderContext::sample(&catalog));
            let mut env = sandbox::environment();
            env.set_fuel(Some(1_000));
            let render = |name, field, source: &str, limit| {
                if let Ok(output) =
                    sandbox::render_string(&env, name, field, source, &context, limit)
                {
                    assert!(output.len() <= limit);
                }
            };
            if let Some(email) = &doc.email {
                render(
                    "subject.txt",
                    "subject",
                    &email.subject,
                    sandbox::MAX_SUBJECT_BYTES,
                );
                render("html.html", "html", &email.html, MAX_HTML_BYTES);
                render("text.txt", "text", &email.text, MAX_TEXT_BYTES);
            }
            if let Some(webhook) = &doc.webhook {
                render(
                    "body.txt",
                    "body",
                    &webhook.body,
                    sandbox::MAX_WEBHOOK_BODY_BYTES,
                );
                for source in webhook.headers.values().take(sandbox::MAX_HEADER_COUNT + 1) {
                    render(
                        "header.txt",
                        "header",
                        source,
                        sandbox::MAX_HEADER_VALUE_BYTES,
                    );
                }
            }
            if let Some(blocks) = &doc.blocks {
                render(
                    "subject.txt",
                    "subject",
                    &blocks.subject,
                    sandbox::MAX_SUBJECT_BYTES,
                );
                if let Ok(converted) = blocks_to_mjml(blocks) {
                    render("blocks.html", "html", &converted.mjml, MAX_HTML_BYTES);
                    render("blocks.txt", "text", &converted.text, MAX_TEXT_BYTES);
                }
            }
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "template sample exceeded five seconds"
    );
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;
    use zirv_flow_core::fuzz::{config, with_fixtures};

    use super::*;

    #[test]
    fn unterminated_conditional_comment_regression() {
        check_template_document(include_bytes!(
            "../tests/fixtures/fuzz/template_document-crash-861764cf.bin"
        ));
    }

    fn inputs() -> BoxedStrategy<Value> {
        let fixtures = [
            include_str!("../tests/fixtures/email_transactional.doc.json"),
            include_str!("../tests/fixtures/email_marketing.doc.json"),
            include_str!("../tests/fixtures/webhook_order.doc.json"),
            include_str!("../tests/fixtures/blocks/newsletter.doc.json"),
            include_str!("../tests/fixtures/blocks/migrations/image.v2.json"),
        ]
        .into_iter()
        .map(|text| serde_json::from_str(text).unwrap())
        .collect();
        prop_oneof![
            with_fixtures(fixtures),
            ".{0,128}".prop_map(
                |source| json!({"schema": 2, "channel": "email", "mode": "code",
                "email": {"subject": "fuzz", "html": source, "category": "transactional"}})
            ),
        ]
        .boxed()
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn fuzz_template_document(value in inputs()) {
            check_template_document(&serde_json::to_vec(&value).unwrap());
        }

        #[test]
        fn fuzz_template_source_limit(extra in 0usize..=1) {
            let doc = TemplateDocument::email_code(crate::EmailContent {
                subject: "fuzz".to_owned(), html: "x".repeat(sandbox::MAX_SOURCE_BYTES + extra),
                category: crate::Category::Transactional, ..Default::default()
            });
            check_template_document(&serde_json::to_vec(&doc).unwrap());
            let result = sandbox::render_string(&sandbox::environment(), "html.html", "html", &doc.email.as_ref().unwrap().html,
                &minijinja::Value::UNDEFINED, MAX_HTML_BYTES);
            prop_assert_eq!(matches!(result, Err(crate::RenderError::SourceTooLarge { .. })), extra == 1);
        }
    }
}
