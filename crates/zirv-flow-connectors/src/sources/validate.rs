//! Checks a source's settings against its spec before anything is stored. Reasons name the field
//! and never repeat a value.

use std::collections::BTreeMap;

use serde_json::Value;
use url::Url;

use super::spec::{FieldKind, FieldSpec, SourceSpec};
use super::types::{StreamDescriptor, SyncMode};

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

/// The settings of a stream a user chose, as they are stored.
#[derive(Clone, Copy, Debug)]
pub struct StreamSettingsInput<'a> {
    pub sync_mode: &'a str,
    pub cursor_field: Option<&'a str>,
    pub primary_key: &'a [String],
    pub staleness_threshold_seconds: Option<i32>,
}

/// Checks a stream's settings against the descriptor discovery last reported: the sync mode is
/// one the stream supports, the cursor field and primary key name fields it has, an incremental
/// stream has a cursor field, and a staleness threshold is positive. Returns the sync mode, or
/// every rule broken. The API checks a change with it and a run re-checks before it reads.
pub fn validate_stream_settings(
    descriptor: &StreamDescriptor,
    settings: &StreamSettingsInput<'_>,
) -> Result<SyncMode, Vec<FieldError>> {
    let mut errors = Vec::new();
    let has_field = |name: &str| descriptor.fields.iter().any(|field| field.name == name);

    let mode = SyncMode::parse(settings.sync_mode);
    match mode {
        None => errors.push(error("sync_mode", "must be full_refresh or incremental")),
        Some(mode) if !descriptor.sync_modes.contains(&mode) => {
            errors.push(error("sync_mode", "is not supported by this stream"));
        }
        Some(_) => {}
    }
    match settings.cursor_field {
        Some(cursor) if !has_field(cursor) => {
            errors.push(error("cursor_field", "is not a field of this stream"));
        }
        None if mode == Some(SyncMode::Incremental) => {
            errors.push(error(
                "cursor_field",
                "is required for an incremental stream",
            ));
        }
        _ => {}
    }
    if settings.primary_key.iter().any(|name| !has_field(name)) {
        errors.push(error(
            "primary_key",
            "names a field this stream does not have",
        ));
    }
    if settings
        .staleness_threshold_seconds
        .is_some_and(|seconds| seconds <= 0)
    {
        errors.push(error("staleness_threshold_seconds", "must be positive"));
    }
    match (errors.is_empty(), mode) {
        (true, Some(mode)) => Ok(mode),
        _ => Err(errors),
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

    fn descriptor() -> StreamDescriptor {
        use super::super::types::{FieldDescriptor, FieldType};
        StreamDescriptor {
            name: "contacts".into(),
            fields: ["id", "updated_at"]
                .into_iter()
                .map(|name| FieldDescriptor {
                    name: name.into(),
                    field_type: FieldType::String,
                })
                .collect(),
            sync_modes: vec![SyncMode::FullRefresh, SyncMode::Incremental],
            default_cursor_field: Some("updated_at".into()),
            default_primary_key: vec!["id".into()],
        }
    }

    fn settings<'a>(
        mode: &'a str,
        cursor: Option<&'a str>,
        key: &'a [String],
    ) -> StreamSettingsInput<'a> {
        StreamSettingsInput {
            sync_mode: mode,
            cursor_field: cursor,
            primary_key: key,
            staleness_threshold_seconds: Some(60),
        }
    }

    #[test]
    fn stream_settings_that_fit_the_descriptor_return_the_sync_mode() {
        let key = vec!["id".to_owned()];

        let mode = validate_stream_settings(
            &descriptor(),
            &settings("incremental", Some("updated_at"), &key),
        );

        assert_eq!(mode.unwrap(), SyncMode::Incremental);
        assert!(
            validate_stream_settings(&descriptor(), &settings("full_refresh", None, &key)).is_ok()
        );
    }

    #[test]
    fn every_stream_setting_the_descriptor_cannot_serve_is_reported() {
        let key = vec!["missing".to_owned()];
        let mut bad = settings("weekly", Some("nope"), &key);
        bad.staleness_threshold_seconds = Some(0);

        let errors = fields_of(validate_stream_settings(&descriptor(), &bad).map(|_| ()));

        assert_eq!(
            errors,
            [
                "sync_mode: must be full_refresh or incremental",
                "cursor_field: is not a field of this stream",
                "primary_key: names a field this stream does not have",
                "staleness_threshold_seconds: must be positive",
            ]
        );
    }

    #[test]
    fn incremental_needs_a_cursor_and_a_mode_the_stream_offers() {
        let key = vec![];
        let mut only_full = descriptor();
        only_full.sync_modes = vec![SyncMode::FullRefresh];

        let no_cursor =
            validate_stream_settings(&descriptor(), &settings("incremental", None, &key));
        let unsupported = validate_stream_settings(
            &only_full,
            &settings("incremental", Some("updated_at"), &key),
        );

        assert_eq!(
            fields_of(no_cursor.map(|_| ())),
            ["cursor_field: is required for an incremental stream"]
        );
        assert_eq!(
            fields_of(unsupported.map(|_| ())),
            ["sync_mode: is not supported by this stream"]
        );
    }
}
