//! A small, rule-driven tokenizer that splits each line into colored spans.
//!
//! It works line by line and carries a [`LineState`] between lines so that
//! constructs spanning lines (block comments, raw strings) stay colored.
//! It scans bytes; every span boundary falls on an ASCII byte, so ranges
//! are always valid `str` slice boundaries even for non-ASCII text.

use std::ops::Range;

use super::{SyntaxRules, TokenKind};

/// A colored range of bytes within one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub kind: TokenKind,
}

/// What is still open at the end of a line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum LineState {
    #[default]
    Normal,
    BlockComment,
    /// Inside a raw string; holds the terminator to look for, e.g. `)delim"`.
    RawString(String),
}

/// Highlights consecutive lines of one file, carrying state between them.
pub struct Highlighter<'r> {
    rules: &'r SyntaxRules,
    state: LineState,
}

impl<'r> Highlighter<'r> {
    pub fn new(rules: &'r SyntaxRules) -> Self {
        Self { rules, state: LineState::Normal }
    }

    /// Highlight the next line (without its newline).
    pub fn line(&mut self, line: &str) -> Vec<Span> {
        highlight_line(self.rules, line, &mut self.state)
    }
}

/// Collects spans, merging neighbours of the same kind.
#[derive(Default)]
struct Spans(Vec<Span>);

impl Spans {
    fn push(&mut self, range: Range<usize>, kind: TokenKind) {
        if range.is_empty() {
            return;
        }
        if let Some(last) = self.0.last_mut() {
            if last.kind == kind && last.range.end == range.start {
                last.range.end = range.end;
                return;
            }
        }
        self.0.push(Span { range, kind });
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Find the end (exclusive) of a quoted literal starting at `start`.
/// Unterminated literals run to the end of the line.
fn scan_quoted(b: &[u8], start: usize, escape: u8) -> usize {
    let quote = b[start];
    let mut j = start + 1;
    while j < b.len() {
        if b[j] == escape {
            j += 2;
        } else if b[j] == quote {
            return j + 1;
        } else {
            j += 1;
        }
    }
    b.len()
}

fn highlight_line(rules: &SyntaxRules, line: &str, state: &mut LineState) -> Vec<Span> {
    use TokenKind::*;

    let b = line.as_bytes();
    let len = b.len();
    let mut out = Spans::default();
    let mut i = 0;

    // Finish whatever the previous line left open.
    match std::mem::take(state) {
        LineState::Normal => {}
        LineState::BlockComment => {
            let close = rules.block_comment.map(|(_, c)| c).unwrap_or("*/");
            match line.find(close) {
                Some(p) => {
                    i = p + close.len();
                    out.push(0..i, Comment);
                }
                None => {
                    out.push(0..len, Comment);
                    *state = LineState::BlockComment;
                    return out.0;
                }
            }
        }
        LineState::RawString(term) => match line.find(&term) {
            Some(p) => {
                i = p + term.len();
                out.push(0..i, String);
            }
            None => {
                out.push(0..len, String);
                *state = LineState::RawString(term);
                return out.0;
            }
        },
    }

    let first_non_ws = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(len);

    while i < len {
        let rest = &line[i..];
        let c = b[i];
        let start = i;

        if c.is_ascii_whitespace() {
            while i < len && b[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push(start..i, Plain);
            continue;
        }

        if c >= 0x80 {
            while i < len && b[i] >= 0x80 {
                i += 1;
            }
            out.push(start..i, Plain);
            continue;
        }

        if let Some(lc) = rules.line_comment {
            if rest.starts_with(lc) {
                out.push(i..len, Comment);
                break;
            }
        }

        if let Some((open, close)) = rules.block_comment {
            if rest.starts_with(open) {
                let body = i + open.len();
                match line[body..].find(close) {
                    Some(p) => {
                        i = body + p + close.len();
                        out.push(start..i, Comment);
                        continue;
                    }
                    None => {
                        out.push(start..len, Comment);
                        *state = LineState::BlockComment;
                        break;
                    }
                }
            }
        }

        // Preprocessor directive: `#` + optional spaces + name, at the start of a line.
        if Some(c) == rules.preprocessor && i == first_non_ws {
            i += 1;
            while i < len && (b[i] == b' ' || b[i] == b'\t') {
                i += 1;
            }
            let name_start = i;
            while i < len && is_ident_continue(b[i]) {
                i += 1;
            }
            out.push(start..i, Preprocessor);

            let directive = &line[name_start..i];
            if matches!(directive, "include" | "include_next" | "import") {
                let ws_start = i;
                while i < len && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                out.push(ws_start..i, Plain);
                if i < len && b[i] == b'<' {
                    if let Some(p) = line[i..].find('>') {
                        out.push(i..i + p + 1, String);
                        i += p + 1;
                    }
                }
            }
            continue;
        }

        if rules.string_delims.contains(&c) {
            i = scan_quoted(b, i, rules.escape);
            out.push(start..i, String);
            continue;
        }

        let starts_number = c.is_ascii_digit() || (c == b'.' && i + 1 < len && b[i + 1].is_ascii_digit());
        if starts_number {
            let hex = rest.starts_with("0x") || rest.starts_with("0X");
            i += 1;
            while i < len {
                let d = b[i];
                let continues = d.is_ascii_alphanumeric()
                    || d == b'_'
                    || d == b'.'
                    || (d == b'\'' && rules.digit_separators && i + 1 < len && b[i + 1].is_ascii_alphanumeric())
                    || ((d == b'+' || d == b'-') && !hex && matches!(b[i - 1], b'e' | b'E' | b'p' | b'P'));
                if !continues {
                    break;
                }
                i += 1;
            }
            out.push(start..i, Number);
            continue;
        }

        if is_ident_start(c) {
            while i < len && is_ident_continue(b[i]) {
                i += 1;
            }
            let word = &line[start..i];
            let next = b.get(i).copied();

            // Raw string: R"delim( ... )delim"
            if next == Some(b'"') && rules.raw_string_prefixes.contains(&word) {
                let delim_start = i + 1;
                if let Some(p) = line[delim_start..].find('(') {
                    let delim = &line[delim_start..delim_start + p];
                    let term = format!("){delim}\"");
                    let body = delim_start + p + 1;
                    match line[body..].find(&term) {
                        Some(q) => {
                            i = body + q + term.len();
                            out.push(start..i, String);
                            continue;
                        }
                        None => {
                            out.push(start..len, String);
                            *state = LineState::RawString(term);
                            break;
                        }
                    }
                }
            }

            // Prefixed literal: L"...", u8"...", U'x'
            if let Some(q) = next {
                if rules.string_delims.contains(&q) && rules.string_prefixes.contains(&word) {
                    i = scan_quoted(b, i, rules.escape);
                    out.push(start..i, String);
                    continue;
                }
            }

            let kind = if rules.keywords.contains(&word) {
                Keyword
            } else if rules.types.contains(&word) {
                Type
            } else if rules.highlight_calls && next_non_ws(b, i) == Some(b'(') {
                Function
            } else {
                Plain
            };
            out.push(start..i, kind);
            continue;
        }

        i += 1;
        out.push(start..i, Punctuation);
    }

    out.0
}

fn next_non_ws(b: &[u8], from: usize) -> Option<u8> {
    b[from..].iter().copied().find(|c| !c.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::cpp::CPP;
    use TokenKind::*;

    /// Highlight `src` as C++ and return (text, kind) pairs, skipping whitespace.
    fn tokens(src: &str) -> Vec<Vec<(std::string::String, TokenKind)>> {
        let lines: Vec<_> = src.lines().collect();
        let mut hl = Highlighter::new(&CPP.rules);
        lines
            .iter()
            .map(|line| hl.line(line))
            .zip(&lines)
            .map(|(spans, line)| {
                // Spans must tile the whole line with no gaps, or text would vanish.
                let mut pos = 0;
                for s in &spans {
                    assert_eq!(s.range.start, pos, "gap in spans for {line:?}: {spans:?}");
                    pos = s.range.end;
                }
                assert_eq!(pos, line.len(), "spans don't reach end of {line:?}");
                spans
                    .into_iter()
                    .map(|s| (line[s.range].trim().to_owned(), s.kind))
                    .filter(|(text, _)| !text.is_empty())
                    .collect()
            })
            .collect()
    }

    fn t(s: &str, k: TokenKind) -> (std::string::String, TokenKind) {
        (s.to_owned(), k)
    }

    #[test]
    fn keywords_types_and_calls() {
        assert_eq!(
            tokens("return foo(x);")[0],
            vec![t("return", Keyword), t("foo", Function), t("(", Punctuation), t("x", Plain), t(");", Punctuation)]
        );
        assert_eq!(tokens("unsigned int n")[0], vec![t("unsigned", Type), t("int", Type), t("n", Plain)]);
    }

    #[test]
    fn comments() {
        assert_eq!(tokens("x // hi")[0], vec![t("x", Plain), t("// hi", Comment)]);
        let lines = tokens("a /* one\ntwo\nthree */ b");
        assert_eq!(lines[0], vec![t("a", Plain), t("/* one", Comment)]);
        assert_eq!(lines[1], vec![t("two", Comment)]);
        assert_eq!(lines[2], vec![t("three */", Comment), t("b", Plain)]);
    }

    #[test]
    fn strings_and_chars() {
        assert_eq!(tokens(r#"s = "a\"b";"#)[0][2], t(r#""a\"b""#, String));
        assert_eq!(tokens("c = '\\'';")[0][2], t("'\\''", String));
        assert_eq!(tokens(r#"L"wide""#)[0], vec![t(r#"L"wide""#, String)]);
    }

    #[test]
    fn raw_string_spans_lines() {
        let lines = tokens("auto s = R\"x(line1\nline2)x\"; int y;");
        assert_eq!(lines[0].last().unwrap(), &t("R\"x(line1", String));
        assert_eq!(lines[1][0], t("line2)x\"", String));
        assert_eq!(lines[1][2], t("int", Type));
    }

    #[test]
    fn numbers() {
        assert_eq!(tokens("1'000'000")[0], vec![t("1'000'000", Number)]);
        assert_eq!(tokens("1.5e-3f")[0], vec![t("1.5e-3f", Number)]);
        assert_eq!(tokens("0xFFu")[0], vec![t("0xFFu", Number)]);
    }

    #[test]
    fn preprocessor() {
        assert_eq!(
            tokens("#include <vector>")[0],
            vec![t("#include", Preprocessor), t("<vector>", String)]
        );
        assert_eq!(
            tokens("  #  define X 1")[0],
            vec![t("#  define", Preprocessor), t("X", Plain), t("1", Number)]
        );
        // A `#` that does not start the line is not a directive.
        assert_eq!(tokens("a # b")[0][1], t("#", Punctuation));
    }

    #[test]
    fn non_ascii_is_safe() {
        assert_eq!(tokens("// héllo")[0], vec![t("// héllo", Comment)]);
        assert_eq!(tokens("x = \"\\é\";")[0][2], t("\"\\é\"", String));
        assert_eq!(tokens("naïve")[0], vec![t("naïve", Plain)]);
    }
}
