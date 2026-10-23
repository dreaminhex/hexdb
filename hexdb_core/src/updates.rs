// HexDB Core Updates: is there a newer release?
//
// Once a minute after starting, then once a day, each hex asks
// `updates.url` (GitHub's latest-release API by default) for the newest
// release, unless `updates.check` is false. Nothing about the hex is sent:
// it's a plain GET. The answer is kept in memory and shown in `/status`
// (`updates`) and on the admin UI's dashboard; the first time a hex sees a
// release newer than the version it runs, it logs a warning and records an
// `update.available` audit event, so administrators hear about it.
//
// Hosted lattices (HexDB Enterprise, `updates.control_url`) can also update
// themselves, one hex at a time, from the admin UI. The Overseer runs the
// rollout: it asks the control service to move each replica's machine to the
// release it offers, and waits until that hex is back on the new version and
// caught up. Then it hands its own role over: it stops taking writes (they
// get 503 for a few seconds), waits until every replica has applied all it
// published, and stops being a candidate, so the next election picks an
// updated hex. Last, it has its own machine updated. The control service only
// ever moves a machine of the lattice's own app to the image it deploys.

use crate::{config::UpdatesConfig, network::discovery::PeerHex, HexDBEngine};
use serde::Serialize;
use std::{
    cmp::Ordering,
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// What the last check found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReleaseCheck {
    /// The newest release's version, without a leading "v".
    pub latest: Option<String>,
    /// Its release notes.
    pub url: Option<String>,
    /// When the last check finished, in epoch milliseconds.
    pub checked: Option<i64>,
    /// Why the last check failed, if it did.
    pub error: Option<String>,
    /// False when `updates.check` is off.
    pub enabled: bool,
    /// A hosted lattice, which can update its hexes from the admin UI.
    pub hosted: bool,
}

static LAST: Mutex<Option<ReleaseCheck>> = Mutex::new(None);
/// The newest version an `update.available` event was recorded for.
static ANNOUNCED: Mutex<Option<String>> = Mutex::new(None);

/// What the last check found (nothing yet if none has run).
pub fn last_check() -> ReleaseCheck {
    LAST.lock().unwrap().clone().unwrap_or_default()
}

/// Compare two versions such as "2.1.2" and "v2.2.0-rc.1": numbers part by
/// part, and a pre-release sorts before the release it leads up to.
pub fn compare(a: &str, b: &str) -> Ordering {
    let split = |v: &str| {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p.to_string())));
        let numbers: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        (numbers, pre)
    };
    let ((na, pa), (nb, pb)) = (split(a), split(b));
    let len = na.len().max(nb.len());
    for i in 0..len {
        match na.get(i).unwrap_or(&0).cmp(nb.get(i).unwrap_or(&0)) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(&b),
    }
}

/// True if `latest` is newer than `running`.
pub fn is_newer(latest: &str, running: &str) -> bool {
    compare(latest, running) == Ordering::Greater
}

/// Ask `url` for the newest release: GitHub's `{"tag_name", "html_url"}`.
async fn fetch(url: &str) -> anyhow::Result<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        #[serde(default)]
        html_url: String,
    }
    let client = crate::crypt::http_client()
        .user_agent(concat!("HexDB/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?;
    let release: Release = client.get(url).header("Accept", "application/vnd.github+json").send().await?.error_for_status()?.json().await?;
    Ok((release.tag_name.trim_start_matches('v').to_string(), release.html_url))
}

/// Check once and remember the answer; announce a release newer than this hex's version.
pub async fn check_now(engine: &HexDBEngine, config: &UpdatesConfig) -> ReleaseCheck {
    let mut result = ReleaseCheck { enabled: true, checked: Some(chrono::Utc::now().timestamp_millis()), ..Default::default() };
    match fetch(&config.url).await {
        Ok((latest, url)) => {
            result.latest = Some(latest.clone());
            result.url = Some(url.clone());
            let newer = is_newer(&latest, &engine.version);
            let first = ANNOUNCED.lock().unwrap().as_deref() != Some(latest.as_str());
            if newer && first {
                *ANNOUNCED.lock().unwrap() = Some(latest.clone());
                warn!("⬆️  HexDB {} is available; this hex runs {}. Release notes: {}", latest, engine.version, url);
                engine.audit("system", "update.available", &latest, serde_json::json!({ "running": engine.version, "latest": latest, "url": url })).await;
            }
        }
        Err(e) => {
            debug!("Checking for a new HexDB release failed: {:#}", e);
            // Keep what an earlier check found, so a network hiccup doesn't hide an update.
            let previous = last_check();
            result.latest = previous.latest;
            result.url = previous.url;
            result.error = Some(format!("{:#}", e));
        }
    }
    *LAST.lock().unwrap() = Some(result.clone());
    result
}

/// Check a minute after starting, then daily, until the hex shuts down.
pub fn spawn_update_check(engine: Arc<HexDBEngine>, config: UpdatesConfig, shutdown_rx: watch::Receiver<()>) {
    if !config.check {
        *LAST.lock().unwrap() = Some(ReleaseCheck { enabled: false, ..Default::default() });
        return;
    }
    crate::tasks::supervise("update check", shutdown_rx.clone(), move || {
        let (engine, config, mut shutdown_rx) = (engine.clone(), config.clone(), shutdown_rx.clone());
        async move {
            let mut wait = Duration::from_secs(60);
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => return,
                    _ = tokio::time::sleep(wait) => {}
                }
                check_now(&engine, &config).await;
                wait = Duration::from_secs(24 * 3600);
            }
        }
    });
}

// ---------- hosted rolling updates ----------

static STEPPING_DOWN: AtomicBool = AtomicBool::new(false);
static ROLLOUT: Mutex<Option<Rollout>> = Mutex::new(None);

/// True while this Overseer hands its role over for an update: it refuses
/// writes and stops being a candidate in elections.
pub fn stepping_down() -> bool {
    STEPPING_DOWN.load(AtomicOrdering::Relaxed)
}

/// The Fly.io Machine this hex runs on, if it does.
pub fn machine_id() -> String {
    std::env::var("FLY_MACHINE_ID").unwrap_or_default()
}

/// A rolling update, as the Overseer running it sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Rollout {
    /// running, done, or failed.
    pub state: String,
    pub target: String,
    pub started_by: String,
    pub started: i64,
    pub finished: Option<i64>,
    pub steps: Vec<RolloutStep>,
    pub error: Option<String>,
}

/// One hex of a rollout.
#[derive(Debug, Clone, Serialize)]
pub struct RolloutStep {
    pub hex: String,
    pub id: String,
    /// The version it ran when the rollout started.
    pub from: String,
    /// waiting, updating, handing over (the Overseer), done, or failed.
    pub state: String,
}

/// The rollout this hex runs or last ran.
pub fn rollout() -> Option<Rollout> {
    ROLLOUT.lock().unwrap().clone()
}

fn update_rollout(f: impl FnOnce(&mut Rollout)) {
    if let Some(r) = ROLLOUT.lock().unwrap().as_mut() {
        f(r);
    }
}

fn set_step(id: &str, state: &str) {
    update_rollout(|r| {
        if let Some(step) = r.steps.iter_mut().find(|s| s.id == id) {
            step.state = state.into();
        }
    });
}

fn finish(state: &str, error: Option<String>) {
    update_rollout(|r| {
        r.state = state.into();
        r.error = error;
        r.finished = Some(chrono::Utc::now().timestamp_millis());
    });
}

/// What the control service would install: the release it deploys.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Offer {
    pub version: String,
    #[serde(default)]
    pub image: String,
}

/// Ask the control service what it offers (no machine), or to update one machine.
async fn control(engine: &HexDBEngine, config: &UpdatesConfig, machine: Option<&str>) -> anyhow::Result<Offer> {
    let client = crate::crypt::http_client()
        .user_agent(concat!("HexDB/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?;
    let url = format!("{}/update", config.control_url.trim_end_matches('/'));
    let request = match machine {
        None => client.get(&url),
        Some(id) => client.post(&url).json(&serde_json::json!({ "machine_id": id })),
    };
    let response = request
        .bearer_auth(config.control_token.trim())
        .header("x-hexdb-lattice", engine.config.network.lattice_name.as_str())
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("the update service answered {}: {}", status, text.chars().take(300).collect::<String>());
    }
    Ok(response.json().await?)
}

static OFFER: Mutex<Option<(Instant, Offer)>> = Mutex::new(None);

/// What the control service would install now (asked at most once a minute).
pub async fn offer(engine: &HexDBEngine, config: &UpdatesConfig) -> anyhow::Result<Offer> {
    if !config.hosted() {
        anyhow::bail!("this lattice isn't hosted; install new releases as the manual's Upgrades section describes");
    }
    if let Some((at, offer)) = OFFER.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(offer.clone());
        }
    }
    let offer = control(engine, config, None).await?;
    *OFFER.lock().unwrap() = Some((Instant::now(), offer.clone()));
    Ok(offer)
}

/// Start a rolling update to what the control service offers. Runs on the Overseer, in the background.
pub async fn start_rollout(engine: Arc<HexDBEngine>, config: UpdatesConfig, by: &str) -> anyhow::Result<Rollout> {
    if !engine.is_writable() {
        anyhow::bail!("only the Overseer runs a rolling update");
    }
    if rollout().is_some_and(|r| r.state == "running") {
        anyhow::bail!("a rolling update is already running");
    }
    let target = offer(&engine, &config).await?.version;
    let me = crate::network::discovery::local_identity(&engine).await;
    let peers: Vec<_> = engine.peers.lock().await.clone();
    if let Some(lost) = peers.iter().find(|p| p.status != "active") {
        anyhow::bail!("{} isn't answering; bring it back (or remove it) before updating", lost.hex.name);
    }
    // Replicants first (they never lead), then Harvesters, then this Overseer.
    let mut hexes: Vec<_> = peers.iter().map(|p| p.hex.clone()).filter(|h| is_newer(&target, &h.version)).collect();
    hexes.sort_by_key(|h| (h.role != crate::network::discovery::ROLE_REPLICANT, h.name.clone()));
    if is_newer(&target, &me.version) {
        hexes.push(me.clone());
    }
    if hexes.is_empty() {
        anyhow::bail!("every hex already runs HexDB {} or later", target);
    }
    if let Some(h) = hexes.iter().find(|h| h.machine_id.is_empty()) {
        anyhow::bail!("{} doesn't report its machine, so it can't be updated from here", h.name);
    }
    let rollout = Rollout {
        state: "running".into(),
        target: target.clone(),
        started_by: by.to_string(),
        started: chrono::Utc::now().timestamp_millis(),
        finished: None,
        steps: hexes.iter().map(|h| RolloutStep { hex: h.name.clone(), id: h.id.clone(), from: h.version.clone(), state: "waiting".into() }).collect(),
        error: None,
    };
    *ROLLOUT.lock().unwrap() = Some(rollout.clone());
    let names: Vec<&str> = hexes.iter().map(|h| h.name.as_str()).collect();
    engine.audit(by, "update.started", &target, serde_json::json!({ "hexes": names })).await;
    info!("⬆️  Rolling update to HexDB {} started by {}: {}.", target, by, names.join(", "));
    tokio::spawn(run_rollout(engine, config, hexes, target));
    Ok(rollout)
}

async fn run_rollout(engine: Arc<HexDBEngine>, config: UpdatesConfig, hexes: Vec<PeerHex>, target: String) {
    let self_id = engine.id.to_string();
    for hex in &hexes {
        let result = if hex.id == self_id { update_self(&engine, &config, hex, &target).await } else { update_replica(&engine, &config, hex, &target).await };
        if let Err(e) = result {
            STEPPING_DOWN.store(false, AtomicOrdering::Relaxed);
            set_step(&hex.id, "failed");
            let message = format!("{}: {:#}", hex.name, e);
            warn!("❌ The rolling update to HexDB {} stopped at {}.", target, message);
            finish("failed", Some(message.clone()));
            engine.audit("system", "update.failed", &target, serde_json::json!({ "hex": hex.name, "error": message })).await;
            return;
        }
    }
    finish("done", None);
    engine.audit("system", "update.done", &target, serde_json::json!({})).await;
}

async fn update_replica(engine: &HexDBEngine, config: &UpdatesConfig, hex: &PeerHex, target: &str) -> anyhow::Result<()> {
    set_step(&hex.id, "updating");
    info!("⬆️  Updating {} to HexDB {}.", hex.name, target);
    control(engine, config, Some(&hex.machine_id)).await?;
    // Back, on the new version, and caught up (a new release can take a few minutes to start).
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let back = engine.peers.lock().await.iter().any(|p| p.hex.id == hex.id && p.status == "active" && !is_newer(target, &p.hex.version));
        let caught_up = engine.replica_applied(&hex.id).is_some_and(|applied| engine.changes.published_seq().saturating_sub(applied) <= 100);
        if back && caught_up {
            set_step(&hex.id, "done");
            engine.audit("system", "update.hex", &hex.name, serde_json::json!({ "version": target })).await;
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("it didn't come back on HexDB {} and catch up within 15 minutes", target);
        }
    }
}

async fn update_self(engine: &HexDBEngine, config: &UpdatesConfig, me: &PeerHex, target: &str) -> anyhow::Result<()> {
    if engine.peers.lock().await.iter().any(|p| p.status == "active") {
        set_step(&me.id, "handing over");
        step_down(engine).await?;
    }
    set_step(&me.id, "updating");
    info!("⬆️  Updating this hex ({}) to HexDB {}; it restarts on the new release.", me.name, target);
    engine.audit("system", "update.hex", &me.name, serde_json::json!({ "version": target, "self": true })).await;
    // Replacing the machine ends this process; every other hex already runs
    // the new release, so the rollout is complete once the request is made.
    control(engine, config, Some(&me.machine_id)).await?;
    set_step(&me.id, "done");
    Ok(())
}

/// Hand the Overseer role to another hex: stop taking writes, wait until every
/// replica has applied everything, then stop being a candidate and wait for
/// the lattice to elect someone else.
async fn step_down(engine: &HexDBEngine) -> anyhow::Result<()> {
    STEPPING_DOWN.store(true, AtomicOrdering::Relaxed);
    // Writes already past the check finish committing.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let head = engine.changes.published_seq();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let replicas: Vec<String> = engine.peers.lock().await.iter().filter(|p| p.status == "active").map(|p| p.hex.id.clone()).collect();
        if replicas.iter().all(|id| engine.replica_applied(id).is_some_and(|applied| applied >= head)) {
            break;
        }
        if Instant::now() > deadline {
            anyhow::bail!("the replicas didn't catch up within a minute, so this hex kept its role");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // The next discovery rounds see this hex as a replicant and elect another.
    let deadline = Instant::now() + Duration::from_secs(5 * 60);
    loop {
        let elected = engine.peers.lock().await.iter().any(|p| p.status == "active" && p.hex.role == crate::network::discovery::ROLE_OVERSEER);
        if elected && !engine.is_writable() {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("no other hex took the Overseer role within 5 minutes");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_and_pre_release() {
        assert!(is_newer("2.2.0", "2.1.2"));
        assert!(is_newer("v2.10.0", "2.9.9"));
        assert!(!is_newer("2.1.2", "2.1.2"));
        assert!(!is_newer("2.1.2", "2.2.0"));
        assert!(is_newer("2.2.0", "2.2.0-rc.1"));
        assert!(!is_newer("2.2.0-rc.1", "2.2.0"));
        assert!(is_newer("3", "2.9"));
    }
}
