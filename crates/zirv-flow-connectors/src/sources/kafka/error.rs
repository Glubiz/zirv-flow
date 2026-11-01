//! Errors as static codes. rskafka's own text carries broker addresses and, for some failures,
//! more, so none of it is ever shown, logged or stored: a failure maps to one of these.

use std::error::Error;

use rskafka::client::error::{Error as ClientError, ProtocolError, RequestError};

use super::super::error::SourceError;
use crate::http::HttpError;

pub(crate) const CONNECT_FAILED: &str = "kafka_connect_failed";
pub(crate) const TLS_FAILED: &str = "kafka_tls_failed";
pub(crate) const AUTH_FAILED: &str = "kafka_auth_failed";
pub(crate) const TOPIC_MISSING: &str = "kafka_topic_missing";
pub(crate) const OFFSET_OUT_OF_RANGE: &str = "kafka_offset_out_of_range";
pub(crate) const TIMEOUT: &str = "kafka_timeout";
pub(crate) const COMPRESSION_UNSUPPORTED: &str = "kafka_compression_unsupported";

/// The text rskafka gives a batch whose codec it was built without. Its codec is the only variable
/// part and comes from a fixed enum, never from broker text.
const UNIMPLEMENTED_COMPRESSION: &str = "Unimplemented compression";

/// The text of the error when the egress policy refused a broker address.
pub(super) const EGRESS_REFUSED: &str = "the broker address is not allowed by the egress policy";

pub(super) fn code(code: &'static str) -> SourceError {
    SourceError::Other(code.to_owned())
}

/// What a failed call is, as far as the reader acts on it.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum FetchError {
    /// The broker says the offset is outside the log.
    OffsetOutOfRange,
    /// The egress policy refused a broker address.
    Egress,
    Failed(&'static str),
}

impl From<FetchError> for SourceError {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::OffsetOutOfRange => code(OFFSET_OUT_OF_RANGE),
            FetchError::Egress => {
                SourceError::Egress(HttpError::Transport(EGRESS_REFUSED.to_owned()))
            }
            FetchError::Failed(reason) => code(reason),
        }
    }
}

/// Walks `error` and its sources for the cause the reader can name.
pub(super) fn classify(error: &(dyn Error + 'static)) -> FetchError {
    let mut found = FetchError::Failed(CONNECT_FAILED);
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(ClientError::ServerError { protocol_error, .. }) =
            error.downcast_ref::<ClientError>()
        {
            // UnknownTopicOrPartition is not named here: a broker answers it briefly during a
            // leader move, so it is a connect failure. A missing topic is told by the topic list.
            if matches!(protocol_error, ProtocolError::OffsetOutOfRange) {
                return FetchError::OffsetOutOfRange;
            }
        }
        // A fetch body that fails to decode arrives as `RequestError::ReadVersionedError`. The
        // type inside it is not exported (rskafka's `protocol` module is private), so its own
        // text is the only handle on a batch in a codec the client was built without.
        if let Some(ClientError::Request(RequestError::ReadVersionedError(inner))) =
            error.downcast_ref::<ClientError>()
            && inner.to_string().contains(UNIMPLEMENTED_COMPRESSION)
        {
            return FetchError::Failed(COMPRESSION_UNSUPPORTED);
        }
        if error.is::<rskafka::SaslError>() {
            return FetchError::Failed(AUTH_FAILED);
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>()
            && io
                .get_ref()
                .is_some_and(|inner| inner.is::<rustls::Error>())
        {
            return FetchError::Failed(TLS_FAILED);
        }
        if error.is::<rustls::Error>() {
            return FetchError::Failed(TLS_FAILED);
        }
        if matches!(
            error.downcast_ref::<ClientError>(),
            Some(ClientError::Timeout)
        ) {
            found = FetchError::Failed(TIMEOUT);
        }
        current = error.source();
    }
    if found == FetchError::Failed(CONNECT_FAILED) {
        return classify_text(error);
    }
    found
}

/// rskafka wraps what a retry gave up on in a type that has no `source`, so the cause is only
/// in its `Debug` text. The text is read for markers and dropped: it is never returned, logged
/// or stored.
fn classify_text(error: &(dyn Error + 'static)) -> FetchError {
    let text = format!("{error:?}").to_ascii_lowercase();
    if text.contains(&UNIMPLEMENTED_COMPRESSION.to_ascii_lowercase()) {
        return FetchError::Failed(COMPRESSION_UNSUPPORTED);
    }
    if text.contains("saslfailed") || text.contains("saslerror") {
        return FetchError::Failed(AUTH_FAILED);
    }
    let tls = [
        "invalidcertificate",
        "alertreceived",
        "peerincompatible",
        "badhostname",
        "unknownissuer",
        "handshakefailure",
    ];
    if tls.iter().any(|marker| text.contains(marker)) {
        return FetchError::Failed(TLS_FAILED);
    }
    FetchError::Failed(CONNECT_FAILED)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn a_timeout_a_refused_connection_and_a_rustls_failure_have_their_own_codes() {
        assert_eq!(classify(&ClientError::Timeout), FetchError::Failed(TIMEOUT));
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(classify(&refused), FetchError::Failed(CONNECT_FAILED));
        let tls = io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::General("bad".into()),
        );
        assert_eq!(classify(&tls), FetchError::Failed(TLS_FAILED));
    }

    #[test]
    fn a_server_error_for_the_offset_is_named_and_an_unknown_topic_is_a_retryable_connect_failure()
    {
        let server = |protocol_error| ClientError::ServerError {
            protocol_error,
            error_message: Some("secret broker text".into()),
            request: rskafka::client::error::RequestContext::Topic("t".into()),
            response: None,
            is_virtual: false,
        };

        assert_eq!(
            classify(&server(ProtocolError::OffsetOutOfRange)),
            FetchError::OffsetOutOfRange
        );
        assert_eq!(
            classify(&server(ProtocolError::UnknownTopicOrPartition)),
            FetchError::Failed(CONNECT_FAILED)
        );
    }

    #[test]
    fn a_batch_in_a_codec_the_client_lacks_is_named_and_a_malformed_one_is_not() {
        struct Wrapped(&'static str);
        impl std::fmt::Debug for Wrapped {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::fmt::Display for Wrapped {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("wrapped")
            }
        }
        impl Error for Wrapped {}

        assert_eq!(
            classify(&Wrapped("Malformed(\"Unimplemented compression: Zstd\")")),
            FetchError::Failed(COMPRESSION_UNSUPPORTED)
        );
        assert_eq!(
            classify(&Wrapped("Malformed(\"bad length\")")),
            FetchError::Failed(CONNECT_FAILED)
        );
    }

    #[test]
    fn a_code_converts_to_an_error_that_is_only_the_code() {
        assert_eq!(
            SourceError::from(FetchError::Failed(AUTH_FAILED)).to_string(),
            "kafka_auth_failed"
        );
        assert_eq!(
            SourceError::from(FetchError::OffsetOutOfRange).to_string(),
            "kafka_offset_out_of_range"
        );
    }
}
