#!/bin/bash
# Build repro.cc against gRPC core from the grpc Ruby source gem, which
# carries the core and all its dependencies with a Makefile.
#
# usage: build.sh WORK_DIR OUTPUT [PATCH...]
#
# Fetches grpc-$VERSION (default 1.85.0) into WORK_DIR unless it is there,
# restores src/core/server/server.cc, applies the PATCHes (delay-hook.patch,
# fix-recheck.patch) to it, builds libgrpc.a (about 20 minutes the first
# time on 4 cores; later builds recompile only server.cc), and links OUTPUT.
# Needs gem, make, g++ and a few GB of disk.

set -eu
work=$1 out=$2
shift 2
here=$(cd "$(dirname "$0")" && pwd)
version=${VERSION:-1.85.0}
mkdir -p "$work"
work=$(cd "$work" && pwd)
src=$work/grpc-$version
if [ ! -d "$src" ]; then
	(cd "$work" && gem fetch grpc -v "$version" --platform ruby &&
		gem unpack "grpc-$version.gem")
	cp "$src/src/core/server/server.cc" "$work/server.cc.orig"
fi
cp "$work/server.cc.orig" "$src/src/core/server/server.cc"
for p in "$@"; do
	patch -d "$src" -p1 <"$(cd "$(dirname "$p")" && pwd)/$(basename "$p")"
done
# If make fails, remove the archive, which may be half written, so that the
# next run doesn't take it as up to date.
lib=$src/libs/opt
make -C "$src" -j"$(nproc)" "$lib/libgrpc.a" CONFIG=opt \
	EMBED_OPENSSL=true EMBED_ZLIB=true >"$work/build.log" 2>&1 ||
	{ rm -f "$lib/libgrpc.a"; tail -20 "$work/build.log"; exit 1; }
g++ -std=c++17 -O2 -s -o "$out" "$here/repro.cc" -I"$src/include" \
	"$lib/libgrpc.a" "$lib/libboringssl.a" "$lib/libcares.a" "$lib/libz.a" \
	-lpthread
echo "built $out"
