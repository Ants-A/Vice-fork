//! Library scheduling and cache regressions use isolated daemons and counted
//! probe processes. No test changes the environment of another Rust test.
use anyhow::{Context, Result};
use futures_util::future::join_all;
use serde_json::{Value, json};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Library {
    root: tempfile::TempDir,
    child: Child,
    client: reqwest::Client,
    base: String,
}

impl Drop for Library {
    fn drop(&mut self) {
        // A failed assertion must also stop any counted probe/sleep children.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn executable(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

impl Library {
    async fn start(count: usize, legacy_cache: bool, probe_delay: &str) -> Result<Self> {
        let root = tempfile::tempdir()?;
        for name in ["run", "clips", "images", "bin", "cache"] {
            std::fs::create_dir(root.path().join(name))?;
        }
        executable(
            &root.path().join("bin/gpu-screen-recorder"),
            "#!/bin/sh\necho 'recording is unavailable in this test' >&2\nexit 1\n",
        )?;
        executable(
            &root.path().join("bin/ffprobe"),
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$VICE_TEST_PROBE_LOG"
sleep "$VICE_TEST_PROBE_DELAY"
printf '%s\n' '{"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720},{"codec_type":"audio","codec_name":"aac","channels":2}],"format":{"duration":"5.0","tags":{}}}'
"#,
        )?;
        executable(
            &root.path().join("bin/ffmpeg"),
            "#!/bin/sh\nfor last do :; done\n[ \"$last\" = - ] && exit 1\nprintf 'fixture-thumbnail' > \"$last\"\n",
        )?;

        let mut cache = serde_json::Map::new();
        for n in 0..count {
            let path = root.path().join(format!("clips/clip-{n}.mp4"));
            std::fs::write(&path, b"video-fixture")?;
            if legacy_cache {
                let stat = std::fs::metadata(&path)?;
                let mtime = stat.mtime() as i128 * 1_000_000_000 + stat.mtime_nsec() as i128;
                cache.insert(
                    path.to_string_lossy().into_owned(),
                    json!({
                        "identity":[stat.len(),mtime],
                        "tags":{},
                        "meta":{
                            "width":1280,"height":720,"duration":n as f64+2.0,
                            "vcodec":"h264","audio_streams":1,"audio_tracks":[],"tags":{}
                        }
                    }),
                );
            }
        }
        if legacy_cache {
            std::fs::write(
                root.path().join("cache/media.json"),
                serde_json::to_vec(&json!({"version":1,"files":cache}))?,
            )?;
        }

        let port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let mut cfg = vice::config::Config::default();
        cfg.sharing.port = port;
        cfg.sharing.enabled = false;
        cfg.sharing.cloudflare_tunnel = false;
        cfg.output.directory = root.path().join("clips").to_string_lossy().into_owned();
        cfg.output.image_directory = root.path().join("images").to_string_lossy().into_owned();
        cfg.recording.backend = "gsr".into();
        cfg.recording.gsr_source = "native".into();
        cfg.discord.enabled = false;
        cfg.updates.check_on_start = false;
        cfg.hotkeys.clip.clear();
        cfg.output.auto_playlist_by_game = false;
        std::fs::write(root.path().join("config.toml"), toml::to_string(&cfg)?)?;
        let log = std::fs::File::create(root.path().join("daemon.log"))?;
        let child = Command::new(env!("CARGO_BIN_EXE_vice"))
            .current_dir(root.path())
            .args(["start", "--no-open-ui"])
            .env("VICE_CONFIG", root.path().join("config.toml"))
            .env("VICE_DATA_DIR", root.path().join("data"))
            .env("VICE_CACHE_DIR", root.path().join("cache"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("VICE_TEST_PROBE_LOG", root.path().join("probes.log"))
            .env("VICE_TEST_PROBE_DELAY", probe_delay)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .process_group(0)
            .spawn()?;
        let mut library = Self {
            root,
            child,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(4))
                .build()?,
            base: format!("http://127.0.0.1:{port}"),
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if library
                .client
                .get(format!("{}/api/status", library.base))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(library);
            }
            if let Some(status) = library.child.try_wait()? {
                anyhow::bail!(
                    "daemon exited {status}: {}",
                    std::fs::read_to_string(library.root.path().join("daemon.log"))?
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!("daemon never answered")
    }

    async fn get(&self, path: &str) -> Result<Value> {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("read API JSON")
    }

    fn full_probes(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("probes.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("-show_streams"))
            .map(str::to_owned)
            .collect()
    }
}

#[tokio::test]
async fn cached_discord_copy_does_not_wait_for_cold_metadata_or_busy_probes() -> Result<()> {
    use sha2::{Digest, Sha256};
    let library = Library::start(2, false, "2").await?;
    let source = library.root.path().join("clips/clip-0.mp4");
    let identity = vice::media::FileIdentity::read(&source)?;
    let prefix = format!(
        "{:x}",
        Sha256::digest(source.as_os_str().as_encoded_bytes())
    );
    let cache = library.root.path().join("cache/discord");
    std::fs::create_dir_all(&cache)?;
    let ready = cache.join(format!(
        "{}-{}-{}-{}.mp4",
        &prefix[..24],
        identity.size,
        identity.mtime_ns,
        identity.inode
    ));
    std::fs::write(&ready, b"completed-discord-copy")?;

    // Hydration owns one probe. A detail request fills the other slot. The
    // ready copy must bypass both the source's metadata lock and probe queue.
    let pending: Vec<_> = (0..2)
        .map(|number| {
            let client = library.client.clone();
            let detail = format!("{}/api/clips/clip-{number}", library.base);
            tokio::spawn(async move { client.get(detail).send().await })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(1);
    while library.full_probes().len() < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        library.full_probes().len(),
        2,
        "both cold metadata probes must be in progress"
    );
    let started = Instant::now();
    let result = library.get("/api/clips/clip-0/discord").await?;
    let elapsed = started.elapsed();
    for request in pending {
        request.abort();
    }
    assert!(
        elapsed < Duration::from_millis(250),
        "an existing local copy waited {elapsed:?} for unrelated probing"
    );
    assert_eq!(result["path"], ready.to_string_lossy().as_ref());
    assert_eq!(result["size"], b"completed-discord-copy".len());
    assert_eq!(
        library.full_probes().len(),
        2,
        "ready Discord copies must not start another probe"
    );
    Ok(())
}

#[tokio::test]
async fn legacy_cache_gives_first_list_durations_without_websocket() -> Result<()> {
    let library = Library::start(24, true, "0.2").await?;
    for _ in 0..4 {
        let response = library.get("/api/clips").await?;
        let clips = response["clips"].as_array().context("clip list")?;
        assert_eq!(clips.len(), 24);
        for clip in clips {
            let number: usize = clip["slug"]
                .as_str()
                .context("slug")?
                .strip_prefix("clip-")
                .context("fixture prefix")?
                .parse()?;
            assert_eq!(clip["duration"], number as f64 + 2.0);
        }
    }
    assert!(
        library.full_probes().is_empty(),
        "unchanged persisted metadata must not launch fresh probes"
    );
    assert!(
        std::fs::read_to_string(library.root.path().join("probes.log"))
            .unwrap_or_default()
            .is_empty(),
        "playlist recovery must reuse persisted container tags"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_detail_requests_share_one_probe_and_warm_lists() -> Result<()> {
    let library = Library::start(1, false, "0.25").await?;
    let requests = (0..12).map(|_| library.get("/api/clips/clip-0"));
    for response in join_all(requests).await {
        assert_eq!(response?["duration"], 5.0);
    }
    for _ in 0..4 {
        assert_eq!(
            library.get("/api/clips").await?["clips"][0]["duration"],
            5.0
        );
    }
    assert_eq!(
        library.full_probes().len(),
        1,
        "foreground requests and background hydration must coalesce"
    );
    let path = library.root.path().join("clips/clip-0.mp4");
    let cache_path = library.root.path().join("cache/media.json");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let cache: Value = std::fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        if cache["files"][path.to_string_lossy().as_ref()]["meta"]["duration"] == 5.0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "completed metadata was not persisted for the next daemon start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

#[tokio::test]
async fn thumbnail_does_not_wait_for_metadata_queue() -> Result<()> {
    let library = Library::start(12, false, "1.0").await?;
    let client = library.client.clone();
    let base = library.base.clone();
    let details = tokio::spawn(async move {
        join_all((0..6).map(|n| client.get(format!("{base}/api/clips/clip-{n}")).send())).await
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    while library.full_probes().is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !library.full_probes().is_empty(),
        "probe queue never started"
    );
    let began = Instant::now();
    let response = library
        .client
        .get(format!("{}/t/clip-11", library.base))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(response.bytes().await?.as_ref(), b"fixture-thumbnail");
    assert!(
        began.elapsed() < Duration::from_millis(700),
        "a thumbnail queued behind one-second metadata probes: {:?}",
        began.elapsed()
    );
    details.abort();
    Ok(())
}

#[tokio::test]
async fn playlist_artwork_does_not_probe_or_remux_the_video_library() -> Result<()> {
    let library = Library::start(3, true, "0.05").await?;
    let created: Value = library
        .client
        .post(format!("{}/api/playlists", library.base))
        .json(&json!({"name":"Artwork","icon":"auto"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let pid = created["playlist"]["id"].as_str().context("playlist id")?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let log_path = library.root.path().join("probes.log");
    assert!(
        std::fs::read_to_string(&log_path)
            .unwrap_or_default()
            .is_empty(),
        "creating an empty playlist must not probe every recording"
    );
    library
        .client
        .post(format!("{}/api/playlists/{pid}/clips", library.base))
        .json(&json!({"slug":"clip-0"}))
        .send()
        .await?
        .error_for_status()?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !before.is_empty(),
        "adding a member must update its portable membership"
    );
    assert!(
        !before.contains("clip-1.mp4") && !before.contains("clip-2.mp4"),
        "membership updates should touch only affected clips: {before}"
    );
    let updated: Value = library
        .client
        .patch(format!("{}/api/playlists/{pid}", library.base))
        .json(&json!({"icon":"","name":"Artwork"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(updated["playlist"]["icon"], "");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        std::fs::read_to_string(&log_path).unwrap_or_default(),
        before,
        "choosing a playlist icon must not read or rewrite its recordings"
    );
    Ok(())
}

#[tokio::test]
async fn cold_thumbnails_queue_and_cache_instead_of_timing_out() -> Result<()> {
    let library = Library::start(8, true, "0").await?;
    executable(
        &library.root.path().join("bin/ffmpeg"),
        r#"#!/bin/sh
for last do :; done
[ "$last" = - ] && exit 1
sleep 0.15
printf 'fixture-thumbnail' > "$last"
"#,
    )?;
    let clips = library.get("/api/clips").await?;
    let urls: Vec<_> = clips["clips"]
        .as_array()
        .context("clips")?
        .iter()
        .map(|clip| clip["thumb_url"].as_str().unwrap().to_owned())
        .collect();
    let requests = urls
        .iter()
        .map(|url| library.client.get(format!("{}{url}", library.base)).send());
    for response in join_all(requests).await {
        let response = response?;
        assert!(
            response.status().is_success(),
            "cold thumbnail was rejected: {}",
            response.status()
        );
        assert_eq!(
            response.headers()["cache-control"],
            "private, max-age=31536000, immutable"
        );
        assert_eq!(response.bytes().await?.as_ref(), b"fixture-thumbnail");
    }
    let stale = library
        .client
        .get(format!("{}/t/clip-0?v=stale", library.base))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(stale.headers()["cache-control"], "no-store");
    Ok(())
}

#[tokio::test]
async fn missing_thumbnails_resume_soon_after_playback_finishes() -> Result<()> {
    let library = Library::start(1, true, "0").await?;
    let video = library
        .client
        .get(format!("{}/v/clip-0", library.base))
        .send()
        .await?
        .error_for_status()?;
    let _ = video.bytes().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let response = library
        .client
        .get(format!("{}/t/clip-0", library.base))
        .send()
        .await?;
    assert!(
        response.status().is_success(),
        "completed playback must not block new thumbnails for four seconds: {}",
        response.status()
    );
    Ok(())
}

#[tokio::test]
async fn portable_tag_writes_wait_for_playback_network_reads() -> Result<()> {
    let library = Library::start(1, true, "0").await?;
    let source = library.root.path().join("clips/clip-0.mp4");
    std::fs::File::options()
        .write(true)
        .open(source)?
        .set_len(64 * 1024 * 1024)?;
    let video = library
        .client
        .get(format!("{}/v/clip-0", library.base))
        .send()
        .await?
        .error_for_status()?;
    // Keep the body unread so HTTP backpressure leaves this video stream
    // active while the playlist change is applied to the client immediately.
    let playlist: Value = library
        .client
        .post(format!("{}/api/playlists", library.base))
        .json(&json!({"name":"While watching"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let pid = playlist["playlist"]["id"].as_str().context("playlist id")?;
    library
        .client
        .post(format!("{}/api/playlists/{pid}/clips", library.base))
        .json(&json!({"slug":"clip-0"}))
        .send()
        .await?
        .error_for_status()?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        std::fs::read_to_string(library.root.path().join("probes.log"))
            .unwrap_or_default()
            .is_empty(),
        "background tag work must not start source probes/remuxes while playback is downloading"
    );
    drop(video);
    Ok(())
}
