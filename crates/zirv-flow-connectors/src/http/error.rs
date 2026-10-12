//! What can go wrong with a guarded request. Messages never carry credentials or query strings:
//! a URL is never formatted into an error.

use std::error::Error;
use std::fmt;

use zirv_flow_core::egress::AddressClass;

/// Why a guarded request failed.
#[derive(Debug, Eq, PartialEq)]
pub enum HttpError {
    /// The egress policy refused the destination.
    EgressDenied {
        host: String,
        class: AddressClass,
    },
    /// The response body went over the cap.
    BodyTooLarge,
    Timeout,
    /// Any other failure, described without the URL.
    Transport(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EgressDenied { host, class } => {
                write!(f, "egress denied: {host} is a {class} address")
            }
            Self::BodyTooLarge => f.write_str("the response body is larger than the allowed size"),
            Self::Timeout => f.write_str("the request timed out"),
            Self::Transport(reason) => write!(f, "the request failed: {reason}"),
        }
    }
}

impl Error for HttpError {}

/// The error the resolver and the redirect policy raise for a refused destination. It travels
/// inside reqwest's error chain and is recovered by [`HttpError::from_reqwest`].
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

impl From<Denied> for HttpError {
    fn from(denied: Denied) -> Self {
        Self::EgressDenied {
            host: denied.host,
            class: denied.class,
        }
    }
}

/// The redirect policy's refusal to follow another hop.
#[derive(Debug)]
pub(crate) struct TooManyRedirects;

impl fmt::Display for TooManyRedirects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("too many redirects")
    }
}

impl Error for TooManyRedirects {}

impl HttpError {
    /// Maps a reqwest failure, looking through its source chain for a refusal we raised.
    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        let mut source: Option<&(dyn Error + 'static)> = Some(&error);
        while let Some(current) = source {
            if let Some(denied) = current.downcast_ref::<Denied>() {
                return Self::EgressDenied {
                    host: denied.host.clone(),
                    class: denied.class,
                };
            }
            if current.is::<TooManyRedirects>() {
                return Self::Transport("too many redirects".to_owned());
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
            "a redirect could not be followed"
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
    fn messages_name_the_host_and_class_but_no_url() {
        let denied = HttpError::EgressDenied {
            host: "internal.example".to_owned(),
            class: AddressClass::Private,
        };
        assert_eq!(
            denied.to_string(),
            "egress denied: internal.example is a private address"
        );
        assert_eq!(
            HttpError::Transport("could not connect".to_owned()).to_string(),
            "the request failed: could not connect"
        );
        assert!(!HttpError::Timeout.to_string().contains("http"));
        assert!(!HttpError::BodyTooLarge.to_string().contains("http"));
    }

    #[test]
    fn the_internal_errors_describe_themselves() {
        let denied = Denied {
            host: "h".to_owned(),
            class: AddressClass::Metadata,
        };
        assert_eq!(denied.to_string(), "egress denied: h is a metadata address");
        assert_eq!(TooManyRedirects.to_string(), "too many redirects");
    }
}
