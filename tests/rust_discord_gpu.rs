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
#[tokio::test]
async fn discord_copy_encodes_compatible_file_and_reuses_it_for_clipboard() -> Result<()> {
    let Some(ffmpeg) = vice::runtime::which("ffmpeg") else {
        return Ok(());
    };
    if vice::runtime::which("ffprobe").is_none() {
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    for name in ["bin", "clips", "images", "run"] {
        std::fs::create_dir(root.join(name))?;
    }
    let source = root.join("clips/clip.mp4");
    if let Some(real_source) = std::env::var_os("VICE_TEST_DISCORD_SOURCE") {
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
                "testsrc2=duration=2:size=320x180:rate=30",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "libx264",
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
    let original_size = std::fs::metadata(&source)?.len();
    let original_mtime = std::fs::metadata(&source)?.modified()?;
    let invocation_log = root.join("ffmpeg.log");
    script(
        &root.join("bin/ffmpeg"),
        &format!(
            "#!/bin/sh\nprintf 'START\\t%s\\t%s\\n' \"$$\" \"$*\" >> \"$VICE_TEST_ENCODER_LOG\"\n'{}' \"$@\"\nresult=$?\nprintf 'END\\t%s\\t%s\\n' \"$$\" \"$result\" >> \"$VICE_TEST_ENCODER_LOG\"\nexit \"$result\"\n",
            ffmpeg.to_string_lossy().replace('\'', "'\\''")
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
    assert_eq!(info.vcodec, "h264");
    assert_eq!(info.audio_streams, 1);
    assert!(info.duration > 1.8);
    assert!(std::fs::metadata(&path)?.len() <= vice::editor::DISCORD_MAX_BYTES);
    let encoded_mtime = std::fs::metadata(&path)?.modified()?;
    let log_before = std::fs::read_to_string(&invocation_log)?;
    let source_arg = format!("-i {}", source.display());
    let jobs: Vec<_> = log_before
        .lines()
        .filter(|line| {
            line.starts_with("START\t") && line.contains(&source_arg) && line.contains("-c:v ")
        })
        .collect();
    assert!(!jobs.is_empty(), "Discord API must invoke the encoder");
    let completed = jobs
        .iter()
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
        "Discord API conversion completed with {encoder}; valid H.264/audio copy stays under 20 MB"
    );
    let daemon_log = std::fs::read_to_string(root.join("daemon.log"))?;
    if daemon_log
        .lines()
        .any(|line| line.contains("GPU video encoding available") && line.contains("h264_"))
    {
        assert!(
            jobs[0].contains("-c:v h264_"),
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
        copied_at.elapsed() < Duration::from_secs(5),
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
    client
        .post(format!("{base}/api/quit"))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}
