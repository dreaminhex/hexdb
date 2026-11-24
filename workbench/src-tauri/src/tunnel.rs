//! SSH tunnels to lattices that aren't reachable directly, through the system's
//! OpenSSH client (Windows 10+, macOS and Linux have it), so keys, agents and
//! ~/.ssh/config work as they do in a terminal.
//!
//! Host keys are always checked (StrictHostKeyChecking=yes): against the user's
//! known_hosts and the workbench's own, where a host is added only after the user
//! has seen and accepted its fingerprint. ssh never prompts (BatchMode=yes).

use crate::envs::Ssh;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::{Child, Command};

pub struct Tunnels {
    known_hosts: PathBuf,
    open: Mutex<HashMap<String, (u16, Child)>>,
}

/// A host's key as ssh-keyscan reports it, with its SHA-256 fingerprint for the user to check.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HostKey {
    pub line: String,
    pub kind: String,
    pub fingerprint: String,
}

impl Tunnels {
    pub fn new(dir: &Path) -> Tunnels {
        Tunnels { known_hosts: dir.join("known_hosts"), open: Mutex::new(HashMap::new()) }
    }

    fn known_hosts_option(&self) -> String {
        // The user's own file first, then the workbench's.
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default();
        format!("UserKnownHostsFile=\"{}\" \"{}\"", slash(&home.join(".ssh").join("known_hosts")), slash(&self.known_hosts))
    }

    /// The local port of `id`'s tunnel, opening it if needed.
    pub async fn open(&self, id: &str, ssh: &Ssh, remote: &reqwest::Url) -> Result<u16> {
        {
            let mut open = self.open.lock().unwrap();
            if let Some((port, child)) = open.get_mut(id) {
                if child.try_wait()?.is_none() {
                    return Ok(*port);
                }
                open.remove(id);
            }
        }
        let port = free_port()?;
        let target = format!("{}:{}", remote.host_str().unwrap_or("127.0.0.1"), remote.port_or_known_default().unwrap_or(7700));
        let mut cmd = Command::new("ssh");
        cmd.arg("-N")
            .args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=15"])
            .args(["-o", &self.known_hosts_option()])
            .args(["-L", &format!("127.0.0.1:{}:{}", port, target)])
            .args(["-p", &ssh.port.to_string()]);
        if !ssh.identity_file.is_empty() {
            cmd.args(["-i", &ssh.identity_file]);
        }
        cmd.arg(format!("{}@{}", ssh.user, ssh.host)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().context("Couldn't run ssh. Is OpenSSH installed?")?;
        // Ready when the local port answers; ssh exiting first means it failed (a host key, a refusal).
        for _ in 0..100 {
            if let Some(status) = child.try_wait()? {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = stderr.read_to_string(&mut err).await;
                }
                if err.contains("Host key verification failed") || err.contains("No ED25519 host key") || err.contains("host key") {
                    bail!("The host key of {} isn't known or has changed. Check its fingerprint and trust it first. ssh said: {}", ssh.host, err.trim());
                }
                bail!("ssh stopped ({}): {}", status, err.trim());
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                self.open.lock().unwrap().insert(id.to_string(), (port, child));
                return Ok(port);
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let _ = child.kill().await;
        Err(anyhow!("The tunnel to {} didn't open within 15 seconds.", ssh.host))
    }

    pub fn close(&self, id: &str) {
        if let Some((_, mut child)) = self.open.lock().unwrap().remove(id) {
            let _ = child.start_kill();
        }
    }

    /// The host's keys, for the user to compare with what its administrator gave them.
    pub async fn scan(&self, ssh: &Ssh) -> Result<Vec<HostKey>> {
        let out = Command::new("ssh-keyscan").args(["-p", &ssh.port.to_string(), "-T", "10", &ssh.host]).output().await.context("Couldn't run ssh-keyscan")?;
        let lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).map(str::to_string).collect();
        if lines.is_empty() {
            bail!("{} didn't offer a host key.", ssh.host);
        }
        let mut keys = Vec::new();
        for line in lines {
            let mut parts = line.split_whitespace();
            let (_, kind, key) = (parts.next(), parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default());
            keys.push(HostKey { fingerprint: fingerprint(key)?, kind, line });
        }
        Ok(keys)
    }

    /// Adds keys the user accepted to the workbench's known_hosts.
    pub fn trust(&self, keys: &[HostKey]) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = self.known_hosts.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.known_hosts)?;
        for key in keys {
            if key.line.contains('\n') || key.line.split_whitespace().count() < 3 {
                bail!("That isn't a host key line.");
            }
            writeln!(file, "{}", key.line.trim())?;
        }
        Ok(())
    }
}

fn slash(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// `SHA256:<base64 without padding>` of a base64 public key blob, as ssh-keygen -l prints it.
pub fn fingerprint(key_base64: &str) -> Result<String> {
    let blob = base64_decode(key_base64).ok_or_else(|| anyhow!("ssh-keyscan returned a key that isn't base64"))?;
    let digest = sha256(&blob);
    Ok(format!("SHA256:{}", base64_encode(&digest).trim_end_matches('=')))
}

// Small, dependency-free base64 and SHA-256 for fingerprints.

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.trim_end_matches('=').bytes() {
        acc = acc << 6 | B64.iter().position(|&x| x == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 {
            h[i] = h[i].wrapping_add(v[i]);
        }
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_encodes() {
        assert_eq!(base64_encode(&sha256(b"abc")), "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=");
        assert_eq!(base64_decode("aGV4ZGI=").unwrap(), b"hexdb");
        assert_eq!(base64_encode(b"hexdb"), "aGV4ZGI=");
    }
}
