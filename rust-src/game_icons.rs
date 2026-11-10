//! Discord game icon lookup with persistent, week-long detectable-game cache.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
pub fn cache_dir() -> PathBuf {
    crate::runtime::cache_dir()
}
fn load(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
}
fn save(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    if let Ok(bytes) = serde_json::to_vec(value)
        && std::fs::write(&tmp, bytes).is_ok()
    {
        let _ = std::fs::rename(tmp, path);
    }
}
pub fn build_name_map(apps: &[Value]) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for app in apps {
        let name = app["name"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let id = app["id"]
            .as_str()
            .map(String::from)
            .or_else(|| app["id"].as_u64().map(|n| n.to_string()));
        if !name.is_empty()
            && let Some(id) = id
        {
            names.entry(name).or_insert(id);
        }
    }
    names
}
pub async fn discord_icon_url(game: &str) -> Option<String> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let key = game.trim().to_lowercase();
    if key.is_empty() {
        return None;
    }
    let _guard = LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let icons_path = cache_dir().join("icons.json");
    let mut icons = load(&icons_path);
    if let Some(value) = icons.get(&key) {
        return value.as_str().filter(|s| !s.is_empty()).map(String::from);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent(format!("Vice/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let path = cache_dir().join("detectable.json");
    let cached = load(&path);
    let now = crate::updates::now();
    let names = if cached["ts"].as_f64().unwrap_or(0.) + 604800. > now
        && cached["names"].as_object().is_some_and(|m| !m.is_empty())
    {
        cached["names"].clone()
    } else {
        match client
            .get("https://discord.com/api/v10/applications/detectable")
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let apps: Vec<Value> = response.json().await.ok()?;
                let names = serde_json::to_value(build_name_map(&apps)).ok()?;
                save(&path, &json!({"ts":now,"names":names}));
                names
            }
            _ => cached["names"].clone(),
        }
    };
    let id = names.get(&key).and_then(Value::as_str);
    let url = if let Some(id) = id {
        let data: Value = client
            .get(format!("https://discord.com/api/v10/applications/{id}/rpc"))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        data["icon"]
            .as_str()
            .map(|icon| format!("https://cdn.discordapp.com/app-icons/{id}/{icon}.png"))
            .unwrap_or_default()
    } else if names.as_object().is_none_or(|m| m.is_empty()) {
        return None;
    } else {
        String::new()
    };
    icons[&key] = json!(url);
    save(&icons_path, &icons);
    if url.is_empty() { None } else { Some(url) }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_game_id_wins() {
        let names = build_name_map(&[
            json!({"name":"Game","id":"1"}),
            json!({"name":"game","id":"2"}),
            json!({"name":"","id":"3"}),
        ]);
        assert_eq!(names.len(), 1);
        assert_eq!(names["game"], "1");
    }
}
