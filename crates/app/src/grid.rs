//! The data grid: a custom-painted, virtualized table. Only visible cells are laid out, so
//! row count does not affect frame time. Supports sticky header and row numbers, column
//! resizing, row/cell selection, keyboard navigation, inline editing and context menus.

#![allow(clippy::needless_range_loop)]

use std::borrow::Cow;
use std::collections::BTreeSet;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, Event, EventFilter, FontId, Id, Key, Modifiers, Pos2, Rect, Sense,
    Stroke, StrokeKind, TextEdit, Ui, pos2, scroll_area::ScrollSource, vec2,
};
use tableplus_core::model::ColumnMeta;
use tableplus_core::sqlgen::CellEdit;

use crate::icons::{self, Icon};
use crate::theme::{self, palette};

pub const ROW_H: f32 = 24.0;
pub const HEADER_H: f32 = 28.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellState {
    Normal,
    Modified,
    Inserted,
    Deleted,
}

pub struct CellView<'a> {
    pub text: Cow<'a, str>,
    pub null: bool,
    pub numeric: bool,
    pub state: CellState,
    /// Placeholder shown for unset cells of new rows (`DEFAULT`).
    pub placeholder: bool,
}

/// Read access to whatever the grid displays (a query result, or a table page with staged edits).
pub trait GridData {
    fn columns(&self) -> &[ColumnMeta];
    fn row_count(&self) -> usize;
    fn cell(&self, row: usize, col: usize) -> CellView<'_>;
    fn row_state(&self, row: usize) -> CellState;
    /// Full text used when editing or copying; `None` for NULL.
    fn full_text(&self, row: usize, col: usize) -> Option<String>;
    fn editable(&self) -> bool {
        false
    }
    fn sort_of(&self, _col: usize) -> Option<bool> {
        None
    }
    fn is_key(&self, _col: usize) -> bool {
        false
    }
    /// Absolute row number shown in the gutter (accounts for paging).
    fn row_number(&self, row: usize) -> u64 {
        row as u64 + 1
    }
}

pub struct CellEditor {
    pub row: usize,
    pub col: usize,
    pub text: String,
    original: Option<String>,
    /// Frames spent requesting focus; the editor is abandoned if it never gets it.
    focus_attempts: u8,
    had_focus: bool,
}

#[derive(Default)]
pub struct GridState {
    pub widths: Vec<f32>,
    width_key: Vec<String>,
    /// Columns the user resized by hand (never auto-sized again).
    user_sized: Vec<bool>,
    remeasure: bool,
    pub focus: Option<(usize, usize)>,
    pub selected: BTreeSet<usize>,
    anchor: Option<usize>,
    pub editing: Option<CellEditor>,
    scroll_to_focus: bool,
    menu_cell: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Tsv,
    Csv,
    Json,
    Insert,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GridEvent {
    Edit { row: usize, col: usize, value: CellEdit },
    Sort { col: usize, add: bool },
    DeleteRows(Vec<usize>),
    UndeleteRows(Vec<usize>),
    DuplicateRow(usize),
    AddRow,
    FilterByValue { col: usize, value: Option<String> },
    CopyAs { rows: Vec<usize>, format: CopyFormat },
}

impl GridState {
    /// Resets selection and widths for a new column set.
    pub fn reset(&mut self) {
        *self = GridState::default();
    }

    pub fn selected_rows(&self) -> Vec<usize> {
        if self.selected.is_empty() {
            self.focus.map(|(r, _)| vec![r]).unwrap_or_default()
        } else {
            self.selected.iter().copied().collect()
        }
    }

    pub fn select_row(&mut self, row: usize, col: usize) {
        self.focus = Some((row, col));
        self.selected.clear();
        self.selected.insert(row);
        self.anchor = Some(row);
        self.scroll_to_focus = true;
    }

    pub fn clamp(&mut self, rows: usize, cols: usize) {
        self.selected.retain(|r| *r < rows);
        if let Some((r, c)) = self.focus {
            if rows == 0 || cols == 0 {
                self.focus = None;
            } else {
                self.focus = Some((r.min(rows - 1), c.min(cols - 1)));
            }
        }
        if self.editing.as_ref().is_some_and(|e| e.row >= rows || e.col >= cols) {
            self.editing = None;
        }
    }

    /// Re-measures auto-sized columns after new rows arrive; columns only grow.
    pub fn remeasure(&mut self) {
        self.remeasure = true;
    }

    fn ensure_widths(&mut self, ui: &Ui, data: &dyn GridData) {
        let names: Vec<String> = data.columns().iter().map(|c| c.name.clone()).collect();
        let fresh = self.widths.len() != names.len() || self.width_key != names;
        if !fresh && !std::mem::take(&mut self.remeasure) {
            return;
        }
        let sample = data.row_count().min(200);
        let char_w = ui.fonts_mut(|f| f.glyph_width(&theme::ui_font(13.0), '0')).max(6.0);
        let measured: Vec<f32> = (0..names.len())
            .map(|c| {
                let header = names[c].chars().count() as f32 * (char_w + 0.6) + 34.0;
                let longest = (0..sample).map(|r| data.cell(r, c).text.chars().count()).max().unwrap_or(4) as f32;
                (longest * char_w + 22.0).max(header).clamp(64.0, 380.0)
            })
            .collect();
        if fresh {
            self.widths = measured;
            self.user_sized = vec![false; names.len()];
            self.width_key = names;
        } else {
            for (c, w) in measured.into_iter().enumerate() {
                if !self.user_sized.get(c).copied().unwrap_or(false) {
                    self.widths[c] = self.widths[c].max(w);
                }
            }
        }
    }

    pub fn start_edit(&mut self, data: &dyn GridData, row: usize, col: usize) {
        if !data.editable() || data.row_state(row) == CellState::Deleted {
            return;
        }
        let original = data.full_text(row, col);
        self.editing = Some(CellEditor {
            row,
            col,
            text: original.clone().unwrap_or_default(),
            original,
            focus_attempts: 0,
            had_focus: false,
        });
    }
}

fn gutter_width(rows: u64) -> f32 {
    let digits = rows.max(1).to_string().len() as f32;
    (digits * 7.5 + 18.0).max(40.0)
}

fn layout_text(ui: &Ui, text: &str, font: FontId, color: Color32, italics: bool) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_string(),
        egui::TextFormat { font_id: font, color, italics, ..Default::default() },
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

/// Draws the grid and returns what the user did.
pub fn show(ui: &mut Ui, id: Id, state: &mut GridState, data: &dyn GridData) -> Vec<GridEvent> {
    let mut events = Vec::new();
    let p = palette(ui);
    let dark = ui.visuals().dark_mode;
    let ncols = data.columns().len();
    let nrows = data.row_count();
    state.ensure_widths(ui, data);
    state.clamp(nrows, ncols);
    let last_number = if nrows == 0 { 0 } else { data.row_number(nrows - 1) };
    let gutter = gutter_width(last_number);
    let col_x: Vec<f32> = state
        .widths
        .iter()
        .scan(gutter, |x, w| {
            let start = *x;
            *x += w;
            Some(start)
        })
        .collect();
    let total_w = gutter + state.widths.iter().sum::<f32>();
    let total_h = HEADER_H + nrows as f32 * ROW_H;
    let has_focus = ui.memory(|m| m.has_focus(id));
    let editable = data.editable();

    // Keyboard handling (only while the grid itself has focus).
    if has_focus && state.editing.is_none() && ncols > 0 {
        let (mods, keys, copy) = ui.input(|i| {
            let keys: Vec<Key> = [
                Key::ArrowUp,
                Key::ArrowDown,
                Key::ArrowLeft,
                Key::ArrowRight,
                Key::Enter,
                Key::F2,
                Key::Delete,
                Key::Backspace,
                Key::PageUp,
                Key::PageDown,
                Key::Home,
                Key::End,
                Key::Escape,
            ]
            .into_iter()
            .filter(|k| i.key_pressed(*k))
            .collect();
            (i.modifiers, keys, i.events.iter().any(|e| matches!(e, Event::Copy)))
        });
        let select_all = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A));
        let (mut r, mut c) = state.focus.unwrap_or((0, 0));
        let page = (ui.available_height() / ROW_H).max(1.0) as usize;
        let mut moved = false;
        for k in keys {
            match k {
                Key::ArrowUp => {
                    r = r.saturating_sub(1);
                    moved = true;
                }
                Key::ArrowDown => {
                    r = (r + 1).min(nrows.saturating_sub(1));
                    moved = true;
                }
                Key::ArrowLeft => {
                    c = c.saturating_sub(1);
                    moved = true;
                }
                Key::ArrowRight => {
                    c = (c + 1).min(ncols - 1);
                    moved = true;
                }
                Key::PageUp => {
                    r = r.saturating_sub(page);
                    moved = true;
                }
                Key::PageDown => {
                    r = (r + page).min(nrows.saturating_sub(1));
                    moved = true;
                }
                Key::Home => {
                    r = 0;
                    moved = true;
                }
                Key::End => {
                    r = nrows.saturating_sub(1);
                    moved = true;
                }
                Key::Enter | Key::F2 if nrows > 0 => state.start_edit(data, r, c),
                Key::Delete | Key::Backspace if editable && nrows > 0 => {
                    events.push(GridEvent::DeleteRows(state.selected_rows()))
                }
                Key::Escape => {
                    state.selected.clear();
                }
                _ => {}
            }
        }
        if moved && nrows > 0 {
            if mods.shift {
                let anchor = state.anchor.unwrap_or(r);
                state.selected = (anchor.min(r)..=anchor.max(r)).collect();
                state.focus = Some((r, c));
                state.scroll_to_focus = true;
            } else {
                state.select_row(r, c);
            }
        }
        if select_all {
            state.selected = (0..nrows).collect();
        }
        if copy {
            let rows = state.selected_rows();
            if rows.len() > 1 || (mods.shift && !rows.is_empty()) {
                events.push(GridEvent::CopyAs { rows, format: CopyFormat::Tsv });
            } else if let Some((r, c)) = state.focus {
                ui.ctx().copy_text(data.full_text(r, c).unwrap_or_else(|| "NULL".into()));
            }
        }
    }

    let mut scroll = egui::ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::MOUSE_WHEEL);
    // Keep the focused cell visible after keyboard navigation.
    if std::mem::take(&mut state.scroll_to_focus)
        && let Some((r, c)) = state.focus
    {
        let viewport = ui.available_size();
        let st = egui::scroll_area::State::load(ui.ctx(), ui.make_persistent_id(id.with("scroll")));
        if let Some(st) = st {
            let mut off = st.offset;
            let top = r as f32 * ROW_H;
            let bottom = top + ROW_H + HEADER_H;
            if top < off.y {
                off.y = top;
            } else if bottom > off.y + viewport.y - 10.0 {
                off.y = bottom - viewport.y + 10.0;
            }
            if c < col_x.len() {
                let left = col_x[c] - gutter;
                let right = col_x[c] + state.widths[c];
                if left < off.x {
                    off.x = left;
                } else if right > off.x + viewport.x - 10.0 {
                    off.x = right - viewport.x + 10.0;
                }
            }
            scroll = scroll.scroll_offset(off);
        }
    }

    scroll.show_viewport(ui, |ui, viewport| {
        let content_h = total_h.max(viewport.height());
        ui.set_min_size(vec2(total_w.max(viewport.width()), content_h));
        let origin = ui.max_rect().min;
        let clip = ui.clip_rect();
        let painter = ui.painter().clone();
        let body_top = clip.top() + HEADER_H;

        // Body interaction (header/gutter widgets are added later and win their areas).
        let body_rect = Rect::from_min_max(pos2(clip.left() + gutter, body_top), clip.max);
        let body = ui.interact(body_rect, id, Sense::click());
        if body.clicked() || body.secondary_clicked() || body.double_clicked() {
            body.request_focus();
        }
        if body.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        let widths = state.widths.clone();
        let hit = |pos: Pos2| -> Option<(usize, usize)> {
            if pos.y < body_top || pos.x < clip.left() + gutter {
                return None;
            }
            let row = ((pos.y - origin.y - HEADER_H) / ROW_H).floor();
            if row < 0.0 || row as usize >= nrows {
                return None;
            }
            let x = pos.x - origin.x;
            let col = col_x.iter().zip(&widths).position(|(start, w)| x >= *start && x < start + w)?;
            Some((row as usize, col))
        };
        if let Some(pos) = body.interact_pointer_pos() {
            if (body.clicked() || body.secondary_clicked())
                && let Some((r, c)) = hit(pos)
            {
                let mods = ui.input(|i| i.modifiers);
                if body.secondary_clicked() {
                    if !state.selected.contains(&r) {
                        state.select_row(r, c);
                    } else {
                        state.focus = Some((r, c));
                    }
                    state.menu_cell = Some((r, c));
                } else if mods.shift {
                    let anchor = state.anchor.unwrap_or(r);
                    state.selected = (anchor.min(r)..=anchor.max(r)).collect();
                    state.focus = Some((r, c));
                } else if mods.command {
                    if !state.selected.insert(r) {
                        state.selected.remove(&r);
                    }
                    state.focus = Some((r, c));
                    state.anchor = Some(r);
                } else {
                    state.select_row(r, c);
                    state.scroll_to_focus = false;
                }
            }
            if body.double_clicked()
                && let Some((r, c)) = hit(pos)
            {
                state.start_edit(data, r, c);
            }
        }

        // Visible range.
        let first = ((viewport.min.y) / ROW_H).floor().max(0.0) as usize;
        let last = (((viewport.max.y - HEADER_H) / ROW_H).ceil().max(0.0) as usize + 1).min(nrows);
        let first_col =
            col_x.iter().zip(&state.widths).position(|(x, w)| x + w >= viewport.min.x + gutter).unwrap_or(ncols);
        let last_col = col_x.iter().position(|x| *x > viewport.max.x).unwrap_or(ncols);
        let cell_font = theme::ui_font(13.0);
        let grid_stroke = Stroke::new(1.0, p.grid_line);

        for r in first..last {
            let y = origin.y + HEADER_H + r as f32 * ROW_H;
            let row_rect = Rect::from_min_size(pos2(clip.left(), y), vec2(clip.width(), ROW_H));
            let row_state = data.row_state(r);
            let selected = state.selected.contains(&r);
            let mut bg = if r % 2 == 1 { p.row_alt } else { p.content_bg };
            match row_state {
                CellState::Inserted => bg = p.inserted_bg,
                CellState::Deleted => bg = p.deleted_bg,
                _ => {}
            }
            if selected {
                bg = if has_focus { p.selection } else { p.selection_inactive };
            }
            painter.rect_filled(
                Rect::from_min_size(pos2(origin.x + gutter, y), vec2(total_w - gutter, ROW_H)),
                CornerRadius::ZERO,
                bg,
            );
            let on_selection = selected && has_focus;
            for c in first_col..last_col {
                let x = origin.x + col_x[c];
                let cell = Rect::from_min_size(pos2(x, y), vec2(state.widths[c], ROW_H));
                let view = data.cell(r, c);
                if view.state == CellState::Modified && !on_selection {
                    painter.rect_filled(cell.shrink2(vec2(0.5, 0.0)), CornerRadius::ZERO, p.modified_bg);
                }
                let mut color = if on_selection { p.accent_text } else { p.text };
                if view.null || view.placeholder {
                    color = if on_selection { p.accent_text.gamma_multiply(0.7) } else { p.null_text };
                }
                if row_state == CellState::Deleted && !on_selection {
                    color = p.deleted_text;
                }
                let inner = cell.shrink2(vec2(7.0, 0.0));
                let galley = layout_text(ui, &view.text, cell_font.clone(), color, view.null || view.placeholder);
                let gx = if view.numeric && !view.null { inner.right() - galley.size().x } else { inner.left() };
                let gpos = pos2(gx.max(inner.left()), cell.center().y - galley.size().y / 2.0);
                let cp = painter.with_clip_rect(inner.intersect(clip));
                if row_state == CellState::Deleted {
                    let w = galley.size().x.min(inner.width());
                    cp.line_segment(
                        [pos2(gpos.x, cell.center().y), pos2(gpos.x + w, cell.center().y)],
                        Stroke::new(1.0, color),
                    );
                }
                cp.galley(gpos, galley, color);
                painter.line_segment([cell.right_top(), cell.right_bottom()], grid_stroke);
            }
            let _ = row_rect;
        }
        // Focused cell outline.
        if let Some((r, c)) = state.focus
            && (first..last).contains(&r)
            && c < ncols
            && state.editing.is_none()
        {
            let cell = Rect::from_min_size(
                pos2(origin.x + col_x[c], origin.y + HEADER_H + r as f32 * ROW_H),
                vec2(state.widths[c], ROW_H),
            );
            let color = if has_focus { if dark { Color32::WHITE } else { p.accent } } else { p.text_faint };
            painter.with_clip_rect(Rect::from_min_max(pos2(clip.left() + gutter, body_top), clip.max)).rect_stroke(
                cell.shrink(1.0),
                CornerRadius::same(2),
                Stroke::new(1.5, color),
                StrokeKind::Inside,
            );
        }

        // Inline editor.
        if let Some(ed) = &mut state.editing {
            let r = ed.row;
            let c = ed.col;
            let cell = Rect::from_min_size(
                pos2(origin.x + col_x[c], origin.y + HEADER_H + r as f32 * ROW_H),
                vec2(state.widths[c].max(160.0), ROW_H),
            );
            painter.rect(cell, CornerRadius::same(2), p.field_bg, Stroke::new(1.5, p.accent), StrokeKind::Inside);
            let edit_id = id.with("cell_editor");
            let resp = ui.put(
                cell.shrink2(vec2(4.0, 2.0)),
                TextEdit::singleline(&mut ed.text)
                    .id(edit_id)
                    .frame(egui::Frame::NONE)
                    .font(cell_font.clone())
                    .vertical_align(egui::Align::Center),
            );
            if !ed.had_focus {
                if resp.has_focus() {
                    ed.had_focus = true;
                } else if ed.focus_attempts < 4 {
                    // Focus is requested over a few frames: the click that opened the editor
                    // can otherwise steal it back on the same frame.
                    resp.request_focus();
                    if let Some(mut st) = TextEdit::load_state(ui.ctx(), edit_id) {
                        let end = egui::text::CCursor::new(ed.text.chars().count());
                        st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), end)));
                        TextEdit::store_state(ui.ctx(), edit_id, st);
                    }
                    ed.focus_attempts += 1;
                    ui.ctx().request_repaint();
                } else {
                    state.editing = None;
                }
            } else if !resp.has_focus() {
                let (escape, tab) = ui.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Tab)));
                if !escape && Some(&ed.text) != ed.original.as_ref() && !(ed.original.is_none() && ed.text.is_empty()) {
                    events.push(GridEvent::Edit { row: r, col: c, value: CellEdit::Text(ed.text.clone()) });
                }
                state.editing = None;
                state.focus = Some((r, if tab { (c + 1).min(ncols.saturating_sub(1)) } else { c }));
                body.request_focus();
                if tab {
                    state.start_edit(data, r, (c + 1).min(ncols.saturating_sub(1)));
                }
            }
        }

        // Sticky header.
        let header = Rect::from_min_size(pos2(clip.left(), clip.top()), vec2(clip.width(), HEADER_H));
        painter.rect_filled(header, CornerRadius::ZERO, p.header_bg);
        painter.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        let head_font = theme::bold_font(12.5);
        for c in first_col..last_col {
            let x = origin.x + col_x[c];
            let cell = Rect::from_min_size(pos2(x, clip.top()), vec2(state.widths[c], HEADER_H));
            let meta = &data.columns()[c];
            let resp = ui.interact(cell.intersect(header), id.with(("head", c)), Sense::click());
            if resp.hovered() {
                painter.rect_filled(cell.intersect(header), CornerRadius::ZERO, p.hover.gamma_multiply(0.7));
            }
            let mut left = cell.left() + 8.0;
            if data.is_key(c) {
                icons::paint(
                    &painter,
                    Rect::from_min_size(pos2(left, cell.center().y - 6.0), vec2(12.0, 12.0)),
                    Icon::Key,
                    p.warning,
                );
                left += 15.0;
            }
            let sort = data.sort_of(c);
            let right_pad = if sort.is_some() { 20.0 } else { 8.0 };
            let text_rect = Rect::from_min_max(pos2(left, cell.top()), pos2(cell.right() - right_pad, cell.bottom()));
            painter.with_clip_rect(text_rect.intersect(clip)).text(
                pos2(left, cell.center().y),
                Align2::LEFT_CENTER,
                &meta.name,
                head_font.clone(),
                p.text,
            );
            if let Some(desc) = sort {
                let cx = cell.right() - 11.0;
                let cy = cell.center().y;
                let pts = if desc {
                    vec![pos2(cx - 4.0, cy - 2.0), pos2(cx + 4.0, cy - 2.0), pos2(cx, cy + 3.0)]
                } else {
                    vec![pos2(cx - 4.0, cy + 2.0), pos2(cx + 4.0, cy + 2.0), pos2(cx, cy - 3.0)]
                };
                painter.add(egui::Shape::convex_polygon(pts, p.accent, Stroke::NONE));
            }
            painter.line_segment(
                [cell.right_top() + vec2(0.0, 5.0), cell.right_bottom() - vec2(0.0, 5.0)],
                Stroke::new(1.0, p.border),
            );
            let tip = if meta.type_name.is_empty() {
                meta.name.clone()
            } else {
                format!("{}  ·  {}", meta.name, meta.type_name)
            };
            let resp = resp.on_hover_text(tip);
            if resp.clicked() {
                let add = ui.input(|i| i.modifiers.shift);
                events.push(GridEvent::Sort { col: c, add });
            }
            // Resize handle on the right edge.
            let handle =
                Rect::from_min_max(pos2(cell.right() - 4.0, cell.top()), pos2(cell.right() + 4.0, cell.bottom()));
            let h =
                ui.interact(handle, id.with(("resize", c)), Sense::drag()).on_hover_cursor(CursorIcon::ResizeColumn);
            if h.dragged() {
                state.widths[c] = (state.widths[c] + h.drag_delta().x).clamp(36.0, 2000.0);
                if let Some(u) = state.user_sized.get_mut(c) {
                    *u = true;
                }
                ui.ctx().set_cursor_icon(CursorIcon::ResizeColumn);
            }
            if h.double_clicked() {
                // Double-click on the edge resets to the auto width.
                state.width_key.clear();
            }
        }

        // Sticky row-number gutter.
        let gut = Rect::from_min_max(pos2(clip.left(), body_top), pos2(clip.left() + gutter, clip.bottom()));
        painter.rect_filled(gut, CornerRadius::ZERO, p.header_bg);
        painter.line_segment([gut.right_top(), gut.right_bottom()], Stroke::new(1.0, p.border));
        let num_font = theme::ui_font(11.5);
        let gutter_resp = ui.interact(gut, id.with("gutter"), Sense::click());
        for r in first..last {
            let y = origin.y + HEADER_H + r as f32 * ROW_H;
            if y + ROW_H < body_top {
                continue;
            }
            let cell = Rect::from_min_size(pos2(clip.left(), y), vec2(gutter, ROW_H));
            let selected = state.selected.contains(&r);
            if selected {
                painter.with_clip_rect(gut).rect_filled(cell, CornerRadius::ZERO, p.selection_inactive);
            }
            let label = match data.row_state(r) {
                CellState::Inserted => "+".to_string(),
                _ => data.row_number(r).to_string(),
            };
            let color = match data.row_state(r) {
                CellState::Inserted => p.success,
                CellState::Deleted => p.deleted_text,
                _ if selected => p.text,
                _ => p.text_faint,
            };
            painter.with_clip_rect(gut).text(
                pos2(cell.right() - 8.0, cell.center().y),
                Align2::RIGHT_CENTER,
                label,
                num_font.clone(),
                color,
            );
        }
        if gutter_resp.clicked()
            && let Some(pos) = gutter_resp.interact_pointer_pos()
        {
            let row = ((pos.y - origin.y - HEADER_H) / ROW_H).floor();
            if row >= 0.0 && (row as usize) < nrows {
                let r = row as usize;
                let mods = ui.input(|i| i.modifiers);
                if mods.shift {
                    let anchor = state.anchor.unwrap_or(r);
                    state.selected = (anchor.min(r)..=anchor.max(r)).collect();
                } else if mods.command {
                    if !state.selected.insert(r) {
                        state.selected.remove(&r);
                    }
                } else {
                    state.select_row(r, state.focus.map_or(0, |f| f.1));
                    state.scroll_to_focus = false;
                }
                body.request_focus();
            }
        }
        // Corner.
        painter.rect_filled(Rect::from_min_size(clip.min, vec2(gutter, HEADER_H)), CornerRadius::ZERO, p.header_bg);
        painter.line_segment(
            [pos2(clip.left(), clip.top() + HEADER_H), pos2(clip.left() + gutter, clip.top() + HEADER_H)],
            Stroke::new(1.0, p.border),
        );
        painter.line_segment(
            [pos2(clip.left() + gutter, clip.top()), pos2(clip.left() + gutter, clip.top() + HEADER_H)],
            Stroke::new(1.0, p.border),
        );

        if nrows == 0 {
            painter.text(
                pos2(clip.center().x, body_top + 48.0),
                Align2::CENTER_CENTER,
                "No rows",
                theme::ui_font(13.0),
                p.text_faint,
            );
        }

        // Context menu for the right-clicked cell.
        body.context_menu(|ui| {
            let Some((r, c)) = state.menu_cell.or(state.focus) else {
                ui.close();
                return;
            };
            if r >= nrows || c >= ncols {
                ui.close();
                return;
            }
            let rows = state.selected_rows();
            let column = data.columns()[c].name.clone();
            ui.set_min_width(200.0);
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(data.full_text(r, c).unwrap_or_else(|| "NULL".into()));
                ui.close();
            }
            ui.menu_button("Copy rows as", |ui| {
                for (label, format) in [
                    ("Tab-separated", CopyFormat::Tsv),
                    ("CSV", CopyFormat::Csv),
                    ("JSON", CopyFormat::Json),
                    ("SQL INSERT", CopyFormat::Insert),
                ] {
                    if ui.button(label).clicked() {
                        events.push(GridEvent::CopyAs { rows: rows.clone(), format });
                        ui.close();
                    }
                }
            });
            if ui.button(format!("Copy column name \"{column}\"")).clicked() {
                ui.ctx().copy_text(column.clone());
                ui.close();
            }
            ui.separator();
            let value = data.full_text(r, c);
            let shown: String = value.as_deref().unwrap_or("NULL").chars().take(24).collect();
            if ui.button(format!("Filter {column} = {shown}")).clicked() {
                events.push(GridEvent::FilterByValue { col: c, value });
                ui.close();
            }
            if editable {
                ui.separator();
                if ui.button("Edit cell").clicked() {
                    state.start_edit(data, r, c);
                    ui.close();
                }
                if ui.button("Set NULL").clicked() {
                    for row in &rows {
                        events.push(GridEvent::Edit { row: *row, col: c, value: CellEdit::Null });
                    }
                    ui.close();
                }
                if data.row_state(r) == CellState::Inserted && ui.button("Set DEFAULT").clicked() {
                    events.push(GridEvent::Edit { row: r, col: c, value: CellEdit::Default });
                    ui.close();
                }
                if ui.button("Set empty string").clicked() {
                    for row in &rows {
                        events.push(GridEvent::Edit { row: *row, col: c, value: CellEdit::Text(String::new()) });
                    }
                    ui.close();
                }
                ui.separator();
                if ui.button("Add row").clicked() {
                    events.push(GridEvent::AddRow);
                    ui.close();
                }
                if ui.button("Duplicate row").clicked() {
                    events.push(GridEvent::DuplicateRow(r));
                    ui.close();
                }
                let deleted = rows.iter().all(|row| data.row_state(*row) == CellState::Deleted);
                let label = if rows.len() > 1 {
                    format!("{} {} rows", if deleted { "Restore" } else { "Delete" }, rows.len())
                } else if deleted {
                    "Restore row".into()
                } else {
                    "Delete row".into()
                };
                if ui
                    .button(egui::RichText::new(label).color(if deleted {
                        palette(ui).text
                    } else {
                        palette(ui).danger
                    }))
                    .clicked()
                {
                    events.push(if deleted {
                        GridEvent::UndeleteRows(rows.clone())
                    } else {
                        GridEvent::DeleteRows(rows.clone())
                    });
                    ui.close();
                }
            }
        });
    });
    events
}

/// Grid over a plain query result (read-only).
pub struct ResultData<'a> {
    pub rs: &'a tableplus_core::ResultSet,
}

impl GridData for ResultData<'_> {
    fn columns(&self) -> &[ColumnMeta] {
        &self.rs.columns
    }
    fn row_count(&self) -> usize {
        self.rs.rows.len()
    }
    fn cell(&self, row: usize, col: usize) -> CellView<'_> {
        let v = &self.rs.rows[row][col];
        CellView {
            text: v.display(),
            null: v.is_null(),
            numeric: v.is_numeric(),
            state: CellState::Normal,
            placeholder: false,
        }
    }
    fn row_state(&self, _row: usize) -> CellState {
        CellState::Normal
    }
    fn full_text(&self, row: usize, col: usize) -> Option<String> {
        self.rs.rows[row][col].to_text()
    }
}
