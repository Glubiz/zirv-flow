use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(2);

/// Probes `GET /readyz` on the configured bind address. Exits 0 on a 200 and 1 otherwise.
pub(crate) async fn run(bind: SocketAddr, stderr: &mut impl Write) -> u8 {
    match tokio::time::timeout(TIMEOUT, probe(target(bind))).await {
        Ok(Ok(())) => 0,
        Ok(Err(message)) => fail(stderr, &message),
        Err(_) => fail(stderr, "timed out"),
    }
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

async fn probe(target: SocketAddr) -> Result<(), String> {
    let mut stream = TcpStream::connect(target)
        .await
        .map_err(|error| format!("connect {target}: {error}"))?;
    let request = format!("GET /readyz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("send: {error}"))?;

    let mut head = Vec::new();
    let mut chunk = [0u8; 256];
    while !head.contains(&b'\n') && head.len() < 1024 {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("receive: {error}"))?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..read]);
    }

    let line = String::from_utf8_lossy(&head);
    let line = line.lines().next().unwrap_or_default();
    match line.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        _ => Err(format!("unexpected response: {line:?}")),
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

        assert_eq!(run(addr, &mut stderr).await, 0);
        assert!(stderr.is_empty());
    }

    #[tokio::test]
    async fn a_503_response_exits_one() {
        let addr = answering("HTTP/1.1 503 Service Unavailable\r\n\r\n").await;
        let mut stderr = Vec::new();

        assert_eq!(run(addr, &mut stderr).await, 1);
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

        assert_eq!(run(addr, &mut stderr).await, 1);
        assert!(!stderr.is_empty());
    }

    #[tokio::test]
    async fn a_listener_that_never_answers_times_out_with_exit_one() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut stderr = Vec::new();

        let started = std::time::Instant::now();
        assert_eq!(run(addr, &mut stderr).await, 1);

        assert!(started.elapsed() < TIMEOUT + Duration::from_secs(2));
        assert!(String::from_utf8(stderr).unwrap().contains("timed out"));
        drop(listener);
    }
}
