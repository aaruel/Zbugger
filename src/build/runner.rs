//! Running a [`LaunchPlan`]: build, then (optionally) run, on a background
//! thread, streaming output lines back.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use super::{BuildStep, LaunchPlan};

/// Don't open a console window for child processes on Windows.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobEvent {
    /// A line of output from the build or the program.
    Output(String),
    /// A message from Zbugger itself ("Build succeeded", ...).
    Status(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Building,
    Running,
    /// Built successfully, nothing run.
    Built,
    Failed,
    Stopped,
    /// The program exited, with its exit code if it had one.
    Exited(Option<i32>),
}

impl JobState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Building | Self::Running)
    }
}

/// A running build/run.
pub struct Job {
    receiver: Receiver<JobEvent>,
    shared: Arc<Shared>,
    plan: LaunchPlan,
}

struct Shared {
    state: Mutex<JobState>,
    /// The process currently running, so Stop can kill it.
    pid: Mutex<Option<u32>>,
    stopped: AtomicBool,
}

impl Job {
    /// Build per `plan`, then run its program if `run` is set.
    pub fn start(plan: LaunchPlan, run: bool, ctx: egui::Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let shared =
            Arc::new(Shared { state: Mutex::new(JobState::Building), pid: Mutex::new(None), stopped: AtomicBool::new(false) });
        let worker = shared.clone();
        let job_plan = plan.clone();
        std::thread::Builder::new()
            .name("build-and-run".into())
            .spawn(move || {
                let state = execute(&job_plan, run, &sender, &worker, &ctx);
                *worker.state.lock().expect("not poisoned") = state;
                ctx.request_repaint();
            })
            .expect("can spawn the build thread");
        Self { receiver, shared, plan }
    }

    /// What this job builds and runs.
    pub fn plan(&self) -> &LaunchPlan {
        &self.plan
    }

    /// Events since the last call.
    pub fn events(&self) -> Vec<JobEvent> {
        self.receiver.try_iter().collect()
    }

    pub fn state(&self) -> JobState {
        *self.shared.state.lock().expect("not poisoned")
    }

    /// Stop the build or program, including any processes it started.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(pid) = *self.shared.pid.lock().expect("not poisoned") {
            kill_tree(pid);
        }
    }
}

fn execute(plan: &LaunchPlan, run: bool, sender: &Sender<JobEvent>, shared: &Shared, ctx: &egui::Context) -> JobState {
    let status = |text: String| {
        let _ = sender.send(JobEvent::Status(text));
        ctx.request_repaint();
    };
    for note in &plan.notes {
        status(format!("Note: {note}"));
    }

    if let Some(build) = &plan.build {
        status(format!("Building {} ({})", plan.project, plan.configuration.key()));
        let command = match build_command(build) {
            Ok(command) => command,
            Err(message) => {
                status(message);
                return JobState::Failed;
            }
        };
        let result = run_streaming(command, sender, shared, ctx);
        if shared.stopped.load(Ordering::Relaxed) {
            status("Build stopped".to_owned());
            return JobState::Stopped;
        }
        match result {
            Ok(Some(0)) => status("Build succeeded".to_owned()),
            Ok(code) => {
                status(format!("Build failed (exit code {})", code.map_or("unknown".to_owned(), |c| c.to_string())));
                return JobState::Failed;
            }
            Err(err) => {
                status(format!("Couldn't start the build: {err}"));
                return JobState::Failed;
            }
        }
    }
    if !run {
        return JobState::Built;
    }

    let Some(step) = &plan.run else {
        status("Nothing to run".to_owned());
        return JobState::Built;
    };
    if !step.program.is_file() {
        status(format!("Program not found: {}", step.program.display()));
        return JobState::Failed;
    }
    *shared.state.lock().expect("not poisoned") = JobState::Running;
    status(format!("Running {} {}", step.program.display(), step.arguments).trim_end().to_owned());
    let mut command = Command::new(&step.program);
    if !step.arguments.trim().is_empty() {
        raw_args(&mut command, &step.arguments);
    }
    let cwd = if step.cwd.is_dir() { step.cwd.clone() } else { step.program.parent().map(PathBuf::from).unwrap_or_default() };
    command.current_dir(cwd);
    let result = run_streaming(command, sender, shared, ctx);
    if shared.stopped.load(Ordering::Relaxed) {
        status("Stopped".to_owned());
        return JobState::Stopped;
    }
    match result {
        Ok(code) => {
            status(format!("{} exited with code {}", plan.project, code.map_or("unknown".to_owned(), |c| c.to_string())));
            JobState::Exited(code)
        }
        Err(err) => {
            status(format!("Couldn't start {}: {err}", step.program.display()));
            JobState::Failed
        }
    }
}

fn build_command(build: &BuildStep) -> Result<Command, String> {
    match build {
        BuildStep::Shell { command_line, cwd } => {
            let mut command = Command::new("cmd.exe");
            // `/S /C "..."` runs the line exactly as written, quotes included.
            raw_args(&mut command, &format!("/S /C \"{command_line}\""));
            command.current_dir(cwd);
            Ok(command)
        }
        BuildStep::MsBuild { project, configuration, solution_dir } => {
            let msbuild = find_msbuild().ok_or("MSBuild wasn't found (is Visual Studio installed?)")?;
            let mut command = Command::new(msbuild);
            command
                .arg(project)
                .arg(format!("/p:Configuration={}", configuration.configuration))
                .arg(format!("/p:Platform={}", configuration.platform))
                .arg(format!("/p:SolutionDir={}\\", solution_dir.display()))
                .args(["/m", "/nologo", "/v:minimal"]);
            if let Some(dir) = project.parent() {
                command.current_dir(dir);
            }
            Ok(command)
        }
    }
}

#[cfg(windows)]
fn raw_args(command: &mut Command, args: &str) {
    use std::os::windows::process::CommandExt;
    command.raw_arg(args);
}

#[cfg(not(windows))]
fn raw_args(command: &mut Command, args: &str) {
    command.args(args.split_whitespace());
}

/// Run `command`, sending each line of its output as it arrives. Returns its
/// exit code (`None` if it was killed).
fn run_streaming(mut command: Command, sender: &Sender<JobEvent>, shared: &Shared, ctx: &egui::Context) -> std::io::Result<Option<i32>> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    *shared.pid.lock().expect("not poisoned") = Some(child.id());
    if shared.stopped.load(Ordering::Relaxed) {
        kill_tree(child.id()); // Stop was pressed while starting
    }
    let readers: Vec<_> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)]
        .into_iter()
        .flatten()
        .map(|stream| {
            let sender = sender.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let mut line = Vec::new();
                while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                    let text = String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_owned();
                    if sender.send(JobEvent::Output(text)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                    line.clear();
                }
            })
        })
        .collect();
    let status = child.wait();
    for reader in readers {
        let _ = reader.join();
    }
    *shared.pid.lock().expect("not poisoned") = None;
    Ok(status?.code())
}

/// Kill a process and everything it started (build tools spawn compilers).
pub(crate) fn kill_tree(pid: u32) {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command.status();
}

/// MSBuild from the newest Visual Studio, found with Microsoft's `vswhere`.
pub fn find_msbuild() -> Option<PathBuf> {
    static MSBUILD: OnceLock<Option<PathBuf>> = OnceLock::new();
    MSBUILD
        .get_or_init(|| {
            let program_files = std::env::var_os("ProgramFiles(x86)")?;
            let vswhere = PathBuf::from(program_files).join("Microsoft Visual Studio/Installer/vswhere.exe");
            let mut command = Command::new(vswhere);
            command.args(["-latest", "-products", "*", "-requires", "Microsoft.Component.MSBuild", "-find", r"MSBuild\**\Bin\MSBuild.exe"]);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(CREATE_NO_WINDOW);
            }
            let output = command.output().ok()?;
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines().map(str::trim).find(|l| !l.is_empty()).map(PathBuf::from).filter(|p| p.is_file())
        })
        .clone()
}
