//! Incremental syntax highlighting backed by a tree-sitter grammar.
//!
//! The grammar parses the file into a syntax tree, and a highlight query
//! (a `.scm` file) tags tree nodes with capture names such as `@function`.
//!
//! After an edit we:
//! 1. find the edited byte range by comparing the old and new text,
//! 2. re-parse incrementally, reusing the previous tree,
//! 3. ask tree-sitter which ranges changed structure, and
//! 4. re-run the query only on the lines covering the edit and those ranges.
//!
//! Highlights are stored per line, so untouched lines are reused as-is.

use std::cmp::Reverse;
use std::ops::Range;
use std::sync::OnceLock;

#[cfg(test)]
use tree_sitter::Point;
use tree_sitter::{InputEdit, Parser, Query, QueryCursor, StreamingIterator, Tree};

use super::{Span, TokenKind};

/// Capture names the queries may use, and the color each one gets.
/// Dotted captures fall back to their prefix, e.g. `@string.escape` → `string`.
const CAPTURES: &[(&str, TokenKind)] = &[
    ("comment", TokenKind::Comment),
    ("preprocessor", TokenKind::Preprocessor),
    ("string", TokenKind::String),
    ("number", TokenKind::Number),
    ("constant", TokenKind::Constant),
    ("function", TokenKind::Function),
    ("type", TokenKind::Type),
    ("namespace", TokenKind::Namespace),
    ("keyword", TokenKind::Keyword),
    ("property", TokenKind::Property),
    ("operator", TokenKind::Punctuation),
    ("punctuation", TokenKind::Punctuation),
];

fn capture_kind(name: &str) -> Option<TokenKind> {
    CAPTURES
        .iter()
        .find(|(base, _)| name == *base || name.strip_prefix(base).is_some_and(|rest| rest.starts_with('.')))
        .map(|(_, kind)| *kind)
}

struct CompiledQuery {
    query: Query,
    /// Color for each capture index in `query`; `None` for unknown captures.
    kinds: Vec<Option<TokenKind>>,
}

/// A tree-sitter grammar plus its highlight query, compiled on first use.
pub struct TreeSitterGrammar {
    pub name: &'static str,
    pub language: fn() -> tree_sitter::Language,
    pub highlights_query: &'static str,
    compiled: OnceLock<Result<CompiledQuery, String>>,
}

impl TreeSitterGrammar {
    pub const fn new(
        name: &'static str,
        language: fn() -> tree_sitter::Language,
        highlights_query: &'static str,
    ) -> Self {
        Self { name, language, highlights_query, compiled: OnceLock::new() }
    }

    fn compiled(&self) -> Result<&CompiledQuery, &str> {
        self.compiled
            .get_or_init(|| {
                let query = Query::new(&(self.language)(), self.highlights_query)
                    .map_err(|e| format!("{} highlight query: {e}", self.name))?;
                let kinds = query.capture_names().iter().map(|name| capture_kind(name)).collect();
                Ok(CompiledQuery { query, kinds })
            })
            .as_ref()
            .map_err(String::as_str)
    }
}

/// Highlights one document, updating incrementally as its text changes.
pub struct IncrementalHighlighter {
    grammar: &'static TreeSitterGrammar,
    parser: Parser,
    tree: Option<Tree>,
    /// Colored spans for each line, relative to the line start.
    lines: Vec<Vec<Span>>,
}

impl IncrementalHighlighter {
    pub fn new(grammar: &'static TreeSitterGrammar) -> Result<Self, String> {
        grammar.compiled().map_err(str::to_owned)?;
        let mut parser = Parser::new();
        parser.set_language(&(grammar.language)()).map_err(|e| e.to_string())?;
        Ok(Self { grammar, parser, tree: None, lines: Vec::new() })
    }

    /// Highlights for each line of the current text.
    pub fn lines(&self) -> &[Vec<Span>] {
        &self.lines
    }

    /// Highlight `text` from scratch.
    pub fn reset(&mut self, text: &str) {
        let tree = self.parse(text, None);
        self.lines = self.highlight_lines(text, &tree, 0..text.len());
        self.tree = Some(tree);
    }

    /// Bring the highlights up to date with `text`, working out what changed
    /// by comparing it with the previous text. Prefer [`Self::apply_edit`]
    /// when the edit is known.
    #[cfg(test)]
    pub fn update(&mut self, text: &str, previous: &str) {
        if self.tree.is_none() {
            self.reset(text);
        } else if previous != text {
            self.apply_edit(text, &input_edit(previous, text));
        }
    }

    /// Update after `edit` turned the previous text into `text`.
    pub fn apply_edit(&mut self, text: &str, edit: &InputEdit) {
        let Some(mut old_tree) = self.tree.take() else {
            self.reset(text);
            return;
        };
        old_tree.edit(edit);
        let tree = self.parse(text, Some(&old_tree));

        // Bytes whose highlighting may have changed, in new-text coordinates,
        // with the rows they sit on.
        let (mut start, mut first_row) = (edit.start_byte, edit.start_position.row);
        let (mut end, mut last_row) = (edit.new_end_byte, edit.new_end_position.row);
        for range in old_tree.changed_ranges(&tree) {
            if range.start_byte < start {
                (start, first_row) = (range.start_byte, range.start_point.row);
            }
            if range.end_byte > end && range.end_byte <= text.len() {
                (end, last_row) = (range.end_byte, range.end_point.row);
            }
        }

        // Re-highlight those whole lines. Rows after the edit shift by the
        // number of lines it added or removed.
        let old_last_row = last_row + edit.old_end_position.row - edit.new_end_position.row;
        let region = line_start(text, start)..line_end(text, end);
        let new_lines = self.highlight_lines(text, &tree, region);
        self.lines.splice(first_row..=old_last_row, new_lines);
        self.tree = Some(tree);
    }

    fn parse(&mut self, text: &str, old_tree: Option<&Tree>) -> Tree {
        self.parser
            .parse(text, old_tree)
            .expect("parser has a language and no timeout, so parsing always succeeds")
    }

    /// Highlight the whole lines in `region` (which must start and end at line boundaries).
    fn highlight_lines(&self, text: &str, tree: &Tree, region: Range<usize>) -> Vec<Vec<Span>> {
        let compiled = self.grammar.compiled().expect("checked in IncrementalHighlighter::new");
        let base = region.start;

        // Collect every capture touching the region: (start, end, pattern, kind).
        let mut captures = Vec::new();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(region.clone());
        let mut matches = cursor.captures(&compiled.query, tree.root_node(), text.as_bytes());
        while let Some((m, i)) = matches.next() {
            let capture = m.captures()[*i];
            if let Some(kind) = compiled.kinds[capture.index as usize] {
                let node = capture.node;
                captures.push((node.start_byte(), node.end_byte(), m.pattern_index, kind));
            }
        }

        // Paint a color per byte: outer nodes first so inner nodes override them,
        // and for the same node, later patterns override earlier ones. Strings and
        // comments are painted last so nothing inside them (like the `)` in a raw
        // string) changes color.
        captures.sort_by_key(|&(start, end, pattern, _)| (start, Reverse(end), pattern));
        let mut kinds = vec![TokenKind::Plain; region.len()];
        let is_literal = |kind: TokenKind| matches!(kind, TokenKind::String | TokenKind::Comment);
        for pass_literals in [false, true] {
            for &(start, end, _, kind) in &captures {
                if is_literal(kind) == pass_literals {
                    let (start, end) = (start.max(region.start), end.min(region.end));
                    if start < end {
                        kinds[start - base..end - base].fill(kind);
                    }
                }
            }
        }

        // Split into lines of merged, line-relative spans.
        let mut lines = Vec::new();
        let mut line_start = region.start;
        for line in text[region].split('\n') {
            let line_end = line_start + line.len();
            let mut spans = Vec::new();
            let mut i = line_start;
            while i < line_end {
                let kind = kinds[i - base];
                let mut j = i + 1;
                while j < line_end && kinds[j - base] == kind {
                    j += 1;
                }
                spans.push(Span { range: i - line_start..j - line_start, kind });
                i = j;
            }
            lines.push(spans);
            line_start = line_end + 1;
        }
        lines
    }
}

/// Describe the change from `old` to `new` as a single edit, by trimming the
/// common prefix and suffix.
#[cfg(test)]
fn input_edit(old: &str, new: &str) -> InputEdit {
    let (o, n) = (old.as_bytes(), new.as_bytes());
    let mut prefix = o.iter().zip(n).take_while(|(a, b)| a == b).count();
    while !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = o.len().min(n.len()) - prefix;
    let mut suffix = o.iter().rev().zip(n.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while !new.is_char_boundary(n.len() - suffix) {
        suffix -= 1;
    }
    InputEdit {
        start_byte: prefix,
        old_end_byte: o.len() - suffix,
        new_end_byte: n.len() - suffix,
        start_position: point_at(new, prefix),
        old_end_position: point_at(old, o.len() - suffix),
        new_end_position: point_at(new, n.len() - suffix),
    }
}

#[cfg(test)]
fn point_at(text: &str, byte: usize) -> Point {
    let before = &text[..byte];
    let column = byte - line_start(text, byte);
    Point { row: count_newlines(before), column }
}

#[cfg(test)]
fn count_newlines(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

/// Byte offset where the line containing `byte` starts.
fn line_start(text: &str, byte: usize) -> usize {
    text[..byte].rfind('\n').map_or(0, |i| i + 1)
}

/// Byte offset where the line containing `byte` ends (before its `\n`).
fn line_end(text: &str, byte: usize) -> usize {
    text[byte..].find('\n').map_or(text.len(), |i| byte + i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::TextBuffer;
    use crate::syntax::cpp::CPP;
    use TokenKind::*;

    fn grammar() -> &'static TreeSitterGrammar {
        CPP.tree_sitter.as_ref().expect("C++ has a tree-sitter grammar")
    }

    /// Highlight `src` from scratch, as absolute (range, kind) spans.
    fn highlight(src: &str) -> Vec<(Range<usize>, TokenKind)> {
        let mut hl = IncrementalHighlighter::new(grammar()).unwrap();
        hl.reset(src);
        flatten(src, hl.lines())
    }

    fn flatten(src: &str, lines: &[Vec<Span>]) -> Vec<(Range<usize>, TokenKind)> {
        let mut out = Vec::new();
        let mut offset = 0;
        for (line, spans) in src.split('\n').zip(lines) {
            for span in spans {
                out.push((offset + span.range.start..offset + span.range.end, span.kind));
            }
            offset += line.len() + 1;
        }
        out
    }

    /// The color of the `nth` occurrence of `needle` in `src`.
    fn kind_of(src: &str, needle: &str, nth: usize) -> TokenKind {
        let spans = highlight(src);
        let at = src.match_indices(needle).nth(nth).expect("needle in source").0;
        spans.iter().find(|(r, _)| r.contains(&at)).expect("span covers needle").1
    }

    #[test]
    fn query_compiles() {
        grammar().compiled().unwrap();
    }

    #[test]
    fn spans_cover_every_line() {
        let src = "#include <vector>\nint main() {\n  return 0; // done\n}\n";
        let mut hl = IncrementalHighlighter::new(grammar()).unwrap();
        hl.reset(src);
        assert_eq!(hl.lines().len(), src.split('\n').count());
        for (line, spans) in src.split('\n').zip(hl.lines()) {
            let mut pos = 0;
            for span in spans {
                assert_eq!(span.range.start, pos, "gap in {line:?}");
                pos = span.range.end;
            }
            assert_eq!(pos, line.len(), "short spans for {line:?}");
        }
    }

    #[test]
    fn functions_and_types() {
        let src = "int add(int a, int b) { return helper(a) + obj.method(b); }";
        assert_eq!(kind_of(src, "int", 0), Type);
        assert_eq!(kind_of(src, "add", 0), Function);
        assert_eq!(kind_of(src, "return", 0), Keyword);
        assert_eq!(kind_of(src, "helper", 0), Function);
        assert_eq!(kind_of(src, "method", 0), Function);
        assert_eq!(kind_of(src, "a)", 0), Plain);
    }

    #[test]
    fn qualified_names() {
        let src = "void Foo::bar() { std::vector<int> v; std::sort(v.begin(), v.end()); }";
        assert_eq!(kind_of(src, "Foo", 0), Namespace);
        assert_eq!(kind_of(src, "bar", 0), Function);
        assert_eq!(kind_of(src, "std", 0), Namespace);
        assert_eq!(kind_of(src, "vector", 0), Type);
        assert_eq!(kind_of(src, "sort", 0), Function);
        assert_eq!(kind_of(src, "begin", 0), Function);
    }

    #[test]
    fn preprocessor_and_constants() {
        let src = "#include <vector>\n#define MAX_ITEMS 10\nint n = MAX_ITEMS + 25;\n";
        assert_eq!(kind_of(src, "#include", 0), Preprocessor);
        assert_eq!(kind_of(src, "<vector>", 0), String);
        assert_eq!(kind_of(src, "#define", 0), Preprocessor);
        assert_eq!(kind_of(src, "MAX_ITEMS", 0), Constant);
        assert_eq!(kind_of(src, "MAX_ITEMS", 1), Constant);
        assert_eq!(kind_of(src, "25", 0), Number);
    }

    #[test]
    fn literals_and_comments() {
        let src = "auto s = R\"(a)b)\"; bool t = true; /* c */ char c = 'x';";
        assert_eq!(kind_of(src, "auto", 0), Type);
        // The `)` inside the raw string stays string-colored.
        assert_eq!(kind_of(src, ")b", 0), String);
        assert_eq!(kind_of(src, "true", 0), Constant);
        assert_eq!(kind_of(src, "/* c */", 0), Comment);
        assert_eq!(kind_of(src, "'x'", 0), String);
    }

    #[test]
    fn namespace_definition() {
        assert_eq!(kind_of("namespace demo { int x; }", "demo", 0), Namespace);
    }

    #[test]
    fn casts_and_fields() {
        let src = "int x = static_cast<int>(p->value); this->count = 1;";
        assert_eq!(kind_of(src, "static_cast", 0), Keyword);
        assert_eq!(kind_of(src, "value", 0), Property);
        assert_eq!(kind_of(src, "this", 0), Keyword);
        assert_eq!(kind_of(src, "count", 0), Property);
    }

    #[test]
    fn input_edit_finds_changed_range() {
        let e = input_edit("int a;\nint b;\n", "int a;\nint xb;\n");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (11, 11, 12));
        assert_eq!(e.start_position, Point { row: 1, column: 4 });
        // Repeated characters: the edit must still be within both strings.
        let e = input_edit("aaa", "aa");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (2, 3, 2));
        // Never splits a multi-byte character.
        let e = input_edit("é", "è");
        assert_eq!((e.start_byte, e.old_end_byte, e.new_end_byte), (0, 2, 2));
    }

    /// Applying a sequence of edits incrementally must give exactly the same
    /// highlights as highlighting each version from scratch.
    #[test]
    fn incremental_matches_full() {
        let original = "#include <vector>\n\nnamespace demo {\nint add(int a, int b) {\n    return a + b; // sum\n}\n\nconst char* s = R\"(x)\";\n}\n";
        let edits: &[(&str, &str)] = &[
            ("int add", "/* int add"),           // open a comment that swallows the rest
            ("/* int add", "int add"),           // and close it again
            ("a + b", "a + b * MAX_VALUE"),       // type in the middle of a line
            ("// sum\n}\n", "// sum\n}\n\nvoid more() {\n    helper(1);\n}\n"), // paste lines
            ("R\"(x)", "R\"(x\ny)"),               // edit inside a raw string
            ("\nconst char* s = R\"(x\ny)\";", ""), // delete lines
            ("namespace demo {", "namespace demo { int ok"), // break the syntax
            ("helper(1);", "helper(\"é\");"),      // non-ASCII text
            ("", "// top\n"),                       // insert at the very start
        ];

        // Two highlighters: one told the exact edit (as the editor does), one
        // that works the edit out by comparing old and new text.
        let mut buffer = TextBuffer::new(original.to_owned());
        let mut exact = IncrementalHighlighter::new(grammar()).unwrap();
        let mut diffed = IncrementalHighlighter::new(grammar()).unwrap();
        exact.reset(buffer.text());
        diffed.reset(buffer.text());
        for (from, to) in edits {
            let previous = buffer.text().to_owned();
            let at = previous.find(from).unwrap_or_else(|| panic!("edit target {from:?} not in text"));
            let edit = buffer.replace(at..at + from.len(), to);
            let text = buffer.text();
            exact.apply_edit(text, &edit);
            diffed.update(text, &previous);
            let expected = highlight(text);
            assert_eq!(flatten(text, exact.lines()), expected, "exact edit: {from:?} -> {to:?}");
            assert_eq!(flatten(text, diffed.lines()), expected, "diffed edit: {from:?} -> {to:?}");
        }
    }

    /// Run with `cargo test --release -- --ignored --nocapture large_file`.
    #[test]
    #[ignore]
    fn large_file_timing() {
        use std::time::Instant;
        let unit = "namespace n {
class Foo {
public:
    int bar(int x) const { return helper(x) * 2; } // c
};
}
";
        let mut buffer = TextBuffer::new(unit.repeat(2000)); // 12,000 lines
        let mut hl = IncrementalHighlighter::new(grammar()).unwrap();

        let start = Instant::now();
        hl.reset(buffer.text());
        println!("initial highlight of {} lines: {:?}", hl.lines().len(), start.elapsed());

        let mid = buffer.len() / 2;
        let mid = buffer.text()[mid..].find("return").unwrap() + mid;
        let start = Instant::now();
        let edit = buffer.replace(mid..mid, "x");
        hl.apply_edit(buffer.text(), &edit);
        println!("edit + re-highlight after typing one character: {:?}", start.elapsed());
    }
}
