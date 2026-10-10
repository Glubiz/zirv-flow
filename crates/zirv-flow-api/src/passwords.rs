//! Password hashing off the async runtime. Argon2 takes tens of milliseconds and would
//! otherwise stall every other request on the same worker thread.

use zirv_flow_core::password;

pub(crate) async fn hash(plain: String) -> String {
    tokio::task::spawn_blocking(move || password::hash(&plain))
        .await
        .expect("the password hashing task does not panic")
}

pub(crate) async fn verify(plain: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || password::verify(&plain, &hash))
        .await
        .expect("the password verification task does not panic")
}

/// Spends the time of one verification, for sign-in paths that have no hash to check.
pub(crate) async fn dummy_verify() {
    tokio::task::spawn_blocking(password::dummy_verify)
        .await
        .expect("the password verification task does not panic");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hash_verifies_only_the_original_password() {
        let hashed = hash("correct horse battery".into()).await;

        assert!(verify("correct horse battery".into(), hashed.clone()).await);
        assert!(!verify("incorrect horse battery".into(), hashed).await);
    }

    #[tokio::test]
    async fn the_dummy_verification_completes() {
        dummy_verify().await;
    }
}
