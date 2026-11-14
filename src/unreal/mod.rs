//! Unreal Engine support: the reflection macros (`UCLASS`, `UPROPERTY`...),
//! their specifiers, and module export macros (`ENGINE_API`...), read from
//! the engine that the open solution uses.
//!
//! tree-sitter parses C++ without a preprocessor, so these macros otherwise
//! derail it. With a profile, [`mask`] blanks them out (keeping every byte
//! offset) before parsing, and the editor colors the hidden parts itself.

mod mask;
mod sources;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use mask::{mask, MaskKind};

use crate::symbols::FileStamp;

/// Which reflection macro a specifier belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MacroKind {
    Class,
    Struct,
    Enum,
    Property,
    Function,
    Interface,
    /// `UPARAM`
    Param,
    /// `UMETA` (on enum values)
    Meta,
    Delegate,
}

impl MacroKind {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "UCLASS" => Self::Class,
            "USTRUCT" => Self::Struct,
            "UENUM" => Self::Enum,
            "UPROPERTY" => Self::Property,
            "UFUNCTION" => Self::Function,
            "UINTERFACE" => Self::Interface,
            "UPARAM" => Self::Param,
            "UMETA" => Self::Meta,
            "UDELEGATE" => Self::Delegate,
            _ => return None,
        })
    }

    /// `ObjectMacros.h` lists each macro's keywords in a namespace: `UC` for
    /// `UCLASS`, `UP` for `UPROPERTY`...
    fn from_keyword_namespace(namespace: &str) -> Option<Self> {
        Some(match namespace {
            "UC" => Self::Class,
            "UI" => Self::Interface,
            "UF" => Self::Function,
            "UP" => Self::Property,
            "US" => Self::Struct,
            "UM" => Self::Meta,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Class => "UCLASS",
            Self::Struct => "USTRUCT",
            Self::Enum => "UENUM",
            Self::Property => "UPROPERTY",
            Self::Function => "UFUNCTION",
            Self::Interface => "UINTERFACE",
            Self::Param => "UPARAM",
            Self::Meta => "UMETA",
            Self::Delegate => "UDELEGATE",
        }
    }

    /// The macros whose keyword lists also apply to this one.
    fn shares_specifiers_with(self) -> &'static [MacroKind] {
        match self {
            // UINTERFACE takes UCLASS's specifiers too, UDELEGATE takes UFUNCTION's.
            Self::Interface => &[Self::Class],
            Self::Delegate => &[Self::Function],
            _ => &[],
        }
    }
}

#[derive(Clone, Debug)]
pub struct Specifier {
    pub name: String,
    pub doc: Option<String>,
}

/// Everything Zbugger knows about an engine's reflection macros.
#[derive(Debug, Default)]
pub struct UnrealProfile {
    pub engine_dir: PathBuf,
    /// `UCLASS`, `UPROPERTY`... as defined by this engine.
    reflection_macros: HashMap<String, MacroKind>,
    /// `GENERATED_BODY` and variants.
    generated_macros: HashSet<String>,
    /// Specifiers per macro, keyed by lowercase name (UHT matches case-insensitively).
    specifiers: HashMap<MacroKind, HashMap<String, Specifier>>,
    /// Specifiers valid on every macro (UHT's `Default` table), e.g. `meta`.
    common_specifiers: HashMap<String, Specifier>,
    /// `ENGINE_API`, `CORE_API`... one per module in the solution.
    export_macros: HashSet<String>,
    /// Changes whenever any of the above does (part of the symbol cache version).
    pub fingerprint: u64,
}

impl UnrealProfile {
    pub fn reflection_macro(&self, name: &str) -> Option<MacroKind> {
        self.reflection_macros.get(name).copied()
    }

    pub fn is_generated_macro(&self, name: &str) -> bool {
        self.generated_macros.contains(name)
    }

    pub fn is_export_macro(&self, name: &str) -> bool {
        self.export_macros.contains(name)
    }

    /// The specifier `name` if it's valid on `kind`.
    pub fn specifier(&self, kind: MacroKind, name: &str) -> Option<&Specifier> {
        let key = name.to_ascii_lowercase();
        std::iter::once(kind)
            .chain(kind.shares_specifiers_with().iter().copied())
            .find_map(|k| self.specifiers.get(&k)?.get(&key))
            .or_else(|| self.common_specifiers.get(&key))
    }

    pub fn specifier_count(&self) -> usize {
        self.specifiers.values().map(HashMap::len).sum::<usize>() + self.common_specifiers.len()
    }

    pub fn export_macro_count(&self) -> usize {
        self.export_macros.len()
    }

    /// Build from parsed sources plus the solution's module names.
    fn from_parts(
        engine_dir: PathBuf,
        reflection_macros: Vec<(String, MacroKind)>,
        generated_macros: Vec<String>,
        specifiers: Vec<sources::SpecifierEntry>,
        modules: &[String],
    ) -> Self {
        let mut profile = Self { engine_dir, ..Self::default() };
        profile.reflection_macros = reflection_macros.into_iter().collect();
        profile.generated_macros = generated_macros.into_iter().collect();
        for entry in specifiers {
            let key = entry.name.to_ascii_lowercase();
            let table = match entry.kind {
                Some(kind) => profile.specifiers.entry(kind).or_default(),
                None => &mut profile.common_specifiers,
            };
            // Keep the first spelling seen, but take a description from any source.
            let existing = table.entry(key).or_insert(Specifier { name: entry.name, doc: None });
            if existing.doc.is_none() {
                existing.doc = entry.doc;
            }
        }
        profile.export_macros = modules.iter().map(|m| format!("{}_API", m.to_ascii_uppercase())).collect();
        profile.fingerprint = profile.compute_fingerprint();
        profile
    }

    fn compute_fingerprint(&self) -> u64 {
        let mut lines = self.to_lines();
        lines.sort();
        lines.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, line| {
            line.bytes().chain([b'\n']).fold(hash, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
        })
    }

    /// The profile as text lines, for the cache and the fingerprint.
    fn to_lines(&self) -> Vec<String> {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let mut lines = Vec::new();
        for (name, kind) in &self.reflection_macros {
            lines.push(format!("macro\t{name}\t{}", kind.name()));
        }
        for name in &self.generated_macros {
            lines.push(format!("generated\t{name}"));
        }
        let tables = self.specifiers.iter().map(|(k, t)| (k.name(), t)).chain([("*", &self.common_specifiers)]);
        for (kind, table) in tables {
            for spec in table.values() {
                lines.push(format!("spec\t{kind}\t{}\t{}", spec.name, clean(spec.doc.as_deref().unwrap_or(""))));
            }
        }
        for name in &self.export_macros {
            lines.push(format!("export\t{name}"));
        }
        lines
    }
}

/// Where the open solution's engine is, if it's an Unreal solution: the
/// directory three levels above `CoreUObject/Public/UObject/ObjectMacros.h`
/// (i.e. `Engine/`), found among the solution's files.
pub fn engine_dir_from_object_macros(object_macros: &Path) -> Option<PathBuf> {
    // ObjectMacros.h → UObject → Public → CoreUObject → Runtime → Source → Engine
    object_macros.ancestors().nth(6).map(Path::to_path_buf)
}

/// Whether `path` is the engine's `ObjectMacros.h`.
pub fn is_object_macros(path: &Path) -> bool {
    let text = path.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    text.ends_with("/coreuobject/public/uobject/objectmacros.h")
}

/// The module name from a `<Module>.Build.cs` file name, if it is one.
pub fn module_from_build_file(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let module = name.strip_suffix(".Build.cs").or_else(|| name.strip_suffix(".build.cs"))?;
    (!module.is_empty()).then(|| module.to_owned())
}

/// The engine files a profile is built from: `ObjectMacros.h` first, then
/// Unreal Header Tool's specifier files in a stable order.
fn source_files(engine_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let uht = engine_dir.join("Source/Programs/Shared/EpicGames.UHT");
    let mut stack = vec![uht];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with("Specifiers.cs")) {
                files.push(path);
            }
        }
    }
    files.sort();
    files.insert(0, engine_dir.join("Source/Runtime/CoreUObject/Public/UObject/ObjectMacros.h"));
    files
}

/// Build the profile for the engine at `engine_dir` from its source files.
pub fn build_profile(engine_dir: &Path, modules: &[String]) -> Option<UnrealProfile> {
    zone!("build unreal profile");
    let files = source_files(engine_dir);
    let object_macros = sources::parse_object_macros(&std::fs::read_to_string(&files[0]).ok()?);
    let mut specifiers = object_macros.specifiers;
    for file in &files[1..] {
        if let Ok(text) = std::fs::read_to_string(file) {
            specifiers.extend(sources::parse_uht_specifiers(&text));
        }
    }
    Some(UnrealProfile::from_parts(
        engine_dir.to_path_buf(),
        object_macros.reflection_macros,
        object_macros.generated_macros,
        specifiers,
        modules,
    ))
}

const CACHE_HEADER: &str = "ZBUNREAL 1";

/// `%LOCALAPPDATA%\Zbugger\unreal\<hash of the engine path>.txt`
fn cache_path(engine_dir: &Path) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let key = crate::document::path_key(engine_dir).to_string_lossy().into_owned();
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3));
    base.join("Zbugger").join("unreal").join(format!("{hash:016x}.txt"))
}

/// The profile for `engine_dir`, from the cache if its source files haven't
/// changed, otherwise built fresh and cached. Returns it and whether the
/// cache was used.
pub fn load_profile(engine_dir: &Path, modules: &[String]) -> Option<(UnrealProfile, bool)> {
    load_profile_with_cache(engine_dir, modules, &cache_path(engine_dir))
}

fn load_profile_with_cache(engine_dir: &Path, modules: &[String], cache: &Path) -> Option<(UnrealProfile, bool)> {
    zone!("load unreal profile");
    if let Some(profile) = read_cache(cache, engine_dir, modules) {
        return Some((profile, true));
    }
    let profile = build_profile(engine_dir, modules)?;
    if let Err(err) = write_cache(cache, &profile) {
        eprintln!("can't write Unreal profile cache {}: {err}", cache.display());
    }
    Some((profile, false))
}

fn write_cache(path: &Path, profile: &UnrealProfile) -> std::io::Result<()> {
    let mut text = String::from(CACHE_HEADER);
    text.push('\n');
    for file in source_files(&profile.engine_dir) {
        let stamp = FileStamp::of(&file);
        text.push_str(&format!("source\t{}\t{}\t{}\n", file.display(), stamp.modified, stamp.size));
    }
    // Modules come from the solution each time, so aren't cached.
    for line in profile.to_lines().into_iter().filter(|l| !l.starts_with("export\t")) {
        text.push_str(&line);
        text.push('\n');
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("txt.tmp");
    std::fs::write(&temp, text)?;
    std::fs::rename(&temp, path)
}

fn read_cache(path: &Path, engine_dir: &Path, modules: &[String]) -> Option<UnrealProfile> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()? != CACHE_HEADER {
        return None;
    }
    let mut sources = Vec::new();
    let (mut macros, mut generated, mut specifiers) = (Vec::new(), Vec::new(), Vec::new());
    for line in lines {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["source", file, modified, size] => {
                sources.push((PathBuf::from(file), FileStamp { modified: modified.parse().ok()?, size: size.parse().ok()? }))
            }
            ["macro", name, kind] => macros.push(((*name).to_owned(), MacroKind::from_name(kind)?)),
            ["generated", name] => generated.push((*name).to_owned()),
            ["spec", kind, name, doc] => specifiers.push(sources::SpecifierEntry {
                kind: if *kind == "*" { None } else { Some(MacroKind::from_name(kind)?) },
                name: (*name).to_owned(),
                doc: (!doc.is_empty()).then(|| (*doc).to_owned()),
            }),
            _ => return None,
        }
    }
    // Valid only if built from exactly the same, unchanged files.
    let current = source_files(engine_dir);
    let unchanged = current.len() == sources.len()
        && current.iter().zip(&sources).all(|(file, (cached, stamp))| file == cached && FileStamp::of(file) == *stamp);
    unchanged.then(|| UnrealProfile::from_parts(engine_dir.to_path_buf(), macros, generated, specifiers, modules))
}

#[cfg(test)]
mod tests;
