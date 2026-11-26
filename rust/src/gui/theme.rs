//! MetaBrain visual language for the native window: the same tokens as the web HMI
//! (field / panel / raised surfaces, amber for command, green for running), IBM Plex type.

use eframe::egui::{
    self, epaint::CornerRadius, Color32, FontData, FontDefinitions, FontFamily, FontId, Margin,
    Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2,
};
use std::sync::Arc;

// ---------- palette (dark HMI) ----------
pub const FIELD: Color32 = Color32::from_rgb(0x1C, 0x21, 0x26);
pub const PANEL: Color32 = Color32::from_rgb(0x23, 0x2A, 0x30);
pub const RAISED: Color32 = Color32::from_rgb(0x2B, 0x33, 0x3A);
pub const RAISED2: Color32 = Color32::from_rgb(0x33, 0x3C, 0x44);
pub const RULE: Color32 = Color32::from_rgb(0x3A, 0x43, 0x4B);
pub const BORDER_SUBTLE: Color32 = Color32::from_rgb(0x46, 0x51, 0x5B);
pub const INK: Color32 = Color32::from_rgb(0xE3, 0xE7, 0xEA);
pub const INK2: Color32 = Color32::from_rgb(0xB4, 0xBE, 0xC6);
pub const INK3: Color32 = Color32::from_rgb(0x9E, 0xA9, 0xB3);
pub const RUN: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x8A);
pub const ATTN: Color32 = Color32::from_rgb(0xE2, 0xA3, 0x3A);
pub const ALARM: Color32 = Color32::from_rgb(0xFA, 0x88, 0x7F);
pub const INFO: Color32 = Color32::from_rgb(0x86, 0xB6, 0xE2);
pub const MASTER: Color32 = Color32::from_rgb(0xF2, 0xB2, 0x4C);

pub const R: u8 = 6;

/// Identity colours for brains and chart series, in fixed order (validated for the dark
/// surface: CVD-separated, >= 3:1 contrast). A brain keeps its colour by config position.
pub const SERIES: [Color32; 8] = [
    Color32::from_rgb(0x39, 0x87, 0xE5),
    Color32::from_rgb(0xD9, 0x59, 0x26),
    Color32::from_rgb(0x19, 0x9E, 0x70),
    Color32::from_rgb(0xC9, 0x85, 0x00),
    Color32::from_rgb(0xD5, 0x51, 0x81),
    Color32::from_rgb(0x0C, 0xA3, 0x0C),
    Color32::from_rgb(0x90, 0x85, 0xE9),
    Color32::from_rgb(0xE6, 0x67, 0x67),
];
pub fn series(i: usize) -> Color32 {
    SERIES[i % SERIES.len()]
} // corner radius, as the web HMI's --rc

pub fn fam(name: &str) -> FontFamily {
    FontFamily::Name(Arc::from(name))
}
pub fn body(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn medium(size: f32) -> FontId {
    FontId::new(size, fam("medium"))
}
pub fn bold(size: f32) -> FontId {
    FontId::new(size, fam("bold"))
}
pub fn cond(size: f32) -> FontId {
    FontId::new(size, fam("cond"))
}
pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let add = |f: &mut FontDefinitions, key: &str, bytes: &'static [u8]| {
        f.font_data
            .insert(key.to_string(), Arc::new(FontData::from_static(bytes)));
    };
    add(
        &mut fonts,
        "plex",
        include_bytes!("../../assets/fonts/plex-sans-400.ttf"),
    );
    add(
        &mut fonts,
        "plex-500",
        include_bytes!("../../assets/fonts/plex-sans-500.ttf"),
    );
    add(
        &mut fonts,
        "plex-600",
        include_bytes!("../../assets/fonts/plex-sans-600.ttf"),
    );
    add(
        &mut fonts,
        "plex-cond",
        include_bytes!("../../assets/fonts/plex-cond-600.ttf"),
    );
    add(
        &mut fonts,
        "plex-mono",
        include_bytes!("../../assets/fonts/plex-mono-400.ttf"),
    );
    // Arrows, checks and box symbols that Plex lacks (DejaVu subset).
    add(
        &mut fonts,
        "symbols",
        include_bytes!("../../assets/fonts/dejavu-symbols.ttf"),
    );
    // egui's bundled fonts stay as fallbacks for symbols Plex lacks.
    let mut fallback: Vec<String> = vec!["symbols".to_string()];
    fallback.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or_default(),
    );
    let chain = |first: &str| {
        let mut v = vec![first.to_string()];
        v.extend(fallback.iter().cloned());
        v
    };
    fonts
        .families
        .insert(FontFamily::Proportional, chain("plex"));
    fonts.families.insert(fam("medium"), chain("plex-500"));
    fonts.families.insert(fam("bold"), chain("plex-600"));
    fonts.families.insert(fam("cond"), chain("plex-cond"));
    let mut mono_chain = vec!["plex-mono".to_string(), "symbols".to_string()];
    mono_chain.extend(
        fonts
            .families
            .get(&FontFamily::Monospace)
            .cloned()
            .unwrap_or_default(),
    );
    fonts.families.insert(FontFamily::Monospace, mono_chain);
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(10.0, 5.0);
        s.spacing.interact_size.y = 28.0;
        s.spacing.scroll.bar_width = 8.0;
        s.text_styles.insert(egui::TextStyle::Body, body(13.5));
        s.text_styles.insert(egui::TextStyle::Button, medium(13.0));
        s.text_styles.insert(egui::TextStyle::Small, body(11.5));
        s.text_styles.insert(egui::TextStyle::Heading, cond(20.0));
        s.text_styles.insert(egui::TextStyle::Monospace, mono(12.5));
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = FIELD;
        v.window_fill = PANEL;
        v.extreme_bg_color = Color32::from_rgb(0x17, 0x1B, 0x1F);
        v.faint_bg_color = PANEL;
        v.window_stroke = Stroke::new(1.0, RULE);
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(R);
        v.override_text_color = None;
        v.hyperlink_color = INFO;
        v.selection.bg_fill = Color32::from_rgba_unmultiplied(0xF2, 0xB2, 0x4C, 60);
        v.selection.stroke = Stroke::new(1.0, MASTER);
        v.interact_cursor = Some(egui::CursorIcon::PointingHand);
        let w = &mut v.widgets;
        for (st, fill, stroke, text) in [
            (&mut w.noninteractive, PANEL, RULE, INK2),
            (&mut w.inactive, RAISED, RULE, INK),
            (&mut w.hovered, RAISED2, INK3, INK),
            (&mut w.active, RAISED2, MASTER, INK),
            (&mut w.open, RAISED2, RULE, INK),
        ] {
            st.bg_fill = fill;
            st.weak_bg_fill = fill;
            st.bg_stroke = Stroke::new(1.0, stroke);
            st.fg_stroke = Stroke::new(1.0, text);
            st.corner_radius = CornerRadius::same(R);
            st.expansion = 0.0;
        }
        w.noninteractive.bg_stroke = Stroke::new(1.0, RULE);
    });
}

// ---------- building blocks ----------

/// A surface card: panel fill, hairline border, optional coloured left edge.
pub fn card<R>(
    ui: &mut Ui,
    edge: Option<Color32>,
    add: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    let frame = egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, RULE))
        .corner_radius(CornerRadius::same(R))
        .inner_margin(Margin::symmetric(14, 12));
    let w = ui.available_width();
    let r = frame.show(ui, |ui| {
        ui.set_max_width((w - 30.0).max(1.0));
        add(ui)
    });
    if let Some(c) = edge {
        let rect = r.response.rect;
        let bar = Rect::from_min_max(rect.min, Pos2::new(rect.min.x + 3.0, rect.max.y));
        ui.painter().rect_filled(
            bar,
            CornerRadius {
                nw: R,
                sw: R,
                ne: 0,
                se: 0,
            },
            c,
        );
    }
    r
}

/// Section title in the condensed face, with an optional quiet count.
pub fn section(ui: &mut Ui, title: &str, note: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(title).font(cond(15.0)).color(INK));
        if let Some(n) = note {
            ui.label(egui::RichText::new(n).font(body(12.0)).color(INK3));
        }
    });
    ui.add_space(4.0);
}

/// Small status pill: coloured dot + text.
pub fn chip(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), medium(11.5), color);
    let size = Vec2::new(galley.size().x + 22.0, 20.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(10),
        color.gamma_multiply(0.12),
        Stroke::new(1.0, color.gamma_multiply(0.35)),
        StrokeKind::Inside,
    );
    p.circle_filled(Pos2::new(rect.min.x + 9.0, rect.center().y), 3.0, color);
    p.galley(
        Pos2::new(rect.min.x + 16.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    resp
}

/// Horizontal meter (quota / budget). `frac` in 0..1; colour shifts at 80 % and 100 %.
pub fn meter(ui: &mut Ui, frac: Option<f64>, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), RULE);
    if let Some(f) = frac {
        let f = f.clamp(0.0, 1.0) as f32;
        let c = if f >= 1.0 {
            ALARM
        } else if f >= 0.8 {
            ATTN
        } else {
            INK2
        };
        let mut r = rect;
        r.set_width(rect.width() * f);
        p.rect_filled(r, CornerRadius::same(2), c);
    }
}

/// Tiny bar sparkline for hourly/daily series.
pub fn spark(ui: &mut Ui, values: &[f64], size: Vec2, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if values.is_empty() || max <= 0.0 {
        ui.painter().line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, RULE),
        );
        return;
    }
    let n = values.len() as f32;
    let w = rect.width() / n;
    for (i, v) in values.iter().enumerate() {
        let h = ((v / max) as f32 * rect.height()).max(if *v > 0.0 { 1.5 } else { 0.0 });
        let x = rect.min.x + i as f32 * w;
        let r = Rect::from_min_max(
            Pos2::new(x + 0.5, rect.max.y - h),
            Pos2::new(x + w - 0.5, rect.max.y),
        );
        ui.painter().rect_filled(
            r,
            CornerRadius::same(1),
            if i + 1 == values.len() {
                color
            } else {
                color.gamma_multiply(0.55)
            },
        );
    }
}

/// Quiet text helpers.
pub fn dim(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).color(INK2)
}
pub fn faint(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into())
        .color(INK3)
        .font(body(12.0))
}

/// Primary (amber), quiet, and danger buttons with proper hover/active visuals.
///
/// Avoids `egui::Button::fill` / `stroke` overrides, which disable egui's built-in
/// hover and pressed state transitions.
fn styled_button(
    ui: &mut Ui,
    text: egui::RichText,
    inactive: (Color32, Stroke, Color32),
    hovered: (Color32, Stroke, Color32),
    active: (Color32, Stroke, Color32),
) -> egui::Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for (st, (fill, stroke, fg)) in [
            (&mut w.inactive, inactive),
            (&mut w.hovered, hovered),
            (&mut w.active, active),
            (&mut w.open, hovered),
        ] {
            st.bg_fill = fill;
            st.weak_bg_fill = fill;
            st.bg_stroke = stroke;
            st.fg_stroke = Stroke::new(1.0, fg);
            st.corner_radius = CornerRadius::same(R);
        }
        w.noninteractive.bg_fill = inactive.0;
        w.noninteractive.weak_bg_fill = inactive.0;
        w.noninteractive.bg_stroke = inactive.1;
        w.noninteractive.fg_stroke = Stroke::new(1.0, INK3);
        w.noninteractive.corner_radius = CornerRadius::same(R);
        ui.add(egui::Button::new(text))
    })
    .inner
}

pub fn primary(ui: &mut Ui, text: &str) -> egui::Response {
    let fg = Color32::from_rgb(0x1C, 0x16, 0x0A);
    styled_button(
        ui,
        egui::RichText::new(text).font(medium(13.0)),
        (MASTER, Stroke::NONE, fg),
        (Color32::from_rgb(0xF7, 0xC2, 0x6A), Stroke::NONE, fg),
        (Color32::from_rgb(0xDC, 0x9B, 0x35), Stroke::NONE, fg),
    )
}
pub fn quiet(ui: &mut Ui, text: &str) -> egui::Response {
    styled_button(
        ui,
        egui::RichText::new(text).font(medium(12.5)),
        (Color32::TRANSPARENT, Stroke::new(1.0, RULE), INK2),
        (RAISED, Stroke::new(1.0, BORDER_SUBTLE), INK),
        (RAISED2, Stroke::new(1.0, MASTER), INK),
    )
}
pub fn danger(ui: &mut Ui, text: &str) -> egui::Response {
    styled_button(
        ui,
        egui::RichText::new(text).font(medium(12.5)),
        (
            Color32::TRANSPARENT,
            Stroke::new(1.0, ALARM.gamma_multiply(0.5)),
            ALARM,
        ),
        (ALARM.gamma_multiply(0.14), Stroke::new(1.0, ALARM), ALARM),
        (ALARM.gamma_multiply(0.24), Stroke::new(1.0, ALARM), INK),
    )
}

/// Compact number: 1.2 M, 34 k.
pub fn num(n: f64) -> String {
    let a = n.abs();
    if a >= 1e9 {
        format!("{:.1} G", n / 1e9)
    } else if a >= 1e6 {
        format!("{:.1} M", n / 1e6)
    } else if a >= 1e4 {
        format!("{:.0} k", n / 1e3)
    } else if a >= 1e3 {
        format!("{:.1} k", n / 1e3)
    } else {
        format!("{:.0}", n)
    }
}

/// "3m ago" style age from a unix timestamp.
pub fn ago(now: f64, ts: f64) -> String {
    let s = (now - ts).max(0.0);
    if s < 60.0 {
        "now".into()
    } else if s < 3600.0 {
        format!("{:.0}m ago", s / 60.0)
    } else if s < 86400.0 {
        format!("{:.0}h ago", s / 3600.0)
    } else {
        format!("{:.0}d ago", s / 86400.0)
    }
}

pub fn level_color(level: &str) -> Color32 {
    match level {
        "alarm" | "error" | "ALARM" => ALARM,
        "warning" | "warn" | "attn" | "WARNING" => ATTN,
        "ok" | "run" | "OK" => RUN,
        _ => INFO,
    }
}

// ---------- vector icons (no glyph fonts needed) ----------
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Home,
    Tasks,
    Brains,
    Usage,
    Alarms,
    Settings,
    Router,
    Memory,
    Services,
    Agents,
    Extensions,
    Shield,
    Link,
    Grip,
    Up,
    Down,
    Pause,
    Play,
    Plus,
    Close,
    Check,
    Left,
    Right,
}

pub fn paint_icon(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let s = Stroke::new(1.5, color);
    let c = rect.center();
    let u = rect.width().min(rect.height()) / 16.0; // 16-unit grid
    let pt = |x: f32, y: f32| Pos2::new(c.x + (x - 8.0) * u, c.y + (y - 8.0) * u);
    let line = |a: (f32, f32), b: (f32, f32)| p.line_segment([pt(a.0, a.1), pt(b.0, b.1)], s);
    let poly = |pts: &[(f32, f32)], closed: bool| {
        let v: Vec<Pos2> = pts.iter().map(|(x, y)| pt(*x, *y)).collect();
        if closed {
            p.add(egui::Shape::closed_line(v, s));
        } else {
            p.add(egui::Shape::line(v, s));
        }
    };
    let rrect = |x0: f32, y0: f32, x1: f32, y1: f32, r: u8| {
        p.rect_stroke(
            Rect::from_two_pos(pt(x0, y0), pt(x1, y1)),
            CornerRadius::same(r),
            s,
            StrokeKind::Middle,
        )
    };
    match icon {
        Icon::Home => {
            poly(&[(2.5, 7.5), (8.0, 2.5), (13.5, 7.5)], false);
            poly(&[(4.0, 6.5), (4.0, 13.5), (12.0, 13.5), (12.0, 6.5)], false);
        }
        Icon::Tasks => {
            for y in [4.0, 8.0, 12.0] {
                line((6.0, y), (13.5, y));
                p.circle_filled(pt(3.0, y), 1.1 * u, color);
            }
        }
        Icon::Brains => {
            p.circle_stroke(pt(8.0, 8.0), 2.2 * u, s);
            for (x, y) in [(3.0, 3.0), (13.0, 3.0), (3.0, 13.0), (13.0, 13.0)] {
                p.circle_stroke(pt(x, y), 1.4 * u, s);
                line(
                    (x + (8.0 - x) * 0.3, y + (8.0 - y) * 0.3),
                    (x + (8.0 - x) * 0.62, y + (8.0 - y) * 0.62),
                );
            }
        }
        Icon::Usage => {
            line((2.5, 13.5), (13.5, 13.5));
            for (x, h) in [(4.5, 5.0), (8.0, 9.0), (11.5, 6.5)] {
                line((x, 13.5), (x, 13.5 - h));
            }
        }
        Icon::Alarms => {
            poly(&[(8.0, 2.0), (14.0, 13.5), (2.0, 13.5)], true);
            line((8.0, 6.5), (8.0, 9.5));
            p.circle_filled(pt(8.0, 11.5), 0.9 * u, color);
        }
        Icon::Settings => {
            p.circle_stroke(pt(8.0, 8.0), 2.3 * u, s);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::TAU / 8.0;
                line(
                    (8.0 + a.cos() * 4.3, 8.0 + a.sin() * 4.3),
                    (8.0 + a.cos() * 6.2, 8.0 + a.sin() * 6.2),
                );
            }
        }
        Icon::Router => {
            p.circle_stroke(pt(3.5, 8.0), 1.5 * u, s);
            p.circle_stroke(pt(12.5, 3.5), 1.5 * u, s);
            p.circle_stroke(pt(12.5, 12.5), 1.5 * u, s);
            line((5.0, 7.3), (11.0, 4.2));
            line((5.0, 8.7), (11.0, 11.8));
        }
        Icon::Memory => {
            rrect(3.0, 2.5, 13.0, 13.5, 2);
            line((5.5, 6.0), (10.5, 6.0));
            line((5.5, 9.0), (10.5, 9.0));
        }
        Icon::Services => {
            rrect(2.5, 3.0, 13.5, 7.0, 2);
            rrect(2.5, 9.0, 13.5, 13.0, 2);
            p.circle_filled(pt(5.0, 5.0), 0.9 * u, color);
            p.circle_filled(pt(5.0, 11.0), 0.9 * u, color);
        }
        Icon::Agents => {
            p.circle_stroke(pt(8.0, 5.0), 2.5 * u, s);
            poly(
                &[
                    (3.0, 14.0),
                    (3.5, 11.0),
                    (8.0, 9.5),
                    (12.5, 11.0),
                    (13.0, 14.0),
                ],
                false,
            );
        }
        Icon::Extensions => {
            rrect(2.5, 2.5, 7.0, 7.0, 1);
            rrect(9.0, 2.5, 13.5, 7.0, 1);
            rrect(2.5, 9.0, 7.0, 13.5, 1);
            line((11.25, 9.5), (11.25, 13.0));
            line((9.5, 11.25), (13.0, 11.25));
        }
        Icon::Shield => {
            poly(
                &[
                    (8.0, 2.0),
                    (13.0, 4.0),
                    (12.5, 9.5),
                    (8.0, 14.0),
                    (3.5, 9.5),
                    (3.0, 4.0),
                ],
                true,
            );
            poly(&[(5.8, 8.0), (7.5, 9.7), (10.5, 6.3)], false);
        }
        Icon::Link => {
            rrect(2.0, 6.0, 9.0, 10.0, 6);
            rrect(7.0, 6.0, 14.0, 10.0, 6);
        }
        Icon::Grip => {
            for y in [4.0, 8.0, 12.0] {
                for x in [6.0, 10.0] {
                    p.circle_filled(pt(x, y), 1.0 * u, color);
                }
            }
        }
        Icon::Up => poly(&[(4.0, 10.0), (8.0, 6.0), (12.0, 10.0)], false),
        Icon::Down => poly(&[(4.0, 6.0), (8.0, 10.0), (12.0, 6.0)], false),
        Icon::Pause => {
            line((6.0, 4.0), (6.0, 12.0));
            line((10.0, 4.0), (10.0, 12.0));
        }
        Icon::Play => poly(&[(5.0, 3.5), (12.5, 8.0), (5.0, 12.5)], true),
        Icon::Plus => {
            line((8.0, 3.0), (8.0, 13.0));
            line((3.0, 8.0), (13.0, 8.0));
        }
        Icon::Close => {
            line((4.0, 4.0), (12.0, 12.0));
            line((12.0, 4.0), (4.0, 12.0));
        }
        Icon::Check => poly(&[(3.5, 8.5), (6.5, 11.5), (12.5, 4.5)], false),
        Icon::Left => poly(&[(10.0, 4.0), (6.0, 8.0), (10.0, 12.0)], false),
        Icon::Right => poly(&[(6.0, 4.0), (10.0, 8.0), (6.0, 12.0)], false),
    }
}

/// Square icon button with tooltip; `on` tints it amber.
pub fn icon_button(ui: &mut Ui, icon: Icon, tip: &str, on: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::splat(26.0),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = resp.hovered() && enabled;
    let p = ui.painter();
    if hovered || on {
        p.rect(
            rect,
            CornerRadius::same(R),
            if hovered {
                RAISED2
            } else {
                Color32::TRANSPARENT
            },
            Stroke::new(1.0, if on { MASTER.gamma_multiply(0.5) } else { RULE }),
            StrokeKind::Inside,
        );
    }
    let col = if !enabled {
        INK3.gamma_multiply(0.4)
    } else if on {
        MASTER
    } else if hovered {
        INK
    } else {
        INK3
    };
    paint_icon(p, rect.shrink(6.0), icon, col);
    if resp.has_focus() {
        p.rect_stroke(
            rect.expand(1.0),
            CornerRadius::same(R),
            Stroke::new(2.0, MASTER),
            StrokeKind::Outside,
        );
    }
    resp.on_hover_text(tip)
}

#[cfg(test)]
mod contrast_tests {
    use super::*;

    fn rel_lum(c: Color32) -> f64 {
        let ch = |v: u8| {
            let s = v as f64 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }

    fn contrast(fg: Color32, bg: Color32) -> f64 {
        let (l1, l2) = (rel_lum(fg), rel_lum(bg));
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }

    #[test]
    fn secondary_and_faint_text_meet_wcag_aa_on_core_surfaces() {
        for bg in [FIELD, PANEL, RAISED, RAISED2] {
            assert!(
                contrast(INK3, bg) >= 4.5,
                "INK3 contrast too low: {:.2}",
                contrast(INK3, bg)
            );
            assert!(
                contrast(INK2, bg) >= 5.5,
                "INK2 contrast too low: {:.2}",
                contrast(INK2, bg)
            );
            assert!(
                contrast(ALARM, bg) >= 4.5,
                "ALARM contrast too low: {:.2}",
                contrast(ALARM, bg)
            );
            assert!(
                contrast(INFO, bg) >= 4.5,
                "INFO contrast too low: {:.2}",
                contrast(INFO, bg)
            );
        }
    }
}
