//! What can go wrong with a model call. Display and Debug never carry a key, a prompt, a response
//! body or a URL: only fixed text, a host name, an address class or a status code.

use std::error::Error;
use std::fmt;

use zirv_flow_core::egress::AddressClass;

/// Why a model call, or the construction of its request, failed.
#[derive(Debug, Eq, PartialEq)]
pub enum AiError {
    /// The egress policy refused the provider's address.
    Egress {
        host: String,
        class: AddressClass,
    },
    /// The provider answered with a non-success status.
    Http(u16),
    Timeout,
    /// The provider lacks a capability the product needs: `tool_calls` or `usage`.
    Capability(String),
    /// The response could not be understood.
    Malformed,
    /// The connection failed for a reason other than the egress policy.
    Transport(String),
    /// The configured base URL cannot be called.
    InvalidEndpoint,
    /// A tool schema outside the supported subset.
    Schema(String),
    /// Personal data was offered to a request built without the workspace's opt-in.
    PersonalDataNotAllowed,
    BudgetExceeded,
    RateLimited,
    Disabled,
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Egress { host, class } => {
                write!(f, "egress denied: {host} is a {class} address")
            }
            Self::Http(status) => write!(f, "the provider answered with status {status}"),
            Self::Timeout => f.write_str("the request timed out"),
            Self::Capability(capability) => {
                write!(f, "the model lacks a required capability: {capability}")
            }
            Self::Malformed => f.write_str("the provider's response could not be understood"),
            Self::Transport(reason) => write!(f, "the request failed: {reason}"),
            Self::InvalidEndpoint => f.write_str("the provider's base URL cannot be called"),
            Self::Schema(reason) => write!(f, "unsupported tool schema: {reason}"),
            Self::PersonalDataNotAllowed => {
                f.write_str("personal data is not allowed in this workspace's AI requests")
            }
            Self::BudgetExceeded => f.write_str("the workspace's monthly AI budget is spent"),
            Self::RateLimited => f.write_str("too many AI calls; try again in a minute"),
            Self::Disabled => f.write_str("the AI assistant is not enabled for this workspace"),
        }
    }
}

impl Error for AiError {}

/// The error the resolver raises for a refused destination. It travels inside reqwest's error
/// chain and is recovered by [`AiError::from_reqwest`].
#[derive(Debug)]
pub(crate) struct Denied {
    pub(crate) host: String,
    pub(crate) class: AddressClass,
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "egress denied: {} is a {} address",
            self.host, self.class
        )
    }
}

impl Error for Denied {}

impl AiError {
    /// Maps a reqwest failure, looking through its source chain for a refusal we raised.
    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        let mut source: Option<&(dyn Error + 'static)> = Some(&error);
        while let Some(current) = source {
            if let Some(denied) = current.downcast_ref::<Denied>() {
                return Self::Egress {
                    host: denied.host.clone(),
                    class: denied.class,
                };
            }
            source = current.source();
        }
        if error.is_timeout() {
            return Self::Timeout;
        }
        // Fixed descriptions: reqwest's own text can name the URL.
        let reason = if error.is_connect() {
            "could not connect"
        } else if error.is_redirect() {
            "a redirect was refused"
        } else if error.is_body() || error.is_decode() {
            "the response body could not be read"
        } else if error.is_builder() {
            "the request could not be built"
        } else {
            "the request could not be sent"
        };
        Self::Transport(reason.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_fixed_text_a_host_or_a_status() {
        let denied = AiError::Egress {
            host: "models.internal".to_owned(),
            class: AddressClass::Private,
        };
        assert_eq!(
            denied.to_string(),
            "egress denied: models.internal is a private address"
        );
        assert_eq!(
            AiError::Http(503).to_string(),
            "the provider answered with status 503"
        );
        assert_eq!(
            AiError::BudgetExceeded.to_string(),
            "the workspace's monthly AI budget is spent"
        );
        for error in [
            AiError::Timeout,
            AiError::Malformed,
            AiError::InvalidEndpoint,
            AiError::PersonalDataNotAllowed,
            AiError::RateLimited,
            AiError::Disabled,
        ] {
            assert!(!error.to_string().contains("http"), "{error}");
        }
    }

    #[test]
    fn the_internal_denial_describes_itself() {
        let denied = Denied {
            host: "h".to_owned(),
            class: AddressClass::Metadata,
        };
        assert_eq!(denied.to_string(), "egress denied: h is a metadata address");
    }

    #[test]
    fn a_capability_error_names_the_capability() {
        let error = AiError::Capability("usage".to_owned());
        assert_eq!(
            error.to_string(),
            "the model lacks a required capability: usage"
        );
    }
}
