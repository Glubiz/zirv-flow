//! Checks a destination's settings against its spec before anything is stored. Reasons name the
//! field and never repeat a value. The rules are the sources' (`sources::validate`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::spec::DestinationSpec;
use crate::sources::{FieldError, check_config, check_credential, check_credential_names};

/// Checks the configuration object: required fields are present, values have the declared type
/// and select values are among the options, and no undeclared key is set.
pub fn validate_config(spec: &DestinationSpec, config: &Value) -> Result<(), Vec<FieldError>> {
    check_config(spec.config_fields, spec.config_check, "destination", config)
}

/// Checks the credential fields given at create time (`complete`) or at replacement: no
/// undeclared key and no empty value, and with `complete` every required field.
pub fn validate_credential<V: AsRef<str>>(
    spec: &DestinationSpec,
    values: &BTreeMap<String, V>,
    complete: bool,
) -> Result<(), Vec<FieldError>> {
    check_credential(spec.credential_fields, "destination", values, complete)
}

/// Checks that a stored credential, which has the fields named in `present`, holds every
/// required credential field of the destination type.
pub fn validate_credential_names(
    spec: &DestinationSpec,
    present: &BTreeSet<String>,
) -> Result<(), Vec<FieldError>> {
    check_credential_names(spec.credential_fields, "destination", present)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::spec::Capabilities;
    use super::*;
    use crate::sources::{FieldKind, FieldSpec};

    const SPEC: DestinationSpec = DestinationSpec {
        kind: "t",
        label: "T",
        description: "",
        config_fields: &[FieldSpec {
            key: "host",
            label: "Host",
            help: "",
            kind: FieldKind::Text,
            required: true,
            secret: false,
        }],
        credential_fields: &[FieldSpec {
            key: "token",
            label: "Token",
            help: "",
            kind: FieldKind::Text,
            required: true,
            secret: true,
        }],
        capabilities: Capabilities {
            channels: &[],
            idempotency_key: false,
            batching: false,
            rate_limit: false,
            erase: false,
        },
        config_check: None,
    };

    #[test]
    fn a_setting_is_checked_against_the_spec_and_the_reason_names_the_destination() {
        assert!(validate_config(&SPEC, &json!({"host": "h"})).is_ok());

        let errors = validate_config(&SPEC, &json!({"extra": 1})).unwrap_err();

        assert!(
            errors
                .iter()
                .any(|e| e.field == "host" && e.reason == "is required")
        );
        assert!(
            errors
                .iter()
                .any(|e| e.field == "extra" && e.reason.contains("destination type"))
        );
    }

    #[test]
    fn a_credential_needs_its_required_fields_when_complete() {
        let mut values = BTreeMap::new();
        values.insert("nope".to_owned(), "x".to_owned());

        let errors = validate_credential(&SPEC, &values, true).unwrap_err();

        assert_eq!(errors.len(), 2);
        assert!(validate_credential(&SPEC, &BTreeMap::<String, String>::new(), false).is_ok());
    }

    #[test]
    fn a_stored_credential_must_hold_every_required_field_of_the_destination_type() {
        let present = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect();

        assert!(validate_credential_names(&SPEC, &present(&["token", "extra"])).is_ok());
        let errors = validate_credential_names(&SPEC, &present(&["extra"])).unwrap_err();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "token");
        assert!(errors[0].reason.contains("destination"));
    }
}
