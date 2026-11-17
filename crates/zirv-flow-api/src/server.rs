//! The accept loop of the HTTP listeners (`serve` and `serve_ops`): the bounds `axum::serve`
//! has no hook for. A client that never finishes its headers, or idles on a kept-alive
//! connection, is closed after [`HEADER_READ_TIMEOUT`], and no more than [`MAX_CONNECTIONS`]
//! connections are open at once, so a flood of silent connections cannot take the process's
//! descriptors or memory.

use std::future::Future;
use std::io;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::problem::ApiError;

/// How long a client has to send the headers of a request, which is also how long a kept-alive
/// connection may wait for the next one.
pub(crate) const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The most connections one listener keeps open; past it, the kernel's backlog queues new ones.
pub(crate) const MAX_CONNECTIONS: usize = 10_000;

/// How long an upload may take in all, from the request to the response. Its body has no size
/// limit, so this is the bound on a client that trickles it.
pub(crate) const UPLOAD_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// The bounds of one listener.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) header_read_timeout: Duration,
    pub(crate) max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_read_timeout: HEADER_READ_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
        }
    }
}

/// Serves `router` on `listener` until `shutdown` completes, then drains the open connections:
/// each finishes the request it is serving and closes.
pub(crate) async fn serve_router(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    limits: Limits,
) -> io::Result<()> {
    let slots = Arc::new(Semaphore::new(limits.max_connections));
    let (drain, draining) = watch::channel(false);
    let (close, closed) = watch::channel(());
    let mut shutdown = pin!(shutdown);
    loop {
        // The permit comes before the accept, so at the cap connections wait in the backlog.
        let permit = tokio::select! {
            permit = Arc::clone(&slots).acquire_owned() => {
                permit.expect("the connection semaphore is never closed")
            }
            () = &mut shutdown => break,
        };
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    accept_failed(&error).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        tokio::spawn(serve_connection(
            stream,
            router.clone(),
            limits.header_read_timeout,
            draining.clone(),
            permit,
            closed.clone(),
        ));
    }
    let _ = drain.send(true);
    drop(closed);
    drop(listener);
    close.closed().await;
    Ok(())
}

/// A refused or reset connection costs nothing; any other failure, such as running out of
/// descriptors, is retried after a pause rather than spun on.
async fn accept_failed(error: &io::Error) {
    use io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
    if !matches!(
        error.kind(),
        ConnectionRefused | ConnectionAborted | ConnectionReset
    ) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Serves one connection until it closes, and starts closing it gracefully when `draining`
/// turns true. Holds its slot and its place in the drain until it ends.
async fn serve_connection(
    stream: TcpStream,
    router: Router,
    header_read_timeout: Duration,
    mut draining: watch::Receiver<bool>,
    _slot: OwnedSemaphorePermit,
    _open: watch::Receiver<()>,
) {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let mut connection = pin!(
        builder
            .serve_connection(TokioIo::new(stream), TowerToHyperService::new(router))
            .with_upgrades()
    );
    let mut told = false;
    loop {
        tokio::select! {
            _ = connection.as_mut() => break,
            _ = draining.wait_for(|draining| *draining), if !told => {
                told = true;
                connection.as_mut().graceful_shutdown();
            }
        }
    }
}

/// Answers 408 when the request is not answered within `deadline`; a handler still reading a
/// trickling body is dropped with it.
pub(crate) async fn upload_deadline(
    State(deadline): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    match tokio::time::timeout(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "the upload did not finish in time",
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::Bytes;
    use axum::middleware::from_fn_with_state;
    use axum::routing::{get, post};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;

    use super::*;

    fn app() -> Router {
        Router::new()
            .route("/ping", get(|| async { "pong" }))
            .route(
                "/slow",
                post(|body: Bytes| async move { body.len().to_string() }).layer(
                    from_fn_with_state(Duration::from_millis(300), upload_deadline),
                ),
            )
    }

    fn limits(header_read_timeout: Duration, max_connections: usize) -> Limits {
        Limits {
            header_read_timeout,
            max_connections,
        }
    }

    async fn start(
        limits: Limits,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> (SocketAddr, JoinHandle<io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_router(listener, app(), shutdown, limits));
        (address, server)
    }

    /// Reads until the first response head is complete, or returns what arrived when the peer
    /// closed.
    async fn read_head(stream: &mut TcpStream) -> String {
        let mut received = Vec::new();
        let mut chunk = [0u8; 512];
        while !received.windows(4).any(|window| window == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => received.extend_from_slice(&chunk[..read]),
            }
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    /// Whether the server closes the connection within `within`.
    async fn closes_within(stream: &mut TcpStream, within: Duration) -> bool {
        let mut chunk = [0u8; 64];
        matches!(
            tokio::time::timeout(within, stream.read(&mut chunk)).await,
            Ok(Ok(0) | Err(_))
        )
    }

    const PING: &[u8] = b"GET /ping HTTP/1.1\r\nHost: x\r\n\r\n";

    #[tokio::test]
    async fn a_request_is_answered_through_the_loop() {
        let (address, _server) = start(Limits::default(), std::future::pending()).await;
        let mut stream = TcpStream::connect(address).await.unwrap();

        stream.write_all(PING).await.unwrap();

        assert!(read_head(&mut stream).await.starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn a_connection_that_never_finishes_its_headers_is_closed() {
        let (address, _server) = start(
            limits(Duration::from_millis(300), 100),
            std::future::pending(),
        )
        .await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(b"GET /ping HTTP/1.1\r\n").await.unwrap();

        assert!(closes_within(&mut stream, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_closed() {
        let (address, _server) = start(
            limits(Duration::from_millis(300), 100),
            std::future::pending(),
        )
        .await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(PING).await.unwrap();
        assert!(read_head(&mut stream).await.starts_with("HTTP/1.1 200"));

        assert!(closes_within(&mut stream, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn at_the_cap_a_connection_is_served_only_after_another_closes() {
        let (address, _server) =
            start(limits(Duration::from_secs(60), 1), std::future::pending()).await;
        let first = TcpStream::connect(address).await.unwrap();
        let mut second = TcpStream::connect(address).await.unwrap();
        second.write_all(PING).await.unwrap();

        let waiting =
            tokio::time::timeout(Duration::from_millis(500), read_head(&mut second)).await;
        assert!(waiting.is_err(), "served beyond the cap: {waiting:?}");

        drop(first);
        let head = tokio::time::timeout(Duration::from_secs(5), read_head(&mut second))
            .await
            .expect("served once the first connection closed");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    }

    #[tokio::test]
    async fn shutdown_returns_once_the_idle_connections_close() {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (address, server) = start(limits(Duration::from_secs(60), 100), async move {
            let _ = stopped.await;
        })
        .await;
        let mut idle = TcpStream::connect(address).await.unwrap();
        idle.write_all(PING).await.unwrap();
        assert!(read_head(&mut idle).await.starts_with("HTTP/1.1 200"));

        stop.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the drain ended")
            .unwrap()
            .unwrap();
        assert!(closes_within(&mut idle, Duration::from_secs(1)).await);
    }

    #[tokio::test]
    async fn an_upload_whose_body_stalls_answers_408() {
        let (address, _server) =
            start(limits(Duration::from_secs(60), 100), std::future::pending()).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"POST /slow HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\nab")
            .await
            .unwrap();

        let head = tokio::time::timeout(Duration::from_secs(5), read_head(&mut stream))
            .await
            .expect("answered");

        assert!(head.starts_with("HTTP/1.1 408"), "{head}");
        assert!(head.contains("application/problem+json"), "{head}");
    }

    #[tokio::test]
    async fn an_upload_that_finishes_in_time_is_not_cut_off() {
        let (address, _server) =
            start(limits(Duration::from_secs(60), 100), std::future::pending()).await;
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"POST /slow HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\nab")
            .await
            .unwrap();

        assert!(read_head(&mut stream).await.starts_with("HTTP/1.1 200"));
    }
}
