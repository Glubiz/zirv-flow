//! Helpers for the tests of the database sources: an injected resolver, a generated CA, and the
//! environment variables the ignored tests need.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::TryStreamExt;
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::Value;
use tokio_rustls::TlsAcceptor;
use zirv_flow_core::SecretString;
use zirv_flow_core::egress::EgressPolicy;

use crate::http::{HttpLimits, Lookup};
use crate::sources::{
    Batch, Checkpoint, Credential, ErasedSource, SourceContext, SourceError, StreamConfig,
};

/// A resolver that answers every name with the same addresses.
pub(crate) struct Fixed(pub(crate) Vec<IpAddr>);

impl Lookup for Fixed {
    fn lookup<'a>(
        &'a self,
        _host: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

pub(crate) fn fixed(ips: &[&str]) -> Arc<dyn Lookup> {
    Arc::new(Fixed(ips.iter().map(|ip| ip.parse().unwrap()).collect()))
}

/// A context for source type `kind` whose egress policy allows `allow` and resolves through
/// `lookup`.
pub(crate) fn context(
    kind: &'static str,
    allow: &[&str],
    lookup: Arc<dyn Lookup>,
) -> SourceContext {
    let entries: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
    SourceContext::new(
        Arc::new(EgressPolicy::parse(&entries).unwrap()),
        HttpLimits::default(),
        kind,
        "source-1",
        Arc::default(),
    )
    .unwrap()
    .with_lookup(lookup)
}

/// A context that resolves with the system resolver and allows loopback.
pub(crate) fn loopback_context(kind: &'static str) -> SourceContext {
    SourceContext::new(
        Arc::new(EgressPolicy::parse(&["127.0.0.0/8".to_owned(), "::1/128".to_owned()]).unwrap()),
        HttpLimits::default(),
        kind,
        "source-1",
        Arc::default(),
    )
    .unwrap()
}

pub(crate) fn credential(user: &str, password: &str) -> Credential {
    let mut credential = Credential::new();
    credential.insert("user", SecretString::new(user.to_owned()));
    credential.insert("password", SecretString::new(password.to_owned()));
    credential
}

/// The value of a variable an ignored test needs. A test that needs a server it was not given
/// fails here, naming the variable, instead of passing without having run.
pub(crate) fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} must be set: this test needs a running database (see the README)")
    })
}

/// A short random suffix for the schema, database and role of one test.
pub(crate) fn suffix() -> String {
    format!("{:08x}", rand::random::<u32>())
}

/// Reads a stream to the end.
pub(crate) async fn read_all(
    source: &dyn ErasedSource,
    ctx: &SourceContext,
    config: &Value,
    credential: &Credential,
    stream: &StreamConfig,
    checkpoint: Option<Checkpoint>,
) -> Result<Vec<Batch>, SourceError> {
    source
        .read(ctx, config, credential, stream, checkpoint)
        .try_collect()
        .await
}

/// The keys of every record of `batches`, in order.
pub(crate) fn keys(batches: &[Batch]) -> Vec<String> {
    batches
        .iter()
        .flat_map(|batch| batch.records.iter().map(|record| record.key.clone()))
        .collect()
}

/// A generated CA and a server certificate it signed for `name`.
pub(crate) struct Pki {
    pub(crate) ca_pem: String,
    pub(crate) acceptor: TlsAcceptor,
}

pub(crate) fn pki(name: &str) -> Pki {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca_params, ca_key);

    let server_key = rcgen::KeyPair::generate().unwrap();
    let server = rcgen::CertificateParams::new(vec![name.to_owned()])
        .unwrap()
        .signed_by(&server_key, &issuer)
        .unwrap();
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![server.der().clone()], key)
            .unwrap();
    Pki {
        ca_pem: ca.pem(),
        acceptor: TlsAcceptor::from(Arc::new(config)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suffix_is_eight_hex_digits_and_differs_between_calls() {
        let (a, b) = (suffix(), suffix());

        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_generated_server_certificate_comes_with_the_ca_that_signed_it() {
        let pki = pki("db.test");

        assert!(crate::sources::sql::tls::pem_problem(&pki.ca_pem).is_none());
    }

    #[test]
    fn the_credential_helper_sets_both_fields() {
        let credential = credential("reader", "pw");

        assert_eq!(credential.get("user").unwrap().expose(), "reader");
        assert_eq!(credential.get("password").unwrap().expose(), "pw");
    }

    #[test]
    #[should_panic(expected = "ZIRV_FLOW_TEST_VARIABLE_THAT_IS_NOT_SET must be set")]
    fn a_missing_test_server_variable_fails_naming_it() {
        required_env("ZIRV_FLOW_TEST_VARIABLE_THAT_IS_NOT_SET");
    }
}
