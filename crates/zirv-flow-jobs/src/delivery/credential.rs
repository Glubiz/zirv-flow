//! A destination's credential, opened for one call: the code every outbound job that delivers
//! through a destination row shares.

use uuid::Uuid;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::secrets::OpenError;

use super::Verdict;
use crate::{JobContext, JobError};

/// The destination's workspace credential, opened from its sealed secret and merged with its
/// non-secret fields; an empty one for a destination with none. A credential that cannot be
/// opened (the worker has no master key, a wrong one, or the credential is gone) is retried like
/// a transport error and, on the last attempt, recorded as the delivery's failure.
pub(crate) async fn open(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    credential: Option<Uuid>,
) -> Result<Result<Credential, Verdict>, JobError> {
    let Some(id) = credential else {
        return Ok(Ok(Credential::new()));
    };
    let Some(keyring) = ctx.keyring().cloned() else {
        return Ok(Err(unavailable()));
    };
    match zirv_flow_store::open_credential(ctx.conn(), &keyring, workspace, id).await {
        Ok(opened) => match Credential::from_parts(&opened.secret, &opened.fields) {
            Ok(credential) => Ok(Ok(credential)),
            Err(_) => Ok(Err(unavailable())),
        },
        Err(OpenError::Store(error)) => Err(error.into()),
        Err(OpenError::NotFound | OpenError::Unopenable) => Ok(Err(unavailable())),
    }
}

/// The verdict of a credential that cannot be opened.
pub(crate) fn unavailable() -> Verdict {
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
            unavailable(),
            Verdict::Retry {
                status: None,
                reason: "credential_unavailable",
                retry_after: None,
                ..
            }
        ));
    }
}
