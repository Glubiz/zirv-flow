use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

pub(crate) const HEADER: HeaderName = HeaderName::from_static("x-request-id");

tokio::task_local! {
    static REQUEST_ID: String;
}

/// The request id of the request being handled, if inside the middleware.
pub(crate) fn current() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}

fn is_valid(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Keeps a valid incoming `x-request-id`, otherwise generates one, and echoes it on the response.
pub(crate) async fn assign(mut request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get(&HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| is_valid(value))
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let value = HeaderValue::from_str(&id).ok();
    if let Some(value) = &value {
        request.headers_mut().insert(HEADER, value.clone());
    }
    let mut response = REQUEST_ID.scope(id, next.run(request)).await;
    if let Some(value) = value {
        response.headers_mut().insert(HEADER, value);
    }
    response
}
