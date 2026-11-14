use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use egui::{pos2, vec2, Align2, FontId, Key, KeyboardShortcut, Modifiers, Rect, RichText, Sense, TextStyle};

use crate::document::{path_key, Document};
use crate::editor::{editor_view, Selection};
use crate::find::FindBar;
use crate::search::{Search, SearchQuery};
use crate::search_panel::SearchPanel;
use crate::build::{Job, JobEvent, JobState, RunStep};
use crate::debug::{self, DebugCommand, DebugEvent, DebugSession, DebugState, Frame, Launch};
use crate::output_panel::OutputLog;
use crate::toolbar::{self, Toolbar, ToolbarAction};
use crate::preferences::{self, Preferences};
use crate::navigation::{DefinitionPicker, History, Location, PickerResult};
use crate::profiling::ProfileRun;
use crate::project_view::{is_project_file, ProjectView};
use crate::symbols::{self, reference_at, Indexer, Phase, RefreshStats, Symbol, SymbolIndex};
use crate::syntax;
use crate::unreal::{self, UnrealProfile};

/// Storage key for the project to reopen on startup.
const PROJECT_STORAGE_KEY: &str = "project";
const BREAKPOINTS_STORAGE_KEY: &str = "breakpoints";

const OPEN_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const BACK_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft);
const FORWARD_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowRight);
const PROJECT_PANEL_SHORTCUT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::E);
const SAVE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const FIND_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
const PLAY_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);
const RUN_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F5);
const STOP_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F5);
const BREAKPOINT_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F9);
const STEP_OVER_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F10);
const STEP_IN_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F11);
const STEP_OUT_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F11);
const BUILD_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::B);
const FIND_IN_FILES_SHORTCUT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::F);
const FIND_NEXT_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F3);
const FIND_PREVIOUS_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::SHIFT, Key::F3);
const CLOSE_TAB_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
const NEXT_TAB_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Tab);
const PREV_TAB_SHORTCUT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Tab);

#[derive(Default)]
pub struct ZbuggerApp {
    /// Open files, in tab order.
    documents: Vec<Document>,
    /// Index of the visible tab; meaningless when `documents` is empty.
    active: usize,
    status: Option<String>,
    title: String,
    prefs: Preferences,
    prefs_open: bool,
    project: Option<ProjectView>,
    project_panel_hidden: bool,
    /// Where symbols are defined, for go-to-definition.
    symbols: SymbolIndex,
    /// Background indexing of the project's files, while it runs.
    indexer: Option<Indexer>,
    /// What the last indexing run reused from the cache and parsed.
    index_stats: Option<RefreshStats>,
    /// Indexer threads that may still be writing the symbol cache.
    cache_writers: Vec<std::thread::JoinHandle<()>>,
    /// Parser for indexing single files on the UI thread (open/save).
    symbol_parser: Option<tree_sitter::Parser>,
    picker: Option<DefinitionPicker>,
    history: History,
    /// The Ctrl+F find bar.
    find: FindBar,
    /// The Ctrl+Shift+F Find in Files panel.
    search_panel: SearchPanel,
    /// Which tab of the bottom panel is showing, if it's open.
    bottom_tab: Option<BottomTab>,
    /// The build toolbar and its running job.
    toolbar: Toolbar,
    /// Output of the last build and program run.
    output: OutputLog,
    /// Breakpoints in files that aren't open (open files keep theirs in the
    /// editor, which moves them as the text changes). By [`file_key`].
    closed_breakpoints: HashMap<PathBuf, (PathBuf, Vec<usize>)>,
    /// The breakpoints version of each open file last sent to the debugger.
    breakpoints_sent: HashMap<PathBuf, u64>,
    /// Where the debugger is stopped (the yellow arrow), if it is.
    exec_location: Option<Location>,
    /// Set when running the scripted profiling scenario.
    profile_run: Option<ProfileRun>,
    /// A project or solution being loaded in the background.
    project_loading: Option<(PathBuf, std::sync::mpsc::Receiver<Result<LoadedProject, String>>)>,
    /// The open Unreal solution's reflection macros and specifiers, if any.
    unreal: Option<Arc<UnrealProfile>>,
    /// Whether `unreal` came from its cache rather than the engine's files.
    unreal_from_cache: bool,
    /// Kept so background work can ask for a repaint.
    ctx: egui::Context,
}

/// Free something large (a solution's tree, a symbol index with millions of
/// entries) on another thread, so the UI doesn't stall while it's dropped.
fn drop_in_background<T: Send + 'static>(value: T) {
    std::thread::spawn(move || drop(value));
}

/// A project or solution loaded in the background, with its Unreal profile
/// (and whether that came from the cache) if it's an Unreal solution.
struct LoadedProject {
    view: ProjectView,
    unreal: Option<(Arc<UnrealProfile>, bool)>,
}

/// The tabs of the bottom panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BottomTab {
    Search,
    Output,
}

/// Identifies a file however its path is spelled (case, short 8.3 names),
/// for breakpoints, which the debugger reports by full path. Touches the
/// disk, so only for occasional use.
fn file_key(path: &Path) -> PathBuf {
    path_key(&debug::long_path(path))
}

/// Whether go-to-definition and indexing apply to this file.
fn is_cpp(path: &Path) -> bool {
    syntax::detect(path).is_some_and(|language| language.name == "C++")
}

impl ZbuggerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        app.ctx = cc.egui_ctx.clone();
        app.profile_run = ProfileRun::from_env();
        if let Some(prefs) = cc.storage.and_then(|s| eframe::get_value(s, preferences::STORAGE_KEY)) {
            app.prefs = prefs;
        }
        app.prefs.apply(&cc.egui_ctx);
        if let Some(saved) = cc.storage.and_then(|s| eframe::get_value(s, toolbar::STORAGE_KEY)) {
            app.toolbar.saved = saved;
        }
        let breakpoints: Vec<(PathBuf, Vec<usize>)> =
            cc.storage.and_then(|s| eframe::get_value(s, BREAKPOINTS_STORAGE_KEY)).unwrap_or_default();
        app.closed_breakpoints =
            breakpoints.into_iter().filter(|(_, lines)| !lines.is_empty()).map(|(path, lines)| (file_key(&path), (path, lines))).collect();
        // Reopen the last project, if it still exists (not when profiling,
        // which opens what's on the command line).
        let last_project: Option<PathBuf> = cc.storage.and_then(|s| eframe::get_value(s, PROJECT_STORAGE_KEY)).flatten();
        if let Some(path) = last_project.filter(|p| p.exists() && app.profile_run.is_none()) {
            app.open_project(&path);
        }
        // `zbugger a.cpp b.h ...` opens each file in its own tab; a .vcxproj opens as the project.
        for path in std::env::args_os().skip(1) {
            app.open_path(Path::new(&path));
        }
        app.update_title(&cc.egui_ctx);
        app
    }

    fn active_doc(&self) -> Option<&Document> {
        self.documents.get(self.active)
    }

    fn open_file_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Open Files");
        if let Some(dir) = self.active_doc().and_then(|d| d.path.parent()) {
            dialog = dialog.set_directory(dir);
        }
        for path in dialog.pick_files().unwrap_or_default() {
            self.open_path(&path);
        }
    }

    fn open_project_dialog(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open Project or Solution")
            .add_filter("Solutions and projects", &["sln", "slnx", "vcxproj"])
            .add_filter("Visual Studio Solution", &["sln", "slnx"])
            .add_filter("Visual C++ Project", &["vcxproj"]);
        if let Some(dir) = self.project.as_ref().and_then(|view| view.path().parent()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_project(&path);
        }
    }

    /// Start loading a project or solution in the background. Large solutions
    /// take a second or more, so this keeps the UI responsive.
    fn open_project(&mut self, path: &Path) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        let load_path = path.to_path_buf();
        std::thread::Builder::new()
            .name("project-loader".into())
            .spawn(move || {
                let loaded = ProjectView::open(&load_path).map_err(|e| e.to_string()).map(|view| {
                    // For Unreal solutions, read (or reuse) the engine's macro profile.
                    let unreal = view
                        .unreal_engine()
                        .and_then(|(engine, modules)| unreal::load_profile(engine, modules))
                        .map(|(profile, from_cache)| (Arc::new(profile), from_cache));
                    LoadedProject { view, unreal }
                });
                let _ = sender.send(loaded);
                ctx.request_repaint();
            })
            .expect("can spawn the project loading thread");
        self.project_loading = Some((path.to_path_buf(), receiver));
        self.project_panel_hidden = false;
    }

    /// Switch Unreal macro support for the open solution, re-highlighting open files.
    fn set_unreal(&mut self, unreal: Option<(Arc<UnrealProfile>, bool)>) {
        self.unreal_from_cache = unreal.as_ref().is_some_and(|(_, cached)| *cached);
        self.unreal = unreal.map(|(profile, _)| profile);
        for doc in &mut self.documents {
            doc.editor.set_unreal(self.unreal.clone());
        }
    }

    /// Install a project or solution once it has loaded, replacing what was open.
    fn poll_project_loading(&mut self) {
        zone!("poll_project_loading");
        let Some((path, receiver)) = &self.project_loading else { return };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(format!("loading {} failed", path.display())),
        };
        self.project_loading = None;
        match result {
            Ok(LoadedProject { mut view, unreal }) => {
                self.toolbar.solution_opened(view.build_model());
                self.status = None;
                self.set_unreal(unreal);
                let files = view.take_index_files();
                let cache_file = symbols::cache_path(view.path());
                drop_in_background(self.project.replace(view));
                self.project_panel_hidden = false;
                drop_in_background(std::mem::take(&mut self.symbols));
                self.index_stats = None;
                self.indexer = Some(Indexer::start(files, cache_file, self.unreal.clone(), self.ctx.clone()));
                for i in 0..self.documents.len() {
                    self.index_document(i);
                }
            }
            Err(err) => self.status = Some(format!("Failed to open project: {err}")),
        }
    }

    /// Open a file in a new tab, or switch to its tab if it is already open.
    /// Project files are loaded into the project panel instead.
    fn open_path(&mut self, path: &Path) {
        if is_project_file(path) {
            self.open_project(path);
            return;
        }
        let key = path_key(path);
        if let Some(i) = self.documents.iter().position(|d| path_key(&d.path) == key) {
            self.active = i;
            return;
        }
        match Document::open(path, self.unreal.clone()) {
            Ok(mut doc) => {
                self.status = None;
                if let Some((_, lines)) = self.closed_breakpoints.remove(&file_key(path)) {
                    doc.editor.set_breakpoint_lines(&lines);
                }
                if let Some(at) = self.exec_location.as_ref().filter(|at| path_key(&at.path) == key) {
                    doc.editor.set_exec_line(Some(at.line));
                }
                self.documents.push(doc);
                self.active = self.documents.len() - 1;
                self.index_document(self.active);
            }
            Err(err) => {
                self.status = Some(format!("Failed to open {}: {err}", path.display()));
            }
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.documents.len() {
            return;
        }
        let doc = self.documents.remove(index);
        let lines = doc.editor.breakpoint_lines();
        if !lines.is_empty() {
            self.closed_breakpoints.insert(file_key(&doc.path), (doc.path.clone(), lines));
        }
        if index < self.active || self.active >= self.documents.len() {
            self.active = self.active.saturating_sub(1);
        }
    }

    /// Move the active tab by `step` positions, wrapping around.
    fn cycle_tab(&mut self, step: isize) {
        let n = self.documents.len() as isize;
        if n > 0 {
            self.active = (self.active as isize + step).rem_euclid(n) as usize;
        }
    }

    fn save(&mut self) {
        let Some(doc) = self.documents.get_mut(self.active) else { return };
        self.status = match doc.save() {
            Ok(()) => None,
            Err(err) => Some(format!("Failed to save {}: {err}", doc.path.display())),
        };
        self.index_document(self.active);
    }

    /// Re-index one open document from its current (possibly unsaved) text.
    fn index_document(&mut self, index: usize) {
        let Some(doc) = self.documents.get(index) else { return };
        if !is_cpp(&doc.path) {
            return;
        }
        let parser = self.symbol_parser.get_or_insert_with(symbols::new_parser);
        self.symbols.update_file_text(parser, &doc.path, doc.editor.text(), self.unreal.as_deref());
    }

    /// Merge finished background indexing into the index.
    fn poll_indexer(&mut self, ctx: &egui::Context) {
        zone!("poll_indexer");
        let Some(indexer) = &self.indexer else { return };
        match indexer.try_take() {
            // Still running: keep the progress count moving.
            None => ctx.request_repaint_after(Duration::from_millis(100)),
            Some(Ok((index, stats))) => {
                // Built on the indexing thread; swap it in and free the old one elsewhere.
                drop_in_background(std::mem::replace(&mut self.symbols, index));
                self.index_stats = Some(stats);
                // The thread may still be writing the cache; keep it to wait for at exit.
                if let Some(indexer) = self.indexer.take() {
                    self.cache_writers.retain(|thread| !thread.is_finished());
                    self.cache_writers.push(indexer.into_thread());
                }
                // Open documents may have unsaved edits newer than the files on disk.
                for i in 0..self.documents.len() {
                    self.index_document(i);
                }
            }
            Some(Err(())) => {
                self.indexer = None;
                self.status = Some("Indexing the project failed".to_owned());
            }
        }
    }

    fn current_location(&self) -> Option<Location> {
        let doc = self.active_doc()?;
        let (line, column) = doc.editor.caret_position();
        Some(Location { path: doc.path.clone(), line, column })
    }

    /// Open `location`'s file and put the caret there.
    fn show_location(&mut self, location: &Location) {
        self.open_path(&location.path);
        let key = path_key(&location.path);
        if let Some(i) = self.documents.iter().position(|d| path_key(&d.path) == key) {
            self.active = i;
            self.documents[i].editor.go_to(location.line, location.column);
        }
    }

    /// Jump to `symbol`, remembering where we came from for Back.
    fn jump_to(&mut self, symbol: &Symbol) {
        if let Some(here) = self.current_location() {
            self.history.push(here);
        }
        self.show_location(&Location::from(symbol));
    }

    fn go_to_definition(&mut self) {
        let Some(doc) = self.documents.get(self.active) else { return };
        if !is_cpp(&doc.path) {
            self.status = Some("Go to definition works in C++ files".to_owned());
            return;
        }
        self.index_document(self.active); // pick up unsaved edits
        let doc = &self.documents[self.active];
        let Some(reference) = reference_at(doc.editor.text(), doc.editor.selection.head) else {
            self.status = Some("No symbol at the cursor".to_owned());
            return;
        };
        let (line, _) = doc.editor.caret_position();
        let here = path_key(&doc.path);
        let mut candidates = self.symbols.find(&reference, &doc.path);

        // On the definition itself? Then go elsewhere (e.g. to the declaration),
        // so F12 toggles between the two.
        let at_reference = |c: &symbols::Candidate| c.symbol.line == line && path_key(&c.symbol.path) == here;
        let was_found = !candidates.is_empty();
        candidates.retain(|c| !at_reference(c));

        let still_indexing = if self.indexer.is_some() { " (the project is still being indexed)" } else { "" };
        match candidates.as_slice() {
            [] if was_found => self.status = Some(format!("Already at the only definition of {}", reference.name)),
            [] => self.status = Some(format!("No definition found for {}{still_indexing}", reference.name)),
            // One candidate, or one that ranks clearly above the rest: go there.
            [only] => {
                let symbol = only.symbol.clone();
                self.jump_to(&symbol);
            }
            [best, second, ..] if best.score > second.score => {
                let symbol = best.symbol.clone();
                self.jump_to(&symbol);
            }
            _ => {
                self.status = None;
                let symbols = candidates.into_iter().map(|c| c.symbol).collect();
                self.picker = Some(DefinitionPicker::new(reference.name, symbols));
            }
        }
    }

    fn navigate_back(&mut self) {
        let current = self.current_location();
        if let Some(target) = self.history.back(current) {
            self.show_location(&target);
        }
    }

    fn navigate_forward(&mut self) {
        let current = self.current_location();
        if let Some(target) = self.history.forward(current) {
            self.show_location(&target);
        }
    }

    /// Keep the window title in sync with the active file and its unsaved state.
    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.active_doc() {
            Some(doc) if doc.is_dirty() => format!("{}* - Zbugger", doc.file_name()),
            Some(doc) => format!("{} - Zbugger", doc.file_name()),
            None => "Zbugger".to_owned(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        zone!("handle_input");
        if let Some(picker) = &mut self.picker {
            match picker.handle_keys(ctx) {
                PickerResult::Open => {}
                PickerResult::Chosen(symbol) => {
                    self.picker = None;
                    self.jump_to(&symbol);
                }
                PickerResult::Cancelled => self.picker = None,
            }
        }
        let mouse_back = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Extra1));
        let mouse_forward = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Extra2));
        if ctx.input_mut(|i| i.consume_shortcut(&BACK_SHORTCUT)) || mouse_back {
            self.navigate_back();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&FORWARD_SHORTCUT)) || mouse_forward {
            self.navigate_forward();
        }
        // Shortcuts are consumed before the text editor sees them, so Ctrl+Tab
        // switches tabs instead of inserting a tab character.
        let pressed = |shortcut| ctx.input_mut(|i| i.consume_shortcut(shortcut));
        let open = pressed(&OPEN_SHORTCUT);
        let save = pressed(&SAVE_SHORTCUT);
        // Check Ctrl+Shift+F first: Ctrl+F alone could also match it.
        // Shift+F5 before F5, which could also match it.
        if pressed(&STOP_SHORTCUT) {
            self.toolbar_action(ToolbarAction::Stop);
        } else if pressed(&RUN_SHORTCUT) {
            self.toolbar_action(ToolbarAction::Run);
        } else if pressed(&PLAY_SHORTCUT) {
            self.toolbar_action(ToolbarAction::Play);
        }
        if pressed(&BREAKPOINT_SHORTCUT) {
            self.toggle_breakpoint_at_caret();
        }
        // Shift+F11 before F11, which could also match it.
        if pressed(&STEP_OUT_SHORTCUT) {
            self.toolbar_action(ToolbarAction::StepOut);
        } else if pressed(&STEP_IN_SHORTCUT) {
            self.toolbar_action(ToolbarAction::StepIn);
        }
        if pressed(&STEP_OVER_SHORTCUT) {
            self.toolbar_action(ToolbarAction::StepOver);
        }
        if pressed(&BUILD_SHORTCUT) {
            self.toolbar_action(ToolbarAction::Build);
        }
        if pressed(&FIND_IN_FILES_SHORTCUT) {
            self.open_search_panel();
        } else if pressed(&FIND_SHORTCUT) {
            self.open_find();
        }
        // Check Shift+F3 first: F3 alone would also match it.
        if pressed(&FIND_PREVIOUS_SHORTCUT) {
            self.find_step(false);
        } else if pressed(&FIND_NEXT_SHORTCUT) {
            self.find_step(true);
        }
        if pressed(&PROJECT_PANEL_SHORTCUT) {
            self.project_panel_hidden = !self.project_panel_hidden;
        }
        let close = pressed(&CLOSE_TAB_SHORTCUT);
        // Check the Shift variant first: Ctrl+Tab would also match Ctrl+Shift+Tab.
        let prev = pressed(&PREV_TAB_SHORTCUT);
        let next = pressed(&NEXT_TAB_SHORTCUT);

        if open {
            self.open_file_dialog();
        }
        if save {
            self.save();
        }
        if close {
            self.close_tab(self.active);
        }
        if prev {
            self.cycle_tab(-1);
        }
        if next {
            self.cycle_tab(1);
        }

        // Dragging files onto the window opens each one in a tab.
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            self.open_path(&path);
        }
    }

    fn menu_bar(&mut self, ctx: &egui::Context) {
        zone!("menu_bar");
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    let has_doc = !self.documents.is_empty();
                    let open = egui::Button::new("Open...")
                        .shortcut_text(ctx.format_shortcut(&OPEN_SHORTCUT));
                    if ui.add(open).clicked() {
                        ui.close_menu();
                        self.open_file_dialog();
                    }
                    if ui.button("Open Project or Solution...").clicked() {
                        ui.close_menu();
                        self.open_project_dialog();
                    }
                    let has_project = self.project.is_some() || self.project_loading.is_some();
                    if ui.add_enabled(has_project, egui::Button::new("Close Project or Solution")).clicked() {
                        ui.close_menu();
                        drop_in_background(self.project.take());
                        self.set_unreal(None);
                        // Drop any background work for it; results arriving later are ignored.
                        self.project_loading = None;
                        self.indexer = None;
                        drop_in_background(std::mem::take(&mut self.symbols));
                        for i in 0..self.documents.len() {
                            self.index_document(i);
                        }
                    }
                    ui.separator();
                    let save = egui::Button::new("Save")
                        .shortcut_text(ctx.format_shortcut(&SAVE_SHORTCUT));
                    if ui.add_enabled(has_doc, save).clicked() {
                        ui.close_menu();
                        self.save();
                    }
                    let close = egui::Button::new("Close Tab")
                        .shortcut_text(ctx.format_shortcut(&CLOSE_TAB_SHORTCUT));
                    if ui.add_enabled(has_doc, close).clicked() {
                        ui.close_menu();
                        self.close_tab(self.active);
                    }
                    ui.separator();
                    if ui.button("Preferences...").clicked() {
                        ui.close_menu();
                        self.prefs_open = true;
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ui.close_menu();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    let has_doc = !self.documents.is_empty();
                    let find_in_files = egui::Button::new("Find in Files...")
                        .shortcut_text(ctx.format_shortcut(&FIND_IN_FILES_SHORTCUT));
                    if ui.add(find_in_files).clicked() {
                        ui.close_menu();
                        self.open_search_panel();
                    }
                    let find = egui::Button::new("Find...").shortcut_text(ctx.format_shortcut(&FIND_SHORTCUT));
                    if ui.add_enabled(has_doc, find).clicked() {
                        ui.close_menu();
                        self.open_find();
                    }
                    let has_query = has_doc && self.find.has_query();
                    let next = egui::Button::new("Find Next").shortcut_text(ctx.format_shortcut(&FIND_NEXT_SHORTCUT));
                    if ui.add_enabled(has_query, next).clicked() {
                        ui.close_menu();
                        self.find_step(true);
                    }
                    let previous =
                        egui::Button::new("Find Previous").shortcut_text(ctx.format_shortcut(&FIND_PREVIOUS_SHORTCUT));
                    if ui.add_enabled(has_query, previous).clicked() {
                        ui.close_menu();
                        self.find_step(false);
                    }
                });
                self.debug_menu(ui, ctx);
                ui.menu_button("View", |ui| {
                    let mut shown = !self.project_panel_hidden;
                    let toggle = egui::Checkbox::new(&mut shown, "Project Panel");
                    if ui.add(toggle).on_hover_text(ctx.format_shortcut(&PROJECT_PANEL_SHORTCUT)).changed() {
                        self.project_panel_hidden = !shown;
                        ui.close_menu();
                    }
                });
            });
        });
    }

    fn project_panel(&mut self, ctx: &egui::Context) {
        zone!("project_panel");
        if self.project_panel_hidden || self.project.is_none() {
            return;
        }
        let active = self.active_doc().map(|d| d.path.clone());
        let Some(view) = &mut self.project else { return };
        let clicked = egui::SidePanel::left("project_panel")
            .resizable(true)
            .default_width(260.0)
            .width_range(120.0..=800.0)
            .show(ctx, |ui| view.ui(ui, active.as_deref()))
            .inner;
        if let Some(path) = clicked {
            self.open_path(&path);
        }
    }

    fn tab_bar(&mut self, ctx: &egui::Context) {
        zone!("tab_bar");
        if self.documents.is_empty() {
            return;
        }
        let mut activate = None;
        let mut close = None;

        let prefs = &self.prefs;
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for (i, doc) in self.documents.iter().enumerate() {
                        match tab(ui, doc, i == self.active, prefs) {
                            TabAction::None => {}
                            TabAction::Activate => activate = Some(i),
                            TabAction::Close => close = Some(i),
                        }
                    }
                });
            });
        });

        if let Some(i) = activate {
            self.active = i;
        }
        if let Some(i) = close {
            self.close_tab(i);
        }
    }

    fn status_bar(&self, ctx: &egui::Context) {
        zone!("status_bar");
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some((path, _)) = &self.project_loading {
                    let name = crate::project::file_name(path);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spinner();
                        ui.label(format!("Loading {name}"));
                    });
                } else if let Some(indexer) = &self.indexer {
                    let text = match indexer.progress() {
                        (Phase::LoadingCache, _, _) => "Loading symbol cache".to_owned(),
                        (Phase::CheckingFiles, done, total) => format!("Checking {done}/{total} files for changes"),
                        (Phase::Parsing, done, total) => format!("Indexing {done}/{total} changed files"),
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spinner();
                        ui.label(text);
                    });
                } else if self.project.is_some() {
                    let mut summary =
                        format!("{} symbols in {} files", self.symbols.symbol_count(), self.symbols.file_count());
                    if let Some(stats) = self.index_stats {
                        summary.push_str(&format!(" ({} from cache, {} parsed)", stats.from_cache, stats.parsed));
                    }
                    if let Some(profile) = &self.unreal {
                        summary.push_str(&format!(
                            " · Unreal: {} specifiers, {} modules{}",
                            profile.specifier_count(),
                            profile.export_macro_count(),
                            if self.unreal_from_cache { " (cached)" } else { "" }
                        ));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(summary).weak());
                    });
                }
                if let Some(status) = &self.status {
                    ui.colored_label(ui.visuals().error_fg_color, status);
                } else if let Some(doc) = self.active_doc() {
                    ui.label(doc.path.display().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(doc.language_name());
                    });
                } else {
                    ui.label("No file open");
                }
            });
        });
    }

    /// Open Find in Files, searching for the selection if there is one.
    fn open_search_panel(&mut self) {
        let selection = self.active_doc().filter(|d| d.image.is_none()).map(|d| d.editor.selected_text().to_owned());
        self.search_panel.open(selection.as_deref());
        self.bottom_tab = Some(BottomTab::Search);
    }

    /// The bottom panel: Find in Files and Output, as tabs.
    fn bottom_panel_ui(&mut self, ctx: &egui::Context) {
        let Some(mut tab) = self.bottom_tab else { return };
        let documents = &self.documents;
        let project = &self.project;
        let mut start = |query: SearchQuery| {
            // Open tabs are searched from memory, so unsaved edits count.
            let open: HashMap<PathBuf, String> = documents
                .iter()
                .filter(|d| d.image.is_none())
                .map(|d| (path_key(&d.path), d.editor.text().to_owned()))
                .collect();
            // The solution's files, plus open tabs that aren't part of it.
            let outside: Vec<PathBuf> = documents
                .iter()
                .filter(|d| d.image.is_none() && !project.as_ref().is_some_and(|p| p.contains_file(&d.path)))
                .map(|d| d.path.clone())
                .collect();
            let files = match project {
                Some(view) if outside.is_empty() => view.search_files(),
                Some(view) => Arc::new(view.search_files().iter().cloned().chain(outside).collect()),
                None => Arc::new(outside),
            };
            Search::start(query, files, open, ctx.clone())
        };
        let panel = &mut self.search_panel;
        let output = &mut self.output;
        let mut close = false;
        let (jump, location) = egui::TopBottomPanel::bottom("bottom_panel")
            .resizable(true)
            .default_height(260.0)
            .height_range(120.0..=900.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut tab, BottomTab::Search, "Find in Files");
                    ui.selectable_value(&mut tab, BottomTab::Output, format!("Output ({})", output.summary()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui.button("×").on_hover_text("Close the panel").clicked();
                        if tab == BottomTab::Output && ui.button("Clear").clicked() {
                            output.clear();
                        }
                    });
                });
                ui.separator();
                match tab {
                    BottomTab::Search => (panel.ui(ui, &mut start), None),
                    BottomTab::Output => (None, output.ui(ui)),
                }
            })
            .inner;
        self.bottom_tab = if close { None } else { Some(tab) };
        if close {
            self.search_panel.stop();
        }
        if let Some(jump) = jump {
            self.jump_to_match(Location { path: jump.path, line: jump.line, column: jump.column }, jump.len);
        }
        if let Some(at) = location {
            self.jump_to_match(Location { path: at.path, line: at.line, column: at.column }, 0);
        }
    }

    /// The build toolbar, under the menu bar.
    fn toolbar_ui(&mut self, ctx: &egui::Context) {
        // Collect build/program output.
        if let Some(job) = &self.toolbar.job {
            for event in job.events() {
                self.output.push(event);
            }
            let state = job.state();
            if state.is_active() {
                ctx.request_repaint_after(Duration::from_millis(100));
            } else if std::mem::take(&mut self.toolbar.debug_after_build) && state == JobState::Built {
                if let Some(run) = self.toolbar.job.as_ref().and_then(|job| job.plan().run.clone()) {
                    self.start_debugging(&run);
                }
            }
        }
        self.poll_debugger();
        let model = self.project.as_ref().map(|view| view.build_model());
        let toolbar = &mut self.toolbar;
        let action = egui::TopBottomPanel::top("toolbar").show(ctx, |ui| toolbar.ui(ui, model)).inner;
        if let Some(action) = action {
            self.toolbar_action(action);
        }
    }

    /// Play, Build, Stop and the debugger's controls.
    fn toolbar_action(&mut self, action: ToolbarAction) {
        let debug_state = self.toolbar.debug_state();
        let debug_command = match action {
            ToolbarAction::Stop => {
                self.toolbar.debug_after_build = false;
                if let Some(job) = &self.toolbar.job {
                    job.stop();
                }
                Some(DebugCommand::Stop)
            }
            ToolbarAction::Play if debug_state == Some(DebugState::Stopped) => Some(DebugCommand::Continue),
            ToolbarAction::Pause => Some(DebugCommand::Pause),
            ToolbarAction::StepOver => Some(DebugCommand::StepOver),
            ToolbarAction::StepIn => Some(DebugCommand::StepIn),
            ToolbarAction::StepOut => Some(DebugCommand::StepOut),
            _ => None,
        };
        if let Some(command) = debug_command {
            if let (Some(session), Some(_)) = (&self.toolbar.debug, debug_state) {
                session.send(command);
            }
            return;
        }
        if self.toolbar.is_busy() {
            return;
        }
        let (Some(view), Some(selection)) = (&self.project, self.toolbar.selection.clone()) else { return };
        let model = view.build_model();
        let Some(project) = model.projects.iter().find(|p| p.name == selection.project) else { return };
        self.output.clear();
        self.bottom_tab = Some(BottomTab::Output);
        let plan = match model.plan(project, &selection.solution_configuration()) {
            Ok(plan) => plan,
            Err(err) => {
                self.output.push(JobEvent::Status(err));
                return;
            }
        };
        // Like Visual Studio: save edited files before building.
        let mut saved = 0;
        for doc in self.documents.iter_mut().filter(|d| d.is_dirty()) {
            match doc.save() {
                Ok(()) => saved += 1,
                Err(err) => self.output.push(JobEvent::Status(format!("Couldn't save {}: {err}", doc.path.display()))),
            }
        }
        if saved > 0 {
            self.output.push(JobEvent::Status(format!("Saved {saved} file{}", if saved == 1 { "" } else { "s" })));
        }
        let debug = action == ToolbarAction::Play;
        if debug && plan.run.is_none() {
            self.output.push(JobEvent::Status(format!("{} doesn't produce a program to debug", plan.project)));
        }
        let debug = debug && plan.run.is_some();
        if debug && plan.build.is_none() {
            // Nothing to build in this configuration: debug what's there.
            let run = plan.run.clone().expect("checked above");
            self.start_debugging(&run);
            return;
        }
        self.toolbar.debug_after_build = debug;
        self.toolbar.job = Some(Job::start(plan, action == ToolbarAction::Run, self.ctx.clone()));
    }

    /// Start LLDB on `run`'s program, with every breakpoint set.
    fn start_debugging(&mut self, run: &RunStep) {
        if !run.program.is_file() {
            self.output.push(JobEvent::Status(format!("Program not found: {}", run.program.display())));
            return;
        }
        let adapter = match debug::find_adapter() {
            Ok(adapter) => adapter,
            Err(err) => {
                self.output.push(JobEvent::Status(err));
                return;
            }
        };
        let cwd = if run.cwd.is_dir() { run.cwd.clone() } else { run.program.parent().map(Path::to_path_buf).unwrap_or_default() };
        let launch = Launch {
            program: run.program.clone(),
            args: debug::split_arguments(&run.arguments),
            cwd,
            breakpoints: self.all_breakpoints(),
        };
        self.output.push(JobEvent::Status(format!("Starting LLDB ({})", adapter.program.display())));
        match DebugSession::start(&adapter, launch, self.ctx.clone()) {
            Ok(session) => {
                self.breakpoints_sent =
                    self.documents.iter().map(|d| (path_key(&d.path), d.editor.breakpoints_version())).collect();
                self.toolbar.debug = Some(session);
            }
            Err(err) => self.output.push(JobEvent::Status(err)),
        }
    }

    /// Every breakpoint: open files' and closed files'.
    fn all_breakpoints(&self) -> Vec<(PathBuf, Vec<usize>)> {
        let mut all: Vec<(PathBuf, Vec<usize>)> = self
            .documents
            .iter()
            .filter(|d| !d.editor.breakpoints().is_empty())
            .map(|d| (d.path.clone(), d.editor.breakpoint_lines()))
            .collect();
        all.extend(self.closed_breakpoints.values().cloned());
        all
    }

    /// F9: add or remove a breakpoint on the caret's line.
    fn toggle_breakpoint_at_caret(&mut self) {
        if let Some(doc) = self.documents.get_mut(self.active).filter(|d| d.image.is_none()) {
            let (line, _) = doc.editor.caret_position();
            doc.editor.toggle_breakpoint(line);
        }
    }

    /// Handle what the debugger reports, and tell it about breakpoint changes.
    fn poll_debugger(&mut self) {
        let Some(session) = &self.toolbar.debug else { return };
        for event in session.events() {
            match event {
                DebugEvent::Output(line) => self.output.push(JobEvent::Output(line)),
                DebugEvent::Status(text) => self.output.push(JobEvent::Status(text)),
                DebugEvent::Stopped { reason, description, frames } => self.debugger_stopped(&reason, description, &frames),
                DebugEvent::Continued => self.set_exec_location(None),
                DebugEvent::Breakpoints { path, breakpoints } => {
                    let key = path_key(&path);
                    if let Some(doc) = self.documents.iter_mut().find(|d| path_key(&d.path) == key) {
                        for status in breakpoints {
                            doc.editor.set_breakpoint_verified(status.requested, Some(status.verified));
                        }
                    }
                }
                DebugEvent::Ended { .. } => {
                    self.set_exec_location(None);
                    for doc in &mut self.documents {
                        doc.editor.clear_breakpoint_status();
                    }
                    self.status = None;
                }
            }
        }
        // Breakpoints added, removed or moved by edits while debugging.
        let Some(session) = self.toolbar.debug.as_ref().filter(|s| s.state().is_active()) else { return };
        for doc in self.documents.iter().filter(|d| d.image.is_none()) {
            let version = doc.editor.breakpoints_version();
            let key = path_key(&doc.path);
            let sent = self.breakpoints_sent.get(&key).copied();
            // Files opened during the session only need sending if they changed.
            if sent.is_none_or(|sent| sent != version) && (sent.is_some() || version > 1) {
                session.send(DebugCommand::SetBreakpoints { path: doc.path.clone(), lines: doc.editor.breakpoint_lines() });
            }
            self.breakpoints_sent.insert(key, version);
        }
    }

    /// The program stopped: show where, with the yellow arrow.
    fn debugger_stopped(&mut self, reason: &str, description: Option<String>, frames: &[Frame]) {
        let why = description.unwrap_or_else(|| reason.to_owned());
        // The innermost frame with source we have (system DLLs have none).
        let Some((frame, path)) = frames.iter().find_map(|f| f.path.as_ref().filter(|p| p.is_file()).map(|p| (f, p))) else {
            let name = frames.first().map_or("unknown code", |f| f.name.as_str());
            self.status = Some(format!("Paused ({why}) in {name}, which has no source"));
            self.set_exec_location(None);
            return;
        };
        // Reuse an open tab for the file even if it was opened by another
        // spelling of its path (short 8.3 names...).
        let long = debug::long_path(path);
        let path = self
            .documents
            .iter()
            .find(|d| d.image.is_none() && debug::long_path(&d.path) == long)
            .map_or_else(|| path.clone(), |d| d.path.clone());
        let location = Location { path, line: frame.line, column: frame.column };
        if let Some(here) = self.current_location() {
            self.history.push(here);
        }
        self.show_location(&location);
        // After opening the file, which clears the status.
        self.status = Some(format!(
            "Paused ({why}) in {} at {}:{}",
            frame.name,
            crate::project::file_name(&location.path),
            frame.line + 1
        ));
        self.set_exec_location(Some(location));
        // Bring Zbugger to the front, like other debuggers do.
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    /// Move the yellow arrow (or remove it).
    fn set_exec_location(&mut self, location: Option<Location>) {
        let key = location.as_ref().map(|at| path_key(&at.path));
        for doc in &mut self.documents {
            let line = location.as_ref().filter(|_| key.as_ref() == Some(&path_key(&doc.path))).map(|at| at.line);
            doc.editor.set_exec_line(line);
        }
        self.exec_location = location;
    }

    /// The Debug menu.
    fn debug_menu(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.menu_button("Debug", |ui| {
            let state = self.toolbar.debug_state();
            let can_start = !self.toolbar.is_busy() && self.toolbar.selection.is_some() && self.project.is_some();
            let item = |ui: &mut egui::Ui, enabled: bool, label: &str, shortcut: &KeyboardShortcut| {
                let button = egui::Button::new(label).shortcut_text(ctx.format_shortcut(shortcut));
                let clicked = ui.add_enabled(enabled, button).clicked();
                if clicked {
                    ui.close_menu();
                }
                clicked
            };
            let stopped = state == Some(DebugState::Stopped);
            let start_label = if stopped { "Continue" } else { "Start Debugging" };
            if item(ui, can_start || stopped, start_label, &PLAY_SHORTCUT) {
                self.toolbar_action(ToolbarAction::Play);
            }
            if item(ui, can_start, "Run Without Debugging", &RUN_SHORTCUT) {
                self.toolbar_action(ToolbarAction::Run);
            }
            if item(ui, self.toolbar.is_busy(), "Stop", &STOP_SHORTCUT) {
                self.toolbar_action(ToolbarAction::Stop);
            }
            ui.separator();
            if item(ui, stopped, "Step Over", &STEP_OVER_SHORTCUT) {
                self.toolbar_action(ToolbarAction::StepOver);
            }
            if item(ui, stopped, "Step Into", &STEP_IN_SHORTCUT) {
                self.toolbar_action(ToolbarAction::StepIn);
            }
            if item(ui, stopped, "Step Out", &STEP_OUT_SHORTCUT) {
                self.toolbar_action(ToolbarAction::StepOut);
            }
            ui.separator();
            let has_doc = self.active_doc().is_some_and(|d| d.image.is_none());
            if item(ui, has_doc, "Toggle Breakpoint", &BREAKPOINT_SHORTCUT) {
                self.toggle_breakpoint_at_caret();
            }
            let any = !self.closed_breakpoints.is_empty() || self.documents.iter().any(|d| !d.editor.breakpoints().is_empty());
            if ui.add_enabled(any, egui::Button::new("Delete All Breakpoints")).clicked() {
                ui.close_menu();
                self.closed_breakpoints.clear();
                for doc in &mut self.documents {
                    doc.editor.set_breakpoint_lines(&[]);
                }
            }
        });
    }

    /// Open a search result with the match selected, remembering where we came from.
    fn jump_to_match(&mut self, location: Location, len: usize) {
        if let Some(here) = self.current_location() {
            self.history.push(here);
        }
        self.show_location(&location);
        if let Some(doc) = self.documents.get_mut(self.active) {
            if path_key(&doc.path) == path_key(&location.path) {
                let head = doc.editor.selection.head;
                let end = (head + len).min(doc.editor.text().len());
                if doc.editor.text().is_char_boundary(end) {
                    doc.editor.select(Selection { anchor: head, head: end });
                    doc.editor.scroll_center = true;
                }
            }
        }
    }

    fn open_find(&mut self) {
        if let Some(doc) = self.documents.get(self.active).filter(|doc| doc.image.is_none()) {
            self.find.open(&doc.editor);
        }
    }

    /// Go to the next or previous match of the last search (F3 / Shift+F3).
    fn find_step(&mut self, forward: bool) {
        if let Some(doc) = self.documents.get_mut(self.active) {
            if self.find.has_query() {
                self.find.step(&doc.path, &mut doc.editor, forward);
            }
        }
    }

    fn text_view(&mut self, ctx: &egui::Context) {
        zone!("text_view");
        let output = egui::CentralPanel::default()
            .show(ctx, |ui| {
                let Some(doc) = self.documents.get_mut(self.active) else {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("File > Open (or drop files here) to view source").weak());
                    });
                    return None;
                };
                if let Some(image) = &mut doc.image {
                    image.ui(ui, &doc.path);
                    return None;
                }
                let editor_rect = ui.max_rect();
                let output = editor_view(ui, &mut doc.editor, &doc.path, &self.prefs, self.find.highlights().as_ref());
                // Drawn after the editor so it floats on top of it.
                self.find.ui(ui.ctx(), editor_rect, &doc.path, &mut doc.editor);
                Some(output)
            })
            .inner;
        if output.is_some_and(|o| o.go_to_definition) {
            self.go_to_definition();
        }
    }

    fn definition_picker(&mut self, ctx: &egui::Context) {
        let Some(picker) = &mut self.picker else { return };
        match picker.show(ctx) {
            PickerResult::Open => {}
            PickerResult::Chosen(symbol) => {
                self.picker = None;
                self.jump_to(&symbol);
            }
            PickerResult::Cancelled => self.picker = None,
        }
    }

    fn preferences_window(&mut self, ctx: &egui::Context) {
        let mut changed = false;
        egui::Window::new("Preferences")
            .open(&mut self.prefs_open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| changed = self.prefs.ui(ui));
        if changed {
            self.prefs.apply(ctx);
        }
    }
}

enum TabAction {
    None,
    Activate,
    Close,
}

/// Draw one tab. Every tab has the same size, set by the tab preferences;
/// names longer than the preferred length are shortened with "…".
fn tab(ui: &mut egui::Ui, doc: &Document, selected: bool, prefs: &Preferences) -> TabAction {
    let scale = prefs.tab_scale;
    let font = FontId::proportional(TextStyle::Button.resolve(ui.style()).size * scale);
    let height = ui.spacing().interact_size.y * scale;
    let pad = 6.0 * scale;
    let close_size = height * 0.7;

    // Room for the longest allowed name plus the unsaved marker.
    let char_width = ui.fonts(|f| f.glyph_width(&font, 'n'));
    let text_width = char_width * (prefs.tab_name_max_chars.max(preferences::MIN_TAB_NAME_CHARS) + 1) as f32;
    let width = pad + text_width + pad + close_size + pad * 0.5;

    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    let response = response.on_hover_text(doc.path.display().to_string());
    // Registered after the tab, so it takes clicks over the tab beneath it.
    let close_rect = Rect::from_center_size(
        pos2(rect.right() - pad * 0.5 - close_size / 2.0, rect.center().y),
        vec2(close_size, close_size),
    );
    let close_response = ui.interact(close_rect, response.id.with("close"), Sense::click());

    if ui.is_rect_visible(rect) {
        let visuals = ui.visuals();
        let (fill, text_color) = if selected {
            (visuals.selection.bg_fill, visuals.selection.stroke.color)
        } else if response.hovered() {
            (visuals.widgets.hovered.weak_bg_fill, visuals.widgets.hovered.text_color())
        } else {
            (visuals.widgets.inactive.weak_bg_fill, visuals.widgets.inactive.text_color())
        };
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius { nw: 4, ne: 4, sw: 0, se: 0 }, fill);

        let mut label = preferences::truncate_name(&doc.file_name(), prefs.tab_name_max_chars);
        if doc.is_dirty() {
            label.push('*');
        }
        let text_rect = Rect::from_min_max(rect.min, pos2(close_rect.left(), rect.max.y));
        painter.with_clip_rect(text_rect).text(
            pos2(rect.left() + pad, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font.clone(),
            text_color,
        );

        if close_response.hovered() {
            painter.rect_filled(close_rect, 3.0, visuals.widgets.hovered.bg_fill);
        }
        let close_color = if close_response.hovered() { visuals.strong_text_color() } else { text_color.gamma_multiply(0.6) };
        painter.text(close_rect.center(), Align2::CENTER_CENTER, "×", font, close_color);
    }

    if close_response.clicked() || response.middle_clicked() {
        TabAction::Close
    } else if response.clicked() {
        TabAction::Activate
    } else {
        TabAction::None
    }
}


impl ZbuggerApp {
    /// Drive the scripted profiling run (see `profiling.rs`): once the project
    /// is loaded and indexed, expand the whole tree, render the configured
    /// number of frames, write the timings and exit.
    fn step_profile_run(&mut self, ctx: &egui::Context, frame: &eframe::Frame, update_time: std::time::Duration) {
        let Some(run) = &mut self.profile_run else { return };
        let ready = self.project.is_some() && self.project_loading.is_none() && self.indexer.is_none();
        if !run.is_measuring() {
            if ready {
                if let Some(view) = &mut self.project {
                    view.expand_all();
                }
                run.start();
            }
            ctx.request_repaint();
            return;
        }
        if run.record(update_time, frame.info().cpu_usage) {
            if let Err(err) = run.finish() {
                eprintln!("can't write profile report: {err}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            ctx.request_repaint(); // keep frames coming
        }
    }
}

impl Drop for ZbuggerApp {
    fn drop(&mut self) {
        zone!("drop app");
        // The app is only dropped as the process exits, and the OS reclaims all
        // memory at once, so don't free the project tree (~100,000 nodes for a
        // large solution) piece by piece first. The symbol index is a handful of
        // large blocks and frees quickly.
        std::mem::forget(self.project.take());
        // Don't leave a build running in the background.
        if let Some(job) = &self.toolbar.job {
            if job.state() == JobState::Building {
                job.stop();
            }
        }
        // Nor a program stuck in the debugger: end it before exiting.
        if let Some(session) = self.toolbar.debug.take() {
            session.stop_and_wait(Duration::from_secs(3));
        }
        // Let a pending symbol cache write finish (a fraction of a second), or
        // the next start would have to index everything again.
        for thread in self.cache_writers.drain(..) {
            let _ = thread.join();
        }
    }
}

impl eframe::App for ZbuggerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let frame_start = std::time::Instant::now();
        {
            zone!("update");
            self.update_ui(ctx);
        }
        // One Tracy frame per egui frame, so per-frame times show up in Tracy.
        tracy_client::frame_mark();
        self.step_profile_run(ctx, frame, frame_start.elapsed());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        zone!("save");
        // A profiling run shouldn't change the user's saved settings.
        if self.profile_run.is_some() {
            return;
        }
        eframe::set_value(storage, preferences::STORAGE_KEY, &self.prefs);
        let project_path = self.project.as_ref().map(|view| view.path().to_path_buf());
        eframe::set_value(storage, PROJECT_STORAGE_KEY, &project_path);
        eframe::set_value(storage, toolbar::STORAGE_KEY, &self.toolbar.saved);
        eframe::set_value(storage, BREAKPOINTS_STORAGE_KEY, &self.all_breakpoints());
    }
}

impl ZbuggerApp {
    /// Everything drawn and handled in one frame.
    fn update_ui(&mut self, ctx: &egui::Context) {
        self.poll_project_loading();
        self.poll_indexer(ctx);
        self.handle_input(ctx);
        // Panel order matters: the project panel takes the full height on the
        // left, so the tab bar added after it only spans the editor.
        self.menu_bar(ctx);
        self.toolbar_ui(ctx);
        self.status_bar(ctx);
        // Above the status bar, full width.
        self.bottom_panel_ui(ctx);
        self.project_panel(ctx);
        self.tab_bar(ctx);
        self.text_view(ctx);
        self.definition_picker(ctx);
        self.preferences_window(ctx);
        self.update_title(ctx);
    }
}

#[cfg(test)]
mod debug_tests {
    use std::time::Instant;

    use super::*;

    fn wait(app: &mut ZbuggerApp, what: &str, done: &dyn Fn(&ZbuggerApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            app.poll_debugger();
            if done(app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Breakpoints set in the editor stop a real program under LLDB, the
    /// stop opens the file with the arrow on the line, and breakpoints added
    /// while debugging take effect.
    /// `cargo test -- --ignored --nocapture app_debugs_with_breakpoints`
    #[test]
    #[ignore]
    fn app_debugs_with_breakpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (source, run) = crate::debug::tests::build_sample(dir.path());
        let mut app = ZbuggerApp::default();
        app.open_path(&source);
        app.documents[0].editor.go_to(2, 0); // `return a + b;`
        app.toggle_breakpoint_at_caret();
        app.close_tab(0); // breakpoints outlive the tab
        assert_eq!(app.all_breakpoints(), vec![(source.clone(), vec![2])]);

        app.start_debugging(&run);
        wait(&mut app, "the breakpoint", &|app| app.exec_location.is_some());
        let doc = app.active_doc().expect("the file was opened");
        assert_eq!(debug::long_path(&doc.path), debug::long_path(&source));
        assert_eq!(doc.editor.exec_line(), Some(2));
        assert_eq!(doc.editor.breakpoint_lines(), [2]);
        assert_eq!(app.toolbar.debug_state(), Some(DebugState::Stopped));
        assert_eq!(app.status.as_deref(), Some("Paused (breakpoint 1.1) in int add(int, int) at main.cpp:3"));

        // Add a breakpoint on `return total;` while stopped, then continue to it.
        let active = app.active;
        app.documents[active].editor.toggle_breakpoint(7);
        app.poll_debugger();
        app.toolbar_action(ToolbarAction::Play);
        wait(&mut app, "the second breakpoint", &|app| app.exec_location.as_ref().is_some_and(|at| at.line == 7));
        assert_eq!(app.documents[active].editor.exec_line(), Some(7));
        assert_eq!(app.documents[active].editor.breakpoint_at(7).unwrap().verified, Some(true));

        // Stop ends the session and removes the arrow.
        app.toolbar_action(ToolbarAction::Stop);
        wait(&mut app, "the end", &|app| app.toolbar.debug_state().is_none());
        app.poll_debugger();
        assert_eq!(app.documents[active].editor.exec_line(), None);
    }
}
