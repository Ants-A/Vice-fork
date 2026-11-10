//! Persistent playlists, including portable membership embedded in video tags.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub const IMAGE_PREFIX: &str = "img:";
pub const PLAYLIST_TAG: &str = "vice_playlists";
pub const PL_COLORS: [(&str, &str); 8] = [
    ("#ff7a45", "#9a3412"),
    ("#f0b429", "#7c4a03"),
    ("#34d399", "#064e3b"),
    ("#38bdf8", "#075985"),
    ("#8b5cf6", "#3b0a74"),
    ("#f472b6", "#831843"),
    ("#ef4444", "#7f1d1d"),
    ("#a3e635", "#3f6212"),
];
pub fn image_slug(stem: &str) -> String {
    format!("{IMAGE_PREFIX}{stem}")
}
pub fn game_key(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else {
            dash = true;
        }
    }
    out.chars().take(48).collect()
}
pub fn build_tag_index(custom_games: &[Value]) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let games: Vec<Value> =
        serde_json::from_str(include_str!("../vice/data/games.json")).unwrap_or_default();
    for game in games.iter().chain(custom_games) {
        if let Some(name) = game["name"].as_str()
            && !name.trim().is_empty()
        {
            result.insert(game_key(name), name.trim().into());
        }
    }
    result
}
fn valid_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|c| c.is_ascii_hexdigit())
}
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn slugs(v: &Value) -> Vec<String> {
    v["clip_slugs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
fn metadata_record(p: &Value, rev: i64) -> Value {
    json!({"id":text(p,"id"),"kind":p["kind"].as_str().unwrap_or("custom"),"name":text(p,"name"),"game":text(p,"game"),"emoji":text(p,"emoji"),"color1":text(p,"color1"),"color2":text(p,"color2"),"created_at":text(p,"created_at"),"edited":p["edited"].as_bool().unwrap_or(false),"rev":rev})
}
pub fn encode_playlist_metadata(playlists: &[Value], slug: &str, rev: Option<i64>) -> String {
    let rev = rev.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    json!({"v":1,"playlists":playlists.iter().filter(|p|slugs(p).iter().any(|s|s==slug)).map(|p|metadata_record(p,rev)).collect::<Vec<_>>()}).to_string()
}
pub fn decode_playlist_metadata(raw: &Value) -> Vec<Value> {
    let parsed;
    if let Some(s) = raw.as_str() {
        parsed = serde_json::from_str(s).unwrap_or(Value::Null);
    } else {
        parsed = raw.clone();
    }
    let Some(items) = parsed["playlists"].as_array() else {
        return vec![];
    };
    items
        .iter()
        .filter_map(|item| {
            let id = text(item, "id").trim();
            if id.is_empty() {
                return None;
            }
            let mut p = metadata_record(item, item["rev"].as_i64().unwrap_or(0));
            p["id"] = json!(id);
            p["name"] = json!(if text(item, "name").trim().is_empty() {
                id
            } else {
                text(item, "name").trim()
            });
            p["game"] = json!(text(item, "game").trim());
            if !["auto", "custom"].contains(&text(item, "kind")) {
                p["kind"] = json!("custom");
            }
            p["emoji"] = json!(text(item, "emoji").chars().take(4).collect::<String>());
            for k in ["color1", "color2"] {
                if !valid_color(text(item, k)) {
                    p[k] = json!("");
                }
            }
            Some(p)
        })
        .collect()
}
fn record_to_playlist(r: &Value) -> Value {
    let mut v = r.clone();
    v.as_object_mut().unwrap().remove("rev");
    v["clip_slugs"] = json!([]);
    if v["edited"] != true {
        v.as_object_mut().unwrap().remove("edited");
    }
    v
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
pub fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
#[derive(Debug)]
pub struct PlaylistStore {
    pub path: PathBuf,
    pub loaded_from_disk: bool,
    pub load_error: String,
    playlists: Vec<Value>,
    dismissed_auto: HashSet<String>,
}
impl PlaylistStore {
    pub fn new(path: PathBuf) -> Self {
        let mut s = Self {
            path,
            loaded_from_disk: false,
            load_error: String::new(),
            playlists: vec![],
            dismissed_auto: HashSet::new(),
        };
        s.load();
        s
    }
    pub fn load(&mut self) {
        self.playlists.clear();
        self.dismissed_auto.clear();
        self.loaded_from_disk = false;
        self.load_error.clear();
        let result = (|| -> Result<()> {
            if !self.path.exists() {
                return Ok(());
            }
            let data: Value = serde_json::from_slice(&std::fs::read(&self.path)?)?;
            self.playlists = data["playlists"]
                .as_array()
                .context("playlists must be an array")?
                .iter()
                .filter(|p| !text(p, "id").is_empty())
                .cloned()
                .collect();
            self.dismissed_auto = data["dismissed_auto"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            self.loaded_from_disk = true;
            Ok(())
        })();
        if let Err(e) = result {
            self.load_error = e.to_string();
        }
    }
    pub fn save(&self) -> Result<()> {
        let mut dismissed: Vec<_> = self.dismissed_auto.iter().cloned().collect();
        dismissed.sort();
        atomic_json(
            &self.path,
            &json!({"version":1,"playlists":self.playlists,"dismissed_auto":dismissed}),
        )
    }
    pub fn list_playlists(&self) -> Vec<Value> {
        let mut ps = self.playlists.clone();
        ps.sort_by_key(|p| std::cmp::Reverse(slugs(p).len()));
        ps
    }
    pub fn get(&self, pid: &str) -> Option<&Value> {
        self.playlists.iter().find(|p| text(p, "id") == pid)
    }
    fn index(&self, pid: &str) -> Result<usize> {
        self.playlists
            .iter()
            .position(|p| text(p, "id") == pid)
            .context("playlist not found")
    }
    pub fn game_for(&self, slug: &str) -> Option<String> {
        self.playlists
            .iter()
            .find(|p| p["kind"] == "auto" && slugs(p).iter().any(|s| s == slug))
            .map(|p| {
                let g = text(p, "game");
                if g.is_empty() {
                    text(p, "name").to_owned()
                } else {
                    g.to_owned()
                }
            })
    }
    pub fn metadata_for(&self, slug: &str, rev: Option<i64>) -> String {
        encode_playlist_metadata(&self.playlists, slug, rev)
    }
    pub fn create_custom(
        &mut self,
        name: &str,
        emoji: &str,
        color1: &str,
        color2: &str,
    ) -> Result<Value> {
        if name.trim().is_empty() {
            bail!("Playlist name is required");
        }
        if emoji.trim().chars().count() > 4 {
            bail!("Playlist emoji must be 4 characters or fewer");
        }
        let (c1, c2) = PL_COLORS[self.playlists.len() % 8];
        let p = json!({"id":format!("pl-{}-{}",chrono::Utc::now().timestamp_millis(),&uuid::Uuid::new_v4().simple().to_string()[..4]),"kind":"custom","name":name.trim(),"emoji":emoji.trim(),"color1":if valid_color(color1){color1}else{c1},"color2":if valid_color(color2){color2}else{c2},"clip_slugs":[],"created_at":chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()});
        self.playlists.push(p.clone());
        self.save()?;
        Ok(p)
    }
    pub fn update_playlist(&mut self, pid: &str, fields: &Value) -> Result<Value> {
        let i = self.index(pid)?;
        let mut p = self.playlists[i].clone();
        if fields.get("name").is_some() {
            let name = text(fields, "name").trim();
            if name.is_empty() {
                bail!("Playlist name is required");
            }
            p["name"] = json!(name);
        }
        if fields.get("emoji").is_some() {
            let emoji = text(fields, "emoji").trim();
            if emoji.chars().count() > 4 {
                bail!("Playlist emoji must be 4 characters or fewer");
            }
            p["emoji"] = json!(emoji);
        }
        if let Some(icon) = fields.get("icon") {
            let icon = icon
                .as_str()
                .context("Playlist icon must be an image or automatic")?;
            if !crate::playlist_icons::valid_icon(icon) {
                bail!("Playlist icon must be a PNG image of at most 256×256 pixels and 64 KB");
            }
            p["icon"] = json!(icon);
        }
        for k in ["color1", "color2"] {
            if valid_color(text(fields, k)) {
                p[k] = fields[k].clone();
            }
        }
        if p["kind"] == "auto" {
            p["edited"] = json!(true);
        }
        self.playlists[i] = p.clone();
        self.save()?;
        Ok(p)
    }
    pub fn delete(&mut self, pid: &str) -> Result<()> {
        let i = self.index(pid)?;
        let p = self.playlists.remove(i);
        if p["kind"] == "auto" {
            let key = pid
                .strip_prefix("auto:")
                .map(str::to_owned)
                .unwrap_or_else(|| game_key(text(&p, "game")));
            if !key.is_empty() {
                self.dismissed_auto.insert(key);
            }
        }
        self.save()
    }
    pub fn add_clip(&mut self, pid: &str, slug: &str) -> Result<Value> {
        let i = self.index(pid)?;
        let mut s = slugs(&self.playlists[i]);
        if !s.iter().any(|s| s == slug) {
            s.push(slug.into());
            self.playlists[i]["clip_slugs"] = json!(s);
            self.save()?;
        }
        Ok(self.playlists[i].clone())
    }
    pub fn remove_clip(&mut self, pid: &str, slug: &str) -> Result<()> {
        let i = self.index(pid)?;
        let mut s = slugs(&self.playlists[i]);
        s.retain(|s| s != slug);
        self.playlists[i]["clip_slugs"] = json!(s);
        if Self::prunable(&self.playlists[i]) {
            self.playlists.remove(i);
        }
        self.save()
    }
    fn prunable(p: &Value) -> bool {
        p["kind"] == "auto" && slugs(p).is_empty() && p["edited"] != true
    }
    pub fn record_auto(
        &mut self,
        game: &str,
        slug: &str,
        display_name: Option<&str>,
        from_backfill: bool,
    ) -> Result<bool> {
        let key = game_key(game);
        if key.is_empty() {
            return Ok(false);
        }
        if self.dismissed_auto.contains(&key) {
            if from_backfill {
                return Ok(false);
            }
            self.dismissed_auto.remove(&key);
        }
        let pid = format!("auto:{key}");
        let i = if let Ok(i) = self.index(&pid) {
            i
        } else {
            let (c1, c2) = PL_COLORS[crc32(key.as_bytes()) as usize % 8];
            let name = display_name.unwrap_or(game);
            self.playlists.push(json!({"id":pid,"kind":"auto","name":name,"game":name,"emoji":"","icon":"auto","color1":c1,"color2":c2,"clip_slugs":[],"created_at":chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()}));
            self.playlists.len() - 1
        };
        if slugs(&self.playlists[i]).iter().any(|s| s == slug) {
            return Ok(false);
        }
        self.add_clip(&pid, slug)?;
        Ok(true)
    }
    pub fn on_clip_renamed(&mut self, old: &str, new: &str) -> Result<bool> {
        let mut changed = false;
        for p in &mut self.playlists {
            let mut ss = slugs(p);
            for s in &mut ss {
                if s == old {
                    *s = new.into();
                    changed = true;
                }
            }
            p["clip_slugs"] = json!(ss);
        }
        if changed {
            self.save()?;
        }
        Ok(changed)
    }
    pub fn on_clip_deleted(&mut self, slug: &str) -> Result<bool> {
        let mut changed = false;
        for p in &mut self.playlists {
            let mut ss = slugs(p);
            let len = ss.len();
            ss.retain(|s| s != slug);
            changed |= len != ss.len();
            p["clip_slugs"] = json!(ss);
        }
        self.playlists.retain(|p| !Self::prunable(p));
        if changed {
            self.save()?;
        }
        Ok(changed)
    }
    pub fn restore_from_metadata(&mut self, entries: &[(String, Value)]) -> Result<bool> {
        self.playlists.clear();
        self.dismissed_auto.clear();
        let mut revisions = HashMap::new();
        for (slug, raw) in entries {
            let records = if raw.is_array() {
                raw.as_array().unwrap().clone()
            } else {
                decode_playlist_metadata(raw)
            };
            for r in records {
                let pid = text(&r, "id").to_owned();
                if pid.is_empty() {
                    continue;
                }
                let rev = r["rev"].as_i64().unwrap_or(0);
                let i = if let Ok(i) = self.index(&pid) {
                    if rev >= *revisions.get(&pid).unwrap_or(&0) {
                        let ss = slugs(&self.playlists[i]);
                        self.playlists[i] = record_to_playlist(&r);
                        self.playlists[i]["clip_slugs"] = json!(ss);
                        revisions.insert(pid.clone(), rev);
                    }
                    i
                } else {
                    self.playlists.push(record_to_playlist(&r));
                    revisions.insert(pid.clone(), rev);
                    self.playlists.len() - 1
                };
                let mut ss = slugs(&self.playlists[i]);
                if !ss.contains(slug) {
                    ss.push(slug.clone());
                    self.playlists[i]["clip_slugs"] = json!(ss);
                }
            }
        }
        self.save()?;
        Ok(!self.playlists.is_empty())
    }
    pub fn merge_metadata(&mut self, entries: &[(String, Value)]) -> Result<bool> {
        let mut changed = false;
        for (slug, raw) in entries {
            let records = if raw.is_array() {
                raw.as_array().unwrap().clone()
            } else {
                decode_playlist_metadata(raw)
            };
            for r in records {
                let pid = text(&r, "id").to_owned();
                if pid.is_empty() {
                    continue;
                }
                if r["kind"] == "auto"
                    && self
                        .dismissed_auto
                        .contains(pid.strip_prefix("auto:").unwrap_or(""))
                {
                    continue;
                }
                let i = if let Ok(i) = self.index(&pid) {
                    i
                } else {
                    self.playlists.push(record_to_playlist(&r));
                    changed = true;
                    self.playlists.len() - 1
                };
                let mut ss = slugs(&self.playlists[i]);
                if !ss.contains(slug) {
                    ss.push(slug.clone());
                    self.playlists[i]["clip_slugs"] = json!(ss);
                    changed = true;
                }
            }
        }
        if changed {
            self.save()?;
        }
        Ok(changed)
    }
    pub fn backfill(
        &mut self,
        existing: &HashSet<String>,
        index: &HashMap<String, String>,
        seed_auto: bool,
    ) -> Result<bool> {
        let mut changed = false;
        for p in &mut self.playlists {
            let old = slugs(p);
            let ss: Vec<_> = old
                .iter()
                .filter(|s| existing.contains(*s))
                .cloned()
                .collect();
            changed |= ss.len() != old.len();
            p["clip_slugs"] = json!(ss);
        }
        self.playlists.retain(|p| !Self::prunable(p));
        if seed_auto {
            let re = regex::Regex::new(r"^(?:img:)?Vice_(?:Clip|Session|Shot)_\d+_(.+)$").unwrap();
            let mut ss: Vec<_> = existing.iter().collect();
            ss.sort();
            for slug in ss {
                if let Some(c) = re.captures(slug) {
                    let tag = &c[1];
                    let display = index
                        .get(&game_key(tag))
                        .cloned()
                        .unwrap_or_else(|| tag.replace('-', " "));
                    changed |= self.record_auto(tag, slug, Some(&display), true)?;
                }
            }
        }
        if changed {
            self.save()?;
        }
        Ok(changed)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> PlaylistStore {
        PlaylistStore::new(
            std::env::temp_dir().join(format!("vice-playlists-{}.json", uuid::Uuid::new_v4())),
        )
    }
    #[test]
    fn icon_changes_persist_without_growing_embedded_video_metadata() {
        let mut s = store();
        let p = s.create_custom("Best", "🎮", "", "").unwrap();
        let id = text(&p, "id");
        s.add_clip(id, "clip").unwrap();
        let before = s.metadata_for("clip", Some(1));
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAGAAAABg";
        assert!(crate::playlist_icons::valid_icon(png));
        s.update_playlist(id, &json!({"icon":png})).unwrap();
        let loaded = PlaylistStore::new(s.path.clone());
        assert_eq!(loaded.get(id).unwrap()["icon"], png);
        assert_eq!(s.metadata_for("clip", Some(1)), before);
        assert!(
            s.update_playlist(id, &json!({"icon":"https://untrusted/image.svg"}))
                .is_err()
        );
        assert_eq!(s.get(id).unwrap()["icon"], png);
        let _ = std::fs::remove_file(s.path);
    }
    #[test]
    fn rename_delete_and_roundtrip() {
        let mut s = store();
        let p = s.create_custom("Best", "", "", "").unwrap();
        let id = text(&p, "id");
        s.add_clip(id, "a").unwrap();
        s.on_clip_renamed("a", "b").unwrap();
        let loaded = PlaylistStore::new(s.path.clone());
        assert_eq!(slugs(loaded.get(id).unwrap()), vec!["b"]);
        s.on_clip_deleted("b").unwrap();
        assert!(slugs(s.get(id).unwrap()).is_empty());
        let _ = std::fs::remove_file(s.path);
    }
    #[test]
    fn auto_dismissal_survives_backfill_but_new_clip_revives() {
        let mut s = store();
        s.record_auto("Game 2", "a", None, false).unwrap();
        s.delete("auto:game-2").unwrap();
        assert!(!s.record_auto("Game 2", "b", None, true).unwrap());
        assert!(s.record_auto("Game 2", "b", None, false).unwrap());
        let _ = std::fs::remove_file(s.path);
    }
    #[test]
    fn metadata_newest_definition_union_membership() {
        let mut a = store();
        let p = a.create_custom("Original", "🎮", "", "").unwrap();
        let id = text(&p, "id");
        a.add_clip(id, "a").unwrap();
        let old = a.metadata_for("a", Some(1));
        a.update_playlist(id, &json!({"name":"New"})).unwrap();
        a.add_clip(id, "b").unwrap();
        let new = a.metadata_for("b", Some(2));
        let mut b = store();
        b.restore_from_metadata(&[("b".into(), json!(new)), ("a".into(), json!(old))])
            .unwrap();
        assert_eq!(b.get(id).unwrap()["name"], "New");
        assert_eq!(slugs(b.get(id).unwrap()).len(), 2);
        let _ = std::fs::remove_file(a.path);
        let _ = std::fs::remove_file(b.path);
    }
    #[test]
    fn crc_matches_zlib() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        assert_eq!(game_key("Overwatch 2!"), "overwatch-2");
    }
}
