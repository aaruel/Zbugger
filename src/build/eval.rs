//! Expanding MSBuild `$(Name)` macros in project settings.
//!
//! This covers what build and launch settings use: the solution and project
//! paths, the configuration, the project's own properties, Visual Studio's
//! defaults for output paths, and environment variables. It's not a full
//! MSBuild evaluator (no property functions or conditions beyond the
//! configuration).

use std::path::Path;

use crate::project::{property_in, Configuration, PropertyGroup};

pub struct Evaluator<'a> {
    groups: &'a [PropertyGroup],
    config: &'a Configuration,
    project_path: &'a Path,
    solution_path: &'a Path,
}

/// `dir` as text with a trailing backslash, the way MSBuild's `*Dir` values are.
fn dir_text(dir: &Path) -> String {
    let mut text = dir.display().to_string();
    if !text.ends_with('\\') && !text.ends_with('/') {
        text.push('\\');
    }
    text
}

fn stem(path: &Path) -> String {
    path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned())
}

impl<'a> Evaluator<'a> {
    pub fn new(
        groups: &'a [PropertyGroup],
        config: &'a Configuration,
        project_path: &'a Path,
        solution_path: &'a Path,
    ) -> Self {
        Self { groups, config, project_path, solution_path }
    }

    /// The raw (unexpanded) value of `name`, if anything defines it.
    fn raw(&self, name: &str) -> Option<String> {
        let project_dir = self.project_path.parent().unwrap_or(Path::new(""));
        let solution_dir = self.solution_path.parent().unwrap_or(Path::new(""));
        // Set by Visual Studio / MSBuild themselves; projects can't override these.
        let builtin = match name.to_ascii_lowercase().as_str() {
            "solutiondir" => Some(dir_text(solution_dir)),
            "solutionpath" => Some(self.solution_path.display().to_string()),
            "solutionname" => Some(stem(self.solution_path)),
            "solutionfilename" => self.solution_path.file_name().map(|n| n.to_string_lossy().into_owned()),
            "configuration" | "configurationname" => Some(self.config.configuration.clone()),
            "platform" | "platformname" => Some(self.config.platform.clone()),
            "projectdir" | "msbuildthisfiledirectory" => Some(dir_text(project_dir)),
            "msbuildprojectdirectory" => Some(project_dir.display().to_string()),
            "projectpath" | "msbuildprojectfullpath" => Some(self.project_path.display().to_string()),
            "projectfilename" | "msbuildprojectfile" => {
                self.project_path.file_name().map(|n| n.to_string_lossy().into_owned())
            }
            "msbuildprojectname" => Some(stem(self.project_path)),
            _ => None,
        };
        if builtin.is_some() {
            return builtin;
        }
        if let Some(value) = property_in(self.groups, name, Some(self.config)) {
            return Some(value.to_owned());
        }
        // Visual Studio's defaults for C++ projects.
        let win32 = self.config.platform.eq_ignore_ascii_case("Win32");
        let default = match name.to_ascii_lowercase().as_str() {
            "projectname" => Some(stem(self.project_path)),
            "outdir" if win32 => Some("$(SolutionDir)$(Configuration)\\".to_owned()),
            "outdir" => Some("$(SolutionDir)$(Platform)\\$(Configuration)\\".to_owned()),
            "intdir" if win32 => Some("$(Configuration)\\".to_owned()),
            "intdir" => Some("$(Platform)\\$(Configuration)\\".to_owned()),
            "targetname" => Some("$(ProjectName)".to_owned()),
            "targetext" => Some(
                match property_in(self.groups, "ConfigurationType", Some(self.config)).unwrap_or("Application") {
                    "DynamicLibrary" => ".dll",
                    "StaticLibrary" => ".lib",
                    _ => ".exe",
                }
                .to_owned(),
            ),
            "targetpath" => Some("$(OutDir)$(TargetName)$(TargetExt)".to_owned()),
            _ => None,
        };
        default.or_else(|| std::env::var(name).ok())
    }

    /// Expand every `$(Name)` in `text`. Returns the result and the names that
    /// nothing defines (expanded to nothing, as MSBuild does).
    pub fn expand(&self, text: &str) -> (String, Vec<String>) {
        let mut unknown = Vec::new();
        let value = self.expand_depth(text, 0, &mut unknown);
        (value, unknown)
    }

    fn expand_depth(&self, text: &str, depth: usize, unknown: &mut Vec<String>) -> String {
        if depth > 16 {
            return text.to_owned(); // a property that refers to itself
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("$(") {
            out.push_str(&rest[..start]);
            let Some(len) = rest[start..].find(')') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let name = &rest[start + 2..start + len];
            let is_name = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !is_name {
                // A property function like $([System.IO.Path]::Combine(...)): leave it.
                out.push_str(&rest[start..start + len + 1]);
            } else if let Some(value) = self.raw(name) {
                out.push_str(&self.expand_depth(&value, depth + 1, unknown));
            } else if !unknown.iter().any(|u| u == name) {
                unknown.push(name.to_owned());
            }
            rest = &rest[start + len + 1..];
        }
        out.push_str(rest);
        out
    }
}
