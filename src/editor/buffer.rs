//! The text being edited, with an index of where each line starts.

use std::ops::Range;

use tree_sitter::{InputEdit, Point};

/// Text plus a line index, kept in sync on every edit.
///
/// Offsets are byte offsets into the UTF-8 text; lines are separated by `\n`.
pub struct TextBuffer {
    text: String,
    /// Byte offset where each line starts. Always has at least one entry (0).
    line_starts: Vec<usize>,
}

impl TextBuffer {
    pub fn new(text: String) -> Self {
        let line_starts = compute_line_starts(&text);
        Self { text, line_starts }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Byte range of line `line`, without its `\n`.
    pub fn line_range(&self, line: usize) -> Range<usize> {
        let start = self.line_starts[line];
        let end = match self.line_starts.get(line + 1) {
            Some(next) => next - 1,
            None => self.text.len(),
        };
        start..end
    }

    pub fn line(&self, line: usize) -> &str {
        &self.text[self.line_range(line)]
    }

    /// Length in bytes of the longest line.
    pub fn longest_line_len(&self) -> usize {
        (0..self.line_count()).map(|i| self.line_range(i).len()).max().unwrap_or(0)
    }

    /// The line containing byte `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&start| start <= offset) - 1
    }

    /// Row and byte column of `offset`.
    pub fn point(&self, offset: usize) -> Point {
        let row = self.line_of(offset);
        Point { row, column: offset - self.line_starts[row] }
    }

    /// Replace `range` with `new_text`, returning a description of the edit
    /// (as tree-sitter wants it) for anything that tracks the text.
    pub fn replace(&mut self, range: Range<usize>, new_text: &str) -> InputEdit {
        let start_position = self.point(range.start);
        let old_end_position = self.point(range.end);

        let first_line = start_position.row;
        let last_line = old_end_position.row;
        self.text.replace_range(range.clone(), new_text);

        // Re-index only the touched lines, then shift the ones after them.
        let delta = new_text.len() as isize - range.len() as isize;
        let new_starts = new_text
            .match_indices('\n')
            .map(|(i, _)| range.start + i + 1);
        self.line_starts.splice(first_line + 1..=last_line, new_starts);
        let shift_from = first_line + 1 + new_text.matches('\n').count();
        for start in &mut self.line_starts[shift_from..] {
            *start = (*start as isize + delta) as usize;
        }

        let new_end = range.start + new_text.len();
        InputEdit {
            start_byte: range.start,
            old_end_byte: range.end,
            new_end_byte: new_end,
            start_position,
            old_end_position,
            new_end_position: self.point(new_end),
        }
    }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_index(buf: &TextBuffer) {
        assert_eq!(buf.line_starts, compute_line_starts(buf.text()), "line index out of sync for {:?}", buf.text());
    }

    #[test]
    fn lines_and_points() {
        let buf = TextBuffer::new("ab\ncde\n\nf".to_owned());
        assert_eq!(buf.line_count(), 4);
        assert_eq!(buf.line(1), "cde");
        assert_eq!(buf.line(2), "");
        assert_eq!(buf.line(3), "f");
        assert_eq!(buf.line_of(2), 0); // the '\n' belongs to line 0
        assert_eq!(buf.line_of(3), 1);
        assert_eq!(buf.point(5), Point { row: 1, column: 2 });
        assert_eq!(buf.longest_line_len(), 3);
    }

    #[test]
    fn replace_keeps_index_in_sync() {
        let mut buf = TextBuffer::new("one\ntwo\nthree\n".to_owned());
        let cases: &[(Range<usize>, &str)] = &[
            (4..7, "TWO"),         // same line count
            (3..4, ""),            // join two lines
            (0..0, "zero\n"),      // insert a line at the start
            (5..5, "a\nb\nc\n"),   // insert several lines
            (0..buf_len_hint(), ""), // delete everything (clamped below)
            (0..0, "é\n😀\nx"),     // multi-byte text
        ];
        for (range, text) in cases {
            let range = range.start.min(buf.len())..range.end.min(buf.len());
            let before = buf.text().to_owned();
            let edit = buf.replace(range.clone(), text);
            check_index(&buf);
            let mut expected = before.clone();
            expected.replace_range(range.clone(), text);
            assert_eq!(buf.text(), expected);
            assert_eq!(edit.start_byte, range.start);
            assert_eq!(edit.old_end_byte, range.end);
            assert_eq!(edit.new_end_byte, range.start + text.len());
            assert_eq!(edit.new_end_position, buf.point(edit.new_end_byte));
        }
    }

    fn buf_len_hint() -> usize {
        usize::MAX
    }
}
