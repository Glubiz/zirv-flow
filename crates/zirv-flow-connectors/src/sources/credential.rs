//! The typed credential `api` opens from a secret and hands to a source (architecture 16.2).

use std::collections::BTreeMap;
use std::fmt;

use zirv_flow_core::SecretString;

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
}
