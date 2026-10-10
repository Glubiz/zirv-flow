//! The session cookie: reading it from a request and building the `Set-Cookie` values.

use axum::http::{HeaderMap, HeaderValue, header};

pub(crate) const SESSION_COOKIE: &str = "zf_session";

/// How long the browser keeps the cookie: the absolute session timeout.
pub(crate) const SESSION_MAX_AGE_SECONDS: i64 = 7 * 24 * 3600;

/// The value of the named cookie, looking through every `Cookie` header.
pub(crate) fn read<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

/// `Set-Cookie` value that stores the session token. HttpOnly keeps it from scripts,
/// SameSite=Lax from cross-site subrequests.
pub(crate) fn session(token: &str, secure: bool) -> HeaderValue {
    build(token, SESSION_MAX_AGE_SECONDS, secure)
}

/// `Set-Cookie` value that removes the session cookie.
pub(crate) fn clear(secure: bool) -> HeaderValue {
    build("", 0, secure)
}

fn build(value: &str, max_age: i64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    // Tokens are base64url, so the value is always a valid header value.
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}"
    ))
    .expect("cookie values are visible ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::COOKIE, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn the_named_cookie_is_found_among_others() {
        let headers = headers(&["a=1; zf_session=tok-en_1; b=2"]);

        assert_eq!(read(&headers, "zf_session"), Some("tok-en_1"));
        assert_eq!(read(&headers, "a"), Some("1"));
        assert_eq!(read(&headers, "missing"), None);
    }

    #[test]
    fn every_cookie_header_is_searched_and_names_match_exactly() {
        let headers = headers(&["x_zf_session=no", "other=1", "zf_session=yes"]);

        assert_eq!(read(&headers, "zf_session"), Some("yes"));
    }

    #[test]
    fn a_request_without_cookies_yields_nothing() {
        assert_eq!(read(&HeaderMap::new(), "zf_session"), None);
    }

    #[test]
    fn the_session_cookie_is_http_only_lax_and_secure_by_default() {
        let value = session("abc", true);
        let text = value.to_str().unwrap();

        assert!(text.starts_with("zf_session=abc;"));
        for attribute in [
            "HttpOnly",
            "SameSite=Lax",
            "Path=/",
            "Max-Age=604800",
            "Secure",
        ] {
            assert!(text.contains(attribute), "{text}");
        }
    }

    #[test]
    fn the_secure_attribute_can_be_turned_off() {
        let text = session("abc", false).to_str().unwrap().to_owned();

        assert!(!text.contains("Secure"));
        assert!(text.contains("HttpOnly"));
    }

    #[test]
    fn clearing_expires_the_cookie_immediately() {
        let text = clear(true).to_str().unwrap().to_owned();

        assert!(text.starts_with("zf_session=;"));
        assert!(text.contains("Max-Age=0"));
    }
}
