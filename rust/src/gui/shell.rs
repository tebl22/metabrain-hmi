//! The frame around every page: the collapsible side bar (animated), the alarm bell with
//! its sliding drawer, and the Ctrl+K command palette.

use super::theme::*;
use super::{App, Page};
use crate::util::J;
use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Rect, Sense, Stroke, Vec2};
use serde_json::{json, Value};

pub const SYSTEM: [Page; 6] = [
    Page::Router,
    Page::Memory,
    Page::Services,
    Page::Agents,
    Page::Extensions,
    Page::Alarms,
];

pub fn is_system(p: Page) -> bool {
    SYSTEM.contains(&p)
}

/// Side bar. Expanded it shows labels; collapsed it is a rail of icons. The width animates.
pub fn sidebar(app: &mut App, ui: &mut egui::Ui, snap: &Value, pending: usize) {
    let ctx = ui.ctx().clone();
    let w = ctx.animate_value_with_time(
        egui::Id::unique("nav_w"),
        if app.ui.nav_open { 214.0 } else { 60.0 },
        0.22,
    );
    let t = ((w - 60.0) / (214.0 - 60.0)).clamp(0.0, 1.0); // 0 = rail, 1 = open

    egui::Panel::left("nav")
        .exact_size(w)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(PANEL)
                .inner_margin(Margin {
                    left: 10,
                    right: 10,
                    top: 12,
                    bottom: 12,
                })
                .stroke(Stroke::new(1.0, RULE)),
        )
        .show(ui, |ui| {
            // Brand + toggle.
            ui.horizontal(|ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                let p = ui.painter();
                p.rect_filled(
                    r,
                    CornerRadius::same(7),
                    if resp.hovered() {
                        RAISED2
                    } else {
                        Color32::from_rgb(0x12, 0x15, 0x18)
                    },
                );
                let s = Stroke::new(1.6, INK);
                let (l, rr, tp, b) = (r.min.x + 8.0, r.max.x - 8.0, r.min.y + 8.0, r.max.y - 8.0);
                for (a, c) in [
                    ((l, tp), (l, b)),
                    ((l, tp), (l + 3.5, tp)),
                    ((l, b), (l + 3.5, b)),
                    ((rr, tp), (rr, b)),
                    ((rr - 3.5, tp), (rr, tp)),
                    ((rr - 3.5, b), (rr, b)),
                ] {
                    p.line_segment([Pos2::new(a.0, a.1), Pos2::new(c.0, c.1)], s);
                }
                p.circle_filled(r.center(), 3.0, RUN);
                if resp
                    .on_hover_text(if app.ui.nav_open {
                        "Collapse menu"
                    } else {
                        "Expand menu"
                    })
                    .clicked()
                {
                    toggle(app);
                }
                if t > 0.5 {
                    let a = ((t - 0.5) * 2.0).clamp(0.0, 1.0);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("MetaBrain")
                                .font(cond(17.0))
                                .color(INK.gamma_multiply(a)),
                        );
                        ui.label(
                            egui::RichText::new(format!("v{}", snap.s("version")))
                                .font(body(11.0))
                                .color(INK3.gamma_multiply(a)),
                        );
                    });
                }
            });
            ui.add_space(16.0);

            let running = snap
                .arr("brains")
                .iter()
                .filter(|b| b.b("active", false))
                .count();
            let live_tasks = super::pages::task_list(app)
                .iter()
                .filter(|t| {
                    t.s("parent_run").is_empty()
                        && matches!(
                            t.s("status").as_str(),
                            "running" | "queued" | "verifying" | "planning" | "synthesizing"
                        )
                })
                .count();
            let items = [
                (Page::Home, "Home", Icon::Home, None),
                (
                    Page::Tasks,
                    "Chat",
                    Icon::Tasks,
                    if pending + live_tasks > 0 {
                        Some((pending + live_tasks, if pending > 0 { MASTER } else { RUN }))
                    } else {
                        None
                    },
                ),
                (
                    Page::Brains,
                    "Brains",
                    Icon::Brains,
                    if running > 0 {
                        Some((running, RUN))
                    } else {
                        None
                    },
                ),
                (Page::Usage, "Usage", Icon::Usage, None),
                (app.ui.sys_tab, "System", Icon::Services, None),
            ];
            for (page, label, icon, badge) in items {
                let selected = if label == "System" {
                    is_system(app.page)
                } else {
                    app.page == page
                };
                if item(ui, label, icon, badge, selected, t) {
                    app.page = page;
                }
            }

            if t > 0.7 {
                ui.add_space(12.0);
                ui.label(faint("MODE"));
                ui.add_space(3.0);
                let cur_mode = snap.s("mode");
                let mut picked_mode: Option<&str> = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (id, lbl) in [("eco", "Eco"), ("normal", "Normal"), ("max", "Max")] {
                        let on = cur_mode == id || (cur_mode.is_empty() && id == "normal");
                        let btn = egui::Button::new(
                            egui::RichText::new(lbl)
                                .font(if on { medium(11.5) } else { body(11.5) })
                                .color(if on { INK } else { INK2 }),
                        )
                        .fill(if on { RAISED2 } else { Color32::TRANSPARENT })
                        .stroke(Stroke::new(
                            1.0,
                            if on { MASTER.gamma_multiply(0.7) } else { RULE },
                        ))
                        .corner_radius(CornerRadius::same(6));
                        if ui.add(btn).clicked() {
                            picked_mode = Some(id);
                        }
                    }
                });
                if let Some(m) = picked_mode {
                    app.act(json!({"a": "mode", "mode": m, "confirm": m == "max"}));
                    app.say(format!("Mode: {}", m.to_uppercase()), RUN);
                }
            }

            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                if item(
                    ui,
                    "Settings",
                    Icon::Settings,
                    None,
                    app.page == Page::Settings,
                    t,
                ) {
                    app.page = Page::Settings;
                }
                ui.add_space(6.0);
                let alarms = snap.arr("alarms");
                let badge = if alarms.is_empty() {
                    None
                } else {
                    Some((
                        alarms.len(),
                        if alarms.iter().any(|a| a.s("level") == "alarm") {
                            ALARM
                        } else {
                            ATTN
                        },
                    ))
                };
                if item(ui, "Alarms", Icon::Alarms, badge, app.ui.drawer, t) {
                    app.ui.drawer = !app.ui.drawer;
                }
                if item(ui, "Search  Ctrl+K", Icon::Router, None, false, t) {
                    app.ui.palette = Some(String::new());
                }
                ui.add_space(6.0);
                let (txt, col) = match snap.s("overall").as_str() {
                    "ALARM" => ("Alarms active", ALARM),
                    "WARNING" => ("Attention", ATTN),
                    _ => ("All clear", RUN),
                };
                if t > 0.6 {
                    let m = snap.get("machine").cloned().unwrap_or(json!({}));
                    ui.label(faint(format!(
                        "CPU {:.0}%   RAM {:.0}%",
                        m.f("cpu").unwrap_or(0.0),
                        m.f("ram").unwrap_or(0.0)
                    )));
                    chip(ui, txt, col);
                } else {
                    let (r, _) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 14.0), Sense::hover());
                    ui.painter()
                        .circle_filled(Pos2::new(r.min.x + 20.0, r.center().y), 4.0, col);
                }
                ui.add_space(4.0);
            });
        });
}

fn toggle(app: &mut App) {
    app.ui.nav_open = !app.ui.nav_open;
    let mut g =
        crate::util::read_json(&crate::util::data_dir().join("gui.json")).unwrap_or(json!({}));
    g["nav_open"] = json!(app.ui.nav_open);
    let _ = crate::util::write_json(&crate::util::data_dir().join("gui.json"), &g);
}

/// One side bar entry; labels fade with the bar's width. Returns true when clicked.
fn item(
    ui: &mut egui::Ui,
    label: &str,
    icon: Icon,
    badge: Option<(usize, Color32)>,
    selected: bool,
    t: f32,
) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(R), RAISED);
        p.rect_filled(
            Rect::from_min_size(rect.min + Vec2::new(0.0, 8.0), Vec2::new(2.5, 18.0)),
            CornerRadius::same(1),
            MASTER,
        );
    } else if resp.hovered() {
        p.rect_filled(
            rect,
            CornerRadius::same(R),
            Color32::from_rgb(0x28, 0x30, 0x37),
        );
    }
    let col = if selected || resp.hovered() {
        INK
    } else {
        INK2
    };
    let ic = Rect::from_center_size(
        Pos2::new(rect.min.x + 20.0, rect.center().y),
        Vec2::splat(16.0),
    );
    paint_icon(p, ic, icon, col);
    let a = ((t - 0.35) / 0.65).clamp(0.0, 1.0);
    if a > 0.0 {
        let text = label.split("  ").next().unwrap_or(label);
        p.text(
            Pos2::new(rect.min.x + 40.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            if selected { medium(13.5) } else { body(13.5) },
            col.gamma_multiply(a),
        );
        if let Some(sc) = label.split("  ").nth(1) {
            p.text(
                Pos2::new(rect.max.x - 8.0, rect.center().y),
                egui::Align2::RIGHT_CENTER,
                sc,
                mono(10.5),
                INK3.gamma_multiply(a),
            );
        }
    }
    if let Some((n, c)) = badge {
        if a > 0.5 {
            let g = p.layout_no_wrap(n.to_string(), medium(11.0), c);
            let w = g.size().x + 12.0;
            let br = Rect::from_center_size(
                Pos2::new(rect.max.x - 8.0 - w / 2.0, rect.center().y),
                Vec2::new(w, 18.0),
            );
            p.rect_filled(br, CornerRadius::same(9), c.gamma_multiply(0.16));
            p.galley(
                Pos2::new(
                    br.center().x - g.size().x / 2.0,
                    br.center().y - g.size().y / 2.0,
                ),
                g,
                c,
            );
        } else {
            p.circle_filled(Pos2::new(ic.max.x + 2.0, ic.min.y + 1.0), 3.5, c);
        }
    }
    if resp.has_focus() {
        p.rect_stroke(
            rect,
            CornerRadius::same(R),
            Stroke::new(2.0, MASTER),
            egui::StrokeKind::Inside,
        );
    }
    let resp = if t < 0.5 {
        resp.on_hover_text(label.split("  ").next().unwrap_or(label))
    } else {
        resp
    };
    resp.clicked()
}

/// Tabs on top of the System pages.
pub fn system_tabs(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    ui.label(egui::RichText::new("System").font(cond(26.0)).color(INK));
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let alarms = snap.arr("alarms").len();
        for p in SYSTEM {
            let label = match p {
                Page::Router => "Router".to_string(),
                Page::Memory => "Memory".to_string(),
                Page::Services => "Services".to_string(),
                Page::Agents => "Agents".to_string(),
                Page::Extensions => "Extensions".to_string(),
                Page::Alarms => {
                    if alarms > 0 {
                        format!("Alarms {alarms}")
                    } else {
                        "Alarms".to_string()
                    }
                }
                _ => String::new(),
            };
            let on = app.page == p;
            let r = ui.add(
                egui::Button::new(
                    egui::RichText::new(label)
                        .font(if on { medium(13.0) } else { body(13.0) })
                        .color(if on { INK } else { INK2 }),
                )
                .fill(if on { RAISED } else { Color32::TRANSPARENT })
                .stroke(Stroke::new(
                    1.0,
                    if on { RULE } else { Color32::TRANSPARENT },
                ))
                .corner_radius(CornerRadius::same(R)),
            );
            if on {
                let rr = r.rect;
                ui.painter().line_segment(
                    [
                        Pos2::new(rr.min.x + 8.0, rr.max.y + 3.0),
                        Pos2::new(rr.max.x - 8.0, rr.max.y + 3.0),
                    ],
                    Stroke::new(2.0, MASTER),
                );
            }
            if r.clicked() {
                app.page = p;
                app.ui.sys_tab = p;
            }
        }
    });
    ui.add_space(14.0);
}

// ---------- alarm bell + drawer ----------

pub fn bell(app: &mut App, ctx: &egui::Context, snap: &Value) {
    let alarms = snap.arr("alarms");
    let n = alarms.len();
    let _ = n;
    // Drawer slides in from the right.
    let k = ctx.animate_bool_with_time(egui::Id::unique("drawer"), app.ui.drawer, 0.22);
    if k <= 0.001 {
        return;
    }
    let screen = ctx.content_rect();
    let w = 380.0_f32.min((screen.width() - 16.0).max(240.0));
    let x = screen.max.x - w * k;
    // Click outside closes it.
    egui::Area::new(egui::Id::unique("drawer_scrim"))
        .fixed_pos(screen.min)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            let (r, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            ui.painter().rect_filled(
                r,
                CornerRadius::ZERO,
                Color32::from_black_alpha((70.0 * k) as u8),
            );
            if resp.clicked() {
                app.ui.drawer = false;
            }
        });
    egui::Area::new(egui::Id::unique("drawer_panel"))
        .fixed_pos(Pos2::new(x, screen.min.y))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, RULE))
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    ui.set_width(w - 32.0);
                    ui.set_min_height(screen.height() - 32.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("Alarms & Notices")
                                .font(cond(18.0))
                                .color(INK),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if icon_button(ui, Icon::Close, "Close", false, true).clicked() {
                                app.ui.drawer = false;
                            }
                        });
                    });
                    ui.add_space(8.0);
                    egui::ScrollArea::vertical()
                        .max_height(screen.height() - 90.0)
                        .show(ui, |ui| {
                            if alarms.is_empty() {
                                ui.label(dim("Nothing needs attention."));
                            }
                            let mut acts: Vec<Value> = vec![];
                            for a in &alarms {
                                let c = level_color(&a.s("level"));
                                card(ui, Some(c), |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(a.s("text")).color(INK),
                                        )
                                        .wrap(),
                                    );
                                    ui.horizontal(|ui| {
                            if a.s("code") == "paused" && primary(ui, "Resume").clicked() {
                                acts.push(json!({"a": "resume", "brain": a.s("brain")}));
                            }
                            if quiet(ui, "Snooze 8h").clicked() {
                                acts.push(json!({"a": "ack_alarm", "id": a.s("id"), "hours": 8}));
                            }
                        });
                                });
                                ui.add_space(6.0);
                            }
                            for a in acts {
                                app.act(a);
                            }
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new("Recent notices")
                                    .font(cond(14.0))
                                    .color(INK2),
                            );
                            for d in snap
                                .arr("diary")
                                .iter()
                                .rev()
                                .filter(|d| d.s("level") != "info")
                                .take(20)
                            {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(format!(
                                            "{}  {}",
                                            d.s("who"),
                                            d.s("text")
                                        ))
                                        .font(body(12.0))
                                        .color(level_color(&d.s("level"))),
                                    )
                                    .wrap(),
                                );
                            }
                        });
                });
        });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.ui.drawer = false;
    }
}

// ---------- Ctrl+K command palette ----------

struct Cmd {
    label: String,
    hint: &'static str,
    run: Value,
}

pub fn palette(app: &mut App, ctx: &egui::Context, snap: &Value) {
    if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::K)) {
        app.ui.palette = if app.ui.palette.is_some() {
            None
        } else {
            Some(String::new())
        };
        app.ui.pal_sel = 0;
    }
    let Some(mut q) = app.ui.palette.clone() else {
        return;
    };

    let mut cmds: Vec<Cmd> = vec![
        Cmd {
            label: "Start a new chat / task".into(),
            hint: "Chat",
            run: json!({"go": "tasks"}),
        },
        Cmd {
            label: "Go to Home".into(),
            hint: "",
            run: json!({"go": "home"}),
        },
        Cmd {
            label: "Go to Brains".into(),
            hint: "",
            run: json!({"go": "brains"}),
        },
        Cmd {
            label: "Add a brain or sign in".into(),
            hint: "Brains",
            run: json!({"go": "add_brain"}),
        },
        Cmd {
            label: "Go to Usage & Savings".into(),
            hint: "",
            run: json!({"go": "usage"}),
        },
        Cmd {
            label: "View alarms".into(),
            hint: "",
            run: json!({"go": "drawer"}),
        },
        Cmd {
            label: "Router: test task assignment".into(),
            hint: "System",
            run: json!({"go": "router"}),
        },
        Cmd {
            label: "Go to Settings".into(),
            hint: "",
            run: json!({"go": "settings"}),
        },
        Cmd {
            label: "Mode: Eco (lowest cost)".into(),
            hint: "Mode",
            run: json!({"a": "mode", "mode": "eco"}),
        },
        Cmd {
            label: "Mode: Normal (balanced)".into(),
            hint: "Mode",
            run: json!({"a": "mode", "mode": "normal"}),
        },
        Cmd {
            label: "Mode: Max (highest capability)".into(),
            hint: "Mode",
            run: json!({"a": "mode", "mode": "max", "confirm": true}),
        },
        Cmd {
            label: "Pause all brains".into(),
            hint: "",
            run: json!({"a": "pause_all"}),
        },
        Cmd {
            label: "Resume all brains".into(),
            hint: "",
            run: json!({"a": "resume_all"}),
        },
        Cmd {
            label: if app.ui.nav_open {
                "Collapse sidebar".into()
            } else {
                "Expand sidebar".into()
            },
            hint: "",
            run: json!({"go": "toggle_nav"}),
        },
    ];
    for b in snap.arr("brains") {
        let (id, name) = (b.s("id"), b.s("name"));
        let paused = b
            .get("effective")
            .map(|e| e.b("paused", false))
            .unwrap_or(false);
        cmds.push(Cmd {
            label: format!("{} {name}", if paused { "Resume" } else { "Pause" }),
            hint: "Brain",
            run: json!({"a": if paused { "resume" } else { "pause" }, "brain": id}),
        });
        cmds.push(Cmd {
            label: format!("{name}: take solo command"),
            hint: "Hierarchy",
            run: json!({"a": "take_command", "brain": id}),
        });
        cmds.push(Cmd {
            label: format!("Inspect {name}"),
            hint: "Home",
            run: json!({"go": "brain", "id": id}),
        });
    }
    let ql = q.to_lowercase();
    let words: Vec<&str> = ql.split_whitespace().collect();
    let shown: Vec<&Cmd> = cmds
        .iter()
        .filter(|c| {
            words
                .iter()
                .all(|w| c.label.to_lowercase().contains(w) || c.hint.to_lowercase().contains(w))
        })
        .take(9)
        .collect();
    if app.ui.pal_sel >= shown.len() {
        app.ui.pal_sel = shown.len().saturating_sub(1);
    }
    let (up, down, enter, esc) = ctx.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::Enter),
            i.key_pressed(egui::Key::Escape),
        )
    });
    if down {
        app.ui.pal_sel = (app.ui.pal_sel + 1).min(shown.len().saturating_sub(1));
    }
    if up {
        app.ui.pal_sel = app.ui.pal_sel.saturating_sub(1);
    }
    let mut chosen: Option<Value> = if enter {
        shown.get(app.ui.pal_sel).map(|c| c.run.clone())
    } else {
        None
    };

    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::unique("pal_scrim"))
        .fixed_pos(screen.min)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            let (r, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            ui.painter()
                .rect_filled(r, CornerRadius::ZERO, Color32::from_black_alpha(110));
            if resp.clicked() {
                app.ui.palette = None;
            }
        });
    egui::Area::new(egui::Id::unique("palette"))
        .anchor(egui::Align2::CENTER_TOP, Vec2::new(0.0, 110.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, RULE))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(10))
                .shadow(egui::Shadow {
                    offset: [0, 10],
                    blur: 36,
                    spread: 0,
                    color: Color32::from_black_alpha(140),
                })
                .show(ui, |ui| {
                    ui.set_width(560.0_f32.min((screen.width() - 32.0).max(260.0)));
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut q)
                            .hint_text("Type a command…")
                            .font(body(15.0))
                            .desired_width(f32::INFINITY)
                            .frame(egui::Frame::NONE),
                    );
                    te.request_focus();
                    ui.separator();
                    if shown.is_empty() {
                        ui.label(dim("No matching commands."));
                    }
                    for (i, c) in shown.iter().enumerate() {
                        let on = i == app.ui.pal_sel;
                        let (r, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 32.0),
                            Sense::click(),
                        );
                        if on || resp.hovered() {
                            ui.painter().rect_filled(
                                r,
                                CornerRadius::same(R),
                                if on { RAISED2 } else { RAISED },
                            );
                        }
                        ui.painter().text(
                            Pos2::new(r.min.x + 10.0, r.center().y),
                            egui::Align2::LEFT_CENTER,
                            &c.label,
                            body(13.5),
                            INK,
                        );
                        if !c.hint.is_empty() {
                            ui.painter().text(
                                Pos2::new(r.max.x - 10.0, r.center().y),
                                egui::Align2::RIGHT_CENTER,
                                c.hint,
                                body(11.5),
                                INK3,
                            );
                        }
                        if resp.clicked() {
                            chosen = Some(c.run.clone());
                        }
                        if resp.hovered() {
                            app.ui.pal_sel = i;
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(faint("↑ ↓ to navigate · Enter to run · Esc to close"));
                });
        });

    if esc {
        app.ui.palette = None;
        return;
    }
    if let Some(r) = chosen {
        app.ui.palette = None;
        if let Some(go) = r.os("go") {
            match go.as_str() {
                "tasks" => app.page = Page::Tasks,
                "home" => app.page = Page::Home,
                "brains" => app.page = Page::Brains,
                "add_brain" => {
                    app.page = Page::Brains;
                    app.ui.wizard = Some(Default::default());
                }
                "usage" => app.page = Page::Usage,
                "drawer" => app.ui.drawer = true,
                "router" => {
                    app.page = Page::Router;
                    app.ui.sys_tab = Page::Router;
                }
                "settings" => app.page = Page::Settings,
                "toggle_nav" => toggle(app),
                "brain" => {
                    app.page = Page::Home;
                    app.ui.sel = r.s("id");
                }
                _ => {}
            }
        } else {
            app.act(r);
            app.say("Done", RUN);
        }
    } else {
        app.ui.palette = Some(q);
    }
}
