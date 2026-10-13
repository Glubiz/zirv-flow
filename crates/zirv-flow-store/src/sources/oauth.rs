//! OAuth2 for a source (architecture 7.5): the consent in progress, and the sealed tokens the
//! consent produced. Every function is scoped to one workspace except [`take_oauth_state`],
//! which is how the callback finds out which workspace a consent was started in.
//!
//! The tokens of a source are one `encrypted_secrets` row of purpose `oauth_token`, linked from
//! `sources.oauth_secret_id`. The plaintext is `{access_token, refresh_token, expires_at,
//! binding}`; it is opened right before use and never logged.

use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize};
use sqlx::prelude::FromRow;
use sqlx::{Acquire, PgConnection, PgExecutor, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{
    Keyring, SecretBytes, SecretId, SecretPurpose, SecretString, WorkspaceId, open, seal,
    secret_aad,
};

use crate::StoreError;
use crate::secrets::{self, SecretWriteError};

/// The sealed tokens of a source. `Debug` shows no token.
pub struct StoredTokens {
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    /// When the access token lapses; `None` when the provider gave no lifetime.
    pub expires_at: Option<OffsetDateTime>,
    /// A fingerprint of the token URL, client id and scopes the tokens were issued for (see
    /// `zirv_flow_connectors::runtime::AccessToken::binding`). Never of a secret.
    pub binding: String,
}

impl fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoredTokens([redacted])")
    }
}

#[derive(Deserialize)]
struct Wire {
    access_token: SecretString,
    refresh_token: SecretString,
    expires_at: Option<i64>,
    binding: String,
}

#[derive(Serialize)]
struct WireOut<'a> {
    access_token: &'a str,
    refresh_token: &'a str,
    expires_at: Option<i64>,
    binding: &'a str,
}

impl StoredTokens {
    /// The sealed form: the JSON the secret row holds. For the consent, which creates the row.
    pub fn to_plaintext(&self) -> Result<SecretBytes, TokenError> {
        serde_json::to_vec(&WireOut {
            access_token: self.access_token.expose(),
            refresh_token: self.refresh_token.expose(),
            expires_at: self.expires_at.map(OffsetDateTime::unix_timestamp),
            binding: &self.binding,
        })
        .map(SecretBytes::new)
        .map_err(|_| TokenError::Unopenable)
    }

    fn from_plaintext(bytes: &SecretBytes) -> Result<Self, TokenError> {
        let wire: Wire =
            serde_json::from_slice(bytes.expose()).map_err(|_| TokenError::Unopenable)?;
        Ok(Self {
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            expires_at: wire
                .expires_at
                .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok()),
            binding: wire.binding,
        })
    }
}

/// Why tokens could not be read or written.
#[derive(Debug)]
pub enum TokenError {
    /// The source has no token secret (no account connected, or the source is gone).
    NotConnected,
    /// The keyring does not open the secret, or its content is not tokens.
    Unopenable,
    /// The writer is refused by the HMAC-root check.
    Write(SecretWriteError),
    Store(StoreError),
}

impl From<StoreError> for TokenError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<SecretWriteError> for TokenError {
    fn from(error: SecretWriteError) -> Self {
        match error {
            SecretWriteError::Store(error) => Self::Store(error),
            other => Self::Write(other),
        }
    }
}

/// Why a serialized refresh failed: the tokens, or the caller's own refresh.
#[derive(Debug)]
pub enum RefreshError<E> {
    Token(TokenError),
    Refresh(E),
}

impl<E> From<TokenError> for RefreshError<E> {
    fn from(error: TokenError) -> Self {
        Self::Token(error)
    }
}

impl<E> From<StoreError> for RefreshError<E> {
    fn from(error: StoreError) -> Self {
        Self::Token(error.into())
    }
}

/// A consent in progress, as the callback finds it.
#[derive(Clone, Eq, PartialEq)]
pub struct OAuthState {
    pub workspace_id: WorkspaceId,
    pub source_id: Uuid,
    pub user_id: Uuid,
    pub pkce_verifier: String,
    /// The token binding of the manifest the consent was started under.
    pub binding: String,
}

impl fmt::Debug for OAuthState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthState")
            .field("workspace_id", &self.workspace_id)
            .field("source_id", &self.source_id)
            .field("user_id", &self.user_id)
            .field("pkce_verifier", &"[redacted]")
            .field("binding", &self.binding)
            .finish()
    }
}

#[derive(FromRow)]
struct StateRow {
    workspace_id: Uuid,
    source_id: Uuid,
    user_id: Uuid,
    pkce_verifier: String,
    binding: String,
}

/// A consent to record: started by `user` for `source`, valid for `ttl_seconds`. `state_hash` is
/// the hash of the single-use `state`; the verifier is kept in clear for that long, as an OIDC
/// login's is. `binding` is the token binding of the manifest the consent was started under, which
/// the callback checks before it sends the code anywhere.
pub struct NewOAuthState<'a> {
    pub workspace: WorkspaceId,
    pub source: Uuid,
    pub user: Uuid,
    pub state_hash: &'a [u8],
    pub pkce_verifier: &'a str,
    pub binding: &'a str,
    pub ttl_seconds: i64,
}

/// Records a consent in progress. A source that is not in the workspace is refused by the
/// database.
pub async fn create_oauth_state<'e>(
    executor: impl PgExecutor<'e>,
    consent: &NewOAuthState<'_>,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO source_oauth_states 
            (state_hash, workspace_id, source_id, user_id, pkce_verifier, binding, expires_at) 
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
    )
    .bind(consent.state_hash)
    .bind(consent.workspace.as_uuid())
    .bind(consent.source)
    .bind(consent.user)
    .bind(consent.pkce_verifier)
    .bind(consent.binding)
    .bind(consent.ttl_seconds as f64)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Takes a consent in progress: it is deleted as it is read, so a state can be used once, and an
/// expired one is not returned. The callback calls this first, so a replay finds nothing and a
/// failure later never leaves a usable state behind.
pub async fn take_oauth_state<'e>(
    executor: impl PgExecutor<'e>,
    state_hash: &[u8],
) -> Result<Option<OAuthState>, StoreError> {
    let row: Option<StateRow> = sqlx::query_as(
        "DELETE FROM source_oauth_states WHERE state_hash = $1 AND expires_at > now() \
         RETURNING workspace_id, source_id, user_id, pkce_verifier, binding",
    )
    .bind(state_hash)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|row| OAuthState {
        workspace_id: WorkspaceId::from_uuid(row.workspace_id),
        source_id: row.source_id,
        user_id: row.user_id,
        pkce_verifier: row.pkce_verifier,
        binding: row.binding,
    }))
}

/// Removes consents that were never completed.
pub async fn delete_expired_oauth_states<'e>(
    executor: impl PgExecutor<'e>,
) -> Result<u64, StoreError> {
    let result = sqlx::query("DELETE FROM source_oauth_states WHERE expires_at <= now()")
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(result.rows_affected())
}

async fn token_secret(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source: Uuid,
) -> Result<Option<SecretId>, StoreError> {
    let secret: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT oauth_secret_id FROM sources WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(source)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(secret.flatten().map(SecretId::from_uuid))
}

fn open_tokens(
    keyring: &Keyring,
    workspace: WorkspaceId,
    secret: SecretId,
    stored: &secrets::StoredSecret,
) -> Result<StoredTokens, TokenError> {
    let aad = secret_aad(workspace, secret, SecretPurpose::OAuthToken);
    if stored.purpose != SecretPurpose::OAuthToken {
        return Err(TokenError::Unopenable);
    }
    let bytes = open(keyring, &stored.sealed, &aad).map_err(|_| TokenError::Unopenable)?;
    StoredTokens::from_plaintext(&bytes)
}

/// The tokens of `source`, or `None` when it has none (or is not in `workspace`).
pub async fn load_tokens(
    pool: &PgPool,
    keyring: &Keyring,
    workspace: WorkspaceId,
    source: Uuid,
) -> Result<Option<StoredTokens>, TokenError> {
    let mut connection = pool.acquire().await.map_err(StoreError::query)?;
    let Some(secret) = token_secret(&mut connection, workspace, source).await? else {
        return Ok(None);
    };
    let Some(stored) = secrets::get(&mut *connection, workspace, secret).await? else {
        return Ok(None);
    };
    open_tokens(keyring, workspace, secret, &stored).map(Some)
}

fn seal_tokens(
    keyring: &Keyring,
    workspace: WorkspaceId,
    secret: SecretId,
    tokens: &StoredTokens,
) -> Result<zirv_flow_core::Sealed, TokenError> {
    let plaintext = tokens.to_plaintext()?;
    Ok(seal(
        keyring,
        plaintext.expose(),
        &secret_aad(workspace, secret, SecretPurpose::OAuthToken),
    ))
}

async fn write_tokens(
    connection: &mut PgConnection,
    keyring: &Keyring,
    workspace: WorkspaceId,
    secret: SecretId,
    tokens: &StoredTokens,
) -> Result<(), TokenError> {
    let sealed = seal_tokens(keyring, workspace, secret, tokens)?;
    if secrets::replace(connection, &keyring.key_ids(), workspace, secret, &sealed).await? {
        Ok(())
    } else {
        Err(TokenError::NotConnected)
    }
}

/// Refreshes the source's tokens so that concurrent refreshers, in any process, make one
/// provider call. In ONE transaction: lock the token secret row, reload and open the newest
/// stored tokens, and if their refresh token differs from `held` another process has already
/// refreshed, so return them without calling the provider. Otherwise call `refresh` with the
/// stored refresh token, save the result (the rotated refresh token included) and commit.
///
/// The transaction spans the provider call, which the runtime bounds with its request timeout. A
/// failed refresh rolls back and stores nothing. Generic in the caller's tokens and error so the
/// store knows nothing of the connector runtime.
pub async fn refresh_serialized<E, F, Fut>(
    pool: &PgPool,
    keyring: &Keyring,
    workspace: WorkspaceId,
    source: Uuid,
    held: &SecretString,
    refresh: F,
) -> Result<StoredTokens, RefreshError<E>>
where
    F: FnOnce(SecretString) -> Fut,
    Fut: Future<Output = Result<StoredTokens, E>>,
{
    let mut tx = crate::begin(pool).await?;
    let secret = token_secret(&mut tx, workspace, source)
        .await?
        .ok_or(TokenError::NotConnected)?;
    let stored = secrets::get_for_update(&mut tx, workspace, secret)
        .await?
        .ok_or(TokenError::NotConnected)?;
    let current = open_tokens(keyring, workspace, secret, &stored)?;
    if current.refresh_token.expose() != held.expose() {
        // Another process refreshed first: use its tokens, call nothing.
        return Ok(current);
    }
    let refreshed = refresh(SecretString::new(current.refresh_token.expose().to_owned()))
        .await
        .map_err(RefreshError::Refresh)?;
    write_tokens(&mut tx, keyring, workspace, secret, &refreshed).await?;
    crate::commit(tx).await?;
    Ok(refreshed)
}

/// Links a token secret to the source and records the connect time. `secret` is `None` when the
/// source already has one (a reconnect replaces the content, not the row). `false` when the
/// workspace has no such source.
pub async fn link_tokens(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source: Uuid,
    secret: Option<SecretId>,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE sources SET oauth_secret_id = COALESCE($3, oauth_secret_id), \
            oauth_connected_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(source)
    .bind(secret.map(|id| id.as_uuid()))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Disconnects the source: clears the link and deletes the token secret, in the caller's
/// transaction. `true` when the source had tokens.
pub async fn clear_tokens(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source: Uuid,
) -> Result<bool, StoreError> {
    let mut tx = connection.begin().await.map_err(StoreError::query)?;
    let Some(secret) = token_secret(&mut tx, workspace, source).await? else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE sources SET oauth_secret_id = NULL, oauth_connected_at = NULL, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(source)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    secrets::delete(&mut *tx, workspace, secret).await?;
    tx.commit().await.map_err(StoreError::query)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use time::macros::datetime;
    use zirv_flow_core::{
        Email, HmacRoot, MasterKey, Slug, WorkspaceName, deployment_key_aad, seal as seal_bytes,
    };

    use super::*;
    use crate::sources::{NewSource, create_source, delete_source};
    use crate::{begin, commit, create_user, create_workspace};

    struct Fixture {
        pool: PgPool,
        keys: Keyring,
        ws: WorkspaceId,
        source: Uuid,
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn source_in(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "http",
                name,
                config: &json!({}),
                credential_secret_id: None,
                credential_fields: &[],
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn new_state<'a>(
        fx: &Fixture,
        workspace: WorkspaceId,
        user: Uuid,
        state_hash: &'a [u8],
        ttl_seconds: i64,
    ) -> NewOAuthState<'a> {
        NewOAuthState {
            workspace,
            source: fx.source,
            user,
            state_hash,
            pkce_verifier: "verifier",
            binding: "b",
            ttl_seconds,
        }
    }

    async fn fixture(pool: PgPool) -> Fixture {
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let root = HmacRoot::bootstrap_from(keys.current());
        let sealed = seal_bytes(
            &keys,
            root.to_secret_bytes().expose(),
            &deployment_key_aad(secrets::HMAC_ROOT),
        );
        secrets::insert_hmac_root_if_absent(&pool, &sealed)
            .await
            .unwrap();
        let ws = workspace(&pool, "one").await;
        let source = source_in(&pool, ws, "api").await;
        Fixture {
            pool,
            keys,
            ws,
            source,
        }
    }

    fn tokens(access: &str, refresh: &str) -> StoredTokens {
        StoredTokens {
            access_token: SecretString::new(access.into()),
            refresh_token: SecretString::new(refresh.into()),
            expires_at: Some(datetime!(2030-01-01 00:00 UTC)),
            binding: "binding-1".into(),
        }
    }

    /// Connects an account as the callback does: a new secret, linked.
    async fn connect(fx: &Fixture, tokens: &StoredTokens) -> SecretId {
        let id = SecretId::new();
        let sealed = seal_tokens(&fx.keys, fx.ws, id, tokens).unwrap();
        let mut tx = begin(&fx.pool).await.unwrap();
        secrets::insert(
            &mut tx,
            &fx.keys.key_ids(),
            fx.ws,
            SecretPurpose::OAuthToken,
            id,
            &sealed,
        )
        .await
        .unwrap();
        assert!(
            link_tokens(&mut tx, fx.ws, fx.source, Some(id))
                .await
                .unwrap()
        );
        commit(tx).await.unwrap();
        id
    }

    #[test]
    fn debug_output_shows_no_token_and_no_verifier() {
        let shown = format!("{:?}", tokens("access-secret", "refresh-secret"));
        assert!(!shown.contains("secret"));
        let state = OAuthState {
            workspace_id: WorkspaceId::new(),
            source_id: Uuid::nil(),
            user_id: Uuid::nil(),
            pkce_verifier: "verifier-secret".into(),
            binding: "b".into(),
        };
        assert!(!format!("{state:?}").contains("secret"));
    }

    #[test]
    fn the_plaintext_round_trips_and_a_non_token_secret_does_not_open() {
        let original = tokens("a", "r");

        let back = StoredTokens::from_plaintext(&original.to_plaintext().unwrap()).unwrap();

        assert_eq!(back.access_token.expose(), "a");
        assert_eq!(back.refresh_token.expose(), "r");
        assert_eq!(back.expires_at, original.expires_at);
        assert_eq!(back.binding, "binding-1");
        assert!(matches!(
            StoredTokens::from_plaintext(&SecretBytes::new(b"{\"token\":\"x\"}".to_vec())),
            Err(TokenError::Unopenable)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_state_can_be_taken_once_and_an_expired_one_is_not_returned(pool: PgPool) {
        let fx = fixture(pool).await;
        let user = create_user(
            &fx.pool,
            &Email::parse("a@example.com").unwrap(),
            None,
            false,
        )
        .await
        .unwrap();
        let user = user.id.as_uuid();
        create_oauth_state(&fx.pool, &new_state(&fx, fx.ws, user, b"fresh", 600))
            .await
            .unwrap();
        create_oauth_state(&fx.pool, &new_state(&fx, fx.ws, user, b"old", -1))
            .await
            .unwrap();

        let taken = take_oauth_state(&fx.pool, b"fresh").await.unwrap().unwrap();
        assert_eq!(
            (taken.workspace_id, taken.source_id, taken.user_id),
            (fx.ws, fx.source, user)
        );
        assert_eq!(taken.pkce_verifier, "verifier");
        assert_eq!(taken.binding, "b");
        assert!(
            take_oauth_state(&fx.pool, b"fresh")
                .await
                .unwrap()
                .is_none()
        );
        assert!(take_oauth_state(&fx.pool, b"old").await.unwrap().is_none());
        assert_eq!(delete_expired_oauth_states(&fx.pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_state_cannot_name_a_source_of_another_workspace(pool: PgPool) {
        let fx = fixture(pool).await;
        let other = workspace(&fx.pool, "two").await;
        let user = create_user(
            &fx.pool,
            &Email::parse("a@example.com").unwrap(),
            None,
            false,
        )
        .await
        .unwrap()
        .id
        .as_uuid();

        let result = create_oauth_state(&fx.pool, &new_state(&fx, other, user, b"s", 600)).await;

        assert!(result.is_err());
        assert!(take_oauth_state(&fx.pool, b"s").await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn tokens_round_trip_a_rotated_refresh_token_and_another_workspace_gets_nothing(
        pool: PgPool,
    ) {
        let fx = fixture(pool).await;
        assert!(
            load_tokens(&fx.pool, &fx.keys, fx.ws, fx.source)
                .await
                .unwrap()
                .is_none(),
            "no account connected yet"
        );
        connect(&fx, &tokens("acc-0", "rt-0")).await;

        let first = load_tokens(&fx.pool, &fx.keys, fx.ws, fx.source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.access_token.expose(), "acc-0");
        assert_eq!(first.refresh_token.expose(), "rt-0");

        refresh_serialized::<(), _, _>(
            &fx.pool,
            &fx.keys,
            fx.ws,
            fx.source,
            &first.refresh_token,
            |_| async { Ok(tokens("acc-1", "rt-1")) },
        )
        .await
        .unwrap();
        let second = load_tokens(&fx.pool, &fx.keys, fx.ws, fx.source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.refresh_token.expose(), "rt-1");

        let other = workspace(&fx.pool, "two").await;
        assert!(
            load_tokens(&fx.pool, &fx.keys, other, fx.source)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            refresh_serialized::<(), _, _>(
                &fx.pool,
                &fx.keys,
                other,
                fx.source,
                &first.refresh_token,
                |_| async { Ok(tokens("x", "y")) },
            )
            .await,
            Err(RefreshError::Token(TokenError::NotConnected))
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn tokens_sealed_for_another_purpose_or_key_do_not_open(pool: PgPool) {
        let fx = fixture(pool).await;
        connect(&fx, &tokens("a", "r")).await;
        let stranger = Keyring::new(MasterKey::generate(), None).unwrap();

        let result = load_tokens(&fx.pool, &stranger, fx.ws, fx.source).await;

        assert!(matches!(result, Err(TokenError::Unopenable)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_source_deletes_its_token_secret(pool: PgPool) {
        let fx = fixture(pool).await;
        let secret = connect(&fx, &tokens("a", "r")).await;
        assert!(
            secrets::get(&fx.pool, fx.ws, secret)
                .await
                .unwrap()
                .is_some()
        );

        let mut tx = begin(&fx.pool).await.unwrap();
        delete_source(&mut tx, fx.ws, fx.source).await.unwrap();
        commit(tx).await.unwrap();

        assert!(
            secrets::get(&fx.pool, fx.ws, secret)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn clearing_the_tokens_disconnects_the_source(pool: PgPool) {
        let fx = fixture(pool).await;
        let secret = connect(&fx, &tokens("a", "r")).await;

        let mut tx = begin(&fx.pool).await.unwrap();
        assert!(clear_tokens(&mut tx, fx.ws, fx.source).await.unwrap());
        assert!(!clear_tokens(&mut tx, fx.ws, fx.source).await.unwrap());
        commit(tx).await.unwrap();

        let source = crate::find_source(&fx.pool, fx.ws, fx.source)
            .await
            .unwrap()
            .unwrap();
        assert!(source.oauth_secret_id.is_none() && source.oauth_connected_at.is_none());
        assert!(
            secrets::get(&fx.pool, fx.ws, secret)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A provider that counts refreshes and revokes the whole family on a reused refresh token.
    #[derive(Default)]
    struct Provider {
        calls: AtomicUsize,
        revoked: std::sync::atomic::AtomicBool,
    }

    impl Provider {
        async fn refresh(&self, refresh_token: SecretString) -> Result<StoredTokens, &'static str> {
            // Slow enough that two refreshers overlap if nothing serializes them.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if refresh_token.expose() != format!("rt-{n}") {
                self.revoked.store(true, Ordering::SeqCst);
                return Err("reused");
            }
            Ok(tokens(&format!("acc-{}", n + 1), &format!("rt-{}", n + 1)))
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_refreshes_make_one_provider_call_and_both_end_with_the_rotated_token(
        pool: PgPool,
    ) {
        let fx = Arc::new(fixture(pool).await);
        connect(&fx, &tokens("acc-0", "rt-0")).await;
        let provider = Arc::new(Provider::default());
        let held = |fx: &Fixture| {
            let _ = fx;
            SecretString::new("rt-0".into())
        };

        let run = |fx: Arc<Fixture>, provider: Arc<Provider>| async move {
            refresh_serialized(
                &fx.pool,
                &fx.keys,
                fx.ws,
                fx.source,
                &held(&fx),
                |current| async move { provider.refresh(current).await },
            )
            .await
        };
        let (a, b) = tokio::join!(
            run(Arc::clone(&fx), Arc::clone(&provider)),
            run(Arc::clone(&fx), Arc::clone(&provider))
        );

        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "one provider call"
        );
        assert!(!provider.revoked.load(Ordering::SeqCst));
        assert_eq!(a.refresh_token.expose(), "rt-1");
        assert_eq!(b.refresh_token.expose(), "rt-1");
        let stored = load_tokens(&fx.pool, &fx.keys, fx.ws, fx.source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.refresh_token.expose(), "rt-1");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_refresh_stores_nothing(pool: PgPool) {
        let fx = fixture(pool).await;
        connect(&fx, &tokens("acc-0", "rt-0")).await;

        let result = refresh_serialized(
            &fx.pool,
            &fx.keys,
            fx.ws,
            fx.source,
            &SecretString::new("rt-0".into()),
            |_| async { Err::<StoredTokens, _>("provider refused") },
        )
        .await;

        assert!(matches!(
            result,
            Err(RefreshError::Refresh("provider refused"))
        ));
        let stored = load_tokens(&fx.pool, &fx.keys, fx.ws, fx.source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.refresh_token.expose(), "rt-0");
        assert_eq!(stored.access_token.expose(), "acc-0");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_refresh_of_a_source_without_tokens_is_not_connected(pool: PgPool) {
        let fx = fixture(pool).await;

        let result = refresh_serialized(
            &fx.pool,
            &fx.keys,
            fx.ws,
            fx.source,
            &SecretString::new("rt-0".into()),
            |_| async { Ok::<_, &str>(tokens("a", "b")) },
        )
        .await;

        assert!(matches!(
            result,
            Err(RefreshError::Token(TokenError::NotConnected))
        ));
    }
}
