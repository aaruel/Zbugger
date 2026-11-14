//! Profiling support: Tracy zones, and a scripted run so profiles are
//! repeatable without anyone clicking around.
//!
//! **Zones.** Build with `--features tracy` and every [`zone!`] becomes a
//! Tracy zone; without the feature they compile to nothing. Connect the Tracy
//! profiler (version 0.14.1) to see them live, or record with `tracy-capture`.
//!
//! **Scripted run.** Set `ZBUGGER_PROFILE_FRAMES=N` and pass a project or
//! solution on the command line. Zbugger waits for the project to load and be
//! indexed, expands every folder in the project tree, renders N frames, writes
//! their timings to `ZBUGGER_PROFILE_OUT` (default `profile_frames.txt`) and exits.
//!
//! ```text
//! cargo build --profile profiling --features tracy
//! tracy-capture -o tree.tracy -f        (in one terminal; waits for Zbugger)
//! ZBUGGER_PROFILE_FRAMES=5 target\profiling\zbugger.exe path\to\Game.sln
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Time the rest of the enclosing block as a Tracy zone named `$name`.
#[macro_export]
macro_rules! zone {
    ($name:literal) => {
        let _zone = tracy_client::span!($name);
    };
}

pub struct ProfileRun {
    frames: usize,
    out: PathBuf,
    /// Set once the project is ready and the tree has been expanded.
    started: Option<Instant>,
    /// Time spent in our `update` for each measured frame.
    update_times: Vec<Duration>,
    /// eframe's CPU time for the previous frame (update + tessellation).
    frame_cpu: Vec<f32>,
}

impl ProfileRun {
    /// A run configured from the environment, or `None` for normal use.
    pub fn from_env() -> Option<Self> {
        let frames = std::env::var("ZBUGGER_PROFILE_FRAMES").ok()?.parse().ok()?;
        let out = std::env::var_os("ZBUGGER_PROFILE_OUT").map_or_else(|| "profile_frames.txt".into(), PathBuf::from);
        Some(Self { frames, out, started: None, update_times: Vec::new(), frame_cpu: Vec::new() })
    }

    pub fn is_measuring(&self) -> bool {
        self.started.is_some()
    }

    /// Call once the project is loaded and indexed; measuring starts next frame.
    pub fn start(&mut self) {
        if self.started.is_none() {
            self.started = Some(Instant::now());
        }
    }

    /// Record one frame. Returns true when the run is complete.
    pub fn record(&mut self, update_time: Duration, previous_frame_cpu: Option<f32>) -> bool {
        // The first measured frame builds the expanded tree state; keep it, but
        // the report also shows the steady-state frames separately.
        self.update_times.push(update_time);
        if let Some(cpu) = previous_frame_cpu {
            self.frame_cpu.push(cpu);
        }
        self.update_times.len() >= self.frames
    }

    /// Write the timing report.
    pub fn finish(&self) -> std::io::Result<()> {
        let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
        let steady: Vec<f64> = self.update_times.iter().skip(1).map(ms).collect();
        let mean = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
        let max = |v: &[f64]| v.iter().copied().fold(0.0, f64::max);
        let cpu: Vec<f64> = self.frame_cpu.iter().skip(2).map(|s| *s as f64 * 1000.0).collect();

        let mut report = String::new();
        report.push_str(&format!("frames measured: {}\n", self.update_times.len()));
        report.push_str(&format!(
            "first frame after expanding (update): {:.1} ms\n",
            self.update_times.first().map(ms).unwrap_or(0.0)
        ));
        report.push_str(&format!("steady frames (update): mean {:.1} ms, max {:.1} ms\n", mean(&steady), max(&steady)));
        report.push_str(&format!("steady frames (eframe cpu, incl. tessellation): mean {:.1} ms, max {:.1} ms\n", mean(&cpu), max(&cpu)));
        report.push_str("per-frame update ms:");
        for t in &self.update_times {
            report.push_str(&format!(" {:.1}", ms(t)));
        }
        report.push('\n');
        // Lets the caller time shutdown: from here to the process exiting.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        report.push_str(&format!("close requested (unix ms): {}\n", now.as_millis()));
        std::fs::write(&self.out, report)
    }
}
