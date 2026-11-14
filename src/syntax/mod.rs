//! Filetype detection and syntax highlighting.
//!
//! Each language is described by a [`Language`] with a set of [`SyntaxRules`].
//! The rules are plain data, so adding a language is mostly a matter of
//! writing a new module like [`cpp`] and registering it in [`LANGUAGES`].

mod cpp;
mod highlight;
mod theme;
mod ts_highlight;

use std::path::Path;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::FontId;
use tree_sitter::InputEdit;

pub use highlight::{Highlighter, Span};
pub use theme::Theme;
pub use ts_highlight::TreeSitterGrammar;
use ts_highlight::IncrementalHighlighter;

use crate::unreal::{self, MaskKind, UnrealProfile};

/// What a piece of source text is, used to pick its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Plain,
    Keyword,
    Type,
    Function,
    Number,
    String,
    Comment,
    Preprocessor,
    Punctuation,
    Constant,
    Namespace,
    Property,
    /// Unreal reflection and export macros: `UPROPERTY`, `GENERATED_BODY`, `ENGINE_API`.
    ReflectionMacro,
    /// A specifier valid on its reflection macro, e.g. `EditAnywhere`.
    Specifier,
    /// A specifier the engine doesn't define for that macro.
    UnknownSpecifier,
}

/// Lexical rules for one language.
pub struct SyntaxRules {
    /// Starts a comment that runs to the end of the line, e.g. `//`.
    pub line_comment: Option<&'static str>,
    /// Opens and closes a comment that may span lines, e.g. `/*` and `*/`.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Quote characters for string and character literals.
    pub string_delims: &'static [u8],
    /// Escape character inside string literals.
    pub escape: u8,
    /// Identifier prefixes that make the following literal part of the string, e.g. `L"..."`.
    pub string_prefixes: &'static [&'static str],
    /// Prefixes for C++-style raw strings, e.g. `R"delim(...)delim"`, which may span lines.
    pub raw_string_prefixes: &'static [&'static str],
    /// Character that starts a preprocessor directive when it begins a line, e.g. `#`.
    pub preprocessor: Option<u8>,
    /// Allow `'` as a digit separator inside numbers, e.g. `1'000'000`.
    pub digit_separators: bool,
    /// Color identifiers followed by `(` as function names.
    pub highlight_calls: bool,
    pub keywords: &'static [&'static str],
    pub types: &'static [&'static str],
}

/// A language Zbugger knows how to highlight.
pub struct Language {
    pub name: &'static str,
    /// Lowercase file extensions without the dot.
    pub extensions: &'static [&'static str],
    /// Simple token rules, used when there is no tree-sitter grammar
    /// or the grammar fails.
    pub rules: SyntaxRules,
    /// Full parser-based highlighting, preferred when present.
    pub tree_sitter: Option<TreeSitterGrammar>,
}

/// Every registered language. Earlier entries win when extensions overlap.
pub static LANGUAGES: &[&Language] = &[&cpp::CPP];

/// Pick a language for a file based on its extension.
pub fn detect(path: &Path) -> Option<&'static Language> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    LANGUAGES
        .iter()
        .copied()
        .find(|lang| lang.extensions.contains(&ext.as_str()))
}

/// Keeps one document's highlighting in step with its text.
pub struct SyntaxHighlighter {
    backend: Backend,
    /// Spans per line, for the backends that don't keep their own.
    lines: Vec<Vec<Span>>,
    /// Unreal macro support: macros are hidden from the parser and colored
    /// from `overlay` instead.
    unreal: Option<Arc<UnrealProfile>>,
    /// The text the parser last saw, with Unreal macros blanked out.
    masked: String,
    /// Colors for the hidden Unreal macros, per line, drawn over `lines`.
    overlay: Vec<Vec<Span>>,
}

enum Backend {
    Plain,
    /// Token rules: cheap, so simply re-run over the whole text on change.
    Rules(&'static SyntaxRules),
    /// Tree-sitter: re-parses and re-highlights only what an edit touched.
    TreeSitter(IncrementalHighlighter),
}

impl SyntaxHighlighter {
    pub fn new(language: Option<&'static Language>, text: &str, unreal: Option<Arc<UnrealProfile>>) -> Self {
        let backend = match language {
            None => Backend::Plain,
            Some(lang) => match lang.tree_sitter.as_ref().map(IncrementalHighlighter::new) {
                Some(Ok(highlighter)) => Backend::TreeSitter(highlighter),
                Some(Err(err)) => {
                    // A broken query is a bug; fall back to the rules in release builds.
                    debug_assert!(false, "tree-sitter setup failed: {err}");
                    Backend::Rules(&lang.rules)
                }
                None => Backend::Rules(&lang.rules),
            },
        };
        let mut highlighter = Self { backend, lines: Vec::new(), unreal, masked: String::new(), overlay: Vec::new() };
        highlighter.reset(text);
        highlighter
    }

    /// Switch Unreal macro support on or off (when a solution opens or closes)
    /// and re-highlight `text`.
    pub fn set_unreal(&mut self, unreal: Option<Arc<UnrealProfile>>, text: &str) {
        self.unreal = unreal;
        self.reset(text);
    }

    /// Highlight `text` from scratch.
    pub fn reset(&mut self, text: &str) {
        match &mut self.backend {
            Backend::Plain => {}
            Backend::Rules(rules) => {
                let mut highlighter = Highlighter::new(rules);
                self.lines = text.split('\n').map(|line| highlighter.line(line)).collect();
            }
            Backend::TreeSitter(highlighter) => match &self.unreal {
                Some(profile) => {
                    let masked = unreal::mask(text, profile);
                    highlighter.reset(&masked.text);
                    self.overlay = overlay_lines(text, &masked.spans);
                    self.masked = masked.text;
                }
                None => {
                    highlighter.reset(text);
                    self.overlay.clear();
                    self.masked.clear();
                }
            },
        }
    }

    /// Update after `edit` changed the document to `text`.
    pub fn apply_edit(&mut self, text: &str, edit: &InputEdit) {
        match &mut self.backend {
            Backend::TreeSitter(highlighter) => match &self.unreal {
                Some(profile) => {
                    // Masking can change beyond the edit itself (typing the `)`
                    // that closes a UPROPERTY hides the whole macro), so work
                    // out the edit the parser sees by comparing masked texts.
                    let masked = unreal::mask(text, profile);
                    let masked_edit = ts_highlight::input_edit(&self.masked, &masked.text);
                    highlighter.apply_edit(&masked.text, &masked_edit);
                    self.overlay = overlay_lines(text, &masked.spans);
                    self.masked = masked.text;
                }
                None => highlighter.apply_edit(text, edit),
            },
            _ => self.reset(text),
        }
    }

    /// Unreal macro colors for line `line`, drawn over [`Self::line_spans`].
    pub fn line_overlay(&self, line: usize) -> &[Span] {
        self.overlay.get(line).map_or(&[], Vec::as_slice)
    }

    /// Colored spans for line `line`, relative to the line start. Empty means plain text.
    pub fn line_spans(&self, line: usize) -> &[Span] {
        let lines = match &self.backend {
            Backend::TreeSitter(highlighter) => highlighter.lines(),
            _ => &self.lines,
        };
        lines.get(line).map_or(&[], Vec::as_slice)
    }
}

/// Split hidden-macro spans (absolute byte ranges) into per-line spans.
fn overlay_lines(text: &str, spans: &[(std::ops::Range<usize>, MaskKind)]) -> Vec<Vec<Span>> {
    let mut lines = vec![Vec::new()];
    let mut line_start = 0;
    let mut spans = spans.iter().peekable();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            lines.push(Vec::new());
        }
        let line_end = line_start + line.len();
        while let Some((range, kind)) = spans.peek() {
            if range.start > line_end {
                break;
            }
            let (start, end) = (range.start.max(line_start), range.end.min(line_end));
            if start < end {
                let kind = match kind {
                    MaskKind::Macro => TokenKind::ReflectionMacro,
                    MaskKind::Specifier => TokenKind::Specifier,
                    MaskKind::UnknownSpecifier => TokenKind::UnknownSpecifier,
                    MaskKind::MetaKey => TokenKind::Property,
                    MaskKind::String => TokenKind::String,
                    MaskKind::Number => TokenKind::Number,
                };
                lines.last_mut().expect("never empty").push(Span { range: start - line_start..end - line_start, kind });
            }
            if range.end > line_end {
                break; // continues on the next line
            }
            spans.next();
        }
        line_start = line_end + 1;
    }
    lines
}

/// Build the colored layout for one line of text: `spans` from the parser,
/// with `overlay` (hidden Unreal macros) drawn on top.
pub fn line_job(line: &str, spans: &[Span], overlay: &[Span], font: &FontId, theme: &Theme) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let format = |kind| TextFormat::simple(font.clone(), theme.color(kind));
    if overlay.is_empty() {
        if spans.is_empty() {
            job.append(line, 0.0, format(TokenKind::Plain));
        }
        for span in spans {
            // Spans can briefly lag behind the text while it is being edited.
            if let Some(text) = line.get(span.range.clone()) {
                job.append(text, 0.0, format(span.kind));
            }
        }
        return job;
    }
    // Merge: a kind per byte, then runs of the same kind.
    let mut kinds = vec![TokenKind::Plain; line.len()];
    for span in spans.iter().chain(overlay) {
        let range = span.range.start.min(line.len())..span.range.end.min(line.len());
        kinds[range].fill(span.kind);
    }
    let mut start = 0;
    while start < line.len() {
        let kind = kinds[start];
        let mut end = start + 1;
        while end < line.len() && (kinds[end] == kind || !line.is_char_boundary(end)) {
            end += 1;
        }
        job.append(&line[start..end], 0.0, format(kind));
        start = end;
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cpp_extensions() {
        for name in ["main.cpp", "a.CC", "x.hpp", "y.h", "z.inl"] {
            assert_eq!(detect(Path::new(name)).map(|l| l.name), Some("C++"), "{name}");
        }
        assert!(detect(Path::new("notes.txt")).is_none());
        assert!(detect(Path::new("Makefile")).is_none());
    }
}
