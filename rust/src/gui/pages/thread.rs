//! Chat thread rendering: message bubbles, grouped activity blocks, parallel runs, errors, and diff view.

use super::chat::{composer_box, file_uri, status_color, status_name, submit_chat};
use super::{short_path, DiffStats, DiffView};
use crate::gui::theme::*;
use crate::gui::{App, Page};
use crate::util::{now, J};
use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Sense, Stroke, Vec2};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(super) fn thread(app: &mut App, ui: &mut egui::Ui, snap: &Value, t: &Value, h: f32) {
    let id = t.s("id");
    if app.ui.ev_task != id {
        app.ui.ev_task = id.clone();
        app.ui.events.clear();
        app.ui.ev_off = 0;
    }
    let (evs, off) = app.sup.tasks.events_since(&id, app.ui.ev_off);
    app.ui.ev_off = off;
    app.ui.events.extend(evs);
    if app.ui.events.len() > 2500 {
        let n = app.ui.events.len() - 2500;
        app.ui.events.drain(..n);
    }
    let st = t.s("status");
    let live = crate::tasks::turn_busy(&st);
    let interruptible = live && st != "stopping";

    // Long metadata and actions may need separate rows on a narrow chat.
    let header_top = ui.next_widget_position().y;
    let mut goto: Option<String> = None;
    let parent_run = t.s("parent_run");
    let orch_root = if !parent_run.is_empty() {
        Some(parent_run.as_str())
    } else if t.s("kind") == "parallel" {
        Some(id.as_str())
    } else {
        None
    };
    if let Some(root_id) = orch_root {
        subchat_strip(app, ui, snap, root_id, &id, &mut goto);
        ui.add_space(4.0);
    }
    let avail_w = ui.available_width();
    let header_meta = |ui: &mut egui::Ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(t.s("title"))
                    .font(cond(20.0))
                    .color(INK),
            )
            .truncate(),
        );
        ui.horizontal_wrapped(|ui| {
            chip(ui, status_name(&st), status_color(&st));
            let model = if t.s("model").is_empty() {
                "modelo por defecto".to_string()
            } else {
                crate::gui::home::model_label(&t.s("model"))
            };
            let effort = t.s("effort");
            let model = if t.s("kind") == "codex" && !effort.is_empty() {
                let effort = match effort.as_str() {
                    "low" => "bajo",
                    "medium" => "medio",
                    "high" => "alto",
                    other => other,
                };
                format!("{model} · esfuerzo {effort}")
            } else {
                model
            };
            let total = num(t.f("tokens").unwrap_or(0.0));
            let usage = t.f("last_turn_tokens").map_or_else(
                || format!("{total} tokens acumulados"),
                |turn| format!("{} tokens en el último turno", num(turn)),
            );
            let tokens = ui.label(
                egui::RichText::new(format!("{} · {} · {usage}", t.s("brain_name"), model))
                    .font(body(12.0))
                    .color(INK2),
            );
            let cache =
                t.f("cached_tokens")
                    .or_else(|| t.f("cached"))
                    .map_or_else(String::new, |cached| {
                        let period = if t.b("cache_breakdown_partial", false) {
                            " desde la actualización"
                        } else {
                            ""
                        };
                        format!(" · {} tokens de entrada en caché{period}", num(cached))
                    });
            tokens.on_hover_text(format!("{total} tokens acumulados{cache}"));
            let dir = t
                .os("wt_path")
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| t.s("cwd"));
            if !dir.is_empty() {
                ui.label(faint(short_path(&dir))).on_hover_text(dir);
            }
        });
    };
    let run_actions = |app: &mut App, ui: &mut egui::Ui| {
        if st == "review" && primary(ui, "Aprobar").clicked() {
            app.post(&format!("api/tasks/{id}/approve"), json!({}), |r| {
                (r.os("result").unwrap_or_else(|| r.s("error")), RUN)
            });
        }
        if !t.s("wt_path").is_empty() && st != "done" && danger(ui, "Descartar").clicked() {
            app.ui.confirm_discard = Some(id.clone());
        }
        if quiet(ui, "Ver cambios").clicked() {
            let sup = app.sup.clone();
            let task_id = id.clone();
            let ctx = ui.ctx().clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let text = sup.tasks.diff(&task_id);
                let _ = tx.send(text);
                ctx.request_repaint();
            });
            app.ui.diff = Some(DiffView::Loading {
                task_id: id.clone(),
                rx,
            });
        }
    };
    let mut interrupt_now = false;
    if avail_w < 760.0 {
        ui.vertical(|ui| {
            ui.set_max_width(avail_w);
            header_meta(ui);
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if !live {
                    run_actions(app, ui);
                } else if interruptible
                    && danger(ui, "Interrumpir")
                        .on_hover_text("Detener este turno (Esc)")
                        .clicked()
                {
                    interrupt_now = true;
                }
            });
        });
    } else {
        let actions_w = 270.0_f32.min(avail_w * 0.36);
        let meta_w = (avail_w - actions_w - 12.0).max(260.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(meta_w, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(meta_w);
                    header_meta(ui);
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 40.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    if !live {
                        run_actions(app, ui);
                    } else if interruptible
                        && danger(ui, "Interrumpir")
                            .on_hover_text("Detener este turno (Esc)")
                            .clicked()
                    {
                        interrupt_now = true;
                    }
                },
            );
        });
    }
    ui.add_space(8.0);

    // Conversation.
    let composer_size_id = ui.make_persistent_id(("reply_height", &id));
    let composer_h = ui
        .data(|d| d.get_temp::<f32>(composer_size_id))
        .unwrap_or(90.0);
    let header_h = ui.next_widget_position().y - header_top;
    let message_h = (h - header_h - composer_h - 12.0).max(96.0);
    let max_w = ui.available_width().min(760.0);
    let mut err_action: Option<ErrorAction> = None;
    let mut interrupt_inline = false;
    egui::ScrollArea::vertical()
        .id_salt(("thread", id.as_str()))
        .max_height(message_h)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            reading_column(ui, max_w, |ui| {
                ui.add_space(6.0);
                let mut attachments = HashSet::new();
                let prompt = t.s("prompt");
                let (text, files) = user_message(t, &prompt);
                bubble_user(ui, text, &files);
                attachments.extend(files);
                if t.s("kind") == "parallel" {
                    parallel_panel(app, ui, snap, &id, &mut goto);
                }
                let evs = &app.ui.events;
                let codex = t.s("kind") == "codex";
                let active_since = evs
                    .iter()
                    .rposition(|ev| ev.s("type") == "status" && ev.s("status") == "running");
                let latest_error = evs.iter().rposition(|ev| ev.s("type") == "error");
                let flow = build_flow(evs, codex, st == "running", active_since);
                let last_activity_pos = flow
                    .iter()
                    .rposition(|item| matches!(item, FlowItem::Activity(_)));
                for (pos, item) in flow.iter().enumerate() {
                    match item {
                        FlowItem::User { text, files } => {
                            bubble_user(ui, text, files);
                            attachments.extend(files.iter().cloned());
                        }
                        FlowItem::Prose(buf) => {
                            if !buf.trim().is_empty() {
                                crate::gui::response::markdown(ui, buf);
                                ui.add_space(8.0);
                            }
                        }
                        FlowItem::Activity(block) => {
                            let open_live =
                                live && block.current_turn && Some(pos) == last_activity_pos;
                            render_activity_block(ui, &id, block, open_live);
                        }
                        FlowItem::Status {
                            status,
                            escalated_to,
                        } => {
                            if status == "escalated" {
                                ui.horizontal(|ui| {
                                    ui.label(faint("Pasó a un cerebro más capaz:"));
                                    if ui.link(escalated_to).clicked() {
                                        goto = Some(escalated_to.clone());
                                    }
                                });
                            } else if !matches!(status.as_str(), "queued" | "running" | "stopping")
                            {
                                divider(ui, status_name(status));
                            }
                        }
                        FlowItem::Info(text) => divider(ui, text),
                        FlowItem::Verify(ev) => verify_card(ui, ev),
                        FlowItem::Error { index, text } => {
                            if let Some(action) = error_card(
                                ui,
                                text,
                                t,
                                snap,
                                st == "failed" && Some(*index) == latest_error,
                            ) {
                                err_action = Some(action);
                            }
                        }
                    }
                }
                // Files the agent made or mentioned: images inline, everything else as a card.
                let base = t
                    .os("wt_path")
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| t.s("cwd"));
                let found = files_in(evs, &base, t.f("created").unwrap_or(0.0), &attachments);
                if !found.is_empty() {
                    files_gallery(ui, &found);
                }
                if live {
                    let status_line = live_status_label(&st, &flow);
                    let elapsed = active_since
                        .and_then(|i| evs.get(i))
                        .and_then(|ev| ev.f("ts"))
                        .or_else(|| t.f("updated"))
                        .map(|ts| (now() - ts).max(0.0))
                        .unwrap_or(0.0);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(status_line)
                                    .font(medium(12.5))
                                    .color(INK2),
                            )
                            .truncate(),
                        );
                        if elapsed >= 1.0 {
                            ui.label(faint(format_elapsed(elapsed)));
                        }
                        if interruptible {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if quiet(ui, "Interrumpir")
                                        .on_hover_text("Detener este turno (Esc)")
                                        .clicked()
                                    {
                                        interrupt_inline = true;
                                    }
                                },
                            );
                        }
                    });
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(120));
                }
            });
        });
    if interrupt_now || interrupt_inline {
        if app
            .ui
            .queued_follow_up
            .as_ref()
            .is_some_and(|q| q.task_id == id)
        {
            app.ui.queued_follow_up = None;
        }
        app.ui.focus_composer = true;
        ui.ctx().request_repaint();
        app.post(&format!("api/tasks/{id}/stop"), json!({}), |r| {
            (r.os("error").unwrap_or_default(), ALARM)
        });
    }
    if let Some(action) = err_action {
        match action {
            ErrorAction::Retry(brain) => submit_chat(
                app,
                &format!("api/tasks/{id}/retry"),
                json!({"brain": brain}),
            ),
            ErrorAction::OpenBrains => app.page = Page::Brains,
            ErrorAction::OpenUsage => app.page = Page::Usage,
            ErrorAction::NewChat => {
                app.ui.sel_task.clear();
                app.ui.focus_composer = true;
            }
        }
    }
    ui.add_space(6.0);
    let composer_top = ui.next_widget_position().y;
    reading_column(ui, max_w, |ui| {
        composer_box(app, ui, snap, Some((&id, &st)));
    });
    let measured_h = ui.next_widget_position().y - composer_top;
    if (measured_h - composer_h).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(composer_size_id, measured_h));
        ui.ctx().request_repaint();
    }
    if let Some(g) = goto {
        app.ui.sel_task = g;
    }
}

fn brain_color_for(snap: &Value, brain_id: &str) -> Color32 {
    if brain_id.is_empty() || brain_id == "parallel" {
        return INFO;
    }
    snap.arr("brains")
        .iter()
        .position(|b| b.s("id") == brain_id)
        .map(series)
        .unwrap_or(MASTER)
}

/// Coherent navigation strip connecting the main orchestrator chat with all its agent subchats.
fn subchat_strip(
    app: &App,
    ui: &mut egui::Ui,
    snap: &Value,
    root_id: &str,
    current_id: &str,
    goto: &mut Option<String>,
) {
    let Some(run) = app.sup.parallel.get(root_id) else {
        if current_id != root_id {
            ui.horizontal(|ui| {
                if quiet(ui, "← Volver al plan paralelo").clicked() {
                    *goto = Some(root_id.to_string());
                }
            });
        }
        return;
    };
    let mut branches = run.arr("branches");
    branches.sort_by(|a, b| {
        let wa = a.get("wave").and_then(|v| v.as_u64()).unwrap_or(0);
        let wb = b.get("wave").and_then(|v| v.as_u64()).unwrap_or(0);
        let ta = a.get("tier").and_then(|v| v.as_u64()).unwrap_or(2);
        let tb = b.get("tier").and_then(|v| v.as_u64()).unwrap_or(2);
        wa.cmp(&wb)
            .then_with(|| ta.cmp(&tb))
            .then_with(|| a.s("id").cmp(&b.s("id")))
    });
    if branches.is_empty() && current_id == root_id {
        return;
    }
    let any_live = branches.iter().any(|b| b.s("status") == "running")
        || matches!(
            run.s("status").as_str(),
            "planning" | "running" | "synthesizing"
        );

    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(
            1.0,
            if any_live {
                INFO.gamma_multiply(0.4)
            } else {
                RULE
            },
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(5.0, 4.0);
                let is_root = current_id == root_id;
                let root_btn = egui::Button::new(
                    egui::RichText::new("★ Principal")
                        .font(if is_root { medium(11.5) } else { body(11.5) })
                        .color(if is_root {
                            Color32::from_rgb(0x1C, 0x16, 0x0A)
                        } else {
                            INK2
                        }),
                )
                .fill(if is_root { MASTER } else { RAISED })
                .stroke(Stroke::new(1.0, if is_root { MASTER } else { RULE }))
                .corner_radius(CornerRadius::same(10));
                if ui
                    .add(root_btn)
                    .on_hover_text("Ir al chat principal de la orquestación")
                    .clicked()
                    && !is_root
                {
                    *goto = Some(root_id.to_string());
                }

                for (idx, b) in branches.iter().enumerate() {
                    let tid = b.s("task_id");
                    let st = b.s("status");
                    let is_sel = !tid.is_empty() && tid == current_id;
                    let is_run = st == "running";
                    let accent = brain_color_for(snap, &b.s("brain"));
                    let wave = b.get("wave").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
                    let bname = b.s("brain_name");
                    let short_brain = bname.split_whitespace().next().unwrap_or("cola");
                    let short_title: String = b.s("title").chars().take(18).collect();
                    let pill_txt = format!("O{wave} · {short_title}");
                    let fill = if is_sel {
                        accent.gamma_multiply(0.24)
                    } else {
                        RAISED
                    };
                    let stroke_col = if is_sel {
                        accent
                    } else if is_run {
                        RUN.gamma_multiply(0.65)
                    } else {
                        RULE
                    };
                    let resp = ui
                        .add_enabled(
                            !tid.is_empty(),
                            egui::Button::new(
                                egui::RichText::new(format!("   {pill_txt}"))
                                    .font(if is_sel { medium(11.0) } else { body(11.0) })
                                    .color(if is_sel {
                                        INK
                                    } else if tid.is_empty() {
                                        INK3
                                    } else {
                                        INK2
                                    }),
                            )
                            .fill(fill)
                            .stroke(Stroke::new(1.0, stroke_col))
                            .corner_radius(CornerRadius::same(10)),
                        )
                        .on_hover_text(format!(
                            "Subchat {} · Ola {wave} · {}\nEstado: {}",
                            b.s("title"),
                            short_brain,
                            status_name(&st)
                        ));
                    let dot_c = Pos2::new(resp.rect.min.x + 10.0, resp.rect.center().y);
                    let dot_col = match st.as_str() {
                        "running" => RUN,
                        "done" => MASTER,
                        "failed" => ALARM,
                        "stopped" => ATTN,
                        _ => INK3,
                    };
                    if is_run {
                        let pulse = ((now() * 3.6 + idx as f64 * 0.8).sin() * 0.5 + 0.5) as f32;
                        ui.painter().circle_stroke(
                            dot_c,
                            3.0 + pulse * 2.6,
                            Stroke::new(1.0, RUN.gamma_multiply(0.75 - 0.5 * pulse)),
                        );
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(28));
                    }
                    ui.painter().circle_filled(dot_c, 2.8, dot_col);
                    if resp.clicked() && !tid.is_empty() && !is_sel {
                        *goto = Some(tid);
                    }
                }
            });
        });
}

/// Live animated DAG / Wave orchestration card for a parallel run inside the conversation thread.
fn parallel_panel(
    app: &App,
    ui: &mut egui::Ui,
    snap: &Value,
    run_id: &str,
    goto: &mut Option<String>,
) {
    let Some(run) = app.sup.parallel.get(run_id) else {
        return;
    };
    let mut branches = run.arr("branches");
    branches.sort_by(|a, b| {
        let wa = a.get("wave").and_then(|v| v.as_u64()).unwrap_or(0);
        let wb = b.get("wave").and_then(|v| v.as_u64()).unwrap_or(0);
        let ta = a.get("tier").and_then(|v| v.as_u64()).unwrap_or(2);
        let tb = b.get("tier").and_then(|v| v.as_u64()).unwrap_or(2);
        wa.cmp(&wb)
            .then_with(|| ta.cmp(&tb))
            .then_with(|| a.s("id").cmp(&b.s("id")))
    });
    let strategy = match run.s("strategy").as_str() {
        "dag" => "DAG paralelo",
        "fanout" => "Mesa paralela",
        "direct" => "Directo",
        _ => "Planificando",
    };
    let director = run.s("director_name");
    let run_status = run.s("status");
    let run_live = matches!(run_status.as_str(), "planning" | "running" | "synthesizing");
    let done_n = branches.iter().filter(|b| b.s("status") == "done").count();
    let active_n = branches
        .iter()
        .filter(|b| b.s("status") == "running")
        .count();
    let total_n = branches.len();
    let target_prog = if total_n == 0 {
        if run_status == "planning" {
            0.12
        } else {
            0.0
        }
    } else {
        let base = (done_n as f32 + active_n as f32 * 0.35) / total_n as f32;
        if run_status == "synthesizing" {
            base.max(0.88)
        } else if run_status == "done" || run_status == "review" {
            1.0
        } else {
            base.clamp(0.05, 0.96)
        }
    };
    let smooth_prog =
        ui.ctx()
            .animate_value_with_time(egui::Id::unique(("par_prog", run_id)), target_prog, 0.35);
    let t_now = now();
    if run_live || active_n > 0 {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(25));
    }

    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(
            1.0,
            if run_live {
                INFO.gamma_multiply(0.5)
            } else {
                RULE
            },
        ))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                if run_live {
                    let (rr, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    let pulse = ((t_now * 3.2).sin() * 0.5 + 0.5) as f32;
                    ui.painter().circle_stroke(
                        rr.center(),
                        4.0 + pulse * 2.8,
                        Stroke::new(1.2, RUN.gamma_multiply(0.8 - 0.55 * pulse)),
                    );
                    ui.painter().circle_filled(rr.center(), 3.2, RUN);
                }
                ui.label(
                    egui::RichText::new("Orquestación multi-agente")
                        .font(cond(15.0))
                        .color(INK),
                );
                chip(ui, strategy, INFO);
                if !director.is_empty() {
                    chip(ui, &format!("Dirige: {director}"), MASTER);
                }
                if total_n > 0 {
                    let live_txt = if active_n > 0 {
                        format!("{done_n}/{total_n} listas · {active_n} activas")
                    } else {
                        format!("{done_n}/{total_n} subchats")
                    };
                    ui.label(faint(live_txt));
                }
                let toks = run.f("total_tokens").unwrap_or(0.0);
                if toks > 0.0 {
                    ui.label(faint(format!("· {} tokens", num(toks))));
                }
            });

            // Smooth animated progress track with travelling light shimmer while active.
            ui.add_space(6.0);
            let (bar_r, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 5.0), Sense::hover());
            let bp = ui.painter();
            bp.rect_filled(bar_r, CornerRadius::same(3), RAISED2);
            let fill_w = (bar_r.width() * smooth_prog.clamp(0.0, 1.0)).max(if smooth_prog > 0.0 {
                6.0
            } else {
                0.0
            });
            let fill_r = egui::Rect::from_min_size(bar_r.min, Vec2::new(fill_w, bar_r.height()));
            let bar_col = if run_status == "failed" {
                ALARM
            } else if run_status == "done" || run_status == "review" {
                MASTER
            } else {
                RUN
            };
            bp.rect_filled(fill_r, CornerRadius::same(3), bar_col);
            if run_live && fill_w > 18.0 {
                let sweep = ((t_now * 0.9) % 1.0) as f32;
                let sx = fill_r.min.x + (fill_w - 18.0) * sweep;
                let glow_r = egui::Rect::from_min_size(
                    Pos2::new(sx, fill_r.min.y),
                    Vec2::new(22.0, fill_r.height()),
                );
                bp.rect_filled(
                    glow_r,
                    CornerRadius::same(3),
                    Color32::from_rgba_unmultiplied(255, 255, 255, 95),
                );
            }

            if branches.is_empty() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(faint(
                        "Construyendo grafo de subtareas y asignando cerebros según cuota…",
                    ));
                });
                return;
            }

            // Animated Wave flow pipeline (Ola 1 ──▸ Ola 2 ──▸ Síntesis).
            let max_wave = branches
                .iter()
                .filter_map(|b| b.get("wave").and_then(|v| v.as_u64()))
                .max()
                .unwrap_or(0) as usize;
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for w in 0..=max_wave {
                    let wave_branches: Vec<&Value> = branches
                        .iter()
                        .filter(|b| {
                            b.get("wave").and_then(|v| v.as_u64()).unwrap_or(0) as usize == w
                        })
                        .collect();
                    let w_running = wave_branches.iter().any(|b| b.s("status") == "running");
                    let w_done = !wave_branches.is_empty()
                        && wave_branches.iter().all(|b| b.s("status") == "done");
                    let w_col = if w_running {
                        RUN
                    } else if w_done {
                        MASTER
                    } else {
                        INK3
                    };
                    let lbl = format!("Ola {} ({})", w + 1, wave_branches.len());
                    chip(ui, &lbl, w_col);
                    // Animated flow connector to next stage.
                    let (cr, _) = ui.allocate_exact_size(Vec2::new(26.0, 18.0), Sense::hover());
                    let cy = cr.center().y;
                    let cp = ui.painter();
                    cp.line_segment(
                        [Pos2::new(cr.min.x + 2.0, cy), Pos2::new(cr.max.x - 4.0, cy)],
                        Stroke::new(1.2, w_col.gamma_multiply(0.55)),
                    );
                    if w_running || (w_done && run_live) {
                        let flow_t = ((t_now * 1.4 + w as f64 * 0.35) % 1.0) as f32;
                        let fx = egui::lerp((cr.min.x + 3.0)..=(cr.max.x - 5.0), flow_t);
                        cp.circle_filled(
                            Pos2::new(fx, cy),
                            2.5,
                            if w_running { RUN } else { MASTER },
                        );
                    }
                }
                let synth_col = if run_status == "synthesizing" {
                    MASTER
                } else if matches!(run_status.as_str(), "done" | "review") {
                    RUN
                } else {
                    INK3
                };
                chip(ui, "Síntesis", synth_col);
            });

            ui.add_space(8.0);
            for (idx, b) in branches.iter().enumerate() {
                let st = b.s("status");
                let is_running = st == "running";
                let (st_label, st_col) = match st.as_str() {
                    "running" => ("trabajando", RUN),
                    "done" => ("lista", MASTER),
                    "failed" => ("falló", ALARM),
                    "stopped" => ("detenida", ATTN),
                    _ => ("esperando", INK3),
                };
                let role_raw = b.s("role");
                let role_lbl = match role_raw.as_str() {
                    "explore" => "explora",
                    "analyze" => "analiza",
                    "review" => "revisa",
                    "implement" => "implementa",
                    "synthesize" => "síntesis",
                    other => other,
                };
                let tier = b.get("tier").and_then(|v| v.as_u64()).unwrap_or(2);
                let wave = b.get("wave").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
                let tid = b.s("task_id");
                let summary = b.s("summary");
                let err = b.s("error");
                let brain_col = brain_color_for(snap, &b.s("brain"));

                // Smooth fade/slide-in entrance per branch card.
                let appear = ui.ctx().animate_bool_with_time(
                    egui::Id::unique(("par_branch_in", run_id, b.s("id"))),
                    true,
                    0.26,
                );
                let pulse = if is_running {
                    ((t_now * 3.4 + idx as f64 * 0.7).sin() * 0.5 + 0.5) as f32
                } else {
                    0.0
                };

                let frame_resp = egui::Frame::new()
                    .fill(RAISED.gamma_multiply(0.85 + 0.15 * appear))
                    .stroke(Stroke::new(
                        if is_running { 1.3 } else { 1.0 },
                        if is_running {
                            brain_col.gamma_multiply(0.45 + 0.35 * pulse)
                        } else {
                            RULE
                        },
                    ))
                    .corner_radius(CornerRadius::same(R))
                    .inner_margin(Margin {
                        left: 13,
                        right: 10,
                        top: 7,
                        bottom: 7,
                    })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            let (dr, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                            if is_running {
                                ui.painter().circle_stroke(
                                    dr.center(),
                                    3.6 + pulse * 3.2,
                                    Stroke::new(1.2, st_col.gamma_multiply(0.75 - 0.55 * pulse)),
                                );
                            }
                            ui.painter().circle_filled(dr.center(), 3.6, st_col);
                            ui.label(
                                egui::RichText::new(b.s("title"))
                                    .font(medium(13.0))
                                    .color(INK),
                            );
                            ui.label(faint(format!("Ola {wave} · T{tier} · {role_lbl}")));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if !tid.is_empty() && quiet(ui, "Ver subchat").clicked() {
                                        *goto = Some(tid.clone());
                                    }
                                    ui.label(
                                        egui::RichText::new(st_label)
                                            .font(body(11.5))
                                            .color(st_col),
                                    );
                                },
                            );
                        });
                        ui.horizontal_wrapped(|ui| {
                            let bname = b.s("brain_name");
                            if !bname.is_empty() {
                                let model = b.s("model");
                                let who = if model.is_empty() {
                                    bname
                                } else {
                                    format!("{bname} ({})", crate::gui::home::model_label(&model))
                                };
                                ui.label(
                                    egui::RichText::new(who).font(body(11.5)).color(brain_col),
                                );
                            } else {
                                let deps: Vec<String> = b
                                    .arr("deps")
                                    .into_iter()
                                    .filter_map(|d| d.as_str().map(String::from))
                                    .collect();
                                if !deps.is_empty() {
                                    ui.label(faint(format!("Espera a: {}", deps.join(", "))));
                                } else {
                                    ui.label(faint("Asignando cerebro disponible…"));
                                }
                            }
                            let bt = b.f("tokens").unwrap_or(0.0);
                            if bt > 0.0 {
                                ui.label(faint(format!("· {} tok", num(bt))));
                            }
                            if b.b("worktree", false) {
                                ui.label(faint("· copia aislada"));
                            }
                        });
                        if !err.is_empty() {
                            ui.label(egui::RichText::new(err).font(body(11.5)).color(ALARM));
                        } else if !summary.is_empty() {
                            egui::CollapsingHeader::new(
                                egui::RichText::new("Ver aporte de este subchat")
                                    .font(body(11.5))
                                    .color(INK3),
                            )
                            .id_salt(("par_sum", run_id, b.s("id")))
                            .show(ui, |ui| {
                                crate::gui::response::markdown(ui, &summary);
                            });
                        }
                    });
                // Left identity bar + subtle live shimmer along top edge of active worker card.
                let fr = frame_resp.response.rect;
                let left_bar =
                    egui::Rect::from_min_max(fr.min, Pos2::new(fr.min.x + 3.0, fr.max.y));
                ui.painter().rect_filled(
                    left_bar,
                    CornerRadius {
                        nw: R,
                        sw: R,
                        ne: 0,
                        se: 0,
                    },
                    brain_col.gamma_multiply(if is_running { 0.95 } else { 0.55 }),
                );
                if is_running && fr.width() > 36.0 {
                    let sweep = ((t_now * 0.75 + idx as f64 * 0.27) % 1.0) as f32;
                    let sx = fr.min.x + 6.0 + (fr.width() - 42.0) * sweep;
                    ui.painter().line_segment(
                        [
                            Pos2::new(sx, fr.min.y + 0.5),
                            Pos2::new(sx + 30.0, fr.min.y + 0.5),
                        ],
                        Stroke::new(2.0, brain_col.gamma_multiply(0.85)),
                    );
                }
                ui.add_space(5.0);
            }
        });
    ui.add_space(10.0);
}

pub(super) fn reading_column(
    ui: &mut egui::Ui,
    max_width: f32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let width = ui.available_width().min(max_width).max(0.0);
    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(width);
                add_contents(ui);
            },
        );
    });
}

pub(super) const IMG_EXT: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
const FILE_EXT: [&str; 24] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "mp4", "mov", "webm", "mkv", "avi", "mp3",
    "wav", "pdf", "docx", "xlsx", "pptx", "csv", "html", "zip", "stl", "step", "dwg",
];

/// Existing files named in the conversation (absolute, or relative to the task folder).
fn files_in(evs: &[Value], base: &str, since: f64, attachments: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut out = vec![];
    for ev in evs {
        if ev.s("type") == "text"
            && ev["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("Tú: "))
        {
            continue;
        }
        let txt = ev.to_string().replace("\\\\", "\\");
        for raw in txt.split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | '|'
                )
        }) {
            let w = raw.trim_end_matches(['.', ':', ';']);
            let Some(ext) = w.rsplit('.').next().map(|e| e.to_lowercase()) else {
                continue;
            };
            if w.len() < 4 || !FILE_EXT.contains(&ext.as_str()) || w.starts_with("http") {
                continue;
            }
            let p = PathBuf::from(w);
            let p = if p.is_absolute() {
                p
            } else if !base.is_empty() {
                Path::new(base).join(w)
            } else {
                continue;
            };
            if attachments.contains(&p) {
                continue;
            }
            // Only what this conversation produced: modified after it started.
            let fresh = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64() >= since - 60.0)
                .unwrap_or(false);
            if p.is_file() && fresh && !out.contains(&p) {
                out.push(p);
            }
            if out.len() >= 12 {
                return out;
            }
        }
    }
    out
}

fn files_gallery(ui: &mut egui::Ui, files: &[std::path::PathBuf]) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new("Archivos")
            .font(medium(12.0))
            .color(INK3),
    );
    ui.add_space(4.0);
    for f in files {
        let ext = f
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        egui::Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, RULE))
            .corner_radius(CornerRadius::same(R))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(560.0));
                if IMG_EXT.contains(&ext.as_str()) {
                    let uri = file_uri(&f.display().to_string());
                    ui.add(
                        egui::Image::new(uri)
                            .max_width(ui.available_width().min(560.0))
                            .max_height(360.0)
                            .corner_radius(CornerRadius::same(6)),
                    );
                    ui.add_space(4.0);
                }
                ui.horizontal_wrapped(|ui| {
                    let kind = match ext.as_str() {
                        "mp4" | "mov" | "webm" | "mkv" | "avi" => "video",
                        "mp3" | "wav" => "audio",
                        e if IMG_EXT.contains(&e) || e == "svg" => "imagen",
                        _ => "archivo",
                    };
                    ui.add(
                        egui::Label::new(egui::RichText::new(&name).font(medium(13.0)).color(INK))
                            .truncate(),
                    );
                    ui.label(faint(kind));
                    if quiet(
                        ui,
                        if kind == "video" || kind == "audio" {
                            "Reproducir"
                        } else {
                            "Abrir"
                        },
                    )
                    .clicked()
                    {
                        crate::app::open_browser(&f.display().to_string());
                    }
                    if quiet(ui, "Carpeta").clicked() {
                        if let Some(d) = f.parent() {
                            crate::app::open_browser(&d.display().to_string());
                        }
                    }
                });
            });
        ui.add_space(6.0);
    }
}

fn user_message<'a>(record: &'a Value, legacy_text: &'a str) -> (&'a str, Vec<PathBuf>) {
    if let Some(message) = record.get("user_message") {
        let text = message["text"].as_str().unwrap_or("");
        let files = message
            .arr("files")
            .iter()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect();
        return (text, files);
    }

    // Older tasks stored the attachment block directly in the prompt or event text.
    let mut offset = 0;
    for line in legacy_text.split_inclusive('\n') {
        if matches!(
            line.trim_end(),
            "Archivos adjuntos (revísalos antes de empezar):"
                | "Archivos adjuntos (ábrelos desde estas rutas):"
        ) {
            let files: Vec<_> = legacy_text[offset + line.len()..]
                .lines()
                .map_while(|line| line.strip_prefix("- "))
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect();
            if !files.is_empty() && files.iter().all(|path| path.is_absolute()) {
                return (legacy_text[..offset].trim_end(), files);
            }
        }
        offset += line.len();
    }
    (legacy_text, vec![])
}

pub(super) fn bubble_user(ui: &mut egui::Ui, text: &str, files: &[PathBuf]) {
    let width = (ui.available_width() * 0.82).min(560.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        egui::Frame::new()
            .fill(RAISED2)
            .corner_radius(CornerRadius {
                nw: 14,
                ne: 14,
                sw: 14,
                se: 4,
            })
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    ui.set_max_width((width - 28.0).max(1.0));
                    for file in files {
                        let name = file
                            .file_name()
                            .unwrap_or(file.as_os_str())
                            .to_string_lossy();
                        let path = file.to_string_lossy();
                        let ext = file
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let response = if IMG_EXT.contains(&ext.as_str()) && file.is_file() {
                            ui.add(
                                egui::Image::new(file_uri(&path))
                                    .fit_to_original_size(1.0)
                                    .max_width(ui.available_width().min(420.0))
                                    .max_height(240.0)
                                    .corner_radius(CornerRadius::same(8))
                                    .alt_text(name.as_ref())
                                    .sense(Sense::click()),
                            )
                        } else {
                            quiet(ui, name.as_ref())
                        };
                        if response.on_hover_text(name.as_ref()).clicked() {
                            crate::app::open_browser(&path);
                        }
                    }
                    if !text.is_empty() {
                        ui.add(
                            egui::Label::new(egui::RichText::new(text).font(body(14.0)).color(INK))
                                .wrap(),
                        );
                    }
                });
            });
    });
    ui.add_space(10.0);
}

fn tool_key(event: &Value) -> Option<(&str, &str)> {
    Some((
        event["turn_id"].as_str().unwrap_or_default(),
        event["tool_id"].as_str()?,
    ))
}

struct ActivityBlock<'a> {
    start_idx: usize,
    current_turn: bool,
    steps: Vec<FlowStep<'a>>,
}

enum FlowStep<'a> {
    Thinking {
        idx: usize,
        text: String,
        active: bool,
    },
    Tool {
        idx: usize,
        ev: &'a Value,
        result: Option<&'a Value>,
        codex: bool,
        running: bool,
    },
    Subagent {
        text: String,
    },
}

enum FlowItem<'a> {
    User {
        text: String,
        files: Vec<PathBuf>,
    },
    Prose(String),
    Activity(ActivityBlock<'a>),
    Status {
        status: String,
        escalated_to: String,
    },
    Info(String),
    Verify(&'a Value),
    Error {
        index: usize,
        text: String,
    },
}

fn build_flow<'a>(
    evs: &'a [Value],
    codex: bool,
    is_running: bool,
    active_since: Option<usize>,
) -> Vec<FlowItem<'a>> {
    let results: HashMap<(&str, &str), &Value> = evs
        .iter()
        .filter(|ev| ev.s("type") == "tool_result")
        .filter_map(|ev| tool_key(ev).map(|key| (key, ev)))
        .collect();
    let calls: HashSet<(&str, &str)> = evs
        .iter()
        .filter(|ev| ev.s("type") == "tool_use")
        .filter_map(tool_key)
        .collect();
    let last_meaningful = evs.iter().rposition(|ev| {
        !matches!(
            ev.s("type").as_str(),
            "usage" | "parallel_plan" | "parallel_branch"
        )
    });

    let mut items: Vec<FlowItem<'a>> = Vec::new();
    let mut prose = String::new();
    let mut block: Option<ActivityBlock<'a>> = None;
    let mut seen_tool_keys: HashMap<(String, String), (usize, usize)> = HashMap::new();

    let flush_prose = |items: &mut Vec<FlowItem<'a>>, prose: &mut String| {
        if !prose.trim().is_empty() {
            items.push(FlowItem::Prose(std::mem::take(prose)));
        } else {
            prose.clear();
        }
    };
    let flush_block = |items: &mut Vec<FlowItem<'a>>, block: &mut Option<ActivityBlock<'a>>| {
        if let Some(b) = block.take() {
            if !b.steps.is_empty() {
                items.push(FlowItem::Activity(b));
            }
        }
    };

    for (index, ev) in evs.iter().enumerate() {
        let ty = ev.s("type");
        let in_current = active_since.is_some_and(|start| index > start);
        match ty.as_str() {
            "text" => {
                let tx = ev.s("text");
                if let Some(user) = tx.strip_prefix("Tú: ") {
                    flush_block(&mut items, &mut block);
                    flush_prose(&mut items, &mut prose);
                    let (text, files) = user_message(ev, user);
                    items.push(FlowItem::User {
                        text: text.to_string(),
                        files,
                    });
                } else {
                    flush_block(&mut items, &mut block);
                    prose.push_str(&tx);
                    if !ev.b("delta", false) && !tx.ends_with('\n') {
                        prose.push('\n');
                    }
                }
            }
            "thinking" => {
                flush_prose(&mut items, &mut prose);
                let tx = ev.s("text");
                if tx.is_empty() {
                    continue;
                }
                let active = is_running && in_current && Some(index) == last_meaningful;
                let b = block.get_or_insert_with(|| ActivityBlock {
                    start_idx: index,
                    current_turn: in_current,
                    steps: Vec::new(),
                });
                b.current_turn |= in_current;
                if let Some(FlowStep::Thinking {
                    text,
                    active: prev_active,
                    ..
                }) = b.steps.last_mut()
                {
                    if !ev.b("delta", false) && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&tx);
                    *prev_active = active;
                } else {
                    b.steps.push(FlowStep::Thinking {
                        idx: index,
                        text: tx,
                        active,
                    });
                }
            }
            "tool_use" | "tool_result" => {
                flush_prose(&mut items, &mut prose);
                let key = tool_key(ev);
                if ty == "tool_result" && key.is_some_and(|k| calls.contains(&k)) {
                    continue;
                }
                let result = if ty == "tool_use" {
                    key.and_then(|k| results.get(&k)).copied()
                } else {
                    Some(ev)
                };
                if ty == "tool_use" {
                    if let Some((turn_k, tool_k)) = key {
                        let owned = (turn_k.to_string(), tool_k.to_string());
                        if let Some(&(item_i, step_i)) = seen_tool_keys.get(&owned) {
                            let target_step = if item_i == usize::MAX {
                                block.as_mut().and_then(|b| b.steps.get_mut(step_i))
                            } else if let Some(FlowItem::Activity(b)) = items.get_mut(item_i) {
                                b.steps.get_mut(step_i)
                            } else {
                                None
                            };
                            if let Some(FlowStep::Tool {
                                ev: prev_ev,
                                result: prev_res,
                                ..
                            }) = target_step
                            {
                                let richer = ev
                                    .get("input")
                                    .and_then(|v| v.as_object())
                                    .is_some_and(|o| !o.is_empty());
                                if richer {
                                    *prev_ev = ev;
                                }
                                *prev_res = result;
                                continue;
                            }
                        }
                    }
                }
                let running = is_running
                    && in_current
                    && result.is_none()
                    && (key.is_some() || Some(index) == last_meaningful);
                let b = block.get_or_insert_with(|| ActivityBlock {
                    start_idx: index,
                    current_turn: in_current,
                    steps: Vec::new(),
                });
                b.current_turn |= in_current;
                let step_i = b.steps.len();
                b.steps.push(FlowStep::Tool {
                    idx: index,
                    ev,
                    result,
                    codex: codex && ty == "tool_use",
                    running,
                });
                if ty == "tool_use" {
                    if let Some((turn_k, tool_k)) = key {
                        seen_tool_keys.insert(
                            (turn_k.to_string(), tool_k.to_string()),
                            (usize::MAX, step_i),
                        );
                    }
                }
            }
            "subagent" => {
                flush_prose(&mut items, &mut prose);
                let tx = ev.s("text");
                if tx.is_empty() {
                    continue;
                }
                let b = block.get_or_insert_with(|| ActivityBlock {
                    start_idx: index,
                    current_turn: in_current,
                    steps: Vec::new(),
                });
                b.current_turn |= in_current;
                b.steps.push(FlowStep::Subagent { text: tx });
            }
            "usage" | "parallel_plan" | "parallel_branch" => {}
            "status" => {
                let s = ev.s("status");
                if s == "queued" || s == "running" || s == "stopping" {
                    continue;
                }
                flush_block(&mut items, &mut block);
                flush_prose(&mut items, &mut prose);
                items.push(FlowItem::Status {
                    status: s,
                    escalated_to: ev.s("to"),
                });
            }
            "info" => {
                flush_block(&mut items, &mut block);
                flush_prose(&mut items, &mut prose);
                items.push(FlowItem::Info(ev.s("text")));
            }
            "verify" => {
                flush_block(&mut items, &mut block);
                flush_prose(&mut items, &mut prose);
                items.push(FlowItem::Verify(ev));
            }
            "error" => {
                flush_block(&mut items, &mut block);
                flush_prose(&mut items, &mut prose);
                items.push(FlowItem::Error {
                    index,
                    text: ev.s("text"),
                });
            }
            _ => {}
        }
        if block.is_none() {
            if let Some(last_i) = items
                .iter()
                .rposition(|it| matches!(it, FlowItem::Activity(_)))
            {
                for v in seen_tool_keys.values_mut() {
                    if v.0 == usize::MAX {
                        v.0 = last_i;
                    }
                }
            }
        }
    }
    flush_block(&mut items, &mut block);
    flush_prose(&mut items, &mut prose);
    items
}

fn clip_one_line(s: &str, max_chars: usize) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let head: String = line.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn tool_action_kind(name: &str, has_command: bool) -> &'static str {
    match name {
        "Read" | "read_file" | "view_file" | "list_directory" => "Leer",
        "Edit" | "MultiEdit" | "edit_file" | "file_change" => "Editar",
        "Write" | "write_file" | "create_file" => "Escribir",
        "Bash" | "run_command" | "shell" | "command_execution" => "Ejecutar",
        "Grep" | "Glob" | "search" | "search_directory" | "find_file" => "Buscar",
        "WebFetch" | "WebSearch" | "web_search" | "search_web" | "read_url_content" => "Consultar",
        "Task" | "Agent" | "invoke_subagent" | "collab_agent_tool_call" => "Subagente",
        "todo_list" => "Actualizar plan",
        _ if has_command => "Ejecutar",
        _ => "Usar",
    }
}

fn tool_summary(ev: &Value, codex: bool) -> (String, Option<String>) {
    let name = ev.s("name");
    let input = ev.get("input").unwrap_or(&Value::Null);
    let codex_command = codex && input.as_object().is_some_and(|o| o.is_empty());
    let command = ["command", "CommandLine", "cmd"]
        .iter()
        .find_map(|k| input.os(k))
        .or_else(|| codex_command.then(|| name.clone()));
    let target = [
        "file_path",
        "Path",
        "path",
        "TargetFile",
        "pattern",
        "query",
        "url",
        "toolSummary",
        "summary",
        "description",
        "Role",
        "role",
    ]
    .iter()
    .find_map(|k| input.os(k))
    .or_else(|| {
        input
            .get("changes")
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                let first = arr.first().and_then(|x| x.os("path"))?;
                Some(if arr.len() > 1 {
                    format!("{} (+{})", short_path(&first), arr.len() - 1)
                } else {
                    short_path(&first)
                })
            })
    })
    .unwrap_or_default();
    let action = tool_action_kind(&name, command.is_some());
    let summary = if let Some(cmd) = &command {
        let preview = clip_one_line(cmd, 62);
        if preview.is_empty() {
            format!("{action} un comando")
        } else {
            format!("{action}: {preview}")
        }
    } else if !target.is_empty() {
        let shown = if target.contains('\\') || target.contains('/') {
            short_path(&target)
        } else {
            clip_one_line(&target, 62)
        };
        format!("{action} {shown}")
    } else if name.is_empty() {
        "Resultado de herramienta".into()
    } else if name == "todo_list" {
        action.into()
    } else {
        let label = input.os("tool").unwrap_or_else(|| name.clone());
        match action {
            "Leer" | "Editar" | "Escribir" | "Ejecutar" | "Buscar" | "Consultar" | "Subagente" => {
                format!("{action} ({label})")
            }
            _ => format!("{action} {label}"),
        }
    };
    (summary, command)
}

fn activity_header_text(block: &ActivityBlock<'_>, live_open: bool) -> (String, usize) {
    let mut reads = 0usize;
    let mut edits = 0usize;
    let mut cmds = 0usize;
    let mut searches = 0usize;
    let mut thinks = 0usize;
    let mut errors = 0usize;
    let mut active_label: Option<String> = None;

    for step in &block.steps {
        match step {
            FlowStep::Thinking { active, .. } => {
                thinks += 1;
                if *active {
                    active_label = Some("Razonando…".into());
                }
            }
            FlowStep::Tool {
                ev,
                result,
                codex,
                running,
                ..
            } => {
                if result.is_some_and(|r| r.b("error", false)) {
                    errors += 1;
                }
                let (sum, cmd) = tool_summary(ev, *codex);
                match tool_action_kind(&ev.s("name"), cmd.is_some()) {
                    "Leer" => reads += 1,
                    "Editar" | "Escribir" => edits += 1,
                    "Ejecutar" => cmds += 1,
                    "Buscar" | "Consultar" => searches += 1,
                    _ => {}
                }
                if *running {
                    active_label = Some(sum);
                }
            }
            FlowStep::Subagent { .. } => {}
        }
    }
    let total = block.steps.len();
    let count = if total == 1 {
        "1 paso".to_string()
    } else {
        format!("{total} pasos")
    };
    if live_open {
        if let Some(active) = active_label {
            return (format!("En curso ({count}) · {active}"), errors);
        }
        return (format!("Flujo en curso ({count})"), errors);
    }
    let mut parts: Vec<String> = Vec::new();
    if reads > 0 {
        parts.push(if reads == 1 {
            "1 lectura".into()
        } else {
            format!("{reads} lecturas")
        });
    }
    if searches > 0 {
        parts.push(if searches == 1 {
            "1 búsqueda".into()
        } else {
            format!("{searches} búsquedas")
        });
    }
    if cmds > 0 {
        parts.push(if cmds == 1 {
            "1 comando".into()
        } else {
            format!("{cmds} comandos")
        });
    }
    if edits > 0 {
        parts.push(if edits == 1 {
            "1 edición".into()
        } else {
            format!("{edits} ediciones")
        });
    }
    if thinks > 0 && parts.is_empty() {
        parts.push("razonamiento".into());
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(", "))
    };
    (format!("Actividad ({count}){detail}"), errors)
}

fn render_activity_block(
    ui: &mut egui::Ui,
    task_id: &str,
    block: &ActivityBlock<'_>,
    open_live: bool,
) {
    let (header_text, errors) = activity_header_text(block, open_live);
    let has_running = block.steps.iter().any(|s| {
        matches!(
            s,
            FlowStep::Tool { running: true, .. } | FlowStep::Thinking { active: true, .. }
        )
    });
    let state_id = ui.make_persistent_id(("activity_flow", task_id, block.start_idx, open_live));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        state_id,
        open_live,
    );
    let is_open = state.is_open();

    let border = if errors > 0 {
        ALARM.gamma_multiply(0.45)
    } else if has_running {
        RUN.gamma_multiply(0.45)
    } else {
        RULE
    };

    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut toggled = false;
            ui.horizontal(|ui| {
                let arrow = if is_open { "▾" } else { "▸" };
                if has_running {
                    ui.spinner();
                } else {
                    let dot_col = if errors > 0 { ALARM } else { RUN };
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 3.2, dot_col);
                }
                let lbl = ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{arrow}  {header_text}"))
                            .font(if open_live { medium(12.5) } else { body(12.5) })
                            .color(if open_live { INK } else { INK2 }),
                    )
                    .truncate()
                    .sense(Sense::click()),
                );
                if lbl
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    toggled = true;
                }
                if errors > 0 {
                    ui.label(
                        egui::RichText::new(if errors == 1 {
                            "1 error".into()
                        } else {
                            format!("{errors} errores")
                        })
                        .font(medium(11.0))
                        .color(ALARM),
                    );
                }
            });
            if toggled {
                state.toggle(ui);
            }
            state.show_body_unindented(ui, |ui| {
                ui.add_space(4.0);
                let top_y = ui.next_widget_position().y;
                let rail_x = ui.next_widget_position().x + 5.0;
                ui.indent(("flow_steps", task_id, block.start_idx), |ui| {
                    for step in &block.steps {
                        match step {
                            FlowStep::Thinking { idx, text, active } => {
                                thinking_row(ui, task_id, *idx, text, *active);
                            }
                            FlowStep::Tool {
                                idx,
                                ev,
                                result,
                                codex,
                                running,
                            } => {
                                tool_row(ui, task_id, *idx, ev, *result, *codex, *running);
                            }
                            FlowStep::Subagent { text, .. } => {
                                egui::Frame::new()
                                    .fill(FIELD)
                                    .stroke(Stroke::new(1.0, INFO.gamma_multiply(0.45)))
                                    .corner_radius(CornerRadius::same(6))
                                    .inner_margin(Margin::symmetric(9, 5))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            let (r, _) = ui.allocate_exact_size(
                                                Vec2::splat(10.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().circle_filled(r.center(), 3.2, INFO);
                                            ui.label(
                                                egui::RichText::new("Subchat · Subagente")
                                                    .font(medium(11.5))
                                                    .color(INFO),
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(text)
                                                        .font(body(12.0))
                                                        .color(INK2),
                                                )
                                                .truncate(),
                                            );
                                        });
                                    });
                                ui.add_space(3.0);
                            }
                        }
                    }
                });
                let bot_y = ui.next_widget_position().y - 4.0;
                if bot_y > top_y + 6.0 {
                    ui.painter().line_segment(
                        [Pos2::new(rail_x, top_y + 2.0), Pos2::new(rail_x, bot_y)],
                        Stroke::new(1.0, RULE),
                    );
                }
            });
        });
    ui.add_space(6.0);
}

fn thinking_row(ui: &mut egui::Ui, task_id: &str, idx: usize, text: &str, active: bool) {
    let preview = clip_one_line(text, 72);
    let label = if preview.is_empty() {
        if active {
            "Razonando…".into()
        } else {
            "Razonamiento".into()
        }
    } else {
        format!("Pensamiento: {preview}")
    };
    let id = ui.make_persistent_id(("step_think", task_id, idx));
    let mut clicked = false;
    let mut header =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                if active {
                    ui.spinner();
                } else {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 2.8, INFO);
                }
                clicked = ui
                    .add(
                        egui::Label::new(egui::RichText::new(label).font(body(12.0)).color(INK3))
                            .truncate()
                            .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked();
            });
    if clicked {
        header.toggle();
    }
    header.body(|ui| {
        egui::Frame::new()
            .fill(FIELD)
            .stroke(Stroke::new(1.0, RULE))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(text.trim())
                            .font(body(12.0))
                            .color(INK2),
                    )
                    .wrap()
                    .selectable(true),
                );
            });
    });
}

fn live_status_label(st: &str, flow: &[FlowItem<'_>]) -> String {
    match st {
        "queued" => return "Esperando turno…".into(),
        "planning" => return "Descomponiendo plan paralelo…".into(),
        "synthesizing" => return "El Director está sintetizando las ramas…".into(),
        "verifying" => return "Comprobando el resultado…".into(),
        "stopping" => return "Interrumpiendo…".into(),
        _ => {}
    }
    if let Some(last) = flow.last() {
        match last {
            FlowItem::Prose(_) => return "Escribiendo respuesta…".into(),
            FlowItem::Activity(block) => {
                if let Some(step) = block.steps.last() {
                    return match step {
                        FlowStep::Thinking { .. } => "Razonando…".into(),
                        FlowStep::Tool { ev, codex, .. } => {
                            let (sum, _) = tool_summary(ev, *codex);
                            format!("{sum}…")
                        }
                        FlowStep::Subagent { text, .. } => {
                            format!("Subagente: {}…", clip_one_line(text, 48))
                        }
                    };
                }
            }
            _ => {}
        }
    }
    "Trabajando…".into()
}

fn format_elapsed(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s < 60 {
        format!("{s} s")
    } else {
        format!("{} min {:02} s", s / 60, s % 60)
    }
}

/// Summary stays short. The expanded row shows the full command or tool input.
/// Subagent invocations render as coherent inline Subchat cards with animated live state.
fn tool_row(
    ui: &mut egui::Ui,
    task_id: &str,
    idx: usize,
    ev: &Value,
    result: Option<&Value>,
    codex: bool,
    running: bool,
) {
    let name = ev.s("name");
    let input = ev.get("input").unwrap_or(&Value::Null);
    let codex_command = codex && input.as_object().is_some_and(|o| o.is_empty());
    let (summary, command) = tool_summary(ev, codex);
    let is_subagent = tool_action_kind(&name, command.is_some()) == "Subagente";

    let id = ui.make_persistent_id(("step_tool", task_id, idx));
    if is_subagent {
        let role = ["Role", "role", "subagent_type", "TypeName", "description"]
            .iter()
            .find_map(|k| input.os(k))
            .unwrap_or_else(|| "Subagente".into());
        let prompt = ["Prompt", "prompt", "task", "description"]
            .iter()
            .find_map(|k| input.os(k))
            .unwrap_or_default();
        let has_err = result.is_some_and(|r| r.b("error", false));
        let accent = if has_err {
            ALARM
        } else if running {
            RUN
        } else {
            INFO
        };
        let pulse = if running {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(30));
            ((now() * 3.5 + idx as f64 * 0.7).sin() * 0.5 + 0.5) as f32
        } else {
            0.0
        };
        let mut clicked = false;
        let fr = egui::Frame::new()
            .fill(FIELD)
            .stroke(Stroke::new(
                if running { 1.3 } else { 1.0 },
                accent.gamma_multiply(if running { 0.55 + 0.35 * pulse } else { 0.45 }),
            ))
            .corner_radius(CornerRadius::same(7))
            .inner_margin(Margin {
                left: 11,
                right: 8,
                top: 5,
                bottom: 5,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut header = egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    id,
                    running,
                )
                .show_header(ui, |ui| {
                    let (dr, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                    if running {
                        ui.painter().circle_stroke(
                            dr.center(),
                            3.2 + pulse * 2.8,
                            Stroke::new(1.1, RUN.gamma_multiply(0.75 - 0.5 * pulse)),
                        );
                    }
                    ui.painter().circle_filled(dr.center(), 3.0, accent);
                    ui.label(
                        egui::RichText::new("Subchat")
                            .font(medium(11.0))
                            .color(accent),
                    );
                    clicked = ui
                        .add(
                            egui::Label::new(
                                egui::RichText::new(clip_one_line(&role, 54))
                                    .font(medium(12.0))
                                    .color(INK),
                            )
                            .truncate()
                            .sense(Sense::click()),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked();
                    let st_txt = if has_err {
                        "falló"
                    } else if running {
                        "trabajando…"
                    } else {
                        "listo"
                    };
                    ui.label(egui::RichText::new(st_txt).font(body(11.0)).color(accent));
                });
                if clicked {
                    header.toggle();
                }
                header.body(|ui| {
                    if !prompt.is_empty() {
                        ui.label(faint("Encargo"));
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(clip_one_line(&prompt, 240))
                                    .font(body(12.0))
                                    .color(INK2),
                            )
                            .wrap(),
                        );
                        ui.add_space(4.0);
                    }
                    if let Some(text) = result.and_then(|r| r.os("text")).filter(|t| !t.is_empty())
                    {
                        crate::gui::response::markdown(ui, &text);
                    } else if running {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(faint("Esperando respuesta del subagente…"));
                        });
                    }
                });
            });
        let bar = egui::Rect::from_min_max(
            fr.response.rect.min,
            Pos2::new(fr.response.rect.min.x + 2.5, fr.response.rect.max.y),
        );
        ui.painter().rect_filled(
            bar,
            CornerRadius {
                nw: 7,
                sw: 7,
                ne: 0,
                se: 0,
            },
            accent,
        );
        ui.add_space(3.0);
        return;
    }

    let mut clicked = false;
    let mut header =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
            .show_header(ui, |ui| {
                if let Some(result) = result {
                    let error = result.b("error", false);
                    ui.label(
                        egui::RichText::new(if error { "Falló" } else { "✓" })
                            .font(medium(11.5))
                            .color(if error { ALARM } else { RUN }),
                    );
                } else if running {
                    ui.spinner();
                } else {
                    ui.label(
                        egui::RichText::new("✓")
                            .font(medium(11.5))
                            .color(RUN.gamma_multiply(0.75)),
                    );
                }
                clicked = ui
                    .add(
                        egui::Label::new(
                            egui::RichText::new(summary)
                                .font(body(12.5))
                                .color(if running { INK } else { INK2 }),
                        )
                        .truncate()
                        .sense(Sense::click()),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked();
            });
    if clicked {
        header.toggle();
    }
    header.body(|ui| {
        if let Some(command) = command
            .as_deref()
            .filter(|_| codex_command || input.as_object().is_some_and(|o| o.len() <= 1))
        {
            crate::gui::response::code_block(ui, "Comando", command);
        } else if !input.is_null() && input.as_object().is_none_or(|o| !o.is_empty()) {
            let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
            crate::gui::response::code_block(
                ui,
                if name.is_empty() { "Entrada" } else { &name },
                &pretty,
            );
        }
        if let Some(text) = result
            .and_then(|result| result.os("text"))
            .filter(|text| !text.is_empty())
        {
            crate::gui::response::code_block(ui, "Resultado", &text);
        }
    });
}

fn divider(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(Vec2::new(w, 16.0), Sense::hover());
        let p = ui.painter();
        let g = p.layout_no_wrap(text.to_string(), body(11.0), INK3);
        let tw = g.size().x + 16.0;
        let cx = r.center().x;
        p.line_segment(
            [
                Pos2::new(r.min.x, r.center().y),
                Pos2::new(cx - tw / 2.0, r.center().y),
            ],
            Stroke::new(1.0, RULE),
        );
        p.line_segment(
            [
                Pos2::new(cx + tw / 2.0, r.center().y),
                Pos2::new(r.max.x, r.center().y),
            ],
            Stroke::new(1.0, RULE),
        );
        p.galley(
            Pos2::new(cx - g.size().x / 2.0, r.center().y - g.size().y / 2.0),
            g,
            INK3,
        );
    });
    ui.add_space(4.0);
}

fn verify_card(ui: &mut egui::Ui, ev: &Value) {
    let cmd = ev.s("cmd");
    match ev.s("phase").as_str() {
        "start" => divider(ui, &format!("Comprobando con {cmd}")),
        "skipped" => divider(ui, "No se pudo ejecutar la prueba; queda para revisar"),
        _ => {
            let ok = ev.b("ok", false);
            card(ui, Some(if ok { RUN } else { ALARM }), |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    egui::RichText::new(if ok {
                        format!(
                            "La prueba pasó ({cmd}, {:.0} s)",
                            ev.f("secs").unwrap_or(0.0)
                        )
                    } else {
                        format!("La prueba falló ({cmd})")
                    })
                    .font(medium(13.0))
                    .color(if ok { RUN } else { ALARM }),
                );
                if !ok {
                    let tail: String = ev
                        .s("text")
                        .chars()
                        .rev()
                        .take(1200)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    ui.add(
                        egui::Label::new(egui::RichText::new(tail).font(mono(11.5)).color(INK3))
                            .wrap(),
                    );
                }
            });
            ui.add_space(6.0);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ErrorAction {
    Retry(String),
    OpenBrains,
    OpenUsage,
    NewChat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    Auth,
    Quota,
    ContextFull,
    Other,
}

fn classify_error(text: &str) -> ErrorKind {
    let l = text.to_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| l.contains(k));
    if has(&[
        "contexto",
        "context_length",
        "context window",
        "prompt is too long",
    ]) {
        ErrorKind::ContextFull
    } else if has(&[
        "429", "402", "límite", "limite", "limit", "quota", "saldo", "balance",
    ]) {
        ErrorKind::Quota
    } else if has(&["401", "403", "clave", "sesión", "sesion", "login", "key"])
        || (l.contains("token") && !l.contains("tokens"))
    {
        ErrorKind::Auth
    } else {
        ErrorKind::Other
    }
}

fn error_header(t: &Value, msg: &str) -> Option<String> {
    let ml = msg.to_lowercase();
    let brain = t
        .os("brain_name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| t.s("brain"));
    let raw_model = t.s("model");
    let model = if raw_model.is_empty() {
        String::new()
    } else {
        crate::gui::home::model_label(&raw_model)
    };
    let has_brain = !brain.is_empty() && ml.contains(&brain.to_lowercase());
    let has_model = !raw_model.is_empty() && ml.contains(&raw_model.to_lowercase());
    match (
        !brain.is_empty() && !has_brain,
        !model.is_empty() && !has_model,
    ) {
        (true, true) => Some(format!("{brain} · {model}")),
        (true, false) => Some(brain),
        (false, true) => Some(if brain.is_empty() {
            model
        } else {
            format!("{brain} · {model}")
        }),
        (false, false) => None,
    }
}

fn error_card(
    ui: &mut egui::Ui,
    text: &str,
    t: &Value,
    snap: &Value,
    can_retry: bool,
) -> Option<ErrorAction> {
    let l = text.to_lowercase();
    let kind = classify_error(text);
    let balance_err = l.contains("402") || l.contains("saldo") || l.contains("balance");
    let msg = if kind == ErrorKind::Quota && !text.contains("HTTP 429.") && !balance_err {
        let when = text
            .split("resets")
            .nth(1)
            .map(|s| format!(" Vuelve {}", s.trim().trim_end_matches('.')))
            .unwrap_or_default();
        format!("{} llegó a su límite de uso.{when}", t.s("brain_name"))
    } else {
        text.to_string()
    };
    let mut action = None;
    card(ui, Some(ALARM), |ui| {
        ui.set_width(ui.available_width());
        if let Some(header) = error_header(t, &msg) {
            ui.label(egui::RichText::new(header).font(medium(12.0)).color(ALARM));
        }
        ui.add(egui::Label::new(egui::RichText::new(msg).font(body(13.5)).color(INK)).wrap());
        if can_retry || kind != ErrorKind::Other {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if can_retry {
                    if primary(ui, "Reintentar turno").clicked() {
                        action = Some(ErrorAction::Retry(String::new()));
                    }
                    let alternatives = snap.arr("brains");
                    let available = snap.arr("chain");
                    ui.menu_button("Elegir otro cerebro", |ui| {
                        let mut found = false;
                        for brain in &alternatives {
                            let id = brain.s("id");
                            if id == t.s("brain")
                                || !available
                                    .iter()
                                    .any(|item| item.s("id") == id && item.b("available", false))
                            {
                                continue;
                            }
                            if t.s("mode") != "plan" && !crate::tasks::can_edit(&brain.s("kind")) {
                                continue;
                            }
                            found = true;
                            if ui.button(brain.s("name")).clicked() {
                                action = Some(ErrorAction::Retry(id));
                                ui.close();
                            }
                        }
                        if !found {
                            ui.label("No hay otro cerebro disponible para este turno.");
                        }
                    });
                }
                match kind {
                    ErrorKind::Auth => {
                        let btn = if can_retry {
                            quiet(ui, "Abrir Cerebros")
                        } else {
                            primary(ui, "Abrir Cerebros")
                        };
                        if btn.clicked() {
                            action = Some(ErrorAction::OpenBrains);
                        }
                    }
                    ErrorKind::Quota => {
                        let btn = if can_retry {
                            quiet(ui, "Ver uso y saldo")
                        } else {
                            primary(ui, "Ver uso y saldo")
                        };
                        if btn.clicked() {
                            action = Some(ErrorAction::OpenUsage);
                        }
                    }
                    ErrorKind::ContextFull => {
                        let btn = if can_retry {
                            quiet(ui, "Nueva conversación")
                        } else {
                            primary(ui, "Nueva conversación")
                        };
                        if btn.clicked() {
                            action = Some(ErrorAction::NewChat);
                        }
                    }
                    ErrorKind::Other => {}
                }
            });
        }
    });
    ui.add_space(6.0);
    action
}

fn diff_stats(diff: &str) -> DiffStats {
    let mut s = DiffStats::default();
    let mut in_hunk = false;
    let mut plus_headers = 0usize;
    for line in diff.lines() {
        s.total_lines += 1;
        if line.starts_with("diff --git ") || line.starts_with("Archivo nuevo: ") {
            s.files += 1;
            in_hunk = false;
        } else if line.starts_with("+++ ") && !in_hunk {
            plus_headers += 1;
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            if line.starts_with('+') && !line.starts_with("+++ ") {
                s.added += 1;
            } else if line.starts_with('-') && line != "---" && !line.starts_with("--- ") {
                s.removed += 1;
            }
        }
    }
    if s.files == 0 && plus_headers > 0 {
        s.files = plus_headers;
    }
    s
}

pub(super) fn diff_window(app: &mut App, ctx: &egui::Context) {
    if let Some(DiffView::Loading { task_id, rx }) = app.ui.diff.as_ref() {
        let done = match rx.try_recv() {
            Ok(text) => Some(text),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some("No se pudieron obtener los cambios.".into())
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        };
        if let Some(text) = done {
            let task_id = task_id.clone();
            let stats = diff_stats(&text);
            app.ui.diff = Some(DiffView::Ready {
                task_id,
                text,
                stats,
            });
        }
    }
    let Some(view) = app.ui.diff.as_ref() else {
        return;
    };
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        app.ui.diff = None;
        return;
    }
    let mut open = true;
    let mut copied = false;
    let title = match view {
        DiffView::Loading { task_id, .. } | DiffView::Ready { task_id, .. } => {
            if task_id.is_empty() {
                "Cambios de la tarea".to_string()
            } else {
                format!("Cambios de la tarea ({task_id})")
            }
        }
    };
    egui::Window::new(title)
        .open(&mut open)
        .default_size([840.0, 560.0])
        .collapsible(false)
        .show(ctx, |ui| match view {
            DiffView::Loading { .. } => {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(dim("Calculando cambios en Git…"));
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(80));
            }
            DiffView::Ready { text, stats, .. } => {
                ui.horizontal(|ui| {
                    let f_lbl = if stats.files == 1 {
                        "1 archivo".to_string()
                    } else {
                        format!("{} archivos", stats.files)
                    };
                    ui.label(egui::RichText::new(f_lbl).font(medium(12.5)).color(INK2));
                    ui.label(
                        egui::RichText::new(format!("+{}", stats.added))
                            .font(mono(12.0))
                            .color(RUN),
                    );
                    ui.label(
                        egui::RichText::new(format!("-{}", stats.removed))
                            .font(mono(12.0))
                            .color(ALARM),
                    );
                    if stats.total_lines > 4000 {
                        ui.label(faint(format!(
                            "(mostrando 4000 de {} líneas)",
                            stats.total_lines
                        )));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_copy = !text.trim().is_empty();
                        if ui
                            .add_enabled(
                                can_copy,
                                egui::Button::new(
                                    egui::RichText::new("Copiar diff")
                                        .font(medium(12.0))
                                        .color(if can_copy { INK2 } else { INK3 }),
                                )
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::new(1.0, RULE))
                                .corner_radius(CornerRadius::same(R)),
                            )
                            .clicked()
                        {
                            ctx.copy_text(text.clone());
                            copied = true;
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::both().show(ui, |ui| {
                    if text.trim().is_empty() {
                        ui.label(dim("Sin cambios."));
                    }
                    for line in text.lines().take(4000) {
                        let c = if line.starts_with('+') && !line.starts_with("+++") {
                            RUN
                        } else if line.starts_with('-') && !line.starts_with("---") {
                            ALARM
                        } else if line.starts_with("@@") {
                            INFO
                        } else {
                            INK2
                        };
                        ui.add(
                            egui::Label::new(egui::RichText::new(line).font(mono(12.0)).color(c))
                                .selectable(true),
                        );
                    }
                });
            }
        });
    if copied {
        app.say("Diff copiado", RUN);
    }
    if !open {
        app.ui.diff = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_message_displays_attachments_without_prompt_instructions() {
        let path = std::env::temp_dir().join("captura con espacios.png");
        let record = json!({"user_message": {"text": "", "files": [&path]}});
        let (text, files) = user_message(&record, "Texto interno");
        assert_eq!(text, "");
        assert_eq!(files, vec![path]);
    }

    #[test]
    fn old_attachment_blocks_still_display_with_their_message() {
        let path = std::env::temp_dir().join("captura con espacios.png");
        for heading in ["revísalos antes de empezar", "ábrelos desde estas rutas"] {
            let message = format!(
                "Revisa la captura\n\nArchivos adjuntos ({heading}):\n- {}",
                path.display()
            );
            let (text, files) = user_message(&Value::Null, &message);
            assert_eq!(text, "Revisa la captura");
            assert_eq!(files, vec![path.clone()]);
        }
    }

    #[test]
    fn message_renders_the_image_texture_from_a_local_file() {
        let path = std::env::temp_dir().join(format!("mb chat preview {}.png", std::process::id()));
        std::fs::write(
            &path,
            include_bytes!("../../../../build/metabrain-tray.png"),
        )
        .unwrap();
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let files = [path.clone()];
        let rendered = (0..100).any(|_| {
            let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
                bubble_user(ui, "Captura", &files);
            });
            frame.textures_delta.clear();
            let has_image = ctx.tessellate(frame.shapes, frame.pixels_per_point).iter().any(|shape| {
                matches!(&shape.primitive, egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id != egui::TextureId::default())
            });
            if !has_image {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            has_image
        });
        std::fs::remove_file(path).unwrap();
        assert!(rendered, "the message should paint the attached PNG");
    }

    #[test]
    fn classify_error_and_header_map_actions_accurately() {
        assert_eq!(
            classify_error("HTTP 401. Clave API inválida"),
            ErrorKind::Auth
        );
        assert_eq!(
            classify_error("HTTP 429: rate limit exceeded for tokens"),
            ErrorKind::Quota
        );
        assert_eq!(
            classify_error("HTTP 402. Saldo insuficiente"),
            ErrorKind::Quota
        );
        assert_eq!(
            classify_error("maximum context_length exceeded (128000 tokens)"),
            ErrorKind::ContextFull
        );
        assert_eq!(classify_error("Error genérico de red"), ErrorKind::Other);

        let task = json!({"brain": "DS", "brain_name": "DeepSeek", "model": "deepseek-v4-pro"});
        assert_eq!(
            error_header(&task, "HTTP 400. Error"),
            Some("DeepSeek · deepseek-v4-pro".into())
        );
        assert_eq!(
            error_header(&task, "DeepSeek (deepseek-v4-pro): HTTP 402"),
            None
        );
    }

    #[test]
    fn diff_stats_counts_files_and_hunk_lines_accurately() {
        let sample = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,3 @@\n-old\n+new1\n+new2\n---\nArchivo nuevo: ./bin.dat (vista previa omitida por tamaño)\n";
        let s = diff_stats(sample);
        assert_eq!(s.files, 2);
        assert_eq!(s.added, 2);
        assert_eq!(s.removed, 1);
        assert_eq!(diff_stats(""), DiffStats::default());
    }

    #[test]
    fn flow_preserves_chronology_merges_thinking_and_formats_tool_summaries() {
        let evs = vec![
            json!({"type": "status", "status": "running", "ts": 100.0}),
            json!({"type": "thinking", "text": "Revisando ", "delta": true}),
            json!({"type": "thinking", "text": "el código", "delta": true}),
            json!({"type": "tool_use", "turn_id": "t1", "tool_id": "agy_1", "name": "run_command", "input": {"command": "cargo test --release"}}),
            json!({"type": "tool_result", "turn_id": "t1", "tool_id": "agy_1", "name": "run_command", "error": false, "text": "ok"}),
            json!({"type": "text", "text": "Hola ", "delta": true}),
            json!({"type": "text", "text": "mundo", "delta": true}),
            json!({"type": "tool_use", "turn_id": "t1", "tool_id": "agy_2", "name": "read_file", "input": {"Path": "C:\\proj\\src\\main.rs"}}),
        ];
        let flow = build_flow(&evs, false, true, Some(0));
        assert_eq!(flow.len(), 3);
        match &flow[0] {
            FlowItem::Activity(b) => {
                assert_eq!(b.steps.len(), 2);
                assert!(
                    matches!(&b.steps[0], FlowStep::Thinking { text, .. } if text == "Revisando el código")
                );
                assert!(
                    matches!(&b.steps[1], FlowStep::Tool { ev, result: Some(_), running: false, .. } if tool_summary(ev, false).0 == "Ejecutar: cargo test --release")
                );
            }
            _ => panic!("expected first item to be Activity"),
        }
        assert!(matches!(&flow[1], FlowItem::Prose(p) if p == "Hola mundo"));
        match &flow[2] {
            FlowItem::Activity(b) => {
                assert_eq!(b.steps.len(), 1);
                assert!(
                    matches!(&b.steps[0], FlowStep::Tool { ev, result: None, running: true, .. } if tool_summary(ev, false).0 == "Leer …/src/main.rs")
                );
            }
            _ => panic!("expected third item to be Activity"),
        }
        assert_eq!(live_status_label("running", &flow), "Leer …/src/main.rs…");
    }
}
