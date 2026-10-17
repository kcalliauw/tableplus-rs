//! The Plan pane's flame chart. Every step is a bar as wide as its time, with the steps it runs
//! beneath it, so the widest bars show where a statement spent its time. The right-hand part of a
//! bar with nothing beneath it is the step's own time, colored by its share of the statement.
//! Double-click zooms into a step; the breadcrumb, Esc and Home zoom back out, and ⌘-scroll or a
//! pinch zooms freely around the pointer.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, EventFilter, FontId, Id, Key, Pos2, Rect, RichText, Sense, Shape, Stroke,
    StrokeKind, Ui, pos2, scroll_area::ScrollSource, text::LayoutJob, vec2,
};
use tableplus_core::plan::{NodeId, Plan, SPAN_SLACK};

use crate::theme::{self, Palette, palette};
use crate::widgets::{fmt_count, fmt_ms, fmt_rows};

const HEADER_H: f32 = 28.0;
const ROW_H: f32 = 24.0;
const BAR_H: f32 = 22.0;
/// Steps the server didn't time get a thinner bar.
const UNTIMED_H: f32 = 14.0;
const PAD: f32 = 10.0;
const ZOOM_SECONDS: f32 = 0.38;
const REVEAL_SECONDS: f32 = 0.3;
/// Own time below this share of the statement stays a quiet blue; from it, amber warms to red.
const NOTABLE_SHARE: f64 = 0.01;
/// Own time at this share of the statement or more gets the hottest color.
const HOT_SHARE: f64 = 0.3;
/// The narrowest window, as a share of the whole plan.
const MIN_WINDOW: f64 = 1e-6;

/// A horizontal extent, in fractions of the root bar's width.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    left: f64,
    width: f64,
}

impl Span {
    const ALL: Span = Span { left: 0.0, width: 1.0 };

    fn right(self) -> f64 {
        self.left + self.width
    }

    fn contains(self, other: Span) -> bool {
        let slack = 1e-9 * self.width.max(1e-6);
        other.left >= self.left - slack && other.right() <= self.right() + slack
    }

    fn touches(self, other: Span) -> bool {
        other.left <= self.right() && other.right() >= self.left
    }
}

struct Bar {
    span: Span,
    /// The right-hand part of the bar with no step beneath it: the step's own time.
    own: f64,
    /// The step's own time as a share of the statement's; `None` when the server didn't time it.
    heat: Option<f64>,
    /// What the steps below report in all, when that is more than this step reports. The bar
    /// then spans them, and its label keeps the step's own number. Steps are timed separately and
    /// MySQL averages over loops, so nested times don't always add up; MySQL 8.0.18 times a scan
    /// of a temporary table without the materialization below it, 4 µs over 6.6 ms of work.
    stretched: Option<f64>,
}

/// What a click or key in the chart asks of the pane.
pub enum Pick {
    Step(NodeId),
    CloseDetail,
    Clear,
}

/// The flame chart of one plan: bar geometry, computed once, and the zoom.
pub struct Flame {
    bars: Vec<Bar>,
    /// Steps by depth, left to right.
    rows: Vec<Vec<NodeId>>,
    /// The statement's time, [`Plan::total_ms`], which shares are of. The root bar can span
    /// more, when steps below report more than any one step covers.
    total_ms: f64,
    camera: Camera,
    /// When the chart was first drawn; its rows unfold from there.
    shown_at: Option<Instant>,
    /// The selection the chart last brought into view.
    seen: Option<NodeId>,
    scroll_to_row: Option<usize>,
    scroll_id: Option<Id>,
    /// The bar the last click landed on. A double-click zooms into it, because the first click
    /// opens the detail panel, which narrows the chart under the second.
    clicked: Option<NodeId>,
}

/// The part of the plan that fills the chart's width, gliding when it changes.
struct Camera {
    /// Where the camera is going, or is.
    window: Span,
    /// The step zoomed into: the last breadcrumb, and where Esc zooms out from.
    focus: NodeId,
    glide: Option<(Span, Instant)>,
}

impl Camera {
    fn current(&self) -> Span {
        match self.glide {
            Some((from, started)) => {
                let t = (started.elapsed().as_secs_f32() / ZOOM_SECONDS).min(1.0);
                glide(from, self.window, f64::from(ease_out(t)))
            }
            None => self.window,
        }
    }

    fn gliding(&self) -> bool {
        self.glide.is_some_and(|(_, started)| started.elapsed().as_secs_f32() < ZOOM_SECONDS)
    }

    fn glide_to(&mut self, window: Span, focus: NodeId) {
        self.glide = Some((self.current(), Instant::now()));
        self.window = window;
        self.focus = focus;
    }
}

/// The window `t` of the way from `from` to `to`. The narrower of the two moves and grows
/// linearly on screen, so in a deep zoom the step being zoomed into stays in view throughout;
/// interpolating the window's edges instead would leave it a sliver until the last frames.
fn glide(from: Span, to: Span, t: f64) -> Span {
    if t <= 0.0 || t >= 1.0 {
        return if t <= 0.0 { from } else { to };
    }
    let anchor = if to.width <= from.width { to } else { from };
    let on_screen = |w: Span| ((anchor.left - w.left) / w.width, (anchor.right() - w.left) / w.width);
    let (a0, b0) = on_screen(from);
    let (a1, b1) = on_screen(to);
    let (a, b) = (a0 + (a1 - a0) * t, b0 + (b1 - b0) * t);
    let width = anchor.width / (b - a);
    Span { left: anchor.left - a * width, width }
}

impl Flame {
    /// The chart of a plan with measured step times; `None` when nothing was timed.
    pub fn new(plan: &Plan) -> Option<Self> {
        let ids: Vec<NodeId> = plan.nodes().map(|(id, _)| id).collect();
        // Each bar is as wide as the step's time or the steps below it, whichever is more, and a
        // step the server didn't time, such as MySQL's `-> Hash`, is as wide as the steps below.
        let time = plan.spans(|id| plan.inclusive_ms(id));
        let total_ms = plan.total_ms().unwrap_or(0.0);
        if !plan.analyzed() || time[NodeId::ROOT.index()] <= 0.0 {
            return None;
        }
        let mut bars: Vec<Bar> =
            ids.iter().map(|_| Bar { span: Span::ALL, own: 0.0, heat: None, stretched: None }).collect();
        let mut rows: Vec<Vec<NodeId>> = Vec::new();
        for &id in &ids {
            let span = bars[id.index()].span;
            let below: f64 = plan.children(id).iter().map(|c| time[c.index()]).sum();
            let scale = span.width / time[id.index()].max(f64::MIN_POSITIVE);
            let mut left = span.left;
            for c in plan.children(id) {
                let width = time[c.index()] * scale;
                bars[c.index()].span = Span { left, width };
                left += width;
            }
            let own_ms = (time[id.index()] - below).max(0.0);
            let bar = &mut bars[id.index()];
            bar.own = own_ms * scale;
            bar.heat = plan.inclusive_ms(id).map(|_| own_ms / total_ms);
            bar.stretched = plan.inclusive_ms(id).filter(|ms| below > ms * SPAN_SLACK).map(|_| below);
            let depth = plan.depth(id);
            if rows.len() <= depth {
                rows.resize_with(depth + 1, Vec::new);
            }
            rows[depth].push(id);
        }
        for row in &mut rows {
            row.sort_by(|a, b| bars[a.index()].span.left.total_cmp(&bars[b.index()].span.left));
        }
        Some(Self {
            bars,
            rows,
            total_ms,
            camera: Camera { window: Span::ALL, focus: NodeId::ROOT, glide: None },
            shown_at: None,
            seen: None,
            scroll_to_row: None,
            scroll_id: None,
            clicked: None,
        })
    }

    fn span(&self, id: NodeId) -> Span {
        self.bars[id.index()].span
    }

    fn zoom_to(&mut self, id: NodeId) {
        let span = self.span(id);
        if span.width > 0.0 {
            self.camera.glide_to(span, id);
        }
    }

    /// Zooms out to the nearest step wider than the window; false when the whole plan shows.
    fn zoom_out(&mut self, plan: &Plan) -> bool {
        let window = self.camera.window;
        let mut at = Some(self.camera.focus);
        while let Some(id) = at {
            if self.span(id).width > window.width * (1.0 + 1e-6) {
                self.zoom_to(id);
                return true;
            }
            at = plan.parent(id);
        }
        false
    }

    /// Sets the window directly, as a pinch or a sideways scroll does, keeping it on the plan.
    fn set_window(&mut self, plan: &Plan, width: f64, left: f64) {
        let width = width.clamp(MIN_WINDOW, 1.0);
        let window = Span { left: left.clamp(0.0, 1.0 - width), width };
        self.camera.window = window;
        self.camera.glide = None;
        self.camera.focus = self.deepest_containing(plan, window);
    }

    fn deepest_containing(&self, plan: &Plan, window: Span) -> NodeId {
        let mut at = NodeId::ROOT;
        while let Some(child) = plan.children(at).iter().find(|c| self.span(**c).contains(window)) {
            at = *child;
        }
        at
    }

    /// The step at `x` (fraction of the plan) in a row, or the nearest within `slack`.
    fn hit(&self, depth: usize, x: f64, slack: f64) -> Option<NodeId> {
        let row = self.rows.get(depth)?;
        let start = row.partition_point(|id| self.span(*id).right() < x - slack);
        let distance = |id: &NodeId| {
            let s = self.span(*id);
            if x < s.left { s.left - x } else { (x - s.right()).max(0.0) }
        };
        row[start..]
            .iter()
            .take_while(|id| self.span(**id).left <= x + slack)
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            .copied()
    }

    /// The bar beside `id` in its row, `step` places to the right.
    fn neighbour(&self, plan: &Plan, id: NodeId, step: isize) -> Option<NodeId> {
        let row = &self.rows[plan.depth(id)];
        let at = row.iter().position(|n| *n == id)?.checked_add_signed(step)?;
        row.get(at).copied()
    }

    /// Brings a selection made elsewhere, in the tree or by a key, into view.
    fn follow_selection(&mut self, plan: &Plan, selected: Option<NodeId>) {
        if selected == self.seen {
            return;
        }
        self.seen = selected;
        let Some(sel) = selected else { return };
        if !self.camera.window.touches(self.span(sel)) {
            let common = common_ancestor(plan, self.camera.focus, sel);
            self.zoom_to(common);
        }
        self.scroll_to_row = Some(plan.depth(sel));
    }
}

/// `id` and the steps above it, up to the root.
fn ancestry(plan: &Plan, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    std::iter::successors(Some(id), |n| plan.parent(*n))
}

fn common_ancestor(plan: &Plan, a: NodeId, b: NodeId) -> NodeId {
    ancestry(plan, b).find(|n| ancestry(plan, a).any(|m| m == *n)).unwrap_or(NodeId::ROOT)
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Own time as a color. Blue to amber has no clean blend, it goes through grey, so a share
/// too small to matter is blue and a notable one runs from amber to red.
fn heat_color(p: &Palette, share: f64) -> Color32 {
    if share < NOTABLE_SHARE {
        return p.plan_heat_low;
    }
    let t = ((share - NOTABLE_SHARE) / (HOT_SHARE - NOTABLE_SHARE)).clamp(0.0, 1.0).sqrt();
    p.plan_heat_mid.lerp_to_gamma(p.plan_heat_high, t as f32)
}

/// `4%`, or `<1%` for a share that would round to nothing.
fn fmt_share(share: f64) -> String {
    if share > 0.0 && share < 0.005 { "<1%".into() } else { format!("{:.0}%", share * 100.0) }
}

/// Draws the chart, or why there is none, and reports what a click or key picked.
pub fn show(
    ui: &mut Ui,
    id: Id,
    plan: &Plan,
    flame: Option<&mut Flame>,
    selected: Option<NodeId>,
    detail_open: bool,
) -> Option<Pick> {
    let Some(flame) = flame else {
        needs_times(ui, plan);
        return None;
    };
    let p = palette(ui);
    flame.follow_selection(plan, selected);
    let mut pick = None;
    if ui.memory(|m| m.has_focus(id)) {
        pick = keyboard(ui, plan, flame, selected, detail_open);
    }
    let has_focus = ui.memory(|m| m.has_focus(id));

    let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::hover());
    let crumb = header_strip(ui, id, header, plan, flame, p);
    if let Some(target) = crumb {
        flame.zoom_to(target);
    }

    let since = flame.shown_at.get_or_insert_with(Instant::now).elapsed().as_secs_f32();
    let stagger = (0.18 / flame.rows.len() as f32).min(0.02);
    let revealing = since < REVEAL_SECONDS + stagger * flame.rows.len() as f32;
    if revealing || flame.camera.gliding() {
        ui.ctx().request_repaint_after(Duration::from_millis(8));
    } else {
        flame.camera.glide = None;
    }

    let rows = flame.rows.len();
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false])
        .scroll_source(ScrollSource::SCROLL_BAR | ScrollSource::MOUSE_WHEEL);
    if let Some(row) = flame.scroll_to_row.take()
        && let Some(state) = flame.scroll_id.and_then(|sid| egui::scroll_area::State::load(ui.ctx(), sid))
    {
        let viewport_h = ui.available_height();
        let (top, mut off) = (row as f32 * ROW_H, state.offset.y);
        if top < off {
            off = top;
        } else if top + ROW_H > off + viewport_h - 8.0 {
            off = top + ROW_H - viewport_h + 8.0;
        }
        scroll = scroll.vertical_scroll_offset(off);
    }

    let mut press = None;
    let output = scroll.show_viewport(ui, |ui, viewport| {
        ui.set_min_size(vec2(ui.available_width(), (rows as f32 * ROW_H + 2.0 * PAD).max(viewport.height())));
        let origin = ui.max_rect().min;
        let clip = ui.clip_rect();
        let painter = ui.painter().clone();
        painter.rect_filled(clip, CornerRadius::ZERO, p.content_bg);
        let chart = Rect::from_min_max(pos2(clip.left() + PAD, clip.top()), pos2(clip.right() - PAD, clip.bottom()));

        let body = ui.interact(clip, id, Sense::click());
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
        if body.hovered()
            && let Some(pos) = body.hover_pos()
        {
            let window = flame.camera.current();
            let (zoom_by, scrolled) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
            if zoom_by != 1.0 {
                let at = f64::from(((pos.x - chart.left()) / chart.width()).clamp(0.0, 1.0));
                let width = (window.width / f64::from(zoom_by)).clamp(MIN_WINDOW, 1.0);
                flame.set_window(plan, width, window.left + at * (window.width - width));
            } else if scrolled.x.abs() > scrolled.y.abs() && window.width < 1.0 {
                let left = window.left - f64::from(scrolled.x / chart.width()) * window.width;
                flame.set_window(plan, window.width, left);
            }
        }

        let window = flame.camera.current();
        let to_x = |f: f64| chart.left() + ((f - window.left) / window.width) as f32 * chart.width();
        let to_f = |x: f32| window.left + f64::from((x - chart.left()) / chart.width()) * window.width;
        let row_top = |depth: usize| origin.y + PAD + depth as f32 * ROW_H;
        let pointer_slack = f64::from(3.0 / chart.width()) * window.width;
        let hit = |pos: Pos2| -> Option<NodeId> {
            let r = ((pos.y - origin.y - PAD) / ROW_H).floor();
            (r >= 0.0).then(|| flame.hit(r as usize, to_f(pos.x), pointer_slack)).flatten()
        };
        if body.double_clicked() {
            press = Some(Press::Double);
        } else if body.clicked() {
            press = body.interact_pointer_pos().map(|pos| Press::Click(hit(pos)));
        }
        let hovered = body.hover_pos().and_then(hit);

        let first = ((viewport.min.y - PAD) / ROW_H).floor().max(0.0) as usize;
        let last = (((viewport.max.y - PAD) / ROW_H).ceil().max(0.0) as usize + 1).min(rows);
        let focus_depth = plan.depth(flame.camera.focus);
        let zoomed = flame.camera.focus != NodeId::ROOT;
        let bars_painter = painter.with_clip_rect(chart.intersect(clip));
        for depth in first..last {
            let unfold = ease_out(((since - depth as f32 * stagger) / REVEAL_SECONDS).clamp(0.0, 1.0));
            if unfold <= 0.0 {
                continue;
            }
            let fade = if zoomed && depth < focus_depth { 0.5 * unfold } else { unfold };
            let cy = row_top(depth) + ROW_H / 2.0 - (1.0 - unfold) * 6.0;
            let row = &flame.rows[depth];
            let start = row.partition_point(|n| flame.span(*n).right() < window.left);
            let mut last_sliver = f32::NEG_INFINITY;
            for &node in &row[start..] {
                let bar = &flame.bars[node.index()];
                if bar.span.left >= window.right() && bar.span.width > 0.0 {
                    break;
                }
                let (x0, x1) = (to_x(bar.span.left), to_x(bar.span.right()));
                if x1 - x0 < 1.5 {
                    if x0 < last_sliver + 1.5 {
                        continue;
                    }
                    last_sliver = x0;
                }
                let look =
                    Look { hovered: hovered == Some(node), selected: selected == Some(node), focused: has_focus, fade };
                paint_bar(&bars_painter, chart, plan, node, bar, (x0, x1), cy, to_x, look, p);
            }
        }

        if let Some(node) = hovered {
            body.on_hover_ui_at_pointer(|ui| tooltip(ui, plan, node, &flame.bars[node.index()], flame.total_ms));
        }
    });
    flame.scroll_id = Some(output.id);

    match press {
        Some(Press::Click(node)) => {
            flame.clicked = node;
            pick = node.map(Pick::Step).or(pick);
        }
        Some(Press::Double) => {
            if let Some(node) = flame.clicked {
                let (window, span) = (flame.camera.window, flame.span(node));
                if span.contains(window) && span.width <= window.width * (1.0 + 1e-6) {
                    flame.zoom_out(plan);
                } else {
                    flame.zoom_to(node);
                }
            }
        }
        None => {}
    }
    if pick.is_some() {
        ui.ctx().request_repaint();
    }
    pick
}

enum Press {
    Click(Option<NodeId>),
    Double,
}

/// How one bar is drawn this frame.
struct Look {
    hovered: bool,
    selected: bool,
    focused: bool,
    /// Rows fade in when the chart first shows, and context above a zoomed step stays faded.
    fade: f32,
}

#[allow(clippy::too_many_arguments)]
fn paint_bar(
    painter: &egui::Painter,
    chart: Rect,
    plan: &Plan,
    node: NodeId,
    bar: &Bar,
    (x0, x1): (f32, f32),
    cy: f32,
    to_x: impl Fn(f64) -> f32,
    look: Look,
    p: &Palette,
) {
    let timed = bar.heat.is_some();
    let h = if timed { BAR_H } else { UNTIMED_H };
    // Clamped, so a zoomed-in ancestor millions of points wide stays a sane rectangle.
    let (left, right) = (x0.max(chart.left() - 4.0), x1.min(chart.right() + 4.0));
    let mut rect = Rect::from_min_max(pos2(left, cy - h / 2.0), pos2(right, cy + h / 2.0));
    if rect.width() > 3.0 {
        rect = rect.shrink2(vec2(0.5, 0.0));
    } else {
        rect = Rect::from_center_size(rect.center(), vec2(rect.width().max(1.5), h));
    }
    let radius = (rect.width() / 2.0).min(3.0) as u8;
    let lift = |c: Color32| {
        let c = if look.hovered { c.lerp_to_gamma(p.text, 0.12) } else { c };
        c.gamma_multiply(look.fade)
    };

    if timed {
        let own = bar.heat.map_or(p.plan_bar, |share| heat_color(p, share));
        let own_left = to_x(bar.span.right() - bar.own).max(rect.left());
        if own_left <= rect.left() + 0.5 {
            painter.rect_filled(rect, CornerRadius::same(radius), lift(own));
        } else {
            painter.rect_filled(rect, CornerRadius::same(radius), lift(p.plan_bar));
            if rect.right() - own_left >= 0.5 {
                let part = Rect::from_min_max(pos2(own_left, rect.top()), rect.max);
                let corners = CornerRadius { nw: 0, sw: 0, ne: radius, se: radius };
                painter.rect_filled(part, corners, lift(own));
            }
        }
        if bar.stretched.is_some() && rect.width() > 10.0 {
            let y = rect.bottom() - 2.0;
            stretch_mark(painter, rect.left() + 3.0, rect.right() - 3.0, y, p.warning.gamma_multiply(look.fade));
        }
    } else {
        let outline = Stroke::new(1.0, p.border.gamma_multiply(look.fade));
        painter.rect(rect, radius, lift(p.plan_bar_track), outline, StrokeKind::Inside);
    }
    if look.selected {
        let color = if look.focused { p.accent } else { p.accent.gamma_multiply(0.7) }.gamma_multiply(look.fade);
        painter.rect_stroke(rect.expand(1.0), radius + 1, Stroke::new(2.0, color), StrokeKind::Inside);
    }

    let text_left = left.max(chart.left()) + 6.0;
    let room = right.min(chart.right()) - 6.0 - text_left;
    if room < 22.0 {
        return;
    }
    let node_ref = plan.node(node);
    let (text, muted) = if timed {
        (p.text.gamma_multiply(look.fade), p.text.gamma_multiply(0.62 * look.fade))
    } else {
        (p.text_muted.gamma_multiply(look.fade), p.text_faint.gamma_multiply(look.fade))
    };
    let size = if timed { 12.0 } else { 11.0 };
    let label = painter.layout_job(label_job(node_ref, size, room, text));
    let full_w = if label.elided { f32::INFINITY } else { label.size().x };
    painter.galley(pos2(text_left, cy - label.size().y / 2.0), label, text);
    if let Some(ms) = plan.inclusive_ms(node) {
        let g = painter.layout_no_wrap(fmt_ms(ms), theme::ui_font(11.0), muted);
        if full_w + g.size().x + 14.0 <= room {
            painter.galley(pos2(text_left + room - g.size().x, cy - g.size().y / 2.0), g, muted);
        }
    }
}

/// `Table scan c`: the operation, and the object it reads in bold, elided to `max_w`.
fn label_job(node: &tableplus_core::plan::PlanNode, size: f32, max_w: f32, color: Color32) -> LayoutJob {
    let fmt = |font: FontId| egui::TextFormat { font_id: font, color, ..Default::default() };
    let mut job = LayoutJob::default();
    job.append(&node.operation, 0.0, fmt(theme::ui_font(size)));
    if let Some(object) = &node.object {
        job.append(object, 5.0, fmt(theme::bold_font(size)));
    }
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job
}

/// A dashed underline: the bar is wider than the step's own reported time.
fn stretch_mark(painter: &egui::Painter, left: f32, right: f32, y: f32, color: Color32) {
    let mut x = left;
    while x < right {
        painter.line_segment([pos2(x, y), pos2((x + 4.0).min(right), y)], Stroke::new(1.5, color));
        x += 7.0;
    }
}

fn tooltip(ui: &mut Ui, plan: &Plan, id: NodeId, bar: &Bar, total_ms: f64) {
    let p = palette(ui);
    let node = plan.node(id);
    ui.set_max_width(380.0);
    ui.label(RichText::new(&node.operation).font(theme::bold_font(13.0)).color(p.text));
    let object: Vec<&str> = [&node.object, &node.index].into_iter().flatten().map(String::as_str).collect();
    if !object.is_empty() {
        ui.label(RichText::new(object.join("  ·  ")).size(12.0).color(p.text_muted));
    }
    if let Some(condition) = &node.condition {
        ui.add(egui::Label::new(RichText::new(condition).font(theme::mono_font(11.5)).color(p.text_muted)).wrap());
    }
    ui.add_space(4.0);
    egui::Grid::new("plan_flame_tip").num_columns(2).striped(false).spacing([14.0, 3.0]).show(ui, |ui| {
        let mut fact = |key: &str, value: String| {
            ui.label(RichText::new(key).size(12.0).color(p.text_muted));
            ui.label(RichText::new(value).size(12.0).color(p.text));
            ui.end_row();
        };
        match plan.inclusive_ms(id) {
            Some(ms) => fact("Time", format!("{}  ·  {} of the statement", fmt_ms(ms), fmt_share(ms / total_ms))),
            None => fact("Time", "not timed".into()),
        }
        match plan.self_ms(id) {
            Some(own) if own > 0.0 => {
                fact("In this step", format!("{}  ·  {}", fmt_ms(own), fmt_share(own / total_ms)))
            }
            Some(_) => fact("In this step", "none".into()),
            None => {}
        }
        if let Some(actual) = node.actual {
            if actual.loops == 0 {
                fact("Rows", "never ran".into());
            } else {
                if let Some(rows) = actual.rows {
                    let per = if actual.loops > 1 { " per loop" } else { "" };
                    fact("Rows", format!("{}{per}", fmt_rows(rows)));
                }
                fact("Loops", fmt_count(actual.loops));
            }
        }
        match plan.estimated_rows(id) {
            Some(rows) => fact("Estimated rows", fmt_rows(rows)),
            None if node.estimate.is_some() => fact("Estimated rows", "not estimated".into()),
            None => {}
        }
    });
    let note = |ui: &mut Ui, text: String, color: Color32| {
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap());
    };
    if let Some(below) = bar.stretched {
        let own = plan.inclusive_ms(id).map_or_else(String::new, |ms| format!("'s {}", fmt_ms(ms)));
        note(
            ui,
            format!(
                "The steps below report {} in all, more than this step{own}. Steps are timed separately, \
                 and MySQL averages over loops, so nested times don't always add up. The bar spans the \
                 steps below.",
                fmt_ms(below)
            ),
            p.warning,
        );
    } else if bar.heat.is_none() {
        note(ui, "The server didn't time this step. Its bar spans the steps below it.".into(), p.text_muted);
    }
}

/// The breadcrumb of the zoom and the color legend. Returns a crumb that was clicked.
fn header_strip(ui: &mut Ui, id: Id, rect: Rect, plan: &Plan, flame: &Flame, p: &Palette) -> Option<NodeId> {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::ZERO, p.header_bg);
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], Stroke::new(1.0, p.border));
    let cy = rect.center().y;
    let legend_left = legend(&painter, pos2(rect.right() - PAD, cy), flame, p);

    let mut path: Vec<NodeId> = ancestry(plan, flame.camera.focus).collect();
    path.reverse();
    let galleys: Vec<_> =
        path.iter().map(|n| painter.layout_job(label_job(plan.node(*n), 12.0, 220.0, p.text))).collect();
    const SEP: f32 = 18.0;
    let room = legend_left - 24.0 - (rect.left() + PAD);
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 12.0).collect();
    // Keep the root and as many of the innermost crumbs as fit, with `…` between.
    let mut shown: Vec<Option<usize>> = (0..path.len()).map(Some).collect();
    if widths.iter().sum::<f32>() + SEP * (path.len() - 1) as f32 > room && path.len() > 2 {
        let ellipsis = 24.0 + SEP;
        let mut used = widths[0] + SEP + ellipsis;
        let mut keep = path.len();
        while keep > 1 && used + widths[keep - 1] + SEP <= room {
            keep -= 1;
            used += widths[keep] + SEP;
        }
        let keep = keep.min(path.len() - 1);
        shown = std::iter::once(Some(0)).chain(std::iter::once(None)).chain((keep..path.len()).map(Some)).collect();
    }

    let mut clicked = None;
    let mut x = rect.left() + PAD - 6.0;
    for (i, crumb) in shown.iter().enumerate() {
        if i > 0 {
            chevron(&painter, pos2(x + SEP / 2.0, cy), p.text_faint);
            x += SEP;
        }
        let Some(k) = *crumb else {
            painter.text(pos2(x + 6.0, cy), Align2::LEFT_CENTER, "…", theme::ui_font(12.0), p.text_muted);
            x += 24.0;
            continue;
        };
        let galley = galleys[k].clone();
        let w = widths[k];
        let r = Rect::from_min_size(pos2(x, cy - 11.0), vec2(w, 22.0));
        let last = k == path.len() - 1;
        let resp = ui.interact(r, id.with(("crumb", k)), Sense::click());
        if resp.hovered() && !last {
            painter.rect_filled(r, CornerRadius::same(5), p.hover);
        }
        let color = if last || resp.hovered() { p.text } else { p.text_muted };
        painter.galley(pos2(r.left() + 6.0, cy - galley.size().y / 2.0), galley, color);
        if resp.clicked() && !last {
            clicked = Some(path[k]);
        }
        x += w;
    }
    let hints = match plan.inclusive_ms(flame.camera.focus) {
        _ if flame.camera.focus == NodeId::ROOT => vec!["Double-click a step to zoom in".to_string()],
        Some(ms) => {
            let share = fmt_share(ms / flame.total_ms);
            vec![format!("{}  ·  {share} of the statement", fmt_ms(ms)), format!("{}  ·  {share}", fmt_ms(ms))]
        }
        None => vec!["not timed".into()],
    };
    for hint in hints {
        let g = painter.layout_no_wrap(hint, theme::ui_font(11.5), p.text_faint);
        if x + 10.0 + g.size().x <= legend_left - 20.0 {
            painter.galley(pos2(x + 10.0, cy - g.size().y / 2.0), g, p.text_faint);
            break;
        }
    }
    clicked
}

fn chevron(painter: &egui::Painter, center: Pos2, color: Color32) {
    let points = vec![center + vec2(-1.5, -3.5), center + vec2(2.0, 0.0), center + vec2(-1.5, 3.5)];
    painter.add(Shape::line(points, Stroke::new(1.3, color)));
}

/// `■ steps below  ■■■ own time  ▭ not timed`, right-aligned at `right_center`; returns its left.
fn legend(painter: &egui::Painter, right_center: Pos2, flame: &Flame, p: &Palette) -> f32 {
    let font = theme::ui_font(11.0);
    let untimed = flame.bars.iter().any(|b| b.heat.is_none());
    let stretched = flame.bars.iter().any(|b| b.stretched.is_some());
    let mut x = right_center.x;
    let y = right_center.y;
    let mut item = |text: &str, swatch_w: f32, paint: &dyn Fn(Rect)| {
        let r = painter.text(pos2(x, y), Align2::RIGHT_CENTER, text, font.clone(), p.text_muted);
        let swatch = Rect::from_min_max(pos2(r.left() - 5.0 - swatch_w, y - 4.0), pos2(r.left() - 5.0, y + 4.0));
        paint(swatch);
        x = swatch.left() - 12.0;
    };
    if stretched {
        item("wider than its time", 15.0, &|r| stretch_mark(painter, r.left(), r.right(), r.center().y, p.warning));
    }
    if untimed {
        let outline = Stroke::new(1.0, p.text_faint);
        item("not timed", 10.0, &|r| {
            painter.rect(r, 2.0, p.plan_bar_track, outline, StrokeKind::Inside);
        });
    }
    item("own time", 24.0, &|r| {
        let w = r.width() / 3.0;
        for (i, share) in [0.0, NOTABLE_SHARE, HOT_SHARE].into_iter().enumerate() {
            let part = Rect::from_min_size(pos2(r.left() + i as f32 * w, r.top()), vec2(w, r.height()));
            let corners = match i {
                0 => CornerRadius { nw: 2, sw: 2, ne: 0, se: 0 },
                2 => CornerRadius { nw: 0, sw: 0, ne: 2, se: 2 },
                _ => CornerRadius::ZERO,
            };
            painter.rect_filled(part, corners, heat_color(p, share));
        }
    });
    item("steps below", 10.0, &|r| {
        painter.rect_filled(r, CornerRadius::same(2), p.plan_bar);
    });
    x
}

/// Arrows walk the bars, Enter zooms in, Home zooms all the way out, and Esc zooms out a step,
/// then closes the details, then clears the selection.
fn keyboard(ui: &Ui, plan: &Plan, flame: &mut Flame, selected: Option<NodeId>, detail_open: bool) -> Option<Pick> {
    let pressed: Vec<Key> = ui.input(|i| {
        [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight, Key::Enter, Key::Space, Key::Escape, Key::Home]
            .into_iter()
            .filter(|k| i.key_pressed(*k))
            .collect()
    });
    let mut pick = None;
    for key in pressed {
        let sel = if let Some(Pick::Step(n)) = pick { Some(n) } else { selected };
        let step = sel.and_then(|s| match key {
            Key::ArrowUp => plan.parent(s),
            Key::ArrowDown => {
                plan.children(s).iter().copied().max_by(|a, b| flame.span(*a).width.total_cmp(&flame.span(*b).width))
            }
            Key::ArrowLeft => flame.neighbour(plan, s, -1),
            Key::ArrowRight => flame.neighbour(plan, s, 1),
            _ => None,
        });
        let arrow = matches!(key, Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight);
        pick = match (key, sel) {
            _ if arrow => sel.map_or(Some(Pick::Step(flame.camera.focus)), |_| step.map(Pick::Step).or(pick)),
            (Key::Enter | Key::Space, Some(s)) => {
                flame.zoom_to(s);
                pick
            }
            (Key::Home, _) => {
                flame.zoom_to(NodeId::ROOT);
                pick
            }
            (Key::Escape, _) if flame.zoom_out(plan) => pick,
            (Key::Escape, Some(_)) if detail_open => Some(Pick::CloseDetail),
            (Key::Escape, Some(_)) => Some(Pick::Clear),
            _ => pick,
        };
    }
    pick
}

/// What the Flame view shows for a plan without step times.
fn needs_times(ui: &mut Ui, plan: &Plan) {
    let p = palette(ui);
    let rect = ui.available_rect_before_wrap();
    ui.painter().rect_filled(rect, CornerRadius::ZERO, p.content_bg);
    let (title, body) = if plan.analyzed() {
        (
            "No step times to chart",
            "The server counted rows for these steps but didn't time them, so there is nothing to size the bars by."
                .to_string(),
        )
    } else {
        (
            "The flame chart needs measured times",
            format!(
                "Explain Analyze ({}) runs the statement and times every step. Estimated costs don't add up \
                 through a plan the way times do. A nested loop's inner side is costed once per loop, and steps \
                 above a temporary table cost next to nothing. The Tree view compares each step's estimated cost.",
                crate::app::shortcut_label("⇧E")
            ),
        )
    };
    let width = rect.width().min(440.0) - 40.0;
    let mut top = rect.top() + (rect.height() * 0.32 - 70.0).max(24.0);
    let icon_w = 64.0;
    let icon_left = rect.center().x - icon_w / 2.0;
    let (cool, warm, hot) = (p.plan_bar, p.plan_heat_mid, p.plan_heat_high);
    let rows: [&[(f32, f32, Color32)]; 3] = [
        &[(0.0, 1.0, cool)],
        &[(0.0, 0.62, cool), (0.64, 0.24, cool), (0.88, 0.12, warm)],
        &[(0.0, 0.34, hot), (0.36, 0.2, warm), (0.64, 0.16, cool)],
    ];
    for (row, bars) in rows.iter().enumerate() {
        for &(left, w, color) in bars.iter() {
            let at = pos2(icon_left + left * icon_w, top + row as f32 * 10.0);
            ui.painter().rect_filled(
                Rect::from_min_size(at, vec2(w * icon_w - 2.0, 8.0)),
                2.0,
                color.gamma_multiply(0.8),
            );
        }
    }
    top += 46.0;
    let title_g = ui.painter().layout_no_wrap(title.into(), theme::bold_font(14.0), p.text);
    ui.painter().galley(pos2(rect.center().x - title_g.size().x / 2.0, top), title_g, p.text);
    top += 26.0;
    let mut job = LayoutJob::simple(body, theme::ui_font(12.5), p.text_muted, width);
    job.halign = egui::Align::Center;
    let body_g = ui.painter().layout_job(job);
    ui.painter().galley(pos2(rect.center().x, top), body_g, p.text_muted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tableplus_core::plan::{Actual, ActualTime, PlanFormat, PlanNode};

    fn timed(op: &str, last: f64, loops: u64) -> PlanNode {
        let mut n = PlanNode::new(op);
        n.actual = Some(Actual {
            rows: Some(1.0),
            loops,
            time: Some(ActualTime { first_row_ms: Some(0.0), last_row_ms: last }),
        });
        n
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// join (10 ms) ─┬─ scan (2 ms)
    ///               ├─ hash (untimed) ── scan (3 ms)
    ///               └─ lookup (0.01 ms × 100 loops)
    fn sample() -> (Plan, [NodeId; 5]) {
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 10.0, 1), "");
        let scan = plan.add_child(NodeId::ROOT, timed("scan", 2.0, 1));
        let hash = plan.add_child(NodeId::ROOT, PlanNode::new("hash"));
        let inner = plan.add_child(hash, timed("scan", 3.0, 1));
        let lookup = plan.add_child(NodeId::ROOT, timed("lookup", 0.01, 100));
        (plan, [NodeId::ROOT, scan, hash, inner, lookup])
    }

    #[test]
    fn bars_are_as_wide_as_their_time_with_own_time_on_the_right() {
        let (plan, [root, scan, hash, inner, lookup]) = sample();
        let flame = Flame::new(&plan).unwrap();
        assert_eq!(flame.span(root), Span::ALL);
        assert!(close(flame.span(scan).width, 0.2) && close(flame.span(scan).left, 0.0));
        assert!(close(flame.span(hash).left, 0.2) && close(flame.span(hash).width, 0.3));
        assert_eq!(flame.span(inner), flame.span(hash));
        assert!(close(flame.span(lookup).left, 0.5) && close(flame.span(lookup).width, 0.1));
        // 10 ms less the 6 ms below: the right 40% of the join is its own.
        assert!(close(flame.bars[root.index()].own, 0.4));
        assert!(close(flame.bars[root.index()].heat.unwrap(), 0.4));
        assert_eq!(flame.rows, vec![vec![root], vec![scan, hash, lookup], vec![inner]]);
        // The untimed hash spans its scan, with no heat and no mark.
        let bar = &flame.bars[hash.index()];
        assert_eq!((bar.heat, bar.stretched), (None, None));
    }

    #[test]
    fn a_step_spans_steps_below_that_report_more_time_and_is_marked() {
        // MySQL 8.0.18: a 30 µs scan of a temporary table over 8 ms of materialization.
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("scan", 0.03, 1), "");
        let a = plan.add_child(NodeId::ROOT, timed("materialize", 6.0, 1));
        let b = plan.add_child(NodeId::ROOT, timed("scan", 2.0, 1));
        let flame = Flame::new(&plan).unwrap();
        assert!(close(flame.span(a).width, 0.75) && close(flame.span(b).left, 0.75));
        assert!(close(flame.span(b).right(), 1.0));
        let root = &flame.bars[0];
        assert_eq!(root.stretched, Some(8.0));
        assert!(close(root.own, 0.0));
        // Shares are of the longest step, as in the tree and the summary.
        assert!(close(flame.total_ms, 6.0));
        assert!(close(flame.bars[a.index()].heat.unwrap(), 1.0));
        assert!(close(flame.bars[b.index()].heat.unwrap(), 2.0 / 6.0));
        // A stretched step widens its parent only as far as the parent's own time allows.
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 10.0, 1), "");
        let x = plan.add_child(NodeId::ROOT, timed("lookup", 1.0, 1));
        plan.add_child(x, timed("materialize", 4.0, 1));
        let z = plan.add_child(NodeId::ROOT, timed("scan", 5.0, 1));
        let flame = Flame::new(&plan).unwrap();
        assert!(close(flame.span(x).width, 0.4) && close(flame.span(z).left, 0.4));
        assert!(close(flame.bars[0].own, 0.1));
        // Rounding in the engine's three digits is not worth a mark.
        let mut plan = Plan::new(PlanFormat::MySqlTree, timed("join", 1.0, 1), "");
        plan.add_child(NodeId::ROOT, timed("scan", 1.02, 1));
        assert_eq!(Flame::new(&plan).unwrap().bars[0].stretched, None);
    }

    #[test]
    fn estimate_only_plans_have_no_chart() {
        let mut plan = Plan::new(PlanFormat::MySqlTree, PlanNode::new("limit"), "");
        plan.add_child(NodeId::ROOT, PlanNode::new("scan"));
        assert!(Flame::new(&plan).is_none());
    }

    #[test]
    fn glide_keeps_the_narrower_span_growing_on_screen() {
        let to = Span { left: 0.4, width: 0.001 };
        assert_eq!(glide(Span::ALL, to, 0.0), Span::ALL);
        assert_eq!(glide(Span::ALL, to, 1.0), to);
        // Halfway into a 1000× zoom, the target already fills about half the chart.
        let half = glide(Span::ALL, to, 0.5);
        let on_screen = to.width / half.width;
        assert!((on_screen - 0.5005).abs() < 1e-6, "{on_screen}");
        // Zooming back out mirrors it.
        let back = glide(to, Span::ALL, 0.5);
        assert!((to.width / back.width - 0.5005).abs() < 1e-6);
    }

    #[test]
    fn zooming_out_steps_to_the_nearest_wider_step() {
        let (plan, [root, _, hash, inner, _]) = sample();
        let mut flame = Flame::new(&plan).unwrap();
        flame.zoom_to(inner);
        assert_eq!((flame.camera.focus, flame.camera.window), (inner, flame.span(inner)));
        // The untimed hash is exactly as wide as its scan, so Esc skips past it to the join.
        assert!(flame.zoom_out(&plan));
        assert_eq!(flame.camera.focus, root);
        assert!(!flame.zoom_out(&plan));
        assert_eq!(flame.deepest_containing(&plan, flame.span(hash)), inner);
    }

    #[test]
    fn a_pinch_keeps_the_window_on_the_plan_and_tracks_the_focus() {
        let (plan, [_, scan, _, _, _]) = sample();
        let mut flame = Flame::new(&plan).unwrap();
        flame.set_window(&plan, 0.1, -0.5);
        assert_eq!(flame.camera.window, Span { left: 0.0, width: 0.1 });
        assert_eq!(flame.camera.focus, scan);
        flame.set_window(&plan, 4.0, 0.3);
        assert_eq!(flame.camera.window, Span::ALL);
        assert_eq!(flame.camera.focus, NodeId::ROOT);
    }

    #[test]
    fn hits_find_the_bar_under_the_pointer_or_the_nearest_sliver() {
        let (plan, [_, scan, hash, _, lookup]) = sample();
        let flame = Flame::new(&plan).unwrap();
        assert_eq!(flame.hit(1, 0.1, 0.0), Some(scan));
        assert_eq!(flame.hit(1, 0.35, 0.0), Some(hash));
        assert_eq!(flame.hit(1, 0.55, 0.0), Some(lookup));
        assert_eq!(flame.hit(1, 0.9, 0.0), None);
        assert_eq!(flame.hit(1, 0.605, 0.01), Some(lookup));
        assert_eq!(flame.hit(7, 0.1, 0.0), None);
    }
}
