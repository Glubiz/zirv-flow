//! A local SMTP server for tests, behind `test-support` (architecture 10.2): it listens on
//! 127.0.0.1, records every message it accepts, can be told to answer a chosen reply at `MAIL`,
//! `RCPT`, `DATA` or the end of the message, and can offer `STARTTLS` or speak implicit TLS with
//! a certificate from a generated authority (its PEM is [`SmtpSink::ca_pem`]). It is not an SMTP
//! implementation to rely on: it understands what the destination sends and nothing more.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hashbrown::HashMap;
use rustls::ServerConfig;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

/// The name the sink's certificate is for, besides `localhost` and 127.0.0.1.
pub const SINK_HOST: &str = "mail.example.test";

/// How the sink secures its connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SinkTls {
    /// No TLS is offered.
    Plain,
    /// `STARTTLS` is offered, and `AUTH` and `MAIL` are refused (530) until it was used.
    StartTls,
    /// Every connection starts with the handshake.
    Implicit,
}

/// The step a chosen reply replaces.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SinkStage {
    Mail,
    Rcpt,
    Data,
    /// The reply to the end of the message.
    DataEnd,
}

/// How a sink behaves.
#[derive(Clone, Debug)]
pub struct SinkOptions {
    pub tls: SinkTls,
    /// The user name and password it accepts; none means it offers no authentication.
    pub auth: Option<(String, String)>,
}

impl Default for SinkOptions {
    fn default() -> Self {
        Self {
            tls: SinkTls::Plain,
            auth: None,
        }
    }
}

/// A message the sink accepted. It names addresses: for assertions, never for logs.
pub struct Captured {
    pub mail_from: String,
    pub recipients: Vec<String>,
    /// The message as it was sent, with the dot-stuffing undone.
    pub data: Vec<u8>,
}

#[derive(Default)]
struct State {
    messages: Vec<Captured>,
    connections: usize,
    authenticated: usize,
    refused_auth: usize,
    replies: HashMap<SinkStage, String>,
    /// Replies for the next arrivals at a stage, one each, before `replies` applies.
    next: HashMap<SinkStage, Vec<String>>,
    /// Every message whose data was received, accepted or not.
    offered: Vec<Vec<u8>>,
}

/// A running sink. Dropping it stops it.
pub struct SmtpSink {
    address: SocketAddr,
    ca_pem: String,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl SmtpSink {
    /// Starts a sink on a free port of 127.0.0.1.
    pub async fn start(options: SinkOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a free port");
        let address = listener.local_addr().expect("a local address");
        let (acceptor, ca_pem) = certificate();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                shared.lock().expect("sink state").connections += 1;
                tokio::spawn(connection(
                    stream,
                    options.clone(),
                    acceptor.clone(),
                    Arc::clone(&shared),
                ));
            }
        });
        Self {
            address,
            ca_pem,
            state,
            task,
        }
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// The certificate authority that signed the sink's certificate, as PEM.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// Answers `reply` (for example `550 5.1.1 no such user`) at `stage` from now on.
    pub fn reply_at(&self, stage: SinkStage, reply: &str) {
        self.state
            .lock()
            .expect("sink state")
            .replies
            .insert(stage, reply.to_owned());
    }

    /// Answers `reply` at `stage` the next time only; replies queued this way are used in order.
    pub fn reply_next(&self, stage: SinkStage, reply: &str) {
        self.state
            .lock()
            .expect("sink state")
            .next
            .entry(stage)
            .or_default()
            .push(reply.to_owned());
    }

    /// The data of every message received, whether the sink accepted it or not.
    pub fn offered(&self) -> Vec<Vec<u8>> {
        self.state.lock().expect("sink state").offered.clone()
    }

    /// The messages accepted so far.
    pub fn messages(&self) -> Vec<Captured> {
        let state = self.state.lock().expect("sink state");
        state
            .messages
            .iter()
            .map(|message| Captured {
                mail_from: message.mail_from.clone(),
                recipients: message.recipients.clone(),
                data: message.data.clone(),
            })
            .collect()
    }

    /// How many connections were opened.
    pub fn connections(&self) -> usize {
        self.state.lock().expect("sink state").connections
    }

    /// How many `AUTH` exchanges succeeded and how many were refused.
    pub fn authentications(&self) -> (usize, usize) {
        let state = self.state.lock().expect("sink state");
        (state.authenticated, state.refused_auth)
    }
}

impl Drop for SmtpSink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A generated authority and the acceptor for a certificate it signed.
fn certificate() -> (TlsAcceptor, String) {
    let ca_key = rcgen::KeyPair::generate().expect("a key");
    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("parameters");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).expect("a CA");
    let issuer = rcgen::Issuer::new(ca_params, ca_key);

    let server_key = rcgen::KeyPair::generate().expect("a key");
    let mut params =
        rcgen::CertificateParams::new(vec![SINK_HOST.to_owned(), "localhost".to_owned()])
            .expect("parameters");
    params.subject_alt_names.push(rcgen::SanType::IpAddress(
        "127.0.0.1".parse().expect("an address"),
    ));
    let server = params
        .signed_by(&server_key, &issuer)
        .expect("a certificate");
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(server_key.serialize_der()));
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![server.der().clone()], key)
            .expect("a server configuration");
    (TlsAcceptor::from(Arc::new(config)), ca.pem())
}

/// What the conversation on one stream came to.
enum Ended {
    Closed,
    /// The client asked for `STARTTLS` and was told to go ahead.
    Upgrade,
}

async fn connection(
    stream: TcpStream,
    options: SinkOptions,
    acceptor: TlsAcceptor,
    state: Arc<Mutex<State>>,
) {
    match options.tls {
        SinkTls::Implicit => {
            if let Ok(tls) = acceptor.accept(stream).await {
                let mut io = BufReader::new(tls);
                let _ = converse(&mut io, &options, &state, true, true).await;
            }
        }
        SinkTls::Plain => {
            let mut io = BufReader::new(stream);
            let _ = converse(&mut io, &options, &state, false, true).await;
        }
        SinkTls::StartTls => {
            let mut io = BufReader::new(stream);
            if let Some(Ended::Upgrade) = converse(&mut io, &options, &state, false, true).await
                && let Ok(tls) = acceptor.accept(io.into_inner()).await
            {
                let mut io = BufReader::new(tls);
                let _ = converse(&mut io, &options, &state, true, false).await;
            }
        }
    }
}

async fn reply<S: AsyncWrite + Unpin>(writer: &mut S, text: &str) -> bool {
    writer.write_all(text.as_bytes()).await.is_ok() && writer.write_all(b"\r\n").await.is_ok()
}

fn stage_reply(state: &Arc<Mutex<State>>, stage: SinkStage, default: &str) -> String {
    let mut state = state.lock().expect("sink state");
    if let Some(queue) = state.next.get_mut(&stage)
        && !queue.is_empty()
    {
        return queue.remove(0);
    }
    state
        .replies
        .get(&stage)
        .cloned()
        .unwrap_or_else(|| default.to_owned())
}

/// Serves one stream until QUIT, a close or STARTTLS. `greet` sends the 220 greeting.
async fn converse<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut BufReader<S>,
    options: &SinkOptions,
    state: &Arc<Mutex<State>>,
    secured: bool,
    greet: bool,
) -> Option<Ended> {
    if greet && !reply(io, "220 mail.example.test ESMTP sink").await {
        return Some(Ended::Closed);
    }
    let mut mail_from = String::new();
    let mut recipients: Vec<String> = Vec::new();
    let mut authed = options.auth.is_none();
    let mut line = String::new();
    loop {
        line.clear();
        if io.read_line(&mut line).await.ok()? == 0 {
            return Some(Ended::Closed);
        }
        let command = line.trim_end().to_owned();
        let upper = command.to_ascii_uppercase();
        let needs_tls = options.tls == SinkTls::StartTls && !secured;
        let ok = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            let mut text = String::from("250-mail.example.test\r\n250-SIZE 10485760\r\n");
            if needs_tls {
                text.push_str("250-STARTTLS\r\n");
            }
            if options.auth.is_some() && !needs_tls {
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
            let argument = command.get("AUTH PLAIN".len()..).unwrap_or_default().trim();
            let argument = if argument.is_empty() {
                if !reply(io, "334 ").await {
                    return Some(Ended::Closed);
                }
                line.clear();
                if io.read_line(&mut line).await.ok()? == 0 {
                    return Some(Ended::Closed);
                }
                line.trim_end().to_owned()
            } else {
                argument.to_owned()
            };
            let decoded = STANDARD.decode(argument).unwrap_or_default();
            let parts: Vec<&[u8]> = decoded.split(|byte| *byte == 0).collect();
            let good = options.auth.as_ref().is_some_and(|(user, password)| {
                parts.len() == 3 && parts[1] == user.as_bytes() && parts[2] == password.as_bytes()
            });
            authed = good;
            answer_auth(io, state, good).await
        } else if upper == "AUTH LOGIN" {
            let user = prompt(io, "334 VXNlcm5hbWU6").await;
            let password = prompt(io, "334 UGFzc3dvcmQ6").await;
            let good = matches!((&user, &password, &options.auth), (Some(u), Some(p), Some((eu, ep))) if u == eu && p == ep);
            authed = good;
            answer_auth(io, state, good).await
        } else if upper.starts_with("MAIL FROM:") {
            if needs_tls {
                reply(io, "530 5.7.0 Must issue a STARTTLS command first").await
            } else if !authed {
                reply(io, "530 5.7.0 Authentication required").await
            } else {
                let chosen = stage_reply(state, SinkStage::Mail, "250 2.1.0 ok");
                if chosen.starts_with('2') {
                    mail_from = path_of(&command);
                    recipients.clear();
                }
                reply(io, &chosen).await
            }
        } else if upper.starts_with("RCPT TO:") {
            let chosen = stage_reply(state, SinkStage::Rcpt, "250 2.1.5 ok");
            if chosen.starts_with('2') {
                recipients.push(path_of(&command));
            }
            reply(io, &chosen).await
        } else if upper == "DATA" {
            let chosen = stage_reply(state, SinkStage::Data, "354 end with <CRLF>.<CRLF>");
            if !reply(io, &chosen).await {
                return Some(Ended::Closed);
            }
            if !chosen.starts_with("354") {
                continue;
            }
            let mut data: Vec<u8> = Vec::new();
            loop {
                let mut raw = Vec::new();
                if io.read_until(b'\n', &mut raw).await.ok()? == 0 {
                    return Some(Ended::Closed);
                }
                if raw == b".\r\n" {
                    break;
                }
                let unstuffed = raw
                    .strip_prefix(b".")
                    .filter(|_| raw.starts_with(b".."))
                    .unwrap_or(&raw);
                data.extend_from_slice(unstuffed);
            }
            state.lock().expect("sink state").offered.push(data.clone());
            let chosen = stage_reply(state, SinkStage::DataEnd, "250 2.0.0 queued");
            if chosen.starts_with('2') {
                state.lock().expect("sink state").messages.push(Captured {
                    mail_from: std::mem::take(&mut mail_from),
                    recipients: std::mem::take(&mut recipients),
                    data,
                });
            }
            reply(io, &chosen).await
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

/// Sends `text` and reads the base64 answer to it.
async fn prompt<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut BufReader<S>,
    text: &str,
) -> Option<String> {
    if !reply(&mut *io, text).await {
        return None;
    }
    let mut answer = String::new();
    if io.read_line(&mut answer).await.ok()? == 0 {
        return None;
    }
    String::from_utf8(STANDARD.decode(answer.trim()).ok()?).ok()
}

async fn answer_auth<S: AsyncWrite + Unpin>(
    writer: &mut S,
    state: &Arc<Mutex<State>>,
    good: bool,
) -> bool {
    {
        let mut state = state.lock().expect("sink state");
        if good {
            state.authenticated += 1;
        } else {
            state.refused_auth += 1;
        }
    }
    if good {
        reply(writer, "235 2.7.0 authenticated").await
    } else {
        reply(writer, "535 5.7.8 authentication failed").await
    }
}

/// The address between the angle brackets of a `MAIL FROM:<...>` or `RCPT TO:<...>` command.
fn path_of(command: &str) -> String {
    let start = command.find('<').map_or(0, |at| at + 1);
    let end = command.rfind('>').unwrap_or(command.len());
    command.get(start..end).unwrap_or_default().to_owned()
}
