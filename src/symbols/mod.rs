//! A project-wide index of where C++ symbols are defined, for go-to-definition.
//!
//! Built with tree-sitter, so it needs no build settings and works on code
//! that doesn't compile. It matches by name, then ranks candidates using the
//! context around the reference (qualifier, member access, call, file).

mod cache;
mod extract;
mod index;
mod indexer;
mod strings;

#[cfg(test)]
use std::path::PathBuf;

pub use cache::cache_path;
pub use extract::new_parser;
pub use index::SymbolIndex;
pub use indexer::{Indexer, Phase, RefreshStats};

/// Stored as one byte in the index and its cache; the order is part of the
/// cache format, so only ever add kinds at the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SymbolKind {
    Function,
    Class,
    Struct,
    Union,
    Enum,
    Enumerator,
    Typedef,
    Namespace,
    Field,
    Variable,
    Macro,
}

impl SymbolKind {
    const ALL: [SymbolKind; 11] = [
        Self::Function,
        Self::Class,
        Self::Struct,
        Self::Union,
        Self::Enum,
        Self::Enumerator,
        Self::Typedef,
        Self::Namespace,
        Self::Field,
        Self::Variable,
        Self::Macro,
    ];

    fn from_u8(value: u8) -> Option<Self> {
        Self::ALL.get(value as usize).copied()
    }

    fn from_capture(name: &str) -> Option<Self> {
        Some(match name {
            "function" => Self::Function,
            "class" => Self::Class,
            "struct" => Self::Struct,
            "union" => Self::Union,
            "enum" => Self::Enum,
            "enumerator" => Self::Enumerator,
            "typedef" => Self::Typedef,
            "namespace" => Self::Namespace,
            "field" => Self::Field,
            "variable" => Self::Variable,
            "macro" => Self::Macro,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Union => "union",
            Self::Enum => "enum",
            Self::Enumerator => "enumerator",
            Self::Typedef => "type alias",
            Self::Namespace => "namespace",
            Self::Field => "field",
            Self::Variable => "variable",
            Self::Macro => "macro",
        }
    }

    fn is_type(self) -> bool {
        matches!(self, Self::Class | Self::Struct | Self::Union | Self::Enum | Self::Typedef)
    }
}

/// Where something is defined or declared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// Enclosing namespaces/classes, e.g. `demo::Stack`; empty at global scope.
    pub scope: String,
    pub kind: SymbolKind,
    /// A definition (has a body) rather than a declaration (prototype).
    pub definition: bool,
    pub path: std::path::PathBuf,
    /// Zero-based line, and byte column within it.
    pub line: usize,
    pub column: usize,
}

impl Symbol {
    pub fn qualified_name(&self) -> String {
        if self.scope.is_empty() { self.name.clone() } else { format!("{}::{}", self.scope, self.name) }
    }
}

/// An identifier in the source, with the context that helps pick the right definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    /// Written qualifier, e.g. `Foo` in `Foo::bar` or `a::b` in `a::b::c`.
    pub qualifier: Option<String>,
    /// Preceded by `.` or `->`, so it names a member.
    pub member: bool,
    /// Followed by `(`, so it's a call (or a declaration of a function).
    pub call: bool,
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The identifier at or just before byte `offset` in `text`.
pub fn reference_at(text: &str, offset: usize) -> Option<Reference> {
    let bytes = text.as_bytes();
    let offset = offset.min(bytes.len());
    // Allow the caret to sit just after the identifier.
    let probe = if offset < bytes.len() && is_ident(bytes[offset]) {
        offset
    } else if offset > 0 && is_ident(bytes[offset - 1]) {
        offset - 1
    } else {
        return None;
    };
    let mut start = probe;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = probe;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    if bytes[start].is_ascii_digit() {
        return None; // a number, not a name
    }
    // `~Foo` names a destructor.
    let name_start = if start > 0 && bytes[start - 1] == b'~' { start - 1 } else { start };
    let name = text[name_start..end].to_owned();

    // Walk back over `a::b::` to collect a qualifier.
    let mut qualifier_start = name_start;
    loop {
        let before = text[..qualifier_start].trim_end();
        let Some(before) = before.strip_suffix("::") else { break };
        let before = before.trim_end();
        let segment_start = before.bytes().rposition(|c| !is_ident(c)).map_or(0, |i| i + 1);
        if segment_start == before.len() {
            break; // `::name` (global scope) or something unusual
        }
        qualifier_start = segment_start;
    }
    let qualifier = (qualifier_start < name_start).then(|| {
        text[qualifier_start..name_start].trim_end().trim_end_matches("::").split_whitespace().collect::<String>()
    });

    let before = text[..qualifier_start].trim_end();
    let member = before.ends_with('.') || before.ends_with("->");
    let call = text[end..].trim_start().starts_with('(');
    Some(Reference { name, qualifier, member, call })
}

/// A possible definition for a reference, with how well it fits.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub symbol: Symbol,
    /// Higher is better; equal scores mean the index can't tell them apart.
    pub score: i32,
}

/// Whether a symbol's scope (`demo::Stack`) satisfies a written qualifier
/// (`Stack` or `demo::Stack`): the qualifier must match its trailing segments.
fn scope_matches(scope: &str, qualifier: &str) -> bool {
    let scope: Vec<&str> = scope.split("::").collect();
    let qualifier: Vec<&str> = qualifier.split("::").filter(|s| !s.is_empty()).collect();
    !qualifier.is_empty() && scope.ends_with(&qualifier)
}

/// Index `files` from scratch (no cache) and count the symbols, for timing tests elsewhere.
#[cfg(test)]
pub fn index_files_for_test(files: &[PathBuf]) -> usize {
    indexer::refresh(files, None, &indexer::Progress::default()).0.symbol_count()
}

#[cfg(test)]
mod tests;
