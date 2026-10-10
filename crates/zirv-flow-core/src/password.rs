//! Password policy, argon2id hashing and verification.

use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

use crate::ValidationError;

const MIN_LENGTH: usize = 12;
const MAX_LENGTH: usize = 1024;
const SALT_BYTES: usize = 16;

/// Checks the length policy: 12 to 1024 characters. The error never echoes the password.
pub fn validate(password: &str) -> Result<(), ValidationError> {
    let length = password.chars().count();
    if (MIN_LENGTH..=MAX_LENGTH).contains(&length) {
        Ok(())
    } else {
        Err(ValidationError::new(
            "password",
            "must be 12 to 1024 characters",
        ))
    }
}

/// Hashes with argon2id at the crate's default (OWASP) parameters and a random salt.
/// Returns a PHC string.
///
/// # Panics
///
/// If the operating system has no randomness source, or the password exceeds argon2's
/// hard input limit (callers apply [`validate`] first).
pub fn hash(password: &str) -> String {
    let mut salt = [0_u8; SALT_BYTES];
    getrandom::fill(&mut salt).expect("the operating system provides randomness");
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .expect("argon2 hashing with default parameters succeeds")
        .to_string()
}

/// Verifies a password against a PHC string. A malformed hash never verifies.
pub fn verify(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// Runs one verification against a fixed hash, so that signing in with an unknown
/// e-mail takes as long as signing in with a known one.
pub fn dummy_verify() {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let hash = DUMMY.get_or_init(|| hash("dummy password for timing equalisation"));
    let _ = verify("an incorrect password", hash);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_bounds_the_length() {
        assert!(validate(&"a".repeat(11)).is_err());
        assert!(validate(&"a".repeat(12)).is_ok());
        assert!(validate(&"a".repeat(1024)).is_ok());
        assert!(validate(&"a".repeat(1025)).is_err());
    }

    #[test]
    fn the_policy_counts_characters() {
        assert!(validate(&"é".repeat(12)).is_ok());
    }

    #[test]
    fn a_policy_error_does_not_echo_the_password() {
        let error = validate("hunter2").unwrap_err();

        assert_eq!(error.field(), "password");
        assert!(!error.to_string().contains("hunter2"));
    }

    #[test]
    fn a_hash_is_an_argon2id_phc_string_that_verifies() {
        let hashed = hash("correct horse battery");

        assert!(
            hashed.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "{hashed}"
        );
        assert!(verify("correct horse battery", &hashed));
        assert!(!verify("correct horse batterz", &hashed));
    }

    #[test]
    fn hashing_the_same_password_twice_uses_different_salts() {
        assert_ne!(hash("correct horse battery"), hash("correct horse battery"));
    }

    #[test]
    fn a_malformed_hash_never_verifies() {
        assert!(!verify("anything at all", "not a phc string"));
        assert!(!verify("anything at all", ""));
    }

    #[test]
    fn dummy_verify_completes() {
        dummy_verify();
    }
}
