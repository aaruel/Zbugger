//! The build toolbar: configuration, platform and startup project, with
//! Play / Build / Stop, and the debugger's Continue / Pause / Step buttons
//! while debugging.

use std::collections::HashMap;

use egui::{Color32, RichText, Ui};
use serde::{Deserialize, Serialize};

use crate::build::{BuildModel, Job, JobState};
use crate::debug::{DebugSession, DebugState};
use crate::document::path_key;
use crate::project::Configuration;

/// What the user picked for one solution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSelection {
    pub configuration: String,
    pub platform: String,
    pub project: String,
}

impl BuildSelection {
    pub fn solution_configuration(&self) -> Configuration {
        Configuration { configuration: self.configuration.clone(), platform: self.platform.clone() }
    }

    /// Sensible defaults: the likely startup project, and Unreal's usual
    /// `Development Editor|Win64` (or `Debug` on a 64-bit platform) if the
    /// solution has it.
    pub fn default_for(model: &BuildModel) -> Option<Self> {
        let project = model.default_startup_project()?.name.clone();
        let preferred = [("Development Editor", "Win64"), ("Debug", "x64"), ("Debug", "Win64")];
        let config = preferred
            .iter()
            .find_map(|(c, p)| model.configurations.iter().find(|k| k.configuration == *c && k.platform == *p))
            .or_else(|| model.configurations.first())?;
        Some(Self { configuration: config.configuration.clone(), platform: config.platform.clone(), project })
    }

    /// Whether this still fits `model` (projects and configurations can change).
    fn is_valid_for(&self, model: &BuildModel) -> bool {
        model.projects.iter().any(|p| p.name == self.project)
            && model.configurations.iter().any(|c| c.configuration == self.configuration)
            && model.configurations.iter().any(|c| c.platform == self.platform)
    }
}

/// What the user clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarAction {
    /// Build, then debug (or continue, when stopped in the debugger).
    Play,
    /// Build, then run without the debugger.
    Run,
    Build,
    Stop,
    Pause,
    StepOver,
    StepIn,
    StepOut,
}

#[derive(Default)]
pub struct Toolbar {
    pub selection: Option<BuildSelection>,
    /// Choices per solution (by path key), saved between runs.
    pub saved: HashMap<String, BuildSelection>,
    /// The build or program started from the toolbar.
    pub job: Option<Job>,
    /// The debugging session started by Play.
    pub debug: Option<DebugSession>,
    /// Set while building, to debug once the build succeeds.
    pub debug_after_build: bool,
}

pub const STORAGE_KEY: &str = "build_selections";

fn solution_key(model: &BuildModel) -> String {
    path_key(&model.path).to_string_lossy().into_owned()
}

impl Toolbar {
    /// Pick up the saved (or default) choices when a solution opens.
    pub fn solution_opened(&mut self, model: &BuildModel) {
        self.selection = self
            .saved
            .get(&solution_key(model))
            .filter(|s| s.is_valid_for(model))
            .cloned()
            .or_else(|| BuildSelection::default_for(model));
    }

    pub fn is_busy(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.state().is_active()) || self.debug_state().is_some()
    }

    /// The debugger's state, while a session is running.
    pub fn debug_state(&self) -> Option<DebugState> {
        self.debug.as_ref().map(DebugSession::state).filter(|s| s.is_active())
    }

    /// Draw the toolbar. `model` is `None` when no solution is open.
    pub fn ui(&mut self, ui: &mut Ui, model: Option<&BuildModel>) -> Option<ToolbarAction> {
        let mut action = None;
        ui.horizontal(|ui| {
            let enabled = model.is_some() && self.selection.is_some();
            let (names, platforms) = model.map(BuildModel::configuration_names).unwrap_or_default();
            let mut projects: Vec<&str> = model.map(|m| m.projects.iter().map(|p| p.name.as_str()).collect()).unwrap_or_default();
            projects.sort_by_key(|n| n.to_lowercase());

            let before = self.selection.clone();
            let selection = self.selection.get_or_insert_with(|| BuildSelection {
                configuration: String::new(),
                platform: String::new(),
                project: String::new(),
            });
            ui.add_enabled_ui(enabled, |ui| {
                combo(ui, "configuration", "Configuration", &mut selection.configuration, &names, 150.0);
                combo(ui, "platform", "Platform", &mut selection.platform, &platforms, 90.0);
                combo(ui, "startup_project", "Startup project", &mut selection.project, &projects, 170.0);
            });
            if !enabled {
                self.selection = before.clone();
            }

            // Not every configuration exists on every platform.
            let available = model.zip(self.selection.as_ref()).is_some_and(|(m, s)| {
                m.configurations.iter().any(|c| c.configuration == s.configuration && c.platform == s.platform)
            });
            if enabled && !available {
                ui.label(RichText::new("⚠").color(ui.visuals().warn_fg_color))
                    .on_hover_text("The solution has no configuration for this combination");
            }

            ui.separator();
            if let Some(state) = self.debug_state() {
                let stopped = state == DebugState::Stopped;
                if stopped {
                    let resume = egui::Button::new(RichText::new("▶ Continue").color(PLAY_GREEN));
                    if ui.add(resume).on_hover_text("Continue (F5)").clicked() {
                        action = Some(ToolbarAction::Play);
                    }
                } else if ui.add_enabled(state == DebugState::Running, egui::Button::new("Pause")).on_hover_text("Break into the debugger").clicked() {
                    action = Some(ToolbarAction::Pause);
                }
                for (label, hover, step) in [
                    ("Step Over", "Run the next line, stepping over calls (F10)", ToolbarAction::StepOver),
                    ("Step Into", "Run the next line, stepping into calls (F11)", ToolbarAction::StepIn),
                    ("Step Out", "Run until the current function returns (Shift+F11)", ToolbarAction::StepOut),
                ] {
                    if ui.add_enabled(stopped, egui::Button::new(label)).on_hover_text(hover).clicked() {
                        action = Some(step);
                    }
                }
            }
            if self.is_busy() {
                if ui.button(RichText::new("■ Stop").color(ui.visuals().error_fg_color)).on_hover_text("Stop (Shift+F5)").clicked() {
                    action = Some(ToolbarAction::Stop);
                }
            } else {
                let play = egui::Button::new(RichText::new("▶ Play").color(PLAY_GREEN));
                let hover = self.plan_preview(model);
                let play = ui
                    .add_enabled(enabled && available, play)
                    .on_hover_text(format!("Build and debug with LLDB (F5)\nRun without debugging: Ctrl+F5 or right-click\n{hover}"));
                if play.clicked() {
                    action = Some(ToolbarAction::Play);
                }
                play.context_menu(|ui| {
                    if ui.button("Run Without Debugging").clicked() {
                        action = Some(ToolbarAction::Run);
                        ui.close_menu();
                    }
                });
                if ui.add_enabled(enabled && available, egui::Button::new("Build")).on_hover_text("Build only (Ctrl+Shift+B)").clicked() {
                    action = Some(ToolbarAction::Build);
                }
            }
            self.status_label(ui);

            // Remember the choice for this solution.
            if let (Some(model), Some(selection)) = (model, &self.selection) {
                if before.as_ref() != Some(selection) {
                    self.saved.insert(solution_key(model), selection.clone());
                }
            }
        });
        action
    }

    /// What Play would run, for its tooltip.
    fn plan_preview(&self, model: Option<&BuildModel>) -> String {
        let (Some(model), Some(selection)) = (model, &self.selection) else { return String::new() };
        let Some(project) = model.projects.iter().find(|p| p.name == selection.project) else { return String::new() };
        match model.plan(project, &selection.solution_configuration()) {
            Ok(plan) => match plan.run {
                Some(run) => format!("Runs {} {}", run.program.display(), run.arguments),
                None => "Nothing to run".to_owned(),
            },
            Err(err) => err,
        }
    }

    fn status_label(&self, ui: &mut Ui) {
        if let Some(state) = self.debug_state() {
            let text = match state {
                DebugState::Starting => "Starting the debugger…",
                DebugState::Running => "Debugging",
                DebugState::Stopped => "Paused",
                DebugState::Ended => "",
            };
            if state == DebugState::Starting {
                ui.spinner();
            }
            let label = if state == DebugState::Stopped {
                RichText::new(text).color(crate::editor::EXEC_COLOR)
            } else {
                RichText::new(text).weak()
            };
            ui.label(label);
            return;
        }
        let Some(job) = &self.job else { return };
        let (text, color) = match job.state() {
            JobState::Building => ("Building…", None),
            JobState::Running => ("Running", None),
            JobState::Built => ("Build succeeded", None),
            JobState::Failed => ("Failed", Some(ui.visuals().error_fg_color)),
            JobState::Stopped => ("Stopped", None),
            JobState::Exited(_) => ("Exited", None),
        };
        if job.state().is_active() {
            ui.spinner();
        }
        let mut label = RichText::new(text);
        label = match color {
            Some(color) => label.color(color),
            None => label.weak(),
        };
        ui.label(label);
    }
}

const PLAY_GREEN: Color32 = Color32::from_rgb(0x4C, 0xAF, 0x50);

fn combo(ui: &mut Ui, id: &str, hover: &str, value: &mut String, options: &[impl AsRef<str>], width: f32) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_str())
        .width(width)
        .height(400.0)
        .show_ui(ui, |ui| {
            for option in options {
                let option = option.as_ref();
                ui.selectable_value(value, option.to_owned(), option);
            }
        })
        .response
        .on_hover_text(hover);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::BuildProject;
    use crate::project::PropertyGroup;

    fn project(name: &str, kind: &str) -> BuildProject {
        BuildProject {
            name: name.to_owned(),
            path: format!("{name}.vcxproj").into(),
            mappings: Vec::new(),
            configurations: Vec::new(),
            property_groups: vec![PropertyGroup {
                label: None,
                condition: None,
                properties: vec![("ConfigurationType".to_owned(), kind.to_owned())],
            }],
        }
    }

    fn configuration(key: &str) -> Configuration {
        let (configuration, platform) = key.split_once('|').unwrap();
        Configuration { configuration: configuration.to_owned(), platform: platform.to_owned() }
    }

    #[test]
    fn defaults_prefer_an_application_and_usual_configurations() {
        let model = BuildModel::for_solution(
            std::path::Path::new("S.sln"),
            vec![configuration("Release|x64"), configuration("Debug|x64")],
            vec![project("Lib", "StaticLibrary"), project("App", "Application")],
        );
        let selection = BuildSelection::default_for(&model).unwrap();
        assert_eq!(selection, BuildSelection { configuration: "Debug".into(), platform: "x64".into(), project: "App".into() });
    }

    /// Draw the toolbar headlessly; returns drawn texts (with centers) and the action.
    fn render(toolbar: &mut Toolbar, model: Option<&BuildModel>, ctx: &egui::Context, events: Vec<egui::Event>) -> (Vec<(String, egui::Pos2)>, Option<ToolbarAction>) {
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1200.0, 800.0));
        let mut action = None;
        let output = ctx.run(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ctx| {
            egui::TopBottomPanel::top("toolbar").show(ctx, |ui| action = toolbar.ui(ui, model));
        });
        let mut drawn = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                drawn.push((t.galley.text().to_owned(), t.pos + t.galley.size() / 2.0));
            }
        }
        (drawn, action)
    }

    #[test]
    fn shows_choices_and_reports_play() {
        let model = BuildModel::for_solution(
            std::path::Path::new("S.sln"),
            vec![configuration("Debug|x64")],
            vec![project("App", "Application")],
        );
        let mut toolbar = Toolbar::default();
        toolbar.solution_opened(&model);
        let ctx = egui::Context::default();
        let (drawn, _) = render(&mut toolbar, Some(&model), &ctx, vec![]);
        let texts: Vec<&str> = drawn.iter().map(|(t, _)| t.as_str()).collect();
        for expected in ["Debug", "x64", "App", "▶ Play", "Build"] {
            assert!(texts.contains(&expected), "{expected} missing from {texts:?}");
        }
        let play = drawn.iter().find(|(t, _)| t == "▶ Play").unwrap().1;
        let button = |pressed| egui::Event::PointerButton { pos: play, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        render(&mut toolbar, Some(&model), &ctx, vec![egui::Event::PointerMoved(play), button(true)]);
        let (_, action) = render(&mut toolbar, Some(&model), &ctx, vec![button(false)]);
        assert_eq!(action, Some(ToolbarAction::Play));

        // Without a solution, there's nothing to choose and Play does nothing.
        let mut empty = Toolbar::default();
        let (_, action) = render(&mut empty, None, &egui::Context::default(), vec![]);
        assert_eq!(action, None);
    }

    /// Symbols used in labels across the app must exist in egui's built-in
    /// fonts, or they'd draw as empty boxes.
    #[test]
    fn label_symbols_have_glyphs() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let font = egui::FontId::proportional(14.0);
        let missing: Vec<char> = "▶■⏹⚠⬆⬇📁×…—"
            .chars()
            .filter(|c| !ctx.fonts(|f| f.has_glyph(&font, *c)))
            .collect();
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }

    #[test]
    fn saved_choices_are_used_while_still_valid() {
        let model = BuildModel::for_solution(
            std::path::Path::new("S.sln"),
            vec![configuration("Debug|x64"), configuration("Release|x64")],
            vec![project("App", "Application")],
        );
        let mut toolbar = Toolbar::default();
        let saved = BuildSelection { configuration: "Release".into(), platform: "x64".into(), project: "App".into() };
        toolbar.saved.insert(solution_key(&model), saved.clone());
        toolbar.solution_opened(&model);
        assert_eq!(toolbar.selection, Some(saved));

        // A saved project that no longer exists falls back to the defaults.
        toolbar.saved.insert(
            solution_key(&model),
            BuildSelection { configuration: "Release".into(), platform: "x64".into(), project: "Gone".into() },
        );
        toolbar.solution_opened(&model);
        assert_eq!(toolbar.selection.unwrap().project, "App");
    }
}
