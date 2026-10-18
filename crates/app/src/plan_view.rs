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
use tableplus_core::plan::{self, Access, Extra, NodeId, Plan, PlanFact, PlanFormat, PlanNode, SPAN_SLACK};

use crate::icons::Icon;
use crate::plan_flame::{self, Flame, Pick};
use crate::plan_graph::{self, GraphView};
use crate::theme::{self, Palette, palette};
use crate::widgets::{self, fmt_count, fmt_factor, fmt_ms, fmt_rows};
pub(crate) use detail::node_text;
use detail::{detail, fmt_fact, plan_summary, stands_out};

mod detail;

const ROW_H: f32 = 26.0;
const HEADER_H: f32 = 28.0;
const PAD: f32 = 10.0;
const CHEVRON: f32 = 16.0;
const BAR_H: f32 = 8.0;
/// Horizontal room around a `temp table` badge's text.
const BADGE_PAD: f32 = 10.0;
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
        match plan::parse(server, rows) {
            Some(Ok(plan)) => PlanContent::Tree(Box::new(plan)),
            Some(Err(e)) => PlanContent::Raw { text: plan::raw_text(rows), reason: format!("{e}.") },
            None => PlanContent::Raw {
                text: plan::raw_text(rows),
                reason: format!("No visual plan for {} output yet. This is the server's own text.", format.label()),
            },
        }
    }

    /// A plan for a result that reads as one, such as a hand-typed `EXPLAIN ANALYZE`. Ordinary
    /// results that only look like a plan, such as a JSON value, stay rows.
    pub fn detected(server: &ServerInfo, rows: &ResultSet) -> Option<Self> {
        plan::parse(server, rows)?.ok().map(|plan| PlanContent::Tree(Box::new(plan)))
    }
}

/// Which picture of the plan the pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Tree,
    Graph,
    Flame,
    Raw,
}

/// Derived numbers per node, computed once when the plan arrives.
struct Metrics {
    inclusive_ms: Option<f64>,
    self_ms: Option<f64>,
    /// The planner's total cost, without MySQL's placeholder.
    cost: Option<f64>,
    /// The cost, or what the steps below cost in all when that is more.
    cost_span: f64,
    extras: Vec<Extra>,
    access: Option<Access>,
    descendants: usize,
}

impl Metrics {
    /// The steps below cost more than this step counts, as MySQL's steps above a temporary table
    /// do, so its bar spans them.
    fn spans_below(&self) -> bool {
        self.cost_span > self.cost.unwrap_or(0.0) * SPAN_SLACK
    }
}

pub struct PlanView {
    content: PlanContent,
    /// Why the plan differs from what was asked for, such as Analyze falling back to estimates.
    note: Option<String>,
    mode: Mode,
    graph: GraphView,
    collapsed: Vec<bool>,
    /// Rows on screen, top to bottom: the nodes whose ancestors are all expanded.
    visible: Vec<NodeId>,
    metrics: Vec<Metrics>,
    /// Bar length for 100%: the statement's time, or without actuals what the whole plan costs.
    scale: f64,
    /// Some step's cost bar spans steps below that cost more than the step counts.
    cost_below: bool,
    selected: Option<NodeId>,
    detail_open: bool,
    scroll_to_selected: bool,
    scroll_id: Option<Id>,
    /// When the tree was first drawn; its bars grow in from there.
    shown_at: Option<Instant>,
    /// `None` when no step was timed.
    flame: Option<Flame>,
    /// Plan-wide figures, such as planning time; the detail panel summarizes them when no step is
    /// selected.
    facts: Vec<PlanFact>,
}

impl PlanView {
    pub fn new(content: PlanContent, note: Option<String>) -> Self {
        let mut view = Self {
            content,
            note,
            mode: Mode::Tree,
            graph: GraphView::default(),
            collapsed: Vec::new(),
            visible: Vec::new(),
            metrics: Vec::new(),
            scale: 0.0,
            cost_below: false,
            selected: None,
            detail_open: true,
            scroll_to_selected: false,
            scroll_id: None,
            shown_at: None,
            flame: None,
            facts: Vec::new(),
        };
        if let PlanContent::Tree(plan) = &view.content {
            view.collapsed = vec![false; plan.node_count()];
            let cost_spans = plan.cost_spans();
            view.metrics = plan
                .nodes()
                .map(|(id, _)| Metrics {
                    inclusive_ms: plan.inclusive_ms(id),
                    self_ms: plan.self_ms(id),
                    cost: plan.estimated_cost(id).map(|c| c.total),
                    cost_span: cost_spans[id.index()],
                    extras: plan.extras(id),
                    access: plan.access(id),
                    descendants: plan.descendant_count(id),
                })
                .collect();
            view.cost_below = view.metrics.iter().any(Metrics::spans_below);
            view.scale =
                if plan.analyzed() { plan.total_ms().unwrap_or(0.0) } else { cost_spans[NodeId::ROOT.index()] };
            view.flame = Flame::new(plan);
            view.facts = plan.facts();
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
                    None if plan.analyzed() => steps,
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

    /// Selects a step and opens its details, unfolding the tree down to it.
    fn select(&mut self, id: NodeId) {
        self.selected = Some(id);
        self.detail_open = true;
        self.reveal_selected();
    }

    /// Unfolds the selected step's ancestors and scrolls to it, after another view moved the selection.
    fn reveal_selected(&mut self) {
        let Some((plan, sel)) = self.plan().zip(self.selected) else { return };
        let ancestors: Vec<NodeId> = std::iter::successors(plan.parent(sel), |n| plan.parent(*n)).collect();
        for a in ancestors {
            self.collapsed[a.index()] = false;
        }
        self.rebuild_visible();
        self.scroll_to_selected = true;
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
    if view.mode == Mode::Raw || view.plan().is_none() {
        raw_view(ui, id, view.raw_text());
        return;
    }
    if view.detail_open && (view.selected.is_some() || !view.facts.is_empty()) {
        let mut open = true;
        egui::Panel::right(id.with("detail"))
            .resizable(true)
            .default_size(300.0)
            .size_range(240.0..=680.0)
            .frame(egui::Frame::new().fill(p.content_bg).stroke(Stroke::new(1.0, p.border)))
            .show(ui, |ui| match view.selected {
                Some(sel) => detail(ui, view, sel, &mut open),
                None => plan_summary(ui, view, &mut open),
            });
        view.detail_open = open;
    }
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| match (view.mode, &view.content) {
        (Mode::Graph, PlanContent::Tree(plan)) => {
            plan_graph::show(ui, id.with("graph"), plan, &mut view.graph, &mut view.selected, &mut view.detail_open)
        }
        (Mode::Flame, PlanContent::Tree(plan)) => {
            match plan_flame::show(ui, id.with("flame"), plan, view.flame.as_mut(), view.selected, view.detail_open) {
                Some(Pick::Step(node)) => view.select(node),
                Some(Pick::CloseDetail) => view.detail_open = false,
                Some(Pick::Clear) => view.selected = None,
                None => {}
            }
        }
        _ => tree(ui, id, view),
    });
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
                let modes = [(Mode::Tree, "Tree"), (Mode::Graph, "Graph"), (Mode::Flame, "Flame"), (Mode::Raw, "Raw")];
                if widgets::segmented(ui, &mut view.mode, &modes) && view.mode == Mode::Tree {
                    view.reveal_selected();
                }
                if view.mode == Mode::Tree {
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
                if !view.facts.is_empty() && facts_button(ui, &view.facts, p).clicked() {
                    view.selected = None;
                    view.detail_open = true;
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

/// `Planning 0.42 ms · Execution 3.91 ms`, with JIT and trigger time that stands out in the
/// warning colour. Ordinary figures give way first when the toolbar is narrow. Clicking it shows
/// the plan's details.
fn facts_button(ui: &mut Ui, facts: &[PlanFact], p: &Palette) -> egui::Response {
    let job = |shown: &[&PlanFact]| {
        let mut job = LayoutJob::default();
        for (i, fact) in shown.iter().enumerate() {
            let format =
                |color: Color32| egui::TextFormat { font_id: theme::ui_font(12.0), color, ..Default::default() };
            if i > 0 {
                job.append("·", 7.0, format(p.text_faint));
            }
            job.append(fact.kind.label(), if i > 0 { 7.0 } else { 0.0 }, format(p.text_faint));
            let value = if stands_out(facts, fact) { p.warning } else { p.text_muted };
            job.append(&fmt_fact(fact.value), 4.0, format(value));
        }
        job
    };
    let mut shown: Vec<&PlanFact> = facts.iter().collect();
    let room = ui.available_width() - 24.0;
    let mut galley = ui.painter().layout_job(job(&shown));
    while galley.size().x > room && shown.len() > 1 {
        let drop = shown.iter().rposition(|f| !f.kind.notable()).unwrap_or(shown.len() - 1);
        shown.remove(drop);
        galley = ui.painter().layout_job(job(&shown));
    }
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 16.0, 24.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(5), p.hover);
    }
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text_muted);
    resp.on_hover_text("Show the plan's details")
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
    /// Off when no step has a number, as in SQLite's plans; the tree then takes the full width.
    shown: bool,
    /// The bar gives up width first when the pane is narrow, so step names stay readable.
    bar: f32,
}

impl Columns {
    const EST: f32 = 78.0;
    const ROWS: f32 = 116.0;
    const LOOPS: f32 = 58.0;
    const TIME: f32 = 74.0;
    const COST: f32 = 74.0;

    fn new(plan: &Plan, pane_width: f32) -> Self {
        let analyzed = plan.analyzed();
        let shown = analyzed || plan.nodes().any(|(_, n)| n.estimate.is_some());
        let mut cols = Self { analyzed, shown, bar: 0.0 };
        cols.bar = (pane_width - cols.numbers() - 2.0 * PAD - 420.0).clamp(96.0, 176.0);
        cols
    }

    fn numbers(&self) -> f32 {
        if self.analyzed { Self::EST + Self::ROWS + Self::LOOPS + Self::TIME } else { Self::EST + Self::COST }
    }

    fn width(&self) -> f32 {
        if !self.shown {
            return 0.0;
        }
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
    let cols = Columns::new(plan, ui.available_width());
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

            // The graph's access colour: in the chevron's place on a leaf, before the text otherwise.
            let mut text_left = x + CHEVRON + 4.0;
            if let Some(access) = m.access {
                let center = if plan.children(node_id).is_empty() {
                    pos2(x + CHEVRON / 2.0, cy)
                } else {
                    text_left += 11.0;
                    pos2(text_left - 7.5, cy)
                };
                let swatch = Rect::from_center_size(center, vec2(7.0, 7.0));
                tp.rect_filled(swatch, CornerRadius::same(2), plan_graph::access_color(access, ui.visuals().dark_mode));
            }
            let hint = collapsed.then(|| format!("+{}", fmt_count(m.descendants as u64)));
            let badge_color = if on_accent { p.accent_text.gamma_multiply(0.85) } else { p.text_muted };
            let badges: Vec<Arc<Galley>> = m
                .extras
                .iter()
                .map(|e| ui.painter().layout_no_wrap(e.label().into(), theme::ui_font(10.5), badge_color))
                .collect();
            let badges_w: f32 = badges.iter().map(|g| g.size().x + BADGE_PAD + 6.0).sum();
            let room = metrics_left - text_left - 10.0 - badges_w - if hint.is_some() { 44.0 } else { 0.0 };
            let galley = label(ui, node, room.max(20.0), on_accent, p);
            let label_w = galley.size().x;
            if galley.elided && hovered_row == Some(r) && hover_x < metrics_left {
                body.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(full_label(node));
                });
            }
            tp.galley(pos2(text_left, cy - galley.size().y / 2.0), galley, p.text);
            let mut after = text_left + label_w + 8.0;
            let badge_border = if on_accent { p.accent_text.gamma_multiply(0.45) } else { p.border };
            for g in badges {
                after += extra_badge(&tp, pos2(after, cy), g, badge_color, badge_border) + 6.0;
            }
            if let Some(hint) = hint {
                let color = if on_accent { p.accent_text } else { p.text_muted };
                let g = ui.painter().layout_no_wrap(hint, theme::bold_font(10.5), color);
                let pill = Rect::from_min_size(pos2(after, cy - 8.0), vec2(g.size().x + 12.0, 16.0));
                tp.rect_filled(
                    pill,
                    CornerRadius::same(8),
                    if on_accent { p.accent_text.gamma_multiply(0.2) } else { p.plan_bar_track },
                );
                tp.galley(pill.center() - g.size() / 2.0, g, color);
            }

            if !cols.shown {
                continue;
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
            // A dash wherever the server gave no estimate, whether it printed a placeholder or nothing.
            let estimated = |left: f32, w: f32, value: Option<f64>, color: Color32| match value {
                Some(v) => number(left, w, &fmt_rows(v), color),
                None => number(left, w, "–", faint),
            };
            let mut cx = metrics_left + PAD;
            estimated(cx, Columns::EST, plan.estimated_rows(node_id), weak);
            cx += Columns::EST;
            if analyzed {
                if let Some(actual) = node.actual {
                    let misestimated = plan.is_misestimated(node_id);
                    let text =
                        if actual.loops == 0 { Some("never ran".to_string()) } else { actual.rows.map(fmt_rows) };
                    let color = if misestimated && !on_accent { p.warning } else { strong };
                    let text_w = text.map_or(0.0, |text| number(cx, Columns::ROWS, &text, color));
                    if misestimated && let Some(ratio) = plan.row_estimate_ratio(node_id) {
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
                estimated(cx, Columns::COST, m.cost, strong);
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
        let head_font = theme::bold_font(12.0);
        let hy = header.center().y;
        painter.text(pos2(clip.left() + PAD + 4.0, hy), Align2::LEFT_CENTER, "Step", head_font.clone(), p.text);
        if !cols.shown {
            return;
        }
        painter.line_segment(
            [pos2(metrics_left, clip.top()), pos2(metrics_left, clip.bottom())],
            Stroke::new(1.0, p.grid_line),
        );
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
        bar_legend(&painter, pos2(cx + 4.0, hy), analyzed, view.cost_below, p);
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
        (Key::Escape, None) if view.detail_open && !view.facts.is_empty() => Move::CloseDetail,
        _ => return None,
    })
}

/// The whole step on one line, for the tooltip of an elided row.
fn full_label(node: &PlanNode) -> String {
    let parts =
        [node.role.as_ref(), Some(&node.operation), node.object.as_ref(), node.index.as_ref(), node.condition.as_ref()];
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
    if let Some(role) = &node.role {
        job.append(role, 0.0, fmt(theme::ui_font(12.0), muted));
    }
    job.append(&node.operation, if node.role.is_some() { 6.0 } else { 0.0 }, fmt(theme::ui_font(13.0), text));
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

/// `temp table`, `filesort`: an outlined tag starting at `left_center`. Returns its width.
fn extra_badge(painter: &Painter, left_center: Pos2, galley: Arc<Galley>, color: Color32, border: Color32) -> f32 {
    let rect = Rect::from_min_size(pos2(left_center.x, left_center.y - 7.5), vec2(galley.size().x + BADGE_PAD, 15.0));
    painter.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, border), egui::StrokeKind::Inside);
    painter.galley(rect.center() - galley.size() / 2.0, galley, color);
    rect.width()
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
    let (track, fill, strong, below) = if on_accent {
        let c = p.accent_text;
        (c.gamma_multiply(0.1), c.gamma_multiply(0.45), c, c.gamma_multiply(0.22))
    } else if analyzed {
        (p.plan_bar_track, p.plan_bar, p.plan_bar_self, p.plan_bar)
    } else {
        (p.plan_bar_track, p.plan_cost, p.plan_cost, p.plan_cost_below)
    };
    let has_value = if analyzed { m.inclusive_ms.is_some() } else { m.cost_span > 0.0 };
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
    } else {
        if m.spans_below() {
            segment(width(m.cost_span), below);
        }
        segment(width(m.cost.unwrap_or(0.0)), fill);
    }
}

fn bar_legend(painter: &Painter, left_center: Pos2, analyzed: bool, cost_below: bool, p: &Palette) {
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
        item(p.plan_bar, "total");
    } else {
        item(p.plan_cost, "cost");
        if cost_below {
            item(p.plan_cost_below, "steps below");
        }
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
        if scale <= 0.0 || m.cost_span <= 0.0 {
            return None;
        }
        let share = |cost: f64| format!("{:.0}% of the plan's cost", 100.0 * cost / scale);
        Some(match m.cost {
            Some(cost) if m.spans_below() => format!(
                "Cost {}\nThe steps below cost {}, which this step doesn't count.\nThe bar spans them: {}",
                fmt_rows(cost),
                fmt_rows(m.cost_span),
                share(m.cost_span)
            ),
            Some(cost) => format!("Cost {}, {}", fmt_rows(cost), share(cost)),
            None => format!(
                "Not costed by the server\nThe steps below cost {}, {}",
                fmt_rows(m.cost_span),
                share(m.cost_span)
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tableplus_core::model::ColumnMeta;
    use tableplus_core::{Flavor, Value, Version};

    #[test]
    fn json_query_results_stay_rows() {
        let one_cell = |column: &str, text: &str| ResultSet {
            columns: vec![ColumnMeta { name: column.into(), type_name: "json".into() }],
            rows: vec![vec![Value::Text(text.into())].into_boxed_slice()],
            truncated: false,
        };
        let postgres = ServerInfo::new(Flavor::Postgres, Version::new(17, 6, 0));
        let mariadb = ServerInfo::new(Flavor::MariaDb, Version::new(11, 8, 0));
        assert!(PlanContent::detected(&postgres, &one_cell("json_agg", "[1,2,3]")).is_none());
        assert!(PlanContent::detected(&mariadb, &one_cell("doc", r#"{"customer_id":1}"#)).is_none());
        let explained = PlanContent::explained(&postgres, &one_cell("QUERY PLAN", "[1,2,3]"), PlanFormat::PostgresJson);
        assert!(matches!(explained, PlanContent::Raw { .. }), "an explicit Explain still shows the server's text");
    }
}
