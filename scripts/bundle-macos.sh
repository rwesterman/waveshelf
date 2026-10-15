#!/usr/bin/env bash
# Builds dist/Waveshelf.app: a universal (Apple Silicon + Intel), macOS 12+,
# self-contained bundle with the LGPL FFmpeg dylibs in Contents/Frameworks,
# ad-hoc signed, plus a zip of it for copying to other Macs.
#
# Usage: scripts/bundle-macos.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MACOS_MIN="12.0"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
BUNDLE_ID="app.waveshelf.Waveshelf"
DIST="$ROOT/dist"
APP="$DIST/Waveshelf.app"
LIBS=(libavutil.60 libswresample.6 libavcodec.62 libavformat.62)

export MACOSX_DEPLOYMENT_TARGET="$MACOS_MIN"
export WAVESHELF_RPATH="@executable_path/../Frameworks"

# (macOS ships bash 3.2: no associative arrays.)
triple() { [[ "$1" == arm64 ]] && echo aarch64-apple-darwin || echo x86_64-apple-darwin; }
for arch in arm64 x86_64; do
  echo "==> FFmpeg ($arch)"
  scripts/build-ffmpeg.sh --arch "$arch" --prefix "$ROOT/target/ffmpeg-$arch" >"$ROOT/target/ffmpeg-$arch.log" 2>&1 ||
    { tail -20 "$ROOT/target/ffmpeg-$arch.log"; exit 1; }
  echo "==> Waveshelf ($arch)"
  rustup target add "$(triple "$arch")" >/dev/null
  FFMPEG_DIR="$ROOT/target/ffmpeg-$arch" cargo build --release --target "$(triple "$arch")"
done

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources/licenses"
lipo -create -output "$APP/Contents/MacOS/waveshelf" \
  "target/aarch64-apple-darwin/release/waveshelf" "target/x86_64-apple-darwin/release/waveshelf"
for lib in "${LIBS[@]}"; do
  # The install names are @rpath/<lib>.dylib, so ship each under that name.
  lipo -create -output "$APP/Contents/Frameworks/$lib.dylib" \
    "target/ffmpeg-arm64/lib/$lib.dylib" "target/ffmpeg-x86_64/lib/$lib.dylib"
done

cp target/ffmpeg-arm64/LICENSE.LGPL "$APP/Contents/Resources/licenses/FFmpeg-LGPL-2.1.txt"
cat target/ffmpeg-arm64/BUILD-INFO.txt target/ffmpeg-x86_64/BUILD-INFO.txt \
  >"$APP/Contents/Resources/licenses/FFmpeg-BUILD-INFO.txt"
cp scripts/build-ffmpeg.sh "$APP/Contents/Resources/licenses/build-ffmpeg.sh"

cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Waveshelf</string>
  <key>CFBundleDisplayName</key><string>Waveshelf</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>waveshelf</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>$MACOS_MIN</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.music</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "==> Signing (ad hoc)"
for lib in "$APP"/Contents/Frameworks/*.dylib; do
  codesign --force --sign - --timestamp=none "$lib"
done
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --strict --verbose=1 "$APP"

ZIP="$DIST/Waveshelf-$VERSION-macos-universal.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
echo "Built $APP"
echo "Built $ZIP"
