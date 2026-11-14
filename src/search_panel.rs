//! The Find in Files panel (Ctrl+Shift+F): a query box with options, and the
//! results grouped by file. Only visible result rows are drawn.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{pos2, vec2, Color32, FontId, Id, Key, Modifiers, RichText, Sense, TextStyle, TextWrapMode, Ui, WidgetText};

use crate::project::file_name;
use crate::search::{FileMatches, Search, SearchQuery, MAX_MATCHES};

/// Typing starts a search after this pause.
const TYPING_PAUSE: Duration = Duration::from_millis(250);
/// Searching as you type starts at this many characters (Enter searches any length).
const MIN_AUTO_LENGTH: usize = 2;

/// Where a clicked result points.
pub struct SearchJump {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub len: usize,
}

#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Match(usize, usize),
}

#[derive(Default)]
pub struct SearchPanel {
    pub open: bool,
    query: SearchQuery,
    search: Option<Search>,
    results: Vec<FileMatches>,
    match_count: usize,
    rows: Vec<Row>,
    rows_dirty: bool,
    /// Set when the query changed; the search starts after [`TYPING_PAUSE`].
    changed_at: Option<Instant>,
    /// The query the current results are for.
    searched: Option<SearchQuery>,
    error: Option<String>,
    focus_query: bool,
    /// The row last clicked.
    selected: Option<usize>,
    content_width: f32,
}

/// The query box's widget id: fixed, so focus stays put while the rest of the
/// panel changes.
fn query_box_id() -> Id {
    Id::new("find_in_files_query")
}

impl SearchPanel {
    /// Open the panel (Ctrl+Shift+F), taking a single-line selection as the query.
    pub fn open(&mut self, selection: Option<&str>) {
        if let Some(selection) = selection.filter(|s| !s.is_empty() && !s.contains('\n')) {
            if selection != self.query.text {
                self.query.text = selection.to_owned();
                self.changed_at = Some(Instant::now() - TYPING_PAUSE); // search right away
            }
        }
        self.open = true;
        self.focus_query = true;
    }

    fn close(&mut self) {
        self.open = false;
        self.search = None; // stops a running search
    }

    /// Start a search for the current query with `start`, replacing any running one.
    fn run(&mut self, start: &mut dyn FnMut(SearchQuery) -> Result<Search, String>) {
        self.changed_at = None;
        self.search = None;
        self.results.clear();
        self.match_count = 0;
        self.rows_dirty = true;
        self.selected = None;
        self.searched = Some(self.query.clone());
        match start(self.query.clone()) {
            Ok(search) => {
                self.error = None;
                self.search = Some(search);
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn collect_results(&mut self) {
        let Some(search) = &mut self.search else { return };
        let new = search.take_results();
        if !new.is_empty() {
            self.match_count += new.iter().map(|f| f.matches.len()).sum::<usize>();
            self.results.extend(new);
            self.rows_dirty = true;
        }
        if search.is_finished() && self.rows_dirty {
            // Results arrive in whatever order the workers finish; sort the final list.
            self.results.sort_by(|a, b| a.path.cmp(&b.path));
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (f, file) in self.results.iter().enumerate() {
            self.rows.push(Row::File(f));
            self.rows.extend((0..file.matches.len()).map(|m| Row::Match(f, m)));
        }
        self.rows_dirty = false;
    }

    /// Draw the panel's contents. `start` begins a search for a query.
    /// Returns the result the user clicked, if any.
    pub fn ui(&mut self, ui: &mut Ui, start: &mut dyn FnMut(SearchQuery) -> Result<Search, String>) -> Option<SearchJump> {
        zone!("SearchPanel::ui");
        let mut search_now = false;

        ui.horizontal(|ui| {
            ui.label(RichText::new("Find in Files").strong());
            let before = self.query.clone();
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.query.text)
                    .id(query_box_id())
                    .hint_text("Search the solution")
                    .desired_width(320.0)
                    .return_key(None), // Enter searches, without losing focus
            );
            if std::mem::take(&mut self.focus_query) {
                response.request_focus();
            }
            if response.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                search_now = true;
            }
            ui.toggle_value(&mut self.query.case_sensitive, "Aa").on_hover_text("Match case");
            ui.toggle_value(&mut self.query.whole_word, "W").on_hover_text("Match whole word");
            ui.toggle_value(&mut self.query.regex, ".*").on_hover_text("Regular expression");
            if self.query != before {
                self.changed_at = Some(Instant::now());
            }

            // Status on the right, close button at the far right.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("×").on_hover_text("Close").clicked() {
                    self.close();
                }
                self.status_label(ui);
            });
        });
        if !self.open {
            return None;
        }

        // Search after a pause in typing, or right away on Enter.
        if let Some(changed) = self.changed_at {
            let waited = changed.elapsed();
            let long_enough = self.query.text.chars().count() >= MIN_AUTO_LENGTH;
            if search_now || (waited >= TYPING_PAUSE && long_enough) {
                self.run(start);
            } else if long_enough {
                ui.ctx().request_repaint_after(TYPING_PAUSE - waited);
            }
        } else if search_now && !self.query.text.is_empty() {
            self.run(start);
        }

        self.collect_results();
        if self.search.as_ref().is_some_and(|s| !s.is_finished()) {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        if self.rows_dirty {
            self.rebuild_rows();
        }
        ui.separator();
        self.results_ui(ui)
    }

    fn status_label(&self, ui: &mut Ui) {
        let weak = |text: String| RichText::new(text).weak();
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(ui.visuals().error_fg_color));
            return;
        }
        let Some(search) = &self.search else {
            if self.searched.is_none() {
                ui.label(weak("Enter searches; typing searches after a pause".to_owned()));
            }
            return;
        };
        let files = self.results.len();
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        let text = if !search.is_finished() {
            format!(
                "Searching {}/{} files · {}",
                search.files_searched(),
                search.total_files,
                plural(self.match_count, "match")
            )
        } else if self.match_count == 0 {
            format!("No matches in {} ({:.2} s)", plural(search.total_files, "file"), search.elapsed().as_secs_f32())
        } else {
            let capped = if self.match_count >= MAX_MATCHES { " (stopped at the limit)" } else { "" };
            format!(
                "{} in {}{capped} · {:.2} s",
                plural(self.match_count, "match"),
                plural(files, "file"),
                search.elapsed().as_secs_f32()
            )
        };
        if !search.is_finished() {
            ui.spinner();
        }
        ui.label(weak(text));
    }

    fn results_ui(&mut self, ui: &mut Ui) -> Option<SearchJump> {
        let row_height = ui.spacing().interact_size.y;
        let indent = ui.spacing().indent;
        let mono = TextStyle::Monospace.resolve(ui.style());
        let mut clicked = None;

        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::both().id_salt("find_in_files_results").auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            self.rows.len(),
            |ui, range| {
                let width = ui.available_width().max(self.content_width);
                for row_index in range {
                    let row = self.rows[row_index];
                    let (rect, response) = ui.allocate_exact_size(vec2(width, row_height), Sense::click());
                    let visuals = ui.visuals();
                    let selected = self.selected == Some(row_index);
                    if selected {
                        ui.painter().rect_filled(rect, 2.0, visuals.selection.bg_fill);
                    } else if response.hovered() {
                        ui.painter().rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
                    }
                    let text_color =
                        if selected { visuals.selection.stroke.color } else { visuals.widgets.inactive.text_color() };

                    let (job, left) = match row {
                        Row::File(f) => (file_row_job(ui, &self.results[f], text_color), rect.left() + 4.0),
                        Row::Match(f, m) => {
                            let found = &self.results[f].matches[m];
                            (match_row_job(ui, found, &mono, text_color), rect.left() + 4.0 + indent)
                        }
                    };
                    let galley = WidgetText::from(job).into_galley(ui, Some(TextWrapMode::Extend), f32::INFINITY, TextStyle::Body);
                    self.content_width = self.content_width.max(left - rect.left() + galley.size().x + 8.0);
                    ui.painter().galley(pos2(left, rect.center().y - galley.size().y / 2.0), galley, text_color);

                    let response = match row {
                        Row::File(f) => response.on_hover_text(self.results[f].path.display().to_string()),
                        Row::Match(..) => response,
                    };
                    if response.clicked() {
                        self.selected = Some(row_index);
                        let (f, m) = match row {
                            Row::File(f) => (f, 0),
                            Row::Match(f, m) => (f, m),
                        };
                        let file = &self.results[f];
                        let found = &file.matches[m];
                        clicked = Some(SearchJump { path: file.path.clone(), line: found.line, column: found.column, len: found.len });
                    }
                }
            },
        );
        clicked
    }
}

/// `Actor.cpp   Engine/Source/Runtime/Engine/Private   12`
fn file_row_job(ui: &Ui, file: &FileMatches, color: Color32) -> LayoutJob {
    let font = TextStyle::Body.resolve(ui.style());
    let weak = ui.visuals().weak_text_color();
    let mut job = LayoutJob::default();
    let name = file_name(&file.path);
    job.append(&name, 0.0, TextFormat { font_id: font.clone(), color, ..Default::default() });
    let folder = file.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
    job.append(&folder, 12.0, TextFormat { font_id: font.clone(), color: weak, ..Default::default() });
    job.append(&file.matches.len().to_string(), 12.0, TextFormat { font_id: font, color: weak, ..Default::default() });
    job
}

/// `  123  the line with the match highlighted`
fn match_row_job(ui: &Ui, found: &crate::search::LineMatch, mono: &FontId, color: Color32) -> LayoutJob {
    let weak = ui.visuals().weak_text_color();
    let highlight = ui.visuals().warn_fg_color.gamma_multiply(0.35);
    let mut job = LayoutJob::default();
    let plain = TextFormat { font_id: mono.clone(), color, ..Default::default() };
    job.append(&format!("{:>6}  ", found.line + 1), 0.0, TextFormat { font_id: mono.clone(), color: weak, ..Default::default() });
    let text = &found.excerpt;
    let hit = found.highlight.start.min(text.len())..found.highlight.end.min(text.len());
    let boundary = |i: usize| text.is_char_boundary(i);
    if !(boundary(hit.start) && boundary(hit.end)) {
        job.append(text, 0.0, plain);
        return job;
    }
    job.append(&text[..hit.start], 0.0, plain.clone());
    job.append(&text[hit.clone()], 0.0, TextFormat { background: highlight, ..plain.clone() });
    job.append(&text[hit.end..], 0.0, plain);
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Run frames of a bottom panel holding `panel`, feeding `events` to the
    /// first. Returns the text drawn in the last frame and any clicked result.
    fn frame(
        ctx: &egui::Context,
        panel: &mut SearchPanel,
        start: &mut dyn FnMut(SearchQuery) -> Result<Search, String>,
        events: Vec<egui::Event>,
    ) -> (Vec<(String, egui::Pos2)>, Option<SearchJump>) {
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0));
        let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
        let mut jump = None;
        let output = ctx.run(input, |ctx| {
            egui::TopBottomPanel::bottom("find_in_files").default_height(400.0).show(ctx, |ui| {
                jump = panel.ui(ui, start);
            });
        });
        let mut drawn = Vec::new();
        fn texts(shape: &egui::Shape, out: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| texts(s, out)),
                egui::Shape::Text(t) => out.push((t.galley.text().to_owned(), t.pos + t.galley.size() / 2.0)),
                _ => {}
            }
        }
        for clipped in &output.shapes {
            texts(&clipped.shape, &mut drawn);
        }
        (drawn, jump)
    }

    #[test]
    fn type_search_and_open_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Health.cpp");
        std::fs::write(&file, "int x;\nfloat Health = 1;\n").unwrap();
        let files = Arc::new(vec![file.clone()]);
        let ctx = egui::Context::default();
        let mut start = |query: SearchQuery| Search::start(query, files.clone(), HashMap::new(), ctx.clone());
        let mut panel = SearchPanel::default();
        panel.open(None);

        // Type the query one character per frame, then press Enter.
        frame(&ctx, &mut panel, &mut start, vec![]);
        for c in "health".chars() {
            frame(&ctx, &mut panel, &mut start, vec![egui::Event::Text(c.to_string())]);
        }
        let enter = egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame(&ctx, &mut panel, &mut start, vec![enter]);
        assert_eq!(panel.query.text, "health");
        assert!(ctx.memory(|m| m.has_focus(query_box_id())), "query box kept focus");

        // Let the search finish.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut drawn = Vec::new();
        while Instant::now() < deadline {
            drawn = frame(&ctx, &mut panel, &mut start, vec![]).0;
            if panel.search.as_ref().is_some_and(|s| s.is_finished()) {
                drawn = frame(&ctx, &mut panel, &mut start, vec![]).0;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let texts: Vec<&str> = drawn.iter().map(|(t, _)| t.as_str()).collect();
        assert!(texts.iter().any(|t| t.starts_with("1 match in 1 file")), "status: {texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("Health.cpp")), "file row: {texts:?}");
        let row = drawn.iter().find(|(t, _)| t.contains("float Health = 1;")).expect("match row drawn");
        assert!(row.0.trim_start().starts_with("2 "), "line number shown: {:?}", row.0);

        // Clicking the match row opens it.
        let at = row.1;
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&ctx, &mut panel, &mut start, vec![egui::Event::PointerMoved(at), button(true)]);
        let (_, jump) = frame(&ctx, &mut panel, &mut start, vec![button(false)]);
        let jump = jump.expect("a result was opened");
        assert_eq!((jump.path, jump.line, jump.column, jump.len), (file, 1, 6, 6));
    }
}
