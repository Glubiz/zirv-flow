//! The manifest's auth and the credential, as the runtime's [`Auth`].

use serde_json::Value;
use zirv_flow_core::SecretString;

use super::super::credential::Credential;
use super::super::error::SourceError;
use crate::manifest::{Auth as ManifestAuth, KeyLocation};
use crate::runtime::{ApiKeyPlacement, Auth, binding_of};

/// The credential key under which the caller puts the stored OAuth2 refresh token. Reserved: a
/// person cannot set it, because a credential is validated against the declared fields.
pub const REFRESH_TOKEN_KEY: &str = "oauth_refresh_token";

/// The credential key under which the caller puts the binding the stored refresh token was
/// issued for. Reserved like [`REFRESH_TOKEN_KEY`].
const REFRESH_BINDING_KEY: &str = "oauth_refresh_binding";

/// The binding a token fetched under the source's current `config` and `credential` carries, or
/// `None` when the manifest is not an authorization-code one. Never holds a secret.
fn active_binding(config: &Value, credential: &Credential) -> Option<String> {
    let manifest = super::manifest_of(config).ok()?;
    let ManifestAuth::OAuth2AuthorizationCode {
        token_url, scopes, ..
    } = &manifest.auth
    else {
        return None;
    };
    let client_id = credential.get("client_id")?;
    Some(binding_of(token_url, client_id.expose(), scopes))
}

/// Puts the stored refresh token into `credential` under the reserved keys, but only when it was
/// issued for the binding the source's current `config` has. Otherwise the credential is left
/// without it, and the source reads as not connected: a token never goes to a token endpoint,
/// or for scopes, it was not issued for. Says whether it was put there.
pub fn seed_refresh_token(
    credential: &mut Credential,
    config: &Value,
    refresh_token: &SecretString,
    issued_binding: &str,
) -> bool {
    if active_binding(config, credential).as_deref() != Some(issued_binding) {
        return false;
    }
    credential.insert(
        REFRESH_TOKEN_KEY,
        SecretString::new(refresh_token.expose().to_owned()),
    );
    credential.insert(
        REFRESH_BINDING_KEY,
        SecretString::new(issued_binding.to_owned()),
    );
    true
}

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
            refresh_binding: credential
                .get(REFRESH_BINDING_KEY)
                .map(SecretString::expose),
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

    fn code_config(token_url: &str, scopes: Value) -> Value {
        json!({"manifest": {
            "version": "v1",
            "base_url": "https://api.example.test",
            "auth": {"type": "oauth2_authorization_code", "authorize_url": "https://a.test/a",
                     "token_url": token_url, "scopes": scopes},
            "streams": [{"name": "s", "path": "/s",
                         "fields": [{"name": "id", "type": "string"}],
                         "primary_key": ["id"]}],
        }})
    }

    #[test]
    fn a_refresh_token_is_seeded_only_for_the_binding_it_was_issued_for() {
        let config = code_config("https://a.test/t", json!(["read"]));
        let issued = active_binding(&config, &credential(&[("client_id", "i")])).unwrap();
        let refresh = SecretString::new("rt".to_owned());

        let mut same = credential(&[("client_id", "i")]);
        assert!(seed_refresh_token(&mut same, &config, &refresh, &issued));
        assert_eq!(same.get(REFRESH_TOKEN_KEY).unwrap().expose(), "rt");
        assert_eq!(same.get(REFRESH_BINDING_KEY).unwrap().expose(), issued);

        for changed in [
            code_config("https://evil.test/t", json!(["read"])),
            code_config("https://a.test/t", json!(["read", "write"])),
        ] {
            let mut other = credential(&[("client_id", "i")]);
            assert!(!seed_refresh_token(&mut other, &changed, &refresh, &issued));
            assert!(other.get(REFRESH_TOKEN_KEY).is_none());
        }
        let mut other_client = credential(&[("client_id", "j")]);
        assert!(!seed_refresh_token(
            &mut other_client,
            &config,
            &refresh,
            &issued
        ));
    }
}
