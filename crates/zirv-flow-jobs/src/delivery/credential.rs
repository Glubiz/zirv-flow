//! Opening a destination's workspace credential, written once for every outbound job.

use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::Destination;
use zirv_flow_store::secrets::OpenError;

use std::time::Duration;

use super::Verdict;
use crate::{JobContext, JobError};

/// What a job opened a credential for, as `credential.used` records it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Usage {
    Send,
    Sync,
    Forward,
    Erase,
}

impl Usage {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Sync => "sync",
            Self::Forward => "forward",
            Self::Erase => "erase",
        }
    }

    /// A send job carries one message, so its use is one row per hour; the rest run once per
    /// batch or run and write a row each.
    pub(crate) const fn once_per(self) -> Option<Duration> {
        match self {
            Self::Send => Some(Duration::from_secs(3600)),
            _ => None,
        }
    }
}

/// The destination's workspace credential, opened from its sealed secret and merged with its
/// non-secret fields; an empty one for a destination with none. A credential that cannot be
/// opened (the worker has no master key, a wrong one, or the credential is gone) is a retryable
/// [`Verdict`], like a transport error.
pub(crate) async fn open_credential(
    ctx: &mut JobContext<'_>,
    workspace: WorkspaceId,
    destination: &Destination,
    usage: Usage,
) -> Result<Result<Credential, Verdict>, JobError> {
    let Some(id) = destination.credential_id else {
        return Ok(Ok(Credential::new()));
    };
    let Some(keyring) = ctx.keyring().cloned() else {
        return Ok(Err(credential_unavailable()));
    };
    match zirv_flow_store::open_credential(ctx.conn(), &keyring, workspace, id).await {
        Ok(opened) => match Credential::from_parts(&opened.secret, &opened.fields) {
            Ok(credential) => {
                ctx.audit_credential_used(workspace, id, usage.as_str(), usage.once_per())
                    .await?;
                Ok(Ok(credential))
            }
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
    fn a_send_collapses_to_one_row_an_hour_and_the_rest_write_one_each() {
        assert_eq!(Usage::Send.once_per(), Some(Duration::from_secs(3600)));
        for usage in [Usage::Sync, Usage::Forward, Usage::Erase] {
            assert_eq!(usage.once_per(), None);
        }
        let names: Vec<&str> = [Usage::Send, Usage::Sync, Usage::Forward, Usage::Erase]
            .iter()
            .map(|usage| usage.as_str())
            .collect();
        assert_eq!(names, ["send", "sync", "forward", "erase"]);
    }

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
