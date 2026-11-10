//! Backwards-compatible TOML configuration shared by the daemon and UI.
use crate::runtime::{actual_home_dir, resolve_path};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};
pub const CLIP_DURATION_MIN: i64 = 5;
pub const CLIP_DURATION_MAX: i64 = 1800;
pub const BUFFER_DURATION_MAX: i64 = 1800;
macro_rules! section {
    ($name:ident { $($field:ident : $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        #[serde(default)]
        pub struct $name { $(pub $field: $ty),* }
        impl Default for $name { fn default() -> Self { Self { $($field: $default),* } } }
    }
}
section!(RecordingConfig {
    buffer_duration: i64 = 120, clip_duration: i64 = 20, fps: i64 = 60,
    display: Option<String> = None, follow_mouse_display: bool = false,
    window_capture: bool = false, resolution: Option<String> = None,
    encoder: String = "auto".into(), crf: i64 = 23, color_depth: String = "8".into(),
    backend: String = "auto".into(), capture_audio: bool = true,
    desktop_volume: f64 = 1., microphone_volume: f64 = 1., capture_microphone: bool = false,
    microphone_source: String = "default_input".into(), microphone_mono: bool = false,
    wf_microphone_strategy: String = "prompt".into(), apply_watermark: bool = false,
    audio_sink: String = "default".into(), gsr_args: String = String::new(),
    gsr_source: String = "auto".into(), gsr_audio_source: String = "default_output".into(),
    gsr_replay_storage: String = "auto".into(), container: String = "mp4".into(),
    audio_tracks: Vec<String> = vec![], audio_tracks_mix_first: bool = false,
    audio_track_volumes: BTreeMap<String, f64> = BTreeMap::new()
});
section!(HotkeyClipPreset {
    key: String = String::new(),
    duration: i64 = 60
});
section!(HotkeyConfig {
    clip: String = "KEY_F9".into(), toggle: Option<String> = None,
    screenshot: Option<String> = None, key_overlay: Option<String> = None,
    clip_presets: Vec<HotkeyClipPreset> = vec![], disable_while_focused: Vec<String> = vec![]
});
section!(OutputConfig {
    directory: String = actual_home_dir()
        .join("Videos/Vice")
        .to_string_lossy()
        .into_owned(),
    image_directory: String = actual_home_dir()
        .join("Pictures/Vice")
        .to_string_lossy()
        .into_owned(),
    filename_format: String = "vice_%Y%m%d_%H%M%S.mp4".into(),
    tag_clips_with_game: bool = true,
    auto_playlist_by_game: bool = true,
    clip_name_template: String = String::new()
});
section!(SharingConfig {
    enabled: bool = true, port: u16 = 8765, public_port: Option<u16> = None,
    cloudflare_tunnel: bool = true, base_url: Option<String> = None,
    embed_color: String = "#0099ff".into(), share_discord_file: bool = false
});
section!(DiscordCustomGame { name: String = String::new(), matches: Vec<String> = vec![] });
section!(DiscordConfig {
    enabled: bool = true, client_id_override: Option<String> = None,
    persist_while_running: bool = true, game_icons: bool = true,
    custom_games: Vec<DiscordCustomGame> = vec![]
});
section!(UpdatesConfig {
    check_on_start: bool = true
});
section!(NotificationsConfig {
    sound_volume: f64 = 1., clip_sound: Option<String> = None,
    clip_failed_sound: Option<String> = None, session_start_sound: Option<String> = None,
    session_end_sound: Option<String> = None, highlight_sound: Option<String> = None,
    screenshot_sound: Option<String> = None
});
section!(UIConfig {
    hardware_video_decode: bool = true,
    scale: f64 = 1.0,
    theme: String = "light".into()
});
section!(OverlayConfig {
    keys: bool = false,
    keys_position: String = "bottom-left".into(),
    keys_x: i64 = 32,
    keys_y: i64 = 32,
    keys_scale: f64 = 1.,
    keys_in_clips: bool = false
});
section!(Config {
    recording: RecordingConfig = RecordingConfig::default(),
    hotkeys: HotkeyConfig = HotkeyConfig::default(),
    output: OutputConfig = OutputConfig::default(),
    sharing: SharingConfig = SharingConfig::default(),
    discord: DiscordConfig = DiscordConfig::default(),
    updates: UpdatesConfig = UpdatesConfig::default(),
    notifications: NotificationsConfig = NotificationsConfig::default(),
    ui: UIConfig = UIConfig::default(),
    overlay: OverlayConfig = OverlayConfig::default()
});
pub fn config_dir() -> PathBuf {
    actual_home_dir().join(".config/vice")
}
pub fn config_path() -> PathBuf {
    std::env::var_os("VICE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir().join("config.toml"))
}
pub fn canonical_modifier(key: &str) -> &str {
    match key {
        "KEY_RIGHTCTRL" => "KEY_LEFTCTRL",
        "KEY_RIGHTALT" => "KEY_LEFTALT",
        "KEY_RIGHTSHIFT" => "KEY_LEFTSHIFT",
        "KEY_RIGHTMETA" => "KEY_LEFTMETA",
        _ => key,
    }
}
pub fn is_modifier(key: &str) -> bool {
    matches!(
        canonical_modifier(key),
        "KEY_LEFTCTRL" | "KEY_LEFTALT" | "KEY_LEFTSHIFT" | "KEY_LEFTMETA"
    )
}
pub fn normalize_combo(combo: &str) -> String {
    let tokens: Vec<_> = combo
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut mods = HashSet::new();
    let mut mains = vec![];
    for token in &tokens {
        let key = canonical_modifier(token);
        if is_modifier(key) {
            mods.insert(key);
        } else {
            mains.push(*token);
        }
    }
    if mains.len() != 1 {
        return tokens.join("+");
    }
    let mut ordered: Vec<_> = [
        "KEY_LEFTCTRL",
        "KEY_LEFTALT",
        "KEY_LEFTSHIFT",
        "KEY_LEFTMETA",
    ]
    .into_iter()
    .filter(|k| mods.contains(k))
    .collect();
    ordered.push(mains[0]);
    ordered.join("+")
}
pub fn effective_clip_bindings(cfg: &Config) -> Vec<(String, i64)> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    for (key, duration) in std::iter::once((&cfg.hotkeys.clip, cfg.recording.clip_duration)).chain(
        cfg.hotkeys
            .clip_presets
            .iter()
            .map(|p| (&p.key, p.duration)),
    ) {
        let key = normalize_combo(key);
        if !key.is_empty() && seen.insert(key.clone()) {
            out.push((key, duration));
        }
    }
    out
}
fn finite_clamp(value: f64, fallback: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}
impl Config {
    pub fn normalize(&mut self) {
        self.recording.clip_duration = self
            .recording
            .clip_duration
            .clamp(CLIP_DURATION_MIN, CLIP_DURATION_MAX);
        self.hotkeys.clip = normalize_combo(&self.hotkeys.clip);
        for k in [
            &mut self.hotkeys.toggle,
            &mut self.hotkeys.screenshot,
            &mut self.hotkeys.key_overlay,
        ]
        .into_iter()
        .flatten()
        {
            *k = normalize_combo(k);
        }
        self.hotkeys.clip_presets.retain(|p| {
            !p.key.trim().is_empty()
                && (CLIP_DURATION_MIN..=CLIP_DURATION_MAX).contains(&p.duration)
        });
        for p in &mut self.hotkeys.clip_presets {
            p.key = normalize_combo(&p.key);
        }
        let longest = self
            .hotkeys
            .clip_presets
            .iter()
            .map(|p| p.duration)
            .max()
            .unwrap_or(0)
            .max(self.recording.clip_duration);
        self.recording.buffer_duration = self
            .recording
            .buffer_duration
            .max(longest)
            .clamp(self.recording.clip_duration, BUFFER_DURATION_MAX);
        for (value, choices, fallback) in [
            (
                &mut self.recording.gsr_replay_storage,
                &["auto", "ram", "disk"][..],
                "auto",
            ),
            (
                &mut self.recording.gsr_source,
                &["auto", "native", "flatpak"][..],
                "auto",
            ),
            (&mut self.recording.color_depth, &["8", "10"][..], "8"),
        ] {
            *value = value.trim().to_lowercase();
            if !choices.contains(&value.as_str()) {
                *value = fallback.into();
            }
        }
        self.recording.desktop_volume = finite_clamp(self.recording.desktop_volume, 1., 0., 2.);
        self.recording.microphone_volume =
            finite_clamp(self.recording.microphone_volume, 1., 0., 2.);
        for volume in self.recording.audio_track_volumes.values_mut() {
            *volume = finite_clamp(*volume, 1., 0., 2.);
        }
        self.notifications.sound_volume = finite_clamp(self.notifications.sound_volume, 1., 0., 1.);
        let mut seen = HashSet::new();
        self.hotkeys.disable_while_focused = self
            .hotkeys
            .disable_while_focused
            .iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect();
        self.output.directory = resolve_path(&self.output.directory)
            .to_string_lossy()
            .into_owned();
        self.output.image_directory = resolve_path(&self.output.image_directory)
            .to_string_lossy()
            .into_owned();
        if ![
            "bottom-left",
            "bottom-right",
            "top-left",
            "top-right",
            "bottom-center",
            "custom",
        ]
        .contains(&self.overlay.keys_position.as_str())
        {
            self.overlay.keys_position = "bottom-left".into();
        }
        self.overlay.keys_x = self.overlay.keys_x.max(0);
        self.overlay.keys_y = self.overlay.keys_y.max(0);
        self.ui.scale = finite_clamp(self.ui.scale, 1.0, 0.75, 2.0);
        if !["light", "dark"].contains(&self.ui.theme.as_str()) {
            self.ui.theme = "light".into();
        }
        self.overlay.keys_scale = finite_clamp(self.overlay.keys_scale, 1., 0.5, 3.);
    }
    pub fn validate(&self) -> Result<()> {
        validate_hotkeys(&self.hotkeys)?;
        if !(1..=240).contains(&self.recording.fps) {
            bail!("recording.fps must be between 1 and 240");
        }
        if !(0..=51).contains(&self.recording.crf) {
            bail!("recording.crf must be between 0 and 51");
        }
        if self.sharing.port == 0 || self.sharing.public_port == Some(0) {
            bail!("server ports must be positive");
        }
        if self
            .sharing
            .public_port
            .unwrap_or(self.sharing.port.saturating_add(1))
            == self.sharing.port
        {
            bail!("local and public ports must differ");
        }
        if !["mp4", "mkv"].contains(&self.recording.container.as_str()) {
            bail!("container must be mp4 or mkv");
        }
        if !["auto", "gsr", "wf-recorder", "ffmpeg"].contains(&self.recording.backend.as_str()) {
            bail!("unknown recording backend");
        }
        Ok(())
    }
}
pub fn validate_hotkeys(keys: &HotkeyConfig) -> Result<()> {
    let mut seen = HashSet::new();
    let key = normalize_combo(&keys.clip);
    if !key.is_empty() {
        seen.insert(key);
    }
    for p in &keys.clip_presets {
        let key = normalize_combo(&p.key);
        if key.is_empty() || !(CLIP_DURATION_MIN..=CLIP_DURATION_MAX).contains(&p.duration) {
            bail!("clip preset needs a key and duration between 5 and 1800 seconds");
        }
        if !seen.insert(key.clone()) {
            bail!("duplicate clip hotkey: {key}");
        }
    }
    for key in [&keys.toggle, &keys.screenshot, &keys.key_overlay]
        .into_iter()
        .flatten()
    {
        let key = normalize_combo(key);
        if !key.is_empty() && !seen.insert(key.clone()) {
            bail!("duplicate hotkey: {key}");
        }
    }
    Ok(())
}
pub fn load_from(path: &Path) -> Result<Config> {
    let mut cfg: Config = if path.exists() {
        toml::from_str(
            &std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?
    } else {
        let cfg = Config::default();
        save_to(&cfg, path)?;
        cfg
    };
    cfg.normalize();
    Ok(cfg)
}
pub fn load() -> Result<Config> {
    load_from(&config_path())
}
pub fn save_to(cfg: &Config, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(cfg)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
pub fn save(cfg: &Config) -> Result<()> {
    save_to(cfg, &config_path())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_combos_and_invalid_input() {
        assert_eq!(
            normalize_combo("KEY_RIGHTSHIFT+KEY_F9+KEY_RIGHTCTRL"),
            "KEY_LEFTCTRL+KEY_LEFTSHIFT+KEY_F9"
        );
        assert_eq!(
            normalize_combo("KEY_LEFTALT+KEY_RIGHTALT+KEY_F9"),
            "KEY_LEFTALT+KEY_F9"
        );
        assert_eq!(normalize_combo(" KEY_A + KEY_B "), "KEY_A+KEY_B");
    }
    #[test]
    fn partial_toml_defaults_and_unknown_fields() {
        let mut cfg: Config = toml::from_str("[recording]\nclip_duration=1801\ncolor_depth='wrong'\nfuture_key=true\n[overlay]\nkeys_scale=99.0").unwrap();
        cfg.normalize();
        assert_eq!(cfg.recording.clip_duration, 1800);
        assert_eq!(cfg.recording.buffer_duration, 1800);
        assert_eq!(cfg.recording.color_depth, "8");
        assert_eq!(cfg.overlay.keys_scale, 3.);
        assert_eq!(cfg.hotkeys.clip, "KEY_F9");
    }
    #[test]
    fn hotkey_collision_after_canonicalization() {
        let mut c = Config::default();
        c.hotkeys.clip = "KEY_LEFTCTRL+KEY_F9".into();
        c.hotkeys.screenshot = Some("KEY_RIGHTCTRL+KEY_F9".into());
        assert!(c.validate().is_err());
    }
    #[test]
    fn appearance_preserves_existing_installs_and_persists_dark() {
        let mut cfg: Config = toml::from_str("[ui]\nscale=1.5\n").unwrap();
        assert_eq!(cfg.ui.theme, "light");
        cfg.ui.theme = "dark".into();
        let parsed: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed.ui.theme, "dark");
        cfg.ui.theme = "unknown".into();
        cfg.normalize();
        assert_eq!(cfg.ui.theme, "light");
    }
    #[test]
    fn ui_scale_is_persistent_and_bounded() {
        assert!(Config::default().ui.hardware_video_decode);
        let mut cfg = Config::default();
        cfg.ui.scale = 1.5;
        let parsed: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed.ui.scale, 1.5);
        cfg.ui.scale = f64::NAN;
        cfg.normalize();
        assert_eq!(cfg.ui.scale, 1.0);
        cfg.ui.scale = 4.0;
        cfg.normalize();
        assert_eq!(cfg.ui.scale, 2.0);
        cfg.ui.scale = 0.1;
        cfg.normalize();
        assert_eq!(cfg.ui.scale, 0.75);
    }
    #[test]
    fn toml_roundtrip() {
        let cfg = Config::default();
        let parsed: Config = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed, cfg);
    }
}
