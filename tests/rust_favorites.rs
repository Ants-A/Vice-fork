//! Favorites survive a fresh daemon with a different data/cache directory.
use serde_json::{Value, json};
use std::{
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
fn port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
async fn start(root: &std::path::Path, n: u8) -> (Daemon, String) {
    let mut cfg = vice::config::Config::default();
    cfg.sharing.port = port();
    cfg.sharing.cloudflare_tunnel = false;
    cfg.recording.backend = "gsr".into();
    cfg.recording.gsr_source = "native".into();
    cfg.output.directory = root.join("clips").to_string_lossy().into();
    cfg.output.image_directory = root.join("images").to_string_lossy().into();
    cfg.discord.enabled = false;
    cfg.updates.check_on_start = false;
    cfg.hotkeys.clip.clear();
    let config = root.join(format!("config{n}.toml"));
    std::fs::write(&config, toml::to_string(&cfg).unwrap()).unwrap();
    let run = root.join(format!("run{n}"));
    std::fs::create_dir(&run).unwrap();
    let base = format!("http://127.0.0.1:{}", cfg.sharing.port);
    let child = Command::new(env!("CARGO_BIN_EXE_vice"))
        .args(["start", "--no-open-ui"])
        .env("VICE_CONFIG", config)
        .env("VICE_DATA_DIR", root.join(format!("data{n}")))
        .env("VICE_CACHE_DIR", root.join(format!("cache{n}")))
        .env("XDG_RUNTIME_DIR", run)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let daemon = Daemon(child);
    for _ in 0..150 {
        if reqwest::get(format!("{base}/api/status")).await.is_ok() {
            return (daemon, base);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon did not start");
}
#[tokio::test]
async fn favorites_are_read_from_files_by_a_fresh_device() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("clips")).unwrap();
    std::fs::create_dir(root.join("images")).unwrap();
    for (name, codec) in [
        ("clips/example.mp4", "mpeg4"),
        ("images/example.png", "png"),
        ("images/other.jpg", "mjpeg"),
    ] {
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=size=16x16:rate=1",
                "-frames:v",
                "1",
                "-threads",
                "1",
                "-c:v",
                codec,
                "-y",
            ])
            .arg(root.join(name))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let (_first, first_base) = start(root, 1).await;
    let client = reqwest::Client::new();
    for endpoint in ["clips/example", "images/example", "images/other"] {
        let response = client
            .post(format!("{first_base}/api/{endpoint}/favorite"))
            .json(&json!({"favorite":true}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{}",
            response.text().await.unwrap()
        );
    }
    let (_second, base) = start(root, 2).await;
    let clip: Value = client
        .get(format!("{base}/api/clips/example"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clip["favorite"], true);
    let images: Value = client
        .get(format!("{base}/api/images"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(images["images"].as_array().unwrap().len(), 2);
    assert!(
        images["images"]
            .as_array()
            .unwrap()
            .iter()
            .all(|image| image["favorite"] == true)
    );
    let response = client
        .post(format!("{base}/api/images/example/favorite"))
        .json(&json!({"favorite":false}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let image: Value = client
        .get(format!("{base}/api/images/example"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(image["favorite"], false);
    let clip: Value = client
        .get(format!("{base}/api/clips/example"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clip["favorite"], true);
    // Both daemons remain running: a rescan must discard the old cached tags.
    let response = client
        .post(format!("{first_base}/api/clips/example/favorite"))
        .json(&json!({"favorite":false}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let mut updated = false;
    for _ in 0..30 {
        client
            .post(format!("{base}/api/library/rescan"))
            .send()
            .await
            .unwrap();
        let clips: Value = client
            .get(format!("{base}/api/clips"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if clips["clips"][0]["favorite"] == false {
            updated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(updated, "second daemon retained stale favorite metadata");
}
