use std::path::{Path, PathBuf};

use super::*;

const SLN: &str = "\u{feff}
Microsoft Visual Studio Solution File, Format Version 12.00
# Visual Studio Version 17
VisualStudioVersion = 17.0.31903.59
MinimumVisualStudioVersion = 10.0.40219.1
Project(\"{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}\") = \"Game\", \"Game\\Game.vcxproj\", \"{aaaaaaaa-0000-0000-0000-000000000001}\"
\tProjectSection(ProjectDependencies) = postProject
\t\t{AAAAAAAA-0000-0000-0000-000000000002} = {AAAAAAAA-0000-0000-0000-000000000002}
\tEndProjectSection
EndProject
Project(\"{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}\") = \"Engine\", \"..\\Engine\\Engine.vcxproj\", \"{AAAAAAAA-0000-0000-0000-000000000002}\"
EndProject
Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Tools\", \"Tools\\Tools.csproj\", \"{AAAAAAAA-0000-0000-0000-000000000003}\"
EndProject
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Engine\", \"Engine\", \"{FFFFFFFF-0000-0000-0000-000000000001}\"
EndProject
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Runtime\", \"Runtime\", \"{FFFFFFFF-0000-0000-0000-000000000002}\"
EndProject
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Solution Items\", \"Solution Items\", \"{FFFFFFFF-0000-0000-0000-000000000003}\"
\tProjectSection(SolutionItems) = preProject
\t\treadme.md = readme.md
\t\t.editorconfig = .editorconfig
\tEndProjectSection
EndProject
Global
\tGlobalSection(SolutionConfigurationPlatforms) = preSolution
\t\tDebug|x64 = Debug|x64
\t\tRelease|x64 = Release|x64
\tEndGlobalSection
\tGlobalSection(ProjectConfigurationPlatforms) = postSolution
\t\t{AAAAAAAA-0000-0000-0000-000000000001}.Debug|x64.ActiveCfg = Debug|x64
\t\t{AAAAAAAA-0000-0000-0000-000000000001}.Debug|x64.Build.0 = Debug|x64
\t\t{AAAAAAAA-0000-0000-0000-000000000001}.Release|x64.ActiveCfg = Release|x64
\t\t{AAAAAAAA-0000-0000-0000-000000000002}.Debug|x64.ActiveCfg = DebugGame|x64
\tEndGlobalSection
\tGlobalSection(SolutionProperties) = preSolution
\t\tHideSolutionNode = FALSE
\tEndGlobalSection
\tGlobalSection(NestedProjects) = preSolution
\t\t{AAAAAAAA-0000-0000-0000-000000000002} = {FFFFFFFF-0000-0000-0000-000000000002}
\t\t{FFFFFFFF-0000-0000-0000-000000000002} = {FFFFFFFF-0000-0000-0000-000000000001}
\tEndGlobalSection
EndGlobal
";

const SLNX: &str = r#"<Solution>
  <Configurations>
    <BuildType Name="Debug" />
    <BuildType Name="Release" />
    <Platform Name="x64" />
  </Configurations>
  <Folder Name="/Engine/Runtime/">
    <Project Path="../Engine/Engine.vcxproj" />
  </Folder>
  <Folder Name="/Solution Items/">
    <File Path="readme.md" />
  </Folder>
  <Project Path="Game/Game.vcxproj">
    <BuildDependency Project="../Engine/Engine.vcxproj" />
  </Project>
  <Project Path="Tools/Tools.csproj" DisplayName="Build Tools" />
</Solution>
"#;

fn write_solution(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("work").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Visual Studio writes CRLF line endings.
    std::fs::write(&path, contents.replace('\n', "\r\n")).unwrap();
    (dir, path)
}

fn project<'a>(solution: &'a Solution, name: &str) -> &'a solution::SolutionProject {
    solution.projects.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("no project {name}"))
}

/// Names in a tree folder: subfolders, then projects, then files.
fn listing(solution: &Solution, folder: &SolutionTreeFolder) -> Vec<String> {
    let folders = folder.folders.iter().map(|f| format!("{}/", f.name));
    let projects = folder.projects.iter().map(|&i| solution.projects[i].name.clone());
    let files = folder.files.iter().map(|p| file_name(p));
    folders.chain(projects).chain(files).collect()
}

#[test]
fn parses_sln() {
    let (dir, path) = write_solution("Game.sln", SLN);
    let solution = load_solution(&path).unwrap();
    let work = dir.path().join("work");

    assert_eq!(solution.name, "Game");
    assert!(solution.warnings.is_empty(), "{:?}", solution.warnings);
    let configs: Vec<_> = solution.configurations.iter().map(Configuration::key).collect();
    assert_eq!(configs, ["Debug|x64", "Release|x64"]);
    assert_eq!(solution.projects.len(), 3);
    assert_eq!(solution.folders.len(), 3);

    let game = project(&solution, "Game");
    assert_eq!(game.path, work.join("Game").join("Game.vcxproj"));
    assert!(game.is_cpp());
    // GUIDs are compared case-insensitively.
    assert_eq!(game.id, "{AAAAAAAA-0000-0000-0000-000000000001}");
    assert_eq!(game.dependencies, ["{AAAAAAAA-0000-0000-0000-000000000002}"]);
    assert_eq!(game.configurations.len(), 2);
    assert_eq!(game.configurations[0].solution, "Debug|x64");
    assert!(game.configurations[0].build);
    assert!(!game.configurations[1].build, "Release has no Build.0 line");

    let engine = project(&solution, "Engine");
    assert_eq!(engine.path, dir.path().join("Engine").join("Engine.vcxproj"));
    assert_eq!(engine.configurations[0].project, "DebugGame|x64");
    assert!(!project(&solution, "Tools").is_cpp());
}

#[test]
fn builds_sln_tree() {
    let (_dir, path) = write_solution("Game.sln", SLN);
    let solution = load_solution(&path).unwrap();
    let tree = solution.tree();
    assert_eq!(tree.root.name, "Game");
    assert_eq!(listing(&solution, &tree.root), ["Engine/", "Solution Items/", "Game", "Tools"]);
    let engine = &tree.root.folders[0];
    assert_eq!(listing(&solution, engine), ["Runtime/"]);
    // The Engine project sits in the Runtime folder, which is in the Engine folder.
    assert_eq!(listing(&solution, &engine.folders[0]), ["Engine"]);
    assert_eq!(listing(&solution, &tree.root.folders[1]), [".editorconfig", "readme.md"]);
}

#[test]
fn parses_slnx() {
    let (dir, path) = write_solution("Game.slnx", SLNX);
    let solution = load_solution(&path).unwrap();
    let configs: Vec<_> = solution.configurations.iter().map(Configuration::key).collect();
    assert_eq!(configs, ["Debug|x64", "Release|x64"]);

    let engine = project(&solution, "Engine");
    assert_eq!(engine.path, dir.path().join("Engine").join("Engine.vcxproj"));
    assert_eq!(project(&solution, "Game").dependencies, [engine.id.clone()]);
    assert_eq!(project(&solution, "Build Tools").path, dir.path().join("work").join("Tools").join("Tools.csproj"));

    // `/Engine/Runtime/` creates both folders, nested.
    let tree = solution.tree();
    assert_eq!(listing(&solution, &tree.root), ["Engine/", "Solution Items/", "Build Tools", "Game"]);
    assert_eq!(listing(&solution, &tree.root.folders[0]), ["Runtime/"]);
    assert_eq!(listing(&solution, &tree.root.folders[0].folders[0]), ["Engine"]);
    assert_eq!(listing(&solution, &tree.root.folders[1]), ["readme.md"]);
}

#[test]
fn survives_bad_nesting() {
    // Two folders that claim each other as parent, and a project in a folder that doesn't exist.
    let sln = "Microsoft Visual Studio Solution File, Format Version 12.00
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"A\", \"A\", \"{A}\"
EndProject
Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"B\", \"B\", \"{B}\"
EndProject
Project(\"{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}\") = \"P\", \"P.vcxproj\", \"{P}\"
EndProject
Global
\tGlobalSection(NestedProjects) = preSolution
\t\t{A} = {B}
\t\t{B} = {A}
\t\t{P} = {MISSING}
\tEndGlobalSection
EndGlobal
";
    let (_dir, path) = write_solution("Bad.sln", sln);
    let solution = load_solution(&path).unwrap();
    let tree = solution.tree();
    // Every folder and project still appears exactly once.
    fn count(folder: &SolutionTreeFolder) -> (usize, usize) {
        folder.folders.iter().map(count).fold((folder.folders.len(), folder.projects.len()), |a, b| (a.0 + b.0, a.1 + b.1))
    }
    assert_eq!(count(&tree.root), (2, 1));
    assert_eq!(tree.root.projects.len(), 1, "project with a missing parent goes to the top level");
}

#[test]
fn rejects_non_solutions() {
    let (_dir, path) = write_solution("x.sln", "hello");
    assert!(matches!(load_solution(&path), Err(ProjectError::NotASolution(_))));
    let (_dir, path) = write_solution("x.slnx", "<Project />");
    assert!(matches!(load_solution(&path), Err(ProjectError::NotASolution(_))));
    let (_dir, path) = write_solution("x.slnx", "<Solution>");
    assert!(matches!(load_solution(&path), Err(ProjectError::Xml(..))));
    assert!(matches!(load_solution(Path::new("missing.sln")), Err(ProjectError::Io(..))));
}

/// `ZBUGGER_SLN=path\to\x.sln cargo test -- --ignored --nocapture real_solution`
#[test]
#[ignore]
fn real_solution() {
    let path = std::env::var("ZBUGGER_SLN").expect("set ZBUGGER_SLN");
    let start = std::time::Instant::now();
    let solution = load_solution(Path::new(&path)).unwrap();
    println!("{}: {} projects, {} folders, {} configurations, loaded in {:?}",
        solution.name, solution.projects.len(), solution.folders.len(), solution.configurations.len(), start.elapsed());
    let missing = solution.projects.iter().filter(|p| !p.path.exists()).count();
    println!("C++ projects: {}  missing on disk: {missing}  warnings: {:?}",
        solution.projects.iter().filter(|p| p.is_cpp()).count(), solution.warnings);
    fn print(solution: &Solution, folder: &SolutionTreeFolder, depth: usize) {
        for sub in &folder.folders {
            println!("{}{}/", "  ".repeat(depth), sub.name);
            print(solution, sub, depth + 1);
        }
        for &i in folder.projects.iter().take(8) {
            let p = &solution.projects[i];
            println!("{}{} ({} deps)", "  ".repeat(depth), p.name, p.dependencies.len());
        }
    }
    print(&solution, &solution.tree().root, 0);
}
