//! Linux replay recording backends. Every child has a private process group,
//! drained stderr, bounded shutdown, and is reaped before its media is read.
use crate::config::{Config, RecordingConfig};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const GSR_APP: &str = "com.dec05eba.gpu_screen_recorder";
const SEGMENT_SECONDS: u64 = 30;

pub fn has_program(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|p| {
            p.join(program)
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}
fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland")
}
pub fn gsr_window_capture_supported() -> bool {
    !wayland()
}
fn x11() -> bool {
    !wayland() && std::env::var_os("DISPLAY").is_some()
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

async fn output(args: &[String], seconds: u64) -> Result<std::process::Output> {
    let (program, args) = args.split_first().context("empty command")?;
    let mut command = Command::new(program);
    command.args(args).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(seconds), command.output())
        .await
        .context("command timed out")?
        .context("could not run command")
}
fn combined(out: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .trim()
    .to_owned()
}
fn in_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}
fn host_prefix(mut args: Vec<String>) -> Vec<String> {
    if in_flatpak() {
        let mut prefix = strings(&["flatpak-spawn", "--host", "--watch-bus"]);
        prefix.append(&mut args);
        prefix
    } else {
        args
    }
}
pub async fn gsr_source(preference: &str) -> Option<&'static str> {
    if !in_flatpak() && preference != "flatpak" && has_program("gpu-screen-recorder") {
        return Some("native");
    }
    if preference == "native" && !in_flatpak() {
        return None;
    }
    if (has_program("flatpak") || in_flatpak())
        && output(&host_prefix(strings(&["flatpak", "info", GSR_APP])), 5)
            .await
            .is_ok_and(|o| o.status.success())
    {
        Some("flatpak")
    } else {
        None
    }
}
async fn gsr_prefix(preference: &str) -> Result<Vec<String>> {
    match gsr_source(preference).await {
        Some("native") => Ok(strings(&["gpu-screen-recorder"])),
        Some("flatpak") => Ok(host_prefix(strings(&[
            "flatpak",
            "run",
            "--command=gpu-screen-recorder",
            GSR_APP,
        ]))),
        _ => {
            bail!("gpu-screen-recorder is not installed; install it or select an alternate backend")
        }
    }
}

#[derive(Debug)]
struct Capture {
    child: Child,
    pid: u32,
    argv: Vec<String>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    reader: JoinHandle<()>,
    stopped: bool,
}
impl Capture {
    async fn spawn(args: Vec<String>) -> Result<Self> {
        let mut command = Command::new(&args[0]);
        command
            .args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);
        let mut child = command
            .spawn()
            .with_context(|| format!("could not run {}", args[0]))?;
        let pid = child.id().context("capture child has no pid")?;
        let tail = Arc::new(Mutex::new(VecDeque::with_capacity(12)));
        let stderr = child.stderr.take().context("capture has no stderr")?;
        let reader_tail = tail.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                tracing::debug!(capture = %line);
                if let Ok(mut tail) = reader_tail.lock() {
                    if tail.len() == 12 {
                        tail.pop_front();
                    }
                    tail.push_back(line.to_string());
                }
            }
        });
        register_capture(pid, &args);
        Ok(Self {
            child,
            pid,
            argv: args,
            stderr: tail,
            reader,
            stopped: false,
        })
    }
    fn healthy(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
    fn last_output(&self) -> String {
        self.stderr
            .lock()
            .map(|q| q.iter().cloned().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
    }
    async fn startup_check(&mut self) -> Result<()> {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        if !self.healthy() {
            tokio::task::yield_now().await;
            bail!(
                "{} exited during startup: {}",
                self.argv[0],
                self.last_output()
            );
        }
        Ok(())
    }
    async fn signal_replay(&mut self) -> Result<()> {
        if !self.healthy() {
            bail!("the recorder stopped: {}", self.last_output());
        }
        let target = self
            .recorder_pid()
            .await
            .context("the Flatpak recorder has not started inside its sandbox")?;
        self.signal_recorder(target, libc::SIGUSR1).await
    }
    fn flatpak(&self) -> bool {
        self.argv.iter().any(|a| a == GSR_APP)
    }
    async fn recorder_pid(&self) -> Option<u32> {
        if !self.flatpak() {
            return Some(self.pid);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let target = if in_flatpak() {
                // Arguments remain separate shell parameters; the fixed script
                // compares complete argv, including output paths containing spaces.
                const FIND: &str = "want=$(printf '%s\\n' \"$@\"); for d in /proc/[0-9]*; do [ \"$(cat \"$d/comm\" 2>/dev/null)\" = gpu-screen-reco ] || continue; [ \"$(tr '\\000' '\\n' < \"$d/cmdline\" 2>/dev/null)\" = \"$want\" ] && { printf '%s\\n' \"${d#/proc/}\"; exit 0; }; done; exit 1";
                let offset = self.argv.iter().position(|a| a == GSR_APP)?;
                let mut args = strings(&["sh", "-c", FIND, "sh", "gpu-screen-recorder"]);
                args.extend(self.argv[offset + 1..].iter().cloned());
                output(&host_prefix(args), 3)
                    .await
                    .ok()
                    .filter(|o| o.status.success())
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse::<u32>()
                            .ok()
                    })
            } else {
                find_recorder_descendant(self.pid)
            };
            if target.is_some() || Instant::now() >= deadline {
                return target;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn signal_recorder(&self, pid: u32, sig: i32) -> Result<()> {
        if pid == 0 || pid > i32::MAX as u32 {
            bail!("invalid recorder pid");
        }
        if self.flatpak() && in_flatpak() {
            checked_command(
                &host_prefix(vec!["kill".into(), format!("-{sig}"), pid.to_string()]),
                3,
            )
            .await
        } else {
            signal(pid as i32, sig)
        }
    }
    async fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        // bwrap is a supervisor, and forwarding SIGINT to its group can kill
        // it before the recorder flushes its container. Signal the inner
        // recorder directly and reap the wrapper only after finalization.
        if self.flatpak() {
            if let Some(target) = self.recorder_pid().await {
                let _ = self.signal_recorder(target, libc::SIGINT).await;
            }
        } else {
            let _ = signal(-(self.pid as i32), libc::SIGINT);
        }
        if tokio::time::timeout(Duration::from_secs(8), self.child.wait())
            .await
            .is_err()
        {
            let _ = signal(-(self.pid as i32), libc::SIGKILL);
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        // A privileged capture helper can outlive its group leader.
        let _ = signal(-(self.pid as i32), libc::SIGKILL);
        self.stopped = true;
        unregister_capture(self.pid);
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut self.reader).await;
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = signal(-(self.pid as i32), libc::SIGKILL);
            let _ = self.child.start_kill();
        }
        self.reader.abort();
        unregister_capture(self.pid);
    }
}
fn signal(pid: i32, sig: i32) -> Result<()> {
    if pid == 0 || pid == -1 {
        bail!("refusing unsafe process id");
    }
    if unsafe { libc::kill(pid, sig) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn descendants(pid: u32) -> Vec<u32> {
    let mut found = vec![];
    let mut pending = vec![pid];
    while let Some(parent) = pending.pop() {
        if found.len() > 1024 {
            break;
        }
        let path = format!("/proc/{parent}/task/{parent}/children");
        if let Ok(text) = std::fs::read_to_string(path) {
            for child in text
                .split_whitespace()
                .filter_map(|n| n.parse::<u32>().ok())
            {
                if !found.contains(&child) {
                    found.push(child);
                    pending.push(child);
                }
            }
        }
    }
    found
}
fn find_recorder_descendant(pid: u32) -> Option<u32> {
    descendants(pid).into_iter().find(|child| {
        std::fs::read(format!("/proc/{child}/cmdline")).is_ok_and(|raw| {
            raw.split(|b| *b == 0).next().is_some_and(|p| {
                Path::new(std::ffi::OsStr::from_bytes(p))
                    .file_name()
                    .is_some_and(|n| n == "gpu-screen-recorder")
            })
        })
    })
}
use std::os::unix::ffi::OsStrExt;
fn registry_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("vice-{}", unsafe { libc::getuid() }))
        .join("captures.json")
}
static REGISTRY_LOCK: Mutex<()> = Mutex::new(());
fn read_registry() -> Vec<Value> {
    std::fs::read(registry_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn write_registry(entries: &[Value]) {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(entries) {
        let temp = path.with_extension("json.tmp");
        if std::fs::write(&temp, bytes).is_ok() {
            let _ = std::fs::rename(temp, path);
        }
    }
}
fn register_capture(pid: u32, argv: &[String]) {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = read_registry();
    entries.retain(|v| v["pid"].as_u64() != Some(pid as u64));
    entries.push(json!({"pid":pid,"owner":std::process::id(),"argv":argv}));
    write_registry(&entries);
}
fn unregister_capture(pid: u32) {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = read_registry();
    entries.retain(|v| v["pid"].as_u64() != Some(pid as u64));
    write_registry(&entries);
}
pub fn reap_orphaned_captures() -> usize {
    let _guard = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut kept = vec![];
    let mut killed = 0;
    for entry in read_registry() {
        let (Some(pid), Some(owner), Some(argv)) = (
            entry["pid"].as_u64(),
            entry["owner"].as_u64(),
            entry["argv"].as_array(),
        ) else {
            continue;
        };
        if pid == 0 || pid > i32::MAX as u64 || pid == std::process::id() as u64 {
            continue;
        }
        if owner != std::process::id() as u64 && unsafe { libc::kill(owner as i32, 0) } == 0 {
            kept.push(entry);
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let running: Vec<_> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let expected: Vec<_> = argv.iter().filter_map(Value::as_str).collect();
        if running.iter().map(String::as_str).eq(expected)
            && signal(-(pid as i32), libc::SIGKILL).is_ok()
        {
            killed += 1;
        }
    }
    write_registry(&kept);
    killed
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter()
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
}
pub fn sanitize_gsr_args(args: &[String], blocked: &[&str]) -> Vec<String> {
    let mut output = vec![];
    let mut i = 0;
    while i < args.len() {
        if blocked.iter().any(|f| args[i] == *f) {
            i += 2;
            continue;
        }
        if blocked
            .iter()
            .any(|f| args[i].starts_with(&format!("{f}=")))
        {
            i += 1;
            continue;
        }
        output.push(args[i].clone());
        i += 1;
    }
    output
}
pub fn codec_for_encoder(encoder: &str, depth: &str) -> Option<String> {
    let base = if encoder.starts_with("h264") {
        "h264"
    } else if encoder.starts_with("hevc") || encoder == "libx265" {
        "hevc"
    } else if encoder.starts_with("av1") {
        "av1"
    } else if depth == "10" {
        "hevc"
    } else {
        return None;
    };
    let ten = depth == "10" && base != "h264";
    Some(format!(
        "{base}{}{}",
        if ten { "_10bit" } else { "" },
        if encoder.ends_with("_vulkan") {
            "_vulkan"
        } else {
            ""
        }
    ))
}
fn source_kind(value: &str) -> &'static str {
    if value == "default_output" || (value.starts_with("device:") && value.ends_with(".monitor")) {
        "monitor"
    } else if value == "default_input" || value.starts_with("device:") {
        "input"
    } else if value.starts_with("app:") || value.starts_with("app-inverse:") {
        "app"
    } else {
        "unknown"
    }
}
fn mic_source(rc: &RecordingConfig) -> String {
    if rc.microphone_source.trim().is_empty() {
        "default_input".into()
    } else {
        rc.microphone_source.trim().into()
    }
}
fn source_layout(rc: &RecordingConfig) -> (Vec<String>, bool) {
    let mut tracks: Vec<String> = rc
        .audio_tracks
        .iter()
        .filter_map(|track| {
            let parts: Vec<_> = track
                .split('|')
                .map(str::trim)
                .filter(|s| !s.is_empty() && (rc.capture_audio || source_kind(s) == "input"))
                .collect();
            (!parts.is_empty()).then(|| parts.join("|"))
        })
        .collect();
    if !tracks.is_empty() && rc.capture_microphone {
        let mic = mic_source(rc);
        if !tracks.iter().any(|t| t.split('|').any(|p| p == mic)) {
            tracks.push(mic);
        }
    }
    let mut mixed = false;
    if rc.audio_tracks_mix_first && tracks.len() > 1 {
        let parts: Vec<_> = tracks.iter().flat_map(|s| s.split('|')).collect();
        let monitor = parts.iter().any(|p| source_kind(p) == "monitor");
        let both = parts.iter().any(|p| p.starts_with("app:"))
            && parts.iter().any(|p| p.starts_with("app-inverse:"));
        if monitor || !both {
            let mut unique = vec![];
            for part in parts {
                if !(monitor && source_kind(part) == "app") && !unique.contains(&part) {
                    unique.push(part);
                }
            }
            let mix = unique.join("|");
            if !mix.is_empty() {
                tracks.insert(0, mix);
                mixed = true;
            }
        }
    }
    (tracks, mixed)
}
fn save_audio_pass(rc: &RecordingConfig) -> bool {
    rc.audio_tracks.is_empty()
        && (rc.microphone_mono && rc.capture_microphone
            || (rc.desktop_volume - 1.).abs() > 0.01
            || (rc.microphone_volume - 1.).abs() > 0.01)
}
pub fn gsr_audio_args(rc: &RecordingConfig, split_for_volume: bool) -> Vec<String> {
    let (tracks, _) = source_layout(rc);
    if !tracks.is_empty() {
        return tracks.into_iter().flat_map(|t| ["-a".into(), t]).collect();
    }
    let desktop = if rc.gsr_audio_source.trim().is_empty() {
        "default_output"
    } else {
        rc.gsr_audio_source.trim()
    };
    let mic = mic_source(rc);
    if split_for_volume && rc.capture_audio && rc.capture_microphone && save_audio_pass(rc) {
        return vec!["-a".into(), desktop.into(), "-a".into(), mic];
    }
    let mut sources = vec![];
    if rc.capture_audio {
        sources.extend(desktop.split('|').map(str::to_owned));
    }
    if rc.capture_microphone && !sources.contains(&mic) {
        sources.push(mic);
    }
    if sources.is_empty() {
        vec![]
    } else {
        vec!["-a".into(), sources.join("|")]
    }
}
fn container(rc: &RecordingConfig) -> &str {
    if rc.container == "mkv" { "mkv" } else { "mp4" }
}
fn valid_resolution(value: &str) -> bool {
    value.split_once('x').is_some_and(|(w, h)| {
        w.parse::<u32>().is_ok_and(|n| n > 0) && h.parse::<u32>().is_ok_and(|n| n > 0)
    })
}
async fn supported_codecs(prefix: &[String]) -> HashSet<String> {
    let mut cmd = prefix.to_vec();
    cmd.push("--info".into());
    let mut codecs = HashSet::new();
    if let Ok(out) = output(&cmd, 5).await {
        let text = combined(&out);
        let mut in_section = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with("section=") {
                in_section = line == "section=video_codecs";
            } else if in_section
                && !line.is_empty()
                && line
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                codecs.insert(line.to_owned());
            }
        }
    }
    codecs
}
fn choose_codec(
    rc: &RecordingConfig,
    available: &HashSet<String>,
    avoid: Option<&str>,
) -> Option<String> {
    let configured = codec_for_encoder(&rc.encoder, &rc.color_depth);
    if let Some(codec) = &configured
        && Some(codec.as_str()) != avoid
        && (available.is_empty() || available.contains(codec))
    {
        return configured;
    }
    if configured.is_none()
        && avoid.is_none()
        && (available.is_empty()
            || ["h264", "hevc", "av1"]
                .iter()
                .any(|c| available.contains(*c)))
    {
        return None;
    }
    let preferred = if rc.color_depth == "10" {
        &[
            "hevc_10bit",
            "av1_10bit",
            "hevc_10bit_vulkan",
            "av1_10bit_vulkan",
        ][..]
    } else {
        &[
            "hevc",
            "av1",
            "h264",
            "h264_vulkan",
            "hevc_vulkan",
            "av1_vulkan",
        ][..]
    };
    preferred
        .iter()
        .find(|c| Some(**c) != avoid && available.contains(**c))
        .map(|s| (*s).into())
}
fn encoder_failure(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    [
        "could not open video codec",
        "failed to create encoder",
        "libnvidia-encode",
        "libcuda",
        "nvenc",
        "vaapi",
        "no encoder",
        "not supported",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

#[derive(Clone)]
struct Segment {
    started: Instant,
    path: PathBuf,
}
enum SegmentCommand {
    Flush(oneshot::Sender<Result<Vec<Segment>>>),
    Stop(oneshot::Sender<()>),
}
struct SegmentEngine {
    tx: mpsc::Sender<SegmentCommand>,
    task: JoinHandle<()>,
    healthy: Arc<AtomicBool>,
    error: Arc<Mutex<String>>,
    directory: PathBuf,
}
async fn start_capture_candidates(commands: Vec<Vec<String>>) -> Result<(Capture, Vec<String>)> {
    let mut failures = vec![];
    for command in commands {
        let mut capture = match Capture::spawn(command.clone()).await {
            Ok(capture) => capture,
            Err(error) => {
                failures.push(error.to_string());
                continue;
            }
        };
        match capture.startup_check().await {
            Ok(()) => return Ok((capture, command)),
            Err(error) => {
                tracing::warn!(%error,"Capture encoder failed; trying fallback");
                failures.push(error.to_string());
                let _ = capture.stop().await;
            }
        }
    }
    bail!("No capture encoder started: {}", failures.join("; "))
}
fn capture_output(template: &[String], path: &Path) -> Vec<String> {
    let mut command = template.to_vec();
    let index = if command.first().is_some_and(|program| {
        Path::new(program)
            .file_name()
            .is_some_and(|name| name == "wf-recorder")
    }) {
        command.iter().position(|arg| arg == "-f").map(|i| i + 1)
    } else {
        command.len().checked_sub(1)
    };
    if let Some(index) = index {
        command[index] = path.to_string_lossy().into_owned();
    }
    command
}

#[derive(Clone, Copy)]
struct BackendAvailability {
    gsr: bool,
    wayland: bool,
    x11: bool,
    wf: bool,
    ffmpeg: bool,
}

fn select_backend(preference: &str, available: BackendAvailability) -> Result<&'static str> {
    match preference {
        "auto" if available.gsr => Ok("gsr"),
        "auto" if available.wayland && available.wf => Ok("wf-recorder"),
        "auto" if available.x11 && available.ffmpeg => Ok("ffmpeg"),
        "auto" => bail!(
            "No supported screen-capture backend is available for this session. Install gpu-screen-recorder, wf-recorder for Wayland, or ffmpeg for X11."
        ),
        "gsr" if available.gsr => Ok("gsr"),
        "gsr" => bail!(
            "gpu-screen-recorder is selected, but is not installed; install it or select an alternate backend"
        ),
        "wf-recorder" if !available.wayland => bail!("wf-recorder requires a Wayland session"),
        "wf-recorder" if !available.wf => bail!("wf-recorder is not installed"),
        "wf-recorder" => Ok("wf-recorder"),
        "ffmpeg" if !available.x11 => bail!("ffmpeg x11grab requires an X11 session"),
        "ffmpeg" if !available.ffmpeg => bail!("ffmpeg is not installed"),
        "ffmpeg" => Ok("ffmpeg"),
        _ => bail!("invalid recording.backend: {preference}"),
    }
}

pub struct Recorder {
    pub cfg: Config,
    pub display_override: Option<String>,
    pub window_override: Option<String>,
    pub game_tag: Option<String>,
    pub cpu_fallback: bool,
    pub codec_fallback: bool,
    /// The discovered GSR installation, retained even if its process fails.
    pub gsr_source: Option<String>,
    pub last_clip_error: String,
    pub last_save_signal_at: Option<Instant>,
    backend: String,
    capture: Option<Capture>,
    segments: Option<SegmentEngine>,
    segment_template: Option<Vec<String>>,
    session: Option<Capture>,
    session_path: Option<PathBuf>,
    session_start: Option<Instant>,
}
impl Recorder {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            display_override: None,
            window_override: None,
            game_tag: None,
            cpu_fallback: false,
            codec_fallback: false,
            gsr_source: None,
            last_clip_error: String::new(),
            last_save_signal_at: None,
            backend: "gpu-screen-recorder".into(),
            capture: None,
            segments: None,
            segment_template: None,
            session: None,
            session_path: None,
            session_start: None,
        }
    }
    pub fn name(&self) -> &str {
        &self.backend
    }
    pub fn is_healthy(&mut self) -> bool {
        self.capture.as_mut().is_some_and(Capture::healthy)
            || self
                .segments
                .as_ref()
                .is_some_and(|s| s.healthy.load(Ordering::Relaxed) && !s.task.is_finished())
    }
    pub fn last_output(&self) -> String {
        self.capture
            .as_ref()
            .map(Capture::last_output)
            .or_else(|| {
                self.segments
                    .as_ref()
                    .and_then(|s| s.error.lock().ok().map(|e| e.clone()))
            })
            .unwrap_or_default()
    }
    pub fn session_elapsed(&self) -> f64 {
        self.session_start
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.)
    }
    pub fn session_active(&self) -> bool {
        self.session.is_some()
    }
    async fn build_gsr_cmd(
        &self,
        session: Option<&Path>,
        cpu: bool,
        codec: Option<&str>,
    ) -> Result<Vec<String>> {
        let rc = &self.cfg.recording;
        let extra = sanitize_gsr_args(
            &shell_words::split(&rc.gsr_args).context("invalid recording.gsr_args quoting")?,
            if session.is_some() {
                &["-o", "-r"]
            } else {
                &["-o"]
            },
        );
        let mut cmd = gsr_prefix(&rc.gsr_source).await?;
        let mut window_size = None;
        if !has_flag(&extra, "-w") {
            let fallback = self
                .display_override
                .as_deref()
                .or(rc.display.as_deref())
                .unwrap_or("screen");
            let mut target = fallback;
            if rc.window_capture
                && gsr_window_capture_supported()
                && let Some(window) = self.window_override.as_deref()
                && let Some(size) = window_geometry(window).await
            {
                target = window;
                window_size = Some(size);
            }
            cmd.extend([
                "-w".into(),
                target.split('|').next().unwrap_or("screen").trim().into(),
            ]);
        }
        if !has_flag(&extra, "-f") {
            cmd.extend(["-f".into(), rc.fps.to_string()]);
        }
        if !has_flag(&extra, "-s") {
            if let Some(res) = &rc.resolution {
                if valid_resolution(res) {
                    cmd.extend(["-s".into(), res.clone()]);
                }
            } else if let Some((width, height)) = window_size {
                cmd.extend(["-s".into(), format!("{width}x{height}")]);
            }
        }
        if session.is_none() && !has_flag(&extra, "-r") {
            cmd.extend(["-r".into(), rc.buffer_duration.to_string()]);
        }
        if !has_flag(&extra, "-c") {
            cmd.extend(["-c".into(), container(rc).into()]);
        }
        if !cpu
            && !has_flag(&extra, "-k")
            && !has_flag(&extra, "-encoder")
            && let Some(codec) = codec
        {
            cmd.extend(["-k".into(), codec.into()]);
        }
        if cpu && !has_flag(&extra, "-encoder") {
            cmd.extend(["-encoder".into(), "cpu".into()]);
        }
        if !has_flag(&extra, "-a") {
            cmd.extend(gsr_audio_args(rc, session.is_none()));
        }
        if session.is_none()
            && (rc.gsr_replay_storage == "disk"
                || rc.gsr_replay_storage == "auto" && rc.buffer_duration > 600)
            && !has_flag(&extra, "-replay-storage")
        {
            let mut help = gsr_prefix(&rc.gsr_source).await?;
            help.push("--help".into());
            if output(&help, 3)
                .await
                .is_ok_and(|out| combined(&out).contains("-replay-storage"))
            {
                cmd.extend(["-replay-storage".into(), "disk".into()]);
            }
        }
        cmd.extend(extra);
        cmd.push("-o".into());
        cmd.push(
            session
                .map(Path::to_path_buf)
                .unwrap_or_else(|| crate::runtime::resolve_path(&self.cfg.output.directory))
                .to_string_lossy()
                .into_owned(),
        );
        Ok(cmd)
    }
    pub async fn start(&mut self) -> Result<()> {
        if self.is_healthy() {
            return Ok(());
        }
        self.cpu_fallback = false;
        self.codec_fallback = false;
        tokio::fs::create_dir_all(crate::runtime::resolve_path(&self.cfg.output.directory)).await?;
        self.gsr_source = gsr_source(&self.cfg.recording.gsr_source)
            .await
            .map(str::to_owned);
        let pref = select_backend(
            &self.cfg.recording.backend,
            BackendAvailability {
                gsr: self.gsr_source.is_some(),
                wayland: wayland(),
                x11: x11(),
                wf: has_program("wf-recorder"),
                ffmpeg: has_program("ffmpeg"),
            },
        )?;
        if self.cfg.recording.backend == "auto" && pref != "gsr" {
            tracing::info!(
                backend = pref,
                "gpu-screen-recorder unavailable; using session-compatible capture backend"
            );
        }
        if pref == "gsr" {
            self.backend = "gpu-screen-recorder".into();
            self.cpu_fallback = false;
            self.codec_fallback = false;
            let prefix = gsr_prefix(&self.cfg.recording.gsr_source).await?;
            let available = supported_codecs(&prefix).await;
            let mut codec = choose_codec(&self.cfg.recording, &available, None);
            let mut capture =
                Capture::spawn(self.build_gsr_cmd(None, false, codec.as_deref()).await?).await?;
            let mut result = capture.startup_check().await;
            if let Err(error) = &result {
                let detail = error.to_string();
                let _ = capture.stop().await;
                if encoder_failure(&detail) {
                    let avoid = codec.as_deref().unwrap_or("h264");
                    let alt = choose_codec(&self.cfg.recording, &available, Some(avoid));
                    if alt.is_some() && alt != codec {
                        codec = alt;
                        capture = Capture::spawn(
                            self.build_gsr_cmd(None, false, codec.as_deref()).await?,
                        )
                        .await?;
                        result = capture.startup_check().await;
                        self.codec_fallback = result.is_ok();
                        if result.is_err() {
                            let _ = capture.stop().await;
                        }
                    }
                    if result.is_err() {
                        tracing::warn!("GPU encoder failed; attempting CPU capture: {detail}");
                        capture =
                            Capture::spawn(self.build_gsr_cmd(None, true, None).await?).await?;
                        result = capture.startup_check().await;
                        self.cpu_fallback = result.is_ok();
                    }
                }
            }
            result?;
            self.capture = Some(capture);
            return Ok(());
        }
        if pref == "wf-recorder"
            && self.cfg.recording.capture_audio
            && self.cfg.recording.capture_microphone
            && self.cfg.recording.wf_microphone_strategy != "mic_only"
        {
            bail!(
                "wf-recorder cannot mix desktop audio and microphone; choose mic-only mode or the GSR backend"
            );
        }
        self.backend = pref.into();
        self.start_segments().await
    }
    async fn start_segments(&mut self) -> Result<()> {
        let directory = std::env::temp_dir()
            .join(format!("vice-{}", unsafe { libc::getuid() }))
            .join(format!("segments-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&directory).await?;
        let path = directory.join("segment-00000000.mp4");
        let cmd = segment_command(
            &self.cfg,
            self.display_override.as_deref(),
            &self.backend,
            &path,
        )
        .await?;
        let candidates = if self.backend == "ffmpeg" && self.cfg.recording.encoder == "auto" {
            crate::encoding::command_candidates(&cmd).await
        } else {
            vec![cmd]
        };
        let (mut capture, template) = start_capture_candidates(candidates).await?;
        self.segment_template = Some(template.clone());
        let (tx, mut rx) = mpsc::channel(8);
        let healthy = Arc::new(AtomicBool::new(true));
        let healthy_task = healthy.clone();
        let error = Arc::new(Mutex::new(String::new()));
        let error_task = error.clone();
        let cfg = self.cfg.clone();
        let backend = self.backend.clone();
        let dir = directory.clone();
        let task = tokio::spawn(async move {
            let mut segment = Segment {
                started: Instant::now() - Duration::from_secs(1),
                path,
            };
            let mut index = 0u64;
            let mut segments = vec![];
            loop {
                let message = tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(SEGMENT_SECONDS).saturating_sub(segment.started.elapsed())) => None,
                    message = rx.recv() => match message { Some(m) => Some(m), None => { let _ = capture.stop().await; break; } },
                    status = capture.child.wait() => {
                        let detail = format!("{backend} exited: {status:?}: {}", capture.last_output());
                        if let Ok(mut e) = error_task.lock() { *e = detail; } break;
                    }
                };
                let _ = capture.stop().await;
                if segment.path.metadata().is_ok_and(|m| m.len() > 0) {
                    segments.push(segment.clone());
                }
                let oldest = Instant::now()
                    .checked_sub(Duration::from_secs(
                        cfg.recording.buffer_duration.max(1) as u64 + SEGMENT_SECONDS,
                    ))
                    .unwrap_or_else(Instant::now);
                while segments.first().is_some_and(|s| s.started < oldest) {
                    let old = segments.remove(0);
                    let _ = tokio::fs::remove_file(old.path).await;
                }
                if let Some(SegmentCommand::Stop(reply)) = message {
                    let _ = reply.send(());
                    break;
                }
                // Finalized paths are stable until the next rotation. Copying them to
                // a private extraction directory below prevents pruning races.
                if let Some(SegmentCommand::Flush(reply)) = message {
                    let _ = reply.send(Ok(segments.clone()));
                }
                index += 1;
                let path = dir.join(format!("segment-{index:08}.mp4"));
                let command = capture_output(&template, &path);
                capture = match Capture::spawn(command).await {
                    Ok(c) => c,
                    Err(e) => {
                        if let Ok(mut last) = error_task.lock() {
                            *last = e.to_string();
                        }
                        break;
                    }
                };
                segment = Segment {
                    started: Instant::now(),
                    path,
                };
            }
            healthy_task.store(false, Ordering::Relaxed);
        });
        self.segments = Some(SegmentEngine {
            tx,
            task,
            healthy,
            error,
            directory,
        });
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.segment_template = None;
        if self.session.is_some() {
            let _ = self.stop_session().await;
        }
        if let Some(mut capture) = self.capture.take() {
            capture.stop().await?;
        }
        if let Some(mut engine) = self.segments.take() {
            let (tx, rx) = oneshot::channel();
            let _ = engine.tx.send(SegmentCommand::Stop(tx)).await;
            let _ = tokio::time::timeout(Duration::from_secs(10), rx).await;
            if tokio::time::timeout(Duration::from_secs(1), &mut engine.task)
                .await
                .is_err()
            {
                engine.task.abort();
            }
            let _ = tokio::fs::remove_dir_all(engine.directory).await;
        }
        Ok(())
    }
    pub async fn save_clip(&mut self, duration: Option<u64>) -> Result<PathBuf> {
        self.last_clip_error.clear();
        let seconds = duration.unwrap_or(self.cfg.recording.clip_duration.max(1) as u64);
        if seconds == 0 || seconds > self.cfg.recording.buffer_duration.max(1) as u64 {
            bail!("clip duration must be between 1 and the replay buffer duration");
        }
        let result = if self.capture.is_some() {
            self.save_gsr(seconds).await
        } else {
            self.save_segments(seconds).await
        };
        match result {
            Ok(path) => {
                if self.backend == "gpu-screen-recorder" {
                    apply_audio(&path, &self.cfg.recording).await;
                }
                if self.cfg.recording.apply_watermark {
                    apply_watermark(&path).await;
                }
                Ok(path)
            }
            Err(e) => {
                self.last_clip_error = e.to_string();
                Err(e)
            }
        }
    }
    async fn save_gsr(&mut self, seconds: u64) -> Result<PathBuf> {
        let out_dir = crate::runtime::resolve_path(&self.cfg.output.directory);
        let baseline = media_files(&out_dir);
        self.capture
            .as_mut()
            .context("the recorder is not running")?
            .signal_replay()
            .await?;
        self.last_save_signal_at = Some(Instant::now());
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let mut candidates: Vec<_> = media_files(&out_dir)
                .difference(&baseline)
                .filter(|n| replay_candidate(n))
                .map(|n| out_dir.join(n))
                .collect();
            candidates.sort_by_key(|p| {
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH)
            });
            if let Some(path) = candidates.pop() {
                wait_for_finalized_clip(&path, Duration::from_secs(20))
                    .await
                    .with_context(|| {
                        format!(
                            "{} was written but cannot be read; leaving the file in place",
                            path.display()
                        )
                    })?;
                let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
                let tag = if self.cfg.output.tag_clips_with_game {
                    self.game_tag.as_deref()
                } else {
                    None
                };
                let next = next_clip_path(&out_dir, ext, tag, &self.cfg.output.clip_name_template);
                tokio::fs::rename(&path, &next).await?;
                trim_to_last(&next, seconds).await?;
                return Ok(next);
            }
        }
        bail!("gpu-screen-recorder did not write a clip within 20 seconds")
    }
    async fn save_segments(&mut self, seconds: u64) -> Result<PathBuf> {
        let engine = self
            .segments
            .as_ref()
            .context("the recorder is not running")?;
        let end = Instant::now();
        self.last_save_signal_at = Some(end);
        let start = end.checked_sub(Duration::from_secs(seconds)).unwrap_or(end);
        let (tx, rx) = oneshot::channel();
        engine
            .tx
            .send(SegmentCommand::Flush(tx))
            .await
            .context("segment recorder stopped")?;
        let segments = tokio::time::timeout(Duration::from_secs(12), rx)
            .await
            .context("segment flush timed out")???;
        let relevant: Vec<_> = segments
            .into_iter()
            .filter(|s| s.started + Duration::from_secs(SEGMENT_SECONDS + 2) >= start)
            .collect();
        if relevant.is_empty() {
            bail!("no recorded segments available yet");
        }
        let extraction = engine
            .directory
            .join(format!("extract-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&extraction).await?;
        let result = async {
            let mut list = String::new();
            for (idx, segment) in relevant.iter().enumerate() {
                let dest = extraction.join(format!("part-{idx}.mp4"));
                if tokio::fs::hard_link(&segment.path, &dest).await.is_err() {
                    tokio::fs::copy(&segment.path, &dest).await?;
                }
                list.push_str(&format!(
                    "file '{}'\n",
                    dest.to_string_lossy().replace('\'', "'\\''")
                ));
            }
            let concat = extraction.join("concat.txt");
            tokio::fs::write(&concat, list).await?;
            let tag = if self.cfg.output.tag_clips_with_game {
                self.game_tag.as_deref()
            } else {
                None
            };
            let out = next_clip_path(
                &crate::runtime::resolve_path(&self.cfg.output.directory),
                "mp4",
                tag,
                &self.cfg.output.clip_name_template,
            );
            let skip = start
                .saturating_duration_since(relevant[0].started)
                .as_secs_f64();
            let cmd = vec![
                "ffmpeg".into(),
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-f".into(),
                "concat".into(),
                "-safe".into(),
                "0".into(),
                "-i".into(),
                concat.to_string_lossy().into_owned(),
                "-ss".into(),
                format!("{skip:.3}"),
                "-t".into(),
                seconds.to_string(),
                "-map".into(),
                "0:v?".into(),
                "-map".into(),
                "0:a?".into(),
                "-c".into(),
                "copy".into(),
                "-movflags".into(),
                "+faststart".into(),
                "-y".into(),
                out.to_string_lossy().into_owned(),
            ];
            checked_command(&cmd, 60).await?;
            verify_media(&out).await?;
            Ok(out)
        }
        .await;
        let _ = tokio::fs::remove_dir_all(extraction).await;
        result
    }
    pub async fn start_session(&mut self) -> Result<PathBuf> {
        if self.session.is_some() {
            bail!("a session is already active");
        }
        let out_dir = crate::runtime::resolve_path(&self.cfg.output.directory);
        tokio::fs::create_dir_all(&out_dir).await?;
        let ext = if self.backend == "gpu-screen-recorder" {
            container(&self.cfg.recording)
        } else {
            "mp4"
        };
        let path = next_numbered_path(&out_dir, "Vice_Session", ext, None, &["mp4", "mkv"]);
        let cmd = if self.backend == "gpu-screen-recorder" {
            let available =
                supported_codecs(&gsr_prefix(&self.cfg.recording.gsr_source).await?).await;
            self.build_gsr_cmd(
                Some(&path),
                false,
                choose_codec(&self.cfg.recording, &available, None).as_deref(),
            )
            .await?
        } else {
            segment_command(
                &self.cfg,
                self.display_override.as_deref(),
                &self.backend,
                &path,
            )
            .await?
        };
        let commands = if self.backend == "ffmpeg" && self.cfg.recording.encoder == "auto" {
            if let Some(template) = &self.segment_template {
                let selected = capture_output(template, &path);
                if selected == cmd {
                    vec![selected]
                } else {
                    vec![selected, cmd]
                }
            } else {
                crate::encoding::command_candidates(&cmd).await
            }
        } else {
            vec![cmd]
        };
        let (capture, _) = start_capture_candidates(commands).await?;
        self.session = Some(capture);
        self.session_path = Some(path.clone());
        self.session_start = Some(Instant::now());
        Ok(path)
    }
    pub async fn stop_session(&mut self) -> Result<PathBuf> {
        let mut capture = self.session.take().context("no active session")?;
        self.session_start = None;
        let path = self
            .session_path
            .take()
            .context("session output path is missing")?;
        capture.stop().await?;
        verify_media(&path).await?;
        if self.cfg.recording.apply_watermark {
            apply_watermark(&path).await;
        }
        Ok(path)
    }
}

pub fn filename_tag(game: Option<&str>) -> Option<String> {
    let mut tag = String::new();
    for c in game.unwrap_or("").chars() {
        if c.is_ascii_alphanumeric() {
            tag.push(c);
        } else if !tag.ends_with('-') && !tag.is_empty() {
            tag.push('-');
        }
    }
    let tag = tag.trim_matches('-').chars().take(48).collect::<String>();
    (!tag.is_empty()).then_some(tag)
}
fn sanitize_name(name: &str) -> String {
    let mut result = String::new();
    for c in name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
    {
        if matches!(c, '_' | '-') && result.ends_with(['_', '-']) {
            continue;
        }
        result.push(c);
    }
    result.trim_matches(['_', '-', ' ', '.']).to_string()
}
pub fn slugify_clip_name(name: &str) -> Option<String> {
    let mut name = name.trim().to_string();
    for ext in [".mp4", ".mkv", ".png", ".jpg", ".jpeg"] {
        if name.to_lowercase().ends_with(ext) {
            name.truncate(name.len() - ext.len());
            break;
        }
    }
    let mut slug = String::new();
    let mut whitespace = false;
    for c in name.chars() {
        if c.is_whitespace() {
            whitespace = true;
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '.' || c == '-' {
            if whitespace && !slug.is_empty() {
                slug.push('-');
            }
            whitespace = false;
            slug.push(c);
        }
    }
    let slug = sanitize_name(&slug);
    (!slug.is_empty()).then_some(slug)
}
pub fn next_numbered_path(
    directory: &Path,
    stem: &str,
    ext: &str,
    tag: Option<&str>,
    extensions: &[&str],
) -> PathBuf {
    let pattern = Regex::new(&format!(
        r"^{}_(\d+)(?:_.+)?\.(?:{})$",
        regex::escape(stem),
        extensions
            .iter()
            .map(|e| regex::escape(e))
            .collect::<Vec<_>>()
            .join("|")
    ))
    .expect("valid naming expression");
    let number = std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| pattern.captures(&n).and_then(|c| c[1].parse::<u64>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    let suffix = filename_tag(tag)
        .map(|s| format!("_{s}"))
        .unwrap_or_default();
    directory.join(format!("{stem}_{number}{suffix}.{ext}"))
}
fn template_pattern(template: &str) -> Option<Regex> {
    if !template.contains("$n") {
        return None;
    }
    let tokens = [
        ("$n", '\u{e000}'),
        ("$date", '\u{e001}'),
        ("$time", '\u{e002}'),
        ("$game", '\u{e003}'),
    ];
    let mut skeleton = template.to_string();
    for (token, marker) in tokens {
        skeleton = skeleton.replace(token, &marker.to_string());
    }
    let mut pattern = String::from("^[_-]*");
    let mut separator = false;
    for c in sanitize_name(&skeleton).chars() {
        if c == '_' || c == '-' {
            if !separator {
                pattern.push_str("[_-]*");
            }
            separator = true;
            continue;
        }
        separator = false;
        match c {
            '\u{e000}' => pattern.push_str("(\\d+)"),
            '\u{e001}' => pattern.push_str("\\d{4}-\\d{2}-\\d{2}"),
            '\u{e002}' => pattern.push_str("\\d{4}"),
            '\u{e003}' => pattern.push_str(".*?"),
            _ => pattern.push_str(&regex::escape(&c.to_string())),
        }
    }
    pattern.push_str(r"[_-]*\.(?:mp4|mkv)$");
    Regex::new(&pattern).ok()
}
pub fn next_clip_path(directory: &Path, ext: &str, tag: Option<&str>, template: &str) -> PathBuf {
    let tag = filename_tag(tag);
    if !template.trim().is_empty() {
        let number = if let Some(pattern) = template_pattern(template) {
            std::fs::read_dir(directory)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter_map(|n| pattern.captures(&n).and_then(|c| c[1].parse::<u64>().ok()))
                .max()
                .unwrap_or(0)
                + 1
        } else {
            1
        };
        let now = Local::now();
        let name = sanitize_name(
            &template
                .replace("$n", &number.to_string())
                .replace("$date", &now.format("%Y-%m-%d").to_string())
                .replace("$time", &now.format("%H%M").to_string())
                .replace("$game", tag.as_deref().unwrap_or("")),
        );
        if !name.is_empty() {
            let path = directory.join(format!("{name}.{ext}"));
            if !path.exists() {
                return path;
            }
            for i in 2..10000 {
                let path = directory.join(format!("{name}_{i}.{ext}"));
                if !path.exists() {
                    return path;
                }
            }
        }
    }
    next_numbered_path(directory, "Vice_Clip", ext, tag.as_deref(), &["mp4", "mkv"])
}
pub fn next_image_path(directory: &Path, tag: Option<&str>, ext: &str) -> PathBuf {
    next_numbered_path(directory, "Vice_Shot", ext, tag, &["png", "jpg", "jpeg"])
}
fn media_files(directory: &Path) -> HashSet<String> {
    std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|s| s == "mp4" || s == "mkv")
        })
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}
fn replay_candidate(name: &str) -> bool {
    !name.starts_with("Vice_Clip_")
        && !name.starts_with("Vice_Session_")
        && ![".trim.", ".trimming.", ".wm.", ".fix.", ".levels.", ".mix."]
            .iter()
            .any(|s| name.contains(s))
}
pub async fn wait_for_finalized_clip(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    let mut stable = 0;
    while Instant::now() < deadline {
        let identity = path.metadata().ok().map(|m| (m.len(), m.modified().ok()));
        if identity.is_some_and(|(len, _)| len > 0) && identity == last {
            stable += 1;
        } else {
            stable = 0;
        }
        if stable >= 2 && verify_media(path).await.is_ok() {
            return Ok(());
        }
        last = identity;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    verify_media(path).await
}
pub async fn verify_media(path: &Path) -> Result<()> {
    let meta = crate::media::probe_media(path).await?;
    if meta.duration <= 0. || meta.width == 0 || meta.height == 0 {
        bail!("media has no readable video duration");
    }
    Ok(())
}
async fn checked_command(args: &[String], seconds: u64) -> Result<()> {
    let out = output(args, seconds).await?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            args.first().map(String::as_str).unwrap_or("command"),
            combined(&out)
        );
    }
    Ok(())
}
fn temporary_media(path: &Path, label: &str) -> PathBuf {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    path.with_extension(format!("{label}.{}.{}", uuid::Uuid::new_v4(), ext))
}
pub async fn trim_to_last(path: &Path, seconds: u64) -> Result<()> {
    let total = crate::media::get_duration(path).await;
    if total <= seconds as f64 + 0.1 {
        return Ok(());
    }
    let temporary = temporary_media(path, "trim");
    let start = total - seconds as f64;
    let mut last = None;
    for reencode in [false, true] {
        let mut cmd = strings(&["ffmpeg", "-hide_banner", "-loglevel", "error", "-ss"]);
        cmd.push(format!("{start:.6}"));
        cmd.push("-i".into());
        cmd.push(path.to_string_lossy().into_owned());
        cmd.extend(["-t".into(), seconds.to_string()]);
        cmd.extend(strings(&["-map", "0:v?", "-map", "0:a?"]));
        if reencode {
            cmd.extend(strings(&[
                "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-c:a", "aac", "-b:a",
                "128k",
            ]));
        } else {
            cmd.extend(strings(&["-c", "copy", "-avoid_negative_ts", "make_zero"]));
        }
        if path.extension().is_some_and(|e| e == "mp4") {
            cmd.extend(strings(&["-movflags", "+faststart"]));
        }
        cmd.extend(["-y".into(), temporary.to_string_lossy().into_owned()]);
        let result = if reencode {
            crate::encoding::run_video_job(&cmd, Duration::from_secs(120)).await
        } else {
            checked_command(&cmd, 120).await
        };
        match result {
            Ok(()) => {
                let duration = crate::media::get_duration(&temporary).await;
                if verify_media(&temporary).await.is_ok()
                    && duration >= (seconds as f64 * 0.75).min(2.)
                    && duration <= seconds as f64 + 5.
                {
                    tokio::fs::rename(&temporary, path).await?;
                    return Ok(());
                }
                last = Some(anyhow!(
                    "trim result did not contain the requested video duration"
                ));
            }
            Err(e) => last = Some(e),
        }
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    // A failed destructive transformation keeps the original recording.
    tracing::warn!(
        "Keeping untrimmed recording {}: {}",
        path.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    );
    Ok(())
}
pub fn encoder_flags(encoder: &str, crf: i64, depth: &str) -> Vec<String> {
    let ten = depth == "10" && !matches!(encoder, "h264_nvenc" | "h264_vaapi" | "libx264");
    let mut flags = if encoder.ends_with("_nvenc") {
        let mut f = strings(&["-c:v", encoder, "-rc", "vbr", "-cq"]);
        f.push(crf.to_string());
        f.extend(strings(&["-preset", "p4", "-tune", "hq"]));
        f
    } else if encoder.ends_with("_vaapi") {
        let mut f = strings(&[
            "-vf",
            if ten {
                "format=p010,hwupload"
            } else {
                "format=nv12,hwupload"
            },
            "-c:v",
            encoder,
            "-qp",
        ]);
        f.push(crf.to_string());
        f
    } else if encoder == "copy" {
        strings(&["-c:v", "copy"])
    } else {
        let mut f = strings(&["-c:v", encoder, "-crf"]);
        f.push(crf.to_string());
        f.extend(strings(&["-preset", "fast"]));
        f
    };
    if ten && !encoder.ends_with("_vaapi") {
        flags.extend(strings(&[
            "-pix_fmt",
            if encoder.ends_with("_nvenc") {
                "p010le"
            } else {
                "yuv420p10le"
            },
        ]));
    }
    flags
}
pub async fn choose_encoder(preferred: &str) -> String {
    if preferred != "auto" {
        return preferred.into();
    }
    crate::encoding::video_candidates()
        .await
        .into_iter()
        .find(|candidate| matches!(candidate.name(), "h264_nvenc" | "h264_vaapi"))
        .map(|candidate| candidate.name().to_owned())
        .unwrap_or_else(|| "libx264".into())
}
async fn pulse_source(microphone: bool, preferred: &str) -> String {
    let preferred = preferred.strip_prefix("device:").unwrap_or(preferred);
    if !preferred.is_empty()
        && preferred != "default"
        && preferred != "default_input"
        && preferred != "default_output"
    {
        return preferred.into();
    }
    let verb = if microphone {
        "get-default-source"
    } else {
        "get-default-sink"
    };
    if let Ok(out) = output(&strings(&["pactl", verb]), 3).await {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if out.status.success() && !name.is_empty() {
            return if microphone {
                name
            } else {
                format!("{name}.monitor")
            };
        }
    }
    if let Ok(out) = output(&strings(&["pactl", "list", "short", "sources"]), 3).await {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some(name) = line.split_whitespace().nth(1)
                && name.ends_with(".monitor") != microphone
            {
                return name.into();
            }
        }
    }
    "default".into()
}
async fn window_geometry(window: &str) -> Option<(u32, u32)> {
    let args = host_prefix(vec![
        "xdotool".into(),
        "getwindowgeometry".into(),
        "--shell".into(),
        window.into(),
    ]);
    let out = output(&args, 3).await.ok()?;
    if !out.status.success() {
        return None;
    }
    let mut width = 0;
    let mut height = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(value) = line.strip_prefix("WIDTH=") {
            width = value.parse().ok()?;
        }
        if let Some(value) = line.strip_prefix("HEIGHT=") {
            height = value.parse().ok()?;
        }
    }
    (width > 0 && height > 0).then_some((width, height))
}
async fn segment_command(
    cfg: &Config,
    display: Option<&str>,
    backend: &str,
    path: &Path,
) -> Result<Vec<String>> {
    let rc = &cfg.recording;
    // Build an ordinary CPU command for auto FFmpeg capture; the shared policy
    // rewrites the complete graph with the probed GPU's actual device path.
    let encoder = if backend == "ffmpeg" && rc.encoder == "auto" {
        "libx264".into()
    } else {
        choose_encoder(&rc.encoder).await
    };
    if backend == "wf-recorder" {
        let mut cmd = strings(&["wf-recorder", "-f"]);
        cmd.push(path.to_string_lossy().into_owned());
        if let Some(display) = display.or(rc.display.as_deref()).filter(|s| !s.is_empty()) {
            cmd.extend(["-o".into(), display.into()]);
        }
        if rc.capture_microphone && rc.wf_microphone_strategy == "mic_only" {
            cmd.push(format!(
                "--audio={}",
                pulse_source(true, &rc.microphone_source).await
            ));
        } else if rc.capture_audio {
            cmd.push(format!(
                "--audio={}",
                pulse_source(false, &rc.audio_sink).await
            ));
        }
        cmd.extend(strings(&[
            "-c",
            if matches!(encoder.as_str(), "h264_nvenc" | "hevc_nvenc" | "h264_vaapi") {
                &encoder
            } else {
                "libx264"
            },
        ]));
        if encoder == "h264_vaapi" {
            cmd.extend(strings(&["-d", "/dev/dri/renderD128"]));
        }
        return Ok(cmd);
    }
    let mut cmd = strings(&["ffmpeg", "-hide_banner", "-loglevel", "warning"]);
    if encoder.ends_with("_vaapi") {
        cmd.extend(strings(&["-vaapi_device", "/dev/dri/renderD128"]));
    }
    cmd.extend(strings(&["-f", "x11grab", "-framerate"]));
    cmd.push(rc.fps.to_string());
    let mut input = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    let mut scale = None;
    let displays = xrandr_displays().await;
    let selected = display
        .or(rc.display.as_deref())
        .and_then(|id| displays.iter().find(|v| v["id"].as_str() == Some(id)));
    let resolution = if let Some(selected) = selected {
        input.push_str(&format!(
            "+{},{}",
            selected["x"].as_i64().unwrap_or(0),
            selected["y"].as_i64().unwrap_or(0)
        ));
        scale = rc.resolution.clone().filter(|s| valid_resolution(s));
        Some(format!(
            "{}x{}",
            selected["width"].as_u64().unwrap_or(1920),
            selected["height"].as_u64().unwrap_or(1080)
        ))
    } else if rc.resolution.as_ref().is_some_and(|s| valid_resolution(s)) {
        rc.resolution.clone()
    } else {
        output(&strings(&["xdpyinfo"]), 3)
            .await
            .ok()
            .and_then(|out| {
                Regex::new(r"dimensions:\s*(\d+x\d+)")
                    .ok()?
                    .captures(&combined(&out))
                    .map(|m| m[1].to_string())
            })
    };
    if let Some(resolution) = resolution {
        cmd.extend(["-video_size".into(), resolution]);
    }
    cmd.extend(["-i".into(), input]);
    let mut input_count = 0;
    if rc.capture_audio {
        cmd.extend(strings(&["-thread_queue_size", "512", "-f", "pulse", "-i"]));
        cmd.push(pulse_source(false, &rc.audio_sink).await);
        input_count += 1;
    }
    if rc.capture_microphone {
        cmd.extend(strings(&["-thread_queue_size", "512", "-f", "pulse", "-i"]));
        cmd.push(pulse_source(true, &rc.microphone_source).await);
        input_count += 1;
    }
    let mut flags = encoder_flags(&encoder, rc.crf, &rc.color_depth);
    if let Some(res) = scale {
        let filter = format!("scale={}", res.replace('x', ":"));
        if let Some(pos) = flags.iter().position(|s| s == "-vf") {
            flags[pos + 1] = format!("{filter},{}", flags[pos + 1]);
        } else {
            flags.extend(["-vf".into(), filter]);
        }
    }
    cmd.extend(flags);
    if input_count == 2 {
        let mic = if rc.microphone_mono {
            "pan=mono|c0=0.5*c0+0.5*c1,"
        } else {
            ""
        };
        let graph = format!(
            "[1:a]volume={}[desktop];[2:a]{mic}volume={}[mic];[desktop][mic]amix=inputs=2:normalize=0[audio]",
            rc.desktop_volume.clamp(0., 2.),
            rc.microphone_volume.clamp(0., 2.)
        );
        cmd.extend(["-filter_complex".into(), graph]);
        cmd.extend(strings(&["-map", "0:v:0", "-map", "[audio]"]));
    } else if input_count == 1 {
        let volume = if rc.capture_microphone {
            rc.microphone_volume
        } else {
            rc.desktop_volume
        };
        if (volume - 1.).abs() > 0.01 {
            cmd.extend(["-af".into(), format!("volume={}", volume.clamp(0., 2.))]);
        }
        cmd.extend(strings(&["-map", "0:v:0", "-map", "1:a:0"]));
    }
    if input_count > 0 {
        cmd.extend(strings(&["-c:a", "aac", "-b:a", "128k"]));
    }
    cmd.extend(["-y".into(), path.to_string_lossy().into_owned()]);
    Ok(cmd)
}
async fn apply_audio(path: &Path, rc: &RecordingConfig) {
    let (layout, mixed) = source_layout(rc);
    let separate = !layout.is_empty();
    if !separate && !save_audio_pass(rc) {
        return;
    }
    let meta = match crate::media::probe_media(path).await {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.audio_streams == 0 {
        return;
    }
    let mut graph = vec![];
    let mut maps = vec![];
    if separate {
        if meta.audio_streams != layout.len() {
            return;
        }
        let mic = mic_source(rc);
        let volumes: Vec<_> = layout
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if mixed && i == 0 {
                    1.
                } else {
                    rc.audio_track_volumes
                        .get(t)
                        .copied()
                        .unwrap_or_else(|| if t == &mic { rc.microphone_volume } else { 1. })
                        .clamp(0., 2.)
                }
            })
            .collect();
        if volumes.iter().all(|v| (*v - 1.).abs() <= 0.01) {
            return;
        }
        let monitor = layout
            .iter()
            .skip(if mixed { 1 } else { 0 })
            .flat_map(|t| t.split('|'))
            .any(|p| source_kind(p) == "monitor");
        let members: Vec<_> = if mixed {
            (1..layout.len())
                .filter(|i| !(monitor && layout[*i].split('|').all(|p| source_kind(p) == "app")))
                .collect()
        } else {
            vec![]
        };
        for (index, volume) in volumes.iter().enumerate() {
            if mixed && index == 0 {
                continue;
            }
            let mut filter = format!("[0:a:{index}]aformat=channel_layouts=stereo,volume={volume}");
            if members.contains(&index) {
                filter.push_str(&format!(",asplit=2[v{index}][m{index}]"));
            } else {
                filter.push_str(&format!("[v{index}]"));
            }
            graph.push(filter);
        }
        if mixed && !members.is_empty() {
            graph.push(format!(
                "{}amix=inputs={}:normalize=0[mix]",
                members
                    .iter()
                    .map(|i| format!("[m{i}]"))
                    .collect::<String>(),
                members.len()
            ));
        }
        for i in 0..layout.len() {
            maps.push(if mixed && i == 0 {
                if members.is_empty() {
                    "0:a:0".into()
                } else {
                    "[mix]".into()
                }
            } else {
                format!("[v{i}]")
            });
        }
    } else if rc.capture_audio && rc.capture_microphone && meta.audio_streams >= 2 {
        let mono = if rc.microphone_mono {
            "pan=mono|c0=0.5*c0+0.5*c1,"
        } else {
            ""
        };
        graph.push(format!("[0:a:0]aformat=channel_layouts=stereo,volume={}[desktop];[0:a:1]{mono}aformat=channel_layouts=stereo,volume={}[mic];[desktop][mic]amix=inputs=2:normalize=0[mix]",rc.desktop_volume.clamp(0.,2.),rc.microphone_volume.clamp(0.,2.)));
        maps.push("[mix]".into());
    } else {
        let mic_only = rc.capture_microphone && !rc.capture_audio;
        let mono = if mic_only && rc.microphone_mono {
            "pan=mono|c0=0.5*c0+0.5*c1,"
        } else {
            ""
        };
        graph.push(format!(
            "[0:a:0]{mono}volume={}[mix]",
            if mic_only {
                rc.microphone_volume.clamp(0., 2.)
            } else {
                rc.desktop_volume.clamp(0., 2.)
            }
        ));
        maps.push("[mix]".into());
    }
    let temporary = temporary_media(path, "levels");
    let mut cmd = strings(&["ffmpeg", "-hide_banner", "-loglevel", "error", "-i"]);
    cmd.push(path.to_string_lossy().into_owned());
    cmd.extend(["-filter_complex".into(), graph.join(";")]);
    cmd.extend(strings(&["-map", "0:v?"]));
    for map in maps {
        cmd.extend(["-map".into(), map]);
    }
    cmd.extend(strings(&["-c:v", "copy", "-c:a", "aac", "-b:a", "192k"]));
    if path.extension().is_some_and(|e| e == "mp4") {
        cmd.extend(strings(&["-movflags", "+faststart"]));
    }
    cmd.extend(["-y".into(), temporary.to_string_lossy().into_owned()]);
    if checked_command(&cmd, 120).await.is_ok() && verify_media(&temporary).await.is_ok() {
        let _ = tokio::fs::rename(&temporary, path).await;
    } else {
        tracing::warn!(
            "Audio level adjustment failed; keeping {} as recorded",
            path.display()
        );
    }
    let _ = tokio::fs::remove_file(temporary).await;
}
async fn apply_watermark(path: &Path) {
    let temporary = temporary_media(path, "wm");
    let mut cmd = strings(&["ffmpeg", "-hide_banner", "-loglevel", "error", "-i"]);
    cmd.push(path.to_string_lossy().into_owned());
    cmd.extend(strings(&["-map","0:v:0?","-map","0:a?","-vf","drawtext=text='Clipped with Vice':fontcolor=white@0.7:fontsize=24:x=w-tw-24:y=h-th-24:shadowcolor=black@0.5:shadowx=1:shadowy=1","-c:v","libx264","-preset","veryfast","-crf","23","-c:a","copy"]));
    if path.extension().is_some_and(|e| e == "mp4") {
        cmd.extend(strings(&["-movflags", "+faststart"]));
    }
    cmd.extend(["-y".into(), temporary.to_string_lossy().into_owned()]);
    if crate::encoding::run_video_job(&cmd, Duration::from_secs(180))
        .await
        .is_ok()
        && verify_media(&temporary).await.is_ok()
    {
        let _ = tokio::fs::rename(&temporary, path).await;
    }
    let _ = tokio::fs::remove_file(temporary).await;
}

pub fn parse_gsr_displays(raw: &str) -> Vec<Value> {
    let generic = [
        "window",
        "focused",
        "screen",
        "screen-direct-force",
        "portal",
    ];
    let quoted = Regex::new(r#"^"([^"]+)"\s*(.*)$"#).unwrap();
    let mut displays = vec![];
    for line in raw.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let line = line.trim_start_matches(['-', '*', '•', ' ']);
        let lower = line.to_lowercase();
        if lower.starts_with("gsr error")
            || lower.starts_with("error:")
            || lower.starts_with("monitor")
            || lower.contains("for_each_active_monitor")
            || lower.contains("failed to open")
            || generic.contains(&lower.as_str())
        {
            continue;
        }
        let (id, label) = if let Some((head, tail)) = line.split_once('|') {
            let id = head.trim().trim_matches('"').trim();
            (
                id.to_owned(),
                if tail.trim().is_empty() {
                    id.into()
                } else {
                    format!("{id} ({})", tail.trim())
                },
            )
        } else if let Some(capture) = quoted.captures(line) {
            let id = capture[1].trim();
            (
                id.into(),
                format!("{id} {}", capture[2].trim()).trim().into(),
            )
        } else if let Some((head, _)) = line.split_once(':').filter(|(h, _)| !h.contains(' ')) {
            (head.trim().trim_matches('"').into(), line.into())
        } else {
            let id = line
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches('"');
            (id.into(), id.into())
        };
        if !id.is_empty()
            && !generic.contains(&id.to_lowercase().as_str())
            && !displays.iter().any(|v: &Value| v["id"] == id)
        {
            displays.push(json!({"id":id,"label":label}));
        }
    }
    displays
}
pub fn parse_xrandr_displays(raw: &str) -> Vec<Value> {
    let pattern =
        Regex::new(r"^\d+:\s+([+*]*)(\S+)\s+(\d+)/\d+x(\d+)/\d+\+(-?\d+)\+(-?\d+)\s+(\S+)$")
            .unwrap();
    raw.lines().filter_map(|line| { let c = pattern.captures(line.trim())?;
        Some(json!({"id":&c[7],"label":format!("{} ({}x{})",&c[7],&c[3],&c[4]),"width":c[3].parse::<u32>().ok()?,"height":c[4].parse::<u32>().ok()?,"x":c[5].parse::<i32>().ok()?,"y":c[6].parse::<i32>().ok()?,"primary":c[1].contains('*')}))
    }).collect()
}
async fn xrandr_displays() -> Vec<Value> {
    output(&strings(&["xrandr", "--listmonitors"]), 3)
        .await
        .map(|o| parse_xrandr_displays(&combined(&o)))
        .unwrap_or_default()
}
pub async fn list_displays(cfg: &Config) -> Value {
    let backend = if cfg.recording.backend == "auto" {
        match select_backend(
            "auto",
            BackendAvailability {
                gsr: gsr_source(&cfg.recording.gsr_source).await.is_some(),
                wayland: wayland(),
                x11: x11(),
                wf: has_program("wf-recorder"),
                ffmpeg: has_program("ffmpeg"),
            },
        ) {
            Ok(backend) => backend,
            Err(error) => {
                return json!({"backend":"auto","displays":[],"warning":error.to_string()});
            }
        }
    } else if cfg.recording.backend == "gsr" {
        "gsr"
    } else {
        &cfg.recording.backend
    };
    let mut displays = vec![];
    let mut warning = None;
    if backend == "gsr" {
        match gsr_prefix(&cfg.recording.gsr_source).await {
            Ok(prefix) => {
                for flag in ["--list-monitors", "--list-capture-options"] {
                    let mut cmd = prefix.clone();
                    cmd.push(flag.into());
                    if let Ok(out) = output(&cmd, 3).await
                        && out.status.success()
                    {
                        displays = parse_gsr_displays(&combined(&out));
                        if !displays.is_empty() {
                            break;
                        }
                    }
                }
            }
            Err(e) => warning = Some(e.to_string()),
        }
    } else if backend == "wf-recorder" {
        if let Ok(out) = output(&strings(&["wf-recorder", "--list-outputs"]), 3).await {
            for line in combined(&out)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
            {
                let line = line.trim_start_matches(['-', '*', '•', ' ']);
                let id = line.split([' ', ':']).next().unwrap_or("");
                if !id.is_empty() {
                    displays.push(json!({"id":id,"label":line}));
                }
            }
        }
    } else {
        displays = xrandr_displays().await;
    }
    json!({"backend":backend,"displays":displays,"warning":warning})
}
pub async fn list_audio_sources() -> Value {
    list_audio_sources_for("auto").await
}
pub async fn list_audio_sources_for(preference: &str) -> Value {
    let mut sources = vec![
        json!({"id":"default_output","label":"Default output","kind":"monitor"}),
        json!({"id":"default_input","label":"Default input","kind":"input"}),
    ];
    let mut warning = None;
    match gsr_prefix(preference).await {
        Ok(prefix) => {
            for (flag, kind) in [
                ("--list-audio-devices", "device"),
                ("--list-application-audio", "app"),
            ] {
                let mut cmd = prefix.clone();
                cmd.push(flag.into());
                match output(&cmd, 3).await {
                    Ok(out) if out.status.success() => {
                        for line in combined(&out)
                            .lines()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            let (name, description) = line.split_once('|').unwrap_or((line, ""));
                            let name = name.trim();
                            if name == "default_input"
                                || name == "default_output"
                                || name.starts_with("gsr ")
                                || name.starts_with("error:")
                            {
                                continue;
                            }
                            let id = if name.starts_with("device:")
                                || name.starts_with("app:")
                                || name.starts_with("app-inverse:")
                            {
                                name.to_owned()
                            } else {
                                format!("{kind}:{name}")
                            };
                            let value = if description.trim().is_empty() {
                                id.split_once(':').map(|(_, s)| s).unwrap_or(&id)
                            } else {
                                description.trim()
                            };
                            if !sources.iter().any(|s| s["id"] == id) {
                                sources.push(json!({"id":id,"label":format!("{}: {value}",if kind == "app" { "Application" } else { "Device" }),"kind":source_kind(&id)}));
                            }
                            if kind == "app" {
                                let name = id.split_once(':').map(|(_, s)| s).unwrap_or(name);
                                sources.push(json!({"id":format!("app-inverse:{name}"),"label":format!("All except: {name}"),"kind":"app"}));
                            }
                        }
                    }
                    Ok(out) => {
                        if warning.is_none() {
                            warning = combined(&out).lines().last().map(str::to_owned);
                        }
                    }
                    Err(e) => {
                        if warning.is_none() {
                            warning = Some(e.to_string());
                        }
                    }
                }
            }
        }
        Err(e) => warning = Some(e.to_string()),
    }
    json!({"sources":sources,"warning":warning})
}
static SCREENSHOT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub async fn capture_screenshot(cfg: &Config) -> Result<PathBuf> {
    let _guard = SCREENSHOT_LOCK.lock().await;
    let directory = crate::runtime::resolve_path(&cfg.output.image_directory);
    tokio::fs::create_dir_all(&directory).await?;
    let tag = if cfg.output.tag_clips_with_game {
        crate::active_window::get_active_window()
            .await
            .and_then(|window| crate::active_window::match_game(&window, &cfg.discord.custom_games))
    } else {
        None
    };
    let path = next_image_path(&directory, tag.as_deref(), "png");
    let mut cmd = gsr_prefix(&cfg.recording.gsr_source).await?;
    cmd.extend([
        "-w".into(),
        cfg.recording.display.as_deref().unwrap_or("screen").into(),
        "-o".into(),
        path.to_string_lossy().into_owned(),
    ]);
    checked_command(&cmd, 20).await?;
    if !path.metadata().is_ok_and(|m| m.len() > 0) {
        let _ = tokio::fs::remove_file(&path).await;
        bail!("gpu-screen-recorder wrote no image");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auto_backend_fallback_matches_session_and_preserves_explicit_selection() {
        let wayland = BackendAvailability {
            gsr: false,
            wayland: true,
            x11: false,
            wf: true,
            ffmpeg: true,
        };
        assert_eq!(select_backend("auto", wayland).unwrap(), "wf-recorder");
        assert!(
            select_backend("ffmpeg", wayland)
                .unwrap_err()
                .to_string()
                .contains("X11")
        );
        assert!(
            select_backend("gsr", wayland)
                .unwrap_err()
                .to_string()
                .contains("not installed")
        );
        assert!(
            select_backend(
                "auto",
                BackendAvailability {
                    wf: false,
                    ..wayland
                }
            )
            .is_err()
        );
        let x11 = BackendAvailability {
            wayland: false,
            x11: true,
            ..wayland
        };
        assert_eq!(select_backend("auto", x11).unwrap(), "ffmpeg");
        assert!(select_backend("wf-recorder", x11).is_err());
        assert!(
            select_backend(
                "auto",
                BackendAvailability {
                    ffmpeg: false,
                    ..x11
                }
            )
            .is_err()
        );
        assert_eq!(
            select_backend(
                "auto",
                BackendAvailability {
                    gsr: true,
                    ..wayland
                }
            )
            .unwrap(),
            "gsr"
        );
        assert_eq!(
            select_backend("auto", BackendAvailability { gsr: true, ..x11 }).unwrap(),
            "gsr"
        );
        assert_eq!(
            select_backend("ffmpeg", BackendAvailability { gsr: true, ..x11 }).unwrap(),
            "ffmpeg"
        );
        assert!(select_backend("invalid", x11).is_err());
    }
    fn temp_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("vice-recorder-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn numbering_spans_containers_and_tags() {
        let dir = temp_dir();
        std::fs::write(dir.join("Vice_Clip_4_Game.mp4"), b"x").unwrap();
        std::fs::write(dir.join("Vice_Clip_9.mkv"), b"x").unwrap();
        assert_eq!(
            next_clip_path(&dir, "mp4", Some("Overwatch 2"), "")
                .file_name()
                .unwrap(),
            "Vice_Clip_10_Overwatch-2.mp4"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn templated_numbers_survive_dates_games() {
        let dir = temp_dir();
        std::fs::write(dir.join("clip_2025-01-01_1230_7_Game.mkv"), b"x").unwrap();
        let path = next_clip_path(&dir, "mp4", Some("New Game"), "clip_$date_$time_$n_$game");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains("_8_New-Game")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn safe_names_and_candidate_filtering() {
        assert_eq!(
            slugify_clip_name(" ../My cool #clip?.mp4 ").as_deref(),
            Some("My-cool-clip")
        );
        assert_eq!(
            filename_tag(Some("Counter-Strike: Global Offensive")).as_deref(),
            Some("Counter-Strike-Global-Offensive")
        );
        assert!(!replay_candidate("Vice_Session_10.mp4"));
        assert!(!replay_candidate("game.trim.mp4"));
        assert!(replay_candidate("2026-01-01_12-00.mp4"));
        assert_eq!(
            sanitize_gsr_args(
                &strings(&["-o", "/bad", "-r=10", "-k", "hevc"]),
                &["-o", "-r"]
            ),
            strings(&["-k", "hevc"])
        );
    }
    #[test]
    fn audio_toggle_preserves_microphone_and_deduplicates_mix() {
        let mut rc = RecordingConfig {
            capture_audio: false,
            capture_microphone: true,
            ..Default::default()
        };
        assert_eq!(gsr_audio_args(&rc, true), strings(&["-a", "default_input"]));
        rc.capture_audio = true;
        rc.audio_tracks = vec!["default_output".into(), "app:Discord".into()];
        rc.audio_tracks_mix_first = true;
        let args = gsr_audio_args(&rc, true);
        assert_eq!(args[1], "default_output|default_input");
        assert_eq!(args.len(), 8);
    }
    #[test]
    fn codecs_keep_vulkan_and_depth() {
        assert_eq!(
            codec_for_encoder("hevc_vulkan", "10").as_deref(),
            Some("hevc_10bit_vulkan")
        );
        assert_eq!(
            codec_for_encoder("h264_nvenc", "10").as_deref(),
            Some("h264")
        );
        let rc = RecordingConfig::default();
        let codecs = HashSet::from(["h264_vulkan".into()]);
        assert_eq!(
            choose_codec(&rc, &codecs, None).as_deref(),
            Some("h264_vulkan")
        );
    }
    #[test]
    fn monitor_parsing_filters_diagnostics() {
        let monitors = parse_gsr_displays(
            "screen\ngsr error: failed to open device\n\"DP-1\" 1920x1080\nHDMI-A-1|2560x1440\n",
        );
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0]["id"], "DP-1");
        let monitors = parse_xrandr_displays("Monitors: 1\n 0: +*DP-1 1920/520x1080/290+0+0  DP-1");
        assert_eq!(monitors[0]["width"], 1920);
        assert_eq!(monitors[0]["primary"], true);
    }
    #[tokio::test]
    async fn process_shutdown_reaps_child() {
        let mut capture = Capture::spawn(strings(&["sleep", "30"])).await.unwrap();
        let pid = capture.pid;
        assert!(capture.healthy());
        capture.stop().await.unwrap();
        assert!(!capture.healthy());
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0);
    }
    async fn fixture(path: &Path, duration: u64) {
        let mut args = strings(&[
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-t",
        ]);
        args.push(duration.to_string());
        args.extend(strings(&[
            "-map", "0:v:0", "-map", "1:a:0", "-map", "2:a:0", "-c:v", "libx264", "-threads", "1",
            "-pix_fmt", "yuv420p", "-g", "10", "-c:a", "aac", "-y",
        ]));
        args.push(path.to_string_lossy().into_owned());
        checked_command(&args, 30).await.unwrap();
    }
    #[tokio::test]
    async fn real_multitrack_trim_preserves_video_and_audio() {
        if !has_program("ffmpeg") || !has_program("ffprobe") {
            return;
        }
        let dir = temp_dir();
        let path = dir.join("recording.mp4");
        fixture(&path, 3).await;
        wait_for_finalized_clip(&path, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            crate::media::probe_media(&path)
                .await
                .unwrap()
                .audio_streams,
            2
        );
        trim_to_last(&path, 1).await.unwrap();
        let trimmed = crate::media::probe_media(&path).await.unwrap();
        assert_eq!(trimmed.audio_streams, 2);
        assert!(
            trimmed.duration >= 0.8 && trimmed.duration <= 2.1,
            "{}",
            trimmed.duration
        );
        assert_eq!((trimmed.width, trimmed.height), (160, 90));
        let mut rc = RecordingConfig {
            audio_tracks: vec!["default_output".into(), "default_input".into()],
            ..Default::default()
        };
        rc.audio_track_volumes.insert("default_input".into(), 0.25);
        apply_audio(&path, &rc).await;
        let leveled = crate::media::probe_media(&path).await.unwrap();
        assert_eq!(leveled.audio_streams, 2);
        assert_eq!((leveled.width, leveled.height), (160, 90));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn real_segment_extraction_handles_multiple_audio_tracks() {
        if !has_program("ffmpeg") || !has_program("ffprobe") {
            return;
        }
        let dir = temp_dir();
        let first = dir.join("first.mp4");
        let second = dir.join("second.mp4");
        fixture(&first, 2).await;
        fixture(&second, 2).await;
        let now = Instant::now();
        let parts = vec![
            Segment {
                started: now - Duration::from_secs(4),
                path: first,
            },
            Segment {
                started: now - Duration::from_secs(2),
                path: second,
            },
        ];
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            if let Some(SegmentCommand::Flush(reply)) = rx.recv().await {
                let _ = reply.send(Ok(parts));
            }
        });
        let mut cfg = Config::default();
        cfg.output.directory = dir.to_string_lossy().into_owned();
        let mut recorder = Recorder::new(cfg);
        recorder.segments = Some(SegmentEngine {
            tx,
            task,
            healthy: Arc::new(AtomicBool::new(true)),
            error: Arc::new(Mutex::new(String::new())),
            directory: dir.clone(),
        });
        let path = recorder.save_clip(Some(3)).await.unwrap();
        let meta = crate::media::probe_media(&path).await.unwrap();
        assert_eq!(meta.audio_streams, 2);
        assert!(
            meta.duration >= 2.7 && meta.duration <= 4.1,
            "{}",
            meta.duration
        );
        assert_eq!((meta.width, meta.height), (160, 90));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn replay_signal_waits_for_media_and_respects_tag_setting() {
        if !has_program("ffmpeg") || !has_program("ffprobe") {
            return;
        }
        let dir = temp_dir();
        let source = dir.join("source.mp4");
        fixture(&source, 2).await;
        let out = dir.join("clips");
        std::fs::create_dir_all(&out).unwrap();
        let script = dir.join("capture.sh");
        std::fs::write(&script,"fixture=$1\nout_dir=$2\ntrap 'cp \"$fixture\" \"$out_dir/2026-replay.mp4\"' USR1\ntrap 'exit 0' INT TERM\nwhile :; do sleep 0.05 & wait $!; done\n").unwrap();
        let args = vec![
            "sh".into(),
            script.to_string_lossy().into_owned(),
            source.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ];
        let mut cfg = Config::default();
        cfg.output.directory = out.to_string_lossy().into_owned();
        cfg.output.tag_clips_with_game = false;
        let mut recorder = Recorder::new(cfg);
        recorder.game_tag = Some("Example Game".into());
        let mut capture = Capture::spawn(args).await.unwrap();
        capture.startup_check().await.unwrap();
        recorder.capture = Some(capture);
        let path = recorder.save_clip(Some(1)).await.unwrap();
        assert_eq!(path.file_name().unwrap(), "Vice_Clip_1.mp4");
        assert_eq!(
            crate::media::probe_media(&path)
                .await
                .unwrap()
                .audio_streams,
            2
        );
        assert!(recorder.last_save_signal_at.is_some());
        recorder.stop().await.unwrap();
        assert!(!recorder.is_healthy());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn session_stop_finalizes_container_and_clears_state() {
        if !has_program("ffmpeg") || !has_program("ffprobe") {
            return;
        }
        let dir = temp_dir();
        let path = dir.join("Vice_Session_1.mp4");
        let mut args = strings(&[
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x90:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-y",
        ]);
        args.push(path.to_string_lossy().into_owned());
        let mut capture = Capture::spawn(args).await.unwrap();
        capture.startup_check().await.unwrap();
        let mut rec = Recorder::new(Config::default());
        rec.session = Some(capture);
        rec.session_path = Some(path.clone());
        rec.session_start = Some(Instant::now() - Duration::from_secs(1));
        assert!(rec.session_active());
        assert!(rec.session_elapsed() >= 1.);
        let saved = rec.stop_session().await.unwrap();
        assert_eq!(saved, path);
        assert!(!rec.session_active());
        assert_eq!(rec.session_elapsed(), 0.);
        let meta = crate::media::probe_media(&saved).await.unwrap();
        assert!(meta.duration > 0.);
        assert_eq!(meta.audio_streams, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn capture_startup_failure_uses_and_retains_cpu_template() {
        let directory = temp_dir();
        let script = directory.join("capture.sh");
        std::fs::write(&script,"if [ \"$1\" = gpu ]; then echo 'encoder is busy' >&2; exit 1; fi\ntrap 'exit 0' INT TERM\nwhile :; do sleep 1 & wait $!; done\n").unwrap();
        let gpu = vec![
            "sh".into(),
            script.to_string_lossy().into_owned(),
            "gpu".into(),
            "first.mp4".into(),
        ];
        let cpu = vec![
            "sh".into(),
            script.to_string_lossy().into_owned(),
            "cpu".into(),
            "first.mp4".into(),
        ];
        let (mut capture, template) = start_capture_candidates(vec![gpu, cpu.clone()])
            .await
            .unwrap();
        assert_eq!(template, cpu);
        assert!(capture.healthy());
        let rotated = capture_output(&template, Path::new("second.mp4"));
        assert_eq!(rotated[2], "cpu");
        assert_eq!(rotated.last().unwrap(), "second.mp4");
        capture.stop().await.unwrap();
        let wf = strings(&["wf-recorder", "-f", "first.mp4", "-c", "h264_vaapi"]);
        assert_eq!(
            capture_output(&wf, Path::new("session.mp4")),
            strings(&["wf-recorder", "-f", "session.mp4", "-c", "h264_vaapi"])
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
