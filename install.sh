#!/usr/bin/env bash
# Build Vice's Rust binaries and install them for the current user.
set -euo pipefail

prefix="${VICE_INSTALL_PREFIX:-$HOME/.local}"
install_deps=1
install_service=1
assume_yes=0
uninstall=0
allow_mixed=0
usage() {
    cat <<'USAGE'
Usage: ./install.sh [--prefix DIR] [--no-deps] [--no-service] [--yes]
       ./install.sh --uninstall [--prefix DIR]

Builds the Rust daemon and desktop window, then installs binaries and assets.
Requires a current Rust toolchain. Node is needed only to rebuild the React UI.
--no-deps       Skip installing system build/runtime packages.
--no-service    Do not enable the systemd user service.
--yes           Accept package-manager and startup-service prompts.
--allow-mixed-install  Allow installing alongside an existing distro package.
--uninstall     Remove this user's installation; preserve clips and settings.
USAGE
}
while (($#)); do
    case "$1" in
        --prefix) [[ $# -ge 2 ]] || { usage >&2; exit 2; }; prefix=$2; shift ;;
        --no-deps) install_deps=0 ;;
        --no-service) install_service=0 ;;
        --yes|-y) assume_yes=1 ;;
        --uninstall) uninstall=1 ;;
        --allow-mixed-install) allow_mixed=1 ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
[[ "$prefix" = /* && "$prefix" != / ]] || { printf 'Prefix must be an absolute directory other than /.\n' >&2; exit 2; }
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="$prefix/bin"
data_dir="$prefix/share/vice"
service_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
service_file="$service_dir/vice.service"

if ((uninstall)); then
    if command -v systemctl >/dev/null; then
        systemctl --user disable --now vice.service 2>/dev/null || true
    fi
    # Each path is an artifact installed by this script; user data is separate.
    rm -f -- "$bin_dir/vice" "$bin_dir/vice-app" "$prefix/share/applications/vice.desktop" \
        "$prefix/share/icons/hicolor/scalable/apps/vice.svg" "$service_file"
    if [[ -d "$data_dir" ]]; then rm -rf -- "$data_dir/ui" "$data_dir/fonts" "$data_dir/data"; fi
    if command -v systemctl >/dev/null; then systemctl --user daemon-reload 2>/dev/null || true; fi
    printf 'Vice removed. Recordings, config and library metadata are preserved.\n'
    exit 0
fi
if [[ -e /run/ostree-booted ]]; then
    printf 'Use the Vice Flatpak on immutable desktops:\n  flatpak install --user https://splitshotz.github.io/vice-flatpak/vice.flatpakref\n' >&2
    exit 1
fi
if (( !allow_mixed )) && command -v pacman >/dev/null && pacman -Q vice-clipper &>/dev/null; then
    printf 'vice-clipper is already installed by pacman. Update it through your package manager.\n' >&2
    exit 1
fi
if ((install_deps)); then
    command -v sudo >/dev/null || { printf 'Install system dependencies first, then use --no-deps.\n' >&2; exit 1; }
    if command -v pacman >/dev/null; then
        pacman_flags=(); (( !assume_yes )) || pacman_flags+=(--noconfirm)
        sudo pacman -S --needed "${pacman_flags[@]}" \
            base-devel rust cargo libayatana-appindicator gtk-layer-shell gtk3 webkit2gtk-4.1 ffmpeg gpu-screen-recorder \
            gst-plugins-base gst-plugins-good gst-plugin-va gst-libav wl-clipboard xclip xdotool xorg-xprop wmctrl cloudflared
    elif command -v apt-get >/dev/null; then
        sudo apt-get update
        apt_flags=(); (( !assume_yes )) || apt_flags+=(-y)
        sudo apt-get install "${apt_flags[@]}" build-essential pkg-config cargo rustc \
            libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-1 libgtk-layer-shell0 ffmpeg gstreamer1.0-plugins-base \
            gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav wl-clipboard xclip xdotool x11-utils wmctrl
    elif command -v dnf >/dev/null; then
        dnf_flags=(); (( !assume_yes )) || dnf_flags+=(-y)
        sudo dnf install "${dnf_flags[@]}" gcc gcc-c++ pkgconf-pkg-config rust cargo \
            gtk3-devel webkit2gtk4.1-devel libayatana-appindicator-gtk3 gtk-layer-shell ffmpeg-free gstreamer1-plugins-base \
            gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugin-libav wl-clipboard xclip xdotool xprop wmctrl
    elif command -v zypper >/dev/null; then
        zypper_flags=(); (( !assume_yes )) || zypper_flags+=(--non-interactive)
        sudo zypper "${zypper_flags[@]}" install gcc gcc-c++ pkgconf-pkg-config rust cargo \
            gtk3-devel webkit2gtk3-devel libappindicator3-1 gtk-layer-shell ffmpeg gstreamer-plugins-base \
            gstreamer-plugins-good gstreamer-plugins-bad gstreamer-plugins-libav wl-clipboard xclip xdotool xprop wmctrl
    else
        printf 'No supported package manager. Install Rust, pkg-config, GTK3, WebKit2GTK 4.1 and FFmpeg, then use --no-deps.\n' >&2
        exit 1
    fi
fi
for tool in cargo rustc pkg-config ffmpeg ffprobe; do
    command -v "$tool" >/dev/null || { printf 'Required tool missing: %s\n' "$tool" >&2; exit 1; }
done
pkg-config --exists gtk+-3.0 webkit2gtk-4.1 || { printf 'GTK3 and WebKit2GTK 4.1 development packages are required.\n' >&2; exit 1; }
cd "$repo_dir"
# The committed bundle is usable without Node. Rebuild after editing ui-src.
if [[ ! -s vice/ui/scripts/app.js ]]; then
    command -v npm >/dev/null || { printf 'UI bundle missing; install Node.js/npm and run npm ci && npm run build.\n' >&2; exit 1; }
    npm ci
    npm run build
fi
cargo build --release --locked
install -Dm755 target/release/vice "$bin_dir/vice"
install -Dm755 target/release/vice-app "$bin_dir/vice-app"
for resource in ui fonts data; do
    mkdir -p "$data_dir/$resource"
    cp -a "vice/$resource/." "$data_dir/$resource/"
done
install -Dm644 assets/vice.svg "$prefix/share/icons/hicolor/scalable/apps/vice.svg"
mkdir -p "$prefix/share/applications"
sed "s|^Exec=.*|Exec=\"$bin_dir/vice-app\" %u|" vice.desktop > "$prefix/share/applications/vice.desktop"
if ((install_deps)); then
    sudo install -Dm644 packaging/vice.rules /etc/udev/rules.d/70-vice-input.rules
    sudo udevadm control --reload-rules || true
    sudo udevadm trigger --subsystem-match=input || true
fi
if command -v systemctl >/dev/null && systemctl --user show-environment &>/dev/null; then
    systemctl --user stop vice.service 2>/dev/null || true
    if ((install_service)); then
        answer=y
        if (( !assume_yes )) && [[ ! -f "$service_file" ]]; then
            read -r -p 'Enable Vice recording at login? [Y/n] ' answer
        fi
        if [[ "${answer,,}" != n && "${answer,,}" != no ]]; then
            mkdir -p "$service_dir"
            sed "s|^ExecStart=.*|ExecStart=\"$bin_dir/vice\" start --no-open-ui|" packaging/vice.service > "$service_file"
            systemctl --user daemon-reload
            systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XAUTHORITY XDG_CURRENT_DESKTOP || true
            systemctl --user reenable vice.service
            systemctl --user restart vice.service
        fi
    fi
fi
command -v update-desktop-database >/dev/null && update-desktop-database "$prefix/share/applications" || true
printf '\nInstalled Vice to %s. Launch %s/vice-app.\n' "$prefix" "$bin_dir"
if ! command -v gpu-screen-recorder >/dev/null; then
    printf 'Install gpu-screen-recorder for GPU capture (or its Flathub package); FFmpeg X11 is the fallback.\n'
fi
printf 'Uninstall with: %s/install.sh --uninstall --prefix "%s"\n' "$repo_dir" "$prefix"
