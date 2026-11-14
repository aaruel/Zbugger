//! User preferences, saved between runs by eframe's storage.

use serde::{Deserialize, Serialize};

pub const STORAGE_KEY: &str = "preferences";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    System,
    Dark,
    Light,
}

impl ThemeChoice {
    fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "Follow system",
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
        }
    }
}

impl From<ThemeChoice> for egui::ThemePreference {
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
            ThemeChoice::Light => egui::ThemePreference::Light,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
// Missing fields (e.g. after adding a new option) fall back to their defaults.
#[serde(default)]
pub struct Preferences {
    pub theme: ThemeChoice,
    /// Editor font size in points.
    pub font_size: f32,
    pub show_line_numbers: bool,
    /// Size of tabs relative to a normal egui button.
    pub tab_scale: f32,
    /// Tab names longer than this many characters are shortened with "…".
    pub tab_name_max_chars: usize,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            font_size: 12.0,
            show_line_numbers: true,
            tab_scale: 1.5,
            tab_name_max_chars: 20,
        }
    }
}

pub const MIN_TAB_NAME_CHARS: usize = 4;

/// Width of the label column, shared by every section so the controls line up.
const LABEL_WIDTH: f32 = 130.0;

impl Preferences {
    /// Push preferences that egui owns (the theme) into the context.
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.set_theme(egui::ThemePreference::from(self.theme));
    }

    /// Draw the preference controls. Returns true if anything changed.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let before = self.clone();

        ui.heading("Appearance");
        egui::Grid::new("prefs_appearance").num_columns(2).min_col_width(LABEL_WIDTH).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Theme");
            egui::ComboBox::from_id_salt("theme")
                .selected_text(self.theme.label())
                .show_ui(ui, |ui| {
                    for choice in [ThemeChoice::System, ThemeChoice::Dark, ThemeChoice::Light] {
                        ui.selectable_value(&mut self.theme, choice, choice.label());
                    }
                });
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.heading("Editor");
        egui::Grid::new("prefs_editor").num_columns(2).min_col_width(LABEL_WIDTH).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Font size");
            ui.add(egui::Slider::new(&mut self.font_size, 8.0..=32.0).step_by(1.0).suffix(" pt"));
            ui.end_row();

            ui.label("Line numbers");
            ui.checkbox(&mut self.show_line_numbers, "Show");
            ui.end_row();
        });

        ui.add_space(8.0);
        ui.heading("Tabs");
        egui::Grid::new("prefs_tabs").num_columns(2).min_col_width(LABEL_WIDTH).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Tab size");
            ui.add(egui::Slider::new(&mut self.tab_scale, 1.0..=2.5).step_by(0.1).suffix("×"));
            ui.end_row();

            ui.label("Max tab name length");
            ui.add(
                egui::Slider::new(&mut self.tab_name_max_chars, MIN_TAB_NAME_CHARS..=60)
                    .suffix(" chars"),
            );
            ui.end_row();
        });

        ui.add_space(12.0);
        ui.separator();
        if ui.button("Reset to defaults").clicked() {
            *self = Self::default();
        }

        *self != before
    }
}

/// Shorten `name` to at most `max_chars` characters, ending in "…" if cut.
pub fn truncate_name(name: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(MIN_TAB_NAME_CHARS);
    if name.chars().count() <= max_chars {
        name.to_owned()
    } else {
        let mut short: String = name.chars().take(max_chars - 1).collect();
        short.push('…');
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_names() {
        assert_eq!(truncate_name("main.cpp", 20), "main.cpp");
        assert_eq!(truncate_name("exactly_ten", 11), "exactly_ten");
        assert_eq!(truncate_name("a_really_long_file_name.cpp", 10), "a_really_…");
        assert_eq!(truncate_name("a_really_long_file_name.cpp", 10).chars().count(), 10);
        // Never shorter than the minimum, and safe for non-ASCII names.
        assert_eq!(truncate_name("ünïcödé_name.h", 1), "ünï…");
    }
}
