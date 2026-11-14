use std::path::{Path, PathBuf};

use super::*;

const VCXPROJ: &str = "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>
<Project DefaultTargets=\"Build\" xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">
  <ItemGroup Label=\"ProjectConfigurations\">
    <ProjectConfiguration Include=\"Debug|x64\">
      <Configuration>Debug</Configuration>
      <Platform>x64</Platform>
    </ProjectConfiguration>
    <ProjectConfiguration Include=\"Release|x64\">
      <Configuration>Release</Configuration>
      <Platform>x64</Platform>
    </ProjectConfiguration>
  </ItemGroup>
  <PropertyGroup Label=\"Globals\">
    <ProjectGuid>{11111111-2222-3333-4444-555555555555}</ProjectGuid>
    <ProjectName>MyGame</ProjectName>
  </PropertyGroup>
  <PropertyGroup>
    <OutDir>bin\\</OutDir>
  </PropertyGroup>
  <PropertyGroup Condition=\"'$(Configuration)|$(Platform)'=='Debug|x64'\">
    <OutDir>bin\\debug\\</OutDir>
  </PropertyGroup>
  <ImportGroup Label=\"Shared\">
    <Import Project=\"..\\shared\\Shared.vcxitems\" Label=\"Shared\" />
  </ImportGroup>
  <Import Project=\"$(VCTargetsPath)\\Microsoft.Cpp.targets\" />
  <ItemGroup>
    <ClCompile Include=\"main.cpp\" />
    <ClCompile Include=\"src\\net\\socket.cpp\" />
    <ClCompile Include=\"..\\outside\\util.cpp\" />
    <ClCompile Include=\"gen\\*.cpp\" Exclude=\"gen\\skip.cpp\" />
    <ClCompile Include=\"$(SolutionDir)src\\unknown.cpp\" />
    <ClCompile Include=\"main.cpp\" Condition=\"'$(Configuration)'=='Debug'\" />
  </ItemGroup>
  <ItemGroup>
    <ClInclude Include=\"a.h;b.h\" />
    <None Include=\"$(ProjectDir)readme.md\" />
  </ItemGroup>
  <ItemGroup>
    <ProjectReference Include=\"..\\lib\\lib.vcxproj\">
      <Project>{aaaa}</Project>
    </ProjectReference>
  </ItemGroup>
</Project>
";

const FILTERS: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<Project ToolsVersion=\"4.0\" xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">
  <ItemGroup>
    <Filter Include=\"Source Files\"><UniqueIdentifier>{1}</UniqueIdentifier></Filter>
    <Filter Include=\"Source Files\\Network\"><UniqueIdentifier>{2}</UniqueIdentifier></Filter>
    <Filter Include=\"Header Files\"><UniqueIdentifier>{3}</UniqueIdentifier></Filter>
    <Filter Include=\"Empty Folder\"><UniqueIdentifier>{4}</UniqueIdentifier></Filter>
  </ItemGroup>
  <ItemGroup>
    <ClCompile Include=\"main.cpp\"><Filter>Source Files</Filter></ClCompile>
    <ClCompile Include=\"SRC\\Net\\socket.cpp\"><Filter>Source Files\\Network</Filter></ClCompile>
    <ClCompile Include=\"gen\\a.cpp\"><Filter>Source Files</Filter></ClCompile>
  </ItemGroup>
  <ItemGroup>
    <ClInclude Include=\"a.h\"><Filter>Header Files</Filter></ClInclude>
    <ClInclude Include=\"b.h\"><Filter>Header Files</Filter></ClInclude>
  </ItemGroup>
</Project>
";

const SHARED_ITEMS: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<Project xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">
  <ItemGroup>
    <ClCompile Include=\"$(MSBuildThisFileDirectory)shared.cpp\" />
  </ItemGroup>
</Project>
";

const SHARED_FILTERS: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>
<Project xmlns=\"http://schemas.microsoft.com/developer/msbuild/2003\">
  <ItemGroup>
    <Filter Include=\"Shared Code\"><UniqueIdentifier>{9}</UniqueIdentifier></Filter>
  </ItemGroup>
  <ItemGroup>
    <ClCompile Include=\"$(MSBuildThisFileDirectory)shared.cpp\"><Filter>Shared Code</Filter></ClCompile>
  </ItemGroup>
</Project>
";

fn write(root: &Path, name: &str, contents: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Creates the sample project on disk and loads it.
fn load_sample() -> (tempfile::TempDir, PathBuf, Project) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "game/Game.vcxproj", VCXPROJ);
    write(root, "game/Game.vcxproj.filters", FILTERS);
    write(root, "shared/Shared.vcxitems", SHARED_ITEMS);
    write(root, "shared/Shared.vcxitems.filters", SHARED_FILTERS);
    for file in ["game/gen/a.cpp", "game/gen/b.cpp", "game/gen/skip.cpp", "game/gen/notes.txt"] {
        write(root, file, "");
    }
    let game = root.join("game");
    let project = load_vcxproj(&game.join("Game.vcxproj")).unwrap();
    (dir, game, project)
}

fn item<'a>(project: &'a Project, name: &str) -> &'a ProjectItem {
    project
        .items
        .iter()
        .find(|i| file_name(&i.path) == name)
        .unwrap_or_else(|| panic!("no item {name}"))
}

#[test]
fn reads_project_settings() {
    let (_dir, game, project) = load_sample();
    assert_eq!(project.name, "MyGame");
    assert_eq!(project.guid.as_deref(), Some("{11111111-2222-3333-4444-555555555555}"));
    let configs: Vec<_> = project.configurations.iter().map(Configuration::key).collect();
    assert_eq!(configs, ["Debug|x64", "Release|x64"]);
    assert_eq!(project.references, [game.parent().unwrap().join("lib").join("lib.vcxproj")]);
    assert!(project.warnings.is_empty(), "{:?}", project.warnings);
}

#[test]
fn property_lookup_respects_configuration() {
    let (_dir, _game, project) = load_sample();
    let debug = &project.configurations[0];
    let release = &project.configurations[1];
    assert_eq!(project.property("OutDir", Some(debug)), Some("bin\\debug\\"));
    assert_eq!(project.property("OutDir", Some(release)), Some("bin\\"));
    assert_eq!(project.property("OutDir", None), Some("bin\\"));
    assert_eq!(project.property("Missing", Some(debug)), None);
}

#[test]
fn resolves_item_paths() {
    let (dir, game, project) = load_sample();
    assert_eq!(item(&project, "main.cpp").path, game.join("main.cpp"));
    assert_eq!(item(&project, "main.cpp").kind, ItemKind::Compile);
    assert_eq!(item(&project, "socket.cpp").path, game.join("src").join("net").join("socket.cpp"));
    // `..` is resolved without the file having to exist.
    assert_eq!(item(&project, "util.cpp").path, dir.path().join("outside").join("util.cpp"));
    // `a;b` lists split into separate items.
    assert_eq!(item(&project, "a.h").kind, ItemKind::Include);
    assert_eq!(item(&project, "b.h").kind, ItemKind::Include);
    // $(ProjectDir) expands; unknown macros are flagged.
    assert_eq!(item(&project, "readme.md").path, game.join("readme.md"));
    assert_eq!(item(&project, "readme.md").kind, ItemKind::None);
    assert!(!item(&project, "readme.md").unresolved);
    assert!(item(&project, "unknown.cpp").unresolved);
    // The second, conditional main.cpp is a duplicate.
    assert_eq!(project.items.iter().filter(|i| file_name(&i.path) == "main.cpp").count(), 1);
}

#[test]
fn expands_wildcards_with_exclude() {
    let (_dir, _game, project) = load_sample();
    let generated: Vec<_> = project
        .items
        .iter()
        .filter(|i| i.path.parent().is_some_and(|p| p.ends_with("gen")))
        .map(|i| file_name(&i.path))
        .collect();
    assert_eq!(generated, ["a.cpp", "b.cpp"]);
}

#[test]
fn includes_shared_items_project() {
    let (dir, _game, project) = load_sample();
    let shared = item(&project, "shared.cpp");
    assert_eq!(shared.path, dir.path().join("shared").join("shared.cpp"));
    assert_eq!(shared.filter.as_deref(), Some("Shared Code"));
}

#[test]
fn builds_filter_tree() {
    let (_dir, _game, project) = load_sample();
    let tree = project.tree();
    let names = |folder: &Folder| -> Vec<String> {
        folder.files.iter().map(|&i| file_name(&project.items[i].path)).collect()
    };

    assert_eq!(tree.root.name, "MyGame");
    let folder_names: Vec<_> = tree.root.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(folder_names, ["Empty Folder", "Header Files", "Shared Code", "Source Files"]);

    let source = &tree.root.folders[3];
    assert_eq!(names(source), ["a.cpp", "main.cpp"]);
    assert_eq!(source.folders.len(), 1);
    assert_eq!(source.folders[0].name, "Network");
    assert_eq!(source.folders[0].path, "Source Files\\Network");
    // Filters match paths case-insensitively (SRC\Net\socket.cpp).
    assert_eq!(names(&source.folders[0]), ["socket.cpp"]);
    assert_eq!(names(&tree.root.folders[1]), ["a.h", "b.h"]);
    assert!(tree.root.folders[0].files.is_empty());
    // Files without a filter sit at the root, sorted.
    assert_eq!(names(&tree.root), ["b.cpp", "readme.md", "unknown.cpp", "util.cpp"]);
}

#[test]
fn without_filters_file_everything_is_at_root() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "p.vcxproj", "<Project><ItemGroup><ClCompile Include=\"b.cpp\" /><ClCompile Include=\"a.cpp\" /></ItemGroup></Project>");
    let project = load_vcxproj(&dir.path().join("p.vcxproj")).unwrap();
    assert_eq!(project.name, "p");
    let tree = project.tree();
    assert!(tree.root.folders.is_empty());
    let names: Vec<_> = tree.root.files.iter().map(|&i| file_name(&project.items[i].path)).collect();
    assert_eq!(names, ["a.cpp", "b.cpp"]);
}

#[test]
fn reports_bad_files() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "bad.vcxproj", "<Project><ItemGroup>");
    write(dir.path(), "other.vcxproj", "<Solution />");
    assert!(matches!(load_vcxproj(&dir.path().join("missing.vcxproj")), Err(ProjectError::Io(..))));
    assert!(matches!(load_vcxproj(&dir.path().join("bad.vcxproj")), Err(ProjectError::Xml(..))));
    assert!(matches!(load_vcxproj(&dir.path().join("other.vcxproj")), Err(ProjectError::NotAProject(..))));
}

#[test]
fn reads_utf16_files() {
    let dir = tempfile::tempdir().unwrap();
    let text = "<Project><ItemGroup><ClCompile Include=\"é.cpp\" /></ItemGroup></Project>";
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(dir.path().join("u.vcxproj"), bytes).unwrap();
    let project = load_vcxproj(&dir.path().join("u.vcxproj")).unwrap();
    assert_eq!(file_name(&project.items[0].path), "é.cpp");
}

/// Load a real project: `ZBUGGER_VCXPROJ=path\to\x.vcxproj cargo test -- --ignored --nocapture real_project`
#[test]
#[ignore]
fn real_project() {
    let path = std::env::var("ZBUGGER_VCXPROJ").expect("set ZBUGGER_VCXPROJ");
    let start = std::time::Instant::now();
    let project = load_vcxproj(Path::new(&path)).unwrap();
    let elapsed = start.elapsed();
    let tree = project.tree();
    let missing = project.items.iter().filter(|i| !i.unresolved && !i.path.exists()).count();
    let unresolved = project.items.iter().filter(|i| i.unresolved).count();
    let filtered = project.items.iter().filter(|i| i.filter.is_some()).count();
    println!("name: {}  guid: {:?}", project.name, project.guid);
    println!("configurations: {:?}", project.configurations.iter().map(Configuration::key).collect::<Vec<_>>());
    println!("items: {} ({filtered} in filters, {missing} missing on disk, {unresolved} with unresolved macros)", project.items.len());
    println!("filters: {}  top-level folders: {:?}", project.filters.len(), tree.root.folders.iter().map(|f| &f.name).collect::<Vec<_>>());
    println!("references: {}  warnings: {:?}", project.references.len(), project.warnings);
    println!("loaded in {elapsed:?}");
    for item in project.items.iter().filter(|i| !i.unresolved && !i.path.exists()).take(5) {
        println!("  missing: {} (Include=\"{}\")", item.path.display(), item.include);
    }
    for item in project.items.iter().filter(|i| i.filter.is_none()).take(5) {
        println!("  no filter: {}", item.path.display());
    }
}
