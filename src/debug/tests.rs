use super::*;

#[test]
fn splits_like_windows() {
    assert_eq!(split_arguments(""), Vec::<String>::new());
    assert_eq!(split_arguments("ShooterGame MainMenu"), ["ShooterGame", "MainMenu"]);
    assert_eq!(split_arguments(r#"  "C:\Some Dir\a.uproject"  -log "#), [r"C:\Some Dir\a.uproject", "-log"]);
    assert_eq!(split_arguments(r#"a\\b "x\"y" c\\\"d e\\"#), [r"a\\b", r#"x"y"#, r#"c\"d"#, r"e\\"]);
    assert_eq!(split_arguments(r#""""#), [""]);
    assert_eq!(split_arguments(r#"-name="a b""#), ["-name=a b"]);
}

/// Build a small C++ program with MSBuild in `dir`; returns its source file
/// and how to run it. Line 2 (zero-based) is `return a + b;` in `add`.
pub(crate) fn build_sample(dir: &std::path::Path) -> (std::path::PathBuf, crate::build::RunStep) {
    use std::time::Duration;

    use crate::build::{BuildModel, Job, JobState};
    use crate::project::{load_vcxproj, Configuration};

    // Kept to one line so the line numbers below stay put.
    std::fs::write(dir.join("sample.h"), "#include <cstdio>\n#include <cstdlib>\n").unwrap();
    let source = dir.join("main.cpp");
    std::fs::write(
        &source,
        concat!(
            "#include \"sample.h\"\n",
            "int add(int a, int b) {\n",
            "    return a + b;\n",
            "}\n",
            "int main(int argc, char** argv) {\n",
            "    int total = add(argc, 40);\n",
            "    std::printf(\"total %d %s heap=%s path=%s\\n\", total, argc > 1 ? argv[1] : \"\", std::getenv(\"_NO_DEBUG_HEAP\") ? std::getenv(\"_NO_DEBUG_HEAP\") : \"unset\", std::getenv(\"PATH\") ? \"yes\" : \"no\");\n",
            "    return total;\n",
            "}\n",
        ),
    )
    .unwrap();
    let vcxproj = dir.join("Hello.vcxproj");
    std::fs::write(
        &vcxproj,
        r#"<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <ItemGroup Label="ProjectConfigurations">
    <ProjectConfiguration Include="Debug|x64"><Configuration>Debug</Configuration><Platform>x64</Platform></ProjectConfiguration>
  </ItemGroup>
  <PropertyGroup Label="Globals"><ProjectGuid>{11111111-1111-1111-1111-111111111111}</ProjectGuid><Keyword>Win32Proj</Keyword></PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.Default.props" />
  <PropertyGroup Label="Configuration"><ConfigurationType>Application</ConfigurationType><PlatformToolset>$(DefaultPlatformToolset)</PlatformToolset><UseDebugLibraries>true</UseDebugLibraries></PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.props" />
  <PropertyGroup Condition="'$(Configuration)|$(Platform)'=='Debug|x64'"><LocalDebuggerCommandArguments>"hello world"</LocalDebuggerCommandArguments></PropertyGroup>
  <ItemGroup><ClCompile Include="main.cpp" /></ItemGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.targets" />
</Project>"#,
    )
    .unwrap();

    let model = BuildModel::for_project(&load_vcxproj(&vcxproj).unwrap());
    let config = Configuration { configuration: "Debug".into(), platform: "x64".into() };
    let plan = model.plan(&model.projects[0], &config).unwrap();
    let run = plan.run.clone().unwrap();
    let job = Job::start(plan, false, egui::Context::default());
    while job.state().is_active() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let build_output = job.events();
    assert_eq!(job.state(), JobState::Built, "{build_output:#?}");
    (source, run)
}

/// Build a small program with MSBuild and debug it with lldb-dap: stop at a
/// breakpoint, step, read the stack, continue to the end.
/// `cargo test -- --ignored --nocapture debugs_a_real_program`
#[test]
#[ignore]
fn debugs_a_real_program() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let (source, run) = build_sample(dir.path());

    let adapter = find_adapter().unwrap();
    println!("adapter: {adapter:?}");
    let launch = Launch {
        program: run.program.clone(),
        args: split_arguments(&run.arguments),
        cwd: run.cwd.clone(),
        breakpoints: vec![(source.clone(), vec![2])], // `return a + b;`
    };
    let session = DebugSession::start(&adapter, launch, egui::Context::default()).unwrap();
    let mut log = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut wait_for = |what: &str, test: &dyn Fn(&DebugEvent) -> bool| -> DebugEvent {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            queue.extend(session.events());
            while let Some(event) = queue.pop_front() {
                println!("{event:?}");
                log.push(event.clone());
                if test(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    let verified = wait_for("a verified breakpoint", &|e| {
        matches!(e, DebugEvent::Breakpoints { breakpoints, .. } if breakpoints.iter().any(|b| b.verified))
    });
    println!("verified: {verified:?}");
    let DebugEvent::Stopped { reason, frames, .. } = wait_for("the breakpoint", &|e| matches!(e, DebugEvent::Stopped { .. })) else {
        unreachable!()
    };
    assert_eq!(reason, "breakpoint");
    assert_eq!(frames[0].line, 2, "{frames:#?}");
    assert!(frames[0].name.contains("add"), "{frames:#?}");
    assert_eq!(crate::document::path_key(frames[0].path.as_ref().unwrap()), crate::document::path_key(&long_path(&source)));
    assert!(frames.iter().any(|f| f.name.contains("main")), "{frames:#?}");
    assert_eq!(session.state(), DebugState::Stopped);

    // Step out of `add`, back into `main` on line 5.
    session.send(DebugCommand::StepOut);
    let DebugEvent::Stopped { frames, .. } = wait_for("the step out", &|e| matches!(e, DebugEvent::Stopped { .. })) else {
        unreachable!()
    };
    assert!(frames[0].name.contains("main"), "{frames:#?}");
    assert_eq!(frames[0].line, 5, "{frames:#?}");
    // Then over the `printf`.
    session.send(DebugCommand::StepOver);
    let DebugEvent::Stopped { frames, .. } = wait_for("the step over", &|e| matches!(e, DebugEvent::Stopped { .. })) else {
        unreachable!()
    };
    assert_eq!(frames[0].line, 6, "{frames:#?}");

    session.send(DebugCommand::Continue);
    let DebugEvent::Ended { exit_code } = wait_for("the end", &|e| matches!(e, DebugEvent::Ended { .. })) else { unreachable!() };
    assert_eq!(exit_code, Some(42));
    assert!(log.contains(&DebugEvent::Output("total 42 hello world heap=1 path=yes".to_owned())), "{log:#?}");
}
