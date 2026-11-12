// HexDB Core Tools: programs HexDB uses when they're installed
//
//   tesseract   OCR: the text of images and of PDFs with little text of their
//               own (scans), rendered page by page by the parser
//   ffmpeg      thumbnails of formats HexDB can't decode itself: HEIC and
//               AVIF photos, HEVC, VP9 and AV1 video (one keyframe the parser
//               finds), and the start of streamable video (WebM, Matroska, ...)
//
// Both run after the parser, on the hex (`engine::files`, `ToolJob`).
//
// `files.tesseract` and `files.ffmpeg` are "auto" (look on the PATH and in
// the usual install folders), a path, or "off". They're found once, at
// startup, and asked for their version; /status lists what was found.
//
// A tool never sees a file on disk: HexDB writes the input to its stdin and
// reads the result from its stdout, so decrypted uploads stay in memory. It
// runs in the process sandbox (`crate::sandbox`, as plugins do) with no
// network, no folders to write, HexDB's own files kept out, a memory limit,
// a CPU limit and one process, in a clean environment, and is killed at its
// deadline. Without the sandbox launcher (library use) it runs unsandboxed.

use crate::sandbox::Policy;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{info, warn};

/// A tool that was found.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: &'static str,
    pub path: PathBuf,
    /// The first line of its version output.
    pub version: String,
    /// Kept out of its sandbox (HexDB's data, config and TLS key).
    #[serde(skip)]
    deny: Vec<PathBuf>,
}

static TESSERACT: OnceLock<Option<Tool>> = OnceLock::new();
static FFMPEG: OnceLock<Option<Tool>> = OnceLock::new();
static LANGUAGES: OnceLock<String> = OnceLock::new();

/// Memory a tool may use.
const MEMORY_MB: u64 = 1536;
/// Most a tool may print (a frame or a page's text is far less).
const MAX_OUTPUT: usize = 64 * 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// Where each tool is usually installed, besides the PATH.
fn usual_places(name: &str) -> Vec<PathBuf> {
    let exe = format!("{}{}", name, std::env::consts::EXE_SUFFIX);
    let mut out = Vec::new();
    if cfg!(windows) {
        if name == "tesseract" {
            out.push(PathBuf::from(r"C:\Program Files\Tesseract-OCR").join(&exe));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            out.push(PathBuf::from(local).join(r"Microsoft\WinGet\Links").join(&exe));
        }
    } else {
        for dir in ["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            out.push(Path::new(dir).join(&exe));
        }
    }
    out
}

/// Find a tool by its setting ("auto", a path, or "off") and ask its version.
async fn find(name: &'static str, setting: &str, deny: &[PathBuf]) -> Option<Tool> {
    let setting = setting.trim();
    if setting.eq_ignore_ascii_case("off") || setting.is_empty() {
        return None;
    }
    let path = if setting.eq_ignore_ascii_case("auto") {
        crate::sandbox::resolve_program(name).or_else(|| usual_places(name).into_iter().find(|p| p.is_file()))
    } else {
        let found = crate::sandbox::resolve_program(setting);
        if found.is_none() {
            warn!("⚠️ files.{} is {}, but there's no program there.", name, setting);
        }
        found
    }?;
    let mut tool = Tool { name, path, version: String::new(), deny: deny.to_vec() };
    let flag = if name == "ffmpeg" { "-version" } else { "--version" };
    match tool.run(&[flag.to_string()], &[], VERSION_TIMEOUT).await {
        // Tesseract prints its version to stdout (older ones to stderr, which `run` folds in on success).
        Ok(out) => tool.version = String::from_utf8_lossy(&out).lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().chars().take(120).collect(),
        Err(e) => {
            warn!("⚠️ Found {} at {}, but it didn't run: {:#}. HexDB won't use it.", name, tool.path.display(), e);
            return None;
        }
    }
    info!("🧰 Using {} ({}) at {}.", name, tool.version, tool.path.display());
    Some(tool)
}

/// Find the tools (once, at startup). `deny`: paths no tool may read.
pub async fn detect(config: &crate::config::FilesConfig, deny: Vec<PathBuf>) {
    let _ = LANGUAGES.set(config.ocr_languages.trim().to_string());
    let tesseract = find("tesseract", &config.tesseract, &deny).await;
    let _ = TESSERACT.set(tesseract);
    let ffmpeg = find("ffmpeg", &config.ffmpeg, &deny).await;
    let _ = FFMPEG.set(ffmpeg);
}

pub fn tesseract() -> Option<&'static Tool> {
    TESSERACT.get().and_then(Option::as_ref)
}

pub fn ffmpeg() -> Option<&'static Tool> {
    FFMPEG.get().and_then(Option::as_ref)
}

/// Tesseract's languages: `files.ocr_languages` ("eng", "eng+deu", ...).
pub fn ocr_languages() -> &'static str {
    LANGUAGES.get().map(String::as_str).filter(|l| !l.is_empty()).unwrap_or("eng")
}

/// What was found, for /status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ToolsStatus {
    pub tesseract: Option<Tool>,
    pub ffmpeg: Option<Tool>,
    pub ocr_languages: String,
}

pub fn status() -> ToolsStatus {
    ToolsStatus { tesseract: tesseract().cloned(), ffmpeg: ffmpeg().cloned(), ocr_languages: ocr_languages().to_string() }
}

/// Variables a tool's runtime needs, passed from HexDB's environment; nothing else is.
const PASSED_ENV: &[&str] = &["PATH", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "TESSDATA_PREFIX", "LANG", "LC_ALL"];

impl Tool {
    fn policy(&self, timeout: Duration) -> Policy {
        let folder = self.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut read = vec![folder];
        if let Some(tessdata) = std::env::var_os("TESSDATA_PREFIX") {
            read.push(PathBuf::from(tessdata));
        }
        Policy {
            name: crate::sandbox::sandbox_name("Tool", self.name),
            read,
            write: Vec::new(),
            deny: self.deny.clone(),
            network: false,
            memory_mb: MEMORY_MB,
            cpu_seconds: timeout.as_secs().max(1) * 2,
            max_processes: 1,
        }
    }

    /// Run the tool with `args`, `input` on its stdin, and return its stdout.
    pub async fn run(&self, args: &[String], input: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        let program = self.path.to_string_lossy().to_string();
        let mut command = match crate::sandbox::launcher() {
            Some(launcher) => {
                let (exe, wrapped) = crate::sandbox::wrap(launcher, &self.policy(timeout), &program, args);
                let mut c = Command::new(exe);
                c.args(wrapped);
                c
            }
            None => {
                let mut c = Command::new(&self.path);
                c.args(args);
                c
            }
        };
        command.env_clear();
        for name in PASSED_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        // One thread each: tools share the machine with the database.
        command.env("OMP_THREAD_LIMIT", "1");
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        if let Some(dir) = self.path.parent() {
            command.current_dir(dir);
        }
        crate::process::contain(&mut command);
        let mut child = command.spawn().with_context(|| format!("couldn't start {}", self.name))?;
        crate::process::adopt(&child);
        let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let input = input.to_vec();
        let work = async move {
            // The tool may stop reading early (ffmpeg after its frame): a broken pipe is fine.
            let write = async move {
                if let Some(mut pipe) = stdin {
                    let _ = pipe.write_all(&input).await;
                    let _ = pipe.shutdown().await;
                }
            };
            let read = async move {
                let mut out = Vec::new();
                if let Some(pipe) = stdout {
                    let _ = pipe.take(MAX_OUTPUT as u64 + 1).read_to_end(&mut out).await;
                }
                out
            };
            let errors = async move {
                let mut out = Vec::new();
                if let Some(pipe) = stderr {
                    let _ = pipe.take(64 * 1024).read_to_end(&mut out).await;
                }
                out
            };
            let ((), out, errors) = tokio::join!(write, read, errors);
            let status = child.wait().await;
            (status, out, errors)
        };
        let (status, mut out, errors) = tokio::time::timeout(timeout, work).await.map_err(|_| anyhow!("{} took longer than {} s", self.name, timeout.as_secs()))?;
        let status = status?;
        if out.len() > MAX_OUTPUT {
            bail!("{} printed more than {} MB", self.name, MAX_OUTPUT / (1024 * 1024));
        }
        if !status.success() {
            let message = String::from_utf8_lossy(&errors);
            let last = message.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            bail!("{} failed ({}){}", self.name, status, if last.is_empty() { String::new() } else { format!(": {}", last.chars().take(300).collect::<String>()) });
        }
        if out.is_empty() {
            out = errors;
        }
        Ok(out)
    }
}


/// A thumbnail at most `max_px` on a side from what ffmpeg decodes of
/// `input`: a whole picture (HEIC, AVIF), a keyframe stream (`format`: "hevc",
/// "obu", "ivf"), or the start of a video it reads without seeking. Turned by
/// `rotation` quarter turns clockwise; never enlarged.
pub async fn ffmpeg_thumbnail(tool: &Tool, input: Vec<u8>, format: Option<&str>, rotation: u32, max_px: u32, timeout: Duration) -> Result<Option<crate::extract::thumbnail::Thumbnail>> {
    let turn = match rotation {
        1 => "transpose=1,",
        2 => "transpose=1,transpose=1,",
        3 => "transpose=2,",
        _ => "",
    };
    let filter = format!("{}scale='min(iw,{m})':'min(ih,{m})':force_original_aspect_ratio=decrease", turn, m = max_px.clamp(16, 2048));
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error"].iter().map(|s| s.to_string()).collect();
    if let Some(format) = format {
        args.extend(["-f".to_string(), format.to_string()]);
    }
    args.extend(
        ["-i", "pipe:0", "-map", "0:v:0", "-frames:v", "1", "-an", "-vf", &filter, "-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "4", "pipe:1"]
            .iter()
            .map(|s| s.to_string()),
    );
    let jpeg = tool.run(&args, &input, timeout).await?;
    if jpeg.is_empty() {
        return Ok(None);
    }
    if !jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
        bail!("ffmpeg didn't make a JPEG");
    }
    let size = imagesize::blob_size(&jpeg).map_err(|e| anyhow!("ffmpeg's picture: {}", e))?;
    let (width, height) = (size.width as u32, size.height as u32);
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        bail!("ffmpeg made a {}x{} picture", width, height);
    }
    Ok(Some(crate::extract::thumbnail::Thumbnail { bytes: jpeg, width, height, content_type: "image/jpeg" }))
}

/// The text Tesseract reads in an image (PNG, JPEG, TIFF, BMP, GIF or WebP).
pub async fn ocr(tool: &Tool, image: Vec<u8>, timeout: Duration) -> Result<String> {
    let args: Vec<String> = ["stdin", "stdout", "-l", ocr_languages(), "--psm", "3"].iter().map(|s| s.to_string()).collect();
    let out = tool.run(&args, &image, timeout).await?;
    Ok(String::from_utf8_lossy(&out).to_string())
}

/// True if OCR found text worth keeping: a photo with no writing in it often
/// reads as a few stray marks. At least three words of three or more letters.
pub fn worth_keeping(text: &str) -> bool {
    text.split_whitespace().filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3).take(3).count() >= 3
}

/// Tidy OCR output: no form feeds, trailing spaces or runs of blank lines.
pub fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.replace('\u{c}', "\n").lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_text_is_kept_only_when_it_reads_like_words() {
        assert!(worth_keeping("Invoice number 1234 for March"));
        assert!(!worth_keeping("~ |l . ,, i"));
        assert!(!worth_keeping("ab cd ef gh"));
        assert_eq!(tidy("one  \n\n\n\ntwo\u{c}\nthree\n\n"), "one\n\ntwo\n\nthree", "a page break leaves one blank line");
    }

    #[test]
    fn tools_that_are_off_or_missing_are_not_used() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert!(runtime.block_on(find("ffmpeg", "off", &[])).is_none());
        assert!(runtime.block_on(find("ffmpeg", "/no/such/ffmpeg", &[])).is_none());
    }
}
