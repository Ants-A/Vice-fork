//! Records only the displayed gameplay keys, never arbitrary typed text.
use anyhow::Result;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};
pub fn layout_key(name: &str) -> Option<&str> {
    match name {
        "KEY_1" => Some("1"),
        "KEY_2" => Some("2"),
        "KEY_3" => Some("3"),
        "KEY_4" => Some("4"),
        "KEY_5" => Some("5"),
        "KEY_Q" => Some("Q"),
        "KEY_W" => Some("W"),
        "KEY_E" => Some("E"),
        "KEY_R" => Some("R"),
        "KEY_A" => Some("A"),
        "KEY_S" => Some("S"),
        "KEY_D" => Some("D"),
        "KEY_F" => Some("F"),
        "KEY_TAB" => Some("TAB"),
        "KEY_LEFTSHIFT" | "KEY_RIGHTSHIFT" => Some("SHIFT"),
        "KEY_LEFTCTRL" | "KEY_RIGHTCTRL" => Some("CTRL"),
        "KEY_SPACE" => Some("SPACE"),
        _ => None,
    }
}
pub const LAYOUT_KEYS: &[&str] = &[
    "1", "2", "3", "4", "5", "Q", "W", "E", "R", "A", "S", "D", "F", "TAB", "SHIFT", "CTRL",
    "SPACE",
];
#[derive(Debug)]
pub struct KeyRing {
    pub horizon: f64,
    events: VecDeque<(f64, String, bool)>,
    held: BTreeMap<String, BTreeSet<String>>,
}
impl KeyRing {
    pub fn new(horizon: f64) -> Self {
        Self {
            horizon,
            events: VecDeque::new(),
            held: BTreeMap::new(),
        }
    }
    pub fn record(&mut self, name: &str, down: bool, at: f64) -> Option<(String, bool)> {
        let key = layout_key(name)?.to_owned();
        let held = self.held.entry(key.clone()).or_default();
        let was = !held.is_empty();
        if down {
            held.insert(name.into());
        } else {
            held.remove(name);
        }
        let now = !held.is_empty();
        if was == now {
            return None;
        }
        self.events.push_back((at, key.clone(), now));
        let cutoff = at - self.horizon;
        // Keep the last state preceding the horizon, so a key held over the cutoff remains visible.
        let mut before = BTreeMap::new();
        while self.events.front().is_some_and(|e| e.0 < cutoff) {
            if let Some((_, key, down)) = self.events.pop_front() {
                before.insert(key, down);
            }
        }
        for (key, down) in before.into_iter().rev() {
            if down {
                self.events.push_front((cutoff, key, true));
            }
        }
        Some((key, now))
    }
    pub fn intervals(&self, start: f64, end: f64) -> Vec<Value> {
        let mut open = BTreeMap::new();
        let mut out = vec![];
        for (at, key, down) in &self.events {
            if *at > end {
                break;
            }
            if *down {
                open.insert(key.clone(), *at);
            } else if let Some(begun) = open.remove(key)
                && *at >= start
                && *at > begun.max(start)
            {
                out.push((
                    round(begun.max(start) - start),
                    round(*at - start),
                    key.clone(),
                ));
            }
        }
        for (key, begun) in open {
            let a = begun.max(start) - start;
            let b = end - start;
            if b > a {
                out.push((round(a), round(b), key));
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)));
        out.into_iter().map(|(a, b, k)| json!([a, b, k])).collect()
    }
}
fn round(n: f64) -> f64 {
    (n * 1000.).round() / 1000.
}
pub fn sidecar_path(slug: &str) -> PathBuf {
    crate::runtime::data_dir()
        .join("keys")
        .join(format!("{slug}.json"))
}
fn valid_slug(slug: &str) -> bool {
    !slug.is_empty() && !slug.contains('/') && !slug.contains('\\') && slug != "." && slug != ".."
}
pub fn save_sidecar(slug: &str, intervals: &[Value]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if !valid_slug(slug) {
        anyhow::bail!("invalid clip slug");
    }
    let path = sidecar_path(slug);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(&json!({"version":1,"keys":intervals}))?)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
pub fn load_sidecar(slug: &str) -> Option<Vec<Value>> {
    if !valid_slug(slug) {
        return None;
    }
    let value: Value = serde_json::from_slice(&std::fs::read(sidecar_path(slug)).ok()?).ok()?;
    Some(
        value["keys"]
            .as_array()?
            .iter()
            .filter(|v| {
                v.as_array().is_some_and(|a| {
                    a.len() == 3 && a[2].as_str().is_some_and(|key| LAYOUT_KEYS.contains(&key))
                })
            })
            .cloned()
            .collect(),
    )
}
pub fn move_sidecar(old: &str, new: &str) {
    if valid_slug(old) && valid_slug(new) {
        let _ = std::fs::rename(sidecar_path(old), sidecar_path(new));
    }
}
pub fn delete_sidecar(slug: &str) {
    if valid_slug(slug) {
        let _ = std::fs::remove_file(sidecar_path(slug));
    }
}
pub fn cut_intervals(intervals: &[Value], start: f64, end: f64) -> Vec<Value> {
    intervals
        .iter()
        .filter_map(|v| {
            let a = v.get(0)?.as_f64()?.max(start);
            let b = v.get(1)?.as_f64()?.min(end);
            if b > a {
                Some(json!([round(a - start), round(b - start), v.get(2)?]))
            } else {
                None
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_letters_and_holds_both_shifts() {
        let mut r = KeyRing::new(10.);
        assert_eq!(r.record("KEY_Z", true, 0.), None);
        r.record("KEY_LEFTSHIFT", true, 1.);
        assert_eq!(r.record("KEY_RIGHTSHIFT", true, 2.), None);
        assert_eq!(r.record("KEY_LEFTSHIFT", false, 3.), None);
        r.record("KEY_RIGHTSHIFT", false, 4.);
        assert_eq!(r.intervals(0., 5.), vec![json!([1., 4., "SHIFT"])]);
    }
    #[test]
    fn trim_rebases_and_clamps() {
        assert_eq!(
            cut_intervals(&[json!([0., 3., "W"]), json!([4., 8., "A"])], 2., 6.),
            vec![json!([0., 1., "W"]), json!([2., 4., "A"])]
        );
    }
}
