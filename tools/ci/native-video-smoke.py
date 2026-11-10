#!/usr/bin/env python3
"""Check actual WebKitGTK video colours under the Rust NVIDIA playback policy.

Requires a desktop session, Python GObject/WebKit2 bindings, and FFmpeg.
Run: cargo run --example native_video_policy -- --nvidia > /tmp/vice-video-policy.json
     python3 tools/ci/native-video-smoke.py /tmp/vice-video-policy.json
Uses generated media and a private local server, never the user's recordings.
"""
import functools
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

policy = json.loads(Path(sys.argv[1]).read_text())
os.environ.update(policy)
# Apply the environment before loading GTK/WebKit or starting video processes.
import gi

gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import GLib, Gtk, WebKit2

with tempfile.TemporaryDirectory(prefix="vice-native-colours-") as root:
    root = Path(root)
    codec = sys.argv[2] if len(sys.argv) > 2 else "h264"
    encoders = {"h264": ["libx264"], "hevc": ["libx265", "-x265-params", "pools=none:frame-threads=1:log-level=error"], "av1": ["libaom-av1", "-cpu-used", "8"]}
    fixture = root / "bars.mp4"
    subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "smptebars=size=320x180:rate=10", "-t", "2", "-c:v", *encoders[codec], "-threads", "1", "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-y", str(fixture)], check=True)
    reference = subprocess.check_output(["ffmpeg", "-v", "error", "-i", str(fixture), "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
    points = [(round(x * 320), 36) for x in (0.07, 0.21, 0.35, 0.5, 0.64, 0.78, 0.92)]
    expected = [list(reference[(y * 320 + x) * 3:(y * 320 + x) * 3 + 3]) for x, y in points]

    class Handler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(root)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (root / "index.html").write_text('<!doctype html><body style="margin:0;background:#111"><video id="video" muted playsinline preload="auto" src="/bars.mp4" style="display:block;width:320px;height:180px"></video>')
    window = Gtk.Window(title="Vice native colour check")
    window.set_default_size(400, 260)
    window.set_decorated(False)
    view = WebKit2.WebView()
    window.add(view)
    result = {"ok": False, "error": "native video colour check timed out"}
    busy = False
    captured = False
    started = time.monotonic()

    def snapshot_done(webview, task, _data):
        global result
        try:
            surface = webview.get_snapshot_finish(task)
            pixels = bytes(surface.get_data())
            stride = surface.get_stride()
            actual = []
            for x, y in points:
                index = y * stride + x * 4
                pixel = pixels[index:index + 4]
                actual.append([pixel[2], pixel[1], pixel[0]] if sys.byteorder == "little" else list(pixel[1:4]))
            error = max(abs(a - e) for rgb, target in zip(actual, expected) for a, e in zip(rgb, target))
            screenshot = os.environ.get("VICE_NATIVE_SCREENSHOT", f"/tmp/vice-native-colours-{codec}.png")
            surface.write_to_png(screenshot)
            result = {"ok": error < 32, "codec": codec, "maxChannelError": error, "expected": expected, "rendered": actual, "screenshot": screenshot}
        except Exception as error:
            result = {"ok": False, "error": str(error)}
        Gtk.main_quit()

    def evaluated(webview, task, _data):
        global busy, captured, result
        busy = False
        try:
            state = json.loads(webview.evaluate_javascript_finish(task).to_json(0))
            if state["ready"] and not captured:
                captured = True
                # Let the decoded first frame reach the software video sink.
                GLib.timeout_add(200, lambda: (webview.get_snapshot(WebKit2.SnapshotRegion.VISIBLE, WebKit2.SnapshotOptions.NONE, None, snapshot_done, None), False)[1])
        except Exception as error:
            result = {"ok": False, "error": str(error)}
            Gtk.main_quit()

    def poll():
        global busy
        if time.monotonic() - started > 15:
            Gtk.main_quit()
            return False
        if not busy and not captured:
            busy = True
            view.evaluate_javascript("({ready:Boolean(document.querySelector('video')?.readyState>=2&&document.querySelector('video')?.videoWidth===320)})", -1, None, None, None, evaluated, None)
        return True

    view.load_uri(f"http://127.0.0.1:{server.server_port}/index.html")
    window.show_all()
    GLib.timeout_add(150, poll)
    Gtk.main()
    window.destroy()
    server.shutdown()
    print(json.dumps(result, indent=2))
    sys.exit(0 if result["ok"] else 1)
