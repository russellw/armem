#!/bin/sh
# Install herdtools7 (herd7, diy7, ...) into ~/.local/opt without root.
#
# Uses Ubuntu's herdtools7 package for the binaries and the matching
# upstream release for the model library (libdir), which the package lacks.
set -eu

VERSION="${HERDTOOLS_VERSION:-7.58}"
PREFIX="${PREFIX:-$HOME/.local/opt}"

mkdir -p "$PREFIX/herdtools7-deb"
cd "$PREFIX/herdtools7-deb"
if ! ls herdtools7_*.deb >/dev/null 2>&1; then
    apt download herdtools7
fi
dpkg -x herdtools7_*.deb "$PREFIX/herdtools7"

if [ ! -d "$PREFIX/herdtools7-src" ]; then
    git clone --depth 1 --branch "$VERSION" https://github.com/herd/herdtools7.git "$PREFIX/herdtools7-src"
fi

echo "herd7:  $PREFIX/herdtools7/usr/bin/herd7 ($("$PREFIX/herdtools7/usr/bin/herd7" -version))"
echo "libdir: $PREFIX/herdtools7-src/herd/libdir"
echo "If the packaged version differs from $VERSION, set HERDTOOLS_VERSION to match it."
