//! Reading Unreal's reflection macros and specifiers from the engine's own
//! files: `ObjectMacros.h` (macro definitions, and documented keyword lists)
//! and Unreal Header Tool's C# source (the definitive specifier tables).

use super::MacroKind;

/// One specifier (`EditAnywhere`, `BlueprintCallable`...) as read from a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecifierEntry {
    /// `None` means it applies to every reflection macro (UHT's `Default` table).
    pub kind: Option<MacroKind>,
    pub name: String,
    pub doc: Option<String>,
}

/// What `ObjectMacros.h` says: which macros exist, and the documented
/// keywords for each.
#[derive(Debug, Default)]
pub struct ObjectMacros {
    /// Reflection macros defined with `(...)`, e.g. `UPROPERTY`.
    pub reflection_macros: Vec<(String, MacroKind)>,
    /// `GENERATED_BODY` and its variants.
    pub generated_macros: Vec<String>,
    pub specifiers: Vec<SpecifierEntry>,
}

pub fn parse_object_macros(text: &str) -> ObjectMacros {
    let mut out = ObjectMacros::default();

    // `#define UPROPERTY(...)` and `#define GENERATED_BODY(...) ...`
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("#define") else { continue };
        let rest = rest.trim_start();
        let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !rest[name.len()..].starts_with('(') {
            continue;
        }
        if let Some(kind) = MacroKind::from_name(&name) {
            if !out.reflection_macros.iter().any(|(n, _)| *n == name) {
                out.reflection_macros.push((name, kind));
            }
        } else if name.starts_with("GENERATED_") && !out.generated_macros.contains(&name) {
            out.generated_macros.push(name);
        }
    }

    // `namespace UP { enum { /// doc \n Name, ... }; }` and friends.
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        let kind = line.strip_prefix("namespace ").map(str::trim).and_then(MacroKind::from_keyword_namespace);
        i += 1;
        let Some(kind) = kind else { continue };
        // Find the enum's opening brace.
        while i < lines.len() && !lines[i].trim_start().starts_with("enum") {
            i += 1;
        }
        while i < lines.len() && !lines[i].contains('{') {
            i += 1;
        }
        i += 1;
        let mut doc: Vec<String> = Vec::new();
        while i < lines.len() {
            let line = lines[i].trim();
            i += 1;
            if line.starts_with("};") || line == "}" {
                break;
            }
            let comment = ["///", "//", "/**", "*/", "*"].iter().find_map(|p| line.strip_prefix(p));
            if let Some(comment) = comment {
                let comment = comment.trim().trim_end_matches("*/").trim();
                if !comment.is_empty() {
                    doc.push(comment.to_owned());
                }
                continue;
            }
            let name: String = line.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if !name.is_empty() && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                let doc = (!doc.is_empty()).then(|| doc.join(" "));
                out.specifiers.push(SpecifierEntry { kind: Some(kind), name, doc });
            }
            doc.clear();
        }
    }
    out
}

/// Specifiers declared in one of Unreal Header Tool's C# files: each
/// `[UhtSpecifier(Extends = UhtTableNames.Class, ...)]` decorates a method
/// named `<Name>Specifier`.
pub fn parse_uht_specifiers(text: &str) -> Vec<SpecifierEntry> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(attribute) = line.split("[UhtSpecifier(").nth(1) else { continue };
        let table = attribute
            .split("UhtTableNames.")
            .nth(1)
            .map(|rest| rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>())
            .unwrap_or_default();
        let kind = match table.as_str() {
            "Class" => Some(MacroKind::Class),
            "Interface" | "NativeInterface" => Some(MacroKind::Interface),
            "Function" => Some(MacroKind::Function),
            "PropertyMember" => Some(MacroKind::Property),
            "PropertyArgument" => Some(MacroKind::Param),
            "ScriptStruct" | "Struct" => Some(MacroKind::Struct),
            "Enum" => Some(MacroKind::Enum),
            "Default" | "" => None,
            _ => continue, // a table we don't map to a macro
        };
        // The decorated method is on one of the next lines (after any other attributes).
        let method = lines[i + 1..].iter().take(4).find_map(|l| {
            let name = l.split("static void ").nth(1)?.split('(').next()?.trim();
            name.strip_suffix("Specifier").map(str::to_owned)
        });
        if let Some(name) = method.filter(|n| !n.is_empty()) {
            out.push(SpecifierEntry { kind, name, doc: None });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECT_MACROS: &str = r#"
#define UPROPERTY(...)
#define UFUNCTION(...)
#define UCLASS(...) BODY_MACRO_COMBINE(CURRENT_FILE_ID,_,__LINE__,_PROLOG)
#define GENERATED_BODY(...) BODY_MACRO_COMBINE(CURRENT_FILE_ID,_,__LINE__,_GENERATED_BODY);
#define GENERATED_UCLASS_BODY(...) GENERATED_BODY_LEGACY()
#define SOME_OTHER_MACRO 1

namespace UP
{
	// valid keywords for the UPROPERTY macro
	enum
	{
		/// This property is const and should be exported as const.
		Const,

		/// Property should be loaded/saved to ini file as permanent profile.
		/// Second line.
		Config,

		EditAnywhere,
	};
}

namespace UC
{
	enum
	{
		/// Group.
		classGroup,
	};
}
"#;

    #[test]
    fn reads_object_macros() {
        let parsed = parse_object_macros(OBJECT_MACROS);
        assert_eq!(
            parsed.reflection_macros,
            [
                ("UPROPERTY".to_owned(), MacroKind::Property),
                ("UFUNCTION".to_owned(), MacroKind::Function),
                ("UCLASS".to_owned(), MacroKind::Class)
            ]
        );
        assert_eq!(parsed.generated_macros, ["GENERATED_BODY", "GENERATED_UCLASS_BODY"]);
        let names: Vec<_> = parsed.specifiers.iter().map(|s| (s.kind, s.name.as_str(), s.doc.as_deref())).collect();
        assert_eq!(
            names,
            [
                (Some(MacroKind::Property), "Const", Some("This property is const and should be exported as const.")),
                (
                    Some(MacroKind::Property),
                    "Config",
                    Some("Property should be loaded/saved to ini file as permanent profile. Second line.")
                ),
                (Some(MacroKind::Property), "EditAnywhere", None),
                (Some(MacroKind::Class), "classGroup", Some("Group.")),
            ]
        );
    }

    #[test]
    fn reads_uht_specifier_tables() {
        let cs = r#"
	[UhtSpecifier(Extends = UhtTableNames.Function, ValueType = UhtSpecifierValueType.Legacy)]
	private static void BlueprintCallableSpecifier(UhtSpecifierContext specifierContext)
	{
	}

	[UhtSpecifier(Extends = UhtTableNames.Default, ValueType = UhtSpecifierValueType.KeyValuePairList)]
	[SomeOtherAttribute]
	private static void MetaSpecifier(UhtSpecifierContext specifierContext)
	{
	}

	[UhtSpecifier(Extends = UhtTableNames.PropertyArgument)]
	private static void RefSpecifier(UhtSpecifierContext specifierContext) {}
"#;
        let specs: Vec<_> = parse_uht_specifiers(cs).into_iter().map(|s| (s.kind, s.name)).collect();
        assert_eq!(
            specs,
            [
                (Some(MacroKind::Function), "BlueprintCallable".to_owned()),
                (None, "Meta".to_owned()),
                (Some(MacroKind::Param), "Ref".to_owned()),
            ]
        );
    }
}
