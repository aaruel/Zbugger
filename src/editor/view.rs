//! The editor widget. Only the lines inside the viewport are laid out and
//! drawn each frame, so cost does not grow with file size.

use std::hash::Hash;
use std::ops::RangeInclusive;
use std::sync::Arc;

use egui::text::CCursor;
use egui::{
    pos2, vec2, Align2, Color32, CursorIcon, Event, EventFilter, FontId, Galley, ImeEvent, Key, Modifiers,
    PointerButton, Pos2, Rect, ScrollArea, Sense, Stroke, Ui, UiBuilder,
};

use super::{next_char, next_word, prev_char, prev_word, word_at, EditKind, Editor, Selection};
use crate::find::FindHighlights;
use crate::preferences::Preferences;
use crate::syntax::{line_job, Theme};

/// Space between the gutter and the text.
const TEXT_MARGIN: f32 = 4.0;

/// Breakpoint dot and its line tint.
pub const BREAKPOINT_COLOR: Color32 = Color32::from_rgb(0xE5, 0x39, 0x35);
const BREAKPOINT_LINE: Color32 = Color32::from_rgba_premultiplied(50, 13, 12, 56);
/// The debugger's current-line arrow and its line tint.
pub const EXEC_COLOR: Color32 = Color32::from_rgb(0xFF, 0xD2, 0x1F);
const EXEC_LINE: Color32 = Color32::from_rgba_premultiplied(51, 42, 6, 51);

/// Font measurements and colors shared by the helpers below.
struct Style<'a> {
    font: FontId,
    row_height: f32,
    char_width: f32,
    theme: &'a Theme,
}

/// What the user asked for in the editor that the app has to carry out.
#[derive(Default)]
pub struct EditorOutput {
    /// Go to the definition of the symbol at the caret (F12, Ctrl+click, context menu).
    pub go_to_definition: bool,
}

/// Draw `editor` filling the available space and handle its input.
/// `id_salt` must be unique per document so each keeps its own scroll position.
/// `find` highlights search matches while the find bar is open.
pub fn editor_view(
    ui: &mut Ui,
    editor: &mut Editor,
    id_salt: impl Hash,
    prefs: &Preferences,
    find: Option<&FindHighlights>,
) -> EditorOutput {
    let mut out = EditorOutput::default();
    let font = FontId::monospace(prefs.font_size);
    let style = Style {
        row_height: ui.fonts(|f| f.row_height(&font)),
        char_width: ui.fonts(|f| f.glyph_width(&font, '0')),
        font,
        theme: Theme::for_visuals(ui.visuals()),
    };
    let id = ui.make_persistent_id(id_salt);

    // Split the panel: fixed gutter on the left (breakpoint margin, then line
    // numbers), scrolling text on the right.
    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let numbers_width = if prefs.show_line_numbers {
        let digits = editor.buffer().line_count().to_string().len().max(3);
        style.char_width * (digits as f32 + 1.0)
    } else {
        0.0
    };
    let gutter_width = marker_width(&style) + numbers_width;
    let gutter_rect = Rect::from_min_size(full.min, vec2(gutter_width, full.height()));
    let text_rect = Rect::from_min_max(pos2(gutter_rect.max.x, full.min.y), full.max);

    // Content size, without laying anything out. Lines with tabs can be wider
    // than their byte length suggests, so also use the widest line seen so far.
    let longest = editor.buffer().longest_line_len() as f32 * style.char_width;
    let content_width = TEXT_MARGIN + longest.max(editor.widest_line) + style.char_width * 4.0;
    let content_height = editor.buffer().line_count() as f32 * style.row_height;

    let mut text_ui = ui.new_child(UiBuilder::new().max_rect(text_rect));
    let output = ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .show_viewport(&mut text_ui, |ui, viewport| {
            // Fill at least the viewport, so clicks and hovering anywhere in it
            // (including past the end of a line, or below the last line) are ours.
            // Allow scrolling half a page past the last line.
            let size = vec2(
                content_width.max(viewport.width()),
                (content_height + viewport.height() * 0.5).max(viewport.height()),
            );
            let (_, content_rect) = ui.allocate_space(size);
            let response = ui.interact(content_rect, id, Sense::click_and_drag());
            let origin = content_rect.min + vec2(TEXT_MARGIN, 0.0);
            let visible = Rect::from_min_size(content_rect.min + viewport.min.to_vec2(), viewport.size());

            if response.hovered() || response.dragged() {
                // With Ctrl held, a click goes to the definition: show a link cursor.
                let icon = if ui.input(|i| i.modifiers.command) { CursorIcon::PointingHand } else { CursorIcon::Text };
                ui.ctx().set_cursor_icon(icon);
            }
            if editor.request_focus || response.is_pointer_button_down_on() {
                editor.request_focus = false;
                response.request_focus();
            }
            let focused = response.has_focus();

            out.go_to_definition |= handle_pointer(ui, &response, editor, &style, origin);
            response.context_menu(|ui| {
                if ui.add(egui::Button::new("Go to Definition").shortcut_text("F12")).clicked() {
                    out.go_to_definition = true;
                    ui.close_menu();
                }
                ui.separator();
                let has_selection = !editor.selection.is_empty();
                if ui.add_enabled(has_selection, egui::Button::new("Cut")).clicked() {
                    ui.ctx().copy_text(editor.selected_text().to_owned());
                    editor.delete(editor.selection.range(), EditKind::Other);
                    ui.close_menu();
                }
                if ui.add_enabled(has_selection, egui::Button::new("Copy")).clicked() {
                    ui.ctx().copy_text(editor.selected_text().to_owned());
                    ui.close_menu();
                }
                if ui.button("Select All").clicked() {
                    editor.select(Selection { anchor: 0, head: editor.buffer().len() });
                    ui.close_menu();
                }
            });
            if focused {
                // Keep Tab and the arrow keys from moving focus to other widgets.
                ui.memory_mut(|mem| {
                    mem.set_focus_lock_filter(
                        id,
                        EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false },
                    )
                });
                let page_rows = (viewport.height() / style.row_height).floor().max(1.0) as usize;
                out.go_to_definition |= handle_keyboard(ui, editor, &style, page_rows);
            }

            paint_lines(ui, editor, &style, origin, viewport, visible, focused, find);

            let caret = caret_rect(ui, editor, &style, origin);
            if editor.scroll_to_cursor {
                editor.scroll_to_cursor = false;
                let align = std::mem::take(&mut editor.scroll_center).then_some(egui::Align::Center);
                ui.scroll_to_rect(caret.expand2(vec2(style.char_width * 4.0, 0.0)), align);
            }
            if focused {
                ui.ctx().output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput { rect: visible, cursor_rect: caret })
                });
            }
        });

    gutter(ui, editor, &style, id.with("gutter"), gutter_rect, output.inner_rect, output.state.offset.y, prefs.show_line_numbers);
    out
}

/// Width of the breakpoint margin.
fn marker_width(style: &Style) -> f32 {
    (style.row_height + 4.0).max(16.0)
}

// ---- Layout helpers ----------------------------------------------------------

/// The laid-out galley for one line. egui caches galleys between frames, so
/// asking again for an unchanged line is cheap.
fn line_galley(ui: &Ui, editor: &Editor, line: usize, style: &Style) -> Arc<Galley> {
    let highlighter = editor.highlighter();
    let job = line_job(
        editor.buffer().line(line),
        highlighter.line_spans(line),
        highlighter.line_overlay(line),
        &style.font,
        style.theme,
    );
    ui.fonts(|f| f.layout_job(job))
}

/// X position of byte `offset` within its line's galley.
fn x_in_galley(galley: &Galley, line_text: &str, column: usize) -> f32 {
    let index = line_text[..column].chars().count();
    galley.pos_from_ccursor(CCursor { index, prefer_next_row: false }).min.x
}

/// X position (relative to the text origin) of byte `offset`.
fn x_of(ui: &Ui, editor: &Editor, style: &Style, offset: usize) -> f32 {
    let line = editor.buffer().line_of(offset);
    let start = editor.buffer().line_range(line).start;
    let galley = line_galley(ui, editor, line, style);
    x_in_galley(&galley, editor.buffer().line(line), offset - start)
}

/// Byte offset in `line` closest to horizontal position `x`. Positions past
/// the end of the line give the end of the line.
fn offset_at(ui: &Ui, editor: &Editor, style: &Style, line: usize, x: f32) -> usize {
    let galley = line_galley(ui, editor, line, style);
    let index = galley.cursor_from_pos(vec2(x, style.row_height * 0.5)).ccursor.index;
    let text = editor.buffer().line(line);
    let column = text.char_indices().nth(index).map_or(text.len(), |(i, _)| i);
    editor.buffer().line_range(line).start + column
}

/// Byte offset under screen position `pos`.
fn offset_at_pos(ui: &Ui, editor: &Editor, style: &Style, origin: Pos2, pos: Pos2) -> usize {
    let row = ((pos.y - origin.y) / style.row_height).floor();
    let last = editor.buffer().line_count() - 1;
    let line = if row < 0.0 { 0 } else { (row as usize).min(last) };
    offset_at(ui, editor, style, line, pos.x - origin.x)
}

/// Screen rectangle of the caret.
fn caret_rect(ui: &Ui, editor: &Editor, style: &Style, origin: Pos2) -> Rect {
    let head = editor.selection.head;
    let line = editor.buffer().line_of(head);
    let x = origin.x + x_of(ui, editor, style, head);
    let y = origin.y + line as f32 * style.row_height;
    Rect::from_min_size(pos2(x, y), vec2(2.0, style.row_height))
}

// ---- Painting ----------------------------------------------------------------

fn paint_lines(
    ui: &Ui,
    editor: &mut Editor,
    style: &Style,
    origin: Pos2,
    viewport: Rect,
    visible: Rect,
    focused: bool,
    find: Option<&FindHighlights>,
) {
    let line_count = editor.buffer().line_count();
    let first = (viewport.min.y / style.row_height).floor().max(0.0) as usize;
    let last = ((viewport.max.y / style.row_height).ceil() as usize).min(line_count - 1);

    let visuals = ui.visuals();
    let painter = ui.painter();
    let selection = editor.selection.range();
    let head_line = editor.buffer().line_of(editor.selection.head);
    let selection_color =
        if focused { visuals.selection.bg_fill } else { visuals.selection.bg_fill.gamma_multiply(0.5) };
    let current_line_color = visuals.text_color().gamma_multiply(0.06);
    let match_color = visuals.warn_fg_color.gamma_multiply(0.25);
    let current_match_color = visuals.warn_fg_color.gamma_multiply(0.55);

    for line in first..=last {
        let y = origin.y + line as f32 * style.row_height;
        let row = |x0: f32, x1: f32| Rect::from_min_max(pos2(x0, y), pos2(x1, y + style.row_height));
        let galley = line_galley(ui, editor, line, style);
        let range = editor.buffer().line_range(line);
        let text = editor.buffer().line(line);

        if editor.breakpoint_at(line).is_some() {
            painter.rect_filled(row(visible.left(), visible.right()), 0.0, BREAKPOINT_LINE);
        }
        if editor.exec_line() == Some(line) {
            painter.rect_filled(row(visible.left(), visible.right()), 0.0, EXEC_LINE);
        }
        if focused && selection.is_empty() && line == head_line {
            painter.rect_filled(row(visible.left(), visible.right()), 0.0, current_line_color);
        }
        // Search matches on this line, found by binary search in the sorted list.
        if let Some(find) = find {
            let first_match = find.matches.partition_point(|m| m.end <= range.start);
            for (i, m) in find.matches.iter().enumerate().skip(first_match) {
                if m.start > range.end || (m.start == range.end && !m.is_empty()) {
                    break;
                }
                let x0 = x_in_galley(&galley, text, m.start.max(range.start) - range.start);
                let x1 = x_in_galley(&galley, text, m.end.min(range.end) - range.start);
                let rect = row(origin.x + x0, origin.x + x1.max(x0 + 2.0));
                if find.current == Some(i) {
                    painter.rect_filled(rect, 2.0, current_match_color);
                    painter.rect_stroke(rect, 2.0, Stroke::new(1.0_f32, visuals.warn_fg_color), egui::StrokeKind::Inside);
                } else {
                    painter.rect_filled(rect, 2.0, match_color);
                }
            }
        }
        // Selected part of this line; a selected line break shows as a little extra width.
        if !selection.is_empty() && selection.start <= range.end && selection.end > range.start {
            let x0 = x_in_galley(&galley, text, selection.start.max(range.start) - range.start);
            let x1 = if selection.end > range.end {
                x_in_galley(&galley, text, text.len()) + style.char_width * 0.6
            } else {
                x_in_galley(&galley, text, selection.end - range.start)
            };
            painter.rect_filled(row(origin.x + x0, origin.x + x1), 0.0, selection_color);
        }

        editor.widest_line = editor.widest_line.max(galley.size().x);
        painter.galley(pos2(origin.x, y), galley, style.theme.plain);
    }

    if focused {
        let caret = caret_rect(ui, editor, style, origin);
        painter.line_segment([caret.left_top(), caret.left_bottom()], Stroke::new(2.0_f32, visuals.text_cursor.stroke.color));
    }
}

/// The gutter: breakpoint dots, the debugger's arrow and line numbers.
/// Clicking it toggles a breakpoint on that line.
#[allow(clippy::too_many_arguments)]
fn gutter(ui: &Ui, editor: &mut Editor, style: &Style, id: egui::Id, gutter: Rect, viewport: Rect, scroll_y: f32, numbers: bool) {
    let line_count = editor.buffer().line_count();
    let line_at = |y: f32| ((y - viewport.top() + scroll_y) / style.row_height).floor();
    let area = Rect::from_x_y_ranges(gutter.x_range(), viewport.y_range());
    let response = ui.interact(area, id, Sense::click());
    let hovered_line = response
        .hover_pos()
        .map(|pos| line_at(pos.y))
        .filter(|&line| line >= 0.0 && (line as usize) < line_count)
        .map(|line| line as usize);
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if response.clicked() {
        if let Some(line) = hovered_line {
            editor.toggle_breakpoint(line);
        }
    }

    let painter = ui.painter_at(gutter);
    let weak = ui.visuals().weak_text_color();
    let strong = ui.visuals().strong_text_color();
    let head_line = editor.buffer().line_of(editor.selection.head);
    let marker_x = gutter.left() + marker_width(style) * 0.5;
    let radius = (style.row_height * 0.33).min(7.0);

    let first = (scroll_y / style.row_height).floor().max(0.0) as usize;
    let last = (((scroll_y + viewport.height()) / style.row_height).ceil() as usize).min(line_count - 1);
    for line in first..=last {
        let y = viewport.top() + line as f32 * style.row_height - scroll_y;
        let center = pos2(marker_x, y + style.row_height * 0.5);
        match editor.breakpoint_at(line) {
            // Placed by the debugger, or not debugging: a solid dot.
            Some(bp) if bp.verified != Some(false) => {
                painter.circle_filled(center, radius, BREAKPOINT_COLOR);
            }
            // The debugger couldn't place it (no code there, or not loaded yet).
            Some(_) => {
                painter.circle(center, radius - 0.75, Color32::TRANSPARENT, Stroke::new(1.5_f32, BREAKPOINT_COLOR));
            }
            None if hovered_line == Some(line) => {
                painter.circle_filled(center, radius, BREAKPOINT_COLOR.gamma_multiply(0.35));
            }
            None => {}
        }
        if editor.exec_line() == Some(line) {
            paint_exec_arrow(&painter, center, radius);
        }
        if numbers {
            let color = if line == head_line { strong } else { weak };
            painter.text(
                pos2(gutter.right() - style.char_width * 0.5, y),
                Align2::RIGHT_TOP,
                (line + 1).to_string(),
                style.font.clone(),
                color,
            );
        }
    }
}

/// The yellow arrow pointing at the line the debugger is stopped on.
fn paint_exec_arrow(painter: &egui::Painter, center: Pos2, radius: f32) {
    let h = radius * 1.15; // half height
    let left = center.x - radius * 1.3;
    let right = center.x + radius * 1.3;
    let neck = center.x + radius * 0.1;
    let points = vec![
        pos2(left, center.y - h * 0.45),
        pos2(neck, center.y - h * 0.45),
        pos2(neck, center.y - h),
        pos2(right, center.y),
        pos2(neck, center.y + h),
        pos2(neck, center.y + h * 0.45),
        pos2(left, center.y + h * 0.45),
    ];
    // egui fills convex shapes only: draw the shaft and the head separately.
    painter.add(egui::Shape::convex_polygon(
        vec![points[0], points[1], points[5], points[6]],
        EXEC_COLOR,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(vec![points[2], points[3], points[4]], EXEC_COLOR, Stroke::NONE));
    let outline = Stroke::new(1.0_f32, Color32::from_rgb(0x80, 0x60, 0x00));
    painter.add(egui::Shape::closed_line(points, outline));
}

// ---- Mouse -------------------------------------------------------------------

/// Returns true for Ctrl+click, which asks to go to the definition.
fn handle_pointer(ui: &Ui, response: &egui::Response, editor: &mut Editor, style: &Style, origin: Pos2) -> bool {
    let Some(pos) = response.interact_pointer_pos() else { return false };
    let offset = offset_at_pos(ui, editor, style, origin, pos);
    let (pressed, secondary, shift, ctrl) = ui.input(|i| {
        (i.pointer.primary_pressed(), i.pointer.secondary_pressed(), i.modifiers.shift, i.modifiers.command)
    });

    if secondary {
        // Right-click outside the selection moves the caret, so the context
        // menu acts on what was clicked.
        if !editor.selection.range().contains(&offset) {
            editor.move_head(offset, false);
        }
        return false;
    }
    if pressed && ctrl {
        editor.move_head(offset, false);
        editor.desired_x = None;
        return true;
    }

    if response.triple_clicked() {
        let line = editor.buffer().line_of(offset);
        let range = editor.buffer().line_range(line);
        let end = if line + 1 < editor.buffer().line_count() { range.end + 1 } else { range.end };
        editor.select(Selection { anchor: range.start, head: end });
    } else if response.double_clicked() {
        let word = word_at(editor.text(), offset);
        editor.select(Selection { anchor: word.start, head: word.end });
    } else if pressed {
        editor.move_head(offset, shift);
        editor.desired_x = None;
    } else if response.dragged_by(PointerButton::Primary) && editor.selection.head != offset {
        editor.move_head(offset, true);
        editor.desired_x = None;
    }
    false
}

// ---- Keyboard ----------------------------------------------------------------

/// Keys the editor handles; everything else is left for the rest of the app.
fn is_editor_key(key: Key, modifiers: Modifiers) -> bool {
    use Key::*;
    match key {
        ArrowLeft | ArrowRight | ArrowUp | ArrowDown | Home | End | PageUp | PageDown | Backspace | Delete
        | Enter | Tab | Escape | F12 => true,
        A | Z | Y => modifiers.command,
        _ => false,
    }
}

/// Returns true if F12 asked to go to the definition.
fn handle_keyboard(ui: &mut Ui, editor: &mut Editor, style: &Style, page_rows: usize) -> bool {
    let mut go_to_definition = false;
    // Take our events out of the input so nothing else (e.g. the scroll area)
    // reacts to them too.
    let events = ui.input_mut(|input| {
        let (ours, rest) = std::mem::take(&mut input.events).into_iter().partition(|event| match event {
            Event::Text(_) | Event::Paste(_) | Event::Copy | Event::Cut | Event::Ime(_) => true,
            Event::Key { key, modifiers, .. } => is_editor_key(*key, *modifiers),
            _ => false,
        });
        input.events = rest;
        ours
    });

    for event in events {
        match event {
            Event::Text(text) | Event::Ime(ImeEvent::Commit(text)) if !text.is_empty() => {
                editor.insert(&text, EditKind::Typing);
            }
            Event::Paste(text) => editor.insert(&text.replace("\r\n", "\n"), EditKind::Other),
            Event::Copy => {
                if !editor.selection.is_empty() {
                    ui.ctx().copy_text(editor.selected_text().to_owned());
                }
            }
            Event::Cut => {
                if !editor.selection.is_empty() {
                    ui.ctx().copy_text(editor.selected_text().to_owned());
                    editor.delete(editor.selection.range(), EditKind::Other);
                }
            }
            Event::Key { key: Key::F12, pressed: true, .. } => go_to_definition = true,
            Event::Key { key, pressed: true, modifiers, .. } => {
                handle_key(ui, editor, style, page_rows, key, modifiers);
            }
            _ => {}
        }
    }
    go_to_definition
}

fn handle_key(ui: &Ui, editor: &mut Editor, style: &Style, page_rows: usize, key: Key, modifiers: Modifiers) {
    let shift = modifiers.shift;
    let by_word = modifiers.command;
    let selection = editor.selection;
    let head = selection.head;
    let line = editor.buffer().line_of(head);
    let line_range = editor.buffer().line_range(line);
    let len = editor.buffer().len();

    match key {
        Key::ArrowLeft | Key::ArrowRight => {
            let left = key == Key::ArrowLeft;
            let to = if !shift && !selection.is_empty() {
                // Collapse the selection to the side we're moving toward.
                if left { selection.range().start } else { selection.range().end }
            } else {
                let text = editor.text();
                match (left, by_word) {
                    (true, true) => prev_word(text, head),
                    (true, false) => prev_char(text, head),
                    (false, true) => next_word(text, head),
                    (false, false) => next_char(text, head),
                }
            };
            editor.move_head(to, shift);
            editor.desired_x = None;
        }
        Key::ArrowUp => vertical_move(ui, editor, style, -1, shift),
        Key::ArrowDown => vertical_move(ui, editor, style, 1, shift),
        Key::PageUp => vertical_move(ui, editor, style, -(page_rows as isize), shift),
        Key::PageDown => vertical_move(ui, editor, style, page_rows as isize, shift),
        Key::Home => {
            let to = if by_word {
                0
            } else {
                // Toggle between the first non-blank character and the line start.
                let text = editor.buffer().line(line);
                let indent_end = line_range.start + (text.len() - text.trim_start().len());
                if head == indent_end { line_range.start } else { indent_end }
            };
            editor.move_head(to, shift);
            editor.desired_x = None;
        }
        Key::End => {
            editor.move_head(if by_word { len } else { line_range.end }, shift);
            editor.desired_x = None;
        }
        Key::Backspace => {
            if !selection.is_empty() {
                editor.delete(selection.range(), EditKind::Deleting);
            } else if head > 0 {
                let from = if by_word { prev_word(editor.text(), head) } else { prev_char(editor.text(), head) };
                editor.delete(from..head, EditKind::Deleting);
            }
        }
        Key::Delete => {
            if !selection.is_empty() {
                editor.delete(selection.range(), EditKind::Deleting);
            } else if head < len {
                let to = if by_word { next_word(editor.text(), head) } else { next_char(editor.text(), head) };
                editor.delete(head..to, EditKind::Deleting);
            }
        }
        Key::Enter => {
            // Keep the current line's indentation.
            let start = selection.range().start;
            let text = editor.buffer().line(editor.buffer().line_of(start));
            let indent: String = text.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let column = start - editor.buffer().line_range(editor.buffer().line_of(start)).start;
            let indent = &indent[..indent.len().min(column)];
            editor.insert(&format!("\n{indent}"), EditKind::Other);
        }
        Key::Tab => {
            let lines = selected_lines(editor);
            if shift {
                outdent(editor, lines);
            } else if lines.start() != lines.end() {
                indent(editor, lines);
            } else {
                let unit = editor.indent_unit;
                editor.insert(unit, EditKind::Other);
            }
        }
        Key::Escape => {
            if !selection.is_empty() {
                editor.move_head(head, false);
            }
        }
        Key::A => editor.select(Selection { anchor: 0, head: len }),
        Key::Z if shift => editor.redo(),
        Key::Z => editor.undo(),
        Key::Y => editor.redo(),
        _ => {}
    }
}

/// Move the caret `rows` lines up (negative) or down, keeping its horizontal position.
fn vertical_move(ui: &Ui, editor: &mut Editor, style: &Style, rows: isize, extend: bool) {
    let head = editor.selection.head;
    let line = editor.buffer().line_of(head);
    let x = editor.desired_x.unwrap_or_else(|| x_of(ui, editor, style, head));
    let target = line as isize + rows;
    let to = if target < 0 {
        0
    } else if target as usize >= editor.buffer().line_count() {
        editor.buffer().len()
    } else {
        offset_at(ui, editor, style, target as usize, x)
    };
    editor.move_head(to, extend);
    editor.desired_x = Some(x);
}

/// Lines touched by the selection. A selection ending at the very start of a
/// line doesn't include that line.
fn selected_lines(editor: &Editor) -> RangeInclusive<usize> {
    let range = editor.selection.range();
    let first = editor.buffer().line_of(range.start);
    let mut last = editor.buffer().line_of(range.end);
    if last > first && range.end == editor.buffer().line_range(last).start {
        last -= 1;
    }
    first..=last
}

fn indent(editor: &mut Editor, lines: RangeInclusive<usize>) {
    let unit = editor.indent_unit;
    let changes: Vec<_> = lines
        .clone()
        .map(|line| {
            let start = editor.buffer().line_range(line).start;
            (start..start, unit.to_owned())
        })
        .collect();
    let added = unit.len() * changes.len();
    let anchor = editor.buffer().line_range(*lines.start()).start;
    let head = editor.buffer().line_range(*lines.end()).end + added;
    editor.edit(changes, Selection { anchor, head }, EditKind::Other);
}

fn outdent(editor: &mut Editor, lines: RangeInclusive<usize>) {
    let mut removed = 0;
    let changes: Vec<_> = lines
        .clone()
        .filter_map(|line| {
            let text = editor.buffer().line(line);
            let count = if text.starts_with('\t') {
                1
            } else {
                text.bytes().take(4).take_while(|&b| b == b' ').count()
            };
            let start = editor.buffer().line_range(line).start;
            removed += count;
            (count > 0).then(|| (start..start + count, String::new()))
        })
        .collect();
    let anchor = editor.buffer().line_range(*lines.start()).start;
    let head = editor.buffer().line_range(*lines.end()).end - removed;
    editor.edit(changes, Selection { anchor, head }, EditKind::Other);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draw the editor headlessly; returns the shapes painted.
    fn render(editor: &mut Editor, ctx: &egui::Context, events: Vec<Event>) -> Vec<egui::Shape> {
        let screen = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let prefs = Preferences::default();
        let output = ctx.run(egui::RawInput { screen_rect: Some(screen), events, ..Default::default() }, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                editor_view(ui, editor, "test_editor", &prefs, None);
            });
        });
        output.shapes.into_iter().map(|clipped| clipped.shape).collect()
    }

    fn filled_rects(shapes: &[egui::Shape], color: Color32) -> Vec<Rect> {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Rect(r) if r.fill == color => Some(r.rect),
                _ => None,
            })
            .collect()
    }

    fn circles(shapes: &[egui::Shape]) -> Vec<egui::epaint::CircleShape> {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Circle(c) => Some(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn gutter_click_toggles_and_markers_are_drawn() {
        let text: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let mut editor = Editor::new(text, None);
        let ctx = egui::Context::default();
        let shapes = render(&mut editor, &ctx, vec![]);
        assert!(circles(&shapes).is_empty());

        // Find line 4's number, then click in the margin to its left.
        let row = ctx.fonts(|f| f.row_height(&FontId::monospace(Preferences::default().font_size)));
        let number = shapes
            .iter()
            .find_map(|s| match s {
                egui::Shape::Text(t) if t.galley.text() == "5" => Some(t.pos),
                _ => None,
            })
            .expect("line number 5 drawn");
        let click = pos2(4.0 + row * 0.5, number.y + row * 0.5);
        let button = |pressed| Event::PointerButton { pos: click, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        render(&mut editor, &ctx, vec![Event::PointerMoved(click), button(true)]);
        render(&mut editor, &ctx, vec![button(false)]);
        assert_eq!(editor.breakpoint_lines(), [4]);

        // A red dot in the margin, a red tint across the line.
        let shapes = render(&mut editor, &ctx, vec![]);
        let dots: Vec<_> = circles(&shapes).into_iter().filter(|c| c.fill == BREAKPOINT_COLOR).collect();
        assert_eq!(dots.len(), 1);
        assert!((dots[0].center.y - click.y).abs() < 1.0, "{dots:?} vs {click:?}");
        let tints = filled_rects(&shapes, BREAKPOINT_LINE);
        assert_eq!(tints.len(), 1);
        assert!(tints[0].width() > 300.0 && (tints[0].center().y - click.y).abs() < 1.0);

        // Stopped there: the yellow arrow and tint.
        editor.set_exec_line(Some(4));
        let shapes = render(&mut editor, &ctx, vec![]);
        let arrow = shapes.iter().any(|s| matches!(s, egui::Shape::Path(p) if p.fill == EXEC_COLOR));
        assert!(arrow, "no arrow drawn");
        assert_eq!(filled_rects(&shapes, EXEC_LINE).len(), 1);

        // An unplaced breakpoint is a ring, not a dot.
        editor.set_breakpoint_verified(4, Some(false));
        let shapes = render(&mut editor, &ctx, vec![]);
        assert!(circles(&shapes).iter().any(|c| c.fill == Color32::TRANSPARENT && c.stroke.color == BREAKPOINT_COLOR));

        // Clicking again removes it.
        render(&mut editor, &ctx, vec![Event::PointerMoved(click), button(true)]);
        render(&mut editor, &ctx, vec![button(false)]);
        assert!(editor.breakpoint_lines().is_empty());
    }
}
