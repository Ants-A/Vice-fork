<p align="center">
  <img src="assets/vice-wordmark.png" width="420" alt="Vice"/>
</p>

<p align="center"><b>Instant-replay game clipping for Linux.</b><br/>
Press F9 to save the last 20 seconds of gameplay.</p>

Vice now runs on Rust: the daemon, recording control, HTTP/WebSocket API, native desktop shell, global hotkeys, sharing, playlists, media processing, and timeline export. The existing React/TypeScript interface is preserved. Python is no longer required to build or run Vice.

## Build and run

Install a current stable Rust toolchain and your distribution's development packages for GTK 3, WebKit2GTK 4.1, JavaScriptCoreGTK 4.1, libsoup 3, OpenSSL, and pkg-config. On Debian/Ubuntu:

```sh
sudo apt install build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev libssl-dev ffmpeg
cargo build --release --locked
./target/release/vice-app
```

The checked-in UI bundle means Node is optional. To rebuild the interface:

```sh
npm ci
npm run typecheck
npm run build
```

Install `gpu-screen-recorder` for GPU replay capture. Vice can also use `wf-recorder` on supported Wayland compositors or FFmpeg's X11 capture. `ffmpeg` and `ffprobe` handle media processing. Optional tools:

| Tool | Purpose |
| --- | --- |
| `cloudflared` | Public share links through a quick tunnel |
| `wl-clipboard` / `xclip` | Clipboard on Wayland / X11 |
| `xdotool`, `xprop`, `wmctrl` | X11 game and display detection |
| `kdotool` | KDE Wayland window detection |
| `gtk-layer-shell` | Keyboard overlay on supported Wayland compositors |
| Ayatana AppIndicator / AppIndicator | Native system tray |

Global hotkeys read Linux input devices. The supplied udev rule grants access to the active desktop user; some systems require membership in the `input` group and a new login. `vice doctor` reports dependency and hotkey availability.

A build without native GTK/WebKit dependencies is available for headless daemons:

```sh
cargo build --release --locked --no-default-features
```

In that build `vice-app` opens the interface in your browser. Live keyboard overlays and burning keyboard overlays into exports require the default desktop feature.

## Install

```sh
./install.sh
```

The installer builds the Rust binaries and installs them, the prebuilt UI, fonts, desktop launcher, icon, and user service. Run `./install.sh --help` for custom prefixes, dependency installation, and service options. To remove a script installation, run `./install.sh --uninstall`.

Arch packaging uses `PKGBUILD`; NixOS uses the flake and `nix/module.nix`; the Flatpak manifest lives in `packaging/flatpak/`. Each now builds the Cargo crate. Full distribution package builds require their respective build environments.

## Use

```sh
vice-app                    # native desktop interface; starts daemon if needed
vice start --no-open-ui     # foreground daemon, suitable for systemd
vice clip                   # configured clip duration
vice clip 60                # last 60 seconds, within the configured buffer
vice clip --duration 60
vice session                # start/stop a full-session recording
vice status
vice doctor
vice config
vice open-config
vice list-keys --filter F9
vice clips
vice stop
```

F9 saves a clip; a double tap starts or ends a session. During a session, a single tap adds a highlight. Additional hotkeys can save different durations. Settings cover recording backends, display selection, following the pointer, game-window capture on X11, audio tracks, microphone levels, naming templates, shortcuts, Discord presence, notifications, and keyboard overlays.

The native window has no system title bar. **Settings → Appearance → UI scale** adjusts the desktop client from 75% to 200% and remembers the choice.

Video conversions try GPU encoders that pass a real capability check, then fall back to software if a job fails. This includes Discord-sized copies, timeline exports, preview conversions, and re-encoded trims. Audio operations and stream-copy remuxes retain their existing paths. `vice doctor` lists working GPU encoders. Discord-copy progress shows the active encoder, and newly encoded copies use 8-bit H.264 for compatibility.

Playback shows buffered ranges and preparation progress. Previews stream during conversion and support range requests once cached. Playable recordings stream at their original quality with bounded read-ahead. **Smaller preview** is optional. Hover previews stop their downloads when the viewer opens. Native hardware decoding is enabled by default; Arch installs include `gst-plugin-va` for VA-API decoding on AMD and Intel. Engines without H.264 support use a WebM preview. Clip metadata and thumbnails are cached by file identity so large libraries do not need repeated probes.

The interface retains the clip and image libraries, playlists, highlights, trim and rename controls, screenshots and frame capture, sharing, and the multi-track timeline editor. Exports use FFmpeg for video, text, transitions, detached audio, and optional recorded keys.

Closing the window leaves the daemon recording. The native tray is available when the desktop provides a StatusNotifier watcher and an AppIndicator library. Public share links require either `cloudflared`, a configured public base URL, or access to the machine's public share port. Without a tunnel or reverse proxy, links are limited to your network.

## Configuration and existing libraries

The Rust backend reads the existing TOML schema from `~/.config/vice/config.toml`. Defaults include:

```toml
[recording]
buffer_duration = 120
clip_duration = 20
fps = 60
backend = "auto"
capture_audio = true
container = "mp4"

[hotkeys]
clip = "KEY_F9"

[sharing]
enabled = true
port = 8765
cloudflare_tunnel = true
```

Clips default to `~/Videos/Vice`, images to `~/Pictures/Vice`. Existing `playlists.json`, `share_tokens.json`, `views.json`, `ui_state.json`, `editor_project.json`, highlights, and key sidecars retain their names and formats under `~/.local/share/vice`. Embedded playlist metadata can restore membership from media files. Media changes use temporary files and validation before replacing originals.

The control API listens only on `127.0.0.1:8765`; a separate public server defaults to port 8766. The public server exposes only token-based share pages, video, and thumbnails. It has no control API or WebSocket. Unix IPC uses `$XDG_RUNTIME_DIR/vice.sock` with an exclusive ownership lock and user-only socket permissions. Stop an older Python daemon before starting this version, because its legacy IPC path differs.

For isolated development and tests, `VICE_CONFIG` selects a config file, `VICE_DATA_DIR` selects writable state, `VICE_CACHE_DIR` selects cache storage, and `VICE_RESOURCE_DIR` selects installed `ui/`, `fonts/`, and `data/` resources. `VICE_ASSET_DIR` overrides only the UI directory.

## Development and verification

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test --no-default-features --all-targets
npm run test:player
npm run typecheck
npm run build
# With a Chromium-compatible browser and target/debug/vice built:
VICE_SMOKE_BROWSER=/path/to/chromium npm run test:browser
```

Rust tests cover configuration, input dispatch, process ownership, Discord IPC framing, playlist lifecycle, public API isolation, and real FFmpeg media/export operations. The daemon integration test uses isolated state and an intentionally unavailable capture backend, so it does not record your desktop. Local sockets must be permitted for the IPC and server tests. Hardware capture, native window presentation, and compositor-specific behavior still need testing on the target desktop; automated media fixtures do not establish GPU or compositor compatibility.

`rust-src/` contains the runtime. `ui-src/` contains the interface. `vice/ui`, `vice/fonts`, and `vice/data` contain resources. The previous Python implementation and its tests are retained in `legacy/python/` as migration references and are not part of the runtime or build.

GPL-3.0-or-later. See [LICENSE](LICENSE).
