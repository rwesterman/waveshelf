#!/usr/bin/env bash
# Builds an LGPL-2.1, audio-only, shared-library FFmpeg into third_party/ffmpeg.
#
# Licensing: no --enable-gpl, --enable-version3 or --enable-nonfree, and
# --disable-autodetect so no system library is pulled in implicitly. Only
# FFmpeg's built-in decoders are enabled; zlib (permissive) is the sole
# external dependency. Ship the resulting dylibs alongside the app (dynamic
# linking) together with LICENSE.LGPL and this script.
#
# Usage: scripts/build-ffmpeg.sh [--arch arm64|x86_64] [--prefix DIR]
#   Defaults: host architecture, third_party/ffmpeg. Idempotent; rebuilds if
#   the version or flags change. macOS builds target macOS $MACOS_MIN+.
set -euo pipefail

MACOS_MIN="12.0"

FFMPEG_VERSION="8.1.2"
# Matches the checksum in Homebrew's ffmpeg 8.1.2 formula.
FFMPEG_SHA256="464beb5e7bf0c311e68b45ae2f04e9cc2af88851abb4082231742a74d97b524c"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="$ROOT/third_party/ffmpeg"
ARCH="$(uname -m)"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ "$ARCH" == "aarch64" ]] && ARCH=arm64
OS="$(uname -s)"

WORK="$ROOT/target/ffmpeg-build"
TARBALL="$WORK/ffmpeg-$FFMPEG_VERSION.tar.xz"
SRC="$WORK/src-$OS-$ARCH/ffmpeg-$FFMPEG_VERSION"

DEMUXERS="wav,w64,aiff,caf,au,voc,mp3,aac,ac3,eac3,dts,flac,ogg,mov,matroska,ape,wv,tta,mpc,mpc8,dsf,iff,asf,xwma,amr,mxf,rf64"
DECODERS="pcm_*,adpcm_*,mp1,mp1float,mp2,mp2float,mp3,mp3float,mp3adu,mp3adufloat,aac,aac_fixed,aac_latm,alac,flac,vorbis,opus,wavpack,ape,tta,mpc7,mpc8,dsd_lsbf,dsd_msbf,dsd_lsbf_planar,dsd_msbf_planar,wmav1,wmav2,wmapro,wmalossless,ac3,ac3_fixed,eac3,dca,truehd,mlp,amrnb,amrwb,gsm,gsm_ms"
PARSERS="aac,aac_latm,ac3,dca,flac,mlp,mpegaudio,opus,vorbis,gsm,amr"

CONFIGURE_FLAGS=(
  --prefix="$PREFIX"
  --install-name-dir=@rpath
  --enable-shared --disable-static --enable-pic
  --disable-autodetect --enable-zlib
  --disable-programs --disable-doc --disable-network
  --disable-avdevice --disable-avfilter --disable-swscale
  --disable-everything
  --enable-protocol=file,pipe
  --enable-demuxer="$DEMUXERS"
  --enable-decoder="$DECODERS"
  --enable-parser="$PARSERS"
)

if [[ "$OS" == "Darwin" ]]; then
  arch_flags="-arch $ARCH -mmacosx-version-min=$MACOS_MIN"
  CONFIGURE_FLAGS+=(--extra-cflags="$arch_flags" --extra-ldflags="$arch_flags")
  if [[ "$ARCH" != "$(uname -m)" ]]; then
    CONFIGURE_FLAGS+=(--enable-cross-compile --arch="$ARCH" --target-os=darwin)
  fi
fi
# FFmpeg's x86 SIMD code needs nasm; without it decoding still works, just slower.
if [[ "$ARCH" == "x86_64" ]] && ! command -v nasm >/dev/null; then
  echo "warning: nasm not found; building without x86 assembly" >&2
  CONFIGURE_FLAGS+=(--disable-x86asm)
fi

stamp="$PREFIX/.build-stamp"
want="$FFMPEG_VERSION $ARCH ${CONFIGURE_FLAGS[*]}"
if [[ -f "$stamp" && "$(cat "$stamp")" == "$want" ]]; then
  echo "FFmpeg $FFMPEG_VERSION already built in $PREFIX"
  exit 0
fi

mkdir -p "$WORK"
if [[ ! -f "$TARBALL" ]]; then
  curl -fL -o "$TARBALL" "https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
fi
if command -v sha256sum >/dev/null; then
  echo "$FFMPEG_SHA256  $TARBALL" | sha256sum -c -
else
  echo "$FFMPEG_SHA256  $TARBALL" | shasum -a 256 -c -
fi

rm -rf "$SRC" "$PREFIX"
mkdir -p "$(dirname "$SRC")"
tar -xJf "$TARBALL" -C "$(dirname "$SRC")"
cd "$SRC"
./configure "${CONFIGURE_FLAGS[@]}"

# Refuse to install anything that is not plain LGPL.
license="$(grep -E '^#define FFMPEG_LICENSE ' config.h)"
if [[ "$license" != *'"LGPL version 2.1 or later"'* ]]; then
  echo "Unexpected FFmpeg license: $license" >&2
  exit 1
fi

make -j"$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
make install
cp COPYING.LGPLv2.1 "$PREFIX/LICENSE.LGPL"
{
  echo "FFmpeg $FFMPEG_VERSION ($ARCH), LGPL 2.1 or later"
  echo "Source: https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz"
  echo "SHA-256: $FFMPEG_SHA256"
  echo "Configured with: ${CONFIGURE_FLAGS[*]}"
} > "$PREFIX/BUILD-INFO.txt"
echo "$want" > "$stamp"
echo "Installed LGPL FFmpeg $FFMPEG_VERSION ($ARCH) to $PREFIX"
