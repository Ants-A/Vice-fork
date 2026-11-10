//! GPU-first FFmpeg encoding selected by actual synthetic encode probes.
//!
//! Decode and CPU filters stay unchanged. Hardware frames are uploaded only
//! after the final video filter, including complex editor graphs. Each codec
//! family is probed once; a failed real job still retries its original CPU
//! command because a working small probe cannot guarantee every resolution.
use anyhow::{Context, Result, bail};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, sync::OnceCell};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    H264,
    Hevc,
    Av1,
    Vp8,
    Vp9,
}
impl Family {
    fn prefix(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
        }
    }
    fn software(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::Hevc => "libx265",
            Self::Av1 => "libsvtav1",
            Self::Vp8 => "libvpx",
            Self::Vp9 => "libvpx-vp9",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Hardware {
    Cpu,
    Nvenc,
    Vaapi(String),
    Qsv(String),
    Vulkan(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoEncoder {
    name: String,
    hardware: Hardware,
    family: Family,
}
impl VideoEncoder {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_hardware(&self) -> bool {
        self.hardware != Hardware::Cpu
    }
    fn cpu(family: Family) -> Self {
        Self {
            name: family.software().into(),
            hardware: Hardware::Cpu,
            family,
        }
    }
}
static H264: OnceCell<Vec<VideoEncoder>> = OnceCell::const_new();
static HEVC: OnceCell<Vec<VideoEncoder>> = OnceCell::const_new();
static AV1: OnceCell<Vec<VideoEncoder>> = OnceCell::const_new();
static VP8: OnceCell<Vec<VideoEncoder>> = OnceCell::const_new();
static VP9: OnceCell<Vec<VideoEncoder>> = OnceCell::const_new();
static ENCODERS: OnceCell<HashSet<String>> = OnceCell::const_new();
static DECODERS: OnceCell<Vec<Hardware>> = OnceCell::const_new();

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).into()).collect()
}
fn encoder_position(args: &[String]) -> Option<usize> {
    args.iter()
        .position(|a| matches!(a.as_str(), "-c:v" | "-c:v:0" | "-codec:v" | "-vcodec"))
}
pub fn video_encoder(args: &[String]) -> Option<&str> {
    args.get(encoder_position(args)? + 1).map(String::as_str)
}
fn family_for(codec: &str) -> Option<Family> {
    match codec {
        "libx264" => Some(Family::H264),
        "libx265" => Some(Family::Hevc),
        "libsvtav1" | "libaom-av1" | "librav1e" => Some(Family::Av1),
        "libvpx" => Some(Family::Vp8),
        "libvpx-vp9" => Some(Family::Vp9),
        _ => None,
    }
}
fn job_family(args: &[String]) -> Option<Family> {
    if args
        .first()
        .and_then(|p| Path::new(p).file_name())
        .is_none_or(|p| p != "ffmpeg")
    {
        return None;
    }
    let index = encoder_position(args)?;
    family_for(args.get(index + 1)?)
}
/// Whether this is a supported FFmpeg video encode rather than copy, audio,
/// image extraction, or a non-FFmpeg subprocess. No probing is done here.
pub fn has_supported_video_codec(args: &[String]) -> bool {
    job_family(args).is_some()
}
fn image_decode_job(args: &[String]) -> bool {
    if args
        .first()
        .and_then(|p| Path::new(p).file_name())
        .is_none_or(|p| p != "ffmpeg")
        || value(args, &["-frames:v", "-frames:v:0", "-vframes"]) != Some("1")
        || args.iter().any(|a| a == "-hwaccel")
    {
        return false;
    }
    value(args, &["-i"])
        .and_then(|p| Path::new(p).extension())
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ["mp4", "mkv", "webm", "mov", "avi", "m4v", "ts"]
                .contains(&ext.to_ascii_lowercase().as_str())
        })
}

async fn available_encoders() -> &'static HashSet<String> {
    ENCODERS
        .get_or_init(|| async {
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-hide_banner", "-encoders"]).kill_on_drop(true);
            match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
                Ok(Ok(output)) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter_map(|line| {
                            let mut fields = line.split_whitespace();
                            let flags = fields.next()?;
                            if !flags.starts_with('V') {
                                return None;
                            }
                            fields.next().map(str::to_owned)
                        })
                        .collect()
                }
                _ => HashSet::new(),
            }
        })
        .await
}
fn render_nodes() -> Vec<String> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();
    nodes.sort();
    nodes
}
fn possible_encoders(
    family: Family,
    available: &HashSet<String>,
    devices: &[String],
) -> Vec<VideoEncoder> {
    let mut result = vec![];
    let nvenc = format!("{}_nvenc", family.prefix());
    if available.contains(&nvenc) {
        result.push(VideoEncoder {
            name: nvenc,
            hardware: Hardware::Nvenc,
            family,
        });
    }
    for device in devices {
        let vaapi = format!("{}_vaapi", family.prefix());
        if available.contains(&vaapi) {
            result.push(VideoEncoder {
                name: vaapi,
                hardware: Hardware::Vaapi(device.clone()),
                family,
            });
        }
        let qsv = format!("{}_qsv", family.prefix());
        if available.contains(&qsv) {
            result.push(VideoEncoder {
                name: qsv,
                hardware: Hardware::Qsv(device.clone()),
                family,
            });
        }
    }
    let vulkan = format!("{}_vulkan", family.prefix());
    if available.contains(&vulkan) {
        for index in 0..devices.len().clamp(1, 4) {
            result.push(VideoEncoder {
                name: vulkan.clone(),
                hardware: Hardware::Vulkan(index),
                family,
            });
        }
    }
    result
}
async fn probed(family: Family) -> Vec<VideoEncoder> {
    let cache = match family {
        Family::H264 => &H264,
        Family::Hevc => &HEVC,
        Family::Av1 => &AV1,
        Family::Vp8 => &VP8,
        Family::Vp9 => &VP9,
    };
    cache
        .get_or_init(|| async move {
            let available = available_encoders().await;
            let mut result = vec![];
            for candidate in possible_encoders(family, available, &render_nodes()) {
                if probe_encoder(&candidate).await {
                    tracing::info!(encoder = candidate.name(), "GPU video encoding available");
                    result.push(candidate);
                }
            }
            result.push(VideoEncoder::cpu(family));
            result
        })
        .await
        .clone()
}
/// Working H.264 encoders, hardware first and the software fallback last.
pub async fn video_candidates() -> Vec<VideoEncoder> {
    probed(Family::H264).await
}
/// Return adapted complete command lines in fallback order. The caller owns
/// progress, cancellation, result validation, and temporary output cleanup.
pub async fn command_candidates(args: &[String]) -> Vec<Vec<String>> {
    let Some(family) = job_family(args) else {
        if image_decode_job(args) {
            let mut commands = vec![];
            for decoder in video_decoders().await {
                if let Ok(command) = rewrite_decode_args(args, &decoder) {
                    commands.push(command);
                }
            }
            commands.push(args.to_vec());
            return commands;
        }
        return vec![args.to_vec()];
    };
    let mut commands = vec![];
    for encoder in probed(family)
        .await
        .into_iter()
        .filter(VideoEncoder::is_hardware)
    {
        if let Ok(command) = rewrite_video_args(args, &encoder) {
            commands.push(command);
        }
    }
    commands.push(args.to_vec());
    commands
}

/// Plain cuts have no software filters: keep decoded frames on the same GPU
/// used by the encoder. Unsupported input codecs/bit depths retry GPU encoding
/// with software decoding, then the original CPU command.
pub async fn trim_command_candidates(args: &[String]) -> Vec<Vec<String>> {
    let Some(family) = job_family(args) else {
        return vec![args.to_vec()];
    };
    let mut commands = vec![];
    for encoder in probed(family)
        .await
        .into_iter()
        .filter(VideoEncoder::is_hardware)
    {
        if let Ok(command) = rewrite_trim_args(args, &encoder) {
            commands.push(command);
        }
        if let Ok(command) = rewrite_video_args(args, &encoder) {
            commands.push(command);
        }
    }
    commands.push(args.to_vec());
    commands
}
fn rewrite_trim_args(args: &[String], encoder: &VideoEncoder) -> Result<Vec<String>> {
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "-vf" | "-filter:v" | "-filter_complex" | "-hwaccel" | "-pix_fmt" | "-pix_fmt:v"
        )
    }) {
        bail!("direct GPU trim requires an unfiltered input");
    }
    let mut command = rewrite_video_args(args, encoder)?;
    let (accel, device) = match &encoder.hardware {
        Hardware::Nvenc => ("cuda", "0".to_owned()),
        Hardware::Vaapi(device) => ("vaapi", device.clone()),
        Hardware::Qsv(_) => ("qsv", "vice_encode".into()),
        Hardware::Vulkan(_) => ("vulkan", "vice_encode".into()),
        Hardware::Cpu => bail!("not a GPU encoder"),
    };
    let mut result = Vec::new();
    let mut index = 0;
    while index < command.len() {
        let option = command[index].as_str();
        // rewrite_video_args adds uploads for CPU-decoded input. Hardware
        // decoded frames already have the encoder's native surface format.
        if matches!(option, "-vf" | "-filter_hw_device" | "-vaapi_device") {
            index += 2;
            continue;
        }
        if option == "-preset" {
            let preset = match encoder.hardware {
                Hardware::Nvenc => "p4",
                Hardware::Qsv(_) => "fast",
                _ => &command[index + 1],
            };
            result.extend([command[index].clone(), preset.to_owned()]);
            index += 2;
            continue;
        }
        result.push(command[index].clone());
        index += 1;
    }
    command = result;
    let input = command
        .iter()
        .position(|arg| arg == "-i")
        .context("trim input")?;
    command.splice(
        input..input,
        [
            "-hwaccel".into(),
            accel.into(),
            "-hwaccel_device".into(),
            device,
            "-hwaccel_output_format".into(),
            accel.into(),
            "-extra_hw_frames".into(),
            "16".into(),
        ],
    );
    Ok(command)
}

fn rewrite_decode_args(args: &[String], hardware: &Hardware) -> Result<Vec<String>> {
    let input = args
        .iter()
        .position(|a| a == "-i")
        .context("decode job has no input")?;
    let mut command = args.to_vec();
    let flags = match hardware {
        Hardware::Nvenc => strings(&["-hwaccel", "cuda", "-hwaccel_device", "0"]),
        Hardware::Vaapi(device) => vec![
            "-hwaccel".into(),
            "vaapi".into(),
            "-hwaccel_device".into(),
            device.clone(),
        ],
        Hardware::Vulkan(device) => vec![
            "-hwaccel".into(),
            "vulkan".into(),
            "-hwaccel_device".into(),
            device.to_string(),
        ],
        Hardware::Qsv(device) => vec![
            "-init_hw_device".into(),
            format!("vaapi=vice_decode_va:{device}"),
            "-init_hw_device".into(),
            "qsv=vice_decode@vice_decode_va".into(),
            "-hwaccel".into(),
            "qsv".into(),
            "-hwaccel_device".into(),
            "vice_decode".into(),
        ],
        Hardware::Cpu => return Ok(command),
    };
    // No hardware output format is requested: FFmpeg downloads decoded frames
    // for the existing CPU scale/image filters, avoiding graph incompatibility.
    command.splice(input..input, flags);
    Ok(command)
}
struct ProbeDirectory(PathBuf);
impl Drop for ProbeDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn video_decoders() -> Vec<Hardware> {
    DECODERS
        .get_or_init(|| async {
            let directory = ProbeDirectory(
                std::env::temp_dir().join(format!("vice-decode-probe-{}", uuid::Uuid::new_v4())),
            );
            if tokio::fs::create_dir_all(&directory.0).await.is_err() {
                return vec![];
            }
            let fixture = directory.0.join("probe.mp4");
            let mut make = strings(&[
                "ffmpeg",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=128x128:r=10",
                "-frames:v",
                "2",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ]);
            make.push(fixture.to_string_lossy().into_owned());
            if run_command(&make, Duration::from_secs(5)).await.is_err() {
                return vec![];
            }
            let devices = render_nodes();
            let mut candidates = vec![Hardware::Nvenc];
            for device in &devices {
                candidates.push(Hardware::Vaapi(device.clone()));
                candidates.push(Hardware::Qsv(device.clone()));
            }
            for index in 0..devices.len().clamp(1, 4) {
                candidates.push(Hardware::Vulkan(index));
            }
            let base = vec![
                "ffmpeg".into(),
                "-v".into(),
                "error".into(),
                "-i".into(),
                fixture.to_string_lossy().into_owned(),
                "-vf".into(),
                "scale=16:16".into(),
                "-frames:v".into(),
                "1".into(),
                "-f".into(),
                "null".into(),
                "-".into(),
            ];
            let mut working = vec![];
            for decoder in candidates {
                if let Ok(command) = rewrite_decode_args(&base, &decoder)
                    && run_command(&command, Duration::from_secs(5)).await.is_ok()
                {
                    tracing::info!(decoder=?decoder,"GPU thumbnail decoding available");
                    working.push(decoder);
                }
            }
            working
        })
        .await
        .clone()
}

fn value<'a>(args: &'a [String], options: &[&str]) -> Option<&'a str> {
    args.windows(2)
        .find(|w| options.contains(&w[0].as_str()))
        .map(|w| w[1].as_str())
}
fn positive_bitrate(raw: &str) -> bool {
    raw.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .parse::<f64>()
        .is_ok_and(|n| n > 0.)
}
fn budgeted(args: &[String]) -> bool {
    value(args, &["-b:v", "-b:v:0", "-vb"]).is_some_and(positive_bitrate)
}
fn ten_bit(args: &[String]) -> bool {
    value(args, &["-pix_fmt", "-pix_fmt:v"])
        .is_some_and(|v| v.contains("10") || v.starts_with("p010"))
}

/// Add the upload to the actual mapped video output. Project exports map
/// their video graph output first, then their audio output; this keeps every
/// audio filter and every existing video filter byte-for-byte intact.
fn append_video_filter(args: &mut Vec<String>, filter: &str) -> Result<()> {
    if let Some(index) = args
        .iter()
        .position(|a| matches!(a.as_str(), "-vf" | "-filter:v" | "-filter:v:0"))
    {
        let old = args
            .get_mut(index + 1)
            .context("video filter has no expression")?;
        old.push(',');
        old.push_str(filter);
        return Ok(());
    }
    if let Some(index) = args.iter().position(|a| a == "-filter_complex") {
        let graph = args
            .get(index + 1)
            .context("complex filter has no expression")?
            .clone();
        let maps: Vec<_> = args
            .windows(2)
            .enumerate()
            .filter(|(_, w)| w[0] == "-map")
            .map(|(i, w)| (i + 1, w[1].clone()))
            .collect();
        // Unfiltered video with an audio-only complex graph can use -vf.
        if maps.first().is_some_and(|(_, map)| map.contains(":v")) {
            let insert = args.len() - 1;
            args.splice(insert..insert, ["-vf".into(), filter.into()]);
            return Ok(());
        }
        let (map_index, label) = maps
            .first()
            .filter(|(_, map)| map.starts_with('[') && map.ends_with(']'))
            .context("cannot identify complex graph's mapped video output")?;
        if label.to_ascii_lowercase().contains("audio") || label == "[aout]" {
            bail!("complex graph maps audio before video; keep its software encoder");
        }
        if !graph.contains(label) {
            bail!("mapped video label is absent from graph");
        }
        let mut output = "vice_hw_video".to_owned();
        while graph.contains(&output) {
            output.push('_');
        }
        args[index + 1] = format!("{graph};{label}{filter}[{output}]");
        args[*map_index] = format!("[{output}]");
        return Ok(());
    }
    let insert = args.len() - 1;
    args.splice(insert..insert, ["-vf".into(), filter.into()]);
    Ok(())
}
/// Convert a CPU video encode to one selected backend without changing its
/// codec family, stream maps, audio, duration, filters, or bitrate budget.
pub fn rewrite_video_args(args: &[String], encoder: &VideoEncoder) -> Result<Vec<String>> {
    let family = job_family(args).context("command has no supported CPU video encoder")?;
    if family != encoder.family {
        bail!("GPU encoder would change the requested codec family");
    }
    if !encoder.is_hardware() {
        return Ok(args.to_vec());
    }
    if args.len() < 3 {
        bail!("FFmpeg command has no output");
    }
    let quality = value(args, &["-crf", "-global_quality", "-qp"])
        .unwrap_or("23")
        .to_owned();
    let budget = budgeted(args);
    let ten = ten_bit(args);
    let upload = matches!(
        encoder.hardware,
        Hardware::Vaapi(_) | Hardware::Qsv(_) | Hardware::Vulkan(_)
    );
    let mut rewritten = vec![];
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if matches!(arg.as_str(), "-c:v" | "-c:v:0" | "-codec:v" | "-vcodec") {
            rewritten.push(arg.clone());
            rewritten.push(encoder.name.clone());
            index += 2;
            continue;
        }
        // These tune software encoders and are not portable to GPU drivers.
        if matches!(
            arg.as_str(),
            "-preset"
                | "-crf"
                | "-global_quality"
                | "-qp"
                | "-tune"
                | "-x264-params"
                | "-x265-params"
                | "-svtav1-params"
                | "-cpu-used"
                | "-deadline"
                | "-row-mt"
                | "-tile-columns"
                | "-frame-parallel"
        ) || (upload && matches!(arg.as_str(), "-pix_fmt" | "-pix_fmt:v"))
        {
            index += 2;
            continue;
        }
        rewritten.push(arg.clone());
        index += 1;
    }
    let mut global = vec![];
    let mut encode = vec![];
    match &encoder.hardware {
        Hardware::Nvenc => {
            encode.extend(strings(&["-preset", "p5", "-rc", "vbr"]));
            if !budget {
                encode.extend(["-cq".into(), quality]);
            }
        }
        Hardware::Vaapi(device) => {
            global.extend(["-vaapi_device".into(), device.clone()]);
            encode.extend(strings(&["-rc_mode", if budget { "VBR" } else { "CQP" }]));
            if !budget {
                encode.extend(["-qp".into(), quality]);
            }
            append_video_filter(
                &mut rewritten,
                if ten {
                    "format=p010le,hwupload"
                } else {
                    "format=nv12,hwupload"
                },
            )?;
        }
        Hardware::Qsv(device) => {
            global.extend([
                "-init_hw_device".into(),
                format!("vaapi=vice_va:{device}"),
                "-init_hw_device".into(),
                "qsv=vice_encode@vice_va".into(),
                "-filter_hw_device".into(),
                "vice_encode".into(),
            ]);
            encode.extend(strings(&["-preset", "medium"]));
            if !budget {
                encode.extend(["-global_quality".into(), quality]);
            }
            append_video_filter(
                &mut rewritten,
                if ten {
                    "format=p010le,hwupload=extra_hw_frames=64"
                } else {
                    "format=nv12,hwupload=extra_hw_frames=64"
                },
            )?;
        }
        Hardware::Vulkan(device) => {
            global.extend([
                "-init_hw_device".into(),
                format!("vulkan=vice_encode:{device}"),
                "-filter_hw_device".into(),
                "vice_encode".into(),
            ]);
            encode.extend(strings(&["-rc_mode", if budget { "vbr" } else { "cqp" }]));
            if !budget {
                encode.extend(["-qp".into(), quality]);
            }
            append_video_filter(
                &mut rewritten,
                if ten {
                    "format=p010le,hwupload"
                } else {
                    "format=nv12,hwupload"
                },
            )?;
        }
        Hardware::Cpu => {}
    }
    // Device initialization must precede inputs. Encoder options belong to
    // the output, after all inputs and existing codec/container options.
    rewritten.splice(1..1, global);
    let output = rewritten.len() - 1;
    rewritten.splice(output..output, encode);
    Ok(rewritten)
}

async fn probe_encoder(encoder: &VideoEncoder) -> bool {
    // Turing and newer NVENC hardware rejects 128-pixel-wide H.264 input
    // (minimum width 145). A tiny invalid probe incorrectly disabled healthy
    // NVIDIA cards for the daemon's entire lifetime. Keep this above the
    // hardware minimum for every supported codec, with an explicit 8-bit input.
    // https://forums.developer.nvidia.com/t/minimum-width-in-turing-gpus/155566
    let original = strings(&[
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=256x256:r=30",
        "-pix_fmt",
        "yuv420p",
        "-frames:v",
        "3",
        "-c:v",
        encoder.family.software(),
        "-b:v",
        "1000000",
        "-maxrate",
        "1000000",
        "-bufsize",
        "2000000",
        "-f",
        "null",
        "-",
    ]);
    let Ok(command) = rewrite_video_args(&original, encoder) else {
        return false;
    };
    match run_command(&command, Duration::from_secs(6)).await {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(encoder=encoder.name(),%error,"GPU encode probe unavailable");
            false
        }
    }
}
async fn run_command(args: &[String], timeout: Duration) -> Result<()> {
    let (program, args) = args.split_first().context("empty command")?;
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    let result = tokio::time::timeout(timeout, command.output())
        .await
        .context("video encode timed out")??;
    if !result.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&result.stderr)
                .chars()
                .take(2000)
                .collect::<String>()
        );
    }
    Ok(())
}
struct PartialOutput(PathBuf);
impl Drop for PartialOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
/// Run GPU candidates then the original CPU command. Encoded files are staged
/// beside the destination and published only after a successful command, so a
/// failed/cancelled retry never deletes or truncates an existing final file.
pub async fn run_video_job(args: &[String], timeout: Duration) -> Result<()> {
    if !has_supported_video_codec(args) && !image_decode_job(args) {
        return run_command(args, timeout).await;
    }
    run_candidates(args, command_candidates(args).await, timeout).await
}
async fn run_candidates(
    args: &[String],
    commands: Vec<Vec<String>>,
    timeout: Duration,
) -> Result<()> {
    let destination = PathBuf::from(args.last().context("missing video output")?);
    let file_output = destination
        .to_str()
        .is_some_and(|p| p != "-" && !p.starts_with("pipe:") && !p.contains("://"));
    let staged = if file_output {
        let extension = destination
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("mp4");
        Some(PartialOutput(destination.with_file_name(format!(
            ".vice-encode-{}.{}",
            uuid::Uuid::new_v4(),
            extension
        ))))
    } else {
        None
    };
    let mut errors = vec![];
    for mut command in commands {
        if let Some(staged) = &staged {
            *command.last_mut().context("missing video output")? =
                staged.0.to_string_lossy().into_owned();
        }
        let name = encoder_position(&command)
            .and_then(|p| command.get(p + 1))
            .cloned()
            .unwrap_or_default();
        tracing::info!(encoder=%name,"Encoding video");
        match run_command(&command, timeout).await {
            Ok(()) => {
                if let Some(staged) = &staged {
                    if !staged.0.metadata().is_ok_and(|m| m.len() > 0) {
                        errors.push(format!("{name} wrote no video"));
                        continue;
                    }
                    tokio::fs::rename(&staged.0, &destination).await?;
                }
                return Ok(());
            }
            Err(error) => {
                tracing::warn!(encoder=%name,%error,"Video encoding failed; trying next backend");
                errors.push(format!("{name}: {error}"));
            }
        }
        if let Some(staged) = &staged {
            let _ = tokio::fs::remove_file(&staged.0).await;
        }
    }
    bail!("No video encoder completed the job: {}", errors.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn encoder(name: &str, hardware: Hardware, family: Family) -> VideoEncoder {
        VideoEncoder {
            name: name.into(),
            hardware,
            family,
        }
    }
    fn discord_args() -> Vec<String> {
        strings(&[
            "ffmpeg",
            "-i",
            "source.mp4",
            "-vf",
            "scale=-2:720,pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-b:v",
            "900k",
            "-maxrate",
            "1000k",
            "-bufsize",
            "1800k",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-y",
            "output.mp4",
        ])
    }
    #[test]
    fn gpu_trims_keep_frames_on_device_and_preserve_selected_interval() {
        let original = strings(&[
            "ffmpeg",
            "-ss",
            "1.25",
            "-i",
            "source.mp4",
            "-ss",
            "0",
            "-t",
            "4.5",
            "-map",
            "0",
            "-c:v",
            "libx264",
            "-c:a",
            "copy",
            "-preset",
            "veryfast",
            "-crf",
            "20",
            "out.mp4",
        ]);
        for hardware in [
            Hardware::Nvenc,
            Hardware::Vaapi("/dev/dri/renderD128".into()),
            Hardware::Qsv("/dev/dri/renderD128".into()),
            Hardware::Vulkan(0),
        ] {
            let name = match hardware {
                Hardware::Nvenc => "h264_nvenc",
                Hardware::Vaapi(_) => "h264_vaapi",
                Hardware::Qsv(_) => "h264_qsv",
                _ => "h264_vulkan",
            };
            let command =
                rewrite_trim_args(&original, &encoder(name, hardware, Family::H264)).unwrap();
            assert_eq!(value(&command, &["-ss"]), Some("1.25"));
            assert_eq!(value(&command, &["-t"]), Some("4.5"));
            assert_eq!(value(&command, &["-c:a"]), Some("copy"));
            assert_eq!(value(&command, &["-map"]), Some("0"));
            assert!(
                !command
                    .iter()
                    .any(|arg| arg.contains("hwupload") || arg.contains("hwdownload"))
            );
            assert!(
                command
                    .iter()
                    .position(|arg| arg == "-hwaccel_output_format")
                    .unwrap()
                    < command.iter().position(|arg| arg == "-i").unwrap()
            );
            assert_eq!(command.last(), original.last());
        }
        let mut filtered = original;
        filtered.splice(1..1, ["-vf".into(), "scale=1280:720".into()]);
        assert!(
            rewrite_trim_args(
                &filtered,
                &encoder("h264_nvenc", Hardware::Nvenc, Family::H264)
            )
            .is_err(),
            "software filters must retain their normal upload path"
        );
    }
    #[test]
    fn discord_nvenc_preserves_exact_bitrate_budget() {
        let original = discord_args();
        let command = rewrite_video_args(
            &original,
            &encoder("h264_nvenc", Hardware::Nvenc, Family::H264),
        )
        .unwrap();
        for flag in ["-b:v", "-maxrate", "-bufsize", "-b:a", "-vf"] {
            assert_eq!(value(&command, &[flag]), value(&original, &[flag]));
        }
        assert_eq!(value(&command, &["-c:v"]), Some("h264_nvenc"));
        assert!(!command.iter().any(|s| s == "-cq" || s == "-crf"));
        assert_eq!(value(&command, &["-c:a"]), Some("aac"));
        assert_eq!(value(&command, &["-pix_fmt"]), Some("yuv420p"));
    }
    #[test]
    fn vaapi_upload_follows_all_cpu_filters() {
        let command = rewrite_video_args(
            &discord_args(),
            &encoder(
                "h264_vaapi",
                Hardware::Vaapi("/dev/dri/renderD129".into()),
                Family::H264,
            ),
        )
        .unwrap();
        assert_eq!(
            value(&command, &["-vaapi_device"]),
            Some("/dev/dri/renderD129")
        );
        assert!(
            value(&command, &["-vf"])
                .unwrap()
                .ends_with("format=nv12,hwupload")
        );
        assert!(!command.iter().any(|s| s == "-pix_fmt"));
        assert_eq!(value(&command, &["-b:v"]), Some("900k"));
        assert_eq!(value(&command, &["-rc_mode"]), Some("VBR"));
    }
    #[test]
    fn complex_editor_graph_keeps_audio_and_uploads_only_final_video() {
        let args = strings(&[
            "ffmpeg",
            "-i",
            "source.mp4",
            "-filter_complex",
            "[0:v]scale=320:180,format=yuv420p[vout];[0:a]volume=0.5[aout]",
            "-map",
            "[vout]",
            "-map",
            "[aout]",
            "-c:v",
            "libx264",
            "-crf",
            "20",
            "-c:a",
            "aac",
            "output.mp4",
        ]);
        let command = rewrite_video_args(
            &args,
            &encoder(
                "h264_vaapi",
                Hardware::Vaapi("/dev/dri/renderD128".into()),
                Family::H264,
            ),
        )
        .unwrap();
        assert_eq!(
            value(&command, &["-filter_complex"]),
            Some(
                "[0:v]scale=320:180,format=yuv420p[vout];[0:a]volume=0.5[aout];[vout]format=nv12,hwupload[vice_hw_video]"
            )
        );
        assert_eq!(value(&command, &["-map"]), Some("[vice_hw_video]"));
        assert!(command.windows(2).any(|w| w == ["-map", "[aout]"]));
    }
    #[test]
    fn vp8_stays_vp8_and_removes_libvpx_only_options() {
        let args = strings(&[
            "ffmpeg",
            "-i",
            "source.mp4",
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-crf",
            "32",
            "-b:v",
            "0",
            "-vf",
            "scale=320:180",
            "-c:a",
            "libopus",
            "output.webm",
        ]);
        let command = rewrite_video_args(
            &args,
            &encoder(
                "vp8_vaapi",
                Hardware::Vaapi("/dev/dri/renderD128".into()),
                Family::Vp8,
            ),
        )
        .unwrap();
        assert_eq!(value(&command, &["-c:v"]), Some("vp8_vaapi"));
        assert_eq!(value(&command, &["-c:a"]), Some("libopus"));
        assert_eq!(value(&command, &["-qp"]), Some("32"));
        assert_eq!(value(&command, &["-rc_mode"]), Some("CQP"));
        assert!(
            !command
                .iter()
                .any(|s| s == "-cpu-used" || s == "-deadline" || s == "-crf")
        );
        assert!(
            rewrite_video_args(
                &args,
                &encoder(
                    "h264_vaapi",
                    Hardware::Vaapi("/dev/dri/renderD128".into()),
                    Family::H264
                )
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn non_video_jobs_bypass_candidate_probes() {
        for args in [
            strings(&["flatpak", "info", "app"]),
            strings(&["ffmpeg", "-i", "a.mp4", "-c:v", "copy", "b.mp4"]),
            strings(&["ffmpeg", "-i", "a.mp4", "-c:a", "aac", "b.m4a"]),
        ] {
            assert!(!has_supported_video_codec(&args));
            assert_eq!(command_candidates(&args).await, vec![args]);
        }
    }
    #[tokio::test]
    async fn actual_device_probes_encode_and_cpu_fallback_is_last() {
        if crate::runtime::which("ffmpeg").is_none() {
            return;
        }
        let candidates = video_candidates().await;
        assert_eq!(candidates.last().unwrap().name(), "libx264");
        for candidate in candidates.iter().filter(|c| c.is_hardware()) {
            assert!(probe_encoder(candidate).await, "{}", candidate.name());
        }
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("encoded.mp4");
        let mut args = strings(&[
            "ffmpeg",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=128x128:r=10",
            "-t",
            "0.4",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-y",
        ]);
        args.push(output.to_string_lossy().into_owned());
        run_video_job(&args, Duration::from_secs(20)).await.unwrap();
        let info = crate::media::probe_media(&output).await.unwrap();
        assert_eq!(info.vcodec, "h264");
        assert!(info.duration > 0.);
    }
    #[tokio::test]
    async fn failed_gpu_job_retries_cpu_without_publishing_partial_output() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let fake = directory.path().join("ffmpeg");
        let output = directory.path().join("final.mp4");
        std::fs::write(&fake,"#!/bin/sh\ncodec=\nprevious=\nfor argument do\n [ \"$previous\" = -c:v ] && codec=$argument\n previous=$argument\n output=$argument\ndone\nif [ \"$codec\" != libx264 ]; then printf partial > \"$output\"; exit 42; fi\nprintf complete > \"$output\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&output, b"existing-final").unwrap();
        let args = vec![
            fake.to_string_lossy().into_owned(),
            "-c:v".into(),
            "libx264".into(),
            "-y".into(),
            output.to_string_lossy().into_owned(),
        ];
        let gpu = rewrite_video_args(&args, &encoder("h264_nvenc", Hardware::Nvenc, Family::H264))
            .unwrap();
        run_candidates(&args, vec![gpu, args.clone()], Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"complete");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        // Failure of every backend must preserve the previous final file.
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor output do :; done\nprintf partial > \"$output\"\nexit 42\n",
        )
        .unwrap();
        assert!(
            run_candidates(&args, vec![args.clone()], Duration::from_secs(5))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"complete");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
    #[tokio::test]
    async fn hardware_thumbnail_decode_keeps_cpu_scale_and_cpu_fallback() {
        if crate::runtime::which("ffmpeg").is_none() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.mp4");
        let image = directory.path().join("thumb.png");
        let mut fixture = strings(&[
            "ffmpeg",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=128x128:r=10",
            "-frames:v",
            "2",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-y",
        ]);
        fixture.push(source.to_string_lossy().into_owned());
        run_command(&fixture, Duration::from_secs(10))
            .await
            .unwrap();
        let args = vec![
            "ffmpeg".into(),
            "-v".into(),
            "error".into(),
            "-i".into(),
            source.to_string_lossy().into_owned(),
            "-frames:v".into(),
            "1".into(),
            "-vf".into(),
            "scale=32:32".into(),
            "-y".into(),
            image.to_string_lossy().into_owned(),
        ];
        let commands = command_candidates(&args).await;
        assert_eq!(commands.last().unwrap(), &args);
        for command in commands.iter().take(commands.len() - 1) {
            assert!(command.iter().any(|v| v == "-hwaccel"));
            assert_eq!(value(command, &["-vf"]), Some("scale=32:32"));
            assert!(!command.iter().any(|v| v == "-hwaccel_output_format"));
        }
        run_video_job(&args, Duration::from_secs(10)).await.unwrap();
        assert!(image.metadata().unwrap().len() > 0);
        let image_info = crate::media::probe_media(&image).await.unwrap();
        assert_eq!((image_info.width, image_info.height), (32, 32));
    }
    #[test]
    fn audio_only_complex_graph_preserves_raw_video_map_and_upload() {
        let args = strings(&[
            "ffmpeg",
            "-i",
            "source.mp4",
            "-filter_complex",
            "[0:a]volume=0.5[aout]",
            "-map",
            "0:v:0",
            "-map",
            "[aout]",
            "-c:v",
            "libx264",
            "-crf",
            "23",
            "output.mp4",
        ]);
        let adapted = rewrite_video_args(
            &args,
            &encoder(
                "h264_vaapi",
                Hardware::Vaapi("/dev/dri/renderD128".into()),
                Family::H264,
            ),
        )
        .unwrap();
        assert_eq!(
            value(&adapted, &["-filter_complex"]),
            Some("[0:a]volume=0.5[aout]")
        );
        assert_eq!(value(&adapted, &["-vf"]), Some("format=nv12,hwupload"));
        assert_eq!(value(&adapted, &["-map"]), Some("0:v:0"));
    }
}
