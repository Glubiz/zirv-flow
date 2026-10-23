//! The MIME message of an email payload: a `multipart/alternative` of the text and the HTML part,
//! both quoted-printable UTF-8, with the headers the destination owns and the extra headers the
//! template rendered (`List-Unsubscribe` and `List-Unsubscribe-Post` for marketing mail).
//!
//! The text is deterministic for one message: the boundary derives from the idempotency key and
//! the `Message-ID` is the idempotency key at the sending domain, so a retried send carries the
//! same id. Only the `Date` changes. Every line ends in CRLF, so the SMTP client can dot-stuff
//! the bytes as they are. No value from the payload is copied into a header unchecked: a control
//! character in a header is refused, never stripped into a different header.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::error::DestinationError;

/// The header the idempotency key travels in.
pub(crate) const IDEMPOTENCY_HEADER: &str = "X-Zirv-Idempotency-Key";

/// Headers a template cannot set: the destination owns them, or they describe the route.
const RESERVED: [&str; 17] = [
    "from",
    "to",
    "cc",
    "bcc",
    "sender",
    "reply-to",
    "subject",
    "date",
    "message-id",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "return-path",
    "received",
    "dkim-signature",
    "x-zirv-idempotency-key",
    "resent-from",
];

/// Headers whose value is a list of links: a non-ASCII byte in one is percent-encoded, never sent
/// as an RFC 2047 encoded word, which a mail client would not read as part of the URI.
const LINK_HEADERS: [&str; 2] = ["list-unsubscribe", "list-unsubscribe-post"];

/// The longest header line RFC 5322 allows, without the line break.
const MAX_LINE: usize = 998;
/// The length a folded header line stays within where white space allows (RFC 5322 2.1.1).
const FOLD_AT: usize = 78;
/// The most bytes of text one encoded word carries (75 characters in all).
const WORD_BYTES: usize = 45;
/// The longest quoted-printable line, before the soft break.
const QP_LINE: usize = 75;

/// A message ready to be signed and sent.
pub(crate) struct Built {
    /// The headers, in the order they are written. A value may hold folds (`CRLF` and a space).
    pub(crate) headers: Vec<(String, String)>,
    /// The bytes after the blank line, ending in CRLF.
    pub(crate) body: Vec<u8>,
    /// The envelope sender and the domain of the `From` address.
    pub(crate) from_address: String,
    pub(crate) from_domain: String,
}

impl Built {
    /// The message as it goes on the wire, with `signature` (a complete `DKIM-Signature` header)
    /// first when there is one.
    pub(crate) fn wire(&self, signature: Option<&str>) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 2048);
        if let Some(signature) = signature {
            out.extend_from_slice(signature.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        for (name, value) in &self.headers {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

fn malformed(field: &str) -> DestinationError {
    DestinationError::Config {
        field: field.to_owned(),
        reason: "is not usable in an email message".to_owned(),
    }
}

/// Builds the message of `payload` (the email payload of the send pipeline) for `idempotency_key`
/// at `now`.
pub(crate) fn build(
    payload: &Value,
    to: &str,
    idempotency_key: &str,
    now: DateTime<Utc>,
) -> Result<Built, DestinationError> {
    let text_of = |value: Option<&Value>, field: &str| {
        value
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| malformed(field))
    };
    let from = payload.get("from");
    let from_address = text_of(from.and_then(|from| from.get("address")), "from")?;
    let from_name = from
        .and_then(|from| from.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let subject = text_of(payload.get("subject"), "subject")?;
    let html = text_of(payload.get("html"), "html")?;
    let text = text_of(payload.get("text"), "text")?;
    let reply_to = payload.get("reply_to").and_then(Value::as_str);

    if !valid_address(&from_address) {
        return Err(malformed("from"));
    }
    if !valid_address(to) {
        return Err(malformed("to"));
    }
    let from_domain = from_address
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_ascii_lowercase())
        .ok_or_else(|| malformed("from"))?;
    if idempotency_key.is_empty()
        || !idempotency_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(malformed("idempotency_key"));
    }

    let boundary = boundary_of(idempotency_key);
    let mut headers: Vec<(String, String)> = vec![
        (
            "From".into(),
            fold("From", &mailbox(from_name, &from_address)),
        ),
        ("To".into(), format!("<{to}>")),
        (
            "Subject".into(),
            fold("Subject", &unstructured(&single_line(&subject))),
        ),
        (
            "Date".into(),
            now.format("%a, %d %b %Y %H:%M:%S +0000").to_string(),
        ),
        (
            "Message-ID".into(),
            format!("<{idempotency_key}@{from_domain}>"),
        ),
        ("MIME-Version".into(), "1.0".into()),
    ];
    if let Some(reply_to) = reply_to {
        if !valid_address(reply_to) {
            return Err(malformed("reply_to"));
        }
        headers.push(("Reply-To".into(), format!("<{reply_to}>")));
    }
    if let Some(extra) = payload.get("headers") {
        let extra = extra.as_object().ok_or_else(|| malformed("headers"))?;
        let mut chosen: Vec<(String, String)> = Vec::new();
        for (name, value) in extra {
            let value = value.as_str().ok_or_else(|| malformed("headers"))?;
            if !valid_header_name(name) {
                return Err(malformed("headers"));
            }
            if RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
                continue;
            }
            if value.chars().any(|ch| ch != '\t' && ch.is_control()) {
                return Err(malformed("headers"));
            }
            let value = if LINK_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                percent_encoded(value)
            } else {
                unstructured(value)
            };
            // Names are case-insensitive and the signature covers one instance of a name: a name
            // given twice, in any case, keeps the last one given.
            chosen.retain(|(earlier, _)| !earlier.eq_ignore_ascii_case(name));
            chosen.push((name.clone(), value));
        }
        headers.extend(chosen);
    }
    headers.push((IDEMPOTENCY_HEADER.into(), idempotency_key.to_owned()));
    headers.push((
        "Content-Type".into(),
        format!("multipart/alternative; boundary=\"{boundary}\""),
    ));
    for (name, value) in &headers {
        let longest = value.split("\r\n").map(str::len).max().unwrap_or(0);
        if name.len() + 2 + longest > MAX_LINE {
            return Err(malformed("headers"));
        }
    }

    let mut body = Vec::with_capacity(text.len() + html.len() + 512);
    for (kind, content) in [("plain", &text), ("html", &html)] {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Type: text/{kind}; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&quoted_printable(content));
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    Ok(Built {
        headers,
        body,
        from_address,
        from_domain,
    })
}

/// A boundary that depends on the idempotency key alone, so a retry builds the same message.
fn boundary_of(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("=_zirv_{hex}")
}

/// Whether `address` can go in an SMTP path as it is: ASCII, one `@`, no specials. An
/// internationalized address needs SMTPUTF8, which this destination does not use.
pub(crate) fn valid_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    address.len() <= 254
        && !local.is_empty()
        && local.len() <= 64
        && !domain.is_empty()
        && !domain.contains('@')
        && address.bytes().all(|byte| {
            byte.is_ascii_graphic()
                && !matches!(
                    byte,
                    b'<' | b'>' | b'(' | b')' | b'[' | b']' | b'\\' | b',' | b';' | b':' | b'"'
                )
        })
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// `text` with each line break and tab as one space, for a header that must stay on one line.
fn single_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// A display name and address as a `From` value.
fn mailbox(name: &str, address: &str) -> String {
    let name = single_line(name);
    let name = name.trim();
    if name.is_empty() {
        return format!("<{address}>");
    }
    if name.is_ascii() {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\" <{address}>")
    } else {
        format!("{} <{address}>", encoded_words(name))
    }
}

/// An unstructured header value: as it is when it is ASCII, else as RFC 2047 encoded words.
fn unstructured(value: &str) -> String {
    if value.is_ascii() {
        value.to_owned()
    } else {
        encoded_words(value)
    }
}

/// `value` with every non-ASCII byte as `%XX` (the UTF-8 octets), for a header that holds URIs.
fn percent_encoded(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut buffer = [0u8; 4];
            for byte in ch.encode_utf8(&mut buffer).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    out
}

/// An unstructured ASCII `value` of the header `name`, folded at white space so a line stays within
/// 78 characters where the white space allows; a word longer than that stays whole on a line of its
/// own, and the 998 limit is checked by the caller. A value that holds a fold already (encoded
/// words) is left as it is.
fn fold(name: &str, value: &str) -> String {
    if value.contains("\r\n") || !value.is_ascii() {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 8);
    let mut width = name.len() + 2;
    let mut on_line = false;
    let mut rest = value;
    while !rest.is_empty() {
        let gap = rest.len() - rest.trim_start_matches(' ').len();
        let after_gap = &rest[gap..];
        let word = after_gap.find(' ').unwrap_or(after_gap.len());
        let (gap_text, word_text) = (&rest[..gap], &after_gap[..word]);
        if on_line && !word_text.is_empty() && width + gap + word > FOLD_AT {
            out.push_str("\r\n");
            width = 0;
        }
        out.push_str(gap_text);
        out.push_str(word_text);
        width += gap + word;
        on_line = on_line || !word_text.is_empty();
        rest = &after_gap[word..];
    }
    out
}

/// `text` as `=?UTF-8?B?...?=` words of at most 75 characters, folded between words.
fn encoded_words(text: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for ch in text.chars() {
        if chunk.len() + ch.len_utf8() > WORD_BYTES {
            words.push(word(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(word(&chunk));
    }
    words.join("\r\n ")
}

fn word(chunk: &str) -> String {
    format!("=?UTF-8?B?{}?=", STANDARD.encode(chunk.as_bytes()))
}

/// `text` as quoted-printable (RFC 2045 6.7) with CRLF line breaks and lines of at most 76
/// characters. The result has no line break at its end.
pub(crate) fn quoted_printable(text: &str) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(text.len() + text.len() / 8);
    let lines: Vec<&str> = text.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line).as_bytes();
        let mut width = 0;
        for (at, byte) in line.iter().enumerate() {
            let last = at + 1 == line.len();
            let token: Vec<u8> = match byte {
                b'=' => b"=3D".to_vec(),
                b' ' | b'\t' if last => format!("={byte:02X}").into_bytes(),
                b' ' | b'\t' | 33..=126 => vec![*byte],
                _ => format!("={byte:02X}").into_bytes(),
            };
            if width + token.len() > QP_LINE {
                out.extend_from_slice(b"=\r\n");
                width = 0;
            }
            width += token.len();
            out.extend_from_slice(&token);
        }
        if index + 1 < lines.len() {
            out.extend_from_slice(b"\r\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_956_528_000, 0).unwrap()
    }

    fn payload() -> Value {
        json!({
            "from": {"name": "The News", "address": "news@example.com"},
            "reply_to": "help@example.com",
            "subject": "Hello",
            "html": "<p>Hi =)</p>",
            "text": "Hi =)",
            "headers": {
                "List-Unsubscribe": "<https://links.example/u/1>",
                "List-Unsubscribe-Post": "List-Unsubscribe=One-Click",
                "Bcc": "x@example.net",
                "X-Campaign": "spring",
            },
        })
    }

    fn header<'a>(built: &'a Built, name: &str) -> Option<&'a str> {
        built
            .headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn the_message_has_the_owned_headers_the_template_headers_and_two_alternative_parts() {
        let built = build(&payload(), "ada@example.net", "msg-1", now()).unwrap();

        assert_eq!(
            header(&built, "From"),
            Some("\"The News\" <news@example.com>")
        );
        assert_eq!(header(&built, "To"), Some("<ada@example.net>"));
        assert_eq!(header(&built, "Reply-To"), Some("<help@example.com>"));
        assert_eq!(header(&built, "Message-ID"), Some("<msg-1@example.com>"));
        assert_eq!(header(&built, IDEMPOTENCY_HEADER), Some("msg-1"));
        assert_eq!(
            header(&built, "Date"),
            Some("Thu, 01 Jan 2032 00:00:00 +0000")
        );
        assert_eq!(
            header(&built, "List-Unsubscribe"),
            Some("<https://links.example/u/1>")
        );
        assert_eq!(
            header(&built, "List-Unsubscribe-Post"),
            Some("List-Unsubscribe=One-Click")
        );
        assert_eq!(header(&built, "X-Campaign"), Some("spring"));
        assert_eq!(
            header(&built, "Bcc"),
            None,
            "a route header is the destination's"
        );
        let body = String::from_utf8(built.body.clone()).unwrap();
        assert!(body.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(body.contains("Content-Type: text/html; charset=utf-8"));
        assert!(body.contains("<p>Hi =3D)</p>"));
        assert!(body.ends_with("--\r\n"));
        assert!(!body.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn a_retry_builds_the_same_message_except_for_the_date() {
        let first = build(&payload(), "ada@example.net", "msg-1", now()).unwrap();
        let again = build(&payload(), "ada@example.net", "msg-1", now()).unwrap();
        let other = build(&payload(), "ada@example.net", "msg-2", now()).unwrap();

        assert_eq!(first.wire(None), again.wire(None));
        assert_ne!(first.body, other.body, "the boundary follows the key");
        assert_eq!(header(&first, "Message-ID"), header(&again, "Message-ID"));
    }

    #[test]
    fn a_name_or_subject_outside_ascii_is_sent_as_encoded_words_and_never_breaks_a_header() {
        let mut body = payload();
        body["from"]["name"] = json!("Zoë Ørsted");
        body["subject"] = json!("Tilbud: åbent hus\r\nBcc: attacker@example.net");

        let built = build(&body, "ada@example.net", "msg-1", now()).unwrap();

        assert!(header(&built, "From").unwrap().starts_with("=?UTF-8?B?"));
        let subject = header(&built, "Subject").unwrap();
        assert!(!subject.contains('\n'));
        assert!(subject.is_ascii(), "{subject}");
        let wire = String::from_utf8(built.wire(None)).unwrap();
        assert!(!wire.contains("\r\nBcc:"));
    }

    #[test]
    fn a_header_with_a_control_character_or_a_bad_name_is_refused() {
        for headers in [
            json!({"List-Unsubscribe": "<x>\r\nBcc: a@b.c"}),
            json!({"Bad Name": "x"}),
            json!({"X-A": 1}),
        ] {
            let mut body = payload();
            body["headers"] = headers;

            let error = build(&body, "ada@example.net", "msg-1", now())
                .err()
                .unwrap();

            assert!(matches!(error, DestinationError::Config { .. }));
        }
    }

    #[test]
    fn an_address_that_could_end_the_path_is_refused() {
        for bad in [
            "a@b.c>\r\nRCPT TO:<x@y.z",
            "a b@c.d",
            "a@@b.c",
            "ø@b.c",
            "@b.c",
            "a@",
        ] {
            assert!(!valid_address(bad), "{bad:?}");
        }
        assert!(valid_address("ada.lovelace+tag@example.net"));
        let error = build(&payload(), "a@b.c>\r\nX", "msg-1", now())
            .err()
            .unwrap();
        assert!(matches!(error, DestinationError::Config { ref field, .. } if field == "to"));
    }

    #[test]
    fn quoted_printable_escapes_equals_trailing_space_and_non_ascii_and_wraps_at_76() {
        assert_eq!(quoted_printable("a=b"), b"a=3Db");
        assert_eq!(quoted_printable("end \nnext"), b"end=20\r\nnext");
        assert_eq!(quoted_printable("Zoë"), b"Zo=C3=AB");
        let long = quoted_printable(&"x".repeat(200));
        for line in long.split(|byte| *byte == b'\n') {
            assert!(line.len() <= 77, "{}", line.len());
        }
        let joined: Vec<u8> = long
            .split(|byte| *byte == b'\n')
            .flat_map(|line| line.strip_suffix(b"=\r").unwrap_or(line).to_vec())
            .collect();
        assert_eq!(joined.len(), 200);
    }

    #[test]
    fn a_non_ascii_link_is_percent_encoded_never_an_encoded_word_and_a_line_break_is_still_refused()
    {
        let mut body = payload();
        body["headers"] = json!({
            "List-Unsubscribe": "<https://links.example/u/ø?name=Zoë>",
            "List-Unsubscribe-Post": "List-Unsubscribe=One-Click",
            "X-Name": "Zoë",
        });

        let built = build(&body, "ada@example.net", "msg-1", now()).unwrap();

        assert_eq!(
            header(&built, "List-Unsubscribe"),
            Some("<https://links.example/u/%C3%B8?name=Zo%C3%AB>")
        );
        assert!(
            header(&built, "X-Name").unwrap().starts_with("=?UTF-8?B?"),
            "other headers still use encoded words"
        );
        body["headers"] = json!({"list-unsubscribe": "<https://a.example/é>\r\nBcc: x@y.z"});
        assert!(build(&body, "ada@example.net", "msg-1", now()).is_err());
    }

    #[test]
    fn a_header_name_given_twice_in_any_case_keeps_one_and_the_owned_ones_cannot_be_set() {
        let mut body = payload();
        body["headers"] = json!({
            "List-Unsubscribe": "<https://a.example/1>",
            "list-unsubscribe": "<https://a.example/2>",
            "X-Campaign": "one",
            "x-campaign": "two",
            "MESSAGE-id": "<forged@example.net>",
            "X-ZIRV-Idempotency-Key": "forged",
            "subject": "forged",
        });

        let built = build(&body, "ada@example.net", "msg-1", now()).unwrap();

        for name in ["List-Unsubscribe", "X-Campaign", "Message-ID", "Subject"] {
            let count = built
                .headers
                .iter()
                .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                .count();
            assert_eq!(count, 1, "{name}");
        }
        assert_eq!(
            built
                .headers
                .iter()
                .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(IDEMPOTENCY_HEADER))
                .count(),
            1
        );
        assert_eq!(
            header(&built, "List-Unsubscribe"),
            Some("<https://a.example/2>"),
            "the last given wins"
        );
        assert_eq!(header(&built, "X-Campaign"), Some("two"));
        assert_eq!(header(&built, "Message-ID"), Some("<msg-1@example.com>"));
        assert_eq!(header(&built, "Subject"), Some("Hello"));
        assert_eq!(header(&built, IDEMPOTENCY_HEADER), Some("msg-1"));
    }

    #[test]
    fn a_long_ascii_subject_and_display_name_fold_at_white_space_within_78_characters() {
        let subject = "Spring offers for every one of our most loyal customers, picked for you this week only";
        let mut body = payload();
        body["subject"] = json!(subject);
        body["from"]["name"] =
            json!("The Newsletter Team of the Example Company Limited, Northern Region");

        let built = build(&body, "ada@example.net", "msg-1", now()).unwrap();

        for name in ["Subject", "From"] {
            let value = header(&built, name).unwrap();
            assert!(value.contains("\r\n "), "{name} folds: {value}");
            for (index, line) in value.split("\r\n").enumerate() {
                let width = if index == 0 { name.len() + 2 } else { 0 } + line.len();
                assert!(width <= FOLD_AT, "{name}: {width} {line:?}");
                assert!(!line.trim().is_empty());
            }
        }
        assert_eq!(
            header(&built, "Subject").unwrap().replace("\r\n", ""),
            subject
        );
        // A short value, and one word longer than a line, are not touched or broken.
        assert_eq!(fold("Subject", "short one"), "short one");
        let long = "x".repeat(90);
        assert_eq!(
            fold("Subject", &format!("a {long} b")),
            format!("a\r\n {long}\r\n b")
        );
    }

    #[test]
    fn every_header_line_stays_within_the_limit() {
        let mut body = payload();
        body["headers"] = json!({"X-Long": "y".repeat(1200)});

        assert!(build(&body, "ada@example.net", "msg-1", now()).is_err());
    }
}
