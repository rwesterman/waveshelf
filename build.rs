//! Locates the LGPL FFmpeg build and sets where the binary looks for its
//! shared libraries at runtime.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo:rerun-if-env-changed=WAVESHELF_RPATH");

    let ffmpeg_dir = std::env::var_os("FFMPEG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third_party/ffmpeg"));
    let lib = ffmpeg_dir.join("lib");
    if !lib.join("pkgconfig/libavcodec.pc").exists() {
        panic!("LGPL FFmpeg not found in {}; run scripts/build-ffmpeg.sh", ffmpeg_dir.display());
    }

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" && os != "linux" {
        return;
    }
    // Packaged builds set this to a bundle-relative location
    // (@executable_path/../Frameworks, $ORIGIN/lib); dev builds use the build dir.
    let rpath = std::env::var("WAVESHELF_RPATH").unwrap_or_else(|_| lib.display().to_string());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
    if os == "linux" {
        // DT_RPATH (unlike DT_RUNPATH) also applies to the FFmpeg libraries'
        // dependencies on each other.
        println!("cargo:rustc-link-arg=-Wl,--disable-new-dtags");
    }
}
