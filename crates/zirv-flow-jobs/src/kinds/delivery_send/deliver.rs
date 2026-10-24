//! Gate 9 of the send pipeline: the call to the destination (architecture 10.1).
//!
//! The credential is opened right before the call, the message id goes as the idempotency key, and
//! the answer is judged by the verdict code every outbound job shares. Nothing here holds a
//! database lock: the caller read everything it needs first and writes the outcome after.

use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{
    DestinationContext, DkimSigner, ErasedDestination, Outbound,
};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_core::{SecretId, SecretPurpose, WorkspaceId};
use zirv_flow_store::secrets::OpenError;
use zirv_flow_store::{Destination, VerifiedSender, find_sending_domain};

use crate::delivery::{self, Attempt, Verdict, credential};
use crate::{JobContext, JobError};

/// One email to hand to a destination.
pub(super) struct Call<'a> {
    pub(super) workspace: WorkspaceId,
    pub(super) destination: &'a Destination,
    pub(super) driver: &'static dyn ErasedDestination,
    /// The sender identity on its verified domain: whose DKIM key signs the message.
    pub(super) sender: &'a VerifiedSender,
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
    let credential =
        match credential::open(ctx, call.workspace, call.destination.credential_id).await? {
            Ok(credential) => credential,
            Err(verdict) => {
                return Ok(Attempt {
                    verdict,
                    elapsed: Duration::ZERO,
                });
            }
        };
    // The sender domain's key is opened only for a destination type that signs.
    let signer = if call.driver.spec().capabilities.signs_dkim {
        match open_signer(ctx, call.workspace, call.sender).await? {
            Ok(signer) => Some(signer),
            Err(verdict) => {
                return Ok(Attempt {
                    verdict,
                    elapsed: Duration::ZERO,
                });
            }
        }
    } else {
        None
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
        dkim: signer.as_ref(),
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

/// The sender domain's DKIM key, opened from its sealed secret and parsed. A key that cannot be
/// opened (no master key, a wrong one, the domain or its secret gone, or bytes that are not an RSA
/// key) is the same retryable verdict as an unopenable credential.
async fn open_signer(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    sender: &VerifiedSender,
) -> Result<Result<DkimSigner, Verdict>, JobError> {
    let Some(domain) = find_sending_domain(ctx.conn(), workspace, sender.domain_id).await? else {
        return Ok(Err(credential::unavailable()));
    };
    let Some(keyring) = ctx.keyring().cloned() else {
        return Ok(Err(credential::unavailable()));
    };
    let secret = SecretId::from_uuid(domain.dkim_secret_id);
    match zirv_flow_store::secrets::open_json(
        ctx.conn(),
        &keyring,
        workspace,
        secret,
        SecretPurpose::DkimKey,
    )
    .await
    {
        Ok(key) => Ok(
            DkimSigner::from_pkcs8(&domain.domain, &domain.selector, key.expose())
                .map_err(|_| credential::unavailable()),
        ),
        Err(OpenError::Store(error)) => Err(error.into()),
        Err(OpenError::NotFound | OpenError::Unopenable) => Ok(Err(credential::unavailable())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_or_credential_that_cannot_be_opened_is_the_shared_retryable_verdict() {
        assert!(matches!(
            credential::unavailable(),
            Verdict::Retry {
                status: None,
                reason: "credential_unavailable",
                ..
            }
        ));
    }
}
