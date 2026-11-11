// HexDB Core: the process sandbox (plugins, and the parser worker on Windows)
//
// Process plugins are programs HexDB starts (`node index.mjs`, a binary), so
// their isolation has to come from the operating system. The server's own
// binary does the setting up: started as
//
//     hexdb --sandbox-exec <policy JSON> -- <program> <args...>
//
// it restricts itself according to the policy and then becomes (Linux, macOS)
// or starts (Windows) the program. Callers spawn it like any other command,
// so pipes, kill-on-drop and the server's job work as before.
//
// A `Policy` names what the program may use: folders it may read (and run
// programs from), folders it may write, whether it may use the network, and
// memory/CPU/process limits. Everything else is refused:
//
//   * Linux: Landlock limits the file system to the system folders a runtime
//     needs (/usr, /lib, /etc but not HexDB's own config, /proc, /dev...), the
//     program's folder and the policy's folders; the policy's `deny` paths
//     (the data directory, config files, TLS key) stay out even when inside
//     an allowed folder. A seccomp filter refuses sockets without `network`
//     (with it: IPv4, IPv6 and netlink only, never Unix sockets), signals to
//     other processes, ptrace, io_uring, mounts, namespaces, BPF, keyrings and
//     module loading. Plus rlimits and no-new-privileges. Kernels without
//     Landlock (before 5.13) get the rest, with a warning in the plugin's log.
//   * Windows: an AppContainer per policy name. The program can read only
//     what every AppContainer can (Windows, Program Files) plus the folders
//     HexDB grants it in their ACLs; the network only with `network` (the
//     internetClient and privateNetworkClientServer capabilities). Windows
//     blocks AppContainers from localhost unless exempted, which needs an
//     administrator; HexDB tries, and logs the command if it can't. A nested
//     job object holds the limits and kills the program with the launcher.
//   * macOS: rlimits only.
//
// The Linux parser worker doesn't use this: it is HexDB's own code, so it
// restricts itself more tightly (see `extract::sandbox`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// The argument that makes the server binary the sandbox launcher.
pub const EXEC_ARG: &str = "--sandbox-exec";

/// What a sandboxed program may use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Names the sandbox (the AppContainer profile on Windows): letters, digits, '.', '-'.
    pub name: String,
    /// Folders (or files) it may read and run programs from.
    #[serde(default)]
    pub read: Vec<PathBuf>,
    /// Folders it may read and write (they must exist).
    #[serde(default)]
    pub write: Vec<PathBuf>,
    /// Paths kept out even inside allowed folders (Linux; elsewhere nothing is
    /// readable unless granted).
    #[serde(default)]
    pub deny: Vec<PathBuf>,
    #[serde(default)]
    pub network: bool,
    /// Memory limit in MB (0: none).
    #[serde(default)]
    pub memory_mb: u64,
    /// CPU time limit in seconds (0: none).
    #[serde(default)]
    pub cpu_seconds: u64,
    /// Processes it may run at once, itself included (0: no limit; Windows only).
    #[serde(default)]
    pub max_processes: u32,
}

static LAUNCHER: OnceLock<PathBuf> = OnceLock::new();

/// The server binary, which can act as the launcher. Set at startup; without
/// it (library use) programs aren't sandboxed.
pub fn set_launcher(exe: PathBuf) {
    let _ = LAUNCHER.set(exe);
}

pub fn launcher() -> Option<&'static Path> {
    LAUNCHER.get().map(PathBuf::as_path)
}

/// How much isolation this platform gives, for status pages.
pub fn isolation() -> &'static str {
    if cfg!(target_os = "linux") {
        "landlock+seccomp"
    } else if cfg!(windows) {
        "appcontainer"
    } else {
        "rlimits"
    }
}

/// A name usable as a sandbox (AppContainer) name: "HexDB.<prefix>.<cleaned>".
pub fn sandbox_name(prefix: &str, id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '.' })
        .collect::<String>()
        .split('.')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(".");
    let mut name = format!("HexDB.{}.{}", prefix, cleaned);
    // AppContainer names are at most 64 characters.
    if name.len() > 64 {
        // FNV-1a, so different long ids keep different names.
        let hash = id.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
        name.truncate(64 - 9);
        name = format!("{}.{:08x}", name.trim_end_matches('.'), hash);
    }
    name
}

/// The launcher's command line for running `program args` under `policy`.
pub fn wrap(launcher: &Path, policy: &Policy, program: &str, args: &[String]) -> (PathBuf, Vec<String>) {
    let mut out = vec![EXEC_ARG.to_string(), serde_json::to_string(policy).unwrap_or_default(), "--".to_string(), program.to_string()];
    out.extend(args.iter().cloned());
    (launcher.to_path_buf(), out)
}

/// True if `inner` is `outer` or inside it (lexically, after making both absolute).
pub fn is_within(inner: &Path, outer: &Path) -> bool {
    let (Ok(inner), Ok(outer)) = (std::path::absolute(inner), std::path::absolute(outer)) else { return false };
    let inner = std::fs::canonicalize(&inner).unwrap_or(inner);
    let outer = std::fs::canonicalize(&outer).unwrap_or(outer);
    if cfg!(windows) {
        let (i, o) = (inner.to_string_lossy().to_lowercase(), outer.to_string_lossy().to_lowercase());
        Path::new(&i).starts_with(Path::new(&o))
    } else {
        inner.starts_with(&outer)
    }
}

/// Find `program` the way a shell would: a path as given (relative to the
/// working directory), or the first match on PATH (with PATHEXT on Windows).
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let given = Path::new(program);
    let with_exts = |path: &Path| -> Option<PathBuf> {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
        if cfg!(windows) && path.extension().is_none() {
            let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            for ext in exts.split(';').filter(|e| !e.is_empty()) {
                let candidate = path.with_extension(ext.trim_start_matches('.'));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        None
    };
    if given.components().count() > 1 || given.is_absolute() {
        return with_exts(given).and_then(|p| std::path::absolute(p).ok());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| with_exts(&dir.join(given)))
}

/// The launcher: `hexdb --sandbox-exec <policy> -- program args`. Never returns.
pub fn launcher_main() -> ! {
    let args: Vec<String> = std::env::args().collect();
    let code = match launch(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sandbox: {:#}", e);
            126
        }
    };
    std::process::exit(code)
}

fn launch(args: &[String]) -> anyhow::Result<i32> {
    use anyhow::{bail, Context};
    // [exe, EXEC_ARG, policy, "--", program, args...]
    if args.len() < 5 || args[1] != EXEC_ARG || args[3] != "--" {
        bail!("usage: {} <policy> -- <program> [args...]", EXEC_ARG);
    }
    let policy: Policy = serde_json::from_str(&args[2]).context("the sandbox policy is invalid")?;
    let program = resolve_program(&args[4]).with_context(|| format!("can't find the program '{}'", args[4]))?;
    #[cfg(target_os = "linux")]
    return linux::run(&policy, &program, &args[5..]);
    #[cfg(windows)]
    return windows::run(&policy, &program, &args[5..]);
    #[cfg(all(unix, not(target_os = "linux")))]
    return unix_rlimits_then_exec(&policy, &program, &args[5..]);
}

#[cfg(unix)]
fn set_rlimits(policy: &Policy) {
    let set = |resource, value: u64| {
        let limit = libc::rlimit { rlim_cur: value as libc::rlim_t, rlim_max: value as libc::rlim_t };
        // SAFETY: plain setrlimit on this process.
        unsafe { libc::setrlimit(resource, &limit) };
    };
    set(libc::RLIMIT_CORE, 0);
    if policy.memory_mb > 0 {
        set(libc::RLIMIT_DATA, policy.memory_mb * 1024 * 1024);
    }
    if policy.cpu_seconds > 0 {
        set(libc::RLIMIT_CPU, policy.cpu_seconds);
    }
}

#[cfg(unix)]
fn exec(program: &Path, args: &[String]) -> anyhow::Result<i32> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(program).args(args).exec();
    Err(anyhow::Error::new(err).context(format!("couldn't run {}", program.display())))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn unix_rlimits_then_exec(policy: &Policy, program: &Path, args: &[String]) -> anyhow::Result<i32> {
    set_rlimits(policy);
    exec(program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_names_are_safe_and_short() {
        assert_eq!(sandbox_name("Plugin", "@streams/kafka"), "HexDB.Plugin.streams.kafka");
        let long = sandbox_name("Plugin", &"x".repeat(100));
        assert!(long.len() <= 64, "{}", long);
        assert!(long.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
        assert_ne!(long, sandbox_name("Plugin", &"x".repeat(101)));
    }

    #[test]
    fn within_compares_whole_components() {
        let base = std::env::temp_dir().join("hexdb-within");
        assert!(is_within(&base.join("a/b"), &base));
        assert!(is_within(&base, &base));
        assert!(!is_within(&base, &base.join("a")));
        assert!(!is_within(Path::new(&format!("{}-other", base.display())), &base));
    }
}
