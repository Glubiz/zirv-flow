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
    "consent.imported",
    "consent_dimension.created",
    "consent_dimension.updated",
    "consent_dimension.value_added",
    "consent_dimension.value_updated",
    "consent_policy.published",
    "consent_policy.saved",
    "contact.bulk_upserted",
    "contact_definition.updated",
    "credential.archived",
    "credential.created",
    "credential.rotated",
    "credential.tested",
    "credential.updated",
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
    "flow.enrollments_exited",
    "flow.paused",
    "flow.published",
    "flow.resumed",
    "flow.started",
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
    "list.updated",
    "membership.removed",
    "membership.set",
    "person.deleted",
    "privacy.erased",
    "privacy.erasure_requested",
    "privacy.erasures_replayed",
    "privacy.export_downloaded",
    "privacy.export_requested",
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

    #[test]
    fn the_area_is_the_text_before_the_first_dot() {
        assert_eq!(area("flow.created"), "flow");
        assert_eq!(area("identity_conflict.kept_separate"), "identity_conflict");
        assert_eq!(area("bare"), "bare");
    }
}
