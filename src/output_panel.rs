//! The Output panel: what a build and the launched program print.

use std::path::PathBuf;
use std::sync::OnceLock;

use egui::{pos2, vec2, RichText, Sense, TextStyle, TextWrapMode, Ui, WidgetText};
use regex::Regex;

use crate::build::JobEvent;

/// Keep at most this many lines (a long-running program can print a lot).
const MAX_LINES: usize = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Normal,
    /// A message from Zbugger itself.
    Status,
    Warning,
    Error,
}

/// A source location named by an error or warning line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLocation {
    pub path: PathBuf,
    /// Zero-based line and byte column.
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct OutputLine {
    pub text: String,
    pub kind: LineKind,
    pub location: Option<OutputLocation>,
}

/// Classify a line of compiler or build tool output, and find the file
/// location in it, if any. Handles MSVC-style `C:\path\file.cpp(12,5): error`
/// (also with MSBuild's `3>` prefix) and clang-style `C:\path\file.cpp:12:5: error`.
pub fn parse_line(text: &str) -> (LineKind, Option<OutputLocation>) {
    static MSVC: OnceLock<Regex> = OnceLock::new();
    static CLANG: OnceLock<Regex> = OnceLock::new();
    let msvc = MSVC.get_or_init(|| {
        Regex::new(r"^\s*(?:\d+>)?\s*([A-Za-z]:[\\/][^(<>|]+?)\((\d+)(?:,(\d+))?\)\s*:\s*(?:fatal\s+)?(error|warning)\b")
            .expect("valid pattern")
    });
    let clang = CLANG.get_or_init(|| {
        Regex::new(r"^\s*([A-Za-z]:[\\/][^:<>|]+?):(\d+):(\d+):\s*(?:fatal\s+)?(error|warning)\b").expect("valid pattern")
    });
    for pattern in [msvc, clang] {
        if let Some(caps) = pattern.captures(text) {
            let number = |i: usize| caps.get(i).and_then(|m| m.as_str().parse::<usize>().ok()).unwrap_or(1);
            let kind = if &caps[4] == "error" { LineKind::Error } else { LineKind::Warning };
            let location = OutputLocation {
                path: PathBuf::from(caps[1].trim()),
                line: number(2).saturating_sub(1),
                column: number(3).saturating_sub(1),
            };
            return (kind, Some(location));
        }
    }
    let lower = text.to_ascii_lowercase();
    let kind = if lower.contains(": error") || lower.contains("error:") || lower.contains(" error c") || lower.contains("fatal error") {
        LineKind::Error
    } else if lower.contains(": warning") || lower.contains("warning:") {
        LineKind::Warning
    } else {
        LineKind::Normal
    };
    (kind, None)
}

#[derive(Default)]
pub struct OutputLog {
    lines: Vec<OutputLine>,
    errors: usize,
    warnings: usize,
    content_width: f32,
}

impl OutputLog {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn push(&mut self, event: JobEvent) {
        let line = match event {
            JobEvent::Status(text) => OutputLine { text: format!("— {text}"), kind: LineKind::Status, location: None },
            JobEvent::Output(text) => {
                let (kind, location) = parse_line(&text);
                OutputLine { text, kind, location }
            }
        };
        match line.kind {
            LineKind::Error => self.errors += 1,
            LineKind::Warning => self.warnings += 1,
            _ => {}
        }
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    pub fn summary(&self) -> String {
        format!("{} errors, {} warnings", self.errors, self.warnings)
    }

    /// Draw the lines. Returns a location the user clicked.
    pub fn ui(&mut self, ui: &mut Ui) -> Option<OutputLocation> {
        let row_height = ui.text_style_height(&TextStyle::Monospace) + 2.0;
        let mut clicked = None;
        if self.lines.is_empty() {
            ui.label(RichText::new("Build and program output appears here.").weak());
            return None;
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::both()
            .id_salt("output_lines")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show_rows(ui, row_height, self.lines.len(), |ui, range| {
                let width = ui.available_width().max(self.content_width);
                for line in &self.lines[range] {
                    let sense = if line.location.is_some() { Sense::click() } else { Sense::hover() };
                    let (rect, response) = ui.allocate_exact_size(vec2(width, row_height), sense);
                    let visuals = ui.visuals();
                    let color = match line.kind {
                        LineKind::Normal => visuals.text_color(),
                        LineKind::Status => visuals.weak_text_color(),
                        LineKind::Warning => visuals.warn_fg_color,
                        LineKind::Error => visuals.error_fg_color,
                    };
                    if response.hovered() && line.location.is_some() {
                        ui.painter().rect_filled(rect, 2.0, visuals.widgets.hovered.weak_bg_fill);
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let galley = WidgetText::from(RichText::new(&line.text).monospace().color(color))
                        .into_galley(ui, Some(TextWrapMode::Extend), f32::INFINITY, TextStyle::Monospace);
                    self.content_width = self.content_width.max(galley.size().x + 12.0);
                    ui.painter().galley(pos2(rect.left() + 4.0, rect.top() + 1.0), galley, color);
                    if response.clicked() {
                        clicked = line.location.clone();
                    }
                }
            });
        clicked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_compiler_messages() {
        let (kind, at) = parse_line(r"W:\Game\Source\Actor.cpp(123): error C2065: 'Foo': undeclared identifier");
        assert_eq!(kind, LineKind::Error);
        assert_eq!(at, Some(OutputLocation { path: PathBuf::from(r"W:\Game\Source\Actor.cpp"), line: 122, column: 0 }));

        let (kind, at) = parse_line(r"  3>C:\src\main.cpp(10,5): warning C4100: unreferenced parameter");
        assert_eq!(kind, LineKind::Warning);
        assert_eq!(at.unwrap().column, 4);

        let (kind, at) = parse_line(r"C:\src\a.cpp:7:3: error: expected ';'");
        assert_eq!((kind, at.unwrap().line), (LineKind::Error, 6));

        assert_eq!(parse_line("LINK : fatal error LNK1104: cannot open file").0, LineKind::Error);
        assert_eq!(parse_line("Building 5 actions with 16 processes...").0, LineKind::Normal);
    }

    #[test]
    fn counts_and_caps() {
        let mut log = OutputLog::default();
        log.push(JobEvent::Status("Building".into()));
        log.push(JobEvent::Output(r"C:\a.cpp(1): error C1: x".into()));
        log.push(JobEvent::Output(r"C:\a.cpp(2): warning C2: y".into()));
        assert_eq!(log.summary(), "1 errors, 1 warnings");
        assert_eq!(log.lines[0].kind, LineKind::Status);
        for i in 0..MAX_LINES + 10 {
            log.push(JobEvent::Output(i.to_string()));
        }
        assert_eq!(log.lines.len(), MAX_LINES);
    }
}
