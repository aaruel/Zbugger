use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use egui::{pos2, vec2, Align2, FontId, Key, KeyboardShortcut, Modifiers, Rect, RichText, Sense, TextStyle};

use crate::document::{path_key, Document};
use crate::editor::editor_view;
use crate::find::FindBar;
use crate::preferences::{self, Preferences};
use crate::navigation::{DefinitionPicker, History, Location, PickerResult};
use crate::profiling::ProfileRun;
use crate::project_view::{is_project_file, ProjectView};
use crate::symbols::{self, reference_at, Indexer, Phase, RefreshStats, Symbol, SymbolIndex};
use crate::syntax;
use crate::unreal::{self, UnrealProfile};

/// Storage key for the project to reopen on startup.
const PROJECT_STORAGE_KEY: &str = "project";

const OPEN_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const BACK_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft);
const FORWARD_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowRight);
const PROJECT_PANEL_SHORTCUT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::E);
const SAVE_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const FIND_SHORTCUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::F);
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
            Ok(doc) => {
                self.status = None;
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
        self.documents.remove(index);
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
        if pressed(&FIND_SHORTCUT) {
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
        self.status_bar(ctx);
        self.project_panel(ctx);
        self.tab_bar(ctx);
        self.text_view(ctx);
        self.definition_picker(ctx);
        self.preferences_window(ctx);
        self.update_title(ctx);
    }
}
