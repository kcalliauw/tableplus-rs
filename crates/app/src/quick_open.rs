//! ⌘K / ⌘P palette: fuzzy-jump to tables, views, routines, open tabs and connections.

use eframe::egui::{self, Align2, CornerRadius, Key, Modifiers, Order, RichText, Sense, Stroke, TextEdit, vec2};
use tableplus_core::config::ConnectionConfig;
use tableplus_core::fuzzy::fuzzy_match;
use tableplus_core::model::SchemaObject;
use uuid::Uuid;

use crate::icons::{self, Icon};
use crate::session::Session;
use crate::theme::{self, palette};
use crate::welcome::driver_badge;

pub enum QuickPick {
    Close,
    Object(SchemaObject),
    Connection(Uuid),
    Tab(usize),
}

#[derive(Default)]
pub struct QuickOpen {
    query: String,
    selected: usize,
}

enum Item<'a> {
    Object(&'a SchemaObject),
    Tab(usize, String),
    Connection(&'a ConnectionConfig),
}

impl QuickOpen {
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        session: Option<&Session>,
        connections: &[ConnectionConfig],
    ) -> Option<QuickPick> {
        let mut pick = None;
        let (up, down, enter, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if escape {
            return Some(QuickPick::Close);
        }

        let mut items: Vec<(i64, Item, Vec<usize>)> = Vec::new();
        if let Some(s) = session {
            for o in &s.catalog.objects {
                let label = if o.schema == s.catalog.default_schema || o.schema.is_empty() {
                    o.name.clone()
                } else {
                    o.qualified_name()
                };
                if let Some((score, hits)) = fuzzy_match(&self.query, &label) {
                    items.push((score + 30, Item::Object(o), hits));
                }
            }
            for (i, t) in s.tabs.iter().enumerate() {
                if let Some((score, hits)) = fuzzy_match(&self.query, &t.title()) {
                    items.push((score + 10, Item::Tab(i, t.title()), hits));
                }
            }
        }
        for c in connections {
            if let Some((score, hits)) = fuzzy_match(&self.query, &c.display_name()) {
                items.push((score, Item::Connection(c), hits));
            }
        }
        if self.query.is_empty() {
            // Stable, useful default ordering: open tabs, then objects, then connections.
            items.sort_by_key(|(_, item, _)| match item {
                Item::Tab(i, _) => (0, *i as i64),
                Item::Object(_) => (1, 0),
                Item::Connection(_) => (2, 0),
            });
        } else {
            items.sort_by_key(|x| std::cmp::Reverse(x.0));
        }
        items.truncate(200);
        if !items.is_empty() {
            if down {
                self.selected = (self.selected + 1).min(items.len() - 1);
            }
            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            self.selected = self.selected.min(items.len() - 1);
        }
        let to_pick = |item: &Item| match item {
            Item::Object(o) => QuickPick::Object((*o).clone()),
            Item::Tab(i, _) => QuickPick::Tab(*i),
            Item::Connection(c) => QuickPick::Connection(c.id),
        };
        if enter && let Some((_, item, _)) = items.get(self.selected) {
            return Some(to_pick(item));
        }

        let screen = ctx.content_rect();
        let dark = ctx.global_style().visuals.dark_mode;
        let p = theme::palette_for(dark);
        // Dim the background; clicking it closes the palette.
        let backdrop = egui::Area::new(egui::Id::new("quick_open_backdrop"))
            .order(Order::Middle)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (r, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
                ui.painter().rect_filled(
                    r,
                    CornerRadius::ZERO,
                    egui::Color32::from_black_alpha(if dark { 90 } else { 40 }),
                );
                resp.clicked()
            });
        if backdrop.inner {
            return Some(QuickPick::Close);
        }
        egui::Area::new(egui::Id::new("quick_open"))
            .order(Order::Foreground)
            .fixed_pos(egui::pos2(screen.center().x - 290.0, screen.top() + 70.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.content_bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .corner_radius(CornerRadius::same(12))
                    .shadow(ui.visuals().window_shadow)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(580.0);
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(22.0, 30.0), Sense::hover());
                            icons::paint(
                                ui.painter(),
                                egui::Rect::from_center_size(r.center(), vec2(15.0, 15.0)),
                                Icon::Search,
                                p.text_faint,
                            );
                            let edit = ui.add(
                                TextEdit::singleline(&mut self.query)
                                    .id(egui::Id::new("quick_open_input"))
                                    .hint_text(
                                        RichText::new("Search tables, views, functions, tabs and connections…")
                                            .color(p.text_faint),
                                    )
                                    .font(theme::ui_font(16.0))
                                    .frame(egui::Frame::NONE)
                                    .desired_width(f32::INFINITY),
                            );
                            edit.request_focus();
                            if edit.changed() {
                                self.selected = 0;
                            }
                        });
                        ui.separator();
                        if items.is_empty() {
                            ui.add_space(10.0);
                            ui.label(RichText::new("No matches").color(p.text_faint));
                            ui.add_space(10.0);
                            return;
                        }
                        let row_h = 30.0;
                        egui::ScrollArea::vertical().max_height(row_h * 12.0).show_rows(
                            ui,
                            row_h,
                            items.len(),
                            |ui, range| {
                                for i in range {
                                    let (_, item, hits) = &items[i];
                                    let (rect, resp) =
                                        ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                                    let selected = i == self.selected;
                                    if selected {
                                        ui.painter().rect_filled(rect, CornerRadius::same(6), p.selection);
                                        if down || up {
                                            resp.scroll_to_me(None);
                                        }
                                    } else if resp.hovered() {
                                        ui.painter().rect_filled(rect, CornerRadius::same(6), p.hover);
                                    }
                                    let text_color = if selected { p.accent_text } else { p.text };
                                    let muted = if selected { p.accent_text.gamma_multiply(0.8) } else { p.text_faint };
                                    let icon_rect = egui::Rect::from_center_size(
                                        rect.left_center() + vec2(18.0, 0.0),
                                        vec2(16.0, 16.0),
                                    );
                                    let (label, detail) = match item {
                                        Item::Object(o) => {
                                            icons::paint(
                                                ui.painter(),
                                                icon_rect,
                                                icons::kind_icon(o.kind),
                                                if selected { p.accent_text } else { icons::kind_color(o.kind, dark) },
                                            );
                                            let s = session.expect("session");
                                            let label = if o.schema == s.catalog.default_schema || o.schema.is_empty() {
                                                o.name.clone()
                                            } else {
                                                o.qualified_name()
                                            };
                                            (label, o.kind.label().to_string())
                                        }
                                        Item::Tab(_, title) => {
                                            icons::paint(ui.painter(), icon_rect, Icon::Sql, muted);
                                            (title.clone(), "Open tab".to_string())
                                        }
                                        Item::Connection(c) => {
                                            driver_badge(ui, icon_rect.expand(1.0), c.driver);
                                            (c.display_name(), format!("Connection · {}", c.driver.label()))
                                        }
                                    };
                                    let mut job = egui::text::LayoutJob::default();
                                    for (ci, ch) in label.chars().enumerate() {
                                        let hit = hits.contains(&ci);
                                        job.append(
                                            &ch.to_string(),
                                            0.0,
                                            egui::TextFormat {
                                                font_id: if hit {
                                                    theme::bold_font(14.0)
                                                } else {
                                                    theme::ui_font(14.0)
                                                },
                                                color: if hit && !selected { p.accent } else { text_color },
                                                ..Default::default()
                                            },
                                        );
                                    }
                                    let galley = ui.painter().layout_job(job);
                                    ui.painter().galley(
                                        egui::pos2(rect.left() + 36.0, rect.center().y - galley.size().y / 2.0),
                                        galley,
                                        text_color,
                                    );
                                    ui.painter().text(
                                        rect.right_center() - vec2(10.0, 0.0),
                                        Align2::RIGHT_CENTER,
                                        detail,
                                        theme::ui_font(11.5),
                                        muted,
                                    );
                                    if resp.clicked() {
                                        pick = Some(to_pick(item));
                                    }
                                    if resp.hovered() && ui.input(|i| i.pointer.delta().length() > 0.0) {
                                        self.selected = i;
                                    }
                                }
                            },
                        );
                    });
            });
        let _ = palette;
        pick
    }
}
