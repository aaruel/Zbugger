//! Pulling symbol definitions and declarations out of C++ source with
//! tree-sitter.

use std::path::PathBuf;
use std::sync::OnceLock;

use rayon::prelude::*;
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use super::index::{ExtractedFile, FileStamp, SymbolRecord};
use super::strings::StringTable;
use super::SymbolKind;
use crate::unreal::UnrealProfile;

struct Compiled {
    query: Query,
    name_capture: u32,
    /// For each capture index: (is a definition, kind), or `None` for `@name`.
    kinds: Vec<Option<(bool, SymbolKind)>>,
}

fn compiled() -> &'static Compiled {
    static COMPILED: OnceLock<Compiled> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let query = Query::new(&tree_sitter_cpp::LANGUAGE.into(), QUERY_SOURCE)
            .expect("symbol query is valid (checked by tests)");
        let names = query.capture_names();
        let name_capture = names.iter().position(|n| *n == "name").expect("query captures @name") as u32;
        let kinds = names
            .iter()
            .map(|name| {
                let (role, kind) = name.split_once('.')?;
                Some((role == "definition", SymbolKind::from_capture(kind)?))
            })
            .collect();
        Compiled { query, name_capture, kinds }
    })
}

/// The symbol query. Part of the cache version: editing it invalidates caches.
pub const QUERY_SOURCE: &str = include_str!("cpp_symbols.scm");

pub fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_cpp::LANGUAGE.into()).expect("tree-sitter-cpp is compatible");
    parser
}

/// Every symbol defined or declared in `text`, with names and scopes
/// interned into `strings`. The records' `file` field is left at 0 for the
/// index to fill in. With an Unreal profile, the engine's macros are hidden
/// from the parser first; without one, only export macros in class heads are.
pub fn extract(parser: &mut Parser, text: &str, strings: &mut StringTable, unreal: Option<&UnrealProfile>) -> Vec<SymbolRecord> {
    let masked = match unreal {
        Some(profile) => crate::unreal::mask(text, profile).text,
        None => mask_export_macros(text),
    };
    let text = masked.as_str();
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    let compiled = compiled();
    let mut symbols = Vec::new();
    let mut scope = String::new(); // reused for every symbol
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&compiled.query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut construct = None;
        for capture in m.captures() {
            if capture.index == compiled.name_capture {
                name_node = Some(capture.node);
            } else if let Some(kind) = compiled.kinds[capture.index as usize] {
                construct = Some((capture.node, kind));
            }
        }
        let (Some(name_node), Some((node, (definition, kind)))) = (name_node, construct) else { continue };

        let (qualifier, name) = split_qualified(&text[name_node.byte_range()]);
        if name.is_empty() {
            continue;
        }
        scope.clear();
        write_enclosing_scope(node, text, &mut scope);
        if !qualifier.is_empty() {
            if !scope.is_empty() {
                scope.push_str("::");
            }
            scope.push_str(&qualifier);
        }
        let position = name_node.start_position();
        symbols.push(SymbolRecord {
            name: strings.intern(&name),
            scope: strings.intern(&scope),
            file: 0,
            line: position.row as u32,
            column: position.column as u32,
            kind,
            definition,
        });
    }
    symbols
}

/// Blank out export macros in class heads, e.g. `ENGINE_API` in
/// `class ENGINE_API AActor : public UObject`, and `__declspec(...)`.
///
/// Without the preprocessor, tree-sitter can't know `ENGINE_API` is a macro
/// and misreads the class as a function named `AActor`. Replacing the macro
/// with spaces keeps every line and column unchanged.
fn mask_export_macros(text: &str) -> String {
    let mut out = text.as_bytes().to_vec();
    let bytes = text.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let skip_space = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };

    let mut i = 0;
    while i < bytes.len() {
        // Find `class`, `struct` or `union` as whole words.
        let keyword = [&b"class"[..], b"struct", b"union"]
            .into_iter()
            .find(|k| bytes[i..].starts_with(k) && (i == 0 || !is_ident(bytes[i - 1])));
        let Some(keyword) = keyword else {
            i += 1;
            continue;
        };
        let mut pos = i + keyword.len();
        if pos < bytes.len() && is_ident(bytes[pos]) {
            i = pos; // part of a longer word, like `classify`
            continue;
        }
        // Blank each macro-looking word that is followed by another name.
        loop {
            let start = skip_space(pos);
            let mut end = start;
            while end < bytes.len() && is_ident(bytes[end]) {
                end += 1;
            }
            let word = &bytes[start..end];
            if word == b"__declspec" && bytes.get(skip_space(end)) == Some(&b'(') {
                let close = bytes[end..].iter().position(|&c| c == b')').map_or(bytes.len(), |p| end + p + 1);
                out[start..close].fill(b' ');
                pos = close;
                continue;
            }
            let looks_like_macro = word.len() >= 2
                && word.iter().any(u8::is_ascii_uppercase)
                && word.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_');
            let next = skip_space(end);
            let followed_by_name = next < bytes.len() && (bytes[next].is_ascii_alphabetic() || bytes[next] == b'_');
            if looks_like_macro && followed_by_name {
                out[start..end].fill(b' ');
                pos = end;
            } else {
                break;
            }
        }
        i = pos.max(i + 1);
    }
    // Only ASCII bytes were replaced with spaces, so this is still valid UTF-8.
    String::from_utf8(out).expect("masking keeps UTF-8 valid")
}

/// Files are processed in chunks, each with its own parser and string table,
/// so workers never contend; the chunks are merged into the index afterwards.
const CHUNK: usize = 64;

/// Symbols from a chunk of files, with ids into the chunk's own string table.
pub struct ExtractedChunk {
    pub strings: StringTable,
    pub files: Vec<ExtractedFile>,
}

/// Read and index `paths` on all cores. Unreadable files get no symbols.
/// `progress` is called (from worker threads) after each file.
pub fn extract_files(paths: &[PathBuf], progress: &(dyn Fn() + Sync), unreal: Option<&UnrealProfile>) -> Vec<ExtractedChunk> {
    paths
        .par_chunks(CHUNK)
        .map(|chunk| {
            let mut parser = new_parser();
            let mut strings = StringTable::default();
            let files = chunk
                .iter()
                .map(|path| {
                    zone!("index file");
                    // Stamp before reading, so a change made meanwhile is picked up next time.
                    let stamp = FileStamp::of(path);
                    let records = match std::fs::read(path) {
                        Ok(bytes) => {
                            // Same normalization as the editor, so line/column positions agree.
                            let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
                            extract(&mut parser, &text, &mut strings, unreal)
                        }
                        Err(_) => Vec::new(),
                    };
                    progress();
                    ExtractedFile { path: path.clone(), stamp, records }
                })
                .collect();
            ExtractedChunk { strings, files }
        })
        .collect()
}

/// Write the namespaces and classes enclosing `node` (outermost first, joined
/// with `::`) into `out`.
fn write_enclosing_scope(node: Node, text: &str, out: &mut String) {
    let mut names = Vec::new();
    let mut current = node.parent();
    while let Some(ancestor) = current {
        let named = matches!(
            ancestor.kind(),
            "namespace_definition" | "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
        );
        if named {
            if let Some(name) = ancestor.child_by_field_name("name") {
                names.push(name);
            }
        }
        current = ancestor.parent();
    }
    for (i, name) in names.iter().rev().enumerate() {
        if i > 0 {
            out.push_str("::");
        }
        out.push_str(&strip_template_args(&text[name.byte_range()]));
    }
}

/// `ns::Foo<T>::bar` → (`ns::Foo`, `bar`). Template arguments are dropped
/// except in operator names like `operator<`.
fn split_qualified(raw: &str) -> (String, String) {
    let raw = raw.trim();
    // Find the last `::` that isn't inside template arguments.
    let mut depth = 0i32;
    let mut split = None;
    let bytes = raw.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' => depth -= 1,
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => split = Some(i),
            _ => {}
        }
        if bytes[i..].starts_with(b"operator") {
            break; // `<`, `>` and `:` after this belong to the operator name
        }
    }
    let (qualifier, name) = match split {
        Some(i) => (&raw[..i], &raw[i + 2..]),
        None => ("", raw),
    };
    let name = if name.starts_with("operator") { name.to_owned() } else { strip_template_args(name) };
    (remove_template_args(qualifier), name)
}

/// `Foo<std::map<a, b>>::Bar<T>` → `Foo::Bar`.
fn remove_template_args(text: &str) -> String {
    let mut depth = 0;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_owned()
}

fn strip_template_args(name: &str) -> String {
    name.split('<').next().unwrap_or(name).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_export_macros() {
        let text = "struct ASSIMP_API aiScene {};\nclass ENGINE_API AActor : public UObject {};\n\
                    class __declspec(dllexport) Foo {};\nclass Bar {};\nstruct X_API Y;\nclass NOT_A_MACRO {};\nclassify CONST x;";
        let mut expected = text.to_owned();
        for macro_text in ["ASSIMP_API", "ENGINE_API", "__declspec(dllexport)", "X_API"] {
            expected = expected.replace(macro_text, &" ".repeat(macro_text.len()));
        }
        // An all-caps class name (nothing after it) is kept, and other keywords are untouched.
        assert_eq!(mask_export_macros(text), expected);
    }

    #[test]
    fn classes_with_export_macros() {
        let text = "class ENGINE_API AActor : public UObject\n{\n    void Tick(float DeltaTime);\n    int Health;\n};";
        let mut strings = StringTable::default();
        let records = extract(&mut new_parser(), text, &mut strings, None);
        let qualified = |r: &SymbolRecord| match strings.get(r.scope) {
            "" => strings.get(r.name).to_owned(),
            scope => format!("{scope}::{}", strings.get(r.name)),
        };
        let names: Vec<_> =
            records.iter().map(|r| (qualified(r), r.kind, r.line as usize, r.column as usize)).collect();
        assert!(names.contains(&("AActor".to_owned(), SymbolKind::Class, 0, 17)), "{names:?}");
        assert!(names.contains(&("AActor::Tick".to_owned(), SymbolKind::Function, 2, 9)), "{names:?}");
        assert!(names.contains(&("AActor::Health".to_owned(), SymbolKind::Field, 3, 8)), "{names:?}");
    }

    #[test]
    fn query_compiles() {
        compiled();
    }

    #[test]
    fn qualified_names() {
        assert_eq!(split_qualified("bar"), (String::new(), "bar".to_owned()));
        assert_eq!(split_qualified("ns::Foo::bar"), ("ns::Foo".to_owned(), "bar".to_owned()));
        assert_eq!(split_qualified("Foo<T>::bar"), ("Foo".to_owned(), "bar".to_owned()));
        assert_eq!(split_qualified("Foo<std::map<a, b>>::get"), ("Foo".to_owned(), "get".to_owned()));
        assert_eq!(split_qualified("Foo::operator<"), ("Foo".to_owned(), "operator<".to_owned()));
        assert_eq!(split_qualified("Foo::~Foo"), ("Foo".to_owned(), "~Foo".to_owned()));
        assert_eq!(split_qualified("swap<int>"), (String::new(), "swap".to_owned()));
    }
}
