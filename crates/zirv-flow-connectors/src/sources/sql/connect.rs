//! The connect point both database sources share: resolve, check, and the rules that depend on
//! the checked addresses.

use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use zirv_flow_core::egress::{AddressClass, classify};

use super::error;
use super::settings::{Settings, TlsMode, config_error};
use crate::sources::{SourceContext, SourceError};

/// How long a connection attempt may take; the MySQL driver has no timeout of its own.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Added to the statement timeout for the outer limit on a driver call: the server-side limit is
/// the primary bound, this the backstop for a stalled network.
const BACKSTOP_MARGIN: Duration = Duration::from_secs(5);

/// Resolves the configured host and checks every address against the egress policy. Call it at
/// every connect. The returned addresses are the only thing handed to a driver.
pub(crate) async fn resolve(
    ctx: &SourceContext,
    settings: &Settings,
) -> Result<Vec<IpAddr>, SourceError> {
    let ips = ctx.resolve_checked(&settings.host).await?;
    tls_allowed(settings.tls, &ips)?;
    Ok(ips)
}

/// `tls = off` is accepted only when no checked address is public: plaintext on a public network
/// is exactly where an attacker sits between the server and the database.
pub(crate) fn tls_allowed(mode: TlsMode, ips: &[IpAddr]) -> Result<(), SourceError> {
    if mode == TlsMode::Off && ips.iter().any(|ip| classify(*ip) == AddressClass::Public) {
        return Err(config_error(
            "tls",
            "cannot be off toward a public address; allowlist the range in the egress settings",
        ));
    }
    Ok(())
}

/// The limit on one driver call for a statement timeout of `statement_timeout`.
pub(crate) fn backstop(statement_timeout: Duration) -> Duration {
    statement_timeout + BACKSTOP_MARGIN
}

/// Runs `future` for at most `limit`; running out is the fixed timeout error.
pub(crate) async fn within<T>(
    limit: Duration,
    future: impl Future<Output = Result<T, SourceError>>,
) -> Result<T, SourceError> {
    match tokio::time::timeout(limit, future).await {
        Ok(result) => result,
        Err(_) => Err(error::timeout()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn tls_off_is_refused_toward_a_public_address_and_allowed_toward_private_ones() {
        assert!(tls_allowed(TlsMode::Verify, &[ip("8.8.8.8")]).is_ok());
        assert!(tls_allowed(TlsMode::Off, &[ip("10.0.0.5"), ip("127.0.0.1")]).is_ok());
        for ips in [vec![ip("8.8.8.8")], vec![ip("10.0.0.5"), ip("8.8.8.8")]] {
            assert_eq!(
                tls_allowed(TlsMode::Off, &ips).unwrap_err(),
                config_error(
                    "tls",
                    "cannot be off toward a public address; allowlist the range in the egress settings"
                )
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_outlasts_its_limit_is_the_fixed_timeout_error() {
        let slow = within(Duration::from_secs(1), async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        })
        .await;
        let fast = within(Duration::from_secs(1), async { Ok(7) }).await;

        assert_eq!(slow, Err(error::timeout()));
        assert_eq!(fast, Ok(7));
    }

    #[test]
    fn the_backstop_is_the_statement_timeout_plus_a_margin() {
        assert_eq!(backstop(Duration::from_secs(60)), Duration::from_secs(65));
    }
}
