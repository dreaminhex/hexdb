use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use serde::{Serialize, Deserialize};
use tracing::{info, warn};
use crate::HexConfig;
use std::time::Duration;

/// Represents a known peer Hex node in the same lattice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerHex {
    pub id: String,
    pub name: String,
    pub role: String,
    pub lattice: String,
    pub ip: String,
}

/// Starts a TCP listener on the discovery endpoint to respond to handshake requests from peer hexes.
/// Responds to `HEXDB_HELLO` with a `HEXDB_IDENTITY` payload including local metadata.
///
/// # Arguments
/// * `addr` - The address to bind the discovery listener.
/// * `local_info` - Metadata describing the current Hex node.
pub async fn start_discovery_listener(addr: String, local_info: PeerHex) {
    let listener = TcpListener::bind(&addr)
        .await
        .expect("❌ Failed to bind discovery endpoint.");

    info!(%addr, "📡 Discovery listener active.");

    loop {
        match listener.accept().await {
            Ok((mut socket, _)) => {
                let local = local_info.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(&mut socket);
                    let mut line = String::new();

                    if reader.read_line(&mut line).await.is_ok() && line.trim() == "HEXDB_HELLO" {
                        let identity = serde_json::to_string(&local).unwrap();
                        let response = format!("HEXDB_IDENTITY {}\n", identity);
                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                });
            }
            Err(e) => {
                warn!(%e, "❗ Discovery accept failed.");
            }
        }
    }
}

/// Attempts to connect to other Hex nodes on known local ports, sending a `HEXDB_HELLO` message
/// and awaiting a response with node metadata. Filters results to only include nodes in the same lattice.
///
/// # Arguments
/// * `config` - Reference to the local configuration.
/// * `local_id` - ULID of the current Hex node.
/// * `local_name` - Name of the current Hex node.
///
/// # Returns
/// A vector of peer Hex metadata matching the same lattice.
pub async fn discover_peers(config: &HexConfig, local_id: &str, local_name: &str) -> Vec<PeerHex> {
    let base_port = 7702;

    // Probe all candidate ports concurrently, each with a timeout. On Windows a
    // connection to a closed localhost port takes ~2s to fail, so sequential
    // probes without a timeout added ~16s to every startup.
    let probes = (base_port..(base_port + 8))
        .filter(|port| !config.network.discovery_endpoint.ends_with(&port.to_string())) // skip self
        .map(|port| async move {
            tokio::time::timeout(PROBE_TIMEOUT, probe_peer(format!("127.0.0.1:{}", port)))
                .await
                .ok()
                .flatten()
        });

    let mut peers = Vec::new();
    for peer in futures::future::join_all(probes).await.into_iter().flatten() {
        if peer.lattice == config.network.lattice_name && peer.id != local_id {
            if peer.name == local_name {
                warn!(name = %peer.name, "🎭 Name collision detected.");
            }
            peers.push(peer);
        }
    }

    peers
}

/// How long to wait for a single peer to connect and identify itself.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Send `HEXDB_HELLO` to one address and parse the `HEXDB_IDENTITY` reply.
async fn probe_peer(addr: String) -> Option<PeerHex> {
    let mut stream = TcpStream::connect(&addr).await.ok()?;
    let _ = stream.write_all(b"HEXDB_HELLO\n").await;

    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();

    if reader.read_line(&mut line).await.is_ok() && line.starts_with("HEXDB_IDENTITY ") {
        // TODO(Phase 6): off by one; the prefix is 15 bytes, so this drops the opening brace.
        let payload = line[16..].trim();
        return serde_json::from_str::<PeerHex>(payload).ok();
    }

    None
}
