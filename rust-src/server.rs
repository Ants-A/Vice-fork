//! Separate loopback control and public, token-only media servers.
use crate::{
    config::{self, Config},
    editor, media,
    playlists::PlaylistStore,
    recorder::Recorder,
    runtime,
};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State, WebSocketUpgrade},
    http::{HeaderMap, Method, Request, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{any, get},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{Mutex, RwLock, Semaphore, broadcast, watch},
};
use tower::ServiceExt;

pub type Shared = Arc<AppState>;
pub struct AppState {
    pub cfg: RwLock<Config>,
    pub recorder: Mutex<Recorder>,
    pub overlay: Mutex<crate::key_overlay::OverlayHost>,
    pub session_highlights: Mutex<Vec<Value>>,
    pub paused: std::sync::atomic::AtomicBool,
    pub recorder_error: RwLock<Option<String>>,
    pub hotkeys_available: std::sync::atomic::AtomicBool,
    pub events: broadcast::Sender<Value>,
    pub shutdown: watch::Sender<bool>,
    pub playlists: Mutex<PlaylistStore>,
    pub files: RwLock<BTreeMap<String, PathBuf>>,
    pub images: RwLock<BTreeMap<String, PathBuf>>,
    pub tokens: Mutex<Value>,
    pub views: Mutex<Value>,
    pub app_state: Mutex<Value>,
    pub export: Mutex<Option<(String, watch::Sender<bool>)>>,
    pub export_committed: std::sync::atomic::AtomicBool,
    pub tunnel_url: RwLock<Option<String>>,
    pub local_url: String,
    pub public_url: String,
    pub active_public_port: u16,
    pub probes: Semaphore,
    pub thumbnail_slots: Semaphore,
    pub identities: RwLock<HashMap<PathBuf, media::FileIdentity>>,
    pub tag_cache: Mutex<HashMap<PathBuf, (u64, u128, Value)>>,
    pub metadata_dirty: std::sync::atomic::AtomicBool,
    pub cache_saved_at: Mutex<Instant>,
    pub hydration_running: std::sync::atomic::AtomicBool,
    pub hydration_requested: std::sync::atomic::AtomicBool,
    pub last_video_request: std::sync::Mutex<Option<Instant>>,
    pub active_video_streams: std::sync::atomic::AtomicUsize,
    pub metadata: Mutex<HashMap<PathBuf, (u64, u128, Value)>>,
    pub media_jobs: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    pub edits: Mutex<()>,
    pub clip_writing: std::sync::atomic::AtomicBool,
    clip_saves: Mutex<()>,
    game_context: Mutex<crate::game_context::GameContext>,
    status_cache: std::sync::Mutex<Option<Value>>,
    managed_names: RwLock<crate::media_names::Names>,
    pub transcodes: Semaphore,
    pub preview_jobs: Mutex<HashMap<PathBuf, Arc<crate::stream_preview::PreviewJob>>>,
    pub playback_sessions: crate::playback_cache::Sessions,
}

pub fn data_dir() -> PathBuf {
    runtime::data_dir()
}
pub fn cache_dir() -> PathBuf {
    runtime::cache_dir()
}
pub fn read_json(path: &FsPath, fallback: Value) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|x| serde_json::from_slice::<Value>(&x).ok())
        .filter(|v| v.is_object() == fallback.is_object() && v.is_array() == fallback.is_array())
        .unwrap_or(fallback)
}
pub fn write_json(path: &FsPath, value: &Value) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, serde_json::to_vec(value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
fn revision(path: &FsPath) -> (u64, u128) {
    std::fs::metadata(path)
        .map(|m| {
            (
                m.len(),
                m.modified()
                    .unwrap_or(SystemTime::UNIX_EPOCH)
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
            )
        })
        .unwrap_or_default()
}
fn cache_prefix(path: &FsPath) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()))[..24].to_owned() + "-"
}
fn identity_cache_key(path: &FsPath, identity: media::FileIdentity) -> String {
    format!(
        "{}{}-{}-{}",
        cache_prefix(path),
        identity.size,
        identity.mtime_ns,
        identity.inode
    )
}
pub(crate) fn cache_key(path: &FsPath) -> String {
    identity_cache_key(path, media::FileIdentity::read(path).unwrap_or_default())
}

struct MediaTemp(PathBuf);
impl Drop for MediaTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct ExportWork(PathBuf);
impl Drop for ExportWork {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub async fn recover_library(state: Shared) -> Result<()> {
    state.recover_library().await
}
fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}
fn html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
#[cfg(test)]
fn scan(dir: &FsPath, extensions: &[&str]) -> BTreeMap<String, PathBuf> {
    scan_indexed(dir, extensions).0
}
fn scan_indexed(
    dir: &FsPath,
    extensions: &[&str],
) -> (
    BTreeMap<String, PathBuf>,
    HashMap<PathBuf, media::FileIdentity>,
) {
    let mut identities = HashMap::new();
    let mut result = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || [
                    ".trimming.",
                    ".trim.",
                    ".wm.",
                    ".levels.",
                    ".mix.",
                    ".fix.",
                    ".export.",
                    ".meta.",
                    ".vice-",
                ]
                .iter()
                .any(|s| name.contains(s))
            {
                continue;
            }
            // Never expose symlinks to files outside the library.
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let ext = p
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            if extensions.contains(&ext.as_str()) {
                let Ok(identity) = entry
                    .metadata()
                    .and_then(|m| media::FileIdentity::from_metadata(&m))
                else {
                    continue;
                };
                identities.insert(p.clone(), identity);
                result.insert(
                    p.file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    p,
                );
            }
        }
    }
    (result, identities)
}
impl AppState {
    pub fn new(cfg: Config) -> Shared {
        let (events, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        let port = cfg.sharing.port;
        let (mut files, mut identities) = scan_indexed(
            &runtime::resolve_path(&cfg.output.directory),
            &["mp4", "mkv", "mov", "webm"],
        );
        let (mut images, mut image_identities) = scan_indexed(
            &runtime::resolve_path(&cfg.output.image_directory),
            &["png", "jpg", "jpeg"],
        );
        let managed_names: crate::media_names::Names =
            serde_json::from_value(read_json(&data_dir().join("media_names.json"), json!({})))
                .unwrap_or_default();
        crate::media_names::apply(
            &managed_names,
            &runtime::resolve_path(&cfg.output.directory),
            false,
            &mut files,
            &mut identities,
        );
        crate::media_names::apply(
            &managed_names,
            &runtime::resolve_path(&cfg.output.image_directory),
            true,
            &mut images,
            &mut image_identities,
        );
        identities.extend(image_identities);
        let (metadata, tags) =
            media::load_metadata_cache(&cache_dir().join("media.json"), &identities);
        Arc::new(Self {
            recorder: Mutex::new(Recorder::new(cfg.clone())),
            overlay: Mutex::new(crate::key_overlay::OverlayHost::default()),
            session_highlights: Mutex::new(Vec::new()),
            paused: std::sync::atomic::AtomicBool::new(false),
            recorder_error: RwLock::new(None),
            hotkeys_available: std::sync::atomic::AtomicBool::new(false),
            cfg: RwLock::new(cfg.clone()),
            events,
            shutdown,
            playlists: Mutex::new(PlaylistStore::new(data_dir().join("playlists.json"))),
            files: RwLock::new(files),
            images: RwLock::new(images),
            identities: RwLock::new(identities),
            tag_cache: Mutex::new(tags),
            metadata_dirty: std::sync::atomic::AtomicBool::new(false),
            cache_saved_at: Mutex::new(Instant::now()),
            hydration_running: std::sync::atomic::AtomicBool::new(false),
            hydration_requested: std::sync::atomic::AtomicBool::new(false),
            last_video_request: std::sync::Mutex::new(None),
            active_video_streams: std::sync::atomic::AtomicUsize::new(0),
            tokens: Mutex::new(read_json(&data_dir().join("share_tokens.json"), json!({}))),
            views: Mutex::new(read_json(&data_dir().join("views.json"), json!({}))),
            app_state: Mutex::new(read_json(&data_dir().join("ui_state.json"), json!({}))),
            export: Mutex::new(None),
            export_committed: std::sync::atomic::AtomicBool::new(false),
            tunnel_url: RwLock::new(None),
            local_url: format!("http://127.0.0.1:{port}"),
            active_public_port: cfg.sharing.public_port.unwrap_or(port.saturating_add(1)),
            public_url: format!(
                "http://{}:{}",
                local_ip(),
                cfg.sharing.public_port.unwrap_or(port.saturating_add(1))
            ),
            probes: Semaphore::new(2),
            thumbnail_slots: Semaphore::new(4),
            metadata: Mutex::new(metadata),
            media_jobs: Mutex::new(HashMap::new()),
            edits: Mutex::new(()),
            clip_writing: std::sync::atomic::AtomicBool::new(false),
            clip_saves: Mutex::new(()),
            game_context: Mutex::new(crate::game_context::GameContext::default()),
            status_cache: std::sync::Mutex::new(None),
            managed_names: RwLock::new(managed_names),
            transcodes: Semaphore::new(1),
            preview_jobs: Mutex::new(HashMap::new()),
            playback_sessions: crate::playback_cache::Sessions::new(cache_dir().join("playback")),
        })
    }
    pub fn emit(&self, value: Value) {
        let _ = self.events.send(value);
    }
    pub async fn rescan(&self) -> usize {
        if self.clip_writing.load(std::sync::atomic::Ordering::Acquire) {
            return 0;
        }
        let Ok(_edit) = self.edits.try_lock() else {
            return 0;
        };
        let active_session = match self.recorder.try_lock() {
            Ok(recorder) => recorder.session_output_path(),
            Err(_) => return 0,
        };
        let managed = self.managed_names.read().await.clone();
        let cfg = self.cfg.read().await.clone();
        let directories = (
            runtime::resolve_path(&cfg.output.directory),
            runtime::resolve_path(&cfg.output.image_directory),
        );
        let ((files, mut identities), (images, image_identities)) =
            tokio::task::spawn_blocking(move || {
                let (mut files, mut identities) =
                    scan_indexed(&directories.0, &["mp4", "mkv", "mov", "webm"]);
                let (mut images, mut image_ids) =
                    scan_indexed(&directories.1, &["png", "jpg", "jpeg"]);
                crate::media_names::apply(
                    &managed,
                    &directories.0,
                    false,
                    &mut files,
                    &mut identities,
                );
                crate::media_names::apply(
                    &managed,
                    &directories.1,
                    true,
                    &mut images,
                    &mut image_ids,
                );
                if let Some(active) = active_session {
                    files.retain(|_, path| path != &active);
                    identities.remove(&active);
                }
                ((files, identities), (images, image_ids))
            })
            .await
            .unwrap_or_default();
        if self.clip_writing.load(std::sync::atomic::Ordering::Acquire) {
            return 0;
        }
        let active_session = match self.recorder.try_lock() {
            Ok(recorder) => recorder.session_output_path(),
            Err(_) => return 0,
        };
        let mut files = files;
        if let Some(active) = active_session {
            files.retain(|_, path| path != &active);
            identities.remove(&active);
        }
        identities.extend(image_identities);
        let mut old = self.files.write().await;
        let added = files.keys().filter(|s| !old.contains_key(*s)).count();
        *old = files;
        drop(old);
        *self.images.write().await = images;
        *self.identities.write().await = identities;
        let mut tokens = self.tokens.lock().await;
        for slug in self.files.read().await.keys() {
            if tokens.get(slug).is_none() {
                tokens[slug] = json!(uuid::Uuid::new_v4().simple().to_string());
            }
        }
        let _ = write_json(&data_dir().join("share_tokens.json"), &tokens);
        added
    }
    /// Recover portable playlist membership before seeding filename game tags.
    /// JSON definitions win when a usable store already exists.
    pub async fn recover_library(self: &Arc<Self>) -> Result<()> {
        let files = self.files.read().await.clone();
        let identities = self.identities.read().await.clone();
        let tags = self.tag_cache.lock().await;
        let entries: Vec<_> = files
            .iter()
            .filter_map(|(slug, path)| {
                let identity = identities.get(path)?;
                let (size, mtime, tags) = tags.get(path)?;
                (*size == identity.size && *mtime == identity.mtime_ns).then(|| {
                    (
                        slug.clone(),
                        tags.get(crate::playlists::PLAYLIST_TAG)
                            .cloned()
                            .unwrap_or(Value::Null),
                    )
                })
            })
            .collect();
        drop(tags);
        let cfg = self.cfg.read().await.clone();
        let custom: Vec<Value> = cfg
            .discord
            .custom_games
            .iter()
            .filter_map(|g| serde_json::to_value(g).ok())
            .collect();
        let mut slugs: std::collections::HashSet<String> = files.keys().cloned().collect();
        slugs.extend(self.images.read().await.keys().map(|s| format!("img:{s}")));
        let mut store = self.playlists.lock().await;
        if store.loaded_from_disk || !store.list_playlists().is_empty() {
            store.merge_metadata(&entries)?;
        } else {
            store.restore_from_metadata(&entries)?;
        }
        store.loaded_from_disk = true;
        store.backfill(
            &slugs,
            &crate::playlists::build_tag_index(&custom),
            cfg.output.auto_playlist_by_game,
        )?;
        self.emit(json!({"type":"playlists_changed","playlists":store.list_playlists()}));
        drop(store);
        self.queue_metadata();
        Ok(())
    }
    pub async fn identity(&self, path: &FsPath) -> Option<media::FileIdentity> {
        self.identities.read().await.get(path).copied()
    }
    pub async fn cached_meta(&self, path: &FsPath) -> Option<Value> {
        let identity = self.identity(path).await?;
        self.metadata
            .lock()
            .await
            .get(path)
            .filter(|(size, mtime, _)| *size == identity.size && *mtime == identity.mtime_ns)
            .map(|(_, _, v)| v.clone())
    }
    pub async fn flush_metadata(&self, force: bool) {
        if !self
            .metadata_dirty
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let mut saved = self.cache_saved_at.lock().await;
        if !force && saved.elapsed() < Duration::from_secs(5) {
            return;
        }
        let identities = self.identities.read().await.clone();
        let metadata = self.metadata.lock().await.clone();
        let tags = self.tag_cache.lock().await.clone();
        self.metadata_dirty
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mut records = serde_json::Map::new();
        for (path, identity) in identities {
            let meta = metadata
                .get(&path)
                .filter(|(size, mtime, value)| {
                    *size == identity.size
                        && *mtime == identity.mtime_ns
                        && value["unreadable"] != true
                })
                .map(|(_, _, v)| v.clone());
            let tags = tags
                .get(&path)
                .filter(|(size, mtime, _)| *size == identity.size && *mtime == identity.mtime_ns)
                .map(|(_, _, v)| v.clone());
            if meta.is_some() || tags.is_some() {
                records.insert(path.to_string_lossy().into_owned(),json!({"identity":[identity.size,identity.mtime_ns],"inode":identity.inode,"meta":meta,"tags":tags}));
            }
        }
        let path = cache_dir().join("media.json");
        let result = tokio::task::spawn_blocking(move || {
            write_json(&path, &json!({"version":1,"files":records}))
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            self.metadata_dirty
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!("could not persist media metadata cache");
        } else {
            *saved = Instant::now();
        }
    }
    /// One worker per library, regardless of how often clients refresh the list.
    /// Recent clips hydrate first; playback pauses background network reads.
    pub fn queue_metadata(self: &Arc<Self>) {
        self.hydration_requested
            .store(true, std::sync::atomic::Ordering::Release);
        if self
            .hydration_running
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                state
                    .hydration_requested
                    .store(false, std::sync::atomic::Ordering::Release);
                let files = state.files.read().await.clone();
                let mut pending = Vec::new();
                for (slug, path) in files {
                    if state.cached_meta(&path).await.is_none() {
                        let modified = state.identity(&path).await.map(|i| i.mtime_ns).unwrap_or(0);
                        pending.push((modified, slug, path));
                    }
                }
                pending.sort_by_key(|a| std::cmp::Reverse(a.0));
                for (_, slug, path) in pending {
                    while state.video_busy() {
                        if *state.shutdown.borrow() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    if *state.shutdown.borrow() {
                        break;
                    }
                    if state.files.read().await.get(&slug) != Some(&path) {
                        continue;
                    }
                    let value = state.meta(&path).await;
                    // A rename/delete/trim may have finished during the probe.
                    // Publish only against the current library entry.
                    let _edit = state.edits.lock().await;
                    if state.files.read().await.get(&slug) != Some(&path) {
                        continue;
                    }
                    if let Some(raw) = value["tags"].get(crate::playlists::PLAYLIST_TAG) {
                        let mut store = state.playlists.lock().await;
                        if store
                            .merge_metadata(&[(slug.clone(), raw.clone())])
                            .unwrap_or(false)
                        {
                            state.emit(json!({"type":"playlists_changed","playlists":store.list_playlists()}));
                        }
                    }
                    state.emit(json!({"type":"clip_saved","clip":state.clip_json(&slug,&path,false).await}));
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                state.flush_metadata(true).await;
                if *state.shutdown.borrow()
                    || !state
                        .hydration_requested
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                {
                    break;
                }
            }
            state
                .hydration_running
                .store(false, std::sync::atomic::Ordering::Release);
            if !*state.shutdown.borrow()
                && state
                    .hydration_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                state.queue_metadata();
            }
        });
    }
    fn video_busy(&self) -> bool {
        self.active_video_streams
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
            || self
                .last_video_request
                .lock()
                .ok()
                .and_then(|t| *t)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(4))
    }
    fn thumbnails_paused(&self) -> bool {
        self.active_video_streams
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
            || self
                .last_video_request
                .lock()
                .ok()
                .and_then(|time| *time)
                .is_some_and(|time| time.elapsed() < Duration::from_millis(200))
    }
    async fn cache_lock(&self, path: &FsPath) -> Arc<Mutex<()>> {
        self.media_jobs
            .lock()
            .await
            .entry(path.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    async fn invalidate_media(&self, path: &FsPath) {
        self.invalidate_media_caches(path, true).await;
    }
    async fn invalidate_media_caches(&self, path: &FsPath, discard_discord: bool) {
        self.metadata.lock().await.remove(path);
        self.tag_cache.lock().await.remove(path);
        self.metadata_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let prefix = cache_prefix(path);
        for dir in ["thumbs", "proxies", "audio", "discord"] {
            if dir == "discord" && !discard_discord {
                continue;
            }
            if let Ok(mut entries) = tokio::fs::read_dir(cache_dir().join(dir)).await {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with(&prefix) && !name.starts_with('.') {
                        let _ = tokio::fs::remove_file(entry.path()).await;
                    }
                }
            }
        }
    }
    pub async fn public_base(&self) -> (String, bool) {
        let cfg = self.cfg.read().await;
        if let Some(url) = &cfg.sharing.base_url {
            return (url.trim_end_matches('/').into(), true);
        }
        if let Some(url) = self.tunnel_url.read().await.as_ref() {
            return (url.clone(), true);
        }
        (
            if cfg.sharing.enabled {
                self.public_url.clone()
            } else {
                self.local_url.clone()
            },
            false,
        )
    }
    pub async fn broadcast_share_links(&self) {
        let (base, public) = self.public_base().await;
        let tokens = self.tokens.lock().await;
        let links: serde_json::Map<String, Value> = self
            .files
            .read()
            .await
            .keys()
            .filter_map(|slug| {
                tokens[slug]
                    .as_str()
                    .map(|token| (slug.clone(), json!(format!("{base}/c/{token}"))))
            })
            .collect();
        self.emit(json!({"type":"share_links_changed","links":links,"share_is_public":public}));
    }
    pub async fn status(&self) -> Value {
        let mut rec = match self.recorder.try_lock() {
            Ok(rec) => rec,
            Err(_) => {
                let cached = self
                    .status_cache
                    .lock()
                    .ok()
                    .and_then(|value| value.clone());
                if let Some(mut cached) = cached {
                    cached["clips"] = json!(self.files.read().await.len());
                    cached["images"] = json!(self.images.read().await.len());
                    return cached;
                }
                self.recorder.lock().await
            }
        };
        let recording = rec.is_healthy();
        let err = self.recorder_error.read().await.clone();
        let (base, public) = self.public_base().await;
        let value = json!({"running":true,"version":crate::VERSION,"clips":self.files.read().await.len(),"images":self.images.read().await.len(),
            "local_url":self.local_url,"public_url":base,"base_url":base,"public_is_tunnel":public,
            "recording":recording,"backend":rec.name(),"gsr":{"source":rec.gsr_source,"flatpak":rec.gsr_source.as_deref()==Some("flatpak")},"session_active":rec.session_elapsed()>0.0,
            "hotkeys_available":self.hotkeys_available.load(std::sync::atomic::Ordering::Relaxed),
            "ready":recording,"recorder_error":err,"update":if self.cfg.read().await.updates.check_on_start{crate::updates::available(&crate::updates::UpdateCache::default().load(),crate::VERSION)}else{None},"key_overlay":self.overlay.lock().await.status(),"cpu_fallback":rec.cpu_fallback,"codec_fallback":rec.codec_fallback,"platform":crate::platform_info::detect_platform(),"window_capture_unavailable":if std::env::var_os("WAYLAND_DISPLAY").is_some() && self.cfg.read().await.recording.window_capture {Some("wayland")} else {None}});
        if let Ok(mut cache) = self.status_cache.lock() {
            *cache = Some(value.clone());
        }
        value
    }
    pub async fn meta(&self, path: &FsPath) -> Value {
        if let Some(value) = self.cached_meta(path).await {
            return value;
        }
        let lock = self.cache_lock(path).await;
        let _guard = lock.lock().await;
        // Another request (or the hydration worker) may have completed while
        // this caller waited. Never launch a second probe for that identity.
        if let Some(value) = self.cached_meta(path).await {
            return value;
        }
        let _slot = self.probes.acquire().await;
        let before = if let Some(identity) = self.identity(path).await {
            identity
        } else {
            let path = path.to_path_buf();
            match tokio::task::spawn_blocking(move || media::FileIdentity::read(&path)).await {
                Ok(Ok(i)) => i,
                _ => return json!({"unreadable":true,"unreadable_reason":"file is unavailable"}),
            }
        };
        let value = match media::probe_media(path).await {
            Ok(m) => serde_json::to_value(m).unwrap_or(json!({})),
            Err(e) => json!({"unreadable":true,"unreadable_reason":e.to_string()}),
        };
        let owned = path.to_path_buf();
        let current = tokio::task::spawn_blocking(move || media::FileIdentity::read(&owned))
            .await
            .ok()
            .and_then(Result::ok);
        if current != Some(before) {
            return json!({"unreadable":false,"metadata_pending":true});
        }
        self.identities.write().await.insert(path.into(), before);
        self.metadata
            .lock()
            .await
            .insert(path.into(), (before.size, before.mtime_ns, value.clone()));
        if value["unreadable"] != true {
            if value["tags"].is_object() {
                self.tag_cache.lock().await.insert(
                    path.into(),
                    (before.size, before.mtime_ns, value["tags"].clone()),
                );
            }
            self.metadata_dirty
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.flush_metadata(false).await;
        }
        value
    }
    pub async fn clip_json(&self, slug: &str, path: &FsPath, probe: bool) -> Value {
        let meta = if probe {
            self.meta(path).await
        } else {
            self.cached_meta(path).await.unwrap_or(json!({}))
        };
        let identity = self.identity(path).await.unwrap_or_default();
        let (base, public) = self.public_base().await;
        let token = self
            .tokens
            .lock()
            .await
            .get(slug)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let game = self.playlists.lock().await.game_for(slug);
        let views = self.views.lock().await[slug].as_u64().unwrap_or(0);
        clip_record(
            slug,
            path,
            identity,
            &meta,
            game.as_deref(),
            views,
            &token,
            &base,
            public,
            &data_dir().join("keys"),
        )
    }
    /// Build a gallery from one consistent in-memory snapshot. In particular,
    /// do not clone container tags or traverse every playlist for every clip.
    pub async fn clips_json(&self) -> Vec<Value> {
        let files = self.files.read().await.clone();
        let identities = self.identities.read().await.clone();
        let (base, public) = self.public_base().await;
        let tokens = self.tokens.lock().await.clone();
        let views = self.views.lock().await.clone();
        let playlists = self.playlists.lock().await.list_playlists();
        let mut games: HashMap<String, String> = HashMap::new();
        for playlist in playlists {
            if playlist["kind"] == "auto" {
                let game = playlist["game"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| playlist["name"].as_str())
                    .unwrap_or("");
                if let Some(slugs) = playlist["clip_slugs"].as_array() {
                    for slug in slugs.iter().filter_map(Value::as_str) {
                        games
                            .entry(slug.to_owned())
                            .or_insert_with(|| game.to_owned());
                    }
                }
            }
        }
        let keys_dir = data_dir().join("keys");
        let metadata = self.metadata.lock().await;
        let empty = Value::Null;
        files
            .into_iter()
            .map(|(slug, path)| {
                let identity = identities.get(&path).copied().unwrap_or_default();
                let meta = metadata
                    .get(&path)
                    .filter(|(size, mtime, _)| {
                        *size == identity.size && *mtime == identity.mtime_ns
                    })
                    .map(|(_, _, value)| value)
                    .unwrap_or(&empty);
                clip_record(
                    &slug,
                    &path,
                    identity,
                    meta,
                    games.get(&slug).map(String::as_str),
                    views[&slug].as_u64().unwrap_or(0),
                    tokens[&slug].as_str().unwrap_or(""),
                    &base,
                    public,
                    &keys_dir,
                )
            })
            .collect()
    }
    pub async fn image_json(&self, slug: &str, path: &FsPath) -> Value {
        let (size, mtime) = revision(path);
        let meta = self.meta(path).await;
        json!({"slug":slug,"name":path.file_name().unwrap_or_default().to_string_lossy(),"size":size,
            "created_at":chrono::DateTime::<chrono::Local>::from(std::fs::metadata(path).ok().and_then(|m|m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH)).to_rfc3339(),
            "game":self.playlists.lock().await.game_for(&format!("img:{slug}")),"width":meta["width"],"height":meta["height"],
            "image_url":format!("/i/{}?v={size}-{mtime}",enc(slug)),"thumb_url":format!("/it/{}?v={size}-{mtime}",enc(slug))})
    }
    async fn remember_name(
        &self,
        old: &FsPath,
        path: &FsPath,
        id: &str,
        image: bool,
        format: &str,
    ) -> Result<()> {
        let mut registry = self.managed_names.write().await;
        let mut names = registry.clone();
        names.remove(old);
        names.insert(
            path.into(),
            crate::media_names::NamedMedia {
                id: id.into(),
                image,
                format: format.into(),
            },
        );
        write_json(
            &data_dir().join("media_names.json"),
            &serde_json::to_value(&names)?,
        )?;
        *registry = names;
        Ok(())
    }
    async fn media_format(&self, path: &FsPath) -> String {
        self.managed_names
            .read()
            .await
            .get(path)
            .map(|entry| entry.format.clone())
            .unwrap_or_else(|| {
                path.extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("mp4")
                    .to_ascii_lowercase()
            })
    }
    pub(crate) async fn media_id(&self, path: &FsPath, image: bool) -> String {
        if let Some(id) = self
            .managed_names
            .read()
            .await
            .get(path)
            .map(|entry| entry.id.clone())
        {
            return id;
        }
        let preferred = path.file_stem().unwrap_or_default().to_string_lossy();
        let files = if image {
            self.images.read().await
        } else {
            self.files.read().await
        };
        crate::media_names::available_id(path, &preferred, &files)
    }
    pub async fn saved(&self, path: &FsPath, image: bool) -> Result<()> {
        let _edit = self.edits.lock().await;
        self.saved_locked(path, image).await
    }
    async fn saved_locked(&self, path: &FsPath, image: bool) -> Result<()> {
        let cfg = self.cfg.read().await.clone();
        let game = crate::active_window::get_active_window()
            .await
            .and_then(|win| crate::active_window::match_game(&win, &cfg.discord.custom_games));
        self.saved_locked_with_game(path, image, game).await
    }
    async fn saved_locked_with_game(
        &self,
        path: &FsPath,
        image: bool,
        game: Option<String>,
    ) -> Result<()> {
        let slug = self.media_id(path, image).await;
        if path
            .file_stem()
            .is_none_or(|stem| stem.to_string_lossy() != slug)
        {
            let format = self.media_format(path).await;
            self.remember_name(path, path, &slug, image, &format)
                .await?;
        }
        let owned = path.to_path_buf();
        let identity =
            tokio::task::spawn_blocking(move || media::FileIdentity::read(&owned)).await??;
        self.invalidate_media(path).await;
        self.identities.write().await.insert(path.into(), identity);
        if image {
            let mut images = self.images.write().await;
            images.retain(|id, candidate| candidate != path || id == &slug);
            images.insert(slug.clone(), path.into());
            drop(images);
        } else {
            let mut files = self.files.write().await;
            files.retain(|id, candidate| candidate != path || id == &slug);
            files.insert(slug.clone(), path.into());
            drop(files);
            let mut tokens = self.tokens.lock().await;
            if tokens.get(&slug).is_none() {
                tokens[&slug] = json!(uuid::Uuid::new_v4().simple().to_string());
            }
            write_json(&data_dir().join("share_tokens.json"), &tokens)?;
        }
        let cfg = self.cfg.read().await.clone();
        if cfg.output.auto_playlist_by_game
            && let Some(game) = game
        {
            let member = if image {
                format!("img:{slug}")
            } else {
                slug.clone()
            };
            let mut ps = self.playlists.lock().await;
            ps.record_auto(&game, &member, None, false)?;
            self.emit(json!({"type":"playlists_changed","playlists":ps.list_playlists()}));
        }
        if image {
            crate::audio::play(
                "screenshot",
                cfg.notifications.sound_volume,
                cfg.notifications.screenshot_sound.as_deref(),
            );
            self.emit(json!({"type":"image_saved","image":self.image_json(&slug,path).await}));
        } else {
            self.emit(json!({"type":"clip_saved","clip":self.clip_json(&slug,path,true).await}));
        }
        Ok(())
    }
    pub async fn observe_game(&self, game: Option<&str>) {
        self.game_context.lock().await.observe(game, Instant::now());
    }
    async fn replay_game(&self, seconds: u64, cfg: &Config) -> Option<String> {
        let game = crate::active_window::get_active_window()
            .await
            .and_then(|win| crate::active_window::match_game(&win, &cfg.discord.custom_games));
        let mut context = self.game_context.lock().await;
        let now = Instant::now();
        context.observe(game.as_deref(), now);
        context.game(now, Duration::from_secs(seconds.max(1)))
    }
    pub async fn save_clip(&self, duration: Option<u64>) -> Result<PathBuf> {
        // Snapshot before waiting on the recorder or doing any post-processing.
        // The replay still contains the game even when focus moves to Vice or
        // a different workspace while the resulting file is being finalized.
        let cfg = self.cfg.read().await.clone();
        let game = self
            .replay_game(
                duration.unwrap_or(cfg.recording.clip_duration.max(1) as u64),
                &cfg,
            )
            .await;
        let _save = self.clip_saves.lock().await;
        let _ = self.status().await;
        struct Publishing<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Publishing<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        self.clip_writing
            .store(true, std::sync::atomic::Ordering::Release);
        let _publishing = Publishing(&self.clip_writing);
        let cfg = self.cfg.read().await.clone();
        crate::audio::play(
            "clip",
            cfg.notifications.sound_volume,
            cfg.notifications.clip_sound.as_deref(),
        );
        self.emit(json!({"type":"clip_saving"}));
        let result = {
            let mut recorder = self.recorder.lock().await;
            recorder.game_tag = game.clone();
            recorder.save_clip(duration).await
        };
        match result {
            Ok(path) => {
                let _edit = self.edits.lock().await;
                self.saved_locked_with_game(&path, false, game).await?;
                Ok(path)
            }
            Err(e) => {
                crate::audio::play(
                    "clip_failed",
                    cfg.notifications.sound_volume,
                    cfg.notifications.clip_failed_sound.as_deref(),
                );
                self.emit(json!({"type":"clip_error","error":e.to_string()}));
                Err(e)
            }
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn clip_record(
    slug: &str,
    path: &FsPath,
    identity: media::FileIdentity,
    meta: &Value,
    game: Option<&str>,
    views: u64,
    token: &str,
    base: &str,
    public: bool,
    keys_dir: &FsPath,
) -> Value {
    let (size, mtime) = (identity.size, identity.mtime_ns);
    let stamp = chrono::DateTime::<chrono::Local>::from(
        SystemTime::UNIX_EPOCH + Duration::from_nanos(mtime.min(u64::MAX as u128) as u64),
    )
    .to_rfc3339();
    json!({"slug":slug,"name":path.file_name().unwrap_or_default().to_string_lossy(),"size":size,"created_at":stamp,
        "game":game,"views":views,"keys":keys_dir.join(format!("{slug}.json")).exists(),
        "metadata_pending":meta["duration"].is_null()&&meta["unreadable"]!=true,"duration":meta.get("duration"),"width":meta.get("width"),"height":meta.get("height"),"vcodec":meta.get("vcodec"),
        "audio_tracks":meta.get("audio_tracks").cloned().unwrap_or(json!([])),"unreadable":meta["unreadable"].as_bool().unwrap_or(false),"unreadable_reason":meta["unreadable_reason"].as_str().unwrap_or(""),
        "share_url":format!("{base}/c/{token}"),"share_is_public":public,"video_url":format!("/v/{}?v={size}-{mtime}",enc(slug)),"thumb_url":format!("/t/{}?v={size}-{mtime}",enc(slug))})
}

fn local_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| {
            s.connect("192.0.2.1:80").ok()?;
            Some(s.local_addr().ok()?.ip().to_string())
        })
        .unwrap_or("127.0.0.1".into())
}

#[derive(Debug)]
pub(crate) struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"ok":false,"error":self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, e.to_string())
    }
}
type ApiResult = std::result::Result<Response, ApiError>;
const FILE_URI_ENCODE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'#')
    .add(b'?')
    .add(b'%');
fn ok(value: Value) -> ApiResult {
    Ok(Json(value).into_response())
}
fn bad(msg: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn missing() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found".into())
}
async fn file(path: PathBuf, req: Request<Body>) -> ApiResult {
    // Larger reads give CIFS/NFS a useful batch per round trip. Small image
    // assets keep the default buffer and byte ranges retain their exact bounds.
    let media = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["mp4", "mkv", "mov", "webm", "m4a", "aac", "ogg"].contains(&e));
    if media {
        return crate::media_stream::serve(path, req)
            .await
            .map_err(ApiError::from);
    }
    let service = tower_http::services::ServeFile::new(path);
    let response = service
        .oneshot(req)
        .await
        .map_err(|e| bad(&e.to_string()))?;
    let mut response = response.map(Body::new);
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    Ok(response)
}
async fn local_guard(req: Request<Body>, next: axum::middleware::Next) -> Response {
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let host_ok = host.starts_with("127.0.0.1:")
        || host.starts_with("localhost:")
        || host.starts_with("[::1]:");
    let origin_ok = req
        .headers()
        .get("origin")
        .and_then(|o| o.to_str().ok())
        .is_none_or(|o| o == format!("http://{host}"));
    if !host_ok || !origin_ok {
        return (StatusCode::FORBIDDEN, "loopback origin required").into_response();
    }
    next.run(req).await
}
pub fn local_router(state: Shared) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/ws", get(ws))
        .route("/api/{*path}", any(api))
        .route("/{kind}/{slug}", get(local_media))
        .with_state(state)
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(axum::middleware::from_fn(local_guard))
}
pub fn public_router(state: Shared) -> Router {
    Router::new()
        .route("/c/{slug}", get(public_embed))
        .route("/v/{slug}", get(public_video))
        .route("/t/{slug}", get(public_thumb))
        .with_state(state)
}
fn asset_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("VICE_ASSET_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("VICE_RESOURCE_DIR") {
        return PathBuf::from(dir).join("ui");
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let installed = exe
        .parent()
        .unwrap_or(FsPath::new("."))
        .join("../share/vice/ui");
    if installed.join("index.html").exists() {
        installed
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vice/ui")
    }
}
async fn ui() -> ApiResult {
    let html = tokio::fs::read_to_string(asset_root().join("index.html"))
        .await
        .map_err(|_| missing())?;
    Ok(Html(html.replace("__VICE_VERSION__", crate::VERSION)).into_response())
}
async fn ws(
    State(state): State<Shared>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if headers
        .get("origin")
        .and_then(|o| o.to_str().ok())
        .is_some_and(|o| o != state.local_url && !o.starts_with("http://localhost:"))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade.on_upgrade(move |socket| async move {
        let (mut sender,mut receiver)=socket.split(); let mut events=state.events.subscribe();
        let mut status=state.status().await; status["type"]=json!("status");
        if sender.send(axum::extract::ws::Message::Text(status.to_string().into())).await.is_err() {return;}
        loop {tokio::select! {
            event=events.recv()=>match event {Ok(v)=>if sender.send(axum::extract::ws::Message::Text(v.to_string().into())).await.is_err(){break;}, Err(broadcast::error::RecvError::Lagged(_))=>continue, Err(_)=>break},
            msg=receiver.next()=>match msg {Some(Ok(axum::extract::ws::Message::Ping(v)))=>{if sender.send(axum::extract::ws::Message::Pong(v)).await.is_err(){break;}},Some(Ok(axum::extract::ws::Message::Close(_)))|None|Some(Err(_))=>break,_=>{}}
        }}
    })
}
async fn public_embed(
    State(s): State<Shared>,
    Path(slug): Path<String>,
    req: Request<Body>,
) -> ApiResult {
    serve_media(s, "c", &slug, true, req).await
}
async fn public_video(
    State(s): State<Shared>,
    Path(slug): Path<String>,
    req: Request<Body>,
) -> ApiResult {
    serve_media(s, "v", &slug, true, req).await
}
async fn public_thumb(
    State(s): State<Shared>,
    Path(slug): Path<String>,
    req: Request<Body>,
) -> ApiResult {
    serve_media(s, "t", &slug, true, req).await
}
async fn local_media(
    State(s): State<Shared>,
    Path((kind, slug)): Path<(String, String)>,
    req: Request<Body>,
) -> ApiResult {
    if ["styles", "scripts", "fonts"].contains(&kind.as_str()) {
        if slug.contains('/') || slug == ".." {
            return Err(missing());
        }
        return file(asset_root().join(kind).join(slug), req).await;
    }
    serve_media(s, &kind, &slug, false, req).await
}
async fn serve_media(
    s: Shared,
    kind: &str,
    reference: &str,
    public: bool,
    req: Request<Body>,
) -> ApiResult {
    if !["c", "v", "t", "i", "it"].contains(&kind) {
        return Err(missing());
    }
    if public && !s.cfg.read().await.sharing.enabled {
        return Err(missing());
    }
    let image = kind == "i" || kind == "it";
    let exact = if !public && (image || s.files.read().await.contains_key(reference)) {
        Some(reference.to_owned())
    } else {
        s.tokens.lock().await.as_object().and_then(|m| {
            m.iter()
                .find(|(_, v)| v.as_str() == Some(reference))
                .map(|(k, _)| k.clone())
        })
    };
    let slug = if let Some(slug) = exact {
        slug
    } else if kind == "v"
        && [".mp4", ".mkv"]
            .iter()
            .any(|ext| reference.to_lowercase().ends_with(ext))
    {
        let stem = &reference[..reference.len() - 4];
        if !public && s.files.read().await.contains_key(stem) {
            stem.to_owned()
        } else {
            s.tokens
                .lock()
                .await
                .as_object()
                .and_then(|m| {
                    m.iter()
                        .find(|(_, v)| v.as_str() == Some(stem))
                        .map(|(k, _)| k.clone())
                })
                .ok_or_else(missing)?
        }
    } else {
        return Err(missing());
    };
    let path = if image {
        s.images.read().await.get(&slug).cloned()
    } else {
        s.files.read().await.get(&slug).cloned()
    }
    .ok_or_else(missing)?;
    if kind == "c" {
        let (base, _) = s.public_base().await;
        let meta = s.meta(&path).await;
        let cfg = s.cfg.read().await;
        let color = cfg.sharing.embed_color.clone();
        return Ok(Html(format!("<!doctype html><html><head><meta charset=\"utf-8\"><title>{0} · Vice</title><meta property=\"og:title\" content=\"{0}\"><meta property=\"og:type\" content=\"video.other\"><meta property=\"og:video\" content=\"{1}/v/{2}\"><meta property=\"og:video:type\" content=\"video/mp4\"><meta property=\"og:video:width\" content=\"{3}\"><meta property=\"og:video:height\" content=\"{4}\"><meta property=\"og:image\" content=\"{1}/t/{2}\"><meta name=\"theme-color\" content=\"{5}\"><meta name=\"twitter:card\" content=\"player\"></head><body style=\"background:#111;color:#eee;margin:0\"><video controls autoplay playsinline style=\"width:100%;height:100vh\" src=\"/v/{2}\" poster=\"/t/{2}\"></video></body></html>",html(&slug),html(&base),enc(reference),meta["width"],meta["height"],html(&color))).into_response());
    }
    if kind == "t" || kind == "it" {
        // Reuse thumbnails from the previous implementation before touching
        // the recording. These cache identities use size and nanosecond mtime.
        let identity = s.identity(&path).await;
        if let Some(identity) = identity {
            let legacy = cache_dir().join("thumbs").join(format!(
                "{}_{}_{}.jpg",
                path.file_stem().unwrap_or_default().to_string_lossy(),
                identity.size,
                identity.mtime_ns
            ));
            if revision(&legacy).0 > 0 {
                return thumbnail_file(legacy, identity, req).await;
            }
            let current = cache_dir()
                .join("thumbs")
                .join(format!("{}.jpg", indexed_cache_key(&s, &path).await));
            if revision(&current).0 > 0 {
                return thumbnail_file(current, identity, req).await;
            }
        }
        if s.thumbnails_paused() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "thumbnail preparation paused during playback; retry".into(),
            ));
        }
        let args = vec![
            "ffmpeg".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-threads".into(),
            "1".into(),
            "-filter_threads".into(),
            "1".into(),
            "-probesize".into(),
            "524288".into(),
            "-analyzeduration".into(),
            "200000".into(),
            "-i".into(),
            path.to_string_lossy().into(),
            "-map".into(),
            "0:v:0".into(),
            "-an".into(),
            "-sn".into(),
            "-dn".into(),
            "-frames:v".into(),
            "1".into(),
            "-vf".into(),
            "scale=w='min(320,iw)':h='min(180,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2".into(),
            "-q:v".into(),
            "6".into(),
            "-threads:v".into(),
            "1".into(),
            "-f".into(),
            "image2".into(),
        ];
        let thumb = cached_media(&s, &path, "thumbs", "jpg", "", args, 60, false).await?;
        return match identity {
            Some(identity) => thumbnail_file(thumb, identity, req).await,
            None => file(thumb, req).await,
        };
    }
    if kind == "v" {
        if let Ok(mut at) = s.last_video_request.lock() {
            *at = Some(Instant::now());
        }
        let requested = req
            .uri()
            .query()
            .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("proxy=")));
        if !public && matches!(requested, Some("1" | "webm" | "small")) {
            let meta = s.meta(&path).await;
            let webm = requested == Some("webm");
            if webm
                || requested == Some("small")
                || !matches!(
                    meta["vcodec"].as_str(),
                    Some("h264" | "vp8" | "vp9" | "av1")
                )
            {
                if !webm
                    && requested != Some("small")
                    && let Some(identity) = s.identity(&path).await
                {
                    let legacy = cache_dir().join("proxies").join(format!(
                        "{}_{}_{}_v2.mp4",
                        path.file_stem().unwrap_or_default().to_string_lossy(),
                        identity.size,
                        identity.mtime_ns
                    ));
                    if revision(&legacy).0 > 0 {
                        return video_file(s, legacy, req).await;
                    }
                }
                let mut args: Vec<String> = [
                    "ffmpeg",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostats",
                    "-threads",
                    "2",
                    "-filter_threads",
                    "1",
                    "-i",
                ]
                .map(str::to_owned)
                .to_vec();
                args.extend([path.to_string_lossy().into(),"-map".into(),"0:v:0".into(),"-map".into(),"0:a:0?".into(),"-vf".into(),"scale=w='min(1280,iw)':h='min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,fps=30".into(),"-threads:v".into(),"2".into()]);
                let ext = if webm {
                    args.extend(
                        [
                            "-c:v",
                            "libvpx",
                            "-deadline",
                            "realtime",
                            "-cpu-used",
                            "8",
                            "-b:v",
                            "1800k",
                            "-crf",
                            "12",
                            "-g",
                            "60",
                            "-pix_fmt",
                            "yuv420p",
                            "-c:a",
                            "libopus",
                            "-b:a",
                            "96k",
                            "-f",
                            "webm",
                        ]
                        .map(str::to_owned),
                    );
                    "webm"
                } else {
                    args.extend(
                        [
                            "-c:v",
                            "libx264",
                            "-b:v",
                            "1800k",
                            "-maxrate",
                            "2200k",
                            "-bufsize",
                            "3600k",
                            "-preset",
                            "veryfast",
                            "-crf",
                            "23",
                            "-pix_fmt",
                            "yuv420p",
                            "-c:a",
                            "aac",
                            "-b:a",
                            "160k",
                            "-movflags",
                            "+faststart",
                        ]
                        .map(str::to_owned),
                    );
                    "mp4"
                };
                return crate::stream_preview::serve(s, path, args, ext, req)
                    .await
                    .map_err(ApiError::from);
            }
        }
        return serve_video_file(s, path, req, !public).await;
    }
    let mime = crate::media_names::mime(&s.media_format(&path).await);
    let mut response = file(path, req).await?;
    if let Some(mime) = mime {
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(mime),
        );
    }
    Ok(response)
}
/// A thumbnail URL contains the source revision. Cache its small response in
/// the browser instead of making the Wi-Fi client refetch on every visit.
async fn thumbnail_file(
    path: PathBuf,
    identity: media::FileIdentity,
    req: Request<Body>,
) -> ApiResult {
    let revision = format!("{}-{}", identity.size, identity.mtime_ns);
    let version = req
        .uri()
        .query()
        .and_then(|query| query.split('&').find_map(|part| part.strip_prefix("v=")));
    let cache = match version {
        Some(version) if version == revision => "private, max-age=31536000, immutable",
        Some(_) => "no-store",
        None => "no-cache",
    };
    let mut response = file(path, req).await?;
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(cache),
    );
    Ok(response)
}

pub(crate) async fn indexed_cache_key(s: &Shared, path: &FsPath) -> String {
    if let Some(identity) = s.identity(path).await {
        identity_cache_key(path, identity)
    } else {
        cache_key(path)
    }
}
struct VideoActivity(Shared);
impl Drop for VideoActivity {
    fn drop(&mut self) {
        self.0
            .active_video_streams
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut at) = self.0.last_video_request.lock() {
            *at = Some(Instant::now());
        }
    }
}
pub(crate) async fn video_file(state: Shared, path: PathBuf, req: Request<Body>) -> ApiResult {
    // Completed previews have their own persistent cache and different bytes.
    // An original's viewer session must never replace a converted response.
    serve_video_file(state, path, req, false).await
}
async fn serve_video_file(
    state: Shared,
    path: PathBuf,
    req: Request<Body>,
    private: bool,
) -> ApiResult {
    let mime = crate::media_names::mime(&state.media_format(&path).await);
    let session = req
        .uri()
        .query()
        .filter(|_| private)
        .and_then(|query| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix("playback_session="))
        })
        .map(str::to_owned);
    let response = if let Some(id) = session {
        state.playback_sessions.serve(&id, req, mime).await?
    } else {
        crate::media_stream::serve_as(path, req, mime).await?
    };
    state
        .active_video_streams
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let activity = VideoActivity(state);
    Ok(response.map(move |body| {
        Body::from_stream(body.into_data_stream().map(move |chunk| {
            let _keep_alive = &activity;
            chunk
        }))
    }))
}
/// Publish complete cache files atomically, deduplicate concurrent requests, and
/// reject a result whose source was edited while FFmpeg was reading it.
#[allow(clippy::too_many_arguments)]
async fn cached_media(
    s: &Shared,
    source: &FsPath,
    kind: &str,
    ext: &str,
    extra: &str,
    mut args: Vec<String>,
    seconds: u64,
    transcode: bool,
) -> Result<PathBuf> {
    let key = indexed_cache_key(s, source).await;
    let target = cache_dir().join(kind).join(format!("{key}{extra}.{ext}"));
    let lock = s.cache_lock(&target).await;
    let _guard = lock.lock().await;
    if revision(&target).0 > 0 {
        return Ok(target);
    }
    let _slot = if transcode {
        s.transcodes.acquire().await?
    } else {
        // Queue cold visible thumbnails instead of rejecting every request
        // that cannot start within 50ms and forcing exponential client retries.
        let slot = s.thumbnail_slots.acquire().await?;
        if s.thumbnails_paused() {
            bail!("thumbnail preparation paused during playback; retry");
        }
        slot
    };
    tokio::fs::create_dir_all(target.parent().context("cache parent")?).await?;
    let tmp = MediaTemp(target.with_file_name(format!(".{}.{ext}", uuid::Uuid::new_v4().simple())));
    args.extend(["-y".into(), tmp.0.to_string_lossy().into_owned()]);
    if kind == "proxies" || kind == "discord" {
        run_preview_process(
            s,
            source,
            args,
            seconds,
            if kind == "discord" {
                "discord_progress"
            } else {
                "preview_progress"
            },
        )
        .await?;
    } else {
        run_command(args, seconds).await?;
    }
    let owned = source.to_path_buf();
    let current = tokio::task::spawn_blocking(move || cache_key(&owned)).await?;
    if key != current || revision(&tmp.0).0 == 0 {
        bail!("the clip changed while its preview was being prepared; retry");
    }
    tokio::fs::rename(&tmp.0, &target).await?;
    Ok(target)
}

async fn run_preview_process(
    state: &Shared,
    source: &FsPath,
    args: Vec<String>,
    seconds: u64,
    event: &str,
) -> Result<()> {
    let mut failure = None;
    let candidates = if event == "discord_progress" {
        crate::encoding::discord_command_candidates(&args).await
    } else {
        crate::encoding::command_candidates(&args).await
    };
    for candidate in candidates {
        let encoder = crate::encoding::video_encoder(&candidate)
            .unwrap_or("unknown")
            .to_owned();
        match run_media_attempt(state, source, candidate, seconds, event).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if *state.shutdown.borrow() {
                    return Err(error);
                }
                tracing::warn!(%encoder, %error, task=event, "video encoder failed; trying the next available encoder");
                failure = Some(error);
            }
        }
    }
    Err(failure.unwrap_or_else(|| anyhow::anyhow!("no preview encoder available")))
}

pub(crate) async fn run_preview_attempt(
    state: &Shared,
    source: &FsPath,
    args: Vec<String>,
    seconds: u64,
) -> Result<()> {
    run_media_attempt(state, source, args, seconds, "preview_progress").await
}
async fn run_media_attempt(
    state: &Shared,
    source: &FsPath,
    args: Vec<String>,
    seconds: u64,
    event: &str,
) -> Result<()> {
    run_media_attempt_for(state, source, args, seconds, event, None).await
}
async fn run_media_attempt_for(
    state: &Shared,
    source: &FsPath,
    mut args: Vec<String>,
    seconds: u64,
    event: &str,
    expected_duration: Option<f64>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let slug = state.media_id(source, false).await;
    let duration = expected_duration.unwrap_or(
        state
            .cached_meta(source)
            .await
            .and_then(|v| v["duration"].as_f64())
            .unwrap_or(0.0),
    );
    let encoder = crate::encoding::video_encoder(&args)
        .unwrap_or("stream copy")
        .to_owned();
    tracing::info!(%encoder, task=event, %slug, "Preparing video conversion");
    args.splice(1..1, ["-progress".into(), "pipe:1".into()]);
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let mut lines = BufReader::new(child.stdout.take().context("preview stdout")?).lines();
    let mut stderr = child.stderr.take().context("preview stderr")?;
    state.emit(
        json!({"type":event,"slug":slug,"encoder":encoder,"phase":"encoding","progress":0.0}),
    );
    let advanced = std::sync::Mutex::new(Instant::now());
    let progress = async {
        let mut furthest = -1.0_f64;
        let mut last = Instant::now() - Duration::from_secs(1);
        while let Some(line) = lines.next_line().await? {
            if let Some(fraction) = editor::parse_progress(&line, duration) {
                if fraction > furthest {
                    furthest = fraction;
                    *advanced.lock().unwrap() = Instant::now();
                }
                if last.elapsed() >= Duration::from_millis(200) {
                    state.emit(
                        json!({"type":event,"slug":slug,"encoder":encoder,"phase":"encoding","progress":fraction}),
                    );
                    last = Instant::now();
                }
            }
        }
        Ok::<_, std::io::Error>(())
    };
    let errors = async {
        let mut tail = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let count = stderr.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..count]);
            if tail.len() > 4096 {
                tail.drain(..tail.len() - 4096);
            }
        }
        Ok::<_, std::io::Error>(tail)
    };
    let stalled = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if advanced.lock().unwrap().elapsed() >= Duration::from_secs(90) {
                break;
            }
        }
    };
    let mut shutdown = state.shutdown.subscribe();
    let (progress, errors, status) = tokio::select! {
        result=tokio::time::timeout(Duration::from_secs(seconds),async{tokio::join!(progress,errors,child.wait())})=>result.context("preview generation timed out")?,
        _=shutdown.changed()=>bail!("video conversion stopped"),
        _=stalled=>bail!("encoder made no progress for 90 seconds"),
    };
    progress?;
    let status = status?;
    let errors = errors?;
    if !status.success() {
        bail!(
            "preview generation failed: {}",
            String::from_utf8_lossy(&errors)
        );
    }
    state.emit(
        json!({"type":event,"slug":slug,"encoder":encoder,"phase":"encoding","progress":1.0}),
    );
    Ok(())
}
pub async fn run_command(args: Vec<String>, seconds: u64) -> Result<()> {
    crate::encoding::run_video_job(&args, Duration::from_secs(seconds)).await
}
fn start_uninstall() -> ApiResult {
    use std::os::unix::process::CommandExt;
    let managed = |message: &str| Err(ApiError(StatusCode::CONFLICT, message.to_owned()));
    if runtime::in_vice_flatpak() {
        return managed(
            "Remove Vice with: flatpak uninstall io.github.splitshotz.Vice. Recordings remain in their output folders.",
        );
    }
    let executable = std::env::current_exe().map_err(anyhow::Error::from)?;
    if executable.starts_with("/nix/store") {
        return managed(
            "Vice is managed by Nix. Remove services.vice.enable or the package and rebuild.",
        );
    }
    if runtime::which("pacman").is_some()
        && std::process::Command::new("pacman")
            .arg("-Qo")
            .arg(&executable)
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout).contains("vice-clipper")
            })
    {
        return managed("Vice is managed by pacman. Run: sudo pacman -Rns vice-clipper");
    }
    let prefix = std::env::var_os("VICE_INSTALL_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime::actual_home_dir().join(".local"));
    if executable.canonicalize().ok() != prefix.join("bin/vice").canonicalize().ok()
        || !prefix.join("bin/vice").is_file()
    {
        return managed(
            "This binary is outside Vice's local user installation. Remove it with the package manager that installed it, or use ./install.sh --uninstall for a source installation.",
        );
    }
    let child = std::process::Command::new(&executable)
        .args(["uninstall", "--yes"])
        .env("VICE_INSTALL_PREFIX", &prefix)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(anyhow::Error::from)?;
    // The separate process stops this daemon through IPC after the HTTP
    // response completes, then removes only installer-owned artifacts.
    ok(json!({"ok":true,"started":true,"pid":child.id()}))
}

async fn api(State(s): State<Shared>, Path(path): Path<String>, req: Request<Body>) -> ApiResult {
    let method = req.method().clone();
    let query = req.uri().query().unwrap_or("").to_owned();
    let headers = req.headers().clone();
    let parts: Vec<&str> = path.split('/').collect();
    let raw = axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024)
        .await
        .map_err(|_| bad("request too large"))?;
    let body = if raw.is_empty() || path.ends_with("/annotate") {
        json!({})
    } else {
        serde_json::from_slice::<Value>(&raw).map_err(|_| bad("invalid JSON"))?
    };
    match (method.as_str(), parts.as_slice()) {
        ("DELETE", ["playback", id]) => {
            s.playback_sessions.close(id);
            ok(json!({"ok":true}))
        }
        ("POST", ["playback", id, "touch"]) => {
            if !s.playback_sessions.touch(id) {
                return Err(missing());
            }
            ok(json!({"ok":true}))
        }
        ("GET", ["status"]) => ok(s.status().await),
        ("GET", ["config"]) => {
            ok(serde_json::to_value(s.cfg.read().await.clone()).map_err(anyhow::Error::from)?)
        }
        ("POST", ["config"]) => {
            let previous = s.cfg.read().await.clone();
            let mut value = serde_json::to_value(config::load().unwrap_or(previous.clone()))
                .map_err(anyhow::Error::from)?;
            merge(&mut value, &body);
            let mut cfg: Config = serde_json::from_value(value).map_err(|e| bad(&e.to_string()))?;
            cfg.normalize();
            cfg.validate()?;
            config::save(&cfg)?;
            let restart = cfg.sharing.enabled != previous.sharing.enabled
                || cfg.sharing.port != previous.sharing.port
                || cfg.sharing.public_port != previous.sharing.public_port;
            let mut rec = s.recorder.lock().await;
            let changed = serde_json::to_value(&cfg.recording).ok()
                != serde_json::to_value(&previous.recording).ok()
                || cfg.output.directory != previous.output.directory;
            let result = if changed {
                rec.stop().await?;
                *rec = Recorder::new(cfg.clone());
                if s.paused.load(std::sync::atomic::Ordering::Relaxed) {
                    Ok(())
                } else {
                    rec.start().await
                }
            } else {
                rec.cfg = cfg.clone();
                Ok(())
            };
            *s.cfg.write().await = cfg;
            let err = result.err().map(|e| e.to_string());
            if changed {
                *s.recorder_error.write().await = err.clone();
            }
            s.emit(json!({"type":"config_changed"}));
            s.broadcast_share_links().await;
            ok(json!({"ok":true,"applied":err.is_none(),"warning":err,"restart_required":restart}))
        }
        ("GET", ["app-state"]) => ok(s.app_state.lock().await.clone()),
        ("POST", ["app-state"]) => {
            if !body.is_object() {
                return Err(bad("expected an object"));
            }
            let mut state = s.app_state.lock().await;
            for (k, v) in body.as_object().unwrap() {
                state[k] = v.clone();
            }
            write_json(&data_dir().join("ui_state.json"), &state)?;
            ok(state.clone())
        }
        ("GET", ["displays"]) => {
            let mut cfg = s.cfg.read().await.clone();
            if let Some(backend) = query.strip_prefix("backend=") {
                cfg.recording.backend = backend.into();
            }
            ok(crate::recorder::list_displays(&cfg).await)
        }
        ("GET", ["audio-sources"]) => ok(crate::recorder::list_audio_sources().await),
        ("POST", ["trigger"]) => {
            tokio::spawn(async move {
                let _ = s.save_clip(None).await;
            });
            ok(json!({"ok":true}))
        }
        ("POST", ["quit"]) => {
            let shutdown = s.shutdown.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = shutdown.send(true);
            });
            ok(json!({"ok":true}))
        }
        ("POST", ["update", "check"]) => ok(crate::updates::check_update(true)
            .await
            .unwrap_or(Value::Null)),
        ("POST", ["library", "rescan"]) => {
            let added = s.rescan().await;
            let cfg = s.cfg.read().await;
            s.emit(json!({"type":"library_changed","added":added}));
            ok(
                json!({"ok":true,"added":added,"clips":s.files.read().await.len(),"directory":cfg.output.directory,"available":runtime::resolve_path(&cfg.output.directory).is_dir()}),
            )
        }
        ("GET", ["game-icon"]) => {
            let parsed = reqwest::Url::parse(&format!("http://localhost/?{query}"))
                .map_err(anyhow::Error::from)?;
            let name = parsed
                .query_pairs()
                .find(|(key, _)| key == "name")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            if name.trim().is_empty() || name.len() > 256 {
                return Err(bad("a game name is required"));
            }
            Ok(crate::playlist_icons::serve_game_icon(&name).await)
        }
        ("GET", ["clips"]) => {
            let mut clips = s.clips_json().await;
            clips.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
            let pending = clips
                .iter()
                .any(|c| c["duration"].is_null() && c["unreadable"] != true);
            if pending {
                s.queue_metadata();
            }
            ok(json!({"clips":clips,"metadata_pending":pending}))
        }
        ("GET", ["images"]) => {
            let files = s.images.read().await.clone();
            let mut images = Vec::new();
            for (slug, p) in files {
                images.push(s.image_json(&slug, &p).await);
            }
            images.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
            ok(json!({"images":images}))
        }
        (_, [kind, slug, rest @ ..]) if *kind == "clips" || *kind == "images" => {
            media_api(s, kind, slug, rest, &method, body, &query, &raw, &headers).await
        }
        ("GET", ["playlists"]) => {
            ok(json!({"playlists":s.playlists.lock().await.list_playlists()}))
        }
        (_, ["playlists", rest @ ..]) => playlist_api(s, rest, &method, &body).await,
        ("GET", ["editor", "project"]) => {
            let project =
                editor::EditorProjectStore::new(data_dir().join("editor_project.json")).load();
            let mut missing = std::collections::BTreeSet::new();
            if let Some(p) = &project
                && let Some(items) = p["items"].as_array()
            {
                for item in items {
                    if let Some(cid) = item["clipId"].as_str()
                        && !s.files.read().await.contains_key(cid)
                    {
                        missing.insert(cid.to_owned());
                    }
                }
            }
            ok(json!({"project":project,"missing":missing}))
        }
        ("POST", ["editor", "project"]) => {
            if !body["tracks"].is_array() || !body["items"].is_array() {
                return Err(bad("expected a project"));
            }
            editor::EditorProjectStore::new(data_dir().join("editor_project.json")).save(&body)?;
            ok(json!({"ok":true}))
        }
        ("POST", ["editor", "export"]) => start_export(s, body).await,
        ("POST", ["editor", "export", id, "cancel"]) => {
            if s.export_committed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "the export has already been saved".into(),
                ));
            }
            let exports = s.export.lock().await;
            if let Some((jid, cancel)) = exports.as_ref()
                && jid == id
            {
                let _ = cancel.send(true);
                return ok(json!({"ok":true}));
            }
            Err(missing())
        }
        ("POST", ["recorder", "install-gsr"]) => {
            if std::env::var_os("FLATPAK_ID").is_none() {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "Only the Flatpak installs gpu-screen-recorder itself.".into(),
                ));
            }
            tokio::spawn(async move {
                s.emit(json!({"type":"gsr_install","phase":"installing","percent":null}));
                let result = run_command(
                    vec![
                        "flatpak-spawn".into(),
                        "--host".into(),
                        "flatpak".into(),
                        "install".into(),
                        "--user".into(),
                        "-y".into(),
                        "flathub".into(),
                        "com.dec05eba.gpu_screen_recorder".into(),
                    ],
                    1800,
                )
                .await;
                s.emit(json!({"type":"gsr_install","phase":if result.is_ok(){"done"}else{"error"},"error":result.err().map(|e|e.to_string()),"percent":null}));
            });
            ok(json!({"ok":true}))
        }
        ("POST", ["overlay", "move"]) => {
            s.overlay
                .lock()
                .await
                .moving(body["on"].as_bool().unwrap_or(false))
                .await?;
            ok(json!({"ok":true}))
        }
        ("POST", ["uninstall"]) => start_uninstall(),
        _ => Err(missing()),
    }
}
fn merge(base: &mut Value, over: &Value) {
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            if let Some(old) = b.get_mut(k) {
                if old.is_object() && v.is_object() && k != "audio_track_volumes" {
                    merge(old, v)
                } else {
                    *old = v.clone();
                }
            }
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn media_api(
    s: Shared,
    kind: &str,
    slug: &str,
    rest: &[&str],
    method: &Method,
    body: Value,
    query: &str,
    raw: &[u8],
    headers: &HeaderMap,
) -> ApiResult {
    let _edit = if *method == Method::DELETE
        || (*method == Method::POST && matches!(rest, ["rename"] | ["trim"] | ["annotate"]))
    {
        Some(s.edits.lock().await)
    } else {
        None
    };
    let image = kind == "images";
    let path = if image {
        s.images.read().await.get(slug).cloned()
    } else {
        s.files.read().await.get(slug).cloned()
    }
    .ok_or_else(missing)?;
    let member = if image {
        format!("img:{slug}")
    } else {
        slug.to_owned()
    };
    let highlights = data_dir().join("highlights").join(format!("{slug}.json"));
    match (method.as_str(), rest) {
        ("POST", ["playback-session"]) if !image => {
            let id = s.playback_sessions.create(path).await?;
            ok(json!({"id":id}))
        }
        ("GET", []) => ok(if image {
            s.image_json(slug, &path).await
        } else {
            s.clip_json(slug, &path, true).await
        }),
        ("DELETE", []) => {
            tokio::fs::remove_file(&path)
                .await
                .map_err(anyhow::Error::from)?;
            s.invalidate_media(&path).await;
            if image {
                s.images.write().await.remove(slug);
            } else {
                s.files.write().await.remove(slug);
            }
            s.identities.write().await.remove(&path);
            {
                let mut names = s.managed_names.write().await;
                names.remove(&path);
                write_json(
                    &data_dir().join("media_names.json"),
                    &serde_json::to_value(&*names).map_err(anyhow::Error::from)?,
                )?;
            }
            let mut views = s.views.lock().await;
            if let Some(map) = views.as_object_mut() {
                map.remove(slug);
            }
            write_json(&data_dir().join("views.json"), &views)?;
            drop(views);
            s.playlists.lock().await.on_clip_deleted(&member)?;
            if !image {
                let mut tokens = s.tokens.lock().await;
                if let Some(o) = tokens.as_object_mut() {
                    o.remove(slug);
                }
                write_json(&data_dir().join("share_tokens.json"), &tokens)?;
                let _ = tokio::fs::remove_file(&highlights).await;
                let _ =
                    tokio::fs::remove_file(data_dir().join("keys").join(format!("{slug}.json")))
                        .await;
            }
            editor::EditorProjectStore::new(data_dir().join("editor_project.json"))
                .on_clip_deleted(slug)?;
            s.rescan().await;
            s.emit(json!({"type":if image{"image_deleted"}else{"clip_deleted"},"slug":slug}));
            ok(json!({"ok":true}))
        }
        ("POST", ["rename"]) => {
            let name = body["name"]
                .as_str()
                .ok_or_else(|| bad("name is required"))?;
            if name.is_empty() || name.contains(['/', '\0']) {
                return Err(bad(
                    "invalid clip title: enter a name without a directory path or NUL",
                ));
            }
            let format = s.media_format(&path).await;
            let cached = s.cached_meta(&path).await;
            let source = path.clone();
            let title = name.to_owned();
            let (target, original_identity) = tokio::task::spawn_blocking(move || {
                let identity = media::FileIdentity::read(&source)?;
                crate::media_names::rename_numbered(&source, &title)
                    .map(|target| (target, identity))
            })
            .await
            .map_err(anyhow::Error::from)?
            .map_err(anyhow::Error::from)?;
            if target != path {
                if let Err(error) = s.remember_name(&path, &target, slug, image, &format).await {
                    let _ = tokio::fs::rename(&target, &path).await;
                    return Err(error.into());
                }
                let owned = target.clone();
                let identity =
                    tokio::task::spawn_blocking(move || media::FileIdentity::read(&owned))
                        .await
                        .map_err(anyhow::Error::from)?
                        .map_err(anyhow::Error::from)?;
                if !image && identity == original_identity {
                    reuse_discord_after_rename(
                        &identity_cache_key(&path, original_identity),
                        &identity_cache_key(&target, identity),
                    )
                    .await;
                }
                // Existing clipboard URIs refer to the old cache filename.
                // Keep those files and link the unchanged result to its new key.
                s.invalidate_media_caches(&path, false).await;
                s.identities.write().await.remove(&path);
                s.identities.write().await.insert(target.clone(), identity);
                if image {
                    s.images.write().await.insert(slug.into(), target.clone());
                } else {
                    s.files.write().await.insert(slug.into(), target.clone());
                }
                if let Some(cached) = cached {
                    s.metadata
                        .lock()
                        .await
                        .insert(target.clone(), (identity.size, identity.mtime_ns, cached));
                }
            }
            let value = if image {
                s.image_json(slug, &target).await
            } else {
                s.clip_json(slug, &target, true).await
            };
            s.emit(json!({"type":if image{"image_saved"}else{"clip_saved"},if image{"image"}else{"clip"}:value}));
            ok(value)
        }
        ("POST", ["reveal"]) | ("POST", ["open"]) => {
            let target = if rest[0] == "reveal" {
                path.parent().unwrap_or(&path)
            } else {
                &path
            };
            Command::new("xdg-open")
                .arg(target)
                .spawn()
                .map_err(anyhow::Error::from)?;
            ok(json!({"ok":true}))
        }
        ("POST", ["view"]) => {
            let mut views = s.views.lock().await;
            let n = views[slug].as_u64().unwrap_or(0) + 1;
            views[slug] = json!(n);
            write_json(&data_dir().join("views.json"), &views)?;
            ok(json!({"ok":true,"views":n}))
        }
        ("GET", ["keys"]) => ok(json!({"keys":crate::keys::load_sidecar(slug)})),
        ("GET", ["highlights"]) => ok(json!({"highlights":read_json(&highlights,json!([]))})),
        (_, ["highlights", ids @ ..]) => {
            let mut list = read_json(&highlights, json!([]))
                .as_array()
                .cloned()
                .unwrap_or_default();
            if ids.is_empty() && method == Method::POST {
                let time = body["time"]
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .ok_or_else(|| bad("invalid highlight time"))?;
                let highlight = json!({"id":chrono::Utc::now().timestamp_micros().to_string(),"time":time,"label":body["label"].as_str().unwrap_or(""),"color":body["color"].as_str().unwrap_or("#0099ff")});
                list.push(highlight.clone());
                list.sort_by(|a, b| {
                    a["time"]
                        .as_f64()
                        .partial_cmp(&b["time"].as_f64())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                write_json(&highlights, &json!(list))?;
                return ok(json!({"ok":true,"highlight":highlight}));
            }
            if ids.len() != 1 {
                return Err(missing());
            }
            let index = list
                .iter()
                .position(|h| h["id"].as_str() == Some(ids[0]))
                .ok_or_else(missing)?;
            if method == Method::DELETE {
                list.remove(index);
            } else if method == Method::PATCH {
                for field in ["label", "color", "time"] {
                    if let Some(v) = body.get(field) {
                        list[index][field] = v.clone();
                    }
                }
            } else {
                return Err(missing());
            }
            write_json(&highlights, &json!(list))?;
            ok(json!({"ok":true}))
        }
        ("POST", ["trim"]) => {
            let start = body["start"]
                .as_f64()
                .ok_or_else(|| bad("start is required"))?;
            let end = body["end"].as_f64().ok_or_else(|| bad("end is required"))?;
            if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
                return Err(bad("invalid trim range"));
            }
            let copy = body["copy"].as_bool().unwrap_or(false);
            let before: media::MediaInfo =
                serde_json::from_value(s.meta(&path).await).map_err(anyhow::Error::from)?;
            if !copy && start <= 0.000_001 && (end - before.duration).abs() < 0.001 {
                return ok(json!({"ok":true,"slug":slug,"copy":false}));
            }
            let wanted = s.playlists.lock().await.metadata_for(slug, None);
            let tags = HashMap::from([("vice_playlists".into(), wanted)]);

            let final_path = if copy {
                trimmed_copy_path(&path)
            } else {
                path.clone()
            };
            let ext = s.media_format(&path).await;
            let tmp = MediaTemp(
                path.with_file_name(format!(".{}.trimming.{ext}", uuid::Uuid::new_v4().simple())),
            );
            let work_dir = cache_dir().join("trims");
            tokio::fs::create_dir_all(&work_dir)
                .await
                .map_err(anyhow::Error::from)?;
            let work = MediaTemp(work_dir.join(format!("{}.{}", uuid::Uuid::new_v4(), ext)));
            let identity = cache_key(&path);
            let info =
                media::trim_range_with_runner(&path, &work.0, start, end, &before, &tags, |args| {
                    run_media_attempt_for(&s, &path, args, 900, "trim_progress", Some(end - start))
                })
                .await?;
            stage_trim(&s, slug, &work.0, &tmp.0).await?;
            if cache_key(&path) != identity {
                return Err(bad("the clip changed while it was being trimmed; retry"));
            }
            let new_slug = if copy {
                uuid::Uuid::new_v4().simple().to_string()
            } else {
                slug.to_owned()
            };
            tokio::fs::rename(&tmp.0, &final_path)
                .await
                .map_err(anyhow::Error::from)?;
            if copy {
                s.remember_name(&final_path, &final_path, &new_slug, false, &ext)
                    .await?;
            }
            s.invalidate_media(&final_path).await;
            if let Some(keys) = crate::keys::load_sidecar(slug) {
                crate::keys::save_sidecar(
                    &new_slug,
                    &crate::keys::cut_intervals(&keys, start, end),
                )?;
            }
            let kept = trim_highlights(&read_json(&highlights, json!([])), start, end);
            write_json(
                &data_dir()
                    .join("highlights")
                    .join(format!("{new_slug}.json")),
                &json!(kept),
            )?;
            if copy {
                let mut store = s.playlists.lock().await;
                let memberships = store.list_playlists();
                for playlist in memberships {
                    if playlist["clip_slugs"]
                        .as_array()
                        .is_some_and(|ss| ss.iter().any(|v| v == slug))
                    {
                        store.add_clip(playlist["id"].as_str().unwrap_or(""), &new_slug)?;
                    }
                }
                s.emit(json!({"type":"playlists_changed","playlists":store.list_playlists()}));
            }
            let owned = final_path.clone();
            let identity = tokio::task::spawn_blocking(move || media::FileIdentity::read(&owned))
                .await
                .map_err(anyhow::Error::from)?
                .map_err(anyhow::Error::from)?;
            s.files
                .write()
                .await
                .insert(new_slug.clone(), final_path.clone());
            s.identities
                .write()
                .await
                .insert(final_path.clone(), identity);
            s.metadata.lock().await.insert(
                final_path.clone(),
                (
                    identity.size,
                    identity.mtime_ns,
                    serde_json::to_value(info).map_err(anyhow::Error::from)?,
                ),
            );
            s.metadata_dirty
                .store(true, std::sync::atomic::Ordering::Relaxed);
            {
                let mut tokens = s.tokens.lock().await;
                if tokens.get(&new_slug).is_none() {
                    tokens[&new_slug] = json!(uuid::Uuid::new_v4().simple().to_string());
                }
                write_json(&data_dir().join("share_tokens.json"), &tokens)?;
            }
            s.flush_metadata(false).await;
            s.emit(
                json!({"type":"clip_saved","clip":s.clip_json(&new_slug,&final_path,true).await}),
            );
            ok(json!({"ok":true,"slug":new_slug,"copy":copy}))
        }
        ("POST", ["frame"]) => {
            let time = body["time"].as_f64().unwrap_or(0.0);
            if !time.is_finite() || time < 0.0 {
                return Err(bad("invalid frame time"));
            }
            let cfg = s.cfg.read().await.clone();
            let dir = runtime::resolve_path(&cfg.output.image_directory);
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(anyhow::Error::from)?;
            let out = unique_path(&dir, "Vice_Shot", "png");
            run_command(
                vec![
                    "ffmpeg".into(),
                    "-loglevel".into(),
                    "error".into(),
                    "-ss".into(),
                    time.to_string(),
                    "-i".into(),
                    path.to_string_lossy().into(),
                    "-frames:v".into(),
                    "1".into(),
                    "-y".into(),
                    out.to_string_lossy().into(),
                ],
                60,
            )
            .await?;
            s.saved(&out, true).await?;
            let mut value = s.image_json(&s.media_id(&out, true).await, &out).await;
            value["ok"] = json!(true);
            value["copied"] = json!(clipboard(&out, true).await.is_ok());
            ok(value)
        }
        ("POST", ["annotate"]) if image => {
            if !raw.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err(bad("expected PNG image"));
            }
            let target = path.clone();
            let tmp = target.with_file_name(format!(".{}.png", uuid::Uuid::new_v4().simple()));
            tokio::fs::write(&tmp, raw)
                .await
                .map_err(anyhow::Error::from)?;
            media::probe_media(&tmp).await?;
            tokio::fs::rename(&tmp, &target)
                .await
                .map_err(anyhow::Error::from)?;
            s.remember_name(&path, &target, slug, true, "png").await?;
            s.saved_locked(&target, true).await?;
            ok(s.image_json(slug, &target).await)
        }
        ("GET", ["discord"]) | ("POST", ["copy-file"]) => {
            let copy = if rest[0] == "discord"
                || body["discord"] == true
                || query.split('&').any(|v| v == "discord=1")
            {
                discord_copy(&s, &path).await?
            } else {
                path
            };
            if method == Method::POST {
                clipboard(&copy, false).await?;
                ok(json!({"ok":true}))
            } else {
                ok(
                    json!({"ok":true,"path":copy,"filename":copy.file_name().unwrap_or_default().to_string_lossy(),"size":revision(&copy).0}),
                )
            }
        }
        ("POST", ["copy"]) if image => {
            clipboard(&path, true).await?;
            ok(json!({"ok":true}))
        }
        ("GET", ["audio", index]) => {
            let index: usize = index.parse().map_err(|_| bad("invalid audio track"))?;
            let meta = s.meta(&path).await;
            if index >= meta["audio_streams"].as_u64().unwrap_or(0) as usize {
                return Err(missing());
            }
            let args = vec![
                "ffmpeg".into(),
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                path.to_string_lossy().into(),
                "-map".into(),
                format!("0:a:{index}"),
                "-vn".into(),
                "-c:a".into(),
                "aac".into(),
                "-f".into(),
                "ipod".into(),
            ];
            let out = cached_media(
                &s,
                &path,
                "audio",
                "m4a",
                &format!("-{index}"),
                args,
                600,
                true,
            )
            .await?;
            let mut request = Request::new(Body::empty());
            *request.headers_mut() = headers.clone();
            file(out, request).await
        }
        _ => Err(missing()),
    }
}
#[cfg(test)]
fn safe_name(name: &str) -> Option<String> {
    crate::media_names::valid_filename(name).then(|| name.to_owned())
}
pub fn unique_path(dir: &FsPath, base: &str, ext: &str) -> PathBuf {
    for n in 1.. {
        let p = dir.join(format!("{base}_{n}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    unreachable!()
}
async fn clipboard(path: &FsPath, image: bool) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let args: Vec<&str> = match (wayland, image) {
        (true, true) => vec!["wl-copy", "--type", "image/png"],
        (true, false) => vec!["wl-copy", "--type", "text/uri-list"],
        (false, true) => vec!["xclip", "-selection", "clipboard", "-t", "image/png", "-i"],
        (false, false) => vec![
            "xclip",
            "-selection",
            "clipboard",
            "-t",
            "text/uri-list",
            "-i",
        ],
    };
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::piped())
        // Clipboard tools fork a background owner which inherits open pipes.
        // Waiting for stderr EOF waits on that owner for the clipboard lifetime.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("Install wl-clipboard (Wayland) or xclip (X11) to use the clipboard")?;
    let payload = if image {
        tokio::fs::read(path).await?
    } else {
        format!(
            "file://{}\r\n",
            percent_encoding::utf8_percent_encode(&path.to_string_lossy(), FILE_URI_ENCODE)
        )
        .into_bytes()
    };
    child
        .stdin
        .take()
        .context("clipboard stdin")?
        .write_all(&payload)
        .await?;
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .context("clipboard tool did not finish")??;
    if !status.success() {
        bail!(
            "clipboard tool exited with {status}; check that your desktop clipboard is available"
        );
    }
    Ok(())
}
async fn reuse_discord_after_rename(old_key: &str, new_key: &str) {
    let directory = cache_dir().join("discord");
    for suffix in ["-budget2", "-budget1", ""] {
        let old = directory.join(format!("{old_key}{suffix}.mp4"));
        let new = directory.join(format!("{new_key}{suffix}.mp4"));
        match tokio::fs::hard_link(&old, &new).await {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
                ) => {}
            Err(error) => tracing::debug!(%error, "could not reuse Discord copy after rename"),
        }
    }
}
async fn discord_copy(s: &Shared, path: &FsPath) -> Result<PathBuf> {
    // A lower-budget retry is just as reusable as the first output. Prefer
    // it before launching another encode if this driver's VBR overshot once.
    let key = indexed_cache_key(s, path).await;
    for suffix in ["-budget2", "-budget1", ""] {
        let cached = cache_dir()
            .join("discord")
            .join(format!("{key}{suffix}.mp4"));
        let bytes = revision(&cached).0;
        if bytes > 0 && bytes <= editor::DISCORD_MAX_BYTES {
            return Ok(cached);
        }
    }
    let meta = s.meta(path).await;
    let size = revision(path).0;
    if size > 0
        && size <= editor::DISCORD_MAX_BYTES
        && path.extension().is_some_and(|e| e == "mp4")
        && meta["vcodec"] == "h264"
    {
        return Ok(path.into());
    }
    let duration = meta["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .context("cannot read clip duration")?;
    let (bitrate, audio) = editor::discord_bitrates(
        duration,
        meta["audio_streams"].as_u64().unwrap_or(1) as usize,
    )?;
    let args = vec![
        "ffmpeg".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        path.to_string_lossy().into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a?".into(),
        "-vf".into(),
        "scale='trunc(min(1280,iw)/2)*2':-2".into(),
        "-c:v".into(),
        "libx264".into(),
        // Discord-compatible 8-bit 4:2:0, including when the recording is
        // 10-bit. Older NVIDIA cards cannot accept 10-bit H.264 encode input.
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-preset".into(),
        "fast".into(),
        "-b:v".into(),
        format!("{bitrate}k"),
        "-maxrate".into(),
        format!("{}k", (bitrate as f64 * 1.45).round() as i64),
        "-bufsize".into(),
        format!("{}k", bitrate * 2),
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        format!("{audio}k"),
        "-movflags".into(),
        "+faststart".into(),
    ];
    let seconds = (duration * 8.0 + 60.0).clamp(120.0, 1800.0) as u64;
    let mut args = args;
    for attempt in 0..3 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-budget{attempt}")
        };
        let out = cached_media(
            s,
            path,
            "discord",
            "mp4",
            &suffix,
            args.clone(),
            seconds,
            true,
        )
        .await?;
        let bytes = revision(&out).0;
        if bytes > 0 && bytes <= editor::DISCORD_MAX_BYTES {
            tracing::info!(path=%out.display(), bytes, "Discord copy ready");
            return Ok(out);
        }
        let _ = tokio::fs::remove_file(&out).await;
        // Some hardware drivers overshoot their requested VBR. Retry using the
        // observed size rather than failing an otherwise valid conversion.
        let ratio = (editor::DISCORD_MAX_BYTES as f64 / bytes.max(1) as f64 * 0.85).min(0.8);
        for flag in ["-b:v", "-maxrate", "-bufsize"] {
            if let Some(index) = args.iter().position(|arg| arg == flag) {
                let rate = args[index + 1].trim_end_matches('k').parse::<f64>()?;
                args[index + 1] = format!("{}k", (rate * ratio).max(100.0) as u64);
            }
        }
    }
    bail!("could not fit the encoded clip under Discord's file limit; trim it shorter")
}
/// Avoid copying a locally stored trim onto the same filesystem. Cross-device
/// libraries (including mounted shares) receive one sequential upload, with
/// progress distinct from encoding so the remaining wait is visible.
async fn stage_trim(state: &Shared, slug: &str, source: &FsPath, target: &FsPath) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let progress = |done: u64, total: u64| {
        state.emit(json!({"type":"trim_progress", "slug":slug,
        "encoder":"", "phase":"uploading", "progress":done as f64 / total.max(1) as f64, "bytes":done, "total":total}))
    };
    match tokio::fs::rename(source, target).await {
        Ok(()) => {
            progress(1, 1);
            return Ok(());
        }
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {}
        Err(error) => return Err(error.into()),
    }
    let mut input = tokio::fs::File::open(source).await?;
    let total = input.metadata().await?.len();
    let mut output = tokio::fs::File::create(target).await?;
    output.set_max_buf_size(4 * 1024 * 1024);
    input.set_max_buf_size(4 * 1024 * 1024);
    let mut buffer = vec![0_u8; 4 * 1024 * 1024];
    let mut done = 0;
    let mut last = Instant::now();
    progress(0, total);
    loop {
        if *state.shutdown.borrow() {
            bail!("trim upload stopped");
        }
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count]).await?;
        done += count as u64;
        if done < total && last.elapsed() >= Duration::from_millis(200) {
            progress(done, total);
            last = Instant::now();
        }
    }
    output.flush().await?;
    if done != total {
        bail!("trim upload ended before the completed file was copied");
    }
    progress(done, total);
    Ok(())
}

fn trimmed_copy_path(path: &FsPath) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    let mut target = path.with_file_name(format!("{stem}-trim.{ext}"));
    let mut n = 2;
    while target.exists() {
        target = path.with_file_name(format!("{stem}-trim-{n}.{ext}"));
        n += 1;
    }
    target
}
fn trim_highlights(highlights: &Value, start: f64, end: f64) -> Vec<Value> {
    highlights
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|value| {
                    let time = value["time"].as_f64()?;
                    if !time.is_finite() || time < start || time > end {
                        return None;
                    }
                    let mut value = value.clone();
                    value["time"] = json!(((time - start) * 1000.0).round() / 1000.0);
                    Some(value)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn playlist_api(s: Shared, parts: &[&str], method: &Method, body: &Value) -> ApiResult {
    let mut store = s.playlists.lock().await;
    let previous = parts.first().and_then(|pid| store.get(pid)).cloned();
    let result = match (method.as_str(), parts) {
        ("POST", []) => {
            if let Some(icon) = body.get("icon")
                && !icon.as_str().is_some_and(crate::playlist_icons::valid_icon)
            {
                return Err(bad("invalid playlist icon"));
            }
            let mut playlist = store.create_custom(
                body["name"].as_str().unwrap_or(""),
                body["emoji"].as_str().unwrap_or(""),
                body["color1"].as_str().unwrap_or(""),
                body["color2"].as_str().unwrap_or(""),
            )?;
            if body.get("icon").is_some() {
                let id = playlist["id"].as_str().context("playlist id")?;
                playlist = store.update_playlist(id, &json!({"icon":body["icon"]}))?;
            }
            json!({"ok":true,"playlist":playlist})
        }
        ("PATCH", [pid]) => json!({"ok":true,"playlist":store.update_playlist(pid,body)?}),
        ("DELETE", [pid]) => {
            store.delete(pid)?;
            json!({"ok":true})
        }
        ("POST", [pid, "clips"]) => {
            let slug = body["slug"]
                .as_str()
                .ok_or_else(|| bad("slug is required"))?;
            let exists = if let Some(stem) = slug.strip_prefix("img:") {
                s.images.read().await.contains_key(stem)
            } else {
                s.files.read().await.contains_key(slug)
            };
            if !exists {
                return Err(missing());
            }
            json!({"ok":true,"playlist":store.add_clip(pid,slug)?})
        }
        ("DELETE", [pid, "clips", slug]) => {
            store.remove_clip(pid, slug)?;
            json!({"ok":true})
        }
        _ => return Err(missing()),
    };
    let artwork_only = method == Method::PATCH
        && previous.as_ref().is_some_and(|before| {
            ["name", "emoji", "color1", "color2"]
                .iter()
                .all(|key| before[key] == result["playlist"][key])
        });
    let playlists = store.list_playlists();
    drop(store);
    s.emit(json!({"type":"playlists_changed","playlists":playlists}));
    // Choosing artwork is a local presentation change, so avoid probing or
    // remuxing the library merely to update an icon.
    let affected: std::collections::HashSet<String> = previous
        .iter()
        .chain(result.get("playlist"))
        .flat_map(|playlist| {
            playlist["clip_slugs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|slug| slug.as_str().map(str::to_owned))
        })
        .collect();
    if artwork_only || affected.is_empty() {
        return ok(result);
    }
    // Update only members of the changed playlist. Creating an empty playlist
    // or choosing artwork must never scan/remux every video on a remote share.
    tokio::spawn(async move {
        let files: Vec<_> = s
            .files
            .read()
            .await
            .iter()
            .filter(|(slug, _)| affected.contains(*slug))
            .map(|(slug, path)| (slug.clone(), path.clone()))
            .collect();
        for (slug, path) in files {
            // Portable tag remuxes read and write the full recording. Wait
            // for playback's share reads to finish instead of competing for
            // the same Wi-Fi link after a user edits a tag in the player.
            while s.video_busy() && !*s.shutdown.borrow() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if *s.shutdown.borrow() {
                break;
            }
            let tag = s.playlists.lock().await.metadata_for(&slug, None);
            let existing = media::probe_container_tags(&path).await.unwrap_or_default();
            let old = existing
                .get("vice_playlists")
                .and_then(|v| serde_json::from_str::<Value>(v).ok());
            let new = serde_json::from_str::<Value>(&tag).ok();
            // Revisions differ on each call; compare user-visible fields only.
            let mut old = old.unwrap_or(json!({"playlists":[]}));
            let mut new = new.unwrap_or(json!({"playlists":[]}));
            for value in [&mut old, &mut new] {
                if let Some(ps) = value["playlists"].as_array_mut() {
                    for p in ps {
                        if let Some(o) = p.as_object_mut() {
                            o.remove("rev");
                        }
                    }
                }
            }
            if old != new {
                let tags = HashMap::from([("vice_playlists".into(), tag)]);
                if let Err(e) = media::write_metadata_tags(&path, &tags, 600.0, Some(1.0)).await {
                    tracing::warn!(%e,"playlist tag write failed");
                }
            }
        }
    });
    ok(result)
}
/// Drain stderr for the entire process lifetime, retaining only a bounded tail.
/// Stopping the reader at a byte limit can deadlock FFmpeg on a full pipe.
async fn run_export_process(
    state: &Shared,
    args: &[String],
    duration: f64,
    job_id: &str,
    cancelled: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut failure = None;
    for candidate in crate::encoding::command_candidates(args).await {
        if *cancelled.borrow() || *state.shutdown.borrow() {
            bail!("export canceled");
        }
        match run_export_attempt(state, &candidate, duration, job_id, cancelled).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if *cancelled.borrow() || *state.shutdown.borrow() {
                    return Err(error);
                }
                tracing::warn!(%error, "export encoder failed; trying the next available encoder");
                state.emit(json!({"type":"export_progress","job_id":job_id,"progress":0.0}));
                failure = Some(error);
            }
        }
    }
    Err(failure.unwrap_or_else(|| anyhow::anyhow!("no export encoder available")))
}

async fn run_export_attempt(
    state: &Shared,
    args: &[String],
    duration: f64,
    job_id: &str,
    cancelled: &mut watch::Receiver<bool>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    if *cancelled.borrow() {
        bail!("export canceled");
    }
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut lines = BufReader::new(child.stdout.take().context("export stdout")?).lines();
    let mut stderr = child.stderr.take().context("export stderr")?;
    let errors = tokio::spawn(async move {
        let mut tail = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    tail.extend_from_slice(&chunk[..count]);
                    if tail.len() > 4096 {
                        tail.drain(..tail.len() - 4096);
                    }
                }
            }
        }
        tail
    });
    let mut shutdown = state.shutdown.subscribe();
    let result:Result<_>=async {
        loop{tokio::select!{
            _=cancelled.changed()=>bail!("export canceled"),
            _=shutdown.changed()=>bail!("export canceled during shutdown"),
            line=lines.next_line()=>match line?{Some(line)=>{if let Some(progress)=editor::parse_progress(&line,duration){state.emit(json!({"type":"export_progress","job_id":job_id,"progress":progress}));}},None=>break}
        }}
        tokio::select!{status=child.wait()=>Ok(status?),_=cancelled.changed()=>bail!("export canceled"),_=shutdown.changed()=>bail!("export canceled during shutdown")}
    }.await;
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    drop(lines);
    let stderr = errors.await.unwrap_or_default();
    let status = result?;
    if !status.success() {
        let reason = String::from_utf8_lossy(&stderr);
        if reason.trim().is_empty() {
            bail!("ffmpeg exited with {status}");
        }
        bail!("{reason}");
    }
    Ok(())
}
pub async fn wait_for_export(state: &Shared) {
    loop {
        if state.export.lock().await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
async fn start_export(s: Shared, body: Value) -> ApiResult {
    let mut exports = s.export.lock().await;
    if exports.is_some() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "an export is already running".into(),
        ));
    }
    let raw = body
        .get("project")
        .filter(|v| v.is_object())
        .ok_or_else(|| bad("expected a project"))?;
    let mut sources = HashMap::new();
    if let Some(items) = raw["items"].as_array() {
        for item in items {
            if let Some(id) = item["clipId"].as_str() {
                if sources.contains_key(id) {
                    continue;
                }
                if let Some(path) = s.files.read().await.get(id).cloned() {
                    let m = media::probe_media(&path).await?;
                    sources.insert(
                        id.to_owned(),
                        editor::Source {
                            path,
                            duration: m.duration,
                            width: m.width,
                            height: m.height,
                            has_audio: m.audio_streams > 0,
                            audio_streams: m.audio_streams,
                        },
                    );
                }
            }
        }
    }
    let (project, errors) = editor::validate_project(raw, &sources);
    if !errors.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"ok":false,"errors":errors})),
        )
            .into_response());
    }
    let cfg = s.cfg.read().await.clone();
    let library = runtime::resolve_path(&cfg.output.directory);
    let dir = match body["location"].as_str().unwrap_or("library") {
        "videos" => runtime::actual_home_dir().join("Videos"),
        "custom" => {
            let p = body["path"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| bad("a folder is required"))?;
            runtime::resolve_path(p)
        }
        _ => library.clone(),
    };
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(anyhow::Error::from)?;
    let name = if let Some(name) = body["filename"].as_str().filter(|s| !s.is_empty()) {
        editor::sanitize_export_name(name).ok_or_else(|| bad("invalid export name"))?
    } else {
        editor::default_export_name(&dir)
    };
    let final_path = dir.join(name);
    if final_path.exists() {
        return Err(bad("a file with that name already exists"));
    }
    let jid = format!("exp-{}", uuid::Uuid::new_v4().simple());
    let work = cache_dir().join("exports").join(&jid);
    tokio::fs::create_dir_all(&work)
        .await
        .map_err(anyhow::Error::from)?;
    let work_guard = ExportWork(work.clone());
    let temp = dir.join(format!(".{}.export.mp4", uuid::Uuid::new_v4().simple()));
    let fonts = editor::font_dir();
    for (file, text) in editor::text_file_contents(&project, &work) {
        tokio::fs::write(file, text)
            .await
            .map_err(anyhow::Error::from)?;
    }
    #[cfg(feature = "desktop")]
    let burn = if body["keys"] == true {
        Some(crate::key_overlay::build_burn(
            &project,
            &sources,
            &work.join("keys"),
            body["accent"].as_str().unwrap_or("#0099ff"),
        )?)
    } else {
        None
    };
    #[cfg(not(feature = "desktop"))]
    let burn: Option<editor::KeyBurn> = if body["keys"] == true {
        return Err(bad("Burning keyboard overlays requires a desktop build"));
    } else {
        None
    };
    let args = editor::build_export_cmd_with_keys(
        &project,
        &sources,
        &temp,
        body["accent"].as_str().unwrap_or("#0099ff"),
        &fonts,
        &work,
        body["discord_optimized"].as_bool().unwrap_or(false),
        burn.as_ref(),
    )?;
    let (cancel, mut cancelled) = watch::channel(false);
    s.export_committed
        .store(false, std::sync::atomic::Ordering::Release);
    *exports = Some((jid.clone(), cancel));
    drop(exports);
    let output = final_path.clone();
    let job_id = jid.clone();
    let duration = editor::project_extent(&project);
    tokio::spawn(async move {
        let _work_guard = work_guard;
        let _temp_guard = MediaTemp(temp.clone());
        let result: Result<()> = async {
            run_export_process(&s, &args, duration, &job_id, &mut cancelled).await?;
            media::probe_media(&temp).await?;
            if body["discord_optimized"] == true && revision(&temp).0 > editor::DISCORD_MAX_BYTES {
                bail!("export exceeds Discord file limit");
            }
            if *cancelled.borrow() {
                bail!("export canceled");
            }
            // A new file appearing during a long render must never be replaced.
            tokio::fs::hard_link(&temp, &final_path)
                .await
                .context("cannot publish export without replacing an existing file")?;
            s.export_committed
                .store(true, std::sync::atomic::Ordering::Release);
            let target = if dir != library && body["add_to_library"] == true {
                tokio::fs::create_dir_all(&library).await?;
                let copy = unique_path(
                    &library,
                    final_path
                        .file_stem()
                        .unwrap()
                        .to_str()
                        .unwrap_or("Vice_Export"),
                    "mp4",
                );
                let library_temp = MediaTemp(
                    library.join(format!(".{}.export.mp4", uuid::Uuid::new_v4().simple())),
                );
                tokio::fs::copy(&final_path, &library_temp.0).await?;
                tokio::fs::hard_link(&library_temp.0, &copy).await?;
                Some(copy)
            } else if dir == library {
                Some(final_path.clone())
            } else {
                None
            };
            let mut clip = Value::Null;
            if let Some(path) = target {
                let _edit = s.edits.lock().await;
                let id = uuid::Uuid::new_v4().simple().to_string();
                s.remember_name(&path, &path, &id, false, "mp4").await?;
                s.saved_locked(&path, false).await?;
                clip = s.clip_json(&id, &path, true).await;
            }
            s.emit(json!({"type":"export_done","job_id":job_id,"path":final_path,"clip":clip}));
            Ok(())
        }
        .await;
        if let Err(e) = result {
            s.emit(json!({"type":"export_error","job_id":job_id,"error":e.to_string(),"canceled":*cancelled.borrow()}));
        }
        let _ = tokio::fs::remove_file(&temp).await;
        *s.export.lock().await = None;
    });
    ok(json!({"ok":true,"job_id":jid,"path":output}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn status_stays_responsive_during_clip_finalization() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.output.directory = dir.path().join("clips").to_string_lossy().into();
        cfg.output.image_directory = dir.path().join("images").to_string_lossy().into();
        let state = AppState::new(cfg);
        let initial = state.status().await;
        let _recorder = state.recorder.lock().await;
        let status = tokio::time::timeout(Duration::from_millis(100), state.status())
            .await
            .unwrap();
        assert_eq!(status["recording"], initial["recording"]);
        assert_eq!(status["clips"], 0);
    }
    #[tokio::test]
    async fn rescan_never_publishes_a_recording_still_being_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.output.directory = dir.path().to_string_lossy().into();
        cfg.output.image_directory = dir.path().join("images").to_string_lossy().into();
        let state = AppState::new(cfg);
        std::fs::write(dir.path().join("unfinished.mp4"), b"partial recording").unwrap();
        state
            .clip_writing
            .store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(state.rescan().await, 0);
        assert!(state.files.read().await.is_empty());
    }
    #[tokio::test]
    async fn staging_a_local_trim_moves_bytes_without_another_copy() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("work.mp4");
        let target = dir.path().join("staged.mp4");
        std::fs::write(&source, b"finished trim").unwrap();
        let inode = source.metadata().unwrap().ino();
        let mut cfg = Config::default();
        cfg.output.directory = dir.path().join("clips").to_string_lossy().into();
        cfg.output.image_directory = dir.path().join("images").to_string_lossy().into();
        let state = AppState::new(cfg);
        stage_trim(&state, "clip", &source, &target).await.unwrap();
        assert!(!source.exists());
        assert_eq!(target.metadata().unwrap().ino(), inode);
        assert_eq!(std::fs::read(&target).unwrap(), b"finished trim");
    }
    #[tokio::test]
    async fn staging_a_trim_across_filesystems_reports_complete_upload() {
        use std::os::unix::fs::MetadataExt;
        let Ok(remote) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        let local = tempfile::tempdir().unwrap();
        if local.path().metadata().unwrap().dev() == remote.path().metadata().unwrap().dev() {
            return;
        }
        let source = local.path().join("work.mp4");
        let target = remote.path().join("staged.mp4");
        let data: Vec<u8> = (0..4 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &data).unwrap();
        let mut cfg = Config::default();
        cfg.output.directory = local.path().join("clips").to_string_lossy().into();
        cfg.output.image_directory = local.path().join("images").to_string_lossy().into();
        let state = AppState::new(cfg);
        let mut events = state.events.subscribe();
        stage_trim(&state, "clip", &source, &target).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), data);
        assert!(source.exists());
        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            assert_eq!(event["phase"], "uploading");
            finished |= event["progress"] == 1.0 && event["bytes"] == data.len() as u64;
        }
        assert!(finished);
    }
    #[test]
    fn trimmed_highlights_rebase_and_keep_user_fields() {
        assert_eq!(
            trim_highlights(
                &json!([{"time":1.0,"label":"Before"},{"time":3.5,"label":"Play","color":"#ff0000"},{"time":7.0,"label":"After"}]),
                2.0,
                6.0
            ),
            vec![json!({"time":1.5,"label":"Play","color":"#ff0000"})]
        );
    }
    #[tokio::test]
    async fn export_drains_large_stderr_without_deadlocking() {
        let state = AppState::new(Config::default());
        let (_sender, mut cancelled) = watch::channel(false);
        let args = vec![
            "sh".into(),
            "-c".into(),
            "head -c 200000 /dev/zero >&2; printf 'out_time_us=1000000\\nprogress=end\\n'".into(),
        ];
        tokio::time::timeout(
            Duration::from_secs(5),
            run_export_process(&state, &args, 1.0, "test", &mut cancelled),
        )
        .await
        .unwrap()
        .unwrap();
    }
    #[tokio::test]
    async fn export_cancels_after_progress_pipe_closes() {
        let state = AppState::new(Config::default());
        let (sender, mut cancelled) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = sender.send(true);
        });
        let args = vec!["sh".into(), "-c".into(), "exec 1>&-; exec sleep 30".into()];
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            run_export_process(&state, &args, 1.0, "test", &mut cancelled),
        )
        .await
        .unwrap();
        assert!(result.unwrap_err().to_string().contains("canceled"));
    }
    #[tokio::test]
    async fn recovery_preserves_playlists_created_during_startup() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(Config::default());
        state.files.write().await.clear();
        state.images.write().await.clear();
        state.cfg.write().await.output.auto_playlist_by_game = false;
        let mut store = state.playlists.lock().await;
        *store = PlaylistStore::new(dir.path().join("playlists.json"));
        let playlist = store.create_custom("New playlist", "", "", "").unwrap();
        drop(store);
        state.recover_library().await.unwrap();
        assert!(
            state
                .playlists
                .lock()
                .await
                .list_playlists()
                .iter()
                .any(|p| p["id"] == playlist["id"])
        );
    }
    #[test]
    fn malformed_state_shapes_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"[]").unwrap();
        assert_eq!(read_json(&path, json!({})), json!({}));
        std::fs::write(&path, b"false").unwrap();
        assert_eq!(read_json(&path, json!([])), json!([]));
        assert_eq!(
            percent_encoding::utf8_percent_encode("/clips/a #1%.mp4", FILE_URI_ENCODE).to_string(),
            "/clips/a%20%231%25.mp4"
        );
    }
    #[tokio::test]
    async fn tunnel_changes_refresh_existing_share_links() {
        let state = AppState::new(Config::default());
        let mut events = state.events.subscribe();
        state
            .files
            .write()
            .await
            .insert("test".into(), PathBuf::from("test.mp4"));
        state.tokens.lock().await["test"] = json!("random-token");
        *state.tunnel_url.write().await = Some("https://sample.trycloudflare.com".into());
        state.broadcast_share_links().await;
        let event = events.recv().await.unwrap();
        assert_eq!(
            event["links"]["test"],
            "https://sample.trycloudflare.com/c/random-token"
        );
        assert_eq!(event["share_is_public"], true);
        *state.tunnel_url.write().await = None;
        state.broadcast_share_links().await;
        assert_eq!(events.recv().await.unwrap()["share_is_public"], false);
    }
    #[test]
    fn path_validation() {
        assert!(safe_name("../../passwd").is_none());
        assert_eq!(safe_name(".hidden"), Some(".hidden".into()));
        assert_eq!(safe_name("clip #1 + 2"), Some("clip #1 + 2".into()));
    }
    #[test]
    fn config_maps_replace() {
        let mut a = json!({"recording":{"fps":60,"audio_track_volumes":{"old":1}}});
        merge(
            &mut a,
            &json!({"recording":{"audio_track_volumes":{"new":0.5}},"unknown":3}),
        );
        assert_eq!(
            a,
            json!({"recording":{"fps":60,"audio_track_volumes":{"new":0.5}}})
        );
    }
    #[test]
    fn no_symlinks_or_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("clip.mp4"), b"test").unwrap();
        std::fs::write(dir.path().join("bad.trimming.mp4"), b"test").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("leak.mp4")).unwrap();
        assert_eq!(scan(dir.path(), &["mp4"]).len(), 1);
    }
    #[tokio::test]
    async fn public_cannot_control_daemon() {
        let s = AppState::new(Config::default());
        let app = public_router(s);
        for path in ["/api/config", "/api/quit", "/ws", "/", "/v/clip"] {
            let res = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND);
        }
    }
    #[tokio::test]
    async fn reject_foreign_origin() {
        let s = AppState::new(Config::default());
        let app = local_router(s);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header("host", "127.0.0.1:8765")
                    .header("origin", "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
}
