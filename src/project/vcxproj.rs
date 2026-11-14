//! Loading Visual Studio C++ projects (`.vcxproj`).
//!
//! A `.vcxproj` is MSBuild XML. We read:
//! - `<ItemGroup>` items with an `Include` (source files, headers, ...),
//! - build configurations (`Debug|x64`, ...) and project references,
//! - `<PropertyGroup>`s, left unevaluated,
//! - shared-items projects (`.vcxitems`) it imports, whose files show up in
//!   the project too,
//! - the `.vcxproj.filters` file next to it, which defines the folder tree
//!   Visual Studio's Solution Explorer shows.
//!
//! This is not a full MSBuild evaluator: conditions are kept but not
//! evaluated, and only path-related macros (`$(ProjectDir)` and friends)
//! are expanded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};

use super::paths::{native_separators, normalize, read_text};
use super::wildcard;
use super::{Configuration, ItemKind, Project, ProjectError, ProjectItem, PropertyGroup};

/// Item types that don't name files in the project.
const NON_FILE_ITEMS: &[&str] = &[
    "ProjectConfiguration",
    "ProjectReference",
    "Reference",
    "PackageReference",
    "ProjectCapability",
    "Filter",
    "ProjectTools",
    "BuildMacro",
    "PropertyPageSchema",
    "AvailableItemName",
    "Analyzer",
    "COMReference",
    "COMFileReference",
    "NativeReference",
    "SDKReference",
];

/// Load a `.vcxproj` and its filters file.
pub fn load_vcxproj(path: &Path) -> Result<Project, ProjectError> {
    let path = normalize(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
    let stem = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let project_dir = path.parent().unwrap_or(Path::new("")).to_path_buf();

    let mut loader = Loader {
        project: Project {
            path: path.clone(),
            name: stem.clone(),
            guid: None,
            configurations: Vec::new(),
            items: Vec::new(),
            filters: Vec::new(),
            references: Vec::new(),
            property_groups: Vec::new(),
            warnings: Vec::new(),
        },
        project_dir,
        project_name: stem,
        item_filters: HashMap::new(),
    };

    // The main project file must load; everything else is best-effort.
    loader.load_file(&path)?;
    loader.load_filters(&filters_path(&path));
    // Per-user settings (debugger command and arguments...) come last, so
    // they win, as in MSBuild.
    let user_file = PathBuf::from(format!("{}.user", path.display()));
    if user_file.is_file() {
        loader.load_property_file(&user_file, 0);
    }

    let mut project = loader.finish();
    if let Some(name) = project.property("ProjectName", None) {
        project.name = name.to_owned();
    }
    project.guid = project.property("ProjectGuid", None).map(str::to_owned);
    Ok(project)
}

/// `foo.vcxproj` → `foo.vcxproj.filters`.
fn filters_path(project_file: &Path) -> PathBuf {
    let mut name = project_file.as_os_str().to_owned();
    name.push(".filters");
    PathBuf::from(name)
}

struct Loader {
    project: Project,
    project_dir: PathBuf,
    project_name: String,
    /// Filter folder for each file, keyed by [`path_key`].
    item_filters: HashMap<String, String>,
}

impl Loader {
    /// Read the items, configurations and properties of one project file
    /// (the `.vcxproj` or an imported `.vcxitems`).
    fn load_file(&mut self, file: &Path) -> Result<(), ProjectError> {
        let text = read_text(file)?;
        let doc = Document::parse(&text).map_err(|e| ProjectError::Xml(file.to_path_buf(), e))?;
        let root = doc.root_element();
        if root.tag_name().name() != "Project" {
            return Err(ProjectError::NotAProject(file.to_path_buf()));
        }
        let file_dir = file.parent().unwrap_or(Path::new("")).to_path_buf();

        for node in elements(root) {
            match node.tag_name().name() {
                "PropertyGroup" => self.project.property_groups.extend(property_groups(node)),
                "ItemGroup" => {
                    for item in elements(node) {
                        self.read_item(item, &file_dir);
                    }
                }
                "Import" => self.read_import(node, &file_dir),
                "ImportGroup" => {
                    for import in elements(node).filter(|n| n.tag_name().name() == "Import") {
                        self.read_import(import, &file_dir);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_item(&mut self, item: Node, file_dir: &Path) {
        let kind = item.tag_name().name();
        let Some(include) = item.attribute("Include") else { return };
        match kind {
            "ProjectConfiguration" => {
                let child = |name| elements(item).find(|n| n.tag_name().name() == name).and_then(|n| n.text());
                let (from_include_config, from_include_platform) = include.split_once('|').unwrap_or((include, ""));
                self.project.configurations.push(Configuration {
                    configuration: child("Configuration").unwrap_or(from_include_config).trim().to_owned(),
                    platform: child("Platform").unwrap_or(from_include_platform).trim().to_owned(),
                });
            }
            "ProjectReference" => {
                for part in split_list(include) {
                    let (path, _) = self.resolve(part, file_dir);
                    self.project.references.push(path);
                }
            }
            _ if NON_FILE_ITEMS.contains(&kind) => {}
            _ => {
                let excludes: Vec<String> = item
                    .attribute("Exclude")
                    .map(|exclude| {
                        split_list(exclude)
                            .map(|pattern| self.resolve(pattern, file_dir).0.to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                for part in split_list(include) {
                    for (include, path, unresolved) in self.expand(part, file_dir) {
                        if excludes.iter().any(|pattern| wildcard::matches(pattern, &path)) {
                            continue;
                        }
                        self.project.items.push(ProjectItem {
                            kind: ItemKind::from_element(kind),
                            include,
                            path,
                            filter: None,
                            unresolved,
                        });
                    }
                }
            }
        }
    }

    /// Follow `<Import>`s of shared-items projects, whose files belong to this
    /// project. Other imports (`.props`, `.targets`) are build machinery and skipped.
    /// Read just the `<PropertyGroup>`s of a property sheet (`.props`) or a
    /// `.vcxproj.user` file, following its own `.props` imports.
    fn load_property_file(&mut self, file: &Path, depth: usize) {
        if depth > 4 {
            return;
        }
        let Ok(text) = read_text(file) else { return };
        let Ok(doc) = Document::parse(&text) else {
            self.project.warnings.push(format!("{} is not valid XML", file.display()));
            return;
        };
        let file_dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
        for node in elements(doc.root_element()) {
            let imports: Vec<Node> = match node.tag_name().name() {
                "PropertyGroup" => {
                    self.project.property_groups.extend(property_groups(node));
                    continue;
                }
                "Import" => vec![node],
                "ImportGroup" => elements(node).filter(|n| n.tag_name().name() == "Import").collect(),
                _ => continue,
            };
            for import in imports {
                let Some(target) = import.attribute("Project") else { continue };
                if !target.to_ascii_lowercase().ends_with(".props") {
                    continue;
                }
                let (path, unresolved) = self.resolve(target, &file_dir);
                if !unresolved && path.is_file() {
                    self.load_property_file(&path, depth + 1);
                }
            }
        }
    }

    fn read_import(&mut self, import: Node, file_dir: &Path) {
        let Some(target) = import.attribute("Project") else { return };
        if target.to_ascii_lowercase().ends_with(".props") {
            // Local property sheets (like Unreal's UECommon.props) define
            // settings the build uses. System ones ($(VCTargetsPath)...) don't
            // resolve and are skipped.
            let (path, unresolved) = self.resolve(target, file_dir);
            if !unresolved && path.is_file() {
                self.load_property_file(&path, 0);
            }
            return;
        }
        if !target.to_ascii_lowercase().ends_with(".vcxitems") {
            return;
        }
        let (path, unresolved) = self.resolve(target, file_dir);
        if unresolved {
            self.project.warnings.push(format!("can't resolve shared project path {target}"));
            return;
        }
        match self.load_file(&path) {
            Ok(()) => self.load_filters(&filters_path(&path)),
            Err(err) => self.project.warnings.push(err.to_string()),
        }
    }

    /// Read a `.filters` file: the folder list, and which folder each file is in.
    /// A missing filters file is normal (the project is then shown flat).
    fn load_filters(&mut self, file: &Path) {
        if !file.exists() {
            return;
        }
        let text = match read_text(file) {
            Ok(text) => text,
            Err(err) => return self.project.warnings.push(err.to_string()),
        };
        let doc = match Document::parse(&text) {
            Ok(doc) => doc,
            Err(err) => return self.project.warnings.push(ProjectError::Xml(file.to_path_buf(), err).to_string()),
        };
        let file_dir = file.parent().unwrap_or(Path::new("")).to_path_buf();

        let item_groups = elements(doc.root_element()).filter(|n| n.tag_name().name() == "ItemGroup");
        for item in item_groups.flat_map(elements) {
            let Some(include) = item.attribute("Include") else { continue };
            if item.tag_name().name() == "Filter" {
                if !self.project.filters.iter().any(|f| f.eq_ignore_ascii_case(include)) {
                    self.project.filters.push(include.to_owned());
                }
                continue;
            }
            let Some(filter) = elements(item).find(|n| n.tag_name().name() == "Filter").and_then(|n| n.text()) else {
                continue;
            };
            for part in split_list(include) {
                for (_, path, _) in self.expand(part, &file_dir) {
                    self.item_filters.insert(path_key(&path), filter.trim().to_owned());
                }
            }
        }
    }

    /// Attach filters to items and drop duplicates (the same file listed
    /// twice, e.g. under different conditions).
    fn finish(mut self) -> Project {
        let mut seen = HashSet::new();
        self.project.items.retain(|item| seen.insert((item.kind.clone(), path_key(&item.path))));
        for item in &mut self.project.items {
            item.filter = self.item_filters.get(&path_key(&item.path)).cloned();
        }
        self.project
    }

    /// One `Include` entry → (include text, absolute path, unresolved) for
    /// each file it names. Wildcards expand to the matching files on disk.
    fn expand(&self, include: &str, base: &Path) -> Vec<(String, PathBuf, bool)> {
        let (expanded, unresolved) = self.expand_macros(include, base);
        if !unresolved && wildcard::has_wildcard(&expanded) {
            return wildcard::expand(base, &native_separators(&expanded))
                .into_iter()
                .map(|path| {
                    let include = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().into_owned();
                    (include, normalize(&path), false)
                })
                .collect();
        }
        let (path, unresolved) = self.resolve(include, base);
        vec![(include.to_owned(), path, unresolved)]
    }

    /// Expand macros in `path` and make it absolute relative to `base`.
    fn resolve(&self, path: &str, base: &Path) -> (PathBuf, bool) {
        let (expanded, unresolved) = self.expand_macros(path, base);
        (normalize(&base.join(native_separators(&expanded))), unresolved)
    }

    /// Replace the path-related MSBuild macros. Returns the text and whether
    /// any `$(...)` macro was left unexpanded.
    fn expand_macros(&self, text: &str, this_file_dir: &Path) -> (String, bool) {
        let mut out = String::with_capacity(text.len());
        let mut unresolved = false;
        let mut rest = text;
        while let Some(start) = rest.find("$(") {
            out.push_str(&rest[..start]);
            let Some(len) = rest[start..].find(')') else {
                out.push_str(&rest[start..]);
                rest = "";
                unresolved = true;
                break;
            };
            let name = &rest[start + 2..start + len];
            let with_slash = |dir: &Path| format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
            match name.to_ascii_lowercase().as_str() {
                "msbuildthisfiledirectory" => out.push_str(&with_slash(this_file_dir)),
                "projectdir" => out.push_str(&with_slash(&self.project_dir)),
                "msbuildprojectdirectory" => out.push_str(&self.project_dir.display().to_string()),
                "projectname" | "msbuildprojectname" => out.push_str(&self.project_name),
                _ => {
                    out.push_str(&rest[start..start + len + 1]);
                    unresolved = true;
                }
            }
            rest = &rest[start + len + 1..];
        }
        out.push_str(rest);
        (out, unresolved)
    }
}

/// A `<PropertyGroup>` as one or more groups: properties with their own
/// `Condition` (as CMake writes them, e.g. one `OutDir` per configuration)
/// become groups of their own, in order, so "last one wins" still holds.
fn property_groups(node: Node) -> Vec<PropertyGroup> {
    let label = node.attribute("Label").map(str::to_owned);
    let group_condition = node.attribute("Condition").map(str::to_owned);
    let mut groups = Vec::new();
    let mut current = PropertyGroup { label: label.clone(), condition: group_condition.clone(), properties: Vec::new() };
    for property in elements(node) {
        let entry = (property.tag_name().name().to_owned(), property.text().unwrap_or("").trim().to_owned());
        match property.attribute("Condition") {
            Some(condition) => {
                if !current.properties.is_empty() {
                    groups.push(std::mem::replace(
                        &mut current,
                        PropertyGroup { label: label.clone(), condition: group_condition.clone(), properties: Vec::new() },
                    ));
                }
                // The property's own condition is the one that decides.
                groups.push(PropertyGroup { label: label.clone(), condition: Some(condition.to_owned()), properties: vec![entry] });
            }
            None => current.properties.push(entry),
        }
    }
    if !current.properties.is_empty() || groups.is_empty() {
        groups.push(current);
    }
    groups
}

/// Child elements of `node`, skipping text and comments.
fn elements<'a, 'input>(node: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children().filter(Node::is_element)
}

/// Split an MSBuild `a;b;c` list, dropping empty entries.
fn split_list(list: &str) -> impl Iterator<Item = &str> {
    list.split(';').map(str::trim).filter(|s| !s.is_empty())
}



/// Key for matching the same file named different ways (case, separators).
fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) { key.to_lowercase() } else { key }
}
