//! The egress relay: a loopback SOCKS5 listener that is the only way the Kafka client reaches the
//! network (spec D6, ruling a37fbac4).
//!
//! rskafka has no resolver hook: it dials `host:port` itself for the bootstrap list and for every
//! broker the metadata names. Its one seam is a SOCKS5 proxy, to which it sends the host as a
//! name and over which it then does TLS with that name. This relay resolves the name, checks
//! every address against the egress policy, connects to the checked address and pipes bytes, so
//! the policy holds for metadata-learned brokers and on every reconnect.
//!
//! Residual risk: while a read runs, any local process can use the listener to reach destinations
//! the policy allows. The policy still applies to them.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use zirv_flow_core::egress::AddressClass;

use crate::http::HttpError;

use super::super::context::SourceContext;
use super::super::error::SourceError;

/// How long one upstream connection attempt, and the SOCKS handshake, may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

const REPLY_OK: u8 = 0;
const REPLY_RULESET: u8 = 2;
const REPLY_UNREACHABLE: u8 = 4;
const REPLY_COMMAND_UNSUPPORTED: u8 = 7;
const REPLY_ADDRESS_UNSUPPORTED: u8 = 8;

/// The refusal a connection met, shared with the reader that owns the relay.
#[derive(Clone, Default)]
pub(super) struct DenialSlot(Arc<Mutex<Option<AddressClass>>>);

impl DenialSlot {
    pub(super) fn get(&self) -> Option<AddressClass> {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set(&self, class: AddressClass) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(class);
    }
}

/// A running relay. Dropping it stops the listener and every connection it carries.
pub(super) struct Relay {
    address: SocketAddr,
    denial: DenialSlot,
    task: JoinHandle<()>,
}

impl Relay {
    /// Binds a loopback port and starts serving.
    pub(super) async fn start(ctx: SourceContext) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let denial = DenialSlot::default();
        let slot = denial.clone();
        let task = tokio::spawn(async move {
            // Dropped with the task, which aborts every connection.
            let mut connections = JoinSet::new();
            while let Ok((client, _)) = listener.accept().await {
                while connections.try_join_next().is_some() {}
                let ctx = ctx.clone();
                let slot = slot.clone();
                connections.spawn(async move {
                    let _ = serve(client, &ctx, &slot).await;
                });
            }
        });
        Ok(Self {
            address,
            denial,
            task,
        })
    }

    /// The `host:port` to give rskafka as its SOCKS5 proxy.
    pub(super) fn proxy(&self) -> String {
        self.address.to_string()
    }

    /// The class of the address the policy refused, if one was.
    pub(super) fn denial(&self) -> Option<AddressClass> {
        self.denial.get()
    }

    /// The slot, for the calls that must tell a refusal from a failure.
    pub(super) fn slot(&self) -> DenialSlot {
        self.denial.clone()
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The destination a client asked for.
struct Target {
    host: String,
    port: u16,
}

async fn reply(client: &mut TcpStream, code: u8) -> io::Result<()> {
    client.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    client.flush().await
}

/// The SOCKS5 greeting and CONNECT request (RFC 1928): no authentication, CONNECT only, with
/// IPv4, IPv6 and domain addresses. Answers a request it cannot serve and returns `None`.
async fn handshake(client: &mut TcpStream) -> io::Result<Option<Target>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5");
    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(invalid());
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 0xff]).await?;
        return Ok(None);
    }
    client.write_all(&[5, 0]).await?;

    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    if request[0] != 5 {
        return Err(invalid());
    }
    if request[1] != 1 {
        reply(client, REPLY_COMMAND_UNSUPPORTED).await?;
        return Ok(None);
    }
    let host = match request[3] {
        1 => {
            let mut octets = [0u8; 4];
            client.read_exact(&mut octets).await?;
            IpAddr::from(octets).to_string()
        }
        4 => {
            let mut octets = [0u8; 16];
            client.read_exact(&mut octets).await?;
            IpAddr::from(octets).to_string()
        }
        3 => {
            let length = client.read_u8().await?;
            let mut name = vec![0u8; usize::from(length)];
            client.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| invalid())?
        }
        _ => {
            reply(client, REPLY_ADDRESS_UNSUPPORTED).await?;
            return Ok(None);
        }
    };
    let port = client.read_u16().await?;
    Ok(Some(Target { host, port }))
}

async fn serve(mut client: TcpStream, ctx: &SourceContext, denial: &DenialSlot) -> io::Result<()> {
    let Some(target) = timeout(STEP_TIMEOUT, handshake(&mut client))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??
    else {
        return Ok(());
    };
    // Resolved and checked at every connect: a changed answer is checked again, an IP literal
    // is decided as it is, and one denied address refuses the host.
    let addresses = match ctx.resolve_checked(&target.host).await {
        Ok(addresses) => addresses,
        Err(SourceError::Egress(HttpError::EgressDenied { class, .. })) => {
            denial.set(class);
            return reply(&mut client, REPLY_RULESET).await;
        }
        Err(_) => return reply(&mut client, REPLY_UNREACHABLE).await,
    };
    let mut upstream = None;
    for ip in addresses {
        if let Ok(Ok(stream)) = timeout(STEP_TIMEOUT, TcpStream::connect((ip, target.port))).await {
            upstream = Some(stream);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        return reply(&mut client, REPLY_UNREACHABLE).await;
    };
    reply(&mut client, REPLY_OK).await?;
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::net::TcpListener;

    use super::*;
    use crate::http::Lookup;
    use zirv_flow_core::egress::EgressPolicy;

    struct Table(BTreeMap<String, Vec<IpAddr>>);

    impl Lookup for Table {
        fn lookup<'a>(
            &'a self,
            host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move {
                self.0
                    .get(host)
                    .cloned()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            })
        }
    }

    fn table(entries: &[(&str, &[&str])]) -> Arc<dyn Lookup> {
        Arc::new(Table(
            entries
                .iter()
                .map(|(host, ips)| {
                    (
                        (*host).to_owned(),
                        ips.iter().map(|ip| ip.parse().unwrap()).collect(),
                    )
                })
                .collect(),
        ))
    }

    /// A listener that counts connections and echoes one line back.
    async fn target() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4];
                    if stream.read_exact(&mut buffer).await.is_ok() {
                        let _ = stream.write_all(&buffer).await;
                    }
                });
            }
        });
        (port, count)
    }

    /// Asks the relay to connect to `host:port` the way rskafka does, as a domain name, and
    /// returns the reply code.
    async fn connect(relay: &Relay, host: &str, port: u16) -> (u8, TcpStream) {
        let mut stream = TcpStream::connect(relay.proxy()).await.unwrap();
        stream.write_all(&[5, 1, 0]).await.unwrap();
        let mut method = [0u8; 2];
        stream.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);
        let mut request = vec![5, 1, 0, 3, u8::try_from(host.len()).unwrap()];
        request.extend_from_slice(host.as_bytes());
        request.extend_from_slice(&port.to_be_bytes());
        stream.write_all(&request).await.unwrap();
        let mut answer = [0u8; 10];
        stream.read_exact(&mut answer).await.unwrap();
        (answer[1], stream)
    }

    async fn relay(allow: &[&str], lookup: Arc<dyn Lookup>) -> Relay {
        let entries: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
        let policy = Arc::new(EgressPolicy::parse(&entries).unwrap());
        let ctx = SourceContext::new(
            policy,
            crate::http::HttpLimits::default(),
            "kafka",
            "source-1",
            Arc::default(),
        )
        .unwrap()
        .with_lookup(lookup);
        Relay::start(ctx).await.unwrap()
    }

    #[tokio::test]
    async fn an_allowed_name_is_connected_by_its_checked_address_and_bytes_flow() {
        let (port, count) = target().await;
        let relay = relay(&["127.0.0.0/8"], table(&[("broker.test", &["127.0.0.1"])])).await;

        let (code, mut stream) = connect(&relay, "broker.test", port).await;
        stream.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).await.unwrap();

        assert_eq!(code, REPLY_OK);
        assert_eq!(&echoed, b"ping");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(relay.denial(), None);
    }

    #[tokio::test]
    async fn an_ip_literal_sent_as_a_name_is_parsed_and_checked() {
        let (port, count) = target().await;
        let relay = relay(&["127.0.0.1/32"], table(&[])).await;

        let (code, _stream) = connect(&relay, "127.0.0.1", port).await;
        assert_eq!(code, REPLY_OK);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let denied = self::relay(&[], table(&[])).await;
        let (code, _stream) = connect(&denied, "127.0.0.1", port).await;
        assert_eq!(code, REPLY_RULESET);
        assert_eq!(denied.denial(), Some(AddressClass::Loopback));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn private_loopback_metadata_and_mixed_resolutions_are_refused_and_never_dialled() {
        let (port, count) = target().await;
        let lookup = table(&[
            ("private.test", &["10.0.0.1"]),
            ("loop.test", &["127.0.0.1"]),
            ("meta.test", &["169.254.169.254"]),
            ("mixed.test", &["8.8.8.8", "127.0.0.1"]),
        ]);
        let relay = relay(&[], lookup).await;

        for (host, class) in [
            ("private.test", AddressClass::Private),
            ("loop.test", AddressClass::Loopback),
            ("meta.test", AddressClass::Metadata),
            ("mixed.test", AddressClass::Loopback),
        ] {
            let (code, _stream) = connect(&relay, host, port).await;
            assert_eq!(code, REPLY_RULESET, "{host}");
            assert_eq!(relay.denial(), Some(class), "{host}");
        }
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "no connection reached a denied address"
        );
    }

    #[tokio::test]
    async fn an_unknown_name_and_a_closed_port_answer_unreachable() {
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        let relay = relay(&["127.0.0.0/8"], table(&[("closed.test", &["127.0.0.1"])])).await;

        let (unknown, _stream) = connect(&relay, "nowhere.test", 9092).await;
        let (refused, _stream) = connect(&relay, "closed.test", closed).await;

        assert_eq!(unknown, REPLY_UNREACHABLE);
        assert_eq!(refused, REPLY_UNREACHABLE);
        assert_eq!(relay.denial(), None);
    }

    #[tokio::test]
    async fn only_connect_without_authentication_is_served() {
        let relay = relay(&[], table(&[])).await;

        let mut needs_auth = TcpStream::connect(relay.proxy()).await.unwrap();
        needs_auth.write_all(&[5, 1, 2]).await.unwrap();
        let mut answer = [0u8; 2];
        needs_auth.read_exact(&mut answer).await.unwrap();
        assert_eq!(answer, [5, 0xff]);

        let mut bind = TcpStream::connect(relay.proxy()).await.unwrap();
        bind.write_all(&[5, 1, 0]).await.unwrap();
        bind.read_exact(&mut answer).await.unwrap();
        bind.write_all(&[5, 2, 0, 1]).await.unwrap();
        let mut reply = [0u8; 10];
        bind.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REPLY_COMMAND_UNSUPPORTED);
    }

    #[tokio::test]
    async fn dropping_the_relay_closes_the_listener() {
        let relay = relay(&[], table(&[])).await;
        let address = relay.proxy();
        drop(relay);
        tokio::task::yield_now().await;

        let mut closed = false;
        for _ in 0..50 {
            if TcpStream::connect(&address).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(closed);
    }
}
