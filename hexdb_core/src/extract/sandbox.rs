// HexDB Core: the parser sandbox
//
// Files come from users, and parsers are where hostile files do damage. So
// metadata extraction, thumbnails and video frame decoding run in a separate
// worker process: the server's own binary started with `--extract-worker`
// (see `set_worker`). A worker handles one job and exits.
//
// The worker never sees the store, the keys or the network. It asks the hex
// for byte ranges of the file over its stdin/stdout, and the hex answers from
// the encrypted chunk store through its own `Source`, so the byte budget and
// the deadline are enforced on the hex's side too: a compromised worker can't
// read more of the file than an honest one, or anything else.
//
// Restrictions, set before the worker reads its job:
//   * every platform: an empty environment, no inherited handles but the
//     three pipes, killed at the deadline by the hex (and when the hex dies);
//   * Linux: rlimits (data segment = `files.sandbox_memory_mb`, CPU seconds,
//     no file writes, few file descriptors, no core dumps), no new privileges,
//     and a seccomp filter that allows only memory, thread, clock, signal and
//     pipe I/O system calls; opening files, sockets and exec fail with EPERM;
//   * macOS: the same rlimits (macOS doesn't enforce the memory one);
//   * Windows: its own job object with a memory limit, a CPU-time limit, one
//     process (it can't start others) and UI restrictions, killed when the
//     hex's handle closes.
//
// Without a worker binary (library use, unit tests) or with
// `files.sandbox = false`, extraction runs in-process as before.
//
// Protocol: frames of `tag (u8) | length (u32 LE) | payload`.
//   hex -> worker   b'J' job (JSON)      b'B' bytes      b'E' error (UTF-8)
//   worker -> hex   b'R' read (offset u64 LE, length u32 LE)   b'D' result (JSON)

use super::{thumbnail::Thumbnail, ByteSource, Extracted, Limits, Source};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The argument that makes the server binary a worker.
pub const WORKER_ARG: &str = "--extract-worker";
/// Largest read the hex answers in one frame.
const MAX_READ: u32 = 4 * 1024 * 1024;
/// Largest result a worker may send (thumbnails and frames are small).
const MAX_RESULT: u32 = 64 * 1024 * 1024;
/// The worker reads ahead in blocks of this size, so small reads don't each cost a round trip.
const BLOCK: u64 = 256 * 1024;

#[derive(Debug, Clone)]
struct Worker {
    exe: PathBuf,
    memory_mb: u64,
}

static WORKER: OnceLock<Worker> = OnceLock::new();
static RUNS: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static STOPPED: AtomicU64 = AtomicU64::new(0);

/// What the sandbox has done since startup, for `/status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SandboxStats {
    /// Parsing runs in worker processes.
    pub enabled: bool,
    pub memory_mb: u64,
    /// Worker runs.
    pub runs: u64,
    /// Runs whose worker crashed, broke the protocol or was refused something.
    pub failed: u64,
    /// Workers stopped at the deadline.
    pub stopped: u64,
    /// Workers run under a seccomp system-call filter (Linux on x86_64, aarch64 and riscv64).
    pub seccomp: bool,
}

pub fn stats() -> SandboxStats {
    SandboxStats {
        enabled: enabled(),
        memory_mb: WORKER.get().map_or(0, |w| w.memory_mb),
        runs: RUNS.load(Ordering::Relaxed),
        failed: FAILED.load(Ordering::Relaxed),
        stopped: STOPPED.load(Ordering::Relaxed),
        seccomp: cfg!(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))),
    }
}

/// Use `exe --extract-worker` for parsing, with this memory limit. The server
/// calls this at startup; without it, parsing runs in-process.
pub fn set_worker(exe: PathBuf, memory_mb: u64) {
    let _ = WORKER.set(Worker { exe, memory_mb: memory_mb.max(64) });
}

/// True if parsing runs in a worker process.
pub fn enabled() -> bool {
    WORKER.get().is_some()
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Job {
    Extract { name: String, content_type: String, len: u64, max_read_bytes: u64, timeout_ms: u64, max_text_bytes: usize, thumbnail_px: Option<u32> },
    VideoFrames { len: u64, max_read_bytes: u64, timeout_ms: u64, count: usize, px: u32 },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ThumbOut {
    data: String,
    width: u32,
    height: u32,
    content_type: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Output {
    #[serde(default)]
    meta: Map<String, Value>,
    #[serde(default)]
    extractors: Vec<String>,
    #[serde(default)]
    errors: Vec<String>,
    /// Extract: Some(Ok(None)) no thumbnail, Some(Err) a thumbnail error.
    #[serde(default)]
    thumbnail: Option<std::result::Result<Option<ThumbOut>, String>>,
    /// VideoFrames.
    #[serde(default)]
    frames: Option<std::result::Result<Vec<ThumbOut>, String>>,
}

fn thumb_out(t: Thumbnail) -> ThumbOut {
    ThumbOut { data: STANDARD.encode(&t.bytes), width: t.width, height: t.height, content_type: t.content_type.to_string() }
}

fn thumb_in(t: ThumbOut) -> Result<Thumbnail> {
    let content_type = match t.content_type.as_str() {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        other => bail!("the worker returned a thumbnail of type {}", other),
    };
    Ok(Thumbnail { bytes: STANDARD.decode(t.data.as_bytes()).context("the worker's thumbnail isn't base64")?, width: t.width, height: t.height, content_type })
}

fn limits_of(max_read_bytes: u64, timeout_ms: u64, max_text_bytes: usize) -> Limits {
    Limits { max_read_bytes, timeout: Duration::from_millis(timeout_ms), max_text_bytes }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

fn write_frame(out: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    out.write_all(&[tag])?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)?;
    out.flush()
}

fn read_frame(input: &mut impl Read, max: u32) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    input.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap());
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a {} byte frame is over the {} byte limit", len, max)));
    }
    let mut payload = vec![0u8; len as usize];
    input.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

// ---------------------------------------------------------------------------
// The hex's side
// ---------------------------------------------------------------------------

/// Extract a file's metadata (and its thumbnail, with `thumbnail_px`), in a
/// worker when the sandbox is on. Blocking: run it on the blocking pool.
pub fn extract(inner: Box<dyn ByteSource>, name: &str, content_type: &str, limits: &Limits, thumbnail_px: Option<u32>) -> (Extracted, Option<Result<Option<Thumbnail>>>) {
    let Some(worker) = WORKER.get() else {
        return in_process(inner, name, content_type, limits, thumbnail_px);
    };
    let job = Job::Extract {
        name: name.to_string(),
        content_type: content_type.to_string(),
        len: inner.len(),
        max_read_bytes: limits.max_read_bytes,
        timeout_ms: limits.timeout.as_millis() as u64,
        max_text_bytes: limits.max_text_bytes,
        thumbnail_px,
    };
    // The thumbnail reads the file again, with its own budget and time, as in-process.
    let budget = Limits { max_read_bytes: limits.max_read_bytes.saturating_mul(2), timeout: limits.timeout * 2, ..*limits };
    match run(worker, inner, &job, &budget) {
        Ok(out) => {
            let thumbnail = out.thumbnail.map(|t| match t {
                Ok(Some(t)) => thumb_in(t).map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(anyhow!(e)),
            });
            (Extracted { meta: out.meta, extractors: out.extractors, errors: out.errors }, thumbnail)
        }
        Err(e) => (Extracted { errors: vec![format!("extraction failed: {}", super::truncate(&format!("{:#}", e), 300))], ..Default::default() }, None),
    }
}

/// Decode `count` keyframes spread through a video, in a worker when the sandbox is on.
pub fn video_frames(inner: Box<dyn ByteSource>, count: usize, px: u32, limits: &Limits) -> Result<Vec<Thumbnail>> {
    let Some(worker) = WORKER.get() else {
        let mut source = Source::new(inner, limits);
        return std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::thumbnail::video_frames(&mut source, count, px)))
            .unwrap_or_else(|_| Err(anyhow!("the video decoder failed on this file")));
    };
    let job = Job::VideoFrames { len: inner.len(), max_read_bytes: limits.max_read_bytes, timeout_ms: limits.timeout.as_millis() as u64, count, px };
    let out = run(worker, inner, &job, limits)?;
    match out.frames {
        Some(Ok(frames)) => frames.into_iter().map(thumb_in).collect(),
        Some(Err(e)) => Err(anyhow!(e)),
        None => bail!("the worker returned no frames"),
    }
}

fn in_process(inner: Box<dyn ByteSource>, name: &str, content_type: &str, limits: &Limits, thumbnail_px: Option<u32>) -> (Extracted, Option<Result<Option<Thumbnail>>>) {
    // One source for both, so the thumbnail can reuse what extraction read.
    let mut source = Source::new(inner, limits);
    let extracted = super::extract(&mut source, name, content_type, limits);
    let thumbnail = thumbnail_px.map(|px| {
        source.restart(limits);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::thumbnail::make(&mut source, content_type, px)))
            .unwrap_or_else(|_| Err(anyhow!("the image decoder failed on this file")))
    });
    (extracted, thumbnail)
}

/// Run one job in a new worker, answering its reads from `inner`.
fn run(worker: &Worker, inner: Box<dyn ByteSource>, job: &Job, limits: &Limits) -> Result<Output> {
    let mut command = Command::new(&worker.exe);
    command.arg(WORKER_ARG).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Some platforms need these to start a process at all.
    for name in ["SYSTEMROOT", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    RUNS.fetch_add(1, Ordering::Relaxed);
    let cpu_seconds = limits.timeout.as_secs() * 2 + 5;
    restrict::before_spawn(&mut command, worker.memory_mb, cpu_seconds);
    let mut child = command.spawn().with_context(|| format!("starting the parser sandbox ({})", worker.exe.display()))?;
    // Held until the run ends (on Windows, closing it kills the worker).
    let _job = match restrict::after_spawn(&child, worker.memory_mb, cpu_seconds) {
        Ok(guard) => guard,
        Err(e) => {
            // Never let a worker run without its restrictions.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let mut to_worker = child.stdin.take().ok_or_else(|| anyhow!("no pipe to the worker"))?;
    let mut from_worker = child.stdout.take().ok_or_else(|| anyhow!("no pipe from the worker"))?;
    let mut errors = child.stderr.take().ok_or_else(|| anyhow!("no error pipe from the worker"))?;
    let stderr = std::thread::spawn(move || {
        let mut tail = Vec::new();
        let _ = (&mut errors).take(64 * 1024).read_to_end(&mut tail);
        String::from_utf8_lossy(&tail[tail.len().saturating_sub(600)..]).trim().to_string()
    });

    // The watchdog kills the worker at the deadline; the pipes then close and the loop below ends.
    let deadline = limits.timeout * 2 + Duration::from_secs(5);
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let started = Instant::now();
        let timed_out = loop {
            if let Ok(Some(_)) = child.try_wait() {
                break false;
            }
            match done_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // Give it a moment to exit by itself, then make sure.
                    for _ in 0..50 {
                        if let Ok(Some(_)) = child.try_wait() {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if started.elapsed() > deadline => break true,
                Err(_) => {}
            }
        };
        if timed_out {
            let _ = child.kill();
        }
        let _ = child.wait();
        timed_out
    });

    let outcome = (|| -> Result<Output> {
        // The hex's own source enforces the budget and deadline on every read the worker asks for.
        let mut source = Source::new(inner, limits);
        write_frame(&mut to_worker, b'J', &serde_json::to_vec(job)?)?;
        loop {
            let (tag, payload) = read_frame(&mut from_worker, MAX_RESULT)?;
            match tag {
                b'R' if payload.len() == 12 => {
                    let offset = u64::from_le_bytes(payload[..8].try_into().unwrap());
                    let len = u32::from_le_bytes(payload[8..].try_into().unwrap()).min(MAX_READ);
                    match source.range(offset, len as usize) {
                        Ok(bytes) => write_frame(&mut to_worker, b'B', &bytes)?,
                        Err(e) => write_frame(&mut to_worker, b'E', e.to_string().as_bytes())?,
                    }
                }
                b'D' => return serde_json::from_slice(&payload).context("the worker's result isn't valid"),
                other => bail!("the worker sent an unexpected frame ({})", other),
            }
        }
    })();
    drop(to_worker);
    drop(from_worker);
    let _ = done_tx.send(());
    let timed_out = watchdog.join().unwrap_or(false);
    let stderr = stderr.join().unwrap_or_default();
    if timed_out {
        STOPPED.fetch_add(1, Ordering::Relaxed);
    } else if outcome.is_err() {
        FAILED.fetch_add(1, Ordering::Relaxed);
    }
    match outcome {
        Ok(out) => Ok(out),
        Err(_) if timed_out => bail!("the parser took too long and was stopped"),
        Err(e) if stderr.is_empty() => Err(e.context("the parser stopped")),
        Err(e) => Err(e.context(format!("the parser stopped: {}", stderr))),
    }
}

// ---------------------------------------------------------------------------
// The worker's side
// ---------------------------------------------------------------------------

/// The file, read from the hex in blocks.
struct PipeSource {
    len: u64,
    input: io::Stdin,
    output: io::Stdout,
    block: Option<(u64, Vec<u8>)>,
}

impl ByteSource for PipeSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let cached = self.block.as_ref().is_some_and(|(start, data)| offset >= *start && offset < start + data.len() as u64);
        if !cached {
            // Large reads go straight through; small ones fetch a block.
            let want = (buf.len() as u64).max(BLOCK).min(MAX_READ as u64).min(self.len - offset);
            let mut request = offset.to_le_bytes().to_vec();
            request.extend_from_slice(&(want as u32).to_le_bytes());
            write_frame(&mut self.output.lock(), b'R', &request)?;
            let (tag, payload) = read_frame(&mut self.input.lock(), MAX_READ)?;
            match tag {
                b'B' => self.block = Some((offset, payload)),
                b'E' => return Err(io::Error::other(String::from_utf8_lossy(&payload).into_owned())),
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected frame from the hex")),
            }
        }
        let (start, data) = self.block.as_ref().expect("a block");
        let from = (offset - start) as usize;
        let n = buf.len().min(data.len() - from);
        buf[..n].copy_from_slice(&data[from..from + n]);
        Ok(n)
    }
}

/// The worker process: restrict itself, read one job, answer, exit.
pub fn worker_main() -> ! {
    // Panics are reported as results, not printed.
    std::panic::set_hook(Box::new(|_| {}));
    if let Err(e) = restrict::in_worker() {
        let _ = writeln!(io::stderr(), "the sandbox couldn't be set up: {:#}", e);
        std::process::exit(2);
    }
    let code = match std::panic::catch_unwind(worker_job) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            let _ = writeln!(io::stderr(), "{:#}", e);
            1
        }
        Err(_) => {
            let _ = writeln!(io::stderr(), "the parser failed on this file");
            1
        }
    };
    std::process::exit(code)
}

fn worker_job() -> Result<()> {
    let (tag, payload) = read_frame(&mut io::stdin().lock(), 1024 * 1024)?;
    if tag != b'J' {
        bail!("expected a job");
    }
    let job: Job = serde_json::from_slice(&payload)?;
    let pipe = |len| Box::new(PipeSource { len, input: io::stdin(), output: io::stdout(), block: None });
    let out = match job {
        Job::Extract { name, content_type, len, max_read_bytes, timeout_ms, max_text_bytes, thumbnail_px } => {
            let limits = limits_of(max_read_bytes, timeout_ms, max_text_bytes);
            let (extracted, thumbnail) = in_process(pipe(len), &name, &content_type, &limits, thumbnail_px);
            Output {
                meta: extracted.meta,
                extractors: extracted.extractors,
                errors: extracted.errors,
                thumbnail: thumbnail.map(|t| t.map(|t| t.map(thumb_out)).map_err(|e| format!("{:#}", e))),
                frames: None,
            }
        }
        Job::VideoFrames { len, max_read_bytes, timeout_ms, count, px } => {
            let limits = limits_of(max_read_bytes, timeout_ms, 0);
            let mut source = Source::new(pipe(len), &limits);
            let frames = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| super::thumbnail::video_frames(&mut source, count, px)))
                .unwrap_or_else(|_| Err(anyhow!("the video decoder failed on this file")));
            Output { frames: Some(frames.map(|f| f.into_iter().map(thumb_out).collect()).map_err(|e| format!("{:#}", e))), ..Default::default() }
        }
    };
    write_frame(&mut io::stdout().lock(), b'D', &serde_json::to_vec(&out)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform restrictions
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod restrict {
    use std::os::unix::process::CommandExt;

    /// Resource limits, set between fork and exec so they hold from the worker's first instruction.
    pub fn before_spawn(command: &mut std::process::Command, memory_mb: u64, cpu_seconds: u64) {
        // SAFETY: setrlimit is async-signal-safe and only changes this (child) process.
        unsafe {
            command.pre_exec(move || {
                let set = |resource, value: u64| {
                    let limit = libc::rlimit { rlim_cur: value as libc::rlim_t, rlim_max: value as libc::rlim_t };
                    libc::setrlimit(resource, &limit);
                };
                set(libc::RLIMIT_CPU, cpu_seconds);
                set(libc::RLIMIT_FSIZE, 0);
                set(libc::RLIMIT_CORE, 0);
                set(libc::RLIMIT_NOFILE, 16);
                set(libc::RLIMIT_DATA, memory_mb * 1024 * 1024);
                #[cfg(target_os = "linux")]
                {
                    // Die with the hex.
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                }
                Ok(())
            });
        }
    }

    /// Nothing to hold on Unix: the rlimits and the death signal are set in `before_spawn`.
    pub struct Job;

    pub fn after_spawn(_child: &std::process::Child, _memory_mb: u64, _cpu_seconds: u64) -> anyhow::Result<Job> {
        Ok(Job)
    }

    /// In the worker, before it reads anything: no new privileges, and (Linux)
    /// a seccomp filter allowing only what parsing needs.
    pub fn in_worker() -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: a plain prctl on this process.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                anyhow::bail!("PR_SET_NO_NEW_PRIVS failed");
            }
            // seccompiler supports 64-bit x86, ARM and RISC-V; on others
            // (32-bit ARM) the worker has the resource limits only.
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
            seccomp::apply()?;
        }
        Ok(())
    }

    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    mod seccomp {
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
        use std::collections::BTreeMap;

        /// System calls a worker may make: memory, threads, time, signals, and
        /// I/O on the descriptors it already has. Everything else (open,
        /// socket, connect, exec, ptrace, ...) fails with EPERM.
        const ALLOWED: &[libc::c_long] = &[
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_readv,
            libc::SYS_writev,
            libc::SYS_close,
            libc::SYS_mmap,
            libc::SYS_munmap,
            libc::SYS_mremap,
            libc::SYS_mprotect,
            libc::SYS_brk,
            libc::SYS_madvise,
            libc::SYS_futex,
            libc::SYS_exit,
            libc::SYS_exit_group,
            libc::SYS_rt_sigreturn,
            libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_sigaltstack,
            libc::SYS_clock_gettime,
            libc::SYS_clock_nanosleep,
            libc::SYS_nanosleep,
            libc::SYS_getrandom,
            libc::SYS_sched_yield,
            libc::SYS_sched_getaffinity,
            libc::SYS_clone,
            libc::SYS_clone3,
            libc::SYS_set_robust_list,
            libc::SYS_rseq,
            libc::SYS_gettid,
            libc::SYS_getpid,
            libc::SYS_tgkill,
            libc::SYS_prctl,
            libc::SYS_fstat,
            libc::SYS_statx,
            libc::SYS_newfstatat,
            #[cfg(target_arch = "x86_64")]
            libc::SYS_arch_prctl,
            #[cfg(target_arch = "x86_64")]
            libc::SYS_poll,
            libc::SYS_ppoll,
        ];

        pub fn apply() -> anyhow::Result<()> {
            // c_long is i64 on 64-bit Linux and i32 on 32-bit.
            #[allow(clippy::useless_conversion)]
            let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = ALLOWED.iter().map(|n| (i64::from(*n), Vec::new())).collect();
            let filter = SeccompFilter::new(rules, SeccompAction::Errno(libc::EPERM as u32), SeccompAction::Allow, std::env::consts::ARCH.try_into()?)?;
            let program: BpfProgram = filter.try_into()?;
            // Also sets no-new-privileges.
            seccompiler::apply_filter(&program)?;
            Ok(())
        }
    }
}

#[cfg(windows)]
mod restrict {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_TIME, JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
        JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
        JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };

    pub fn before_spawn(command: &mut std::process::Command, _memory_mb: u64, _cpu_seconds: u64) {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    /// The worker's job: closing it (when the run ends, or the hex dies) kills the worker.
    pub struct Job(HANDLE);

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateJobObjectW and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn after_spawn(child: &std::process::Child, memory_mb: u64, cpu_seconds: u64) -> anyhow::Result<Job> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: plain Win32 calls on handles that stay valid for the call.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                anyhow::bail!("couldn't create the sandbox's job object");
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_PROCESS_TIME;
            info.BasicLimitInformation.ActiveProcessLimit = 1;
            // In 100-nanosecond units.
            info.BasicLimitInformation.PerProcessUserTimeLimit = (cpu_seconds as i64) * 10_000_000;
            info.ProcessMemoryLimit = (memory_mb as usize) * 1024 * 1024;
            if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const core::ffi::c_void, std::mem::size_of_val(&info) as u32) == 0 {
                CloseHandle(job);
                anyhow::bail!("couldn't set the sandbox's limits");
            }
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
            };
            SetInformationJobObject(job, JobObjectBasicUIRestrictions, &ui as *const _ as *const core::ffi::c_void, std::mem::size_of_val(&ui) as u32);
            if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
                CloseHandle(job);
                anyhow::bail!("couldn't put the parser in its sandbox");
            }
            Ok(Job(job))
        }
    }

    pub fn in_worker() -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_refuse_oversized_payloads() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b'R', &[1, 2, 3]).unwrap();
        let (tag, payload) = read_frame(&mut buf.as_slice(), 16).unwrap();
        assert_eq!((tag, payload), (b'R', vec![1, 2, 3]));
        let mut big = Vec::new();
        write_frame(&mut big, b'D', &[0; 64]).unwrap();
        assert!(read_frame(&mut big.as_slice(), 32).is_err(), "a frame over the limit is refused before it's read");
    }

    /// The filter really blocks files, the network and programs. The test
    /// binary runs itself again as a child that applies the filter (it can't
    /// be lifted, so it mustn't touch the test runner's own process).
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
    #[test]
    fn the_linux_sandbox_blocks_files_network_and_programs() {
        if std::env::var_os("HEXDB_SANDBOX_PROBE").is_some() {
            restrict::in_worker().expect("the sandbox applies");
            let file = std::fs::File::open("/etc/hostname");
            let net = std::net::TcpStream::connect("127.0.0.1:9");
            let exec = std::process::Command::new("/bin/true").status();
            let blocked = |r: &std::io::Result<()>| r.as_ref().err().and_then(|e| e.raw_os_error()) == Some(libc::EPERM);
            let report = [blocked(&file.map(|_| ())), blocked(&net.map(|_| ())), exec.is_err()];
            // The parsing a worker does still works under the filter.
            let (extracted, _) = in_process(Box::new(b"still parses".to_vec()), "a.txt", "text/plain", &Limits::default(), None);
            let parsed = extracted.meta.get("text").and_then(Value::as_str) == Some("still parses");
            std::process::exit(if report.iter().all(|b| *b) && parsed { 0 } else { 3 });
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "extract::sandbox::tests::the_linux_sandbox_blocks_files_network_and_programs", "--nocapture"])
            .env("HEXDB_SANDBOX_PROBE", "1")
            .status()
            .unwrap();
        assert!(status.success(), "the child could open a file, connect or exec under the filter ({:?})", status);
    }

    #[test]
    fn in_process_extraction_is_the_fallback() {
        assert!(!enabled());
        let (extracted, thumbnail) = extract(Box::new(b"hello sandbox".to_vec()), "a.txt", "text/plain", &Limits::default(), None);
        assert_eq!(extracted.meta.get("text").and_then(Value::as_str), Some("hello sandbox"));
        assert!(thumbnail.is_none());
    }
}
