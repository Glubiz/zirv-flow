//! The audit action catalogue: every action string an audit row may carry, as `area.verb`.
//!
//! The list is sorted so [`is_known`] can binary-search it. `append_audit` asserts, in debug
//! builds, that its action is listed, so a new action is one line here.

/// Every action, sorted, grouped by area (the text before the dot).
pub const ACTIONS: &[&str] = &[
    "ai.draft.accepted",
    "ai.settings.updated",
    "api_key.created",
    "api_key.revoked",
    "asset.deleted",
    "asset.uploaded",
    "audit.exported",
    "audit.purged",
    "consent.imported",
    "consent.person_set",
    "consent_dimension.created",
    "consent_dimension.updated",
    "consent_dimension.value_added",
    "consent_dimension.value_updated",
    "consent_policy.published",
    "consent_policy.saved",
    "contact.bulk_upserted",
    "contact.created",
    "contact.deleted",
    "contact.updated",
    "contact_definition.updated",
    "credential.archived",
    "credential.created",
    "credential.rotated",
    "credential.tested",
    "credential.updated",
    "credential.used",
    "destination.archived",
    "destination.checked",
    "destination.created",
    "destination.updated",
    "event_policy.updated",
    "event_schema.created",
    "event_schema.draft_saved",
    "event_schema.published",
    "event_schema.updated",
    "events.exported",
    "flow.archived",
    "flow.created",
    "flow.enrollment_retried",
    "flow.enrollment_skipped",
    "flow.enrollments_exited",
    "flow.paused",
    "flow.published",
    "flow.resumed",
    "flow.started",
    "flow.test_run",
    "flow.updated",
    "forwarding.redriven",
    "forwarding.rule_created",
    "forwarding.rule_deleted",
    "forwarding.rule_updated",
    "identity_conflict.identifier_moved",
    "identity_conflict.kept_separate",
    "import.cancelled",
    "import.created",
    "import.rejects_downloaded",
    "import.run_started",
    "ingest_rejects.replayed",
    "job.retried",
    "list.created",
    "list.deleted",
    "list.members_changed",
    "list.updated",
    "membership.removed",
    "membership.set",
    "person.deleted",
    "privacy.erased",
    "privacy.erasure_requested",
    "privacy.erasures_replayed",
    "privacy.export_downloaded",
    "privacy.export_requested",
    "retention.ran",
    "retention.updated",
    "secret.created",
    "secret.deleted",
    "secret.replaced",
    "secrets.master_key_rotated",
    "segment.archived",
    "segment.created",
    "segment.refresh_requested",
    "segment.updated",
    "send_settings.updated",
    "sender_identity.archived",
    "sender_identity.created",
    "sender_identity.updated",
    "sending_domain.created",
    "sending_domain.deleted",
    "sending_domain.verification_requested",
    "session.sign_in_failed",
    "session.signed_in",
    "session.signed_out",
    "snippet.archived",
    "snippet.created",
    "snippet.draft_saved",
    "snippet.published",
    "snippet.updated",
    "source.created",
    "source.deleted",
    "source.discovered",
    "source.oauth_connected",
    "source.oauth_disconnected",
    "source.oauth_failed",
    "source.oauth_started",
    "source.tested",
    "source.updated",
    "stream.configured",
    "suppression.added",
    "suppression.imported",
    "suppression.removed",
    "sync_run.cancelled",
    "sync_run.started",
    "template.archived",
    "template.created",
    "template.draft_converted",
    "template.draft_saved",
    "template.preview_person_added",
    "template.preview_person_removed",
    "template.previewed",
    "template.published",
    "template.test_sent",
    "template.updated",
    "test_send.allowlist_added",
    "test_send.allowlist_removed",
    "theme.archived",
    "theme.created",
    "theme.default_set",
    "theme.draft_saved",
    "theme.published",
    "theme.updated",
    "topic.created",
    "topic.updated",
    "trait.created",
    "trait.indexed",
    "trait.unindexed",
    "trait.updated",
    "user.created",
    "user.identity_linked",
    "user.password_reset",
    "webhook.created",
    "webhook.deleted",
    "webhook.secret_rotated",
    "webhook.updated",
    "webhook_signing.created",
    "webhook_signing.rotated",
    "workspace.created",
    "workspace.privacy_settings_updated",
    "workspace.updated",
    "workspace.utm_updated",
];

/// Keys an audit summary never carries: they name personal data, free-form values or secrets.
const BANNED_KEYS: &[&str] = &[
    "address",
    "email",
    "first_name",
    "full_name",
    "last_name",
    "password",
    "phone",
    "properties",
    "secret",
    "token",
    "traits",
    "value",
    "values",
];

/// Why a summary was refused. The text names the key path or the kind of value, never the value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SummaryIssue {
    /// A key that names personal data or a secret.
    BannedKey(String),
    /// A string value shaped like an email address.
    Email(String),
    /// A string value shaped like a phone number.
    Phone(String),
    /// A string value shaped like a bearer token.
    Token(String),
}

impl std::fmt::Display for SummaryIssue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BannedKey(path) => write!(formatter, "the key at {path} may hold personal data"),
            Self::Email(path) => write!(formatter, "the value at {path} looks like an email"),
            Self::Phone(path) => write!(formatter, "the value at {path} looks like a phone number"),
            Self::Token(path) => write!(formatter, "the value at {path} looks like a token"),
        }
    }
}

impl std::error::Error for SummaryIssue {}

/// Checks that an audit summary holds ids, counts and short enumerated words only (ruling
/// e3c37d2b): no key from the banned list at any depth, and no string that parses as an email
/// address, a phone number or a bearer token.
pub fn validate_summary(summary: &serde_json::Value) -> Result<(), SummaryIssue> {
    walk(summary, "$")
}

fn walk(value: &serde_json::Value, path: &str) -> Result<(), SummaryIssue> {
    use serde_json::Value;

    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                let lower = key.to_ascii_lowercase();
                let here = format!("{path}.{key}");
                if BANNED_KEYS.contains(&lower.as_str()) {
                    return Err(SummaryIssue::BannedKey(here));
                }
                walk(inner, &here)?;
            }
            Ok(())
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .try_for_each(|(index, inner)| walk(inner, &format!("{path}[{index}]"))),
        Value::String(text) => check_text(text, path),
        _ => Ok(()),
    }
}

fn check_text(text: &str, path: &str) -> Result<(), SummaryIssue> {
    let text = text.trim();
    if looks_like_email(text) {
        Err(SummaryIssue::Email(path.to_owned()))
    } else if looks_like_token(text) {
        Err(SummaryIssue::Token(path.to_owned()))
    } else if looks_like_phone(text) {
        Err(SummaryIssue::Phone(path.to_owned()))
    } else {
        Ok(())
    }
}

/// `local@domain.tld` with no whitespace, anywhere in the text.
fn looks_like_email(text: &str) -> bool {
    text.split_whitespace().any(|word| {
        word.rsplit_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        })
    })
}

/// An optional `+`, then seven to fifteen digits with only spaces, dashes, dots and brackets
/// between them. A uuid or a plain count never matches: those have letters, or too few digits.
fn looks_like_phone(text: &str) -> bool {
    let is_date = text.len() == 10
        && text.bytes().enumerate().all(|(index, b)| match index {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        });
    if is_date {
        return false;
    }
    let body = text.strip_prefix('+').unwrap_or(text);
    let digits = body.bytes().filter(u8::is_ascii_digit).count();
    (7..=15).contains(&digits)
        && !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b' ' | b'-' | b'.' | b'(' | b')'))
}

/// A `Bearer ` header value, a JWT (three base64url parts) or a long unbroken run of token
/// characters. A uuid is exempt.
fn looks_like_token(text: &str) -> bool {
    if text.len() >= 7 && text[..7].eq_ignore_ascii_case("bearer ") {
        return true;
    }
    if uuid::Uuid::parse_str(text).is_ok() {
        return false;
    }
    let base64url = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() == 3 && parts.iter().all(|part| base64url(part)) && text.len() >= 20 {
        return true;
    }
    text.len() >= 32
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'+' | b'/' | b'='))
        && text.bytes().any(|b| b.is_ascii_digit())
        && text.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Whether `action` is in the catalogue.
pub fn is_known(action: &str) -> bool {
    ACTIONS.binary_search(&action).is_ok()
}

/// The area of an action: the text before the first dot.
pub fn area(action: &str) -> &str {
    action.split_once('.').map_or(action, |(area, _)| area)
}

/// Whether `action` is a lowercase `area.verb`, each part letters and underscores. One action,
/// `ai.draft.accepted`, predates the rule and has a second dot; a verb may have one sub-verb.
#[cfg(test)]
fn is_area_verb(action: &str) -> bool {
    let part = |text: &str| {
        !text.is_empty()
            && !text.starts_with('_')
            && !text.ends_with('_')
            && text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    };
    action.split_once('.').is_some_and(|(area, verb)| {
        part(area) && verb.split('.').count() <= 2 && verb.split('.').all(part)
    })
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;

    use super::*;

    #[test]
    fn the_catalogue_is_sorted_with_no_duplicates() {
        assert!(ACTIONS.windows(2).all(|pair| pair[0] < pair[1]));
        let distinct: HashSet<_> = ACTIONS.iter().collect();
        assert_eq!(distinct.len(), ACTIONS.len());
    }

    #[test]
    fn every_action_is_an_area_dot_verb() {
        for action in ACTIONS {
            assert!(is_area_verb(action), "{action}");
        }
    }

    #[test]
    fn the_area_verb_rule_refuses_other_shapes() {
        for bad in [
            "",
            "flow",
            "flow.",
            ".created",
            "flow.a.b.c",
            "flow..b",
            "Flow.created",
            "flow.created2",
        ] {
            assert!(!is_area_verb(bad), "{bad}");
        }
    }

    #[test]
    fn known_actions_are_found_and_others_are_not() {
        assert!(is_known("flow.created"));
        assert!(is_known("audit.exported"));
        assert!(!is_known("flow.created "));
        assert!(!is_known("made.up"));
    }

    fn refused(summary: serde_json::Value) -> SummaryIssue {
        validate_summary(&summary).expect_err("refused")
    }

    #[test]
    fn every_banned_key_is_refused_at_any_depth() {
        for key in BANNED_KEYS {
            assert!(matches!(
                refused(serde_json::json!({ *key: 1 })),
                SummaryIssue::BannedKey(_)
            ));
            assert!(matches!(
                refused(serde_json::json!({"a": [{"b": {key.to_uppercase(): true}}]})),
                SummaryIssue::BannedKey(_)
            ));
        }
    }

    #[test]
    fn an_email_a_phone_and_a_token_are_refused() {
        assert!(matches!(
            refused(serde_json::json!({"note": "ada@example.com"})),
            SummaryIssue::Email(_)
        ));
        assert!(matches!(
            refused(serde_json::json!({"note": "mail ada@example.com now"})),
            SummaryIssue::Email(_)
        ));
        assert!(matches!(
            refused(serde_json::json!({"note": "a@b@example.com"})),
            SummaryIssue::Email(_)
        ));
        for phone in ["+4512345678", "12 34 56 78", "(555) 123-4567"] {
            assert!(matches!(
                refused(serde_json::json!({ "note": phone })),
                SummaryIssue::Phone(_)
            ));
        }
        for token in [
            "Bearer abc",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
            "zf_live_9fA3kLm2Qx7Rt1Vb8Yc4Dd6Ee0Ff5Gh2",
        ] {
            assert!(matches!(
                refused(serde_json::json!({ "note": token })),
                SummaryIssue::Token(_)
            ));
        }
    }

    #[test]
    fn counts_ids_dates_and_short_words_pass() {
        let id = uuid::Uuid::now_v7().to_string();
        assert!(
            validate_summary(&serde_json::json!({
                "count": 12,
                "ok": true,
                "none": null,
                "list_id": id,
                "ids": [id, id],
                "state": "opted_in",
                "name": "Welcome series",
                "cutoff": "2030-01-01",
                "at": "2030-01-01T00:00:00Z",
                "nested": {"added": 3, "kind": "send"},
            }))
            .is_ok()
        );
    }

    #[test]
    fn an_issue_names_the_path_and_never_the_value() {
        let issue = refused(serde_json::json!({"a": [{"note": "ada@example.com"}]}));
        let text = issue.to_string();
        assert!(text.contains("$.a[0].note"), "{text}");
        assert!(!text.contains("ada"), "{text}");
    }

    #[test]
    fn the_area_is_the_text_before_the_first_dot() {
        assert_eq!(area("flow.created"), "flow");
        assert_eq!(area("identity_conflict.kept_separate"), "identity_conflict");
        assert_eq!(area("bare"), "bare");
    }
}
