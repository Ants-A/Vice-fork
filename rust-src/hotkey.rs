//! Global evdev hotkeys, including modifier chords, hotplug and double taps.
use crate::config::{
    Config, canonical_modifier, effective_clip_bindings, is_modifier, normalize_combo,
};
use evdev::{Device, EventSummary, KeyCode};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(350);
#[derive(Clone, Debug, PartialEq)]
pub enum HotkeyEvent {
    Clip(i64),
    SessionToggle,
    RecordingToggle,
    Screenshot,
    OverlayToggle,
    Key { name: String, down: bool },
    Availability(bool),
}
#[derive(Default)]
pub struct Dispatcher {
    held: HashSet<String>,
    bindings: HashMap<String, HotkeyEvent>,
    doubles: HashSet<String>,
    pending: HashMap<String, Instant>,
}
impl Dispatcher {
    pub fn new(cfg: &Config) -> Self {
        let mut this = Self::default();
        for (key, duration) in effective_clip_bindings(cfg) {
            this.bindings.insert(key, HotkeyEvent::Clip(duration));
        }
        if !cfg.hotkeys.clip.is_empty() {
            this.doubles.insert(normalize_combo(&cfg.hotkeys.clip));
        }
        for (key, event) in [
            (&cfg.hotkeys.toggle, HotkeyEvent::RecordingToggle),
            (&cfg.hotkeys.screenshot, HotkeyEvent::Screenshot),
            (&cfg.hotkeys.key_overlay, HotkeyEvent::OverlayToggle),
        ] {
            if let Some(key) = key {
                this.bindings.insert(normalize_combo(key), event);
            }
        }
        this
    }
    pub fn disconnect(&mut self) {
        self.held.clear();
    }
    pub fn handle(&mut self, name: &str, value: i32, now: Instant) -> Vec<HotkeyEvent> {
        if value == 2 {
            return vec![];
        }
        let mut out = vec![HotkeyEvent::Key {
            name: name.into(),
            down: value == 1,
        }];
        if is_modifier(name) {
            if value == 1 {
                self.held.insert(name.into());
            } else {
                self.held.remove(name);
            }
            return out;
        }
        if value != 1 {
            return out;
        }
        let mut tokens: Vec<_> = self.held.iter().map(|s| canonical_modifier(s)).collect();
        tokens.push(name);
        let combo = normalize_combo(&tokens.join("+"));
        if let Some(last) = self.pending.remove(&combo) {
            if now.duration_since(last) < DOUBLE_TAP_WINDOW {
                out.push(HotkeyEvent::SessionToggle);
                return out;
            } else if let Some(event) = self.bindings.get(&combo) {
                out.push(event.clone());
            }
        }
        if self.doubles.contains(&combo) {
            self.pending.insert(combo, now);
        } else if let Some(event) = self.bindings.get(&combo) {
            out.push(event.clone());
        }
        out
    }
    pub fn flush(&mut self, now: Instant) -> Vec<HotkeyEvent> {
        let ready: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, at)| now.duration_since(**at) >= DOUBLE_TAP_WINDOW)
            .map(|(key, _)| key.clone())
            .collect();
        let mut out = vec![];
        for key in ready {
            self.pending.remove(&key);
            if let Some(event) = self.bindings.get(&key) {
                out.push(event.clone());
            }
        }
        out
    }
}
fn keyboards() -> Vec<(PathBuf, Device)> {
    evdev::enumerate()
        .filter(|(_, dev)| {
            dev.supported_keys().is_some_and(|keys| {
                keys.contains(KeyCode::KEY_A) || keys.contains(KeyCode::KEY_SPACE)
            })
        })
        .collect()
}
pub fn can_access_hotkeys() -> bool {
    !keyboards().is_empty()
}
pub fn list_available_keys() -> Vec<String> {
    let mut keys: Vec<_> = (0..=0x2ff)
        .map(|n| format!("{:?}", KeyCode::new(n)))
        .filter(|s| s.starts_with("KEY_"))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}
pub struct HotkeyListener {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
impl HotkeyListener {
    pub fn start(cfg: Config, events: mpsc::UnboundedSender<HotkeyEvent>) -> Self {
        let (stop, receiver) = watch::channel(false);
        let task = tokio::task::spawn_blocking(move || run(cfg, events, receiver));
        Self { stop, task }
    }
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
fn run(cfg: Config, events: mpsc::UnboundedSender<HotkeyEvent>, stop: watch::Receiver<bool>) {
    let mut devices: HashMap<PathBuf, Device> = HashMap::new();
    let mut dispatcher = Dispatcher::new(&cfg);
    let mut last_scan = Instant::now() - Duration::from_secs(4);
    let mut available = false;
    while !*stop.borrow() && !events.is_closed() {
        let now = Instant::now();
        if now.duration_since(last_scan) >= Duration::from_secs(3) {
            last_scan = now;
            for (path, dev) in keyboards() {
                if !devices.contains_key(&path) && dev.set_nonblocking(true).is_ok() {
                    tracing::info!("Listening for hotkeys on {}", path.display());
                    devices.insert(path, dev);
                }
            }
        }
        let mut gone = vec![];
        for (path, dev) in &mut devices {
            match dev.fetch_events() {
                Ok(input) => {
                    for event in input {
                        if let EventSummary::Key(_, key, value) = event.destructure() {
                            let name = format!("{key:?}");
                            for event in dispatcher.handle(&name, value, now) {
                                let _ = events.send(event);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => gone.push(path.clone()),
            }
        }
        for path in gone {
            devices.remove(&path);
            dispatcher.disconnect();
        }
        let current = !devices.is_empty();
        if current != available {
            available = current;
            let _ = events.send(HotkeyEvent::Availability(current));
        }
        for event in dispatcher.flush(now) {
            let _ = events.send(event);
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn double_tap_suppresses_single() {
        let mut d = Dispatcher::new(&Config::default());
        let now = Instant::now();
        assert_eq!(d.handle("KEY_F9", 1, now).len(), 1);
        assert!(
            d.handle("KEY_F9", 1, now + Duration::from_millis(100))
                .contains(&HotkeyEvent::SessionToggle)
        );
        assert!(d.flush(now + Duration::from_secs(1)).is_empty());
    }
    #[test]
    fn single_and_both_modifier_sides() {
        let mut cfg = Config::default();
        cfg.hotkeys.clip = "KEY_LEFTCTRL+KEY_F9".into();
        let mut d = Dispatcher::new(&cfg);
        let now = Instant::now();
        d.handle("KEY_LEFTCTRL", 1, now);
        d.handle("KEY_RIGHTCTRL", 1, now);
        d.handle("KEY_LEFTCTRL", 0, now);
        d.handle("KEY_F9", 1, now);
        assert_eq!(
            d.flush(now + Duration::from_secs(1)),
            vec![HotkeyEvent::Clip(20)]
        );
    }
}
