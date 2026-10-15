# Waveshelf

A fast audio browser and player built around a big waveform view. Browse your
existing folders, click a file and it plays immediately — made for wading
through large numbers of sounds, samples and tracks. Written in Rust (egui for
the UI, cpal for audio output) with FFmpeg for decoding.

Waveshelf is an independent, clean-room project: its behaviour was specified
from public, user-facing descriptions of audio browsers, not from any other
program's code.

## Project status

**Version 0.1.0: early, usable, not yet released.** The playable core
(milestone 1) is done and tested on macOS. Packaging works for macOS; Linux
packaging is written but has not been run yet.

| Platform | Builds | Runs | Packaged | Notes |
| --- | --- | --- | --- | --- |
| macOS, Apple Silicon | ✅ | ✅ tested | ✅ `Waveshelf.app` (universal) | Primary development platform |
| macOS, Intel | ✅ | ✅ launches under Rosetta | ✅ same universal app | Not tested on real Intel hardware |
| Linux x86_64 / arm64 | ❓ | ❓ | ⚠️ script written, **not yet run** | Needs a Linux machine or Docker to verify |
| Windows | ❌ | ❌ | ❌ | Code is portable; FFmpeg build script is not |

### Milestone 1: playable core — done

- Folder tree (home, drives, whole disk) with back / forward / parent navigation
- File list with sortable columns: name, extension, length (ms precision),
  sample rate, bit depth, channels, artist, title, album, modified date;
  metadata read in the background
- Click-to-play (or double-click mode), auto-advance (next / random / off),
  loop, stop-after-current, two-stage stop (rewind, then unload)
- Big per-channel waveform computed in the background, with ruler, playhead,
  hover time, click/drag to seek
- Click-free pause, mute and dim (hold Space); sample-accurate seeking;
  gapless looping; recovers when the output device disappears
- Decodes via FFmpeg. Tested: WAV, FLAC, MP3, AAC/M4A, ALAC, Ogg Vorbis.
  Enabled but untested: AIFF, Opus, WavPack, APE, DSD, WMA, AC-3/DTS and others

### Milestone 2: daily use — not started

- Filter bar (Ctrl/Cmd+F)
- Live updates when files change on disk
- Remember settings, last folder and track, window layout
- On-disk metadata cache (large folders are slow to scan until then)
- Output device selection
- Copy file paths; drop a file onto the waveform to play it

### Known limitations

- Not yet listened to on a wide range of real-world files; automated tests use
  generated audio.
- Dragging files out to Finder or a DAW isn't supported (needs native code).
- No app icon yet.
- The macOS app is ad-hoc signed, not notarized — see "Installing on another
  Mac" below.

## Building from source

Requirements: Rust (stable), a C toolchain (Xcode command line tools on macOS),
`curl`. On Linux, also the packages listed in `scripts/linux/Dockerfile`.

```sh
scripts/build-ffmpeg.sh   # once: builds LGPL FFmpeg into third_party/ffmpeg
cargo run --release -- ~/Music
```

`.cargo/config.toml` sets `FFMPEG_DIR` so the build only ever links this LGPL
FFmpeg, never a system install (Homebrew's FFmpeg is GPL).

## Packaging

### macOS

```sh
scripts/bundle-macos.sh
```

Produces `dist/Waveshelf.app` and `dist/Waveshelf-<version>-macos-universal.zip`:
one app for Apple Silicon and Intel, macOS 12 or later, with FFmpeg inside
`Contents/Frameworks`. Requires `nasm` (`brew install nasm`) for FFmpeg's Intel
code paths. Takes about 5 minutes the first time.

**Installing on another Mac:** unzip and move `Waveshelf.app` to
Applications. Because the app is ad-hoc signed (no Apple Developer ID),
Gatekeeper blocks it the first time when it arrived by download or AirDrop:
right-click the app → Open → Open, or run
`xattr -dr com.apple.quarantine /Applications/Waveshelf.app`. Distributing to
other people properly needs a Developer ID certificate and notarization.

### Linux (unverified)

On a Linux machine:

```sh
scripts/package-linux.sh
```

or from any machine with Docker (builds in Ubuntu 22.04):

```sh
scripts/package-linux-docker.sh x86_64   # or arm64
```

Produces `dist/waveshelf-<version>-linux-<arch>.tar.gz` containing the binary,
FFmpeg in `lib/`, a `.desktop` file and licenses. Unpack anywhere and run
`./waveshelf`. Should run on glibc 2.35+ (Ubuntu 22.04+, Debian 12+, Fedora
36+) with ALSA and X11 or Wayland.

## Using it

| Key | Action |
| --- | --- |
| Click a file | Play it (toggle double-click mode in the status bar) |
| ↑ / ↓ | Play previous / next file |
| Space | Play / pause; hold while playing to dim |
| Alt+Space | Play a random file |
| Enter | Play (restart) the focused file; in the tree, play the folder |
| ← / → | Seek −5 s / +5 s (in the tree: collapse / expand) |
| Tab | Switch focus between folder tree and file list |
| Cmd+↑ / Cmd+↓ | Previous / next sibling folder (Ctrl on Linux) |
| Backspace, Alt+↑ | Parent folder |
| Alt+← / Alt+→ | Folder history back / forward |
| Cmd+W | Unload the current file (Ctrl on Linux) |

On the waveform: click or drag to seek, right-click to stop, wheel for volume,
Alt+wheel to seek.

## Tests

```sh
cargo test                                   # unit + decoder tests
WAVESHELF_MEDIA_DIR=~/Music/some-folder \
  cargo test -- --ignored media_dir          # decode and seek every file in a folder
cargo test -- --ignored engine_plays         # real output device, at zero volume
WAVESHELF_MEDIA_DIR=... WAVESHELF_SNAPSHOT_DIR=/tmp \
  cargo test -- --ignored renders_ui         # headless UI render to PNG (muted)
```

## Licensing

Waveshelf's own code is MIT OR Apache-2.0. It dynamically links an FFmpeg
built by `scripts/build-ffmpeg.sh` under the **LGPL 2.1**: no GPL, version-3 or
non-free components, only FFmpeg's built-in decoders. The packages ship the
FFmpeg libraries as separate shared libraries together with the LGPL text,
the exact FFmpeg version and configure flags, and the build script. Some
codecs (e.g. AAC) may carry patent obligations for commercial distribution in
some jurisdictions; check before shipping commercially.
