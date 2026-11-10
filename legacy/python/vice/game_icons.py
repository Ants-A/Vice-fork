"""Discord game icons for detected games.

Matches a game name against Discord's public detectable-games list, reads the
application's icon hash, and returns a CDN URL. Results are cached on disk.
Lookups run in a background thread, so callers get None until one finishes.
"""
import json
import logging
import threading
import time
import urllib.request
from pathlib import Path
from typing import Optional

from . import __version__
from .runtime import actual_home_dir

log = logging.getLogger("vice")

_API = "https://discord.com/api/v10"
_LIST_TTL = 7 * 86400
_lock = threading.Lock()
_inflight: set[str] = set()


def cache_dir() -> Path:
    return actual_home_dir() / ".cache" / "vice"


def _fetch_json(url: str):
    req = urllib.request.Request(url, headers={"User-Agent": f"Vice/{__version__}"})
    with urllib.request.urlopen(req, timeout=8) as resp:
        return json.load(resp)


def _load(path: Path, default):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        log.debug("Game icon cache %s unreadable: %s", path.name, exc)
        return default


def _save(path: Path, data) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data))
        tmp.replace(path)
    except OSError as exc:
        log.debug("Could not write game icon cache %s: %s", path.name, exc)


def build_name_map(apps: list) -> dict:
    """Lowercased game name -> Discord application id. First entry wins."""
    names: dict = {}
    for app in apps or []:
        name = str(app.get("name", "")).strip().lower()
        if name and app.get("id") and name not in names:
            names[name] = app["id"]
    return names


def _name_map() -> dict:
    path = cache_dir() / "detectable.json"
    cached = _load(path, {})
    if cached.get("names") and cached.get("ts", 0) + _LIST_TTL > time.time():
        return cached["names"]
    try:
        apps = _fetch_json(f"{_API}/applications/detectable")
    except Exception as exc:
        log.debug("Could not fetch Discord's detectable games list: %s", exc)
        return cached.get("names", {})
    names = build_name_map(apps)
    _save(path, {"ts": time.time(), "names": names})
    return names


def _resolve(key: str) -> None:
    try:
        names = _name_map()
        if not names:
            log.debug("No detectable games list available, not caching %r", key)
            return
        app_id = names.get(key)
        url = ""
        if app_id:
            icon = _fetch_json(f"{_API}/applications/{app_id}/rpc").get("icon")
            if icon:
                url = f"https://cdn.discordapp.com/app-icons/{app_id}/{icon}.png"
    except Exception as exc:
        log.debug("Game icon lookup for %r failed, not caching: %s", key, exc)
        return
    with _lock:
        path = cache_dir() / "icons.json"
        icons = _load(path, {})
        icons[key] = url
        _save(path, icons)


def discord_icon_url(game: str) -> Optional[str]:
    """Cached icon URL for a game name, or None. Starts a lookup on a miss."""
    key = (game or "").strip().lower()
    if not key:
        return None
    icons = _load(cache_dir() / "icons.json", {})
    if key in icons:
        return icons[key] or None
    with _lock:
        if key in _inflight:
            return None
        _inflight.add(key)

    def run() -> None:
        try:
            _resolve(key)
        finally:
            with _lock:
                _inflight.discard(key)

    threading.Thread(target=run, daemon=True, name="vice-game-icon").start()
    return None
