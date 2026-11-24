//! Named environments (a lattice's address, a color, an optional SSH tunnel),
//! saved as JSON in the app's config folder. Secrets (an API key per
//! environment) live in the OS keychain, never in that file.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEYCHAIN_SERVICE: &str = "HexDB Workbench";

/// The colors an environment can have; the window's banner and border use it,
/// so production never looks like development.
pub const COLORS: &[&str] = &["green", "blue", "purple", "amber", "red", "gray"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Environment {
    pub id: String,
    pub name: String,
    /// One of [`COLORS`].
    pub color: String,
    /// The lattice's address as seen from this machine, or, through a tunnel,
    /// the address on the far side (usually http://127.0.0.1:7700).
    pub url: String,
    #[serde(default)]
    pub ssh: Option<Ssh>,
    /// Whether an API key is in the keychain (for compare and health).
    #[serde(default)]
    pub has_key: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ssh {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    /// A private key file; empty uses the SSH agent and the default keys.
    #[serde(default)]
    pub identity_file: String,
}

fn default_ssh_port() -> u16 {
    22
}

impl Environment {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.name.len() > 64 {
            bail!("Give the environment a name of 1 to 64 characters.");
        }
        if !COLORS.contains(&self.color.as_str()) {
            bail!("Pick one of the colors: {}.", COLORS.join(", "));
        }
        let url = reqwest::Url::parse(&self.url).context("The address must be a URL, such as https://acme.hexdb.app or http://127.0.0.1:7700.")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("The address must start with http:// or https://.");
        }
        if let Some(ssh) = &self.ssh {
            if ssh.host.trim().is_empty() || ssh.user.trim().is_empty() {
                bail!("An SSH tunnel needs a host and a user.");
            }
            // Arguments go to ssh as they are: refuse anything that could be read as an option.
            for value in [&ssh.host, &ssh.user, &ssh.identity_file] {
                if value.starts_with('-') || value.chars().any(|c| c.is_control()) {
                    bail!("SSH settings can't start with '-' or hold control characters.");
                }
            }
            if url.scheme() != "http" {
                bail!("Through an SSH tunnel, the address is the far side's, such as http://127.0.0.1:7700 (the tunnel encrypts it).");
            }
        }
        Ok(())
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Store {
        Store { path: dir.join("environments.json") }
    }

    pub fn list(&self) -> Result<Vec<Environment>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("environments.json is damaged")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get(&self, id: &str) -> Result<Environment> {
        self.list()?.into_iter().find(|e| e.id == id).ok_or_else(|| anyhow!("There's no environment {}.", id))
    }

    fn write(&self, list: &[Environment]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(list)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Adds or replaces an environment; `key` replaces its API key when given
    /// (an empty string removes it).
    pub fn save(&self, mut env: Environment, key: Option<&str>) -> Result<Environment> {
        if env.id.is_empty() {
            env.id = ulid::Ulid::new().to_string().to_ascii_lowercase();
        }
        env.validate()?;
        let mut list = self.list()?;
        let previous = list.iter().find(|e| e.id == env.id).map(|e| e.has_key).unwrap_or(false);
        env.has_key = match key {
            Some("") => {
                delete_key(&env.id)?;
                false
            }
            Some(k) => {
                set_key(&env.id, k)?;
                true
            }
            None => previous,
        };
        list.retain(|e| e.id != env.id);
        list.push(env.clone());
        list.sort_by_key(|e| e.name.to_lowercase());
        self.write(&list)?;
        Ok(env)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let mut list = self.list()?;
        list.retain(|e| e.id != id);
        self.write(&list)?;
        delete_key(id)
    }
}

fn entry(id: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(KEYCHAIN_SERVICE, id)?)
}

pub fn set_key(id: &str, key: &str) -> Result<()> {
    entry(id)?.set_password(key.trim()).context("Couldn't save the API key in the keychain")
}

pub fn get_key(id: &str) -> Result<Option<String>> {
    match entry(id)?.get_password() {
        Ok(k) => Ok(Some(k)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow!("Couldn't read the API key from the keychain: {}", e)),
    }
}

fn delete_key(id: &str) -> Result<()> {
    match entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!("Couldn't remove the API key from the keychain: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Environment {
        Environment { id: String::new(), name: "Production".into(), color: "red".into(), url: "https://acme.hexdb.app".into(), ssh: None, has_key: false }
    }

    #[test]
    fn validates_environments() {
        assert!(env().validate().is_ok());
        assert!(Environment { color: "pink".into(), ..env() }.validate().is_err());
        assert!(Environment { url: "ftp://x".into(), ..env() }.validate().is_err());
        let ssh = Ssh { host: "db.example.com".into(), port: 22, user: "deploy".into(), identity_file: String::new() };
        assert!(Environment { ssh: Some(ssh.clone()), ..env() }.validate().is_err(), "a tunnel's far side is http");
        assert!(Environment { ssh: Some(ssh.clone()), url: "http://127.0.0.1:7700".into(), ..env() }.validate().is_ok());
        let sneaky = Ssh { host: "-oProxyCommand=calc".into(), ..ssh };
        assert!(Environment { ssh: Some(sneaky), url: "http://127.0.0.1:7700".into(), ..env() }.validate().is_err());
    }

    #[test]
    fn saves_and_lists_without_secrets_in_the_file() {
        let dir = std::env::temp_dir().join(format!("hexdb-workbench-test-{}", ulid::Ulid::new()));
        let store = Store::new(&dir);
        let saved = store.save(env(), None).unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(store.list().unwrap(), vec![saved.clone()]);
        store.save(Environment { name: "Prod".into(), ..saved.clone() }, None).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        store.delete(&saved.id).unwrap();
        assert!(store.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
