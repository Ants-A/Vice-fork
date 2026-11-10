//! Active-window adapters for Linux desktops and game matching.
use crate::{config::DiscordCustomGame, runtime};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ActiveWindow {
    pub process: String,
    #[serde(rename = "class")]
    pub class: String,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Adapter {
    Hyprland,
    Sway,
    Niri,
    Kde,
    X11,
    None,
}
fn env(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.is_empty())
}
fn adapter() -> Adapter {
    if env("HYPRLAND_INSTANCE_SIGNATURE") {
        return Adapter::Hyprland;
    }
    if env("SWAYSOCK") {
        return Adapter::Sway;
    }
    if env("NIRI_SOCKET") && runtime::host_which("niri") {
        return Adapter::Niri;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    if env("WAYLAND_DISPLAY")
        && (desktop.contains("kde") || desktop.contains("plasma"))
        && runtime::host_which("kdotool")
    {
        return Adapter::Kde;
    }
    if env("DISPLAY") {
        Adapter::X11
    } else {
        Adapter::None
    }
}
async fn run(args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let args = runtime::host_argv(&args, false);
    let mut command = tokio::process::Command::new(&args[0]);
    command.args(&args[1..]).kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(2), command.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }
        _ => String::new(),
    }
}
fn process(pid: u32) -> String {
    runtime::read_host_proc(pid, "comm")
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .unwrap_or_default()
}
fn window(data: &Value, class: Option<&str>) -> Option<ActiveWindow> {
    let pid = data["pid"].as_u64().unwrap_or(0) as u32;
    let class = class.unwrap_or_default().to_owned();
    let process = process(pid);
    if class.is_empty() && process.is_empty() {
        return None;
    }
    Some(ActiveWindow {
        process,
        class,
        pid,
        window_id: None,
    })
}
pub fn focused_sway_node(node: &Value) -> Option<&Value> {
    if node["focused"].as_bool() == Some(true)
        && node["nodes"].as_array().is_none_or(|n| n.is_empty())
        && node["floating_nodes"]
            .as_array()
            .is_none_or(|n| n.is_empty())
    {
        return Some(node);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node[key].as_array() {
            for child in children {
                if let Some(hit) = focused_sway_node(child) {
                    return Some(hit);
                }
            }
        }
    }
    None
}
async fn json(args: &[&str]) -> Option<Value> {
    serde_json::from_str(&run(args).await).ok()
}
async fn x11() -> Option<ActiveWindow> {
    let wid = run(&["xdotool", "getactivewindow"]).await;
    if wid.is_empty() {
        return None;
    }
    let pid = run(&["xdotool", "getwindowpid", &wid])
        .await
        .parse()
        .unwrap_or(0);
    let wmclass = run(&["xprop", "-id", &wid, "WM_CLASS"]).await;
    let class = wmclass
        .split_once('=')
        .map(|(_, v)| {
            v.rsplit(',')
                .next()
                .unwrap_or(v)
                .trim()
                .trim_matches('"')
                .to_owned()
        })
        .unwrap_or_default();
    let process = process(pid);
    if class.is_empty() && process.is_empty() {
        return None;
    }
    Some(ActiveWindow {
        process,
        class,
        pid,
        window_id: Some(wid),
    })
}
pub async fn get_active_window() -> Option<ActiveWindow> {
    match adapter() {
        Adapter::Hyprland => {
            let data = json(&["hyprctl", "activewindow", "-j"]).await?;
            window(&data, data["class"].as_str())
        }
        Adapter::Sway => {
            let tree = json(&["swaymsg", "-t", "get_tree"]).await?;
            let data = focused_sway_node(&tree)?;
            window(
                data,
                data["app_id"]
                    .as_str()
                    .or_else(|| data["window_properties"]["class"].as_str()),
            )
        }
        Adapter::Niri => {
            let data = json(&["niri", "msg", "--json", "focused-window"]).await?;
            window(&data, data["app_id"].as_str())
        }
        Adapter::Kde => {
            let pid = run(&["kdotool", "getactivewindow", "getwindowpid"])
                .await
                .parse()
                .unwrap_or(0);
            let class = run(&["kdotool", "getactivewindow", "getwindowclassname"]).await;
            let process = process(pid);
            if class.is_empty() && process.is_empty() {
                x11().await
            } else {
                Some(ActiveWindow {
                    pid,
                    class,
                    process,
                    window_id: None,
                })
            }
        }
        Adapter::X11 => x11().await,
        Adapter::None => None,
    }
}
pub async fn get_focused_window_id() -> Option<String> {
    let wid = run(&["xdotool", "getactivewindow"]).await;
    if wid.is_empty() { None } else { Some(wid) }
}
fn size(value: &Value) -> Option<(u32, u32)> {
    let a = value.as_array()?;
    let w = a.first()?.as_u64()? as u32;
    let h = a.get(1)?.as_u64()? as u32;
    if w > 0 && h > 0 { Some((w, h)) } else { None }
}
pub async fn active_window_geometry() -> Option<(u32, u32)> {
    match adapter() {
        Adapter::Hyprland => size(&json(&["hyprctl", "activewindow", "-j"]).await?["size"]),
        Adapter::Niri => size(
            &json(&["niri", "msg", "--json", "focused-window"]).await?["layout"]["window_size"],
        ),
        Adapter::Sway => {
            let tree = json(&["swaymsg", "-t", "get_tree"]).await?;
            let node = focused_sway_node(&tree)?;
            let w = node["rect"]["width"].as_u64()? as u32;
            let h = node["rect"]["height"].as_u64()? as u32;
            if w > 0 && h > 0 { Some((w, h)) } else { None }
        }
        Adapter::X11 | Adapter::Kde => {
            let wid = get_focused_window_id().await?;
            let out = run(&["xdotool", "getwindowgeometry", "--shell", &wid]).await;
            let mut w = 0;
            let mut h = 0;
            for line in out.lines() {
                if let Some(v) = line.strip_prefix("WIDTH=") {
                    w = v.parse().unwrap_or(0);
                }
                if let Some(v) = line.strip_prefix("HEIGHT=") {
                    h = v.parse().unwrap_or(0);
                }
            }
            if w > 0 && h > 0 { Some((w, h)) } else { None }
        }
        Adapter::None => None,
    }
}
pub fn read_steam_app_id(pid: u32) -> Option<String> {
    let raw = runtime::read_host_proc(pid, "environ")?;
    steam_app_id_from_environment(&raw)
}
pub fn steam_app_id_from_environment(raw: &[u8]) -> Option<String> {
    let mut fallback = None;
    for entry in raw.split(|b| *b == 0) {
        if let Some((key, value)) = String::from_utf8_lossy(entry).split_once('=')
            && ["SteamAppId", "SteamGameId"].contains(&key)
            && value != "0"
            && !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_digit())
        {
            if key == "SteamAppId" {
                return Some(value.into());
            }
            fallback = Some(value.into());
        }
    }
    fallback
}
pub fn detection_tools_status() -> Value {
    serde_json::json!({"xdotool":runtime::host_which("xdotool"),"xprop":runtime::host_which("xprop"),"wmctrl":runtime::host_which("wmctrl"),"kdotool":runtime::host_which("kdotool")})
}
pub fn pointer_display_supported() -> bool {
    matches!(adapter(), Adapter::Hyprland | Adapter::Sway)
        || (adapter() == Adapter::X11 && !env("WAYLAND_DISPLAY"))
}
pub async fn pointer_display() -> Option<String> {
    match adapter() {
        Adapter::Hyprland => {
            let monitors = json(&["hyprctl", "monitors", "-j"]).await?;
            let pos = json(&["hyprctl", "cursorpos", "-j"]).await;
            for m in monitors.as_array()? {
                if let Some(p) = &pos {
                    let x = p["x"].as_f64().unwrap_or(0.);
                    let y = p["y"].as_f64().unwrap_or(0.);
                    let mx = m["x"].as_f64().unwrap_or(0.);
                    let my = m["y"].as_f64().unwrap_or(0.);
                    let scale = m["scale"].as_f64().unwrap_or(1.).max(0.1);
                    if x >= mx
                        && x < mx + m["width"].as_f64().unwrap_or(0.) / scale
                        && y >= my
                        && y < my + m["height"].as_f64().unwrap_or(0.) / scale
                    {
                        return m["name"].as_str().map(String::from);
                    }
                }
            }
            monitors
                .as_array()?
                .iter()
                .find(|m| m["focused"].as_bool() == Some(true))?["name"]
                .as_str()
                .map(String::from)
        }
        Adapter::Sway => {
            let outputs = json(&["swaymsg", "-t", "get_outputs", "-r"]).await?;
            outputs
                .as_array()?
                .iter()
                .find(|o| o["focused"].as_bool() == Some(true))?["name"]
                .as_str()
                .map(String::from)
        }
        Adapter::X11 if !env("WAYLAND_DISPLAY") => {
            let location = run(&["xdotool", "getmouselocation", "--shell"]).await;
            let mut x = None;
            let mut y = None;
            for line in location.lines() {
                if let Some(v) = line.strip_prefix("X=") {
                    x = v.parse::<i64>().ok();
                }
                if let Some(v) = line.strip_prefix("Y=") {
                    y = v.parse::<i64>().ok();
                }
            }
            let (x, y) = (x?, y?);
            let out = run(&["xrandr", "--listactivemonitors"]).await;
            let re = regex::Regex::new(r"(\d+)/\d+x(\d+)/\d+\+(-?\d+)\+(-?\d+)").ok()?;
            for line in out.lines() {
                if let Some(c) = re.captures(line) {
                    let (w, h, mx, my) = (
                        c[1].parse::<i64>().ok()?,
                        c[2].parse::<i64>().ok()?,
                        c[3].parse::<i64>().ok()?,
                        c[4].parse::<i64>().ok()?,
                    );
                    if x >= mx && x < mx + w && y >= my && y < my + h {
                        return line.split_whitespace().last().map(String::from);
                    }
                }
            }
            None
        }
        _ => None,
    }
}
pub fn match_game(win: &ActiveWindow, custom: &[DiscordCustomGame]) -> Option<String> {
    let app = read_steam_app_id(win.pid);
    let mut haystacks = vec![win.process.to_lowercase(), win.class.to_lowercase()];
    if let Some(id) = &app {
        haystacks.push(format!("steam_app_{id}"));
    }
    let builtin: Vec<DiscordCustomGame> =
        serde_json::from_str(include_str!("../vice/data/games.json")).unwrap_or_default();
    for entries in [custom, builtin.as_slice()] {
        let mut best: Option<(&DiscordCustomGame, usize)> = None;
        for entry in entries {
            for needle in &entry.matches {
                let needle = needle.to_lowercase();
                if !needle.is_empty()
                    && haystacks.iter().any(|h| needle_hits(&needle, h))
                    && best.is_none_or(|(_, len)| needle.len() > len)
                {
                    best = Some((entry, needle.len()));
                }
            }
        }
        if let Some((entry, _)) = best {
            return Some(entry.name.clone());
        }
    }
    let from_class = win
        .class
        .strip_prefix("steam_app_")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    if let Some(id) = app.as_deref().or(from_class)
        && let Some(name) = steam_game_name(id)
    {
        return Some(name);
    }
    let process = win.process.to_lowercase();
    if process.len() == 15 {
        for entries in [custom, builtin.as_slice()] {
            let mut best: Option<(&DiscordCustomGame, usize)> = None;
            for entry in entries {
                for needle in &entry.matches {
                    let needle = needle.to_lowercase();
                    if needle.len() > 15
                        && needle.starts_with(&process)
                        && best.is_none_or(|(_, len)| needle.len() > len)
                    {
                        best = Some((entry, needle.len()));
                    }
                }
            }
            if let Some((entry, _)) = best {
                return Some(entry.name.clone());
            }
        }
    }
    None
}
fn needle_hits(needle: &str, haystack: &str) -> bool {
    if needle
        .strip_prefix("steam_app_")
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    {
        return haystack.match_indices(needle).any(|(at, _)| {
            haystack
                .as_bytes()
                .get(at + needle.len())
                .is_none_or(|b| !b.is_ascii_digit())
        });
    }
    haystack.contains(needle)
}
pub fn steam_roots_at(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<_> = [
        ".local/share/Steam",
        ".steam/steam",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect();
    let pattern = regex::Regex::new(r#""path"\s+"([^\"]+)""#).unwrap();
    let mut libraries = vec![];
    for root in &roots {
        if let Ok(text) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libraries.extend(
                pattern
                    .captures_iter(&text)
                    .map(|c| std::path::PathBuf::from(&c[1])),
            );
        }
    }
    roots.extend(libraries);
    let mut seen = std::collections::BTreeSet::new();
    roots.retain(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())));
    roots
}
pub fn steam_game_name_at(home: &std::path::Path, app_id: &str) -> Option<String> {
    if app_id.is_empty() || !app_id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pattern = regex::Regex::new(r#""name"\s+"([^\"]+)""#).ok()?;
    for root in steam_roots_at(home) {
        if let Ok(text) = std::fs::read_to_string(
            root.join("steamapps")
                .join(format!("appmanifest_{app_id}.acf")),
        ) && let Some(c) = pattern.captures(&text)
        {
            let name = c[1].trim();
            let lower = name.to_lowercase();
            if ["proton", "steam linux runtime", "steamworks"]
                .iter()
                .any(|p| lower.starts_with(p))
            {
                return None;
            }
            if !name.is_empty() {
                return Some(name.into());
            }
        }
    }
    None
}
pub fn steam_game_name(app_id: &str) -> Option<String> {
    steam_game_name_at(&runtime::actual_home_dir(), app_id)
}
pub fn supported_compositor() -> bool {
    adapter() != Adapter::None
}
pub fn uses_x11_adapter() -> bool {
    adapter() == Adapter::X11
}
pub fn adapter_name() -> &'static str {
    match adapter() {
        Adapter::Hyprland => "hyprland",
        Adapter::Sway => "sway",
        Adapter::Niri => "niri",
        Adapter::Kde => "kde",
        Adapter::X11 => "x11",
        Adapter::None => "none",
    }
}
pub fn focus_blocked(win: &ActiveWindow, patterns: &[String]) -> bool {
    let process = win.process.to_lowercase();
    let class = win.class.to_lowercase();
    patterns.iter().any(|p| {
        !p.trim().is_empty()
            && (process.contains(&p.to_lowercase()) || class.contains(&p.to_lowercase()))
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn steam_environment_priority() {
        assert_eq!(
            steam_app_id_from_environment(b"SteamGameId=123\0SteamAppId=440\0"),
            Some("440".into())
        );
        assert_eq!(
            steam_app_id_from_environment(b"SteamAppId=0\0SteamGameId=no"),
            None
        );
    }
    #[test]
    fn focused_floating_leaf() {
        let tree =
            serde_json::json!({"nodes":[],"floating_nodes":[{"focused":true,"app_id":"game"}]});
        assert_eq!(focused_sway_node(&tree).unwrap()["app_id"], "game");
    }
    #[test]
    fn steam_ids_have_numeric_boundaries() {
        assert!(!needle_hits("steam_app_70", "steam_app_700330"));
        assert!(needle_hits("steam_app_70", "steam_app_70"));
    }
}
#[cfg(test)]
mod steam_tests {
    use super::*;
    #[test]
    fn steam_libraries_name_unlisted_games() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let root = home.join(".local/share/Steam");
        let extra = dir.path().join("games");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        std::fs::create_dir_all(extra.join("steamapps")).unwrap();
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} }}",
                extra.display()
            ),
        )
        .unwrap();
        std::fs::write(
            extra.join("steamapps/appmanifest_123.acf"),
            "\"AppState\" {\"name\" \"Unknown Game\"}",
        )
        .unwrap();
        assert_eq!(
            steam_game_name_at(&home, "123"),
            Some("Unknown Game".into())
        );
        assert_eq!(steam_game_name_at(&home, "../123"), None);
        std::fs::write(
            extra.join("steamapps/appmanifest_123.acf"),
            "\"AppState\" {\"name\" \"Proton Experimental\"}",
        )
        .unwrap();
        assert_eq!(steam_game_name_at(&home, "123"), None);
    }
}
