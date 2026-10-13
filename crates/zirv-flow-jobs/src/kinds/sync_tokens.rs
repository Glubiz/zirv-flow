//! The OAuth2 tokens of a source around a sync read (architecture 7.5): loaded and seeded into
//! the source's context, and refreshed through the store so a refresh is serialized across
//! processes. A refresh token that rotates is saved by the store in the same transaction as the
//! refresh; nothing here logs a token.

use std::sync::Arc;

use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_connectors::runtime::{AccessToken, RefreshFuture, RuntimeError, TokenStore};
use zirv_flow_connectors::sources::{Credential, SourceContext, seed_refresh_token};
use zirv_flow_core::{Keyring, SecretString, WorkspaceId};
use zirv_flow_store::{
    PgPool, RefreshError, Source, StoredTokens, load_tokens, refresh_serialized,
};

/// Persists authorization-code refreshes of one source through the store.
struct StoredRefresh {
    pool: PgPool,
    keyring: Arc<Keyring>,
    workspace: WorkspaceId,
    source: Uuid,
}

fn to_stored(token: &AccessToken) -> Result<StoredTokens, RuntimeError> {
    let refresh = token
        .refresh_token()
        .ok_or(RuntimeError::TokenEndpoint { status: None })?;
    Ok(StoredTokens {
        access_token: SecretString::new(token.token().expose().to_owned()),
        refresh_token: SecretString::new(refresh.expose().to_owned()),
        expires_at: token
            .expires_at_unix()
            .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok()),
        binding: token.binding().unwrap_or_default().to_owned(),
    })
}

pub(super) fn from_stored(stored: StoredTokens) -> AccessToken {
    let token = AccessToken::new_unix(
        stored.access_token,
        stored.expires_at.map(OffsetDateTime::unix_timestamp),
    )
    .with_refresh_token(stored.refresh_token);
    if stored.binding.is_empty() {
        token
    } else {
        token.with_binding(stored.binding)
    }
}

impl TokenStore for StoredRefresh {
    fn refresh<'a>(
        &'a self,
        held: &'a SecretString,
        refresh: &'a (dyn Fn(SecretString) -> RefreshFuture<'a> + Send + Sync),
    ) -> RefreshFuture<'a> {
        Box::pin(async move {
            let stored = refresh_serialized(
                &self.pool,
                &self.keyring,
                self.workspace,
                self.source,
                held,
                |current| async move { to_stored(&refresh(current).await?) },
            )
            .await;
            match stored {
                Ok(stored) => Ok(from_stored(stored)),
                Err(RefreshError::Refresh(error)) => Err(error),
                // The store could not lock, open or write the tokens: the refresh did not happen.
                Err(RefreshError::Token(_)) => Err(RuntimeError::TokenEndpoint { status: None }),
            }
        })
    }
}

/// Prepares `context` and `credential` for a source that has connected an account: the stored
/// refresh token goes into the credential under the reserved key, the stored access token seeds
/// the context, and refreshes go through the store. A source with no tokens is left as it is.
/// The error is a static reason for a run failure.
pub(super) async fn prepare(
    pool: &PgPool,
    keyring: Option<&Arc<Keyring>>,
    workspace: WorkspaceId,
    source: &Source,
    credential: &mut Credential,
    context: SourceContext,
) -> Result<SourceContext, &'static str> {
    if source.oauth_secret_id.is_none() {
        return Ok(context);
    }
    let keyring = keyring.ok_or("the worker has no master key to open the account tokens")?;
    let stored = load_tokens(pool, keyring, workspace, source.id)
        .await
        .map_err(|_| "the account tokens could not be opened")?;
    let Some(stored) = stored else {
        return Ok(context);
    };
    // Tokens issued for another token endpoint, client or scopes stay unused: the source reads as
    // not connected until the account is connected again.
    if !seed_refresh_token(
        credential,
        &source.config,
        &stored.refresh_token,
        &stored.binding,
    ) {
        return Ok(context);
    }
    Ok(context
        .with_access_token(from_stored(stored))
        .with_token_store(Arc::new(StoredRefresh {
            pool: pool.clone(),
            keyring: Arc::clone(keyring),
            workspace,
            source: source.id,
        })))
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn stored() -> StoredTokens {
        StoredTokens {
            access_token: SecretString::new("acc".into()),
            refresh_token: SecretString::new("rt".into()),
            expires_at: Some(datetime!(2030-01-01 00:00 UTC)),
            binding: "b".into(),
        }
    }

    #[test]
    fn stored_tokens_survive_the_trip_through_the_runtime_token() {
        let token = from_stored(stored());

        assert_eq!(token.token().expose(), "acc");
        assert_eq!(token.refresh_token().unwrap().expose(), "rt");
        assert_eq!(token.binding(), Some("b"));

        let back = to_stored(&token).unwrap();
        assert_eq!(back.access_token.expose(), "acc");
        assert_eq!(back.refresh_token.expose(), "rt");
        assert_eq!(back.expires_at, Some(datetime!(2030-01-01 00:00 UTC)));
        assert_eq!(back.binding, "b");
    }

    #[test]
    fn a_token_without_a_refresh_token_cannot_be_stored() {
        let token = AccessToken::new(SecretString::new("acc".into()), None);

        assert!(matches!(
            to_stored(&token),
            Err(RuntimeError::TokenEndpoint { status: None })
        ));
    }

    #[test]
    fn a_stored_token_without_a_binding_is_not_seeded_as_bound() {
        let mut tokens = stored();
        tokens.binding = String::new();

        assert_eq!(from_stored(tokens).binding(), None);
    }
}
