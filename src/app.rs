//! Application state and UI.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use egui::{Align, Color32, Key, Layout, Modifiers, RichText, Sense, Ui};
use egui_extras::{Column, TableBuilder};
use rand::Rng;

use crate::audio::engine::{Command, EngineHandle, Event};
use crate::format;
use crate::library::{self, FileEntry, History, MetadataScanner, SortColumn};
use crate::waveform::PeakJob;
use crate::waveform_view;

const PLAYING: Color32 = Color32::from_rgb(90, 200, 120);
/// Hold Space at least this long while playing to dim instead of pausing.
const SPACE_HOLD_SECS: f64 = 0.25;
const DIM_GAIN: f32 = 0.1;
const SEEK_STEP_SECS: f64 = 5.0;
const TREE_ROW_HEIGHT: f32 = 20.0;
const LIST_ROW_HEIGHT: f32 = 20.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Tree,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClickMode {
    /// Clicking a file plays it.
    Single,
    /// Clicking selects; double-clicking plays.
    Double,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Advance {
    Off,
    Next,
    Random,
}

struct Playing {
    id: u64,
    path: PathBuf,
    name: String,
    duration: f64,
    finished: bool,
}

struct SpaceHold {
    pressed_at: f64,
    dimming: bool,
}

/// UI interactions collected while drawing, applied afterwards.
enum Action {
    Navigate(PathBuf),
    NavigateAndPlay(PathBuf),
    ToggleExpand(PathBuf),
    Focus(usize),
    Play(usize),
    Sort(SortColumn),
    Seek(f64),
}

pub struct WaveshelfApp {
    ctx: egui::Context,
    engine: EngineHandle,
    scanner: MetadataScanner,
    scan_generation: u64,
    scan_pending: usize,

    roots: Vec<(String, PathBuf)>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
    expanded: HashSet<PathBuf>,
    current_dir: Option<PathBuf>,
    history: History,
    files: Vec<FileEntry>,
    sort: (SortColumn, bool),
    cursor: Option<usize>,
    focus: Pane,
    scroll_list: bool,
    scroll_tree: bool,
    show_hidden: bool,

    next_id: u64,
    playing: Option<Playing>,
    played: HashSet<PathBuf>,
    peaks: Option<PeakJob>,
    click_mode: ClickMode,
    advance: Advance,
    looping: bool,
    stop_after_current: bool,
    volume: f32,
    muted: bool,
    space: Option<SpaceHold>,

    device: Option<String>,
    message: Option<String>,
}

impl WaveshelfApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let start = std::env::args_os()
            .nth(1)
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .or_else(|| std::env::home_dir().map(|h| h.join("Music")).filter(|p| p.is_dir()))
            .or_else(std::env::home_dir);
        Self::with_start_dir(cc, start)
    }

    pub fn with_start_dir(cc: &eframe::CreationContext<'_>, start: Option<PathBuf>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let engine = EngineHandle::spawn({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 6));
        let scanner = MetadataScanner::new(threads, move || ctx.request_repaint());

        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            engine,
            scanner,
            scan_generation: 0,
            scan_pending: 0,
            roots: discover_roots(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            current_dir: None,
            history: History::default(),
            files: Vec::new(),
            sort: (SortColumn::Name, true),
            cursor: None,
            focus: Pane::List,
            scroll_list: false,
            scroll_tree: false,
            show_hidden: false,
            next_id: 0,
            playing: None,
            played: HashSet::new(),
            peaks: None,
            click_mode: ClickMode::Single,
            advance: Advance::Next,
            looping: false,
            stop_after_current: false,
            volume: 0.8,
            muted: false,
            space: None,
            device: None,
            message: None,
        };
        if let Some(dir) = start {
            app.navigate(dir, false);
        }
        app
    }

    // ---- navigation -------------------------------------------------------

    fn navigate(&mut self, dir: PathBuf, record_history: bool) {
        if self.current_dir.as_ref() == Some(&dir) {
            return;
        }
        if record_history && let Some(current) = self.current_dir.take() {
            self.history.visit(current);
        }
        self.files = library::list_audio_files(&dir, self.show_hidden);
        library::sort_files(&mut self.files, self.sort.0, self.sort.1);
        self.cursor = self.playing.as_ref().and_then(|p| self.index_of(&p.path));
        self.scan_generation = self.scanner.scan(self.files.iter().map(|f| f.path.clone()));
        self.scan_pending = self.files.len();
        self.reveal_in_tree(&dir);
        self.current_dir = Some(dir);
        self.scroll_list = true;
        self.scroll_tree = true;
    }

    fn reveal_in_tree(&mut self, dir: &Path) {
        let root = self
            .roots
            .iter()
            .filter(|(_, r)| dir.starts_with(r))
            .max_by_key(|(_, r)| r.components().count())
            .map(|(_, r)| r.clone());
        if let Some(root) = root {
            let mut p = dir.parent();
            while let Some(ancestor) = p {
                if !ancestor.starts_with(&root) {
                    break;
                }
                self.expanded.insert(ancestor.to_path_buf());
                p = ancestor.parent();
            }
        }
    }

    fn go_parent(&mut self) {
        if let Some(parent) = self
            .current_dir
            .as_ref()
            .and_then(|d| d.parent())
            .map(Path::to_path_buf)
        {
            self.navigate(parent, true);
        }
    }

    fn go_sibling(&mut self, forward: bool) {
        if let Some(next) = self
            .current_dir
            .as_ref()
            .and_then(|d| library::sibling_dir(d, forward, self.show_hidden))
        {
            self.navigate(next, true);
        }
    }

    fn go_history(&mut self, forward: bool) {
        let Some(current) = self.current_dir.clone() else {
            return;
        };
        let target = if forward {
            self.history.forward(current)
        } else {
            self.history.back(current)
        };
        if let Some(target) = target {
            self.current_dir = None; // force reload without touching history
            self.navigate(target, false);
        }
    }

    fn tree_children(&mut self, dir: &Path) -> &[PathBuf] {
        if !self.children.contains_key(dir) {
            let kids = library::list_subdirs(dir, self.show_hidden);
            self.children.insert(dir.to_path_buf(), kids);
        }
        &self.children[dir]
    }

    /// Flattened visible tree: (path, label, depth).
    fn tree_rows(&mut self) -> Vec<(PathBuf, String, usize)> {
        let mut rows = Vec::new();
        let roots = self.roots.clone();
        for (label, root) in roots {
            rows.push((root.clone(), label, 0));
            if self.expanded.contains(&root) {
                self.push_tree_children(&root, 1, &mut rows);
            }
        }
        rows
    }

    fn push_tree_children(&mut self, dir: &Path, depth: usize, rows: &mut Vec<(PathBuf, String, usize)>) {
        for child in self.tree_children(dir).to_vec() {
            rows.push((child.clone(), library::display_name(&child), depth));
            if self.expanded.contains(&child) {
                self.push_tree_children(&child, depth + 1, rows);
            }
        }
    }

    // ---- playback ---------------------------------------------------------

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.files.iter().position(|f| f.path == path)
    }

    fn playing_index(&self) -> Option<usize> {
        self.playing.as_ref().and_then(|p| self.index_of(&p.path))
    }

    fn play_index(&mut self, index: usize) {
        let Some(file) = self.files.get(index) else { return };
        self.next_id += 1;
        let path = file.path.clone();
        self.playing = Some(Playing {
            id: self.next_id,
            path: path.clone(),
            name: file.name.clone(),
            duration: file.meta.as_ref().map_or(0.0, |m| m.duration),
            finished: false,
        });
        self.engine.send(Command::Load {
            id: self.next_id,
            path: path.clone(),
            start: 0.0,
            paused: false,
        });
        let ctx_wake = self.repaint_handle();
        self.peaks = Some(PeakJob::start(path.clone(), ctx_wake));
        self.played.insert(path);
        self.cursor = Some(index);
        self.scroll_list = true;
        self.message = None;
    }

    fn repaint_handle(&self) -> impl Fn() + Send + 'static {
        let ctx = self.ctx.clone();
        move || ctx.request_repaint()
    }

    fn play_relative(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        if self.advance == Advance::Random {
            self.play_random();
            return;
        }
        let from = self.playing_index().or(self.cursor);
        let next = match from {
            Some(i) => i as isize + delta,
            None if delta >= 0 => 0,
            None => self.files.len() as isize - 1,
        };
        if (0..self.files.len() as isize).contains(&next) {
            self.play_index(next as usize);
        }
    }

    fn play_random(&mut self) {
        let n = self.files.len();
        if n == 0 {
            return;
        }
        let current = self.playing_index();
        let mut rng = rand::rng();
        let mut i = rng.random_range(0..n);
        if n > 1 && Some(i) == current {
            i = (i + 1 + rng.random_range(0..n - 1)) % n;
        }
        self.play_index(i);
    }

    fn toggle_play(&mut self) {
        match &self.playing {
            None => {
                if let Some(i) = self.cursor.or(if self.files.is_empty() { None } else { Some(0) }) {
                    self.play_index(i);
                }
            }
            Some(p) if p.finished => {
                // Play again from the start.
                if let Some(p) = &mut self.playing {
                    p.finished = false;
                }
                self.engine.send(Command::Seek(0.0));
                self.engine.send(Command::Play);
            }
            Some(_) if self.engine.is_paused() => self.engine.send(Command::Play),
            Some(_) => self.engine.send(Command::Pause),
        }
    }

    fn seek(&mut self, secs: f64) {
        let Some(p) = &mut self.playing else { return };
        if p.duration > 0.0 && secs >= p.duration {
            // Seeking past the end continues with the next file.
            self.on_finished();
            return;
        }
        p.finished = false;
        self.engine.send(Command::Seek(secs.max(0.0)));
    }

    fn seek_relative(&mut self, delta: f64) {
        if let Some(pos) = self.engine.position() {
            self.seek(pos + delta);
        }
    }

    fn stop(&mut self) {
        // First stop rewinds; stopping again unloads the file.
        let at_start = self.engine.is_paused() && self.engine.position().is_some_and(|p| p < 0.001);
        if at_start {
            self.unload();
        } else {
            self.engine.send(Command::Stop);
        }
    }

    fn unload(&mut self) {
        self.engine.send(Command::Unload);
        self.playing = None;
        self.peaks = None;
    }

    fn on_finished(&mut self) {
        if let Some(p) = &mut self.playing {
            p.finished = true;
        }
        if self.stop_after_current {
            return;
        }
        match self.advance {
            Advance::Off => {}
            Advance::Next => {
                if let Some(i) = self.playing_index().filter(|i| i + 1 < self.files.len()) {
                    self.play_index(i + 1);
                }
            }
            Advance::Random => self.play_random(),
        }
    }

    fn gain(&self) -> f32 {
        if self.muted {
            return 0.0;
        }
        let dim = if self.space.as_ref().is_some_and(|s| s.dimming) {
            DIM_GAIN
        } else {
            1.0
        };
        self.volume * self.volume * dim
    }

    // ---- background events ------------------------------------------------

    fn poll_events(&mut self) {
        while let Ok(event) = self.engine.events.try_recv() {
            match event {
                Event::Loaded { id, duration } => {
                    if let Some(p) = self.playing.as_mut().filter(|p| p.id == id)
                        && duration > 0.0
                    {
                        p.duration = duration;
                    }
                }
                Event::Finished { id } => {
                    if self.playing.as_ref().is_some_and(|p| p.id == id) {
                        self.on_finished();
                    }
                }
                Event::Failed { id, error } => {
                    if let Some(p) = self.playing.as_ref().filter(|p| p.id == id) {
                        self.message = Some(format!("Can't play {}: {error}", p.name));
                        self.playing = None;
                        self.peaks = None;
                    }
                }
                Event::Device { name, error } => {
                    self.device = name;
                    if let Some(e) = error {
                        self.message = Some(format!("Audio device: {e}"));
                    }
                }
            }
        }
        while let Ok((generation, path, meta)) = self.scanner.results.try_recv() {
            if generation != self.scan_generation {
                continue;
            }
            self.scan_pending = self.scan_pending.saturating_sub(1);
            let Some(file) = self.files.iter_mut().find(|f| f.path == path) else {
                continue;
            };
            match meta {
                Some(m) => {
                    if let Some(p) = self
                        .playing
                        .as_mut()
                        .filter(|p| p.path == file.path && p.duration == 0.0)
                    {
                        p.duration = m.duration;
                    }
                    file.meta = Some(m);
                }
                None => file.meta_failed = true,
            }
        }
    }

    // ---- keyboard ---------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let pressed = |mods: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(mods, key));

        // Space: tap toggles play/pause; holding it while playing dims.
        if pressed(Modifiers::ALT, Key::Space) {
            self.play_random();
        } else if pressed(Modifiers::NONE, Key::Space) {
            let playing = self.playing.is_some() && !self.engine.is_paused();
            if playing {
                self.space = Some(SpaceHold {
                    pressed_at: now,
                    dimming: false,
                });
            } else {
                self.toggle_play();
            }
        }
        if let Some(hold) = &mut self.space {
            if ctx.input(|i| i.key_down(Key::Space)) {
                if now - hold.pressed_at >= SPACE_HOLD_SECS {
                    hold.dimming = true;
                }
                ctx.request_repaint();
            } else {
                let was_dimming = hold.dimming;
                self.space = None;
                if !was_dimming {
                    self.toggle_play();
                }
            }
        }

        let pressed = |mods: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(mods, key));
        if pressed(Modifiers::NONE, Key::Tab) {
            self.focus = if self.focus == Pane::Tree {
                Pane::List
            } else {
                Pane::Tree
            };
            if self.focus == Pane::Tree {
                self.scroll_tree = true;
            }
        }
        if pressed(Modifiers::COMMAND, Key::ArrowDown) {
            self.go_sibling(true);
        }
        if pressed(Modifiers::COMMAND, Key::ArrowUp) {
            self.go_sibling(false);
        }
        if pressed(Modifiers::ALT, Key::ArrowUp) || pressed(Modifiers::NONE, Key::Backspace) {
            self.go_parent();
        }
        if pressed(Modifiers::ALT, Key::ArrowLeft) {
            self.go_history(false);
        }
        if pressed(Modifiers::ALT, Key::ArrowRight) {
            self.go_history(true);
        }
        if pressed(Modifiers::COMMAND, Key::W) {
            self.unload();
        }

        match self.focus {
            Pane::List => {
                let single = self.click_mode == ClickMode::Single;
                if pressed(Modifiers::NONE, Key::ArrowDown) {
                    self.move_cursor(1, single);
                }
                if pressed(Modifiers::NONE, Key::ArrowUp) {
                    self.move_cursor(-1, single);
                }
                if pressed(Modifiers::NONE, Key::Enter)
                    && let Some(i) = self.cursor
                {
                    self.play_index(i);
                }
                if pressed(Modifiers::NONE, Key::ArrowRight) {
                    self.seek_relative(SEEK_STEP_SECS);
                }
                if pressed(Modifiers::NONE, Key::ArrowLeft) {
                    self.seek_relative(-SEEK_STEP_SECS);
                }
            }
            Pane::Tree => {
                let rows = self.tree_rows();
                let at = self
                    .current_dir
                    .as_ref()
                    .and_then(|d| rows.iter().position(|(p, _, _)| p == d));
                if pressed(Modifiers::NONE, Key::ArrowDown) {
                    let next = at.map_or(0, |i| (i + 1).min(rows.len().saturating_sub(1)));
                    if let Some((p, _, _)) = rows.get(next) {
                        self.navigate(p.clone(), true);
                    }
                }
                if pressed(Modifiers::NONE, Key::ArrowUp) {
                    let prev = at.map_or(0, |i| i.saturating_sub(1));
                    if let Some((p, _, _)) = rows.get(prev) {
                        self.navigate(p.clone(), true);
                    }
                }
                if pressed(Modifiers::NONE, Key::ArrowRight)
                    && let Some(d) = self.current_dir.clone()
                {
                    self.expanded.insert(d);
                }
                if pressed(Modifiers::NONE, Key::ArrowLeft)
                    && let Some(d) = self.current_dir.clone()
                    && !self.expanded.remove(&d)
                {
                    self.go_parent();
                }
                if pressed(Modifiers::NONE, Key::Enter) && !self.files.is_empty() {
                    self.focus = Pane::List;
                    self.play_index(0);
                }
            }
        }
    }

    fn move_cursor(&mut self, delta: isize, play: bool) {
        if play && self.advance == Advance::Random {
            self.play_random();
            return;
        }
        let n = self.files.len() as isize;
        if n == 0 {
            return;
        }
        let from = self
            .cursor
            .or(self.playing_index())
            .map_or(if delta > 0 { -1 } else { n }, |i| i as isize);
        let to = (from + delta).clamp(0, n - 1) as usize;
        if play {
            if Some(to) != self.playing_index() {
                self.play_index(to);
            }
        } else {
            self.cursor = Some(to);
            self.scroll_list = true;
        }
    }

    fn apply(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Navigate(dir) => {
                    self.focus = Pane::Tree;
                    self.navigate(dir, true);
                }
                Action::NavigateAndPlay(dir) => {
                    self.navigate(dir, true);
                    if !self.files.is_empty() {
                        self.focus = Pane::List;
                        self.play_index(0);
                    }
                }
                Action::ToggleExpand(dir) => {
                    if !self.expanded.remove(&dir) {
                        self.expanded.insert(dir);
                    }
                }
                Action::Focus(i) => {
                    self.focus = Pane::List;
                    self.cursor = Some(i);
                }
                Action::Play(i) => {
                    self.focus = Pane::List;
                    // Clicking the file that is already playing doesn't restart it.
                    if self.playing_index() != Some(i) || self.playing.as_ref().is_some_and(|p| p.finished) {
                        self.play_index(i);
                    }
                }
                Action::Sort(column) => {
                    self.sort = if self.sort.0 == column {
                        (column, !self.sort.1)
                    } else {
                        (column, true)
                    };
                    let cursor_path = self.cursor.and_then(|i| self.files.get(i)).map(|f| f.path.clone());
                    library::sort_files(&mut self.files, self.sort.0, self.sort.1);
                    self.cursor = cursor_path.and_then(|p| self.index_of(&p));
                }
                Action::Seek(secs) => self.seek(secs),
            }
        }
    }
}

fn discover_roots() -> Vec<(String, PathBuf)> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::home_dir() {
        roots.push(("Home".to_string(), home));
    }
    #[cfg(target_os = "macos")]
    for volume in library::list_subdirs(Path::new("/Volumes"), false) {
        roots.push((library::display_name(&volume), volume));
    }
    roots.push(("Computer".to_string(), PathBuf::from("/")));
    roots
}

// ---- drawing ----------------------------------------------------------------

impl eframe::App for WaveshelfApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_events();
        self.handle_keys(&ctx);
        self.engine.set_gain(self.gain());

        let mut actions = Vec::new();
        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header(ui);
            ui.add_space(2.0);
            self.transport(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::top("waveform")
            .resizable(true)
            .default_size(170.0)
            .min_size(60.0)
            .show(ui, |ui| self.waveform(ui, &mut actions));
        egui::Panel::left("tree")
            .resizable(true)
            .default_size(240.0)
            .min_size(120.0)
            .show(ui, |ui| self.tree(ui, &mut actions));
        egui::CentralPanel::default().show(ui, |ui| self.file_list(ui, &mut actions));
        self.apply(actions);

        if self.playing.is_some() && !self.engine.is_paused() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

impl WaveshelfApp {
    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let (name, duration) = self
                .playing
                .as_ref()
                .map_or(("No file loaded".to_string(), 0.0), |p| (p.name.clone(), p.duration));
            ui.label(RichText::new(name).size(18.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let pos = self.engine.position().unwrap_or(0.0);
                let time = if self.playing.is_some() {
                    format!(
                        "{} / {}",
                        format::time_precise(pos.min(duration.max(pos))),
                        format::time_precise(duration)
                    )
                } else {
                    String::new()
                };
                ui.label(RichText::new(time).monospace().size(15.0));
            });
        });
    }

    fn transport(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button("⏮").on_hover_text("Previous file (↑)").clicked() {
                self.play_relative(-1);
            }
            let paused = self.playing.is_none() || self.engine.is_paused();
            if ui
                .button(if paused { "▶" } else { "⏸" })
                .on_hover_text("Play / pause (Space)")
                .clicked()
            {
                self.toggle_play();
            }
            if ui.button("⏹").on_hover_text("Stop; again to unload").clicked() {
                self.stop();
            }
            if ui.button("⏭").on_hover_text("Next file (↓)").clicked() {
                self.play_relative(1);
            }
            ui.separator();
            if ui
                .selectable_label(self.looping, "🔁 Loop")
                .on_hover_text("Repeat the current file")
                .clicked()
            {
                self.looping = !self.looping;
                self.engine.send(Command::SetLoop(self.looping));
            }
            let advance = match self.advance {
                Advance::Off => "⏹ No advance",
                Advance::Next => "⏩ Next",
                Advance::Random => "🔀 Random",
            };
            if ui
                .button(advance)
                .on_hover_text("What plays when a file ends")
                .clicked()
            {
                self.advance = match self.advance {
                    Advance::Off => Advance::Next,
                    Advance::Next => Advance::Random,
                    Advance::Random => Advance::Off,
                };
            }
            ui.toggle_value(&mut self.stop_after_current, "Stop after current");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false));
                let icon = if self.muted || self.volume == 0.0 {
                    "🔇"
                } else {
                    "🔊"
                };
                if ui
                    .button(icon)
                    .on_hover_text("Mute (hold Space while playing to dim)")
                    .clicked()
                {
                    self.muted = !self.muted;
                }
                if self.space.as_ref().is_some_and(|s| s.dimming) {
                    ui.label(RichText::new("DIM").small().color(Color32::from_rgb(255, 150, 40)));
                }
            });
        });
    }

    fn waveform(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let duration = self.playing.as_ref().map_or(0.0, |p| p.duration);
        let position = self.playing.as_ref().and(self.engine.position());
        let guard = self.peaks.as_ref().map(|job| job.peaks.lock().unwrap());
        let response = waveform_view::show(ui, guard.as_deref(), duration, position);
        drop(guard);
        if let Some(t) = response.seek_to {
            actions.push(Action::Seek(t));
        }
        if response.stop {
            self.stop();
        }
        if response.volume_steps != 0.0 {
            self.volume = (self.volume + 0.05 * response.volume_steps).clamp(0.0, 1.0);
        }
        if response.seek_steps != 0.0 {
            self.seek_relative(SEEK_STEP_SECS * f64::from(response.seek_steps));
        }
    }

    fn tree(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let rows = self.tree_rows();
        let current = self.current_dir.clone();
        let playing_dir = self
            .playing
            .as_ref()
            .and_then(|p| p.path.parent().map(Path::to_path_buf));
        let focused = self.focus == Pane::Tree;
        // Scroll sideways rather than letting long folder names widen the panel.
        let mut scroll = egui::ScrollArea::both().id_salt("tree").auto_shrink([false, false]);
        if std::mem::take(&mut self.scroll_tree)
            && let Some(i) = current.as_ref().and_then(|d| rows.iter().position(|(p, _, _)| p == d))
        {
            let target = i as f32 * TREE_ROW_HEIGHT - ui.available_height() / 2.0;
            scroll = scroll.vertical_scroll_offset(target.max(0.0));
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.history.can_go_back(), egui::Button::new("⬅"))
                .on_hover_text("Back (Alt+←)")
                .clicked()
            {
                self.go_history(false);
            }
            if ui
                .add_enabled(self.history.can_go_forward(), egui::Button::new("➡"))
                .on_hover_text("Forward (Alt+→)")
                .clicked()
            {
                self.go_history(true);
            }
            let has_parent = current.as_ref().and_then(|d| d.parent()).is_some();
            if ui
                .add_enabled(has_parent, egui::Button::new("⬆"))
                .on_hover_text("Parent folder (Backspace)")
                .clicked()
            {
                self.go_parent();
            }
        });
        ui.separator();
        scroll.show_rows(ui, TREE_ROW_HEIGHT, rows.len(), |ui, range| {
            for (path, label, depth) in &rows[range] {
                ui.horizontal(|ui| {
                    ui.set_height(TREE_ROW_HEIGHT);
                    ui.add_space(*depth as f32 * 14.0);
                    let open = self.expanded.contains(path);
                    let arrow = ui.add(egui::Label::new(if open { "⏷" } else { "⏵" }).sense(Sense::click()));
                    if arrow.clicked() {
                        actions.push(Action::ToggleExpand(path.clone()));
                    }
                    let selected = current.as_ref() == Some(path);
                    let mut text = RichText::new(label);
                    if playing_dir.as_ref() == Some(path) {
                        text = text.color(PLAYING);
                    }
                    if selected && focused {
                        text = text.strong();
                    }
                    let row = ui.selectable_label(selected, text);
                    if row.double_clicked() {
                        actions.push(Action::NavigateAndPlay(path.clone()));
                    } else if row.clicked() {
                        actions.push(Action::Navigate(path.clone()));
                    }
                });
            }
        });
    }

    fn file_list(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        if self.files.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("No audio files in this folder").weak());
            });
            return;
        }
        egui::ScrollArea::horizontal()
            .id_salt("file-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.file_table(ui, actions);
            });
    }

    fn file_table(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let playing_index = self.playing_index();
        let weak = ui.visuals().weak_text_color();
        let focused = self.focus == Pane::List;
        let mut table = TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(18.0))
            .column(Column::initial(240.0).at_least(80.0).clip(true))
            .column(Column::initial(44.0))
            .column(Column::initial(76.0))
            .column(Column::initial(66.0))
            .column(Column::initial(36.0))
            .column(Column::initial(52.0))
            .column(Column::initial(130.0).clip(true))
            .column(Column::initial(150.0).clip(true))
            .column(Column::initial(130.0).clip(true))
            .column(Column::remainder().at_least(110.0));
        if std::mem::take(&mut self.scroll_list)
            && let Some(i) = self.cursor
        {
            table = table.scroll_to_row(i, None);
        }
        let headers = [
            (None, ""),
            (Some(SortColumn::Name), "Name"),
            (Some(SortColumn::Ext), "Ext"),
            (Some(SortColumn::Length), "Length"),
            (Some(SortColumn::Rate), "Rate"),
            (Some(SortColumn::Bits), "Bits"),
            (Some(SortColumn::Channels), "Ch"),
            (Some(SortColumn::Artist), "Artist"),
            (Some(SortColumn::Title), "Title"),
            (Some(SortColumn::Album), "Album"),
            (Some(SortColumn::Modified), "Modified"),
        ];
        let sort = self.sort;
        table
            .header(LIST_ROW_HEIGHT, |mut header| {
                for (column, title) in headers {
                    header.col(|ui| {
                        let Some(column) = column else { return };
                        let arrow = if sort.0 == column {
                            if sort.1 { " ⏶" } else { " ⏷" }
                        } else {
                            ""
                        };
                        if ui
                            .add(
                                egui::Label::new(RichText::new(format!("{title}{arrow}")).strong())
                                    .sense(Sense::click()),
                            )
                            .clicked()
                        {
                            actions.push(Action::Sort(column));
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(LIST_ROW_HEIGHT, self.files.len(), |mut row| {
                    let i = row.index();
                    let file = &self.files[i];
                    let is_playing = playing_index == Some(i);
                    row.set_selected(self.cursor == Some(i) && (focused || !is_playing));
                    let style = |s: String| {
                        let t = RichText::new(s);
                        if is_playing {
                            t.color(PLAYING)
                        } else if self.played.contains(&file.path) {
                            t.color(weak)
                        } else {
                            t
                        }
                    };
                    let meta = file.meta.as_ref();
                    let pending = meta.is_none() && !file.meta_failed;
                    row.col(|ui| {
                        if is_playing {
                            ui.label(RichText::new(if self.engine.is_paused() { "⏸" } else { "▶" }).color(PLAYING));
                        }
                    });
                    row.col(|ui| {
                        ui.label(style(file.name.clone()));
                    });
                    row.col(|ui| {
                        ui.label(style(file.ext.clone()));
                    });
                    row.col(|ui| {
                        let text = match meta {
                            Some(m) if m.duration > 0.0 => format::time_precise(m.duration),
                            _ if pending => "…".into(),
                            _ => String::new(),
                        };
                        ui.label(style(text).monospace());
                    });
                    row.col(|ui| {
                        ui.label(style(
                            meta.map(|m| format::sample_rate(m.sample_rate)).unwrap_or_default(),
                        ));
                    });
                    row.col(|ui| {
                        ui.label(style(meta.map(|m| m.bit_depth.clone()).unwrap_or_default()));
                    });
                    row.col(|ui| {
                        ui.label(style(meta.map(|m| format::channels(m.channels)).unwrap_or_default()));
                    });
                    for text in [meta.map(|m| &m.artist), meta.map(|m| &m.title), meta.map(|m| &m.album)] {
                        row.col(|ui| {
                            ui.label(style(text.cloned().unwrap_or_default()));
                        });
                    }
                    row.col(|ui| {
                        ui.label(style(format::modified(file.modified)));
                    });

                    let response = row.response();
                    match self.click_mode {
                        ClickMode::Single if response.clicked() => actions.push(Action::Play(i)),
                        ClickMode::Double if response.double_clicked() => actions.push(Action::Play(i)),
                        ClickMode::Double if response.clicked() => actions.push(Action::Focus(i)),
                        _ => {}
                    }
                });
            });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let mode = match self.click_mode {
                ClickMode::Single => "Single-click plays",
                ClickMode::Double => "Double-click plays",
            };
            if ui
                .small_button(mode)
                .on_hover_text("Toggle how clicking a file plays it")
                .clicked()
            {
                self.click_mode = match self.click_mode {
                    ClickMode::Single => ClickMode::Double,
                    ClickMode::Double => ClickMode::Single,
                };
            }
            ui.separator();
            ui.label(format!("{} files", self.files.len()));
            if self.scan_pending > 0 {
                ui.separator();
                ui.spinner();
                ui.label(format!("Reading {} files", self.scan_pending));
            }
            if let Some(msg) = &self.message {
                ui.separator();
                ui.label(RichText::new(msg).color(ui.visuals().warn_fg_color));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(self.device.as_deref().unwrap_or("No output device")).weak());
                ui.label(RichText::new("🔈").weak());
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;

    /// Renders the UI headlessly to PNGs in $WAVESHELF_SNAPSHOT_DIR: after the
    /// folder scan, and after pressing ↓ (muted) to start the first file.
    /// Run with: cargo test -- --ignored renders_ui
    #[test]
    #[ignore]
    fn renders_ui() {
        ffmpeg_next::init().unwrap();
        let media = PathBuf::from(std::env::var("WAVESHELF_MEDIA_DIR").expect("set WAVESHELF_MEDIA_DIR"));
        let out = PathBuf::from(std::env::var("WAVESHELF_SNAPSHOT_DIR").expect("set WAVESHELF_SNAPSHOT_DIR"));
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1200.0, 760.0))
            .wgpu()
            .build_eframe(|cc| WaveshelfApp::with_start_dir(cc, Some(media)));
        harness.state_mut().muted = true;

        let settle = |harness: &mut Harness<'_, WaveshelfApp>, secs: f64, done: &dyn Fn(&WaveshelfApp) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
            while std::time::Instant::now() < deadline && !done(harness.state()) {
                harness.step();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            harness.step();
        };
        settle(&mut harness, 10.0, &|app| app.scan_pending == 0);
        assert_eq!(harness.state().scan_pending, 0, "metadata scan did not finish");
        assert!(
            harness.state().files.iter().all(|f| f.meta.is_some()),
            "some files have no metadata"
        );
        harness.render().unwrap().save(out.join("browse.png")).unwrap();

        harness.key_press(Key::ArrowDown);
        settle(&mut harness, 10.0, &|app| {
            app.peaks.as_ref().is_some_and(|p| p.peaks.lock().unwrap().done)
        });
        std::thread::sleep(std::time::Duration::from_millis(1500));
        harness.step();
        let app = harness.state();
        assert!(app.playing.is_some(), "↓ did not start playback: {:?}", app.message);
        let pos = app.engine.position().unwrap();
        assert!(pos > 0.5, "playhead did not advance: {pos}");
        harness.render().unwrap().save(out.join("playing.png")).unwrap();
    }
}
