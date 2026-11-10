#!/usr/bin/env bash
# Build the Arch package from the committed tree, the way the AUR will build
# it from the release tag, so a broken PKGBUILD shows up on the pull request
# rather than on everyone's machine after the AUR push.
#
# Runs as root in an archlinux:base-devel container, from the repo root:
#   docker run --rm -v "$PWD":/src -w /src archlinux:base-devel tools/ci/arch-package.sh
set -euo pipefail

repo="$(pwd)"
srcinfo_field() { sed -n "s/^\t$1 = //p" "$repo/.SRCINFO"; }

pacman -Syu --noconfirm --needed git $(srcinfo_field makedepends) $(srcinfo_field depends) >/dev/null

# makepkg refuses to run as root.
id builder &>/dev/null || useradd -m builder
work="$(mktemp -d)"
chown builder: "$work"
git config --global --add safe.directory "$repo"

echo "==> .SRCINFO matches PKGBUILD"
cp "$repo/PKGBUILD" "$repo/vice-clipper.install" "$work/"
if ! diff -u "$repo/.SRCINFO" <(cd "$work" && runuser -u builder -- makepkg --printsrcinfo); then
    echo "::error::.SRCINFO is out of date. Run: makepkg --printsrcinfo > .SRCINFO"
    exit 1
fi

echo "==> every dependency exists in the Arch repositories"
missing=0
for dep in $(srcinfo_field depends); do
    if ! pacman -Si "$dep" &>/dev/null; then
        echo "::error::depends on '$dep', which no Arch repository provides"
        missing=1
    fi
done
[ "$missing" -eq 0 ]

echo "==> build from the committed tree"
pkgver="$(srcinfo_field pkgver)"
tarball="$work/vice-clipper-$pkgver.tar.gz"
git -C "$repo" archive --format=tar.gz --prefix="Vice-$pkgver/" -o "$tarball" HEAD
sha="$(sha256sum "$tarball" | cut -d' ' -f1)"
# The real source is the release tag, which does not exist yet for a pull
# request, so point the recipe at the tree being tested instead.
sed -i "s|^source=.*|source=(\"vice-clipper-$pkgver.tar.gz\")|;s|^sha256sums=.*|sha256sums=('$sha')|" "$work/PKGBUILD"
chown builder: "$work"/*
(cd "$work" && runuser -u builder -- makepkg --nodeps --noconfirm)

echo "==> package contents"
pkg="$(ls "$work"/vice-clipper-"$pkgver"-*.pkg.tar.zst)"
listing="$(tar -tf "$pkg")"
for path in usr/bin/vice usr/bin/vice-app \
            usr/lib/systemd/user/vice.service \
            usr/lib/udev/rules.d/70-vice-input.rules \
            usr/share/applications/vice.desktop \
            usr/share/icons/hicolor/scalable/apps/vice.svg; do
    if ! grep -qx "$path" <<<"$listing"; then
        echo "::error::the package is missing $path"
        exit 1
    fi
done
for path in usr/share/vice/ui/index.html usr/share/vice/fonts/Geist-Regular.ttf usr/share/vice/data/games.json; do
    if ! grep -qx "$path" <<<"$listing"; then
        echo "::error::the package is missing $path"
        exit 1
    fi
done
built="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)"
if [ "$built" != "$pkgver" ]; then
    echo "::error::PKGBUILD says $pkgver but Cargo.toml reports $built"
    exit 1
fi
echo "Built $(basename "$pkg")"
