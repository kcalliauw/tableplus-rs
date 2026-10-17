//! The Plan pane: an EXPLAIN plan as a collapsible tree, one row per step with its estimated
//! rows, actual rows, loops and a time bar, a resizable detail panel for the selected step, and
//! the server's raw output. Rows are custom-painted and only the visible ones are laid out, like
//! the data grid, so long and deeply nested plans stay smooth.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, EventFilter, FontId, Galley, Id, Key, Painter, Pos2, Rect, RichText, Sense,
    Shape, Stroke, Ui, pos2, scroll_area::ScrollSource, text::LayoutJob, vec2,
};
use tableplus_core::ServerInfo;
use tableplus_core::model::ResultSet;
use tableplus_core::plan::{self, NodeId, Plan, PlanFormat, PlanNode};

use crate::icons::Icon;
use crate::theme::{self, Palette, palette};
use crate::widgets::{self, fmt_count};

const ROW_H: f32 = 26.0;
const HEADER_H: f32 = 28.0;
const PAD: f32 = 10.0;
const CHEVRON: f32 = 16.0;
const BAR_H: f32 = 8.0;
const GROW_SECONDS: f32 = 0.45;

/// What a Plan pane shows.
pub enum PlanContent {
    Tree(Box<Plan>),
    /// The server's output when no parser reads its format, and why.
    Raw {
        text: String,
        reason: String,
    },
}

impl PlanContent {
    /// The output of an Explain action: a tree when a parser reads it, otherwise the raw text.
    pub fn explained(server: &ServerInfo, rows: &ResultSet, format: PlanFormat) -> Self {
        Self::detected(server, rows).unwrap_or_else(|| PlanContent::Raw {
            text: plan::raw_text(rows),
            reason: format!("No visual plan for {} output yet. This is the server's own text.", format.label()),
        })
    }

    /// A plan for any result that is one, such as a hand-typed `EXPLAIN ANALYZE`.
    pub fn detected(server: &ServerInfo, rows: &ResultSet) -> Option<Self> {
        Some(match plan::parse(server, rows)? {
            Ok(plan) => PlanContent::Tree(Box::new(plan)),
            Err(e) => PlanContent::Raw { text: plan::raw_text(rows), reason: format!("{e}.") },
        })
    }
}

/// Derived numbers per node, computed once when the plan arrives.
struct Metrics {
    inclusive_ms: Option<f64>,
    self_ms: Option<f64>,
    descendants: usize,
}

pub struct PlanView {
    content: PlanContent,
    /// Why the plan differs from what was asked for, such as Analyze falling back to estimates.
    note: Option<String>,
    show_raw: bool,
    collapsed: Vec<bool>,
    /// Rows on screen, top to bottom: the nodes whose ancestors are all expanded.
    visible: Vec<NodeId>,
    metrics: Vec<Metrics>,
    /// Bar length for 100%: the largest inclusive time, or the largest cost without actuals.
    scale: f64,
    selected: Option<NodeId>,
    detail_open: bool,
    scroll_to_selected: bool,
    scroll_id: Option<Id>,
    /// When the tree was first drawn; its bars grow in from there.
    shown_at: Option<Instant>,
}

impl PlanView {
    pub fn new(content: PlanContent, note: Option<String>) -> Self {
        let mut view = Self {
            content,
            note,
            show_raw: false,
            collapsed: Vec::new(),
            visible: Vec::new(),
            metrics: Vec::new(),
            scale: 0.0,
            selected: None,
            detail_open: true,
            scroll_to_selected: false,
            scroll_id: None,
            shown_at: None,
        };
        if let PlanContent::Tree(plan) = &view.content {
            view.collapsed = vec![false; plan.node_count()];
            view.metrics = plan
                .nodes()
                .map(|(id, _)| Metrics {
                    inclusive_ms: plan.inclusive_ms(id),
                    self_ms: plan.self_ms(id),
                    descendants: plan.descendant_count(id),
                })
                .collect();
            // Normally the root's time; MySQL 8.0.18 can time a materialization above its root.
            view.scale = if plan.analyzed() {
                view.metrics.iter().filter_map(|m| m.inclusive_ms).fold(plan.total_ms().unwrap_or(0.0), f64::max)
            } else {
                plan.nodes().filter_map(|(_, n)| n.estimate?.cost.map(|c| c.total)).fold(0.0, f64::max)
            };
            view.rebuild_visible();
        }
        view
    }

    pub fn plan(&self) -> Option<&Plan> {
        match &self.content {
            PlanContent::Tree(plan) => Some(plan),
            PlanContent::Raw { .. } => None,
        }
    }

    pub fn raw_text(&self) -> &str {
        match &self.content {
            PlanContent::Tree(plan) => &plan.raw,
            PlanContent::Raw { text, .. } => text,
        }
    }

    /// `56 steps · 4.68 ms`, for the result strip.
    pub fn summary(&self) -> String {
        match self.plan() {
            Some(plan) => {
                let steps = widgets::plural(plan.node_count() as u64, "step");
                match plan.total_ms() {
                    Some(ms) => format!("{steps} · {}", fmt_ms(ms)),
                    None => format!("{steps} · estimated"),
                }
            }
            None => "Server output".into(),
        }
    }

    fn rebuild_visible(&mut self) {
        let Some(plan) = self.plan() else { return };
        let mut visible = Vec::with_capacity(plan.node_count());
        let mut stack = vec![NodeId::ROOT];
        while let Some(id) = stack.pop() {
            visible.push(id);
            if !self.collapsed[id.index()] {
                stack.extend(plan.children(id).iter().rev());
            }
        }
        self.visible = visible;
    }

    fn set_collapsed(&mut self, id: NodeId, collapsed: bool) {
        let Some(plan) = self.plan() else { return };
        if plan.children(id).is_empty() || self.collapsed[id.index()] == collapsed {
            return;
        }
        self.collapsed[id.index()] = collapsed;
        self.rebuild_visible();
    }

    /// Expands or collapses every step with children; collapsing keeps the root open.
    pub fn set_all_collapsed(&mut self, collapsed: bool) {
        let Some(plan) = self.plan() else { return };
        let parents: Vec<bool> = plan.nodes().map(|(id, _)| !plan.children(id).is_empty()).collect();
        for (c, parent) in self.collapsed.iter_mut().zip(parents) {
            *c = collapsed && parent;
        }
        if let Some(root) = self.collapsed.first_mut() {
            *root = false;
        }
        self.rebuild_visible();
        if let Some(sel) = self.selected
            && !self.visible.contains(&sel)
        {
            self.selected = self.visible_ancestor(sel);
        }
    }

    fn visible_ancestor(&self, id: NodeId) -> Option<NodeId> {
        let plan = self.plan()?;
        let mut at = Some(id);
        while let Some(n) = at {
            if self.visible.contains(&n) {
                return Some(n);
            }
            at = plan.parent(n);
        }
        None
    }

    /// Selects the `index`th visible row and opens its details.
    pub fn select_row(&mut self, index: usize) {
        if let Some(id) = self.visible.get(index.min(self.visible.len().saturating_sub(1))) {
            self.selected = Some(*id);
            self.detail_open = true;
            self.scroll_to_selected = true;
        }
    }
}

/// Draws the pane.
pub fn show(ui: &mut Ui, id: Id, view: &mut PlanView) {
    let p = palette(ui);
    toolbar(ui, id, view, p);
    if let Some(note) = &view.note {
        banner(ui, id.with("note"), note, p.warning, p);
    }
    if view.show_raw || view.plan().is_none() {
        raw_view(ui, id, view.raw_text());
        return;
    }
    if view.detail_open
        && let Some(sel) = view.selected
    {
        let mut open = true;
        egui::Panel::right(id.with("detail"))
            .resizable(true)
            .default_size(300.0)
            .size_range(240.0..=680.0)
            .frame(egui::Frame::new().fill(p.content_bg).stroke(Stroke::new(1.0, p.border)))
            .show(ui, |ui| detail(ui, view, sel, &mut open));
        view.detail_open = open;
    }
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| tree(ui, id, view));
}

fn toolbar(ui: &mut Ui, id: Id, view: &mut PlanView, p: &Palette) {
    egui::Panel::top(id.with("toolbar")).exact_size(34.0).frame(egui::Frame::NONE).show(ui, |ui| {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, CornerRadius::ZERO, p.content_bg);
        ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
        ui.horizontal_centered(|ui| {
            ui.add_space(10.0);
            if let Some(plan) = view.plan() {
                let kind = if plan.analyzed() { "Analyzed" } else { "Estimated" };
                let label = format!("{kind} · {}", plan.format.label());
                widgets::segmented(ui, &mut view.show_raw, &[(false, "Tree"), (true, "Raw")]);
                if !view.show_raw {
                    ui.add_space(6.0);
                    if text_button(ui, "Expand all").clicked() {
                        view.set_all_collapsed(false);
                    }
                    if text_button(ui, "Collapse all").clicked() {
                        view.set_all_collapsed(true);
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new(label).size(12.0).color(p.text_faint));
            }
            if let PlanContent::Raw { reason, .. } = &view.content {
                ui.add_space(4.0);
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, p.text_faint);
                ui.label(RichText::new(reason).size(12.0).color(p.text_muted));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(8.0);
                if widgets::icon_button(ui, Icon::Copy, "Copy the server's output").clicked() {
                    ui.ctx().copy_text(view.raw_text().to_string());
                }
            });
        });
    });
}

/// Borderless text button with a hover background, quieter than the framed toolbar buttons.
fn text_button(ui: &mut Ui, text: &str) -> egui::Response {
    let p = palette(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), theme::ui_font(12.0), p.text_muted);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 24.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(5), p.hover);
    }
    let color = if resp.hovered() { p.text } else { p.text_muted };
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

fn banner(ui: &mut Ui, id: Id, text: &str, dot: Color32, p: &Palette) {
    egui::Panel::top(id).exact_size(28.0).frame(egui::Frame::NONE).show(ui, |ui| {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, CornerRadius::ZERO, p.header_bg);
        ui.painter().line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
        ui.horizontal_centered(|ui| {
            ui.add_space(14.0);
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 3.5, dot);
            ui.label(RichText::new(text).size(12.0).color(p.text_muted));
        });
    });
}

fn raw_view(ui: &mut Ui, id: Id, text: &str) {
    let p = palette(ui);
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let job = LayoutJob::simple(buf.as_str().to_string(), theme::mono_font(12.0), p.text, f32::INFINITY);
        ui.painter().layout_job(job)
    };
    egui::ScrollArea::both().id_salt(id.with("raw")).auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut &*text)
                    .id(id.with("raw_text"))
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .layouter(&mut layouter),
            );
        });
    });
}

/// The right-hand number and bar columns, which stay put while the tree scrolls sideways.
struct Columns {
    analyzed: bool,
    /// The bar gives up width first when the pane is narrow, so step names stay readable.
    bar: f32,
}

impl Columns {
    const EST: f32 = 78.0;
    const ROWS: f32 = 116.0;
    const LOOPS: f32 = 58.0;
    const TIME: f32 = 74.0;
    const COST: f32 = 74.0;

    fn new(analyzed: bool, pane_width: f32) -> Self {
        let mut cols = Self { analyzed, bar: 0.0 };
        cols.bar = (pane_width - cols.numbers() - 2.0 * PAD - 420.0).clamp(96.0, 176.0);
        cols
    }

    fn numbers(&self) -> f32 {
        if self.analyzed { Self::EST + Self::ROWS + Self::LOOPS + Self::TIME } else { Self::EST + Self::COST }
    }

    fn width(&self) -> f32 {
        self.numbers() + self.bar + 2.0 * PAD
    }
}

fn tree(ui: &mut Ui, id: Id, view: &mut PlanView) {
    if ui.memory(|m| m.has_focus(id)) {
        keyboard(ui, view);
    }
    let has_focus = ui.memory(|m| m.has_focus(id));
    let p = palette(ui);
    // Bars grow in when a plan first appears.
    let since = view.shown_at.get_or_insert_with(Instant::now).elapsed().as_secs_f32();
    let grow = ease_out((since / GROW_SECONDS).min(1.0));
    if grow < 1.0 {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
    let Some(plan) = view.plan() else { return };
    let analyzed = plan.analyzed();
    let cols = Columns::new(analyzed, ui.available_width());
    let rows = view.visible.len();
    let max_depth = view.visible.iter().map(|n| plan.depth(*n)).max().unwrap_or(0) as f32;

    // Deep plans indent less, so their steps stay readable before the tree scrolls sideways.
    let tree_w = (ui.available_width() - cols.width()).max(240.0);
    let indent = ((tree_w - 260.0) / max_depth.max(1.0)).clamp(11.0, 18.0);
    let content_w = (PAD + max_depth * indent + CHEVRON + 260.0).max(tree_w) + cols.width();

    let mut scroll = egui::ScrollArea::both()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::MOUSE_WHEEL);
    if view.scroll_to_selected
        && let Some(row) = view.selected.and_then(|s| view.visible.iter().position(|v| *v == s))
        && let Some(state) = view.scroll_id.and_then(|sid| egui::scroll_area::State::load(ui.ctx(), sid))
    {
        let viewport_h = ui.available_height() - HEADER_H;
        let mut off = state.offset;
        let top = row as f32 * ROW_H;
        if top < off.y {
            off.y = top;
        } else if top + ROW_H > off.y + viewport_h - 8.0 {
            off.y = top + ROW_H - viewport_h + 8.0;
        }
        scroll = scroll.scroll_offset(off);
    }

    let mut toggle = None;
    let mut clicked = None;
    let output = scroll.show_viewport(ui, |ui, viewport| {
        ui.set_min_size(vec2(content_w, (HEADER_H + rows as f32 * ROW_H).max(viewport.height())));
        let origin = ui.max_rect().min;
        let clip = ui.clip_rect();
        let painter = ui.painter().clone();
        let body_top = clip.top() + HEADER_H;
        let metrics_left = clip.right() - cols.width();
        let tree_clip = Rect::from_min_max(pos2(clip.left(), body_top), pos2(metrics_left, clip.bottom()));
        let numbers_clip = Rect::from_min_max(pos2(metrics_left, body_top), clip.max);

        let body = ui.interact(Rect::from_min_max(pos2(clip.left(), body_top), clip.max), id, Sense::click());
        if body.clicked() || body.double_clicked() {
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
        let row_at = |pos: Pos2| -> Option<usize> {
            let r = ((pos.y - origin.y - HEADER_H) / ROW_H).floor();
            (pos.y >= body_top && r >= 0.0 && (r as usize) < rows).then_some(r as usize)
        };
        let chevron_x = |depth: usize| origin.x + PAD + depth as f32 * indent;
        if let Some(pos) = body.interact_pointer_pos()
            && let Some(r) = row_at(pos)
        {
            let node = view.visible[r];
            let x = chevron_x(plan.depth(node));
            let on_chevron = !plan.children(node).is_empty()
                && pos.x < metrics_left
                && pos.x >= x - 3.0
                && pos.x < x + CHEVRON + 3.0;
            if body.clicked() {
                if on_chevron {
                    toggle = Some(node);
                } else {
                    clicked = Some(node);
                }
            }
            if body.double_clicked() && !on_chevron {
                toggle = Some(node);
            }
        }
        let hovered_row = body.hover_pos().and_then(row_at);
        let hover_x = body.hover_pos().map_or(f32::NEG_INFINITY, |h| h.x);

        let first = (viewport.min.y / ROW_H).floor().max(0.0) as usize;
        let last = ((viewport.max.y / ROW_H).ceil() as usize + 1).min(rows);
        let selected_depth = view.selected.map(|s| plan.depth(s));
        let tp = painter.with_clip_rect(tree_clip);
        let np = painter.with_clip_rect(numbers_clip);
        let num_font = theme::ui_font(12.5);

        for r in first..last {
            let node_id = view.visible[r];
            let node = plan.node(node_id);
            let m = &view.metrics[node_id.index()];
            let depth = plan.depth(node_id);
            let y = origin.y + HEADER_H + r as f32 * ROW_H;
            let row_rect = Rect::from_min_size(pos2(clip.left(), y), vec2(clip.width(), ROW_H));
            let selected = view.selected == Some(node_id);
            let on_accent = selected && has_focus;
            let bg = if selected {
                if has_focus { p.selection } else { p.selection_inactive }
            } else if hovered_row == Some(r) {
                p.hover
            } else if r % 2 == 1 {
                p.row_alt
            } else {
                p.content_bg
            };
            painter.rect_filled(row_rect, CornerRadius::ZERO, bg);
            let cy = row_rect.center().y;

            // Indent guides, one per ancestor level; the selected step's subtree is highlighted.
            let in_selection = view.selected.is_some_and(|s| s != node_id && is_within(plan, node_id, s));
            for level in 0..depth {
                let x = chevron_x(level) + CHEVRON / 2.0;
                let color = if on_accent {
                    p.accent_text.gamma_multiply(0.18)
                } else if in_selection && selected_depth == Some(level) {
                    p.plan_guide_active
                } else {
                    p.plan_guide
                };
                tp.line_segment([pos2(x, y), pos2(x, y + ROW_H)], Stroke::new(1.0, color));
            }

            let x = chevron_x(depth);
            let collapsed = view.collapsed[node_id.index()];
            if !plan.children(node_id).is_empty() {
                let open = ui.ctx().animate_bool_with_time(id.with(("open", node_id)), !collapsed, 0.12);
                chevron(&tp, pos2(x + CHEVRON / 2.0, cy), open, if on_accent { p.accent_text } else { p.text_muted });
            }

            let text_left = x + CHEVRON + 4.0;
            let hint = collapsed.then(|| format!("+{}", fmt_count(m.descendants as u64)));
            let max_w = (metrics_left - text_left - 10.0 - if hint.is_some() { 44.0 } else { 0.0 }).max(20.0);
            let galley = label(ui, node, max_w, on_accent, p);
            let label_w = galley.size().x;
            if galley.elided && hovered_row == Some(r) && hover_x < metrics_left {
                body.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(full_label(node));
                });
            }
            tp.galley(pos2(text_left, cy - galley.size().y / 2.0), galley, p.text);
            if let Some(hint) = hint {
                let color = if on_accent { p.accent_text } else { p.text_muted };
                let g = ui.painter().layout_no_wrap(hint, theme::bold_font(10.5), color);
                let pill =
                    Rect::from_min_size(pos2(text_left + label_w + 8.0, cy - 8.0), vec2(g.size().x + 12.0, 16.0));
                tp.rect_filled(
                    pill,
                    CornerRadius::same(8),
                    if on_accent { p.accent_text.gamma_multiply(0.2) } else { p.plan_bar_track },
                );
                tp.galley(pill.center() - g.size() / 2.0, g, color);
            }

            // Number columns, painted over the row so long step text ends under them.
            np.rect_filled(
                Rect::from_min_max(pos2(metrics_left, y), pos2(clip.right(), y + ROW_H)),
                CornerRadius::ZERO,
                bg,
            );
            let (strong, weak, faint) = if on_accent {
                (p.accent_text, p.accent_text.gamma_multiply(0.8), p.accent_text.gamma_multiply(0.6))
            } else {
                (p.text, p.text_muted, p.text_faint)
            };
            let number = |left: f32, w: f32, text: &str, color: Color32| {
                np.text(pos2(left + w - 8.0, cy), Align2::RIGHT_CENTER, text, num_font.clone(), color).width()
            };
            let mut cx = metrics_left + PAD;
            if let Some(est) = node.estimate {
                number(cx, Columns::EST, &fmt_rows(est.rows), weak);
            }
            cx += Columns::EST;
            if analyzed {
                if let Some(actual) = node.actual {
                    let misestimated = node.is_misestimated();
                    let text = if actual.loops == 0 { "never ran".to_string() } else { fmt_rows(actual.rows) };
                    let color = if misestimated && !on_accent { p.warning } else { strong };
                    let text_w = number(cx, Columns::ROWS, &text, color);
                    if misestimated && let Some(ratio) = node.row_estimate_ratio() {
                        let right = cx + Columns::ROWS - 8.0 - text_w - 5.0;
                        misestimate_badge(
                            &np,
                            pos2(right, cy),
                            ratio,
                            if on_accent { p.accent_text } else { p.warning },
                        );
                    }
                    let loops_color = if actual.loops == 1 { faint } else { weak };
                    number(cx + Columns::ROWS, Columns::LOOPS, &fmt_count(actual.loops), loops_color);
                }
                cx += Columns::ROWS + Columns::LOOPS;
                if let Some(ms) = m.inclusive_ms {
                    number(cx, Columns::TIME, &fmt_ms(ms), strong);
                }
                cx += Columns::TIME;
            } else {
                if let Some(cost) = node.estimate.and_then(|e| e.cost) {
                    number(cx, Columns::COST, &fmt_rows(cost.total), strong);
                }
                cx += Columns::COST;
            }
            let bar = Rect::from_min_size(pos2(cx + 4.0, cy - BAR_H / 2.0), vec2(cols.bar - 12.0, BAR_H));
            paint_bar(&np, bar, node, m, view.scale, grow, analyzed, on_accent, p);
            if hovered_row == Some(r)
                && hover_x >= bar.left() - 4.0
                && let Some(tip) = bar_tooltip(node, m, view.scale, analyzed)
            {
                body.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(tip);
                });
            }
        }

        // Sticky header.
        let header = Rect::from_min_size(clip.min, vec2(clip.width(), HEADER_H));
        painter.rect_filled(header, CornerRadius::ZERO, p.header_bg);
        painter.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
        painter.line_segment(
            [pos2(metrics_left, clip.top()), pos2(metrics_left, clip.bottom())],
            Stroke::new(1.0, p.grid_line),
        );
        let head_font = theme::bold_font(12.0);
        let hy = header.center().y;
        painter.text(pos2(clip.left() + PAD + 4.0, hy), Align2::LEFT_CENTER, "Step", head_font.clone(), p.text);
        let head = |left: f32, w: f32, text: &str| {
            painter.text(pos2(left + w - 8.0, hy), Align2::RIGHT_CENTER, text, head_font.clone(), p.text);
        };
        let mut cx = metrics_left + PAD;
        head(cx, Columns::EST, "Est. rows");
        cx += Columns::EST;
        if analyzed {
            head(cx, Columns::ROWS, "Rows");
            head(cx + Columns::ROWS, Columns::LOOPS, "Loops");
            head(cx + Columns::ROWS + Columns::LOOPS, Columns::TIME, "Time");
            cx += Columns::ROWS + Columns::LOOPS + Columns::TIME;
        } else {
            head(cx, Columns::COST, "Cost");
            cx += Columns::COST;
        }
        bar_legend(&painter, pos2(cx + 4.0, hy), analyzed, p);
    });
    view.scroll_id = Some(output.id);
    view.scroll_to_selected = false;

    if let Some(node) = toggle {
        let collapsed = !view.collapsed[node.index()];
        view.set_collapsed(node, collapsed);
        view.selected = Some(node);
    }
    if let Some(node) = clicked {
        view.selected = Some(node);
        view.detail_open = true;
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// True when `node` is `ancestor` or below it.
fn is_within(plan: &Plan, node: NodeId, ancestor: NodeId) -> bool {
    let mut at = Some(node);
    while let Some(n) = at {
        if n == ancestor {
            return true;
        }
        at = plan.parent(n);
    }
    false
}

fn keyboard(ui: &mut Ui, view: &mut PlanView) {
    let pressed: Vec<Key> = ui.input(|i| {
        [
            Key::ArrowUp,
            Key::ArrowDown,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::Space,
            Key::Enter,
            Key::Escape,
        ]
        .into_iter()
        .filter(|k| i.key_pressed(*k))
        .collect()
    });
    let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
    let page = (ui.available_height() / ROW_H).max(1.0) as usize;
    for key in pressed {
        match next_move(view, key, page) {
            Some(Move::Row(r)) => view.select_row(r),
            Some(Move::Fold(node, collapsed)) => view.set_collapsed(node, collapsed),
            Some(Move::CloseDetail) => view.detail_open = false,
            Some(Move::Deselect) => view.selected = None,
            None => {}
        }
    }
    if copy
        && let Some(plan) = view.plan()
        && let Some(sel) = view.selected
    {
        ui.ctx().copy_text(node_text(plan.node(sel)));
    }
}

enum Move {
    Row(usize),
    Fold(NodeId, bool),
    CloseDetail,
    Deselect,
}

/// What a navigation key does: arrows move, Left and Right fold and unfold, Space toggles.
fn next_move(view: &PlanView, key: Key, page: usize) -> Option<Move> {
    let plan = view.plan()?;
    let row = view.selected.and_then(|s| view.visible.iter().position(|v| *v == s));
    let last = view.visible.len().saturating_sub(1);
    let position = |id: NodeId| view.visible.iter().position(|v| *v == id);
    let folded = |id: NodeId| view.collapsed[id.index()];
    Some(match (key, view.selected) {
        (Key::ArrowUp, _) => Move::Row(row.map_or(0, |r| r.saturating_sub(1))),
        (Key::ArrowDown, _) => Move::Row(row.map_or(0, |r| (r + 1).min(last))),
        (Key::PageUp, _) => Move::Row(row.unwrap_or(0).saturating_sub(page)),
        (Key::PageDown, _) => Move::Row((row.unwrap_or(0) + page).min(last)),
        (Key::Home, _) => Move::Row(0),
        (Key::End, _) => Move::Row(last),
        (Key::ArrowLeft, Some(sel)) if !plan.children(sel).is_empty() && !folded(sel) => Move::Fold(sel, true),
        (Key::ArrowLeft, Some(sel)) => Move::Row(position(plan.parent(sel)?)?),
        (Key::ArrowRight, Some(sel)) if folded(sel) => Move::Fold(sel, false),
        (Key::ArrowRight, Some(sel)) => Move::Row(position(*plan.children(sel).first()?)?),
        (Key::Space | Key::Enter, Some(sel)) => Move::Fold(sel, !folded(sel)),
        (Key::Escape, Some(_)) if view.detail_open => Move::CloseDetail,
        (Key::Escape, Some(_)) => Move::Deselect,
        _ => return None,
    })
}

/// The whole step on one line, for the tooltip of an elided row.
fn full_label(node: &PlanNode) -> String {
    let parts = [Some(&node.operation), node.object.as_ref(), node.index.as_ref(), node.condition.as_ref()];
    parts.into_iter().flatten().map(String::as_str).collect::<Vec<_>>().join("  ·  ")
}

/// `Covering index lookup  p2  · idx_products_category_id   category_id=cat2.category_id`, elided.
fn label(ui: &Ui, node: &PlanNode, max_w: f32, on_accent: bool, p: &Palette) -> Arc<Galley> {
    let (text, muted, faint) = if on_accent {
        (p.accent_text, p.accent_text.gamma_multiply(0.85), p.accent_text.gamma_multiply(0.7))
    } else {
        (p.text, p.text_muted, p.text_faint)
    };
    let fmt = |font: FontId, color: Color32| egui::TextFormat { font_id: font, color, ..Default::default() };
    let mut job = LayoutJob::default();
    job.append(&node.operation, 0.0, fmt(theme::ui_font(13.0), text));
    if let Some(object) = &node.object {
        job.append(object, 6.0, fmt(theme::bold_font(13.0), text));
    }
    if let Some(index) = &node.index {
        job.append(&format!("· {index}"), 6.0, fmt(theme::ui_font(12.0), muted));
    }
    if let Some(condition) = &node.condition {
        job.append(&condition.replace('\n', " "), 10.0, fmt(theme::mono_font(11.5), faint));
    }
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    ui.painter().layout_job(job)
}

fn chevron(painter: &Painter, center: Pos2, open: f32, color: Color32) {
    let (s, c) = (open * std::f32::consts::FRAC_PI_2).sin_cos();
    let rot = |x: f32, y: f32| center + vec2(x * c - y * s, x * s + y * c);
    painter.add(Shape::convex_polygon(vec![rot(-2.5, -4.0), rot(3.5, 0.0), rot(-2.5, 4.0)], color, Stroke::NONE));
}

/// `▲ 67×`: actual rows above (▲) or below (▼) the estimate, right-aligned at `right_center`.
fn misestimate_badge(painter: &Painter, right_center: Pos2, ratio: f64, color: Color32) {
    let factor = if ratio >= 1.0 { ratio } else { 1.0 / ratio };
    let g = painter.layout_no_wrap(format!("{}×", fmt_factor(factor)), theme::bold_font(10.5), color);
    let w = g.size().x + 17.0;
    let rect = Rect::from_min_size(pos2(right_center.x - w, right_center.y - 8.0), vec2(w, 16.0));
    painter.rect_filled(rect, CornerRadius::same(4), color.gamma_multiply(0.16));
    let (cx, cy) = (rect.left() + 7.0, rect.center().y);
    let tri = if ratio >= 1.0 {
        vec![pos2(cx - 3.0, cy + 2.5), pos2(cx + 3.0, cy + 2.5), pos2(cx, cy - 2.5)]
    } else {
        vec![pos2(cx - 3.0, cy - 2.5), pos2(cx + 3.0, cy - 2.5), pos2(cx, cy + 2.5)]
    };
    painter.add(Shape::convex_polygon(tri, color, Stroke::NONE));
    painter.galley(pos2(rect.left() + 12.0, cy - g.size().y / 2.0), g, color);
}

#[allow(clippy::too_many_arguments)]
fn paint_bar(
    painter: &Painter,
    bar: Rect,
    node: &PlanNode,
    m: &Metrics,
    scale: f64,
    grow: f32,
    analyzed: bool,
    on_accent: bool,
    p: &Palette,
) {
    let radius = CornerRadius::same((BAR_H / 2.0) as u8);
    let (track, fill, strong) = if on_accent {
        let c = p.accent_text;
        (c.gamma_multiply(0.1), c.gamma_multiply(0.45), c)
    } else if analyzed {
        (p.plan_bar_track, p.plan_bar, p.plan_bar_self)
    } else {
        (p.plan_bar_track, p.plan_cost, p.plan_cost)
    };
    let has_value = if analyzed { m.inclusive_ms.is_some() } else { node.estimate.is_some_and(|e| e.cost.is_some()) };
    if !has_value || scale <= 0.0 {
        return;
    }
    painter.rect_filled(bar, radius, track);
    let width = |value: f64| bar.width() * (value / scale).clamp(0.0, 1.0) as f32 * grow;
    // Thin slivers stay thin: a dot on every short step would read as a hot spot.
    let segment = |w: f32, color: Color32| {
        if w > 0.0 {
            let w = w.max(1.5);
            let r = CornerRadius::same((w / 2.0).min(BAR_H / 2.0) as u8);
            painter.rect_filled(Rect::from_min_size(bar.min, vec2(w, bar.height())), r, color);
        }
    };
    if analyzed {
        let Some(total) = m.inclusive_ms else { return };
        let w = width(total);
        segment(w, fill);
        if let Some(own) = m.self_ms.filter(|own| *own > 0.0) {
            segment(width(own).min(w), strong);
        }
        if let Some(time) = node.actual.and_then(|a| a.time)
            && let Some(first) = time.first_row_ms
            && time.last_row_ms > 0.0
            && w >= 10.0
        {
            let x = bar.left() + w * (first / time.last_row_ms).clamp(0.0, 1.0) as f32;
            let tick = if on_accent { p.accent_text } else { p.plan_tick };
            painter.line_segment([pos2(x, bar.top() - 3.0), pos2(x, bar.bottom() + 3.0)], Stroke::new(1.5, tick));
        }
    } else if let Some(cost) = node.estimate.and_then(|e| e.cost) {
        segment(width(cost.total), fill);
    }
}

fn bar_legend(painter: &Painter, left_center: Pos2, analyzed: bool, p: &Palette) {
    let font = theme::ui_font(11.0);
    let mut x = left_center.x;
    let mut item = |color: Color32, text: &str| {
        let swatch = Rect::from_center_size(pos2(x + 4.0, left_center.y), vec2(8.0, 8.0));
        painter.rect_filled(swatch, CornerRadius::same(2), color);
        let r = painter.text(pos2(x + 12.0, left_center.y), Align2::LEFT_CENTER, text, font.clone(), p.text_muted);
        x = r.right() + 10.0;
    };
    if analyzed {
        item(p.plan_bar_self, "self");
        item(p.plan_bar, "with children");
    } else {
        item(p.plan_cost, "relative cost");
    }
}

fn bar_tooltip(node: &PlanNode, m: &Metrics, scale: f64, analyzed: bool) -> Option<String> {
    if analyzed {
        let total = m.inclusive_ms?;
        let mut tip = format!("{} including the steps below", fmt_ms(total));
        if let Some(own) = m.self_ms {
            tip.push_str(&format!("\n{} in this step itself", fmt_ms(own)));
        }
        if scale > 0.0 {
            tip.push_str(&format!("\n{:.0}% of the whole statement", 100.0 * total / scale));
        }
        if let Some(first) = node.actual.and_then(|a| a.time).and_then(|t| t.first_row_ms) {
            tip.push_str(&format!("\nFirst row after {} (per loop)", fmt_ms(first)));
        }
        Some(tip)
    } else {
        let cost = node.estimate?.cost?;
        (scale > 0.0)
            .then(|| format!("Cost {}, {:.0}% of the costliest step", fmt_rows(cost.total), 100.0 * cost.total / scale))
    }
}

fn detail(ui: &mut Ui, view: &PlanView, id: NodeId, open: &mut bool) {
    let p = palette(ui);
    let Some(plan) = view.plan() else { return };
    let node = plan.node(id);
    let m = &view.metrics[id.index()];
    egui::ScrollArea::vertical().id_salt(("plan_detail", id)).auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(14, 12)).show(ui, |ui| {
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    let title = RichText::new(&node.operation).font(theme::bold_font(14.5)).color(p.text);
                    ui.add(egui::Label::new(title).wrap());
                },
                |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close (Esc)").clicked() {
                        *open = false;
                    }
                    if widgets::icon_button(ui, Icon::Copy, "Copy this step").clicked() {
                        ui.ctx().copy_text(node_text(node));
                    }
                },
            );
            if let Some(object) = &node.object {
                let text = match &node.index {
                    Some(index) => format!("{object}  ·  {index}"),
                    None => object.clone(),
                };
                ui.label(RichText::new(text).font(theme::ui_font(12.5)).color(p.text_muted));
            }
            if let Some(condition) = &node.condition {
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(p.row_alt)
                    .stroke(Stroke::new(1.0, p.grid_line))
                    .corner_radius(CornerRadius::same(5))
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(RichText::new(condition).font(theme::mono_font(11.5)).color(p.text))
                                .wrap(),
                        );
                    });
            }

            let mut facts: Vec<(&str, String, Option<Color32>)> = Vec::new();
            if let Some(total) = m.inclusive_ms {
                facts.push(("Time, with steps below", fmt_ms(total), None));
            }
            if let Some(own) = m.self_ms {
                facts.push(("Time in this step", fmt_ms(own), None));
            }
            if let Some(actual) = node.actual {
                if let Some(time) = actual.time {
                    if let Some(first) = time.first_row_ms {
                        facts.push(("First row, per loop", fmt_ms(first), None));
                    }
                    facts.push(("Last row, per loop", fmt_ms(time.last_row_ms), None));
                }
                facts.push(("Rows per loop", fmt_rows(actual.rows), None));
                facts.push(("Loops", fmt_count(actual.loops), None));
                if actual.loops > 1 {
                    facts.push(("Rows in all loops", fmt_rows(actual.rows * actual.loops as f64), None));
                }
            }
            if let Some(est) = node.estimate {
                facts.push(("Estimated rows per loop", fmt_rows(est.rows), None));
                if let Some(cost) = est.cost {
                    let text = match cost.startup {
                        Some(s) if s != cost.total => {
                            format!("{} to first row, {} total", fmt_rows(s), fmt_rows(cost.total))
                        }
                        _ => fmt_rows(cost.total),
                    };
                    facts.push(("Estimated cost", text, None));
                }
            }
            if let Some(ratio) = node.row_estimate_ratio() {
                let (factor, way) = if ratio >= 1.0 { (ratio, "more") } else { (1.0 / ratio, "fewer") };
                let text = if factor < 1.05 {
                    "on target".to_string()
                } else {
                    format!("{}× {way} rows than estimated", fmt_factor(factor))
                };
                facts.push(("Estimate", text, node.is_misestimated().then_some(p.warning)));
            }
            if m.descendants > 0 {
                facts.push(("Steps below", fmt_count(m.descendants as u64), None));
            }
            if !facts.is_empty() {
                ui.add_space(12.0);
                widgets::section_title(ui, "Measured and estimated");
                egui::Grid::new(("plan_facts", id)).num_columns(2).striped(false).spacing([14.0, 6.0]).show(ui, |ui| {
                    for (k, v, color) in facts {
                        ui.label(RichText::new(k).size(12.0).color(p.text_muted));
                        ui.label(RichText::new(v).size(12.5).color(color.unwrap_or(p.text)));
                        ui.end_row();
                    }
                });
            }

            ui.add_space(12.0);
            widgets::section_title(ui, "Reported by the server");
            egui::Grid::new(("plan_attrs", id)).num_columns(2).striped(false).spacing([14.0, 6.0]).show(ui, |ui| {
                for a in &node.attributes {
                    ui.label(RichText::new(&a.key).size(12.0).color(p.text_muted));
                    ui.add(egui::Label::new(RichText::new(&a.value).font(theme::mono_font(11.5)).color(p.text)).wrap());
                    ui.end_row();
                }
            });
        });
    });
}

/// A step as `key: value` lines, for the clipboard.
fn node_text(node: &PlanNode) -> String {
    node.attributes.iter().map(|a| format!("{}: {}", a.key, a.value)).collect::<Vec<_>>().join("\n")
}

/// `287`, `127,044`, `33.7`, `1.95`, `0.31`, `12.3M`.
fn fmt_rows(n: f64) -> String {
    if n >= 10_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 100.0 || n.fract() == 0.0 {
        fmt_count(n.round() as u64)
    } else if n >= 10.0 {
        format!("{n:.1}")
    } else {
        let s = format!("{n:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// `4.68 ms`, `61.2 ms`, `578 µs`, `1.24 s`.
pub fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 100.0 {
        format!("{ms:.0} ms")
    } else if ms >= 10.0 {
        format!("{ms:.1} ms")
    } else if ms >= 1.0 {
        format!("{ms:.2} ms")
    } else if ms >= 0.0005 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms > 0.0 {
        "<1 µs".into()
    } else {
        "0".into()
    }
}

fn fmt_factor(f: f64) -> String {
    if f >= 1000.0 { fmt_count(f.round() as u64) } else { format!("{f:.0}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats() {
        assert_eq!(fmt_rows(287.0), "287");
        assert_eq!(fmt_rows(127_044.0), "127,044");
        assert_eq!(fmt_rows(33.7), "33.7");
        assert_eq!(fmt_rows(1.95), "1.95");
        assert_eq!(fmt_rows(0.31), "0.31");
        assert_eq!(fmt_rows(0.889), "0.89");
        assert_eq!(fmt_rows(0.0), "0");
        assert_eq!(fmt_ms(4.68), "4.68 ms");
        assert_eq!(fmt_ms(0.578), "578 µs");
        assert_eq!(fmt_ms(244e-6), "<1 µs");
        assert_eq!(fmt_ms(1240.0), "1.24 s");
        assert_eq!(fmt_factor(67.2), "67");
        assert_eq!(fmt_factor(4539.0), "4,539");
    }
}
