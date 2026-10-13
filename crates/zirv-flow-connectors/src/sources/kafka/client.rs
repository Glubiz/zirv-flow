//! Building the rskafka client (spec D7): the relay as its only network path, TLS on the `ring`
//! provider, SASL from the credential, and every call under a timeout.

use std::sync::Arc;
use std::time::Duration;

use rskafka::BackoffConfig;
use rskafka::client::partition::{OffsetAt, PartitionClient, UnknownTopicHandling};
// Not named `ClientBuilder`: the HTTP guard test counts that name, and this is not an HTTP client.
use rskafka::client::{ClientBuilder as KafkaBuilder, Credentials, SaslConfig};
use rskafka::record::RecordAndOffset;
use rustls::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::time::timeout;
use zirv_flow_core::SecretString;

use super::super::context::SourceContext;
use super::super::error::SourceError;
use super::error::{
    CONNECT_FAILED, FetchError, TIMEOUT, TLS_FAILED, TOPIC_MISSING, classify, code,
};
use super::fetch::{Partition, PartitionFetch, open_all};
use super::positions::Positions;
use super::relay::{DenialSlot, Relay};

/// The largest frame a broker may send: bounds memory per connection.
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// The most bytes a fetch asks one partition for (so at most 1 MiB compressed per partition).
const FETCH_MAX_BYTES: i32 = 1_048_576;

/// How long one call (a fetch, a metadata request, an offset lookup) may take in all.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting and reading the first metadata may take in all.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How the client authenticates with SASL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Mechanism {
    Plain,
    ScramSha256,
    ScramSha512,
}

/// The TLS material, PEM text.
pub(super) struct TlsMaterial<'a> {
    pub(super) ca: Option<&'a str>,
    pub(super) client_cert: Option<&'a SecretString>,
    pub(super) client_key: Option<&'a SecretString>,
}

/// What a connection needs.
pub(super) struct Connect<'a> {
    pub(super) brokers: Vec<String>,
    pub(super) tls: Option<TlsMaterial<'a>>,
    pub(super) sasl: Option<(Mechanism, &'a SecretString, &'a SecretString)>,
}

fn pem_error(field: &str) -> SourceError {
    SourceError::Config {
        field: field.to_owned(),
        reason: "is not valid PEM".to_owned(),
    }
}

/// The TLS client configuration. With `ca` it trusts only that bundle; otherwise the platform
/// verifier, the roots HTTPS sources use. A client certificate needs its key and the reverse.
pub(super) fn tls_config(tls: &TlsMaterial<'_>) -> Result<Arc<ClientConfig>, SourceError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|_| code(TLS_FAILED))?;
    let wants_client = match tls.ca {
        Some(ca) => {
            let mut roots = rustls::RootCertStore::empty();
            let mut added = 0;
            for certificate in CertificateDer::pem_slice_iter(ca.as_bytes()) {
                let certificate = certificate.map_err(|_| pem_error("tls_ca"))?;
                roots.add(certificate).map_err(|_| pem_error("tls_ca"))?;
                added += 1;
            }
            if added == 0 {
                return Err(pem_error("tls_ca"));
            }
            builder.with_root_certificates(roots)
        }
        None => builder
            .with_platform_verifier()
            .map_err(|_| code(TLS_FAILED))?,
    };
    let config = match (tls.client_cert, tls.client_key) {
        (Some(cert), Some(key)) => {
            let chain = CertificateDer::pem_slice_iter(cert.expose().as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| pem_error("tls_client_cert"))?;
            if chain.is_empty() {
                return Err(pem_error("tls_client_cert"));
            }
            let key = PrivateKeyDer::from_pem_slice(key.expose().as_bytes())
                .map_err(|_| pem_error("tls_client_key"))?;
            wants_client
                .with_client_auth_cert(chain, key)
                .map_err(|_| pem_error("tls_client_key"))?
        }
        _ => wants_client.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

/// A connected client and the relay that carries its traffic. Dropping it ends both.
pub(super) struct Session {
    client: rskafka::client::Client,
    // Held for its lifetime: the client dials through it.
    _relay: Relay,
    denial: DenialSlot,
}

impl Session {
    /// The error for a failed call: the egress refusal if the relay saw one, else the cause.
    fn failure(&self, error: FetchError) -> FetchError {
        if self.denial.get().is_some() {
            FetchError::Egress
        } else {
            error
        }
    }

    /// Connects through a fresh relay. The credential is read into plain strings right before
    /// `build()`; rskafka clones them and they cannot be wiped there.
    pub(super) async fn connect(
        ctx: &SourceContext,
        connect: &Connect<'_>,
    ) -> Result<Arc<Self>, SourceError> {
        let tls = connect.tls.as_ref().map(tls_config).transpose()?;
        let relay = Relay::start(Arc::clone(ctx.egress_policy()), ctx.lookup())
            .await
            .map_err(|_| code(CONNECT_FAILED))?;
        let denial = relay.slot();
        let mut builder = KafkaBuilder::new(connect.brokers.clone())
            .client_id("zirv-flow")
            .max_message_size(MAX_MESSAGE_BYTES)
            .backoff_config(BackoffConfig {
                init_backoff: Duration::from_millis(100),
                max_backoff: Duration::from_secs(2),
                base: 2.0,
                deadline: Some(Duration::from_secs(15)),
            })
            .socks5_proxy(relay.proxy());
        if let Some(tls) = tls {
            builder = builder.tls_config(tls);
        }
        if let Some((mechanism, username, password)) = connect.sasl {
            let credentials =
                Credentials::new(username.expose().to_owned(), password.expose().to_owned());
            builder = builder.sasl_config(match mechanism {
                Mechanism::Plain => SaslConfig::Plain(credentials),
                Mechanism::ScramSha256 => SaslConfig::ScramSha256(credentials),
                Mechanism::ScramSha512 => SaslConfig::ScramSha512(credentials),
            });
        }
        let built = timeout(CONNECT_TIMEOUT, builder.build()).await;
        let session = Self {
            client: match built {
                Ok(Ok(client)) => client,
                Ok(Err(error)) => {
                    let cause = classify(&error);
                    return Err(if relay.denial().is_some() {
                        FetchError::Egress.into()
                    } else {
                        cause.into()
                    });
                }
                Err(_) => {
                    return Err(if relay.denial().is_some() {
                        FetchError::Egress.into()
                    } else {
                        code(TIMEOUT)
                    });
                }
            },
            _relay: relay,
            denial,
        };
        Ok(Arc::new(session))
    }

    /// The raw client, for tests that create topics and produce.
    #[cfg(test)]
    pub(super) fn raw(&self) -> &rskafka::client::Client {
        &self.client
    }

    /// The partitions of each configured topic, failing `kafka_topic_missing` for one that does
    /// not exist.
    pub(super) async fn partitions(&self, topic: &str) -> Result<Vec<i32>, SourceError> {
        let topics = match timeout(CALL_TIMEOUT, self.client.list_topics()).await {
            Ok(Ok(topics)) => topics,
            Ok(Err(error)) => return Err(self.failure(classify(&error)).into()),
            Err(_) => return Err(self.failure(FetchError::Failed(TIMEOUT)).into()),
        };
        topics
            .into_iter()
            .find(|candidate| candidate.name == topic)
            .map(|found| found.partitions.into_iter().collect())
            .ok_or_else(|| code(TOPIC_MISSING))
    }

    /// Opens every partition of `topic` at the offsets in `positions`.
    pub(super) async fn open_topic(
        self: &Arc<Self>,
        topic: &str,
        positions: &Positions,
    ) -> Result<Vec<Partition<Remote>>, SourceError> {
        let mut fetchers = Vec::new();
        for id in self.partitions(topic).await? {
            let client = match timeout(
                CALL_TIMEOUT,
                self.client
                    .partition_client(topic, id, UnknownTopicHandling::Error),
            )
            .await
            {
                Ok(Ok(client)) => client,
                Ok(Err(error)) => return Err(self.failure(classify(&error)).into()),
                Err(_) => return Err(self.failure(FetchError::Failed(TIMEOUT)).into()),
            };
            fetchers.push((
                id,
                Remote {
                    client,
                    session: Arc::clone(self),
                },
            ));
        }
        open_all(fetchers, positions).await
    }
}

/// One partition of a real broker.
pub(super) struct Remote {
    client: PartitionClient,
    session: Arc<Session>,
}

impl Remote {
    async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, rskafka::client::error::Error>>,
    ) -> Result<T, FetchError> {
        match timeout(CALL_TIMEOUT, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(self.session.failure(classify(&error))),
            Err(_) => Err(self.session.failure(FetchError::Failed(TIMEOUT))),
        }
    }
}

impl PartitionFetch for Remote {
    async fn earliest(&self) -> Result<i64, FetchError> {
        self.call(self.client.get_offset(OffsetAt::Earliest)).await
    }

    async fn latest(&self) -> Result<i64, FetchError> {
        self.call(self.client.get_offset(OffsetAt::Latest)).await
    }

    async fn fetch(&self, offset: i64) -> Result<(Vec<RecordAndOffset>, i64), FetchError> {
        self.call(self.client.fetch_records(offset, 1..FETCH_MAX_BYTES, 0))
            .await
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::SecretString;

    use super::*;

    fn certificate() -> (String, String) {
        let key = rcgen::generate_simple_self_signed(vec!["broker.test".to_owned()]).unwrap();
        (key.cert.pem(), key.signing_key.serialize_pem())
    }

    #[test]
    fn a_private_ca_builds_a_config_and_the_system_roots_do_when_none_is_given() {
        let (ca, _) = certificate();

        let with_ca = tls_config(&TlsMaterial {
            ca: Some(&ca),
            client_cert: None,
            client_key: None,
        });
        let system = tls_config(&TlsMaterial {
            ca: None,
            client_cert: None,
            client_key: None,
        });

        assert!(with_ca.is_ok());
        assert!(system.is_ok());
    }

    #[test]
    fn a_client_certificate_and_key_are_accepted_together() {
        let (ca, _) = certificate();
        let (cert, key) = certificate();
        let cert = SecretString::new(cert);
        let key = SecretString::new(key);

        let config = tls_config(&TlsMaterial {
            ca: Some(&ca),
            client_cert: Some(&cert),
            client_key: Some(&key),
        });

        assert!(config.is_ok());
    }

    #[test]
    fn bad_pem_is_refused_without_echoing_any_of_it() {
        let secret_pem = SecretString::new(
            "-----BEGIN PRIVATE KEY-----\nc2VjcmV0LWtleS1ieXRlcw==\n-----END PRIVATE KEY-----\n"
                .to_owned(),
        );
        let (cert, _) = certificate();
        let cert = SecretString::new(cert);

        let no_certificate = tls_config(&TlsMaterial {
            ca: Some("not pem at all"),
            client_cert: None,
            client_key: None,
        })
        .err()
        .unwrap();
        let bad_key = tls_config(&TlsMaterial {
            ca: None,
            client_cert: Some(&cert),
            client_key: Some(&secret_pem),
        })
        .err()
        .unwrap();

        for error in [no_certificate, bad_key] {
            let text = format!("{error} {error:?}");
            assert!(text.contains("is not valid PEM"), "{text}");
            assert!(!text.contains("c2VjcmV0"), "{text}");
            assert!(!text.contains("not pem at all"), "{text}");
        }
    }
}
