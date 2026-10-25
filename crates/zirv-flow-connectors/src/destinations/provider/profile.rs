//! The request of a profile operation (#226): the external key placed in the path and the body,
//! and the node's listed traits placed in the body, at the places the manifest names.

use serde_json::{Map, Value};

use super::super::error::DestinationError;
use crate::manifest::{KEY_PLACEHOLDER, ProfileSpec, TraitPlacement, place};

/// The longest external key a request carries.
pub(super) const MAX_KEY_BYTES: usize = 512;

/// Whether `key`, placed as a path segment, would be a dot segment. The encoder percent-encodes
/// every byte outside the unreserved set, `%` included, so only the exact keys `.` and `..` can
/// reach the path as one.
pub(super) fn is_dot_segment(key: &str) -> bool {
    key == "." || key == ".."
}

/// `key` as one path segment: every byte outside the unreserved set is percent-encoded, so a `/`,
/// `?` or `#` cannot end the segment or start a query or fragment, and `.` and `..` cannot
/// navigate.
pub(super) fn encode_segment(key: &str) -> String {
    if is_dot_segment(key) {
        return key.replace('.', "%2E");
    }
    let mut out = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The operation's path with the placeholder, if it has one, replaced by the encoded key.
pub(super) fn path_for(spec: &ProfileSpec, key: &str) -> String {
    spec.path.replace(KEY_PLACEHOLDER, &encode_segment(key))
}

fn unusable() -> DestinationError {
    DestinationError::Config {
        field: "manifest".to_owned(),
        reason: "does not make a valid body".to_owned(),
    }
}

/// The JSON body of one operation, or none when the manifest places nothing. A listed trait the
/// person has no value for is sent as `null`, so a cleared trait clears at the provider too.
pub(super) fn build_body(
    spec: &ProfileSpec,
    key: &str,
    traits: &Map<String, Value>,
) -> Result<Option<Value>, DestinationError> {
    if !spec.has_body() {
        return Ok(None);
    }
    let mut body = Value::Null;
    for (pointer, value) in &spec.statics {
        place(&mut body, pointer, value.clone()).map_err(|_| unusable())?;
    }
    if let Some(pointer) = &spec.key {
        place(&mut body, pointer, Value::from(key)).map_err(|_| unusable())?;
    }
    match &spec.traits {
        Some(TraitPlacement::Object(pointer)) => {
            place(&mut body, pointer, Value::Object(traits.clone())).map_err(|_| unusable())?;
        }
        Some(TraitPlacement::Fields(fields)) => {
            for (name, pointer) in fields {
                if let Some(value) = traits.get(name) {
                    place(&mut body, pointer, value.clone()).map_err(|_| unusable())?;
                }
            }
        }
        None => {}
    }
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::manifest::{KeySource, ProfileMethod};

    fn spec(path: &str, key: Option<&str>, traits: Option<TraitPlacement>) -> ProfileSpec {
        ProfileSpec {
            method: ProfileMethod::Put,
            path: path.to_owned(),
            idempotency_header: "Idempotency-Key".to_owned(),
            key_from: KeySource::Identity("email".to_owned()),
            key: key.map(str::to_owned),
            traits,
            statics: vec![("/source".to_owned(), json!("zirv"))],
            rate_limit: None,
            errors: Vec::new(),
        }
    }

    #[test]
    fn a_key_stays_in_one_path_segment() {
        for key in [
            "a/b",
            "a?b=c",
            "a#b",
            "a b",
            "a%2Fb",
            "../x",
            "ä@x.test",
            "a\\b",
        ] {
            let path = path_for(&spec("/profiles/{key}/x", None, None), key);
            let segment = path
                .strip_prefix("/profiles/")
                .and_then(|rest| rest.strip_suffix("/x"))
                .unwrap_or_else(|| panic!("{path}"));

            assert!(
                !segment.contains(['/', '?', '#', ' ', '\\']),
                "{key} -> {path}"
            );
            assert!(
                segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-._~%".contains(&byte))
            );
        }
        assert_eq!(encode_segment("a/b?c#d"), "a%2Fb%3Fc%23d");
        assert_eq!(encode_segment("ada@example.test"), "ada%40example.test");
        assert_eq!(encode_segment("."), "%2E");
        assert_eq!(encode_segment(".."), "%2E%2E");
        assert_eq!(
            encode_segment("a%2Fb"),
            "a%252Fb",
            "a percent sign is encoded itself"
        );
    }

    fn decoded(segment: &str) -> String {
        let bytes = segment.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'%' {
                let pair = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap();
                out.push(u8::from_str_radix(pair, 16).unwrap());
                at += 3;
            } else {
                out.push(bytes[at]);
                at += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_keys_url_ends_in_one_segment_that_decodes_to_the_key_under_the_base_path() {
        let base = url::Url::parse("https://api.example.test/v1/").unwrap();

        for key in [
            "ada@example.test",
            "a/b?c#d",
            "a%2eb",
            "%2e%2e",
            ".%2E",
            "a..b",
        ] {
            let path = path_for(&spec("/profiles/{key}/attrs", None, None), key);
            let url = super::super::joined(&base, &path).unwrap();
            let segments: Vec<&str> = url.path_segments().unwrap().collect();

            assert_eq!(segments.len(), 4, "{key}");
            assert_eq!(segments[..2], ["v1", "profiles"], "{key}");
            assert_eq!(segments[3], "attrs", "{key}");
            assert_eq!(decoded(segments[2]), key, "{key}");
            assert!(url.query().is_none() && url.fragment().is_none());
        }
    }

    #[test]
    fn a_path_without_the_placeholder_is_unchanged() {
        assert_eq!(path_for(&spec("/profiles", None, None), "k/1"), "/profiles");
    }

    #[test]
    fn the_object_form_holds_exactly_the_listed_traits_and_the_key_and_statics() {
        let traits: Map<String, Value> =
            serde_json::from_value(json!({"plan": "pro", "age": null})).unwrap();
        let placement = TraitPlacement::Object("/profile/attributes".to_owned());

        let body = build_body(&spec("/p", Some("/id"), Some(placement)), "k-1", &traits)
            .unwrap()
            .unwrap();

        assert_eq!(
            body,
            json!({
                "source": "zirv",
                "id": "k-1",
                "profile": {"attributes": {"plan": "pro", "age": null}}
            })
        );
    }

    #[test]
    fn the_fields_form_places_only_the_listed_and_named_traits() {
        let traits: Map<String, Value> =
            serde_json::from_value(json!({"plan": "pro", "other": 1})).unwrap();
        let placement = TraitPlacement::Fields(vec![
            ("plan".to_owned(), "/p".to_owned()),
            ("country".to_owned(), "/c".to_owned()),
        ]);

        let body = build_body(&spec("/p", None, Some(placement)), "k", &traits)
            .unwrap()
            .unwrap();

        assert_eq!(body, json!({"source": "zirv", "p": "pro"}));
    }

    #[test]
    fn an_operation_that_places_nothing_has_no_body() {
        let mut bare = spec("/p/{key}", None, None);
        bare.statics.clear();

        assert_eq!(build_body(&bare, "k", &Map::new()).unwrap(), None);
    }
}
