"""
Vice share server, HTTP server that powers:
  • A local control UI/server  (/ → UI, /api/*, /ws, media)
  • A public share-only server  (/c/{token}, /v/{token}, /t/{token})

WebSocket event types (server → client):
  {"type": "clip_saved",   "clip":  <clip_json>}
  {"type": "clip_deleted", "slug":  "..."}
  {"type": "status",       "recording": bool, "backend": "..."}
  {"type": "tunnel_url",   "url":   "https://..."}
  {"type": "tunnel_error", "error": "..."}
  {"type": "playlists_changed", "playlists": [<playlist_json>]}
  {"type": "library_recovered", "added": 12, "clips": 1012}
  {"type": "library_changed",   "added": 0,  "clips": 1000}
  {"type": "export_progress", "job_id": "...", "progress": 0.42}
  {"type": "export_done",  "job_id": "...", "path": "...", "clip": <clip_json>|null}
  {"type": "export_error", "job_id": "...", "error": "...", "canceled": bool}
  {"type": "editor_project_changed"}
"""

from __future__ import annotations

import asyncio
import copy
import fnmatch
import glob
import html
import json
import logging
import os
import re
import secrets
import shutil
import socket
import stat
import subprocess
import tempfile
import time
from dataclasses import asdict
from datetime import datetime
from pathlib import Path
from typing import Callable, Coroutine, Optional
from urllib.parse import quote

from importlib.resources import files as _pkg_files

from aiohttp import WSMsgType, web

from . import __version__
from . import keys as keystore
from .editor import (DISCORD_MAX_BYTES, DiscordTooLarge, EditorProjectStore,
                     ExportBusy, ExportManager, KeyBurn, Source, build_export_cmd,
                     canvas_for,
                     default_export_name, discord_bitrates, project_extent,
                     sanitize_export_name, text_file_contents,
                     validate_project)
from .media import (TEMP_FILE_GLOBS, communicate_with_timeout,
                    probe_container_tags, probe_media, probe_media_detailed,
                    write_metadata_tags)
from .playlists import (IMAGE_PREFIX, PLAYLIST_TAG, PlaylistStore,
                        build_tag_index, decode_playlist_metadata, image_slug)
from .recorder import (IMAGE_EXTS, KEEP_ALL_STREAMS, _available_encoders,
                       _is_nvidia, filename_tag, list_display_options,
                       list_gsr_audio_sources, next_image_path,
                       slugify_clip_name)
from .runtime import actual_home_dir, load_user_systemd_env, resolve_path

log = logging.getLogger("vice.share")

# Config maps whose keys are user data rather than field names.
FREE_MAP_KEYS = frozenset({"audio_track_volumes"})
UI_VERSION_TOKEN = "__VICE_VERSION__"

# Playlist membership lives inside each video, which means a remux. On a
# network clip folder that is a lot of I/O, so the background backfill is
# deliberately polite: one clip at a time, a pause between files, and nothing
# at all while somebody is using the app or watching a clip.
PLAYLIST_TAG_STARTUP_DELAY = 15.0
PLAYLIST_TAG_WRITE_PAUSE = 1.0
PLAYLIST_TAG_IDLE_SECONDS = 10.0
PLAYLIST_TAG_MEDIA_IDLE_SECONDS = 90.0

# A thumbnail frame is read from the clip's own file. After boot the grid can
# ask for dozens of them at once, which uses up the network share's read
# throughput and leaves a video opening at the same time starved (the player
# shows the first frame and duration, then waits). Thumbnail work therefore
# stops as soon as playback starts and resumes when the player is gone.
# A thumbnail request must never sit on one of the browser's six connections
# per host while another thumbnail runs: the click that follows would wait
# behind it. Wait only long enough to pick up a slot that is already free.
THUMB_SLOT_WAIT_SECONDS = 0.05


def _is_transient_clip_name(name: str) -> bool:
    """Whether a filename is an edit/remux temp rather than a real clip.

    pathlib globs include dotfiles, so the hidden ``.vice-meta-*`` files used
    while embedding playlist metadata used to appear as clips while they were
    being written. They are never clips, and neither are the visible
    ``*.trimming.mp4`` / ``*.fix.mp4`` leftovers from an interrupted edit.
    """
    if name.startswith("."):
        return True
    return any(fnmatch.fnmatch(name, pattern) for pattern in TEMP_FILE_GLOBS)


def _resolve_ui_index() -> Path | None:
    """Resolve the web UI index path across installed and source checkouts."""
    candidates: list[Path] = []

    # Preferred path for packaged installs.
    try:
        ui = _pkg_files("vice") / "ui" / "index.html"
        candidates.append(Path(str(ui)))
    except Exception as exc:
        log.debug("importlib.resources lookup for UI index failed: %s", exc)

    # Fallback for source checkouts / direct execution.
    candidates.append(Path(__file__).resolve().parent / "ui" / "index.html")

    for cand in candidates:
        try:
            if cand.exists() and cand.is_file():
                return cand
        except OSError:
            continue
    return None


# UI assets shipped inside the package (bundled, no network needed).
_UI_ASSET_KINDS = {"fonts", "styles", "scripts"}
_UI_CONTENT_TYPES = {
    "fonts":   "font/woff2",
    "styles":  "text/css; charset=utf-8",
    "scripts": "application/javascript; charset=utf-8",
}


_UI_ASSET_REV: Optional[str] = None


def _ui_asset_rev() -> str:
    """Cache key for the bundled UI assets: the version plus a fingerprint of
    the shipped files. Assets are served immutable for a year, so without the
    fingerprint a rebuild that keeps the same version (every test build during
    development) would keep serving the previous build's scripts."""
    global _UI_ASSET_REV
    if _UI_ASSET_REV is not None:
        return _UI_ASSET_REV
    latest = 0
    for kind in _UI_ASSET_KINDS:
        base = _resolve_ui_asset_dir(kind)
        if not base:
            continue
        try:
            for asset in base.iterdir():
                if asset.is_file():
                    st = asset.stat()
                    latest = max(latest, st.st_mtime_ns ^ st.st_size)
        except OSError:
            continue
    _UI_ASSET_REV = f"{__version__}-{latest & 0xffffffff:08x}" if latest else __version__
    return _UI_ASSET_REV


def _resolve_ui_asset_dir(kind: str) -> Path | None:
    if kind not in _UI_ASSET_KINDS:
        return None
    candidates: list[Path] = []
    try:
        candidates.append(Path(str(_pkg_files("vice") / "ui" / kind)))
    except Exception as exc:
        log.debug("importlib.resources lookup for UI dir %s failed: %s", kind, exc)
    candidates.append(Path(__file__).resolve().parent / "ui" / kind)
    for cand in candidates:
        try:
            if cand.is_dir():
                return cand
        except OSError:
            continue
    return None


def _resolve_ui_asset(kind: str, name: str) -> Path | None:
    """Resolve a bundled UI asset (only allows known kinds + simple filenames)."""
    if kind not in _UI_ASSET_KINDS:
        return None
    if not name or "/" in name or "\\" in name or name.startswith("."):
        return None
    candidates: list[Path] = []
    try:
        f = _pkg_files("vice") / "ui" / kind / name
        candidates.append(Path(str(f)))
    except Exception as exc:
        log.debug("importlib.resources lookup for UI asset %s/%s failed: %s", kind, name, exc)
    candidates.append(Path(__file__).resolve().parent / "ui" / kind / name)
    for cand in candidates:
        try:
            if cand.exists() and cand.is_file():
                return cand
        except OSError:
            continue
    return None

# Thumbnails go in the cache dir, separate from the clip files.
THUMB_DIR      = actual_home_dir() / ".cache" / "vice" / "thumbs"
# Probe results keyed by file identity. Startup used to run ffprobe for every
# clip on every launch, which is what made a thousand-clip library take minutes
# to appear.
MEDIA_CACHE_PATH = actual_home_dir() / ".cache" / "vice" / "media.json"
# H.264 preview copies of clips the native WebEngine can't decode (H.265).
PROXY_DIR      = actual_home_dir() / ".cache" / "vice" / "proxies"
# Discord-sized copies live in a hidden folder beside the clip they came from,
# not in ~/.cache: the drag hands another app a path, and a sandboxed one
# (Discord ships as a Flatpak) can only read the folders it was granted. The
# clip's own directory is the one place the user has already opened up, so a
# copy there is readable wherever the original is.
DISCORD_SUBDIR = ".discord"
# Scratch space for editor export jobs (drawtext sidecar files).
EXPORT_WORK_DIR = actual_home_dir() / ".cache" / "vice" / "exports"
HIGHLIGHTS_DIR = actual_home_dir() / ".local" / "share" / "vice" / "highlights"


def _load_highlights(slug: str) -> list:
    f = HIGHLIGHTS_DIR / f"{slug}.json"
    if not f.exists():
        return []
    try:
        return json.loads(f.read_text())
    except Exception as exc:
        log.warning("Highlights file %s is unreadable: %s", f.name, exc)
        return []


def _save_highlights(slug: str, highlights: list) -> None:
    HIGHLIGHTS_DIR.mkdir(parents=True, exist_ok=True)
    (HIGHLIGHTS_DIR / f"{slug}.json").write_text(json.dumps(highlights))


# In-app view counts per slug. Like playlist membership, the counters are
# migrated on rename and dropped on delete so a reused clip number never
# inherits another clip's history.
VIEWS_PATH = actual_home_dir() / ".local" / "share" / "vice" / "views.json"


def _load_views() -> dict[str, int]:
    if not VIEWS_PATH.exists():
        return {}
    try:
        return {str(k): int(v) for k, v in json.loads(VIEWS_PATH.read_text()).items()}
    except Exception as exc:
        log.warning("Views file %s is unreadable: %s", VIEWS_PATH, exc)
        return {}


def _save_views(views: dict[str, int]) -> None:
    # Write-and-rename so a crash mid-write can't truncate the whole file and
    # lose every count (a single JSON file, unlike per-clip highlights).
    VIEWS_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = VIEWS_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(views))
    tmp.replace(VIEWS_PATH)


# Public links name a clip by a random token rather than its filename. Clip
# names are sequential, so anyone holding one link could reach every other
# clip by editing the number (#222). A token is all it takes to fetch a clip,
# so the file is kept readable by its owner only.
SHARE_TOKENS_PATH = actual_home_dir() / ".local" / "share" / "vice" / "share_tokens.json"


def _load_share_tokens() -> dict[str, str]:
    if not SHARE_TOKENS_PATH.exists():
        return {}
    try:
        data = json.loads(SHARE_TOKENS_PATH.read_text())
        return {str(k): v for k, v in data.items() if isinstance(v, str) and v}
    except Exception as exc:
        # Failing closed: every clip gets a fresh token, so links shared
        # before this point stop working rather than resolving to anything.
        log.warning("Share link file %s is unreadable, issuing new links: %s",
                    SHARE_TOKENS_PATH, exc)
        return {}


def _save_share_tokens(tokens: dict[str, str]) -> None:
    SHARE_TOKENS_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = SHARE_TOKENS_PATH.with_suffix(".json.tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    # A leftover temp file from a crash keeps its old mode through O_CREAT.
    os.fchmod(fd, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write(json.dumps(tokens))
    tmp.replace(SHARE_TOKENS_PATH)


# Small bag of UI state that must outlive the web view. The native window's
# localStorage does not reliably survive restarts on every QtWebEngine build,
# which made the first-run tutorial reappear every launch.
APP_STATE_PATH = actual_home_dir() / ".local" / "share" / "vice" / "ui_state.json"


def _load_app_state() -> dict:
    if not APP_STATE_PATH.exists():
        return {}
    try:
        data = json.loads(APP_STATE_PATH.read_text())
        return data if isinstance(data, dict) else {}
    except Exception as exc:
        log.warning("UI state file %s is unreadable: %s", APP_STATE_PATH, exc)
        return {}


def _save_app_state(state: dict) -> None:
    APP_STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
    APP_STATE_PATH.write_text(json.dumps(state))


def _thumb_path(path: Path, identity: tuple[int, int] | None = None) -> Path:
    """Return cache path unique to this clip file content/version.

    *identity* is a known ``(size, mtime_ns)`` from the library index. Pass it
    on request paths: ``stat()`` on a network clip folder can block the event
    loop, and the scan has already read the same values.
    """
    if identity is not None:
        size, mtime_ns = identity
        return THUMB_DIR / f"{path.stem}_{size}_{mtime_ns}.jpg"
    try:
        st = path.stat()
        key = f"{path.stem}_{st.st_size}_{st.st_mtime_ns}"
    except OSError:
        key = path.stem
    return THUMB_DIR / f"{key}.jpg"


class _MediaCache:
    """Persistent ffprobe results, keyed by path and file identity.

    A clip's metadata changes only when the file does, so the size and
    nanosecond mtime are enough to reuse a probe. Entries also carry the
    container tags read from the file, letting startup recover playlist
    membership without opening every video again.
    """

    def __init__(self, path: Path) -> None:
        self.path = path
        self._entries: dict[str, dict] = {}
        self._loaded = False
        self._dirty = False
        self._last_save = 0.0

    @staticmethod
    def _key(path: Path) -> str:
        # abspath, not resolve(): resolving walks every path component with
        # lstat, and this runs once per clip on every gallery listing. The
        # output directory is already an absolute mount path, so the key is
        # the same without touching the network.
        return os.path.abspath(os.fspath(path))

    @staticmethod
    def _identity(path: Path) -> Optional[list[int]]:
        try:
            st = path.stat()
            return [st.st_size, st.st_mtime_ns]
        except OSError:
            return None

    def _load(self) -> None:
        if self._loaded:
            return
        self._loaded = True
        if not self.path.exists():
            return
        try:
            data = json.loads(self.path.read_text())
            entries = data.get("files", {})
            if isinstance(entries, dict):
                self._entries = {
                    str(k): v for k, v in entries.items() if isinstance(v, dict)
                }
        except Exception as exc:
            log.warning("Media cache %s is unreadable: %s", self.path, exc)

    def _entry(self, path: Path,
               identity: Optional[list[int]] = None) -> Optional[dict]:
        self._load()
        entry = self._entries.get(self._key(path))
        if not isinstance(entry, dict):
            return None
        if identity is None:
            identity = self._identity(path)
        if identity is None or entry.get("identity") != identity:
            self._entries.pop(self._key(path), None)
            self._dirty = True
            return None
        return entry

    def get_meta(self, path: Path,
                 identity: Optional[list[int]] = None) -> Optional[dict]:
        entry = self._entry(path, identity)
        meta = entry.get("meta") if entry else None
        return dict(meta) if isinstance(meta, dict) else None

    def get_tags(self, path: Path,
                 identity: Optional[list[int]] = None) -> Optional[dict[str, str]]:
        entry = self._entry(path, identity)
        tags = entry.get("tags") if entry else None
        if not isinstance(tags, dict):
            return None
        return {str(k).lower(): str(v) for k, v in tags.items()}

    def forget_temp_files(self) -> None:
        """Drop cached probes for temp files indexed by older Vice versions."""
        self._load()
        stale = [
            key for key in self._entries
            if _is_transient_clip_name(Path(key).name)
        ]
        for key in stale:
            self._entries.pop(key, None)
        if stale:
            self._dirty = True

    def _put(self, path: Path, **fields) -> None:
        self._load()
        identity = self._identity(path)
        if identity is None:
            return
        key = self._key(path)
        entry = self._entries.get(key)
        if not isinstance(entry, dict):
            entry = {}
        entry["identity"] = identity
        entry.update(fields)
        self._entries[key] = entry
        self._dirty = True

    def put_meta(self, path: Path, meta: dict) -> None:
        self._put(path, meta=meta)

    def put_tags(self, path: Path, tags: dict[str, str]) -> None:
        self._put(path, tags=tags)

    def forget(self, path: Path) -> None:
        self._load()
        if self._entries.pop(self._key(path), None) is not None:
            self._dirty = True

    def save(self, force: bool = False) -> None:
        if not self._dirty:
            return
        # Cold-probing a large library updates this cache a thousand times in
        # a few seconds. Rewriting the whole JSON that often would undo the
        # speed-up the cache exists for, so coalesce those writes. Shutdown
        # and bulk scans pass force=True.
        now = time.monotonic()
        if not force and now - self._last_save < 5.0:
            return
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            tmp = self.path.with_suffix(".json.tmp")
            tmp.write_text(json.dumps({"version": 1, "files": self._entries}))
            tmp.replace(self.path)
            self._dirty = False
            self._last_save = now
        except OSError as exc:
            log.warning("Could not save the media cache %s: %s", self.path, exc)


def _playlist_tag_matches(old: str, new: str) -> bool:
    """Whether two tags describe the same membership and definitions.

    The revision is deliberately ignored: it advances on every encode, and
    treating that alone as a change would remux a clip every time any
    playlist was touched.
    """
    if old == new:
        return True
    def content(raw: str) -> list[dict]:
        records = decode_playlist_metadata(raw)
        for record in records:
            record.pop("rev", None)
        return records
    return content(old) == content(new)


def _purge_slug_thumbs(slug: str, keep: frozenset[Path] = frozenset()) -> None:
    """Remove any cached thumbs for a slug (legacy + versioned variants)."""
    THUMB_DIR.mkdir(parents=True, exist_ok=True)
    for t in THUMB_DIR.glob(f"{glob.escape(slug)}*.jpg"):
        if t not in keep:
            t.unlink(missing_ok=True)


def _proxy_path(path: Path) -> Path:
    """Cache path for a clip's H.264 preview, keyed by file identity so a trim
    or a reused clip number naturally invalidates it (same idea as _thumb_path)."""
    try:
        st = path.stat()
        key = f"{path.stem}_{st.st_size}_{st.st_mtime_ns}"
    except OSError:
        key = path.stem
    # Invalidate previews made before the eight-bit playback fix (#172).
    return PROXY_DIR / f"{key}_v2.mp4"


def _purge_slug_proxies(slug: str, keep: frozenset[Path] = frozenset()) -> None:
    """Remove any cached preview proxies for a slug (all file versions)."""
    PROXY_DIR.mkdir(parents=True, exist_ok=True)
    for pattern in (f"{glob.escape(slug)}*.mp4", f"{glob.escape(slug)}*_audio_*.m4a"):
        for p in PROXY_DIR.glob(pattern):
            if p not in keep:
                p.unlink(missing_ok=True)


def _current_cache_files(path: Path) -> list[Path]:
    """The thumbnail and previews already made for this exact file version."""
    proxy = _proxy_path(path)
    found = [_thumb_path(path), proxy, *PROXY_DIR.glob(f"{glob.escape(proxy.stem)}_audio_*.m4a")]
    return [p for p in found if p.exists()]


def _audio_preview_path(path: Path, index: int) -> Path:
    proxy = _proxy_path(path)
    return proxy.with_name(f"{proxy.stem}_audio_{index}.m4a")


async def _make_audio_preview(path: Path, index: int) -> Path:
    """Browser-readable audio for one recorded stream, keeping its timeline."""
    target = _audio_preview_path(path, index)
    if target.exists() and target.stat().st_size > 0:
        return target
    target.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(prefix=".audio-", suffix=".m4a",
                                     dir=target.parent, delete=False) as handle:
        tmp = Path(handle.name)
    proc = None
    spawn = None
    try:
        spawn = asyncio.create_task(asyncio.create_subprocess_exec(
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-threads", "2",
            "-i", str(path), "-map", f"0:a:{index}", "-vn",
            "-af", "aresample=48000:async=1:first_pts=0", "-ac", "2",
            "-c:a", "aac", "-b:a", "192k", "-threads", "2",
            "-movflags", "+faststart", "-y", str(tmp),
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE,
        ))
        proc = await asyncio.shield(spawn)
        _, stderr = await communicate_with_timeout(proc, timeout=300)
        if proc.returncode != 0 or tmp.stat().st_size == 0:
            reason = (stderr or b"").decode(errors="replace").strip()[-300:]
            raise RuntimeError(reason or "audio preview produced no output")
        if _audio_preview_path(path, index) != target or not path.exists():
            raise RuntimeError("clip changed while preparing its audio")
        tmp.replace(target)
        return target
    finally:
        if proc is None and spawn is not None:
            try:
                proc = await spawn
            except OSError:
                pass  # The spawn error is propagated by the main path.
        if proc is not None and proc.returncode is None:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            await proc.communicate()
        tmp.unlink(missing_ok=True)


def _discord_path(path: Path) -> Path:
    """Where a clip's Discord-sized copy goes, keyed by file identity for the
    same reason as _proxy_path."""
    try:
        st = path.stat()
        key = f"{path.stem}_{st.st_size}_{st.st_mtime_ns}"
    except OSError:
        key = path.stem
    return path.parent / DISCORD_SUBDIR / f"{key}.mp4"


def _purge_clip_discord(path: Optional[Path]) -> None:
    """Remove any Discord copies of *path* (all file versions)."""
    if path is None:
        return
    folder = path.parent / DISCORD_SUBDIR
    if not folder.is_dir():
        return
    for p in folder.glob(f"{glob.escape(path.stem)}*.mp4"):
        p.unlink(missing_ok=True)


# WebEngine plays these without help; anything else gets an H.264 preview proxy.
_WEB_PLAYABLE_VCODECS = {"h264", "avc1", "vp8", "vp9", "av1"}

_IMAGE_MIME = {
    ".png":  "image/png",
    ".jpg":  "image/jpeg",
    ".jpeg": "image/jpeg",
}


async def copy_image_to_clipboard(path: Path) -> tuple[bool, str]:
    """Put the picture itself on the clipboard, not a link to it.

    Both tools keep owning the selection in the background after forking, so
    the clipboard outlives the request that filled it. Returns the reason on
    failure rather than raising: a screenshot that saved but could not be
    copied is still a screenshot, and the UI says so instead of losing it.
    """
    mime = _IMAGE_MIME.get(path.suffix.lower())
    if not mime:
        return False, f"{path.suffix} is not an image format Vice can copy."
    if shutil.which("wl-copy"):
        cmd = ["wl-copy", "--type", mime]
    elif shutil.which("xclip"):
        cmd = ["xclip", "-selection", "clipboard", "-t", mime]
    else:
        return False, "Copying images needs wl-clipboard (Wayland) or xclip (X11)."

    try:
        data = await asyncio.to_thread(path.read_bytes)
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        proc.stdin.write(data)
        await proc.stdin.drain()
        proc.stdin.close()
    except Exception as exc:
        log.warning("Copying %s to the clipboard failed: %s", path.name, exc)
        return False, "Could not reach the clipboard."
    return True, ""


# Encoders to try when a trim has to re-encode, best first, per source codec.
# Software is last on every row, so a machine with no usable hardware encoder
# behaves exactly the way it always has.
_TRIM_ENCODERS = {
    "hevc": ("hevc_nvenc", "hevc_vaapi", "libx265", "libx264"),
    "h265": ("hevc_nvenc", "hevc_vaapi", "libx265", "libx264"),
    "av1":  ("av1_nvenc", "av1_vaapi", "libsvtav1", "libx264"),
}
_TRIM_ENCODERS_DEFAULT = ("h264_nvenc", "h264_vaapi", "libx264")
_VAAPI_RENDER_NODE = "/dev/dri/renderD128"

_trim_encoder_cache: dict[str, list[str]] = {}


def _trim_encoder_candidates(vcodec: str) -> list[str]:
    """Encoders worth trying for a trim of a *vcodec* clip, best first.

    Keeping the clip's own codec matters. Trimming used to rewrite every H.265
    recording as H.264, against the user's own Video codec setting and the file
    size they picked it for (#172). Probing is cached per codec because it
    shells out to ffmpeg and nvidia-smi.
    """
    key = (vcodec or "").lower()
    if key not in _trim_encoder_cache:
        names = _TRIM_ENCODERS.get(key, _TRIM_ENCODERS_DEFAULT)
        installed = _available_encoders()
        # An empty probe is no opinion, so try the whole list rather than none.
        picked = [n for n in names if n in installed] if installed else list(names)
        if not _is_nvidia():
            picked = [n for n in picked if not n.endswith("_nvenc")]
        _trim_encoder_cache[key] = picked or ["libx264"]
    return _trim_encoder_cache[key]


def _remember_trim_encoder(vcodec: str, encoder: str) -> None:
    """Move the encoder that worked to the front, so the next trim of the same
    codec does not open a device that already refused once. ffmpeg being built
    with an encoder says nothing about the driver accepting it."""
    picked = _trim_encoder_cache.get((vcodec or "").lower())
    if picked and encoder in picked:
        picked.remove(encoder)
        picked.insert(0, encoder)


def _trim_encoder_args(encoder: str,
                       bitrate: Optional[int] = None) -> tuple[list[str], list[str]]:
    """(arguments before -i, arguments after) for one trim encoder.

    Quality is per encoder because the scales are not comparable: x265 at 23
    and NVENC at 22 land near x264 at 20, which is what trimming has always
    used. Audio is copied whichever encoder wins, so every recorded track
    survives at its original quality.

    *bitrate* is the source's video bitrate in bits per second. When it is
    known the re-encode is capped at it, so a trim cannot come back larger
    per second than the recording the user tuned (#256). VAAPI's constant QP
    ignores a cap, so it targets the bitrate instead.
    """
    cap = ([] if not bitrate else
           ["-maxrate", str(bitrate), "-bufsize", str(bitrate)])
    if encoder.endswith("_nvenc"):
        return [], ["-c:v", encoder, "-rc", "vbr", "-cq", "22", *cap, "-preset", "p4",
                    "-c:a", "copy"]
    if encoder.endswith("_vaapi"):
        rate = (["-rc_mode", "VBR", "-b:v", str(bitrate), "-maxrate", str(bitrate)]
                if bitrate else ["-qp", "22"])
        return (["-vaapi_device", _VAAPI_RENDER_NODE],
                ["-vf", "format=nv12,hwupload", "-c:v", encoder, *rate,
                 "-c:a", "copy"])
    if encoder == "libsvtav1":
        return [], ["-c:v", encoder, "-crf", "30", *cap, "-preset", "8", "-c:a", "copy"]
    crf = "23" if encoder == "libx265" else "20"
    return [], ["-c:v", encoder, "-preset", "veryfast", "-crf", crf, *cap, "-c:a", "copy"]


def _trimmed_copy_path(path: Path) -> Path:
    """A free name beside *path* for a trimmed copy: "<name>-trim", then
    "<name>-trim-2" and so on."""
    stem = f"{path.stem}-trim"
    candidate = path.with_name(f"{stem}{path.suffix}")
    n = 2
    while candidate.exists():
        candidate = path.with_name(f"{stem}-{n}{path.suffix}")
        n += 1
    return candidate


def _key_burn_for(project: dict, sources: dict, work: Path, accent: str) -> Optional[KeyBurn]:
    """The keyboard panel for an export, or None when no clip in it has keys
    or the panel cannot be drawn here."""
    intervals = {}
    for cid in sources:
        kept = keystore.load_sidecar(cid)
        if kept is not None:
            intervals[cid] = kept
    if not intervals:
        return None
    _, height, _ = canvas_for(project, sources)
    try:
        from .key_overlay import render_burn_assets
        assets = render_burn_assets(work, accent, max(0.5, height / 1080))
    except Exception as exc:  # pycairo missing, or a drawing failure
        log.warning("Exporting without the keyboard panel: %s", exc)
        return None
    return KeyBurn(panel=assets["panel"], size=assets["size"], keys=assets["keys"],
                   intervals=intervals)


async def _video_bitrate(path: Path) -> Optional[int]:
    """The first video stream's bitrate in bits per second, or None."""
    cmd = [
        "ffprobe", "-v", "error", "-select_streams", "v:0",
        "-show_entries", "stream=bit_rate", "-of", "csv=p=0", str(path),
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await communicate_with_timeout(proc, timeout=60)
    except (asyncio.TimeoutError, OSError) as exc:
        log.debug("Bitrate probe of %s failed: %s", path.name, exc)
        return None
    try:
        rate = int((out or b"").decode(errors="replace").strip().split("\n")[0])
    except ValueError:
        return None
    return rate if rate > 0 else None


async def _first_video_packet(path: Path) -> Optional[tuple[float, str]]:
    """(timestamp, flags) of the first video packet, or None when ffprobe
    cannot say.

    Deliberately unanchored. Asking for the packet at timestamp zero misses it
    entirely on a stream copy, because ffmpeg expresses "start here" with a
    leading edit list and timestamps that begin below zero, and ffprobe then
    answers with nothing at all (#172).
    """
    cmd = [
        "ffprobe", "-v", "error", "-select_streams", "v:0",
        "-show_entries", "packet=pts_time,flags", "-of", "csv=p=0", str(path),
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        out, _ = await communicate_with_timeout(proc, timeout=60)
    except (asyncio.TimeoutError, OSError) as exc:
        log.debug("Packet probe of %s failed: %s", path.name, exc)
        return None
    if proc.returncode != 0:
        return None
    first = (out or b"").decode(errors="replace").strip().split("\n")[0]
    pts, _, flags = first.partition(",")
    try:
        return float(pts), flags
    except ValueError:
        return None


async def _decode_complaint(path: Path) -> Optional[str]:
    """What a decoder says when asked for a picture from the start of *path*,
    or None when it produces one without complaining. ffmpeg reports a missing
    reference picture on stderr and still exits zero, so the output is the
    answer, not the exit code."""
    cmd = [
        "ffmpeg", "-v", "error", "-i", str(path),
        "-map", "0:v:0", "-frames:v", "1", "-f", "null", "-",
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await communicate_with_timeout(proc, timeout=60)
    except (asyncio.TimeoutError, OSError) as exc:
        log.debug("Decode probe of %s failed: %s", path.name, exc)
        return None
    complaint = (stderr or b"").decode(errors="replace").strip()
    if proc.returncode == 0 and not complaint:
        return None
    return complaint.splitlines()[0] if complaint else "ffmpeg could not read it"


async def _xdg_open(target: Path) -> None:
    """Hand *target* to the desktop's default handler, and log why when it
    fails.

    The daemon can outlive the environment it started with, so the desktop's
    variables are refreshed from the session first. xdg-open picks its opener
    from them, and on Plasma a missing KDE_SESSION_VERSION sends it to a
    kfmclient that no longer exists (#252).
    """
    try:
        await asyncio.to_thread(load_user_systemd_env)
        proc = await asyncio.create_subprocess_exec(
            "xdg-open", str(target),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await communicate_with_timeout(proc, timeout=30)
    except (asyncio.TimeoutError, OSError) as exc:
        log.warning("xdg-open %s failed: %s", target, exc)
        return
    complaint = (stderr or b"").decode(errors="replace").strip()
    if proc.returncode or complaint:
        log.warning("xdg-open %s exited %s: %s", target, proc.returncode,
                    complaint or "no output")


async def _trim_result_problem(path: Path) -> str:
    """Why the trimmed *path* would not play in the app window, or "" when it
    will.

    A trimmed clip has to start the way an untrimmed one does, and two shapes
    get past every other check. A stream copy that begins between keyframes
    leaves a track whose frames refer back to a picture the file does not
    contain. A copy that begins on a leading edit list only plays where the
    player honours edit lists. Both probe fine, both play in mpv and VLC, and
    both are a black frame with the duration filled in and no error at all in
    the app window (#172).

    An unanswerable probe returns "", because keeping the copy is better than
    re-encoding on a guess.
    """
    packet = await _first_video_packet(path)
    if packet is None:
        return ""
    pts, flags = packet
    if "K" not in flags:
        return "starts between keyframes"
    if pts < 0:
        return "starts on an edit list before zero"
    complaint = await _decode_complaint(path)
    if complaint:
        return f"cannot decode its first frame ({complaint})"
    return ""


_PREVIEW_TIMEOUT = 300


async def _make_preview_proxy(path: Path, vcodec: str) -> Optional[Path]:
    """Return an H.264 copy of *path* for in-app playback, transcoding once and
    caching it. Returns None when the source is already web-playable or the
    transcode fails, so the caller can just serve the original."""
    if vcodec and vcodec in _WEB_PLAYABLE_VCODECS:
        return None
    proxy = _proxy_path(path)
    if proxy.exists() and proxy.stat().st_size > 0:
        return proxy

    PROXY_DIR.mkdir(parents=True, exist_ok=True)
    tmp = proxy.with_suffix(".mp4.tmp")
    # Same duration and fps as the source so trim in/out points map 1:1 to the
    # original file, which is what the trim endpoint actually cuts.
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-threads", "2", "-filter_threads", "1",
        "-i", str(path),
        "-map", "0:v:0?", "-map", "0:a?",
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "23",
        "-threads:v", "2",
        "-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2", "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-b:a", "160k",
        "-movflags", "+faststart",
        # The temp name ends in .tmp, so name the container explicitly.
        "-f", "mp4",
        "-y", str(tmp),
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await communicate_with_timeout(proc, timeout=_PREVIEW_TIMEOUT)
        if proc.returncode != 0 or not tmp.exists() or tmp.stat().st_size == 0:
            log.warning("preview proxy for %s failed: %s", path.name,
                        (stderr or b"").decode(errors="replace")[:200])
            return None
        tmp.replace(proxy)
        return proxy
    except asyncio.TimeoutError:
        log.warning("preview proxy for %s timed out after %ss", path.name, _PREVIEW_TIMEOUT)
        return None
    except OSError as exc:
        log.warning("preview proxy for %s errored: %s", path.name, exc)
        return None
    finally:
        tmp.unlink(missing_ok=True)


_DISCORD_TIMEOUT = 600


async def _make_discord_copy(path: Path, duration: float,
                            audio_streams: int = 1, vcodec: str = "") -> Optional[Path]:
    """Return a copy of *path* small enough to drop into a Discord chat.

    The source itself when it is already an H.264 MP4 that fits, a cached
    H.264 transcode otherwise, and None when the transcode fails. Raises
    DiscordTooLarge when no bitrate fits the clip under the cap. Same bitrate
    budget as the editor's "optimize for Discord" export, so the two produce
    comparable files.

    *vcodec* is the source's video codec as ffprobe names it. A small AV1 or
    HEVC clip used to go out unconverted, and plenty of phones cannot play
    either in Discord (#289), so only H.264 is passed through.
    """
    try:
        already_small = (path.suffix.lower() == ".mp4"
                         and vcodec == "h264"
                         and path.stat().st_size <= DISCORD_MAX_BYTES)
    except OSError:
        return None
    if already_small:
        return path

    out = _discord_path(path)
    if out.exists() and 0 < out.stat().st_size <= DISCORD_MAX_BYTES:
        return out

    video_kbps, audio_kbps = discord_bitrates(duration, audio_streams)

    out.parent.mkdir(parents=True, exist_ok=True)
    tmp = out.with_suffix(".mp4.tmp")
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-i", str(path),
        "-map", "0:v:0?", "-map", "0:a?",
        # 1440p at a Discord-sized bitrate looks worse than the same bits
        # spent on 1080p, and Discord's inline player tops out there anyway.
        # The pad rounds both sides up: yuv420p has no odd dimension, and
        # libx264 refuses the frame rather than fixing it (a 127x73 source
        # scaled to 128x73 failed outright).
        "-vf", "scale=-2:min(1080\\,ih),pad=ceil(iw/2)*2:ceil(ih/2)*2",
        "-c:v", "libx264", "-preset", "veryfast",
        "-b:v", f"{video_kbps}k",
        "-maxrate", f"{round(video_kbps * 1.45)}k",
        "-bufsize", f"{video_kbps * 2}k",
        "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-b:a", f"{audio_kbps}k",
        "-movflags", "+faststart",
        "-f", "mp4",
        "-y", str(tmp),
    ]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await communicate_with_timeout(proc, timeout=_DISCORD_TIMEOUT)
        if proc.returncode != 0 or not tmp.exists() or tmp.stat().st_size == 0:
            log.warning("Discord copy of %s failed: %s", path.name,
                        (stderr or b"").decode(errors="replace")[:200])
            return None
        # Single-pass ABR aims at the budget, it does not promise it. Never
        # hand out or cache a copy Discord would reject.
        size = tmp.stat().st_size
        if size > DISCORD_MAX_BYTES:
            log.warning("Discord copy of %s came out at %.1f MB, over the cap",
                        path.name, size / 1048576)
            return None
        tmp.replace(out)
        return out
    except asyncio.TimeoutError:
        log.warning("Discord copy of %s timed out after %ss", path.name,
                    _DISCORD_TIMEOUT)
        return None
    except OSError as exc:
        log.warning("Discord copy of %s errored: %s", path.name, exc)
        return None
    finally:
        # communicate_with_timeout has already killed and reaped the encoder,
        # so nothing is still writing to this file.
        tmp.unlink(missing_ok=True)


# ── helpers ──────────────────────────────────────────────────────────────────

def _local_ip() -> str:
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception:
        return "127.0.0.1"


_PROBE_DEFAULTS = {"width": 1920, "height": 1080, "duration": 0, "vcodec": ""}


# A moov repair rewrites the original file. It must never run in parallel
# with another repair (or with the rest of the gallery's probes): a handful of
# them at once saturates a network mount, which in turn makes probes time out
# and asks for even more repairs.
_MOOV_REPAIR_SEM = asyncio.Semaphore(1)


async def _remux_moov(path: Path) -> bool:
    """Try to recover `path` via `ffmpeg -c copy -movflags +faststart`.

    Used for clips whose MP4 container is damaged (no moov atom), happens
    when the encoder was killed mid-finalize. The original file is only
    replaced when the remuxed copy probes as a sane video of comparable
    size; a remux that produces a near-empty file means the input was
    *not* a simple moov-atom problem, and replacing would destroy data
    (this used to truncate healthy clips to 0.02 s). Returns True when
    the remuxed file replaced the original.
    """
    tmp = path.with_suffix(path.suffix + ".fix.mp4")
    try:
        before = path.stat()
        proc = await asyncio.create_subprocess_exec(
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(path), *KEEP_ALL_STREAMS, "-c", "copy",
            "-movflags", "+faststart", str(tmp),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        await communicate_with_timeout(proc, timeout=60)
        if proc.returncode == 0 and tmp.exists():
            remuxed = await probe_media(tmp)
            orig_size = before.st_size
            if (
                remuxed
                and remuxed["duration"] > 0
                and tmp.stat().st_size >= orig_size * 0.5
            ):
                # Keep the recording's timestamp. Without this the repaired
                # clip looks brand new to the gallery and jumps to the top,
                # and its cached metadata is invalidated by the new identity.
                try:
                    os.utime(tmp, ns=(before.st_atime_ns, before.st_mtime_ns))
                except OSError as exc:
                    log.debug("Could not keep the mtime of %s: %s",
                              tmp.name, exc)
                tmp.replace(path)
                try:
                    os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
                except OSError as exc:
                    log.debug("Could not keep the mtime of %s: %s",
                              path.name, exc)
                return True
            log.warning(
                "Remux of %s produced an invalid or truncated file "
                "(duration=%.2fs, %d → %d bytes), keeping the original",
                path.name,
                remuxed["duration"] if remuxed else 0.0,
                orig_size,
                tmp.stat().st_size,
            )
    except Exception as exc:
        log.warning("Remux of %s failed: %s", path.name, exc)
    finally:
        try:
            tmp.unlink(missing_ok=True)
        except OSError as exc:
            log.debug("Could not remove the remux temp file %s: %s", tmp.name, exc)
    return False


async def _ffprobe(path: Path) -> dict:
    """Return {"width", "height", "duration"} via ffprobe.

    If the file cannot be probed at all, its container is probably missing
    the moov atom, so try one (validated, non-destructive) remux and re-probe
    before giving up.

    A file that still cannot be read comes back carrying "unreadable" and the
    reason, so the gallery can say so. It used to fall back to the defaults
    and be listed as an ordinary clip of 0:00 with no thumbnail, which is
    what a broken clip looks like to the person who recorded it (#154).

    A probe that *times out* is not evidence of a damaged container: on a
    network library it usually means the mount is busy. Repairing those was
    catastrophic: eight timeouts started eight full remuxes, which made the
    mount slower and produced more timeouts. Timeouts therefore come back
    marked "_transient" (so they are not cached) and never remux.
    """
    meta, why = await probe_media_detailed(path)
    if meta and meta["duration"] > 0:
        return meta
    if path.suffix.lower() != ".mp4":
        # The remux below repairs MP4 moov atoms; other containers are
        # served as-is.
        return meta or _unreadable_meta(why)
    if _probe_failure_is_transient(why):
        return _transient_meta(why)
    async with _MOOV_REPAIR_SEM:
        # Repairing replaces the user's file, so confirm the failure with a
        # second probe while no other repair is running. The first failure is
        # often just contention on the network mount.
        meta, why = await probe_media_detailed(path)
        if meta and meta["duration"] > 0:
            return meta
        if _probe_failure_is_transient(why):
            return _transient_meta(why)
        log.warning("ffprobe cannot read %s (%s), attempting moov remux",
                    path.name, why or "it reads, but reports no duration")
        if await _remux_moov(path):
            meta, why = await probe_media_detailed(path)
            log.info(
                "Remuxed %s, duration now %.2fs",
                path.name, meta["duration"] if meta else 0.0,
            )
    return meta or _unreadable_meta(why)


def _probe_failure_is_transient(reason: str) -> bool:
    """A failure caused by the mount, not by the clip's container.

    Both a timeout and a file that vanished mid-probe say nothing about the
    MP4 structure, so neither justifies rewriting the file.
    """
    return (reason.startswith("ffprobe timed out")
            or "No such file or directory" in reason)


def _transient_meta(reason: str) -> dict:
    """A failed probe that says nothing about the file itself.

    The caller must not cache this: the same clip may read fine on the next
    attempt once the mount is quiet.
    """
    meta = dict(_PROBE_DEFAULTS)
    meta["_transient"] = True
    meta["unreadable_reason"] = reason
    return meta


def _unreadable_meta(reason: str) -> dict:
    meta = dict(_PROBE_DEFAULTS)
    meta["unreadable"] = True
    meta["unreadable_reason"] = reason or "ffprobe could not read this file"
    return meta


# Thumbnail ffmpegs that are reading a clip file right now. Playback kills
# them so the share's bandwidth belongs to the player; _make_thumb removes
# each process from the set as soon as it is reaped.
_THUMB_PROCS: set[asyncio.subprocess.Process] = set()


def _stop_thumb_processes() -> None:
    """Abandon any thumbnail frame that is being read for a player."""
    for proc in list(_THUMB_PROCS):
        try:
            proc.kill()
        except ProcessLookupError:
            pass


async def _make_thumb(path: Path, duration: float = 0.0,
                      thumb: Path | None = None, width: int = 640) -> Path:
    """Lazily generate a 640px-wide JPEG thumbnail stored in THUMB_DIR.

    Short clips (< 1 s) used to come back blank because `-ss 0.75` seeks
    past EOF and `-vf thumbnail` needs a 100-frame lookahead. Now we seek
    to `min(duration/2, 0.75)` and use a plain scale filter, which works
    on sub-second clips too.

    *thumb* may be supplied by the caller when the library index already
    knows the file's identity, which avoids a stat on a network mount.
    """
    THUMB_DIR.mkdir(parents=True, exist_ok=True)
    if thumb is None:
        thumb = _thumb_path(path)
    if thumb.exists() and thumb.stat().st_size > 0:
        return thumb
    if duration and duration > 0:
        seek_ts = min(duration / 2.0, 0.75)
    else:
        seek_ts = 0.0
    # Publish only complete images. Concurrent requests get separate temporary
    # files, so cancellation cannot delete another request's finished thumbnail.
    with tempfile.NamedTemporaryFile(
        prefix=f".{thumb.stem}.", suffix=".jpg", dir=THUMB_DIR, delete=False,
    ) as handle:
        tmp = Path(handle.name)
    try:
        proc = await asyncio.create_subprocess_exec(
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-ss", f"{seek_ts:.3f}",
            "-i", str(path),
            "-frames:v", "1",
            "-vf", f"scale={width}:-2",
            "-q:v", "4",
            "-y", str(tmp),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        _THUMB_PROCS.add(proc)
        try:
            await communicate_with_timeout(proc, timeout=20)
            if proc.returncode == 0 and tmp.stat().st_size > 0:
                tmp.replace(thumb)
        finally:
            _THUMB_PROCS.discard(proc)
    except Exception as exc:
        log.debug("Thumbnail generation failed for %s: %s", path.name, exc)
    finally:
        tmp.unlink(missing_ok=True)
    return thumb


# Discord can use the Twitter player metadata to skip its thumbnailing pass
# for direct video links. Keep the OpenGraph metadata as the fallback used by
# other unfurlers (issues #77, #100, #207).
_EMBED_PAGE = """\
<!DOCTYPE html>
<html><head>
  <meta charset="utf-8">
  <meta name="theme-color"              content="{color}">
  <meta name="twitter:card"             content="player">
  <meta name="twitter:player"           content="{video_url}">
  <meta name="twitter:player:stream"    content="{video_url}">
  <meta name="twitter:player:stream:content_type" content="{video_type}">
  <meta name="twitter:image"            content="{thumb_url}">
  <meta property="og:site_name"         content="Vice">
  <meta property="og:type"              content="video.other">
  <meta property="og:url"               content="{page_url}">
  <meta property="og:title"             content="{title}">
  <meta property="og:description"       content="Clipped with Vice on Linux">
  <meta property="og:video"             content="{video_url}">
  <meta property="og:video:url"         content="{video_url}">
  <meta property="og:video:secure_url"  content="{video_url}">
  <meta property="og:video:type"        content="{video_type}">
  <meta property="og:video:width"       content="{width}">
  <meta property="og:video:height"      content="{height}">
  <meta property="og:image"             content="{thumb_url}">
  <title>{title}</title>
  <style>
    body{{margin:0;background:#000;display:flex;align-items:center;
         justify-content:center;min-height:100vh}}
    video{{max-width:100%;max-height:100vh}}
  </style>
</head>
<body>
  <video src="{video_url}" controls autoplay muted loop></video>
</body></html>
"""


# cloudflared prints its own infrastructure hostnames alongside the tunnel
# address. api.trycloudflare.com winning the match meant every share link
# pointed at Cloudflare's API, which answers "Method Not Allowed" (#143).
_TRYCLOUDFLARE_RE = re.compile(r"https://([a-zA-Z0-9-]+)\.trycloudflare\.com")
_NOT_A_TUNNEL = {"api", "www", "dash", "developers", "blog"}
_TUNNEL_RETRY_INITIAL = 5.0
_TUNNEL_RETRY_MAX = 300.0


def _cloudflared_failure_detail(lines: list[str]) -> str:
    """Return a short actionable cloudflared diagnostic from its output."""
    relevant = [
        line.strip()
        for line in lines
        if any(marker in line.lower() for marker in ("err", "error", "failed", "unable"))
    ]
    if not relevant:
        return ""
    detail = " | ".join(relevant[-3:])
    return " ".join(detail.split())[:600]


def _quick_tunnel_url(line: str) -> Optional[str]:
    """The quick-tunnel address in a line of cloudflared output, or None.

    Quick tunnels get multi-word hyphenated subdomains, so a hyphenless host
    is ranked last rather than rejected: a naming change at Cloudflare should
    cost a worse guess, not a broken feature.
    """
    fallback: Optional[str] = None
    for match in _TRYCLOUDFLARE_RE.finditer(line):
        if match.group(1).lower() in _NOT_A_TUNNEL:
            continue
        if "-" in match.group(1):
            return match.group(0)
        if fallback is None:
            fallback = match.group(0)
    return fallback


# ── share server ─────────────────────────────────────────────────────────────

class _TrackedFileResponse(web.FileResponse):
    """A FileResponse that reports its whole body-sending lifetime.

    ``FileResponse.prepare`` does not return until the file has been sent (or
    the client disconnects), so wrapping it is what lets the server tell a
    player apart from a request that merely looked at a video URL.
    """

    def __init__(self, path, *, on_start: Callable[[], None],
                 on_finish: Callable[[], None], **kwargs) -> None:
        super().__init__(path, **kwargs)
        self._on_start = on_start
        self._on_finish = on_finish

    async def prepare(self, request):
        self._on_start()
        try:
            return await super().prepare(request)
        finally:
            self._on_finish()


class ShareServer:
    def __init__(self, cfg) -> None:
        self.cfg = cfg
        # Every local and public request marks the app busy. Background
        # metadata remuxes wait for a quiet moment, so they never compete with
        # a player or a grid of thumbnails on a slow network folder.
        self._last_activity = 0.0
        self._last_media_activity = 0.0
        # Video responses that are still sending their body. This stays above
        # zero for the whole of playback, not just while the request handler
        # runs, which is what thumbnail work needs to watch.
        self._media_streams = 0
        # Video handlers that have been entered but have not returned their
        # response yet. A range response can spend a while on its first stat
        # when a thumbnail ffmpeg is still winding down, and no thumbnail may
        # slip into that gap.
        self._media_pending = 0
        self._local_app = web.Application(middlewares=[self._note_activity])
        self._public_app = web.Application(middlewares=[self._note_activity])
        self._local_runner: Optional[web.AppRunner] = None
        self._local_site: Optional[web.TCPSite] = None
        self._public_runner: Optional[web.AppRunner] = None
        self._public_site: Optional[web.TCPSite] = None
        self._legacy_public_site: Optional[web.TCPSite] = None

        # slug → Path  (populated from disk on start + runtime additions)
        self._clips: dict[str, Path] = {}
        # slug → (path, size, mtime_ns, mtime). The scanner already stats
        # every file, so list responses, probes and thumbnails reuse that
        # instead of running hundreds of extra stats over a network mount.
        self._clip_stats: dict[str, tuple[Path, int, int, float]] = {}
        # slug → {width, height, duration}
        self._meta:  dict[str, dict] = {}
        # Screenshots, kept in their own index. Slugs are filename stems like a
        # clip's, and they can collide with one, so nothing may look an image
        # up in _clips or the other way round.
        self._images: dict[str, Path] = {}

        # Probes and playlist tags survive restarts. Both are expensive for a
        # large library, and neither depends on the daemon being alive.
        self._media_cache = _MediaCache(MEDIA_CACHE_PATH)
        # slug → the vice_playlists tag last known to be in the file.
        self._playlist_tags: dict[str, str] = {}
        # A cold clip can be requested by the list, its thumbnail and a player
        # at the same time. One ffprobe per clip is plenty.
        self._meta_locks: dict[str, asyncio.Lock] = {}
        self._list_probes: dict[str, tuple[Path, asyncio.Task]] = {}
        # ffprobe is a read, but four at once already saturates a CIFS mount
        # enough that other probes time out. The list no longer waits for
        # these, so politeness costs nothing the user can see.
        self._probe_sem = asyncio.Semaphore(4)
        # A metadata write is a full remux. One at a time keeps a big backfill
        # from saturating a WiFi/CIFS library and making the whole app crawl.
        self._tag_write_sem = asyncio.Semaphore(1)
        self._tag_locks: dict[str, asyncio.Lock] = {}
        self._background_tasks: set[asyncio.Task] = set()
        # One playlist-metadata backfill at a time. A rescan or the UI button
        # can request another pass while one is already running.
        self._playlist_tag_task: Optional[asyncio.Task] = None
        self._playlist_tag_recheck = False
        # Serialise rescans so the 30-second watcher and the recheck button
        # cannot race each other with two different directory snapshots.
        self._rescan_lock = asyncio.Lock()
        # Thumbnail ffmpegs are short but still open the source file. A grid
        # asks for a dozen at once; let a few through, not all of them.
        self._thumb_sem = asyncio.Semaphore(3)
        # Signature of the last successful output scan, so the mount watcher
        # can notice a change without probing or stat-ing every clip twice.
        self._library_signature: tuple = ()
        # Set by _scan_library when the mount refused to stat some files.
        # Incomplete scans must never be read as deletions.
        self._last_scan_incomplete = False

        self.playlists = PlaylistStore()
        self._views = _load_views()
        self._share_tokens = _load_share_tokens()
        self._share_slugs = {token: slug for slug, token in self._share_tokens.items()}
        self.editor_project = EditorProjectStore()
        self._exports = ExportManager(self.broadcast)

        # One encoder across the library, shared by preview proxies and
        # Discord copies. Different clips used to spawn independent encoders,
        # each allocating its own frame queues (#193).
        self._proxy_lock = asyncio.Lock()
        self._proxy_tasks: set[asyncio.Task] = set()
        self._proxy_stopping = False

        self._tunnel_proc: Optional[asyncio.subprocess.Process] = None
        self._tunnel_task: Optional[asyncio.Task] = None
        self._tunnel_stopping = False
        self._tunnel_url:  Optional[str] = None
        self._local_base_url: Optional[str] = None
        self._public_bind_url: Optional[str] = None
        self._legacy_public_bind_url: Optional[str] = None

        # Connected WebSocket clients
        self._ws_clients: set[web.WebSocketResponse] = set()

        # Injected by ViceDaemon so /api/trigger works
        self.trigger_clip_cb: Optional[Callable[[], Coroutine]] = None
        # Injected so the Settings "Check now" button can skip the daily wait.
        self.check_update_cb: Optional[Callable[[], Coroutine]] = None
        # Injected so /api/status can report live state
        self.get_status_cb: Optional[Callable[[], dict]] = None
        # Injected so config changes can be applied without restart when possible.
        self.apply_config_cb: Optional[Callable[[], Coroutine]] = None
        # Injected so Settings can put the keyboard overlay into move mode.
        self.key_overlay_move_cb: Optional[Callable[[bool], None]] = None
        # Starts installing the GSR Flatpak (Vice Flatpak only); False when it cannot.
        self.install_gsr_cb: Optional[Callable[[], bool]] = None

        self._setup_local_routes()
        self._setup_public_routes()

    # ── routes ───────────────────────────────────────────────────────────────

    @web.middleware
    async def _note_activity(self, request: web.Request, handler):
        """Mark the app busy, and remember when a video was last opened."""
        now = time.monotonic()
        self._last_activity = now
        is_video = request.path.startswith("/v/")
        if is_video:
            self._last_media_activity = now
            self._media_pending += 1
            # A click can arrive while the grid is still making thumbnails.
            # Abandon those reads now; they can be made again later, but the
            # player cannot wait for them on a slow network folder.
            _stop_thumb_processes()
        try:
            return await handler(request)
        finally:
            if is_video:
                self._media_pending = max(0, self._media_pending - 1)

    def _media_busy(self) -> bool:
        """Whether a clip is streaming, or is about to start streaming."""
        return (self._media_pending > 0 or self._media_streams > 0
                or bool(self._proxy_tasks))

    def _video_stream_started(self) -> None:
        self._media_streams += 1
        self._last_media_activity = time.monotonic()
        _stop_thumb_processes()

    def _video_stream_finished(self) -> None:
        self._media_streams = max(0, self._media_streams - 1)
        self._last_media_activity = time.monotonic()

    def _video_response(self, path: Path, *, content_type: str) -> web.Response:
        """Serve a clip and keep thumbnail work away until it finishes."""
        return _TrackedFileResponse(
            path,
            on_start=self._video_stream_started,
            on_finish=self._video_stream_finished,
            headers={
                "Content-Type": content_type,
                "Accept-Ranges": "bytes",
                "Cache-Control": "no-cache",
            },
        )

    def _setup_local_routes(self) -> None:
        r = self._local_app.router

        # Web UI
        r.add_get("/",            self._ui)
        for _kind in _UI_ASSET_KINDS:
            r.add_get(f"/{_kind}/{{name}}",
                      lambda req, k=_kind: self._ui_asset(req, kind=k))

        # Discord embed pages
        r.add_get("/c/{ref}",     self._embed_page)

        # Media
        r.add_get("/v/{ref}",     self._video)
        r.add_get("/t/{ref}",     self._thumb)
        # Images, local only. There is no share link for a screenshot, so
        # these have no counterpart on the public app below.
        r.add_get("/i/{slug}",    self._image_file)
        r.add_get("/it/{slug}",   self._image_thumb)

        # REST
        r.add_get("/api/clips",              self._api_clips)
        r.add_post("/api/library/rescan",    self._api_rescan_library)
        r.add_get("/api/clips/{slug}",       self._api_clip_info)
        r.add_delete("/api/clips/{slug}",    self._api_delete)
        r.add_post("/api/clips/{slug}/trim",              self._api_trim)
        r.add_post("/api/clips/{slug}/rename",            self._api_rename)
        r.add_post("/api/clips/{slug}/reveal",            self._api_reveal)
        r.add_post("/api/clips/{slug}/open",              self._api_open)
        r.add_post("/api/clips/{slug}/copy-file",         self._api_copy_file)
        r.add_get("/api/clips/{slug}/discord",            self._api_discord_copy)
        r.add_post("/api/clips/{slug}/frame",             self._api_save_frame)
        r.add_get("/api/clips/{slug}/audio/{index}",      self._audio_track)
        r.add_get("/api/app-state",                       self._api_get_app_state)
        r.add_post("/api/app-state",                      self._api_set_app_state)
        r.add_post("/api/clips/{slug}/view",              self._api_view)
        r.add_get("/api/clips/{slug}/highlights",         self._api_get_highlights)
        r.add_get("/api/clips/{slug}/keys",               self._api_get_keys)
        r.add_post("/api/overlay/move",                   self._api_overlay_move)
        r.add_post("/api/recorder/install-gsr",           self._api_install_gsr)
        r.add_post("/api/clips/{slug}/highlights",        self._api_add_highlight)
        r.add_patch("/api/clips/{slug}/highlights/{hid}", self._api_patch_highlight)
        r.add_delete("/api/clips/{slug}/highlights/{hid}",self._api_del_highlight)
        r.add_get("/api/editor/project",              self._api_editor_get_project)
        r.add_post("/api/editor/project",             self._api_editor_save_project)
        r.add_post("/api/editor/export",              self._api_editor_export)
        r.add_post("/api/editor/export/{jid}/cancel", self._api_editor_export_cancel)
        r.add_get("/api/images",                self._api_images)
        r.add_delete("/api/images/{slug}",      self._api_image_delete)
        r.add_post("/api/images/{slug}/rename", self._api_image_rename)
        r.add_post("/api/images/{slug}/reveal", self._api_image_reveal)
        r.add_post("/api/images/{slug}/open",   self._api_image_open)
        r.add_post("/api/images/{slug}/copy",   self._api_image_copy)
        r.add_post("/api/images/{slug}/annotate", self._api_image_annotate)
        r.add_get("/api/playlists",            self._api_playlists)
        r.add_post("/api/playlists",           self._api_create_playlist)
        r.add_patch("/api/playlists/{pid}",    self._api_patch_playlist)
        r.add_delete("/api/playlists/{pid}",   self._api_delete_playlist)
        r.add_post("/api/playlists/{pid}/clips",           self._api_playlist_add_clip)
        r.add_delete("/api/playlists/{pid}/clips/{slug}",  self._api_playlist_remove_clip)
        r.add_get("/api/config",               self._api_get_config)
        r.add_get("/api/displays",             self._api_get_displays)
        r.add_get("/api/audio-sources",        self._api_get_audio_sources)
        r.add_post("/api/config",              self._api_set_config)
        r.add_get("/api/status",               self._api_status)
        r.add_post("/api/update/check",         self._api_check_update)
        r.add_post("/api/trigger",             self._api_trigger)
        r.add_post("/api/quit",                self._api_quit)
        r.add_post("/api/uninstall",           self._api_uninstall)

        # WebSocket
        r.add_get("/ws", self._ws_handler)

    def _setup_public_routes(self) -> None:
        r = self._public_app.router
        r.add_get("/c/{ref}", self._public_embed_page)
        r.add_get("/v/{ref}", self._public_video)
        r.add_get("/t/{ref}", self._public_thumb)

    # ── library scanning and metadata ────────────────────────────────────────

    def _scan_library(self) -> tuple[
            dict[str, Path], tuple,
            dict[str, tuple[Path, int, int, float]]]:
        """The clip index, identity signature and file stats of the folder.

        All three are produced in one directory walk. The watcher runs this
        periodically, so stat-ing every clip twice (or globbing the folder
        once per container) is wasteful on a large library or a network drive.
        """
        out_dir = resolve_path(self.cfg.output.directory)
        found: dict[str, Path] = {}
        identities: dict[str, tuple[str, int, int]] = {}
        stats: dict[str, tuple[Path, int, int, float]] = {}
        self._last_scan_incomplete = False
        if not out_dir.exists():
            return found, (), stats
        media: list[tuple[Path, float, int, int]] = []
        # Files whose stat failed this pass. A busy CIFS/NFS mount can fail an
        # individual stat even though the file is there; dropping it from the
        # index made the next scan report hundreds of "new" clips at once.
        unread: list[Path] = []
        try:
            with os.scandir(out_dir) as entries:
                for entry in entries:
                    name = entry.name
                    if _is_transient_clip_name(name):
                        continue
                    if not name.lower().endswith((".mp4", ".mkv")):
                        continue
                    try:
                        if not entry.is_file():
                            continue
                        st = entry.stat()
                    except OSError:
                        # A clip saved or deleted while the scan runs must not
                        # make us mistake the rest of the directory for gone.
                        unread.append(out_dir / name)
                        continue
                    media.append(
                        (out_dir / name, st.st_mtime, st.st_size,
                         st.st_mtime_ns))
        except OSError as exc:
            log.warning("Could not scan the clip directory %s: %s", out_dir, exc)
            return found, (), stats
        for attempt in range(3):
            if not unread:
                break
            if attempt:
                time.sleep(0.25)
            still: list[Path] = []
            for path in unread:
                try:
                    st = path.stat()
                except OSError:
                    still.append(path)
                    continue
                if not stat.S_ISREG(st.st_mode):
                    continue
                media.append((path, st.st_mtime, st.st_size, st.st_mtime_ns))
            unread = still
        if unread:
            # The scan is incomplete, not empty: callers must keep the clips
            # they already know about instead of treating them as deleted.
            self._last_scan_incomplete = True
            log.warning(
                "Could not read %d file%s in %s; keeping the previous library "
                "index for %s",
                len(unread), "" if len(unread) == 1 else "s", out_dir,
                "it" if len(unread) == 1 else "them",
            )
        for path, mtime, size, mtime_ns in sorted(media, key=lambda item: item[1]):
            slug = path.stem
            found[slug] = path
            identities[slug] = (str(path), size, mtime_ns)
            stats[slug] = (path, size, mtime_ns, mtime)
        signature = tuple(
            (slug, *identities[slug]) for slug in sorted(identities)
        )
        return found, signature, stats

    def _scan_output_dir(self) -> tuple[dict[str, Path], tuple]:
        """Backwards-compatible index-and-signature view of _scan_library."""
        found, signature, _ = self._scan_library()
        return found, signature

    def _file_info(self, path: Path) -> Optional[tuple[Path, int, int, float]]:
        try:
            st = path.stat()
        except OSError:
            return None
        return path, st.st_size, st.st_mtime_ns, st.st_mtime

    def _clip_info(self, slug: str,
                   path: Path) -> Optional[tuple[Path, int, int, float]]:
        """Cached identity for a clip, falling back to one stat."""
        info = getattr(self, "_clip_stats", {}).get(slug)
        if info is not None and info[0] == path:
            return info
        info = self._file_info(path)
        if info is None:
            self._clip_stats.pop(slug, None)
        else:
            self._clip_stats[slug] = info
        return info

    def _known_thumb_path(self, slug: str, path: Path) -> Optional[Path]:
        """Thumbnail path from the scan's identity, without touching the mount."""
        info = getattr(self, "_clip_stats", {}).get(slug)
        if info is None or info[0] != path:
            return None
        return _thumb_path(path, identity=(info[1], info[2]))

    def _remember_clip(self, slug: str, path: Path) -> None:
        info = self._file_info(path)
        if info is None:
            self._clip_stats.pop(slug, None)
        else:
            self._clip_stats[slug] = info

    def _clip_identity(self, slug: str, path: Path) -> Optional[list[int]]:
        info = getattr(self, "_clip_stats", {}).get(slug)
        if info is not None and info[0] == path:
            return [info[1], info[2]]
        return _MediaCache._identity(path)

    def _track_task(self, task: asyncio.Task) -> asyncio.Task:
        self._background_tasks.add(task)

        def _finished(done: asyncio.Task) -> None:
            self._background_tasks.discard(done)
            if done.cancelled():
                return
            exc = done.exception()
            if exc is not None:
                log.debug("A background library task failed: %s", exc)

        task.add_done_callback(_finished)
        return task

    async def _clip_tags(self, slug: str, path: Path) -> dict[str, str]:
        """Container tags for one clip, cached by file identity."""
        identity = self._clip_identity(slug, path)
        tags = self._media_cache.get_tags(path, identity)
        if tags is None:
            async with self._probe_sem:
                tags = await probe_container_tags(path)
            self._media_cache.put_tags(path, tags)
        self._playlist_tags[slug] = tags.get(PLAYLIST_TAG, "")
        return tags

    async def _playlist_tag_entries(self, only: Optional[set[str]] = None,
                                    *, throttle: bool = False
                                    ) -> list[tuple[str, str]]:
        """Read the playlist tag from every clip, in parallel.

        *throttle* is for the background migration: one file at a time, with
        a pause, and only while nobody is using the app. The read-only probe
        is much cheaper than a remux, but eight of them at once on a WiFi
        library still made the gallery feel sluggish.
        """
        async def one(slug: str, path: Path):
            tags = await self._clip_tags(slug, path)
            raw = tags.get(PLAYLIST_TAG, "")
            return (slug, raw) if raw else None

        items = [
            (slug, path) for slug, path in self._clips.items()
            if only is None or slug in only
        ]
        if throttle:
            # Serve whatever the cache already knows without a wait or a
            # pause. Only files whose tags were never read need a probe, and
            # those are the ones that must be polite.
            found: list[tuple[str, str]] = []
            pending: list[tuple[str, Path]] = []
            for slug, path in items:
                identity = self._clip_identity(slug, path)
                tags = self._media_cache.get_tags(path, identity)
                if tags is None:
                    pending.append((slug, path))
                    continue
                raw = tags.get(PLAYLIST_TAG, "")
                self._playlist_tags[slug] = raw
                if raw:
                    found.append((slug, raw))
            for index, (slug, path) in enumerate(pending):
                await self._wait_for_tag_idle()
                entry = await one(slug, path)
                if entry is not None:
                    found.append(entry)
                if index + 1 < len(pending):
                    await asyncio.sleep(PLAYLIST_TAG_WRITE_PAUSE)
            self._media_cache.save(force=True)
            return found
        found = await asyncio.gather(
            *(one(slug, path) for slug, path in items),
            return_exceptions=True,
        )
        self._media_cache.save(force=True)
        return [entry for entry in found
                if isinstance(entry, tuple) and entry[1]]

    def _playlist_sync_slugs(self, slugs) -> list[str]:
        seen: set[str] = set()
        result: list[str] = []
        for slug in slugs or []:
            if (slug and slug not in seen and not slug.startswith(IMAGE_PREFIX)
                    and slug in self._clips):
                seen.add(slug)
                result.append(slug)
        return result

    async def _write_playlist_tag(self, slug: str, path: Path, *,
                                  throttle: bool = False) -> None:
        """Put the clip's current playlist membership into its own file."""
        payload = self.playlists.metadata_for(slug)
        known = self._playlist_tags.get(slug)
        if known is not None and _playlist_tag_matches(known, payload):
            return
        lock = self._tag_locks.setdefault(str(path), asyncio.Lock())
        async with lock:
            # Another edit may have landed while this task waited for the
            # file, so build the payload from the current store, not from the
            # snapshot taken before the lock.
            payload = self.playlists.metadata_for(slug)
            known = self._playlist_tags.get(slug)
            if known is not None and _playlist_tag_matches(known, payload):
                return
            cached_files = frozenset(_current_cache_files(path))
            old_thumb = _thumb_path(path)
            old_proxy = _proxy_path(path)
            identity = self._clip_identity(slug, path)
            old_tags = dict(self._media_cache.get_tags(path, identity) or {})
            async with self._tag_write_sem:
                ok = await write_metadata_tags(
                    path, {PLAYLIST_TAG: payload},
                    readrate=4 if throttle else None)
            if not ok:
                return
            # A delete or rename can land while the remux runs. Do not put a
            # stale clip back into the in-memory indexes.
            if self._clips.get(slug) != path:
                return
            self._remember_clip(slug, path)
            self._playlist_tags[slug] = payload
            # A remux changes the file's identity, so cached thumbnails and
            # previews would be orphaned under the old key. Carry them to the
            # key the new file version will be looked up under.
            new_thumb = _thumb_path(path)
            new_proxy = _proxy_path(path)
            for old in cached_files:
                if old == old_thumb:
                    new = new_thumb
                elif old == old_proxy:
                    new = new_proxy
                elif old.name.startswith(old_proxy.stem + "_audio_"):
                    suffix = old.name[len(old_proxy.stem):]
                    new = new_proxy.with_name(new_proxy.stem + suffix)
                else:
                    continue
                if new == old:
                    continue
                try:
                    old.replace(new)
                except OSError as exc:
                    log.debug("Could not carry %s across a metadata write: %s",
                              old.name, exc)
            old_tags[PLAYLIST_TAG] = payload
            self._media_cache.put_tags(path, old_tags)
            meta = self._meta.get(slug)
            if meta is not None:
                meta = dict(meta)
                meta["tags"] = old_tags
                new_identity = _MediaCache._identity(path)
                if new_identity is not None:
                    meta["_identity"] = new_identity
                self._meta[slug] = meta
                self._media_cache.put_meta(path, meta)
            self._media_cache.save()
            log.debug("Stored playlist metadata in %s", path.name)

    async def _sync_playlist_tags(self, slugs, budget: Optional[float] = None,
                                  *, throttle: bool = False) -> None:
        """Write membership to the given clips, optionally time-boxed.

        Small changes (adding or removing one clip) wait for the write so a
        restart immediately afterwards keeps the change. Editing a playlist
        with many members is time-boxed: the response does not wait for a
        hundred remuxes, but they continue in the background. Background
        callers pass *throttle* to keep a network library responsive.
        """
        tasks = [
            self._track_task(asyncio.create_task(
                self._write_playlist_tag(slug, self._clips[slug],
                                         throttle=throttle)))
            for slug in self._playlist_sync_slugs(slugs)
        ]
        if not tasks:
            return
        if budget is None:
            await asyncio.gather(*tasks, return_exceptions=True)
        else:
            await asyncio.wait(tasks, timeout=budget)

    def _playlist_tag_members(self) -> list[str]:
        """Clips that should carry at least one playlist definition."""
        members: set[str] = set()
        for playlist in self.playlists.list_playlists():
            for raw_slug in playlist.get("clip_slugs", []):
                slug = str(raw_slug)
                if (slug and not slug.startswith(IMAGE_PREFIX)
                        and slug in self._clips):
                    members.add(slug)
        return sorted(members)

    def _kick_playlist_tag_backfill(self) -> None:
        """Start (or queue) a background sync of playlists.json into the clips.

        The first run after upgrading from a Vice that only wrote
        playlists.json can remux the whole library, so it must not hold up
        startup or an API response. Calling this again while one is running
        asks for one more pass, which is how a rescan catches clips that
        appeared after the current pass took its snapshot.
        """
        task = self._playlist_tag_task
        if task is not None and not task.done():
            self._playlist_tag_recheck = True
            return
        self._playlist_tag_recheck = False
        self._playlist_tag_task = self._track_task(
            asyncio.create_task(self._backfill_after_startup()))

    async def _wait_for_tag_idle(self) -> None:
        """Wait until nobody is using the gallery or watching a clip.

        A backfill remux is a full read and rewrite of a video. On a network
        clip folder, doing that while somebody is browsing or watching is what
        made the app feel broken, so the worker simply gets out of the way.
        """
        while True:
            now = time.monotonic()
            quiet = now - getattr(self, "_last_activity", 0.0)
            media_quiet = now - getattr(self, "_last_media_activity", 0.0)
            if (quiet >= PLAYLIST_TAG_IDLE_SECONDS
                    and media_quiet >= PLAYLIST_TAG_MEDIA_IDLE_SECONDS):
                return
            await asyncio.sleep(2.0)

    async def _backfill_after_startup(self) -> None:
        # Let the window, the recorder and the first gallery load finish
        # before touching the clip folder.
        await asyncio.sleep(PLAYLIST_TAG_STARTUP_DELAY)
        await self._backfill_playlist_tags()

    async def _backfill_playlist_tags(self) -> None:
        """Copy playlist membership from playlists.json into its member videos.

        A valid playlists.json is the source of truth. Videos recorded before
        container tags existed have no copy of it, so they are remuxed once to
        add the tag. Videos already carrying the same definitions are skipped
        by _write_playlist_tag, which keeps later startups cheap.
        """
        while True:
            self._playlist_tag_recheck = False
            members = self._playlist_tag_members()
            if members:
                # Read the existing tags first. _clip_tags also fills
                # _playlist_tags, so files that already match are skipped
                # without being remuxed.
                await self._playlist_tag_entries(only=set(members),
                                                 throttle=True)
                needs_write = [
                    slug for slug in members
                    if not _playlist_tag_matches(
                        self._playlist_tags.get(slug, ""),
                        self.playlists.metadata_for(slug),
                    )
                ]
                if needs_write:
                    log.info("Writing playlist metadata into %d clip%s",
                             len(needs_write), "" if len(needs_write) == 1 else "s")
                    # One at a time, with a pause, and only while the app is
                    # idle. This can take a while on a big network library,
                    # which is fine: playlists.json already holds the truth.
                    for index, slug in enumerate(needs_write):
                        await self._wait_for_tag_idle()
                        await self._sync_playlist_tags([slug], throttle=True)
                        if index + 1 < len(needs_write):
                            await asyncio.sleep(PLAYLIST_TAG_WRITE_PAUSE)
                self._media_cache.save(force=True)
            if not self._playlist_tag_recheck:
                return

    async def rescan_clips(self) -> int:
        """Refresh the clip index, e.g. when a late-mounted drive appears.

        Returns the number of clips that were not in the index before. The UI
        is told to reload when anything changed, which is what makes the
        gallery recover without restarting the daemon.
        """
        async with self._rescan_lock:
            return await self._rescan_clips_locked()

    async def _rescan_clips_locked(self) -> int:
        before_paths = dict(self._clips)
        # A directory walk over a network folder can take seconds. Keep it off
        # the event loop so the UI, websocket and video streams stay live.
        found, signature, stats = await asyncio.to_thread(self._scan_library)

        # A clip can be saved, renamed or deleted while the scan runs. Keep
        # that newer in-memory change rather than letting the older snapshot
        # undo it.
        for slug, path in self._clips.items():
            if before_paths.get(slug) == path:
                continue
            info = self._clip_stats.get(slug)
            if info is None or info[0] != path:
                info = self._file_info(path)
            if info is None:
                found.pop(slug, None)
                stats.pop(slug, None)
            else:
                found[slug] = path
                stats[slug] = info
        for slug in before_paths.keys() - self._clips.keys():
            found.pop(slug, None)
            stats.pop(slug, None)

        incomplete = bool(getattr(self, "_last_scan_incomplete", False))
        if incomplete:
            # The walk could not stat every file (a busy network mount).
            # Keep what the previous index knew about the files it missed;
            # only a complete walk may conclude that a clip was deleted.
            for slug, path in before_paths.items():
                if slug in found:
                    continue
                info = self._clip_stats.get(slug)
                if info is not None and info[0] == path:
                    found[slug] = path
                    stats[slug] = info

        out_dir = resolve_path(self.cfg.output.directory)
        # A drive that is not mounted must not be mistaken for an empty
        # library: leave the existing index and playlist membership alone.
        if not found and not await asyncio.to_thread(out_dir.exists):
            return 0
        if (not incomplete
                and signature == self._library_signature
                and set(found) == set(self._clips)):
            self._clip_stats = stats
            return 0

        previous = set(self._clips)
        added = set(found) - previous
        removed = previous - set(found)
        self._clips = found
        self._clip_stats = stats
        if not incomplete:
            self._library_signature = signature
        for slug in removed:
            self._meta.pop(slug, None)

        if added:
            self._ensure_share_tokens()
            entries = await self._playlist_tag_entries(only=added)
            changed = self.playlists.merge_metadata(entries)
            # Both indexes in one call: backfill drops membership outside the
            # set it is given and screenshots share the same playlists.
            changed = self.playlists.backfill(
                set(self._clips) | {image_slug(s) for s in self._images},
                build_tag_index(self.cfg.discord.custom_games),
                seed_auto=self.cfg.output.auto_playlist_by_game,
            ) or changed
            if changed:
                await self._broadcast_playlists()
            await self.broadcast({
                "type": "library_recovered",
                "added": len(added),
                "clips": len(self._clips),
            })
            log.info("Found %d new clip%s in %s",
                     len(added), "" if len(added) == 1 else "s", out_dir)
            # Newly discovered members need their metadata written, including
            # when a drive mounted after the startup backfill already ran.
            self._kick_playlist_tag_backfill()
        elif removed:
            # An empty scan with a non-empty previous index usually means the
            # drive was unmounted under us, not that every clip was deleted.
            # Drop the stale clip cards, but do not use that scan to prune
            # playlist membership: it is restored when the drive reappears.
            if found:
                self.playlists.backfill(
                    set(self._clips) | {image_slug(s) for s in self._images},
                    build_tag_index(self.cfg.discord.custom_games),
                    seed_auto=self.cfg.output.auto_playlist_by_game,
                )
            await self.broadcast({
                "type": "library_changed",
                "added": 0,
                "clips": len(self._clips),
            })
        self._media_cache.save()
        return len(added)

    # ── lifecycle ─────────────────────────────────────────────────────────────

    async def start(self) -> None:
        # Pre-populate from output dir. The walk stats every clip, which can
        # take a while on a network mount, so keep it off the event loop.
        self._clips, self._library_signature, self._clip_stats = (
            await asyncio.to_thread(self._scan_library))
        self._media_cache.forget_temp_files()
        self._ensure_share_tokens()
        self.rescan_images()
        # When playlists.json is missing or corrupt, rebuild it from the tags
        # stored in the clips themselves. Filename backfill remains the
        # fallback for libraries recorded before tags existed.
        if not self.playlists.loaded_from_disk:
            entries = await self._playlist_tag_entries()
            self.playlists.restore_from_metadata(entries)
        # Only prune when there is a library to compare against. A drive that
        # has not mounted yet must not empty every playlist on startup.
        if self._clips or self._images:
            # Both indexes in one call. backfill drops membership for anything
            # not in the set it is given, so handing it the clips alone would
            # empty every playlist of its screenshots.
            self.playlists.backfill(
                set(self._clips) | {image_slug(s) for s in self._images},
                build_tag_index(self.cfg.discord.custom_games),
                seed_auto=self.cfg.output.auto_playlist_by_game,
            )
        elif not self.playlists.loaded_from_disk:
            # Create the file even for a genuinely empty library, so it is
            # visible that the launch scan ran.
            self.playlists.save()
        self._media_cache.save()
        # A valid playlists.json can predate container tags (or come from an
        # older Vice). Put the definitions on its member videos in the
        # background, without delaying the UI or the recorder.
        self._kick_playlist_tag_backfill()
        # View counts persist like highlights: they are only dropped when a
        # clip is deleted (in _api_delete) or its number is reused by a new
        # recording (in add_clip). They are never purged against the startup
        # scan, which wiped valid counts when the output dir was slow to mount.

        local_port = self.cfg.sharing.port
        public_port = self.cfg.sharing.public_port or (local_port + 1)
        public_host = _local_ip()

        self._local_runner = web.AppRunner(self._local_app, access_log=None)
        await self._local_runner.setup()
        self._local_site = web.TCPSite(self._local_runner, "127.0.0.1", local_port)
        await self._local_site.start()

        self._public_runner = web.AppRunner(self._public_app, access_log=None)
        await self._public_runner.setup()
        self._public_site = web.TCPSite(self._public_runner, "0.0.0.0", public_port)
        await self._public_site.start()

        self._local_base_url = f"http://127.0.0.1:{local_port}"
        self._public_bind_url = f"http://{public_host}:{public_port}"
        log.info("Vice local control UI: %s", self._local_base_url)
        log.info("Vice public share server: %s", self._public_bind_url)
        if self.cfg.sharing.base_url:
            log.info("Vice public base URL override: %s", self.cfg.sharing.base_url)
        elif public_host not in {"127.0.0.1", "0.0.0.0"}:
            try:
                self._legacy_public_site = web.TCPSite(self._public_runner, public_host, local_port)
                await self._legacy_public_site.start()
                self._legacy_public_bind_url = f"http://{public_host}:{local_port}"
                log.info(
                    "Vice legacy share compatibility URL: %s",
                    self._legacy_public_bind_url,
                )
            except OSError as exc:
                log.warning(
                    "Failed to enable legacy share compatibility on %s:%d: %s",
                    public_host,
                    local_port,
                    exc,
                )

        if self.cfg.sharing.cloudflare_tunnel:
            await self._start_tunnel(public_port)

    async def stop(self) -> None:
        self._proxy_stopping = True
        tasks = list(self._proxy_tasks)
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        # Tests build minimal servers with __new__, so stay tolerant of the
        # metadata fields not being initialised.
        background = list(getattr(self, "_background_tasks", ()))
        for task in background:
            task.cancel()
        await asyncio.gather(*background, return_exceptions=True)
        cache = getattr(self, "_media_cache", None)
        if cache is not None:
            cache.save(force=True)
        await self._exports.stop()
        for ws in list(self._ws_clients):
            try:
                await ws.close()
            except Exception as exc:
                log.debug("A websocket client did not close cleanly: %s", exc)
        self._tunnel_stopping = True
        tunnel_task = getattr(self, "_tunnel_task", None)
        if tunnel_task:
            tunnel_task.cancel()
            await asyncio.gather(tunnel_task, return_exceptions=True)
            self._tunnel_task = None
        elif getattr(self, "_tunnel_proc", None):
            await self._stop_tunnel_process(self._tunnel_proc)
        if self._local_runner:
            await self._local_runner.cleanup()
        if self._public_runner:
            await self._public_runner.cleanup()

    # ── public helpers (called by ViceDaemon) ─────────────────────────────────

    def add_clip(self, path: Path, game: Optional[str] = None) -> str:
        """Register a new clip and return its share URL."""
        slug = path.stem
        self._clips[slug] = path
        self._remember_clip(slug, path)
        self._meta.pop(slug, None)
        # A reused clip number has a different file; do not let the old file's
        # playlist tag make the sync below believe the new one is up to date.
        self._playlist_tags.pop(slug, None)
        # A fresh recording under a reused clip number must not inherit the
        # old clip's view count, nor its link: anyone still holding the old
        # one would be shown the new recording.
        if self._views.pop(slug, None) is not None:
            _save_views(self._views)
        self._issue_share_token(slug)
        if (game and self.cfg.output.auto_playlist_by_game
                and self.playlists.record_auto(game, slug)):
            asyncio.create_task(self._broadcast_playlists())
            self._track_task(asyncio.create_task(
                self._sync_playlist_tags([slug], throttle=True)))
        asyncio.create_task(self.broadcast({
            "type": "clip_saved",
            "clip": self._clip_json(slug, path, {}),
        }))
        asyncio.create_task(self._broadcast_clip(slug, path))
        return self.share_url(slug)

    # ── share tokens ──────────────────────────────────────────────────────────

    def _new_share_token(self) -> str:
        token = secrets.token_urlsafe(9)
        while token in self._share_slugs:
            token = secrets.token_urlsafe(9)
        return token

    def _ensure_share_tokens(self) -> None:
        """Give every clip in the library a token, in one write.

        Tokens for clips that are missing are kept, not pruned: a clips folder
        on a drive that is not mounted yet would otherwise lose every link.
        A recording that reuses a clip number gets a fresh token in add_clip,
        which is the case that matters.
        """
        missing = [slug for slug in self._clips if slug not in self._share_tokens]
        for slug in missing:
            token = self._new_share_token()
            self._share_tokens[slug] = token
            self._share_slugs[token] = slug
        if missing:
            _save_share_tokens(self._share_tokens)

    def _issue_share_token(self, slug: str) -> str:
        """Give *slug* a new token, retiring any link it had before."""
        old = self._share_tokens.get(slug)
        if old is not None:
            self._share_slugs.pop(old, None)
        token = self._new_share_token()
        self._share_tokens[slug] = token
        self._share_slugs[token] = slug
        _save_share_tokens(self._share_tokens)
        return token

    def _move_share_token(self, slug: str, new_slug: str) -> None:
        """Keep a shared link working when its clip is renamed."""
        token = self._share_tokens.pop(slug, None)
        if token is None:
            return
        stale = self._share_tokens.get(new_slug)
        if stale is not None:
            self._share_slugs.pop(stale, None)
        self._share_tokens[new_slug] = token
        self._share_slugs[token] = new_slug
        _save_share_tokens(self._share_tokens)

    def _forget_share_token(self, slug: str) -> None:
        token = self._share_tokens.pop(slug, None)
        if token is not None:
            self._share_slugs.pop(token, None)
            _save_share_tokens(self._share_tokens)

    def share_url(self, slug: str) -> str:
        base = self.public_base_url() or self.local_base_url() or ""
        token = self._share_tokens.get(slug) or self._issue_share_token(slug)
        return f"{base}/c/{token}"

    def _slug_for_ref(self, ref: str, *, allow_slug: bool) -> Optional[str]:
        """The clip a URL segment names, or None.

        The public server resolves tokens only. The local server keeps
        accepting slugs, which is what the app itself uses, and also takes
        tokens so a share link that fell back to the local address still
        opens on this machine.
        """
        if allow_slug and ref in self._clips:
            return ref
        return self._share_slugs.get(ref)

    def local_base_url(self) -> Optional[str]:
        return self._local_base_url

    def public_base_url(self) -> Optional[str]:
        if self.cfg.sharing.base_url:
            return self.cfg.sharing.base_url.rstrip("/")
        return (self._tunnel_url or self._public_bind_url or "").rstrip("/") or None

    def public_is_reachable(self) -> bool:
        """Whether share links work outside the local network. False means we
        fell back to a LAN address because there is no tunnel (#105)."""
        return bool(self.cfg.sharing.base_url or self._tunnel_url)

    def _share_links_update(self) -> dict:
        return {
            "type": "share_links_changed",
            "links": {slug: self.share_url(slug) for slug in self._clips},
            "share_is_public": self.public_is_reachable(),
        }

    async def _broadcast_share_links(self) -> None:
        if self._clips:
            await self.broadcast(self._share_links_update())

    async def broadcast(self, msg: dict) -> None:
        if not self._ws_clients:
            return
        text = json.dumps(msg)
        dead: set[web.WebSocketResponse] = set()
        for ws in self._ws_clients:
            try:
                await ws.send_str(text)
            except Exception:
                dead.add(ws)
        self._ws_clients -= dead

    # ── internal broadcast helpers ────────────────────────────────────────────

    async def _broadcast_playlists(self) -> None:
        await self.broadcast({
            "type": "playlists_changed",
            "playlists": self.playlists.list_playlists(),
        })

    async def refresh_clip(self, slug: str) -> None:
        """Tell the UI a clip's details changed."""
        path = self._clips.get(slug)
        if path is not None and path.exists():
            await self._broadcast_clip(slug, path)

    async def _api_get_keys(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        if slug not in self._clips:
            raise web.HTTPNotFound()
        return web.json_response({"keys": keystore.load_sidecar(slug)})

    async def _api_install_gsr(self, _: web.Request) -> web.Response:
        """Install the gpu-screen-recorder Flatpak. Progress arrives as
        gsr_install events over the WebSocket."""
        if self.install_gsr_cb is None or not self.install_gsr_cb():
            return web.json_response(
                {"ok": False, "error": "Only the Vice Flatpak installs gpu-screen-recorder itself."},
                status=409)
        return web.json_response({"ok": True})

    async def _api_overlay_move(self, req: web.Request) -> web.Response:
        body = await req.json()
        if self.key_overlay_move_cb is None:
            return web.json_response({"ok": False})
        self.key_overlay_move_cb(bool(body.get("on")))
        return web.json_response({"ok": True})

    async def _broadcast_clip(self, slug: str, path: Path) -> None:
        meta = await self._get_meta(slug, path)
        # A rename or delete can land while the probe runs, as when a trim
        # saves and the dialog applies its new name straight after. That path
        # is gone by now, so announcing it would put an unreadable copy of
        # the old name back in the gallery (#246).
        if self._clips.get(slug) != path or not path.exists():
            self._meta.pop(slug, None)
            return
        await self.broadcast({"type": "clip_saved", "clip": self._clip_json(slug, path, meta)})

    async def _get_meta(self, slug: str, path: Path) -> dict:
        identity = self._clip_identity(slug, path)
        meta = self._cached_meta(slug, path, identity)
        if meta is not None:
            return meta

        # Several routes ask for the same cold clip at once (the list, its
        # thumbnail and an open player). Without this only one of them runs
        # ffprobe; the rest wait and then take the cached result.
        lock = self._meta_locks.setdefault(slug, asyncio.Lock())
        async with lock:
            identity = self._clip_identity(slug, path)
            meta = self._cached_meta(slug, path, identity)
            if meta is not None:
                return meta
            async with self._probe_sem:
                meta = await _ffprobe(path)
            meta = dict(meta)
            if meta.pop("_transient", False):
                # A timeout over a busy network mount is not a property of the
                # file. Do not cache it, so the next request can try again.
                return meta
            # A moov repair can rewrite the file under the same slug, so take
            # the identity again before caching the result against it.
            info = self._file_info(path)
            if info is not None:
                self._clip_stats[slug] = info
                identity = [info[1], info[2]]
            if identity is not None:
                meta["_identity"] = identity
            self._meta[slug] = meta
            self._media_cache.put_meta(path, meta)
            tags = meta.get("tags")
            if isinstance(tags, dict):
                self._playlist_tags[slug] = tags.get(PLAYLIST_TAG, "")
                self._media_cache.put_tags(path, tags)
            self._media_cache.save()
            return meta

    def _clip_json(self, slug: str, path: Path, meta: dict) -> dict:
        info = getattr(self, "_clip_stats", {}).get(slug)
        if info is None or info[0] != path:
            info = self._clip_info(slug, path)
        if info is not None:
            _, size, mtime_ns, mtime = info
            created_at = datetime.fromtimestamp(mtime).isoformat()
        else:
            size, mtime_ns, created_at = 0, 0, ""

        # The slug is a filename, so it can hold spaces and punctuation that
        # would truncate or corrupt a URL (#138). Encode it in every link.
        enc = quote(slug, safe="")
        thumb_rev = f"{size}-{mtime_ns}"
        # Always advertise the thumbnail URL. The /t route makes it lazily,
        # so the grid paints immediately and only the visible cards pay for
        # an ffmpeg thumbnail instead of the list waiting for all of them.
        thumb_url = f"/t/{enc}?v={thumb_rev}" if size else None
        return {
            "slug":       slug,
            "name":       path.name,
            "size":       size,
            "created_at": created_at,
            "game":       self.playlists.game_for(slug),
            "views":      self._views.get(slug, 0),
            # Keys were kept beside this clip for the viewer's overlay.
            "keys":       keystore.sidecar_path(slug).exists(),
            "duration":   meta.get("duration", 0),
            "width":      meta.get("width",    0),
            "height":     meta.get("height",   0),
            # Lets the UI request an H.264 preview proxy for codecs the native
            # WebEngine can't decode (H.265).
            "vcodec":     meta.get("vcodec",   ""),
            "audio_tracks": meta.get("audio_tracks", []),
            # ffmpeg cannot read this file. It is still listed, because it is
            # the user's recording and may be recoverable by hand, but the
            # card says so instead of showing a 0:00 clip that will not play.
            "unreadable": bool(meta.get("unreadable")),
            "unreadable_reason": meta.get("unreadable_reason", ""),
            # Keep share links public, but serve media via local relative URLs
            # so the app UI never fetches video through an external tunnel.
            "share_url":  self.share_url(slug),
            "share_is_public": self.public_is_reachable(),
            # Cache-bust media URLs by clip file identity: deleted clip numbers
            # get reused (Vice_Clip_5 can name a brand-new file), and a trim
            # rewrites the file under the same slug, without the version the
            # browser may play a cached older video for the clip it shows.
            "video_url":  f"/v/{enc}?v={thumb_rev}",
            "thumb_url":  thumb_url,
        }

    # ── images ────────────────────────────────────────────────────────────────

    def _image_dir(self) -> Path:
        return resolve_path(
            getattr(self.cfg.output, "image_directory", "")
            or str(actual_home_dir() / "Pictures" / "Vice")
        )

    def next_image_path(self, tag: Optional[str] = None) -> Path:
        """Where the next screenshot should be written."""
        out_dir = self._image_dir()
        out_dir.mkdir(parents=True, exist_ok=True)
        return next_image_path(out_dir, tag)

    def add_image(self, path: Path, game: Optional[str] = None) -> str:
        """Register a new screenshot and return its slug."""
        slug = path.stem
        self._images[slug] = path
        if (game and self.cfg.output.auto_playlist_by_game
                and self.playlists.record_auto(game, image_slug(slug))):
            asyncio.create_task(self._broadcast_playlists())
        asyncio.create_task(self._broadcast_image(slug, path))
        return slug

    def image_count(self) -> int:
        return len(self._images)

    def rescan_images(self) -> None:
        """Rebuild the image index from whatever the image directory holds now.

        Playlist membership is deliberately left alone: pointing Vice at a
        second folder and back must not throw away which playlists the first
        folder's screenshots were in.
        """
        found: dict[str, Path] = {}
        img_dir = self._image_dir()
        if img_dir.exists():
            stills = [p for ext in IMAGE_EXTS for p in img_dir.glob(f"*.{ext}")]
            for shot in sorted(stills, key=lambda p: p.stat().st_mtime):
                found[shot.stem] = shot
        self._images = found

    async def _broadcast_image(self, slug: str, path: Path) -> None:
        # The URL is always advertised and /it makes the JPEG on demand, so a
        # save notification never waits for an ffmpeg run.
        await self.broadcast({"type": "image_saved", "image": await self._image_json(slug, path)})

    async def _image_json(self, slug: str, path: Path) -> dict:
        try:
            st = path.stat()
            size = st.st_size
            mtime_ns = st.st_mtime_ns
            created_at = datetime.fromtimestamp(st.st_mtime).isoformat()
        except OSError:
            size, mtime_ns, created_at = 0, 0, ""

        # probe_media, never _ffprobe: _ffprobe reads a duration of zero as a
        # damaged container and goes off to attempt a moov remux, which means
        # nothing for a still and would rewrite the user's screenshot.
        width = height = 0
        try:
            probed = self._media_cache.get_meta(path)
            if probed is None:
                async with self._probe_sem:
                    probed = await probe_media(path)
                if probed is not None:
                    self._media_cache.put_meta(path, probed)
            probed = probed or {}
            width = int(probed.get("width") or 0)
            height = int(probed.get("height") or 0)
        except Exception as exc:
            log.debug("Could not read the dimensions of %s: %s", path.name, exc)

        enc = quote(slug, safe="")
        rev = f"{size}-{mtime_ns}"
        return {
            "slug":       slug,
            "name":       path.name,
            "size":       size,
            "created_at": created_at,
            "game":       self.playlists.game_for(image_slug(slug)),
            "width":      width,
            "height":     height,
            # Annotating rewrites the file under the same slug, so both URLs
            # carry the file's identity or the window shows the pre-edit copy.
            # The thumbnail is made on first request, not while listing.
            "image_url":  f"/i/{enc}?v={rev}",
            "thumb_url":  f"/it/{enc}?v={rev}" if size else None,
        }

    # ── route handlers ────────────────────────────────────────────────────────

    async def _ui(self, _: web.Request) -> web.Response:
        ui_index = _resolve_ui_index()
        if not ui_index:
            log.error("Vice UI not found (missing vice/ui/index.html)")
            return web.Response(
                text="<h1>Vice UI not found</h1><p>Reinstall Vice from this checkout or AUR package.</p>",
                content_type="text/html",
                status=500,
            )

        try:
            content = ui_index.read_text(encoding="utf-8")
            # Asset URLs are versioned by content, the visible version string
            # stays the plain version.
            content = content.replace(f"?v={UI_VERSION_TOKEN}", f"?v={_ui_asset_rev()}")
            content = content.replace(UI_VERSION_TOKEN, __version__)
            return web.Response(
                text=content,
                content_type="text/html",
                # The page itself must never be cached, or a stale copy keeps
                # pointing at the previous build's assets.
                headers={"Cache-Control": "no-store"},
            )
        except Exception as exc:
            log.error("Failed reading UI file %s: %s", ui_index, exc)
            return web.Response(
                text="<h1>Vice UI failed to load</h1><p>Check vice logs for details.</p>",
                content_type="text/html",
                status=500,
            )

    async def _ui_asset(self, req: web.Request, *, kind: str) -> web.Response:
        path = _resolve_ui_asset(kind, req.match_info["name"])
        if not path:
            raise web.HTTPNotFound()
        return web.FileResponse(
            path,
            headers={
                "Content-Type": _UI_CONTENT_TYPES[kind],
                "Cache-Control": "public, max-age=31536000, immutable",
            },
        )

    async def _embed_page(self, req: web.Request) -> web.Response:
        return await self._serve_embed_page(req, public=False)

    async def _public_embed_page(self, req: web.Request) -> web.Response:
        return await self._serve_embed_page(req, public=True)

    def _clip_for_ref(self, ref: str, *, public: bool) -> tuple[str, Path]:
        slug = self._slug_for_ref(ref, allow_slug=not public)
        path = self._clips.get(slug) if slug else None
        if not path or not path.exists():
            raise web.HTTPNotFound()
        return slug, path

    async def _serve_embed_page(self, req: web.Request, *, public: bool) -> web.Response:
        ref = req.match_info["ref"]
        slug, path = self._clip_for_ref(ref, public=public)
        meta = await self._get_meta(slug, path)
        # cloudflared terminates TLS and forwards plain HTTP, so req.scheme
        # is "http" even when the visitor came in over https. Discord and
        # other scrapers reject non-https og:video URLs, which breaks
        # embeds for tunnel links (issue #100).
        scheme = req.headers.get("X-Forwarded-Proto", req.scheme)
        base = f"{scheme}://{req.host}"
        # Direct file URL with the real container suffix; some unfurlers
        # sniff the extension. _video strips it back off.
        suffix = path.suffix.lower() or ".mp4"
        # The media links reuse whatever named the page, so a page reached by
        # token never hands out the filename that would lead to other clips.
        enc = quote(ref, safe="")
        page = _EMBED_PAGE.format(
            title=html.escape(f"Vice clip, {slug}", quote=True),
            page_url=f"{base}/c/{enc}",
            video_url=f"{base}/v/{enc}{suffix}",
            video_type="video/x-matroska" if suffix == ".mkv" else "video/mp4",
            thumb_url=f"{base}/t/{enc}",
            width=meta.get("width", 1920),
            height=meta.get("height", 1080),
            color=self._embed_color(),
        )
        return web.Response(text=page, content_type="text/html")

    def _embed_color(self) -> str:
        """Validated embed accent color (guards against HTML injection)."""
        color = getattr(self.cfg.sharing, "embed_color", "") or ""
        if re.fullmatch(r"#[0-9a-fA-F]{6}", color):
            return color
        return "#0099ff"

    async def _video(self, req: web.Request) -> web.Response:
        return await self._serve_video(req, public=False)

    async def _public_video(self, req: web.Request) -> web.Response:
        return await self._serve_video(req, public=True)

    async def _serve_video(self, req: web.Request, *, public: bool) -> web.Response:
        ref = req.match_info["ref"]
        slug = self._slug_for_ref(ref, allow_slug=not public)
        if slug is None and ref.lower().endswith((".mp4", ".mkv")):
            # Embed pages link the file with its container suffix. Exact
            # match first so slugs that themselves contain dots keep working.
            slug = self._slug_for_ref(ref.rsplit(".", 1)[0], allow_slug=not public)
        path = self._clips.get(slug) if slug else None
        if not path or not await asyncio.to_thread(path.exists):
            raise web.HTTPNotFound()

        # The UI asks for proxy=1 when the clip's codec (H.265) can't play in
        # the native WebEngine. Serve a cached H.264 copy instead; the original
        # is never touched. Falls through to the source if it's already
        # web-playable or the transcode fails. Local only: over a share link
        # it would let a stranger make this machine transcode on request.
        if not public and req.query.get("proxy") == "1":
            served = await self._serve_preview_proxy(slug, path)
            if served is not None:
                return served

        # no-cache = revalidate before reuse. Slugs are not stable identities
        # (clip numbers get reused after deletes, trims rewrite in place), so
        # a cached response may belong to a different video than the slug
        # currently names.
        content_type = ("video/x-matroska" if path.suffix.lower() == ".mkv"
                        else "video/mp4")
        return self._video_response(path, content_type=content_type)

    async def _serve_preview_proxy(self, slug: str, path: Path):
        """Return a FileResponse for the clip's H.264 preview proxy, or None to
        fall back to serving the original."""
        if self._proxy_stopping:
            raise web.HTTPServiceUnavailable()
        task = asyncio.current_task()
        self._proxy_tasks.add(task)
        try:
            proxy = _proxy_path(path)
            if not proxy.exists() or proxy.stat().st_size == 0:
                async with self._proxy_lock:
                    meta = await self._get_meta(slug, path)
                    proxy = await _make_preview_proxy(path, meta.get("vcodec", ""))
        finally:
            self._proxy_tasks.discard(task)
        if proxy is None or not proxy.exists():
            return None
        return self._video_response(proxy, content_type="video/mp4")

    async def _prepare_discord_copy(self, slug: str, path: Path) -> Optional[Path]:
        """The clip's Discord-sized file, transcoding it once if needed.

        Raises DiscordTooLarge when the clip cannot fit the cap at all.
        """
        if self._proxy_stopping:
            raise web.HTTPServiceUnavailable()
        task = asyncio.current_task()
        self._proxy_tasks.add(task)
        try:
            # A drag that is already encoded must not queue behind someone
            # else's encode: the whole library shares one encoder lock.
            cached = _discord_path(path)
            if cached.exists() and 0 < cached.stat().st_size <= DISCORD_MAX_BYTES:
                return cached
            async with self._proxy_lock:
                meta = await self._get_meta(slug, path)
                return await _make_discord_copy(
                    path,
                    meta.get("duration", 0),
                    meta.get("audio_streams", 1),
                    meta.get("vcodec", ""),
                )
        finally:
            self._proxy_tasks.discard(task)

    async def _audio_track(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        raw_index = req.match_info["index"]
        if not path or not path.exists() or not re.fullmatch(r"[0-9]{1,4}", raw_index):
            raise web.HTTPNotFound()
        index = int(raw_index)
        meta = await self._get_meta(slug, path)
        if index >= meta.get("audio_streams", 0):
            raise web.HTTPNotFound()
        if self._proxy_stopping:
            raise web.HTTPServiceUnavailable()
        task = asyncio.current_task()
        self._proxy_tasks.add(task)
        try:
            target = _audio_preview_path(path, index)
            if not target.exists() or target.stat().st_size == 0:
                async with self._proxy_lock:
                    target = await _make_audio_preview(path, index)
        except (OSError, RuntimeError, asyncio.TimeoutError) as exc:
            log.warning("Audio preview failed for %s track %d: %s", slug, index, exc)
            raise web.HTTPServiceUnavailable(text="Could not prepare this audio track") from exc
        finally:
            self._proxy_tasks.discard(task)
        return web.FileResponse(target, headers={
            "Content-Type": "audio/mp4", "Accept-Ranges": "bytes", "Cache-Control": "no-cache",
        })

    async def _thumb(self, req: web.Request) -> web.Response:
        return await self._serve_thumb(req, public=False)

    async def _public_thumb(self, req: web.Request) -> web.Response:
        return await self._serve_thumb(req, public=True)

    async def _serve_thumb(self, req: web.Request, *, public: bool) -> web.Response:
        slug = self._slug_for_ref(req.match_info["ref"], allow_slug=not public)
        path = self._clips.get(slug) if slug else None
        if path is None:
            raise web.HTTPNotFound()
        # Answer from the local thumbnail cache first, using the identity the
        # scan already recorded. The clip folder is not touched here: stats on
        # a busy network mount block the event loop, which is what used to
        # delay a player behind a grid of thumbnails.
        width = 240 if req.query.get("w") == "240" else 640
        thumb = self._known_thumb_path(slug, path)
        if thumb is not None and width != 640:
            thumb = thumb.with_name(f"{thumb.stem}_{width}.jpg")
        if thumb is not None and thumb.exists() and thumb.stat().st_size > 0:
            return web.FileResponse(thumb, headers={"Content-Type": "image/jpeg"})
        # The grid asks for a dozen of these at once, but a video opening at
        # the same time comes first. Answer without waiting so the browser can
        # reuse the connection for the player; the card asks again later.
        if self._media_busy() or not await self._take_thumb_slot():
            raise web.HTTPNotFound()
        try:
            if self._media_busy():
                raise web.HTTPNotFound()
            if thumb is None:
                thumb = _thumb_path(path)
                if width != 640:
                    thumb = thumb.with_name(f"{thumb.stem}_{width}.jpg")
            meta = await self._get_meta(slug, path)
            t = await _make_thumb(path, duration=meta.get("duration", 0),
                                  thumb=thumb, width=width)
        finally:
            self._thumb_sem.release()
        if not t.exists() or t.stat().st_size == 0:
            raise web.HTTPNotFound()
        return web.FileResponse(t, headers={"Content-Type": "image/jpeg"})

    async def _take_thumb_slot(self) -> bool:
        """Take a free thumbnail slot without holding a request open for one."""
        try:
            await asyncio.wait_for(self._thumb_sem.acquire(),
                                   THUMB_SLOT_WAIT_SECONDS)
            return True
        except asyncio.TimeoutError:
            return False

    # ── REST handlers ─────────────────────────────────────────────────────────

    async def _probe_for_list(self, slug: str, path: Path) -> dict:
        """Finish a cold probe after the list response has already gone out."""
        # The clip can be deleted or renamed while its probe waits for a slot.
        # Probing a file that is already gone only produces a failed ffprobe
        # and a pointless remux attempt. Compare by path value: a rescan
        # rebuilds the Path objects without changing which clip they name.
        if self._clips.get(slug) != path:
            return {}
        meta = await self._get_meta(slug, path)
        if self._clips.get(slug) is path and path.exists():
            await self.broadcast({
                "type": "clip_saved",
                "clip": self._clip_json(slug, path, meta),
            })
        return meta

    def _cached_meta(self, slug: str, path: Path,
                     identity: Optional[list[int]] = None) -> Optional[dict]:
        if identity is None:
            identity = self._clip_identity(slug, path)
        current = self._meta.get(slug)
        if isinstance(current, dict):
            known = current.get("_identity")
            if known is None or identity is None or known == identity:
                return current
            self._meta.pop(slug, None)
            self._playlist_tags.pop(slug, None)
        cached = self._media_cache.get_meta(path, identity)
        if cached is not None:
            cached["_identity"] = identity
            self._meta[slug] = cached
            tags = cached.get("tags")
            if isinstance(tags, dict):
                self._playlist_tags[slug] = tags.get(PLAYLIST_TAG, "")
        return cached

    async def _api_clips(self, _: web.Request) -> web.Response:
        # Everything here comes from the last directory scan. Stats over a
        # CIFS/NFS mount are not free, and doing one per clip on every list
        # request used to stall the whole daemon while the grid reloaded.
        items: list[tuple[str, Path, tuple[Path, int, int, float]]] = []
        for slug, path in self._clips.items():
            info = self._clip_info(slug, path)
            if info is not None:
                items.append((slug, path, info))
        items.sort(key=lambda item: item[2][3], reverse=True)
        metas: dict[str, dict] = {}
        for slug, path, info in items:
            cached = self._cached_meta(slug, path, [info[1], info[2]])
            if cached is not None:
                metas[slug] = cached
                continue
            metas[slug] = {}
            # A cold probe keeps running in the background and announces its
            # result over the WebSocket. The list must not wait for it: on a
            # network library one slow ffprobe used to hold up the whole grid
            # for 1.5 seconds on every request, and a poll during that time
            # started duplicate probes for the same clips.
            known = self._list_probes.get(slug)
            if known is not None and known[0] == path and not known[1].done():
                continue
            task = self._track_task(
                asyncio.create_task(self._probe_for_list(slug, path)))
            self._list_probes[slug] = (path, task)
            task.add_done_callback(
                lambda done, slug=slug: self._forget_list_probe(slug, done))

        result = [self._clip_json(slug, path, metas[slug])
                  for slug, path, _ in items]
        return web.json_response({"clips": result})

    def _forget_list_probe(self, slug: str, task: asyncio.Task) -> None:
        known = self._list_probes.get(slug)
        if known is not None and known[1] is task:
            self._list_probes.pop(slug, None)

    async def _api_rescan_library(self, _: web.Request) -> web.Response:
        """Recheck the clip folder on demand, e.g. after mounting a drive."""
        added = await self.rescan_clips()
        # rescan_clips already starts the metadata sync when it found clips.
        # A manual recheck must also repair existing clips that predate the
        # playlist tags even when no file was added or removed.
        if not added:
            self._kick_playlist_tag_backfill()
        out_dir = resolve_path(self.cfg.output.directory)
        return web.json_response({
            "ok": True,
            "added": added,
            "clips": len(self._clips),
            "directory": str(out_dir),
            "available": await asyncio.to_thread(out_dir.exists),
        })

    async def _api_clip_info(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()
        meta = await self._get_meta(slug, path)
        return web.json_response(self._clip_json(slug, path, meta))

    def _delete_clip_files(self, path: Path) -> None:
        """Remove a clip and its Discord copies off the event loop."""
        _purge_clip_discord(path)
        try:
            path.unlink(missing_ok=True)
        except OSError as exc:
            log.warning("Could not delete %s: %s", path, exc)

    async def _api_delete(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.pop(slug, None)
        self._clip_stats.pop(slug, None)
        _purge_slug_thumbs(slug)
        _purge_slug_proxies(slug)
        (HIGHLIGHTS_DIR / f"{slug}.json").unlink(missing_ok=True)
        keystore.delete_sidecar(slug)
        self._meta.pop(slug, None)
        self._playlist_tags.pop(slug, None)
        if path is not None:
            self._media_cache.forget(path)
            # Unlink can block for seconds on a network folder. Take the card
            # away immediately and let the filesystem catch up in the
            # background, so deleting a clip does not freeze the gallery.
            self._track_task(asyncio.create_task(
                asyncio.to_thread(self._delete_clip_files, path)))
        if self._views.pop(slug, None) is not None:
            _save_views(self._views)
        self._forget_share_token(slug)
        if self.playlists.on_clip_deleted(slug):
            await self._broadcast_playlists()
        if self.editor_project.on_clip_deleted(slug):
            await self.broadcast({"type": "editor_project_changed"})
        await self.broadcast({"type": "clip_deleted", "slug": slug})
        return web.json_response({"ok": True})

    async def _api_trim(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()

        body  = await req.json()
        start = float(body.get("start", 0))
        end   = float(body.get("end",   0))
        # A copy keeps the original. Replacing it lost the rest of a session
        # recording for someone who expected Save trim to make a new clip (#255).
        as_copy = bool(body.get("copy"))
        if end <= start:
            return web.json_response({"ok": False, "error": "end must be after start"})

        ext = path.suffix.lstrip(".") or "mp4"
        tmp = path.with_suffix(f".trimming.{ext}")
        faststart = ["-movflags", "+faststart"] if ext == "mp4" else []
        source_codec = (await self._get_meta(slug, path)).get("vcodec", "")

        def _trim_cmd(encoder: Optional[str], bitrate: Optional[int] = None) -> list[str]:
            """Stream copy when *encoder* is None, otherwise re-encode the video
            with it. Audio is copied either way, so every recorded track
            survives at its original quality.

            A re-encode starts the video exactly on the in-point, but copied
            audio still reaches back to the keyframe before it, as packets
            marked for discard under a leading edit list. When that lead-in is
            not a whole number of audio packets, Chromium before about 107
            refuses the file with "unfixable negative timestamp", which is the
            QtWebEngine 6.4 that Ubuntu 24.04 and Pop!_OS ship (#246). The
            output-side -ss 0 drops those packets instead.
            """
            if encoder is None:
                pre, out, drop_lead_in = [], ["-c", "copy"], []
            else:
                pre, out = _trim_encoder_args(encoder, bitrate)
                drop_lead_in = ["-ss", "0"]
            return [
                "ffmpeg", "-hide_banner", "-loglevel", "error", *pre,
                "-ss", str(start), "-i", str(path), *drop_lead_in,
                "-t",  str(end - start),
                *KEEP_ALL_STREAMS,
                *out,
                *faststart,
                "-y",  str(tmp),
            ]

        async def _run(cmd: list[str], timeout: int) -> tuple[bool, str]:
            try:
                proc = await asyncio.create_subprocess_exec(
                    *cmd,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.PIPE,
                )
                _, stderr = await communicate_with_timeout(proc, timeout=timeout)
                return proc.returncode == 0, (stderr or b"").decode()[:300]
            except asyncio.TimeoutError:
                return False, "ffmpeg timed out"

        # Copy first: it is instant and lossless, and a cut that lands on a
        # keyframe needs nothing else. When the copy would not play, re-encode
        # into the clip's own codec, which is the only way to give the exact
        # frame the user asked for. Seeking to the previous keyframe instead
        # would play, but it would silently hand back a clip that starts before
        # the chosen in-point (#172).
        ok, err = await _run(_trim_cmd(None), 120)
        if ok:
            problem = await _trim_result_problem(tmp)
            if problem:
                log.info("Copy trim of %s %s, re-encoding", path.name, problem)
        else:
            problem = "could not be stream copied"
            log.warning("Copy trim of %s failed, re-encoding: %s", path.name, err)

        if problem:
            candidates = await asyncio.to_thread(_trim_encoder_candidates, source_codec)
            bitrate = await _video_bitrate(path)
            for encoder in candidates:
                ok, err = await _run(_trim_cmd(encoder, bitrate), 900)
                if not ok:
                    log.info("Trim of %s with %s failed: %s", path.name, encoder, err)
                    continue
                problem = await _trim_result_problem(tmp)
                if problem:
                    err = f"the trimmed clip {problem}"
                    log.warning("Trim of %s with %s %s", path.name, encoder, problem)
                    continue
                _remember_trim_encoder(source_codec, encoder)
                log.info("Trimmed %s with %s", path.name, encoder)
                break

        # Never replace the user's recording with a file Vice has not decoded.
        # Failing here leaves the original on disk and says why, which beats
        # handing back a clip that opens black and explains nothing (#172).
        if problem or not ok or not tmp.exists():
            tmp.unlink(missing_ok=True)
            return web.json_response(
                {"ok": False, "error": err or problem or "trim failed"})

        kept_keys = keystore.load_sidecar(slug)
        if as_copy:
            target = _trimmed_copy_path(path)
            tmp.replace(target)
            if kept_keys is not None:
                keystore.save_sidecar(target.stem, keystore.cut_intervals(kept_keys, start, end))
            self.add_clip(target, self.playlists.game_for(slug))
            return web.json_response({"ok": True, "slug": target.stem, "copy": True})

        tmp.replace(path)
        self._remember_clip(slug, path)
        if kept_keys is not None:
            keystore.save_sidecar(slug, keystore.cut_intervals(kept_keys, start, end))
        # Clear cached thumbnail and metadata so they regenerate on next access
        _purge_slug_thumbs(slug)
        _purge_slug_proxies(slug)
        _purge_clip_discord(path)
        self._meta.pop(slug, None)
        self._media_cache.forget(path)
        # ffmpeg normally copies container tags across a trim, but do not trust
        # it with membership that cannot be regenerated from the filename.
        # Read the tag the new file actually has, then rewrite it only when
        # ffmpeg dropped or outdated it. Waiting here means the response
        # leaves a replacement that already carries its playlist metadata.
        if any(slug in p.get("clip_slugs", [])
               for p in self.playlists.list_playlists()):
            await self._clip_tags(slug, path)
            await self._sync_playlist_tags([slug])
        asyncio.create_task(self._broadcast_clip(slug, path))
        return web.json_response({"ok": True, "slug": slug})

    async def _api_rename(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()

        body     = await req.json()
        requested = str(body.get("name") or "").strip()
        if not requested:
            return web.json_response({"ok": False, "error": "name is required"})

        # Spaces and punctuation are normalised away rather than rejected, so
        # "Insane wallbang" saves as Insane-wallbang.mp4. Keep the clip's own
        # container.
        ext = path.suffix.lower() or ".mp4"
        stem = slugify_clip_name(requested)
        if not stem:
            return web.json_response({"ok": False, "error": "that name will not work as a file"})

        new_path = path.parent / f"{stem}{ext}"
        if new_path.exists() and new_path != path:
            return web.json_response({"ok": False, "error": "A clip with that name already exists"})

        # A rename keeps the file's size and modification time, so the cached
        # thumbnail and previews are still valid. Purging them made an H.265
        # clip transcode its preview again after every rename (#226).
        cached = frozenset(_current_cache_files(path))
        old_stem = path.stem
        old_meta = self._meta.pop(slug, None)
        old_tags = self._media_cache.get_tags(path)
        old_playlist_tag = self._playlist_tags.pop(slug, None)
        path.rename(new_path)
        new_slug = new_path.stem

        # Update internal state
        self._clips.pop(slug, None)
        self._clips[new_slug] = new_path
        info = self._clip_stats.pop(slug, None)
        if info is not None:
            self._clip_stats[new_slug] = (
                new_path, info[1], info[2], info[3])
        _purge_slug_thumbs(slug, keep=cached)
        _purge_slug_proxies(slug, keep=cached)
        _purge_clip_discord(path)
        for old in cached:
            try:
                old.replace(old.with_name(new_slug + old.name[len(old_stem):]))
            except OSError as exc:
                log.debug("Could not carry %s across the rename: %s", old.name, exc)
        # The file identity did not change, so its probe and metadata tag are
        # still valid; only the path key moved.
        self._media_cache.forget(path)
        if old_meta is not None:
            self._meta[new_slug] = old_meta
            self._media_cache.put_meta(new_path, old_meta)
        if old_tags is not None:
            self._media_cache.put_tags(new_path, old_tags)
        if old_playlist_tag is not None:
            self._playlist_tags[new_slug] = old_playlist_tag

        # Rename highlights file if it exists
        old_hl = HIGHLIGHTS_DIR / f"{slug}.json"
        if old_hl.exists():
            HIGHLIGHTS_DIR.mkdir(parents=True, exist_ok=True)
            old_hl.rename(HIGHLIGHTS_DIR / f"{new_slug}.json")
        keystore.move_sidecar(slug, new_slug)

        # Playlist membership, the view counter and the editor project follow
        # the clip
        if self.playlists.on_clip_renamed(slug, new_slug):
            await self._broadcast_playlists()
        if self.editor_project.on_clip_renamed(slug, new_slug):
            await self.broadcast({"type": "editor_project_changed"})
        if slug in self._views:
            self._views[new_slug] = self._views.pop(slug)
            _save_views(self._views)
        self._move_share_token(slug, new_slug)

        # Tell the UI: old card gone, new card appears
        await self.broadcast({"type": "clip_deleted", "slug": slug})
        asyncio.create_task(self._broadcast_clip(new_slug, new_path))
        return web.json_response({"ok": True, "slug": new_slug, "name": new_path.name})

    async def _api_reveal(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()
        asyncio.create_task(_xdg_open(path.parent))
        return web.json_response({"ok": True})

    async def _api_get_app_state(self, _: web.Request) -> web.Response:
        return web.json_response(_load_app_state())

    async def _api_set_app_state(self, req: web.Request) -> web.Response:
        try:
            body = await req.json()
        except Exception:
            return web.json_response({"ok": False, "error": "invalid JSON"}, status=400)
        if not isinstance(body, dict):
            return web.json_response({"ok": False, "error": "expected an object"}, status=400)
        state = _load_app_state()
        state.update(body)
        _save_app_state(state)
        return web.json_response({"ok": True})

    async def _api_copy_file(self, req: web.Request) -> web.Response:
        """Put the clip file itself on the clipboard so it can be pasted
        straight into Discord instead of shared as a link (#117).

        With ``?discord=1`` the file is the Discord-sized copy, built first if
        it does not exist yet. That is what the Share button copies when the
        user chose Discord files over links.
        """
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()

        if req.query.get("discord") == "1":
            try:
                copy_path = await self._prepare_discord_copy(slug, path)
            except DiscordTooLarge as exc:
                return web.json_response({"ok": False, "error": str(exc)})
            if copy_path is None or not copy_path.exists():
                return web.json_response(
                    {"ok": False, "error": "Could not build a Discord-sized copy"})
            path = copy_path

        uri = path.resolve().as_uri()
        # Chromium and Electron read pasted files from text/uri-list. Both
        # tools keep owning the selection in the background after forking,
        # so the clipboard survives this request returning.
        if shutil.which("wl-copy"):
            cmd = ["wl-copy", "--type", "text/uri-list"]
        elif shutil.which("xclip"):
            cmd = ["xclip", "-selection", "clipboard", "-t", "text/uri-list"]
        else:
            return web.json_response({
                "ok": False,
                "error": "Copying files needs wl-clipboard (Wayland) or xclip (X11).",
            })

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            proc.stdin.write(f"{uri}\r\n".encode())
            await proc.stdin.drain()
            proc.stdin.close()
        except Exception as exc:
            log.warning("Copying %s to the clipboard failed: %s", path.name, exc)
            return web.json_response({"ok": False, "error": "Could not reach the clipboard."})
        return web.json_response({"ok": True})

    # ── image handlers ────────────────────────────────────────────────────────

    def _image_or_404(self, req: web.Request) -> tuple[str, Path]:
        slug = req.match_info["slug"]
        path = self._images.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()
        return slug, path

    async def _image_file(self, req: web.Request) -> web.Response:
        _, path = self._image_or_404(req)
        return web.FileResponse(path, headers={
            "Content-Type": _IMAGE_MIME.get(path.suffix.lower(), "application/octet-stream"),
            # An annotation rewrites this path, so the URL's revision is what
            # makes the new picture appear. Never cache past that.
            "Cache-Control": "no-cache",
        })

    async def _image_thumb(self, req: web.Request) -> web.Response:
        _, path = self._image_or_404(req)
        width = 240 if req.query.get("w") == "240" else 640
        thumb = _thumb_path(path)
        if width != 640:
            thumb = thumb.with_name(f"{thumb.stem}_{width}.jpg")
        async with self._thumb_sem:
            t = await _make_thumb(path, thumb=thumb, width=width)
        if not t.exists():
            raise web.HTTPNotFound()
        return web.FileResponse(t, headers={"Content-Type": "image/jpeg"})

    async def _api_discord_copy(self, req: web.Request) -> web.Response:
        """Build (or reuse) the clip's Discord-sized copy and say where it is.

        The share modal calls this when it opens so the file is on disk before
        anyone tries to drag it: a drag cannot wait for an encode.
        """
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()
        try:
            copy_path = await self._prepare_discord_copy(slug, path)
        except DiscordTooLarge as exc:
            return web.json_response({"ok": False, "error": str(exc)})
        if copy_path is None or not copy_path.exists():
            return web.json_response(
                {"ok": False, "error": "Could not build a Discord-sized copy"})
        st = copy_path.stat()
        return web.json_response({
            "ok": True,
            # A path, not a URL: a drop target outside the window takes files
            # as a file:// uri-list. There is deliberately no HTTP route for
            # the copy, so nothing can fall back to the full-size original
            # while claiming to serve a Discord-sized one.
            "path": str(copy_path),
            "filename": f"{slug}.mp4",
            "size": st.st_size,
            "limit": DISCORD_MAX_BYTES,
        })

    async def _api_images(self, _: web.Request) -> web.Response:
        items = [
            (slug, path) for slug, path in sorted(
                self._images.items(),
                key=lambda kv: kv[1].stat().st_mtime if kv[1].exists() else 0,
                reverse=True,
            )
            if path.exists()
        ]

        # Listing never waits for thumbnails; /it/{slug} generates each one
        # when the browser actually asks for it.
        result = await asyncio.gather(
            *(self._image_json(slug, path) for slug, path in items),
            return_exceptions=False,
        )
        return web.json_response({"images": list(result)})

    async def _api_image_delete(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._images.pop(slug, None)
        if path and path.exists():
            path.unlink()
        _purge_slug_thumbs(slug)
        if self.playlists.on_clip_deleted(image_slug(slug)):
            await self._broadcast_playlists()
        await self.broadcast({"type": "image_deleted", "slug": slug})
        return web.json_response({"ok": True})

    async def _api_image_rename(self, req: web.Request) -> web.Response:
        slug, path = self._image_or_404(req)
        try:
            body = await req.json()
        except Exception:
            return web.json_response({"ok": False, "error": "invalid JSON"}, status=400)
        stem = slugify_clip_name(str(body.get("name", "")))
        if not stem:
            return web.json_response({"ok": False, "error": "that name has nothing usable in it"})
        if stem == slug:
            return web.json_response(await self._image_json(slug, path))

        target = path.with_name(f"{stem}{path.suffix}")
        if target.exists():
            return web.json_response({"ok": False, "error": "an image by that name already exists"})
        try:
            path.rename(target)
        except OSError as exc:
            log.warning("Renaming %s failed: %s", path.name, exc)
            return web.json_response({"ok": False, "error": str(exc)})

        self._images.pop(slug, None)
        self._images[stem] = target
        _purge_slug_thumbs(slug)
        if self.playlists.on_clip_renamed(image_slug(slug), image_slug(stem)):
            await self._broadcast_playlists()
        await self.broadcast({"type": "image_deleted", "slug": slug})
        await self._broadcast_image(stem, target)
        return web.json_response(await self._image_json(stem, target))

    async def _api_image_reveal(self, req: web.Request) -> web.Response:
        _, path = self._image_or_404(req)
        asyncio.create_task(_xdg_open(path.parent))
        return web.json_response({"ok": True})

    async def _api_image_open(self, req: web.Request) -> web.Response:
        _, path = self._image_or_404(req)
        asyncio.create_task(_xdg_open(path))
        return web.json_response({"ok": True})

    async def _api_image_copy(self, req: web.Request) -> web.Response:
        _, path = self._image_or_404(req)
        ok, error = await copy_image_to_clipboard(path)
        return web.json_response({"ok": ok} if ok else {"ok": False, "error": error})

    async def _api_image_annotate(self, req: web.Request) -> web.Response:
        """Replace an image with the annotated version the window composited.

        The body is the PNG itself rather than JSON: it is already megabytes,
        and base64 in a JSON envelope would make it a third bigger for nothing.
        """
        slug, path = self._image_or_404(req)
        data = await req.read()
        if not data.startswith(b"\x89PNG\r\n\x1a\n"):
            return web.json_response({"ok": False, "error": "expected a PNG body"}, status=400)

        # Written beside the original and moved into place, so an interrupted
        # save cannot leave the user with half a screenshot.
        tmp = path.with_suffix(f".annotating{path.suffix}")
        target = path.with_suffix(".png")
        try:
            tmp.write_bytes(data)
            tmp.replace(target)
        except OSError as exc:
            tmp.unlink(missing_ok=True)
            log.warning("Saving the annotated %s failed: %s", path.name, exc)
            return web.json_response({"ok": False, "error": str(exc)})

        # A jpg screenshot annotated once becomes a png, so the index has to
        # follow the file rather than keep pointing at a path that is gone.
        if target != path:
            path.unlink(missing_ok=True)
        self._images[slug] = target
        _purge_slug_thumbs(slug)
        await self._broadcast_image(slug, target)
        return web.json_response(await self._image_json(slug, target))

    async def _api_save_frame(self, req: web.Request) -> web.Response:
        """Save the frame at `time` of a clip as a screenshot (#171)."""
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()
        try:
            body = await req.json()
        except Exception:
            body = {}
        try:
            at = max(0.0, float(body.get("time", 0)))
        except (TypeError, ValueError):
            at = 0.0

        # The frame inherits the clip's game, so a still pulled out of a
        # Deadlock clip files itself under Deadlock like a screenshot would.
        game = self.playlists.game_for(slug)
        out = self.next_image_path(filename_tag(game))
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "error",
            "-ss", f"{at:.3f}", "-i", str(path),
            "-frames:v", "1", "-update", "1",
            "-y", str(out),
        ]
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await communicate_with_timeout(proc, timeout=60)
        except asyncio.TimeoutError:
            out.unlink(missing_ok=True)
            return web.json_response({"ok": False, "error": "ffmpeg timed out reading that frame"})
        except OSError as exc:
            return web.json_response({"ok": False, "error": str(exc)})
        if proc.returncode != 0 or not out.exists():
            out.unlink(missing_ok=True)
            reason = (stderr or b"").decode(errors="replace").strip().splitlines()
            return web.json_response({
                "ok": False,
                "error": reason[-1] if reason else "ffmpeg could not read that frame",
            })

        self.add_image(out, game=game)
        copied, copy_error = await copy_image_to_clipboard(out)
        payload = await self._image_json(out.stem, out)
        payload["ok"] = True
        payload["copied"] = copied
        if not copied:
            payload["copy_error"] = copy_error
        return web.json_response(payload)

    async def _api_open(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        path = self._clips.get(slug)
        if not path or not path.exists():
            raise web.HTTPNotFound()
        # Playback escape hatch for WebEngine builds that cannot decode the
        # clip (PyPI wheels ship without H.264 support): hand the file to the
        # system default video player.
        asyncio.create_task(_xdg_open(path))
        return web.json_response({"ok": True})

    async def _api_playlists(self, _: web.Request) -> web.Response:
        return web.json_response({"playlists": self.playlists.list_playlists()})

    async def _api_create_playlist(self, req: web.Request) -> web.Response:
        body = await req.json()
        try:
            playlist = self.playlists.create_custom(
                name=str(body.get("name", "")),
                emoji=str(body.get("emoji", "") or ""),
                color1=str(body.get("color1", "") or ""),
                color2=str(body.get("color2", "") or ""),
            )
        except ValueError as exc:
            return web.json_response({"ok": False, "error": str(exc)}, status=400)
        await self._broadcast_playlists()
        return web.json_response({"ok": True, "playlist": playlist})

    async def _api_patch_playlist(self, req: web.Request) -> web.Response:
        pid = req.match_info["pid"]
        body = await req.json()
        existing = self.playlists.get(pid)
        members = list(existing.get("clip_slugs", [])) if existing else []
        try:
            playlist = self.playlists.update_playlist(pid, body)
        except KeyError:
            raise web.HTTPNotFound()
        except ValueError as exc:
            return web.json_response({"ok": False, "error": str(exc)}, status=400)
        await self._sync_playlist_tags(members, budget=2.0, throttle=True)
        await self._broadcast_playlists()
        return web.json_response({"ok": True, "playlist": playlist})

    async def _api_delete_playlist(self, req: web.Request) -> web.Response:
        pid = req.match_info["pid"]
        existing = self.playlists.get(pid)
        members = list(existing.get("clip_slugs", [])) if existing else []
        try:
            self.playlists.delete(pid)
        except KeyError:
            raise web.HTTPNotFound()
        except ValueError as exc:
            return web.json_response({"ok": False, "error": str(exc)}, status=400)
        await self._sync_playlist_tags(members, budget=2.0, throttle=True)
        await self._broadcast_playlists()
        return web.json_response({"ok": True})

    async def _api_playlist_add_clip(self, req: web.Request) -> web.Response:
        pid = req.match_info["pid"]
        body = await req.json()
        slug = str(body.get("slug", ""))
        # Clips and screenshots share one membership list, so this accepts
        # either namespace and refuses anything that is neither.
        known = (
            slug[len(IMAGE_PREFIX):] in self._images
            if slug.startswith(IMAGE_PREFIX)
            else slug in self._clips
        )
        if not known:
            raise web.HTTPNotFound()
        try:
            playlist = self.playlists.add_clip(pid, slug)
        except KeyError:
            raise web.HTTPNotFound()
        # playlists.json already records the change, and the background
        # backfill owns the (slow) remux. Do not make the card wait on a full
        # copy of a video stored on a network drive.
        await self._sync_playlist_tags([slug], budget=0.0, throttle=True)
        await self._broadcast_playlists()
        return web.json_response({"ok": True, "playlist": playlist})

    async def _api_playlist_remove_clip(self, req: web.Request) -> web.Response:
        pid = req.match_info["pid"]
        slug = req.match_info["slug"]
        try:
            self.playlists.remove_clip(pid, slug)
        except KeyError:
            raise web.HTTPNotFound()
        await self._sync_playlist_tags([slug], budget=0.0, throttle=True)
        await self._broadcast_playlists()
        return web.json_response({"ok": True})

    async def _api_view(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        if slug not in self._clips:
            raise web.HTTPNotFound()
        self._views[slug] = self._views.get(slug, 0) + 1
        _save_views(self._views)
        return web.json_response({"ok": True, "views": self._views[slug]})

    async def _api_get_highlights(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        return web.json_response({"highlights": _load_highlights(slug)})

    async def _api_add_highlight(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        body  = await req.json()
        time_ = round(float(body.get("time", 0)), 3)
        label = (body.get("label") or "Highlight").strip() or "Highlight"
        color = body.get("color") or "#f59e0b"
        hl = _load_highlights(slug)
        next_id = str(max((int(h["id"]) for h in hl if str(h.get("id","")).isdigit()), default=0) + 1)
        entry = {"id": next_id, "time": time_, "label": label, "color": color}
        hl.append(entry)
        hl.sort(key=lambda h: h["time"])
        _save_highlights(slug, hl)
        return web.json_response({"ok": True, "highlight": entry})

    async def _api_patch_highlight(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        hid  = req.match_info["hid"]
        body  = await req.json()
        hl = _load_highlights(slug)
        for h in hl:
            if str(h.get("id")) == hid:
                if "label" in body:
                    h["label"] = (body["label"] or "Highlight").strip() or "Highlight"
                if "color" in body:
                    h["color"] = body["color"]
                if "time" in body:
                    try:
                        h["time"] = round(max(0.0, float(body["time"])), 3)
                    except (TypeError, ValueError):
                        pass
                hl.sort(key=lambda x: float(x.get("time", 0)))
                _save_highlights(slug, hl)
                return web.json_response({"ok": True})
        return web.json_response({"ok": False, "error": "highlight not found"})

    async def _api_del_highlight(self, req: web.Request) -> web.Response:
        slug = req.match_info["slug"]
        hid  = req.match_info["hid"]
        hl = [h for h in _load_highlights(slug) if str(h.get("id")) != hid]
        _save_highlights(slug, hl)
        return web.json_response({"ok": True})

    # ── editor ───────────────────────────────────────────────────────────────

    async def _api_editor_get_project(self, _: web.Request) -> web.Response:
        project = self.editor_project.load()
        missing: list[str] = []
        if project:
            refs = {it.get("clipId") for it in project.get("items", [])
                    if isinstance(it, dict) and it.get("clipId")}
            missing = sorted(c for c in refs if c not in self._clips)
        return web.json_response({"project": project, "missing": missing})

    async def _api_editor_save_project(self, req: web.Request) -> web.Response:
        # Autosave is lenient on purpose: items may reference clips that no
        # longer exist and still get persisted, so a slow-mounting output dir
        # can't eat the project.
        try:
            body = await req.json()
        except Exception:
            return web.json_response({"ok": False, "error": "invalid JSON"}, status=400)
        if (not isinstance(body, dict)
                or not isinstance(body.get("tracks"), list)
                or not isinstance(body.get("items"), list)):
            return web.json_response({"ok": False, "error": "expected a project"},
                                     status=400)
        self.editor_project.save({"version": 1, "tracks": body["tracks"],
                                  "items": body["items"]})
        return web.json_response({"ok": True})

    async def _editor_sources(self, project: dict) -> dict[str, Source]:
        sources: dict[str, Source] = {}
        for it in project.get("items", []):
            cid = it.get("clipId") if isinstance(it, dict) else None
            if not cid or cid in sources:
                continue
            path = self._clips.get(cid)
            if not path or not path.exists():
                continue
            meta = await self._get_meta(cid, path)
            sources[cid] = Source(
                path=path,
                duration=meta.get("duration", 0),
                width=meta.get("width", 0),
                height=meta.get("height", 0),
                has_audio=meta.get("audio_streams", 0) > 0,
                audio_streams=meta.get("audio_streams", 0),
            )
        return sources

    async def _api_editor_export(self, req: web.Request) -> web.Response:
        try:
            body = await req.json()
        except Exception:
            return web.json_response({"ok": False, "error": "invalid JSON"}, status=400)
        raw = body.get("project")
        if not isinstance(raw, dict):
            return web.json_response({"ok": False, "error": "expected a project"},
                                     status=400)
        if self._exports.busy:
            return web.json_response(
                {"ok": False, "error": "an export is already running"}, status=409)

        sources = await self._editor_sources(raw)
        project, errors = validate_project(raw, sources)
        if errors:
            return web.json_response({"ok": False, "errors": errors}, status=400)

        out_dir = resolve_path(self.cfg.output.directory)
        location = body.get("location", "library")
        if location == "videos":
            dest = actual_home_dir() / "Videos"
        elif location == "custom":
            custom = str(body.get("path", "")).strip()
            if not custom:
                return web.json_response(
                    {"ok": False, "error": "a folder is required"}, status=400)
            dest = resolve_path(custom)
        else:
            location, dest = "library", out_dir
        try:
            dest.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            return web.json_response(
                {"ok": False, "error": f"cannot use that folder: {exc}"}, status=400)

        requested = body.get("filename")
        if requested:
            name = sanitize_export_name(str(requested))
            if not name:
                return web.json_response(
                    {"ok": False, "error": "that name will not work as a file"},
                    status=400)
        else:
            name = default_export_name(dest)
        final = dest / name
        if final.exists():
            return web.json_response(
                {"ok": False, "error": "a file with that name already exists"},
                status=400)

        job_id = f"exp-{int(datetime.now().timestamp() * 1000)}"
        work = EXPORT_WORK_DIR / job_id
        work.mkdir(parents=True, exist_ok=True)
        for path, text in text_file_contents(project, work).items():
            path.write_text(text)

        tmp = dest / f".{final.stem}.export.mp4"
        discord_optimized = bool(body.get("discord_optimized"))
        accent = str(body.get("accent", "")) or "#0099ff"
        key_burn = None
        if body.get("keys"):
            key_burn = await asyncio.to_thread(
                _key_burn_for, project, sources, work / "keys", accent)
        try:
            cmd = build_export_cmd(project, sources, tmp,
                                   accent=accent,
                                   text_dir=work,
                                   discord_optimized=discord_optimized,
                                   key_burn=key_burn)
        except DiscordTooLarge as exc:
            shutil.rmtree(work, ignore_errors=True)
            return web.json_response({"ok": False, "error": str(exc)}, status=400)
        add_to_library = bool(body.get("add_to_library"))

        async def on_done(path: Path) -> Optional[dict]:
            target = path
            if location != "library" and add_to_library:
                copy = out_dir / name
                if copy.exists():
                    copy = out_dir / default_export_name(out_dir)
                out_dir.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, copy)
                target = copy
            elif location != "library":
                return None
            self.add_clip(target)
            meta = await self._get_meta(target.stem, target)
            return self._clip_json(target.stem, target, meta)

        def cleanup() -> None:
            shutil.rmtree(work, ignore_errors=True)

        try:
            self._exports.start(job_id, cmd, project_extent(project), tmp, final,
                                on_done=on_done, cleanup=cleanup,
                                max_bytes=DISCORD_MAX_BYTES if discord_optimized else None)
        except ExportBusy:
            cleanup()
            return web.json_response(
                {"ok": False, "error": "an export is already running"}, status=409)
        log.info("Editor export %s started: %s", job_id, final)
        return web.json_response({"ok": True, "job_id": job_id, "path": str(final)})

    async def _api_editor_export_cancel(self, req: web.Request) -> web.Response:
        if await self._exports.cancel(req.match_info["jid"]):
            return web.json_response({"ok": True})
        raise web.HTTPNotFound()

    async def _api_uninstall(self, _: web.Request) -> web.Response:
        """Launch a detached uninstall process, then exit the daemon cleanly."""
        import os
        import signal as _sig
        import subprocess as _sp
        import sys

        # Use a shell subprocess in a *new session* so it survives after we
        # send SIGTERM to this daemon process.  The `sleep 2` delay lets the
        # daemon finish shutting down (and the Unix socket disappear) before
        # the uninstall script tries to stop it via IPC, avoiding a deadlock
        # where the uninstall blocks waiting to talk to a daemon that is
        # waiting for the uninstall to finish.
        exe = sys.executable.replace("'", r"\'")
        cmd = f"sleep 2 && '{exe}' -m vice.main uninstall --yes"
        try:
            _sp.Popen(
                ["bash", "-c", cmd],
                start_new_session=True,
                stdin=_sp.DEVNULL,
                stdout=_sp.DEVNULL,
                stderr=_sp.DEVNULL,
            )
        except Exception as exc:
            log.error("Failed to launch uninstall subprocess: %s", exc)

        # Stop the daemon after giving the HTTP response time to reach the client.
        async def _exit() -> None:
            await asyncio.sleep(0.4)
            os.kill(os.getpid(), _sig.SIGTERM)

        asyncio.create_task(_exit())
        return web.json_response({"ok": True})

    async def _api_get_config(self, _: web.Request) -> web.Response:
        from .config import load as load_cfg
        return web.json_response(asdict(load_cfg()))

    async def _api_get_displays(self, req: web.Request) -> web.Response:
        backend = (req.query.get("backend") or self.cfg.recording.backend or "auto").strip() or "auto"
        # Enumeration shells out (with timeouts); keep it off the event loop.
        from .active_window import pointer_display_supported
        payload = await asyncio.to_thread(list_display_options, backend)
        payload["selected"] = self.cfg.recording.display
        payload["follow_mouse_supported"] = pointer_display_supported()
        return web.json_response(payload)

    async def _api_get_audio_sources(self, _: web.Request) -> web.Response:
        payload = await asyncio.to_thread(list_gsr_audio_sources)
        payload["selected"] = self.cfg.recording.gsr_audio_source
        return web.json_response(payload)

    async def _api_set_config(self, req: web.Request) -> web.Response:
        from .config import (
            Config, RecordingConfig, HotkeyConfig, OutputConfig, SharingConfig,
            DiscordConfig, DiscordCustomGame, UpdatesConfig, NotificationsConfig,
            UIConfig, OverlayConfig,
            clamp_recording_limits, ensure_buffer_covers_clip_presets, normalize_overlay,
            normalize_clip_presets, normalize_combo, normalize_focus_blocklist,
            validate_hotkeys,
            load as load_cfg, save as save_cfg,
        )

        body = await req.json()

        def _merge(base: dict, over: dict) -> dict:
            for k, v in over.items():
                # Keyed by source name, so the keys are data: a new source has
                # no field to merge into and must be stored whole (#263).
                if k in FREE_MAP_KEYS and isinstance(v, dict):
                    base[k] = dict(v)
                elif k in base and isinstance(base[k], dict) and isinstance(v, dict):
                    _merge(base[k], v)
                elif k in base:
                    base[k] = v
            return base

        # Merge onto the persisted config so partial saves never depend on
        # transient in-memory rollback state.
        persisted_cfg = load_cfg()
        merged = _merge(asdict(persisted_cfg), body)

        discord_raw = dict(merged.get("discord", {}))
        custom_games_raw = discord_raw.pop("custom_games", []) or []
        discord_custom_games = [
            DiscordCustomGame(
                name=str(g.get("name", "")),
                matches=[str(m) for m in (g.get("matches") or [])],
            )
            for g in custom_games_raw
            if isinstance(g, dict)
        ]
        hotkeys_raw = dict(merged.get("hotkeys", {}))
        if hotkeys_raw.get("clip"):
            hotkeys_raw["clip"] = normalize_combo(str(hotkeys_raw["clip"]).strip())
        # Cleared in the UI means unbound, not bound to the empty string.
        shot = str(hotkeys_raw.get("screenshot") or "").strip()
        hotkeys_raw["screenshot"] = normalize_combo(shot) if shot else None
        toggle = str(hotkeys_raw.get("key_overlay") or "").strip()
        hotkeys_raw["key_overlay"] = normalize_combo(toggle) if toggle else None
        try:
            hotkeys_raw["clip_presets"] = normalize_clip_presets(
                hotkeys_raw.get("clip_presets", []),
                strict=True,
            )
        except ValueError as exc:
            return web.json_response({"ok": False, "error": str(exc)}, status=400)
        hotkeys_raw["disable_while_focused"] = normalize_focus_blocklist(
            hotkeys_raw.get("disable_while_focused")
        )

        new_cfg = Config(
            recording=RecordingConfig(**{
                k: v for k, v in merged["recording"].items()
                if k in RecordingConfig.__dataclass_fields__
            }),
            hotkeys=HotkeyConfig(**{
                k: v for k, v in hotkeys_raw.items()
                if k in HotkeyConfig.__dataclass_fields__
            }),
            output=OutputConfig(**{
                k: v for k, v in merged["output"].items()
                if k in OutputConfig.__dataclass_fields__
            }),
            sharing=SharingConfig(**{
                k: v for k, v in merged["sharing"].items()
                if k in SharingConfig.__dataclass_fields__
            }),
            discord=DiscordConfig(
                **{k: v for k, v in discord_raw.items()
                   if k in DiscordConfig.__dataclass_fields__ and k != "custom_games"},
                custom_games=discord_custom_games,
            ),
            updates=UpdatesConfig(**{
                k: v for k, v in merged.get("updates", {}).items()
                if k in UpdatesConfig.__dataclass_fields__
            }),
            notifications=NotificationsConfig(**{
                k: v for k, v in merged.get("notifications", {}).items()
                if k in NotificationsConfig.__dataclass_fields__
            }),
            ui=UIConfig(**{
                k: v for k, v in merged.get("ui", {}).items()
                if k in UIConfig.__dataclass_fields__
            }),
            overlay=OverlayConfig(**{
                k: v for k, v in merged.get("overlay", {}).items()
                if k in OverlayConfig.__dataclass_fields__
            }),
        )
        try:
            validate_hotkeys(new_cfg.hotkeys)
        except ValueError as exc:
            return web.json_response({"ok": False, "error": str(exc)}, status=400)
        ensure_buffer_covers_clip_presets(new_cfg)
        clamp_recording_limits(new_cfg)
        normalize_overlay(new_cfg)

        old_cfg = copy.deepcopy(self.cfg)
        image_dir_changed = (
            getattr(old_cfg.output, "image_directory", "")
            != getattr(new_cfg.output, "image_directory", "")
        )
        # embed_color is read per-request (the UI syncs it on theme switches)
        # and share_discord_file only by the UI, so neither demands a daemon
        # restart.
        old_sharing = copy.deepcopy(old_cfg.sharing)
        new_sharing = copy.deepcopy(new_cfg.sharing)
        old_sharing.embed_color = new_sharing.embed_color = ""
        old_sharing.share_discord_file = new_sharing.share_discord_file = False
        restart_required = (
            old_sharing != new_sharing
            or old_cfg.recording.gsr_args != new_cfg.recording.gsr_args
        )

        # Apply live (some settings still require daemon restart, e.g. recorder
        # backend). "ui" is only read by vice-app when the window opens, but it
        # is carried across so self.cfg still matches what is on disk.
        for field in ("recording", "hotkeys", "output", "sharing", "discord", "ui", "overlay"):
            setattr(self.cfg, field, getattr(new_cfg, field))

        apply_error: str | None = None
        if self.apply_config_cb:
            try:
                await self.apply_config_cb()
            except Exception as exc:
                # Keep runtime state stable and reject invalid live changes.
                for field in ("recording", "hotkeys", "output", "sharing", "discord"):
                    setattr(self.cfg, field, getattr(old_cfg, field))

                try:
                    await self.apply_config_cb()
                except Exception as rollback_exc:
                    log.warning("Rollback apply failed: %s", rollback_exc)

                apply_error = str(exc) or exc.__class__.__name__
                log.warning("Live config apply failed; settings saved for next restart: %s", exc)

        # Always persist validated settings, even when live apply fails.
        # This keeps restart-intended config changes from being lost.
        save_cfg(new_cfg)

        # Pointing Vice at a different pictures folder has to show that
        # folder's contents now, not after a restart.
        if image_dir_changed and not apply_error:
            self.rescan_images()

        if apply_error:
            return web.json_response({
                "ok": True,
                "applied": False,
                "restart_required": True,
                "warning": "Settings saved for next restart. Restart Vice to apply them.",
                "error": apply_error,
            })

        payload = {"ok": True, "applied": True, "restart_required": restart_required}
        if restart_required:
            payload["warning"] = "Some sharing settings require a full app restart to take effect."
        return web.json_response(payload)

    def clip_count(self) -> int:
        """How many clips are in the library right now."""
        return len(self._clips)

    async def _api_status(self, _: web.Request) -> web.Response:
        extra = self.get_status_cb() if self.get_status_cb else {}
        public_url = self.public_base_url()
        return web.json_response({
            "running":  True,
            "version":  __version__,
            "clips":    self.clip_count(),
            "images":   self.image_count(),
            "local_url": self.local_base_url(),
            "public_url": public_url,
            "base_url": public_url,
            "public_is_tunnel": self.public_is_reachable(),
            **extra,
        })

    async def _api_trigger(self, _: web.Request) -> web.Response:
        if self.trigger_clip_cb:
            asyncio.create_task(self.trigger_clip_cb())
        return web.json_response({"ok": True})

    async def _api_check_update(self, _: web.Request) -> web.Response:
        """Check now, ignoring the daily interval. Returns the newer release
        or null; a failed check is reported as null, never as an error."""
        if not self.check_update_cb:
            return web.json_response({"ok": True, "update": None})
        try:
            found = await self.check_update_cb()
        except Exception as exc:
            log.debug("Manual update check failed: %s", exc)
            found = None
        return web.json_response({"ok": True, "update": found})

    async def _api_quit(self, _: web.Request) -> web.Response:
        """Stop the daemon (browser-mode quit, native window uses pywebview API)."""
        import os, signal as _sig
        response = web.json_response({"ok": True})
        asyncio.get_event_loop().call_later(0.2, lambda: os.kill(os.getpid(), _sig.SIGTERM))
        return response

    # ── WebSocket ─────────────────────────────────────────────────────────────

    async def _ws_handler(self, req: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(req)
        self._ws_clients.add(ws)
        try:
            async for msg in ws:
                if msg.type in (WSMsgType.ERROR, WSMsgType.CLOSE):
                    break
        finally:
            self._ws_clients.discard(ws)
        return ws

    # ── Tunnel (Cloudflare quick tunnel) ──────────────────────────────────────
    #
    # cloudflared is the only supported tunnel. There used to be an SSH
    # fallback via serveo.net, but it produced broken links (serveo prints
    # promotional URLs that got parsed as the tunnel address), is operated
    # by an unaccountable third party, and silently man-in-the-middles all
    # traffic. Failing loudly with an install hint is strictly better.

    async def _start_tunnel(self, port: int) -> None:
        if not shutil.which("cloudflared"):
            await self._tunnel_failed(
                "cloudflared is not installed. Install it to get public share "
                "links (https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/), "
                "or turn off the public tunnel in Settings."
            )
            return
        log.info("Starting Cloudflare Tunnel on port %d", port)
        if self._tunnel_task and not self._tunnel_task.done():
            return
        self._tunnel_stopping = False
        self._tunnel_task = asyncio.create_task(self._run_tunnel(port))

    async def _run_tunnel(self, port: int) -> None:
        delay = _TUNNEL_RETRY_INITIAL
        while not self._tunnel_stopping:
            try:
                proc = await asyncio.create_subprocess_exec(
                    "cloudflared", "tunnel", "--url", f"http://localhost:{port}",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.STDOUT,
                )
            except OSError as exc:
                await self._tunnel_failed(f"cloudflared failed to start: {exc}")
                connected = False
            else:
                self._tunnel_proc = proc
                try:
                    _, connected = await self._read_cloudflare_url(proc)
                except asyncio.CancelledError:
                    await self._stop_tunnel_process(proc)
                    raise
                finally:
                    if self._tunnel_proc is proc:
                        self._tunnel_proc = None

            if self._tunnel_stopping:
                return
            if connected:
                delay = _TUNNEL_RETRY_INITIAL
            log.warning("Retrying Cloudflare Tunnel in %.0f seconds", delay)
            await asyncio.sleep(delay)
            delay = min(delay * 2, _TUNNEL_RETRY_MAX)

    @staticmethod
    async def _stop_tunnel_process(proc: asyncio.subprocess.Process) -> None:
        if proc.returncode is not None:
            return
        try:
            proc.terminate()
            await asyncio.wait_for(proc.wait(), timeout=5)
        except ProcessLookupError:
            return
        except asyncio.TimeoutError:
            try:
                proc.kill()
            except ProcessLookupError:
                return
            await proc.wait()

    async def _tunnel_failed(self, reason: str) -> None:
        log.error("Public share tunnel unavailable: %s", reason)
        self._tunnel_url = None
        await self.broadcast({"type": "tunnel_error", "error": reason})
        await self._broadcast_share_links()

    async def _read_cloudflare_url(
        self, proc: Optional[asyncio.subprocess.Process] = None,
    ) -> tuple[str, bool]:
        proc = proc or self._tunnel_proc
        assert proc and proc.stdout
        diagnostics: list[str] = []
        connected = False
        async for raw in proc.stdout:
            line = raw.decode(errors="replace").strip()
            # Keep draining stdout after the URL so process exit is still
            # detected.
            url = _quick_tunnel_url(line) if self._tunnel_url is None else None
            if url:
                self._tunnel_url = url
                connected = True
                log.info("Cloudflare Tunnel URL: %s", self._tunnel_url)
                await self.broadcast({"type": "tunnel_url", "url": self._tunnel_url})
                await self._broadcast_share_links()
            if not url and line and any(
                marker in line.lower() for marker in ("err", "error", "failed", "unable")
            ):
                diagnostics.append(line[-600:])
                diagnostics = diagnostics[-20:]

        rc = proc.returncode if proc.returncode is not None else await proc.wait()
        if connected:
            reason = f"cloudflared exited (code {rc}) after providing a tunnel URL"
        else:
            reason = f"cloudflared exited (code {rc}) before providing a tunnel URL"
        detail = _cloudflared_failure_detail(diagnostics)
        if detail:
            reason += f": {detail}"
        if proc is self._tunnel_proc:
            await self._tunnel_failed(reason)
        return reason, connected
