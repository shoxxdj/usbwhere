#!/usr/bin/env bash
# Build a .deb from an already-built usbwhere binary.
# Run from the repository root.
#
# Usage: build-deb.sh VERSION DEB_ARCH BINARY MAINTAINER OUT_DIR
#   VERSION     e.g. 0.1.0 or 0.2.0-rc1 (becomes 0.2.0~rc1 for dpkg)
#   DEB_ARCH    amd64 or arm64
#   MAINTAINER  "Name <email>"
set -euo pipefail

if [ "$#" -ne 5 ]; then
    echo "usage: $0 VERSION DEB_ARCH BINARY MAINTAINER OUT_DIR" >&2
    exit 2
fi
version=$1 arch=$2 binary=$3 maintainer=$4 out=$5

deb_version=${version//-/'~'}
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
chmod 755 "$root"

install -Dm755 "$binary" "$root/usr/bin/usbwhere"
install -Dm644 Readme.md "$root/usr/share/doc/usbwhere/README.md"
for f in LICENSE*; do
    [ -f "$f" ] && install -Dm644 "$f" "$root/usr/share/doc/usbwhere/$f"
done

mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<EOF
Package: usbwhere
Version: $deb_version
Section: utils
Priority: optional
Architecture: $arch
Depends: libudev1
Maintainer: $maintainer
Installed-Size: $(du -sk "$root" | cut -f1)
Homepage: https://github.com/shoxxdj/usbwhere
Description: Show where each USB device can be accessed
 For every USB device and interface, shows the bound driver and the
 matching nodes (/dev/sdX with its mount point, /dev/videoN, /dev/ttyACMN,
 network interfaces, sound cards). Has a --watch mode based on udev.
EOF

mkdir -p "$out"
dpkg-deb --root-owner-group --build "$root" "$out/usbwhere_${deb_version}_${arch}.deb"
