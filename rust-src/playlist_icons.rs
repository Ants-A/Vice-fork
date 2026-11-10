//! Small playlist artwork: explicit uploads or Steam's existing local game icons.
use axum::{
    body::Body,
    http::{Response, StatusCode, header},
};
use base64::Engine;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, Instant},
};

pub const MAX_ICON_LENGTH: usize = 65_536;
/// Store only a bounded raster image. Never accept executable SVG or external URLs.
pub fn valid_icon(icon: &str) -> bool {
    if icon.is_empty() || icon == "auto" {
        return true;
    }
    let Some(encoded) = icon.strip_prefix("data:image/png;base64,") else {
        return false;
    };
    if icon.len() > MAX_ICON_LENGTH {
        return false;
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.get(12..16) != Some(b"IHDR")
        || bytes.len() < 24
    {
        return false;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    width > 0 && height > 0 && width <= 256 && height <= 256
}

fn steam_app_ids(game: &str, roots: &[PathBuf]) -> Vec<String> {
    let wanted = game.trim().to_lowercase();
    let pattern = regex::Regex::new(r#""name"\s+"([^\"]+)""#).unwrap();
    let mut ids = Vec::new();
    for root in roots {
        if let Ok(entries) = std::fs::read_dir(root.join("steamapps")) {
            for entry in entries.flatten() {
                let filename = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = filename
                    .strip_prefix("appmanifest_")
                    .and_then(|s| s.strip_suffix(".acf"))
                else {
                    continue;
                };
                if !id.bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                if let Ok(text) = std::fs::read_to_string(entry.path())
                    && pattern
                        .captures(&text)
                        .is_some_and(|c| c[1].trim().to_lowercase() == wanted)
                {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    // Known Steam process IDs also work when a game is currently uninstalled,
    // as long as Steam still has its artwork cached.
    let games: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../vice/data/games.json")).unwrap_or_default();
    for entry in games {
        if entry["name"]
            .as_str()
            .is_some_and(|s| s.to_lowercase() == wanted)
            && let Some(matches) = entry["matches"].as_array()
        {
            for value in matches {
                if let Some(id) = value.as_str().and_then(|s| s.strip_prefix("steam_app_"))
                    && !id.is_empty()
                    && id.bytes().all(|b| b.is_ascii_digit())
                {
                    ids.push(id.into());
                }
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

pub fn steam_icon_at(home: &Path, game: &str) -> Option<(Vec<u8>, &'static str)> {
    let roots = crate::active_window::steam_roots_at(home);
    for id in steam_app_ids(game, &roots) {
        let mut candidates = Vec::new();
        for size in ["128x128", "96x96", "64x64", "48x48", "32x32", "16x16"] {
            candidates.push(home.join(format!(
                ".local/share/icons/hicolor/{size}/apps/steam_icon_{id}.png"
            )));
        }
        for root in &roots {
            let cache = root.join("appcache/librarycache");
            candidates.push(cache.join(format!("{id}_icon.jpg")));
            // Current Steam places the small icon under its content hash.
            if let Ok(entries) = std::fs::read_dir(cache.join(&id)) {
                let mut hashes: Vec<_> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|path| {
                        path.file_stem().and_then(|s| s.to_str()).is_some_and(|s| {
                            s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
                        }) && path.extension().is_some_and(|s| s == "jpg" || s == "png")
                    })
                    .collect();
                hashes.sort();
                candidates.extend(hashes);
            }
        }
        for path in candidates {
            let Ok(meta) = path.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() > 512 * 1024 {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                "image/png"
            } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                "image/jpeg"
            } else {
                continue;
            };
            return Some((bytes, mime));
        }
    }
    None
}

type Icon = Option<(Vec<u8>, &'static str)>;
type IconCache = tokio::sync::Mutex<HashMap<String, (Instant, Icon)>>;
pub async fn serve_game_icon(game: &str) -> axum::response::Response {
    static CACHE: OnceLock<IconCache> = OnceLock::new();
    let key = game.trim().to_lowercase();
    let cache = CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    let found = cache
        .lock()
        .await
        .get(&key)
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(300))
        .map(|(_, icon)| icon.clone());
    let icon = if let Some(icon) = found {
        icon
    } else {
        let owned = game.to_owned();
        let local = tokio::task::spawn_blocking(move || {
            steam_icon_at(&crate::runtime::actual_home_dir(), &owned)
        })
        .await
        .ok()
        .flatten();
        let resolved = if local.is_some() {
            local
        } else {
            discord_icon(game).await
        };
        let mut entries = cache.lock().await;
        if entries.len() >= 256 {
            entries.clear();
        }
        entries.insert(key, (Instant::now(), resolved.clone()));
        resolved
    };
    match icon {
        Some((bytes, mime)) => Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "private, max-age=300")
            .header("X-Content-Type-Options", "nosniff")
            .body(Body::from(bytes))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CACHE_CONTROL, "private, max-age=300")
            .body(Body::empty())
            .unwrap(),
    }
}
async fn discord_icon(game: &str) -> Icon {
    let url = crate::game_icons::discord_icon_url(game).await?;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response.content_length().is_some_and(|n| n > 512 * 1024) {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.len() > 512 * 1024 {
        return None;
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some((bytes.to_vec(), "image/png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some((bytes.to_vec(), "image/jpeg"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uploaded_icons_are_bounded_png_images() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(96_u32.to_be_bytes());
        png.extend(96_u32.to_be_bytes());
        let icon = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        assert!(valid_icon(&icon));
        assert!(valid_icon("auto"));
        assert!(valid_icon(""));
        for invalid in [
            "https://example.com/icon.png",
            "data:image/svg+xml,<svg onload='bad'/>",
            "data:image/png;base64,bad",
        ] {
            assert!(!valid_icon(invalid));
        }
        png[16..20].copy_from_slice(&10000_u32.to_be_bytes());
        assert!(!valid_icon(&format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        )));
    }
    #[test]
    fn local_steam_icon_uses_matching_manifest_and_cached_artwork() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".local/share/Steam");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        std::fs::create_dir_all(root.join("appcache/librarycache/123")).unwrap();
        std::fs::write(
            root.join("steamapps/appmanifest_123.acf"),
            "\"name\" \"My Game\"",
        )
        .unwrap();
        let bytes = b"\xff\xd8\xffsample-icon";
        std::fs::write(
            root.join("appcache/librarycache/123/0123456789012345678901234567890123456789.jpg"),
            bytes,
        )
        .unwrap();
        assert_eq!(
            steam_icon_at(home.path(), "my game"),
            Some((bytes.to_vec(), "image/jpeg"))
        );
        assert_eq!(steam_icon_at(home.path(), "../123"), None);
    }
}
