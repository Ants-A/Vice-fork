"""Shared ffprobe helpers for clip files.

Used by both the recorder (clip finalization, trimming) and the share
server (metadata, thumbnails). Kept in one place so duration handling
behaves identically everywhere.
"""

from __future__ import annotations

import asyncio
import fnmatch
import json
import logging
import math
import os
import tempfile
from fractions import Fraction
from pathlib import Path
from typing import Optional

log = logging.getLogger("vice.media")

# Suffix patterns for temp files written during in-place edits
# (trim / watermark / remux). Leftovers mean a previous run was
# interrupted mid-edit; they are safe to delete at daemon startup.
TEMP_FILE_GLOBS = ("*.trim.mp4", "*.wm.mp4", "*.fix.mp4", "*.trimming.mp4",
                   "*.trim.mkv", "*.wm.mkv", "*.fix.mkv", "*.trimming.mkv",
                   "*.export.mp4",
                   # Left behind when an image annotation is interrupted
                   # between writing the new picture and moving it into place.
                   "*.annotating.png", "*.annotating.jpg", "*.annotating.jpeg",
                   # Playlist metadata is remuxed into a hidden temp file
                   # beside the clip before replacing it.
                   ".vice-meta-*")


async def communicate_with_timeout(
    proc: asyncio.subprocess.Process, timeout: float,
) -> tuple[Optional[bytes], Optional[bytes]]:
    """Collect a finite media command, reaping it on timeout or cancellation.

    Keep draining its pipes during cleanup. Waiting only for process exit can
    deadlock when buffered stdout or stderr has filled the pipe transport.
    Recording processes have their own graceful finalization and must not use
    this helper.
    """
    communication = asyncio.create_task(proc.communicate())
    try:
        return await asyncio.wait_for(asyncio.shield(communication), timeout)
    finally:
        if proc.returncode is None:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
        await communication


async def probe_media(path: Path) -> Optional[dict]:
    """Probe *path* with ffprobe.

    Returns ``{"width", "height", "duration", "vcodec", "audio_streams"}``
    or ``None`` when ffprobe fails or the file has no video stream.
    """
    meta, _ = await probe_media_detailed(path)
    return meta


async def probe_media_detailed(path: Path) -> tuple[Optional[dict], str]:
    """Probe *path*, returning the metadata and why it failed.

    Same result as :func:`probe_media`, plus ffprobe's own explanation when
    the probe comes back empty. Vice used to run ffprobe with ``-v quiet``
    and discard stderr, so a clip that could not be read produced a log line
    saying only that JSON parsing failed. That cost #154 three round trips
    with the reporter, so the reason is kept and logged now.

    Duration prefers the container (format) value over the stream value:
    fragmented MP4, which gpu-screen-recorder writes for replay clips, has
    no per-stream duration tag, so reading only the stream field reports 0
    for perfectly healthy files.
    """
    stderr = b""
    try:
        proc = await asyncio.create_subprocess_exec(
            "ffprobe", "-v", "error", "-print_format", "json",
            "-show_format", "-show_streams", str(path),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await communicate_with_timeout(proc, timeout=15)
        data = json.loads(stdout)
    except FileNotFoundError:
        reason = "ffprobe not found, install ffmpeg to read clip metadata"
        log.error("%s", reason)
        return None, reason
    except asyncio.TimeoutError:
        reason = "ffprobe timed out after 15s"
        log.warning("Could not read %s: %s", path.name, reason)
        return None, reason
    except Exception as exc:
        reason = _ffprobe_reason(stderr, path) or str(exc)
        log.warning("Could not read %s: %s", path.name, reason)
        return None, reason

    if proc.returncode != 0 or not data:
        reason = _ffprobe_reason(stderr, path) or f"ffprobe exited {proc.returncode}"
        log.warning("Could not read %s: %s", path.name, reason)
        return None, reason

    video = next(
        (s for s in data.get("streams", []) if s.get("codec_type") == "video"),
        None,
    )
    if video is None:
        reason = "the file has no video stream"
        log.warning("Could not read %s: %s", path.name, reason)
        return None, reason

    duration = _parse_duration(data.get("format", {}).get("duration"))
    if duration <= 0:
        duration = _parse_duration(video.get("duration"))
    if duration <= 0:
        # Some gpu-screen-recorder and FFmpeg combinations write a complete
        # MP4 with zero duration fields while retaining the video sample
        # count and frame rate. That file is playable, but treating it as
        # unreadable makes Vice discard a valid clip (#154). Estimate only
        # when both values are present and the rate is sane. We keep the
        # zero-duration result for files that do not provide enough evidence.
        duration = _duration_from_video_samples(video)
        # The sample count is only evidence if the frames can be read. A file
        # gpu-screen-recorder 5.13.3 wrote as MP4 on Debian 13 stamps every
        # frame at zero, and FFmpeg's MP4 reader keeps one frame per
        # timestamp, so it reads 1 of 3655. Estimating 60 seconds from the
        # index let that file through to the trim, which replaced the
        # recording with a single frame (#154).
        # A positive estimate means nb_frames already parsed as a count.
        frames = int(video["nb_frames"]) if duration > 0 else 0
        if frames > 1:
            readable = await _readable_video_packets(path)
            if readable is not None and readable * 2 < frames:
                reason = (
                    f"every frame is stamped at the same time, so only "
                    f"{readable} of its {frames} frames can be read"
                )
                log.warning("Could not read %s: %s", path.name, reason)
                return None, reason
    audio = [s for s in data.get("streams", []) if s.get("codec_type") == "audio"]
    audio_tracks = []
    for index, stream in enumerate(audio):
        tags = {k.lower(): v for k, v in stream.get("tags", {}).items()}
        audio_tracks.append({
            "index": index,
            "title": str(tags.get("title", ""))[:200],
            "language": str(tags.get("language", ""))[:32],
            "channels": int(stream.get("channels") or 0),
        })
    # Container-level tags. Playlist membership lives here so it travels with
    # the clip through moves, renames and reinstalls. Keys are lowercased
    # because Matroska writers do not agree on the case and MP4 writers are
    # not consistent either.
    format_tags = {
        str(k).lower(): str(v)
        for k, v in (data.get("format", {}).get("tags") or {}).items()
        if k is not None and v is not None
    }
    return {
        "width": int(video.get("width") or 0),
        "height": int(video.get("height") or 0),
        "duration": duration,
        "vcodec": (video.get("codec_name") or "").lower(),
        "audio_streams": len(audio),
        "audio_tracks": audio_tracks,
        "tags": format_tags,
    }, ""


async def probe_container_tags(path: Path) -> dict[str, str]:
    """Read only a file's container tags.

    Cheaper than :func:`probe_media_detailed` because ffprobe does not have to
    walk the streams. That matters on startup, when every clip in a large
    library is checked for playlist metadata.
    """
    try:
        proc = await asyncio.create_subprocess_exec(
            "ffprobe", "-v", "error", "-show_entries", "format_tags",
            "-of", "json", str(path),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        stdout, _ = await communicate_with_timeout(proc, timeout=10)
        if proc.returncode != 0:
            return {}
        data = json.loads(stdout)
        raw = (data.get("format") or {}).get("tags") or {}
        return {
            str(k).lower(): str(v)
            for k, v in raw.items()
            if k is not None and v is not None
        }
    except (asyncio.TimeoutError, FileNotFoundError, ValueError, OSError) as exc:
        log.debug("Could not read the container tags of %s: %s", path.name, exc)
        return {}


async def write_metadata_tags(path: Path, tags: dict[str, str],
                              timeout: float = 120,
                              readrate: Optional[float] = None) -> bool:
    """Write container-level *tags* into *path* by remuxing it in place.

    Streams are copied, so the video and audio are untouched. The file is
    replaced atomically only after ffmpeg has produced a complete copy, and
    its mode and mtime are preserved so a tag update does not look like a new
    recording to the rest of Vice. Returns False (and leaves the original
    alone) when the container cannot carry the tags or ffmpeg fails.

    *readrate* caps how fast ffmpeg may read the input, as a multiple of
    realtime. Background playlist backfills pass a small value so a remux
    cannot saturate a WiFi/CIFS library; foreground edits leave it unset.
    """
    if not tags:
        return False
    suffix = path.suffix.lower()
    if suffix not in {".mp4", ".mkv"}:
        return False
    try:
        before = path.stat()
    except OSError:
        return False

    with tempfile.NamedTemporaryFile(
        prefix=".vice-meta-", suffix=suffix, dir=path.parent, delete=False,
    ) as handle:
        tmp = Path(handle.name)
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
    ]
    if readrate is not None:
        # A tag write copies the whole file. A network library should not be
        # saturated by a background remux, so cap the input read at 4× media
        # speed; a 65 s clip takes at least ~16 s, spread out instead of a
        # burst that makes playback and the recorder stutter.
        cmd += ["-readrate", str(readrate)]
    cmd += [
        "-i", str(path),
        "-map", "0", "-map_metadata", "0", "-c", "copy",
    ]
    for key, value in tags.items():
        cmd += ["-metadata", f"{key}={value}"]
    if suffix == ".mp4":
        # With use_metadata_tags ffmpeg also writes the input's format-level
        # brand fields into mdta; leaving them in makes every remux append
        # another copy ("isom;isom;isom"). Clear the derived fields while
        # preserving user tags such as title and comment.
        cmd += [
            "-metadata", "major_brand=",
            "-metadata", "minor_version=",
            "-metadata", "compatible_brands=",
            "-movflags", "+faststart+use_metadata_tags", "-f", "mp4",
        ]
    else:
        cmd += ["-f", "matroska"]
    cmd.append(str(tmp))

    proc = None
    spawn = None
    try:
        spawn = asyncio.create_task(asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.PIPE,
        ))
        proc = await spawn
        _, stderr = await communicate_with_timeout(proc, timeout=timeout)
        if proc.returncode != 0 or not tmp.exists() or tmp.stat().st_size == 0:
            reason = (stderr or b"").decode(errors="replace").strip()[-300:]
            log.debug("Writing metadata to %s failed: %s", path.name,
                      reason or f"ffmpeg exited {proc.returncode}")
            return False
        # Some muxers silently drop tags. Confirm the copy actually carries
        # the membership before it replaces the user's only recording.
        written = await probe_container_tags(tmp)
        if any(written.get(str(key).lower()) != str(value)
               for key, value in tags.items()):
            log.debug("The container did not keep the metadata for %s",
                      path.name)
            return False
        # A clip may be deleted or renamed while its tag is being written. If
        # the file at this path changed at all, the remux read a different
        # video than the one now there, so leave it alone.
        try:
            current = path.stat()
        except OSError:
            return False
        if ((current.st_dev, current.st_ino) != (before.st_dev, before.st_ino)
                or current.st_size != before.st_size
                or current.st_mtime_ns != before.st_mtime_ns):
            return False
        os.chmod(tmp, before.st_mode & 0o777)
        # Stamp the replacement before it is moved into place. Doing it after
        # os.replace leaves a window where the scanner sees a brand-new mtime
        # (and the clip jumps to the top of the gallery), and a failed utime
        # on a network filesystem would leave it that way for good.
        os.utime(tmp, ns=(before.st_atime_ns, before.st_mtime_ns))
        os.replace(tmp, path)
        # Keep the old mtime: playlist edits are not edits to the recording.
        try:
            os.utime(path, ns=(before.st_atime_ns, before.st_mtime_ns))
        except OSError as exc:
            log.debug("Could not restore the mtime of %s: %s", path.name, exc)
        return True
    except (asyncio.TimeoutError, OSError) as exc:
        log.debug("Writing metadata to %s failed: %s", path.name, exc)
        return False
    finally:
        if proc is None and spawn is not None:
            try:
                proc = await spawn
            except (OSError, asyncio.CancelledError):
                pass
        if proc is not None and proc.returncode is None:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            await proc.communicate()
        tmp.unlink(missing_ok=True)


def _ffprobe_reason(stderr: Optional[bytes], path: Optional[Path] = None) -> str:
    """The most useful line of ffprobe's stderr, short enough for a toast.

    ffprobe prefixes its message with the full path it was given, which is
    redundant next to a log line that already names the file and too long
    for the UI, so it comes off.
    """
    if not stderr:
        return ""
    lines = [
        line.strip()
        for line in stderr.decode("utf-8", "replace").splitlines()
        if line.strip()
    ]
    if not lines:
        return ""
    reason = lines[-1]
    if path:
        prefix = f"{path}: "
        if reason.startswith(prefix):
            reason = reason[len(prefix):]
    return reason[:300]


def _parse_duration(raw) -> float:
    try:
        value = float(raw)
    except (TypeError, ValueError):
        return 0.0
    return value if math.isfinite(value) and value > 0 else 0.0


def _duration_from_video_samples(stream: dict) -> float:
    """Estimate duration from a video stream's sample count and frame rate."""
    try:
        frames = int(stream.get("nb_frames") or 0)
    except (TypeError, ValueError):
        return 0.0
    if frames <= 0:
        return 0.0

    for raw_rate in (stream.get("avg_frame_rate"), stream.get("r_frame_rate")):
        try:
            rate = float(Fraction(str(raw_rate)))
        except (TypeError, ValueError, ZeroDivisionError):
            continue
        if math.isfinite(rate) and 0 < rate <= 1000:
            estimate = frames / rate
            if math.isfinite(estimate) and estimate > 0:
                return estimate
    return 0.0


async def _readable_video_packets(path: Path) -> Optional[int]:
    """How many video packets FFmpeg can actually read, or None if unknown.

    Demuxes without decoding, so it costs a read of the file. None means no
    opinion, and callers must then behave exactly as they did without it.
    """
    try:
        proc = await asyncio.create_subprocess_exec(
            "ffprobe", "-v", "error", "-count_packets", "-select_streams", "v:0",
            "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0", str(path),
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        stdout, _ = await communicate_with_timeout(proc, timeout=30)
        return int(stdout.decode().strip().split(",")[0])
    except Exception as exc:
        log.debug("Could not count packets in %s: %s", path.name, exc)
        return None


async def get_duration(path: Path) -> float:
    """Duration of *path* in seconds, or 0.0 when it cannot be read."""
    meta = await probe_media(path)
    return meta["duration"] if meta else 0.0


def cleanup_temp_files(directory: Path) -> None:
    """Delete leftover in-place-edit temp files from interrupted runs."""
    if not directory.is_dir():
        return
    # One directory walk, not one glob per pattern. On a network clip folder
    # globbing the same huge directory a dozen times is slow and noisy.
    try:
        with os.scandir(directory) as iterator:
            entries = list(iterator)
    except OSError as exc:
        log.warning("Could not check %s for stale temp files: %s", directory, exc)
        return
    for entry in entries:
        if not any(fnmatch.fnmatch(entry.name, pattern)
                   for pattern in TEMP_FILE_GLOBS):
            continue
        try:
            Path(entry.path).unlink()
            log.info("Removed stale temp file %s", entry.name)
        except OSError as exc:
            log.warning("Could not remove stale temp file %s: %s",
                        entry.path, exc)
