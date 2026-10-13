//! The manifest's auth and the credential, as the runtime's [`Auth`].

use zirv_flow_core::SecretString;

use super::super::credential::Credential;
use super::super::error::SourceError;
use crate::manifest::{Auth as ManifestAuth, KeyLocation};
use crate::runtime::{ApiKeyPlacement, Auth};

/// The credential key under which the caller puts the stored OAuth2 refresh token. Reserved: a
/// person cannot set it, because a credential is validated against the declared fields.
pub const REFRESH_TOKEN_KEY: &str = "oauth_refresh_token";

fn missing(name: &str) -> SourceError {
    SourceError::Config {
        field: format!("credential.{name}"),
        reason: "is required".to_owned(),
    }
}

fn required<'a>(credential: &'a Credential, name: &str) -> Result<&'a SecretString, SourceError> {
    credential.get(name).ok_or_else(|| missing(name))
}

/// The runtime auth for `auth`, borrowing the secrets of `credential`. A credential field the
/// auth type needs and the source does not hold is a configuration error naming the field.
pub(super) fn build<'a>(
    auth: &'a ManifestAuth,
    credential: &'a Credential,
) -> Result<Auth<'a>, SourceError> {
    Ok(match auth {
        ManifestAuth::None => Auth::None,
        ManifestAuth::ApiKey { location, name } => Auth::ApiKey {
            placement: match location {
                KeyLocation::Header => ApiKeyPlacement::Header(name),
                KeyLocation::Query => ApiKeyPlacement::Query(name),
            },
            value: required(credential, "api_key")?,
        },
        ManifestAuth::Basic => Auth::Basic {
            username: required(credential, "username")?.expose(),
            password: required(credential, "password")?,
        },
        ManifestAuth::Bearer => Auth::Bearer {
            token: required(credential, "token")?,
        },
        ManifestAuth::OAuth2ClientCredentials {
            token_url,
            scopes,
            client_auth,
        } => Auth::OAuth2ClientCredentials {
            token_url: token_url.clone(),
            client_id: required(credential, "client_id")?.expose(),
            client_secret: required(credential, "client_secret")?,
            scopes,
            client_auth: *client_auth,
        },
        ManifestAuth::OAuth2AuthorizationCode {
            token_url,
            scopes,
            client_auth,
            ..
        } => Auth::OAuth2AuthorizationCode {
            token_url: token_url.clone(),
            client_id: required(credential, "client_id")?.expose(),
            client_secret: credential.get("client_secret"),
            // Not a field a person fills in: the consent put it there.
            refresh_token: credential.get(REFRESH_TOKEN_KEY).ok_or_else(|| {
                SourceError::Config {
                    field: "oauth".to_owned(),
                    reason: "is not connected: connect the account first".to_owned(),
                }
            })?,
            scopes,
            client_auth: *client_auth,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::validate;
    use serde_json::{Value, json};

    fn manifest_auth(auth: Value) -> ManifestAuth {
        let manifest = validate(&json!({
            "version": "v1",
            "base_url": "https://api.example.test",
            "auth": auth,
            "streams": [{"name": "s", "path": "/s",
                         "fields": [{"name": "id", "type": "string"}],
                         "primary_key": ["id"]}],
        }))
        .unwrap();
        manifest.auth
    }

    fn credential(fields: &[(&str, &str)]) -> Credential {
        let mut credential = Credential::new();
        for (key, value) in fields {
            credential.insert(*key, SecretString::new((*value).to_owned()));
        }
        credential
    }

    #[test]
    fn each_auth_type_reads_the_credential_fields_it_declares() {
        let cases = [
            (json!({"type": "none"}), vec![], "Auth::None"),
            (
                json!({"type": "api_key", "in": "query", "name": "key"}),
                vec![("api_key", "k")],
                "Auth::ApiKey",
            ),
            (
                json!({"type": "basic"}),
                vec![("username", "u"), ("password", "p")],
                "Auth::Basic",
            ),
            (
                json!({"type": "bearer"}),
                vec![("token", "t")],
                "Auth::Bearer",
            ),
            (
                json!({"type": "oauth2_client_credentials", "token_url": "https://a.test/t"}),
                vec![("client_id", "i"), ("client_secret", "s")],
                "Auth::OAuth2ClientCredentials",
            ),
            (
                json!({"type": "oauth2_authorization_code",
                       "authorize_url": "https://a.test/a", "token_url": "https://a.test/t"}),
                vec![("client_id", "i"), (REFRESH_TOKEN_KEY, "r")],
                "Auth::OAuth2AuthorizationCode",
            ),
        ];
        for (auth, fields, expected) in cases {
            let auth = manifest_auth(auth);
            let credential = credential(&fields);

            let built = build(&auth, &credential).unwrap();

            assert_eq!(format!("{built:?}"), expected);
        }
    }

    #[test]
    fn a_missing_field_is_a_config_error_naming_it_and_never_a_value() {
        let auth = manifest_auth(json!({"type": "basic"}));

        let error = build(&auth, &credential(&[("username", "u")])).unwrap_err();

        assert_eq!(
            error,
            SourceError::Config {
                field: "credential.password".into(),
                reason: "is required".into()
            }
        );
    }

    #[test]
    fn an_authorization_code_source_without_a_consent_says_so() {
        let auth = manifest_auth(json!({"type": "oauth2_authorization_code",
            "authorize_url": "https://a.test/a", "token_url": "https://a.test/t"}));

        let error = build(&auth, &credential(&[("client_id", "i")])).unwrap_err();

        assert!(matches!(error, SourceError::Config { field, .. } if field == "oauth"));
    }
}
