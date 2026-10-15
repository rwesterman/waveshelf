//! Folder listing, audio metadata and navigation history.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::thread;
use std::time::SystemTime;

use crossbeam_channel::{Receiver, Sender, unbounded};
use ffmpeg_next as ff;

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "wave", "w64", "rf64", "bwf", "aif", "aiff", "aifc", "caf", "au", "snd", "voc", "mp3", "mp2", "mp1", "m4a",
    "m4b", "mp4", "aac", "adts", "alac", "flac", "ogg", "oga", "opus", "spx", "wv", "ape", "tta", "mpc", "mp+", "dsf",
    "dff", "wma", "ac3", "eac3", "dts", "mka", "amr", "3gp", "gsm",
];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metadata {
    pub duration: f64,
    pub sample_rate: u32,
    pub channels: u16,
    /// e.g. "16", "24", "32f"; empty for lossy codecs.
    pub bit_depth: String,
    pub codec: String,
    pub artist: String,
    pub title: String,
    pub album: String,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub ext: String,
    pub modified: Option<SystemTime>,
    pub meta: Option<Metadata>,
    pub meta_failed: bool,
}

pub fn list_audio_files(dir: &Path, show_hidden: bool) -> Vec<FileEntry> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<FileEntry> = read
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink()))
        .map(|e| e.path())
        .filter(|p| is_audio(p) && (show_hidden || !is_hidden(p)))
        .map(|path| {
            let md = fs::metadata(&path).ok();
            FileEntry {
                name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                ext: path.extension().unwrap_or_default().to_string_lossy().to_lowercase(),
                modified: md.and_then(|m| m.modified().ok()),
                meta: None,
                meta_failed: false,
                path,
            }
        })
        .collect();
    files.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    files
}

pub fn list_subdirs(dir: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir() && (show_hidden || !is_hidden(p)))
        .collect();
    dirs.sort_by(|a, b| natural_cmp(&display_name(a), &display_name(b)));
    dirs
}

pub fn display_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The folder after (`forward`) or before `dir` among its siblings.
pub fn sibling_dir(dir: &Path, forward: bool, show_hidden: bool) -> Option<PathBuf> {
    let parent = dir.parent()?;
    let siblings = list_subdirs(parent, show_hidden);
    let i = siblings.iter().position(|d| d == dir)?;
    if forward {
        siblings.get(i + 1).cloned()
    } else {
        i.checked_sub(1).map(|j| siblings[j].clone())
    }
}

/// Case-insensitive comparison that orders embedded numbers numerically
/// ("kick 2" < "kick 10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_number(&mut a);
                let nb = take_number(&mut b);
                let ord = na
                    .trim_start_matches('0')
                    .len()
                    .cmp(&nb.trim_start_matches('0').len())
                    .then_with(|| na.trim_start_matches('0').cmp(nb.trim_start_matches('0')))
                    .then_with(|| na.len().cmp(&nb.len()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn take_number(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
        s.push(c);
        it.next();
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortColumn {
    Name,
    Ext,
    Length,
    Rate,
    Bits,
    Channels,
    Artist,
    Title,
    Album,
    Modified,
}

pub fn sort_files(files: &mut [FileEntry], column: SortColumn, ascending: bool) {
    files.sort_by(|a, b| {
        let ord = compare(a, b, column).then_with(|| natural_cmp(&a.name, &b.name));
        if ascending { ord } else { ord.reverse() }
    });
}

fn compare(a: &FileEntry, b: &FileEntry, column: SortColumn) -> Ordering {
    let (ma, mb) = (a.meta.as_ref(), b.meta.as_ref());
    let text = |f: fn(&Metadata) -> &str| natural_cmp(ma.map_or("", f), mb.map_or("", f));
    match column {
        SortColumn::Name => natural_cmp(&a.name, &b.name),
        SortColumn::Ext => a.ext.cmp(&b.ext),
        SortColumn::Length => ma
            .map(|m| m.duration)
            .partial_cmp(&mb.map(|m| m.duration))
            .unwrap_or(Ordering::Equal),
        SortColumn::Rate => ma.map(|m| m.sample_rate).cmp(&mb.map(|m| m.sample_rate)),
        SortColumn::Bits => text(|m| &m.bit_depth),
        SortColumn::Channels => ma.map(|m| m.channels).cmp(&mb.map(|m| m.channels)),
        SortColumn::Artist => text(|m| &m.artist),
        SortColumn::Title => text(|m| &m.title),
        SortColumn::Album => text(|m| &m.album),
        SortColumn::Modified => a.modified.cmp(&b.modified),
    }
}

/// Reads technical metadata and common tags.
pub fn probe(path: &Path) -> anyhow::Result<Metadata> {
    let input = ff::format::input(path)?;
    let stream = input
        .streams()
        .best(ff::media::Type::Audio)
        .ok_or_else(|| anyhow::anyhow!("no audio"))?;
    let params = stream.parameters();
    let (rate, channels, raw_bits, coded_bits, format) = unsafe {
        let p = &*params.as_ptr();
        (
            p.sample_rate as u32,
            p.ch_layout.nb_channels as u16,
            p.bits_per_raw_sample,
            p.bits_per_coded_sample,
            p.format,
        )
    };
    let codec = params.id();
    let lossy = matches!(
        codec,
        ff::codec::Id::MP1
            | ff::codec::Id::MP2
            | ff::codec::Id::MP3
            | ff::codec::Id::AAC
            | ff::codec::Id::AAC_LATM
            | ff::codec::Id::VORBIS
            | ff::codec::Id::OPUS
            | ff::codec::Id::WMAV1
            | ff::codec::Id::WMAV2
            | ff::codec::Id::WMAPRO
            | ff::codec::Id::AC3
            | ff::codec::Id::EAC3
            | ff::codec::Id::DTS
            | ff::codec::Id::MUSEPACK7
            | ff::codec::Id::MUSEPACK8
            | ff::codec::Id::AMR_NB
            | ff::codec::Id::AMR_WB
            | ff::codec::Id::SPEEX
            | ff::codec::Id::GSM
            | ff::codec::Id::GSM_MS
    );
    let float = {
        use ff::ffi::AVSampleFormat::*;
        let f: ff::ffi::AVSampleFormat = unsafe { std::mem::transmute(format) };
        matches!(
            f,
            AV_SAMPLE_FMT_FLT | AV_SAMPLE_FMT_FLTP | AV_SAMPLE_FMT_DBL | AV_SAMPLE_FMT_DBLP
        )
    };
    let bits = if raw_bits > 0 { raw_bits } else { coded_bits };
    let bit_depth = if lossy || bits <= 0 {
        String::new()
    } else if float {
        format!("{bits}f")
    } else {
        bits.to_string()
    };

    let (format_tags, stream_tags) = (input.metadata(), stream.metadata());
    let tag = |key: &str| {
        format_tags
            .get(key)
            .or_else(|| stream_tags.get(key))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Ok(Metadata {
        duration: crate::audio::decoder::duration_secs(&input, &stream),
        sample_rate: rate,
        channels,
        bit_depth,
        codec: codec.name().to_string(),
        artist: tag("artist"),
        title: tag("title"),
        album: tag("album"),
    })
}

/// A pool of background threads that probe files for metadata.
pub struct MetadataScanner {
    jobs: Sender<(u64, PathBuf)>,
    pub results: Receiver<(u64, PathBuf, Option<Metadata>)>,
    generation: Arc<AtomicU64>,
}

impl MetadataScanner {
    pub fn new(threads: usize, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (jobs, job_rx) = unbounded::<(u64, PathBuf)>();
        let (result_tx, results) = unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let wake = Arc::new(wake);
        for i in 0..threads {
            let (job_rx, result_tx, generation, wake) =
                (job_rx.clone(), result_tx.clone(), generation.clone(), wake.clone());
            thread::Builder::new()
                .name(format!("metadata-{i}"))
                .spawn(move || {
                    for (generation_id, path) in job_rx {
                        if generation_id != generation.load(AtomicOrdering::Relaxed) {
                            continue; // stale: the folder changed
                        }
                        let meta = probe(&path).ok();
                        if result_tx.send((generation_id, path, meta)).is_err() {
                            return;
                        }
                        wake();
                    }
                })
                .expect("spawning metadata worker");
        }
        Self {
            jobs,
            results,
            generation,
        }
    }

    /// Cancels pending work and queues `paths`; returns the new generation id.
    pub fn scan(&self, paths: impl IntoIterator<Item = PathBuf>) -> u64 {
        let generation_id = self.generation.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        for path in paths {
            let _ = self.jobs.send((generation_id, path));
        }
        generation_id
    }
}

/// Back/forward folder history.
#[derive(Debug, Default)]
pub struct History {
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
}

impl History {
    /// Records leaving `from` for a new location.
    pub fn visit(&mut self, from: PathBuf) {
        if self.back.last() != Some(&from) {
            self.back.push(from);
        }
        self.forward.clear();
    }

    pub fn back(&mut self, current: PathBuf) -> Option<PathBuf> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    pub fn forward(&mut self, current: PathBuf) -> Option<PathBuf> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_handles_numbers_and_case() {
        let mut names = vec!["Kick 10.wav", "kick 2.wav", "Kick 1.wav", "snare.wav", "kick 02.wav"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            ["Kick 1.wav", "kick 2.wav", "kick 02.wav", "Kick 10.wav", "snare.wav"]
        );
    }

    #[test]
    fn natural_order_prefix_sorts_first() {
        assert_eq!(natural_cmp("abc", "abcd"), Ordering::Less);
        assert_eq!(natural_cmp("a1", "a"), Ordering::Greater);
    }

    #[test]
    fn audio_extension_check_is_case_insensitive() {
        assert!(is_audio(Path::new("/x/Loop.WAV")));
        assert!(is_audio(Path::new("song.flac")));
        assert!(!is_audio(Path::new("notes.txt")));
        assert!(!is_audio(Path::new("noext")));
    }

    fn entry(name: &str, duration: Option<f64>) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.into(),
            ext: name.rsplit('.').next().unwrap().into(),
            modified: None,
            meta: duration.map(|d| Metadata {
                duration: d,
                ..Default::default()
            }),
            meta_failed: false,
        }
    }

    #[test]
    fn sort_by_length_puts_unscanned_first_and_breaks_ties_by_name() {
        let mut files = vec![
            entry("b.wav", Some(2.0)),
            entry("c.wav", None),
            entry("a.wav", Some(2.0)),
            entry("d.wav", Some(1.0)),
        ];
        sort_files(&mut files, SortColumn::Length, true);
        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["c.wav", "d.wav", "a.wav", "b.wav"]);
        sort_files(&mut files, SortColumn::Length, false);
        assert_eq!(files[0].name, "b.wav");
    }

    #[test]
    fn history_back_and_forward() {
        let mut h = History::default();
        h.visit("/a".into());
        h.visit("/b".into());
        assert_eq!(h.back("/c".into()), Some("/b".into()));
        assert_eq!(h.back("/b".into()), Some("/a".into()));
        assert_eq!(h.back("/a".into()), None);
        assert_eq!(h.forward("/a".into()), Some("/b".into()));
        // Visiting somewhere new drops the forward stack.
        h.visit("/b".into());
        assert!(!h.can_go_forward());
    }

    #[test]
    fn listing_filters_audio_and_hidden_files() {
        let dir = std::env::temp_dir().join(format!("waveshelf-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        for f in ["b.wav", "a.mp3", "readme.txt", ".hidden.wav"] {
            fs::write(dir.join(f), b"").unwrap();
        }
        let names: Vec<_> = list_audio_files(&dir, false).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["a.mp3", "b.wav"]);
        assert_eq!(list_audio_files(&dir, true).len(), 3);
        assert_eq!(list_subdirs(&dir, false), vec![dir.join("sub")]);
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod probe_tests {
    use crate::audio::decoder::tests::temp_wav;

    #[test]
    fn probe_reads_wav_format() {
        let path = temp_wav("probe", 48_000, 24_000);
        let m = super::probe(&path).unwrap();
        assert_eq!((m.sample_rate, m.channels, m.bit_depth.as_str()), (48_000, 2, "16"));
        assert!((m.duration - 0.5).abs() < 1e-3);
        std::fs::remove_file(path).unwrap();
    }
}
