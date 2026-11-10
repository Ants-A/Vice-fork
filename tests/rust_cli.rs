//! CLI behavior is verified in subprocesses with separate config/runtime/data.
use std::{
    path::Path,
    process::{Command, Output},
};
fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vice"));
    cmd.env("VICE_CONFIG", root.join("config.toml"))
        .env("VICE_DATA_DIR", root.join("data"))
        .env("VICE_CACHE_DIR", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("VICE_INSTALL_PREFIX", root.join("installation"))
        .env("DISPLAY", ":vice-test")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("FLATPAK_ID")
        .env_remove("INVOCATION_ID")
        .env_remove("JOURNAL_STREAM");
    cmd
}
fn output(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().unwrap()
}
fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
#[test]
fn version_help_and_keys_do_not_need_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let version = output(dir.path(), &["--version"]);
    assert!(version.status.success());
    assert!(text(&version).contains(vice::VERSION));
    let help = output(dir.path(), &["start", "--help"]);
    assert!(help.status.success());
    assert!(text(&help).contains("--no-open-ui"));
    let keys = output(dir.path(), &["list-keys", "--filter", "f9"]);
    assert!(keys.status.success());
    let names = String::from_utf8_lossy(&keys.stdout);
    assert!(names.lines().any(|s| s == "KEY_F9"));
    assert!(!names.lines().any(|s| s == "KEY_F8"));
    assert!(!dir.path().join("config.toml").exists());
}
#[test]
fn config_roundtrips_and_clips_handles_missing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = output(dir.path(), &["config"]);
    assert!(cfg.status.success(), "{}", text(&cfg));
    let mut value: toml::Value = toml::from_str(&String::from_utf8_lossy(&cfg.stdout)).unwrap();
    assert_eq!(value["recording"]["clip_duration"].as_integer(), Some(20));
    value["output"]["directory"] = toml::Value::String(
        dir.path()
            .join("absent-clips")
            .to_string_lossy()
            .into_owned(),
    );
    std::fs::write(
        dir.path().join("config.toml"),
        toml::to_string(&value).unwrap(),
    )
    .unwrap();
    let clips = output(dir.path(), &["clips"]);
    assert!(clips.status.success());
    assert!(text(&clips).contains("No clips directory found"));
}
#[test]
fn invalid_duration_is_reported_before_daemon_connection() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["clip", "0"],
        vec!["clip", "-d", "0"],
        vec!["clip", "1801"],
        vec!["clip", "20", "-d", "30"],
    ] {
        let out = output(dir.path(), &args);
        assert!(!out.status.success());
        assert!(
            !text(&out).contains("Vice is not running"),
            "{}",
            text(&out)
        );
    }
    let out = output(dir.path(), &["clip", "20", "-d", "20"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("Vice is not running"), "{}", text(&out));
}
#[test]
fn missing_daemon_and_doctor_are_actionable() {
    let dir = tempfile::tempdir().unwrap();
    let out = output(dir.path(), &["status"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("Vice is not running"));
    let doctor = output(dir.path(), &["doctor"]);
    assert!(doctor.status.success(), "{}", text(&doctor));
    for field in [
        "Vice ", "Config:", "Socket:", "ffmpeg:", "Hotkeys:", "Daemon:",
    ] {
        assert!(text(&doctor).contains(field));
    }
}
#[test]
fn uninstall_removes_only_install_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("installation");
    for name in [
        "bin/vice",
        "bin/vice-app",
        "share/applications/vice.desktop",
        "share/icons/hicolor/scalable/apps/vice.svg",
        "share/vice/ui/index.html",
        "share/vice/fonts/font.ttf",
        "share/vice/data/games.json",
    ] {
        let path = prefix.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"installed").unwrap();
    }
    let preserved = dir.path().join("data/playlists.json");
    std::fs::create_dir_all(preserved.parent().unwrap()).unwrap();
    std::fs::write(&preserved, b"keep").unwrap();
    let out = output(dir.path(), &["uninstall", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!prefix.join("bin/vice").exists());
    assert!(!prefix.join("share/vice/ui").exists());
    assert_eq!(std::fs::read(preserved).unwrap(), b"keep");
}
