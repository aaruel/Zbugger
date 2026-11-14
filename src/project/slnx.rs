//! The XML `.slnx` solution format (Visual Studio 2022 17.13+, default in later versions).
//!
//! ```xml
//! <Solution>
//!   <Configurations>
//!     <BuildType Name="Debug" />
//!     <Platform Name="x64" />
//!   </Configurations>
//!   <Folder Name="/Engine/Source/">
//!     <Project Path="Engine/Engine.vcxproj" />
//!     <File Path="notes.md" />
//!   </Folder>
//!   <Project Path="Game/Game.vcxproj">
//!     <BuildDependency Project="Engine/Engine.vcxproj" />
//!   </Project>
//! </Solution>
//! ```

use std::path::Path;

use roxmltree::{Document, Node};

use super::paths::{native_separators, normalize};
use super::solution::{Solution, SolutionFolder, SolutionProject};
use super::{Configuration, ProjectError};

pub fn parse(text: &str, path: &Path) -> Result<Solution, ProjectError> {
    let doc = Document::parse(text).map_err(|e| ProjectError::Xml(path.to_path_buf(), e))?;
    let root = doc.root_element();
    if root.tag_name().name() != "Solution" {
        return Err(ProjectError::NotASolution(path.to_path_buf()));
    }
    let dir = path.parent().unwrap_or(Path::new(""));
    let resolve = |relative: &str| normalize(&dir.join(native_separators(relative)));
    let mut solution = Solution::default();

    for node in elements(root) {
        match node.tag_name().name() {
            "Configurations" => {
                let names = |tag| -> Vec<String> {
                    elements(node)
                        .filter(|n| n.tag_name().name() == tag)
                        .filter_map(|n| n.attribute("Name").map(str::to_owned))
                        .collect()
                };
                for configuration in names("BuildType") {
                    for platform in names("Platform") {
                        solution.configurations.push(Configuration { configuration: configuration.clone(), platform });
                    }
                }
            }
            "Folder" => {
                let Some(name) = node.attribute("Name") else { continue };
                let id = ensure_folder(&mut solution, name);
                for child in elements(node) {
                    match child.tag_name().name() {
                        "Project" => add_project(&mut solution, child, Some(id.clone()), &resolve),
                        "File" => {
                            if let Some(file) = child.attribute("Path") {
                                let folder = solution.folders.iter_mut().find(|f| f.id == id).expect("just added");
                                folder.files.push(resolve(file));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "Project" => add_project(&mut solution, node, None, &resolve),
            _ => {}
        }
    }

    // Dependencies were recorded as paths; turn them into project ids.
    for i in 0..solution.projects.len() {
        let dependencies = std::mem::take(&mut solution.projects[i].dependencies);
        solution.projects[i].dependencies = dependencies
            .iter()
            .filter_map(|dep| {
                let key = project_id(&resolve(dep));
                solution.projects.iter().find(|p| p.id == key).map(|p| p.id.clone())
            })
            .collect();
    }
    Ok(solution)
}

fn add_project(
    solution: &mut Solution,
    node: Node,
    parent: Option<String>,
    resolve: &dyn Fn(&str) -> std::path::PathBuf,
) {
    let Some(relative) = node.attribute("Path") else { return };
    let path = resolve(relative);
    let name = node
        .attribute("DisplayName")
        .map(str::to_owned)
        .unwrap_or_else(|| path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned()));
    let dependencies = elements(node)
        .filter(|n| n.tag_name().name() == "BuildDependency")
        .filter_map(|n| n.attribute("Project").map(str::to_owned))
        .collect();
    solution.projects.push(SolutionProject {
        id: project_id(&path),
        name,
        type_id: node.attribute("Type").map(str::to_owned),
        path,
        parent,
        dependencies,
        configurations: Vec::new(),
    });
}

/// `.slnx` projects have no GUIDs; identify them by path.
fn project_id(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// Make sure folder `/A/B/` and its parents exist; returns its id.
fn ensure_folder(solution: &mut Solution, name: &str) -> String {
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty()).collect();
    let mut parent: Option<String> = None;
    let mut id = String::from("/");
    for part in parts {
        id.push_str(part);
        id.push('/');
        if !solution.folders.iter().any(|f| f.id == id) {
            solution.folders.push(SolutionFolder {
                id: id.clone(),
                name: part.to_owned(),
                parent: parent.clone(),
                files: Vec::new(),
            });
        }
        parent = Some(id.clone());
    }
    id
}

fn elements<'a, 'input>(node: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children().filter(Node::is_element)
}
