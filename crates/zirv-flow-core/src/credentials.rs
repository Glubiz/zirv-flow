//! Workspace credentials (architecture 10.1 and 16.2): the schema that types one, the built-in
//! kinds, the validation of the values entered for it and the display rules for what is stored.
//!
//! A schema is a list of fields. Each field has a key, a label, a type, `required` and `secret`.
//! The non-secret values are stored as plain JSON; every secret value is sealed together as one
//! JSON object. Everything here is pure and never repeats a value: an issue names a field and a
//! rule only, so it can be shown to anyone and logged.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::HmacKey;

/// The most fields one schema may declare.
pub const MAX_FIELDS: usize = 32;
/// The longest field key, in bytes.
pub const MAX_KEY_LEN: usize = 64;
/// The longest label, in characters.
pub const MAX_LABEL_LEN: usize = 128;
/// The most options an enum field may list.
pub const MAX_OPTIONS: usize = 32;
/// The longest value of a field, in bytes.
pub const MAX_VALUE_LEN: usize = 8 * 1024;
/// The shortest secret value that shows its last four characters.
pub const HINT_MIN_LEN: usize = 16;

/// The value type of a credential field.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Integer,
    Enum,
    /// An absolute `http` or `https` URL without credentials or a fragment.
    Url,
}

/// One field of a credential schema.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    /// A secret field is sealed, never returned and never shown. Only a `string` can be secret.
    #[serde(default)]
    pub secret: bool,
    /// The allowed values of an `enum` field; empty for every other type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// A list of fields: the shape of one credential.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CredentialSchema {
    pub fields: Vec<CredentialField>,
}

/// What was wrong with a schema or with the values entered for one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueCode {
    /// A required field has no value.
    Required,
    /// A value for a key the schema does not declare.
    Unknown,
    /// The value does not have the field's type.
    WrongType,
    /// The value is not one of the field's options.
    NotAnOption,
    /// A secret field's value was given with the non-secret values, or the reverse.
    WrongPlace,
    /// The value is empty or too long.
    BadLength,
    /// The schema itself breaks a rule.
    BadSchema,
}

impl IssueCode {
    /// A fixed sentence for the code, with no value in it.
    pub const fn message(self) -> &'static str {
        match self {
            Self::Required => "is required",
            Self::Unknown => "is not a field of this credential kind",
            Self::WrongType => "has the wrong type",
            Self::NotAnOption => "is not one of the allowed values",
            Self::WrongPlace => {
                "is in the wrong group: secret fields go in `secret`, the rest in `fields`"
            }
            Self::BadLength => "must be 1 to 8192 bytes",
            Self::BadSchema => "is not a valid schema field",
        }
    }
}

/// One failed rule, naming the field and never its value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Issue {
    pub field: String,
    pub code: IssueCode,
}

impl fmt::Display for Issue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.code.message())
    }
}

fn issue(field: &str, code: IssueCode) -> Issue {
    Issue {
        field: field.to_owned(),
        code,
    }
}

fn key_is_valid(key: &str) -> bool {
    let mut chars = key.chars();
    key.len() <= MAX_KEY_LEN
        && chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl CredentialSchema {
    /// The field with this key.
    pub fn field(&self, key: &str) -> Option<&CredentialField> {
        self.fields.iter().find(|field| field.key == key)
    }

    /// The keys of the secret fields.
    pub fn secret_keys(&self) -> impl Iterator<Item = &str> {
        self.fields
            .iter()
            .filter(|field| field.secret)
            .map(|field| field.key.as_str())
    }

    /// Checks the schema itself: one to 32 fields, unique lowercase keys, labels, options only
    /// on enums (and at least one there), and secrets only on `string` fields.
    pub fn check(&self) -> Result<(), Vec<Issue>> {
        let mut issues = Vec::new();
        if self.fields.is_empty() || self.fields.len() > MAX_FIELDS {
            issues.push(issue("fields", IssueCode::BadSchema));
        }
        for (position, field) in self.fields.iter().enumerate() {
            let name = if key_is_valid(&field.key) {
                field.key.clone()
            } else {
                format!("fields[{position}].key")
            };
            let duplicate = self.fields[..position]
                .iter()
                .any(|earlier| earlier.key == field.key);
            let label_ok =
                !field.label.trim().is_empty() && field.label.chars().count() <= MAX_LABEL_LEN;
            let options_ok = match field.field_type {
                FieldType::Enum => {
                    !field.options.is_empty()
                        && field.options.len() <= MAX_OPTIONS
                        && field
                            .options
                            .iter()
                            .all(|option| !option.is_empty() && option.len() <= 64)
                }
                _ => field.options.is_empty(),
            };
            let secret_ok = !field.secret || field.field_type == FieldType::String;
            if !key_is_valid(&field.key) || duplicate || !label_ok || !options_ok || !secret_ok {
                issues.push(issue(&name, IssueCode::BadSchema));
            }
        }
        done(issues)
    }

    /// Checks the non-secret values. With `complete`, every required non-secret field must be
    /// present (creating); without, only the given keys are checked (a partial change).
    pub fn validate_fields(
        &self,
        values: &BTreeMap<String, Value>,
        complete: bool,
    ) -> Result<(), Vec<Issue>> {
        let mut issues = Vec::new();
        for (key, value) in values {
            match self.field(key) {
                None => issues.push(issue(key, IssueCode::Unknown)),
                Some(field) if field.secret => issues.push(issue(key, IssueCode::WrongPlace)),
                Some(field) => {
                    if let Some(code) = check_value(field, value) {
                        issues.push(issue(key, code));
                    }
                }
            }
        }
        if complete {
            for field in self.fields.iter().filter(|f| f.required && !f.secret) {
                if values.get(&field.key).is_none_or(Value::is_null) {
                    issues.push(issue(&field.key, IssueCode::Required));
                }
            }
        }
        done(issues)
    }

    /// Checks the secret values. `complete` demands every required secret field (creating and
    /// rotating always do).
    pub fn validate_secrets<V: AsRef<str>>(
        &self,
        values: &BTreeMap<String, V>,
        complete: bool,
    ) -> Result<(), Vec<Issue>> {
        let mut issues = Vec::new();
        for (key, value) in values {
            match self.field(key) {
                None => issues.push(issue(key, IssueCode::Unknown)),
                Some(field) if !field.secret => issues.push(issue(key, IssueCode::WrongPlace)),
                Some(_) => {
                    let length = value.as_ref().len();
                    if length == 0 || length > MAX_VALUE_LEN {
                        issues.push(issue(key, IssueCode::BadLength));
                    }
                }
            }
        }
        if complete {
            for field in self.fields.iter().filter(|f| f.required && f.secret) {
                if !values.contains_key(&field.key) {
                    issues.push(issue(&field.key, IssueCode::Required));
                }
            }
        }
        done(issues)
    }

    /// Checks a whole credential: the non-secret values then the secret ones, both complete.
    pub fn validate<V: AsRef<str>>(
        &self,
        fields: &BTreeMap<String, Value>,
        secrets: &BTreeMap<String, V>,
    ) -> Result<(), Vec<Issue>> {
        let mut issues = self.validate_fields(fields, true).err().unwrap_or_default();
        issues.extend(
            self.validate_secrets(secrets, true)
                .err()
                .unwrap_or_default(),
        );
        done(issues)
    }
}

fn done(issues: Vec<Issue>) -> Result<(), Vec<Issue>> {
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

fn check_value(field: &CredentialField, value: &Value) -> Option<IssueCode> {
    if value.is_null() {
        return field.required.then_some(IssueCode::Required);
    }
    match field.field_type {
        FieldType::String => match value.as_str() {
            Some(text) if text.is_empty() || text.len() > MAX_VALUE_LEN => {
                Some(IssueCode::BadLength)
            }
            Some(_) => None,
            None => Some(IssueCode::WrongType),
        },
        FieldType::Integer => value.as_i64().is_none().then_some(IssueCode::WrongType),
        FieldType::Enum => match value.as_str() {
            Some(text) if field.options.iter().any(|option| option == text) => None,
            Some(_) => Some(IssueCode::NotAnOption),
            None => Some(IssueCode::WrongType),
        },
        FieldType::Url => match value.as_str() {
            Some(text) if text.len() > MAX_VALUE_LEN => Some(IssueCode::BadLength),
            Some(text) => match Url::parse(text) {
                Ok(url)
                    if matches!(url.scheme(), "http" | "https")
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none() =>
                {
                    None
                }
                _ => Some(IssueCode::WrongType),
            },
            None => Some(IssueCode::WrongType),
        },
    }
}

/// The kind of a credential that was imported from a source's or destination's own sealed
/// secret: it has no schema, every field is secret and rotation keeps the same field names.
pub const IMPORTED_KIND: &str = "imported";

/// The built-in credential kinds. A fixed set of data, not a registry: each variant has its schema.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialKind {
    /// A key sent as a named header or as a bearer token.
    ApiKey,
    /// A username and password.
    Basic,
    /// A mail server: host, port, TLS mode, username and password.
    Smtp,
    /// An OAuth2 client: client id, client secret and token URL.
    OAuth2Client,
    /// A secret that signs or verifies messages.
    SigningSecret,
}

impl CredentialKind {
    /// Every built-in kind.
    pub const ALL: [Self; 5] = [
        Self::ApiKey,
        Self::Basic,
        Self::Smtp,
        Self::OAuth2Client,
        Self::SigningSecret,
    ];

    /// The stored schema id.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Basic => "basic",
            Self::Smtp => "smtp",
            Self::OAuth2Client => "oauth2_client",
            Self::SigningSecret => "signing_secret",
        }
    }

    /// The kind with this schema id.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// The name shown to a person.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
            Self::Basic => "Username and password",
            Self::Smtp => "SMTP server",
            Self::OAuth2Client => "OAuth2 client",
            Self::SigningSecret => "Signing secret",
        }
    }

    /// The schema of the kind.
    pub fn schema(self) -> CredentialSchema {
        let field = |key: &str, label: &str, field_type, required, secret| CredentialField {
            key: key.to_owned(),
            label: label.to_owned(),
            field_type,
            required,
            secret,
            options: Vec::new(),
        };
        let with_options = |mut field: CredentialField, options: &[&str]| {
            field.options = options.iter().map(|option| (*option).to_owned()).collect();
            field
        };
        let fields = match self {
            Self::ApiKey => vec![
                field("api_key", "API key", FieldType::String, true, true),
                with_options(
                    field("send_as", "Sent as", FieldType::Enum, false, false),
                    &["header", "bearer"],
                ),
                field(
                    "header_name",
                    "Header name",
                    FieldType::String,
                    false,
                    false,
                ),
            ],
            Self::Basic => vec![
                field("username", "Username", FieldType::String, true, false),
                field("password", "Password", FieldType::String, true, true),
            ],
            Self::Smtp => vec![
                field("host", "Host", FieldType::String, true, false),
                field("port", "Port", FieldType::Integer, true, false),
                with_options(
                    field("tls", "TLS mode", FieldType::Enum, true, false),
                    &["starttls", "tls", "none"],
                ),
                field("username", "Username", FieldType::String, true, false),
                field("password", "Password", FieldType::String, true, true),
            ],
            Self::OAuth2Client => vec![
                field("client_id", "Client id", FieldType::String, true, false),
                field(
                    "client_secret",
                    "Client secret",
                    FieldType::String,
                    true,
                    true,
                ),
                field("token_url", "Token URL", FieldType::Url, true, false),
            ],
            Self::SigningSecret => vec![field(
                "signing_secret",
                "Signing secret",
                FieldType::String,
                true,
                true,
            )],
        };
        CredentialSchema { fields }
    }
}

/// What a secret value shows of itself: its last four characters when it has at least
/// [`HINT_MIN_LEN`] characters, and nothing (fully masked) otherwise.
pub fn secret_hint(value: &str) -> String {
    let length = value.chars().count();
    if length >= HINT_MIN_LEN {
        value.chars().skip(length - 4).collect()
    } else {
        String::new()
    }
}

/// The redaction HMAC of the sealed plaintext, as lowercase hex: equal secrets have equal
/// fingerprints, so a screen can warn that the same secret was entered twice.
pub fn fingerprint(key: &HmacKey, sealed_plaintext: &[u8]) -> String {
    key.digest(sealed_plaintext)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{HmacPurpose, HmacRoot, MasterKey};

    fn fields(value: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    fn secrets(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn every_built_in_kind_has_a_valid_schema_and_round_trips_its_name() {
        for kind in CredentialKind::ALL {
            assert!(kind.schema().check().is_ok(), "{}", kind.as_str());
            assert_eq!(CredentialKind::parse(kind.as_str()), Some(kind));
            assert!(kind.schema().secret_keys().count() >= 1);
        }
        assert_eq!(CredentialKind::parse("other"), None);
    }

    #[test]
    fn a_complete_credential_is_accepted() {
        let schema = CredentialKind::Smtp.schema();

        let result = schema.validate(
            &fields(json!({"host": "h", "port": 587, "tls": "starttls", "username": "u"})),
            &secrets(&[("password", "p")]),
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn a_missing_required_field_is_refused_by_name() {
        let schema = CredentialKind::Basic.schema();

        let issues = schema
            .validate(&fields(json!({})), &secrets(&[]))
            .unwrap_err();

        let named: Vec<_> = issues
            .iter()
            .map(|found| (found.field.as_str(), found.code))
            .collect();
        assert_eq!(
            named,
            [
                ("username", IssueCode::Required),
                ("password", IssueCode::Required)
            ]
        );
    }

    #[test]
    fn an_unknown_field_is_refused_in_both_groups() {
        let schema = CredentialKind::SigningSecret.schema();

        let public = schema
            .validate_fields(&fields(json!({"extra": "x"})), false)
            .unwrap_err();
        let secret = schema
            .validate_secrets(&secrets(&[("extra", "x")]), false)
            .unwrap_err();

        assert_eq!(public[0].code, IssueCode::Unknown);
        assert_eq!(secret[0].code, IssueCode::Unknown);
    }

    #[test]
    fn a_wrong_type_or_option_is_refused() {
        let schema = CredentialKind::Smtp.schema();

        let issues = schema
            .validate_fields(
                &fields(json!({"host": 5, "port": "587", "tls": "plain"})),
                false,
            )
            .unwrap_err();

        let codes: Vec<_> = issues
            .iter()
            .map(|found| (found.field.as_str(), found.code))
            .collect();
        assert_eq!(
            codes,
            [
                ("host", IssueCode::WrongType),
                ("port", IssueCode::WrongType),
                ("tls", IssueCode::NotAnOption)
            ]
        );
    }

    #[test]
    fn a_url_field_takes_only_a_plain_http_url() {
        let schema = CredentialKind::OAuth2Client.schema();
        let check = |url: &str| schema.validate_fields(&fields(json!({"token_url": url})), false);

        assert!(check("https://auth.example.com/token").is_ok());
        for bad in [
            "ftp://a.example",
            "https://u:p@a.example",
            "not a url",
            "https://a.example/#x",
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_secret_value_in_a_non_secret_field_is_refused_and_so_is_the_reverse() {
        let schema = CredentialKind::Basic.schema();

        let hidden = schema
            .validate_fields(&fields(json!({"password": "hunter2hunter2"})), false)
            .unwrap_err();
        let shown = schema
            .validate_secrets(&secrets(&[("username", "u")]), false)
            .unwrap_err();

        assert_eq!(hidden[0].code, IssueCode::WrongPlace);
        assert_eq!(shown[0].code, IssueCode::WrongPlace);
        assert!(!format!("{hidden:?}").contains("hunter2"));
    }

    #[test]
    fn an_empty_secret_is_refused() {
        let schema = CredentialKind::SigningSecret.schema();

        let issues = schema
            .validate_secrets(&secrets(&[("signing_secret", "")]), true)
            .unwrap_err();

        assert_eq!(issues[0].code, IssueCode::BadLength);
    }

    #[test]
    fn a_schema_check_refuses_duplicates_bad_keys_secret_integers_and_empty_enums() {
        let field = |key: &str, field_type, secret| CredentialField {
            key: key.to_owned(),
            label: "L".to_owned(),
            field_type,
            required: true,
            secret,
            options: Vec::new(),
        };
        let bad = [
            CredentialSchema { fields: vec![] },
            CredentialSchema {
                fields: vec![
                    field("a", FieldType::String, false),
                    field("a", FieldType::String, false),
                ],
            },
            CredentialSchema {
                fields: vec![field("Bad Key", FieldType::String, false)],
            },
            CredentialSchema {
                fields: vec![field("n", FieldType::Integer, true)],
            },
            CredentialSchema {
                fields: vec![field("e", FieldType::Enum, false)],
            },
        ];

        for schema in bad {
            assert!(schema.check().is_err(), "{schema:?}");
        }
    }

    #[test]
    fn a_schema_round_trips_through_json() {
        let schema = CredentialKind::Smtp.schema();

        let again: CredentialSchema =
            serde_json::from_value(serde_json::to_value(&schema).unwrap()).unwrap();

        assert_eq!(again, schema);
    }

    #[test]
    fn a_hint_shows_the_last_four_characters_from_sixteen_characters_on() {
        assert_eq!(secret_hint("abcdefghijklmno"), "");
        assert_eq!(secret_hint("abcdefghijklmnop"), "mnop");
        assert_eq!(secret_hint(""), "");
        assert_eq!(secret_hint(&format!("{}1", "å".repeat(15))), "ååå1");
        assert_eq!(secret_hint(&format!("{}1", "å".repeat(14))), "");
    }

    #[test]
    fn a_fingerprint_is_stable_per_secret_and_differs_between_secrets() {
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);

        let one = fingerprint(&key, b"{\"k\":\"a\"}");

        assert_eq!(one, fingerprint(&key, b"{\"k\":\"a\"}"));
        assert_ne!(one, fingerprint(&key, b"{\"k\":\"b\"}"));
        assert_eq!(one.len(), 64);
    }
}
