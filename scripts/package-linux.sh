#!/usr/bin/env bash
# Builds dist/waveshelf-<version>-linux-<arch>.tar.gz on a Linux machine: the
# binary plus the LGPL FFmpeg shared libraries in lib/ (found via $ORIGIN),
# a .desktop file and licenses. Run it inside scripts/linux/Dockerfile (see
# scripts/package-linux-docker.sh) for a build that runs on glibc >= 2.35.
#
# Build requirements: see scripts/linux/Dockerfile.
set -euo pipefail

[[ "$(uname -s)" == "Linux" ]] || { echo "run this on Linux (or via scripts/package-linux-docker.sh)" >&2; exit 1; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
BUILD="$ROOT/target/linux-$ARCH"
NAME="waveshelf-$VERSION-linux-$ARCH"
STAGE="$ROOT/dist/$NAME"
LIBS=(libavutil.so.60 libswresample.so.6 libavcodec.so.62 libavformat.so.62)

mkdir -p "$BUILD"
echo "==> FFmpeg ($ARCH)"
scripts/build-ffmpeg.sh --prefix "$BUILD/ffmpeg" >"$BUILD-ffmpeg.log" 2>&1 || { tail -20 "$BUILD-ffmpeg.log"; exit 1; }

echo "==> Waveshelf ($ARCH)"
# Separate target dir so a shared checkout doesn't mix macOS and Linux artifacts.
CARGO_TARGET_DIR="$BUILD/cargo" FFMPEG_DIR="$BUILD/ffmpeg" WAVESHELF_RPATH='$ORIGIN/lib' \
  cargo build --release

echo "==> Assembling $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/lib" "$STAGE/licenses"
cp "$BUILD/cargo/release/waveshelf" "$STAGE/"
for lib in "${LIBS[@]}"; do
  cp -L "$BUILD/ffmpeg/lib/$lib" "$STAGE/lib/$lib" # dereference: ship under the soname
done
strip --strip-unneeded "$STAGE/waveshelf" "$STAGE"/lib/*.so.*
cp "$BUILD/ffmpeg/LICENSE.LGPL" "$STAGE/licenses/FFmpeg-LGPL-2.1.txt"
cp "$BUILD/ffmpeg/BUILD-INFO.txt" "$STAGE/licenses/FFmpeg-BUILD-INFO.txt"
cp scripts/build-ffmpeg.sh "$STAGE/licenses/"

cat >"$STAGE/waveshelf.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Waveshelf
Comment=Fast audio browser and player
Exec=waveshelf %f
Terminal=false
Categories=AudioVideo;Audio;Player;
DESKTOP

cat >"$STAGE/README.txt" <<'TXT'
Waveshelf for Linux

Run:      ./waveshelf [folder]
Needs:    glibc 2.35+, ALSA (libasound2), and X11 or Wayland with OpenGL
          (libxkbcommon, libGL/libEGL) -- present on any desktop install.
Licenses: licenses/ (FFmpeg is LGPL 2.1, dynamically linked from lib/).
TXT

echo "==> Checking linkage"
missing="$(cd "$STAGE" && ldd ./waveshelf | grep 'not found' || true)"
[[ -z "$missing" ]] || { echo "unresolved libraries:"; echo "$missing"; exit 1; }
(cd "$STAGE" && ldd ./waveshelf | grep -E 'libav|libsw')

tar -C "$ROOT/dist" -czf "$ROOT/dist/$NAME.tar.gz" "$NAME"
echo "Built $ROOT/dist/$NAME.tar.gz"
