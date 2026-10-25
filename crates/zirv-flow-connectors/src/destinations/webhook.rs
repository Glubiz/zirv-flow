//! The built-in `webhook` destination: one signed HTTP POST (architecture 9.1 and 10.1, ruling
//! 9cb0bea9). The `webhook.call` job of an `action.webhook` node delivers through it, so the
//! signing, the headers and the guarded client with no redirects are written once.
//!
//! The caller renders the message and hands over the exact bytes: [`Outbound::payload`] is
//! `{"body": "<text>", "timestamp": <unix seconds>, "headers": {"Name": "value"}}`. The body is
//! text because a JSON value does not keep its key order, and the signature covers the bytes
//! that are sent. The destination adds the content type, the timestamp, the idempotency key and
//! one signature per signing secret (the current one first, then the previous one while it still
//! signs), and sends. `headers` are extra headers a template rendered; they never replace the
//! four the destination owns, nor a header the connection owns.
//!
//! The `event` channel carries forwarded events (`event.forward`): the body is the mapped payload as
//! JSON text and the idempotency key is the outbox row's.
//!
//! The configuration is the URL. The credential holds the signing secret and, during a rotation,
//! the previous one. The response body is never read. A 2xx answer is a delivery; any other
//! status is [`DestinationError::Answered`] with the raw `Retry-After`, and the pipeline's
//! verdict function decides what it means.

use serde_json::Value;
use zirv_flow_core::flow::{
    IDEMPOTENCY_HEADER, SIGNATURE_HEADER, TIMESTAMP_HEADER, signature_value,
};

use super::context::DestinationContext;
use super::destination::{Delivered, Destination, Outbound};
use super::error::DestinationError;
use super::spec::{Capabilities, DestinationSpec};
use crate::sources::{Credential, FieldKind, FieldSpec};

/// The content type of the body.
const CONTENT_TYPE: &str = "application/json";

/// Headers a template cannot set: the destination owns them (the body is signed JSON, so the
/// content type is always `application/json`), or they describe the connection (hop-by-hop).
const RESERVED: [&str; 15] = [
    "content-type",
    "x-zirv-signature",
    "x-zirv-timestamp",
    "x-zirv-idempotency-key",
    "host",
    "content-length",
    "expect",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub(crate) struct Webhook;

impl Destination for Webhook {
    const SPEC: DestinationSpec = DestinationSpec {
        kind: "webhook",
        label: "Webhook",
        description: "A signed HTTP POST to a URL.",
        config_fields: &[FieldSpec {
            key: "url",
            label: "URL",
            help: "The address that receives the call.",
            kind: FieldKind::Url,
            required: true,
            secret: false,
        }],
        credential_fields: &[
            FieldSpec {
                key: "signing_secret",
                label: "Signing secret",
                help: "Signs every call.",
                kind: FieldKind::Text,
                required: true,
                secret: true,
            },
            FieldSpec {
                key: "previous_signing_secret",
                label: "Previous signing secret",
                help: "Also signs while a rotation is open.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
        ],
        capabilities: Capabilities {
            channels: &["webhook", "event"],
            idempotency_key: true,
            batching: false,
            // A raw URL has no rate bucket (architecture 10.1).
            rate_limit: false,
            erase: false,
            signs_dkim: false,
        },
        config_check: None,
    };

    async fn check(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), DestinationError> {
        let url = url_of(config)?;
        credential
            .get("signing_secret")
            .ok_or_else(|| missing("signing_secret"))?;
        // Nothing is sent: the host must resolve to addresses the egress policy allows.
        if let Some(host) = url::Url::parse(url)
            .ok()
            .as_ref()
            .and_then(url::Url::host_str)
        {
            ctx.resolve_checked(host.trim_matches(['[', ']'])).await?;
        }
        Ok(())
    }

    async fn deliver(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        message: &Outbound<'_>,
    ) -> Result<Delivered, DestinationError> {
        let url = url_of(config)?;
        let current = credential
            .get("signing_secret")
            .ok_or_else(|| missing("signing_secret"))?;
        let body = message
            .payload
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("body"))?;
        let timestamp = message
            .payload
            .get("timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| malformed("timestamp"))?;

        let timestamp_text = timestamp.to_string();
        let mut headers: Vec<(String, String)> = vec![("Content-Type".into(), CONTENT_TYPE.into())];
        if let Some(extra) = message.payload.get("headers").and_then(Value::as_object) {
            for (name, value) in extra {
                let Some(value) = value.as_str() else {
                    return Err(malformed("headers"));
                };
                if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
                    continue;
                }
                headers.push((name.clone(), value.to_owned()));
            }
        }
        headers.push((TIMESTAMP_HEADER.into(), timestamp_text));
        headers.push((
            IDEMPOTENCY_HEADER.into(),
            message.idempotency_key.to_owned(),
        ));
        for secret in [Some(current), credential.get("previous_signing_secret")]
            .into_iter()
            .flatten()
        {
            headers.push((
                SIGNATURE_HEADER.into(),
                signature_value(secret.expose().as_bytes(), timestamp, body.as_bytes()),
            ));
        }
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();

        let response = ctx
            .http()
            .post(url, &borrowed, body.as_bytes().to_vec())
            .await?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            // The body is dropped unread.
            return Ok(Delivered {
                status: Some(status),
                remote_id: None,
            });
        }
        Err(DestinationError::Answered {
            status,
            retry_after: response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        })
    }
}

fn url_of(config: &Value) -> Result<&str, DestinationError> {
    config
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| missing("url"))
}

fn missing(field: &str) -> DestinationError {
    DestinationError::Config {
        field: field.to_owned(),
        reason: "is required".to_owned(),
    }
}

fn malformed(field: &str) -> DestinationError {
    DestinationError::Other(format!("the webhook message has no usable {field}"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::json;
    use tokio::net::TcpListener;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::super::error::Disposition;
    use super::*;
    use crate::http::{HttpError, HttpLimits};

    type Seen = Arc<Mutex<Vec<(Vec<(String, String)>, Vec<u8>)>>>;

    #[derive(Clone)]
    struct Mock {
        seen: Seen,
        status: u16,
        retry_after: Option<&'static str>,
    }

    async fn receive(
        State(mock): State<Mock>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let headers = headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        mock.seen.lock().unwrap().push((headers, body.to_vec()));
        let mut response =
            (StatusCode::from_u16(mock.status).unwrap(), "RESPONSE-BODY").into_response();
        if let Some(wait) = mock.retry_after {
            response
                .headers_mut()
                .insert("retry-after", wait.parse().unwrap());
        }
        response
    }

    async fn serve(status: u16, retry_after: Option<&'static str>) -> (String, Seen) {
        let seen = Seen::default();
        let app = Router::new()
            .route("/hook", post(receive))
            .with_state(Mock {
                seen: Arc::clone(&seen),
                status,
                retry_after,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen)
    }

    fn ctx() -> DestinationContext {
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        DestinationContext::new(
            Arc::new(policy),
            HttpLimits {
                max_redirects: 0,
                ..HttpLimits::default()
            },
            "webhook",
            "destination-1",
        )
        .unwrap()
    }

    fn credential(current: &str, previous: Option<&str>) -> Credential {
        let mut credential = Credential::new();
        credential.insert("signing_secret", SecretString::new(current.to_owned()));
        if let Some(previous) = previous {
            credential.insert(
                "previous_signing_secret",
                SecretString::new(previous.to_owned()),
            );
        }
        credential
    }

    fn message(payload: &Value) -> Outbound<'_> {
        Outbound {
            idempotency_key: "key-1",
            channel: "webhook",
            to: "",
            payload,
            dkim: None,
        }
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
        headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    #[test]
    fn the_spec_has_a_secret_credential_no_rate_limit_and_the_webhook_and_event_channels() {
        let spec = &Webhook::SPEC;

        assert_eq!(spec.kind, "webhook");
        assert!(!spec.capabilities.rate_limit);
        assert_eq!(spec.capabilities.channels, &["webhook", "event"]);
        assert!(spec.credential_fields.iter().all(|field| field.secret));
    }

    #[tokio::test]
    async fn the_exact_body_bytes_are_signed_by_every_secret_and_the_owned_headers_win() {
        let (url, seen) = serve(204, None).await;
        let body = r#"{"b":1,"a":2.50,"c":"x"}"#;
        let payload = json!({
            "body": body,
            "timestamp": 1_700_000_000,
            "headers": {
                "X-Custom": "yes",
                "x-zirv-signature": "forged",
                "Host": "evil.test",
                "Content-Type": "text/plain"
            }
        });

        let delivered = Webhook
            .deliver(
                &ctx(),
                &json!({"url": url}),
                &credential("zfwh_current", Some("zfwh_previous")),
                &message(&payload),
            )
            .await
            .unwrap();

        assert_eq!(
            delivered,
            Delivered {
                status: Some(204),
                remote_id: None
            }
        );
        let seen = seen.lock().unwrap();
        let (headers, sent) = &seen[0];
        assert_eq!(sent, body.as_bytes());
        assert_eq!(header(headers, "x-custom"), ["yes"]);
        assert_eq!(
            header(headers, "content-type"),
            ["application/json"],
            "the signed JSON body is always sent as JSON"
        );
        assert_eq!(header(headers, "x-zirv-timestamp"), ["1700000000"]);
        assert_eq!(header(headers, "x-zirv-idempotency-key"), ["key-1"]);
        assert_eq!(
            header(headers, "x-zirv-signature"),
            [
                signature_value(b"zfwh_current", 1_700_000_000, body.as_bytes()),
                signature_value(b"zfwh_previous", 1_700_000_000, body.as_bytes()),
            ]
        );
        assert_ne!(header(headers, "host"), ["evil.test"]);
    }

    #[tokio::test]
    async fn a_template_cannot_set_a_hop_by_hop_header() {
        let (url, seen) = serve(204, None).await;
        let payload = json!({
            "body": "{}",
            "timestamp": 1,
            "headers": {
                "TE": "trailers",
                "Upgrade": "websocket",
                "Keep-Alive": "timeout=5",
                "Trailer": "X-A",
                "Proxy-Connection": "keep-alive",
                "Proxy-Authorization": "Basic eA==",
                "Transfer-Encoding": "chunked",
                "Connection": "upgrade",
                "X-Kept": "yes"
            }
        });

        Webhook
            .deliver(
                &ctx(),
                &json!({"url": url}),
                &credential("s", None),
                &message(&payload),
            )
            .await
            .unwrap();

        let seen = seen.lock().unwrap();
        let (headers, _) = &seen[0];
        for name in [
            "te",
            "upgrade",
            "keep-alive",
            "trailer",
            "proxy-connection",
            "proxy-authorization",
        ] {
            assert!(header(headers, name).is_empty(), "{name}");
        }
        assert_ne!(header(headers, "transfer-encoding"), ["chunked"]);
        assert_ne!(header(headers, "connection"), ["upgrade"]);
        assert_eq!(header(headers, "x-kept"), ["yes"]);
    }

    #[tokio::test]
    async fn a_non_success_status_is_answered_with_the_raw_retry_after() {
        for (status, wait, disposition) in [
            (503, Some("7"), Disposition::Retryable),
            (404, None, Disposition::Terminal),
            (302, None, Disposition::Terminal),
        ] {
            let (url, seen) = serve(status, wait).await;
            let payload = json!({"body": "{}", "timestamp": 1});

            let error = Webhook
                .deliver(
                    &ctx(),
                    &json!({"url": url}),
                    &credential("s", None),
                    &message(&payload),
                )
                .await
                .unwrap_err();

            assert_eq!(
                error,
                DestinationError::Answered {
                    status,
                    retry_after: wait.map(str::to_owned)
                }
            );
            assert_eq!(error.disposition(), disposition);
            assert_eq!(seen.lock().unwrap().len(), 1, "a redirect is not followed");
        }
    }

    #[tokio::test]
    async fn a_refused_address_is_an_egress_denial_and_nothing_is_sent() {
        let payload = json!({"body": "{}", "timestamp": 1});
        let ctx = DestinationContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "webhook",
            "destination-1",
        )
        .unwrap();

        let error = Webhook
            .deliver(
                &ctx,
                &json!({"url": "http://127.0.0.1:9/hook"}),
                &credential("s", None),
                &message(&payload),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                DestinationError::Egress(HttpError::EgressDenied { .. })
            ),
            "{error:?}"
        );
        assert_eq!(error.disposition(), Disposition::Terminal);
    }

    #[tokio::test]
    async fn a_message_or_setting_that_is_not_usable_is_refused_without_a_call() {
        let ok = json!({"body": "{}", "timestamp": 1});
        let credential = credential("s", None);
        let config = json!({"url": "http://127.0.0.1:9/hook"});

        for payload in [
            json!({"timestamp": 1}),
            json!({"body": "{}"}),
            json!({"body": "{}", "timestamp": 1, "headers": {"X-A": 1}}),
        ] {
            let error = Webhook
                .deliver(&ctx(), &config, &credential, &message(&payload))
                .await
                .unwrap_err();
            assert!(matches!(error, DestinationError::Other(_)), "{error:?}");
        }
        assert!(matches!(
            Webhook
                .deliver(&ctx(), &json!({}), &credential, &message(&ok))
                .await,
            Err(DestinationError::Config { ref field, .. }) if field == "url"
        ));
        assert!(matches!(
            Webhook
                .deliver(&ctx(), &config, &Credential::new(), &message(&ok))
                .await,
            Err(DestinationError::Config { ref field, .. }) if field == "signing_secret"
        ));
    }

    #[tokio::test]
    async fn check_resolves_the_host_through_the_policy_and_needs_the_secret() {
        let config = json!({"url": "http://127.0.0.1:9/hook"});

        assert_eq!(
            Webhook.check(&ctx(), &config, &credential("s", None)).await,
            Ok(())
        );
        assert!(matches!(
            Webhook.check(&ctx(), &config, &Credential::new()).await,
            Err(DestinationError::Config { .. })
        ));
        let strict = DestinationContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "webhook",
            "destination-1",
        )
        .unwrap();
        assert!(matches!(
            Webhook
                .check(&strict, &config, &credential("s", None))
                .await,
            Err(DestinationError::Egress(HttpError::EgressDenied { .. }))
        ));
    }
}
