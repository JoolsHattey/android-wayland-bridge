#!/usr/bin/env bash
# Builds waypipe for Android (x86_64) as OUT_DIR/x86_64/libwaypipe.so, an
# executable the app runs from its native library directory (see
# dev.awb.VmDisplay). Same version as Debian trixie's waypipe, which the VM
# runs, so the two ends speak the same protocol. No compression, dmabuf or
# video: the VM sends shm buffers over a local link.
# Usage: build-waypipe.sh NDK_DIR OUT_DIR
set -euo pipefail
NDK=$1
OUT=$2
VERSION=0.9.2
SHA256=76d7b34ced11dca56a6db878ec84f78d1eef1f93b33b1ad7336f0794b73955c2
URL=https://deb.debian.org/debian/pool/main/w/waypipe/waypipe_$VERSION.orig.tar.gz

WORK=$(dirname "$OUT")/waypipe-src
mkdir -p "$WORK"
TARBALL=$WORK/waypipe_$VERSION.orig.tar.gz
[ -f "$TARBALL" ] || curl -fsSL -o "$TARBALL" "$URL"
echo "$SHA256  $TARBALL" | sha256sum -c --quiet
SRC=$WORK/waypipe-v$VERSION
[ -d "$SRC" ] || tar -xzf "$TARBALL" -C "$WORK"

GEN=$WORK/gen
mkdir -p "$GEN" "$OUT/x86_64"
python3 -I "$SRC/protocols/symgen.py" data "$SRC/protocols/function_list.txt" "$GEN/protocols.c" "$SRC"/protocols/*.xml
python3 -I "$SRC/protocols/symgen.py" header "$SRC/protocols/function_list.txt" "$GEN/protocols.h" "$SRC"/protocols/*.xml
printf '%s\n' "#define WAYPIPE_VERSION \"$VERSION-awb\"" '#define HAVE_AVX512F 1' '#define HAVE_AVX2 1' \
    '#define HAVE_SSE3 1' '#define HAS_VSOCK 1' > "$GEN/config-waypipe.h"

CC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android35-clang
CFLAGS=(-D_DEFAULT_SOURCE -O2 -fPIE -I"$GEN" -I"$SRC/protocols" -I"$SRC/src")
# The SIMD diff kernels, picked at run time (as waypipe's meson build does).
"$CC" "${CFLAGS[@]}" -c "$SRC/src/kernel_avx512f.c" -mavx512f -mlzcnt -mbmi -o "$GEN/kernel_avx512f.o"
"$CC" "${CFLAGS[@]}" -c "$SRC/src/kernel_avx2.c" -mavx2 -mlzcnt -mbmi -o "$GEN/kernel_avx2.o"
"$CC" "${CFLAGS[@]}" -c "$SRC/src/kernel_sse3.c" -msse3 -o "$GEN/kernel_sse3.o"
"$CC" "${CFLAGS[@]}" -pie -s -o "$OUT/x86_64/libwaypipe.so" "$GEN/protocols.c" \
    "$SRC"/src/{bench,client,dmabuf,handlers,interval,kernel,mainloop,parsing,platform,server,shadow,util,video,waypipe}.c \
    "$GEN"/kernel_{avx512f,avx2,sse3}.o
