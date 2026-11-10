//! Session environment, paths, daemon ownership and Flatpak host commands.
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    ffi::CStr,
    fs::{File, OpenOptions},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        io::AsRawFd,
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub const VICE_FLATPAK_ID: &str = "io.github.splitshotz.Vice";
pub fn actual_home_dir() -> PathBuf {
    // getpwuid_r avoids the shared static storage of getpwuid.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0u8; 16384];
        if libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        ) == 0
            && !result.is_null()
            && !pwd.pw_dir.is_null()
        {
            return PathBuf::from(CStr::from_ptr(pwd.pw_dir).to_string_lossy().into_owned());
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
pub fn data_dir() -> PathBuf {
    std::env::var_os("VICE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| actual_home_dir().join(".local/share/vice"))
}
pub fn cache_dir() -> PathBuf {
    std::env::var_os("VICE_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| actual_home_dir().join(".cache/vice"))
}
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}
pub fn socket_path() -> PathBuf {
    runtime_dir().join("vice.sock")
}
pub fn pid_path() -> PathBuf {
    runtime_dir().join("vice.pid")
}
pub fn resolve_path(path: impl AsRef<str>) -> PathBuf {
    let home = actual_home_dir().to_string_lossy().into_owned();
    let text = path.as_ref();
    let text = if text == "~" {
        home.clone()
    } else if let Some(suffix) = text.strip_prefix("~/") {
        format!("{home}/{suffix}")
    } else {
        text.to_owned()
    };
    let text = text.replace("${HOME}", &home).replace("$HOME", &home);
    let regex =
        regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    PathBuf::from(
        regex
            .replace_all(&text, |caps: &regex::Captures<'_>| {
                std::env::var(caps.get(1).or_else(|| caps.get(2)).unwrap().as_str())
                    .unwrap_or_else(|_| caps[0].to_owned())
            })
            .into_owned(),
    )
}
pub fn in_vice_flatpak() -> bool {
    std::env::var("FLATPAK_ID").is_ok_and(|s| s == VICE_FLATPAK_ID)
}
pub fn which(tool: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if tool.contains('/') {
        let p = PathBuf::from(tool);
        return p
            .metadata()
            .ok()
            .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .map(|_| p);
    }
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(tool))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}
pub fn host_argv(argv: &[String], watch: bool) -> Vec<String> {
    if !in_vice_flatpak() {
        return argv.to_vec();
    }
    let mut out = vec!["flatpak-spawn".into(), "--host".into()];
    if watch {
        out.push("--watch-bus".into());
    }
    for key in [
        "HYPRLAND_INSTANCE_SIGNATURE",
        "SWAYSOCK",
        "NIRI_SOCKET",
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
    ] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
        {
            out.push(format!("--env={key}={value}"));
        }
    }
    out.extend_from_slice(argv);
    out
}
pub fn host_command(argv: &[String]) -> Command {
    let args = host_argv(argv, false);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd
}
pub fn host_which(tool: &str) -> bool {
    if !in_vice_flatpak() {
        return which(tool).is_some();
    }
    let args = ["sh", "-c", "command -v \"$1\" >/dev/null", "sh", tool].map(String::from);
    host_command(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
pub fn read_host_proc(pid: u32, name: &str) -> Option<Vec<u8>> {
    if name.contains('/') || name.contains("..") {
        return None;
    }
    let path = format!("/proc/{pid}/{name}");
    if !in_vice_flatpak() {
        return std::fs::read(path).ok();
    }
    host_command(&["cat".into(), path])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}
pub fn daemon_is_running(socket: &Path, pid: &Path) -> bool {
    match std::fs::read_to_string(pid) {
        Ok(value) => {
            if let Ok(pid) = value.trim().parse::<i32>()
                && pid > 0
                && (unsafe { libc::kill(pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
            {
                return true;
            }
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return true,
        _ => {}
    }
    match UnixStream::connect(socket) {
        Ok(_) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
    }
}
pub fn claim_daemon_lock(socket: &Path) -> Result<File> {
    claim_daemon_lock_timeout(socket, Duration::ZERO)
}
pub fn claim_daemon_lock_timeout(socket: &Path, timeout: Duration) -> Result<File> {
    let path = socket.with_extension("lock");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error.into());
        }
        if Instant::now() >= deadline {
            bail!("Vice is already starting or running.");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
pub const RUNTIME_ENV_KEYS: &[&str] = &[
    "HOME",
    "XDG_RUNTIME_DIR",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XAUTHORITY",
    "KDE_SESSION_VERSION",
];
pub fn runtime_env_snapshot() -> BTreeMap<String, String> {
    RUNTIME_ENV_KEYS
        .iter()
        .map(|k| ((*k).to_owned(), std::env::var(k).unwrap_or_default()))
        .collect()
}
pub fn user_systemd_env_snapshot() -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    if let Ok(out) = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((key, value)) = line.split_once('=')
                && key != "HOME"
                && RUNTIME_ENV_KEYS.contains(&key)
                && !value.is_empty()
            {
                values.insert(key.into(), value.into());
            }
        }
    }
    values
}
fn needs_expansion(key: &str) -> bool {
    std::env::var(key).map_or(true, |v| {
        v.is_empty() || v.contains("${") || v.contains("$(")
    })
}
pub fn load_user_systemd_env() -> Vec<String> {
    let mut filled = vec![];
    for (key, value) in user_systemd_env_snapshot() {
        if needs_expansion(&key) {
            unsafe { std::env::set_var(&key, value) };
            filled.push(key);
        }
    }
    filled
}
pub fn has_display() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()))
}
pub fn running_under_systemd() -> bool {
    std::env::var_os("INVOCATION_ID").is_some() || std::env::var_os("JOURNAL_STREAM").is_some()
}
pub fn recover_wayland_display() -> bool {
    if std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty()) {
        return true;
    }
    for dir in [
        runtime_dir(),
        PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
        PathBuf::from(format!("/tmp/wayland-{}", unsafe { libc::getuid() })),
    ] {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                if entry.file_name().to_string_lossy().starts_with("wayland-")
                    && entry.file_type().is_ok_and(|t| t.is_socket())
                {
                    unsafe {
                        std::env::set_var("WAYLAND_DISPLAY", entry.file_name());
                        std::env::set_var("XDG_RUNTIME_DIR", &dir);
                    }
                    return true;
                }
            }
        }
    }
    false
}
pub fn normalize_runtime_environment() {
    if needs_expansion("HOME") {
        unsafe {
            std::env::set_var("HOME", actual_home_dir());
        }
    }
    if needs_expansion("XDG_RUNTIME_DIR") {
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", format!("/run/user/{}", libc::getuid()));
        }
    }
    if !has_display() {
        load_user_systemd_env();
        if !has_display() {
            recover_wayland_display();
        }
    }
}
pub async fn wait_for_display(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !has_display() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(2)).await;
        load_user_systemd_env();
        recover_wayland_display();
    }
    has_display()
}
pub fn systemd_unit_loaded(unit: &str) -> bool {
    if in_vice_flatpak() {
        return false;
    }
    Command::new("systemctl")
        .args(["--user", "show", unit, "-p", "LoadState", "--value"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "loaded")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn placeholder_resolution() {
        assert_eq!(
            resolve_path("~/Videos/Vice"),
            actual_home_dir().join("Videos/Vice")
        );
        assert_eq!(
            resolve_path("${HOME}/Videos"),
            actual_home_dir().join("Videos")
        );
    }
    #[test]
    fn exclusive_lock_lifetime() {
        let dir = std::env::temp_dir().join(format!("vice-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("vice.sock");
        let lock = claim_daemon_lock(&socket).unwrap();
        assert!(claim_daemon_lock(&socket).is_err());
        drop(lock); /* Other parallel tests spawn subprocesses: an in-flight fork may inherit the open lock until exec closes CLOEXEC. The contention check above remains immediate; allow that temporary inheritor to exec before checking reacquisition. */
        assert!(claim_daemon_lock_timeout(&socket, Duration::from_secs(1)).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
