use std::path::{Path, PathBuf};

use super::indexer::{refresh, Progress, RefreshStats};
use super::*;

const HEADER: &str = r#"#pragma once
#define MAX_ITEMS 16
#define CLAMP(x) ((x) < 0 ? 0 : (x))

namespace demo {

enum class Color { Red, Green };

struct Point { int x; int y; };

typedef unsigned int Id;
using Name = const char*;

class Stack {
public:
    void push(int value);
    int* top();
    bool empty() const { return count == 0; }
    static int& instances();

private:
    int count = 0;
    int* items;
};

int add(int a, int b);
extern int g_total;

} // namespace demo
"#;

const SOURCE: &str = r#"#include "stack.h"

namespace demo {

int g_total = 0;

void Stack::push(int value) {
    items[count++] = value;
}

int* Stack::top() { return items; }

int& Stack::instances() { static int n; return n; }

int add(int a, int b) { int local = a; return local + b; }

}

void push(int) {}

int main() {
    demo::Stack s;
    s.push(1);
    push(2);
    return demo::add(1, 2);
}
"#;

/// Everything indexed from `text`, as if it were the file `path`.
fn symbols(path: &str, text: &str) -> Vec<Symbol> {
    let mut index = SymbolIndex::default();
    index.update_file_text(&mut new_parser(), Path::new(path), text, None);
    index.symbols_in_file(Path::new(path))
}

fn find<'a>(symbols: &'a [Symbol], qualified: &str, definition: bool) -> &'a Symbol {
    symbols
        .iter()
        .find(|s| s.qualified_name() == qualified && s.definition == definition)
        .unwrap_or_else(|| {
            let all: Vec<_> = symbols.iter().map(|s| (s.qualified_name(), s.kind, s.definition)).collect();
            panic!("no {qualified} (definition: {definition}) in {all:#?}")
        })
}

#[test]
fn extracts_header_symbols() {
    let syms = symbols("stack.h", HEADER);
    assert_eq!(find(&syms, "MAX_ITEMS", true).kind, SymbolKind::Macro);
    assert_eq!(find(&syms, "CLAMP", true).kind, SymbolKind::Macro);
    assert_eq!(find(&syms, "demo", true).kind, SymbolKind::Namespace);
    assert_eq!(find(&syms, "demo::Color", true).kind, SymbolKind::Enum);
    assert_eq!(find(&syms, "demo::Color::Red", true).kind, SymbolKind::Enumerator);
    assert_eq!(find(&syms, "demo::Point", true).kind, SymbolKind::Struct);
    assert_eq!(find(&syms, "demo::Point::x", true).kind, SymbolKind::Field);
    assert_eq!(find(&syms, "demo::Id", true).kind, SymbolKind::Typedef);
    assert_eq!(find(&syms, "demo::Name", true).kind, SymbolKind::Typedef);
    assert_eq!(find(&syms, "demo::Stack", true).kind, SymbolKind::Class);
    // Declarations inside the class, including pointer/reference returns.
    assert_eq!(find(&syms, "demo::Stack::push", false).kind, SymbolKind::Function);
    assert_eq!(find(&syms, "demo::Stack::top", false).kind, SymbolKind::Function);
    assert_eq!(find(&syms, "demo::Stack::instances", false).kind, SymbolKind::Function);
    // Inline definition inside the class.
    assert_eq!(find(&syms, "demo::Stack::empty", true).kind, SymbolKind::Function);
    assert_eq!(find(&syms, "demo::Stack::items", true).kind, SymbolKind::Field);
    assert_eq!(find(&syms, "demo::add", false).kind, SymbolKind::Function);

    let stack = find(&syms, "demo::Stack", true);
    assert_eq!((stack.line, stack.column), (13, 6));
}

#[test]
fn extracts_source_definitions() {
    let syms = symbols("stack.cpp", SOURCE);
    assert_eq!(find(&syms, "demo::Stack::push", true).line, 6);
    assert_eq!(find(&syms, "demo::Stack::top", true).line, 10);
    assert_eq!(find(&syms, "demo::Stack::instances", true).line, 12);
    assert_eq!(find(&syms, "demo::add", true).line, 14);
    assert_eq!(find(&syms, "push", true).line, 18);
    assert_eq!(find(&syms, "demo::g_total", true).kind, SymbolKind::Variable);
    // Locals are not indexed.
    assert!(!syms.iter().any(|s| s.name == "local" || s.name == "n" || s.name == "s"));
}

#[test]
fn reference_context() {
    let text = "a = demo::Stack::instances(); s.push(1); p->top (); ~Stack(); x::y; 42;";
    let at = |needle: &str| reference_at(text, text.find(needle).unwrap() + 1).unwrap();

    let r = at("instances");
    assert_eq!((r.name.as_str(), r.qualifier.as_deref(), r.member, r.call), ("instances", Some("demo::Stack"), false, true));
    let r = at("push");
    assert_eq!((r.qualifier, r.member, r.call), (None, true, true));
    let r = at("top");
    assert_eq!((r.member, r.call), (true, true));
    assert_eq!(at("Stack()").name, "~Stack");
    assert_eq!(at("y;").qualifier.as_deref(), Some("x"));
    assert!(reference_at(text, text.find("42").unwrap()).is_none());
    // The caret just after a name still finds it.
    let end = text.find("push").unwrap() + 4;
    assert_eq!(reference_at(text, end).unwrap().name, "push");
}

fn index() -> SymbolIndex {
    let mut index = SymbolIndex::default();
    index.update_file_text(&mut new_parser(), Path::new("stack.h"), HEADER, None);
    index.update_file_text(&mut new_parser(), Path::new("stack.cpp"), SOURCE, None);
    index
}

fn first(index: &SymbolIndex, text: &str, needle: &str, from: &str) -> Symbol {
    let reference = reference_at(text, text.find(needle).unwrap() + 1).unwrap();
    index.find(&reference, Path::new(from)).into_iter().next().expect("a candidate").symbol
}

#[test]
fn ranks_definitions_and_context() {
    let index = index();
    let main = SOURCE;

    // `s.push(1)` is a member call: the method definition beats the free function.
    let s = first(&index, main, "push(1)", "stack.cpp");
    assert_eq!((s.qualified_name().as_str(), s.definition), ("demo::Stack::push", true));

    // `demo::add(1, 2)`: qualifier picks the namespace function; definition beats prototype.
    let s = first(&index, main, "add(1, 2)", "stack.cpp");
    assert_eq!((s.qualified_name().as_str(), s.definition, s.path.as_path()), ("demo::add", true, Path::new("stack.cpp")));

    // `demo::Stack s;` names the class.
    let s = first(&index, main, "Stack s", "stack.cpp");
    assert_eq!((s.kind, s.path.as_path()), (SymbolKind::Class, Path::new("stack.h")));

    // From the header, `push` still prefers the definition in the paired .cpp.
    let s = first(&index, HEADER, "push(int", "stack.h");
    assert_eq!((s.definition, s.path.as_path()), (true, Path::new("stack.cpp")));
}

/// Index a real project and look names up:
/// `ZBUGGER_VCXPROJ=x.vcxproj ZBUGGER_LOOKUP="Foo,ns::Bar,obj.method(" cargo test --release -- --ignored --nocapture real_project_index`
#[test]
#[ignore]
fn real_project_index() {
    let project = crate::project::load_vcxproj(Path::new(&std::env::var("ZBUGGER_VCXPROJ").unwrap())).unwrap();
    let files: Vec<PathBuf> = project
        .items
        .iter()
        .filter(|i| !i.unresolved && crate::syntax::detect(&i.path).is_some_and(|l| l.name == "C++"))
        .map(|i| i.path.clone())
        .collect();
    let start = std::time::Instant::now();
    let (index, stats, _) = refresh(&files, None, &Progress::default(), None);
    println!("indexed {} files, {} symbols in {:?} ({stats:?})", index.file_count(), index.symbol_count(), start.elapsed());
    for lookup in std::env::var("ZBUGGER_LOOKUP").unwrap_or_default().split(',').filter(|s| !s.is_empty()) {
        // Build a snippet so the lookup gets the same context as in code.
        let snippet = format!("x = {lookup}");
        let name_at = snippet.rfind(|c: char| c.is_alphanumeric() || c == '_').unwrap();
        let reference = reference_at(&snippet, name_at).unwrap();
        let found = index.find(&reference, Path::new("lookup.cpp"));
        println!("{lookup}: {} candidates", found.len());
        for c in found.iter().take(4) {
            let s = &c.symbol;
            println!(
                "  [{}] {} {}{} {}:{}",
                c.score,
                s.qualified_name(),
                s.kind.label(),
                if s.definition { "" } else { " decl" },
                crate::project::file_name(&s.path),
                s.line + 1
            );
        }
    }
}

#[test]
fn reindexing_a_file_replaces_its_symbols() {
    let mut index = index();
    let before = index.symbol_count();
    index.update_file_text(&mut new_parser(), Path::new("stack.cpp"), "void only_this() {}", None);
    assert!(index.symbol_count() < before);
    let r = reference_at("only_this", 0).unwrap();
    assert_eq!(index.find(&r, Path::new("x.cpp")).len(), 1);
    let r = reference_at("add", 0).unwrap();
    assert!(index.find(&r, Path::new("x.cpp")).iter().all(|c| !c.symbol.definition), "old definitions removed");
    assert_eq!(index.file_count(), 2);
}

/// Time a fresh index, writing the cache, and a cached reload of a real solution:
/// `ZBUGGER_SLN=x.sln ZBUGGER_CACHE=some\file.zbix cargo test --release -- --ignored --nocapture cache_timing`
#[test]
#[ignore]
fn cache_timing() {
    use std::time::Instant;
    let solution = std::env::var("ZBUGGER_SLN").expect("set ZBUGGER_SLN");
    let cache_file = PathBuf::from(std::env::var("ZBUGGER_CACHE").expect("set ZBUGGER_CACHE"));
    let _ = std::fs::remove_file(&cache_file);
    let files = crate::project_view::ProjectView::open(Path::new(&solution)).unwrap().take_index_files();

    let start = Instant::now();
    let (fresh, stats, save) = refresh(&files, Some(&cache_file), &Progress::default(), None);
    let fresh_time = start.elapsed();
    let bytes = save.unwrap();
    let start = Instant::now();
    cache::write(&cache_file, &bytes).unwrap();
    println!(
        "fresh: {} files, {} symbols, {} strings in {fresh_time:?} ({stats:?}); cache {:.1} MB written in {:?}",
        fresh.file_count(),
        fresh.symbol_count(),
        fresh.strings.len(),
        bytes.len() as f64 / 1e6,
        start.elapsed()
    );

    let start = Instant::now();
    let (cached, stats, _) = refresh(&files, Some(&cache_file), &Progress::default(), None);
    println!("cached: {} symbols in {:?} ({stats:?})", cached.symbol_count(), start.elapsed());

    let start = Instant::now();
    drop(fresh);
    println!("freeing an index: {:?}", start.elapsed());
}

fn write_file(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn find_one(index: &SymbolIndex, name: &str) -> Vec<String> {
    let reference = reference_at(name, 0).unwrap();
    index.find(&reference, Path::new("x.cpp")).into_iter().map(|c| c.symbol.qualified_name()).collect()
}

#[test]
fn cache_reuses_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_file(dir.path(), "a.cpp", "int alpha() { return 1; }");
    let b = write_file(dir.path(), "b.h", "struct Beta {};");
    let cache_file = dir.path().join("cache").join("index.zbix");
    let progress = Progress::default();

    // First run: no cache, everything parsed.
    let (first, stats, save) = refresh(&[a.clone(), b.clone()], Some(&cache_file), &progress, None);
    assert_eq!(stats, RefreshStats { from_cache: 0, parsed: 2, removed: 0 });
    cache::write(&cache_file, &save.expect("a new cache to save")).unwrap();

    // Nothing changed: everything comes from the cache, nothing to save.
    let (second, stats, save) = refresh(&[a.clone(), b.clone()], Some(&cache_file), &progress, None);
    assert_eq!(stats, RefreshStats { from_cache: 2, parsed: 0, removed: 0 });
    assert!(save.is_none());
    for name in ["alpha", "Beta"] {
        let r = reference_at(name, 0).unwrap();
        let symbols = |index: &SymbolIndex| -> Vec<Symbol> {
            index.find(&r, Path::new("x.cpp")).into_iter().map(|c| c.symbol).collect()
        };
        assert_eq!(symbols(&first), symbols(&second), "{name} differs after loading the cache");
    }

    // b.h changes (a different size, so it's detected even within the timer's resolution).
    write_file(dir.path(), "b.h", "struct Beta {};\nstruct Gamma {};");
    let (third, stats, save) = refresh(&[a.clone(), b.clone()], Some(&cache_file), &progress, None);
    assert_eq!(stats, RefreshStats { from_cache: 1, parsed: 1, removed: 0 });
    assert_eq!(find_one(&third, "Gamma"), ["Gamma"]);
    assert_eq!(find_one(&third, "alpha"), ["alpha"]);
    cache::write(&cache_file, &save.unwrap()).unwrap();

    // a.cpp leaves the project.
    let (fourth, stats, _) = refresh(&[b.clone()], Some(&cache_file), &progress, None);
    assert_eq!(stats, RefreshStats { from_cache: 1, parsed: 0, removed: 1 });
    assert!(find_one(&fourth, "alpha").is_empty());
    assert_eq!(fourth.file_count(), 1);
}

#[test]
fn cache_rejects_other_versions_and_damage() {
    let index = index();
    let bytes = cache::serialize(&index);
    assert!(cache::deserialize(&bytes).is_some());

    let mut wrong_version = bytes.clone();
    wrong_version[4] ^= 0xFF;
    assert!(cache::deserialize(&wrong_version).is_none());
    assert!(cache::deserialize(&bytes[..bytes.len() - 1]).is_none(), "truncated");
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(cache::deserialize(&trailing).is_none(), "trailing bytes");
    assert!(cache::deserialize(b"nonsense").is_none());
}

#[test]
fn cached_index_finds_the_same_as_fresh() {
    let fresh = index();
    let loaded = cache::deserialize(&cache::serialize(&fresh)).unwrap();
    assert_eq!(loaded.symbol_count(), fresh.symbol_count());
    for name in ["push", "add", "Stack", "MAX_ITEMS", "g_total", "Red", "~Stack"] {
        let r = reference_at(name, name.len()).unwrap();
        let names = |index: &SymbolIndex| -> Vec<(String, PathBuf, usize)> {
            index.find(&r, Path::new("stack.cpp")).into_iter().map(|c| (c.symbol.qualified_name(), c.symbol.path, c.symbol.line)).collect()
        };
        assert_eq!(names(&loaded), names(&fresh), "{name}");
    }
}

#[test]
fn edits_are_seen_before_and_after_rebuilding_the_lookup() {
    let mut index = index();
    index.rebuild_postings(); // stack.h and stack.cpp now in the sorted lookup
    // Re-index stack.cpp a few times; lookups must only ever see the latest text.
    for round in 0..3 {
        let text = format!("void round{round}() {{}}\nint add(int a, int b) {{ return a + b + {round}; }}");
        index.update_file_text(&mut new_parser(), Path::new("stack.cpp"), &text, None);
        assert_eq!(find_one(&index, &format!("round{round}")), [format!("round{round}")]);
        if round > 0 {
            assert!(find_one(&index, &format!("round{}", round - 1)).is_empty(), "stale symbol from round {}", round - 1);
        }
        // One definition (stack.cpp) and one declaration (stack.h) of add.
        assert_eq!(find_one(&index, "add").len(), 2, "round {round}");
    }
    let before: Vec<_> = ["add", "round2", "push"].iter().map(|n| find_one(&index, n)).collect();
    index.rebuild_postings();
    let after: Vec<_> = ["add", "round2", "push"].iter().map(|n| find_one(&index, n)).collect();
    assert_eq!(before, after);
}
