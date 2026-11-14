//! The project panel: a solution's folders and projects, or a single
//! project, with each project's filter folders and files as a tree.
//!
//! Large solutions have tens of thousands of files, so the panel is built
//! for that:
//! - Everything a row needs (label, whether the file exists, its path key) is
//!   worked out once when the project loads, on the loading thread.
//! - The expanded part of the tree is flattened into a list of visible rows,
//!   rebuilt only when a folder opens or closes, and only the rows inside the
//!   scroll view are drawn each frame.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{pos2, vec2, Color32, RichText, Sense, Shape, Stroke, TextStyle, TextWrapMode, Ui, WidgetText};
use rayon::prelude::*;

use crate::build::{BuildModel, BuildProject};
use crate::document::path_key;
use crate::project::{
    file_name, load_solution, load_vcxproj, Folder, Project, ProjectError, Solution, SolutionTreeFolder,
};

/// Whether `path` is a file the project panel opens (rather than a text editor).
pub fn is_project_file(path: &Path) -> bool {
    path.extension().is_some_and(|e| ["vcxproj", "sln", "slnx"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// One entry in the tree.
struct Node {
    label: String,
    kind: NodeKind,
    children: Vec<usize>,
}

enum NodeKind {
    /// The invisible root, a solution folder, or a project's filter folder.
    Folder,
    Project { path: PathBuf, error: Option<String>, supported: bool },
    File(FileEntry),
}

struct FileEntry {
    path: PathBuf,
    /// [`path_key`] of `path`, for matching the active tab.
    key: PathBuf,
    exists: bool,
    /// Why the file can't be opened, e.g. an unexpandable MSBuild macro.
    problem: Option<String>,
    /// The names of its folders, project and itself, lowercased, for the
    /// filter box: e.g. `engine/source files/actor.cpp`.
    search: String,
}

pub struct ProjectView {
    path: PathBuf,
    title: String,
    warnings: Vec<String>,
    /// C++ files from every loaded project, deduplicated, for indexing.
    index_files: Vec<PathBuf>,
    /// For Unreal solutions: the engine directory and the solution's module names.
    unreal_engine: Option<(PathBuf, Vec<String>)>,
    /// Every file Find in Files searches: all of the solution's files except
    /// images and unresolvable paths, deduplicated.
    search_files: Arc<Vec<PathBuf>>,
    /// Configurations and projects for the build toolbar.
    build: BuildModel,
    /// [`path_key`] of every file in the solution.
    file_keys: HashSet<PathBuf>,
    /// The tree, with the invisible root at index 0.
    nodes: Vec<Node>,
    expanded: HashSet<usize>,
    /// Visible rows: (node, depth). Rebuilt when `expanded` changes.
    rows: Vec<(usize, usize)>,
    rows_dirty: bool,
    /// Widest row drawn so far, for horizontal scrolling.
    content_width: f32,
    /// The filter box's text.
    filter: String,
    /// The filter's lowercased words; a file matches if it contains all of them.
    filter_words: Vec<String>,
    /// Files matching the filter, as of the last row rebuild.
    filter_matches: usize,
}

impl ProjectView {
    /// Open a `.vcxproj`, or a `.sln`/`.slnx` and all the C++ projects in it.
    /// Slow for big solutions (disk access for every file): call it off the UI thread.
    pub fn open(path: &Path) -> Result<Self, ProjectError> {
        zone!("ProjectView::open");
        let is_solution =
            path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sln") || e.eq_ignore_ascii_case("slnx"));
        let mut builder = TreeBuilder::default();
        let (path, title) = if is_solution {
            let solution = load_solution(path)?;
            let projects: Vec<Result<Option<Project>, String>> = solution
                .projects
                .par_iter()
                .map(|p| {
                    if p.is_cpp() {
                        load_vcxproj(&p.path).map(Some).map_err(|e| e.to_string())
                    } else {
                        Ok(None)
                    }
                })
                .collect();
            builder.warnings.extend(solution.warnings.iter().cloned());
            builder.add_solution_folder(0, &solution, &projects, &solution.tree().root);
            // What the toolbar needs to build and run: configurations, and each
            // C++ project's mappings and settings.
            let build_projects = solution
                .projects
                .iter()
                .zip(&projects)
                .filter_map(|(info, loaded)| match loaded {
                    Ok(Some(project)) => {
                        let mut build = BuildProject::from_project(project, info.configurations.clone());
                        build.name = info.name.clone();
                        Some(build)
                    }
                    _ => None,
                })
                .collect();
            builder.build = BuildModel::for_solution(&solution.path, solution.configurations.clone(), build_projects);
            let title = format!("Solution '{}' ({} projects)", solution.name, solution.projects.len());
            (solution.path.clone(), title)
        } else {
            let project = load_vcxproj(path)?;
            builder.add_project(0, &project);
            builder.build = BuildModel::for_project(&project);
            (project.path.clone(), project.name.clone())
        };
        Ok(builder.finish(path, title))
    }

    /// The `.vcxproj`, `.sln` or `.slnx` that was opened.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The C++ files to index, handed over once (they're only needed at load).
    pub fn take_index_files(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.index_files)
    }

    /// For an Unreal solution, the engine directory and the module names
    /// (for `ENGINE_API`-style export macros).
    /// Whether `path` is one of the solution's files.
    pub fn contains_file(&self, path: &Path) -> bool {
        self.file_keys.contains(&path_key(path))
    }

    /// The files Find in Files searches (shared, cheap to clone).
    /// Configurations and projects for the build toolbar.
    pub fn build_model(&self) -> &BuildModel {
        &self.build
    }

    pub fn search_files(&self) -> Arc<Vec<PathBuf>> {
        self.search_files.clone()
    }

    pub fn unreal_engine(&self) -> Option<(&Path, &[String])> {
        self.unreal_engine.as_ref().map(|(dir, modules)| (dir.as_path(), modules.as_slice()))
    }

    /// Expand every folder and project (used by the profiling run).
    pub fn expand_all(&mut self) {
        self.expanded = (0..self.nodes.len()).filter(|&i| !self.nodes[i].children.is_empty()).collect();
        self.rows_dirty = true;
    }

    /// Whether the filter box has any words in it.
    fn filtering(&self) -> bool {
        !self.filter_words.is_empty()
    }

    /// Set the filter text: case-insensitive words, matched in any order.
    fn set_filter(&mut self, text: &str) {
        self.filter = text.to_owned();
        self.filter_words = text.split_whitespace().map(str::to_lowercase).collect();
        self.rows_dirty = true;
    }

    fn rebuild_rows(&mut self) {
        zone!("ProjectView::rebuild_rows");
        fn walk(nodes: &[Node], expanded: &HashSet<usize>, node: usize, depth: usize, rows: &mut Vec<(usize, usize)>) {
            for &child in &nodes[node].children {
                rows.push((child, depth));
                if expanded.contains(&child) {
                    walk(nodes, expanded, child, depth + 1, rows);
                }
            }
        }
        /// Matching files plus the folders and projects above them, all open.
        /// Returns how many files matched under `node`.
        fn walk_filtered(nodes: &[Node], words: &[String], node: usize, depth: usize, rows: &mut Vec<(usize, usize)>) -> usize {
            let mut matched = 0;
            for &child in &nodes[node].children {
                match &nodes[child].kind {
                    NodeKind::File(file) => {
                        if words.iter().all(|word| file.search.contains(word.as_str())) {
                            rows.push((child, depth));
                            matched += 1;
                        }
                    }
                    _ => {
                        // Add the folder, then take it back out if nothing below it matched.
                        let at = rows.len();
                        rows.push((child, depth));
                        let below = walk_filtered(nodes, words, child, depth + 1, rows);
                        if below == 0 {
                            rows.truncate(at);
                        }
                        matched += below;
                    }
                }
            }
            matched
        }
        self.rows.clear();
        if self.filtering() {
            self.filter_matches = walk_filtered(&self.nodes, &self.filter_words, 0, 0, &mut self.rows);
        } else {
            walk(&self.nodes, &self.expanded, 0, 0, &mut self.rows);
        }
        self.rows_dirty = false;
    }

    /// Draw the tree. `active` is the file in the current tab, which gets
    /// highlighted. Returns the file the user clicked, if any.
    pub fn ui(&mut self, ui: &mut Ui, active: Option<&Path>) -> Option<PathBuf> {
        zone!("ProjectView::ui");
        let active = active.map(path_key);
        let mut clicked = None;

        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.title).strong()).on_hover_text(self.path.display().to_string());
            if !self.warnings.is_empty() {
                ui.label(RichText::new(format!("⚠ {}", self.warnings.len())).color(ui.visuals().warn_fg_color))
                    .on_hover_ui(|ui| {
                        ui.label(self.warnings.join("\n"));
                    });
            }
        });

        // Filter box: words separated by spaces, any order, any case.
        // Laid out right to left: the clear button first, then the box takes
        // exactly the space left. (Sizing the box by hand overshot by a few
        // points, and side panels grow to fit their contents, so the panel
        // widened on every keystroke.) The row is one line high: a centered
        // right-to-left layout would otherwise take the panel's whole height.
        let row_size = vec2(ui.available_width(), ui.spacing().interact_size.y);
        ui.allocate_ui_with_layout(row_size, egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut text = self.filter.clone();
            if !text.is_empty() && ui.small_button("×").on_hover_text("Clear the filter").clicked() {
                text.clear();
            }
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    // A fixed id: focus is tracked by id, and an automatic one
                    // would change when the clear button (added before the box)
                    // appears or disappears, dropping focus mid-typing.
                    .id(filter_box_id())
                    .hint_text("Filter files")
                    // Fill the remaining width, margins included.
                    .desired_width(f32::INFINITY),
            );
            // Escape in the box (which also takes focus away from it) clears it.
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                text.clear();
            }
            response.on_hover_text(
                "Words separated by spaces, matched in any order and any case, against file, folder and project names",
            );
            if text != self.filter {
                self.set_filter(&text);
            }
        });
        if self.rows_dirty {
            self.rebuild_rows();
        }
        if self.filtering() {
            let summary = match self.filter_matches {
                0 => "No matching files".to_owned(),
                1 => "1 matching file".to_owned(),
                n => format!("{n} matching files"),
            };
            ui.label(RichText::new(summary).weak().small());
        }
        ui.separator();
        let row_height = ui.spacing().interact_size.y;
        let indent = ui.spacing().indent;
        let mut toggled = None;
        let filtering = self.filtering();

        // Rows touch; set before `show_rows`, which uses the spacing to place them.
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::both().id_salt(&self.path).auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            self.rows.len(),
            |ui, range| {
                zone!("project tree rows");
                let width = ui.available_width().max(self.content_width);
                for &(index, depth) in &self.rows[range] {
                    let node = &self.nodes[index];
                    let (rect, response) = ui.allocate_exact_size(vec2(width, row_height), Sense::click());
                    let text_left = rect.left() + 4.0 + depth as f32 * indent + indent;

                    let selected = matches!(&node.kind, NodeKind::File(f) if active.as_ref() == Some(&f.key));
                    let visuals = ui.visuals();
                    if selected {
                        ui.painter().rect_filled(rect, 2.0, visuals.selection.bg_fill);
                    } else if response.hovered() {
                        ui.painter().rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
                    }
                    let text_color =
                        if selected { visuals.selection.stroke.color } else { visuals.widgets.inactive.text_color() };

                    if !node.children.is_empty() {
                        let center = pos2(text_left - indent * 0.5, rect.center().y);
                        // While filtering, everything shown is open.
                        paint_arrow(ui, center, filtering || self.expanded.contains(&index), text_color);
                    }

                    let galley = WidgetText::from(row_text(node)).into_galley(
                        ui,
                        Some(TextWrapMode::Extend),
                        f32::INFINITY,
                        TextStyle::Button,
                    );
                    let text_pos = pos2(text_left, rect.center().y - galley.size().y / 2.0);
                    self.content_width = self.content_width.max(text_left - rect.left() + galley.size().x + 8.0);
                    ui.painter().galley(text_pos, galley, text_color);

                    // Built only when actually hovered.
                    let response = response.on_hover_ui(|ui| {
                        ui.label(row_tooltip(node));
                    });
                    if response.clicked() {
                        match &node.kind {
                            NodeKind::File(file) if file.problem.is_none() => clicked = Some(file.path.clone()),
                            _ if !node.children.is_empty() && !filtering => toggled = Some(index),
                            _ => {}
                        }
                    }
                }
            },
        );

        if let Some(index) = toggled {
            if !self.expanded.remove(&index) {
                self.expanded.insert(index);
            }
            self.rows_dirty = true;
        }
        clicked
    }
}

/// The filter box's widget id. There is only ever one project panel.
fn filter_box_id() -> egui::Id {
    egui::Id::new("project_panel_filter")
}

fn row_text(node: &Node) -> RichText {
    let text = RichText::new(&node.label);
    match &node.kind {
        NodeKind::Folder => text,
        NodeKind::Project { error: Some(_), .. } => text.weak().italics(),
        NodeKind::Project { supported: false, .. } => text.weak(),
        NodeKind::Project { .. } => text.strong(),
        NodeKind::File(file) if !file.exists || file.problem.is_some() => text.weak().italics(),
        NodeKind::File(_) => text,
    }
}

fn row_tooltip(node: &Node) -> String {
    match &node.kind {
        NodeKind::Folder => node.label.clone(),
        NodeKind::Project { path, error: Some(error), .. } => format!("{}\n{error}", path.display()),
        NodeKind::Project { path, supported: false, .. } => {
            format!("{}\nOnly C++ projects (.vcxproj) can be shown", path.display())
        }
        NodeKind::Project { path, .. } => path.display().to_string(),
        NodeKind::File(FileEntry { problem: Some(problem), .. }) => problem.clone(),
        NodeKind::File(FileEntry { path, exists: false, .. }) => format!("{}\n(file not found)", path.display()),
        NodeKind::File(file) => file.path.display().to_string(),
    }
}

/// The expand/collapse triangle.
fn paint_arrow(ui: &Ui, center: egui::Pos2, open: bool, color: Color32) {
    let size = 3.5;
    let points = if open {
        vec![center + vec2(-size, -size * 0.6), center + vec2(size, -size * 0.6), center + vec2(0.0, size * 0.8)]
    } else {
        vec![center + vec2(-size * 0.6, -size), center + vec2(-size * 0.6, size), center + vec2(size * 0.8, 0.0)]
    };
    ui.painter().add(Shape::convex_polygon(points, color, Stroke::NONE));
}

/// Builds the node list for a solution or project.
#[derive(Default)]
struct TreeBuilder {
    nodes: Vec<Node>,
    warnings: Vec<String>,
    seen_files: HashSet<PathBuf>,
    index_files: Vec<PathBuf>,
    search_files: Vec<PathBuf>,
    /// The engine's `ObjectMacros.h`, if the solution includes the engine.
    object_macros: Option<PathBuf>,
    /// Module names from `<Module>.Build.cs` files.
    modules: Vec<String>,
    build: BuildModel,
}

impl TreeBuilder {
    fn add(&mut self, parent: usize, label: String, kind: NodeKind) -> usize {
        if self.nodes.is_empty() {
            self.nodes.push(Node { label: String::new(), kind: NodeKind::Folder, children: Vec::new() });
        }
        self.nodes.push(Node { label, kind, children: Vec::new() });
        let index = self.nodes.len() - 1;
        self.nodes[parent].children.push(index);
        index
    }

    fn add_file(&mut self, parent: usize, path: &Path, problem: Option<String>) {
        if self.object_macros.is_none() && crate::unreal::is_object_macros(path) {
            self.object_macros = Some(path.to_path_buf());
        }
        if let Some(module) = crate::unreal::module_from_build_file(path) {
            self.modules.push(module);
        }
        let key = path_key(path);
        if problem.is_none() && self.seen_files.insert(key.clone()) {
            if !crate::image_view::is_image(path) {
                self.search_files.push(path.to_path_buf());
            }
            if is_cpp(path) {
                self.index_files.push(path.to_path_buf());
            }
        }
        let entry = FileEntry { path: path.to_path_buf(), key, exists: false, problem, search: String::new() };
        self.add(parent, file_name(path), NodeKind::File(entry));
    }

    fn add_solution_folder(
        &mut self,
        parent: usize,
        solution: &Solution,
        projects: &[Result<Option<Project>, String>],
        folder: &SolutionTreeFolder,
    ) {
        for sub in &folder.folders {
            let node = self.add(parent, format!("📁 {}", sub.name), NodeKind::Folder);
            self.add_solution_folder(node, solution, projects, sub);
        }
        for &index in &folder.projects {
            let info = &solution.projects[index];
            match &projects[index] {
                Ok(Some(project)) => {
                    let kind = NodeKind::Project { path: info.path.clone(), error: None, supported: true };
                    let node = self.add(parent, info.name.clone(), kind);
                    self.add_project(node, project);
                }
                Ok(None) => {
                    let kind = NodeKind::Project { path: info.path.clone(), error: None, supported: false };
                    self.add(parent, info.name.clone(), kind);
                }
                Err(error) => {
                    self.warnings.push(format!("{}: {error}", info.name));
                    let kind = NodeKind::Project { path: info.path.clone(), error: Some(error.clone()), supported: true };
                    self.add(parent, format!("{} (failed to load)", info.name), kind);
                }
            }
        }
        for file in &folder.files {
            self.add_file(parent, file, None);
        }
    }

    fn add_project(&mut self, parent: usize, project: &Project) {
        self.warnings.extend(project.warnings.iter().map(|w| format!("{}: {w}", project.name)));
        self.add_project_folder(parent, project, &project.tree().root);
    }

    fn add_project_folder(&mut self, parent: usize, project: &Project, folder: &Folder) {
        for sub in &folder.folders {
            let node = self.add(parent, format!("📁 {}", sub.name), NodeKind::Folder);
            self.add_project_folder(node, project, sub);
        }
        for &index in &folder.files {
            let item = &project.items[index];
            let problem = item
                .unresolved
                .then(|| format!("{}\n(contains an MSBuild macro Zbugger can't expand)", item.include));
            self.add_file(parent, &item.path, problem);
        }
    }

    fn finish(mut self, path: PathBuf, title: String) -> ProjectView {
        if self.nodes.is_empty() {
            self.nodes.push(Node { label: String::new(), kind: NodeKind::Folder, children: Vec::new() });
        }
        // Check every file on disk once, in parallel, rather than every frame.
        {
            zone!("check files exist");
            self.nodes.par_iter_mut().for_each(|node| {
                if let NodeKind::File(file) = &mut node.kind {
                    file.exists = file.problem.is_none() && file.path.exists();
                }
            });
        }
        // Each file's searchable text: the names of everything above it, then its own.
        {
            zone!("build file search text");
            let mut stack = Vec::new();
            fn fill(nodes: &mut [Node], node: usize, stack: &mut Vec<String>) {
                for child in nodes[node].children.clone() {
                    let name = nodes[child].label.trim_start_matches("📁 ").to_lowercase();
                    match &mut nodes[child].kind {
                        NodeKind::File(file) => {
                            let mut search = stack.join("/");
                            if !search.is_empty() {
                                search.push('/');
                            }
                            search.push_str(&name);
                            file.search = search;
                        }
                        _ => {
                            stack.push(name);
                            fill(nodes, child, stack);
                            stack.pop();
                        }
                    }
                }
            }
            fill(&mut self.nodes, 0, &mut stack);
        }
        ProjectView {
            path,
            title,
            warnings: self.warnings,
            index_files: self.index_files,
            search_files: Arc::new(self.search_files),
            build: self.build,
            file_keys: self.seen_files,
            unreal_engine: self
                .object_macros
                .as_deref()
                .and_then(crate::unreal::engine_dir_from_object_macros)
                .map(|engine| (engine, self.modules)),
            nodes: self.nodes,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            content_width: 0.0,
            filter: String::new(),
            filter_words: Vec::new(),
            filter_matches: 0,
        }
    }
}

fn is_cpp(path: &Path) -> bool {
    crate::syntax::detect(path).is_some_and(|language| language.name == "C++")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLN: &str = "Microsoft Visual Studio Solution File, Format Version 12.00
Project(\"{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}\") = \"Game\", \"Game\\Game.vcxproj\", \"{A1}\"
EndProject
Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Tools\", \"Tools\\Tools.csproj\", \"{A2}\"
EndProject
Project(\"{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}\") = \"Broken\", \"Broken\\Broken.vcxproj\", \"{A3}\"
EndProject
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Libs\", \"Libs\", \"{F1}\"
EndProject
Global
\tGlobalSection(NestedProjects) = preSolution
\t\t{A3} = {F1}
\tEndGlobalSection
EndGlobal
";

    const VCXPROJ: &str = r#"<Project>
  <ItemGroup>
    <ClCompile Include="main.cpp" />
    <ClCompile Include="gone.cpp" />
    <ClInclude Include="main.h" />
    <ClCompile Include="$(SolutionDir)src\x.cpp" />
  </ItemGroup>
</Project>"#;

    const FILTERS: &str = r#"<Project>
  <ItemGroup><Filter Include="Source" /></ItemGroup>
  <ItemGroup>
    <ClCompile Include="main.cpp"><Filter>Source</Filter></ClCompile>
    <ClCompile Include="gone.cpp"><Filter>Source</Filter></ClCompile>
  </ItemGroup>
</Project>"#;

    fn sample() -> (tempfile::TempDir, ProjectView) {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Game.sln", SLN);
        write("Game/Game.vcxproj", VCXPROJ);
        write("Game/Game.vcxproj.filters", FILTERS);
        write("Game/main.cpp", "");
        write("Game/main.h", "");
        write("Broken/Broken.vcxproj", "<Project>");
        let view = ProjectView::open(&dir.path().join("Game.sln")).unwrap();
        (dir, view)
    }

    /// The visible rows as indented labels.
    fn visible(view: &mut ProjectView) -> Vec<String> {
        view.rebuild_rows();
        view.rows.iter().map(|&(i, depth)| format!("{}{}", "  ".repeat(depth), view.nodes[i].label)).collect()
    }

    fn node(view: &ProjectView, label: &str) -> usize {
        view.nodes.iter().position(|n| n.label == label).unwrap()
    }

    #[test]
    fn rows_follow_expansion() {
        let (_dir, mut view) = sample();
        assert_eq!(visible(&mut view), ["📁 Libs", "Game", "Tools"]);

        view.expanded.insert(node(&view, "Game"));
        assert_eq!(visible(&mut view), ["📁 Libs", "Game", "  📁 Source", "  main.h", "  x.cpp", "Tools"]);

        view.expand_all();
        assert_eq!(
            visible(&mut view),
            [
                "📁 Libs",
                "  Broken (failed to load)",
                "Game",
                "  📁 Source",
                "    gone.cpp",
                "    main.cpp",
                "  main.h",
                "  x.cpp",
                "Tools",
            ]
        );
    }

    #[test]
    fn file_status_is_precomputed() {
        let (_dir, view) = sample();
        let file = |label: &str| match &view.nodes[node(&view, label)].kind {
            NodeKind::File(file) => (file.exists, file.problem.is_some()),
            _ => panic!("{label} is not a file"),
        };
        assert_eq!(file("main.cpp"), (true, false));
        assert_eq!(file("gone.cpp"), (false, false));
        assert_eq!(file("x.cpp"), (false, true), "unresolved macro");
        assert_eq!(view.warnings.len(), 1, "the broken project");
    }

    fn filtered(view: &mut ProjectView, filter: &str) -> Vec<String> {
        view.set_filter(filter);
        visible(view)
    }

    #[test]
    fn filter_matches_words_in_any_order_and_case() {
        let (_dir, mut view) = sample();
        // Matching files appear with their folders and project, all open.
        assert_eq!(filtered(&mut view, "main"), ["Game", "  📁 Source", "    main.cpp", "  main.h"]);
        assert_eq!(view.filter_matches, 2);
        // Case doesn't matter; neither does word order.
        assert_eq!(filtered(&mut view, "MAIN cpp"), ["Game", "  📁 Source", "    main.cpp"]);
        assert_eq!(filtered(&mut view, "cpp main"), ["Game", "  📁 Source", "    main.cpp"]);
        // Words can match folder and project names, not just file names.
        assert_eq!(filtered(&mut view, "source"), ["Game", "  📁 Source", "    gone.cpp", "    main.cpp"]);
        assert_eq!(filtered(&mut view, "game h"), ["Game", "  main.h"]);
        // Every word must match.
        assert!(filtered(&mut view, "main zzz").is_empty());
        assert_eq!(view.filter_matches, 0);
        // Extra spaces are ignored; clearing the filter brings back the normal tree.
        assert_eq!(filtered(&mut view, "  gone   "), ["Game", "  📁 Source", "    gone.cpp"]);
        assert_eq!(filtered(&mut view, ""), ["📁 Libs", "Game", "Tools"]);
    }

    /// Render the side panel holding `view` for several frames. Returns the
    /// panel's width each frame, and the text drawn on screen in the last one.
    fn render_panel(view: &mut ProjectView, frames: usize) -> (Vec<f32>, Vec<String>) {
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0));
        let ctx = egui::Context::default();
        let mut widths = Vec::new();
        let mut shapes = Vec::new();
        for _ in 0..frames {
            let input = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let output = ctx.run(input, |ctx| {
                let panel = egui::SidePanel::left("project_panel")
                    .resizable(true)
                    .default_width(260.0)
                    .width_range(120.0..=800.0)
                    .show(ctx, |ui| {
                        view.ui(ui, None);
                    });
                widths.push(panel.response.rect.width());
            });
            shapes = output.shapes;
        }
        // Collect text that's actually visible: inside its clip rect and on screen.
        fn texts(shape: &egui::Shape, clip: egui::Rect, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| texts(s, clip, out)),
                egui::Shape::Text(text) => {
                    let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                    if clip.intersects(rect) {
                        out.push(text.galley.text().to_owned());
                    }
                }
                _ => {}
            }
        }
        let mut drawn = Vec::new();
        for clipped in &shapes {
            texts(&clipped.shape, clipped.clip_rect.intersect(screen), &mut drawn);
        }
        (widths, drawn)
    }

    fn panel_widths(view: &mut ProjectView, frames: usize) -> Vec<f32> {
        render_panel(view, frames).0
    }

    /// Click the filter box, then type `text` one character per frame, like a
    /// user would. Returns whether anything still has keyboard focus afterwards.
    fn type_into_filter(view: &mut ProjectView, text: &str) -> bool {
        let screen = egui::Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0));
        let ctx = egui::Context::default();
        let frame = |view: &mut ProjectView, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(screen), events, ..Default::default() };
            ctx.run(input, |ctx| {
                egui::SidePanel::left("project_panel").resizable(true).default_width(260.0).show(ctx, |ui| {
                    view.ui(ui, None);
                });
            })
        };
        // Find the box by its hint text, then click it.
        let output = frame(view, Vec::new());
        let mut hint_pos = None;
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                if t.galley.text() == "Filter files" {
                    hint_pos = Some(t.pos + t.galley.size() / 2.0);
                }
            }
        }
        let at = hint_pos.expect("filter box drawn");
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(view, vec![egui::Event::PointerMoved(at), button(true)]);
        frame(view, vec![button(false)]);
        for c in text.chars() {
            frame(view, vec![egui::Event::Text(c.to_string())]);
        }
        frame(view, Vec::new());
        ctx.memory(|m| m.focused().is_some())
    }

    #[test]
    fn typing_in_the_filter_keeps_focus() {
        let (_dir, mut view) = sample();
        // The first character makes the clear button appear and the tree change;
        // focus must stay in the box throughout.
        let focused = type_into_filter(&mut view, "main");
        assert_eq!(view.filter, "main");
        assert!(focused, "the filter box lost focus while typing");
        assert_eq!(view.filter_matches, 2);
    }

    #[test]
    fn tree_rows_are_drawn() {
        let (_dir, mut view) = sample();
        let (_, drawn) = render_panel(&mut view, 3);
        for row in ["Game", "Tools"] {
            assert!(drawn.iter().any(|t| t == row), "{row} not drawn; drawn: {drawn:?}");
        }
        view.set_filter("main");
        let (_, drawn) = render_panel(&mut view, 3);
        for row in ["Game", "main.cpp", "main.h", "2 matching files"] {
            assert!(drawn.iter().any(|t| t == row), "{row} not drawn with a filter; drawn: {drawn:?}");
        }
    }

    #[test]
    fn panel_keeps_its_width_while_filtering() {
        let (_dir, mut view) = sample();
        let mut first_widths = Vec::new();
        for filter in ["", "main", "a much longer filter text than fits in the box", ""] {
            view.set_filter(filter);
            let widths = panel_widths(&mut view, 8);
            assert!(widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "filter {filter:?}: panel width changed: {widths:?}");
            first_widths.push(widths[0]);
        }
        // Typing or clearing the filter doesn't change the width either.
        assert!(first_widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "width differs between filters: {first_widths:?}");
    }

    #[test]
    fn index_files_are_cpp_and_resolvable() {
        let (_dir, mut view) = sample();
        let mut names: Vec<String> = view.take_index_files().iter().map(|p| file_name(p)).collect();
        names.sort();
        assert_eq!(names, ["gone.cpp", "main.cpp", "main.h"]);
        assert!(view.take_index_files().is_empty(), "handed over once");
    }

    /// Time opening a solution the way the app does. Prints counts and timings only.
    /// `ZBUGGER_SLN=path\to\x.sln cargo test --release -- --ignored --nocapture open_timing`
    #[test]
    #[ignore]
    fn open_timing() {
        let path = std::env::var("ZBUGGER_SLN").expect("set ZBUGGER_SLN");
        let start = std::time::Instant::now();
        let mut view = ProjectView::open(Path::new(&path)).unwrap();
        println!("opened in {:?}: {} tree nodes, {} warnings", start.elapsed(), view.nodes.len(), view.warnings.len());
        let start = std::time::Instant::now();
        view.expand_all();
        view.rebuild_rows();
        println!("expand all: {} visible rows in {:?}", view.rows.len(), start.elapsed());
        for filter in ["a", "actor", "actor component", "Engine Runtime actor CPP", "zzzz"] {
            let start = std::time::Instant::now();
            view.set_filter(filter);
            view.rebuild_rows();
            println!("filter {filter:?}: {} files, {} rows in {:?}", view.filter_matches, view.rows.len(), start.elapsed());
        }
        let files = view.take_index_files();
        let start = std::time::Instant::now();
        let indexed = crate::symbols::index_files_for_test(&files);
        println!("indexed {} files, {indexed} symbols in {:?}", files.len(), start.elapsed());
    }
}
