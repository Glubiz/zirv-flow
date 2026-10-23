//! The gates of the send pipeline that read the person and the workspace (architecture 10.1):
//! the sender, the address, consent and suppression, and the two that #221 fills.
//!
//! Each returns the gate's [`GateRecord`] and, when the gate stops the message, the code it is
//! skipped with. None of them takes a row lock that outlives the call, so nothing is held while
//! the message is rendered and delivered: the sender and consent checks run in transactions of
//! their own on the worker's pool, and the rest are plain reads on the job's connection.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::HmacKey;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::Channel;
use zirv_flow_core::people::{Identifier, identifier_hash};
use zirv_flow_store::{
    GateRecord, PgPool, ReadOutcome, SenderError, SuppressionScope, VerifiedSender, begin,
    consent_read, read_send_address, require_verified_sender, suppressing_scope,
};

use super::payload::SendJob;
use crate::JobError;

/// The code of a message the sender gate stopped.
pub(super) const SENDER_UNVERIFIED: &str = "sender_unverified";
/// The code of a message the address gate stopped.
pub(super) const NO_ADDRESS: &str = "no_address";
/// The code of a message the consent gate stopped.
pub(super) const NO_CONSENT: &str = "no_consent";
/// The code of a message the suppression gate stopped.
pub(super) const SUPPRESSED: &str = "suppressed";

/// A gate that stopped the message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Blocked {
    /// What the gate recorded; its reason is the code the message is skipped with.
    pub(super) gate: GateRecord,
    /// What decided, for the message log: `recorded_consent`, `topic_default`, a suppression's
    /// scope, or none.
    pub(super) rule: Option<&'static str>,
}

impl Blocked {
    fn new(gate: &'static str, reason: &'static str) -> Self {
        Self {
            gate: GateRecord::blocked(gate, reason),
            rule: None,
        }
    }
}

/// What a gate that let the message through leaves for the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Passed {
    pub(super) gate: GateRecord,
    pub(super) rule: Option<&'static str>,
}

/// The sender identity the template's version names, when it is live and its domain is verified.
///
/// The check runs in a transaction of its own that ends here: [`require_verified_sender`] locks
/// the identity and the domain until the transaction ends, and a lock held across the delivery
/// would stop an operator archiving an identity or a DNS re-check unverifying a domain for as long
/// as the destination takes to answer. A message already on the wire when the domain stops
/// verifying is allowed to complete.
pub(super) async fn sender(
    pool: &PgPool,
    workspace: WorkspaceId,
    identity: Option<Uuid>,
) -> Result<Result<VerifiedSender, Blocked>, JobError> {
    let Some(identity) = identity else {
        return Ok(Err(Blocked::new("sender", SENDER_UNVERIFIED)));
    };
    let mut tx = begin(pool).await?;
    let checked = require_verified_sender(&mut tx, workspace, identity).await;
    drop(tx);
    match checked {
        Ok(sender) => Ok(Ok(sender)),
        Err(SenderError::Unknown | SenderError::Archived | SenderError::Unverified { .. }) => {
            Ok(Err(Blocked::new("sender", SENDER_UNVERIFIED)))
        }
        Err(SenderError::Store(error)) => Err(error.into()),
    }
}

/// The address to send to (gate 2).
pub(super) async fn address(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Result<String, Blocked>, JobError> {
    match read_send_address(connection, workspace, person).await? {
        Some(address) => Ok(Ok(address)),
        None => Ok(Err(Blocked::new("address", NO_ADDRESS))),
    }
}

/// Consent for the message (gate 3), by the recorded consent and the topic's policy.
///
/// A transactional message skips the gate (OQ-11): it is not marketing, and a person's opt-out of
/// marketing does not stop a receipt. It still passes gate 4. A scope the workspace no longer has
/// (a topic or dimension archived since the flow was published) is not consented to either.
pub(super) async fn consent(
    pool: &PgPool,
    job: &SendJob,
    transactional: bool,
) -> Result<Result<Passed, Blocked>, JobError> {
    if transactional {
        return Ok(Ok(Passed {
            gate: GateRecord::passed("consent"),
            rule: Some("transactional"),
        }));
    }
    let read = consent_read(
        pool,
        job.workspace,
        job.person,
        Channel::Email,
        job.topic,
        &job.dimensions,
    )
    .await?;
    let ReadOutcome::Read { verdict, .. } = read else {
        let mut blocked = Blocked::new("consent", NO_CONSENT);
        blocked.rule = Some("unknown_scope");
        return Ok(Err(blocked));
    };
    // A record decided, or the topic's policy default did (opt-in blocks, opt-out allows).
    let rule = if verdict.deciding_change.is_some() {
        "recorded_consent"
    } else {
        "topic_default"
    };
    let mut gate = if verdict.allowed {
        GateRecord::passed("consent")
    } else {
        GateRecord::blocked("consent", NO_CONSENT)
    };
    if let Some(change) = verdict.deciding_change {
        gate = gate.with_detail("change", change);
    }
    Ok(if verdict.allowed {
        Ok(Passed {
            gate,
            rule: Some(rule),
        })
    } else {
        Err(Blocked {
            gate,
            rule: Some(rule),
        })
    })
}

/// Suppression of the address (gate 4), in both scopes, for marketing and transactional mail
/// alike. The address is hashed here and only the hash is read; neither it nor the hash is kept.
pub(super) async fn suppression(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    address: &str,
) -> Result<Result<GateRecord, Blocked>, JobError> {
    let Ok(identifier) = Identifier::parse("email", address) else {
        return Ok(Err(Blocked::new("address", NO_ADDRESS)));
    };
    let hash = identifier_hash(key, &identifier);
    Ok(
        match suppressing_scope(connection, workspace, &hash).await? {
            None => Ok(GateRecord::passed("suppression")),
            Some(scope) => Err(Blocked {
                gate: GateRecord::blocked("suppression", SUPPRESSED),
                rule: Some(match scope {
                    SuppressionScope::Workspace => "suppression_workspace",
                    SuppressionScope::Organization => "suppression_organization",
                }),
            }),
        },
    )
}

/// Gate 5, the frequency cap per person, channel and window. #221 fills it; until then it lets
/// every message through.
pub(super) fn frequency_cap() -> GateRecord {
    GateRecord::passed("frequency_cap")
}

/// Gate 6, quiet hours in the recipient's time zone. #221 fills it; until then it lets every
/// message through.
pub(super) fn quiet_hours() -> GateRecord {
    GateRecord::passed("quiet_hours")
}

/// Whether the category is the transactional one.
pub(super) fn is_transactional(category: &str) -> bool {
    category == "transactional"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_gate_names_the_code_the_message_is_skipped_with() {
        let blocked = Blocked::new("address", NO_ADDRESS);

        assert_eq!(blocked.gate.reason, Some("no_address"));
        assert!(!blocked.gate.passed);
        assert_eq!(blocked.rule, None);
    }

    #[test]
    fn the_stubs_for_the_caps_let_everything_through_under_their_own_names() {
        assert_eq!(frequency_cap(), GateRecord::passed("frequency_cap"));
        assert_eq!(quiet_hours(), GateRecord::passed("quiet_hours"));
    }

    #[test]
    fn only_the_transactional_category_skips_consent() {
        assert!(is_transactional("transactional"));
        assert!(!is_transactional("marketing"));
        assert!(!is_transactional(""));
    }
}
