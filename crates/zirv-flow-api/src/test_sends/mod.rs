//! Template test sends and their log (architecture 11.7, #224).
//!
//! `POST /templates/{id}/test-send` reserves a slot under the rate limit, writes the `queued` log
//! row and enqueues `template.test_send` in one transaction; the job renders and sends. A test goes
//! only to a workspace member, at the address on their account, or to an address an admin put on the
//! workspace allowlist. The log and the allowlist hold keyed hashes, never addresses.
//!
//! Permissions: a test send needs `Write` (`Admin` when a real person's values render it, as a
//! preview does until the `view_pii` grant exists), the log needs `Read`, and the allowlist needs
//! `Admin`. A caller with no access gets 404.

pub(crate) mod allowlist;
pub(crate) mod send;

pub(crate) use allowlist::{
    AllowlistAddRequest, AllowlistBody, AllowlistEntryBody, AllowlistOptionBody, allowlist_add,
    allowlist_delete, allowlist_list, allowlist_options,
};
pub(crate) use send::{
    QuotaBody, QuotaUsageBody, TestSendRecipient, TestSendRequest, TestSendsBody, list, quota,
    test_send,
};

use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::ListedTestSend;

/// A test send as the log shows it. It holds no address, no hash and no person.
#[derive(Serialize, ToSchema)]
pub(crate) struct TestSendBody {
    /// Also the idempotency key at the destination.
    id: Uuid,
    template_id: Uuid,
    /// The published version sent; null for the draft.
    template_version: Option<i32>,
    /// The draft revision a draft test rendered; null for a version test.
    draft_revision: Option<i32>,
    /// The user who asked; null for an API key.
    member_id: Option<Uuid>,
    /// The email of the member who asked; null for an API key or a removed member.
    sent_by_email: Option<String>,
    destination_id: Uuid,
    sender_identity_id: Uuid,
    /// The workspace member the test went to; null for an allowlisted address.
    recipient_member_id: Option<Uuid>,
    /// A member's email, or the hint of the allowlist entry the test went to; `removed address`
    /// once the entry is gone. Never an allowlisted address.
    recipient_display: String,
    /// `person` when a person's values rendered the template, else `sample`.
    #[schema(example = "sample")]
    render_context: String,
    /// `queued`, `sent`, `suppressed`, `sender_unverified`, `render_failed`, `unconfirmed` or
    /// `failed`.
    #[schema(example = "queued")]
    outcome: String,
    /// A fixed code that says why a test did not go out.
    reason: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    completed_at: Option<OffsetDateTime>,
}

impl From<ListedTestSend> for TestSendBody {
    fn from(listed: ListedTestSend) -> Self {
        let ListedTestSend {
            send,
            recipient_display,
            sent_by_email,
        } = listed;
        Self {
            id: send.id,
            template_id: send.template_id,
            template_version: send.template_version,
            draft_revision: send.draft_revision,
            member_id: send.member_id,
            sent_by_email,
            destination_id: send.destination_id,
            sender_identity_id: send.sender_identity_id,
            recipient_member_id: send.recipient_member_id,
            recipient_display,
            render_context: send.render_context,
            outcome: send.outcome.as_str().to_owned(),
            reason: send.reason,
            created_at: send.created_at,
            completed_at: send.completed_at,
        }
    }
}

#[cfg(test)]
mod acceptance;

#[cfg(test)]
mod tests {
    use zirv_flow_store::{TestSend, TestSendOutcome};

    use super::*;

    #[test]
    fn a_log_row_is_shown_with_its_outcome_and_without_addresses() {
        let body = TestSendBody::from(ListedTestSend {
            recipient_display: "removed address".into(),
            sent_by_email: None,
            send: TestSend {
                id: Uuid::nil(),
                template_id: Uuid::nil(),
                template_version: None,
                member_id: None,
                destination_id: Uuid::nil(),
                sender_identity_id: Uuid::nil(),
                recipient_hash: vec![9; 32],
                recipient_member_id: None,
                render_context: "sample".into(),
                outcome: TestSendOutcome::Suppressed,
                reason: Some("suppressed".into()),
                created_at: OffsetDateTime::UNIX_EPOCH,
                completed_at: None,
                draft_revision: None,
            },
        });

        let json = serde_json::to_value(&body).unwrap();

        assert_eq!(json["outcome"], "suppressed");
        assert_eq!(json["reason"], "suppressed");
        assert!(json.get("recipient_hash").is_none() && json.get("address").is_none());
    }
}
