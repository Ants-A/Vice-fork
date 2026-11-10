//! Exercise the real Rust binary with isolated state and a deliberately unavailable capture backend.
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    time::Duration,
};
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
#[tokio::test]
async fn daemon_api_media_lifecycle_and_public_isolation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for name in ["run", "clips", "images", "bin"] {
        std::fs::create_dir(root.join(name)).unwrap();
    }
    let fake = root.join("bin/gpu-screen-recorder");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'test capture backend is intentionally unavailable' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("clips/test.mp4"), b"test-video-data").unwrap();
    let mut cfg = vice::config::Config::default();
    let port = free_port();
    let public_port = free_port();
    cfg.sharing.port = port;
    cfg.sharing.public_port = Some(public_port);
    cfg.sharing.cloudflare_tunnel = false;
    cfg.recording.backend = "gsr".into();
    cfg.recording.gsr_source = "native".into();
    cfg.output.directory = root.join("clips").to_string_lossy().into();
    cfg.output.image_directory = root.join("images").to_string_lossy().into();
    cfg.discord.enabled = false;
    cfg.updates.check_on_start = false;
    cfg.hotkeys.clip.clear();
    let config = root.join("config.toml");
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let log = std::fs::File::create(root.join("daemon.log")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_vice"))
        .args(["start", "--no-open-ui"])
        .env("VICE_CONFIG", &config)
        .env("VICE_DATA_DIR", root.join("data"))
        .env("VICE_CACHE_DIR", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let mut daemon = Daemon(child);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let public = format!("http://127.0.0.1:{public_port}");
    let mut ready = false;
    for _ in 0..150 {
        if let Ok(response) = client.get(format!("{base}/api/status")).send().await
            && response.status().is_success()
        {
            ready = true;
            break;
        }
        if let Some(exit) = daemon.0.try_wait().unwrap() {
            panic!(
                "daemon exited {exit}: {}",
                std::fs::read_to_string(root.join("daemon.log")).unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        ready,
        "{}",
        std::fs::read_to_string(root.join("daemon.log")).unwrap()
    );
    let status: Value = client
        .get(format!("{base}/api/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["running"], true);
    assert_eq!(status["recording"], false);
    assert_eq!(status["ready"], false);
    assert_eq!(status["gsr"]["source"], "native");
    let clips: Value = client
        .get(format!("{base}/api/clips"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clips["clips"].as_array().unwrap().len(), 1);
    let token = clips["clips"][0]["share_url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    for path in ["/api/config", "/api/quit", "/ws", "/", "/v/test"] {
        assert_eq!(
            client
                .get(format!("{public}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let res = client
        .get(format!("{public}/v/{token}"))
        .header("Range", "bytes=0-3")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 206);
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"test");
    assert_eq!(
        client
            .post(format!("{base}/api/quit"))
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let playlist: Value = client
        .post(format!("{base}/api/playlists"))
        .json(&json!({"name":"Rust clips","emoji":"🦀"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = playlist["playlist"]["id"].as_str().unwrap();
    assert!(
        client
            .post(format!("{base}/api/playlists/{pid}/clips"))
            .json(&json!({"slug":"test"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let renamed: Value = client
        .post(format!("{base}/api/clips/test/rename"))
        .json(&json!({"name":"renamed #1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(renamed["slug"], "renamed #1");
    assert_eq!(
        client
            .get(format!("{public}/v/{token}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let lists: Value = client
        .get(format!("{base}/api/playlists"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(lists["playlists"][0]["clip_slugs"], json!(["renamed #1"]));
    let state: Value = client
        .post(format!("{base}/api/app-state"))
        .json(&json!({"tutorial_done":true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["tutorial_done"], true);
    assert_eq!(
        client
            .get(format!("{base}/api/clips/renamed%20%231/keys"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["keys"],
        Value::Null
    );
    assert!(
        client
            .delete(format!("{base}/api/clips/renamed%20%231"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        client
            .get(format!("{public}/v/{token}"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert!(
        client
            .post(format!("{base}/api/quit"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    for _ in 0..100 {
        if daemon.0.try_wait().unwrap().is_some() {
            assert!(!root.join("run/vice.sock").exists());
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon failed to shut down");
}
