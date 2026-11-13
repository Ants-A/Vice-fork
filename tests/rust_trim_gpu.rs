//! Real media validation with process-isolated encoder probes and driver failures.
//! The simulated NVIDIA driver uses libx264 to render successful requests; it
//! verifies fallback order without claiming access to physical NVIDIA hardware.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};
use vice::{encoding, media};

fn executable(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
fn fixture(ffmpeg: &Path, path: &Path) -> Result<()> {
    let result = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=320x180:rate=20:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:duration=3:sample_rate=48000",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-map",
            "2:a:0",
            "-vf",
            "drawbox=color=blue:t=fill:enable='gte(t,1.25)*lt(t,2.75)'",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-threads",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "1000",
            "-keyint_min",
            "1000",
            "-sc_threshold",
            "0",
            "-bf",
            "0",
            "-c:a",
            "aac",
            "-y",
        ])
        .arg(path)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

#[test]
fn trim_uses_available_gpu_and_preserves_selected_frames_and_audio() -> Result<()> {
    run_case("actual")
}
#[test]
fn unsupported_gpu_decode_retries_gpu_encoding_before_cpu() -> Result<()> {
    run_case("decode-fails")
}
#[test]
fn unavailable_gpu_encoder_falls_back_without_changing_source() -> Result<()> {
    run_case("encode-fails")
}

fn run_case(mode: &str) -> Result<()> {
    let Some(ffmpeg) = vice::runtime::which("ffmpeg") else {
        return Ok(());
    };
    if vice::runtime::which("ffprobe").is_none() {
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    std::fs::create_dir(root.join("bin"))?;
    let source = root.join("source.mp4");
    fixture(&ffmpeg, &source)?;
    let before_bytes = std::fs::read(&source)?;
    let before_identity = media::FileIdentity::read(&source)?;
    executable(
        &root.join("bin/ffmpeg"),
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$VICE_TEST_TRIM_LOG"
if [ "$VICE_TEST_TRIM_MODE" != actual ]; then
    for argument do
        if [ "$argument" = -encoders ]; then
            printf ' V..... h264_nvenc NVIDIA NVENC H.264 encoder\n'
            exit 0
        fi
    done
    codec= input= accel= previous=
    for argument do
        [ "$previous" = -c:v ] && codec=$argument
        [ "$previous" = -i ] && input=$argument
        [ "$previous" = -hwaccel ] && accel=$argument
        previous=$argument
    done
    if [ "$codec" = h264_nvenc ]; then
        case "$input" in color=*) exit 0;; esac
        if [ "$VICE_TEST_TRIM_MODE" = encode-fails ]; then
            echo 'simulated NVENC initialization failure' >&2
            exit 42
        fi
        if [ "$accel" = cuda ]; then
            echo 'simulated unsupported NVDEC input' >&2
            exit 43
        fi
        # Preserve seeking, stream maps, metadata and audio. Only translate the
        # simulated encoder's codec/rate-control options for the real renderer.
        count=$#
        while [ "$count" -gt 0 ]; do
            argument=$1
            shift
            count=$((count-1))
            case "$argument" in
                -c:v) shift; count=$((count-1)); set -- "$@" -c:v libx264 ;;
                -preset) shift; count=$((count-1)); set -- "$@" -preset veryfast ;;
                -rc) shift; count=$((count-1)) ;;
                -cq) quality=$1; shift; count=$((count-1)); set -- "$@" -crf "$quality" ;;
                *) set -- "$@" "$argument" ;;
            esac
        done
    fi
fi
exec "$VICE_REAL_FFMPEG" "$@"
"#,
    )?;
    let child = Command::new(std::env::current_exe()?)
        .args(["--ignored", "--exact", "trim_child", "--nocapture"])
        .env("VICE_TEST_TRIM_ROOT", root)
        .env("VICE_TEST_TRIM_MODE", mode)
        .env("VICE_TEST_TRIM_LOG", root.join("commands.log"))
        .env("VICE_REAL_FFMPEG", &ffmpeg)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .output()?;
    assert!(
        child.status.success(),
        "{mode}:\n{}\n{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr),
        std::fs::read_to_string(root.join("commands.log")).unwrap_or_default()
    );
    assert_eq!(std::fs::read(&source)?, before_bytes);
    assert_eq!(media::FileIdentity::read(&source)?, before_identity);
    let report: Value = serde_json::from_slice(&std::fs::read(root.join("report.json"))?)?;
    let attempts = report["attempts"].as_array().context("runner commands")?;
    let encoder = |args: &Value| -> String {
        let args: Vec<String> = serde_json::from_value(args.clone()).unwrap();
        encoding::video_encoder(&args).unwrap_or("copy").to_owned()
    };
    assert!(
        attempts.iter().all(|args| encoder(args) != "copy"),
        "a cut between keyframes must skip stream copy"
    );
    if mode == "decode-fails" {
        assert_eq!(attempts.len(), 2);
        assert_eq!(encoder(&attempts[0]), "h264_nvenc");
        assert!(
            attempts[0]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "-hwaccel")
        );
        assert_eq!(encoder(&attempts[1]), "h264_nvenc");
        assert!(
            !attempts[1]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "-hwaccel")
        );
    } else if mode == "encode-fails" {
        assert_eq!(attempts.len(), 3);
        assert_eq!(encoder(&attempts[0]), "h264_nvenc");
        assert_eq!(encoder(&attempts[1]), "h264_nvenc");
        assert_eq!(encoder(&attempts[2]), "libx264");
    } else if let Some(first) = report["hardware"].as_array().unwrap().first() {
        assert_eq!(encoder(&attempts[0]), first.as_str().unwrap());
        assert!(
            attempts[0]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg == "-hwaccel")
        );
        assert_ne!(
            encoder(attempts.last().unwrap()),
            "libx264",
            "simple H264 input must retain a working GPU encoder"
        );
    }
    eprintln!(
        "trim {mode}: hardware={}, attempts={}, duration={}",
        report["hardware"],
        attempts.len(),
        report["duration"]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "spawned by process-isolated trim cases"]
async fn trim_child() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var("VICE_TEST_TRIM_ROOT")?);
    let source = root.join("source.mp4");
    let target = root.join("trim.mp4");
    let before = media::probe_media(&source).await?;
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let log = attempts.clone();
    let tags = HashMap::from([("vice_playlists".into(), "{\"v\":1,\"playlists\":[]}".into())]);
    let result =
        media::trim_range_with_runner(&source, &target, 1.25, 2.75, &before, &tags, move |args| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(args.clone());
                let output = media::run_command(&args, 60.0).await?;
                if !output.status.success() {
                    bail!("{}", String::from_utf8_lossy(&output.stderr));
                }
                Ok(())
            }
        })
        .await?;
    assert!((result.duration - 1.5).abs() < 0.1, "{}", result.duration);
    assert_eq!(result.audio_streams, 2);
    assert_eq!((result.width, result.height), (320, 180));
    assert_eq!(
        media::probe_container_tags(&target)
            .await?
            .get("vice_playlists"),
        tags.get("vice_playlists")
    );
    let ffmpeg = std::env::var("VICE_REAL_FFMPEG")?;
    let frame = Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&target)
        .args([
            "-vf",
            "scale=1:1",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .output()?;
    assert!(frame.status.success());
    assert_eq!(
        frame.stdout.len(),
        30 * 3,
        "the exact selection contains 30 frames"
    );
    assert!(
        frame
            .stdout
            .as_chunks::<3>()
            .0
            .iter()
            .all(|rgb| rgb[0] < 20 && rgb[1] < 20 && rgb[2] > 230),
        "trim must contain exactly the selected blue frames, without earlier/later red frames: {:?}",
        frame.stdout
    );
    let mut crossings = Vec::new();
    for track in [0, 1] {
        let audio = Command::new(&ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&target)
            .args([
                "-map",
                &format!("0:a:{track}"),
                "-t",
                "0.2",
                "-ac",
                "1",
                "-ar",
                "48000",
                "-f",
                "s16le",
                "pipe:1",
            ])
            .output()?;
        assert!(audio.status.success());
        let samples: Vec<_> = audio
            .stdout
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| i16::from_le_bytes([v[0], v[1]]))
            .collect();
        let count = samples
            .windows(2)
            .filter(|v| (v[0] < 0) != (v[1] < 0))
            .count();
        assert!(count > 100, "audio track {track} must contain its tone");
        crossings.push(count);
    }
    let ratio = crossings[1] as f64 / crossings[0] as f64;
    assert!(
        (1.8..2.2).contains(&ratio),
        "both distinct audio tracks must survive: {crossings:?}"
    );
    let hardware: Vec<_> = encoding::video_candidates()
        .await
        .into_iter()
        .filter(|encoder| encoder.is_hardware())
        .map(|encoder| encoder.name().to_owned())
        .collect();
    std::fs::write(
        root.join("report.json"),
        serde_json::to_vec(
            &json!({"attempts":*attempts.lock().unwrap(),"hardware":hardware,"duration":result.duration}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
async fn hevc_trim_keeps_codec_and_limits_reencode_bitrate() -> Result<()> {
    let Some(ffmpeg) = vice::runtime::which("ffmpeg") else {
        return Ok(());
    };
    let encoders = Command::new(&ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()?;
    if !String::from_utf8_lossy(&encoders.stdout).contains("libx265") {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let source = dir.path().join("source.mp4");
    let target = dir.path().join("trim.mp4");
    let result = Command::new(&ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=20:duration=3",
            "-c:v",
            "libx265",
            "-x265-params",
            "pools=none:frame-threads=1:log-level=error:keyint=120",
            "-y",
        ])
        .arg(&source)
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let before = media::probe_media(&source).await?;
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let log = attempts.clone();
    let info = media::trim_range_with_runner(
        &source,
        &target,
        0.55,
        2.05,
        &before,
        &HashMap::new(),
        move |args| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(args.clone());
                let output = media::run_command(&args, 60.0).await?;
                if !output.status.success() {
                    bail!("{}", String::from_utf8_lossy(&output.stderr));
                }
                Ok(())
            }
        },
    )
    .await?;
    assert_eq!(info.vcodec, "hevc");
    let attempts = attempts.lock().unwrap();
    assert!(attempts.iter().any(
        |args| args.iter().any(|arg| arg == "-b:v") && args.iter().any(|arg| arg == "-maxrate")
    ));
    Ok(())
}
