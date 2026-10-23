//! The typed credential `api` opens from a secret and hands to a source (architecture 16.2).

use std::collections::BTreeMap;
use std::fmt;

use zirv_flow_core::{SecretBytes, SecretString};

/// A secret that is not a JSON object of text values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialFormatError;

impl fmt::Display for CredentialFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the stored credential is not a JSON object of text values")
    }
}

impl std::error::Error for CredentialFormatError {}

/// The credential fields of one source, wiped on drop. No `Serialize` and no `Clone`, and
/// `Debug` shows no value and no key.
#[derive(Default)]
pub struct Credential {
    fields: BTreeMap<String, SecretString>,
}

impl Credential {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: impl Into<String>, value: SecretString) {
        self.fields.insert(key.into(), value);
    }

    /// The value of a field the source declared, if it is set.
    pub fn get(&self, key: &str) -> Option<&SecretString> {
        self.fields.get(key)
    }

    /// Builds a credential from the plaintext of a sealed secret: a JSON object of field name
    /// to text value. Fails when the bytes are anything else.
    pub fn from_json(bytes: &SecretBytes) -> Result<Self, CredentialFormatError> {
        let values: BTreeMap<String, SecretString> =
            serde_json::from_slice(bytes.expose()).map_err(|_| CredentialFormatError)?;
        let mut credential = Self::new();
        for (key, value) in values {
            credential.insert(key, value);
        }
        Ok(credential)
    }

    /// Builds a credential from a workspace credential: the plaintext of its sealed secret
    /// (a JSON object of text values) and its non-secret `fields` (a JSON object whose text
    /// and integer values are read as text). A secret value wins over a field of the same name.
    pub fn from_parts(
        secret: &SecretBytes,
        public: &serde_json::Value,
    ) -> Result<Self, CredentialFormatError> {
        let mut credential = Self::new();
        if let Some(public) = public.as_object() {
            for (key, value) in public {
                let text = match value {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Number(number) => number.to_string(),
                    serde_json::Value::Bool(flag) => flag.to_string(),
                    _ => continue,
                };
                credential.insert(key.clone(), SecretString::new(text));
            }
        }
        for (key, value) in Self::from_json(secret)?.fields {
            credential.insert(key, value);
        }
        Ok(credential)
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Credential([redacted; {} fields])", self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_returns_what_was_set_and_debug_shows_no_value_or_key() {
        let mut credential = Credential::new();
        credential.insert("token", SecretString::new("hunter2".into()));

        assert_eq!(credential.get("token").unwrap().expose(), "hunter2");
        assert!(credential.get("other").is_none());
        let debug = format!("{credential:?}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("token"),
            "{debug}"
        );
        assert_eq!(debug, "Credential([redacted; 1 fields])");
    }

    #[test]
    fn an_empty_credential_is_empty() {
        assert!(Credential::new().is_empty());
    }

    #[test]
    fn a_credential_is_read_from_its_sealed_json_and_nothing_else_is_accepted() {
        let bytes = SecretBytes::new(br#"{"token":"abc","user":"u"}"#.to_vec());

        let credential = Credential::from_json(&bytes).unwrap();

        assert_eq!(credential.get("token").unwrap().expose(), "abc");
        assert_eq!(credential.get("user").unwrap().expose(), "u");
        assert_eq!(credential.len(), 2);
        for bad in ["[1]", "{\"token\":1}", "not json"] {
            let bytes = SecretBytes::new(bad.as_bytes().to_vec());
            assert_eq!(
                Credential::from_json(&bytes).unwrap_err(),
                CredentialFormatError,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_credential_merges_the_non_secret_fields_with_the_sealed_secret() {
        let secret = SecretBytes::new(br#"{"password":"p","host":"sealed"}"#.to_vec());
        let public = serde_json::json!({"username": "u", "port": 587, "host": "plain", "n": null});

        let credential = Credential::from_parts(&secret, &public).unwrap();

        assert_eq!(credential.get("username").unwrap().expose(), "u");
        assert_eq!(credential.get("port").unwrap().expose(), "587");
        assert_eq!(credential.get("password").unwrap().expose(), "p");
        assert_eq!(credential.get("host").unwrap().expose(), "sealed");
        assert!(credential.get("n").is_none());
        assert_eq!(credential.len(), 4);
    }
}
