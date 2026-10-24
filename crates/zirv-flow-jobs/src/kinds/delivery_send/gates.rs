//! The gates of the send pipeline that read the person and the workspace (architecture 10.1):
//! the sender, the address, consent and suppression, and the two that #221 fills.
//!
//! Each returns the gate's [`GateRecord`] and, when the gate stops the message, the code it is
//! skipped with. None of them takes a row lock that outlives the call, so nothing is held while
//! the message is rendered and delivered: the sender and consent checks run in transactions of
//! their own on the worker's pool, and the rest are plain reads on the job's connection.

use sqlx::PgConnection;
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_core::HmacKey;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::Channel;
use zirv_flow_core::people::{Identifier, identifier_hash};
use zirv_flow_store::{
    CapVerdict, Clock, GateRecord, PgPool, ReadOutcome, SenderError, SuppressionScope,
    VerifiedSender, begin, check_frequency_caps, consent_read, read_send_address, read_send_limits,
    require_verified_sender, step_zones, suppressing_scope,
};

use super::payload::SendJob;
use super::{policy, quiet};
use crate::JobError;

/// The code of a message the sender gate stopped.
pub(super) const SENDER_UNVERIFIED: &str = "sender_unverified";
/// The code of a message the address gate stopped.
pub(super) const NO_ADDRESS: &str = "no_address";
/// The code of a message the consent gate stopped.
pub(super) const NO_CONSENT: &str = "no_consent";
/// The note on a consent gate that a published rule that no longer compiles stopped.
pub(super) const POLICY_RULE_UNEVALUABLE: &str = "policy_rule_unevaluable";
/// The code of a message the suppression gate stopped.
pub(super) const SUPPRESSED: &str = "suppressed";

/// A gate that stopped the message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Blocked {
    /// What the gate recorded; its reason is the code the message is skipped with.
    pub(super) gate: GateRecord,
    /// What decided, for the message log: `recorded_consent`, `topic_default`, the name of a
    /// consent policy rule, a suppression's scope, or none.
    pub(super) rule: Option<String>,
}

impl Blocked {
    pub(super) fn new(gate: &'static str, reason: &'static str) -> Self {
        Self {
            gate: GateRecord::blocked(gate, reason),
            rule: None,
        }
    }
}

/// What a gate that let the message through leaves for the log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Passed {
    pub(super) gate: GateRecord,
    pub(super) rule: Option<String>,
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
    Ok(read_send_address(connection, workspace, person)
        .await?
        .ok_or_else(|| Blocked::new("address", NO_ADDRESS)))
}

/// Consent for the message (gate 3), by the recorded consent and the topic's policy.
///
/// A transactional message skips the gate (OQ-11): it is not marketing, and a person's opt-out of
/// marketing does not stop a receipt. It still passes gate 4. A scope the workspace no longer has
/// (a topic or dimension archived since the flow was published) is not consented to either.
pub(super) async fn consent(
    pool: &PgPool,
    clock: &Clock,
    job: &SendJob,
    transactional: bool,
) -> Result<Result<Passed, Blocked>, JobError> {
    if transactional {
        return Ok(Ok(Passed {
            gate: GateRecord::passed("consent"),
            rule: Some("transactional".to_owned()),
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
    let ReadOutcome::Read {
        verdict, recorded, ..
    } = read
    else {
        let mut blocked = Blocked::new("consent", NO_CONSENT);
        blocked.rule = Some("unknown_scope".to_owned());
        return Ok(Err(blocked));
    };
    // The workspace's published consent policy decides, and with none the recorded consent does.
    let denial = policy::denial(
        pool,
        clock,
        job.workspace,
        job.person,
        &job.dimensions,
        recorded,
        !verdict.allowed,
    )
    .await?;
    if let Some(denial) = denial.filter(|denial| !denial.is_builtin()) {
        // A rule of the policy denied the send: the message names it by name and the gate result
        // by id.
        let mut gate = GateRecord::blocked("consent", NO_CONSENT);
        if let Ok(id) = denial.id.parse::<Uuid>() {
            gate = gate.with_detail("rule", id);
        }
        if denial.unevaluable {
            // The rule did not match; it could not be read. Say so apart from a plain denial.
            warn!(
                workspace = %job.workspace.as_uuid(),
                rule = %denial.id,
                "a published consent rule cannot be evaluated; the send is denied"
            );
            gate = gate.with_note(POLICY_RULE_UNEVALUABLE);
        }
        return Ok(Err(Blocked {
            gate,
            rule: Some(denial.name),
        }));
    }
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
            rule: Some(rule.to_owned()),
        })
    } else {
        Err(Blocked {
            gate,
            rule: Some(rule.to_owned()),
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
                rule: Some(
                    match scope {
                        SuppressionScope::Workspace => "suppression_workspace",
                        SuppressionScope::Organization => "suppression_organization",
                    }
                    .to_owned(),
                ),
            }),
        },
    )
}

/// The suppression hash of `address`, as the gate takes it; none for text that is not an email
/// address.
pub(super) fn address_hash(key: &HmacKey, address: &str) -> Option<[u8; 32]> {
    let identifier = Identifier::parse("email", address).ok()?;
    Some(identifier_hash(key, &identifier))
}

/// The code of a message the frequency cap gate stopped.
pub(super) const FREQUENCY_CAP: &str = "frequency_cap";
/// The note on the quiet-hours gate and its decision when the person's `timezone` trait is not a
/// zone and the workspace's was used.
pub(super) const TIMEZONE_FALLBACK: &str = "timezone_fallback";

/// A gate that holds the message back for later instead of ending it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Deferral {
    /// The gate that deferred.
    pub(super) gate: &'static str,
    /// When the message runs again.
    pub(super) until: OffsetDateTime,
    /// A code that adds to the gate record, such as `timezone_fallback`.
    pub(super) note: Option<&'static str>,
    /// Whether the message gives up the frequency-cap slot it holds: it takes one again when it
    /// really sends.
    pub(super) release_slot: bool,
}

/// What a gate that does not let the message through does with it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Stop {
    /// The message ends skipped.
    Skip(Blocked),
    /// The message runs again later.
    Defer(Deferral),
}

/// Gate 5, the frequency caps of the person on the channel: check and record.
///
/// [`check_frequency_caps`] counts the slots in each cap's window and reserves this message's own
/// in a short transaction of its own, under a per-person lock, so the lock is gone before the
/// message is rendered or delivered.
pub(super) async fn frequency_cap(
    pool: &PgPool,
    job: &SendJob,
    now: OffsetDateTime,
) -> Result<Result<GateRecord, Stop>, JobError> {
    let verdict = check_frequency_caps(
        pool,
        job.workspace,
        job.person,
        job.message,
        Channel::Email.as_str(),
        now,
    )
    .await?;
    Ok(match verdict {
        CapVerdict::Reserved => Ok(GateRecord::passed("frequency_cap")),
        CapVerdict::Skip => Err(Stop::Skip(Blocked::new("frequency_cap", FREQUENCY_CAP))),
        CapVerdict::Defer { until } => Err(Stop::Defer(Deferral {
            gate: "frequency_cap",
            until,
            note: None,
            release_slot: false,
        })),
    })
}

/// Gate 6, quiet hours in the recipient's time zone.
///
/// The zone is the person's `timezone` trait, else the workspace's (also for a trait that is not a
/// zone, which the gate record notes). A message that arrives inside a quiet period waits for its
/// end and gives up its frequency-cap slot meanwhile.
pub(super) async fn quiet_hours(
    connection: &mut PgConnection,
    job: &SendJob,
    now: OffsetDateTime,
) -> Result<Result<GateRecord, Stop>, JobError> {
    let limits = read_send_limits(connection, job.workspace).await?;
    let Some(quiet) = limits.quiet_hours else {
        return Ok(Ok(GateRecord::passed("quiet_hours")));
    };
    let (workspace_zone, person_zone) = step_zones(connection, job.workspace, job.person).await?;
    let (zone, fell_back) = quiet::recipient_zone(&workspace_zone, person_zone.as_deref());
    let note = fell_back.then_some(TIMEZONE_FALLBACK);
    Ok(match quiet::quiet_until(now, zone, &quiet) {
        None => {
            let record = GateRecord::passed("quiet_hours");
            Ok(match note {
                Some(note) => record.with_note(note),
                None => record,
            })
        }
        Some(until) => Err(Stop::Defer(Deferral {
            gate: "quiet_hours",
            until,
            note,
            release_slot: true,
        })),
    })
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
    fn only_the_transactional_category_skips_consent() {
        assert!(is_transactional("transactional"));
        assert!(!is_transactional("marketing"));
        assert!(!is_transactional(""));
    }
}
