//! Replay game identity survives focus/workspace changes during finalization.
//! The test daemon uses isolated state, synthetic media, and a fake recorder.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
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
fn script(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
async fn get(client: &reqwest::Client, base: &str, path: &str) -> Result<Value> {
    Ok(client
        .get(format!("{base}{path}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}
async fn next_clip(client: &reqwest::Client, base: &str, seen: &HashSet<String>) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        let response = get(client, base, "/api/clips").await?;
        if let Some(clip) = response["clips"]
            .as_array()
            .context("clip list")?
            .iter()
            .find(|clip| {
                clip["slug"]
                    .as_str()
                    .is_some_and(|slug| !seen.contains(slug))
                    && clip["duration"]
                        .as_f64()
                        .is_some_and(|duration| duration > 0.5)
            })
        {
            return Ok(clip.clone());
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    bail!("new finalized clip did not appear")
}

#[tokio::test]
async fn first_game_clip_tags_after_workspace_switch_and_old_context_expires() -> Result<()> {
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
    let fixture = root.join("fixture.mp4");
    let media = Command::new(ffmpeg)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=blue:size=160x90:rate=10:duration=1",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            "-y",
        ])
        .arg(&fixture)
        .output()?;
    assert!(
        media.status.success(),
        "{}",
        String::from_utf8_lossy(&media.stderr)
    );
    let focus = root.join("focus");
    let queries = root.join("focus-queries");
    std::fs::write(&focus, "game")?;
    script(
        &root.join("bin/hyprctl"),
        r#"#!/bin/sh
if [ "$1" = activewindow ]; then
    focus=$(cat "$VICE_TEST_FOCUS")
    printf '%s\n' "$focus" >> "$VICE_TEST_FOCUS_QUERIES"
    case "$focus" in
        game) printf '%s\n' '{"class":"Cyberpunk2077.exe","pid":0,"size":[160,90]}' ;;
        *) printf '%s\n' '{"class":"test-terminal","pid":0,"size":[160,90]}' ;;
    esac
else
    printf '%s\n' '[]'
fi
"#,
    )?;
    script(
        &root.join("bin/gpu-screen-recorder"),
        r#"#!/bin/sh
case "$1" in
    --info) printf 'section=video_codecs\nh264\n'; exit 0 ;;
    --help) printf '%s\n' '-w -f -r -c -a -o'; exit 0 ;;
    --list-*) exit 0 ;;
esac
number=0
save_replay() {
    number=$((number+1))
    printf terminal > "$VICE_TEST_FOCUS"
    printf '%s\n' "$number" >> "$VICE_TEST_SIGNALS"
    sleep 0.1
    cp "$VICE_TEST_FIXTURE" "$VICE_TEST_CLIPS/2026-replay-$number.mp4"
}
trap save_replay USR1
trap 'exit 0' INT TERM
while :; do sleep 0.05 & wait $!; done
"#,
    )?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let mut cfg = vice::config::Config::default();
    cfg.sharing.port = port;
    cfg.sharing.enabled = false;
    cfg.sharing.cloudflare_tunnel = false;
    cfg.recording.backend = "gsr".into();
    cfg.recording.gsr_source = "native".into();
    cfg.recording.capture_audio = false;
    cfg.recording.clip_duration = 5;
    cfg.recording.buffer_duration = 5;
    cfg.recording.follow_mouse_display = false;
    cfg.recording.window_capture = false;
    cfg.output.directory = root.join("clips").to_string_lossy().into();
    cfg.output.image_directory = root.join("images").to_string_lossy().into();
    cfg.output.auto_playlist_by_game = true;
    cfg.output.tag_clips_with_game = true;
    cfg.discord.enabled = false;
    cfg.updates.check_on_start = false;
    cfg.hotkeys.clip.clear();
    cfg.notifications.sound_volume = 0.0;
    std::fs::write(root.join("config.toml"), toml::to_string(&cfg)?)?;
    let log = std::fs::File::create(root.join("daemon.log"))?;
    let child = Command::new(env!("CARGO_BIN_EXE_vice"))
        .args(["start", "--no-open-ui"])
        .env("VICE_CONFIG", root.join("config.toml"))
        .env("VICE_DATA_DIR", root.join("data"))
        .env("VICE_CACHE_DIR", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("HYPRLAND_INSTANCE_SIGNATURE", "vice-tagging-test")
        .env("VICE_TEST_FOCUS", &focus)
        .env("VICE_TEST_FOCUS_QUERIES", &queries)
        .env("VICE_TEST_SIGNALS", root.join("signals"))
        .env("VICE_TEST_FIXTURE", &fixture)
        .env("VICE_TEST_CLIPS", root.join("clips"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env_remove("INVOCATION_ID")
        .env_remove("JOURNAL_STREAM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()?;
    let mut daemon = Daemon(child);
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()?;
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut ready = false;
    while Instant::now() < deadline {
        if let Ok(status) = get(&client, &base, "/api/status").await
            && status["ready"] == true
            && std::fs::read_to_string(&queries)
                .unwrap_or_default()
                .contains("game")
        {
            ready = true;
            break;
        }
        if let Some(exit) = daemon.0.try_wait()? {
            bail!(
                "daemon exited {exit}: {}",
                std::fs::read_to_string(root.join("daemon.log"))?
            );
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        ready,
        "{}",
        std::fs::read_to_string(root.join("daemon.log"))?
    );
    assert_eq!(
        get(&client, &base, "/api/playlists").await?["playlists"],
        json!([])
    );
    let mut seen = HashSet::new();
    client
        .post(format!("{base}/api/trigger"))
        .send()
        .await?
        .error_for_status()?;
    let first = next_clip(&client, &base, &seen).await?;
    assert_eq!(
        std::fs::read_to_string(&focus)?,
        "terminal",
        "focus must change before the file is finalized"
    );
    assert_eq!(
        first["game"], "Cyberpunk 2077",
        "first clip must create an automatic game playlist even if saving finishes on another workspace"
    );
    assert!(
        first["name"]
            .as_str()
            .context("filename")?
            .contains("Cyberpunk"),
        "filename and playlist must share the captured game: {first}"
    );
    let first_slug = first["slug"].as_str().context("slug")?.to_owned();
    seen.insert(first_slug.clone());
    let lists = get(&client, &base, "/api/playlists").await?;
    let playlists = lists["playlists"].as_array().context("playlists")?;
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0]["kind"], "auto");
    assert_eq!(playlists[0]["game"], "Cyberpunk 2077");
    assert_eq!(playlists[0]["clip_slugs"], json!([first_slug]));

    // Save on the other workspace while the replay still contains the game.
    client
        .post(format!("{base}/api/trigger"))
        .send()
        .await?
        .error_for_status()?;
    let second = next_clip(&client, &base, &seen).await?;
    assert_eq!(
        second["game"], "Cyberpunk 2077",
        "recent replay game should survive a workspace switch before the trigger"
    );
    seen.insert(second["slug"].as_str().context("second slug")?.to_owned());
    assert_eq!(
        get(&client, &base, "/api/playlists").await?["playlists"][0]["clip_slugs"]
            .as_array()
            .context("members")?
            .len(),
        2
    );

    // Non-game focus samples must not keep refreshing the previous game.
    tokio::time::sleep(Duration::from_secs(6)).await;
    client
        .post(format!("{base}/api/trigger"))
        .send()
        .await?
        .error_for_status()?;
    let third = next_clip(&client, &base, &seen).await?;
    assert!(
        third["game"].is_null(),
        "an old game outside the requested five-second replay must expire: {third}"
    );
    assert!(
        !third["name"]
            .as_str()
            .context("third filename")?
            .contains("Cyberpunk")
    );
    assert_eq!(
        get(&client, &base, "/api/playlists").await?["playlists"][0]["clip_slugs"]
            .as_array()
            .context("members")?
            .len(),
        2
    );
    Ok(())
}
