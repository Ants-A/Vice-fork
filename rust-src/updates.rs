//! Quiet release checks with persistent daily caching.
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub const RELEASES_URL: &str = "https://api.github.com/repos/splitshotz/Vice/releases/latest";
pub const RELEASE_PAGE: &str = "https://github.com/splitshotz/Vice/releases/latest";
pub const CHECK_INTERVAL: f64 = 86400.;
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn parse_version(text: &str) -> Option<Vec<u64>> {
    let re = regex::Regex::new(r"^\s*v?(\d+(?:\.\d+)*)").ok()?;
    let caps = re.captures(text)?;
    caps[1].split('.').map(|s| s.parse().ok()).collect()
}
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    match (parse_version(candidate), parse_version(installed)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}
pub fn summarize_notes(body: &str, limit: usize) -> Vec<String> {
    let bold = regex::Regex::new(r"^\s*[-*]\s+\*\*(.+?)\*\*").unwrap();
    let bullet = regex::Regex::new(r"^\s*[-*]\s+(.+)").unwrap();
    let issues = regex::Regex::new(r"\s*\(#\d+\)").unwrap();
    let links = regex::Regex::new(r"\[(.+?)\]\(.+?\)").unwrap();
    let mut leads = vec![];
    let mut bullets = vec![];
    for line in body.lines() {
        if let Some(c) = bold.captures(line) {
            leads.push(c[1].to_owned());
        } else if let Some(c) = bullet.captures(line) {
            bullets.push(c[1].to_owned());
        }
    }
    if leads.is_empty() {
        leads = bullets;
    }
    leads
        .into_iter()
        .take(limit)
        .filter_map(|line| {
            let clean = links.replace_all(&line, "$1");
            let clean: String = clean
                .chars()
                .filter(|c| !matches!(c, '`' | '*' | '_'))
                .collect();
            let clean = issues.replace_all(&clean, "");
            let clean = clean.trim().trim_end_matches('.');
            if clean.is_empty() {
                return None;
            }
            if clean.chars().count() > 110 {
                let short: String = clean.chars().take(110).collect();
                Some(format!(
                    "{}…",
                    short
                        .rsplit_once(' ')
                        .map_or(short.as_str(), |(left, _)| left)
                ))
            } else {
                Some(clean.to_owned())
            }
        })
        .collect()
}
pub async fn fetch_latest(etag: Option<&str>) -> Option<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent(format!("vice/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let mut request = client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json");
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let etag = response
        .headers()
        .get("ETag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let data: Value = response.json().await.ok()?;
    if data["draft"].as_bool() == Some(true) || data["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = data["tag_name"].as_str()?.trim_start_matches(['v', 'V']);
    parse_version(version)?;
    Some(
        json!({"version":version,"url":data["html_url"].as_str().unwrap_or(RELEASE_PAGE),"notes":summarize_notes(data["body"].as_str().unwrap_or_default(),3),"etag":etag}),
    )
}
pub struct UpdateCache {
    pub path: PathBuf,
}
impl Default for UpdateCache {
    fn default() -> Self {
        Self {
            path: crate::runtime::data_dir().join("update.json"),
        }
    }
}
impl UpdateCache {
    pub fn load(&self) -> Value {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }
    pub fn save(&self, value: &Value) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec(value)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _ = std::fs::rename(tmp, &self.path);
        }
    }
    pub fn stale(&self, now: f64) -> bool {
        self.load()["checked_at"]
            .as_f64()
            .is_none_or(|t| now - t >= CHECK_INTERVAL)
    }
}
pub fn available(cache: &Value, installed: &str) -> Option<Value> {
    let version = cache["version"].as_str()?;
    if !is_newer(version, installed) {
        return None;
    }
    Some(
        json!({"version":version,"url":cache["url"].as_str().unwrap_or(RELEASE_PAGE),"notes":cache["notes"].as_array().cloned().unwrap_or_default()}),
    )
}
pub async fn check(force: bool) -> Option<Value> {
    let cache = UpdateCache::default();
    let mut data = cache.load();
    if force || cache.stale(now()) {
        if let Some(release) = fetch_latest(data["etag"].as_str()).await {
            data = release;
        }
        data["checked_at"] = json!(now());
        cache.save(&data);
    }
    available(&data, env!("CARGO_PKG_VERSION"))
}
/// Compatibility helper for the HTTP control endpoint.
pub async fn check_update(force: bool) -> anyhow::Result<Value> {
    Ok(check(force).await.unwrap_or(Value::Null))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn numeric_version_order() {
        assert!(is_newer("v2.10.0", "2.9.9"));
        assert!(!is_newer("2.17.0", "2.17.0"));
        assert!(!is_newer("invalid", "2.0"));
    }
    #[test]
    fn note_titles_and_noise() {
        assert_eq!(
            summarize_notes(
                "- **Better clips.** More detail\n- **Audio fixed (#123).** Detail",
                3
            ),
            vec!["Better clips", "Audio fixed"]
        );
    }
}
