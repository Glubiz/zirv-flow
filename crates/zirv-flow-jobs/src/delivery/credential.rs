//! Opening a destination's workspace credential, written once for every outbound job.

use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::Destination;
use zirv_flow_store::secrets::OpenError;

use super::Verdict;
use crate::{JobContext, JobError};

/// The destination's workspace credential, opened from its sealed secret and merged with its
/// non-secret fields; an empty one for a destination with none. A credential that cannot be
/// opened (the worker has no master key, a wrong one, or the credential is gone) is a retryable
/// [`Verdict`], like a transport error.
pub(crate) async fn open_credential(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    destination: &Destination,
) -> Result<Result<Credential, Verdict>, JobError> {
    let Some(id) = destination.credential_id else {
        return Ok(Ok(Credential::new()));
    };
    let Some(keyring) = ctx.keyring().cloned() else {
        return Ok(Err(credential_unavailable()));
    };
    match zirv_flow_store::open_credential(ctx.conn(), &keyring, workspace, id).await {
        Ok(opened) => match Credential::from_parts(&opened.secret, &opened.fields) {
            Ok(credential) => Ok(Ok(credential)),
            Err(_) => Ok(Err(credential_unavailable())),
        },
        Err(OpenError::Store(error)) => Err(error.into()),
        Err(OpenError::NotFound | OpenError::Unopenable) => Ok(Err(credential_unavailable())),
    }
}

pub(crate) fn credential_unavailable() -> Verdict {
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
    fn an_unopenable_credential_is_a_retryable_verdict_with_a_fixed_reason() {
        assert!(matches!(
            credential_unavailable(),
            Verdict::Retry {
                reason: "credential_unavailable",
                ..
            }
        ));
    }
}
