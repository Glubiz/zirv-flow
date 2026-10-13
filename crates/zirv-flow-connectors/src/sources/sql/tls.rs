//! The TLS trust of a database connection. One rule for both drivers: a configured CA is the only
//! trust anchor, else the roots compiled into the binary. The system store is not used.

use std::sync::Arc;

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rustls::{ClientConfig, RootCertStore};

use super::settings::config_error;
use crate::sources::SourceError;

/// The certificates of a PEM text, or `None` if it holds no certificate or a malformed one.
pub(crate) fn certificates(pem: &str) -> Option<Vec<CertificateDer<'static>>> {
    let certificates: Result<Vec<_>, _> = CertificateDer::pem_slice_iter(pem.as_bytes()).collect();
    certificates.ok().filter(|found| !found.is_empty())
}

/// Why `pem` is not an acceptable `ca_certificate`, for the settings checks.
pub(crate) fn pem_problem(pem: &str) -> Option<&'static str> {
    let Some(certificates) = certificates(pem) else {
        return Some("must hold at least one PEM certificate");
    };
    let mut store = RootCertStore::empty();
    let (_, rejected) = store.add_parsable_certificates(certificates);
    (store.is_empty() || rejected > 0).then_some("holds a certificate that cannot be used as a CA")
}

/// The roots to verify the server against: the configured CA alone, else the compiled-in roots.
pub(crate) fn root_store(ca_certificate: Option<&str>) -> Result<RootCertStore, SourceError> {
    let mut store = RootCertStore::empty();
    match ca_certificate {
        Some(pem) => {
            let certificates = certificates(pem).ok_or_else(ca_error)?;
            for certificate in certificates {
                store.add(certificate).map_err(|_| ca_error())?;
            }
        }
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(store)
}

fn ca_error() -> SourceError {
    config_error(
        "ca_certificate",
        "must hold PEM certificates usable as a CA",
    )
}

/// The verifying client configuration, on the ring provider, with the default verifier and
/// protocol versions (TLS 1.2 and 1.3).
pub(crate) fn client_config(ca_certificate: Option<&str>) -> Result<ClientConfig, SourceError> {
    let roots = root_store(ca_certificate)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| SourceError::Other("the database connection could not be secured".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca_pem() -> String {
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().pem()
    }

    #[test]
    fn a_pem_certificate_is_accepted_and_text_that_is_not_one_is_not() {
        assert_eq!(pem_problem(&ca_pem()), None);
        for bad in [
            "",
            "hello",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        ] {
            assert!(pem_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn the_roots_are_the_configured_ca_alone_or_else_the_compiled_in_ones() {
        let own = root_store(Some(&ca_pem())).unwrap();
        let built_in = root_store(None).unwrap();

        assert_eq!(own.len(), 1);
        assert_eq!(built_in.len(), webpki_roots::TLS_SERVER_ROOTS.len());
        assert!(built_in.len() > 50);
    }

    #[test]
    fn a_client_config_builds_for_both_root_choices() {
        assert!(client_config(None).is_ok());
        assert!(client_config(Some(&ca_pem())).is_ok());
        assert!(client_config(Some("nope")).is_err());
    }
}
