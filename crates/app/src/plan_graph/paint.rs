//! Painting the graph: boxes, join diamonds, edges, the Result cap, the legend and tooltips,
//! at the camera's zoom.

use eframe::egui::emath::TSTransform;
use eframe::egui::epaint::{CubicBezierShape, Mesh, Shadow, TextShape};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Painter, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, pos2, text::LayoutJob, vec2,
};
use tableplus_core::plan::{Access, NodeId, PlanNode};

use super::layout::ROW_GAP;
use super::{Camera, DIAMOND_R, DIAMOND_Y, Prepared, Step, cap_rect, rows_text};
use crate::theme::{self, Palette, palette};
use crate::widgets::{fmt_count, fmt_factor, fmt_rows};

/// Unique and non-unique lookups share a colour, so the legend shows them as one.
pub(super) fn legend_class(access: Access) -> Access {
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

/// The canvas and box surfaces, which the palette has no names for.
pub(super) struct Surfaces {
    pub(super) canvas: Color32,
    pub(super) card: Color32,
    pub(super) card_border: Color32,
    pub(super) dots: Color32,
    pub(super) edge: Color32,
    pub(super) shadow: Option<Shadow>,
}

pub(super) fn surfaces(p: &Palette, dark: bool) -> Surfaces {
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
pub(super) struct TextScale {
    pub(super) layout: f32,
    pub(super) mesh: f32,
}

impl TextScale {
    pub(super) fn new(zoom: f32, moving: bool) -> Self {
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
pub(super) struct Frame<'a> {
    pub(super) painter: &'a Painter,
    pub(super) canvas: Rect,
    pub(super) camera: Camera,
    pub(super) p: &'static Palette,
    pub(super) s: Surfaces,
    pub(super) dark: bool,
    pub(super) analyzed: bool,
    /// `None` when zoomed out too far for text.
    pub(super) text: Option<TextScale>,
    pub(super) focused: bool,
    /// The selected step and everything below it, by node index: the rows it is made of.
    pub(super) lit: Vec<bool>,
}

impl Frame<'_> {
    fn zoom(&self) -> f32 {
        self.camera.zoom
    }

    pub(super) fn screen(&self, r: Rect) -> Rect {
        self.camera.rect(self.canvas, r)
    }

    fn accent(&self) -> Color32 {
        if self.focused { self.p.accent } else { self.p.accent.gamma_multiply(0.6) }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Look {
    pub(super) selected: bool,
    /// 0 to 1, eased in and out.
    pub(super) hovered: f32,
    pub(super) alpha: f32,
}

/// A dot grid that moves with the graph, so panning and zooming feel anchored.
pub(super) fn dots(painter: &Painter, canvas: Rect, camera: Camera, color: Color32) {
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

pub(super) fn paint_box(f: &Frame, r: Rect, node: &PlanNode, step: &Step, look: Look) {
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
    // A long name without an object or condition, such as `Aggregate using temporary table`,
    // takes the second line too.
    let has_detail = node.object.is_some() || node.index.is_some() || node.condition.is_some();
    let mut title = line(&node.operation, theme::bold_font(12.5), text, w, scale);
    title.wrap.max_rows = if has_detail { 1 } else { 2 };
    title.wrap.break_anywhere = has_detail;
    paint_text(painter, pos2(left, r.top() + 9.0 * z), Align2::LEFT_TOP, title, scale);

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

pub(super) fn paint_join(f: &Frame, r: Rect, node: &PlanNode, step: &Step, look: Look) {
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
    let mut numbers = LayoutJob::default();
    let muted = scale.format(theme::ui_font(11.0), fade(p.text_muted));
    if !step.rows.is_empty() {
        let rows = if step.misestimate.is_some() { fade(p.warning) } else { fade(p.text_muted) };
        numbers.append(&step.rows, 0.0, scale.format(theme::ui_font(11.0), rows));
    }
    if let Some(value) = &step.value {
        let sep = if numbers.is_empty() { "" } else { " · " };
        numbers.append(&format!("{sep}{value}"), 0.0, muted);
    }
    if !numbers.is_empty() {
        elide(&mut numbers, w, scale);
        paint_text(painter, pos2(c.x, r.top() + 61.0 * z), Align2::CENTER_TOP, numbers, scale);
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

/// The edges of two cards behind a folded box, standing for the steps it hides.
pub(super) fn fold_stack(f: &Frame, card: Rect) {
    let z = f.zoom();
    for depth in [2.0, 1.0] {
        let r = card.translate(vec2(0.0, 6.0 * z * depth)).shrink2(vec2(8.0 * z * depth, 0.0));
        f.painter.rect(r, radius(8.0 * z), f.s.card, Stroke::new(1.0, f.s.card_border), StrokeKind::Inside);
    }
}

/// The `+12` tab under a folded step; clicking it unfolds the step.
pub(super) fn fold_pill(f: &Frame, card: Rect, hidden: usize) {
    let rect = fold_pill_rect(card, f.zoom());
    f.painter.rect(rect, radius(rect.height() / 2.0), f.s.card, Stroke::new(1.0, f.s.card_border), StrokeKind::Inside);
    if let Some(scale) = f.text {
        let job = line(&format!("+{}", fmt_count(hidden as u64)), theme::bold_font(10.5), f.p.text_muted, 60.0, scale);
        paint_text(f.painter, rect.center(), Align2::CENTER_CENTER, job, scale);
    }
}

pub(super) fn fold_pill_rect(card: Rect, zoom: f32) -> Rect {
    Rect::from_center_size(pos2(card.center().x, card.bottom() + 28.0 * zoom), vec2(46.0, 20.0) * zoom)
}

/// Edges widen with the logarithm of the rows they carry: 1 row is a hairline, 100,000 rows the
/// widest.
fn edge_width(flow: Option<f64>) -> f32 {
    flow.map_or(1.25, |rows| (1.0 + 1.35 * (1.0 + rows).log10() as f32).min(7.0))
}

/// An edge from a step up to its parent, in graph points.
pub(super) struct Edge {
    /// Cubic Bézier control points, from the child's top to the parent.
    points: [Pos2; 4],
    /// The height of the middle of the gap between the rows, where the row count goes.
    label_y: f32,
    width: f32,
}

impl Edge {
    pub(super) fn new(
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

    pub(super) fn bounds(&self) -> Rect {
        Rect::from_points(&self.points).expand(self.width)
    }

    pub(super) fn paint(&self, f: &Frame, lit: bool, flow: Option<f64>, alpha: f32) {
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
pub(super) fn cap(f: &Frame, root: Rect, prepared: &Prepared, alpha: f32) {
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

pub(super) fn tooltip(ui: &mut Ui, node: &PlanNode, step: &Step, analyzed: bool, dark: bool) {
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
        // MySQL prints zero rows for steps it doesn't estimate.
        let rows = (est.rows > 0.0).then(|| rows_text(est.rows));
        let cost = est.cost.map(|c| format!("cost {}", fmt_rows(c.total)));
        let parts: Vec<String> = rows.into_iter().chain(cost).collect();
        if !parts.is_empty() {
            facts.push(format!("Estimated {}", parts.join(", ")));
        }
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
pub(super) fn legend(ui: &Ui, canvas: Rect, prepared: &Prepared, p: &Palette, dark: bool) {
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

/// Four corner brackets: fit to view.
pub(super) fn fit_icon(painter: &Painter, c: Pos2, color: Color32) {
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
