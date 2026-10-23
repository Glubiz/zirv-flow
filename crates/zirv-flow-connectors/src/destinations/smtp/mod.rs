//! The built-in `smtp` destination: one email through a mail server or relay (architecture 10.1 and
//! 10.2).
//!
//! **Crate choice.** A small MIME builder ([`message`]) and a minimal SMTP client ([`client`]) over
//! `tokio-rustls`, not `lettre`. The client must dial an address the egress policy already checked
//! (`DestinationContext::resolve_checked`) and use the host name as the TLS server name; `lettre`
//! resolves the host name itself when it connects and has no way to be handed a connected socket,
//! so it would bypass the policy. TLS is rustls on the `ring` provider, as everywhere else in the
//! workspace: no aws-lc and no OpenSSL.
//!
//! **The conversation.** Resolve and check, connect (every address in turn, each within a
//! timeout), TLS (implicit from the first byte, or `STARTTLS` and then `EHLO` again; a server that
//! does not offer `STARTTLS` is not talked to in the clear), `AUTH PLAIN` or `LOGIN` with the
//! workspace credential's `username` and `password`, `MAIL`, `RCPT`, `DATA` with dot-stuffing,
//! `QUIT`. Every step has a timeout and every read a bound ([`client`]). The server's `host`,
//! `port` and `tls` mode come from the same `smtp` credential; the destination's own
//! configuration is only the optional `ca_certificate`. Plaintext (`tls: none`) is allowed only
//! in a test build or with `test-support`; anywhere else the check and a delivery refuse it as a
//! configuration error before any connection.
//!
//! **The message.** `multipart/alternative` of the text and the HTML part, the template's extra
//! headers (`List-Unsubscribe` and `List-Unsubscribe-Post`), a `Message-ID` and an
//! `X-Zirv-Idempotency-Key` that are both the message id, so a retry carries the same ones. It is
//! signed with the sender domain's DKIM key, which the send pipeline opens and passes in
//! [`Outbound::dkim`]; a message with none is refused before any connection is made.
//!
//! **Classification (OQ-12)** is in [`classify`]: a permanent recipient failure at `RCPT` or
//! `DATA` is [`Refusal::HardBounce`], other permanent refusals are [`Refusal::Rejected`], a 4xx
//! is [`Refusal::Deferred`].

mod classify;
mod client;
mod message;
#[cfg(any(test, feature = "test-support"))]
mod sink;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use zeroize::Zeroizing;

use self::classify::{Stage, refusal};
use self::client::{
    DATA_TIMEOUT, QUIT_TIMEOUT, Reply, STEP_TIMEOUT, Session, WireError, dot_stuffed,
};
use super::context::DestinationContext;
use super::destination::{Delivered, Destination, Outbound};
use super::error::DestinationError;
use super::spec::{Capabilities, DestinationSpec};
use crate::http::HttpError;
use crate::sources::{Credential, FieldError, FieldKind, FieldSpec};

#[cfg(feature = "test-support")]
pub use self::sink::{
    Captured, SINK_HOST, SINK_PASSWORD, SINK_USER, SinkOptions, SinkStage, SinkTls, SmtpSink,
};

/// How long one address gets to accept the connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest message this destination sends, whatever the server accepts.
const MAX_MESSAGE_BYTES: usize = 25 * 1024 * 1024;
/// The name given to `EHLO` when no message says which domain is sending (the connection check).
const CHECK_HELO: &str = "localhost";

/// Whether this build may speak plaintext: test builds only, for the sink.
const PLAINTEXT_ALLOWED: bool = cfg!(any(test, feature = "test-support"));

pub(crate) struct Smtp;

impl Destination for Smtp {
    const SPEC: DestinationSpec = DestinationSpec {
        kind: "smtp",
        label: "SMTP",
        description: "Email through a mail server or relay, signed with the sending domain's DKIM key.",
        config_fields: &[FieldSpec {
            key: "ca_certificate",
            label: "CA certificate",
            help: "Trust only this certificate authority (PEM) for the server, such as a private relay's. Empty trusts the public roots.",
            kind: FieldKind::LongText,
            required: false,
            secret: false,
        }],
        // The five fields of the built-in `smtp` workspace credential kind.
        credential_fields: &[
            FieldSpec {
                key: "host",
                label: "Host",
                help: "The mail server's name, as its certificate states it.",
                kind: FieldKind::Text,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "port",
                label: "Port",
                help: "Usually 587 for STARTTLS and 465 for implicit TLS.",
                kind: FieldKind::Number,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "tls",
                label: "TLS mode",
                help: "starttls upgrades the connection after the greeting; tls secures it from the first byte.",
                kind: FieldKind::Select(&["starttls", "tls", "none"]),
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "username",
                label: "Username",
                help: "Authenticates the connection, together with the password.",
                kind: FieldKind::Text,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "password",
                label: "Password",
                help: "Authenticates the connection, together with the username.",
                kind: FieldKind::Text,
                required: true,
                secret: true,
            },
        ],
        capabilities: Capabilities {
            channels: &["email"],
            idempotency_key: true,
            batching: false,
            rate_limit: true,
            erase: false,
            signs_dkim: true,
        },
        config_check: Some(config_check),
    };

    async fn check(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), DestinationError> {
        let settings = Settings::parse(config, credential, PLAINTEXT_ALLOWED)?;
        let auth = Auth::of(credential)?;
        run(ctx, &settings, &auth, CHECK_HELO, None).await?;
        Ok(())
    }

    async fn deliver(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        message: &Outbound<'_>,
    ) -> Result<Delivered, DestinationError> {
        let settings = Settings::parse(config, credential, PLAINTEXT_ALLOWED)?;
        let auth = Auth::of(credential)?;
        let built = message::build(
            message.payload,
            message.to,
            message.idempotency_key,
            Utc::now(),
        )?;
        // No key, no connection: an unsigned message from a sending domain is never sent.
        let Some(signer) = message.dkim else {
            return Err(DestinationError::Config {
                field: "dkim".to_owned(),
                reason: "is required: the sender domain's key was not provided".to_owned(),
            });
        };
        if !signer.domain().eq_ignore_ascii_case(&built.from_domain) {
            return Err(DestinationError::Config {
                field: "dkim".to_owned(),
                reason: "is for another domain than the sender's".to_owned(),
            });
        }
        let now = Utc::now();
        let body = built.body.as_slice();
        let signature = signer
            .sign(&built.headers, body, now.timestamp())
            .map_err(|_| DestinationError::Config {
                field: "dkim".to_owned(),
                reason: "could not sign the message".to_owned(),
            })?;
        let wire = built.wire(Some(&signature));
        if wire.len() > MAX_MESSAGE_BYTES {
            return Err(DestinationError::Refused(super::error::Refusal::Rejected));
        }
        let envelope = Envelope {
            from: &built.from_address,
            to: message.to,
            data: dot_stuffed(&wire),
            size: wire.len(),
        };
        run(ctx, &settings, &auth, &built.from_domain, Some(&envelope)).await
    }
}

/// Checks the CA certificate, the only setting the configuration holds.
fn config_check(config: &Value) -> Vec<FieldError> {
    match config.get("ca_certificate").and_then(Value::as_str) {
        Some(pem) if !pem.trim().is_empty() && root_store(Some(pem)).is_none() => {
            vec![FieldError {
                field: "ca_certificate".to_owned(),
                reason: "must hold PEM certificates usable as a CA".to_owned(),
            }]
        }
        _ => Vec::new(),
    }
}

fn host_ok(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
        || (!host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')))
}

/// The roots to verify the server against: the configured CA alone, else the compiled-in roots.
/// None when the PEM holds no usable certificate.
fn root_store(ca_certificate: Option<&str>) -> Option<RootCertStore> {
    let mut store = RootCertStore::empty();
    match ca_certificate.filter(|pem| !pem.trim().is_empty()) {
        Some(pem) => {
            let found: Result<Vec<CertificateDer<'static>>, _> =
                CertificateDer::pem_slice_iter(pem.as_bytes()).collect();
            let found = found.ok().filter(|found| !found.is_empty())?;
            for certificate in found {
                store.add(certificate).ok()?;
            }
        }
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Some(store)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TlsMode {
    Implicit,
    StartTls,
    Plain,
}

impl TlsMode {
    /// The mode the credential's `tls` value names. Plaintext is a configuration error unless
    /// `allow_plain`.
    fn parse(value: Option<&str>, allow_plain: bool) -> Result<Self, DestinationError> {
        match value {
            Some("tls") => Ok(Self::Implicit),
            Some("starttls") => Ok(Self::StartTls),
            Some("none") if allow_plain => Ok(Self::Plain),
            Some("none") => Err(DestinationError::Config {
                field: "tls".to_owned(),
                reason: "must not be none: the connection has to be encrypted".to_owned(),
            }),
            _ => Err(invalid("tls")),
        }
    }
}

fn invalid(field: &str) -> DestinationError {
    DestinationError::Config {
        field: field.to_owned(),
        reason: "is missing or invalid".to_owned(),
    }
}

/// The connection settings: the server from the `smtp` credential, the CA from the configuration.
struct Settings {
    host: String,
    port: u16,
    tls: TlsMode,
    ca_certificate: Option<String>,
}

impl Settings {
    fn parse(
        config: &Value,
        credential: &Credential,
        allow_plain: bool,
    ) -> Result<Self, DestinationError> {
        let text = |key: &str| credential.get(key).map(|value| value.expose());
        let host = text("host")
            .filter(|host| host_ok(host))
            .ok_or_else(|| invalid("host"))?
            .to_owned();
        let port = text("port")
            .and_then(|port| port.trim().parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("port"))?;
        let tls = TlsMode::parse(text("tls"), allow_plain)?;
        let ca_certificate = config
            .get("ca_certificate")
            .and_then(Value::as_str)
            .filter(|pem| !pem.trim().is_empty())
            .map(str::to_owned);
        Ok(Self {
            host,
            port,
            tls,
            ca_certificate,
        })
    }

    fn connector(&self) -> Result<TlsConnector, DestinationError> {
        let roots =
            root_store(self.ca_certificate.as_deref()).ok_or_else(|| DestinationError::Config {
                field: "ca_certificate".to_owned(),
                reason: "must hold PEM certificates usable as a CA".to_owned(),
            })?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| DestinationError::Other("the TLS settings could not be built".to_owned()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(TlsConnector::from(Arc::new(config)))
    }
}

/// The user name and password of the credential.
struct Auth<'a> {
    username: &'a str,
    password: &'a str,
}

impl<'a> Auth<'a> {
    fn of(credential: &'a Credential) -> Result<Self, DestinationError> {
        match (credential.get("username"), credential.get("password")) {
            (Some(username), Some(password)) => Ok(Self {
                username: username.expose(),
                password: password.expose(),
            }),
            // Without both it cannot authenticate: the credential is the wrong kind.
            _ => Err(DestinationError::Auth),
        }
    }
}

/// What `DATA` sends.
struct Envelope<'a> {
    from: &'a str,
    to: &'a str,
    /// The dot-stuffed message and its end marker.
    data: Vec<u8>,
    /// The size of the message before stuffing.
    size: usize,
}

/// What the server advertised in answer to `EHLO`.
#[derive(Default)]
struct Extensions {
    starttls: bool,
    plain: bool,
    login: bool,
    /// The largest message the server takes; none when it did not say.
    size: Option<usize>,
}

impl Extensions {
    fn of(reply: &Reply) -> Self {
        let mut found = Self::default();
        for line in reply.lines.iter().skip(1) {
            let mut words = line.split_whitespace();
            match words.next().map(str::to_ascii_uppercase).as_deref() {
                Some("STARTTLS") => found.starttls = true,
                Some("SIZE") => {
                    found.size = words
                        .next()
                        .and_then(|size| size.parse().ok())
                        .filter(|size| *size > 0);
                }
                Some("AUTH") => {
                    for mechanism in words {
                        match mechanism.to_ascii_uppercase().as_str() {
                            "PLAIN" => found.plain = true,
                            "LOGIN" => found.login = true,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        found
    }
}

impl From<WireError> for DestinationError {
    fn from(error: WireError) -> Self {
        match error {
            WireError::Timeout => Self::Egress(HttpError::Timeout),
            WireError::Io => Self::Unreachable,
            WireError::Protocol => {
                Self::Other("the mail server sent a reply that could not be read".to_owned())
            }
        }
    }
}

/// Checks that a reply is a success of the class `expected` (a first digit), else says what its
/// refusal means at `stage`.
fn expect(stage: Stage, reply: &Reply, expected: u16) -> Result<(), DestinationError> {
    if reply.code / 100 == expected {
        Ok(())
    } else {
        Err(refusal(stage, reply))
    }
}

/// The whole conversation: the connection check when `envelope` is none, else one delivery.
async fn run(
    ctx: &DestinationContext,
    settings: &Settings,
    auth: &Auth<'_>,
    helo: &str,
    envelope: Option<&Envelope<'_>>,
) -> Result<Delivered, DestinationError> {
    // Every connect resolves and checks again, so a changed DNS answer is checked too.
    let addresses = ctx.resolve_checked(&settings.host).await?;
    let tcp = connect(&addresses, settings.port).await?;
    match settings.tls {
        TlsMode::Implicit => {
            let tls = handshake(settings, tcp).await?;
            let mut session = Session::new(tls);
            greet(&mut session).await?;
            let extensions = ehlo(&mut session, helo).await?;
            finish(session, &extensions, auth, envelope).await
        }
        TlsMode::StartTls => {
            let mut session = Session::new(tcp);
            greet(&mut session).await?;
            let extensions = ehlo(&mut session, helo).await?;
            if !extensions.starttls {
                return Err(DestinationError::Other(
                    "the mail server does not offer STARTTLS".to_owned(),
                ));
            }
            let reply = session.command("STARTTLS", STEP_TIMEOUT).await?;
            expect(Stage::StartTls, &reply, 2)?;
            if session.has_buffered() {
                return Err(DestinationError::Other(
                    "the mail server sent data before the TLS handshake".to_owned(),
                ));
            }
            let tls = handshake(settings, session.into_inner()).await?;
            let mut session = Session::new(tls);
            let extensions = ehlo(&mut session, helo).await?;
            finish(session, &extensions, auth, envelope).await
        }
        TlsMode::Plain => {
            let mut session = Session::new(tcp);
            greet(&mut session).await?;
            let extensions = ehlo(&mut session, helo).await?;
            finish(session, &extensions, auth, envelope).await
        }
    }
}

/// Dials the checked addresses in turn; the first that accepts is used.
async fn connect(addresses: &[IpAddr], port: u16) -> Result<TcpStream, DestinationError> {
    let mut last = DestinationError::Unreachable;
    for address in addresses {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect(SocketAddr::new(*address, port)),
        )
        .await
        {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(_)) => {}
            Err(_) => last = DestinationError::Egress(HttpError::Timeout),
        }
    }
    Err(last)
}

/// The TLS handshake, with the credential's host name as the server name.
async fn handshake(
    settings: &Settings,
    tcp: TcpStream,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>, DestinationError> {
    let connector = settings.connector()?;
    let name =
        ServerName::try_from(settings.host.clone()).map_err(|_| DestinationError::Config {
            field: "host".to_owned(),
            reason: "is not a valid server name".to_owned(),
        })?;
    match tokio::time::timeout(STEP_TIMEOUT, connector.connect(name, tcp)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(_)) => Err(DestinationError::Other(
            "the TLS handshake with the mail server failed".to_owned(),
        )),
        Err(_) => Err(DestinationError::Egress(HttpError::Timeout)),
    }
}

async fn greet<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut Session<S>,
) -> Result<(), DestinationError> {
    let reply = session.reply(STEP_TIMEOUT).await?;
    expect(Stage::Greeting, &reply, 2)
}

async fn ehlo<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut Session<S>,
    helo: &str,
) -> Result<Extensions, DestinationError> {
    let reply = session
        .command(&format!("EHLO {helo}"), STEP_TIMEOUT)
        .await?;
    expect(Stage::Ehlo, &reply, 2)?;
    Ok(Extensions::of(&reply))
}

/// Everything after the secured `EHLO`: authenticate, then deliver or, for the check, leave.
async fn finish<S: AsyncRead + AsyncWrite + Unpin>(
    mut session: Session<S>,
    extensions: &Extensions,
    auth: &Auth<'_>,
    envelope: Option<&Envelope<'_>>,
) -> Result<Delivered, DestinationError> {
    authenticate(&mut session, extensions, auth).await?;
    let outcome = match envelope {
        Some(envelope) => send(&mut session, extensions, envelope).await,
        None => Ok(()),
    };
    // The goodbye is courtesy: its answer, or its absence, changes nothing.
    let _ = session.command("QUIT", QUIT_TIMEOUT).await;
    outcome.map(|()| Delivered::default())
}

async fn authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut Session<S>,
    extensions: &Extensions,
    auth: &Auth<'_>,
) -> Result<(), DestinationError> {
    if extensions.plain {
        let mut raw = Zeroizing::new(Vec::new());
        raw.push(0);
        raw.extend_from_slice(auth.username.as_bytes());
        raw.push(0);
        raw.extend_from_slice(auth.password.as_bytes());
        let command = Zeroizing::new(format!("AUTH PLAIN {}", STANDARD.encode(raw.as_slice())));
        let reply = session.command(&command, STEP_TIMEOUT).await?;
        return expect(Stage::Auth, &reply, 2);
    }
    if extensions.login {
        let reply = session.command("AUTH LOGIN", STEP_TIMEOUT).await?;
        if reply.code != 334 {
            return Err(refusal(Stage::Auth, &reply));
        }
        let user = Zeroizing::new(STANDARD.encode(auth.username.as_bytes()));
        let reply = session.command(&user, STEP_TIMEOUT).await?;
        if reply.code != 334 {
            return Err(refusal(Stage::Auth, &reply));
        }
        let password = Zeroizing::new(STANDARD.encode(auth.password.as_bytes()));
        let reply = session.command(&password, STEP_TIMEOUT).await?;
        return expect(Stage::Auth, &reply, 2);
    }
    // The credential has a password and the server takes none this client speaks.
    Err(DestinationError::Auth)
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    session: &mut Session<S>,
    extensions: &Extensions,
    envelope: &Envelope<'_>,
) -> Result<(), DestinationError> {
    if extensions.size.is_some_and(|limit| envelope.size > limit) {
        return Err(DestinationError::Refused(super::error::Refusal::Rejected));
    }
    let reply = session
        .command(&format!("MAIL FROM:<{}>", envelope.from), STEP_TIMEOUT)
        .await?;
    expect(Stage::MailFrom, &reply, 2)?;
    let reply = session
        .command(&format!("RCPT TO:<{}>", envelope.to), STEP_TIMEOUT)
        .await?;
    expect(Stage::RcptTo, &reply, 2)?;
    let reply = session.command("DATA", STEP_TIMEOUT).await?;
    if reply.code != 354 {
        return Err(match reply.code / 100 {
            2 | 3 => DestinationError::Other("the mail server sent an unexpected reply".to_owned()),
            _ => refusal(Stage::Data, &reply),
        });
    }
    let reply = session.exchange(&envelope.data, DATA_TIMEOUT).await?;
    expect(Stage::DataEnd, &reply, 2)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;

    use serde_json::json;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::sending::dkim_record_text;

    use super::super::dkim::DkimSigner;
    use super::super::dkim::tests::{key, signer, verify};
    use super::super::error::{Disposition, Refusal};
    use super::sink::{SINK_HOST, SinkOptions, SinkStage, SinkTls, SmtpSink};
    use super::*;
    use crate::destinations::validate_config;
    use crate::http::{HttpLimits, Lookup};

    struct Fixed(Vec<IpAddr>);

    impl Lookup for Fixed {
        fn lookup<'a>(
            &'a self,
            _host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    fn context(allow_loopback: bool) -> DestinationContext {
        let allow: Vec<String> = if allow_loopback {
            vec!["127.0.0.0/8".to_owned()]
        } else {
            Vec::new()
        };
        DestinationContext::new(
            Arc::new(EgressPolicy::parse(&allow).unwrap()),
            HttpLimits::default(),
            "smtp",
            "destination-1",
        )
        .unwrap()
        .with_lookup(Arc::new(Fixed(vec!["127.0.0.1".parse().unwrap()])))
    }

    /// The destination's own configuration: the sink's authority.
    fn config(sink: &SmtpSink) -> Value {
        json!({"ca_certificate": sink.ca_pem()})
    }

    /// An `smtp` credential for `sink`: `tls` is `none`, `starttls` or `tls`.
    fn credential(sink: &SmtpSink, tls: &str, user: &str, password: &str) -> Credential {
        let host = if tls == "none" {
            "127.0.0.1"
        } else {
            SINK_HOST
        };
        let mut credential = Credential::new();
        for (key, value) in [
            ("host", host.to_owned()),
            ("port", sink.port().to_string()),
            ("tls", tls.to_owned()),
            ("username", user.to_owned()),
            ("password", password.to_owned()),
        ] {
            credential.insert(key, SecretString::new(value));
        }
        credential
    }

    /// An `smtp` credential for `sink` with the login a default sink accepts.
    fn sink_login(sink: &SmtpSink, tls: &str) -> Credential {
        credential(sink, tls, SINK_USER, SINK_PASSWORD)
    }

    fn payload() -> Value {
        json!({
            "from": {"name": "The News", "address": "news@example.com"},
            "reply_to": null,
            "subject": "Hello",
            "html": "<p>Hi</p>",
            "text": "Hi",
            "headers": {
                "List-Unsubscribe": "<https://links.example/u/1>",
                "List-Unsubscribe-Post": "List-Unsubscribe=One-Click",
            },
        })
    }

    /// One send through a default sink with no TLS, as the key `key`.
    async fn plain_send(
        sink: &SmtpSink,
        payload: &Value,
        key: &str,
        dkim: Option<&DkimSigner>,
    ) -> Result<Delivered, DestinationError> {
        send(
            sink,
            &sink_login(sink, "none"),
            payload,
            key,
            dkim,
            &context(true),
        )
        .await
    }

    async fn send(
        sink: &SmtpSink,
        credential: &Credential,
        payload: &Value,
        key: &str,
        dkim: Option<&DkimSigner>,
        ctx: &DestinationContext,
    ) -> Result<Delivered, DestinationError> {
        let message = Outbound {
            idempotency_key: key,
            channel: "email",
            to: "ada@example.net",
            payload,
            dkim,
        };
        Smtp.deliver(ctx, &config(sink), credential, &message).await
    }

    fn data_of(sink: &SmtpSink, index: usize) -> Vec<u8> {
        sink.messages()[index].data.clone()
    }

    /// The value of the first header called `name`, unfolded.
    fn header_of(data: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(data).into_owned();
        let head = text.split("\r\n\r\n").next()?.to_owned();
        let mut found: Option<String> = None;
        for line in head.split("\r\n") {
            if line.starts_with([' ', '\t']) {
                if let Some(value) = found.as_mut() {
                    value.push_str(line);
                }
            } else if found.is_some() {
                break;
            } else if let Some((candidate, value)) = line.split_once(':')
                && candidate.eq_ignore_ascii_case(name)
            {
                found = Some(value.trim().to_owned());
            }
        }
        found
    }

    #[tokio::test]
    async fn a_message_from_a_verified_domain_is_captured_and_its_signature_verifies_against_the_published_key()
     {
        let sink = SmtpSink::start(SinkOptions::default()).await;

        let delivered = plain_send(&sink, &payload(), "msg-1", Some(&signer("example.com"))).await;

        assert_eq!(delivered, Ok(Delivered::default()));
        let messages = sink.messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].mail_from, "news@example.com");
        assert_eq!(messages[0].recipients, ["ada@example.net"]);
        let data = &messages[0].data;
        assert_eq!(verify(data, &dkim_record_text(&key().public_key)), Ok(()));
        let signature = header_of(data, "DKIM-Signature").unwrap();
        assert!(signature.contains("d=example.com;") && signature.contains("s=zirv;"));
        for signed in [
            "from",
            "to",
            "subject",
            "date",
            "message-id",
            "list-unsubscribe",
            "list-unsubscribe-post",
        ] {
            assert!(signature.contains(signed), "{signed} is signed");
        }
        assert!(
            header_of(data, "Content-Type")
                .unwrap()
                .starts_with("multipart/alternative;")
        );
    }

    #[tokio::test]
    async fn the_unsubscribe_pair_the_message_id_and_the_idempotency_key_are_on_the_wire() {
        let sink = SmtpSink::start(SinkOptions::default()).await;

        plain_send(&sink, &payload(), "msg-7", Some(&signer("example.com")))
            .await
            .unwrap();

        let data = data_of(&sink, 0);
        assert_eq!(
            header_of(&data, "List-Unsubscribe").as_deref(),
            Some("<https://links.example/u/1>")
        );
        assert_eq!(
            header_of(&data, "List-Unsubscribe-Post").as_deref(),
            Some("List-Unsubscribe=One-Click")
        );
        assert_eq!(
            header_of(&data, "X-Zirv-Idempotency-Key").as_deref(),
            Some("msg-7")
        );
        assert_eq!(
            header_of(&data, "Message-ID").as_deref(),
            Some("<msg-7@example.com>")
        );
    }

    #[tokio::test]
    async fn a_retried_send_reuses_the_message_id_and_the_idempotency_key() {
        let sink = SmtpSink::start(SinkOptions::default()).await;
        let signer = signer("example.com");

        for _ in 0..2 {
            plain_send(&sink, &payload(), "msg-3", Some(&signer))
                .await
                .unwrap();
        }

        let (first, again) = (data_of(&sink, 0), data_of(&sink, 1));
        for name in ["Message-ID", "X-Zirv-Idempotency-Key", "Content-Type"] {
            assert_eq!(header_of(&first, name), header_of(&again, name), "{name}");
        }
        assert!(header_of(&first, "Message-ID").is_some());
    }

    #[tokio::test]
    async fn a_message_without_a_key_or_with_another_domains_key_is_refused_before_connecting() {
        let sink = SmtpSink::start(SinkOptions::default()).await;
        let other = signer("other.example");

        let unsigned = plain_send(&sink, &payload(), "m", None).await;
        let foreign = plain_send(&sink, &payload(), "m", Some(&other)).await;

        for refused in [unsigned, foreign] {
            let error = refused.unwrap_err();
            assert!(
                matches!(error, DestinationError::Config { ref field, .. } if field == "dkim"),
                "{error}"
            );
            assert_eq!(error.disposition(), Disposition::Terminal);
        }
        assert_eq!(sink.connections(), 0);
    }

    #[tokio::test]
    async fn starttls_and_implicit_tls_authenticate_and_deliver_with_the_configured_authority() {
        let signer = signer("example.com");
        for (tls, mode) in [("starttls", SinkTls::StartTls), ("tls", SinkTls::Implicit)] {
            let sink = SmtpSink::start(SinkOptions {
                tls: mode,
                auth: Some(("ada".to_owned(), "pw".to_owned())),
            })
            .await;

            let result = send(
                &sink,
                &credential(&sink, tls, "ada", "pw"),
                &payload(),
                "msg-1",
                Some(&signer),
                &context(true),
            )
            .await;

            assert_eq!(result, Ok(Delivered::default()), "{tls}");
            assert_eq!(sink.messages().len(), 1, "{tls}");
            assert_eq!(sink.authentications(), (1, 0), "{tls}");
        }
    }

    #[tokio::test]
    async fn a_wrong_password_is_an_auth_failure_that_does_not_deliver() {
        let sink = SmtpSink::start(SinkOptions {
            tls: SinkTls::StartTls,
            auth: Some(("ada".to_owned(), "pw".to_owned())),
        })
        .await;

        let result = send(
            &sink,
            &credential(&sink, "starttls", "ada", "wrong"),
            &payload(),
            "m",
            Some(&signer("example.com")),
            &context(true),
        )
        .await;

        assert_eq!(result, Err(DestinationError::Auth));
        assert!(sink.messages().is_empty());
        assert_eq!(sink.authentications(), (0, 1));
    }

    #[tokio::test]
    async fn a_server_without_starttls_and_a_certificate_nobody_trusts_are_not_used() {
        let signer = signer("example.com");
        let plain = SmtpSink::start(SinkOptions::default()).await;
        let secured = SmtpSink::start(SinkOptions {
            tls: SinkTls::StartTls,
            ..SinkOptions::default()
        })
        .await;
        let ctx = context(true);

        let no_starttls = send(
            &plain,
            &sink_login(&plain, "starttls"),
            &payload(),
            "m",
            Some(&signer),
            &ctx,
        )
        .await;
        let body = payload();
        let message = Outbound {
            idempotency_key: "m",
            channel: "email",
            to: "ada@example.net",
            payload: &body,
            dkim: Some(&signer),
        };
        let handshake = Smtp
            .deliver(
                &ctx,
                &json!({"ca_certificate": null}),
                &sink_login(&secured, "starttls"),
                &message,
            )
            .await;

        assert!(matches!(no_starttls, Err(DestinationError::Other(_))));
        assert!(matches!(handshake, Err(DestinationError::Other(_))));
        assert!(plain.messages().is_empty() && secured.messages().is_empty());
    }

    #[tokio::test]
    async fn replies_are_classified_end_to_end() {
        use SinkStage::{Data, DataEnd, Mail, Rcpt};
        let signer = signer("example.com");
        let table = [
            (Rcpt, "550 5.1.1 no such user", Refusal::HardBounce),
            (Rcpt, "554 5.7.1 relay denied", Refusal::Rejected),
            (Rcpt, "451 4.7.1 try later", Refusal::Deferred),
            (Mail, "550 5.7.1 sender refused", Refusal::Rejected),
            (Data, "554 5.7.1 no", Refusal::Rejected),
            (DataEnd, "550 5.1.1 gone after content", Refusal::HardBounce),
            (DataEnd, "554 5.7.1 content rejected", Refusal::Rejected),
            (DataEnd, "452 4.2.2 mailbox full", Refusal::Deferred),
        ];
        for (stage, reply, expected) in table {
            let sink = SmtpSink::start(SinkOptions::default()).await;
            sink.reply_at(stage, reply);

            let result = plain_send(&sink, &payload(), "m", Some(&signer)).await;

            assert_eq!(result, Err(DestinationError::Refused(expected)), "{reply}");
            assert!(sink.messages().is_empty(), "{reply}");
        }
    }

    #[tokio::test]
    async fn a_sink_address_in_a_denied_egress_class_is_refused_and_never_dialled() {
        let sink = SmtpSink::start(SinkOptions::default()).await;

        let result = send(
            &sink,
            &sink_login(&sink, "none"),
            &payload(),
            "m",
            Some(&signer("example.com")),
            &context(false),
        )
        .await;

        let error = result.unwrap_err();
        assert!(
            matches!(
                error,
                DestinationError::Egress(HttpError::EgressDenied { .. })
            ),
            "{error}"
        );
        assert_eq!(error.disposition(), Disposition::Terminal);
        assert_eq!(sink.connections(), 0);
    }

    #[tokio::test]
    async fn the_check_connects_and_authenticates_without_delivering() {
        let sink = SmtpSink::start(SinkOptions {
            tls: SinkTls::StartTls,
            auth: Some(("ada".to_owned(), "pw".to_owned())),
        })
        .await;
        let ctx = context(true);
        let config = config(&sink);

        let good = Smtp
            .check(&ctx, &config, &credential(&sink, "starttls", "ada", "pw"))
            .await;
        let bad = Smtp
            .check(&ctx, &config, &credential(&sink, "starttls", "ada", "no"))
            .await;

        assert_eq!(good, Ok(()));
        assert_eq!(bad, Err(DestinationError::Auth));
        assert!(sink.messages().is_empty());
    }

    #[tokio::test]
    async fn html_mjml_and_blocks_style_parts_survive_the_wire_and_the_signature() {
        // The shapes the three content types render: a plain document with markup and an equals
        // sign; the long single-line output of a compiler with conditional comments; and a
        // generated document with a line that starts with a dot and non-ASCII text.
        let long_html = format!(
            "<!--[if mso]><table><tr><td><![endif]-->{}<!--[if mso]></td></tr></table><![endif]-->",
            "<p style=\"margin:0\">word </p>".repeat(200)
        );
        let long_text = "A long plain text line that goes on and on. ".repeat(40);
        let fixtures = [
            (
                "<!doctype html>\n<html><body><a href=\"https://x.example/?a=1&b=2\">Zoë</a></body></html>\n",
                "Zoë https://x.example/?a=1&b=2\n",
            ),
            (long_html.as_str(), long_text.as_str()),
            (
                "<p>one</p>\n.<p>starts with a dot</p>\n<p>trailing space </p>\n",
                "one\n.starts with a dot\ntrailing space \n",
            ),
        ];
        for (html, text) in fixtures {
            let sink = SmtpSink::start(SinkOptions::default()).await;
            let mut body = payload();
            body["html"] = json!(html);
            body["text"] = json!(text);

            plain_send(&sink, &body, "m", Some(&signer("example.com")))
                .await
                .unwrap();

            let data = data_of(&sink, 0);
            assert_eq!(verify(&data, &dkim_record_text(&key().public_key)), Ok(()));
            let (plain, rich) = alternatives(&data);
            assert_eq!(plain, text.replace('\n', "\r\n"));
            assert_eq!(rich, html.replace('\n', "\r\n"));
        }
    }

    /// The decoded text and HTML parts of a captured multipart message.
    fn alternatives(data: &[u8]) -> (String, String) {
        let text = String::from_utf8_lossy(data).into_owned();
        let boundary = text
            .split("boundary=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        let body = text
            .split("\r\n\r\n")
            .skip(1)
            .collect::<Vec<_>>()
            .join("\r\n\r\n");
        let delimiter = format!("--{boundary}");
        let mut parts = body
            .split(delimiter.as_str())
            .filter(|part| part.contains("Content-Transfer-Encoding: quoted-printable"));
        let mut decoded = |kind: &str| {
            let part = parts.next().unwrap();
            assert!(part.contains(&format!("text/{kind}")), "{kind}");
            let encoded = part
                .split("\r\n\r\n")
                .skip(1)
                .collect::<Vec<_>>()
                .join("\r\n\r\n");
            let encoded = encoded.strip_suffix("\r\n").unwrap_or(&encoded);
            unquote(encoded)
        };
        (decoded("plain"), decoded("html"))
    }

    fn unquote(encoded: &str) -> String {
        let bytes = encoded.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'=' {
                if bytes[at + 1..].starts_with(b"\r\n") {
                    at += 3;
                    continue;
                }
                out.push(u8::from_str_radix(&encoded[at + 1..at + 3], 16).unwrap());
                at += 3;
            } else {
                out.push(bytes[at]);
                at += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_configuration_is_checked_before_it_is_stored() {
        let spec = &Smtp::SPEC;
        assert!(validate_config(spec, &json!({})).is_ok());
        assert!(validate_config(spec, &json!({"ca_certificate": ""})).is_ok());

        let errors = validate_config(spec, &json!({"ca_certificate": "nope"})).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "ca_certificate");
        // The server lives in the credential, so the configuration refuses it.
        assert!(validate_config(spec, &json!({"host": "mail.example.net"})).is_err());
    }

    #[test]
    fn the_credential_fields_are_those_of_the_smtp_credential_kind() {
        use zirv_flow_core::credentials::CredentialKind;

        let declared: Vec<&str> = Smtp::SPEC.credential_fields.iter().map(|f| f.key).collect();
        let kind = CredentialKind::Smtp.schema();
        let schema: Vec<&str> = kind.fields.iter().map(|f| f.key.as_str()).collect();

        assert_eq!(declared, schema);
        let secret: Vec<&str> = Smtp::SPEC
            .credential_fields
            .iter()
            .filter(|f| f.secret)
            .map(|f| f.key)
            .collect();
        assert_eq!(secret, ["password"]);
    }

    #[test]
    fn plaintext_is_a_configuration_error_unless_the_build_allows_it() {
        for allowed in [false, true] {
            let mode = TlsMode::parse(Some("none"), allowed);
            if allowed {
                assert_eq!(mode.unwrap(), TlsMode::Plain);
            } else {
                assert!(
                    matches!(mode, Err(DestinationError::Config { ref field, .. }) if field == "tls"),
                    "{mode:?}"
                );
            }
        }
        assert_eq!(
            TlsMode::parse(Some("tls"), false).unwrap(),
            TlsMode::Implicit
        );
        assert_eq!(
            TlsMode::parse(Some("starttls"), false).unwrap(),
            TlsMode::StartTls
        );
        assert!(TlsMode::parse(Some("implicit"), true).is_err());
        assert!(TlsMode::parse(None, true).is_err());
    }

    #[test]
    fn a_release_build_refuses_plaintext_in_the_check_and_in_a_delivery() {
        let mut login = Credential::new();
        for (key, value) in [
            ("host", "127.0.0.1"),
            ("port", "25"),
            ("tls", "none"),
            ("username", "u"),
            ("password", "p"),
        ] {
            login.insert(key, SecretString::new(value.to_owned()));
        }

        // `check` and `deliver` both parse the settings with the build's flag first.
        let refused = Settings::parse(&json!({}), &login, false);

        assert!(matches!(
            refused,
            Err(DestinationError::Config { ref field, .. }) if field == "tls"
        ));
        assert!(Settings::parse(&json!({}), &login, true).is_ok());
    }

    #[tokio::test]
    async fn a_credential_without_a_host_is_a_configuration_error_and_nothing_connects() {
        let sink = SmtpSink::start(SinkOptions::default()).await;
        let mut hostless = Credential::new();
        for (key, value) in [
            ("port", sink.port().to_string()),
            ("tls", "none".to_owned()),
            ("username", SINK_USER.to_owned()),
            ("password", SINK_PASSWORD.to_owned()),
        ] {
            hostless.insert(key, SecretString::new(value));
        }
        let ctx = context(true);

        let checked = Smtp.check(&ctx, &json!({}), &hostless).await;
        let sent = send(
            &sink,
            &hostless,
            &payload(),
            "m",
            Some(&signer("example.com")),
            &ctx,
        )
        .await;

        for result in [checked.map(|()| Delivered::default()), sent] {
            let error = result.unwrap_err();
            assert!(
                matches!(error, DestinationError::Config { ref field, .. } if field == "host"),
                "{error}"
            );
            assert_eq!(error.disposition(), Disposition::Terminal);
        }
        assert_eq!(sink.connections(), 0);
    }

    #[test]
    fn extensions_are_read_from_the_ehlo_lines_and_a_half_login_cannot_authenticate() {
        let reply = Reply {
            code: 250,
            enhanced: None,
            lines: vec![
                "mail.test".into(),
                "STARTTLS".into(),
                "SIZE 2000".into(),
                "auth login PLAIN XOAUTH2".into(),
            ],
        };

        let extensions = Extensions::of(&reply);

        assert!(extensions.starttls && extensions.plain && extensions.login);
        assert_eq!(extensions.size, Some(2000));
        let mut half = Credential::new();
        half.insert("password", SecretString::new("pw".into()));
        assert!(matches!(Auth::of(&half), Err(DestinationError::Auth)));
        assert!(matches!(
            Auth::of(&Credential::new()),
            Err(DestinationError::Auth)
        ));
    }
}
