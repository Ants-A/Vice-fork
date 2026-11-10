//! End-to-end coverage using the real FFmpeg binaries when installed.
use anyhow::Result;
use serde_json::json;
use std::{collections::HashMap, path::Path};
use vice::{editor, media};

fn available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok()
        && std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .is_ok()
}
async fn make_video(path: &Path, fragmented: bool) -> Result<()> {
    let mut args = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc=duration=1:size=128x72:rate=10".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "sine=frequency=440:duration=1".into(),
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
    ];
    if fragmented {
        args.extend(["-movflags".into(), "frag_keyframe+empty_moov".into()]);
    }
    args.extend(["-y".into(), path.to_string_lossy().into_owned()]);
    let out = media::run_command(&args, 30.0).await?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}
#[tokio::test]
async fn regular_and_fragmented_mp4_have_real_duration() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    for fragmented in [false, true] {
        let path = tmp.path().join(format!("clip-{fragmented}.mp4"));
        make_video(&path, fragmented).await?;
        let m = media::probe_media(&path).await?;
        assert_eq!((m.width, m.height), (128, 72));
        assert!(m.duration > 0.9);
        assert_eq!(m.audio_streams, 1);
    }
    let junk = tmp.path().join("junk.mp4");
    std::fs::write(&junk, b"this is not a video")?;
    assert!(media::probe_media(&junk).await.is_err());
    Ok(())
}
#[tokio::test]
async fn playlist_tags_survive_remux_and_preserve_timestamp() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    for ext in ["mp4", "mkv"] {
        let clip = tmp.path().join(format!("clip.{ext}"));
        make_video(&clip, false).await?;
        let timestamp =
            std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
        std::fs::File::options()
            .write(true)
            .open(&clip)?
            .set_times(std::fs::FileTimes::new().set_modified(timestamp))?;
        let payload = json!({"v":1,"playlists":[{"id":"pl-1","name":"Best","emoji":"🏆","rev":1}]})
            .to_string();
        let tags = HashMap::from([("vice_playlists".into(), payload.clone())]);
        assert!(media::write_metadata_tags(&clip, &tags, 30.0, None).await?);
        assert_eq!(std::fs::metadata(&clip)?.modified()?, timestamp);
        assert_eq!(
            media::probe_container_tags(&clip)
                .await?
                .get("vice_playlists"),
            Some(&payload)
        );
        assert!(media::probe_media(&clip).await?.duration > 0.9);
    }
    Ok(())
}
#[tokio::test]
async fn editor_renders_multitrack_video_and_detached_audio() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    let input = tmp.path().join("input.mp4");
    make_video(&input, false).await?;
    let sources = HashMap::from([(
        "input".into(),
        editor::Source {
            path: input,
            duration: 1.0,
            width: 128,
            height: 72,
            has_audio: true,
            audio_streams: 1,
        },
    )]);
    let raw = json!({"tracks":[{"id":"T1","type":"text"},{"id":"V2","type":"video"},{"id":"V1","type":"video"},{"id":"A1","type":"audio"}],"items":[
        {"id":"base","kind":"clip","trackId":"V1","clipId":"input","start":0,"dur":1,"offset":0,"muted":true},
        {"id":"top","kind":"clip","trackId":"V2","clipId":"input","start":0.25,"dur":0.5,"offset":0},
        {"id":"audio","kind":"audio","trackId":"A1","clipId":"input","start":0,"dur":1,"offset":0,"audioStream":0,"volume":0.5}
    ]});
    let (project, errors) = editor::validate_project(&raw, &sources);
    assert!(errors.is_empty(), "{errors:?}");
    let output = tmp.path().join("export.mp4");
    let cmd = editor::build_export_cmd(
        &project,
        &sources,
        &output,
        "#0099ff",
        &editor::font_dir(),
        tmp.path(),
        false,
    )?;
    // Exercise the same hardware-first policy as the real editor export.
    vice::encoding::run_video_job(&cmd, std::time::Duration::from_secs(60)).await?;
    let info = media::probe_media(&output).await?;
    assert_eq!((info.width, info.height), (128, 72));
    assert!(info.duration > 0.9);
    assert_eq!(info.audio_streams, 1);
    Ok(())
}

#[tokio::test]
async fn trim_between_keyframes_starts_at_selected_frame_and_keeps_audio() -> Result<()> {
    if !available() {
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    let input = tmp.path().join("input.mp4");
    make_video(&input, false).await?;
    let output = tmp.path().join("trim.mp4");
    media::trim_range(&input, &output, 0.25, 0.75).await?;
    let info = media::probe_media(&output).await?;
    assert!(
        (info.duration - 0.5).abs() < 0.15,
        "trim duration {}",
        info.duration
    );
    assert_eq!(info.audio_streams, 1);
    assert!(
        media::probe_media(&input).await?.duration > 0.9,
        "original remains unchanged"
    );
    assert!(
        media::trim_range(&input, &tmp.path().join("invalid.mp4"), 0.0, 3.0)
            .await
            .is_err()
    );
    Ok(())
}
