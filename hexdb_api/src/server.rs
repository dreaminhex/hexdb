// HexDB API: the HTTP(S) server
//
// A small accept loop on hyper-util, so HexDB controls what a client can hold
// open:
//
//   * at most `limits.max_connections` connections at once (more wait in the
//     listen backlog until one closes), and `max_connections_per_client` per
//     client address (more are closed immediately);
//   * `limits.header_timeout_seconds` to finish the TLS handshake and to send
//     each request's headers (this also closes idle keep-alive connections);
//   * `limits.request_timeout_seconds` per request, enforced by the
//     `request_timeout` middleware (long polls and streams are exempt).
//
// Each request gets the peer address as `ConnectInfo<SocketAddr>`. On
// shutdown the loop stops accepting and waits (up to 30 s) for open requests.

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{ConnectInfo, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use hexdb_core::config::{LimitsConfig, TlsConfig};
use hyper::body::Incoming;
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::{conn::auto::Builder, graceful::GracefulShutdown},
};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use tracing::{debug, info, warn};

/// How long open requests may take to finish after shutdown starts.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Load a PEM certificate chain and private key into a TLS acceptor (HTTP/1.1 and HTTP/2).
pub fn tls_acceptor(tls: &TlsConfig) -> Result<TlsAcceptor> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certs = CertificateDer::pem_file_iter(&tls.cert_file)
        .with_context(|| format!("Failed to read tls.cert_file {}", tls.cert_file))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("tls.cert_file {} is not a PEM certificate chain", tls.cert_file))?;
    if certs.is_empty() {
        return Err(anyhow!("tls.cert_file {} contains no certificates", tls.cert_file));
    }
    let key = PrivateKeyDer::from_pem_file(&tls.key_file)
        .with_context(|| format!("Failed to read a private key from tls.key_file {}", tls.key_file))?;
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow!("tls.cert_file / tls.key_file: {}", e))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Open connections per client address.
#[derive(Default)]
struct PerClient(Mutex<HashMap<IpAddr, usize>>);

impl PerClient {
    fn enter(self: &Arc<Self>, ip: IpAddr, max: usize) -> Option<ClientSlot> {
        let mut counts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(ip).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(ClientSlot { owner: self.clone(), ip })
    }
}

struct ClientSlot {
    owner: Arc<PerClient>,
    ip: IpAddr,
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut counts = self.owner.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

/// Serve `app` on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsAcceptor>,
    limits: &LimitsConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let connections = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let per_client = Arc::new(PerClient::default());
    let per_client_max = limits.max_connections_per_client.max(1);
    let header_timeout = Duration::from_secs(limits.header_timeout_seconds.max(1));

    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_timeout)
        .max_buf_size(256 * 1024);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20))
        .max_concurrent_streams(256);
    // With TLS, ALPN has already chosen the protocol; plain connections are sniffed.
    let builders = Arc::new([builder.clone(), builder.clone().http1_only(), builder.http2_only()]);
    let graceful = GracefulShutdown::new();

    tokio::pin!(shutdown);
    loop {
        // Wait for a free connection slot before accepting more.
        let permit = tokio::select! {
            permit = connections.clone().acquire_owned() => permit.expect("semaphore is never closed"),
            _ = &mut shutdown => break,
        };
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    // Usually out of file descriptors; back off instead of spinning.
                    warn!("⚠️ Failed to accept a connection: {}", e);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let Some(slot) = per_client.enter(peer.ip(), per_client_max) else {
            debug!(client = %peer.ip(), "Closing a connection: too many open connections from this client.");
            continue;
        };
        let _ = stream.set_nodelay(true);
        let router = app.clone();
        let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer));
            router.clone().oneshot(request.map(axum::body::Body::new))
        });
        let builders = builders.clone();
        let watcher = graceful.watcher();
        let tls = tls.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _slot = slot;
            let result = match tls {
                Some(acceptor) => match tokio::time::timeout(header_timeout, acceptor.accept(stream)).await {
                    Ok(Ok(stream)) => {
                        let builder = if stream.get_ref().1.alpn_protocol() == Some(b"h2") { &builders[2] } else { &builders[1] };
                        watcher.watch(builder.serve_connection_with_upgrades(TokioIo::new(stream), service).into_owned()).await
                    }
                    Ok(Err(e)) => return debug!(client = %peer.ip(), "TLS handshake failed: {}", e),
                    Err(_) => return debug!(client = %peer.ip(), "TLS handshake timed out."),
                },
                None => {
                    // Protocol detection waits for the first bytes without a
                    // timeout of its own, so a silent client is dropped here.
                    let mut first = [0u8; 1];
                    match tokio::time::timeout(header_timeout, stream.peek(&mut first)).await {
                        Ok(Ok(n)) if n > 0 => {}
                        _ => return debug!(client = %peer.ip(), "Closing a connection that sent nothing."),
                    }
                    watcher.watch(builders[0].serve_connection_with_upgrades(TokioIo::new(stream), service).into_owned()).await
                }
            };
            if let Err(e) = result {
                debug!(client = %peer.ip(), "Connection ended with an error: {}", e);
            }
        });
    }

    drop(listener);
    info!("🛑 HexDB is shutting down gracefully...");
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await.is_err() {
        warn!("⚠️ Some connections were still open {} s after shutdown started; closing them.", SHUTDOWN_GRACE.as_secs());
    }
    Ok(())
}

/// Routes that hold a request open on purpose (long polls and streams). They
/// cap their own waits, so the request timeout doesn't apply.
pub(crate) fn is_long_running(path: &str) -> bool {
    matches!(path, "/changes" | "/changes/stream" | "/lattice/changes")
        // A2A answers (and their event streams) take as long as the chat's lookups; the chat bounds them.
        || path == "/a2a"
        // So are chat answers, streamed or not.
        || path == "/ai/chat"
        // A security review waits for the AI's answer.
        || path == "/security/reviews"
        // A backup takes as long as copying the data needs (it runs as a task of its own).
        || path == "/backup"
        // File uploads and downloads take as long as the file needs.
        || path.contains("/_files/")
        || path.contains("/_uploads/")
        || path.ends_with("/content")
        || (path.starts_with("/streams/") && (path.ends_with("/messages") || path.ends_with("/subscribe")))
}

/// A request body that notes when it has been read to its end, and if it's dropped before
/// that, leaves the rest in `rest` (see `drain_unread`).
struct TrackedBody {
    inner: Option<axum::body::Body>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set once any of it was asked for: the handler meant to read it.
    started: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rest: std::sync::Arc<std::sync::Mutex<Option<axum::body::Body>>>,
}

impl http_body::Body for TrackedBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.started.store(true, std::sync::atomic::Ordering::Relaxed);
        let Some(inner) = self.inner.as_mut() else { return std::task::Poll::Ready(None) };
        let polled = std::pin::Pin::new(inner).poll_frame(cx);
        if let std::task::Poll::Ready(None) = &polled {
            self.done.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.as_ref().is_none_or(|b| b.is_end_stream())
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.as_ref().map(|b| b.size_hint()).unwrap_or_default()
    }
}

impl Drop for TrackedBody {
    fn drop(&mut self) {
        if !self.done.load(std::sync::atomic::Ordering::Relaxed) {
            if let (Some(body), Ok(mut rest)) = (self.inner.take(), self.rest.lock()) {
                *rest = Some(body);
            }
        }
    }
}

/// Most of an unread request body read (and dropped) to keep its connection usable.
const DRAIN_LIMIT: usize = 32 * 1024 * 1024;
/// Longest spent reading it.
const DRAIN_TIME: Duration = Duration::from_secs(5);

/// An answer given without reading its request's body at all (an upload refused at once: over a
/// quota, not allowed): the body is read and dropped first (up to 32 MB, for 5 s), so the client
/// finishes sending, reads the answer, and can use the connection again. A bigger or slower body,
/// or one a handler began to read and gave up on (a request timeout, a client sending too slowly),
/// ends the HTTP/1 connection instead, and the answer says so (`Connection: close`) so the client
/// doesn't put it back in its pool and lose its next request on it.
pub async fn drain_unread(request: Request, next: Next) -> Response {
    use http_body::Body as _;
    if request.version() > axum::http::Version::HTTP_11 || request.body().is_end_stream() {
        return next.run(request).await;
    }
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rest = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (parts, body) = request.into_parts();
    let tracked = axum::body::Body::new(TrackedBody { inner: Some(body), done: done.clone(), started: started.clone(), rest: rest.clone() });
    let mut response = next.run(Request::from_parts(parts, tracked)).await;
    if done.load(std::sync::atomic::Ordering::Relaxed) {
        return response;
    }
    let unread = rest.lock().ok().and_then(|mut r| r.take());
    // A body a handler began to read and gave up on may never arrive: don't wait for it.
    let unread = unread.filter(|_| !started.load(std::sync::atomic::Ordering::Relaxed));
    let drained = match unread {
        Some(body) => matches!(tokio::time::timeout(DRAIN_TIME, axum::body::to_bytes(body, DRAIN_LIMIT)).await, Ok(Ok(_))),
        // Still held somewhere (a task that outlives the request): don't wait for it.
        None => false,
    };
    if !drained {
        response.headers_mut().insert(axum::http::header::CONNECTION, axum::http::HeaderValue::from_static("close"));
    }
    response
}

/// Fail requests that take longer than `limits.request_timeout_seconds` with 503.
pub async fn request_timeout(axum::extract::State(timeout): axum::extract::State<Duration>, request: Request, next: Next) -> Response {
    if is_long_running(request.uri().path()) {
        return next.run(request).await;
    }
    match tokio::time::timeout(timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => crate::handlers::ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "timeout",
            format!("The request took longer than {} seconds.", timeout.as_secs()),
        )
        .into_response(),
    }
}
