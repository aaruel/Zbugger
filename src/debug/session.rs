//! A debugging session: one `lldb-dap` process driving one program.
//!
//! A worker thread owns the protocol. The UI sends [`DebugCommand`]s and
//! picks up [`DebugEvent`]s each frame; neither side ever blocks the other.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::adapter::Adapter;
use super::dap::{read_message, request, write_message};
use crate::document::path_key;

/// What the UI asks the debugger to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugCommand {
    Continue,
    Pause,
    StepOver,
    StepIn,
    StepOut,
    /// End the session, killing the program.
    Stop,
    /// Replace the breakpoints in `path` (zero-based lines).
    SetBreakpoints { path: PathBuf, lines: Vec<usize> },
}

/// One entry of the call stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub name: String,
    pub path: Option<PathBuf>,
    /// Zero-based.
    pub line: usize,
    /// Zero-based.
    pub column: usize,
}

/// Where a requested breakpoint ended up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointStatus {
    /// The zero-based line that was asked for.
    pub requested: usize,
    /// Whether the debugger could place it (the code is loaded).
    pub verified: bool,
    /// The zero-based line it's actually on, if the debugger moved it.
    pub line: Option<usize>,
}

/// What happened in the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugEvent {
    /// A line the program printed.
    Output(String),
    /// A message from the debugger or Zbugger.
    Status(String),
    /// The program stopped; `frames[0]` is where.
    Stopped { reason: String, description: Option<String>, frames: Vec<Frame> },
    /// The program is running again.
    Continued,
    /// Placement of the breakpoints in `path`, in the order they were set.
    Breakpoints { path: PathBuf, breakpoints: Vec<BreakpointStatus> },
    /// The session is over.
    Ended { exit_code: Option<i64> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugState {
    Starting,
    Running,
    Stopped,
    Ended,
}

impl DebugState {
    pub fn is_active(self) -> bool {
        self != Self::Ended
    }
}

/// What to debug.
#[derive(Clone, Debug)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Breakpoints to set before the program starts: file and zero-based lines.
    pub breakpoints: Vec<(PathBuf, Vec<usize>)>,
}

pub struct DebugSession {
    input: Sender<Input>,
    events: Receiver<DebugEvent>,
    state: Arc<Mutex<DebugState>>,
}

enum Input {
    Message(Value),
    Command(DebugCommand),
    /// A line the adapter wrote to stderr.
    Log(String),
    /// The adapter's stdout closed (it exited).
    Closed,
}

impl DebugSession {
    pub fn start(adapter: &Adapter, launch: Launch, ctx: egui::Context) -> Result<Self, String> {
        let mut command = adapter.command();
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child =
            command.spawn().map_err(|err| format!("Couldn't start {}: {err}", adapter.program.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let (input, inputs) = mpsc::channel();
        let wake = |ctx: &egui::Context, sender: &Sender<Input>, message: Input| {
            let ok = sender.send(message).is_ok();
            ctx.request_repaint();
            ok
        };
        {
            let (sender, ctx) = (input.clone(), ctx.clone());
            std::thread::Builder::new()
                .name("dap-reader".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    while let Ok(Some(message)) = read_message(&mut reader) {
                        if !wake(&ctx, &sender, Input::Message(message)) {
                            return;
                        }
                    }
                    wake(&ctx, &sender, Input::Closed);
                })
                .expect("can spawn a thread");
        }
        {
            let (sender, ctx) = (input.clone(), ctx.clone());
            std::thread::Builder::new()
                .name("dap-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if !wake(&ctx, &sender, Input::Log(line)) {
                            return;
                        }
                    }
                })
                .expect("can spawn a thread");
        }

        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(DebugState::Starting));
        let worker = Worker {
            stdin,
            seq: 0,
            pending: HashMap::new(),
            launch,
            thread_id: None,
            configured: false,
            stopping: false,
            finished: false,
            exit_code: None,
            breakpoint_ids: HashMap::new(),
            partial: HashMap::new(),
            events,
            state: state.clone(),
            ctx,
        };
        std::thread::Builder::new()
            .name("debug-session".into())
            .spawn(move || worker.run(inputs, child))
            .expect("can spawn a thread");
        Ok(Self { input, events: receiver, state })
    }

    pub fn send(&self, command: DebugCommand) {
        let _ = self.input.send(Input::Command(command));
    }

    /// Events since the last call.
    pub fn events(&self) -> Vec<DebugEvent> {
        self.events.try_iter().collect()
    }

    pub fn state(&self) -> DebugState {
        *self.state.lock().expect("not poisoned")
    }

    /// End the session and wait (up to `timeout`) for the program and the
    /// adapter to exit.
    pub fn stop_and_wait(self, timeout: Duration) {
        self.send(DebugCommand::Stop);
        let deadline = Instant::now() + timeout;
        while self.state().is_active() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for DebugSession {
    /// Don't leave the program running under a debugger nobody is watching.
    fn drop(&mut self) {
        self.send(DebugCommand::Stop);
    }
}

/// What a request was for, to handle its response.
enum Pending {
    Initialize,
    Launch,
    SetBreakpoints { path: PathBuf, lines: Vec<usize> },
    StackTrace { reason: String, description: Option<String> },
    Resume,
    Disconnect,
    Other,
}

struct Worker {
    stdin: ChildStdin,
    seq: i64,
    pending: HashMap<i64, (String, Pending)>,
    launch: Launch,
    /// The thread that last stopped, which stepping applies to.
    thread_id: Option<i64>,
    /// Set once the adapter is ready for breakpoints.
    configured: bool,
    stopping: bool,
    finished: bool,
    exit_code: Option<i64>,
    /// Breakpoint id → file and index, to apply `breakpoint` events.
    breakpoint_ids: HashMap<i64, (PathBuf, usize)>,
    /// Program output not yet ended by a line break, per category.
    partial: HashMap<String, String>,
    events: Sender<DebugEvent>,
    state: Arc<Mutex<DebugState>>,
    ctx: egui::Context,
}

/// `path` with its real, full-length names. Debug info records full paths,
/// and LLDB doesn't match `C:\Users\GROVEU~1\...` against them.
pub fn long_path(path: &std::path::Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(full) => {
            let text = full.to_string_lossy();
            match text.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
                _ => full.clone(),
            }
        }
        Err(_) => path.to_path_buf(),
    }
}

/// Set `ZBUGGER_DAP_TRACE=1` to print every protocol message to stderr.
fn trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("ZBUGGER_DAP_TRACE").is_some())
}

/// How long the adapter gets to end the program and exit after Stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

impl Worker {
    fn run(mut self, inputs: Receiver<Input>, mut child: Child) {
        self.request(
            "initialize",
            json!({
                "clientID": "zbugger",
                "clientName": "Zbugger",
                "adapterID": "lldb-dap",
                "pathFormat": "path",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "supportsRunInTerminalRequest": false,
            }),
            Pending::Initialize,
        );
        let mut deadline: Option<Instant> = None;
        while !self.finished {
            let next = match deadline {
                Some(at) => inputs.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => inputs.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            if trace() {
                match &next {
                    Ok(Input::Message(message)) => eprintln!("dap <- {message}"),
                    Ok(Input::Log(line)) => eprintln!("dap stderr: {line}"),
                    Ok(Input::Closed) => eprintln!("dap: adapter closed"),
                    _ => {}
                }
            }
            match next {
                Ok(Input::Message(message)) => self.message(message),
                Ok(Input::Command(DebugCommand::Stop)) => {
                    if self.stopping {
                        break; // asked twice: don't wait any longer
                    }
                    self.stop(true);
                    deadline = Some(Instant::now() + STOP_TIMEOUT);
                }
                Ok(Input::Command(command)) => self.command(command),
                Ok(Input::Log(line)) => self.emit(DebugEvent::Status(format!("lldb-dap: {line}"))),
                Ok(Input::Closed) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    self.emit(DebugEvent::Status("The debugger didn't stop in time; killing it".to_owned()));
                    break;
                }
            }
            if self.stopping && deadline.is_none() {
                deadline = Some(Instant::now() + STOP_TIMEOUT);
            }
        }

        // Flush output that didn't end with a line break.
        for (_, text) in std::mem::take(&mut self.partial) {
            if !text.is_empty() {
                self.emit(DebugEvent::Output(text));
            }
        }
        let wait_until = Instant::now() + Duration::from_secs(1);
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < wait_until {
            std::thread::sleep(Duration::from_millis(20));
        }
        if matches!(child.try_wait(), Ok(None)) {
            crate::build::kill_tree(child.id());
        }
        let _ = child.wait();
        self.set_state(DebugState::Ended);
        self.emit(DebugEvent::Ended { exit_code: self.exit_code });
    }

    fn emit(&self, event: DebugEvent) {
        let _ = self.events.send(event);
        self.ctx.request_repaint();
    }

    fn set_state(&self, state: DebugState) {
        *self.state.lock().expect("not poisoned") = state;
        self.ctx.request_repaint();
    }

    fn state(&self) -> DebugState {
        *self.state.lock().expect("not poisoned")
    }

    fn request(&mut self, command: &str, arguments: Value, pending: Pending) {
        self.seq += 1;
        if trace() {
            eprintln!("dap -> {command} {arguments}");
        }
        if let Err(err) = write_message(&mut self.stdin, &request(self.seq, command, arguments)) {
            self.emit(DebugEvent::Status(format!("Couldn't talk to the debugger: {err}")));
            self.finished = true;
            return;
        }
        self.pending.insert(self.seq, (command.to_owned(), pending));
    }

    /// End the session: `disconnect` kills the program (if `terminate`) and
    /// lets the adapter exit.
    fn stop(&mut self, terminate: bool) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.request("disconnect", json!({ "terminateDebuggee": terminate }), Pending::Disconnect);
    }

    fn command(&mut self, command: DebugCommand) {
        let thread = json!({ "threadId": self.thread_id.unwrap_or(0) });
        let stopped = self.state() == DebugState::Stopped;
        match command {
            DebugCommand::Continue if stopped => self.resume("continue", thread),
            DebugCommand::StepOver if stopped => self.resume("next", thread),
            DebugCommand::StepIn if stopped => self.resume("stepIn", thread),
            DebugCommand::StepOut if stopped => self.resume("stepOut", thread),
            DebugCommand::Pause if self.state() == DebugState::Running => self.request("pause", thread, Pending::Other),
            DebugCommand::SetBreakpoints { path, lines } => {
                let key = path_key(&path);
                self.launch.breakpoints.retain(|(p, _)| path_key(p) != key);
                self.launch.breakpoints.push((path.clone(), lines.clone()));
                if self.configured {
                    self.set_breakpoints(path, lines);
                }
            }
            _ => {}
        }
    }

    /// Continue or step. The program is running from here until the next
    /// `stopped` event, which can only come after this request.
    fn resume(&mut self, command: &str, thread: Value) {
        self.request(command, thread, Pending::Resume);
        self.set_state(DebugState::Running);
        self.emit(DebugEvent::Continued);
    }

    fn set_breakpoints(&mut self, path: PathBuf, lines: Vec<usize>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let breakpoints: Vec<Value> = lines.iter().map(|line| json!({ "line": line + 1 })).collect();
        self.request(
            "setBreakpoints",
            json!({
                "source": { "path": long_path(&path), "name": name },
                "breakpoints": breakpoints,
                "sourceModified": false,
            }),
            Pending::SetBreakpoints { path, lines },
        );
    }

    fn message(&mut self, message: Value) {
        match message["type"].as_str() {
            Some("response") => {
                let Some((command, pending)) = message["request_seq"].as_i64().and_then(|seq| self.pending.remove(&seq))
                else {
                    return;
                };
                if message["success"].as_bool() == Some(true) {
                    self.response(pending, &message["body"]);
                } else {
                    let reason = message["body"]["error"]["format"]
                        .as_str()
                        .or(message["message"].as_str())
                        .unwrap_or("no reason given");
                    self.emit(DebugEvent::Status(format!("Debugger: {command} failed: {reason}")));
                    if matches!(pending, Pending::Initialize | Pending::Launch) {
                        self.stop(true);
                    }
                    match pending {
                        Pending::Disconnect => self.finished = true,
                        // Still where it was.
                        Pending::Resume => self.set_state(DebugState::Stopped),
                        _ => {}
                    }
                }
            }
            Some("event") => {
                let event = message["event"].as_str().unwrap_or("").to_owned();
                self.event(&event, &message["body"]);
            }
            Some("request") => {
                // Reverse requests (runInTerminal...) aren't supported.
                self.seq += 1;
                let reply = json!({
                    "seq": self.seq,
                    "type": "response",
                    "request_seq": message["seq"],
                    "command": message["command"],
                    "success": false,
                    "message": "not supported",
                });
                let _ = write_message(&mut self.stdin, &reply);
            }
            _ => {}
        }
    }

    fn response(&mut self, pending: Pending, body: &Value) {
        match pending {
            Pending::Initialize => {
                let launch = &self.launch;
                let arguments = json!({
                    "program": launch.program,
                    "args": launch.args,
                    "cwd": launch.cwd,
                    "stopOnEntry": false,
                    // Windows gives a program started under a debugger its
                    // slow, checking debug heap; allocation-heavy programs
                    // (Unreal) crawl. Visual Studio turns it off the same way.
                    // Added to the inherited environment, not replacing it.
                    "env": { "_NO_DEBUG_HEAP": "1" },
                });
                self.emit(DebugEvent::Status(format!("Debugging {}", launch.program.display())));
                self.request("launch", arguments, Pending::Launch);
            }
            Pending::Launch => {
                if self.state() == DebugState::Starting {
                    self.set_state(DebugState::Running);
                }
            }
            Pending::SetBreakpoints { path, lines } => {
                let empty = Vec::new();
                let results = body["breakpoints"].as_array().unwrap_or(&empty);
                let mut breakpoints = Vec::with_capacity(lines.len());
                for (index, &requested) in lines.iter().enumerate() {
                    let result = results.get(index).unwrap_or(&Value::Null);
                    if let Some(id) = result["id"].as_i64() {
                        self.breakpoint_ids.insert(id, (path.clone(), index));
                    }
                    breakpoints.push(BreakpointStatus {
                        requested,
                        verified: result["verified"].as_bool().unwrap_or(false),
                        line: result["line"].as_u64().map(|l| (l as usize).saturating_sub(1)),
                    });
                }
                self.emit(DebugEvent::Breakpoints { path, breakpoints });
            }
            Pending::StackTrace { reason, description } => {
                let empty = Vec::new();
                let frames = body["stackFrames"]
                    .as_array()
                    .unwrap_or(&empty)
                    .iter()
                    .map(|frame| Frame {
                        name: frame["name"].as_str().unwrap_or("?").to_owned(),
                        path: frame["source"]["path"].as_str().map(PathBuf::from),
                        line: (frame["line"].as_u64().unwrap_or(1) as usize).saturating_sub(1),
                        column: (frame["column"].as_u64().unwrap_or(1) as usize).saturating_sub(1),
                    })
                    .collect();
                self.set_state(DebugState::Stopped);
                self.emit(DebugEvent::Stopped { reason, description, frames });
            }
            Pending::Disconnect => self.finished = true,
            Pending::Resume | Pending::Other => {}
        }
    }

    fn event(&mut self, event: &str, body: &Value) {
        match event {
            "initialized" => {
                for (path, lines) in self.launch.breakpoints.clone() {
                    self.set_breakpoints(path, lines);
                }
                self.request("setExceptionBreakpoints", json!({ "filters": [] }), Pending::Other);
                self.request("configurationDone", json!({}), Pending::Other);
                self.configured = true;
            }
            "stopped" => {
                if let Some(id) = body["threadId"].as_i64() {
                    self.thread_id = Some(id);
                }
                let reason = body["reason"].as_str().unwrap_or("unknown").to_owned();
                let description = body["description"].as_str().or(body["text"].as_str()).map(str::to_owned);
                let thread = self.thread_id.unwrap_or(0);
                self.request(
                    "stackTrace",
                    json!({ "threadId": thread, "startFrame": 0, "levels": 64 }),
                    Pending::StackTrace { reason, description },
                );
            }
            "continued" => {
                if self.state() != DebugState::Running {
                    self.set_state(DebugState::Running);
                    self.emit(DebugEvent::Continued);
                }
            }
            "output" => {
                let category = body["category"].as_str().unwrap_or("console");
                let text = body["output"].as_str().unwrap_or("");
                match category {
                    "telemetry" => {}
                    // On Windows the program's output arrives as "console".
                    "stdout" | "stderr" | "console" => {
                        let partial = self.partial.entry(category.to_owned()).or_default();
                        partial.push_str(text);
                        let mut lines: Vec<String> = Vec::new();
                        while let Some(end) = partial.find('\n') {
                            let line: String = partial.drain(..=end).collect();
                            lines.push(line.trim_end_matches(['\r', '\n']).to_owned());
                        }
                        for line in lines {
                            self.emit(DebugEvent::Output(line));
                        }
                    }
                    _ => {
                        for line in text.lines().filter(|l| !l.trim().is_empty()) {
                            self.emit(DebugEvent::Status(line.trim_end().to_owned()));
                        }
                    }
                }
            }
            "breakpoint" => {
                let breakpoint = &body["breakpoint"];
                let Some((path, index)) = breakpoint["id"].as_i64().and_then(|id| self.breakpoint_ids.get(&id)).cloned()
                else {
                    return;
                };
                let Some((_, lines)) = self.launch.breakpoints.iter().find(|(p, _)| *p == path) else { return };
                let Some(&requested) = lines.get(index) else { return };
                self.emit(DebugEvent::Breakpoints {
                    path,
                    breakpoints: vec![BreakpointStatus {
                        requested,
                        verified: breakpoint["verified"].as_bool().unwrap_or(false),
                        line: breakpoint["line"].as_u64().map(|l| (l as usize).saturating_sub(1)),
                    }],
                });
            }
            "process" => {
                if let Some(pid) = body["systemProcessId"].as_i64() {
                    self.emit(DebugEvent::Status(format!("Process {pid} started")));
                }
            }
            "exited" => {
                self.exit_code = body["exitCode"].as_i64();
                let code = self.exit_code.map_or("unknown".to_owned(), |c| c.to_string());
                self.emit(DebugEvent::Status(format!("The program exited with code {code}")));
            }
            "terminated" => self.stop(false),
            _ => {}
        }
    }
}
