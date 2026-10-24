//! The pages of the tracking routes: server-rendered, no script, no external asset, and nothing
//! personal on them beyond the choices themselves.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// Escapes text for an HTML body or a double-quoted attribute.
pub(super) fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A page with `title` and `body` (already escaped markup) answering `status`.
pub(super) fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{title}</title>\
         <style>body{{font:16px/1.5 system-ui,sans-serif;max-width:32rem;margin:3rem auto;padding:0 1rem}}\
         button{{font:inherit;padding:.5rem 1rem}}fieldset{{margin:1rem 0}}label{{display:block}}</style>\
         </head><body>{body}</body></html>",
        title = esc(title),
    );
    let mut response = (status, html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// The page for a token that opens nothing: bad, tampered, of the wrong kind or for a message
/// that is gone. It never says which.
pub(super) fn not_found() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "Link not found",
        "<h1>Link not found</h1><p>This link is not valid. It may have expired or been copied \
         incorrectly.</p>",
    )
}

/// What every `/u` route answers for the token of a test send: nothing was sent to a person, so
/// nothing is changed or written.
pub(super) fn test_send() -> Response {
    page(
        StatusCode::OK,
        "Test send",
        "<h1>Test send</h1><p>This was a test send. Nothing was changed.</p>",
    )
}

pub(super) fn rate_limited(retry_after: u64) -> Response {
    let mut response = page(
        StatusCode::TOO_MANY_REQUESTS,
        "Too many requests",
        "<h1>Too many requests</h1><p>Please wait a moment and try again.</p>",
    );
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

pub(super) fn unavailable() -> Response {
    page(
        StatusCode::SERVICE_UNAVAILABLE,
        "Try again later",
        "<h1>Try again later</h1><p>This page is not available right now. Nothing was changed.</p>",
    )
}

/// The confirmation a GET shows: a prefetcher that follows the link changes nothing, and a person
/// presses the button.
pub(super) fn confirm(token: &str) -> Response {
    let token = esc(token);
    page(
        StatusCode::OK,
        "Unsubscribe",
        &format!(
            "<h1>Unsubscribe</h1><p>Stop receiving these emails?</p>\
             <form method=\"post\" action=\"/u/{token}\"><button type=\"submit\">Unsubscribe</button></form>\
             <p><a href=\"/u/{token}/preferences\">Choose what you receive instead</a></p>"
        ),
    )
}

/// What a POST answers once consent is withdrawn (and again on a repeat).
pub(super) fn done(token: &str) -> Response {
    let token = esc(token);
    page(
        StatusCode::OK,
        "Unsubscribed",
        &format!(
            "<h1>You are unsubscribed</h1><p>You will not receive these emails any more.</p>\
             <p><a href=\"/u/{token}/preferences\">Manage your preferences</a></p>"
        ),
    )
}

/// One checkbox of the preference page.
pub(super) struct Choice {
    /// The form field: `topic` or `dim`.
    pub(super) field: &'static str,
    /// What the field carries: a topic key, or `dimension/value`.
    pub(super) value: String,
    pub(super) label: String,
    pub(super) checked: bool,
}

/// A titled group of choices.
pub(super) struct Group {
    pub(super) title: String,
    pub(super) choices: Vec<Choice>,
}

/// The preference center: the groups with the person's current choices, and a note after a save.
pub(super) fn preferences(token: &str, groups: &[Group], saved: bool) -> Response {
    let token = esc(token);
    let mut body = String::from("<h1>Email preferences</h1>");
    if saved {
        body.push_str("<p role=\"status\">Your preferences are saved.</p>");
    }
    if groups.iter().all(|group| group.choices.is_empty()) {
        body.push_str(&format!(
            "<p>There is nothing to choose here.</p>\
             <p><a href=\"/u/{token}\">Unsubscribe from these emails</a></p>"
        ));
        return page(StatusCode::OK, "Email preferences", &body);
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/u/{token}/preferences\">"
    ));
    for group in groups.iter().filter(|group| !group.choices.is_empty()) {
        body.push_str(&format!("<fieldset><legend>{}</legend>", esc(&group.title)));
        for choice in &group.choices {
            body.push_str(&format!(
                "<label><input type=\"checkbox\" name=\"{}\" value=\"{}\"{}> {}</label>",
                choice.field,
                esc(&choice.value),
                if choice.checked { " checked" } else { "" },
                esc(&choice.label)
            ));
        }
        body.push_str("</fieldset>");
    }
    body.push_str("<button type=\"submit\">Save preferences</button></form>");
    page(StatusCode::OK, "Email preferences", &body)
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;

    use super::*;

    async fn text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn text_is_escaped_for_markup_and_attributes() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[tokio::test]
    async fn every_page_is_html_without_script_and_never_stored() {
        for response in [
            not_found(),
            unavailable(),
            rate_limited(3),
            confirm("tok"),
            done("tok"),
            preferences("tok", &[], false),
        ] {
            let headers = response.headers();
            assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
            assert_eq!(headers[header::CACHE_CONTROL], "no-store");
            assert!(
                headers[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .starts_with("default-src 'none'")
            );
            let body = text(response).await.to_lowercase();
            assert!(!body.contains("<script"), "{body}");
            assert!(
                !body.contains("http://") && !body.contains("https://"),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn the_confirmation_posts_to_the_token_and_the_preferences_escape_the_names() {
        let confirm = text(confirm("a-b_c")).await;
        assert!(
            confirm.contains("method=\"post\" action=\"/u/a-b_c\""),
            "{confirm}"
        );

        let groups = [Group {
            title: "Topics <b>".to_owned(),
            choices: vec![Choice {
                field: "topic",
                value: "news".to_owned(),
                label: "News & \"offers\"".to_owned(),
                checked: true,
            }],
        }];
        let page = text(preferences("tok", &groups, true)).await;
        assert!(page.contains("Topics &lt;b&gt;"), "{page}");
        assert!(page.contains("News &amp; &quot;offers&quot;"), "{page}");
        assert!(
            page.contains("name=\"topic\" value=\"news\" checked"),
            "{page}"
        );
        assert!(page.contains("Your preferences are saved."), "{page}");
    }
}
