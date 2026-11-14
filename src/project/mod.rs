//! Project files (currently Visual Studio `.vcxproj`) and the folder tree
//! the project view shows for them.

// Configurations, properties and references are read now for the debugger,
// which doesn't use them yet.
#![allow(dead_code)]

mod paths;
mod sln;
mod slnx;
mod solution;
#[cfg(test)]
mod solution_tests;
#[cfg(test)]
mod tests;
mod vcxproj;
mod wildcard;

pub use solution::{load_solution, ConfigurationMapping, Solution, SolutionTreeFolder};

use std::fmt;
use std::path::{Path, PathBuf};

pub use vcxproj::load_vcxproj;

/// A loaded project.
#[derive(Debug)]
pub struct Project {
    /// The project file itself.
    pub path: PathBuf,
    /// Display name: `<ProjectName>` if set, otherwise the file name without extension.
    pub name: String,
    pub guid: Option<String>,
    pub configurations: Vec<Configuration>,
    /// Source and other files in the project, in file order.
    pub items: Vec<ProjectItem>,
    /// Folder paths from the filters file, e.g. `Source Files\Network`,
    /// including folders that contain no files.
    pub filters: Vec<String>,
    /// Other projects this one references.
    pub references: Vec<PathBuf>,
    /// Every `<PropertyGroup>`, unevaluated, for settings like the output path.
    pub property_groups: Vec<PropertyGroup>,
    /// Problems that didn't stop loading (missing filters file, unreadable import...).
    pub warnings: Vec<String>,
}

/// A build configuration such as `Debug|x64`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub configuration: String,
    pub platform: String,
}

impl Configuration {
    /// The `Debug|x64` form MSBuild uses in conditions.
    pub fn key(&self) -> String {
        format!("{}|{}", self.configuration, self.platform)
    }
}

/// One file in the project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectItem {
    pub kind: ItemKind,
    /// The `Include` value as written (after splitting `a;b` lists and expanding wildcards).
    pub include: String,
    /// Absolute path with `..` resolved. Not checked for existence.
    pub path: PathBuf,
    /// Filter folder from the filters file, e.g. `Source Files\Network`.
    pub filter: Option<String>,
    /// True if the path still contains an MSBuild `$(Macro)` we couldn't resolve.
    pub unresolved: bool,
}

/// What MSBuild does with an item, from its element name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// `ClCompile`: a source file that gets compiled.
    Compile,
    /// `ClInclude`: a header.
    Include,
    ResourceCompile,
    /// `None`: listed in the project but not built.
    None,
    Text,
    Image,
    Natvis,
    CustomBuild,
    Other(String),
}

impl ItemKind {
    fn from_element(name: &str) -> Self {
        match name {
            "ClCompile" => Self::Compile,
            "ClInclude" => Self::Include,
            "ResourceCompile" => Self::ResourceCompile,
            "None" => Self::None,
            "Text" => Self::Text,
            "Image" => Self::Image,
            "Natvis" => Self::Natvis,
            "CustomBuild" => Self::CustomBuild,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// A `<PropertyGroup>` with its condition left unevaluated.
#[derive(Clone, Debug, Default)]
pub struct PropertyGroup {
    pub label: Option<String>,
    pub condition: Option<String>,
    pub properties: Vec<(String, String)>,
}

impl Project {
    /// The last value of property `name` that applies to `config` (or to all
    /// configurations), the same "last one wins" rule MSBuild uses. Only the
    /// common `'$(Configuration)|$(Platform)'=='Debug|x64'` condition form is
    /// understood; groups with other conditions are skipped. Values are not
    /// macro-expanded.
    pub fn property(&self, name: &str, config: Option<&Configuration>) -> Option<&str> {
        property_in(&self.property_groups, name, config)
    }

    /// Group the items into the folder tree the project view shows.
    pub fn tree(&self) -> ProjectTree {
        let mut root = Folder { name: self.name.clone(), path: String::new(), folders: Vec::new(), files: Vec::new() };
        for filter in &self.filters {
            root.folder_mut(filter);
        }
        for (index, item) in self.items.iter().enumerate() {
            let folder = match &item.filter {
                Some(filter) => root.folder_mut(filter),
                None => &mut root,
            };
            folder.files.push(index);
        }
        root.sort(&self.items);
        ProjectTree { root }
    }
}

/// The last value of property `name` in `groups` that applies to `config`
/// (or to all configurations); see [`Project::property`].
pub fn property_in<'a>(groups: &'a [PropertyGroup], name: &str, config: Option<&Configuration>) -> Option<&'a str> {
    let key = config.map(Configuration::key);
    groups
        .iter()
        .filter(|group| match (&group.condition, &key) {
            (None, _) => true,
            (Some(condition), Some(key)) => condition_config(condition).is_some_and(|c| c.eq_ignore_ascii_case(key)),
            (Some(_), None) => false,
        })
        .flat_map(|group| group.properties.iter())
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .last()
}

/// The condition `'$(Configuration)|$(Platform)'=='Debug|x64'` → `Debug|x64`.
fn condition_config(condition: &str) -> Option<&str> {
    let compact = condition.trim();
    let (lhs, rhs) = compact.split_once("==")?;
    let lhs = lhs.trim().trim_matches('\'');
    if !lhs.eq_ignore_ascii_case("$(Configuration)|$(Platform)") {
        return None;
    }
    Some(rhs.trim().trim_matches('\''))
}

/// The project as nested folders, ready for a tree view.
#[derive(Debug)]
pub struct ProjectTree {
    pub root: Folder,
}

#[derive(Debug)]
pub struct Folder {
    pub name: String,
    /// Full filter path, e.g. `Source Files\Network`; empty for the root.
    pub path: String,
    pub folders: Vec<Folder>,
    /// Indexes into [`Project::items`].
    pub files: Vec<usize>,
}

impl Folder {
    /// The folder for `filter` (`A\B\C`), creating it and its parents if needed.
    /// Folder names match case-insensitively, like Visual Studio.
    fn folder_mut(&mut self, filter: &str) -> &mut Folder {
        let mut folder = self;
        for part in filter.split(['\\', '/']).filter(|p| !p.is_empty()) {
            let index = match folder.folders.iter().position(|f| f.name.eq_ignore_ascii_case(part)) {
                Some(i) => i,
                None => {
                    let path = if folder.path.is_empty() { part.to_owned() } else { format!("{}\\{part}", folder.path) };
                    folder.folders.push(Folder { name: part.to_owned(), path, folders: Vec::new(), files: Vec::new() });
                    folder.folders.len() - 1
                }
            };
            folder = &mut folder.folders[index];
        }
        folder
    }

    /// Sort folders and files by name, case-insensitively, at every level.
    fn sort(&mut self, items: &[ProjectItem]) {
        self.folders.sort_by_key(|f| f.name.to_lowercase());
        self.files.sort_by_key(|&i| file_name(&items[i].path).to_lowercase());
        for folder in &mut self.folders {
            folder.sort(items);
        }
    }
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Why a project couldn't be loaded.
#[derive(Debug)]
pub enum ProjectError {
    Io(PathBuf, std::io::Error),
    Xml(PathBuf, roxmltree::Error),
    NotAProject(PathBuf),
    NotASolution(PathBuf),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, err) => write!(f, "can't read {}: {err}", path.display()),
            Self::Xml(path, err) => write!(f, "{} is not valid XML: {err}", path.display()),
            Self::NotAProject(path) => write!(f, "{} is not an MSBuild project", path.display()),
            Self::NotASolution(path) => write!(f, "{} is not a Visual Studio solution", path.display()),
        }
    }
}

impl std::error::Error for ProjectError {}
