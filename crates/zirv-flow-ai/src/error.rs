//! What can go wrong with a model call. Display and Debug never carry a key, a prompt, a response
//! body or a URL: only fixed text, a host name, an address class or a status code.

use std::error::Error;
use std::fmt;

use zirv_flow_core::egress::AddressClass;

use crate::response::Usage;

/// The fixed text of a transport failure after the request left, where the provider may already
/// have billed the call. The other transport texts mean it never reached the provider.
const SENT_REASONS: [&str; 2] = [
    "the response body could not be read",
    "the request could not be sent",
];

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
    /// The provider answered and billed the call, but the answer was unusable: `usage` is what it
    /// reported and `cause` why the answer failed. A caller that books cost reads the usage.
    Billed {
        usage: Usage,
        cause: Box<AiError>,
    },
    /// A tool call was cut off at the output cap. Repeating the request at the same cap would be
    /// cut off again and billed again, so this is not repaired. It holds what the call used.
    Truncated(Usage),
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
            Self::Billed { usage, cause } => write!(
                f,
                "{cause} (the call used {} input and {} output tokens)",
                usage.input_tokens, usage.output_tokens
            ),
            Self::Truncated(_) => f.write_str("the model's answer was cut off at the output limit"),
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
    /// An answer that failed after the provider billed it. A cause that already holds usage is
    /// kept as it is.
    pub(crate) fn billed(usage: Usage, cause: Self) -> Self {
        match cause {
            Self::Billed { .. } | Self::Truncated(_) => cause,
            cause => Self::Billed {
                usage,
                cause: Box::new(cause),
            },
        }
    }

    /// The error without its billing: a [`Billed`](Self::Billed) shows its cause.
    pub fn cause(&self) -> &Self {
        match self {
            Self::Billed { cause, .. } => cause,
            other => other,
        }
    }

    /// The tokens the provider reported, when the error holds them.
    pub fn usage(&self) -> Option<Usage> {
        match self {
            Self::Billed { usage, .. } | Self::Truncated(usage) => Some(*usage),
            _ => None,
        }
    }

    /// Whether the request was sent and the provider may have billed it although the error holds
    /// no usage: a timeout, a body that could not be read, or an answer that could not be
    /// understood. A refusal before the request left, or a status error, was not billed.
    pub fn may_have_billed(&self) -> bool {
        match self {
            Self::Timeout | Self::Malformed | Self::Capability(_) => true,
            Self::Transport(reason) => SENT_REASONS.contains(&reason.as_str()),
            _ => false,
        }
    }

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
            SENT_REASONS[0]
        } else if error.is_builder() {
            "the request could not be built"
        } else {
            SENT_REASONS[1]
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
    fn a_billed_error_shows_its_cause_and_its_tokens_and_exposes_both() {
        let usage = Usage {
            input_tokens: 40,
            output_tokens: 1024,
        };
        let error = AiError::billed(usage, AiError::Capability("tool_calls".to_owned()));
        assert_eq!(error.usage(), Some(usage));
        assert_eq!(error.cause(), &AiError::Capability("tool_calls".to_owned()));
        assert_eq!(
            error.to_string(),
            "the model lacks a required capability: tool_calls (the call used 40 input and 1024 output tokens)"
        );
        // Billing twice keeps the first record.
        assert_eq!(
            AiError::billed(Usage::default(), error).usage(),
            Some(usage)
        );
        assert_eq!(AiError::Timeout.usage(), None);
        assert_eq!(AiError::Truncated(usage).usage(), Some(usage));
        assert_eq!(
            AiError::Truncated(usage).cause(),
            &AiError::Truncated(usage)
        );
    }

    #[test]
    fn only_errors_after_the_request_left_may_have_billed() {
        for error in [
            AiError::Timeout,
            AiError::Malformed,
            AiError::Capability("usage".to_owned()),
            AiError::Transport("the request could not be sent".to_owned()),
            AiError::Transport("the response body could not be read".to_owned()),
        ] {
            assert!(error.may_have_billed(), "{error}");
        }
        for error in [
            AiError::Http(500),
            AiError::InvalidEndpoint,
            AiError::Transport("could not connect".to_owned()),
            AiError::Transport("a redirect was refused".to_owned()),
            AiError::Egress {
                host: "h".to_owned(),
                class: AddressClass::Private,
            },
            AiError::Disabled,
        ] {
            assert!(!error.may_have_billed(), "{error}");
        }
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
