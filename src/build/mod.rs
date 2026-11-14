//! Building and running the solution's startup project, as described by the
//! `.sln` and `.vcxproj` files.
//!
//! Two kinds of C++ project are handled:
//! - **Makefile projects** (`NMakeBuildCommandLine`, used by Unreal Engine):
//!   build by running the command line; run `NMakeOutput`.
//! - **Regular MSBuild projects**: build with MSBuild; run
//!   `LocalDebuggerCommand`, or `$(OutDir)$(TargetName)$(TargetExt)`.
//!
//! Launch arguments and working directory come from `LocalDebuggerCommandArguments`
//! and `LocalDebuggerWorkingDirectory`, usually set in the `.vcxproj.user` file.

mod eval;
mod runner;

use std::path::{Path, PathBuf};

pub use eval::Evaluator;
#[cfg(test)]
pub use runner::find_msbuild;
pub use runner::{Job, JobEvent, JobState};
pub(crate) use runner::kill_tree;
#[cfg(windows)]
pub(crate) use runner::CREATE_NO_WINDOW;

use crate::project::{property_in, Configuration, ConfigurationMapping, Project, PropertyGroup};

/// What the toolbar offers: configurations and runnable projects.
#[derive(Clone, Debug, Default)]
pub struct BuildModel {
    /// The `.sln`, or the `.vcxproj` when one was opened on its own.
    pub path: PathBuf,
    /// Solution configurations (`Development Editor|Win64`), in file order.
    pub configurations: Vec<Configuration>,
    /// C++ projects, in solution order.
    pub projects: Vec<BuildProject>,
}

/// The parts of a project that building and running need.
#[derive(Clone, Debug)]
pub struct BuildProject {
    pub name: String,
    pub path: PathBuf,
    /// Which project configuration each solution configuration uses. Empty
    /// for a project opened on its own (configurations map to themselves).
    pub mappings: Vec<ConfigurationMapping>,
    pub configurations: Vec<Configuration>,
    pub property_groups: Vec<PropertyGroup>,
}

impl BuildProject {
    pub fn from_project(project: &Project, mappings: Vec<ConfigurationMapping>) -> Self {
        Self {
            name: project.name.clone(),
            path: project.path.clone(),
            mappings,
            configurations: project.configurations.clone(),
            property_groups: project.property_groups.clone(),
        }
    }

    /// The project configuration used for solution configuration `solution`
    /// (`Debug|x64`), and whether the solution builds the project in it.
    pub fn project_configuration(&self, solution: &str) -> Option<(Configuration, bool)> {
        if self.mappings.is_empty() {
            let config = self.configurations.iter().find(|c| c.key().eq_ignore_ascii_case(solution))?;
            return Some((config.clone(), true));
        }
        let mapping = self.mappings.iter().find(|m| m.solution.eq_ignore_ascii_case(solution))?;
        let (configuration, platform) = mapping.project.split_once('|').unwrap_or((&mapping.project, ""));
        Some((Configuration { configuration: configuration.to_owned(), platform: platform.to_owned() }, mapping.build))
    }

    fn property(&self, name: &str, config: &Configuration) -> Option<&str> {
        property_in(&self.property_groups, name, Some(config)).filter(|v| !v.is_empty())
    }

    /// How likely this is the project to run: Unreal game projects (their
    /// build command names a `.uproject` outside the engine) first, then
    /// engine programs with a `.uproject`, then applications.
    pub fn startup_rank(&self) -> u8 {
        let any = |name: &str, test: &dyn Fn(&str) -> bool| {
            self.property_groups.iter().flat_map(|g| &g.properties).any(|(n, v)| n == name && test(v))
        };
        let uproject = |v: &str| -> Option<String> {
            let lower = v.to_ascii_lowercase();
            let at = lower.find("-project=")?;
            Some(lower[at..].split(".uproject").next()?.replace('/', "\\")).filter(|_| lower.contains(".uproject"))
        };
        if any("NMakeBuildCommandLine", &|v| uproject(v).is_some_and(|p| !p.contains("engine\\") && !p.contains("programs\\"))) {
            3
        } else if any("NMakeBuildCommandLine", &|v| uproject(v).is_some()) {
            2
        } else if any("ConfigurationType", &|v| v == "Application") || any("NMakeOutput", &|v| v.ends_with(".exe")) {
            1
        } else {
            0
        }
    }
}

/// How to build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildStep {
    /// Run a command line through `cmd.exe` in `cwd` (makefile projects).
    Shell { command_line: String, cwd: PathBuf },
    /// Build the project with MSBuild.
    MsBuild { project: PathBuf, configuration: Configuration, solution_dir: PathBuf },
}

/// What to run after building.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStep {
    pub program: PathBuf,
    /// The argument string, passed as-is.
    pub arguments: String,
    pub cwd: PathBuf,
}

/// The resolved build and run steps for one project and configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    pub project: String,
    pub configuration: Configuration,
    /// `None` when the solution doesn't build the project in this configuration.
    pub build: Option<BuildStep>,
    /// `None` when there's nothing to run (e.g. a library).
    pub run: Option<RunStep>,
    /// Things worth telling the user (unknown macros...).
    pub notes: Vec<String>,
}

impl BuildModel {
    /// The model for a solution, from its loaded C++ projects.
    pub fn for_solution(path: &Path, configurations: Vec<Configuration>, projects: Vec<BuildProject>) -> Self {
        Self { path: path.to_path_buf(), configurations, projects }
    }

    /// The model for a project opened on its own.
    pub fn for_project(project: &Project) -> Self {
        Self {
            path: project.path.clone(),
            configurations: project.configurations.clone(),
            projects: vec![BuildProject::from_project(project, Vec::new())],
        }
    }

    /// The directory `$(SolutionDir)` refers to.
    pub fn solution_dir(&self) -> PathBuf {
        self.path.parent().unwrap_or(Path::new("")).to_path_buf()
    }

    /// The project to run by default: the highest [`BuildProject::startup_rank`],
    /// earliest in the solution.
    pub fn default_startup_project(&self) -> Option<&BuildProject> {
        self.projects.iter().rev().max_by_key(|p| p.startup_rank())
    }

    /// Configuration names (`Development Editor`) and platforms (`Win64`), in
    /// solution order, without duplicates.
    pub fn configuration_names(&self) -> (Vec<String>, Vec<String>) {
        let mut names: Vec<String> = Vec::new();
        let mut platforms: Vec<String> = Vec::new();
        for config in &self.configurations {
            if !names.contains(&config.configuration) {
                names.push(config.configuration.clone());
            }
            if !platforms.contains(&config.platform) {
                platforms.push(config.platform.clone());
            }
        }
        (names, platforms)
    }

    /// Work out how to build and run `project` in solution configuration `solution`.
    pub fn plan(&self, project: &BuildProject, solution: &Configuration) -> Result<LaunchPlan, String> {
        let (config, builds) = project
            .project_configuration(&solution.key())
            .ok_or_else(|| format!("{} has no configuration for {}", project.name, solution.key()))?;
        let eval = Evaluator::new(&project.property_groups, &config, &project.path, &self.path);
        let project_dir = project.path.parent().unwrap_or(Path::new("")).to_path_buf();
        let mut notes = Vec::new();
        let unknown = std::cell::RefCell::new(Vec::<String>::new());
        let expand = |text: &str| {
            let (value, names) = eval.expand(text);
            unknown.borrow_mut().extend(names);
            value
        };

        let nmake = project.property("NMakeBuildCommandLine", &config);
        let build = builds.then(|| match nmake {
            Some(command) => BuildStep::Shell { command_line: expand(command), cwd: project_dir.clone() },
            None => BuildStep::MsBuild { project: project.path.clone(), configuration: config.clone(), solution_dir: self.solution_dir() },
        });
        if !builds {
            notes.push(format!("{} isn't built in {}", project.name, solution.key()));
        }

        let program = match (project.property("LocalDebuggerCommand", &config), nmake, project.property("NMakeOutput", &config)) {
            (Some(command), _, _) => Some(expand(command)),
            (None, Some(_), Some(output)) => Some(expand(output)),
            (None, Some(_), None) => None,
            (None, None, _) => {
                let kind = project.property("ConfigurationType", &config).unwrap_or("Application");
                (kind == "Application").then(|| expand("$(TargetPath)"))
            }
        };
        let run = program.filter(|p| !p.trim().is_empty()).map(|program| RunStep {
            program: absolute(&project_dir, &program),
            arguments: project.property("LocalDebuggerCommandArguments", &config).map(expand).unwrap_or_default(),
            cwd: project
                .property("LocalDebuggerWorkingDirectory", &config)
                .map(|dir| absolute(&project_dir, &expand(dir)))
                .unwrap_or_else(|| project_dir.clone()),
        });
        if run.is_none() {
            notes.push(format!("{} doesn't produce a program to run", project.name));
        }
        let mut unknown = unknown.into_inner();
        unknown.dedup();
        for name in unknown {
            let note = format!("$({name}) isn't defined; it was left empty");
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        Ok(LaunchPlan { project: project.name.clone(), configuration: config, build, run, notes })
    }
}

/// `path` made absolute relative to `base` and tidied (`..` resolved), with
/// any surrounding quotes removed.
fn absolute(base: &Path, path: &str) -> PathBuf {
    let path = path.trim().trim_matches('"');
    let joined = base.join(path.replace('/', "\\"));
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests;
