//! Visual Studio solutions (`.sln` and `.slnx`): the projects they contain,
//! the solution folders those are grouped into, and solution configurations.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{file_name, Configuration, ProjectError};

#[derive(Debug, Default)]
pub struct Solution {
    /// The solution file itself.
    pub path: PathBuf,
    /// File name without extension, as Visual Studio shows it.
    pub name: String,
    pub folders: Vec<SolutionFolder>,
    pub projects: Vec<SolutionProject>,
    /// Solution-level configurations, e.g. `Debug|x64`.
    pub configurations: Vec<Configuration>,
    pub warnings: Vec<String>,
}

/// A virtual folder in Solution Explorer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolutionFolder {
    /// Unique id: the GUID in `.sln`, the folder path (`/A/B/`) in `.slnx`.
    pub id: String,
    pub name: String,
    /// Id of the folder this one is nested in.
    pub parent: Option<String>,
    /// Loose files ("Solution Items") shown in the folder.
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolutionProject {
    /// Unique id: the project GUID in `.sln`, the project path in `.slnx`.
    pub id: String,
    pub name: String,
    /// Absolute path of the project file. Not checked for existence.
    pub path: PathBuf,
    /// Project type GUID from `.sln`, if any (identifies C++, C#, ...).
    pub type_id: Option<String>,
    /// Id of the solution folder containing the project.
    pub parent: Option<String>,
    /// Ids of projects that must build before this one.
    pub dependencies: Vec<String>,
    /// What each solution configuration builds in this project.
    pub configurations: Vec<ConfigurationMapping>,
}

impl SolutionProject {
    /// A Visual C++ project, which Zbugger can open.
    pub fn is_cpp(&self) -> bool {
        self.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("vcxproj"))
    }
}

/// "In solution configuration `Debug|x64`, build this project's `Debug|Win32`."
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationMapping {
    pub solution: String,
    pub project: String,
    pub build: bool,
}

/// Load a `.sln` or `.slnx`, choosing the format from the extension.
pub fn load_solution(path: &Path) -> Result<Solution, ProjectError> {
    let path = super::paths::normalize(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
    let text = super::paths::read_text(&path)?;
    let is_slnx = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("slnx"));
    let mut solution = if is_slnx { super::slnx::parse(&text, &path)? } else { super::sln::parse(&text, &path)? };
    solution.path = path.clone();
    solution.name = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    Ok(solution)
}

/// Parse `Debug|x64` into its parts.
pub(super) fn parse_configuration(text: &str) -> Configuration {
    let (configuration, platform) = text.split_once('|').unwrap_or((text, ""));
    Configuration { configuration: configuration.trim().to_owned(), platform: platform.trim().to_owned() }
}

/// The solution as nested folders, for a tree view.
#[derive(Debug, Default)]
pub struct SolutionTree {
    pub root: SolutionTreeFolder,
}

#[derive(Debug, Default)]
pub struct SolutionTreeFolder {
    pub name: String,
    pub folders: Vec<SolutionTreeFolder>,
    /// Indexes into [`Solution::projects`].
    pub projects: Vec<usize>,
    pub files: Vec<PathBuf>,
}

impl Solution {
    /// Nest folders and projects by their parent ids, sorted by name. Anything
    /// whose parent is missing (or part of a cycle) goes at the top level.
    pub fn tree(&self) -> SolutionTree {
        let folder_ids: HashSet<&str> = self.folders.iter().map(|f| f.id.as_str()).collect();
        let parent_of = |parent: &Option<String>| -> Option<String> {
            parent.as_ref().filter(|p| folder_ids.contains(p.as_str())).cloned()
        };

        // Children of each folder id ("" for the top level).
        let mut child_folders: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, folder) in self.folders.iter().enumerate() {
            let parent = parent_of(&folder.parent).filter(|p| !self.is_cycle(&folder.id, p));
            child_folders.entry(parent.unwrap_or_default()).or_default().push(i);
        }
        let mut child_projects: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, project) in self.projects.iter().enumerate() {
            child_projects.entry(parent_of(&project.parent).unwrap_or_default()).or_default().push(i);
        }

        let mut visited = HashSet::new();
        let root = self.build_folder(String::new(), &self.name, Vec::new(), &child_folders, &child_projects, &mut visited);
        SolutionTree { root }
    }

    fn build_folder(
        &self,
        id: String,
        name: &str,
        files: Vec<PathBuf>,
        child_folders: &HashMap<String, Vec<usize>>,
        child_projects: &HashMap<String, Vec<usize>>,
        visited: &mut HashSet<String>,
    ) -> SolutionTreeFolder {
        visited.insert(id.clone());
        let children: Vec<usize> = child_folders.get(&id).cloned().unwrap_or_default();
        let mut folders = Vec::new();
        for i in children {
            let folder = &self.folders[i];
            if !visited.contains(&folder.id) {
                folders.push(self.build_folder(
                    folder.id.clone(),
                    &folder.name,
                    folder.files.clone(),
                    child_folders,
                    child_projects,
                    visited,
                ));
            }
        }
        folders.sort_by_key(|f| f.name.to_lowercase());
        let mut projects = child_projects.get(&id).cloned().unwrap_or_default();
        projects.sort_by_key(|&i| self.projects[i].name.to_lowercase());
        let mut files = files;
        files.sort_by_key(|p| file_name(p).to_lowercase());
        SolutionTreeFolder { name: name.to_owned(), folders, projects, files }
    }

    /// Whether making `parent` the parent of folder `id` would loop.
    fn is_cycle(&self, id: &str, parent: &str) -> bool {
        let mut current = Some(parent.to_owned());
        let mut steps = 0;
        while let Some(folder_id) = current {
            if folder_id == id || steps > self.folders.len() {
                return true;
            }
            current = self.folders.iter().find(|f| f.id == folder_id).and_then(|f| f.parent.clone());
            steps += 1;
        }
        false
    }
}
