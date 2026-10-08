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
fn is_long_running(path: &str) -> bool {
    matches!(path, "/changes" | "/changes/stream" | "/lattice/changes")
        || (path.starts_with("/streams/") && (path.ends_with("/messages") || path.ends_with("/subscribe")))
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
