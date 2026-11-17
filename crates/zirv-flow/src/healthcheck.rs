use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(2);

/// Why a probe failed: the connection was refused or failed, or the listener answered badly.
enum Failure {
    Connect(String),
    Other(String),
}

/// Probes `GET /readyz` on the configured bind address. Exits 0 on a 200 and 1 otherwise.
///
/// A worker or scheduler process has no listener on `bind`; its `/readyz` is on `ops`. So when
/// `bind` cannot be connected to and `ops` is set, `ops` is probed and its result reported. A
/// listener on `bind` that answers badly or not at all never falls back: it is the unhealthy one.
pub(crate) async fn run(bind: SocketAddr, ops: Option<SocketAddr>, stderr: &mut impl Write) -> u8 {
    let failure = match probe_within(bind).await {
        Ok(()) => return 0,
        Err(failure) => failure,
    };
    let failure = match (failure, ops) {
        (Failure::Connect(_), Some(ops)) => match probe_within(ops).await {
            Ok(()) => return 0,
            Err(failure) => failure,
        },
        (failure, _) => failure,
    };
    let (Failure::Connect(message) | Failure::Other(message)) = failure;
    fail(stderr, &message)
}

/// One probe of `bind` within [`TIMEOUT`]. A connect that is refused or does not complete in time
/// is a [`Failure::Connect`]: nothing listens there. Once connected, a silent listener is not.
async fn probe_within(bind: SocketAddr) -> Result<(), Failure> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let target = target(bind);
    let stream = match tokio::time::timeout_at(deadline, TcpStream::connect(target)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => return Err(Failure::Connect(format!("connect {target}: {error}"))),
        Err(_) => return Err(Failure::Connect(format!("connect {target}: timed out"))),
    };
    tokio::time::timeout_at(deadline, probe(stream, target))
        .await
        .unwrap_or_else(|_| Err(Failure::Other("timed out".to_owned())))
}

fn fail(stderr: &mut impl Write, message: &str) -> u8 {
    let _ = writeln!(stderr, "error: healthcheck: {message}");
    1
}

/// An unspecified bind address is reached through the loopback address of the same family.
fn target(bind: SocketAddr) -> SocketAddr {
    let ip = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bind.port())
}

async fn probe(mut stream: TcpStream, target: SocketAddr) -> Result<(), Failure> {
    let other = Failure::Other;
    let request = format!("GET /readyz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| other(format!("send: {error}")))?;

    let mut head = Vec::new();
    let mut chunk = [0u8; 256];
    while !head.contains(&b'\n') && head.len() < 1024 {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| other(format!("receive: {error}")))?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
    }

    let line = String::from_utf8_lossy(&head);
    let line = line.lines().next().unwrap_or_default();
    match line.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        _ => Err(other(format!("unexpected response: {line:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn answering(response: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).await;
            let _ = stream.write_all(response.as_bytes()).await;
        });
        addr
    }

    #[test]
    fn unspecified_addresses_map_to_loopback_of_the_same_family() {
        assert_eq!(
            target("0.0.0.0:8080".parse().unwrap()),
            "127.0.0.1:8080".parse().unwrap()
        );
        assert_eq!(
            target("[::]:8080".parse().unwrap()),
            "[::1]:8080".parse().unwrap()
        );
        assert_eq!(
            target("10.1.2.3:9".parse().unwrap()),
            "10.1.2.3:9".parse().unwrap()
        );
    }

    #[tokio::test]
    async fn a_200_response_exits_zero_and_prints_nothing() {
        let addr = answering("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        let mut stderr = Vec::new();

        assert_eq!(run(addr, None, &mut stderr).await, 0);
        assert!(stderr.is_empty());
    }

    #[tokio::test]
    async fn a_503_response_exits_one() {
        let addr = answering("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
        let mut stderr = Vec::new();

        assert_eq!(run(addr, None, &mut stderr).await, 1);
        let message = String::from_utf8(stderr).unwrap();
        assert!(message.starts_with("error: healthcheck: "), "{message}");
    }

    #[tokio::test]
    async fn a_refused_connection_exits_one() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let mut stderr = Vec::new();

        assert_eq!(run(addr, None, &mut stderr).await, 1);
        assert!(!stderr.is_empty());
    }

    async fn unused() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
    }

    #[tokio::test]
    async fn a_refused_server_bind_falls_back_to_the_ops_bind() {
        let ops = answering("HTTP/1.1 200 OK\r\n\r\n").await;
        let mut stderr = Vec::new();

        assert_eq!(run(unused().await, Some(ops), &mut stderr).await, 0);
        assert!(stderr.is_empty());

        let ops = answering("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
        assert_eq!(run(unused().await, Some(ops), &mut stderr).await, 1);
        assert!(String::from_utf8(stderr).unwrap().contains("503"));
    }

    #[tokio::test]
    async fn a_server_bind_that_answers_503_never_falls_back_to_the_ops_bind() {
        let server = answering("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
        let ops = answering("HTTP/1.1 200 OK\r\n\r\n").await;
        let mut stderr = Vec::new();

        assert_eq!(run(server, Some(ops), &mut stderr).await, 1);
        assert!(String::from_utf8(stderr).unwrap().contains("503"));
    }

    #[tokio::test]
    async fn when_neither_bind_listens_the_failure_is_named() {
        let mut stderr = Vec::new();

        assert_eq!(
            run(unused().await, Some(unused().await), &mut stderr).await,
            1
        );
        assert!(String::from_utf8(stderr).unwrap().contains("connect"));
    }

    #[tokio::test]
    async fn a_listener_that_never_answers_times_out_with_exit_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut stderr = Vec::new();

        let started = std::time::Instant::now();
        assert_eq!(run(addr, None, &mut stderr).await, 1);

        assert!(started.elapsed() < TIMEOUT + Duration::from_secs(2));
        assert!(String::from_utf8(stderr).unwrap().contains("timed out"));
        drop(listener);
    }
}
