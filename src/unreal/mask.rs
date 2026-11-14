//! Hiding Unreal's macros from the C++ parser.
//!
//! `UPROPERTY(EditAnywhere) float Health;` isn't valid C++ to a parser with
//! no preprocessor: `UPROPERTY(...)` reads as a call missing its semicolon,
//! and the declaration after it gets mangled. Replacing the macro (and
//! `*_API`, `GENERATED_BODY()`) with spaces leaves plain C++ with every byte
//! where it was; the spans record what was hidden so it can be colored.

use std::ops::Range;

use super::{MacroKind, UnrealProfile};

/// What a hidden piece of text was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskKind {
    /// `UPROPERTY`, `GENERATED_BODY`, `ENGINE_API`...
    Macro,
    /// A specifier valid on its macro, e.g. `EditAnywhere`.
    Specifier,
    /// A specifier the engine doesn't define for that macro.
    UnknownSpecifier,
    /// A metadata key, e.g. `ClampMin` in `meta=(ClampMin="0")`.
    MetaKey,
    String,
    Number,
}

pub struct Masked {
    /// The text with macros replaced by spaces; same length, same line breaks.
    pub text: String,
    /// What was hidden, in order.
    pub spans: Vec<(Range<usize>, MaskKind)>,
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// End of the literal (string or char) starting at `start`, whose quote is `b[start]`.
fn literal_end(b: &[u8], start: usize) -> usize {
    let quote = b[start];
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            b'\n' => return i, // unterminated: stop at the line end
            _ => i += 1,
        }
    }
    b.len()
}

/// If a comment, literal or raw string starts at `i`, the index after it.
fn skip_non_code(b: &[u8], i: usize) -> Option<usize> {
    let rest = &b[i..];
    if rest.starts_with(b"//") {
        return Some(b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p));
    }
    if rest.starts_with(b"/*") {
        return Some(b[i + 2..].windows(2).position(|w| w == b"*/").map_or(b.len(), |p| i + 2 + p + 2));
    }
    // Raw string R"delim( ... )delim" (also after a u8/L/u/U prefix).
    if rest.starts_with(b"R\"") && (i == 0 || !is_ident(b[i - 1]) || matches!(b[i - 1], b'8' | b'L' | b'u' | b'U')) {
        let open = b[i + 2..].iter().position(|&c| c == b'(')?;
        let delim = &b[i + 2..i + 2 + open];
        let mut close = Vec::with_capacity(delim.len() + 2);
        close.push(b')');
        close.extend_from_slice(delim);
        close.push(b'"');
        let body = i + 2 + open + 1;
        return Some(b[body..].windows(close.len()).position(|w| w == close).map_or(b.len(), |p| body + p + close.len()));
    }
    match b[i] {
        b'"' => Some(literal_end(b, i)),
        // A quote after a digit or letter is a digit separator (1'000), not a char literal.
        b'\'' if i == 0 || !is_ident(b[i - 1]) => Some(literal_end(b, i)),
        _ => None,
    }
}

/// The `)` matching the `(` at `open`. `None` if the text ends first or hits
/// `;` or `{` (code still being typed), so nothing large gets hidden.
fn matching_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        if let Some(next) = skip_non_code(b, i) {
            i = next.max(i + 1);
            continue;
        }
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b';' | b'{' | b'}' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Replace `range` with spaces, keeping line breaks.
fn blank(out: &mut [u8], range: Range<usize>) {
    for c in &mut out[range] {
        if *c != b'\n' && *c != b'\r' {
            *c = b' ';
        }
    }
}

pub fn mask(text: &str, profile: &UnrealProfile) -> Masked {
    let b = text.as_bytes();
    let mut out = b.to_vec();
    let mut spans = Vec::new();
    let mut at_line_start = true;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            at_line_start = true;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        // Preprocessor lines (with `\` continuations) are left alone.
        if at_line_start && c == b'#' {
            while i < b.len() && !(b[i] == b'\n' && b[i - 1] != b'\\') {
                i += 1;
            }
            continue;
        }
        at_line_start = false;
        if let Some(next) = skip_non_code(b, i) {
            i = next.max(i + 1);
            continue;
        }
        if c.is_ascii_digit() {
            while i < b.len() && (is_ident(b[i]) || b[i] == b'.' || b[i] == b'\'') {
                i += 1;
            }
            continue;
        }
        if !is_ident_start(c) {
            i += 1;
            continue;
        }

        let start = i;
        while i < b.len() && is_ident(b[i]) {
            i += 1;
        }
        let word = &text[start..i];

        // `ENGINE_API` and friends: module export macros. All are hidden from
        // the parser; only those of real modules get the macro color, so a
        // typo stands out.
        if word.len() > 4 && word.ends_with("_API") && word.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_') {
            blank(&mut out, start..i);
            if profile.is_export_macro(word) {
                spans.push((start..i, MaskKind::Macro));
            }
            continue;
        }

        let kind = profile.reflection_macro(word);
        if kind.is_none() && !profile.is_generated_macro(word) {
            continue;
        }
        let mut open = i;
        while open < b.len() && b[open].is_ascii_whitespace() {
            open += 1;
        }
        if open >= b.len() || b[open] != b'(' {
            continue;
        }
        let Some(close) = matching_paren(b, open) else { continue };
        blank(&mut out, start..close + 1);
        spans.push((start..i, MaskKind::Macro));
        if let Some(kind) = kind {
            specifier_spans(text, open + 1..close, kind, profile, &mut spans);
        }
        i = close + 1;
    }
    // Only ASCII bytes were replaced with spaces, so the text is still valid UTF-8.
    let text = String::from_utf8(out).expect("masking keeps UTF-8 valid");
    Masked { text, spans }
}

/// Classify what's inside a macro's parentheses: `EditAnywhere, Category="X",
/// meta=(ClampMin="0")`. Keys at the top level are specifiers (checked
/// against the profile); keys inside nested groups like `meta=(...)` are
/// metadata keys; values are left plain except strings and numbers.
fn specifier_spans(text: &str, range: Range<usize>, kind: MacroKind, profile: &UnrealProfile, spans: &mut Vec<(Range<usize>, MaskKind)>) {
    let b = text.as_bytes();
    // Per nesting level: whether the next word is a key (vs. a value after `=`).
    let mut expect_key = vec![true];
    let mut i = range.start;
    while i < range.end {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            let end = literal_end(b, i).min(range.end);
            spans.push((i..end, MaskKind::String));
            i = end;
            continue;
        }
        if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            i += 1;
            while i < range.end && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            spans.push((start..i, MaskKind::Number));
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < range.end && is_ident(b[i]) {
                i += 1;
            }
            let nested = expect_key.len() > 1;
            let expecting = expect_key.last_mut().expect("never empty");
            if *expecting {
                let span_kind = if nested {
                    MaskKind::MetaKey
                } else if profile.specifier(kind, &text[start..i]).is_some() {
                    MaskKind::Specifier
                } else if kind == MacroKind::Meta {
                    MaskKind::MetaKey // UMETA keys are free-form metadata
                } else {
                    MaskKind::UnknownSpecifier
                };
                spans.push((start..i, span_kind));
                *expecting = false;
            }
            continue;
        }
        match c {
            b'(' => expect_key.push(true),
            b')' if expect_key.len() > 1 => {
                expect_key.pop();
            }
            b',' => *expect_key.last_mut().expect("never empty") = true,
            _ => {}
        }
        i += 1;
    }
}
