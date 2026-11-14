//! The find bar (Ctrl+F): searches the current document as you type,
//! highlights every match and steps between them.

use std::ops::Range;
use std::path::{Path, PathBuf};

use egui::text::{CCursor, CCursorRange};
use egui::{pos2, Frame, Id, Key, Modifiers, Order, Rect, RichText, TextEdit};

use crate::editor::{Editor, Selection};

/// Stop counting after this many matches, so a one-letter search in a huge
/// file stays quick.
const MAX_MATCHES: usize = 100_000;

/// Non-overlapping occurrences of `query` in `text`, in order.
///
/// Case-insensitive matching folds ASCII letters only, which keeps byte
/// offsets identical between the folded and original text.
pub fn find_all(text: &str, query: &str, case_sensitive: bool, whole_word: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let (folded_text, folded_query);
    let (haystack, needle): (&str, &str) = if case_sensitive {
        (text, query)
    } else {
        folded_text = text.to_ascii_lowercase();
        folded_query = query.to_ascii_lowercase();
        (&folded_text, &folded_query)
    };
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut matches = Vec::new();
    let mut from = 0;
    while let Some(found) = haystack[from..].find(needle) {
        let start = from + found;
        let end = start + needle.len();
        let bounded = !whole_word
            || (!text[..start].chars().next_back().is_some_and(is_word) && !text[end..].chars().next().is_some_and(is_word));
        if bounded {
            matches.push(start..end);
            if matches.len() >= MAX_MATCHES {
                break;
            }
            from = end;
        } else {
            // Try again one character further on.
            from = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    matches
}

/// What the editor needs to draw search highlights.
pub struct FindHighlights<'a> {
    /// Sorted, non-overlapping match ranges.
    pub matches: &'a [Range<usize>],
    pub current: Option<usize>,
}

/// What the matches were computed for; if any of it changes, search again.
#[derive(Clone, PartialEq, Eq)]
struct SearchKey {
    path: PathBuf,
    version: u64,
    query: String,
    case_sensitive: bool,
    whole_word: bool,
}

#[derive(Default)]
pub struct FindBar {
    pub open: bool,
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    matches: Vec<Range<usize>>,
    /// Index into `matches` of the match being shown.
    current: Option<usize>,
    key: Option<SearchKey>,
    /// Focus the query box (and select its text) on the next frame.
    focus_query: bool,
}

impl FindBar {
    /// Open the bar (Ctrl+F), taking a single-line selection as the query.
    pub fn open(&mut self, editor: &Editor) {
        let selected = editor.selected_text();
        if !selected.is_empty() && !selected.contains('\n') {
            self.query = selected.to_owned();
        }
        self.open = true;
        self.focus_query = true;
        self.key = None; // search again from the caret
    }

    pub fn close(&mut self, editor: &mut Editor) {
        self.open = false;
        editor.request_focus = true;
    }

    pub fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    /// Highlights for the editor, while the bar is open.
    pub fn highlights(&self) -> Option<FindHighlights<'_>> {
        (self.open && !self.matches.is_empty()).then(|| FindHighlights { matches: &self.matches, current: self.current })
    }

    /// Search again if the query, options or text changed. When the query or
    /// options changed, jump to the first match at or after the selection.
    fn refresh(&mut self, path: &Path, editor: &mut Editor) {
        let key = SearchKey {
            path: path.to_path_buf(),
            version: editor.version(),
            query: self.query.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let search_changed = self.key.as_ref().is_none_or(|old| {
            old.path != key.path || old.query != key.query || old.case_sensitive != key.case_sensitive || old.whole_word != key.whole_word
        });
        self.matches = find_all(editor.text(), &self.query, self.case_sensitive, self.whole_word);
        self.key = Some(key);

        if search_changed {
            // Find as you type: the first match at or after where the selection starts.
            let from = editor.selection.range().start;
            self.current = (!self.matches.is_empty())
                .then(|| self.matches.iter().position(|m| m.start >= from).unwrap_or(0));
            self.show_current(editor);
        } else {
            // The text was edited: keep the current match only if it's still selected.
            let selected = editor.selection.range();
            self.current = self.matches.iter().position(|m| *m == selected);
        }
    }

    /// Go to the next (or previous) match after (before) the selection, wrapping around.
    pub fn step(&mut self, path: &Path, editor: &mut Editor, forward: bool) {
        self.refresh(path, editor);
        if self.matches.is_empty() {
            return;
        }
        let selection = editor.selection.range();
        let index = if forward {
            self.matches.iter().position(|m| m.start >= selection.end && m.start != selection.start).unwrap_or(0)
        } else {
            self.matches.iter().rposition(|m| m.end <= selection.start).unwrap_or(self.matches.len() - 1)
        };
        self.current = Some(index);
        self.show_current(editor);
    }

    /// Select the current match and scroll it to the middle of the view.
    fn show_current(&self, editor: &mut Editor) {
        if let Some(range) = self.current.map(|i| self.matches[i].clone()) {
            editor.select(Selection { anchor: range.start, head: range.end });
            editor.scroll_center = true;
        }
    }

    /// Draw the bar at the top right of `editor_rect` and handle its keys.
    pub fn ui(&mut self, ctx: &egui::Context, editor_rect: Rect, path: &Path, editor: &mut Editor) {
        if !self.open {
            return;
        }
        let query_id = Id::new("find_query");
        let width = 420.0;
        let mut step = None;
        let mut close = false;

        egui::Area::new(Id::new("find_bar"))
            .order(Order::Foreground)
            .fixed_pos(pos2((editor_rect.right() - width - 24.0).max(editor_rect.left()), editor_rect.top() + 6.0))
            .show(ctx, |ui| {
                Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            TextEdit::singleline(&mut self.query)
                                .id(query_id)
                                .hint_text("Find")
                                .desired_width(200.0)
                                // Keep focus on Enter; it steps to the next match instead.
                                .return_key(None),
                        );
                        if std::mem::take(&mut self.focus_query) {
                            response.request_focus();
                            // Select the query so typing replaces it.
                            if let Some(mut state) = TextEdit::load_state(ctx, query_id) {
                                let all = CCursorRange::two(CCursor::new(0), CCursor::new(self.query.chars().count()));
                                state.cursor.set_char_range(Some(all));
                                state.store(ctx, query_id);
                            }
                        }
                        if response.has_focus() {
                            if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter)) {
                                step = Some(false);
                            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                                step = Some(true);
                            }
                        }
                        if (response.has_focus() || response.lost_focus()) && ui.input(|i| i.key_pressed(Key::Escape)) {
                            close = true;
                        }

                        let count = match (self.matches.len(), self.current) {
                            (_, _) if self.query.is_empty() => String::new(),
                            (0, _) => "No results".to_owned(),
                            (n, Some(i)) => format!("{} of {n}{}", i + 1, if n >= MAX_MATCHES { "+" } else { "" }),
                            (n, None) => format!("{n} found"),
                        };
                        let count_color =
                            if self.matches.is_empty() && !self.query.is_empty() { ui.visuals().warn_fg_color } else { ui.visuals().weak_text_color() };
                        ui.add_sized([72.0, 18.0], egui::Label::new(RichText::new(count).color(count_color)));

                        ui.toggle_value(&mut self.case_sensitive, "Aa").on_hover_text("Match case");
                        ui.toggle_value(&mut self.whole_word, "W").on_hover_text("Match whole word");
                        if ui.button("⬆").on_hover_text("Previous match (Shift+Enter, Shift+F3)").clicked() {
                            step = Some(false);
                        }
                        if ui.button("⬇").on_hover_text("Next match (Enter, F3)").clicked() {
                            step = Some(true);
                        }
                        if ui.button("×").on_hover_text("Close (Escape)").clicked() {
                            close = true;
                        }
                    });
                });
            });

        if close {
            self.close(editor);
            return;
        }
        self.refresh(path, editor);
        if let Some(forward) = step {
            self.step(path, editor, forward);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_all_occurrences() {
        assert_eq!(find_all("abcabcab", "ab", true, false), [0..2, 3..5, 6..8]);
        assert_eq!(find_all("aaaa", "aa", true, false), [0..2, 2..4], "non-overlapping");
        assert!(find_all("abc", "", true, false).is_empty());
        assert!(find_all("abc", "x", true, false).is_empty());
    }

    #[test]
    fn case_folding() {
        assert_eq!(find_all("Foo foo FOO", "foo", false, false), [0..3, 4..7, 8..11]);
        assert_eq!(find_all("Foo foo FOO", "foo", true, false), [4..7]);
        // Non-ASCII text keeps its byte offsets.
        assert_eq!(find_all("é Foo", "foo", false, false), [3..6]);
    }

    fn bar_with_query(query: &str) -> FindBar {
        FindBar { open: true, query: query.to_owned(), ..FindBar::default() }
    }

    #[test]
    fn opens_with_the_selection_as_query() {
        let mut editor = Editor::new("let value = other;".to_owned(), None);
        editor.select(Selection { anchor: 4, head: 9 });
        let mut bar = FindBar::default();
        bar.open(&editor);
        assert!(bar.open);
        assert_eq!(bar.query, "value");
        // A multi-line selection isn't used as the query.
        let mut editor = Editor::new("a\nb".to_owned(), None);
        editor.select(Selection { anchor: 0, head: 3 });
        bar.open(&editor);
        assert_eq!(bar.query, "value");
    }

    #[test]
    fn jumps_to_first_match_after_caret_and_wraps() {
        let path = Path::new("a.cpp");
        let mut editor = Editor::new("x foo foo x foo".to_owned(), None);
        editor.move_head(7, false); // between the 2nd and 3rd "foo"
        let mut bar = bar_with_query("foo");
        bar.refresh(path, &mut editor);
        assert_eq!(bar.matches, [2..5, 6..9, 12..15]);
        assert_eq!(bar.current, Some(2), "first match at or after the caret");
        assert_eq!(editor.selection.range(), 12..15, "and it's selected");

        bar.step(path, &mut editor, true);
        assert_eq!((bar.current, editor.selection.range()), (Some(0), 2..5), "wraps to the start");
        bar.step(path, &mut editor, true);
        assert_eq!(bar.current, Some(1));
        bar.step(path, &mut editor, false);
        bar.step(path, &mut editor, false);
        assert_eq!((bar.current, editor.selection.range()), (Some(2), 12..15), "wraps to the end");
    }

    #[test]
    fn follows_edits_to_the_text() {
        let path = Path::new("a.cpp");
        let mut editor = Editor::new("foo bar foo".to_owned(), None);
        let mut bar = bar_with_query("foo");
        bar.refresh(path, &mut editor);
        assert_eq!(bar.matches.len(), 2);

        // Typing elsewhere changes the matches without moving the selection.
        editor.move_head(11, false);
        editor.insert(" foo", crate::editor::EditKind::Typing);
        bar.refresh(path, &mut editor);
        assert_eq!(bar.matches, [0..3, 8..11, 12..15]);
        assert_eq!(bar.current, None, "no match is selected any more");
        assert_eq!(editor.selection.range(), 15..15);
    }

    #[test]
    fn whole_words() {
        let text = "count counter recount count_x count";
        assert_eq!(find_all(text, "count", true, true), [0..5, 30..35]);
        // A rejected partial match doesn't hide a whole word right after it.
        assert_eq!(find_all("countcount count", "count", true, true), [11..16]);
    }
}
