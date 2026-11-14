//! The code editor: a text model with selection and undo ([`Editor`]), and
//! a widget that draws and edits it while only laying out visible lines
//! ([`editor_view`]).

mod buffer;
mod view;

use std::ops::Range;
use std::sync::Arc;

pub use buffer::TextBuffer;
pub use view::editor_view;

use crate::syntax::{Language, SyntaxHighlighter};
use crate::unreal::UnrealProfile;

/// A selection from `anchor` (where it started) to `head` (where the caret is).
/// Both are byte offsets; an empty selection is just a caret.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    pub fn caret(offset: usize) -> Self {
        Self { anchor: offset, head: offset }
    }

    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// How an edit was made, so runs of typing or deleting undo as one step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Typing,
    Deleting,
    Other,
}

/// One replacement, recorded so it can be reversed.
struct Change {
    start: usize,
    removed: String,
    inserted: String,
}

/// One undo step: changes in the order they were applied.
struct UndoEntry {
    changes: Vec<Change>,
    before: Selection,
    after: Selection,
    version_before: u64,
    version_after: u64,
    kind: EditKind,
}

const MAX_UNDO: usize = 1000;

/// Text, highlighting, selection and undo history for one open file.
pub struct Editor {
    buffer: TextBuffer,
    highlighter: SyntaxHighlighter,
    pub selection: Selection,
    /// Horizontal position to aim for when moving up and down, in points.
    pub desired_x: Option<f32>,
    /// What one indent level inserts: a tab or spaces, matching the file.
    pub indent_unit: &'static str,
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    /// Identifies the current text; restored by undo/redo so "dirty" is exact.
    version: u64,
    next_version: u64,
    saved_version: u64,
    /// Set when the caret moved and should be scrolled into view.
    pub scroll_to_cursor: bool,
    /// Scroll so the caret line is centered, rather than just visible.
    pub scroll_center: bool,
    /// Set to give the editor keyboard focus on the next frame.
    pub request_focus: bool,
    /// Widest line laid out so far, in points (lines with tabs can be wider
    /// than their length suggests).
    pub widest_line: f32,
}

impl Editor {
    pub fn new(text: String, language: Option<&'static Language>) -> Self {
        Self::with_unreal(text, language, None)
    }

    /// An editor whose highlighting understands the given Unreal engine's
    /// reflection macros.
    pub fn with_unreal(text: String, language: Option<&'static Language>, unreal: Option<Arc<UnrealProfile>>) -> Self {
        let indent_unit = detect_indent(&text);
        let highlighter = SyntaxHighlighter::new(language, &text, unreal);
        Self {
            buffer: TextBuffer::new(text),
            highlighter,
            selection: Selection::default(),
            desired_x: None,
            indent_unit,
            undo: Vec::new(),
            redo: Vec::new(),
            version: 0,
            next_version: 1,
            saved_version: 0,
            scroll_to_cursor: false,
            scroll_center: false,
            request_focus: true,
            widest_line: 0.0,
        }
    }

    pub fn buffer(&self) -> &TextBuffer {
        &self.buffer
    }

    /// Switch Unreal macro support on or off and re-highlight.
    pub fn set_unreal(&mut self, unreal: Option<Arc<UnrealProfile>>) {
        self.highlighter.set_unreal(unreal, self.buffer.text());
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn highlighter(&self) -> &SyntaxHighlighter {
        &self.highlighter
    }

    /// Identifies the current text: changes with every edit, and undo/redo
    /// restore the version of the text they restore. Lets callers cache
    /// results (like search matches) per text.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
    }

    pub fn selected_text(&self) -> &str {
        &self.text()[self.selection.range()]
    }

    /// Move the caret to `offset`, extending the selection if `extend`.
    pub fn move_head(&mut self, offset: usize, extend: bool) {
        let offset = offset.min(self.buffer.len());
        self.selection = if extend {
            Selection { anchor: self.selection.anchor, head: offset }
        } else {
            Selection::caret(offset)
        };
        self.scroll_to_cursor = true;
    }

    /// Put the caret at `line` (zero-based) and byte `column`, centered on screen.
    pub fn go_to(&mut self, line: usize, column: usize) {
        let line = line.min(self.buffer.line_count() - 1);
        let range = self.buffer.line_range(line);
        let mut offset = (range.start + column).min(range.end);
        while !self.text().is_char_boundary(offset) {
            offset -= 1;
        }
        self.move_head(offset, false);
        self.desired_x = None;
        self.scroll_center = true;
        self.request_focus = true;
    }

    /// Zero-based line and byte column of the caret.
    pub fn caret_position(&self) -> (usize, usize) {
        let point = self.buffer.point(self.selection.head);
        (point.row, point.column)
    }

    pub fn select(&mut self, selection: Selection) {
        let len = self.buffer.len();
        self.selection = Selection { anchor: selection.anchor.min(len), head: selection.head.min(len) };
        self.scroll_to_cursor = true;
    }

    /// Replace the selection with `text` and put the caret after it.
    pub fn insert(&mut self, text: &str, kind: EditKind) {
        let range = self.selection.range();
        let caret = range.start + text.len();
        self.edit(vec![(range, text.to_owned())], Selection::caret(caret), kind);
    }

    /// Delete `range`, leaving the caret at its start.
    pub fn delete(&mut self, range: Range<usize>, kind: EditKind) {
        let caret = range.start;
        self.edit(vec![(range, String::new())], Selection::caret(caret), kind);
    }

    /// Apply non-overlapping `changes` (ranges in the current text, in
    /// ascending order) as one undo step, then set the selection to `after`.
    pub fn edit(&mut self, changes: Vec<(Range<usize>, String)>, after: Selection, kind: EditKind) {
        if changes.iter().all(|(range, text)| range.is_empty() && text.is_empty()) {
            return;
        }
        let before = self.selection;
        let version_before = self.version;

        // Apply from the end so earlier ranges stay valid.
        let applied: Vec<Change> =
            changes.into_iter().rev().map(|(range, text)| self.apply(range, &text)).collect();

        self.version = self.next_version;
        self.next_version += 1;
        self.redo.clear();
        self.select(after);
        self.desired_x = None;

        if let Some(last) = self.undo.last_mut() {
            if can_merge(last, &applied, kind) {
                last.changes.extend(applied);
                last.after = self.selection;
                last.version_after = self.version;
                return;
            }
        }
        self.undo.push(UndoEntry {
            changes: applied,
            before,
            after: self.selection,
            version_before,
            version_after: self.version,
            kind,
        });
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self) {
        let Some(mut entry) = self.undo.pop() else { return };
        for change in entry.changes.iter().rev() {
            self.apply(change.start..change.start + change.inserted.len(), &change.removed);
        }
        self.version = entry.version_before;
        self.select(entry.before);
        self.desired_x = None;
        // Don't let new typing merge into an entry that was undone and redone.
        entry.kind = EditKind::Other;
        self.redo.push(entry);
    }

    pub fn redo(&mut self) {
        let Some(entry) = self.redo.pop() else { return };
        for change in &entry.changes {
            self.apply(change.start..change.start + change.removed.len(), &change.inserted);
        }
        self.version = entry.version_after;
        self.select(entry.after);
        self.desired_x = None;
        self.undo.push(entry);
    }

    /// The single place text changes: updates the buffer and the highlighter.
    fn apply(&mut self, range: Range<usize>, text: &str) -> Change {
        let removed = self.buffer.text()[range.clone()].to_owned();
        let start = range.start;
        let edit = self.buffer.replace(range, text);
        self.highlighter.apply_edit(self.buffer.text(), &edit);
        Change { start, removed, inserted: text.to_owned() }
    }
}

/// Whether `applied` continues the run of typing or deleting in `last`.
fn can_merge(last: &UndoEntry, applied: &[Change], kind: EditKind) -> bool {
    let ([new], Some(prev)) = (applied, last.changes.last()) else { return false };
    if kind != last.kind || kind == EditKind::Other {
        return false;
    }
    match kind {
        EditKind::Typing => {
            let contiguous = new.removed.is_empty() && new.start == prev.start + prev.inserted.len();
            // Start a new step at each word, so undo removes a word at a time.
            let starts_word = new.inserted.starts_with(char::is_whitespace)
                && !prev.inserted.ends_with(char::is_whitespace);
            contiguous && !starts_word && !new.inserted.contains('\n')
        }
        EditKind::Deleting => {
            new.inserted.is_empty()
                && (new.start + new.removed.len() == prev.start || new.start == prev.start)
        }
        EditKind::Other => false,
    }
}

/// Use tabs if more lines are indented with tabs than with spaces.
fn detect_indent(text: &str) -> &'static str {
    let (mut tabs, mut spaces) = (0, 0);
    for line in text.lines().take(2000) {
        if line.starts_with('\t') {
            tabs += 1;
        } else if line.starts_with("  ") {
            spaces += 1;
        }
    }
    if tabs > spaces { "\t" } else { "    " }
}

// ---- Character and word boundaries ------------------------------------------

#[derive(PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Other,
}

fn class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

pub fn prev_char(text: &str, pos: usize) -> usize {
    text[..pos].char_indices().next_back().map_or(0, |(i, _)| i)
}

pub fn next_char(text: &str, pos: usize) -> usize {
    text[pos..].chars().next().map_or(pos, |c| pos + c.len_utf8())
}

/// Start of the word before `pos`, skipping whitespace first.
pub fn prev_word(text: &str, pos: usize) -> usize {
    let mut chars = text[..pos].char_indices().rev().peekable();
    while chars.next_if(|&(_, c)| class(c) == CharClass::Space).is_some() {}
    let Some(&(_, first)) = chars.peek() else { return 0 };
    let kind = class(first);
    let mut start = pos;
    while let Some((i, _)) = chars.next_if(|&(_, c)| class(c) == kind) {
        start = i;
    }
    start.min(pos)
}

/// End of the word at or after `pos`, skipping whitespace after it.
pub fn next_word(text: &str, pos: usize) -> usize {
    let mut chars = text[pos..].char_indices().peekable();
    let Some(&(_, first)) = chars.peek() else { return pos };
    let kind = class(first);
    if kind != CharClass::Space {
        while chars.next_if(|&(_, c)| class(c) == kind).is_some() {}
    }
    while chars.next_if(|&(_, c)| class(c) == CharClass::Space && c != '\n').is_some() {}
    chars.peek().map_or(text.len(), |&(i, _)| pos + i)
}

/// The word (or run of punctuation/whitespace) around `pos`, for double-click.
pub fn word_at(text: &str, pos: usize) -> Range<usize> {
    let probe = if pos < text.len() && !text[pos..].starts_with('\n') { pos } else { prev_char(text, pos) };
    let Some(c) = text[probe..].chars().next() else { return pos..pos };
    if c == '\n' {
        return pos..pos;
    }
    let kind = class(c);
    let start = text[..probe]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == kind && c != '\n')
        .last()
        .map_or(probe, |(i, _)| i);
    let end = text[probe..]
        .char_indices()
        .find(|&(_, c)| class(c) != kind || c == '\n')
        .map_or(text.len(), |(i, _)| probe + i);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        Editor::new(text.to_owned(), None)
    }

    fn type_text(ed: &mut Editor, text: &str) {
        for c in text.chars() {
            ed.insert(&c.to_string(), EditKind::Typing);
        }
    }

    #[test]
    fn typing_undoes_word_by_word() {
        let mut ed = editor("");
        type_text(&mut ed, "hello big world");
        assert_eq!(ed.text(), "hello big world");
        ed.undo();
        assert_eq!(ed.text(), "hello big");
        ed.undo();
        assert_eq!(ed.text(), "hello");
        ed.redo();
        assert_eq!(ed.text(), "hello big");
        assert_eq!(ed.selection, Selection::caret(9));
    }

    #[test]
    fn backspace_run_is_one_step() {
        let mut ed = editor("abcdef");
        ed.move_head(6, false);
        for _ in 0..3 {
            let head = ed.selection.head;
            ed.delete(prev_char(ed.text(), head)..head, EditKind::Deleting);
        }
        assert_eq!(ed.text(), "abc");
        ed.undo();
        assert_eq!(ed.text(), "abcdef");
    }

    #[test]
    fn dirty_follows_undo_to_save_point() {
        let mut ed = editor("x");
        assert!(!ed.is_dirty());
        ed.insert("a", EditKind::Other);
        assert!(ed.is_dirty());
        ed.mark_saved();
        assert!(!ed.is_dirty());
        ed.insert("b", EditKind::Other);
        assert!(ed.is_dirty());
        ed.undo();
        assert!(!ed.is_dirty());
        ed.undo();
        assert!(ed.is_dirty());
    }

    #[test]
    fn multi_change_edit_undoes_together() {
        let mut ed = editor("a\nb\nc");
        let changes = vec![(0..0, "\t".to_owned()), (2..2, "\t".to_owned()), (4..4, "\t".to_owned())];
        ed.edit(changes, Selection::caret(0), EditKind::Other);
        assert_eq!(ed.text(), "\ta\n\tb\n\tc");
        ed.undo();
        assert_eq!(ed.text(), "a\nb\nc");
        ed.redo();
        assert_eq!(ed.text(), "\ta\n\tb\n\tc");
    }

    #[test]
    fn word_motion() {
        let text = "let foo_bar = 42; // x";
        assert_eq!(next_word(text, 0), 4);
        assert_eq!(next_word(text, 4), 12);
        assert_eq!(prev_word(text, 11), 4);
        assert_eq!(prev_word(text, 12), 4);
        assert_eq!(prev_word(text, 4), 0);
        assert_eq!(word_at(text, 6), 4..11);
        assert_eq!(word_at(text, text.len()), 21..22);
        assert_eq!(prev_char("é", 2), 0);
        assert_eq!(next_char("é", 0), 2);
    }

    #[test]
    fn indent_detection() {
        assert_eq!(detect_indent("a\n\tb\n\tc\n    d"), "\t");
        assert_eq!(detect_indent("a\n    b\n"), "    ");
    }
}
