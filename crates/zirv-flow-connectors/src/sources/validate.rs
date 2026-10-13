//! Checks a source's settings against its spec before anything is stored. Reasons name the field
//! and never repeat a value.

use std::collections::BTreeMap;

use serde_json::Value;
use url::Url;

use super::spec::{FieldKind, FieldSpec, SourceSpec};

/// One setting that failed validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldError {
    pub field: String,
    pub reason: String,
}

fn error(field: &str, reason: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Checks the configuration object: required fields are present, values have the declared type
/// and select values are among the options, and no undeclared key is set.
pub fn validate_config(spec: &SourceSpec, config: &Value) -> Result<(), Vec<FieldError>> {
    let Some(object) = config.as_object() else {
        return Err(vec![error("config", "must be an object")]);
    };
    let mut errors = Vec::new();
    for key in object.keys() {
        if !spec.config_fields.iter().any(|field| field.key == key) {
            errors.push(error(key, "is not a setting of this source type"));
        }
    }
    for field in spec.config_fields {
        match object.get(field.key) {
            None | Some(Value::Null) if field.required => {
                errors.push(error(field.key, "is required"));
            }
            None | Some(Value::Null) => {}
            Some(value) => {
                if let Some(reason) = check_value(field, value) {
                    errors.push(error(field.key, reason));
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Checks the credential fields given at create time (`complete`) or at replacement: no
/// undeclared key and no empty value, and with `complete` every required field.
pub fn validate_credential<V: AsRef<str>>(
    spec: &SourceSpec,
    values: &BTreeMap<String, V>,
    complete: bool,
) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    for (key, value) in values {
        if !spec.credential_fields.iter().any(|field| field.key == key) {
            errors.push(error(key, "is not a credential field of this source type"));
        } else if value.as_ref().is_empty() {
            errors.push(error(key, "must not be empty"));
        }
    }
    if complete {
        for field in spec.credential_fields.iter().filter(|field| field.required) {
            if !values.contains_key(field.key) {
                errors.push(error(field.key, "is required"));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_value(field: &FieldSpec, value: &Value) -> Option<&'static str> {
    match field.kind {
        FieldKind::Text => match value.as_str() {
            Some(text) if field.required && text.trim().is_empty() => Some("must not be empty"),
            Some(_) => None,
            None => Some("must be text"),
        },
        FieldKind::Url => match value.as_str() {
            Some(text) => match Url::parse(text) {
                Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => None,
                _ => Some("must be an http or https URL"),
            },
            None => Some("must be text"),
        },
        FieldKind::Number => (!value.is_number()).then_some("must be a number"),
        FieldKind::Bool => (!value.is_boolean()).then_some("must be true or false"),
        FieldKind::Select(options) => match value.as_str() {
            Some(text) if options.contains(&text) => None,
            _ => Some("must be one of the listed options"),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::spec::Capabilities;
    use super::*;

    const fn field(key: &'static str, kind: FieldKind, required: bool, secret: bool) -> FieldSpec {
        FieldSpec {
            key,
            label: key,
            help: "",
            kind,
            required,
            secret,
        }
    }

    static SPEC: SourceSpec = SourceSpec {
        kind: "t",
        label: "T",
        description: "",
        config_fields: &[
            field("name", FieldKind::Text, true, false),
            field("url", FieldKind::Url, false, false),
            field("count", FieldKind::Number, false, false),
            field("flag", FieldKind::Bool, false, false),
            field("mode", FieldKind::Select(&["a", "b"]), false, false),
        ],
        credential_fields: &[
            field("token", FieldKind::Text, true, true),
            field("extra", FieldKind::Text, false, true),
        ],
        rate_limit: None,
        capabilities: Capabilities {
            incremental: false,
            full_refresh: true,
            emits_deletes: false,
        },
    };

    fn fields_of(result: Result<(), Vec<FieldError>>) -> Vec<String> {
        result
            .unwrap_err()
            .into_iter()
            .map(|e| format!("{}: {}", e.field, e.reason))
            .collect()
    }

    #[test]
    fn a_valid_config_passes_and_optional_fields_may_be_absent_or_null() {
        let config = json!({"name": "x", "url": "https://example.com/a", "count": 3,
            "flag": false, "mode": "b"});

        assert!(validate_config(&SPEC, &config).is_ok());
        assert!(validate_config(&SPEC, &json!({"name": "x", "url": null})).is_ok());
    }

    #[test]
    fn each_wrong_value_is_named_without_being_repeated() {
        let config = json!({"name": "  ", "url": "ftp://secret-host/", "count": "7",
            "flag": "yes", "mode": "c", "extra": 1});

        let errors = fields_of(validate_config(&SPEC, &config));

        assert_eq!(errors.len(), 6, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("name: must not be empty"))
        );
        assert!(errors.iter().any(|e| e.starts_with("url: must be an http")));
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("count: must be a number"))
        );
        assert!(errors.iter().any(|e| e.starts_with("flag: must be true")));
        assert!(errors.iter().any(|e| e.starts_with("mode: must be one of")));
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("extra: is not a setting"))
        );
        assert!(!errors.join(" ").contains("secret-host"));
    }

    #[test]
    fn a_missing_required_field_and_a_non_object_are_refused() {
        assert_eq!(
            fields_of(validate_config(&SPEC, &json!({}))),
            ["name: is required"]
        );
        assert_eq!(
            fields_of(validate_config(&SPEC, &json!([]))),
            ["config: must be an object"]
        );
    }

    #[test]
    fn credential_values_are_checked_for_keys_emptiness_and_completeness() {
        let mut values = BTreeMap::new();
        assert_eq!(
            fields_of(validate_credential(&SPEC, &values, true)),
            ["token: is required"]
        );
        assert!(validate_credential(&SPEC, &values, false).is_ok());

        values.insert("token".to_owned(), String::new());
        values.insert("other".to_owned(), "x".to_owned());
        let errors = fields_of(validate_credential(&SPEC, &values, true));
        assert_eq!(errors.len(), 2, "{errors:?}");

        values.clear();
        values.insert("token".to_owned(), "ok".to_owned());
        assert!(validate_credential(&SPEC, &values, true).is_ok());
    }
}
