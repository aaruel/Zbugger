//! The classic line-based `.sln` format.
//!
//! ```text
//! Microsoft Visual Studio Solution File, Format Version 12.00
//! Project("{TYPE-GUID}") = "Name", "Dir\Name.vcxproj", "{PROJECT-GUID}"
//!     ProjectSection(ProjectDependencies) = postProject
//!         {OTHER-GUID} = {OTHER-GUID}
//!     EndProjectSection
//! EndProject
//! Global
//!     GlobalSection(SolutionConfigurationPlatforms) = preSolution
//!         Debug|x64 = Debug|x64
//!     EndGlobalSection
//!     GlobalSection(ProjectConfigurationPlatforms) = postSolution
//!         {PROJECT-GUID}.Debug|x64.ActiveCfg = Debug|x64
//!         {PROJECT-GUID}.Debug|x64.Build.0 = Debug|x64
//!     EndGlobalSection
//!     GlobalSection(NestedProjects) = preSolution
//!         {CHILD-GUID} = {FOLDER-GUID}
//!     EndGlobalSection
//! EndGlobal
//! ```

use std::path::Path;

use super::paths::{native_separators, normalize};
use super::solution::{parse_configuration, ConfigurationMapping, Solution, SolutionFolder, SolutionProject};
use super::ProjectError;

/// Project type GUID of a solution folder.
const SOLUTION_FOLDER_TYPE: &str = "{2150E333-8FDC-42A3-9474-1A3956D46DE8}";

/// Which block of the file we're inside.
enum Section {
    None,
    /// Inside `Project(...) ... EndProject`, for the project or folder at this index.
    Project(Entry),
    ProjectDependencies(Entry),
    SolutionItems(Entry),
    SolutionConfigurations,
    ProjectConfigurations,
    NestedProjects,
    /// A section we don't use; skip until its end.
    Other,
}

#[derive(Clone, Copy)]
enum Entry {
    Project(usize),
    Folder(usize),
}

pub fn parse(text: &str, path: &Path) -> Result<Solution, ProjectError> {
    let is_solution = text.lines().map(str::trim).find(|l| !l.is_empty())
        .is_some_and(|l| l.starts_with("Microsoft Visual Studio Solution File"));
    if !is_solution {
        return Err(ProjectError::NotASolution(path.to_path_buf()));
    }
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut solution = Solution::default();
    let mut nesting = Vec::new();
    let mut section = Section::None;

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        section = match section {
            Section::None if line.starts_with("Project(") => match parse_project_line(line) {
                Some((type_id, name, relative, id)) => {
                    if type_id.eq_ignore_ascii_case(SOLUTION_FOLDER_TYPE) {
                        solution.folders.push(SolutionFolder { id, name, parent: None, files: Vec::new() });
                        Section::Project(Entry::Folder(solution.folders.len() - 1))
                    } else {
                        solution.projects.push(SolutionProject {
                            id,
                            name,
                            path: normalize(&dir.join(native_separators(&relative))),
                            type_id: Some(type_id.to_uppercase()),
                            parent: None,
                            dependencies: Vec::new(),
                            configurations: Vec::new(),
                        });
                        Section::Project(Entry::Project(solution.projects.len() - 1))
                    }
                }
                None => {
                    solution.warnings.push(format!("line {}: can't read project entry", number + 1));
                    Section::Other
                }
            },
            Section::None if line.starts_with("GlobalSection(") => match section_name(line) {
                "SolutionConfigurationPlatforms" => Section::SolutionConfigurations,
                "ProjectConfigurationPlatforms" => Section::ProjectConfigurations,
                "NestedProjects" => Section::NestedProjects,
                _ => Section::Other,
            },
            // `Global`, `EndGlobal`, version lines and anything else at the top level.
            Section::None => Section::None,

            Section::Project(_) if line == "EndProject" => Section::None,
            Section::Project(entry) if line.starts_with("ProjectSection(") => match section_name(line) {
                "ProjectDependencies" => Section::ProjectDependencies(entry),
                "SolutionItems" => Section::SolutionItems(entry),
                _ => Section::Other,
            },
            Section::Project(entry) => Section::Project(entry),

            Section::ProjectDependencies(entry) | Section::SolutionItems(entry) if line == "EndProjectSection" => {
                Section::Project(entry)
            }
            Section::ProjectDependencies(entry) => {
                if let (Entry::Project(i), Some((dependency, _))) = (entry, split_pair(line)) {
                    solution.projects[i].dependencies.push(dependency.to_uppercase());
                }
                Section::ProjectDependencies(entry)
            }
            Section::SolutionItems(entry) => {
                if let (Entry::Folder(i), Some((file, _))) = (entry, split_pair(line)) {
                    solution.folders[i].files.push(normalize(&dir.join(native_separators(file))));
                }
                Section::SolutionItems(entry)
            }

            Section::SolutionConfigurations
            | Section::ProjectConfigurations
            | Section::NestedProjects
            | Section::Other
                if line == "EndGlobalSection" || line == "EndProjectSection" || line == "EndProject" =>
            {
                Section::None
            }
            Section::SolutionConfigurations => {
                if let Some((name, _)) = split_pair(line) {
                    solution.configurations.push(parse_configuration(name));
                }
                Section::SolutionConfigurations
            }
            Section::ProjectConfigurations => {
                parse_project_configuration(line, &mut solution);
                Section::ProjectConfigurations
            }
            Section::NestedProjects => {
                if let Some((child, parent)) = split_pair(line) {
                    nesting.push((child.to_uppercase(), parent.to_uppercase()));
                }
                Section::NestedProjects
            }
            Section::Other => Section::Other,
        };
    }

    for (child, parent) in nesting {
        if let Some(project) = solution.projects.iter_mut().find(|p| p.id == child) {
            project.parent = Some(parent);
        } else if let Some(folder) = solution.folders.iter_mut().find(|f| f.id == child) {
            folder.parent = Some(parent);
        }
    }
    Ok(solution)
}

/// `Project("{TYPE}") = "Name", "Path", "{GUID}"` → (type, name, path, guid).
/// GUIDs are upper-cased so they compare reliably.
fn parse_project_line(line: &str) -> Option<(String, String, String, String)> {
    // The quoted strings are at the odd positions when splitting on `"`.
    let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
    match quoted.as_slice() {
        [type_id, name, path, id, ..] => {
            Some((type_id.to_uppercase(), (*name).to_owned(), (*path).to_owned(), id.to_uppercase()))
        }
        _ => None,
    }
}

/// `GlobalSection(Name) = preSolution` → `Name`.
fn section_name(line: &str) -> &str {
    line.split_once('(').and_then(|(_, rest)| rest.split_once(')')).map_or("", |(name, _)| name.trim())
}

/// `left = right`, trimmed.
fn split_pair(line: &str) -> Option<(&str, &str)> {
    line.split_once('=').map(|(a, b)| (a.trim(), b.trim()))
}

/// `{GUID}.Debug|x64.ActiveCfg = Debug|Win32` or `{GUID}.Debug|x64.Build.0 = ...`.
fn parse_project_configuration(line: &str, solution: &mut Solution) {
    let Some((key, value)) = split_pair(line) else { return };
    let Some((id, rest)) = key.split_once('.') else { return };
    let Some(project) = solution.projects.iter_mut().find(|p| p.id.eq_ignore_ascii_case(id)) else { return };
    let (config, property) = if let Some(config) = rest.strip_suffix(".ActiveCfg") {
        (config, "ActiveCfg")
    } else if let Some(config) = rest.strip_suffix(".Build.0") {
        (config, "Build")
    } else {
        return; // e.g. Deploy.0
    };
    let index = match project.configurations.iter().position(|m| m.solution == config) {
        Some(i) => i,
        None => {
            project.configurations.push(ConfigurationMapping {
                solution: config.to_owned(),
                project: value.to_owned(),
                build: false,
            });
            project.configurations.len() - 1
        }
    };
    let mapping = &mut project.configurations[index];
    match property {
        "ActiveCfg" => mapping.project = value.to_owned(),
        _ => mapping.build = true,
    }
}
