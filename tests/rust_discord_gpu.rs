//! Exercise the public Discord-copy API, its GPU attempts and cache reuse.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn script(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Actual,
    SimulatedNvenc,
    SimulatedNvencFailure,
    SimulatedAsyncInputFailure,
    SimulatedHevcNvenc,
    SimulatedHevcFailure,
    SimulatedHevcCorruption,
}
#[tokio::test]
async fn discord_copy_encodes_compatible_file_and_reuses_it_for_clipboard() -> Result<()> {
    discord_case(Backend::Actual).await
}
#[tokio::test]
async fn nvidia_minimum_resolution_probe_keeps_nvenc_for_discord_copy() -> Result<()> {
    discord_case(Backend::SimulatedNvenc).await
}
#[tokio::test]
async fn nvidia_job_failure_retries_cpu_and_reports_the_failed_encoder() -> Result<()> {
    discord_case(Backend::SimulatedNvencFailure).await
}
#[tokio::test]
async fn asynchronous_input_failure_retries_raw_input_with_the_same_gpu_encoder() -> Result<()> {
    discord_case(Backend::SimulatedAsyncInputFailure).await
}
#[tokio::test]
async fn hevc_gpu_copy_retains_hvc1_and_compatibility_selects_h264() -> Result<()> {
    discord_case(Backend::SimulatedHevcNvenc).await
}
#[tokio::test]
async fn hevc_job_failure_falls_back_to_h264_without_software_x265() -> Result<()> {
    discord_case(Backend::SimulatedHevcFailure).await
}
#[tokio::test]
async fn successful_but_corrupt_hevc_job_and_cached_copy_are_repaired() -> Result<()> {
    discord_case(Backend::SimulatedHevcCorruption).await
}
fn corrupt_mp4_payload(path: &Path) -> Result<()> {
    let mut bytes = std::fs::read(path)?;
    let marker = bytes
        .windows(4)
        .position(|part| part == b"mdat")
        .context("MP4 mdat")?;
    let length = u32::from_be_bytes(bytes[marker - 4..marker].try_into()?) as usize;
    let end = (marker - 4 + length).min(bytes.len());
    bytes[marker + 4..end].fill(0xff);
    std::fs::write(path, bytes)?;
    Ok(())
}

async fn discord_case(backend: Backend) -> Result<()> {
    let Some(ffmpeg) = vice::runtime::which("ffmpeg") else {
        return Ok(());
    };
    if vice::runtime::which("ffprobe").is_none() {
        return Ok(());
    }
    let temp = tempfile::Builder::new()
        .prefix("vice-discord + ? ")
        .tempdir()?;
    let root = temp.path();
    for name in ["bin", "clips", "images", "run"] {
        std::fs::create_dir(root.join(name))?;
    }
    let source = root.join("clips/clip.mp4");
    if let Some(real_source) =
        std::env::var_os("VICE_TEST_DISCORD_SOURCE").filter(|_| backend == Backend::Actual)
    {
        std::fs::copy(real_source, &source)?;
    } else {
        let output = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                if backend == Backend::Actual {
                    "testsrc2=duration=2:size=1440x810:rate=30"
                } else {
                    "testsrc2=duration=2:size=320x180:rate=30"
                },
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                if backend == Backend::Actual {
                    "yuv420p"
                } else {
                    "yuv420p10le"
                },
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
                "-y",
            ])
            .arg(&source)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Padding forces the >20 MB conversion branch without an expensive fixture.
        std::fs::File::options()
            .write(true)
            .open(&source)?
            .set_len(24 * 1024 * 1024)?;
    }
    let broken = root.join("broken-hevc.mp4");
    if backend == Backend::SimulatedHevcCorruption {
        let encoded = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=30",
                "-frames:v",
                "12",
                "-c:v",
                "libx265",
                "-x265-params",
                "pools=none:frame-threads=1:log-level=error",
                "-pix_fmt",
                "yuv420p",
                "-tag:v",
                "hvc1",
                "-y",
            ])
            .arg(&broken)
            .output()?;
        assert!(encoded.status.success());
        corrupt_mp4_payload(&broken)?;
        assert_eq!(
            vice::media::probe_media(&broken).await?.vcodec,
            "hevc",
            "container metadata must remain plausible"
        );
        assert!(
            vice::encoding::validate_video_stream(&broken, Duration::from_secs(10))
                .await
                .is_err()
        );
    }
    let original_info = vice::media::probe_media(&source).await?;
    let original_size = std::fs::metadata(&source)?.len();
    let original_mtime = std::fs::metadata(&source)?.modified()?;
    let invocation_log = root.join("ffmpeg.log");
    let forward = if backend == Backend::Actual {
        ffmpeg.clone()
    } else {
        root.join("bin/nvenc-driver")
    };
    if backend != Backend::Actual {
        // Simulate the NVIDIA size restriction and failure mode only. Successful
        // NVENC requests are rendered by the real software encoder so ffprobe
        // can validate the resulting video; this does not claim hardware testing.
        script(
            &forward,
            r#"#!/bin/sh
for argument do
    if [ "$argument" = -encoders ]; then
        printf ' V..... h264_nvenc NVIDIA NVENC H.264 encoder\n'
        [ "$VICE_TEST_HEVC" = 1 ] && printf ' V..... hevc_nvenc NVIDIA NVENC HEVC encoder\n'
        exit 0
    fi
done
codec= input= pixel= previous=
for argument do
    [ "$previous" = -c:v ] && codec=$argument
    [ "$previous" = -i ] && input=$argument
    [ "$previous" = -pix_fmt ] && pixel=$argument
    previous=$argument
done
case "$input" in
    async:file:*)
        if [ "$VICE_TEST_ASYNC_FAIL" = 1 ]; then
            echo 'simulated asynchronous file read unavailable' >&2
            exit 43
        fi
        ;;
esac
if [ "$codec" = h264_nvenc ] || [ "$codec" = hevc_nvenc ]; then
    case "$input" in
        color=*)
            size=${input##*s=}
            size=${size%%:*}
            width=${size%x*}
            height=${size#*x}
            if [ "$width" -lt 145 ] || [ "$height" -lt 49 ]; then
                echo 'NVENC: probe resolution is below hardware minimum' >&2
                exit 1
            fi
            exit 0
            ;;
    esac
    if [ "$pixel" != yuv420p ]; then
        echo 'NVENC: 10-bit H.264 input is not supported on this GPU' >&2
        exit 1
    fi
    if [ "$codec" = hevc_nvenc ] && [ "$VICE_TEST_HEVC_CORRUPT" = 1 ]; then
        case "$input" in
            testsrc2=*) ;;
            *) cp "$VICE_TEST_BROKEN_HEVC" "$previous"; exit 0 ;;
        esac
    fi
    if [ "$codec" = hevc_nvenc ] && [ "$VICE_TEST_HEVC_FAIL" = 1 ]; then
        echo 'simulated HEVC hardware initialization failure' >&2
        exit 44
    fi
    if [ "$VICE_TEST_NVENC_FAIL" = 1 ]; then
        echo 'simulated NVIDIA encoder initialization failure' >&2
        exit 42
    fi
    count=$#
    while [ "$count" -gt 0 ]; do
        argument=$1
        shift
        count=$((count-1))
        case "$argument" in
            -c:v)
                shift; count=$((count-1))
                if [ "$codec" = hevc_nvenc ]; then
                    set -- "$@" -c:v libx265 -x265-params 'pools=none:frame-threads=1:log-level=error'
                else
                    set -- "$@" -c:v libx264
                fi
                ;;
            -preset) shift; count=$((count-1)); set -- "$@" -preset fast ;;
            -rc) shift; count=$((count-1)) ;;
            *) set -- "$@" "$argument" ;;
        esac
    done
fi
exec "$VICE_REAL_FFMPEG" "$@"
"#,
        )?;
    }
    script(
        &root.join("bin/ffmpeg"),
        &format!(
            "#!/bin/sh\nprintf 'START\\t%s\\t%s\\n' \"$$\" \"$*\" >> \"$VICE_TEST_ENCODER_LOG\"\n'{}' \"$@\"\nresult=$?\nprintf 'END\\t%s\\t%s\\n' \"$$\" \"$result\" >> \"$VICE_TEST_ENCODER_LOG\"\nexit \"$result\"\n",
            forward.to_string_lossy().replace('\'', "'\\''")
        ),
    )?;
    script(
        &root.join("bin/gpu-screen-recorder"),
        "#!/bin/sh\necho 'test recorder is disabled' >&2\nexit 1\n",
    )?;
    for program in ["wl-copy", "xclip"] {
        script(
            &root.join("bin").join(program),
            "#!/bin/sh\ncat > \"$VICE_TEST_CLIPBOARD\"\n(sleep 15) &\nexit 0\n",
        )?;
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let mut cfg = vice::config::Config::default();
    cfg.sharing.port = port;
    cfg.sharing.enabled = false;
    cfg.sharing.cloudflare_tunnel = false;
    cfg.recording.video_codec = if matches!(
        backend,
        Backend::SimulatedHevcNvenc
            | Backend::SimulatedHevcFailure
            | Backend::SimulatedHevcCorruption
    ) {
        "hevc"
    } else {
        "h264"
    }
    .into();
    cfg.recording.backend = "gsr".into();
    cfg.recording.gsr_source = "native".into();
    cfg.output.directory = root.join("clips").to_string_lossy().into_owned();
    cfg.output.image_directory = root.join("images").to_string_lossy().into_owned();
    cfg.output.auto_playlist_by_game = false;
    cfg.hotkeys.clip.clear();
    cfg.discord.enabled = false;
    cfg.updates.check_on_start = false;
    std::fs::write(root.join("config.toml"), toml::to_string(&cfg)?)?;
    let log = std::fs::File::create(root.join("daemon.log"))?;
    let child = Command::new(env!("CARGO_BIN_EXE_vice"))
        .args(["start", "--no-open-ui"])
        .env("VICE_CONFIG", root.join("config.toml"))
        .env("VICE_DATA_DIR", root.join("data"))
        .env("VICE_CACHE_DIR", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("VICE_TEST_ENCODER_LOG", &invocation_log)
        .env("VICE_REAL_FFMPEG", &ffmpeg)
        .env("VICE_TEST_BROKEN_HEVC", &broken)
        .env(
            "VICE_TEST_HEVC_CORRUPT",
            if backend == Backend::SimulatedHevcCorruption {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "VICE_TEST_HEVC",
            if matches!(
                backend,
                Backend::SimulatedHevcNvenc
                    | Backend::SimulatedHevcFailure
                    | Backend::SimulatedHevcCorruption
            ) {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "VICE_TEST_HEVC_FAIL",
            if backend == Backend::SimulatedHevcFailure {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "VICE_TEST_NVENC_FAIL",
            if backend == Backend::SimulatedNvencFailure {
                "1"
            } else {
                "0"
            },
        )
        .env(
            "VICE_TEST_ASYNC_FAIL",
            if backend == Backend::SimulatedAsyncInputFailure {
                "1"
            } else {
                "0"
            },
        )
        .env("VICE_TEST_CLIPBOARD", root.join("clipboard"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()?;
    let mut daemon = Daemon(child);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;
    let mut ready = false;
    for _ in 0..150 {
        if client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        if let Some(status) = daemon.0.try_wait()? {
            anyhow::bail!(
                "daemon exited {status}: {}",
                std::fs::read_to_string(root.join("daemon.log"))?
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        ready,
        "{}",
        std::fs::read_to_string(root.join("daemon.log"))?
    );
    let response = client
        .get(format!("{base}/api/clips/clip/discord"))
        .send()
        .await?;
    let status = response.status();
    let result: Value = response.json().await?;
    assert!(status.is_success(), "{result}");
    let path = std::path::PathBuf::from(result["path"].as_str().context("copy path")?);
    assert_ne!(path, source);
    assert!(path.starts_with(root.join("cache")));
    let info = vice::media::probe_media(&path).await?;
    assert_eq!(
        info.vcodec,
        if backend == Backend::SimulatedHevcNvenc {
            "hevc"
        } else {
            "h264"
        }
    );
    let tag = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_tag_string",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&path)
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&tag.stdout).trim(),
        if backend == Backend::SimulatedHevcNvenc {
            "hvc1"
        } else {
            "avc1"
        }
    );
    assert_eq!(
        info.audio_streams, original_info.audio_streams,
        "Discord copies retain every audio track"
    );
    assert!(info.width <= 1280 && info.width <= original_info.width);
    assert_eq!(info.width % 2, 0);
    assert_eq!(info.height % 2, 0);
    let expected_width = original_info.width.min(1280) / 2 * 2;
    assert_eq!(info.width, expected_width);
    assert!(
        (info.height as f64
            - original_info.height as f64 * info.width as f64 / original_info.width as f64)
            .abs()
            <= 2.0,
        "resizing must retain aspect ratio within even-pixel rounding"
    );
    assert!(info.duration > 1.8);
    let pixel = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=pix_fmt",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(&path)
        .output()?;
    assert!(pixel.status.success());
    assert_eq!(
        String::from_utf8_lossy(&pixel.stdout).trim(),
        "yuv420p",
        "Discord copies must stay 8-bit, including from a 10-bit recording"
    );

    assert!(std::fs::metadata(&path)?.len() <= vice::editor::DISCORD_MAX_BYTES);
    let encoded_mtime = std::fs::metadata(&path)?.modified()?;
    let log_before = std::fs::read_to_string(&invocation_log)?;
    let source_arg = format!("-i {}", source.display());
    let async_source_arg = format!("-i async:file:{}", source.display());
    let jobs: Vec<_> = log_before
        .lines()
        .filter(|line| {
            line.starts_with("START\t")
                && (line.contains(&source_arg) || line.contains(&async_source_arg))
                && line.contains("-c:v ")
        })
        .collect();
    assert!(!jobs.is_empty(), "Discord API must invoke the encoder");
    let completed = jobs
        .iter()
        .rev()
        .find(|line| {
            line.split('\t').nth(1).is_some_and(|pid| {
                log_before
                    .lines()
                    .any(|end| end == format!("END\t{pid}\t0"))
            })
        })
        .context("no completed Discord encoding invocation")?;
    let encoder = completed
        .split("-c:v ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("unknown");
    eprintln!(
        "Discord API conversion completed with {encoder}; direct GPU decode/resize={}; async input={}; valid H.264/audio copy stays under 20 MB",
        completed.contains("-hwaccel_output_format"),
        completed.contains("-i async:file:")
    );
    let daemon_log = std::fs::read_to_string(root.join("daemon.log"))?;
    match backend {
        Backend::SimulatedNvenc => {
            assert_eq!(
                encoder, "h264_nvenc",
                "a supported NVIDIA GPU must not be lost to an invalid tiny probe"
            );
            assert!(
                !jobs.iter().any(|line| line.contains("-c:v libx264")),
                "no CPU video encoding should be requested after successful NVENC"
            );
        }
        Backend::SimulatedNvencFailure => {
            assert!(jobs[0].contains("-c:v h264_nvenc"));
            assert_eq!(encoder, "libx264");
            assert!(daemon_log.contains("simulated NVIDIA encoder initialization failure"));
            assert!(daemon_log.contains("encoder=h264_nvenc"));
        }
        Backend::SimulatedAsyncInputFailure => {
            assert_eq!(
                encoder, "h264_nvenc",
                "failed async reads must retry the same GPU encoder before CPU encoding"
            );
            assert!(!completed.contains("-i async:file:"));
            assert!(jobs[0].contains("-i async:file:"));
            assert!(daemon_log.contains("simulated asynchronous file read unavailable"));
            assert!(!jobs.iter().any(|line| line.contains("-c:v libx264")));
        }
        Backend::SimulatedHevcNvenc => {
            assert_eq!(encoder, "hevc_nvenc");
            assert!(
                !jobs
                    .iter()
                    .any(|line| line.contains("-c:v libx265") || line.contains("-c:v libx264"))
            );
        }
        Backend::SimulatedHevcFailure => {
            assert!(
                jobs[0].contains("-c:v h264_nvenc"),
                "failed rate-control probes must skip the bad path before encoding the real clip"
            );
            assert_eq!(encoder, "h264_nvenc");
            assert!(!jobs.iter().any(|line| line.contains("-c:v libx265")));
            assert!(daemon_log.contains("Discord rate-control probe produced an unusable video"));
        }
        Backend::SimulatedHevcCorruption => {
            assert_eq!(encoder, "h264_nvenc");
            assert!(daemon_log.contains("Converted video is malformed"));
            vice::encoding::validate_video_stream(&path, Duration::from_secs(20)).await?;
        }
        Backend::Actual => {}
    }
    if daemon_log
        .lines()
        .any(|line| line.contains("GPU video encoding available") && line.contains("h264_"))
    {
        assert!(
            jobs[0].contains("-c:v h264_") || jobs[0].contains("-c:v hevc_"),
            "a confirmed GPU should be attempted before the CPU: {}",
            jobs[0]
        );
    }
    let repeated: Value = client
        .get(format!("{base}/api/clips/clip/discord"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(repeated["path"], result["path"]);
    assert_eq!(std::fs::metadata(&path)?.modified()?, encoded_mtime);
    let copied_at = std::time::Instant::now();
    let copy: Value = client
        .post(format!("{base}/api/clips/clip/copy-file?discord=1"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(
        copied_at.elapsed() < Duration::from_millis(250),
        "clipboard owner must not block the response"
    );
    assert_eq!(copy["ok"], true);
    assert!(std::fs::read_to_string(root.join("clipboard"))?.contains("file://"));
    assert_eq!(
        std::fs::read_to_string(&invocation_log)?,
        log_before,
        "cached GET and clipboard copy should not encode again"
    );
    assert_eq!(std::fs::metadata(&source)?.len(), original_size);
    assert_eq!(std::fs::metadata(&source)?.modified()?, original_mtime);
    // Renaming changes presentation only. Keep already copied clipboard paths
    // usable, and reuse the encoded bytes under the clip's new filename.
    let renamed: Value = client
        .post(format!("{base}/api/clips/clip/rename"))
        .json(&serde_json::json!({"name":"Renamed Discord clip"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(renamed["slug"], "clip");
    let renamed_source = root.join("clips/Renamed Discord clip.mp4");
    assert!(renamed_source.is_file());
    assert_eq!(std::fs::metadata(&renamed_source)?.len(), original_size);
    assert_eq!(
        std::fs::metadata(&renamed_source)?.modified()?,
        original_mtime
    );
    assert!(
        path.is_file(),
        "renaming must not break the file URI already placed on the clipboard"
    );
    let started = std::time::Instant::now();
    let after_rename: Value = client
        .get(format!("{base}/api/clips/clip/discord"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "rename caused a slow Discord copy: {elapsed:?}"
    );
    let renamed_copy =
        std::path::PathBuf::from(after_rename["path"].as_str().context("renamed copy path")?);
    assert_eq!(
        std::fs::read(&renamed_copy)?,
        std::fs::read(&path)?,
        "renaming should reuse exactly the existing encoded bytes"
    );
    assert_eq!(std::fs::metadata(&path)?.modified()?, encoded_mtime);
    assert_eq!(
        std::fs::read_to_string(&invocation_log)?,
        log_before,
        "renaming an unchanged video must not launch another FFmpeg job"
    );
    if matches!(
        backend,
        Backend::SimulatedHevcNvenc
            | Backend::SimulatedHevcFailure
            | Backend::SimulatedHevcCorruption
    ) {
        let before_switch = std::fs::read_to_string(&invocation_log)?;
        client
            .post(format!("{base}/api/config"))
            .json(&serde_json::json!({"recording":{"video_codec":"h264"}}))
            .send()
            .await?
            .error_for_status()?;
        let compatible: Value = client
            .get(format!("{base}/api/clips/clip/discord"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let compatible_path = std::path::PathBuf::from(
            compatible["path"]
                .as_str()
                .context("compatible copy path")?,
        );
        assert_eq!(
            vice::media::probe_media(&compatible_path).await?.vcodec,
            "h264"
        );
        if backend == Backend::SimulatedHevcNvenc {
            assert_ne!(
                compatible_path, renamed_copy,
                "H.264 compatibility must not return the HEVC copy"
            );
        } else {
            assert_eq!(
                std::fs::read_to_string(&invocation_log)?,
                before_switch,
                "fallback H.264 bytes must also be reusable in compatibility mode"
            );
        }
        let before_return = std::fs::read_to_string(&invocation_log)?;
        client
            .post(format!("{base}/api/config"))
            .json(&serde_json::json!({"recording":{"video_codec":"hevc"}}))
            .send()
            .await?
            .error_for_status()?;
        let preferred: Value = client
            .get(format!("{base}/api/clips/clip/discord"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(preferred["path"], after_rename["path"]);
        assert_eq!(
            std::fs::read_to_string(&invocation_log)?,
            before_return,
            "changing preferences must retain existing compatible copies"
        );
    }
    if backend == Backend::SimulatedHevcCorruption {
        use sha2::{Digest, Sha256};
        let original_hash = Sha256::digest(std::fs::read(&renamed_source)?);
        corrupt_mp4_payload(&renamed_copy)?;
        let before_repair = std::fs::read_to_string(&invocation_log)?;
        let repaired: Value = client
            .get(format!("{base}/api/clips/clip/discord"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let repaired_path =
            std::path::PathBuf::from(repaired["path"].as_str().context("repaired path")?);
        vice::encoding::validate_video_stream(&repaired_path, Duration::from_secs(20)).await?;
        let after_repair = std::fs::read_to_string(&invocation_log)?;
        assert!(
            after_repair.len() > before_repair.len(),
            "changed cached bytes must be checked and regenerated"
        );
        assert!(
            !after_repair[before_repair.len()..].contains("-c:v hevc_nvenc"),
            "a driver that emitted malformed frames must remain disabled for Discord copies"
        );
        client
            .get(format!("{base}/api/clips/clip/discord"))
            .send()
            .await?
            .error_for_status()?;
        assert_eq!(
            std::fs::read_to_string(&invocation_log)?,
            after_repair,
            "valid repaired bytes must retain the instant cache path"
        );
        assert_eq!(
            Sha256::digest(std::fs::read(&renamed_source)?),
            original_hash
        );
    }
    client
        .post(format!("{base}/api/quit"))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
