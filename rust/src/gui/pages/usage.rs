//! Usage and savings charts, gauges, and token metrics.

use super::{columns2, hhmm};
use crate::gui::theme::*;
use crate::gui::App;
use crate::util::J;
use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Rect, Sense, Stroke, Vec2};
use serde_json::Value;

// =====================================================================================
// Uso y ahorro
// =====================================================================================

pub(super) fn usage(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    let bs = snap.arr("brains");
    let sum = |k: &str| {
        bs.iter()
            .map(|b| b.get("usage").and_then(|u| u.f(k)).unwrap_or(0.0))
            .sum::<f64>()
    };
    let sv = app
        .data
        .read()
        .map(|d| d.savings.clone())
        .unwrap_or(Value::Null);

    // 1. Headline numbers.
    let usd: f64 = sum("usd_today");
    let relay_saved = sv.f("relay_saved_tokens").unwrap_or(0.0);
    let routing_saved = sv.f("routing_saved_tokens").unwrap_or(0.0);
    let tiles: [(String, &str, String, Color32); 4] = [
        (
            num(sum("today")),
            "tokens hoy",
            format!("{} en 5 h", num(sum("h5"))),
            INK,
        ),
        (
            num(sum("d7")),
            "tokens en 7 días",
            format!("{} cerebros", bs.len()),
            INK,
        ),
        (
            format!("{usd:.2} USD"),
            "gastado hoy en APIs",
            "las cuentas de Claude no cobran por token".into(),
            INK,
        ),
        (
            num(relay_saved),
            "ahorro estimado por relevos",
            format!(
                "{} equivalentes a Opus por enrutamiento · 14 días",
                num(routing_saved)
            ),
            INK,
        ),
    ];
    let gap = 12.0;
    let cols = if ui.available_width() >= 780.0 { 4 } else { 2 };
    let tw = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).max(120.0);
    for row_tiles in tiles.chunks(cols) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (v, l, s, c) in row_tiles {
                ui.allocate_ui_with_layout(
                    Vec2::new(tw, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(tw);
                        card(ui, None, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(egui::RichText::new(v).font(cond(28.0)).color(*c));
                            ui.label(egui::RichText::new(*l).font(medium(12.5)).color(INK2));
                            ui.add(egui::Label::new(faint(s)).wrap());
                        });
                    },
                );
            }
        });
        if cols < 4 {
            ui.add_space(gap);
        }
    }
    ui.add_space(16.0);

    // 2. Quota gauges per brain (small multiples of the same ring the map uses).
    section(
        ui,
        "Cuánto le queda a cada cerebro",
        Some("anillo = cuota usada; la hora es cuándo se restablece"),
    );
    let gw = 236.0;
    let per_row = ((ui.available_width() + gap) / (gw + gap)).floor().max(1.0) as usize;
    for row in bs.chunks(per_row) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for b in row {
                let i = bs.iter().position(|x| x.s("id") == b.s("id")).unwrap_or(0);
                gauge_card(ui, b, series(i), gw);
            }
        });
        ui.add_space(gap);
    }
    ui.add_space(6.0);

    // 3. Daily tokens, stacked by brain.
    section(
        ui,
        "Tokens por día",
        Some("últimos 30 días · cada color es un cerebro"),
    );
    card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        legend(ui, &bs);
        ui.add_space(6.0);
        let series_data: Vec<(String, Color32, Vec<f64>)> = bs
            .iter()
            .enumerate()
            .map(|(i, b)| {
                (
                    b.s("name"),
                    series(i),
                    b.get("usage")
                        .map(|u| {
                            u.arr("daily")
                                .iter()
                                .map(|v| v.as_f64().unwrap_or(0.0))
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            })
            .collect();
        stacked(ui, &series_data, 190.0);
    });
    ui.add_space(16.0);

    // 4. Today by hour + efficiency, side by side on wide windows, stacked when narrow.
    columns2(
        ui,
        |ui| {
            section(ui, "Hoy por hora", Some("últimas 24 h"));
            card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                let s: Vec<(String, Color32, Vec<f64>)> = bs
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        (
                            b.s("name"),
                            series(i),
                            b.get("usage")
                                .map(|u| {
                                    u.arr("hourly")
                                        .iter()
                                        .map(|v| v.as_f64().unwrap_or(0.0))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                stacked(ui, &s, 140.0);
            });
        },
        |ui| {
            section(
                ui,
                "Eficiencia",
                Some("tokens por turno · más bajo es mejor"),
            );
            card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                if sv.is_null() {
                    ui.label(dim("Calculando…"));
                    return;
                }
                let pts: Vec<(String, f64)> = sv
                    .arr("daily")
                    .iter()
                    .map(|d| (d.s("day"), d.f("per_turn").unwrap_or(0.0)))
                    .collect();
                line_chart(ui, &pts, 140.0, INFO);
                ui.horizontal_wrapped(|ui| {
                    ui.label(faint(format!(
                        "{} sesiones",
                        sv.f("sessions").unwrap_or(0.0)
                    )));
                    ui.label(faint(format!(
                        "· {} relevos de sesión",
                        sv.arr("relays").len()
                    )));
                    let routed_n = sv
                        .get("routing")
                        .and_then(|r| r.f("routed_tasks"))
                        .unwrap_or(0.0);
                    if routed_n > 0.0 {
                        ui.label(faint(format!("· {:.0} tareas enrutadas", routed_n)));
                    }
                });
            });
        },
    );
    ui.add_space(16.0);

    // 5. What spent the most.
    ui.horizontal(|ui| {
        section(ui, "Qué gastó más", None);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for d in [30, 7, 1] {
                if ui
                    .selectable_label(
                        app.ui.days == d,
                        match d {
                            1 => "Hoy",
                            7 => "7 días",
                            _ => "30 días",
                        },
                    )
                    .clicked()
                {
                    app.ui.days = d;
                }
            }
        });
    });
    let u = app.get(&format!("api/usage?days={}", app.ui.days), 60.0);
    card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        let top = u.arr("top_questions");
        if u.is_null() {
            ui.label(dim("Leyendo las sesiones…"));
            return;
        }
        if top.is_empty() {
            ui.label(dim("Sin preguntas registradas en ese periodo."));
            return;
        }
        let max = top
            .iter()
            .filter_map(|q| q.f("tokens"))
            .fold(1.0_f64, f64::max);
        for q in top.iter().take(12) {
            let tok = q.f("tokens").unwrap_or(0.0);
            ui.horizontal(|ui| {
                let w = ui.available_width();
                let text: String = q.s("text").chars().take(80).collect();
                ui.allocate_ui_with_layout(
                    Vec2::new(w * 0.55, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(w * 0.55);
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).font(body(12.5)).color(INK))
                                .truncate(),
                        )
                        .on_hover_text(q.s("text"));
                        ui.label(faint(format!(
                            "{} · {} · {}",
                            q.s("account"),
                            q.s("topic"),
                            q.s("when")
                        )));
                    },
                );
                // Bar proportional to tokens, value as text beside it.
                let bw = (w * 0.45 - 70.0).max(40.0);
                let (r, _) = ui.allocate_exact_size(Vec2::new(bw, 10.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(3), RAISED2);
                let mut rr = r;
                rr.set_width((r.width() * (tok / max) as f32).max(2.0));
                ui.painter()
                    .rect_filled(rr, CornerRadius::same(3), MASTER.gamma_multiply(0.8));
                ui.label(egui::RichText::new(num(tok)).font(mono(12.0)).color(INK2));
            });
            ui.add_space(4.0);
        }
        ui.label(faint(format!(
            "Total del periodo: {} tokens",
            num(u.f("total").unwrap_or(0.0))
        )));
    });
}

fn legend(ui: &mut egui::Ui, bs: &[Value]) {
    ui.horizontal_wrapped(|ui| {
        for (i, b) in bs.iter().enumerate() {
            let (r, _) = ui.allocate_exact_size(Vec2::new(12.0, 12.0), Sense::hover());
            ui.painter()
                .rect_filled(r.shrink(1.0), CornerRadius::same(3), series(i));
            ui.label(
                egui::RichText::new(b.s("name"))
                    .font(body(12.0))
                    .color(INK2),
            );
            ui.add_space(10.0);
        }
    });
}

/// One brain's quota as a ring with the percent inside, like on the map.
fn gauge_card(ui: &mut egui::Ui, b: &Value, color: Color32, w: f32) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, RULE))
        .corner_radius(CornerRadius::same(R))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(w - 24.0);
            let quotas: Vec<Value> = b
                .arr("quota")
                .into_iter()
                .filter(|q| q.f("pct").is_some())
                .collect();
            let (pct, label, reset) = crate::gui::home::gauge(b);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                let c = r.center();
                let p = ui.painter();
                let ring = |d0: f32, d1: f32, col: Color32| {
                    let n = 40;
                    let pts: Vec<Pos2> = (0..=n)
                        .map(|k| {
                            let a = (d0 + (d1 - d0) * k as f32 / n as f32 - 90.0).to_radians();
                            Pos2::new(c.x + 26.0 * a.cos(), c.y + 26.0 * a.sin())
                        })
                        .collect();
                    p.add(egui::Shape::line(pts, Stroke::new(6.0, col)));
                };
                ring(0.0, 360.0, RAISED2);
                if let Some(v) = pct {
                    let col = if v >= 100.0 {
                        ALARM
                    } else if v >= 80.0 {
                        ATTN
                    } else {
                        color
                    };
                    ring(0.0, 360.0 * (v as f32 / 100.0).clamp(0.0, 1.0), col);
                }
                p.text(
                    c,
                    egui::Align2::CENTER_CENTER,
                    pct.map(|v| format!("{v:.0}%"))
                        .unwrap_or_else(|| "–".into()),
                    mono(13.0),
                    INK,
                );
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let (dr, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                        ui.painter().circle_filled(dr.center(), 3.5, color);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(b.s("name")).font(bold(13.0)).color(INK),
                            )
                            .truncate(),
                        );
                    });
                    if let Some(v) = pct {
                        let rem = (100.0 - v).clamp(0.0, 100.0);
                        ui.label(faint(format!("{label} · queda {rem:.0} %")));
                    } else {
                        // No plan limits published for this brain: show what it is doing now.
                        let day0 = crate::util::now() - crate::util::now() % 86400.0;
                        let today: f64 = b
                            .arr("calls")
                            .iter()
                            .filter(|c| c.f("ts").unwrap_or(0.0) >= day0)
                            .filter_map(|c| c.f("tokens"))
                            .sum();
                        let live = if b.b("active", false) {
                            "trabajando ahora"
                        } else {
                            "en espera"
                        };
                        ui.label(faint(format!("{} tokens hoy · {live}", num(today))));
                    }
                    if let Some(bal) = b.get("balance").filter(|x| x.f("amount").is_some()) {
                        ui.label(
                            egui::RichText::new(format!(
                                "saldo {:.2} {}",
                                bal.f("amount").unwrap_or(0.0),
                                bal.s("currency")
                            ))
                            .font(body(11.5))
                            .color(INK2),
                        );
                    }
                    if !b.s("account").is_empty() {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(b.s("account"))
                                    .font(body(11.0))
                                    .color(INK3),
                            )
                            .truncate(),
                        );
                    }
                    if let Some(rs) = reset {
                        ui.label(
                            egui::RichText::new(format!("vuelve {}", hhmm(rs)))
                                .font(body(11.5))
                                .color(INK2),
                        );
                    }
                    let secondary_group = if label == "semana" {
                        "session"
                    } else {
                        "weekly"
                    };
                    let secondary_label = if secondary_group == "session" {
                        "sesión 5 h"
                    } else {
                        "semana"
                    };
                    if let Some(sec) = quotas.iter().find(|q| q.s("group") == secondary_group) {
                        let v = sec.f("pct").unwrap_or(0.0);
                        let srem = (100.0 - v).clamp(0.0, 100.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "{secondary_label} {v:.0} % · queda {srem:.0} %"
                            ))
                            .font(body(11.5))
                            .color(if v >= 100.0 {
                                ALARM
                            } else if v >= 80.0 {
                                ATTN
                            } else {
                                INK3
                            }),
                        );
                    }
                });
            });
        });
}

/// Stacked bars: one column per period, a segment per series, 2 px gaps, hover breakdown.
fn stacked(ui: &mut egui::Ui, data: &[(String, Color32, Vec<f64>)], height: f32) {
    let n = data.iter().map(|d| d.2.len()).max().unwrap_or(0);
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height + 20.0), Sense::hover());
    let p = ui.painter();
    let totals: Vec<f64> = (0..n)
        .map(|i| {
            data.iter()
                .map(|d| d.2.get(i).copied().unwrap_or(0.0))
                .sum()
        })
        .collect();
    let max = totals.iter().cloned().fold(0.0_f64, f64::max).max(1.0);
    let plot = Rect::from_min_max(
        Pos2::new(rect.min.x + 46.0, rect.min.y + 6.0),
        Pos2::new(rect.max.x - 4.0, rect.min.y + height),
    );
    for f in [0.0, 0.5, 1.0] {
        let y = plot.max.y - f as f32 * plot.height();
        p.line_segment(
            [Pos2::new(plot.min.x, y), Pos2::new(plot.max.x, y)],
            Stroke::new(
                1.0,
                if f == 0.0 {
                    RULE
                } else {
                    RULE.gamma_multiply(0.5)
                },
            ),
        );
        p.text(
            Pos2::new(plot.min.x - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            num(max * f),
            body(10.5),
            INK3,
        );
    }
    if n == 0 {
        p.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "Sin datos todavía",
            body(12.0),
            INK3,
        );
        return;
    }
    let bw = plot.width() / n as f32;
    let hover_i = resp
        .hover_pos()
        .filter(|h| plot.expand2(Vec2::new(0.0, 20.0)).contains(*h))
        .map(|h| (((h.x - plot.min.x) / bw).floor() as isize).clamp(0, n as isize - 1) as usize);
    for i in 0..n {
        let x0 = plot.min.x + i as f32 * bw + (bw * 0.18).min(6.0);
        let x1 = plot.min.x + (i + 1) as f32 * bw - (bw * 0.18).min(6.0);
        let mut y = plot.max.y;
        let hot = hover_i == Some(i);
        if hot {
            p.rect_filled(
                Rect::from_min_max(
                    Pos2::new(plot.min.x + i as f32 * bw, plot.min.y),
                    Pos2::new(plot.min.x + (i + 1) as f32 * bw, plot.max.y),
                ),
                CornerRadius::ZERO,
                Color32::from_white_alpha(6),
            );
        }
        let segs: Vec<(f32, Color32)> = data
            .iter()
            .map(|d| {
                (
                    (d.2.get(i).copied().unwrap_or(0.0) / max) as f32 * plot.height(),
                    d.1,
                )
            })
            .filter(|s| s.0 > 0.0)
            .collect();
        let last = segs.len().saturating_sub(1);
        for (k, (h, c)) in segs.iter().enumerate() {
            let top = y - h;
            let r = Rect::from_min_max(
                Pos2::new(x0, top + if k == last { 0.0 } else { 1.0 }),
                Pos2::new(x1, y - if k == 0 { 0.0 } else { 1.0 }),
            );
            let cr = if k == last {
                CornerRadius {
                    nw: 3,
                    ne: 3,
                    sw: 0,
                    se: 0,
                }
            } else {
                CornerRadius::ZERO
            };
            if r.height() > 0.3 {
                p.rect_filled(
                    r,
                    cr,
                    if hover_i.is_some() && !hot {
                        c.gamma_multiply(0.55)
                    } else {
                        *c
                    },
                );
            }
            y = top;
        }
    }
    // X labels: first, middle, last.
    let lab = |i: usize| -> String {
        let back = n - 1 - i;
        if back == 0 {
            "ahora".into()
        } else if n == 24 {
            format!("-{back} h")
        } else {
            format!("-{back} d")
        }
    };
    for i in [0, n / 2, n - 1] {
        p.text(
            Pos2::new(plot.min.x + (i as f32 + 0.5) * bw, plot.max.y + 11.0),
            egui::Align2::CENTER_CENTER,
            lab(i),
            body(10.5),
            INK3,
        );
    }
    if let Some(i) = hover_i {
        let mut lines = vec![format!("{}: {} tokens", lab(i), num(totals[i]))];
        for d in data {
            let v = d.2.get(i).copied().unwrap_or(0.0);
            if v > 0.0 {
                lines.push(format!("{}  {}", d.0, num(v)));
            }
        }
        resp.on_hover_text(lines.join("\n"));
    }
}

/// Single-series line with a crosshair on hover.
fn line_chart(ui: &mut egui::Ui, pts: &[(String, f64)], height: f32, color: Color32) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height + 18.0), Sense::hover());
    let p = ui.painter();
    let plot = Rect::from_min_max(
        Pos2::new(rect.min.x + 46.0, rect.min.y + 6.0),
        Pos2::new(rect.max.x - 6.0, rect.min.y + height),
    );
    let max = pts.iter().map(|x| x.1).fold(0.0_f64, f64::max).max(1.0);
    for f in [0.0, 0.5, 1.0] {
        let y = plot.max.y - f as f32 * plot.height();
        p.line_segment(
            [Pos2::new(plot.min.x, y), Pos2::new(plot.max.x, y)],
            Stroke::new(
                1.0,
                if f == 0.0 {
                    RULE
                } else {
                    RULE.gamma_multiply(0.5)
                },
            ),
        );
        p.text(
            Pos2::new(plot.min.x - 8.0, y),
            egui::Align2::RIGHT_CENTER,
            num(max * f),
            body(10.5),
            INK3,
        );
    }
    if pts.len() < 2 {
        p.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "Hacen falta más días de datos",
            body(12.0),
            INK3,
        );
        return;
    }
    let at = |i: usize, v: f64| {
        Pos2::new(
            plot.min.x + plot.width() * i as f32 / (pts.len() - 1) as f32,
            plot.max.y - (v / max) as f32 * plot.height(),
        )
    };
    let line: Vec<Pos2> = pts.iter().enumerate().map(|(i, x)| at(i, x.1)).collect();
    // Soft area under the line.
    let mut area = line.clone();
    area.push(Pos2::new(plot.max.x, plot.max.y));
    area.push(Pos2::new(plot.min.x, plot.max.y));
    for k in 0..line.len() - 1 {
        let quad = vec![
            line[k],
            line[k + 1],
            Pos2::new(line[k + 1].x, plot.max.y),
            Pos2::new(line[k].x, plot.max.y),
        ];
        p.add(egui::Shape::convex_polygon(
            quad,
            color.gamma_multiply(0.10),
            Stroke::NONE,
        ));
    }
    p.add(egui::Shape::line(line.clone(), Stroke::new(2.0, color)));
    p.circle_filled(*line.last().unwrap_or(&plot.max), 3.5, color);
    for i in [0, pts.len() - 1] {
        let l: String = pts[i].0.chars().skip(5).collect();
        p.text(
            Pos2::new(line[i].x, plot.max.y + 11.0),
            egui::Align2::CENTER_CENTER,
            l,
            body(10.5),
            INK3,
        );
    }
    if let Some(h) = resp.hover_pos().filter(|h| plot.contains(*h)) {
        let i = (((h.x - plot.min.x) / plot.width()) * (pts.len() - 1) as f32)
            .round()
            .clamp(0.0, (pts.len() - 1) as f32) as usize;
        let q = line[i];
        p.line_segment(
            [Pos2::new(q.x, plot.min.y), Pos2::new(q.x, plot.max.y)],
            Stroke::new(1.0, INK3),
        );
        p.circle(q, 4.5, PANEL, Stroke::new(2.0, color));
        resp.on_hover_text(format!("{}: {} tokens por turno", pts[i].0, num(pts[i].1)));
    }
}
