use std::path::{Path, PathBuf};

use super::*;
use crate::project::{load_vcxproj, Configuration};

fn config(key: &str) -> Configuration {
    let (configuration, platform) = key.split_once('|').unwrap();
    Configuration { configuration: configuration.to_owned(), platform: platform.to_owned() }
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

/// An Unreal-style makefile project, laid out like the engine's generated ones.
const UNREAL_VCXPROJ: &str = r#"<Project>
  <ItemGroup Label="ProjectConfigurations">
    <ProjectConfiguration Include="Development_Editor|x64"><Configuration>Development_Editor</Configuration><Platform>x64</Platform></ProjectConfiguration>
  </ItemGroup>
  <Import Project="UECommon.props" />
  <PropertyGroup Condition="'$(Configuration)|$(Platform)'=='Development_Editor|x64'">
    <NMakeBuildCommandLine>$(BuildBatchScript) -Target="GameEditor Win64 Development -Project=\"$(SolutionDir)Game\Game.uproject\"" -WaitMutex</NMakeBuildCommandLine>
    <NMakeOutput>..\..\Binaries\Win64\UnrealEditor.exe</NMakeOutput>
  </PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.targets" />
</Project>"#;

const UECOMMON_PROPS: &str = r#"<Project>
  <PropertyGroup>
    <BuildBatchScript>..\..\Build\BatchFiles\Build.bat</BuildBatchScript>
  </PropertyGroup>
</Project>"#;

const UNREAL_USER: &str = r#"<Project>
  <PropertyGroup Condition="'$(Configuration)|$(Platform)'=='Development_Editor|x64'">
    <LocalDebuggerCommandArguments>"$(SolutionDir)Game\Game.uproject" -skipcompile</LocalDebuggerCommandArguments>
  </PropertyGroup>
</Project>"#;

#[test]
fn plans_an_unreal_makefile_project() {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().join("Engine/Intermediate/ProjectFiles");
    let vcxproj = write(&files, "Game.vcxproj", UNREAL_VCXPROJ);
    write(&files, "UECommon.props", UECOMMON_PROPS);
    write(&files, "Game.vcxproj.user", UNREAL_USER);
    let project = load_vcxproj(&vcxproj).unwrap();
    let mapping = ConfigurationMapping {
        solution: "Development Editor|Win64".to_owned(),
        project: "Development_Editor|x64".to_owned(),
        build: true,
    };
    let solution = dir.path().join("UE5.sln");
    let model = BuildModel::for_solution(
        &solution,
        vec![config("Development Editor|Win64")],
        vec![BuildProject::from_project(&project, vec![mapping])],
    );
    let root = format!("{}\\", dir.path().display());

    let plan = model.plan(&model.projects[0], &config("Development Editor|Win64")).unwrap();
    assert_eq!(plan.configuration.key(), "Development_Editor|x64");
    assert_eq!(
        plan.build,
        Some(BuildStep::Shell {
            command_line: format!(r#"..\..\Build\BatchFiles\Build.bat -Target="GameEditor Win64 Development -Project=\"{root}Game\Game.uproject\"" -WaitMutex"#),
            cwd: files.clone(),
        })
    );
    let run = plan.run.unwrap();
    assert_eq!(run.program, dir.path().join(r"Engine\Binaries\Win64\UnrealEditor.exe"));
    assert_eq!(run.arguments, format!("\"{root}Game\\Game.uproject\" -skipcompile"));
    assert_eq!(run.cwd, files);
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    // A game project ranks first as the startup project.
    assert_eq!(model.projects[0].startup_rank(), 3);

    // A configuration the project doesn't have.
    assert!(model.plan(&model.projects[0], &config("Shipping|Win64")).is_err());
}

#[test]
fn plans_a_regular_msbuild_project_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let vcxproj = write(
        dir.path(),
        "app/Hello.vcxproj",
        r#"<Project>
  <ItemGroup Label="ProjectConfigurations">
    <ProjectConfiguration Include="Debug|x64" />
    <ProjectConfiguration Include="Release|Win32" />
  </ItemGroup>
  <PropertyGroup><ConfigurationType>Application</ConfigurationType></PropertyGroup>
  <PropertyGroup Condition="'$(Configuration)|$(Platform)'=='Debug|x64'">
    <LocalDebuggerCommandArguments>--verbose $(UnknownThing)</LocalDebuggerCommandArguments>
    <LocalDebuggerWorkingDirectory>$(ProjectDir)data</LocalDebuggerWorkingDirectory>
  </PropertyGroup>
</Project>"#,
    );
    let project = load_vcxproj(&vcxproj).unwrap();
    let model = BuildModel::for_project(&project);

    // Debug|x64: VS's default OutDir is $(SolutionDir)$(Platform)\$(Configuration)\.
    let plan = model.plan(&model.projects[0], &config("Debug|x64")).unwrap();
    assert!(matches!(plan.build, Some(BuildStep::MsBuild { .. })));
    let run = plan.run.unwrap();
    assert_eq!(run.program, dir.path().join(r"app\x64\Debug\Hello.exe"));
    assert_eq!(run.arguments, "--verbose ");
    assert_eq!(run.cwd, dir.path().join(r"app\data"));
    assert_eq!(plan.notes, ["$(UnknownThing) isn't defined; it was left empty"]);

    // Win32 has no platform folder in the default OutDir.
    let plan = model.plan(&model.projects[0], &config("Release|Win32")).unwrap();
    assert_eq!(plan.run.unwrap().program, dir.path().join(r"app\Release\Hello.exe"));
    assert_eq!(model.configuration_names(), (vec!["Debug".to_owned(), "Release".to_owned()], vec!["x64".to_owned(), "Win32".to_owned()]));
}

#[test]
fn libraries_have_nothing_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let vcxproj = write(
        dir.path(),
        "Lib.vcxproj",
        r#"<Project>
  <ItemGroup Label="ProjectConfigurations"><ProjectConfiguration Include="Debug|x64" /></ItemGroup>
  <PropertyGroup><ConfigurationType>StaticLibrary</ConfigurationType></PropertyGroup>
</Project>"#,
    );
    let model = BuildModel::for_project(&load_vcxproj(&vcxproj).unwrap());
    let plan = model.plan(&model.projects[0], &config("Debug|x64")).unwrap();
    assert!(plan.run.is_none());
    assert_eq!(model.projects[0].startup_rank(), 0);
}

#[test]
fn expands_macros() {
    let groups = vec![crate::project::PropertyGroup {
        label: None,
        condition: None,
        properties: vec![("A".into(), "$(B)-x".into()), ("B".into(), "$(Configuration)".into()), ("Loop".into(), "$(Loop)".into())],
    }];
    let config = config("Debug|x64");
    let eval = Evaluator::new(&groups, &config, Path::new(r"C:\s\p\P.vcxproj"), Path::new(r"C:\s\S.sln"));
    assert_eq!(eval.expand("$(A)").0, "Debug-x");
    assert_eq!(eval.expand("$(SolutionDir)|$(ProjectDir)|$(ProjectName)").0, r"C:\s\|C:\s\p\|P");
    assert_eq!(eval.expand("$([System.IO.Path]::Combine(a))").0, "$([System.IO.Path]::Combine(a))");
    // A property that refers to itself stops rather than looping forever.
    assert!(eval.expand("$(Loop)").0.len() < 100);
}

/// Really build and run a tiny C++ program with MSBuild (needs Visual Studio's
/// C++ tools): `cargo test -- --ignored --nocapture builds_and_runs_a_real_project`
#[test]
#[ignore]
fn builds_and_runs_a_real_project() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "main.cpp", "#include <cstdio>\nint main(int argc, char** argv) { std::printf(\"hello from zbugger %s\\n\", argc > 1 ? argv[1] : \"\"); return 7; }\n");
    let vcxproj = write(
        dir.path(),
        "Hello.vcxproj",
        r#"<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <ItemGroup Label="ProjectConfigurations">
    <ProjectConfiguration Include="Debug|x64"><Configuration>Debug</Configuration><Platform>x64</Platform></ProjectConfiguration>
  </ItemGroup>
  <PropertyGroup Label="Globals"><ProjectGuid>{11111111-1111-1111-1111-111111111111}</ProjectGuid><Keyword>Win32Proj</Keyword></PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.Default.props" />
  <PropertyGroup Label="Configuration"><ConfigurationType>Application</ConfigurationType><PlatformToolset>$(DefaultPlatformToolset)</PlatformToolset></PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.props" />
  <PropertyGroup Condition="'$(Configuration)|$(Platform)'=='Debug|x64'"><LocalDebuggerCommandArguments>world</LocalDebuggerCommandArguments></PropertyGroup>
  <ItemGroup><ClCompile Include="main.cpp" /></ItemGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.targets" />
</Project>"#,
    );
    println!("MSBuild: {:?}", find_msbuild());
    let model = BuildModel::for_project(&load_vcxproj(&vcxproj).unwrap());
    let plan = model.plan(&model.projects[0], &config("Debug|x64")).unwrap();
    let job = Job::start(plan, true, egui::Context::default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut output = Vec::new();
    while job.state().is_active() && std::time::Instant::now() < deadline {
        output.extend(job.events());
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    output.extend(job.events());
    for event in &output {
        println!("{event:?}");
    }
    assert_eq!(job.state(), JobState::Exited(Some(7)));
    assert!(output.contains(&JobEvent::Output("hello from zbugger world".to_owned())));
    assert!(output.contains(&JobEvent::Status("Build succeeded".to_owned())));
}

/// Show what Play would do for a real solution (resolves only; runs nothing):
/// `ZBUGGER_SLN=x.sln ZBUGGER_CONFIG="Development Editor|Win64" cargo test --release -- --ignored --nocapture real_solution_plan`
#[test]
#[ignore]
fn real_solution_plan() {
    let solution = std::env::var("ZBUGGER_SLN").expect("set ZBUGGER_SLN");
    let view = crate::project_view::ProjectView::open(Path::new(&solution)).unwrap();
    let model = view.build_model();
    let (names, platforms) = model.configuration_names();
    println!("{} configurations ({} names x {} platforms), {} C++ projects", model.configurations.len(), names.len(), platforms.len(), model.projects.len());
    let startup = model.default_startup_project().expect("a startup project");
    println!("default startup project: {} (rank {})", startup.name, startup.startup_rank());
    let key = std::env::var("ZBUGGER_CONFIG").unwrap_or_else(|_| model.configurations[0].key());
    let plan = model.plan(startup, &config(&key)).unwrap();
    println!("{key} -> project configuration {}", plan.configuration.key());
    println!("build: {:?}", plan.build);
    if let Some(run) = &plan.run {
        println!("run: {} (exists: {})", run.program.display(), run.program.is_file());
        println!("arguments: {}", run.arguments);
        println!("working directory: {}", run.cwd.display());
    }
    println!("notes: {:?}", plan.notes);
}

/// CMake puts the condition on each property instead of on the group.
#[test]
fn per_property_conditions_pick_the_right_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let vcxproj = write(
        dir.path(),
        "unit.vcxproj",
        r#"<Project>
  <ItemGroup Label="ProjectConfigurations">
    <ProjectConfiguration Include="Debug|x64" /><ProjectConfiguration Include="Release|x64" />
  </ItemGroup>
  <PropertyGroup><ConfigurationType>Application</ConfigurationType></PropertyGroup>
  <PropertyGroup>
    <OutDir Condition="'$(Configuration)|$(Platform)'=='Debug|x64'">bin\Debug\</OutDir>
    <TargetName>unit</TargetName>
    <OutDir Condition="'$(Configuration)|$(Platform)'=='Release|x64'">bin\Release\</OutDir>
  </PropertyGroup>
</Project>"#,
    );
    let model = BuildModel::for_project(&load_vcxproj(&vcxproj).unwrap());
    for name in ["Debug", "Release"] {
        let plan = model.plan(&model.projects[0], &config(&format!("{name}|x64"))).unwrap();
        assert_eq!(plan.run.unwrap().program, dir.path().join("bin").join(name).join("unit.exe"), "{name}");
    }
}
