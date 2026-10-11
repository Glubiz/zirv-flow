//! Why an identity-provider call failed.

use std::error::Error;
use std::fmt;

/// A failed call to the identity provider.
///
/// The messages name the step that failed and nothing else: no token, code, secret or claim
/// value ever appears in `Display` or `Debug`, because both end up in logs.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum IdentityError {
    /// The settings cannot be used, for example an unparsable URL.
    Settings(&'static str),
    /// Discovery or the key set could not be fetched or was not valid.
    Discovery,
    /// The provider refused or failed the code exchange.
    Exchange,
    /// The ID token did not verify: signature, issuer, audience, expiry or nonce.
    InvalidToken,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Settings(what) => write!(formatter, "invalid OIDC settings: {what}"),
            Self::Discovery => formatter.write_str("OIDC discovery failed"),
            Self::Exchange => formatter.write_str("OIDC code exchange failed"),
            Self::InvalidToken => formatter.write_str("the OIDC ID token is not valid"),
        }
    }
}

impl fmt::Debug for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl Error for IdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_names_its_step_in_display_and_debug() {
        for (error, text) in [
            (IdentityError::Discovery, "discovery"),
            (IdentityError::Exchange, "exchange"),
            (IdentityError::InvalidToken, "ID token"),
            (IdentityError::Settings("the issuer URL"), "the issuer URL"),
        ] {
            assert!(error.to_string().contains(text), "{error}");
            assert_eq!(error.to_string(), format!("{error:?}"));
        }
    }
}
