//! Device and OS reporting; this does not change recording behavior.
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}
pub fn detect_platform() -> Value {
    detect_platform_at(Path::new("/"))
}
pub fn detect_platform_at(root: &Path) -> Value {
    let flatpak = crate::runtime::in_vice_flatpak();
    let mut release = if flatpak {
        read(root.join("run/host/os-release"))
    } else {
        String::new()
    };
    if release.is_empty() {
        release = read(root.join("etc/os-release"));
    }
    if release.is_empty() {
        release = read(root.join("usr/lib/os-release"));
    }
    let values: BTreeMap<_, _> = release
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().trim_matches('"').to_owned()))
        .collect();
    let id = values.get("ID").cloned().unwrap_or_default().to_lowercase();
    let like = values
        .get("ID_LIKE")
        .cloned()
        .unwrap_or_default()
        .to_lowercase();
    let variant = values
        .get("VARIANT_ID")
        .cloned()
        .unwrap_or_default()
        .to_lowercase();
    let vendor = read(root.join("sys/class/dmi/id/board_vendor")).to_lowercase();
    let mut product = read(root.join("sys/class/dmi/id/product_name")).to_lowercase();
    if product.is_empty() {
        product = read(root.join("sys/class/dmi/id/board_name")).to_lowercase();
    }
    let device = if vendor.contains("valve") && ["jupiter", "galileo"].contains(&product.as_str()) {
        "steamdeck"
    } else {
        "desktop"
    };
    let immutable_ids = [
        "steamos",
        "bazzite",
        "silverblue",
        "kinoite",
        "bluefin",
        "aurora",
    ];
    let immutable = immutable_ids.contains(&id.as_str())
        || like
            .split_whitespace()
            .any(|part| immutable_ids.contains(&part))
        || root.join("run/ostree-booted").exists();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let session = if std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some()
        || desktop.to_lowercase().contains("gamescope")
    {
        String::from("gamescope")
    } else if !desktop.is_empty() {
        desktop
    } else {
        std::env::var("XDG_SESSION_TYPE").unwrap_or_default()
    };
    let exe = std::env::current_exe().unwrap_or_default();
    let packaging = if flatpak {
        "flatpak"
    } else if exe.starts_with("/nix/store") {
        "nix"
    } else if exe.starts_with(crate::runtime::actual_home_dir().join(".local")) {
        "script"
    } else {
        "unknown"
    };
    json!({"device":device,"os_id":id,"variant":variant,"immutable":immutable,"packaging":packaging,"session":session})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_steamdeck_and_immutable_os() {
        let root = std::env::temp_dir().join(format!("vice-platform-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sys/class/dmi/id")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("sys/class/dmi/id/board_vendor"), "Valve").unwrap();
        std::fs::write(root.join("sys/class/dmi/id/product_name"), "Galileo").unwrap();
        std::fs::write(
            root.join("etc/os-release"),
            "ID=steamos\nVARIANT_ID=steamdeck",
        )
        .unwrap();
        let result = detect_platform_at(&root);
        assert_eq!(result["device"], "steamdeck");
        assert_eq!(result["immutable"], true);
        std::fs::remove_dir_all(root).unwrap();
    }
}
