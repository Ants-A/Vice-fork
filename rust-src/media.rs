//! Media probing and atomic, stream-copy metadata updates.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};
use tokio::process::Command;

/// The library scanner reads this once; gallery requests need no network stat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub size: u64,
    pub mtime_ns: u128,
    pub inode: u64,
}
impl FileIdentity {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        Self::from_metadata(&std::fs::metadata(path)?)
    }
    pub fn from_metadata(metadata: &std::fs::Metadata) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            size: metadata.len(),
            mtime_ns: metadata
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            inode: metadata.ino(),
        })
    }
    pub fn matches_legacy(self, value: &Value) -> bool {
        value.get(0).and_then(Value::as_u64) == Some(self.size)
            && value.get(1).and_then(Value::as_u64).map(u128::from) == Some(self.mtime_ns)
    }
}

/// Read the existing Python cache as well as Rust's extended version. Preserve
/// tag-only records so playlist recovery does not reread every recording.
pub type MetadataEntries = HashMap<PathBuf, (u64, u128, Value)>;
pub fn load_metadata_cache(
    path: &Path,
    identities: &HashMap<PathBuf, FileIdentity>,
) -> (MetadataEntries, MetadataEntries) {
    let mut metadata = HashMap::new();
    let mut tags = HashMap::new();
    let value: Value = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if let Some(files) = value["files"].as_object() {
        for (name, record) in files {
            let path = PathBuf::from(name);
            let Some(identity) = identities.get(&path).copied() else {
                continue;
            };
            if !identity.matches_legacy(&record["identity"])
                || record["inode"]
                    .as_u64()
                    .is_some_and(|n| n != identity.inode)
            {
                continue;
            }
            let mut meta = record["meta"].clone();
            let known_tags = if record["tags"].is_object() {
                record["tags"].clone()
            } else {
                meta["tags"].clone()
            };
            if known_tags.is_object() {
                tags.insert(
                    path.clone(),
                    (identity.size, identity.mtime_ns, known_tags.clone()),
                );
            }
            if meta.is_object() && meta["unreadable"] != true {
                if !meta["tags"].is_object() {
                    meta["tags"] = known_tags;
                }
                metadata.insert(path, (identity.size, identity.mtime_ns, meta));
            }
        }
    }
    (metadata, tags)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub vcodec: String,
    pub audio_streams: usize,
    pub audio_tracks: Vec<Value>,
    pub tags: HashMap<String, String>,
}

pub async fn run_command(argv: &[String], timeout: f64) -> Result<Output> {
    let (program, args) = argv.split_first().context("empty media command")?;
    let child = Command::new(program)
        .args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {program}"))?;
    tokio::time::timeout(
        Duration::from_secs_f64(timeout.max(0.01)),
        child.wait_with_output(),
    )
    .await
    .with_context(|| format!("{program} timed out after {timeout}s"))?
    .with_context(|| format!("waiting for {program}"))
}

async fn capture(program: &str, args: &[&str], path: &Path, timeout: f64) -> Result<Output> {
    let child = Command::new(program)
        .args(args)
        .arg(path)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("{program} not found; install ffmpeg"))?;
    Ok(
        tokio::time::timeout(Duration::from_secs_f64(timeout), child.wait_with_output())
            .await
            .with_context(|| format!("{program} timed out after {timeout}s"))??,
    )
}

fn number(value: &Value) -> f64 {
    let n = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0);
    if n.is_finite() && n > 0.0 { n } else { 0.0 }
}
pub fn duration_from_video_samples(stream: &Value) -> f64 {
    let frames = number(&stream["nb_frames"]);
    for key in ["avg_frame_rate", "r_frame_rate"] {
        let rate = stream[key]
            .as_str()
            .map(|s| {
                if let Some((a, b)) = s.split_once('/') {
                    a.parse::<f64>().unwrap_or(0.0) / b.parse::<f64>().unwrap_or(0.0)
                } else {
                    s.parse().unwrap_or(0.0)
                }
            })
            .unwrap_or_else(|| number(&stream[key]));
        if rate.is_finite() && rate > 0.0 && rate <= 1000.0 && frames > 0.0 {
            return frames / rate;
        }
    }
    0.0
}
pub fn ffprobe_reason(stderr: &[u8], path: &Path) -> String {
    let s = String::from_utf8_lossy(stderr);
    let s = s
        .lines()
        .rfind(|s| !s.trim().is_empty())
        .unwrap_or("")
        .trim();
    s.strip_prefix(&format!("{}: ", path.display()))
        .unwrap_or(s)
        .chars()
        .take(300)
        .collect()
}
fn tags(value: &Value) -> HashMap<String, String> {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| {
                    (
                        k.to_lowercase(),
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}
pub async fn probe_media(path: &Path) -> Result<MediaInfo> {
    let out = capture(
        "ffprobe",
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ],
        path,
        15.0,
    )
    .await?;
    if !out.status.success() {
        bail!("{}", ffprobe_reason(&out.stderr, path));
    }
    let data: Value = serde_json::from_slice(&out.stdout).context("invalid ffprobe JSON")?;
    let streams = data["streams"]
        .as_array()
        .context("ffprobe returned no streams")?;
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .context("the file has no video stream")?;
    let mut duration = number(&data["format"]["duration"]);
    if duration <= 0.0 {
        duration = number(&video["duration"]);
    }
    if duration <= 0.0 {
        duration = duration_from_video_samples(video);
        let frames = number(&video["nb_frames"]);
        if duration > 0.0
            && frames > 1.0
            && let Ok(out) = capture(
                "ffprobe",
                &[
                    "-v",
                    "error",
                    "-count_packets",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=nb_read_packets",
                    "-of",
                    "csv=p=0",
                ],
                path,
                30.0,
            )
            .await
            && let Ok(count) = String::from_utf8_lossy(&out.stdout)
                .trim()
                .split(',')
                .next()
                .unwrap_or("")
                .parse::<f64>()
            && count * 2.0 < frames
        {
            bail!(
                "every frame is stamped at the same time, so only {count} of its {frames} frames can be read"
            );
        }
    }
    let audio_tracks: Vec<Value>=streams.iter().filter(|s|s["codec_type"]=="audio").enumerate().map(|(index,s)| {
        let tags=tags(&s["tags"]);
        json!({"index":index,"title":tags.get("title").map(|s|s.chars().take(200).collect::<String>()).unwrap_or_default(),
            "language":tags.get("language").map(|s|s.chars().take(32).collect::<String>()).unwrap_or_default(),"channels":s["channels"].as_u64().unwrap_or(0)})
    }).collect();
    Ok(MediaInfo {
        width: video["width"].as_u64().unwrap_or(0) as u32,
        height: video["height"].as_u64().unwrap_or(0) as u32,
        duration,
        vcodec: video["codec_name"].as_str().unwrap_or("").to_lowercase(),
        audio_streams: audio_tracks.len(),
        audio_tracks,
        tags: tags(&data["format"]["tags"]),
    })
}
pub async fn probe_media_detailed(path: &Path) -> (Option<MediaInfo>, String) {
    match probe_media(path).await {
        Ok(m) => (Some(m), String::new()),
        Err(e) => (None, format!("{e:#}")),
    }
}
pub async fn get_duration(path: &Path) -> f64 {
    probe_media(path).await.map(|m| m.duration).unwrap_or(0.0)
}
pub async fn probe_container_tags(path: &Path) -> Result<HashMap<String, String>> {
    let out = capture(
        "ffprobe",
        &["-v", "error", "-show_entries", "format_tags", "-of", "json"],
        path,
        10.0,
    )
    .await?;
    if !out.status.success() {
        bail!("{}", ffprobe_reason(&out.stderr, path));
    }
    Ok(tags(
        &serde_json::from_slice::<Value>(&out.stdout)?["format"]["tags"],
    ))
}
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Only replace the original after verifying all requested tags survived the muxer.
pub async fn write_metadata_tags(
    path: &Path,
    requested: &HashMap<String, String>,
    timeout: f64,
    readrate: Option<f64>,
) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    if requested.is_empty() || !["mp4", "mkv"].contains(&ext.as_str()) {
        return Ok(false);
    }
    let before = std::fs::metadata(path)?;
    let tmp = TempFile(path.with_file_name(format!(".vice-meta-{}.{}", uuid::Uuid::new_v4(), ext)));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
    if let Some(r) = readrate {
        cmd.args(["-readrate", &r.to_string()]);
    }
    cmd.arg("-i")
        .arg(path)
        .args(["-map", "0", "-map_metadata", "0", "-c", "copy"]);
    for (k, v) in requested {
        cmd.arg("-metadata").arg(format!("{k}={v}"));
    }
    if ext == "mp4" {
        cmd.args([
            "-metadata",
            "major_brand=",
            "-metadata",
            "minor_version=",
            "-metadata",
            "compatible_brands=",
            "-movflags",
            "+faststart+use_metadata_tags",
            "-f",
            "mp4",
        ]);
    } else {
        cmd.args(["-f", "matroska"]);
    }
    cmd.arg(&tmp.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs_f64(timeout), cmd.output()).await??;
    if !out.status.success() || std::fs::metadata(&tmp.0).map(|m| m.len()).unwrap_or(0) == 0 {
        return Ok(false);
    }
    let written = probe_container_tags(&tmp.0).await?;
    if requested
        .iter()
        .any(|(k, v)| written.get(&k.to_lowercase()) != Some(v))
    {
        return Ok(false);
    }
    let now = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    if (
        now.dev(),
        now.ino(),
        now.len(),
        now.mtime(),
        now.mtime_nsec(),
    ) != (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
    ) {
        return Ok(false);
    }
    std::fs::set_permissions(&tmp.0, before.permissions())?;
    let times = std::fs::FileTimes::new()
        .set_accessed(before.accessed()?)
        .set_modified(before.modified()?);
    std::fs::File::options()
        .write(true)
        .open(&tmp.0)?
        .set_times(times)?;
    std::fs::rename(&tmp.0, path)?;
    Ok(true)
}
pub fn is_temp_file(name: &str) -> bool {
    name.starts_with(".vice-meta-")
        || [
            "trim.mp4",
            "wm.mp4",
            "fix.mp4",
            "trimming.mp4",
            "trim.mkv",
            "wm.mkv",
            "fix.mkv",
            "trimming.mkv",
            "export.mp4",
            "annotating.png",
            "annotating.jpg",
            "annotating.jpeg",
        ]
        .iter()
        .any(|suffix| name.ends_with(&format!(".{suffix}")))
}
pub fn cleanup_temp_files(directory: &Path) {
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            if is_temp_file(&entry.file_name().to_string_lossy())
                && entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Trim exactly the selected interval. A stream copy is accepted only when its
/// first packet is independently decodable at zero; otherwise encode the cut.
/// The caller owns the temporary output and commits it after this succeeds.
pub async fn trim_range(source: &Path, target: &Path, start: f64, end: f64) -> Result<()> {
    let before = probe_media(source).await?;
    trim_range_with_metadata(source, target, start, end, &before, &HashMap::new())
        .await
        .map(|_| ())
}

pub async fn trim_range_with_metadata(
    source: &Path,
    target: &Path,
    start: f64,
    end: f64,
    before: &MediaInfo,
    tags: &HashMap<String, String>,
) -> Result<MediaInfo> {
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
        bail!("invalid trim range");
    }
    if before.duration <= 0.0 || end > before.duration + 0.1 {
        bail!("trim extends beyond the clip");
    }
    let duration = end - start;
    let make = |encoder: Option<&str>| {
        let mut args: Vec<String> = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-ss"]
            .map(str::to_owned)
            .to_vec();
        args.extend([
            start.to_string(),
            "-i".into(),
            source.to_string_lossy().into_owned(),
        ]);
        if encoder.is_some() {
            args.extend(["-ss".into(), "0".into()]);
        }
        args.extend([
            "-t".into(),
            duration.to_string(),
            "-map".into(),
            "0".into(),
            "-map_metadata".into(),
            "0".into(),
        ]);
        if let Some(encoder) = encoder {
            args.extend([
                "-c:v".into(),
                encoder.into(),
                "-preset".into(),
                if encoder == "libsvtav1" {
                    "8".into()
                } else {
                    "veryfast".into()
                },
                "-crf".into(),
                if encoder == "libx265" {
                    "23".into()
                } else {
                    "20".into()
                },
                "-c:a".into(),
                "copy".into(),
            ]);
        } else {
            args.extend(["-c".into(), "copy".into()]);
        }
        for (key, value) in tags {
            args.extend(["-metadata".into(), format!("{key}={value}")]);
        }
        if target.extension().is_some_and(|s| s == "mp4") {
            args.extend(["-movflags".into(), "+faststart+use_metadata_tags".into()]);
        }
        args.extend(["-y".into(), target.to_string_lossy().into_owned()]);
        args
    };
    // Most slider cuts start between keyframes. Do not copy an entire large
    // interval only to discover at validation that it needs encoding anyway.
    if starts_on_keyframe(source, start).await
        && run_command(&make(None), 120.0)
            .await
            .is_ok_and(|out| out.status.success())
        && let Some(info) = validated_trim(target, duration).await
    {
        return Ok(info);
    }
    let encoders = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .await?;
    let available = String::from_utf8_lossy(&encoders.stdout);
    let preferred = match before.vcodec.as_str() {
        "hevc" | "h265" if available.contains("libx265") => "libx265",
        "av1" if available.contains("libsvtav1") => "libsvtav1",
        _ => "libx264",
    };
    let mut reason = String::new();
    // Hardware support is confirmed by a real encode, then the selected cut
    // is decoded before accepting it. A runtime GPU failure can safely fall
    // through to the original software command without touching the source.
    for args in crate::encoding::command_candidates(&make(Some(preferred))).await {
        match run_command(&args, 900.0).await {
            Ok(output) if output.status.success() => {
                if let Some(info) = validated_trim(target, duration).await {
                    return Ok(info);
                }
                reason =
                    "the trimmed file does not decode correctly; the original has been kept".into();
            }
            Ok(output) => {
                reason = String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(500)
                    .collect()
            }
            Err(error) => reason = error.to_string(),
        }
    }
    bail!("trim failed: {reason}")
}
async fn starts_on_keyframe(path: &Path, start: f64) -> bool {
    if start <= 0.000_001 {
        return true;
    }
    let interval = format!("{start:.6}%+#1");
    let Ok(output) = capture(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            &interval,
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "json",
        ],
        path,
        2.0,
    )
    .await
    else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return false;
    };
    let packet = &value["packets"][0];
    packet["flags"]
        .as_str()
        .is_some_and(|flags| flags.contains('K'))
        && packet["pts_time"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .is_some_and(|pts| (pts - start).abs() < 0.000_5)
}
async fn validated_trim(path: &Path, expected: f64) -> Option<MediaInfo> {
    let Ok(info) = probe_media(path).await else {
        return None;
    };
    if info.duration < expected - 0.1 || info.duration > expected + 0.2 {
        return None;
    }
    let packet = capture(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-read_intervals",
            "%+#1",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ],
        path,
        30.0,
    )
    .await;
    if let Ok(packet) = packet {
        let raw = String::from_utf8_lossy(&packet.stdout);
        if let Some(first) = raw.lines().next()
            && let Some((pts, flags)) = first.split_once(',')
            && (!flags.contains('K') || pts.parse::<f64>().is_ok_and(|n| n < 0.0))
        {
            return None;
        }
    }
    let argv = vec![
        "ffmpeg".into(),
        "-v".into(),
        "error".into(),
        "-i".into(),
        path.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-frames:v".into(),
        "1".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ];
    run_command(&argv, 30.0)
        .await
        .is_ok_and(|out| out.status.success() && out.stderr.is_empty())
        .then_some(info)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_cache_reuses_tags_and_duration_but_rejects_changed_content() {
        let root = tempfile::tempdir().unwrap();
        let clip = root.path().join("clip.mp4");
        std::fs::write(&clip, b"original").unwrap();
        let identity = FileIdentity::read(&clip).unwrap();
        let cache = root.path().join("media.json");
        let record = json!({"identity":[identity.size,identity.mtime_ns],"meta":{"duration":12.5,"width":1920,"height":1080,"vcodec":"h264"},"tags":{"vice_playlists":"portable membership"}});
        std::fs::write(
            &cache,
            serde_json::to_vec(&json!({"version":1,"files":{clip.to_string_lossy():record}}))
                .unwrap(),
        )
        .unwrap();
        let mut identities = HashMap::from([(clip.clone(), identity)]);
        let (meta, tags) = load_metadata_cache(&cache, &identities);
        assert_eq!(meta[&clip].2["duration"], 12.5);
        assert_eq!(
            meta[&clip].2["tags"]["vice_playlists"],
            "portable membership"
        );
        assert!(tags.contains_key(&clip));
        identities.get_mut(&clip).unwrap().size += 1;
        assert!(load_metadata_cache(&cache, &identities).0.is_empty());
    }
    #[test]
    fn rust_cache_rejects_replacement_with_identical_size_and_timestamp() {
        let root = tempfile::tempdir().unwrap();
        let clip = root.path().join("clip.mp4");
        std::fs::write(&clip, b"original").unwrap();
        let before = std::fs::metadata(&clip).unwrap();
        let identity = FileIdentity::read(&clip).unwrap();
        let cache = root.path().join("media.json");
        std::fs::write(&cache,serde_json::to_vec(&json!({"version":1,"files":{clip.to_string_lossy():{"identity":[identity.size,identity.mtime_ns],"inode":identity.inode,"meta":{"duration":12.5}}}})).unwrap()).unwrap();
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"new-data").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(before.modified().unwrap()))
            .unwrap();
        std::fs::rename(&replacement, &clip).unwrap();
        let replacement = FileIdentity::read(&clip).unwrap();
        assert_eq!(
            (identity.size, identity.mtime_ns),
            (replacement.size, replacement.mtime_ns)
        );
        assert_ne!(identity.inode, replacement.inode);
        assert!(
            load_metadata_cache(&cache, &HashMap::from([(clip, replacement)]))
                .0
                .is_empty()
        );
    }
    #[test]
    fn estimates_samples_only_with_sane_rate() {
        assert_eq!(
            duration_from_video_samples(&json!({"nb_frames":"120","avg_frame_rate":"60/1"})),
            2.0
        );
        assert_eq!(
            duration_from_video_samples(
                &json!({"nb_frames":"120","avg_frame_rate":"0/0","r_frame_rate":"30/1"})
            ),
            4.0
        );
        assert_eq!(
            duration_from_video_samples(&json!({"nb_frames":"120","avg_frame_rate":"1001/1"})),
            0.0
        );
    }
    #[test]
    fn temp_patterns_exclude_real_clips() {
        assert!(is_temp_file("clip.trim.mkv"));
        assert!(!is_temp_file("Vice_Clip_1.mp4"));
    }
    #[test]
    fn reason_removes_path() {
        assert_eq!(
            ffprobe_reason(
                b"[mov] bad\n/tmp/a.mp4: invalid data\n",
                Path::new("/tmp/a.mp4")
            ),
            "invalid data"
        );
    }
}
