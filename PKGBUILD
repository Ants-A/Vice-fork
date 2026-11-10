# Maintainer: Andrew Marin <andrewmarin367@gmail.com>
pkgname=vice-clipper
pkgver=2.17.0
pkgrel=1
pkgdesc="Medal.tv-style game clip recorder for Linux: instant replay, session recording, and one-click sharing"
arch=('x86_64')
url="https://github.com/splitshotz/Vice"
license=('GPL-3.0-or-later')
install=vice-clipper.install
depends=(
    'systemd' 'libayatana-appindicator' 'gtk-layer-shell' 'gtk3' 'webkit2gtk-4.1' 'ffmpeg' 'gpu-screen-recorder'
    'gstreamer' 'gst-plugins-base' 'gst-plugins-good' 'gst-plugin-va' 'gst-plugin-dav1d' 'gst-libav'
    'wl-clipboard' 'xclip' 'cloudflared' 'xdotool' 'xorg-xprop' 'wmctrl'
)
optdepends=(
    'wf-recorder: Wayland fallback recording backend'
    'kdotool: game detection on KDE Plasma Wayland'
)
makedepends=('rust' 'cargo' 'pkgconf')
source=("$pkgname-$pkgver.tar.gz::https://github.com/splitshotz/Vice/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
    cd "$srcdir/Vice-$pkgver"
    export CARGO_HOME="$srcdir/cargo-home"
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/^host: //p')"
}
build() {
    cd "$srcdir/Vice-$pkgver"
    export CARGO_HOME="$srcdir/cargo-home"
    cargo build --release --frozen
}
check() {
    cd "$srcdir/Vice-$pkgver"
    export CARGO_HOME="$srcdir/cargo-home"
    cargo test --release --frozen
}
package() {
    cd "$srcdir/Vice-$pkgver"
    install -Dm755 target/release/vice "$pkgdir/usr/bin/vice"
    install -Dm755 target/release/vice-app "$pkgdir/usr/bin/vice-app"
    for resource in ui fonts data; do
        install -d "$pkgdir/usr/share/vice/$resource"
        cp -a "vice/$resource/." "$pkgdir/usr/share/vice/$resource/"
    done
    install -Dm644 vice.desktop "$pkgdir/usr/share/applications/vice.desktop"
    install -Dm644 assets/vice.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/vice.svg"
    install -Dm644 packaging/vice.rules "$pkgdir/usr/lib/udev/rules.d/70-vice-input.rules"
    install -Dm644 packaging/vice.service "$pkgdir/usr/lib/systemd/user/vice.service"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
