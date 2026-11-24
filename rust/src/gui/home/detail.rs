//! Core summary panel, slide-over brain detail drawer, and AI work architecture card.

use super::hierarchy::{levels, move_brain, send};
use super::{
    balance_text_snap, de_default, gauge_snap, hhmm, panel, pct_color, stat, state_of_snap,
    task_activity, ChainSnap, HomeSnap, TaskSnap,
};
use crate::gui::theme::*;
use crate::gui::App;
use crate::util::now;
use eframe::egui::{self, Pos2, Sense, Vec2};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Default, Deserialize)]
struct ArchSnap {
    #[serde(default, deserialize_with = "de_default")]
    by_model: bool,
    #[serde(default, deserialize_with = "de_default")]
    auto: bool,
    #[serde(default, deserialize_with = "de_default")]
    differs: bool,
    #[serde(default, deserialize_with = "de_default")]
    summary: String,
    #[serde(default, deserialize_with = "de_default")]
    levels: Vec<Vec<String>>,
    #[serde(default, deserialize_with = "de_default")]
    roles: HashMap<String, String>,
    #[serde(default, deserialize_with = "de_default")]
    lines: Vec<String>,
}

pub(super) fn core_detail(app: &mut App, ui: &mut egui::Ui, snap: &HomeSnap) {
    let bs = &snap.brains;
    architecture(app, ui, snap);
    ui.add_space(10.0);
    panel(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Resumen").font(cond(18.0)).color(INK));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (t, col) = match snap.overall.as_str() {
                    "ALARM" => ("Con alarmas", ALARM),
                    "WARNING" => ("Atención", ATTN),
                    _ => ("Todo en orden", RUN),
                };
                chip(ui, t, col);
            });
        });
        ui.add_space(6.0);
        let active = bs.iter().filter(|b| b.active).count();
        let subs: usize = bs
            .iter()
            .map(|b| b.subagents.iter().filter(|s| s.alive).count())
            .sum();
        let today: f64 = bs.iter().map(|b| b.usage.today).sum();
        ui.horizontal_wrapped(|ui| {
            stat(ui, &format!("{active}/{}", bs.len()), "trabajando");
            ui.add_space(18.0);
            stat(ui, &subs.to_string(), "subagentes");
            ui.add_space(18.0);
            stat(ui, &num(today), "tokens hoy");
        });
        ui.add_space(8.0);
        for b in bs {
            let (pct, _, _) = gauge_snap(b);
            let (sw, sc) = state_of_snap(b);
            let r = ui.horizontal(|ui| {
                let (dr, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dr.center(), 3.5, sc);
                ui.label(egui::RichText::new(&b.name).font(medium(13.0)).color(INK));
                ui.label(faint(sw));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(balance_text_snap(b).unwrap_or_else(|| {
                            pct.map(|v| format!("{v:.0} %"))
                                .unwrap_or_else(|| "–".into())
                        }))
                        .font(mono(12.0))
                        .color(pct_color(pct)),
                    );
                });
            });
            if r.response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                app.ui.sel = b.id.clone();
            }
        }
    });
    ui.add_space(10.0);
    let alarms = &snap.alarms;
    panel(ui, |ui| {
        ui.label(
            egui::RichText::new(format!("Alarmas ({})", alarms.len()))
                .font(cond(15.0))
                .color(INK),
        );
        if alarms.is_empty() {
            ui.label(dim("Nada que atender."));
        }
        for a in alarms.iter().take(5) {
            ui.horizontal_top(|ui| {
                let (dr, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
                ui.painter().circle_filled(
                    Pos2::new(dr.center().x, dr.min.y + 8.0),
                    3.0,
                    level_color(&a.level),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(crate::l10n::es(&a.text))
                            .font(body(12.5))
                            .color(INK2),
                    )
                    .wrap(),
                );
            });
        }
        if alarms.len() > 5 && quiet(ui, "Ver todas").clicked() {
            app.page = crate::gui::Page::Alarms;
        }
    });
    ui.add_space(10.0);
    panel(ui, |ui| {
        ui.label(egui::RichText::new("Diario").font(cond(15.0)).color(INK));
        for d in snap.diary.iter().rev().take(12) {
            ui.horizontal_top(|ui| {
                ui.label(egui::RichText::new(hhmm(d.ts)).font(mono(11.0)).color(INK3));
                ui.add(
                    egui::Label::new(egui::RichText::new(&d.text).font(body(12.0)).color(INK2))
                        .wrap(),
                );
            });
        }
    });
}

pub(super) fn brain_detail(app: &mut App, ui: &mut egui::Ui, snap: &HomeSnap, id: &str) {
    let Some(b) = snap.brains.iter().find(|b| b.id == id) else {
        return;
    };
    let ch = snap
        .chain
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .unwrap_or_default();
    let (sw, sc) = state_of_snap(b);
    let paused = b.effective.paused || b.state.paused;

    panel(ui, |ui| {
        ui.horizontal(|ui| {
            if icon_button(ui, Icon::Close, "Volver al resumen", false, true).clicked() {
                app.ui.sel = "core".into();
            }
            ui.label(egui::RichText::new(&b.name).font(cond(19.0)).color(INK));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                chip(ui, sw, sc);
            });
        });
        if !ch.reason.is_empty() && ch.reason != "disponible" {
            ui.label(
                egui::RichText::new(crate::l10n::es(&ch.reason))
                    .font(body(12.5))
                    .color(ATTN),
            );
        }
        if !b.warning.is_empty() {
            ui.label(
                egui::RichText::new(crate::l10n::es(&b.warning))
                    .font(body(12.5))
                    .color(ATTN),
            );
        }
        ui.add_space(6.0);
        // Quotas.
        for q in b.quota.iter().filter(|q| q.pct.is_some()) {
            let v = q.pct.unwrap_or(0.0);
            let rem = (100.0 - v).clamp(0.0, 100.0);
            ui.horizontal(|ui| {
                ui.label(faint(match q.group.as_str() {
                    "session" => "Sesión 5 h",
                    "weekly" => "Semana",
                    _ => "Cuota",
                }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(r) = q.reset {
                        ui.label(faint(format!("vuelve {}", hhmm(r))));
                    }
                    ui.label(
                        egui::RichText::new(format!("{v:.0} % (queda {rem:.0} %)"))
                            .font(mono(11.5))
                            .color(pct_color(Some(v))),
                    );
                });
            });
            meter(ui, Some(v / 100.0), ui.available_width());
            ui.add_space(3.0);
        }
        if let Some(r) = b.effective.ratio {
            ui.horizontal(|ui| {
                ui.label(faint("Presupuesto del día"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{:.0} %", (r * 100.0).min(100.0)))
                            .font(mono(12.0))
                            .color(pct_color(Some(r * 100.0))),
                    );
                });
            });
            meter(ui, Some(r), ui.available_width());
        }
        if let Some(bal) = &b.balance {
            ui.horizontal(|ui| {
                ui.label(faint("Saldo"));
                ui.label(
                    egui::RichText::new(format!(
                        "{:.2} {}",
                        bal.amount.unwrap_or(0.0),
                        bal.currency
                    ))
                    .font(mono(12.0))
                    .color(INK2),
                );
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            stat(ui, &num(b.usage.today), "hoy");
            ui.add_space(14.0);
            stat(ui, &num(b.usage.h5), "5 h");
            ui.add_space(14.0);
            stat(ui, &num(b.usage.d7), "7 días");
        });
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            let cur = b
                .effective
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("automático");
            let combo_w = (ui.available_width() - 90.0).clamp(110.0, 180.0);
            if !b.models.is_empty() {
                egui::ComboBox::from_id_salt(("dmodel", id))
                    .selected_text(egui::RichText::new(cur).font(mono(12.0)))
                    .width(combo_w)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(false, "automático").clicked() {
                            app.act(
                                json!({"a": "set", "brain": id, "field": "model", "value": null}),
                            );
                        }
                        for m in &b.models {
                            if ui
                                .selectable_label(cur == m, egui::RichText::new(m).font(mono(12.0)))
                                .clicked()
                            {
                                app.act(
                                    json!({"a": "set", "brain": id, "field": "model", "value": m, "confirm": true}),
                                );
                            }
                        }
                    });
            }
            if paused {
                if primary(ui, "Reanudar").clicked() {
                    app.act(json!({"a": "resume", "brain": id}));
                }
            } else if quiet(ui, "Pausar").clicked() {
                app.act(json!({"a": "pause", "brain": id}));
            }
        });
    });
    ui.add_space(10.0);

    // Hierarchy for this brain.
    let lv = levels(&snap.chain);
    let li = lv
        .iter()
        .position(|l| l.iter().any(|e| e.id == id))
        .unwrap_or(0);
    let shared = lv.get(li).map(|l| l.len() > 1).unwrap_or(false);
    panel(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Jerarquía").font(cond(15.0)).color(INK));
            ui.label(faint(format!(
                "nivel {} de {}{}",
                li + 1,
                lv.len(),
                if shared { " · comparte nivel" } else { "" }
            )));
        });
        ui.label(faint(if ch.master {
            "Está al mando ahora."
        } else {
            "Manda cuando los niveles de arriba no están disponibles."
        }));
        ui.add_space(6.0);
        let mut new: Option<Vec<Vec<ChainSnap>>> = None;
        let mut solo = false;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(li > 0 || shared, egui::Button::new("Subir"))
                .on_hover_text("Un nivel más arriba")
                .clicked()
            {
                new = Some(move_brain(&lv, id, -1));
            }
            if ui
                .add_enabled(li + 1 < lv.len() || shared, egui::Button::new("Bajar"))
                .on_hover_text("Un nivel más abajo")
                .clicked()
            {
                new = Some(move_brain(&lv, id, 1));
            }
            if li > 0
                && ui
                    .button("Mandar junto al de arriba")
                    .on_hover_text("Comparten el mando y se reparten las tareas")
                    .clicked()
            {
                let mut v = lv.clone();
                if let Some(pos) = v[li].iter().position(|e| e.id == id) {
                    let x = v[li].remove(pos);
                    v[li - 1].push(x);
                }
                v.retain(|l| !l.is_empty());
                new = Some(v);
            }
            if shared
                && ui
                    .button("Nivel propio")
                    .on_hover_text("Sale del nivel compartido")
                    .clicked()
            {
                let mut v = lv.clone();
                if let Some(pos) = v[li].iter().position(|e| e.id == id) {
                    let x = v[li].remove(pos);
                    v.insert(li + 1, vec![x]);
                }
                v.retain(|l| !l.is_empty());
                new = Some(v);
            }
            if (li > 0 || shared) && primary(ui, "Mandar solo").clicked() {
                solo = true;
            }
        });
        if let Some(v) = new {
            send(app, &v);
            app.say("Jerarquía guardada", MASTER);
        }
        if solo {
            app.act(json!({"a": "take_command", "brain": id}));
            app.say(format!("{} manda solo", b.name), MASTER);
        }
    });
    ui.add_space(10.0);

    // Subagents, delegations and tasks of this brain.
    let subs: Vec<_> = b.subagents.iter().filter(|s| s.alive).collect();
    let jobs: Vec<_> = b.jobs.iter().filter(|j| j.alive).collect();
    let tasks: Vec<_> = crate::gui::pages::task_list(app)
        .into_iter()
        .filter_map(|v| serde_json::from_value::<TaskSnap>(v).ok())
        .filter(|t| t.brain == id)
        .filter_map(|t| task_activity(&t, &app.ui.convs).map(|(_, color)| (t, color)))
        .take(8)
        .collect();
    panel(ui, |ui| {
        ui.label(
            egui::RichText::new("Subagentes y tareas")
                .font(cond(15.0))
                .color(INK),
        );
        if subs.is_empty() && jobs.is_empty() && tasks.is_empty() {
            ui.label(dim("Sin trabajo en curso."));
        }
        for s in subs.iter().take(10) {
            ui.horizontal(|ui| {
                let (dr, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dr.center(), 3.5, RUN);
                ui.add(
                    egui::Label::new(egui::RichText::new(&s.desc).font(body(12.5)).color(INK))
                        .truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(faint(num(s.tokens)));
                    ui.label(
                        egui::RichText::new(s.model.replace("claude-", ""))
                            .font(mono(11.0))
                            .color(INK3),
                    );
                });
            });
        }
        for j in &jobs {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("delegó a {}", j.to))
                        .font(medium(12.0))
                        .color(RUN),
                );
                ui.add(egui::Label::new(faint(&j.task)).truncate());
            });
        }
        for (t, color) in &tasks {
            let r = ui.horizontal(|ui| {
                let (dr, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter().circle_filled(dr.center(), 3.5, *color);
                ui.add(
                    egui::Label::new(egui::RichText::new(&t.title).font(body(12.5)).color(INK2))
                        .truncate(),
                );
            });
            if r.response
                .interact(Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked()
            {
                app.ui.sel_task = t.id.clone();
                app.page = crate::gui::Page::Tasks;
            }
        }
    });
    let _ = now();
}

/// Recommended work architecture for the brains available now, with apply / automatic.
fn architecture(app: &mut App, ui: &mut egui::Ui, snap: &HomeSnap) {
    let raw_a = app
        .data
        .read()
        .map(|d| d.arch.clone())
        .unwrap_or(Value::Null);
    if raw_a.is_null() {
        return;
    }
    let a: ArchSnap = serde_json::from_value(raw_a.clone()).unwrap_or_default();
    let names: HashMap<&str, (&str, usize)> = snap
        .brains
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.as_str(), (b.name.as_str(), i)))
        .collect();
    let mut toggle: Option<bool> = None;
    let mut apply = false;
    panel(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Arquitectura de trabajo")
                    .font(cond(16.0))
                    .color(INK),
            );
            if a.by_model {
                chip(ui, "por IA", INFO);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if a.auto {
                    chip(ui, "automática", RUN);
                }
            });
        });
        ui.label(faint("Según los cerebros disponibles ahora"));
        ui.add_space(6.0);
        // Role rows: dot in the brain's colour, name, role.
        for l in &a.levels {
            for id in l {
                let role = a.roles.get(id).map(String::as_str).unwrap_or("");
                let (name, i) = names.get(id.as_str()).copied().unwrap_or((id.as_str(), 0));
                let (label, col) = match role {
                    "director" => ("dirige", MASTER),
                    "submanager" => ("subgerente", INFO),
                    "producer" => ("produce", INK),
                    "workhorse" => ("burro de carga", RUN),
                    _ => ("reserva", INK3),
                };
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(
                        r.center(),
                        4.0,
                        if role == "reserve" { INK3 } else { series(i) },
                    );
                    ui.label(
                        egui::RichText::new(name)
                            .font(medium(13.0))
                            .color(if role == "reserve" { INK3 } else { INK }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(label).font(body(12.0)).color(col));
                    });
                });
            }
        }
        ui.add_space(4.0);
        for line in &a.lines {
            ui.add(egui::Label::new(egui::RichText::new(line).font(body(12.0)).color(INK3)).wrap());
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if a.differs && !a.auto && primary(ui, "Aplicar").clicked() {
                apply = true;
            }
            if quiet(ui, "Organizar con IA")
                .on_hover_text("Gemini 4 Neon (o el mejor cerebro disponible) propone la jerarquía")
                .clicked()
            {
                app.act(json!({"a": "organize"}));
                app.say("Pidiendo a la IA que organice el trabajo…", INFO);
            }
            let mut on = a.auto;
            if ui
                .checkbox(
                    &mut on,
                    egui::RichText::new("Reorganizar sola cuando cambie la disponibilidad")
                        .font(body(12.0))
                        .color(INK2),
                )
                .changed()
            {
                toggle = Some(on);
            }
        });
    });
    if apply {
        app.act(crate::architect::apply_payload(&raw_a));
        app.say(format!("Arquitectura aplicada: {}", a.summary), MASTER);
    }
    if let Some(on) = toggle {
        let mut c = app.sup.cfg.get();
        if !c["architect"].is_object() {
            c["architect"] = json!({});
        }
        c["architect"]["auto"] = json!(on);
        let r = app.sup.act(&json!({"a": "config", "config": c}));
        app.say(
            if r == "ok" {
                if on {
                    "MetaBrain reorganizará el trabajo solo".to_string()
                } else {
                    "Reorganización automática apagada".to_string()
                }
            } else {
                r
            },
            RUN,
        );
    }
}
