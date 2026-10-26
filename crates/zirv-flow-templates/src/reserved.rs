//! The reserved channels (architecture 11.1): typed `sms`, `push` and `in_app` contents with
//! their length rules. They are validated and never publishable, so `validate` still returns the
//! blocking `unsupported_channel` after the content checks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::issue::{Issue, code};

/// Characters of an SMS text.
pub const MAX_SMS_CHARS: usize = 1_600;
/// Characters of an SMS sender id.
pub const MAX_SMS_SENDER_CHARS: usize = 15;
/// Segments above which an SMS text draws a warning.
pub const SMS_SEGMENT_WARNING: u32 = 10;
/// Characters of a push title.
pub const MAX_PUSH_TITLE_CHARS: usize = 65;
/// Characters of a push body.
pub const MAX_PUSH_BODY_CHARS: usize = 240;
/// Bytes of the push data map, keys and values together.
pub const MAX_PUSH_DATA_BYTES: usize = 4 * 1024;
/// Characters of an in-app title.
pub const MAX_IN_APP_TITLE_CHARS: usize = 100;
/// Characters of an in-app body.
pub const MAX_IN_APP_BODY_CHARS: usize = 1_000;
/// Bytes of a URL of a reserved content.
const MAX_URL_BYTES: usize = 2_048;

/// SMS content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SmsContent {
    /// 1 to 1,600 characters.
    pub text: String,
    pub sender_id: String,
}

/// Push notification content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PushContent {
    /// 1 to 65 characters.
    pub title: String,
    /// 1 to 240 characters.
    pub body: String,
    /// An `https` URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep_link: Option<String>,
    /// Extra string pairs, at most 4 KiB together.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, String>,
}

/// In-app message content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InAppContent {
    /// 1 to 100 characters.
    pub title: String,
    /// 1 to 1,000 characters.
    pub body: String,
    /// An `https` URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cta_label: Option<String>,
    /// An `https` URL; a call to action has a label and a URL together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cta_url: Option<String>,
}

/// How an SMS text is encoded and how many segments it takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmsEncoding {
    /// The GSM 03.38 alphabet: 160 septets in one segment, 153 in each of several.
    Gsm7,
    /// UCS-2, for any other character: 70 units in one segment, 67 in each of several.
    Ucs2,
}

/// The size of an SMS text on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmsSegments {
    pub encoding: SmsEncoding,
    /// Septets (GSM-7) or UTF-16 code units (UCS-2).
    pub units: usize,
    pub segments: u32,
}

const GSM_BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";
/// Characters the extension table reaches with an escape: two septets each.
const GSM_EXTENSION: &str = "^{}\\[~]|€\u{c}";

/// The encoding of `text` and its segment count, as a pure function of the text.
pub fn sms_segments(text: &str) -> SmsSegments {
    let mut septets = 0usize;
    let mut gsm = true;
    for character in text.chars() {
        if GSM_BASIC.contains(character) {
            septets += 1;
        } else if GSM_EXTENSION.contains(character) {
            septets += 2;
        } else {
            gsm = false;
            break;
        }
    }
    let (encoding, units, single, multi) = if gsm {
        (SmsEncoding::Gsm7, septets, 160, 153)
    } else {
        (SmsEncoding::Ucs2, text.encode_utf16().count(), 70, 67)
    };
    let segments = if units == 0 {
        0
    } else if units <= single {
        1
    } else {
        u32::try_from(units.div_ceil(multi)).unwrap_or(u32::MAX)
    };
    SmsSegments {
        encoding,
        units,
        segments,
    }
}

fn length(at: &str, text: &str, max: usize, issues: &mut Vec<Issue>) {
    let count = text.trim().chars().count();
    if count == 0 {
        issues.push(Issue::new(code::MISSING_CONTENT, at));
    } else if text.chars().count() > max {
        issues.push(
            Issue::new(code::CONTENT_TOO_LARGE, at)
                .with_message(format!("at most {max} characters")),
        );
    }
}

fn https(at: &str, url: &str, issues: &mut Vec<Issue>) {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("https://")
        || url.len() > MAX_URL_BYTES
        || url.contains(char::is_whitespace)
    {
        issues.push(Issue::new(code::INVALID_URL, at).with_message("an https URL without spaces"));
    }
}

/// Check an SMS content: the text and sender id, and warn above ten segments.
pub fn validate_sms(sms: &SmsContent) -> Vec<Issue> {
    let mut issues = Vec::new();
    length("/sms/text", &sms.text, MAX_SMS_CHARS, &mut issues);
    length(
        "/sms/sender_id",
        &sms.sender_id,
        MAX_SMS_SENDER_CHARS,
        &mut issues,
    );
    let size = sms_segments(&sms.text);
    if size.segments > SMS_SEGMENT_WARNING {
        issues.push(
            Issue::new(code::SMS_MANY_SEGMENTS, "/sms/text").with_message(format!(
                "{} segments; a message over {SMS_SEGMENT_WARNING} may be refused or costly",
                size.segments
            )),
        );
    }
    issues
}

/// Check a push content.
pub fn validate_push(push: &PushContent) -> Vec<Issue> {
    let mut issues = Vec::new();
    length(
        "/push/title",
        &push.title,
        MAX_PUSH_TITLE_CHARS,
        &mut issues,
    );
    length("/push/body", &push.body, MAX_PUSH_BODY_CHARS, &mut issues);
    if let Some(image) = &push.image {
        https("/push/image", image, &mut issues);
    }
    if let Some(link) = &push.deep_link
        && (link.trim().is_empty()
            || link.len() > MAX_URL_BYTES
            || link.contains(|c: char| c.is_whitespace() || c.is_control()))
    {
        issues.push(
            Issue::new(code::INVALID_URL, "/push/deep_link")
                .with_message("a link without spaces, at most 2,048 bytes"),
        );
    }
    let data: usize = push.data.iter().map(|(k, v)| k.len() + v.len()).sum();
    if data > MAX_PUSH_DATA_BYTES {
        issues.push(
            Issue::new(code::CONTENT_TOO_LARGE, "/push/data")
                .with_message(format!("at most {MAX_PUSH_DATA_BYTES} bytes")),
        );
    }
    issues
}

/// Check an in-app content.
pub fn validate_in_app(message: &InAppContent) -> Vec<Issue> {
    let mut issues = Vec::new();
    length(
        "/in_app/title",
        &message.title,
        MAX_IN_APP_TITLE_CHARS,
        &mut issues,
    );
    length(
        "/in_app/body",
        &message.body,
        MAX_IN_APP_BODY_CHARS,
        &mut issues,
    );
    if let Some(image) = &message.image {
        https("/in_app/image", image, &mut issues);
    }
    if let Some(url) = &message.cta_url {
        https("/in_app/cta_url", url, &mut issues);
    }
    if let Some(label) = &message.cta_label {
        length("/in_app/cta_label", label, 100, &mut issues);
    }
    if message.cta_label.is_some() != message.cta_url.is_some() {
        issues.push(
            Issue::new(code::MISSING_CONTENT, "/in_app/cta_url")
                .with_message("a call to action has a label and a URL"),
        );
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(issues: &[Issue]) -> Vec<(&'static str, &str)> {
        issues
            .iter()
            .map(|issue| (issue.code, issue.at.as_str()))
            .collect()
    }

    #[test]
    fn gsm_text_counts_septets_and_the_extension_table_costs_two() {
        let size = sms_segments(&"a".repeat(160));
        assert_eq!(
            (size.encoding, size.units, size.segments),
            (SmsEncoding::Gsm7, 160, 1)
        );
        assert_eq!(sms_segments(&"a".repeat(161)).segments, 2);
        assert_eq!(sms_segments(&"a".repeat(306)).segments, 2);
        assert_eq!(sms_segments(&"a".repeat(307)).segments, 3);
        // Eighty euro signs are 160 septets; a 81st makes two segments.
        assert_eq!(sms_segments(&"€".repeat(80)).segments, 1);
        assert_eq!(sms_segments(&"€".repeat(81)).segments, 2);
        assert_eq!(sms_segments("").segments, 0);
    }

    #[test]
    fn any_other_character_makes_the_text_ucs2() {
        let size = sms_segments(&format!("{}{}", "a".repeat(69), "\u{1f600}"));
        // 69 units plus a surrogate pair is 71 units.
        assert_eq!(
            (size.encoding, size.units, size.segments),
            (SmsEncoding::Ucs2, 71, 2)
        );
        assert_eq!(sms_segments(&"é".repeat(70)).encoding, SmsEncoding::Gsm7);
        assert_eq!(sms_segments(&"ç".repeat(70)).segments, 1);
        assert_eq!(sms_segments(&"ç".repeat(71)).segments, 2);
        assert_eq!(sms_segments(&"ç".repeat(135)).segments, 3);
    }

    #[test]
    fn an_sms_is_checked_and_warns_above_ten_segments() {
        let ok = SmsContent {
            text: "Hello".into(),
            sender_id: "Shop".into(),
        };
        assert_eq!(validate_sms(&ok), Vec::new());
        let empty = SmsContent {
            text: " ".into(),
            sender_id: "x".repeat(16),
        };
        assert_eq!(
            codes(&validate_sms(&empty)),
            [
                ("missing_content", "/sms/text"),
                ("content_too_large", "/sms/sender_id")
            ]
        );
        let long = SmsContent {
            text: "a".repeat(MAX_SMS_CHARS),
            sender_id: "Shop".into(),
        };
        let issues = validate_sms(&long);
        assert_eq!(codes(&issues), [("sms_many_segments", "/sms/text")]);
        assert!(!issues[0].is_blocking());
        let over = SmsContent {
            text: "a".repeat(MAX_SMS_CHARS + 1),
            sender_id: "Shop".into(),
        };
        assert_eq!(validate_sms(&over)[0].code, "content_too_large");
    }

    #[test]
    fn a_push_is_checked_by_length_url_and_data_size() {
        let mut push = PushContent {
            title: "t".repeat(65),
            body: "b".repeat(240),
            image: Some("https://img.example/a.png".into()),
            deep_link: Some("app://orders/1".into()),
            data: BTreeMap::from([("k".into(), "v".into())]),
        };
        assert_eq!(validate_push(&push), Vec::new());
        push.title.push('t');
        push.body.push('b');
        push.image = Some("http://img.example/a.png".into());
        push.deep_link = Some("a b".into());
        push.data
            .insert("big".into(), "x".repeat(MAX_PUSH_DATA_BYTES));
        assert_eq!(
            codes(&validate_push(&push)),
            [
                ("content_too_large", "/push/title"),
                ("content_too_large", "/push/body"),
                ("invalid_url", "/push/image"),
                ("invalid_url", "/push/deep_link"),
                ("content_too_large", "/push/data"),
            ]
        );
        push.title.clear();
        assert_eq!(validate_push(&push)[0].code, "missing_content");
    }

    #[test]
    fn an_in_app_message_is_checked_and_a_call_to_action_needs_both_parts() {
        let mut message = InAppContent {
            title: "t".repeat(100),
            body: "b".repeat(1000),
            image: None,
            cta_label: Some("Open".into()),
            cta_url: Some("https://shop.example/".into()),
        };
        assert_eq!(validate_in_app(&message), Vec::new());
        message.title.push('t');
        message.body.push('b');
        message.cta_url = None;
        assert_eq!(
            codes(&validate_in_app(&message)),
            [
                ("content_too_large", "/in_app/title"),
                ("content_too_large", "/in_app/body"),
                ("missing_content", "/in_app/cta_url"),
            ]
        );
    }

    #[test]
    fn an_unknown_field_is_refused() {
        assert!(
            serde_json::from_str::<SmsContent>("{\"text\":\"a\",\"sender_id\":\"b\",\"x\":1}")
                .is_err()
        );
        assert!(serde_json::from_str::<PushContent>("{\"title\":\"a\",\"body\":\"b\"}").is_ok());
    }
}
