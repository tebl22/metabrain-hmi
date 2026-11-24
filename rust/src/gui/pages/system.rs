//! System pages (alarms, router, memory, services, agents, extensions, settings) and permissions overlay.

use super::{columns2, empty, hhmm, short_path};
use crate::gui::theme::*;
use crate::gui::{App, Page};
use crate::util::{now, J};
use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Sense, Stroke, Vec2};
use serde_json::{json, Value};

// =====================================================================================
// Alarmas
// =====================================================================================

pub(super) fn alarms(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    let list = snap.arr("alarms");
    let mut go: Option<Page> = None;
    let mut acts: Vec<Value> = vec![];
    columns2(
        ui,
        |ui| {
            section(ui, "Activas", Some(&list.len().to_string()));
            if list.is_empty() {
                empty(ui, "Ninguna alarma. Todo en orden.");
            }
            for a in &list {
                let c = level_color(&a.s("level"));
                card(ui, Some(c), |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new(match a.s("level").as_str() {
                            "alarm" => "Alarma",
                            "warning" => "Aviso",
                            _ => "Info",
                        })
                        .font(medium(11.5))
                        .color(c),
                    );
                    ui.label(egui::RichText::new(crate::l10n::es(&a.s("text"))).color(INK));
                    ui.horizontal(|ui| {
                        let brain = a.s("brain");
                        match a.s("code").as_str() {
                            "paused" => {
                                if primary(ui, "Reanudar").clicked() {
                                    acts.push(json!({"a": "resume", "brain": brain}));
                                }
                            }
                            "readonly" => {
                                if quiet(ui, "Quitar solo lectura").clicked() {
                                    acts.push(json!({"a": "set", "brain": brain, "field": "readonly", "value": false}));
                                }
                            }
                            "quota" => {
                                if quiet(ui, "Ver uso").clicked() {
                                    go = Some(Page::Usage);
                                }
                            }
                            "service_down" => {
                                if quiet(ui, "Ver servicios").clicked() {
                                    go = Some(Page::Services);
                                }
                            }
                            "signin"
                                if quiet(ui, "Ver diagnóstico").clicked() => {
                                    go = Some(Page::Settings);
                                }
                            _ => {}
                        }
                        if quiet(ui, "Ocultar 8 h").clicked() {
                            acts.push(json!({"a": "ack_alarm", "id": a.s("id"), "hours": 8}));
                        }
                    });
                });
                ui.add_space(8.0);
            }
        },
        |ui| {
            section(ui, "Registro", Some("avisos y alarmas recientes"));
            diary(ui, snap, true);
        },
    );
    for a in acts {
        app.act(a);
    }
    if let Some(p) = go {
        app.page = p;
    }
}

fn diary(ui: &mut egui::Ui, snap: &Value, only_important: bool) {
    card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        let list: Vec<Value> = snap
            .arr("diary")
            .into_iter()
            .filter(|e| !only_important || e.s("level") != "info")
            .collect();
        if list.is_empty() {
            ui.label(dim("Sin registros."));
        }
        for e in list.iter().rev().take(80) {
            ui.horizontal_top(|ui| {
                ui.label(
                    egui::RichText::new(hhmm(e.f("ts").unwrap_or(0.0)))
                        .font(mono(11.5))
                        .color(INK3),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{}  {}", e.s("who"), e.s("text")))
                            .font(body(12.5))
                            .color(if e.s("level") == "info" {
                                INK2
                            } else {
                                level_color(&e.s("level"))
                            }),
                    )
                    .wrap(),
                );
            });
        }
    });
}

// =====================================================================================
// Router
// =====================================================================================

pub(super) fn router(app: &mut App, ui: &mut egui::Ui) {
    card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        section(
            ui,
            "Probar",
            Some("escribe una tarea y mira a quién la mandaría y por qué"),
        );
        let mut decide_now = false;
        if ui.available_width() < 540.0 {
            ui.add(
                egui::TextEdit::singleline(&mut app.ui.rt_task)
                    .hint_text("p. ej. renombra la variable x en utils.py")
                    .desired_width(ui.available_width()),
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.ui.rt_project)
                        .hint_text("project")
                        .desired_width((ui.available_width() - 96.0).max(120.0)),
                );
                if primary(ui, "Decide").clicked() {
                    decide_now = true;
                }
            });
        } else {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.ui.rt_task)
                        .hint_text("e.g. rename variable x in utils.rs")
                        .desired_width((ui.available_width() - 260.0).max(160.0)),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut app.ui.rt_project)
                        .hint_text("project")
                        .desired_width(150.0),
                );
                if primary(ui, "Decide").clicked() {
                    decide_now = true;
                }
            });
        }
        if decide_now && !app.ui.rt_task.trim().is_empty() && app.ui.rt_pending.is_none() {
            let p = if app.ui.rt_project.trim().is_empty() {
                "general".to_string()
            } else {
                app.ui.rt_project.trim().to_string()
            };
            let task = app.ui.rt_task.trim().to_string();
            let sup = app.sup.clone();
            let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
            app.ui.rt_pending = Some(slot.clone());
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                let res = sup.route_task(&p, &task, None);
                if let Ok(mut g) = slot.lock() {
                    *g = Some(res);
                }
                ctx.request_repaint();
            });
        }
        if let Some(slot) = app.ui.rt_pending.clone() {
            let done = slot.lock().ok().and_then(|mut g| g.take());
            if let Some(res) = done {
                app.ui.rt_result = res;
                app.ui.rt_pending = None;
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(faint("Routing task…"));
                });
            }
        }
        let r = app.ui.rt_result.clone();
        if !r.is_null() {
            ui.add_space(8.0);
            let pick = r.get("pick").cloned().unwrap_or(Value::Null);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Level {:.0} · {}",
                        r.f("level").unwrap_or(0.0),
                        r.s("level_name")
                    ))
                    .font(cond(15.0))
                    .color(MASTER),
                );
                ui.label(
                    egui::RichText::new(format!("→ {} with {}", pick.s("brain"), pick.s("model")))
                        .font(medium(14.0))
                        .color(INK),
                );
                if !pick.s("effort").is_empty() {
                    ui.label(faint(format!("effort {}", pick.s("effort"))));
                }
            });
            ui.add(egui::Label::new(faint(r.s("why"))).wrap());
        }
    });
    ui.add_space(14.0);

    let t = app
        .data
        .read()
        .map(|d| d.router.clone())
        .unwrap_or(Value::Null);
    ui.horizontal(|ui| {
        section(ui, "Learned per project", None);
        if let Some(s) = t.f("saving_pct").filter(|s| *s > 0.0) {
            ui.label(
                egui::RichText::new(format!("estimated cost savings vs always-Opus: {s:.0}%"))
                    .font(body(12.5))
                    .color(RUN),
            );
        }
    });
    let projects = t.arr("projects");
    if projects.is_empty() {
        empty(
            ui,
            "No learned projects yet. Each routed and verified task trains the router.",
        );
        return;
    }
    for p in &projects {
        card(ui, None, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(p.s("project"))
                    .font(bold(14.0))
                    .color(INK),
            );
            egui::Grid::new(("rt", p.s("project")))
                .num_columns(3)
                .spacing([18.0, 6.0])
                .show(ui, |ui| {
                    for l in p.arr("levels") {
                        ui.label(
                            egui::RichText::new(format!(
                                "{:.0} {}",
                                l.f("level").unwrap_or(0.0),
                                l.s("level_name")
                            ))
                            .font(medium(12.5))
                            .color(INK2),
                        );
                        ui.label(
                            egui::RichText::new(l.s("chosen"))
                                .font(mono(12.0))
                                .color(MASTER),
                        );
                        ui.horizontal_wrapped(|ui| {
                            for c in l.arr("candidates") {
                                let n = c.f("n").unwrap_or(0.0);
                                let sr = c.f("success_rate").unwrap_or(0.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} {:.0} % ({:.0})",
                                        c.s("candidate"),
                                        sr * 100.0,
                                        n
                                    ))
                                    .font(mono(11.5))
                                    .color(if c.b("chosen", false) { INK } else { INK3 }),
                                );
                            }
                        });
                        ui.end_row();
                    }
                });
        });
        ui.add_space(8.0);
    }
}

// =====================================================================================
// Memoria
// =====================================================================================

pub(super) fn memory(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    let m = app.get("api/memory", 15.0);
    let name = |id: &str| {
        snap.arr("brains")
            .iter()
            .find(|b| b.s("id") == id)
            .map(|b| b.s("name"))
            .unwrap_or_else(|| id.to_string())
    };
    columns2(
        ui,
        |ui| {
            section(
                ui,
                "Archivos en uso",
                Some("un cerebro los está editando; los demás esperan"),
            );
            card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                let locks = m.arr("locks");
                if locks.is_empty() {
                    ui.label(dim("Ningún archivo bloqueado."));
                }
                for l in locks {
                    let f = l.os("file").unwrap_or_else(|| l.s("path"));
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(short_path(&f))
                                .font(mono(12.0))
                                .color(INK),
                        )
                        .on_hover_text(&f);
                        ui.label(faint(name(&l.s("brain"))));
                    });
                }
            });
            ui.add_space(12.0);
            section(
                ui,
                "Notas compartidas",
                Some("lo que un cerebro dejó para los demás"),
            );
            card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                let rec = m.arr("recent");
                if rec.is_empty() {
                    ui.label(dim(if m.is_null() {
                        "Leyendo…"
                    } else {
                        "Sin notas recientes."
                    }));
                }
                for n in rec.iter().take(40) {
                    ui.horizontal_top(|ui| {
                        ui.label(
                            egui::RichText::new(hhmm(n.f("ts").unwrap_or(0.0)))
                                .font(mono(11.5))
                                .color(INK3),
                        );
                        let txt = n
                            .os("text")
                            .unwrap_or_else(|| format!("editó {}", short_path(&n.s("file"))));
                        ui.add(
                            egui::Label::new(egui::RichText::new(txt).font(body(12.5)).color(INK2))
                                .wrap(),
                        );
                    });
                }
            });
        },
        |ui| {
            section(ui, "Diario", None);
            diary(ui, snap, false);
        },
    );
}

// =====================================================================================
// Servicios
// =====================================================================================

pub(super) fn services(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    let svcs = snap.arr("services");
    let m = snap.get("machine").cloned().unwrap_or(Value::Null);
    let acts: std::cell::RefCell<Vec<Value>> = Default::default();
    columns2(
        ui,
        |ui| {
            section(ui, "Servicios", None);
            if svcs.is_empty() {
                empty(ui, "No hay servicios configurados.");
            }
            for s in &svcs {
                let (st, c) = if s.b("running", false) {
                    if s.b("paused", false) {
                        ("En pausa", ATTN)
                    } else {
                        ("Funcionando", RUN)
                    }
                } else {
                    ("Detenido", ALARM)
                };
                card(ui, None, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(s.s("name")).font(bold(13.5)).color(INK));
                        chip(ui, st, c);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let id = s.s("id");
                            if s.b("running", false) {
                                if quiet(ui, "Detener").clicked() {
                                    acts.borrow_mut()
                                        .push(json!({"a": "svc_stop", "service": id}));
                                }
                                if s.b("can_pause", false) {
                                    let (a, l) = if s.b("paused", false) {
                                        ("svc_resume", "Reanudar")
                                    } else {
                                        ("svc_pause", "Pausar")
                                    };
                                    if quiet(ui, l).clicked() {
                                        acts.borrow_mut().push(json!({"a": a, "service": id}));
                                    }
                                }
                            } else if primary(ui, "Iniciar").clicked() {
                                acts.borrow_mut()
                                    .push(json!({"a": "svc_start", "service": id}));
                            }
                        });
                    });
                    if let Some(ts) = s.f("heartbeat_ts") {
                        ui.label(faint(format!("último latido {}", ago(now(), ts))));
                    }
                    let hb = s.s("heartbeat");
                    if !hb.is_empty() {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(hb.chars().take(140).collect::<String>())
                                    .font(mono(11.5))
                                    .color(INK3),
                            )
                            .wrap(),
                        );
                    }
                });
                ui.add_space(8.0);
            }
        },
        |ui| {
            section(ui, "Máquina", None);
            card(ui, None, |ui| {
                ui.set_width(ui.available_width());
                for (l, v) in [("CPU", m.f("cpu")), ("RAM", m.f("ram"))] {
                    ui.horizontal(|ui| {
                        ui.label(dim(l));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!("{:.0} %", v.unwrap_or(0.0)))
                                    .font(mono(12.0))
                                    .color(INK2),
                            );
                        });
                    });
                    meter(ui, v.map(|x| x / 100.0), ui.available_width());
                    ui.add_space(4.0);
                }
                if let Some(g) = m.get("gpu").filter(|g| g.is_object()) {
                    ui.horizontal(|ui| {
                        ui.label(dim("GPU"));
                        ui.label(faint(format!(
                            "{} · {:.0} °C",
                            g.s("name"),
                            g.f("temp").unwrap_or(0.0)
                        )));
                    });
                    meter(ui, g.f("util").map(|x| x / 100.0), ui.available_width());
                }
                let heavy = m.arr("heavy");
                if !heavy.is_empty() {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Procesos pesados")
                            .font(cond(14.0))
                            .color(INK),
                    );
                    for h in heavy {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{} · PID {:.0} · {:.0} MB", h.s("kind"), h.f("pid").unwrap_or(0.0), h.f("mb").unwrap_or(0.0))).font(body(12.5)).color(INK2));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if danger(ui, "Cerrar").clicked() {
                                    acts.borrow_mut().push(json!({"a": "kill", "pid": h.get("pid").cloned().unwrap_or(Value::Null)}));
                                }
                            });
                        });
                    }
                }
            });
        },
    );
    for a in acts.into_inner() {
        app.act(a);
    }
}

// =====================================================================================
// Agentes
// =====================================================================================

pub(super) fn agents(_app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    let t = now();
    for b in snap.arr("brains") {
        let sess: Vec<Value> = b
            .arr("sessions")
            .into_iter()
            .filter(|s| s.b("active", false) || t - s.f("last").unwrap_or(0.0) < 3.0 * 3600.0)
            .take(8)
            .collect();
        let subs = b.arr("subagents");
        let jobs = b.arr("jobs");
        card(ui, None, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let active = b.b("active", false);
                let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                ui.painter()
                    .circle_filled(r.center(), 4.0, if active { RUN } else { INK3 });
                ui.label(egui::RichText::new(b.s("name")).font(bold(14.0)).color(INK));
                ui.label(faint(b.s("status")));
            });
            if sess.is_empty() && subs.is_empty() && jobs.is_empty() {
                ui.label(dim("Sin actividad en las últimas 3 horas."));
            }
            for s in &sess {
                ui.horizontal(|ui| {
                    ui.add_space(14.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(
                        r.center(),
                        3.0,
                        if s.b("active", false) { RUN } else { INK3 },
                    );
                    ui.label(
                        egui::RichText::new(s.s("project"))
                            .font(medium(12.5))
                            .color(INK),
                    );
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(s.s("title").chars().take(80).collect::<String>())
                                .font(body(12.5))
                                .color(INK2),
                        )
                        .truncate(),
                    );
                    ui.label(faint(ago(t, s.f("last").unwrap_or(t))));
                });
                for x in subs.iter().filter(|x| x.s("session") == s.s("id")) {
                    sub_row(ui, x);
                }
            }
            for x in subs.iter().filter(|x| {
                x.s("session").is_empty() || !sess.iter().any(|s| s.s("id") == x.s("session"))
            }) {
                sub_row(ui, x);
            }
            for j in &jobs {
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "delegado a {}: {}",
                                j.s("to"),
                                j.s("task").chars().take(90).collect::<String>()
                            ))
                            .font(body(12.0))
                            .color(if j.b("alive", false) { RUN } else { INK3 }),
                        )
                        .truncate(),
                    );
                });
            }
        });
        ui.add_space(10.0);
    }
}

fn sub_row(ui: &mut egui::Ui, x: &Value) {
    ui.horizontal(|ui| {
        ui.add_space(28.0);
        let (r, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
        ui.painter().circle_filled(
            r.center(),
            2.5,
            if x.b("alive", false) { RUN } else { INK3 },
        );
        ui.label(
            egui::RichText::new(x.s("desc"))
                .font(body(12.5))
                .color(INK2),
        );
        ui.label(faint(x.s("type")));
    });
}

// =====================================================================================
// Extensiones
// =====================================================================================

pub(super) fn extensions(app: &mut App, ui: &mut egui::Ui) {
    let list = app.get("api/extensions", 30.0);
    if list.is_null() {
        empty(ui, "Leyendo extensiones…");
        return;
    }
    let mut remove: Option<Value> = None;
    for b in list.as_array().cloned().unwrap_or_default() {
        card(ui, None, |ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new(b.s("name")).font(bold(14.0)).color(INK));
            if !b.b("supported", true) {
                ui.label(dim(b.s("reason")));
                return;
            }
            columns2(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(format!("Servidores MCP ({})", b.arr("mcp").len()))
                            .font(cond(13.5))
                            .color(INK2),
                    );
                    for m in b.arr("mcp") {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(m.s("name")).font(mono(12.0)).color(INK));
                            ui.label(faint(m.s("scope")));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if icon_button(ui, Icon::Close, "Quitar este MCP", false, true).clicked() {
                                    remove = Some(json!({"brain": b.s("brain"), "op": "remove_mcp", "name": m.s("name"), "scope": m.s("scope")}));
                                }
                            });
                        });
                    }
                    if b.arr("mcp").is_empty() {
                        ui.label(faint("Ninguno"));
                    }
                },
                |ui| {
                    ui.label(
                        egui::RichText::new(format!("Skills ({})", b.arr("skills").len()))
                            .font(cond(13.5))
                            .color(INK2),
                    );
                    for s in b.arr("skills") {
                        ui.horizontal_top(|ui| {
                            ui.label(egui::RichText::new(s.s("name")).font(mono(12.0)).color(INK));
                            ui.add(
                                egui::Label::new(faint(
                                    s.s("description").chars().take(110).collect::<String>(),
                                ))
                                .wrap(),
                            );
                        });
                    }
                    if b.arr("skills").is_empty() {
                        ui.label(faint("Ninguna"));
                    }
                },
            );
        });
        ui.add_space(10.0);
    }
    if let Some(body) = remove {
        app.post("api/extensions", body, |r| {
            (r.os("result").unwrap_or_else(|| r.s("error")), INFO)
        });
        app.forget("api/extensions");
    }
}

// =====================================================================================
// Ajustes
// =====================================================================================

pub(super) fn settings(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    type ConfigEdit = Box<dyn FnOnce(&mut Value)>;
    let c = app.sup.cfg.get();
    let mut edits: Vec<ConfigEdit> = vec![];
    let mut acts: Vec<Value> = vec![];
    let mut install_hooks = false;

    let mut sync_autostart = false;
    set_section(
        ui,
        "Acerca de",
        "Versión, ejecutable activo y arranque automático",
        |ui| {
            ui.label(
                egui::RichText::new(format!("MetaBrain v{}", env!("CARGO_PKG_VERSION")))
                    .font(medium(14.0))
                    .color(INK),
            );
            if let Ok(path) = std::env::current_exe() {
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(path.display().to_string())
                                .font(mono(12.0))
                                .color(INK2),
                        )
                        .selectable(true),
                    );
                    if quiet(ui, "Abrir carpeta").clicked() {
                        if let Some(dir) = path.parent() {
                            crate::app::open_browser(&dir.display().to_string());
                        }
                    }
                });
            }
            ui.add_space(4.0);
            let status = app
                .ui
                .autostart
                .get_or_insert_with(crate::app::autostart_status)
                .clone();
            ui.horizontal_wrapped(|ui| match &status {
                crate::app::AutostartStatus::Synced { .. } => {
                    chip(ui, "Arranque sincronizado", RUN);
                }
                crate::app::AutostartStatus::Mismatch { registered, .. } => {
                    chip(ui, "Arranque desactualizado", ATTN);
                    if primary(ui, "Sincronizar arranque").clicked() {
                        sync_autostart = true;
                    }
                    ui.label(faint(format!("Apunta a: {registered}")));
                }
                crate::app::AutostartStatus::Missing { .. } => {
                    chip(ui, "Arranque desactivado", INK3);
                    if quiet(ui, "Sincronizar arranque").clicked() {
                        sync_autostart = true;
                    }
                }
                crate::app::AutostartStatus::Error(e) => {
                    chip(ui, "No se pudo leer el arranque", ALARM);
                    ui.label(faint(e));
                }
                crate::app::AutostartStatus::Unsupported => {
                    ui.label(faint("Arranque automático disponible en Windows"));
                }
            });
        },
    );
    if sync_autostart {
        match crate::app::set_autostart(true) {
            Ok(()) => {
                app.ui.autostart = Some(crate::app::autostart_status());
                app.say("Arranque automático sincronizado", RUN);
            }
            Err(e) => app.say(format!("No se pudo sincronizar el arranque: {e}"), ALARM),
        }
    }

    // Operation Mode (Eco / Normal / Max) + Autopilot per brain (migrated from Web UI).
    set_section(
        ui,
        "Operation Mode & Autopilot",
        "Switch global model/effort presets and per-brain autopilot supervision",
        |ui| {
            let cur_mode = snap.s("mode");
            row(ui, "Global operation mode", "Eco uses cheapest models/low effort · Normal balances · Max unlocks frontier models", |ui| {
            ui.horizontal(|ui| {
                for (id, label) in [("eco", "Eco"), ("normal", "Normal"), ("max", "Max")] {
                    let on = cur_mode == id || (cur_mode.is_empty() && id == "normal");
                    let btn = egui::Button::new(
                        egui::RichText::new(label)
                            .font(if on { medium(12.5) } else { body(12.5) })
                            .color(if on { INK } else { INK2 }),
                    )
                    .fill(if on { RAISED2 } else { Color32::TRANSPARENT })
                    .stroke(Stroke::new(1.0, if on { MASTER } else { RULE }))
                    .corner_radius(CornerRadius::same(R));
                    if ui.add(btn).clicked() {
                        acts.push(json!({"a": "mode", "mode": id}));
                    }
                }
            });
        });
            for b in snap.arr("brains") {
                let bid = b.s("id");
                let bname = b.os("name").unwrap_or_else(|| bid.clone());
                let cur_pilot = {
                    let p = b.s("pilot");
                    if p.is_empty() {
                        "off".to_string()
                    } else {
                        p
                    }
                };
                row(ui, &format!("Autopilot · {bname}"), "Off = manual · Advise = log suggestions · Auto = step down automatically under load", |ui| {
                ui.horizontal(|ui| {
                    for (pid, lbl) in [("off", "Off"), ("advise", "Advise"), ("auto", "Auto")] {
                        let on = cur_pilot == pid;
                        let btn = egui::Button::new(
                            egui::RichText::new(lbl)
                                .font(if on { medium(12.0) } else { body(12.0) })
                                .color(if on { INK } else { INK2 }),
                        )
                        .fill(if on { RAISED2 } else { Color32::TRANSPARENT })
                        .stroke(Stroke::new(1.0, if on { INFO } else { RULE }))
                        .corner_radius(CornerRadius::same(R));
                        if ui.add(btn).clicked() {
                            acts.push(json!({"a": "pilot", "brain": bid, "mode": pid}));
                        }
                    }
                });
            });
            }
        },
    );

    // Diagnosis first: it tells you what to fix.
    let doc = app
        .data
        .read()
        .map(|d| d.doctor.clone())
        .unwrap_or(Value::Null);
    set_section(
        ui,
        "Diagnostics",
        "Health checks MetaBrain verifies automatically",
        |ui| {
            for ch in doc.arr("checks") {
                let ok = ch.b("ok", false);
                ui.horizontal_top(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 18.0), Sense::hover());
                    ui.painter().circle_filled(
                        Pos2::new(r.center().x, r.min.y + 9.0),
                        4.0,
                        if ok { RUN } else { ATTN },
                    );
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(ch.s("title")).color(INK));
                        if !ch.s("detail").is_empty() {
                            ui.label(faint(ch.s("detail")));
                        }
                        if !ok && !ch.s("fix").is_empty() {
                            ui.label(
                                egui::RichText::new(ch.s("fix"))
                                    .font(body(12.5))
                                    .color(ATTN),
                            );
                        }
                    });
                });
            }
            if doc.is_null() {
                ui.label(dim("Checking…"));
            }
            if quiet(ui, "Install Claude Code hooks").clicked() {
                install_hooks = true;
            }
        },
    );

    // Succession thresholds (the hierarchy itself is ordered on Home).
    let succ = c.get("succession").cloned().unwrap_or(json!({}));
    let s0 = app.ui.succ.unwrap_or((
        succ.f("threshold_pct").unwrap_or(90.0),
        succ.f("stall_minutes").unwrap_or(6.0),
        succ.f("handoff_tokens").unwrap_or(120000.0) / 1000.0,
    ));
    let mut s = s0;
    set_section(
        ui,
        "Account Failover & Succession",
        "When a brain steps down and the next tier takes command. Order hierarchy on Home.",
        |ui| {
            row(
                ui,
                "Step down when quota reaches",
                "Next tier takes command above this threshold",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut s.0)
                            .range(50.0..=100.0)
                            .suffix(" %"),
                    );
                },
            );
            row(
                ui,
                "Mark task stalled after",
                "0 disables stall detection",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut s.1)
                            .range(0.0..=120.0)
                            .suffix(" min"),
                    );
                },
            );
            row(
                ui,
                "Session handoff at",
                "0 disables automatic long-session handoff",
                |ui| {
                    ui.add(
                        egui::DragValue::new(&mut s.2)
                            .range(0.0..=2000.0)
                            .suffix("k tokens"),
                    );
                },
            );
            row(
                ui,
                "Pre-emptive account switch at",
                "Headroom margin before quota exhaustion",
                |ui| {
                    let mut f = c.f("failover_pct").unwrap_or(85.0);
                    let r = ui.add(egui::DragValue::new(&mut f).range(50.0..=99.0).suffix(" %"));
                    if r.drag_stopped() || r.lost_focus() {
                        edits.push(Box::new(move |c| c["failover_pct"] = json!(f)));
                    }
                },
            );
        },
    );
    if s != s0 {
        app.ui.succ = Some(s);
    }
    if app.ui.succ.is_some()
        && !ui.ctx().egui_is_using_pointer()
        && !ui.ctx().memory(|m| m.focused().is_some())
    {
        let (a, b, d) = app.ui.succ.take().unwrap_or(s);
        edits.push(Box::new(
            move |c| {
                c["succession"] =
                    json!({"threshold_pct": a, "stall_minutes": b, "handoff_tokens": d * 1000.0})
            },
        ));
    }

    let rules = snap.get("rules").cloned().unwrap_or(json!({}));
    set_section(
        ui,
        "Budgets",
        "How MetaBrain reacts when a brain approaches its limit",
        |ui| {
            let (mut down, mut pause) = (
                rules.f("downgrade_at").unwrap_or(80.0),
                rules.f("pause_at").unwrap_or(100.0),
            );
            row(ui, "Downgrade model at", "", |ui| {
                let r = ui.add(
                    egui::DragValue::new(&mut down)
                        .range(1.0..=100.0)
                        .suffix(" %"),
                );
                if r.drag_stopped() || r.lost_focus() {
                    acts.push(json!({"a": "rules", "downgrade_at": down, "pause_at": pause}));
                }
            });
            row(ui, "Pause brain at", "", |ui| {
                let r = ui.add(
                    egui::DragValue::new(&mut pause)
                        .range(1.0..=100.0)
                        .suffix(" %"),
                );
                if r.drag_stopped() || r.lost_focus() {
                    acts.push(json!({"a": "rules", "downgrade_at": down, "pause_at": pause}));
                }
            });
            row(
                ui,
                "Auto-resume",
                "Resume automatically once quota recovers",
                |ui| {
                    let mut on = rules.b("auto_resume", true);
                    if ui.checkbox(&mut on, "").changed() {
                        edits.push(Box::new(move |c| {
                            if !c["rules"].is_object() {
                                c["rules"] = json!({});
                            }
                            c["rules"]["auto_resume"] = json!(on);
                        }));
                    }
                },
            );
        },
    );

    let rt = c.get("router").cloned().unwrap_or(json!({}));
    let ensure = |c: &mut Value, k: &str| {
        if !c[k].is_object() {
            c[k] = json!({});
        }
    };
    set_section(
        ui,
        "Task Routing",
        "How tasks are distributed between frontier and low-cost brains",
        |ui| {
            row(
                ui,
                "Shadow mode",
                "Log routing recommendations without changing execution",
                |ui| {
                    let mut on = rt.b("shadow", false);
                    if ui.checkbox(&mut on, "").changed() {
                        edits.push(Box::new(move |c| {
                            ensure(c, "router");
                            c["router"]["shadow"] = json!(on);
                        }));
                    }
                },
            );
            row(
                ui,
                "Classify with cheap model",
                "Use the workhorse brain to estimate task difficulty",
                |ui| {
                    let mut on = rt.b("llm_classify", false);
                    if ui.checkbox(&mut on, "").changed() {
                        edits.push(Box::new(move |c| {
                            ensure(c, "router");
                            c["router"]["llm_classify"] = json!(on);
                        }));
                    }
                },
            );
            row(
                ui,
                "Minimum success rate for cheapest tier",
                "Escalate to next model when below this threshold",
                |ui| {
                    let mut t = rt.f("target").unwrap_or(0.85) * 100.0;
                    let r = ui.add(egui::DragValue::new(&mut t).range(50.0..=99.0).suffix(" %"));
                    if r.drag_stopped() || r.lost_focus() {
                        edits.push(Box::new(move |c| {
                            ensure(c, "router");
                            c["router"]["target"] = json!(t / 100.0);
                        }));
                    }
                },
            );
            row(
                ui,
                "Max parallel tasks",
                "Global concurrency cap across all brains",
                |ui| {
                    let mut n = c.f("max_parallel_total").unwrap_or(8.0);
                    let r = ui.add(egui::DragValue::new(&mut n).range(1.0..=32.0));
                    if r.drag_stopped() || r.lost_focus() {
                        edits.push(Box::new(move |c| c["max_parallel_total"] = json!(n)));
                    }
                },
            );
        },
    );

    let ap = c.get("approvals").cloned().unwrap_or(json!({}));
    set_section(
        ui,
        "Permissions & Execution Safety",
        "Control tool approvals and CLI sandbox policies",
        |ui| {
            row(
                ui,
                "Show permission prompts here",
                "Approve or deny Claude Code tool requests from MetaBrain",
                |ui| {
                    let mut on = ap.b("enabled", true);
                    if ui.checkbox(&mut on, "").changed() {
                        edits.push(Box::new(move |c| {
                            ensure(c, "approvals");
                            c["approvals"]["enabled"] = json!(on);
                        }));
                    }
                },
            );
            row(
                ui,
                "Approval timeout",
                "Falls back to terminal prompt after this duration",
                |ui| {
                    let mut w = ap.f("wait_s").unwrap_or(20.0);
                    let r = ui.add(egui::DragValue::new(&mut w).range(2.0..=600.0).suffix(" s"));
                    if r.drag_stopped() || r.lost_focus() {
                        edits.push(Box::new(move |c| {
                            ensure(c, "approvals");
                            c["approvals"]["wait_s"] = json!(w);
                        }));
                    }
                },
            );
            row(ui, "Allow unsafe sandbox bypass (explicit opt-in)", "WARNING: enables danger-full-access / --dangerously-skip-permissions on CLI build tasks. Keep off for least privilege.", |ui| {
            let mut on = c.b("allow_unsafe_exec", false);
            if ui.checkbox(&mut on, "").changed() {
                edits.push(Box::new(move |c| c["allow_unsafe_exec"] = json!(on)));
            }
        });
        },
    );

    set_section(ui, "General", "", |ui| {
        row(
            ui,
            "Desktop notifications",
            "Alerts for alarms and command handoffs",
            |ui| {
                let mut on = c.b("notifications", true);
                if ui.checkbox(&mut on, "").changed() {
                    edits.push(Box::new(move |c| c["notifications"] = json!(on)));
                }
            },
        );
        row(ui, "History retention", "", |ui| {
            let mut d = c.f("history_days").unwrap_or(30.0);
            let r = ui.add(
                egui::DragValue::new(&mut d)
                    .range(7.0..=365.0)
                    .suffix(" days"),
            );
            if r.drag_stopped() || r.lost_focus() {
                edits.push(Box::new(move |c| c["history_days"] = json!(d)));
            }
        });
        row(
            ui,
            "Mobile / LAN monitor",
            "Read-only LAN dashboard protected by PIN (requires --lan; HTTP without TLS)",
            |ui| {
                let mut on = c.get("lan").map(|l| l.b("enabled", false)).unwrap_or(false);
                if ui.checkbox(&mut on, "").changed() {
                    edits.push(Box::new(move |c| {
                        ensure(c, "lan");
                        c["lan"]["enabled"] = json!(on);
                    }));
                }
            },
        );
        let lan_info = app.get("api/lan", 15.0);
        if !lan_info.is_null() {
            let listening = lan_info.b("listening", false);
            let enabled = lan_info.b("enabled", false);
            if listening {
                ui.label(egui::RichText::new(format!(
                    "Activo en red local ({}) · protegido con PIN · HTTP sin TLS (úsalo solo en redes de confianza)",
                    lan_info.s("url")
                )).font(body(11.5)).color(ATTN));
            } else if enabled {
                ui.label(faint("Preferencia guardada, pero esta instancia escucha solo en 127.0.0.1. Inicia con --lan para abrirlo en tu red."));
            } else {
                ui.label(faint("Escuchando solo en este equipo (127.0.0.1)."));
            }
        }
    });

    let mut save_json: Option<Value> = None;
    set_section(ui, "Avanzado", "La configuración completa en JSON", |ui| {
        if !app.ui.cfg_open {
            if quiet(ui, "Editar JSON").clicked() {
                let mut v = c.clone();
                if v.get("lan").and_then(|l| l.get("pin")).is_some() {
                    v["lan"]["pin"] = json!("••••••");
                }
                app.ui.cfg_text = serde_json::to_string_pretty(&v).unwrap_or_default();
                app.ui.cfg_open = true;
                app.ui.cfg_err.clear();
            }
            return;
        }
        ui.add(
            egui::TextEdit::multiline(&mut app.ui.cfg_text)
                .font(mono(12.0))
                .desired_rows(18)
                .desired_width(f32::INFINITY)
                .code_editor(),
        );
        if !app.ui.cfg_err.is_empty() {
            ui.label(egui::RichText::new(&app.ui.cfg_err).color(ALARM));
        }
        ui.horizontal(|ui| {
            if primary(ui, "Guardar").clicked() {
                match serde_json::from_str::<Value>(&app.ui.cfg_text) {
                    Ok(mut nc) => {
                        if nc
                            .get("lan")
                            .and_then(|l| l.get("pin"))
                            .and_then(|p| p.as_str())
                            == Some("••••••")
                        {
                            nc["lan"]["pin"] = c
                                .get("lan")
                                .and_then(|l| l.get("pin"))
                                .cloned()
                                .unwrap_or(Value::Null);
                        }
                        save_json = Some(nc);
                    }
                    Err(e) => app.ui.cfg_err = format!("JSON inválido: {e}"),
                }
            }
            if quiet(ui, "Cancelar").clicked() {
                app.ui.cfg_open = false;
            }
        });
    });

    if install_hooks {
        app.post("api/hooks/install", json!({}), |r| {
            (
                r.arr("result")
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(". "),
                RUN,
            )
        });
    }
    for a in acts {
        app.act(a);
        app.say("Guardado", RUN);
    }
    if !edits.is_empty() {
        let mut nc = app.sup.cfg.get();
        for e in edits {
            e(&mut nc);
        }
        let r = app.sup.act(&json!({"a": "config", "config": nc}));
        app.say(if r == "ok" { "Guardado".to_string() } else { r }, RUN);
    }
    if let Some(nc) = save_json {
        let r = app.sup.act(&json!({"a": "config", "config": nc}));
        if r == "ok" {
            app.ui.cfg_open = false;
            app.say("Configuración guardada", RUN);
        } else {
            app.ui.cfg_err = r;
        }
    }
}

fn set_section(ui: &mut egui::Ui, title: &str, sub: &str, body: impl FnOnce(&mut egui::Ui)) {
    let w = ui.available_width().min(900.0);
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, RULE))
        .corner_radius(CornerRadius::same(R))
        .inner_margin(Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width((w - 32.0).max(1.0));
            ui.label(egui::RichText::new(title).font(cond(16.0)).color(INK));
            if !sub.is_empty() {
                ui.add(egui::Label::new(faint(sub)).wrap());
            }
            ui.add_space(8.0);
            body(ui);
        });
    ui.add_space(12.0);
}

fn row(ui: &mut egui::Ui, label: &str, hint: &str, ctl: impl FnOnce(&mut egui::Ui)) {
    if ui.available_width() < 460.0 {
        ui.label(egui::RichText::new(label).color(INK));
        if !hint.is_empty() {
            ui.add(egui::Label::new(faint(hint)).wrap());
        }
        ctl(ui);
    } else {
        ui.horizontal(|ui| {
            let lw = (ui.available_width() - 180.0).max(160.0);
            ui.allocate_ui_with_layout(
                Vec2::new(lw, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(lw);
                    ui.label(egui::RichText::new(label).color(INK));
                    if !hint.is_empty() {
                        ui.add(egui::Label::new(faint(hint)).wrap());
                    }
                },
            );
            ctl(ui);
        });
    }
    ui.add_space(6.0);
}

// =====================================================================================
// Permission inbox (bottom-right, over every page)
// =====================================================================================

pub fn approvals_overlay(app: &mut App, ctx: &egui::Context) {
    let data = app
        .data
        .read()
        .map(|d| d.approvals.clone())
        .unwrap_or(Value::Null);
    let wait_s = data.f("wait_s").unwrap_or(20.0);
    let t = now();
    let pending: Vec<Value> = data
        .arr("pending")
        .into_iter()
        .filter(|r| !app.ui.decided.contains(&r.s("id")))
        .filter(|r| r.f("ts").map(|ts| ts + wait_s > t).unwrap_or(true))
        .collect();
    if pending.is_empty() {
        return;
    }
    // Keyboard: require Ctrl+Shift+A to allow or Ctrl+Shift+D to deny (prevents accidental single-key approval).
    let typing = ctx.memory(|m| m.focused().is_some());
    let (key_a, key_d) = ctx.input(|i| {
        let mods = i.modifiers.ctrl && i.modifiers.shift;
        (
            mods && i.key_pressed(egui::Key::A),
            mods && i.key_pressed(egui::Key::D),
        )
    });
    let mut decide: Option<(String, bool)> = None;
    if !typing {
        if key_a {
            decide = Some((pending[0].s("id"), true));
        } else if key_d {
            decide = Some((pending[0].s("id"), false));
        }
    }

    egui::Area::new(egui::Id::unique("approvals"))
        .anchor(egui::Align2::RIGHT_BOTTOM, Vec2::new(-18.0, -18.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, MASTER.gamma_multiply(0.6)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(12))
                .shadow(egui::Shadow {
                    offset: [0, 6],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(120),
                })
                .show(ui, |ui| {
                    ui.set_width((ctx.content_rect().width() - 36.0).clamp(240.0, 380.0));
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                        paint_icon(ui.painter(), r, Icon::Shield, MASTER);
                        ui.label(
                            egui::RichText::new(if pending.len() == 1 {
                                "Permission requested".to_string()
                            } else {
                                format!("{} permission requests", pending.len())
                            })
                            .font(cond(15.0))
                            .color(INK),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(faint("Ctrl+Shift+A allow · Ctrl+Shift+D deny"));
                        });
                    });
                    ui.add_space(6.0);
                    for r in pending.iter().take(3) {
                        let left = (r.f("ts").unwrap_or(t) + wait_s - t).max(0.0);
                        egui::Frame::new()
                            .fill(RAISED)
                            .corner_radius(CornerRadius::same(R))
                            .inner_margin(Margin::same(10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(r.s("brain_name"))
                                            .font(bold(13.0))
                                            .color(INK),
                                    );
                                    ui.label(faint(r.s("project")));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(format!("{left:.0} s"))
                                                    .font(mono(12.0))
                                                    .color(if left < 6.0 { ALARM } else { INK3 }),
                                            );
                                        },
                                    );
                                });
                                ui.label(
                                    egui::RichText::new(r.s("tool"))
                                        .font(medium(12.5))
                                        .color(MASTER),
                                );
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(r.s("summary"))
                                            .font(mono(12.0))
                                            .color(INK2),
                                    )
                                    .wrap(),
                                );
                                meter(ui, Some(1.0 - left / wait_s.max(1.0)), ui.available_width());
                                ui.horizontal(|ui| {
                                    if primary(ui, "Permitir").clicked() {
                                        decide = Some((r.s("id"), true));
                                    }
                                    if danger(ui, "Denegar").clicked() {
                                        decide = Some((r.s("id"), false));
                                    }
                                });
                            });
                        ui.add_space(6.0);
                    }
                    if pending.len() > 3 {
                        ui.label(faint(format!("y {} más", pending.len() - 3)));
                    }
                });
        });

    if let Some((id, allow)) = decide {
        app.ui.decided.insert(id.clone());
        if crate::approvals::decide(&id, allow, "ventana") {
            let rec = crate::approvals::get(&id).unwrap_or(Value::Null);
            app.sup.diary.add(
                "Tú",
                &format!(
                    "{} {}: {}",
                    if allow { "permitiste a" } else { "denegaste a" },
                    rec.s("brain_name"),
                    rec.s("tool")
                ),
                "info",
            );
            app.say(
                if allow { "Permitido" } else { "Denegado" },
                if allow { RUN } else { ALARM },
            );
        } else {
            app.say("Ya se respondió en la terminal o expiró", ATTN);
        }
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(500));
}
