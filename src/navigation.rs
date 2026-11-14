//! Jumping around the code: the back/forward history and the picker shown
//! when go-to-definition finds several candidates.

use std::path::PathBuf;

use egui::{Align2, Key, Modifiers, RichText};

use crate::project::file_name;
use crate::symbols::Symbol;

/// A caret position in a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

impl From<&Symbol> for Location {
    fn from(symbol: &Symbol) -> Self {
        Self { path: symbol.path.to_path_buf(), line: symbol.line, column: symbol.column }
    }
}

const MAX_HISTORY: usize = 100;

/// Where you were before each jump, for Back and Forward.
#[derive(Default)]
pub struct History {
    back: Vec<Location>,
    forward: Vec<Location>,
}

impl History {
    /// Record `from` before jumping somewhere new.
    pub fn push(&mut self, from: Location) {
        if self.back.last() != Some(&from) {
            self.back.push(from);
            if self.back.len() > MAX_HISTORY {
                self.back.remove(0);
            }
        }
        self.forward.clear();
    }

    /// Step back from `current`, returning where to go.
    pub fn back(&mut self, current: Option<Location>) -> Option<Location> {
        let target = self.back.pop()?;
        self.forward.extend(current);
        Some(target)
    }

    /// Step forward from `current`, returning where to go.
    pub fn forward(&mut self, current: Option<Location>) -> Option<Location> {
        let target = self.forward.pop()?;
        self.back.extend(current);
        Some(target)
    }
}

/// A list of candidate definitions to choose from.
pub struct DefinitionPicker {
    pub name: String,
    pub candidates: Vec<Symbol>,
    pub selected: usize,
}

pub enum PickerResult {
    Open,
    Chosen(Symbol),
    Cancelled,
}

impl DefinitionPicker {
    pub fn new(name: String, candidates: Vec<Symbol>) -> Self {
        Self { name, candidates, selected: 0 }
    }

    /// Handle Up/Down/Enter/Escape. Call before the editor runs, so it
    /// doesn't also act on these keys.
    pub fn handle_keys(&mut self, ctx: &egui::Context) -> PickerResult {
        let key = |k| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if key(Key::Escape) {
            return PickerResult::Cancelled;
        }
        if key(Key::ArrowDown) {
            self.selected = (self.selected + 1) % self.candidates.len();
        }
        if key(Key::ArrowUp) {
            self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
        }
        if key(Key::Enter) {
            return PickerResult::Chosen(self.candidates[self.selected].clone());
        }
        PickerResult::Open
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PickerResult {
        let mut open = true;
        let mut chosen = None;
        egui::Window::new(format!("{} definitions of {}", self.candidates.len(), self.name))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 60.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
                    egui::Grid::new("definition_candidates").num_columns(3).spacing([16.0, 4.0]).show(ui, |ui| {
                        for (i, symbol) in self.candidates.iter().enumerate() {
                            let mut name = RichText::new(symbol.qualified_name()).monospace();
                            if symbol.definition {
                                name = name.strong();
                            }
                            let row = ui.selectable_label(i == self.selected, name);
                            if i == self.selected {
                                row.scroll_to_me(None);
                            }
                            let kind = if symbol.definition {
                                symbol.kind.label().to_owned()
                            } else {
                                format!("{} declaration", symbol.kind.label())
                            };
                            ui.label(RichText::new(kind).weak());
                            let place = ui
                                .label(format!("{}:{}", file_name(&symbol.path), symbol.line + 1))
                                .on_hover_text(symbol.path.display().to_string());
                            if row.clicked() || place.clicked() {
                                chosen = Some(symbol.clone());
                            }
                            ui.end_row();
                        }
                    });
                });
                ui.separator();
                ui.label(RichText::new("⬆⬇ to choose, Enter to open, Esc to cancel").weak().small());
            });
        match (chosen, open) {
            (Some(symbol), _) => PickerResult::Chosen(symbol),
            (None, false) => PickerResult::Cancelled,
            (None, true) => PickerResult::Open,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: usize) -> Location {
        Location { path: PathBuf::from("a.cpp"), line, column: 0 }
    }

    #[test]
    fn back_and_forward() {
        let mut history = History::default();
        history.push(at(1)); // jumped from line 1 to line 10
        history.push(at(10)); // jumped from line 10 to line 20
        assert_eq!(history.back(Some(at(20))), Some(at(10)));
        assert_eq!(history.back(Some(at(10))), Some(at(1)));
        assert_eq!(history.back(Some(at(1))), None);
        assert_eq!(history.forward(Some(at(1))), Some(at(10)));
        assert_eq!(history.forward(Some(at(10))), Some(at(20)));
        // A new jump clears the forward list.
        history.back(Some(at(20)));
        history.push(at(10));
        assert_eq!(history.forward(Some(at(30))), None);
    }
}
