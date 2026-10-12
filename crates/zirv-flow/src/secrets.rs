//! Startup of the secrets: the keyring from configuration, and the deployment HMAC root from
//! the database (architecture section 16.2).

use std::fs;
use std::sync::Arc;

use zirv_flow_store::{
    HmacRoot, Keyring, MasterKey, PgPool, deployment_key_aad, open_secret, seal_secret, secrets,
};

use crate::config::{Config, MasterKeySource};

/// What the roles that use secrets are started with.
#[derive(Clone)]
pub(crate) struct Secrets {
    pub(crate) keyring: Arc<Keyring>,
    pub(crate) hmac_root: Arc<HmacRoot>,
}

/// Reads one master key from its configured source. The error is the message to report; it
/// names the source but never the key.
fn read_key(
    source: MasterKeySource<'_>,
    inline_key: &str,
    file_key: &str,
) -> Result<Option<MasterKey>, String> {
    match source {
        MasterKeySource::Unset => Ok(None),
        MasterKeySource::Inline(text) => MasterKey::parse(text)
            .map(Some)
            .map_err(|error| format!("{inline_key}: {error}")),
        MasterKeySource::File(path) => {
            let text = fs::read_to_string(path)
                .map_err(|_| format!("{file_key} {}: not readable", path.display()))?;
            MasterKey::parse(&text)
                .map(Some)
                .map_err(|error| format!("{file_key} {}: {error}", path.display()))
        }
    }
}

/// The current master key and, during a rotation, the previous one.
pub(crate) fn load_keyring(config: &Config) -> Result<Keyring, String> {
    let current = read_key(
        config.master_key_source(),
        "secrets.master_key",
        "secrets.master_key_file",
    )?
    .ok_or_else(|| {
        "the master key is required for serve and worker: set \
         secrets.master_key_file or ZIRV_FLOW_SECRETS__MASTER_KEY; create one with \
         zirv-flow admin generate-master-key --out <path>"
            .to_owned()
    })?;
    let previous = read_key(
        config.previous_master_key_source(),
        "secrets.previous_master_key",
        "secrets.previous_master_key_file",
    )?;
    Keyring::new(current, previous).map_err(|error| format!("secrets.previous_master_key: {error}"))
}

/// The root every HMAC key derives from. A deployment that has none yet gets one made from the
/// current master key, which is the key the existing hashes were made with; concurrent first
/// starts all try, one row wins, and each reads back and opens that row. Once a previous key
/// is configured the root must exist: bootstrapping from a rotation's new key would change
/// every hash.
pub(crate) async fn load_hmac_root(pool: &PgPool, keyring: &Keyring) -> Result<HmacRoot, String> {
    let aad = deployment_key_aad(secrets::HMAC_ROOT);
    let mut stored = secrets::hmac_root(pool)
        .await
        .map_err(|error| error.to_string())?;
    if stored.is_none() {
        if keyring.has_previous() {
            return Err(
                "no HMAC root exists yet; start once with only the master key that \
                 produced the existing hashes before rotating"
                    .to_owned(),
            );
        }
        let root = HmacRoot::bootstrap_from(keyring.current());
        let sealed = seal_secret(keyring, root.to_secret_bytes().expose(), &aad);
        secrets::insert_hmac_root_if_absent(pool, &sealed)
            .await
            .map_err(|error| error.to_string())?;
        stored = secrets::hmac_root(pool)
            .await
            .map_err(|error| error.to_string())?;
    }
    let sealed = stored.ok_or_else(|| "the HMAC root could not be stored".to_owned())?;
    let bytes =
        open_secret(keyring, &sealed, &aad).map_err(|error| format!("the HMAC root: {error}"))?;
    HmacRoot::from_bytes(&bytes).map_err(|error| format!("the HMAC root: {error}"))
}

/// The keyring and the HMAC root together, for serve and worker.
pub(crate) async fn load_secrets(pool: &PgPool, keyring: Keyring) -> Result<Secrets, String> {
    let hmac_root = load_hmac_root(pool, &keyring).await?;
    Ok(Secrets {
        keyring: Arc::new(keyring),
        hmac_root: Arc::new(hmac_root),
    })
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::HmacPurpose;

    use super::*;

    fn keyring(current: &MasterKey, previous: Option<&MasterKey>) -> Keyring {
        let copy = |key: &MasterKey| MasterKey::parse(&key.to_base64()).unwrap();
        Keyring::new(copy(current), previous.map(copy)).unwrap()
    }

    fn digest(root: &HmacRoot) -> [u8; 32] {
        root.hmac_key(HmacPurpose::Identifier)
            .digest(b"person@example.com")
    }

    fn config(env: Vec<(&str, String)>) -> Config {
        let mut vars = vec![(
            "ZIRV_FLOW_DATABASE__URL".into(),
            "postgres://localhost/database".into(),
        )];
        vars.extend(env.into_iter().map(|(k, v)| (k.into(), v.into())));
        Config::load(None, vars).unwrap()
    }

    #[test]
    fn the_keyring_holds_the_current_key_and_an_optional_previous_one() {
        let (a, b) = (MasterKey::generate(), MasterKey::generate());

        let alone = load_keyring(&config(vec![(
            "ZIRV_FLOW_SECRETS__MASTER_KEY",
            a.to_base64(),
        )]))
        .unwrap();
        let rotating = load_keyring(&config(vec![
            ("ZIRV_FLOW_SECRETS__MASTER_KEY", b.to_base64()),
            ("ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY", a.to_base64()),
        ]))
        .unwrap();

        assert!(!alone.has_previous());
        assert_eq!(alone.current().key_id(), a.key_id());
        assert!(rotating.has_previous());
        assert!(rotating.key(&a.key_id()).is_some());
        assert_eq!(rotating.current().key_id(), b.key_id());
    }

    #[test]
    fn a_previous_key_equal_to_the_current_one_is_refused() {
        let a = MasterKey::generate();

        let error = load_keyring(&config(vec![
            ("ZIRV_FLOW_SECRETS__MASTER_KEY", a.to_base64()),
            ("ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY", a.to_base64()),
        ]))
        .unwrap_err();

        assert_eq!(
            error,
            "secrets.previous_master_key: the previous master key is the same as the current one"
        );
    }

    #[test]
    fn a_bad_or_unreadable_previous_key_is_named_and_never_echoed() {
        let a = MasterKey::generate();

        let bad = load_keyring(&config(vec![
            ("ZIRV_FLOW_SECRETS__MASTER_KEY", a.to_base64()),
            ("ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY", "hunter2".into()),
        ]))
        .unwrap_err();
        let missing = load_keyring(&config(vec![
            ("ZIRV_FLOW_SECRETS__MASTER_KEY", a.to_base64()),
            (
                "ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY_FILE",
                "/nonexistent/old.key".into(),
            ),
        ]))
        .unwrap_err();

        assert_eq!(bad, "secrets.previous_master_key: not a base64 32-byte key");
        assert!(
            missing.starts_with("secrets.previous_master_key_file "),
            "{missing}"
        );
        assert!(missing.ends_with(": not readable"), "{missing}");
    }

    #[test]
    fn without_a_master_key_the_keyring_cannot_be_built() {
        let error = load_keyring(&config(vec![])).unwrap_err();

        assert!(
            error.starts_with("the master key is required for serve and worker"),
            "{error}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_first_start_stores_a_root_made_from_the_master_key(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let key = MasterKey::generate();

        let root = load_hmac_root(&pool, &keyring(&key, None)).await.unwrap();

        assert_eq!(digest(&root), digest(&HmacRoot::bootstrap_from(&key)));
        assert!(secrets::hmac_root(&pool).await.unwrap().is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_start_with_another_key_cannot_open_and_names_it(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        load_hmac_root(&pool, &keyring(&a, None)).await.unwrap();

        let error = load_hmac_root(&pool, &keyring(&b, None)).await.unwrap_err();

        assert!(error.starts_with("the HMAC root: "), "{error}");
        assert!(error.contains(&a.key_id()), "{error}");
        assert!(
            error.contains("secrets.previous_master_key_file"),
            "{error}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_start_with_a_previous_key_and_no_root_is_refused(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (a, b) = (MasterKey::generate(), MasterKey::generate());

        let error = load_hmac_root(&pool, &keyring(&b, Some(&a)))
            .await
            .unwrap_err();

        assert!(error.contains("no HMAC root exists yet"), "{error}");
        assert!(
            error.contains("start once with only the master key that produced the existing hashes"),
            "{error}"
        );
        assert!(secrets::hmac_root(&pool).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_first_starts_converge_on_one_root(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let key = MasterKey::generate();

        let mut starts = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            let keys = keyring(&key, None);
            starts.push(tokio::spawn(async move {
                digest(&load_hmac_root(&pool, &keys).await.unwrap())
            }));
        }
        let mut digests = Vec::new();
        for start in starts {
            digests.push(start.await.unwrap());
        }

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM deployment_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        assert!(digests.iter().all(|one| *one == digests[0]));
        assert_eq!(digests[0], digest(&HmacRoot::bootstrap_from(&key)));
    }
}
