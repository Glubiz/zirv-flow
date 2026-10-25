//! The bench SMTP sink: the stand-in for a provider in the send-throughput benchmark (#233).
//!
//! It accepts every message and records when (unix microseconds) and for whom, one JSON line per
//! message, so the report can compute the rate at the sink and the delay from enrollment. The
//! released image refuses plaintext SMTP, so the sink offers `STARTTLS` (or implicit TLS) with a
//! certificate from a generated authority; the authority's PEM goes to the destination's
//! `ca_certificate`. It accepts any login and understands what the SMTP destination sends and
//! nothing more. It is a bench tool, not an SMTP implementation to rely on.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// How the sink secures its connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsMode {
    /// No TLS. Only a test build or a `test-support` build of the product talks to this.
    Plain,
    /// `STARTTLS` is offered; `AUTH` and `MAIL` are refused until it was used.
    StartTls,
    /// Every connection starts with the handshake.
    Implicit,
}

impl TlsMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "plain" | "none" => Some(Self::Plain),
            "starttls" => Some(Self::StartTls),
            "implicit" | "tls" => Some(Self::Implicit),
            _ => None,
        }
    }
}

pub struct SinkConfig {
    pub tls: TlsMode,
    /// Host names the certificate is valid for, besides `localhost` and 127.0.0.1.
    pub names: Vec<String>,
    /// Added to every message's final reply: a provider that takes this long to accept.
    pub response_delay: Duration,
    /// Where to append one JSON line per accepted message.
    pub log: Option<std::path::PathBuf>,
    /// Keep the arrivals in memory too (tests).
    pub keep: bool,
}

/// One accepted message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Arrival {
    /// Unix microseconds when the final dot was received.
    pub at_us: u64,
    /// The first recipient.
    pub rcpt: String,
}

struct State {
    accepted: AtomicU64,
    connections: AtomicU64,
    log: Mutex<Option<std::io::BufWriter<std::fs::File>>>,
    kept: Mutex<Vec<Arrival>>,
    keep: bool,
}

impl State {
    fn record(&self, arrival: Arrival) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
        {
            let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(writer) = log.as_mut() {
                let rcpt = serde_json::Value::from(arrival.rcpt.as_str());
                // A failed write drops one line from the log; the counter above still counts it.
                let _ = writeln!(writer, "{{\"t_us\":{},\"rcpt\":{rcpt}}}", arrival.at_us);
                let _ = writer.flush();
            }
        }
        if self.keep {
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(arrival);
        }
    }
}

/// A running sink. Dropping it stops it.
pub struct Sink {
    address: SocketAddr,
    ca_pem: String,
    state: Arc<State>,
    task: JoinHandle<()>,
}

impl Sink {
    /// Binds `listen` (port 0 picks a free one) and starts accepting.
    pub async fn start(listen: SocketAddr, config: SinkConfig) -> Result<Self, String> {
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|error| format!("cannot listen on {listen}: {error}"))?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let (acceptor, ca_pem) = certificate(&config.names)?;
        let log = match &config.log {
            Some(path) => Some(std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|error| format!("cannot open the arrivals log: {error}"))?,
            )),
            None => None,
        };
        let state = Arc::new(State {
            accepted: AtomicU64::new(0),
            connections: AtomicU64::new(0),
            log: Mutex::new(log),
            kept: Mutex::new(Vec::new()),
            keep: config.keep,
        });
        let shared = Arc::clone(&state);
        let tls = config.tls;
        let delay = config.response_delay;
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let _ = stream.set_nodelay(true);
                shared.connections.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(connection(
                    stream,
                    tls,
                    delay,
                    acceptor.clone(),
                    Arc::clone(&shared),
                ));
            }
        });
        Ok(Self {
            address,
            ca_pem,
            state,
            task,
        })
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// The authority that signed the sink's certificate, as PEM.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn accepted(&self) -> u64 {
        self.state.accepted.load(Ordering::Relaxed)
    }

    pub fn connections(&self) -> u64 {
        self.state.connections.load(Ordering::Relaxed)
    }

    /// The arrivals kept in memory (empty unless `keep` was set).
    pub fn arrivals(&self) -> Vec<Arrival> {
        self.state
            .kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Writes the authority's PEM where the bench scripts read it.
pub fn write_ca(dir: &Path, pem: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    std::fs::write(dir.join("ca.pem"), pem).map_err(|error| error.to_string())
}

fn certificate(names: &[String]) -> Result<(TlsAcceptor, String), String> {
    let fail = |error: rcgen::Error| format!("cannot generate the certificate: {error}");
    let ca_key = rcgen::KeyPair::generate().map_err(fail)?;
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).map_err(fail)?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).map_err(fail)?;
    let issuer = rcgen::Issuer::new(ca_params, ca_key);

    let server_key = rcgen::KeyPair::generate().map_err(fail)?;
    let mut all: Vec<String> = names.to_vec();
    all.push("localhost".to_owned());
    let mut params = rcgen::CertificateParams::new(all).map_err(fail)?;
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::from([
            127, 0, 0, 1,
        ])));
    let server = params.signed_by(&server_key, &issuer).map_err(fail)?;
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?
            .with_no_client_auth()
            .with_single_cert(vec![server.der().clone()], key)
            .map_err(|error| error.to_string())?;
    Ok((TlsAcceptor::from(Arc::new(config)), ca.pem()))
}

enum Ended {
    Closed,
    /// The client asked for `STARTTLS` and was told to go ahead.
    Upgrade,
}

async fn connection(
    stream: TcpStream,
    tls: TlsMode,
    delay: Duration,
    acceptor: TlsAcceptor,
    state: Arc<State>,
) {
    match tls {
        TlsMode::Implicit => {
            if let Ok(secured) = acceptor.accept(stream).await {
                let mut io = BufReader::new(secured);
                let _ = converse(&mut io, tls, delay, &state, true, true).await;
            }
        }
        TlsMode::Plain => {
            let mut io = BufReader::new(stream);
            let _ = converse(&mut io, tls, delay, &state, false, true).await;
        }
        TlsMode::StartTls => {
            let mut io = BufReader::new(stream);
            if let Some(Ended::Upgrade) = converse(&mut io, tls, delay, &state, false, true).await
                && let Ok(secured) = acceptor.accept(io.into_inner()).await
            {
                let mut io = BufReader::new(secured);
                let _ = converse(&mut io, tls, delay, &state, true, false).await;
            }
        }
    }
}

async fn reply<S: AsyncWrite + Unpin>(writer: &mut S, text: &str) -> bool {
    writer.write_all(text.as_bytes()).await.is_ok() && writer.write_all(b"\r\n").await.is_ok()
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64)
}

/// The address between the angle brackets of a `MAIL FROM:<...>` or `RCPT TO:<...>` command.
fn path_of(command: &str) -> String {
    let start = command.find('<').map_or(0, |at| at + 1);
    let end = command.rfind('>').unwrap_or(command.len());
    command.get(start..end).unwrap_or_default().to_owned()
}

/// Serves one stream until QUIT, a close or STARTTLS. `greet` sends the 220 greeting.
async fn converse<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut BufReader<S>,
    tls: TlsMode,
    delay: Duration,
    state: &State,
    secured: bool,
    greet: bool,
) -> Option<Ended> {
    if greet && !reply(io, "220 mail.bench.test ESMTP sink").await {
        return Some(Ended::Closed);
    }
    let needs_tls = tls == TlsMode::StartTls && !secured;
    let mut first_rcpt = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if io.read_line(&mut line).await.ok()? == 0 {
            return Some(Ended::Closed);
        }
        let command = line.trim_end().to_owned();
        let upper = command.to_ascii_uppercase();
        let ok = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            let mut text = String::from("250-mail.bench.test\r\n250-SIZE 10485760\r\n");
            if needs_tls {
                text.push_str("250-STARTTLS\r\n");
            } else {
                text.push_str("250-AUTH PLAIN LOGIN\r\n");
            }
            text.push_str("250 8BITMIME");
            reply(io, &text).await
        } else if upper == "STARTTLS" {
            if needs_tls {
                if !reply(&mut *io, "220 2.0.0 ready to start TLS").await {
                    return Some(Ended::Closed);
                }
                return Some(Ended::Upgrade);
            }
            reply(io, "503 5.5.1 TLS is not available").await
        } else if upper.starts_with("AUTH") && needs_tls {
            reply(io, "530 5.7.0 Must issue a STARTTLS command first").await
        } else if upper.starts_with("AUTH PLAIN") {
            if command.len() > "AUTH PLAIN".len() {
                reply(io, "235 2.7.0 authenticated").await
            } else {
                // Without an initial response: ask for it, accept whatever comes.
                if !reply(io, "334 ").await {
                    return Some(Ended::Closed);
                }
                line.clear();
                if io.read_line(&mut line).await.ok()? == 0 {
                    return Some(Ended::Closed);
                }
                reply(io, "235 2.7.0 authenticated").await
            }
        } else if upper == "AUTH LOGIN" {
            for prompt in ["334 VXNlcm5hbWU6", "334 UGFzc3dvcmQ6"] {
                if !reply(io, prompt).await {
                    return Some(Ended::Closed);
                }
                line.clear();
                if io.read_line(&mut line).await.ok()? == 0 {
                    return Some(Ended::Closed);
                }
            }
            reply(io, "235 2.7.0 authenticated").await
        } else if upper.starts_with("MAIL FROM:") {
            if needs_tls {
                reply(io, "530 5.7.0 Must issue a STARTTLS command first").await
            } else {
                first_rcpt.clear();
                reply(io, "250 2.1.0 ok").await
            }
        } else if upper.starts_with("RCPT TO:") {
            if first_rcpt.is_empty() {
                first_rcpt = path_of(&command);
            }
            reply(io, "250 2.1.5 ok").await
        } else if upper == "DATA" {
            if !reply(io, "354 end with <CRLF>.<CRLF>").await {
                return Some(Ended::Closed);
            }
            let mut raw = Vec::new();
            loop {
                raw.clear();
                if io.read_until(b'\n', &mut raw).await.ok()? == 0 {
                    return Some(Ended::Closed);
                }
                if raw == b".\r\n" {
                    break;
                }
            }
            state.record(Arrival {
                at_us: now_us(),
                rcpt: std::mem::take(&mut first_rcpt),
            });
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            reply(io, "250 2.0.0 queued").await
        } else if upper == "RSET" || upper == "NOOP" {
            reply(io, "250 2.0.0 ok").await
        } else if upper == "QUIT" {
            let _ = reply(io, "221 2.0.0 bye").await;
            return Some(Ended::Closed);
        } else {
            reply(io, "502 5.5.2 not implemented").await
        };
        if !ok {
            return Some(Ended::Closed);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use rustls::RootCertStore;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName};
    use tokio_rustls::TlsConnector;

    use super::*;

    pub(crate) fn test_config(tls: TlsMode) -> SinkConfig {
        SinkConfig {
            tls,
            names: vec!["mail.bench.test".to_owned()],
            response_delay: Duration::ZERO,
            log: None,
            keep: true,
        }
    }

    fn loopback() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 0))
    }

    /// Reads a reply to its last line.
    async fn reply_of<S: AsyncRead + Unpin>(io: &mut BufReader<S>) -> String {
        loop {
            let mut line = String::new();
            let read = io.read_line(&mut line).await;
            assert!(matches!(read, Ok(n) if n > 0), "the sink closed");
            if line.as_bytes().get(3) != Some(&b'-') {
                return line.trim_end().to_owned();
            }
        }
    }

    async fn ask<S: AsyncRead + AsyncWrite + Unpin>(io: &mut BufReader<S>, text: &str) -> String {
        assert!(io.write_all(format!("{text}\r\n").as_bytes()).await.is_ok());
        reply_of(io).await
    }

    /// One message through an open, secured conversation.
    pub(crate) async fn send_one<S: AsyncRead + AsyncWrite + Unpin>(
        io: &mut BufReader<S>,
        to: &str,
    ) {
        assert!(ask(io, "EHLO bench.test").await.starts_with("250"));
        assert!(ask(io, "AUTH PLAIN AGJlbmNoAHg=").await.starts_with("235"));
        assert!(
            ask(io, "MAIL FROM:<news@example.com>")
                .await
                .starts_with("250")
        );
        assert!(ask(io, &format!("RCPT TO:<{to}>")).await.starts_with("250"));
        assert!(ask(io, "DATA").await.starts_with("354"));
        assert!(
            ask(io, "Subject: hi\r\n\r\nbody\r\n..dot\r\n.")
                .await
                .starts_with("250")
        );
        assert!(ask(io, "QUIT").await.starts_with("221"));
    }

    /// Sends `count` messages to `sink` over STARTTLS, trusting its authority.
    pub(crate) async fn send_many_starttls(sink: &Sink, count: usize) {
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(sink.ca_pem().as_bytes()).flatten() {
            assert!(roots.add(cert).is_ok());
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions();
        assert!(config.is_ok());
        let Ok(config) = config else { return };
        let connector = TlsConnector::from(Arc::new(
            config.with_root_certificates(roots).with_no_client_auth(),
        ));
        for n in 0..count {
            let stream = TcpStream::connect(("127.0.0.1", sink.port())).await;
            assert!(stream.is_ok());
            let Ok(stream) = stream else { return };
            let mut io = BufReader::new(stream);
            assert!(reply_of(&mut io).await.starts_with("220"));
            assert!(ask(&mut io, "EHLO bench.test").await.starts_with("250"));
            // AUTH is refused before the upgrade.
            assert!(ask(&mut io, "AUTH PLAIN AA==").await.starts_with("530"));
            assert!(ask(&mut io, "STARTTLS").await.starts_with("220"));
            let name = ServerName::try_from("mail.bench.test");
            assert!(name.is_ok());
            let Ok(name) = name else { return };
            let secured = connector.connect(name, io.into_inner()).await;
            assert!(secured.is_ok(), "the handshake trusts the sink's authority");
            let Ok(secured) = secured else { return };
            let mut io = BufReader::new(secured);
            send_one(&mut io, &format!("person-{n}@example.com")).await;
        }
    }

    #[tokio::test]
    async fn starttls_refuses_auth_first_then_records_each_message_with_its_recipient() {
        let started = Sink::start(loopback(), test_config(TlsMode::StartTls)).await;
        assert!(started.is_ok());
        let Ok(sink) = started else { return };

        send_many_starttls(&sink, 3).await;

        assert_eq!(sink.accepted(), 3);
        assert_eq!(sink.connections(), 3);
        let arrivals = sink.arrivals();
        let rcpts: Vec<&str> = arrivals.iter().map(|a| a.rcpt.as_str()).collect();
        assert_eq!(
            rcpts,
            [
                "person-0@example.com",
                "person-1@example.com",
                "person-2@example.com"
            ]
        );
        assert!(
            arrivals
                .windows(2)
                .all(|pair| pair[0].at_us <= pair[1].at_us)
        );
        assert!(arrivals[0].at_us > 1_700_000_000_000_000);
    }

    #[tokio::test]
    async fn plain_mode_takes_a_message_and_the_log_gets_one_line_per_message() {
        let dir = std::env::temp_dir().join(format!("sendbench-sink-{}", now_us()));
        assert!(std::fs::create_dir_all(&dir).is_ok());
        let log = dir.join("arrivals.ndjson");
        let mut config = test_config(TlsMode::Plain);
        config.log = Some(log.clone());
        let started = Sink::start(loopback(), config).await;
        assert!(started.is_ok());
        let Ok(sink) = started else { return };

        let stream = TcpStream::connect(("127.0.0.1", sink.port())).await;
        assert!(stream.is_ok());
        let Ok(stream) = stream else { return };
        let mut io = BufReader::new(stream);
        assert!(reply_of(&mut io).await.starts_with("220"));
        send_one(&mut io, "ada@example.com").await;

        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "{text}");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap_or_default();
        assert_eq!(parsed["rcpt"], "ada@example.com");
        assert!(parsed["t_us"].as_u64().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_modes_parse_by_name() {
        assert_eq!(TlsMode::parse("starttls"), Some(TlsMode::StartTls));
        assert_eq!(TlsMode::parse("tls"), Some(TlsMode::Implicit));
        assert_eq!(TlsMode::parse("none"), Some(TlsMode::Plain));
        assert_eq!(TlsMode::parse("ssl"), None);
    }

    #[test]
    fn a_path_is_the_text_between_the_angle_brackets() {
        assert_eq!(path_of("RCPT TO:<a@b.test> NOTIFY=NEVER"), "a@b.test");
        assert_eq!(path_of("MAIL FROM:<>"), "");
    }
}
