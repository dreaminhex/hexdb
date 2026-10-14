// Remote storage for file content: keeps the chunk store's remote tier (see
// `crate::blobs::remote_tier`) in line with the configuration in effect on
// this hex (`file_storage.*`, which a hex may override), and runs its steps.
//
// Every hex keeps its own copy of the content, so each uploads its own packs
// under its own ID. A backend that still holds packs after the configuration
// moved away from it is reached with the settings of the configuration
// version it was set up from, so its packs can be moved.

use crate::{
    blobs::Tier,
    engine::HexDBEngine,
    lattice_config::AppliedConfig,
    remote::{self, BackendSpec},
};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tracing::{info, warn};

/// How often the remote tier runs (it also runs soon after the backend changes).
const STEP_INTERVAL: Duration = Duration::from_secs(30);

/// Build a backend's store on the blocking pool (its HTTP client blocks).
async fn open_tier(config: AppliedConfig, spec: BackendSpec, hex: String) -> Result<Tier> {
    tokio::task::spawn_blocking(move || {
        let store = remote::open(&config)?.ok_or_else(|| anyhow!("no remote backend is configured"))?;
        Ok(Tier::new(store, spec, &hex, config.version))
    })
    .await
    .map_err(|e| anyhow!("{}", e))?
}

impl HexDBEngine {
    fn local_cache_bytes(&self) -> u64 {
        self.config_value("file_storage.local_cache_gb").as_u64().unwrap_or(20) * 1024 * 1024 * 1024
    }

    /// Point the chunk store at the configured backend, and at older ones that still hold packs.
    pub(crate) async fn refresh_tiers(&self) -> Result<()> {
        let applied = self.lattice_config();
        let wanted = remote::spec(&applied);
        let current = match &wanted {
            Some(spec) if self.blobs.current_tier_id().as_deref() == Some(spec.id.as_str()) => self.blobs.tiers_now().0,
            Some(spec) => {
                let tier = open_tier(applied.clone(), spec.clone(), self.id.to_string()).await?;
                info!("📦 File content goes to {} from now on ({}).", spec.kind, spec.id);
                Some(Arc::new(tier))
            }
            None => None,
        };
        // A previous backend still holding packs: reach it with its own settings.
        let mut previous = None;
        for (backend, version) in self.blobs.other_backends(current.as_ref().map(|t| t.id())) {
            if let Some(tier) = self.blobs.tiers_now().1.filter(|t| t.id() == backend) {
                previous = Some(tier);
                break;
            }
            let Some(config) = self.resolved_config_at(version).await else {
                warn!("⚠️ Packs are on a storage backend ({}) whose settings aren't in the configuration history; they can't be read.", backend);
                continue;
            };
            match remote::spec(&config).filter(|s| s.id == backend) {
                Some(spec) => match open_tier(config, spec, self.id.to_string()).await {
                    Ok(tier) => {
                        previous = Some(Arc::new(tier));
                        break;
                    }
                    Err(e) => warn!("⚠️ Couldn't reach the previous storage backend {}: {:#}", backend, e),
                },
                None => warn!("⚠️ Configuration version {} no longer describes storage backend {}.", version, backend),
            }
        }
        let replaced = self.blobs.set_tiers(current, previous);
        tokio::task::spawn_blocking(move || drop(replaced));
        Ok(())
    }

    /// Keep the remote tier going: every 30 seconds, and soon after the configuration changes.
    pub async fn run_remote_storage(self: Arc<Self>, mut shutdown: watch::Receiver<()>) {
        let mut seen_version = u64::MAX;
        let mut next = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep_until(next) => {}
            }
            let version = self.lattice_config().version;
            let changed = version != seen_version;
            seen_version = version;
            if changed || remote::spec(&self.lattice_config()).is_some() || self.blobs.remote_status().moving_packs > 0 {
                if let Err(e) = self.refresh_tiers().await {
                    warn!("⚠️ Remote file storage isn't usable: {:#}", e);
                    self.blobs.note_remote_problem(&e);
                }
                let store = self.blobs.clone();
                let budget = self.local_cache_bytes();
                let seal = Duration::from_secs(self.config.files.remote_seal_seconds);
                match tokio::task::spawn_blocking(move || store.remote_step(budget, seal)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => warn!("⚠️ Remote file storage step failed: {:#}", e),
                    Err(e) => warn!("⚠️ Remote file storage step stopped: {}", e),
                }
            }
            next = tokio::time::Instant::now() + STEP_INTERVAL;
        }
    }

    /// Check the backend this hex's configuration chooses: write, read back and
    /// delete a small object. Uses the saved configuration (save first, then test).
    pub async fn test_file_storage(&self) -> Value {
        let applied = self.lattice_config();
        let Some(spec) = remote::spec(&applied) else {
            return json!({ "ok": true, "backend": "local", "message": "File content is kept on this hex's disks; there's nothing remote to test." });
        };
        let kind = spec.kind.clone();
        let outcome = match open_tier(applied, spec, self.id.to_string()).await {
            Ok(tier) => tokio::task::spawn_blocking(move || tier.check()).await.map_err(|e| anyhow!("{}", e)).and_then(|r| r),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => json!({ "ok": true, "backend": kind, "hex": self.name }),
            Err(e) => json!({ "ok": false, "backend": kind, "hex": self.name, "error": format!("{:#}", e) }),
        }
    }
}
