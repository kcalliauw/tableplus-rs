//! The Plan pane's graph view, in the style of MySQL Workbench's Visual Explain: one box per step,
//! coloured by how it reads its table, joins as diamonds, and edges as wide as the rows flowing up
//! them to the result at the top. Drag or scroll to pan, ⌘-scroll or pinch to zoom at the
//! pointer, arrows to walk the tree. The layout is computed once per plan and collapse state; a
//! frame paints only the steps inside the view.

mod layout;

use std::time::Duration;

use eframe::egui::emath::TSTransform;
use eframe::egui::epaint::{CubicBezierShape, Mesh, Shadow, TextShape};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, EventFilter, FontId, Id, Key, Painter, Pos2, Rect, RichText,
    Sense, Shape, Stroke, StrokeKind, Ui, Vec2, pos2, text::LayoutJob, vec2,
};
use tableplus_core::plan::{Access, NodeId, Plan, PlanNode};

use crate::icons::{self, Icon};
use crate::theme::{self, Palette, palette};
use crate::widgets::{fmt_count, fmt_factor, fmt_ms, fmt_rows};
use layout::{BOX_W, Layout, ROW_GAP};

const MIN_ZOOM: f32 = 0.08;
const MAX_ZOOM: f32 = 2.5;
/// Below this zoom the boxes drop their text and the graph reads as a map of colours.
const TEXT_ZOOM: f32 = 0.36;
/// The first view fits the plan, but no smaller than this; a larger plan opens at its top.
const OPEN_ZOOM: f32 = 0.5;
const MOVE_SECONDS: f32 = 0.3;
const MARGIN: f32 = 28.0;
/// Room kept free along the bottom for the legend and the zoom controls.
const OVERLAY_H: f32 = 40.0;
/// The join diamond's centre below the top of its slot, and its half-diagonal.
const DIAMOND_Y: f32 = 21.0;
const DIAMOND_R: f32 = 20.0;
/// The `Result` cap above the root.
const CAP_GAP: f32 = 40.0;
const CAP_H: f32 = 26.0;

/// What a step shows, worked out once per plan.
struct Step {
    access: Option<Access>,
    join: bool,
    /// Rows the step passes up over all its loops, or the estimate: its edge's width and label.
    flow: Option<f64>,
    /// `287 rows`, `≈ 1,616 rows`, `never ran`.
    rows: String,
    /// `× 830` when the step ran more than once.
    loops: Option<String>,
    /// Time including the steps below, or the estimated cost.
    value: Option<String>,
    /// Share of the statement's time with and without the steps below, or of the costliest
    /// step's cost.
    bar: Option<(f32, f32)>,
    misestimate: Option<f64>,
    descendants: usize,
}

struct Prepared {
    /// Every node's id, by index, so per-node vectors map back to the plan.
    ids: Vec<NodeId>,
    layout: Layout,
    steps: Vec<Step>,
    analyzed: bool,
    /// `20 rows · 9.38 ms`, on the cap above the root.
    result: String,
    /// The access classes in this plan, for the legend.
    legend: Vec<Access>,
    has_joins: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct Camera {
    /// The graph point at the centre of the canvas.
    center: Pos2,
    zoom: f32,
}

impl Camera {
    fn to_screen(self, canvas: Rect, p: Pos2) -> Pos2 {
        canvas.center() + (p - self.center) * self.zoom
    }

    fn to_graph(self, canvas: Rect, s: Pos2) -> Pos2 {
        self.center + (s - canvas.center()) / self.zoom
    }

    fn rect(self, canvas: Rect, r: Rect) -> Rect {
        Rect::from_min_max(self.to_screen(canvas, r.min), self.to_screen(canvas, r.max))
    }
}

/// A camera move animated from `from` to `to`, zooming at a constant rate.
#[derive(Clone, Copy)]
struct Flight {
    from: Camera,
    to: Camera,
    /// A point on screen, from the canvas centre, that stays over the same graph point all the
    /// way: where a zoom happens. Without one the view glides straight to `to`.
    pivot: Option<Vec2>,
    start: f64,
    seconds: f32,
}

impl Flight {
    fn at(&self, now: f64) -> (Camera, bool) {
        let t = ((now - self.start) as f32 / self.seconds).clamp(0.0, 1.0);
        let e = ease_out(t);
        let zoom = (self.from.zoom.ln() + (self.to.zoom.ln() - self.from.zoom.ln()) * e).exp();
        let center = match self.pivot {
            Some(off) => self.from.center + off / self.from.zoom - off / zoom,
            None => self.from.center + (self.to.center - self.from.center) * e,
        };
        (Camera { center, zoom }, t >= 1.0)
    }

    fn shift(&mut self, d: Vec2) {
        self.from.center += d;
        self.to.center += d;
    }
}

/// Boxes gliding from where they were before a step was folded or unfolded.
struct Motion {
    from: Vec<Option<Rect>>,
    /// The step that was folded; boxes come out of it and go back into it.
    origin: NodeId,
    start: f64,
}

/// The graph's state in a Plan pane: its cached layout, camera and folded steps.
pub struct GraphView {
    prepared: Option<Prepared>,
    collapsed: Vec<bool>,
    camera: Camera,
    flight: Option<Flight>,
    motion: Option<Motion>,
    /// The frame the graph was last drawn in; a gap means it was hidden in between.
    drawn_frame: Option<u64>,
    opened_at: f64,
    /// When a wheel or pinch last zoomed, so text is laid out afresh once the zoom settles.
    zoomed_at: f64,
    /// Frames left in which to bring the selection into view, long enough for a detail panel
    /// that opens with the selection to take its room.
    reveal: u8,
    last_selected: Option<NodeId>,
    canvas_size: Option<Vec2>,
}

impl Default for GraphView {
    fn default() -> Self {
        Self {
            prepared: None,
            collapsed: Vec::new(),
            camera: Camera { center: Pos2::ZERO, zoom: 1.0 },
            flight: None,
            motion: None,
            drawn_frame: None,
            opened_at: 0.0,
            zoomed_at: f64::NEG_INFINITY,
            reveal: 0,
            last_selected: None,
            canvas_size: None,
        }
    }
}

impl GraphView {
    fn fly(&mut self, to: Camera, now: f64, seconds: f32) {
        let to = Camera { zoom: to.zoom.clamp(MIN_ZOOM, MAX_ZOOM), ..to };
        self.flight = Some(Flight { from: self.camera, to, pivot: None, start: now, seconds });
    }

    /// Zooms to `zoom` over a moment, keeping the graph point under `pivot` (from the canvas
    /// centre) in place.
    fn zoom_to(&mut self, zoom: f32, pivot: Vec2, now: f64) {
        let from = self.camera;
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let to = Camera { center: from.center + pivot / from.zoom - pivot / zoom, zoom };
        self.flight = Some(Flight { from, to, pivot: Some(pivot), start: now, seconds: 0.3 });
    }

    /// Keeps the graph still under the canvas's top-left corner when the canvas changes size, so
    /// a detail panel opening beside it covers the graph instead of shifting it.
    fn anchor(&mut self, canvas: Rect) {
        if let Some(old) = self.canvas_size.replace(canvas.size())
            && old != canvas.size()
        {
            let d = (canvas.size() - old) / (2.0 * self.camera.zoom);
            self.camera.center += d;
            if let Some(f) = &mut self.flight {
                f.shift(d);
            }
        }
    }

    /// Where the camera is headed: the end of the current flight, or where it is.
    fn target(&self) -> Camera {
        self.flight.map_or(self.camera, |f| f.to)
    }

    fn fit(&self, canvas: Rect) -> Camera {
        let bounds = self.prepared.as_ref().map_or(Rect::ZERO, |p| graph_bounds(&p.layout));
        let room = canvas.shrink(MARGIN).size() - vec2(0.0, OVERLAY_H);
        let zoom = (room.x / bounds.width()).min(room.y / bounds.height()).clamp(MIN_ZOOM, 1.0);
        Camera { center: bounds.center() + vec2(0.0, OVERLAY_H / 2.0 / zoom), zoom }
    }

    /// The first view: the whole plan when it is readable that way, otherwise the selected step
    /// or the top of the plan, where its result comes out.
    fn opening(&self, canvas: Rect, selected: Option<NodeId>) -> Camera {
        let fit = self.fit(canvas);
        let Some(prepared) = &self.prepared else { return fit };
        if fit.zoom >= OPEN_ZOOM {
            return fit;
        }
        if let Some(rect) = selected.and_then(|s| prepared.layout.rect(s)) {
            return Camera { center: rect.center(), zoom: 0.85 };
        }
        let bounds = graph_bounds(&prepared.layout);
        let root = prepared.layout.rect(NodeId::ROOT).unwrap_or(bounds);
        let width_fit = (canvas.width() - 2.0 * MARGIN) / bounds.width();
        let zoom = width_fit.clamp(OPEN_ZOOM, 0.85);
        let x = if width_fit >= OPEN_ZOOM { bounds.center().x } else { root.center().x };
        Camera { center: pos2(x, bounds.top() + (canvas.height() / 2.0 - MARGIN) / zoom), zoom }
    }

    /// Flies the camera so `rect` (graph points) is inside the canvas, unless it already is.
    fn reveal_rect(&mut self, canvas: Rect, rect: Rect, now: f64) {
        let target = self.target();
        let safe = Rect::from_min_max(canvas.min + vec2(MARGIN, MARGIN), canvas.max - vec2(MARGIN, MARGIN + OVERLAY_H));
        let shown = target.rect(canvas, rect);
        let shift = |lo: f32, hi: f32, safe_lo: f32, safe_hi: f32| {
            if hi - lo > safe_hi - safe_lo {
                (lo + hi - safe_lo - safe_hi) / 2.0
            } else if lo < safe_lo {
                lo - safe_lo
            } else if hi > safe_hi {
                hi - safe_hi
            } else {
                0.0
            }
        };
        let d = vec2(
            shift(shown.left(), shown.right(), safe.left(), safe.right()),
            shift(shown.top(), shown.bottom(), safe.top(), safe.bottom()),
        );
        if d.length() > 0.5 {
            self.fly(Camera { center: target.center + d / target.zoom, zoom: target.zoom }, now, 0.28);
        }
    }

    fn zoom_at(&mut self, canvas: Rect, at: Pos2, factor: f32) {
        let g = self.camera.to_graph(canvas, at);
        let zoom = (self.camera.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.camera = Camera { center: g - (at - canvas.center()) / zoom, zoom };
    }

    /// Folds or unfolds `node`, keeping it in place on screen while the other boxes glide.
    fn toggle(&mut self, plan: &Plan, node: NodeId, now: f64, selected: &mut Option<NodeId>) {
        if plan.children(node).is_empty() {
            return;
        }
        let Some(prepared) = self.prepared.take() else { return };
        let before = prepared.layout.rects;
        self.collapsed[node.index()] = !self.collapsed[node.index()];
        let next = prepare(plan, &self.collapsed);
        if let (Some(a), Some(b)) = (before[node.index()], next.layout.rect(node)) {
            let d = b.center() - a.center();
            self.camera.center += d;
            if let Some(f) = &mut self.flight {
                f.shift(d);
            }
        }
        if selected.is_some_and(|s| next.layout.rect(s).is_none()) {
            *selected = Some(node);
        }
        self.prepared = Some(next);
        self.motion = Some(Motion { from: before, origin: node, start: now });
    }

    /// Every step's box this frame, with its opacity: in place, or gliding after a fold.
    fn frame_rects(&mut self, now: f64) -> Vec<Option<(Rect, f32)>> {
        let Some(prepared) = &self.prepared else { return Vec::new() };
        let layout = &prepared.layout;
        let Some(motion) = &self.motion else {
            return layout.rects.iter().map(|r| r.map(|r| (r, 1.0))).collect();
        };
        let t = ((now - motion.start) as f32 / MOVE_SECONDS).min(1.0);
        let e = ease_out(t);
        let origin_from = motion.from[motion.origin.index()].unwrap_or(Rect::ZERO);
        let origin_to = layout.rect(motion.origin).unwrap_or(origin_from);
        let lerp = |a: Rect, b: Rect| Rect::from_min_max(a.min.lerp(b.min, e), a.max.lerp(b.max, e));
        let rects = layout
            .rects
            .iter()
            .zip(&motion.from)
            .map(|(to, from)| match (from, to) {
                (Some(a), Some(b)) => Some((lerp(*a, *b), 1.0)),
                (None, Some(b)) => Some((lerp(shrunk(origin_from, *b), *b), e)),
                (Some(a), None) => (t < 1.0).then(|| (lerp(*a, shrunk(origin_to, *a)), 1.0 - e)),
                (None, None) => None,
            })
            .collect();
        if t >= 1.0 {
            self.motion = None;
        }
        rects
    }
}

/// A box shaped like `like` but smaller, centred on `at`: where folded boxes come from and go.
fn shrunk(at: Rect, like: Rect) -> Rect {
    Rect::from_center_size(at.center(), like.size() * 0.3)
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn prepare(plan: &Plan, collapsed: &[bool]) -> Prepared {
    let analyzed = plan.analyzed();
    let scale = if analyzed {
        plan.nodes().filter_map(|(id, _)| plan.inclusive_ms(id)).fold(plan.total_ms().unwrap_or(0.0), f64::max)
    } else {
        plan.nodes().filter_map(|(_, n)| n.estimate?.cost.map(|c| c.total)).fold(0.0, f64::max)
    };
    let share = |v: f64| if scale > 0.0 { (v / scale).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let steps: Vec<Step> = plan
        .nodes()
        .map(|(id, node)| {
            let (rows, loops, value, bar) = if analyzed {
                let inclusive = plan.inclusive_ms(id);
                let rows = match node.actual {
                    Some(a) if a.loops == 0 => "never ran".to_string(),
                    Some(a) => rows_text(a.rows),
                    None => String::new(),
                };
                let loops = node.actual.filter(|a| a.loops > 1).map(|a| format!("× {}", fmt_count(a.loops)));
                let bar = inclusive.map(|t| (share(t), share(plan.self_ms(id).unwrap_or(0.0))));
                (rows, loops, inclusive.map(fmt_ms), bar)
            } else {
                let rows = node.estimate.filter(|e| e.rows > 0.0).map(|e| format!("≈ {}", rows_text(e.rows)));
                let cost = node.estimate.and_then(|e| e.cost);
                let bar = cost.map(|c| (share(c.total), 0.0));
                (rows.unwrap_or_default(), None, cost.map(|c| format!("cost {}", fmt_rows(c.total))), bar)
            };
            Step {
                access: node.access(),
                join: node.is_join(),
                flow: flow(plan, id, analyzed),
                rows,
                loops,
                value,
                bar,
                misestimate: node.row_estimate_ratio().filter(|_| node.is_misestimated()),
                descendants: plan.descendant_count(id),
            }
        })
        .collect();
    let root = &steps[NodeId::ROOT.index()];
    let result = match (root.flow, plan.total_ms()) {
        (Some(rows), Some(ms)) => format!("{} · {}", rows_text(rows), fmt_ms(ms)),
        (Some(rows), None) => format!("≈ {}", rows_text(rows)),
        (None, Some(ms)) => fmt_ms(ms),
        (None, None) => String::new(),
    };
    let order = [Access::FullScan, Access::IndexScan, Access::Range, Access::Lookup, Access::Constant];
    let legend = order.into_iter().filter(|a| steps.iter().any(|s| s.access.map(legend_class) == Some(*a))).collect();
    Prepared {
        ids: plan.nodes().map(|(id, _)| id).collect(),
        layout: Layout::new(plan, collapsed),
        has_joins: steps.iter().any(|s| s.join),
        steps,
        analyzed,
        result,
        legend,
    }
}

fn rows_text(rows: f64) -> String {
    format!("{} {}", fmt_rows(rows), if rows == 1.0 { "row" } else { "rows" })
}

/// Rows a step passes up over all its loops, or its estimate per loop. A step without numbers,
/// such as MySQL's `Hash`, passes on what its children produce.
fn flow(plan: &Plan, id: NodeId, analyzed: bool) -> Option<f64> {
    let node = plan.node(id);
    let own = if analyzed {
        node.actual.map(|a| a.rows * a.loops as f64)
    } else {
        node.estimate.map(|e| e.rows).filter(|r| *r > 0.0)
    };
    own.or_else(|| {
        let below: Vec<f64> = plan.children(id).iter().filter_map(|c| flow(plan, *c, analyzed)).collect();
        (!below.is_empty()).then(|| below.iter().sum())
    })
}

/// Unique and non-unique lookups share a colour, so the legend shows them as one.
fn legend_class(access: Access) -> Access {
    if access == Access::UniqueLookup { Access::Lookup } else { access }
}

fn legend_label(access: Access) -> &'static str {
    match access {
        Access::FullScan => "Full table scan",
        Access::IndexScan => "Full index scan",
        Access::Range => "Range",
        Access::Lookup | Access::UniqueLookup => "Key lookup",
        Access::Constant => "Constant",
    }
}

/// Workbench's colours: red for a full table scan, red-orange for a full index scan, orange for a
/// range, green for key lookups and blue for a constant row.
pub(crate) fn access_color(access: Access, dark: bool) -> Color32 {
    let (light, night) = match access {
        Access::FullScan => (0xE5484D, 0xFF6369),
        Access::IndexScan => (0xEC6A2C, 0xFF8A4D),
        Access::Range => (0xE59A00, 0xFFBA3B),
        Access::Lookup | Access::UniqueLookup => (0x2E9E5B, 0x3DD68C),
        Access::Constant => (0x1B9BD8, 0x55C3F7),
    };
    let rgb: u32 = if dark { night } else { light };
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn cap_rect(root: Rect) -> Rect {
    Rect::from_center_size(pos2(root.center().x, root.top() - CAP_GAP - CAP_H / 2.0), vec2(BOX_W, CAP_H))
}

fn graph_bounds(layout: &Layout) -> Rect {
    layout.bounds.union(cap_rect(layout.rect(NodeId::ROOT).unwrap_or(Rect::ZERO)))
}

/// The canvas and box surfaces, which the palette has no names for.
struct Surfaces {
    canvas: Color32,
    card: Color32,
    card_border: Color32,
    dots: Color32,
    edge: Color32,
    shadow: Option<Shadow>,
}

fn surfaces(p: &Palette, dark: bool) -> Surfaces {
    if dark {
        Surfaces {
            canvas: p.content_bg,
            card: Color32::from_rgb(0x28, 0x28, 0x2B),
            card_border: Color32::from_rgb(0x3D, 0x3D, 0x42),
            dots: Color32::from_rgb(0x2D, 0x2D, 0x31),
            edge: Color32::from_rgb(0x58, 0x59, 0x60),
            shadow: None,
        }
    } else {
        Surfaces {
            canvas: Color32::from_rgb(0xF5, 0xF6, 0xF8),
            card: Color32::WHITE,
            card_border: Color32::from_rgb(0xD8, 0xDA, 0xDF),
            dots: Color32::from_rgb(0xDD, 0xDF, 0xE4),
            edge: Color32::from_rgb(0xB9, 0xBE, 0xC8),
            shadow: Some(Shadow { offset: [0, 1], blur: 6, spread: 0, color: Color32::from_black_alpha(18) }),
        }
    }
}

/// Text is laid out at a font size close to the zoom and its glyphs scaled the rest of the way.
/// At rest that size follows the zoom finely, so text is crisp; while the zoom moves it steps by
/// powers of √2, so a zoom gesture doesn't rasterize the font at every size it passes.
#[derive(Clone, Copy)]
struct TextScale {
    layout: f32,
    mesh: f32,
}

impl TextScale {
    fn new(zoom: f32, moving: bool) -> Self {
        let layout = if moving { 2f32.powf((zoom.log2() * 2.0).ceil() / 2.0) } else { (zoom * 32.0).round() / 32.0 };
        Self { layout, mesh: zoom / layout }
    }

    fn font(&self, font: FontId) -> FontId {
        FontId::new(font.size * self.layout, font.family)
    }

    fn format(&self, font: FontId, color: Color32) -> egui::TextFormat {
        egui::TextFormat::simple(self.font(font), color)
    }
}

/// One elided line of `text`, `max_width` wide in graph points.
fn line(text: &str, font: FontId, color: Color32, max_width: f32, scale: TextScale) -> LayoutJob {
    let mut job = LayoutJob::single_section(text.to_string(), scale.format(font, color));
    elide(&mut job, max_width, scale);
    job
}

fn elide(job: &mut LayoutJob, max_width: f32, scale: TextScale) {
    job.wrap.max_width = max_width * scale.layout;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
}

/// Paints `job` anchored at `pos` and returns its size on screen.
fn paint_text(painter: &Painter, pos: Pos2, align: Align2, job: LayoutJob, scale: TextScale) -> Vec2 {
    let galley = painter.layout_job(job);
    let mut shape = TextShape::new(Pos2::ZERO, galley, Color32::PLACEHOLDER);
    if (scale.mesh - 1.0).abs() > 1e-3 {
        shape.transform(TSTransform::from_scaling(scale.mesh));
    }
    let size = shape.galley.size();
    shape.pos = align.anchor_size(pos, size).min;
    painter.add(shape);
    size
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}

/// What a frame paints with.
struct Frame<'a> {
    painter: &'a Painter,
    canvas: Rect,
    camera: Camera,
    p: &'static Palette,
    s: Surfaces,
    dark: bool,
    analyzed: bool,
    /// `None` when zoomed out too far for text.
    text: Option<TextScale>,
    focused: bool,
    /// The selected step and everything below it, by node index: the rows it is made of.
    lit: Vec<bool>,
}

impl Frame<'_> {
    fn zoom(&self) -> f32 {
        self.camera.zoom
    }

    fn screen(&self, r: Rect) -> Rect {
        self.camera.rect(self.canvas, r)
    }

    fn accent(&self) -> Color32 {
        if self.focused { self.p.accent } else { self.p.accent.gamma_multiply(0.6) }
    }
}

#[derive(Clone, Copy)]
struct Look {
    selected: bool,
    /// 0 to 1, eased in and out.
    hovered: f32,
    alpha: f32,
}

/// Draws the graph and handles its input. `selected` and `detail_open` are shared with the tree,
/// so either view picks up where the other left off.
pub fn show(
    ui: &mut Ui,
    id: Id,
    plan: &Plan,
    view: &mut GraphView,
    selected: &mut Option<NodeId>,
    detail_open: &mut bool,
) {
    let p = palette(ui);
    let dark = ui.visuals().dark_mode;
    let now = ui.input(|i| i.time);
    let frame_nr = ui.ctx().cumulative_frame_nr();
    let canvas = ui.max_rect();
    if view.prepared.is_none() {
        view.collapsed = vec![false; plan.node_count()];
        view.prepared = Some(prepare(plan, &view.collapsed));
    }

    match view.drawn_frame {
        None => {
            let open = view.opening(canvas, *selected);
            view.camera = Camera { zoom: open.zoom * 0.94, ..open };
            view.fly(open, now, 0.45);
            view.opened_at = now;
        }
        Some(f) if f + 1 < frame_nr => view.reveal = 2,
        _ => {}
    }
    view.drawn_frame = Some(frame_nr);
    view.anchor(canvas);

    let response = ui.interact(canvas, id, Sense::click_and_drag());
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let focused = response.has_focus();
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
        keyboard(ui, plan, view, selected, detail_open, now);
    }

    if response.dragged() {
        view.flight = None;
        view.camera.center -= response.drag_delta() / view.camera.zoom;
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    }
    if response.contains_pointer() {
        let (zoom, scroll, pointer) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
        if zoom != 1.0 {
            view.flight = None;
            view.zoomed_at = now;
            view.zoom_at(canvas, pointer.unwrap_or(canvas.center()), zoom);
        }
        if scroll != Vec2::ZERO {
            view.flight = None;
            view.camera.center -= scroll / view.camera.zoom;
        }
    }

    let rects = view.frame_rects(now);
    let ids = view.prepared.as_ref().map(|p| p.ids.clone()).unwrap_or_default();
    let camera = view.camera;
    let hit = |pos: Pos2| -> Option<NodeId> {
        rects.iter().zip(&ids).rev().find_map(|(r, id)| {
            let (rect, alpha) = (*r)?;
            (alpha > 0.5 && camera.rect(canvas, rect).contains(pos)).then_some(*id)
        })
    };
    let fold_pill_at = |pos: Pos2| -> Option<NodeId> {
        rects.iter().zip(&ids).find_map(|(r, id)| {
            let (rect, _) = (*r)?;
            let pill = fold_pill_rect(camera.rect(canvas, rect), camera.zoom).expand(3.0);
            (view.collapsed[id.index()] && pill.contains(pos)).then_some(*id)
        })
    };
    let hovered = response.hover_pos().filter(|_| !response.dragged()).and_then(hit);
    let mut toggle = None;
    if let Some(pos) = response.interact_pointer_pos() {
        if response.double_clicked() {
            match hit(pos) {
                Some(node) => toggle = Some(node),
                None => view.zoom_to(view.target().zoom * 2.0, pos - canvas.center(), now),
            }
        } else if response.clicked() {
            if let Some(node) = fold_pill_at(pos) {
                toggle = Some(node);
            } else if let Some(node) = hit(pos) {
                *selected = Some(node);
                *detail_open = true;
            }
        }
    }
    if let Some(node) = toggle {
        view.toggle(plan, node, now, selected);
    }

    // Keep the selection in view after it moves, or when the graph comes back into view.
    if *selected != view.last_selected {
        view.last_selected = *selected;
        view.reveal = 2;
    }
    if view.reveal > 0 {
        view.reveal -= 1;
        if let Some(rect) = selected.and_then(|s| view.prepared.as_ref()?.layout.rect(s)) {
            view.reveal_rect(canvas, rect.expand(8.0), now);
        }
    }
    if let Some(flight) = view.flight {
        let (camera, done) = flight.at(now);
        view.camera = camera;
        if done {
            view.flight = None;
        }
    }

    let rects = view.frame_rects(now);
    let Some(prepared) = view.prepared.as_ref() else { return };
    let opacity = ease_out(((now - view.opened_at) as f32 / 0.3).clamp(0.0, 1.0));
    let moving = view.flight.is_some() || view.motion.is_some() || now - view.zoomed_at < 0.2;
    if moving || opacity < 1.0 {
        ui.ctx().request_repaint();
    } else if now - view.zoomed_at < 0.4 {
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }

    let mut painter = ui.painter_at(canvas);
    let s = surfaces(p, dark);
    painter.rect_filled(canvas, CornerRadius::ZERO, s.canvas);
    dots(&painter, canvas, view.camera, s.dots);
    painter.set_opacity(opacity);

    let mut lit = vec![false; plan.node_count()];
    let mut stack: Vec<NodeId> = selected.iter().copied().collect();
    while let Some(n) = stack.pop() {
        lit[n.index()] = true;
        stack.extend(plan.children(n));
    }
    let f = Frame {
        painter: &painter,
        canvas,
        camera: view.camera,
        p,
        s,
        dark,
        analyzed: prepared.analyzed,
        text: (view.camera.zoom >= TEXT_ZOOM).then(|| TextScale::new(view.camera.zoom, moving)),
        focused,
        lit,
    };
    let visible =
        Rect::from_min_max(view.camera.to_graph(canvas, canvas.min), view.camera.to_graph(canvas, canvas.max));

    for (i, slot) in rects.iter().enumerate() {
        let Some((rect, alpha)) = slot else { continue };
        let child = prepared.ids[i];
        let Some(parent) = plan.parent(child) else { continue };
        let Some((parent_rect, parent_alpha)) = rects[parent.index()] else { continue };
        let inputs: Vec<NodeId> =
            plan.children(parent).iter().copied().filter(|c| rects[c.index()].is_some()).collect();
        let edge = Edge::new(&prepared.steps, child, *rect, parent, parent_rect, &inputs);
        if edge.bounds().expand(24.0).intersects(visible) {
            edge.paint(&f, f.lit[i], prepared.steps[i].flow, alpha.min(parent_alpha));
        }
    }
    if let Some((root, alpha)) = rects[NodeId::ROOT.index()] {
        cap(&f, root, prepared, alpha);
    }
    for (i, slot) in rects.iter().enumerate() {
        let Some((rect, alpha)) = *slot else { continue };
        if !rect.expand(20.0).intersects(visible) {
            continue;
        }
        let node_id = prepared.ids[i];
        let look = Look {
            selected: *selected == Some(node_id),
            hovered: ui.ctx().animate_bool_with_time(id.with(("hover", i)), hovered == Some(node_id), 0.12),
            alpha,
        };
        let step = &prepared.steps[i];
        let screen = f.screen(rect);
        if step.join {
            paint_join(&f, screen, plan.node(node_id), step, look);
        } else {
            paint_box(&f, screen, plan.node(node_id), step, look);
        }
        if view.collapsed[i] && alpha >= 1.0 {
            fold_pill(&f, screen, step.descendants);
        }
    }
    drop(f);

    if let Some(node) = hovered {
        let step = &prepared.steps[node.index()];
        response.on_hover_ui_at_pointer(|ui| tooltip(ui, plan.node(node), step, prepared.analyzed, dark));
    }
    legend(ui, canvas, prepared, p, dark);
    controls(ui, id, canvas, view, now);
}

/// A dot grid that moves with the graph, so panning and zooming feel anchored.
fn dots(painter: &Painter, canvas: Rect, camera: Camera, color: Color32) {
    let mut step = 24.0;
    while step * camera.zoom < 14.0 {
        step *= 2.0;
    }
    let min = camera.to_graph(canvas, canvas.min);
    let max = camera.to_graph(canvas, canvas.max);
    let size = (1.6 * camera.zoom).clamp(1.0, 2.0);
    let mut mesh = Mesh::default();
    let mut y = (min.y / step).floor() * step;
    while y <= max.y {
        let mut x = (min.x / step).floor() * step;
        while x <= max.x {
            let at = camera.to_screen(canvas, pos2(x, y));
            mesh.add_colored_rect(Rect::from_center_size(at, Vec2::splat(size)), color);
            x += step;
        }
        y += step;
    }
    painter.add(Shape::mesh(mesh));
}

fn paint_box(f: &Frame, r: Rect, node: &PlanNode, step: &Step, look: Look) {
    let (p, z, painter) = (f.p, f.zoom(), f.painter);
    let corner = radius(8.0 * z);
    let tint = step.access.map(|a| access_color(a, f.dark));
    let (mut card, mut border) = (f.s.card, f.s.card_border);
    if let Some(c) = tint {
        card = mix(card, c, if f.dark { 0.11 } else { 0.06 });
        border = mix(border, c, if f.dark { 0.42 } else { 0.5 });
    }
    border = mix(border, p.text_faint, look.hovered * 0.7);
    let fade = |c: Color32| c.gamma_multiply(look.alpha);

    if let Some(shadow) = f.s.shadow {
        let lift = Shadow {
            offset: [0, 1 + (look.hovered * 2.0) as i8],
            blur: shadow.blur + (look.hovered * 8.0) as u8,
            color: fade(shadow.color),
            ..shadow
        };
        painter.add(lift.as_shape(r, corner));
    }
    if look.selected {
        selection_ring(f, r.expand(3.0), radius(8.0 * z + 3.0), look.alpha);
    }
    painter.rect(r, corner, fade(card), Stroke::new(1.0, fade(border)), StrokeKind::Inside);
    if let Some(c) = tint {
        let strip = Rect::from_min_max(r.min, pos2(r.left() + (4.0 * z).max(2.0), r.bottom()));
        painter.rect_filled(strip, CornerRadius { nw: corner.nw, sw: corner.sw, ne: 0, se: 0 }, fade(c));
    }

    // Along the bottom: the share of the statement's time with the steps below and in the step
    // itself, or of the costliest step's cost.
    if let Some((total, own)) = step.bar {
        let track = Rect::from_min_max(
            pos2(r.left() + 14.0 * z, r.bottom() - 9.5 * z),
            pos2(r.right() - 12.0 * z, (r.bottom() - 6.0 * z).max(r.bottom() - 9.5 * z + 1.5)),
        );
        let bar_r = radius(track.height() / 2.0);
        let (fill, strong) = if f.analyzed { (p.plan_bar, p.plan_bar_self) } else { (p.plan_cost, p.plan_cost) };
        painter.rect_filled(track, bar_r, fade(p.plan_bar_track));
        for (share, color) in [(total, fill), (own.min(total), strong)] {
            if share > 0.0 {
                let w = (track.width() * share).max(track.height());
                painter.rect_filled(Rect::from_min_size(track.min, vec2(w, track.height())), bar_r, fade(color));
            }
        }
    }

    let Some(scale) = f.text else { return };
    let left = r.left() + 14.0 * z;
    let w = (r.right() - 10.0 * z - left) / z;
    let (text, muted, faint) = (fade(p.text), fade(p.text_muted), fade(p.text_faint));
    paint_text(
        painter,
        pos2(left, r.top() + 9.0 * z),
        Align2::LEFT_TOP,
        line(&node.operation, theme::bold_font(12.5), text, w, scale),
        scale,
    );

    let mut detail = LayoutJob::default();
    if let Some(object) = &node.object {
        detail.append(object, 0.0, scale.format(theme::bold_font(12.0), text));
    }
    if let Some(index) = &node.index {
        detail.append(&format!("· {index}"), 5.0 * scale.layout, scale.format(theme::ui_font(12.0), muted));
    }
    if let Some(condition) = &node.condition {
        let gap = if detail.is_empty() { 0.0 } else { 8.0 * scale.layout };
        detail.append(&condition.replace('\n', " "), gap, scale.format(theme::mono_font(11.0), faint));
    }
    if !detail.is_empty() {
        elide(&mut detail, w, scale);
        paint_text(painter, pos2(left, r.top() + 28.5 * z), Align2::LEFT_TOP, detail, scale);
    }

    let y = r.top() + 47.0 * z;
    let mut x = left;
    if !step.rows.is_empty() {
        let color = if step.misestimate.is_some() { fade(p.warning) } else { muted };
        let job = line(&step.rows, theme::ui_font(11.5), color, w, scale);
        x += paint_text(painter, pos2(x, y), Align2::LEFT_TOP, job, scale).x;
    }
    if let Some(loops) = &step.loops {
        x += 4.0 * z;
        x += paint_text(
            painter,
            pos2(x, y),
            Align2::LEFT_TOP,
            line(loops, theme::ui_font(11.5), faint, w, scale),
            scale,
        )
        .x;
    }
    if let Some(ratio) = step.misestimate {
        misestimate_badge(f, pos2(x + 5.0 * z, y + 7.5 * z), ratio, scale, look.alpha);
    }
    if let Some(value) = &step.value {
        let job = line(value, theme::bold_font(11.5), text, w, scale);
        paint_text(painter, pos2(r.right() - 10.0 * z, y), Align2::RIGHT_TOP, job, scale);
    }
}

fn paint_join(f: &Frame, r: Rect, node: &PlanNode, step: &Step, look: Look) {
    let (p, z, painter) = (f.p, f.zoom(), f.painter);
    let fade = |c: Color32| c.gamma_multiply(look.alpha);
    let c = pos2(r.center().x, r.top() + DIAMOND_Y * z);
    let d = DIAMOND_R * z;
    let diamond = |grow: f32| {
        let d = d + grow;
        vec![pos2(c.x, c.y - d), pos2(c.x + d, c.y), pos2(c.x, c.y + d), pos2(c.x - d, c.y)]
    };
    let accent = f.accent();
    if look.selected {
        painter.add(Shape::convex_polygon(
            diamond(6.0 * z.min(1.0) + 2.0),
            fade(accent.gamma_multiply(0.22)),
            Stroke::NONE,
        ));
    }
    let (fill, stroke, glyph) = if look.selected {
        (accent, accent, p.accent_text)
    } else {
        (f.s.card, mix(f.s.card_border, p.text_muted, 0.3 + look.hovered * 0.5), p.text_muted)
    };
    if let Some(shadow) = f.s.shadow {
        let shadow = Shadow { color: fade(shadow.color), ..shadow };
        painter.add(shadow.as_shape(Rect::from_center_size(c, Vec2::splat(d * 1.25)), radius(4.0 * z)));
    }
    painter.add(Shape::convex_polygon(diamond(0.0), fade(fill), Stroke::new(1.25, fade(stroke))));
    // A bow tie, the relational join symbol.
    let k = 0.4 * d;
    for side in [-1.0, 1.0] {
        let tri = vec![pos2(c.x + side * k, c.y - k * 0.8), pos2(c.x + side * k, c.y + k * 0.8), c];
        painter.add(Shape::convex_polygon(tri, fade(glyph), Stroke::NONE));
    }

    let Some(scale) = f.text else { return };
    // Narrower than the slot, so the curves into the diamond's corners pass beside the label.
    let w = r.width() / z - 34.0;
    let label = if look.selected { fade(accent) } else { fade(p.text) };
    let job = line(&node.operation, theme::bold_font(11.5), label, w, scale);
    paint_text(painter, pos2(c.x, r.top() + 46.0 * z), Align2::CENTER_TOP, job, scale);
    let numbers: Vec<&str> =
        [Some(step.rows.as_str()).filter(|r| !r.is_empty()), step.value.as_deref()].into_iter().flatten().collect();
    if !numbers.is_empty() {
        let color = if step.misestimate.is_some() { fade(p.warning) } else { fade(p.text_muted) };
        let job = line(&numbers.join(" · "), theme::ui_font(11.0), color, w, scale);
        paint_text(painter, pos2(c.x, r.top() + 61.0 * z), Align2::CENTER_TOP, job, scale);
    }
}

fn selection_ring(f: &Frame, r: Rect, corner: CornerRadius, alpha: f32) {
    let color = f.accent();
    let glow = Shadow { offset: [0, 0], blur: 16, spread: 0, color: color.gamma_multiply(0.4 * alpha) };
    f.painter.add(glow.as_shape(r, corner));
    f.painter.rect_stroke(r, corner, Stroke::new(2.0, color.gamma_multiply(alpha)), StrokeKind::Outside);
}

/// `▲ 67×`: actual rows above (▲) or below (▼) the estimate, starting at `left_center`.
fn misestimate_badge(f: &Frame, left_center: Pos2, ratio: f64, scale: TextScale, alpha: f32) {
    let z = f.zoom();
    let color = f.p.warning.gamma_multiply(alpha);
    let factor = if ratio >= 1.0 { ratio } else { 1.0 / ratio };
    let job = line(&format!("{}×", fmt_factor(factor)), theme::bold_font(10.0), color, f32::INFINITY, scale);
    let text_w = f.painter.layout_job(job.clone()).size().x * scale.mesh;
    let rect = Rect::from_min_size(pos2(left_center.x, left_center.y - 7.5 * z), vec2(text_w + 16.0 * z, 15.0 * z));
    f.painter.rect_filled(rect, radius(4.0 * z), color.gamma_multiply(0.16));
    let (cx, cy) = (rect.left() + 6.5 * z, rect.center().y);
    let (a, b) = (2.75 * z, 2.25 * z);
    let tri = if ratio >= 1.0 {
        vec![pos2(cx - a, cy + b), pos2(cx + a, cy + b), pos2(cx, cy - b)]
    } else {
        vec![pos2(cx - a, cy - b), pos2(cx + a, cy - b), pos2(cx, cy + b)]
    };
    f.painter.add(Shape::convex_polygon(tri, color, Stroke::NONE));
    paint_text(f.painter, pos2(rect.left() + 11.5 * z, cy), Align2::LEFT_CENTER, job, scale);
}

/// The `+12` tab under a folded step; clicking it unfolds the step.
fn fold_pill(f: &Frame, card: Rect, hidden: usize) {
    let rect = fold_pill_rect(card, f.zoom());
    f.painter.rect(rect, radius(rect.height() / 2.0), f.s.card, Stroke::new(1.0, f.s.card_border), StrokeKind::Inside);
    if let Some(scale) = f.text {
        let job = line(&format!("+{}", fmt_count(hidden as u64)), theme::bold_font(10.5), f.p.text_muted, 60.0, scale);
        paint_text(f.painter, rect.center(), Align2::CENTER_CENTER, job, scale);
    }
}

fn fold_pill_rect(card: Rect, zoom: f32) -> Rect {
    Rect::from_center_size(pos2(card.center().x, card.bottom() + 10.0 * zoom), vec2(42.0, 18.0) * zoom)
}

/// Edges widen with the logarithm of the rows they carry: 1 row is a hairline, 100,000 rows the
/// widest.
fn edge_width(flow: Option<f64>) -> f32 {
    flow.map_or(1.25, |rows| (1.0 + 1.35 * (1.0 + rows).log10() as f32).min(7.0))
}

/// An edge from a step up to its parent, in graph points.
struct Edge {
    /// Cubic Bézier control points, from the child's top to the parent.
    points: [Pos2; 4],
    /// The height of the middle of the gap between the rows, where the row count goes.
    label_y: f32,
    width: f32,
}

impl Edge {
    fn new(
        steps: &[Step],
        child: NodeId,
        child_rect: Rect,
        parent: NodeId,
        parent_rect: Rect,
        inputs: &[NodeId],
    ) -> Self {
        let lift = if steps[child.index()].join { DIAMOND_Y - DIAMOND_R } else { 0.0 };
        let start = pos2(child_rect.center().x, child_rect.top() + lift);
        let width = edge_width(steps[child.index()].flow);
        let label_y = child_rect.top() - ROW_GAP / 2.0;
        let pc = parent_rect.center().x;
        let points = match (steps[parent.index()].join, inputs.iter().position(|c| *c == child)) {
            // A join's two inputs flow into the diamond's left and right corners.
            (true, Some(i)) if inputs.len() == 2 => {
                let dir = if i == 0 { -1.0 } else { 1.0 };
                let end = pos2(pc + dir * DIAMOND_R, parent_rect.top() + DIAMOND_Y);
                let rise = start.y - end.y;
                let run = (end.x - start.x).abs();
                [start, pos2(start.x, start.y - rise * 0.62), pos2(end.x + dir * run * 0.6, end.y), end]
            }
            _ => {
                let spread = if inputs.len() > 1 { ((start.x - pc) * 0.25).clamp(-60.0, 60.0) } else { 0.0 };
                let end = pos2(pc + spread, parent_rect.bottom());
                let dy = (start.y - end.y) * 0.5;
                [start, pos2(start.x, start.y - dy), pos2(end.x, end.y + dy), end]
            }
        };
        Self { points, label_y, width }
    }

    fn bounds(&self) -> Rect {
        Rect::from_points(&self.points).expand(self.width)
    }

    fn paint(&self, f: &Frame, lit: bool, flow: Option<f64>, alpha: f32) {
        let z = f.zoom();
        let screen = self.points.map(|p| f.camera.to_screen(f.canvas, p));
        let color = if lit { f.p.plan_guide_active } else { f.s.edge }.gamma_multiply(alpha);
        let width = (self.width * z).max(1.0);
        // The curve stops short of the parent, where the arrowhead takes over.
        let [a, b, c, d] = screen;
        let dir = (d - c).normalized();
        let head = (5.0 + self.width * 0.8) * z;
        let (c, d2) = (c - dir * head, d - dir * head);
        let curve = CubicBezierShape::from_points_stroke([a, b, c, d2], false, Color32::TRANSPARENT, Stroke::NONE);
        if flow == Some(0.0) {
            let points: Vec<Pos2> = (0..=24).map(|i| curve.sample(i as f32 / 24.0)).collect();
            f.painter.extend(Shape::dashed_line(&points, Stroke::new(width, color), 4.0 * z, 3.0 * z));
        } else {
            f.painter.add(CubicBezierShape { stroke: Stroke::new(width, color).into(), ..curve });
        }
        let normal = vec2(-dir.y, dir.x) * (head * 0.6).max(width * 0.85);
        f.painter.add(Shape::convex_polygon(vec![d, d2 + normal, d2 - normal], color, Stroke::NONE));

        let (Some(rows), Some(scale)) = (flow, f.text) else { return };
        let target = f.camera.to_screen(f.canvas, pos2(0.0, self.label_y)).y;
        let at = (0..=32)
            .map(|i| curve.sample(i as f32 / 32.0))
            .min_by(|p, q| (p.y - target).abs().total_cmp(&(q.y - target).abs()))
            .unwrap_or(a);
        let text = if lit { f.p.plan_guide_active } else { f.p.text_muted }.gamma_multiply(alpha);
        let job = line(&fmt_rows(rows), theme::bold_font(10.5), text, 120.0, scale);
        let size = f.painter.layout_job(job.clone()).size() * scale.mesh;
        let pill = Rect::from_center_size(at, size + vec2(10.0, 4.0) * z);
        f.painter.rect(
            pill,
            radius(pill.height() / 2.0),
            f.s.canvas.gamma_multiply(alpha),
            Stroke::new(1.0, color.gamma_multiply(0.5)),
            StrokeKind::Inside,
        );
        paint_text(f.painter, pill.center(), Align2::CENTER_CENTER, job, scale);
    }
}

/// The `Result` cap above the root: what the statement returned, and how long it took.
fn cap(f: &Frame, root: Rect, prepared: &Prepared, alpha: f32) {
    let z = f.zoom();
    let p = f.p;
    let rect = f.screen(cap_rect(root));
    let step = &prepared.steps[NodeId::ROOT.index()];
    let lift = if step.join { DIAMOND_Y - DIAMOND_R } else { 0.0 };
    let (from, to) = (pos2(root.center().x, root.top() + lift), pos2(root.center().x, cap_rect(root).bottom()));
    let third = (to.y - from.y) / 3.0;
    let edge = Edge {
        points: [from, from + vec2(0.0, third), to - vec2(0.0, third), to],
        label_y: f32::NAN,
        width: edge_width(step.flow),
    };
    edge.paint(f, f.lit[NodeId::ROOT.index()], None, alpha);

    let fade = |c: Color32| c.gamma_multiply(alpha);
    let Some(scale) = f.text else {
        let pill = Rect::from_center_size(rect.center(), vec2(90.0 * z, rect.height()));
        f.painter.rect_filled(pill, radius(pill.height() / 2.0), fade(f.s.card_border));
        return;
    };
    let mut job = LayoutJob::default();
    job.append("Result", 0.0, scale.format(theme::bold_font(12.0), fade(p.text)));
    if !prepared.result.is_empty() {
        job.append(&prepared.result, 8.0 * scale.layout, scale.format(theme::ui_font(12.0), fade(p.text_muted)));
    }
    let size = f.painter.layout_job(job.clone()).size() * scale.mesh;
    let pill = Rect::from_center_size(rect.center(), vec2(size.x + 28.0 * z, rect.height()));
    f.painter.rect(
        pill,
        radius(pill.height() / 2.0),
        fade(f.s.card),
        Stroke::new(1.0, fade(f.s.card_border)),
        StrokeKind::Inside,
    );
    paint_text(f.painter, pill.center(), Align2::CENTER_CENTER, job, scale);
}

fn tooltip(ui: &mut Ui, node: &PlanNode, step: &Step, analyzed: bool, dark: bool) {
    let p = palette(ui);
    ui.set_max_width(440.0);
    ui.label(RichText::new(&node.operation).font(theme::bold_font(13.0)).color(p.text));
    if let Some(object) = &node.object {
        let text = match &node.index {
            Some(index) => format!("{object}  ·  {index}"),
            None => object.clone(),
        };
        ui.label(RichText::new(text).size(12.5).color(p.text_muted));
    }
    if let Some(access) = step.access {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(8.0, 12.0), Sense::hover());
            ui.painter().rect_filled(
                Rect::from_center_size(r.center(), vec2(8.0, 8.0)),
                CornerRadius::same(2),
                access_color(access, dark),
            );
            ui.label(RichText::new(access.label()).size(12.0).color(p.text_muted));
        });
    }
    if let Some(condition) = &node.condition {
        ui.add(egui::Label::new(RichText::new(condition).font(theme::mono_font(11.5)).color(p.text)).wrap());
    }
    let mut facts = Vec::new();
    if analyzed {
        if let Some(a) = node.actual {
            if a.loops == 0 {
                facts.push("Never ran".to_string());
            } else if a.loops == 1 {
                facts.push(rows_text(a.rows));
            } else {
                facts.push(format!("{} per loop × {} loops", rows_text(a.rows), fmt_count(a.loops)));
            }
        }
        if let Some(time) = &step.value {
            facts.push(format!("{time} with the steps below"));
        }
    }
    if let Some(est) = node.estimate {
        let mut text = format!("Estimated {}", rows_text(est.rows));
        if let Some(cost) = est.cost {
            text.push_str(&format!(", cost {}", fmt_rows(cost.total)));
        }
        facts.push(text);
    }
    if !facts.is_empty() {
        ui.add_space(2.0);
        ui.label(RichText::new(facts.join("\n")).size(12.0).color(p.text));
    }
    if step.descendants > 0 {
        ui.label(RichText::new("Double-click to fold the steps below").size(11.0).color(p.text_faint));
    }
}

/// The colours in use, bottom left.
fn legend(ui: &Ui, canvas: Rect, prepared: &Prepared, p: &Palette, dark: bool) {
    if prepared.legend.is_empty() && !prepared.has_joins {
        return;
    }
    let s = surfaces(p, dark);
    let font = theme::ui_font(11.0);
    let items: Vec<(Option<Access>, &str)> = prepared
        .legend
        .iter()
        .map(|a| (Some(*a), legend_label(*a)))
        .chain(prepared.has_joins.then_some((None, "Join")))
        .collect();
    let painter = ui.painter_at(canvas);
    let widths: Vec<f32> = items
        .iter()
        .map(|(_, l)| painter.layout_no_wrap(l.to_string(), font.clone(), p.text_muted).size().x + 16.0)
        .collect();
    let total = widths.iter().sum::<f32>() + 10.0 * (items.len() - 1) as f32 + 20.0;
    let rect = Rect::from_min_size(pos2(canvas.left() + 12.0, canvas.bottom() - 12.0 - 26.0), vec2(total, 26.0));
    painter.rect(
        rect,
        CornerRadius::same(7),
        s.card.gamma_multiply(0.94),
        Stroke::new(1.0, s.card_border),
        StrokeKind::Inside,
    );
    let mut x = rect.left() + 10.0;
    let cy = rect.center().y;
    for ((access, label), w) in items.iter().zip(widths) {
        let swatch = Rect::from_center_size(pos2(x + 4.0, cy), vec2(8.0, 8.0));
        match access {
            Some(a) => {
                painter.rect_filled(swatch, CornerRadius::same(2), access_color(*a, dark));
            }
            None => {
                let c = swatch.center();
                let d = 4.5;
                let pts = vec![pos2(c.x, c.y - d), pos2(c.x + d, c.y), pos2(c.x, c.y + d), pos2(c.x - d, c.y)];
                painter.add(Shape::convex_polygon(pts, s.card, Stroke::new(1.25, p.text_muted)));
            }
        }
        painter.text(pos2(x + 13.0, cy), Align2::LEFT_CENTER, *label, font.clone(), p.text_muted);
        x += w + 10.0;
    }
}

/// Zoom out, the zoom level (click for 100%), zoom in and fit to view, bottom right.
fn controls(ui: &mut Ui, id: Id, canvas: Rect, view: &mut GraphView, now: f64) {
    let p = palette(ui);
    let s = surfaces(p, ui.visuals().dark_mode);
    let zoom = view.target().zoom;
    let label = format!("{:.0}%", zoom * 100.0);
    let (bw, lw, h) = (28.0, 50.0, 26.0);
    let rect =
        Rect::from_min_size(canvas.max - vec2(12.0 + 3.0 * bw + lw + 9.0, 12.0 + h), vec2(3.0 * bw + lw + 9.0, h));
    let painter = ui.painter_at(canvas);
    painter.rect(
        rect,
        CornerRadius::same(7),
        s.card.gamma_multiply(0.94),
        Stroke::new(1.0, s.card_border),
        StrokeKind::Inside,
    );
    let slot = |left: f32, w: f32| Rect::from_min_size(pos2(rect.left() + left, rect.top() + 2.0), vec2(w, h - 4.0));
    let (r_out, r_reset, r_in, r_fit) =
        (slot(2.0, bw), slot(2.0 + bw, lw), slot(2.0 + bw + lw, bw), slot(5.0 + 2.0 * bw + lw, bw));
    let button = |r: Rect, key: &str, tip: &str| {
        let resp = ui.interact(r, id.with(key), Sense::click()).on_hover_text(tip);
        if resp.hovered() {
            painter.rect_filled(r, CornerRadius::same(5), p.hover);
        }
        resp
    };
    let out = button(r_out, "zoom_out", "Zoom out (⌘-scroll or pinch)");
    let reset = button(r_reset, "zoom_reset", "Actual size");
    let zin = button(r_in, "zoom_in", "Zoom in (⌘-scroll or pinch)");
    let fit = button(r_fit, "fit", "Fit to view");
    icons::paint(&painter, Rect::from_center_size(r_out.center(), Vec2::splat(12.0)), Icon::Minus, p.text_muted);
    painter.text(r_reset.center(), Align2::CENTER_CENTER, &label, theme::ui_font(11.5), p.text_muted);
    icons::paint(&painter, Rect::from_center_size(r_in.center(), Vec2::splat(12.0)), Icon::Plus, p.text_muted);
    let sep = r_fit.left() - 1.5;
    painter
        .line_segment([pos2(sep, rect.top() + 6.0), pos2(sep, rect.bottom() - 6.0)], Stroke::new(1.0, s.card_border));
    fit_icon(&painter, r_fit.center(), p.text_muted);

    let to = [(out, zoom / 1.25), (zin, zoom * 1.25), (reset, 1.0)].into_iter().find(|(r, _)| r.clicked());
    if let Some((_, zoom)) = to {
        view.zoom_to(zoom, Vec2::ZERO, now);
    } else if fit.clicked() {
        view.fly(view.fit(canvas), now, 0.4);
    }
}

/// Four corner brackets: fit to view.
fn fit_icon(painter: &Painter, c: Pos2, color: Color32) {
    let (o, l) = (5.5, 3.5);
    let stroke = Stroke::new(1.4, color);
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        let corner = pos2(c.x + sx * o, c.y + sy * o);
        painter.add(Shape::line(
            vec![pos2(corner.x, corner.y - sy * l), corner, pos2(corner.x - sx * l, corner.y)],
            stroke,
        ));
    }
}

/// Arrows walk the tree: Up to the parent, Down to the first input, Left and Right along the
/// row. Enter or Space folds, Escape closes the details and then clears the selection.
fn keyboard(
    ui: &mut Ui,
    plan: &Plan,
    view: &mut GraphView,
    selected: &mut Option<NodeId>,
    detail_open: &mut bool,
    now: f64,
) {
    let keys = [Key::ArrowUp, Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight, Key::Enter, Key::Space, Key::Escape];
    let pressed: Vec<Key> = ui.input(|i| keys.into_iter().filter(|k| i.key_pressed(*k)).collect());
    let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
    for key in pressed {
        let Some(layout) = view.prepared.as_ref().map(|p| &p.layout) else { return };
        let neighbour = |sel: NodeId, step: isize| {
            let row = &layout.rows[layout.row_of(sel)?];
            let at = row.iter().position(|n| *n == sel)? as isize + step;
            row.get(usize::try_from(at).ok()?).copied()
        };
        let to = match (key, *selected) {
            (Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight, None) => Some(NodeId::ROOT),
            (Key::ArrowUp, Some(sel)) => plan.parent(sel),
            (Key::ArrowDown, Some(sel)) => plan.children(sel).first().filter(|c| layout.rect(**c).is_some()).copied(),
            (Key::ArrowLeft, Some(sel)) => neighbour(sel, -1),
            (Key::ArrowRight, Some(sel)) => neighbour(sel, 1),
            (Key::Enter | Key::Space, Some(sel)) => {
                view.toggle(plan, sel, now, selected);
                None
            }
            (Key::Escape, Some(_)) if *detail_open => {
                *detail_open = false;
                None
            }
            (Key::Escape, Some(_)) => {
                *selected = None;
                None
            }
            _ => None,
        };
        if let Some(node) = to {
            *selected = Some(node);
            *detail_open = true;
        }
    }
    if copy && let Some(sel) = *selected {
        ui.ctx().copy_text(crate::plan_view::node_text(plan.node(sel)));
    }
}
