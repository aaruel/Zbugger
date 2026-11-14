use std::path::{Path, PathBuf};

use super::mask::Masked;
use super::sources::SpecifierEntry;
use super::*;

/// A small profile, as if read from an engine.
pub fn test_profile() -> UnrealProfile {
    let spec = |kind, name: &str| SpecifierEntry { kind, name: name.to_owned(), doc: None };
    UnrealProfile::from_parts(
        PathBuf::from("Engine"),
        ["UCLASS", "USTRUCT", "UENUM", "UPROPERTY", "UFUNCTION", "UINTERFACE", "UPARAM", "UMETA"]
            .iter()
            .map(|n| ((*n).to_owned(), MacroKind::from_name(n).unwrap()))
            .collect(),
        vec!["GENERATED_BODY".to_owned(), "GENERATED_UCLASS_BODY".to_owned()],
        vec![
            spec(Some(MacroKind::Class), "Blueprintable"),
            spec(Some(MacroKind::Property), "EditAnywhere"),
            spec(Some(MacroKind::Property), "BlueprintReadWrite"),
            spec(Some(MacroKind::Property), "Category"),
            spec(Some(MacroKind::Function), "BlueprintCallable"),
            spec(Some(MacroKind::Param), "Ref"),
            spec(None, "Meta"),
        ],
        &["MyGame".to_owned(), "Engine".to_owned()],
    )
}

pub const ACTOR_H: &str = r#"#pragma once
#include "MyActor.generated.h"

UCLASS(Blueprintable)
class MYGAME_API AMyActor : public AActor
{
	GENERATED_BODY()
public:
	UPROPERTY(EditAnywhere, BlueprintReadWrite, Category = "Stats", meta = (ClampMin = "0", UIMin = 1))
	float Health = 100.f;

	UPROPERTY(Bogus)
	int32 Ammo;

	UFUNCTION(BlueprintCallable)
	void Heal(UPARAM(ref) float& Amount);

	// UPROPERTY(EditAnywhere) in a comment stays as it is
	const char* Name = "UFUNCTION(x)";
};
"#;

fn spans_of<'a>(text: &'a str, masked: &Masked) -> Vec<(&'a str, MaskKind)> {
    masked.spans.iter().map(|(r, k)| (&text[r.clone()], *k)).collect()
}

#[test]
fn masks_macros_keeping_positions() {
    let profile = test_profile();
    let masked = mask(ACTOR_H, &profile);
    assert_eq!(masked.text.len(), ACTOR_H.len());
    assert_eq!(masked.text.lines().count(), ACTOR_H.lines().count());
    for (original, after) in ACTOR_H.lines().zip(masked.text.lines()) {
        assert_eq!(original.len(), after.len(), "line length changed: {original:?}");
    }
    let lines: Vec<&str> = masked.text.lines().map(str::trim_end).collect();
    assert_eq!(lines[3].trim(), "", "UCLASS(...) hidden");
    assert_eq!(lines[4], "class            AMyActor : public AActor");
    assert_eq!(lines[6].trim(), "", "GENERATED_BODY() hidden");
    assert_eq!(lines[9].trim(), "float Health = 100.f;");
    assert_eq!(lines[15].trim(), format!("void Heal({} float& Amount);", " ".repeat("UPARAM(ref)".len())));
    // Comments, strings and #include lines are untouched.
    assert!(lines[17].contains("UPROPERTY(EditAnywhere) in a comment"));
    assert!(lines[18].contains("\"UFUNCTION(x)\""));
    assert!(lines[1].contains("MyActor.generated.h"));
}

#[test]
fn classifies_what_was_hidden() {
    let profile = test_profile();
    let masked = mask(ACTOR_H, &profile);
    let spans = spans_of(ACTOR_H, &masked);
    use MaskKind::*;
    assert_eq!(
        spans,
        [
            ("UCLASS", Macro),
            ("Blueprintable", Specifier),
            ("MYGAME_API", Macro),
            ("GENERATED_BODY", Macro),
            ("UPROPERTY", Macro),
            ("EditAnywhere", Specifier),
            ("BlueprintReadWrite", Specifier),
            ("Category", Specifier),
            ("\"Stats\"", String),
            ("meta", Specifier),
            ("ClampMin", MetaKey),
            ("\"0\"", String),
            ("UIMin", MetaKey),
            ("1", Number),
            ("UPROPERTY", Macro),
            ("Bogus", UnknownSpecifier),
            ("UFUNCTION", Macro),
            ("BlueprintCallable", Specifier),
            ("UPARAM", Macro),
            ("ref", Specifier),
        ]
    );
}

#[test]
fn leaves_unfinished_macros_alone() {
    let profile = test_profile();
    // Still typing: no closing parenthesis before the next declaration.
    let text = "UPROPERTY(EditAnywhere\nfloat Health;\n";
    let masked = mask(text, &profile);
    assert_eq!(masked.text, text);
    // A name that only looks like a macro (no parentheses) is left alone too.
    let text = "int UPROPERTY = 1;";
    assert_eq!(mask(text, &profile).text, text);
}

#[test]
fn specifier_lookup() {
    let profile = test_profile();
    assert!(profile.specifier(MacroKind::Property, "editanywhere").is_some(), "case-insensitive");
    assert!(profile.specifier(MacroKind::Function, "EditAnywhere").is_none(), "property-only");
    assert!(profile.specifier(MacroKind::Function, "Meta").is_some(), "common to all macros");
    assert!(profile.specifier(MacroKind::Interface, "Blueprintable").is_some(), "UINTERFACE takes UCLASS's");
    assert!(profile.is_export_macro("MYGAME_API"));
}

#[test]
fn recognizes_engine_files() {
    assert!(is_object_macros(Path::new(r"W:\UE\Engine\Source\Runtime\CoreUObject\Public\UObject\ObjectMacros.h")));
    assert!(!is_object_macros(Path::new(r"W:\Game\Source\ObjectMacros.h")));
    assert_eq!(
        engine_dir_from_object_macros(Path::new("/UE/Engine/Source/Runtime/CoreUObject/Public/UObject/ObjectMacros.h")),
        Some(PathBuf::from("/UE/Engine"))
    );
    assert_eq!(module_from_build_file(Path::new("Engine.Build.cs")).as_deref(), Some("Engine"));
    assert_eq!(module_from_build_file(Path::new("Engine.Target.cs")), None);
}

/// Lay out a tiny fake engine on disk.
fn fake_engine(root: &Path) -> PathBuf {
    let engine = root.join("Engine");
    let object_macros = engine.join("Source/Runtime/CoreUObject/Public/UObject/ObjectMacros.h");
    std::fs::create_dir_all(object_macros.parent().unwrap()).unwrap();
    std::fs::write(
        &object_macros,
        "#define UPROPERTY(...)\n#define GENERATED_BODY(...) X\nnamespace UP\n{\n\tenum\n\t{\n\t\t/// Edit it.\n\t\tEditAnywhere,\n\t};\n}\n",
    )
    .unwrap();
    let uht = engine.join("Source/Programs/Shared/EpicGames.UHT/Specifiers");
    std::fs::create_dir_all(&uht).unwrap();
    std::fs::write(
        uht.join("UhtPropertyMemberSpecifiers.cs"),
        "[UhtSpecifier(Extends = UhtTableNames.PropertyMember)]\nprivate static void VisibleAnywhereSpecifier(UhtSpecifierContext c) {}\n",
    )
    .unwrap();
    engine
}

#[test]
fn builds_and_caches_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let engine = fake_engine(dir.path());
    let cache = dir.path().join("cache").join("profile.txt");
    let modules = ["Game".to_owned()];

    let (fresh, from_cache) = load_profile_with_cache(&engine, &modules, &cache).unwrap();
    assert!(!from_cache);
    let spec = fresh.specifier(MacroKind::Property, "EditAnywhere").unwrap();
    assert_eq!(spec.doc.as_deref(), Some("Edit it."));
    assert!(fresh.specifier(MacroKind::Property, "VisibleAnywhere").is_some());
    assert!(fresh.is_generated_macro("GENERATED_BODY"));
    assert!(fresh.is_export_macro("GAME_API"));

    // Second load: from the cache, identical.
    let (cached, from_cache) = load_profile_with_cache(&engine, &modules, &cache).unwrap();
    assert!(from_cache);
    assert_eq!(cached.fingerprint, fresh.fingerprint);
    assert_eq!(cached.specifier_count(), fresh.specifier_count());

    // Changing a source file invalidates the cache.
    let object_macros = engine.join("Source/Runtime/CoreUObject/Public/UObject/ObjectMacros.h");
    let mut text = std::fs::read_to_string(&object_macros).unwrap();
    text.push_str("#define UFUNCTION(...)\n");
    std::fs::write(&object_macros, text).unwrap();
    let (rebuilt, from_cache) = load_profile_with_cache(&engine, &modules, &cache).unwrap();
    assert!(!from_cache);
    assert!(rebuilt.reflection_macro("UFUNCTION").is_some());
    assert_ne!(rebuilt.fingerprint, fresh.fingerprint);
}

#[test]
fn unknown_export_macros_are_hidden_but_not_colored() {
    let profile = test_profile();
    let text = "class OTHER_API AThing {};";
    let masked = mask(text, &profile);
    assert_eq!(masked.text, "class           AThing {};");
    assert!(masked.spans.is_empty());
}

/// Highlighting an Unreal header with a profile: the declarations parse as
/// plain C++, and the hidden macros get their own colors.
#[test]
fn highlights_unreal_header() {
    use crate::syntax::{detect, SyntaxHighlighter, TokenKind};
    let profile = std::sync::Arc::new(test_profile());
    let language = detect(Path::new("MyActor.h"));
    let highlighter = SyntaxHighlighter::new(language, ACTOR_H, Some(profile));
    let kind_at = |needle: &str| {
        let line = ACTOR_H.lines().position(|l| l.contains(needle)).unwrap();
        let column = ACTOR_H.lines().nth(line).unwrap().find(needle).unwrap();
        // Overlay first (hidden macros), then the parser's colors.
        highlighter
            .line_overlay(line)
            .iter()
            .chain(highlighter.line_spans(line))
            .find(|s| s.range.contains(&column))
            .map(|s| s.kind)
    };
    assert_eq!(kind_at("UCLASS"), Some(TokenKind::ReflectionMacro));
    assert_eq!(kind_at("Blueprintable"), Some(TokenKind::Specifier));
    assert_eq!(kind_at("MYGAME_API"), Some(TokenKind::ReflectionMacro));
    assert_eq!(kind_at("AMyActor"), Some(TokenKind::Type));
    assert_eq!(kind_at("GENERATED_BODY"), Some(TokenKind::ReflectionMacro));
    assert_eq!(kind_at("EditAnywhere"), Some(TokenKind::Specifier));
    assert_eq!(kind_at("ClampMin"), Some(TokenKind::Property));
    assert_eq!(kind_at("Bogus"), Some(TokenKind::UnknownSpecifier));
    // The declarations after the macros parse cleanly.
    assert_eq!(kind_at("Health"), Some(TokenKind::Property));
    assert_eq!(kind_at("int32"), Some(TokenKind::Type));
    assert_eq!(kind_at("Heal("), Some(TokenKind::Function));
}

/// The symbol index sees through the macros too.
#[test]
fn indexes_unreal_header() {
    use crate::symbols::{new_parser, SymbolIndex};
    let profile = test_profile();
    let mut index = SymbolIndex::default();
    index.update_file_text(&mut new_parser(), Path::new("MyActor.h"), ACTOR_H, Some(&profile));
    let names: Vec<_> = index
        .symbols_in_file(Path::new("MyActor.h"))
        .iter()
        .map(|s| (s.qualified_name(), s.kind.label().to_owned()))
        .collect();
    for (name, kind) in [
        ("AMyActor", "class"),
        ("AMyActor::Health", "field"),
        ("AMyActor::Ammo", "field"),
        ("AMyActor::Heal", "function"),
    ] {
        assert!(names.contains(&(name.to_owned(), kind.to_owned())), "{name} ({kind}) missing from {names:?}");
    }
}

/// Build the profile for a real Unreal solution's engine and measure its effect:
/// `ZBUGGER_SLN=x.sln ZBUGGER_CACHE=scratch\unreal.txt cargo test --release -- --ignored --nocapture real_engine_profile`
#[test]
#[ignore]
fn real_engine_profile() {
    use rayon::prelude::*;
    use std::collections::HashMap;
    use std::time::Instant;

    let solution = std::env::var("ZBUGGER_SLN").expect("set ZBUGGER_SLN");
    let cache = PathBuf::from(std::env::var("ZBUGGER_CACHE").expect("set ZBUGGER_CACHE"));
    let _ = std::fs::remove_file(&cache);
    let mut view = crate::project_view::ProjectView::open(Path::new(&solution)).unwrap();
    let (engine, modules) = view.unreal_engine().map(|(e, m)| (e.to_path_buf(), m.to_vec())).expect("an Unreal solution");
    println!("engine: {}  modules: {}", engine.display(), modules.len());

    let start = Instant::now();
    let (profile, from_cache) = load_profile_with_cache(&engine, &modules, &cache).unwrap();
    println!(
        "profile built in {:?} (from cache: {from_cache}): {} reflection macros, {} generated macros, {} specifiers, {} export macros",
        start.elapsed(),
        profile.reflection_macros.len(),
        profile.generated_macros.len(),
        profile.specifier_count(),
        profile.export_macro_count()
    );
    let start = Instant::now();
    let (_, from_cache) = load_profile_with_cache(&engine, &modules, &cache).unwrap();
    println!("profile loaded again in {:?} (from cache: {from_cache})", start.elapsed());

    // Headers that use the reflection macros.
    let headers: Vec<(PathBuf, String)> = view
        .take_index_files()
        .into_par_iter()
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("h")))
        .filter_map(|p| {
            let text = String::from_utf8_lossy(&std::fs::read(&p).ok()?).replace("\r\n", "\n");
            (text.contains("UPROPERTY(") || text.contains("UCLASS(") || text.contains("UFUNCTION(")).then_some((p, text))
        })
        .collect();
    let bytes: usize = headers.iter().map(|(_, t)| t.len()).sum();

    let start = Instant::now();
    let masked: Vec<_> = headers.par_iter().map(|(_, text)| mask(text, &profile)).collect();
    println!("masked {} headers ({:.1} MB) in {:?}", headers.len(), bytes as f64 / 1e6, start.elapsed());

    fn errors(parser: &mut tree_sitter::Parser, text: &str) -> usize {
        let tree = parser.parse(text, None).unwrap();
        if !tree.root_node().has_error() {
            return 0;
        }
        let mut count = 0;
        let mut cursor = tree.walk();
        'walk: loop {
            let node = cursor.node();
            if node.is_error() || node.is_missing() {
                count += 1;
            }
            if node.has_error() && cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break 'walk;
                }
            }
        }
        count
    }
    let counts: Vec<(usize, usize)> = headers
        .par_iter()
        .zip(&masked)
        .map_init(crate::symbols::new_parser, |parser, ((_, text), m)| (errors(parser, text), errors(parser, &m.text)))
        .collect();
    let files_before = counts.iter().filter(|(b, _)| *b > 0).count();
    let files_after = counts.iter().filter(|(_, a)| *a > 0).count();
    let (errors_before, errors_after): (usize, usize) = counts.iter().fold((0, 0), |(b, a), (x, y)| (b + x, a + y));
    println!("parse errors: {errors_before} in {files_before} files without masking, {errors_after} in {files_after} files with it");

    let mut unknown: HashMap<String, usize> = HashMap::new();
    let mut specifier_spans = 0;
    for ((_, text), m) in headers.iter().zip(&masked) {
        for (range, kind) in &m.spans {
            match kind {
                MaskKind::Specifier => specifier_spans += 1,
                MaskKind::UnknownSpecifier => *unknown.entry(text[range.clone()].to_owned()).or_default() += 1,
                _ => {}
            }
        }
    }
    let total_unknown: usize = unknown.values().sum();
    let mut unknown: Vec<_> = unknown.into_iter().collect();
    unknown.sort_by(|a, b| b.1.cmp(&a.1));
    println!("specifiers: {specifier_spans} recognized, {total_unknown} unknown ({} distinct)", unknown.len());
    for (name, n) in unknown.iter().take(15) {
        println!("  unknown: {name} x{n}");
    }
}
