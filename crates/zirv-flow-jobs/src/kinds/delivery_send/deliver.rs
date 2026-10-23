//! Gate 9 of the send pipeline: the call to the destination (architecture 10.1).
//!
//! The credential is opened right before the call, the message id goes as the idempotency key, and
//! the answer is judged by the verdict code every outbound job shares. Nothing here holds a
//! database lock: the caller read everything it needs first and writes the outcome after.

use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{DestinationContext, ErasedDestination, Outbound};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::{SecretId, SecretPurpose, WorkspaceId};
use zirv_flow_store::Destination;
use zirv_flow_store::secrets::{self, OpenError};

use crate::delivery::{self, Attempt, Verdict};
use crate::{JobContext, JobError};

/// One email to hand to a destination.
pub(super) struct Call<'a> {
    pub(super) workspace: WorkspaceId,
    pub(super) destination: &'a Destination,
    pub(super) driver: &'static dyn ErasedDestination,
    /// The message id: the idempotency key at the destination.
    pub(super) message: Uuid,
    pub(super) address: &'a str,
    pub(super) payload: &'a Value,
}

/// Delivers one email and judges the destination's answer. An error is a failure of this worker or
/// of the database; the destination's answer, and a credential that cannot be opened, are a
/// [`Verdict`].
pub(super) async fn deliver(
    ctx: &mut JobContext<'_>,
    call: &Call<'_>,
    now: OffsetDateTime,
) -> Result<Attempt, JobError> {
    let credential = match open_credential(ctx, call.workspace, call.destination).await? {
        Ok(credential) => credential,
        Err(verdict) => {
            return Ok(Attempt {
                verdict,
                elapsed: Duration::ZERO,
            });
        }
    };
    // No redirects: a redirect would turn the call into something that was never the message.
    let limits = HttpLimits {
        max_redirects: 0,
        ..HttpLimits::default()
    };
    let context = DestinationContext::new(
        ctx.egress().clone(),
        limits,
        call.driver.spec().kind,
        call.destination.id.to_string(),
    )
    .map_err(|_| JobError::new("the outbound client could not be built"))?;
    let key = call.message.to_string();
    let outbound = Outbound {
        idempotency_key: &key,
        channel: "email",
        to: call.address,
        payload: call.payload,
    };
    Ok(delivery::send(
        call.driver,
        &context,
        &call.destination.config,
        &credential,
        &outbound,
        now,
    )
    .await)
}

/// The destination's credential, opened from its sealed secret; an empty one for a destination
/// with none. A secret that cannot be opened (the worker has no master key, or a wrong one) is
/// retried like a transport error and, on the last attempt, recorded as the message's failure.
async fn open_credential(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    destination: &Destination,
) -> Result<Result<Credential, Verdict>, JobError> {
    let Some(secret) = destination.credential_secret_id else {
        return Ok(Ok(Credential::new()));
    };
    let Some(keyring) = ctx.keyring().cloned() else {
        return Ok(Err(credential_unavailable()));
    };
    match secrets::open_json(
        ctx.conn(),
        &keyring,
        workspace,
        SecretId::from_uuid(secret),
        SecretPurpose::DestinationCredential,
    )
    .await
    {
        Ok(bytes) => match Credential::from_json(&bytes) {
            Ok(credential) => Ok(Ok(credential)),
            Err(_) => Ok(Err(credential_unavailable())),
        },
        Err(OpenError::Store(error)) => Err(error.into()),
        Err(OpenError::NotFound | OpenError::Unopenable) => Ok(Err(credential_unavailable())),
    }
}

fn credential_unavailable() -> Verdict {
    Verdict::Retry {
        status: None,
        reason: "credential_unavailable",
        message: "the destination credential could not be opened",
        retry_after: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unopenable_credential_is_retried_under_a_fixed_reason() {
        assert!(matches!(
            credential_unavailable(),
            Verdict::Retry {
                status: None,
                reason: "credential_unavailable",
                retry_after: None,
                ..
            }
        ));
    }
}
