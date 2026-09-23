#!/bin/sh
# Builds a .deb for fourttyswipe. Usage: scripts/build-deb.sh <arch>
# <arch> is a Debian arch name (arm64, amd64, armhf, i386) mapped to a
# rustup target triple below.
set -e

ARCH="${1:-arm64}"
VERSION="0.6.0"

case "$ARCH" in
  arm64) TARGET=aarch64-unknown-linux-gnu ;;
  amd64) TARGET=x86_64-unknown-linux-gnu ;;
  armhf) TARGET=armv7-unknown-linux-gnueabihf ;;
  i386)  TARGET=i686-unknown-linux-gnu ;;
  *) echo "unknown arch: $ARCH" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."
cargo build --release --target "$TARGET"

PKGROOT=$(mktemp -d)
mkdir -p "$PKGROOT/DEBIAN" "$PKGROOT/usr/bin" "$PKGROOT/lib/systemd/system"
cp "target/$TARGET/release/fourttyswipe" "$PKGROOT/usr/bin/fourttyswipe"
chmod 755 "$PKGROOT/usr/bin/fourttyswipe"
cp fourttyswipe.service "$PKGROOT/lib/systemd/system/fourttyswipe.service"

INSTALLED_SIZE=$(du -sk "$PKGROOT/usr" "$PKGROOT/lib" | awk '{sum+=$1} END {print sum}')

cat > "$PKGROOT/DEBIAN/control" << EOF
Package: fourttyswipe
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Depends: libc6, libgcc-s1
Installed-Size: $INSTALLED_SIZE
Maintainer: HeroineOS <noreply@heroineos.github.io>
Homepage: https://github.com/HeroineOS/fourttyswipe
Description: Four-finger swipe between active TTYs
 Reads a touchscreen directly via evdev and switches virtual
 terminals via kernel VT ioctls, independent of any compositor,
 window manager, or display server. Part of the HeroineOS
 fourswipe project.
EOF

cat > "$PKGROOT/DEBIAN/postinst" << 'EOF'
#!/bin/sh
set -e
systemctl daemon-reload || true
echo "fourttyswipe installed. Enable with: systemctl enable --now fourttyswipe"
EOF
chmod 755 "$PKGROOT/DEBIAN/postinst"

OUT="$(pwd)/dist"
mkdir -p "$OUT"
DEBNAME="fourttyswipe_${VERSION}_${ARCH}"
BUILD=$(mktemp -d)
echo "2.0" > "$BUILD/debian-binary"
fakeroot tar -C "$PKGROOT" --owner=0 --group=0 -czf "$BUILD/control.tar.gz" DEBIAN --transform 's,^DEBIAN,.,'
fakeroot tar -C "$PKGROOT" --owner=0 --group=0 -czf "$BUILD/data.tar.gz" usr lib
(cd "$BUILD" && ar rcs "$OUT/$DEBNAME.deb" debian-binary control.tar.gz data.tar.gz)

echo "built: $OUT/$DEBNAME.deb"
