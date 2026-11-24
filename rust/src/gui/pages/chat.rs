//! Chat page: conversation list, dialogs, composer, attachments, and submission flow.

use super::chat_intent::{detect_chat_intent, ChatIntent};
use super::thread::{bubble_user, diff_window, reading_column, thread, IMG_EXT};
use super::{short_path, QueuedFollowUp, UiState};
use crate::gui::theme::*;
use crate::gui::App;
use crate::parallel::ParallelRun;
use crate::util::{now, J};
use eframe::egui::{self, Color32, CornerRadius, Margin, Pos2, Rect, Sense, Stroke, Vec2};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// =====================================================================================
// Tareas
// =====================================================================================

// =====================================================================================
// Chat (tasks as conversations, Claude Desktop style)
// =====================================================================================

pub(super) fn status_name(s: &str) -> &'static str {
    match s {
        "queued" => "En cola",
        "planning" => "Planificando",
        "running" => "Trabajando",
        "synthesizing" => "Sintetizando",
        "verifying" => "Verificando",
        "stopping" => "Interrumpiendo",
        "review" => "Para revisar",
        "done" => "Hecha",
        "failed" => "Falló",
        "stopped" => "Detenida",
        "escalated" => "Escalada",
        _ => "—",
    }
}

pub(super) fn status_color(s: &str) -> Color32 {
    match s {
        "running" | "verifying" => RUN,
        "planning" => INFO,
        "synthesizing" | "review" => MASTER,
        "failed" => ALARM,
        "stopped" | "stopping" => ATTN,
        "done" => INK3,
        _ => INFO,
    }
}

fn consume_chat_key(input: &mut egui::InputState, key: egui::Key) -> bool {
    let mut pressed = false;
    input.events.retain(|event| {
        if let egui::Event::Key {
            key: event_key,
            modifiers,
            pressed: true,
            repeat,
            ..
        } = event
        {
            if *event_key == key && *modifiers == egui::Modifiers::NONE {
                pressed |= !repeat;
                return false;
            }
        }
        true
    });
    pressed
}

fn chat_editor(ui: &mut egui::Ui, buf: &mut String, id: egui::Id, fresh: bool) -> bool {
    let send =
        ui.memory(|m| m.has_focus(id)) && ui.input_mut(|i| consume_chat_key(i, egui::Key::Enter));
    let hint = if fresh {
        "Describe lo que necesitas… (pega, arrastra o adjunta imágenes y archivos)"
    } else {
        "Responde o pide un cambio…"
    };
    ui.add(
        egui::TextEdit::multiline(buf)
            .id(id)
            .return_key(egui::KeyboardShortcut::new(
                egui::Modifiers::SHIFT,
                egui::Key::Enter,
            ))
            .hint_text(egui::RichText::new(hint).color(INK3))
            .desired_rows(if fresh { 3 } else { 1 })
            .desired_width(f32::INFINITY)
            .frame(egui::Frame::NONE)
            .text_color(INK)
            .font(body(14.0)),
    );
    send
}

pub fn task_list(app: &App) -> Vec<Value> {
    app.data
        .read()
        .map(|d| {
            d.tasks
                .as_array()
                .cloned()
                .unwrap_or_else(|| d.tasks.arr("tasks"))
        })
        .unwrap_or_default()
}

pub(super) fn parallel_run_list(app: &App) -> Vec<ParallelRun> {
    app.data
        .read()
        .map(|d| d.parallel_runs.clone())
        .unwrap_or_default()
}

#[derive(Clone, Debug)]
struct SubchatEntry {
    /// Linked child task id (empty while a planned branch has not started yet).
    task_id: String,
    branch_id: String,
    title: String,
    role: String,
    wave: usize,
    tier: u8,
    brain_id: String,
    brain_name: String,
    model: String,
    status: String,
    tokens: f64,
}

impl SubchatEntry {
    fn is_live(&self) -> bool {
        matches!(
            self.status.as_str(),
            "running" | "queued" | "verifying" | "planning" | "synthesizing"
        )
    }
}

fn build_subchat_map(
    all_tasks: &[Value],
    parallel_runs: &[ParallelRun],
) -> HashMap<String, Vec<SubchatEntry>> {
    let mut by_parent: HashMap<String, Vec<SubchatEntry>> = HashMap::new();
    let mut seen_task_ids: HashMap<String, std::collections::HashSet<String>> = HashMap::new();

    // 1. Seed from ParallelRun branches (preserves wave order and shows planned branches early).
    for run in parallel_runs {
        let mut entries = Vec::new();
        let seen = seen_task_ids.entry(run.id.clone()).or_default();
        for br in &run.branches {
            if !br.task_id.is_empty() {
                seen.insert(br.task_id.clone());
            }
            entries.push(SubchatEntry {
                task_id: br.task_id.clone(),
                branch_id: br.id.clone(),
                title: br.title.clone(),
                role: br.role.clone(),
                wave: br.wave + 1,
                tier: br.tier,
                brain_id: br.brain.clone(),
                brain_name: br.brain_name.clone(),
                model: br.model.clone(),
                status: br.status.clone(),
                tokens: br.tokens as f64,
            });
        }
        if !entries.is_empty() {
            by_parent.insert(run.id.clone(), entries);
        }
    }

    // 2. Merge live child task records (parent_run != "") so status/tokens stay fresh even for historical runs.
    for t in all_tasks {
        let parent = t.s("parent_run");
        if parent.is_empty() {
            continue;
        }
        let tid = t.s("id");
        let list = by_parent.entry(parent.clone()).or_default();
        if let Some(existing) = list
            .iter_mut()
            .find(|e| !tid.is_empty() && e.task_id == tid)
        {
            existing.status = t.s("status");
            let tok = t.f("tokens").unwrap_or(0.0);
            if tok > existing.tokens {
                existing.tokens = tok;
            }
            if existing.brain_name.is_empty() {
                existing.brain_name = t.s("brain_name");
            }
            if existing.brain_id.is_empty() {
                existing.brain_id = t.s("brain");
            }
            if existing.model.is_empty() {
                existing.model = t.s("model");
            }
        } else {
            let seen = seen_task_ids.entry(parent).or_default();
            if seen.insert(tid.clone()) {
                list.push(SubchatEntry {
                    task_id: tid,
                    branch_id: t.s("branch_id"),
                    title: t.s("title"),
                    role: t.s("branch_role"),
                    wave: t
                        .get("branch_wave")
                        .and_then(|v| v.as_u64())
                        .map(|w| w as usize + 1)
                        .unwrap_or(1),
                    tier: t.get("branch_tier").and_then(|v| v.as_u64()).unwrap_or(2) as u8,
                    brain_id: t.s("brain"),
                    brain_name: t.s("brain_name"),
                    model: t.s("model"),
                    status: t.s("status"),
                    tokens: t.f("tokens").unwrap_or(0.0),
                });
            }
        }
    }

    // 3. Coherent automatic ordering: Wave ascending -> live first within wave -> Tier ascending -> branch_id.
    for list in by_parent.values_mut() {
        list.sort_by(|a, b| {
            a.wave
                .cmp(&b.wave)
                .then_with(|| (!a.is_live()).cmp(&(!b.is_live())))
                .then_with(|| a.tier.cmp(&b.tier))
                .then_with(|| a.branch_id.cmp(&b.branch_id))
        });
    }
    by_parent
}

fn brain_accent_color(snap: &Value, brain_id: &str) -> Color32 {
    if brain_id == "parallel" {
        return INFO;
    }
    if brain_id.is_empty() || brain_id == "auto" {
        return MASTER;
    }
    snap.arr("brains")
        .iter()
        .position(|b| b.s("id") == brain_id)
        .map(series)
        .unwrap_or(MASTER)
}

pub(super) fn tasks(app: &mut App, ui: &mut egui::Ui, snap: &Value) {
    finish_chat_submit(app);
    drain_queued_follow_up(app, ui.ctx());
    // Files dropped anywhere on the window become attachments.
    let dropped: Vec<String> = ui.ctx().input(|i| {
        i.raw
            .dropped_files
            .iter()
            .map(|f| f.path().display().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    });
    let all_tasks: Vec<Value> = task_list(app)
        .into_iter()
        .filter(|t| {
            !app.ui
                .convs
                .get(t.s("id"))
                .map(|m| m.b("deleted", false))
                .unwrap_or(false)
        })
        .collect();
    let parallel_runs = parallel_run_list(app);
    let subchats = build_subchat_map(&all_tasks, &parallel_runs);
    let mut items: Vec<Value> = all_tasks
        .iter()
        .filter(|t| t.s("parent_run").is_empty())
        .cloned()
        .collect();
    let selected = app
        .sup
        .tasks
        .get(&app.ui.sel_task)
        .filter(|t| !conv_meta(app, &t.s("id")).b("deleted", false));
    if let Some(t) = &selected {
        let parent_id = t.s("parent_run");
        if parent_id.is_empty() {
            if let Some(row) = items.iter_mut().find(|row| row.s("id") == app.ui.sel_task) {
                *row = t.clone();
            } else {
                items.insert(0, t.clone());
            }
        } else {
            // Ensure the parent orchestration stays expanded while viewing one of its subchats.
            app.ui.subchat_open.entry(parent_id).or_insert(true);
        }
    }
    // Coherent automatic sorting: active orchestrations rise to the top of live items.
    items.sort_by(|a, b| {
        let live_score = |t: &Value| -> u8 {
            let st = t.s("status");
            let busy = matches!(
                st.as_str(),
                "planning" | "running" | "synthesizing" | "verifying" | "queued" | "stopping"
            );
            if busy && t.s("kind") == "parallel" {
                0
            } else if busy {
                1
            } else {
                2
            }
        };
        live_score(a).cmp(&live_score(b)).then_with(|| {
            let ta = a.f("updated").unwrap_or(0.0);
            let tb = b.f("updated").unwrap_or(0.0);
            tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    let h = ui.available_height().max(1.0);
    let open = !app.ui.conv_closed;
    let t = ui
        .ctx()
        .animate_bool_with_time(egui::Id::unique("conv_open"), open, 0.22);
    let expanded_w = (ui.available_width() * 0.32).clamp(170.0, 270.0);
    let list_w = egui::lerp(40.0..=expanded_w, t);
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(list_w, h), Sense::hover());
        {
            let mut ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("conversation_list")
                    .max_rect(rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            ui.shrink_clip_rect(rect);
            ui.horizontal(|ui| {
                let tip = if open {
                    "Ocultar conversaciones"
                } else {
                    "Mostrar conversaciones"
                };
                if icon_button(
                    ui,
                    if open { Icon::Left } else { Icon::Right },
                    tip,
                    false,
                    true,
                )
                .clicked()
                {
                    app.ui.conv_closed = open;
                }
            });
            if t > 0.85 {
                conversations(app, &mut ui, snap, &items, &subchats);
            } else if !open
                && icon_button(&mut ui, Icon::Plus, "Nueva conversación", false, true).clicked()
            {
                app.ui.sel_task.clear();
                app.ui.focus_composer = true;
            }
        }
        ui.add_space(14.0);
        let w = ui.available_width();
        ui.allocate_ui_with_layout(
            Vec2::new(w, h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(w);
                let selected = app
                    .sup
                    .tasks
                    .get(&app.ui.sel_task)
                    .filter(|t| !conv_meta(app, &t.s("id")).b("deleted", false));
                let draft = app
                    .ui
                    .drafts
                    .entry(selected.as_ref().map_or_else(String::new, |t| t.s("id")))
                    .or_default();
                for file in dropped {
                    if !draft.files.contains(&file) {
                        draft.files.push(file);
                    }
                }
                egui::ScrollArea::vertical()
                    .id_salt(("chat_column", selected.as_ref().map(|t| t.s("id"))))
                    .max_height(h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| match selected {
                        Some(t) => thread(app, ui, snap, &t, h),
                        None => new_chat(app, ui, snap, h),
                    });
            },
        );
    });
    diff_window(app, ui.ctx());
    if let Some(id) = app.ui.confirm_discard.clone() {
        egui::Modal::new(egui::Id::unique("confirm_discard")).show(ui.ctx(), |ui| {
            ui.heading("¿Descartar la copia de trabajo?");
            ui.label("Git eliminará la copia aislada, su rama y sus cambios.");
            ui.horizontal(|ui| {
                if quiet(ui, "Cancelar").clicked() {
                    app.ui.confirm_discard = None;
                }
                if danger(ui, "Descartar definitivamente").clicked() {
                    app.ui.confirm_discard = None;
                    app.post(&format!("api/tasks/{id}/discard"), json!({}), |r| {
                        if let Some(error) = r.os("error") {
                            (error, ALARM)
                        } else {
                            (r.s("result"), RUN)
                        }
                    });
                }
            });
        });
    }
}

fn conv_meta(app: &App, id: &str) -> Value {
    app.ui.convs.get(id).cloned().unwrap_or_else(|| json!({}))
}

fn conv_set(app: &mut App, id: &str, k: &str, v: Value) {
    if !app.ui.convs.is_object() {
        app.ui.convs = json!({});
    }
    if !app.ui.convs[id].is_object() {
        app.ui.convs[id] = json!({});
    }
    app.ui.convs[id][k] = v;
    let _ = crate::util::write_json(&crate::util::data_dir().join("convs.json"), &app.ui.convs);
}

fn conv_title(app: &App, t: &Value) -> String {
    let m = conv_meta(app, &t.s("id"));
    if m.s("name").is_empty() {
        t.s("title")
    } else {
        m.s("name")
    }
}

/// Names already used for groups or projects, sorted.
fn conv_names(app: &App, k: &str) -> Vec<String> {
    let mut v: Vec<String> = app
        .ui
        .convs
        .as_object()
        .map(|m| {
            m.values()
                .map(|x| x.s(k))
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.dedup();
    v
}

enum ConvAct {
    Open(String),
    ToggleSubchats(String, bool),
    Set(String, &'static str, Value),
    Rename(String, String),
    NewName(String, bool),
    Delete(String),
    DropProject(String),
}

fn conv_dots_rect(row: Rect) -> Rect {
    Rect::from_center_size(
        Pos2::new(row.max.x - 16.0, row.center().y),
        Vec2::new(24.0, 22.0),
    )
}

fn paint_conv_dots(
    ui: &egui::Ui,
    dots: Rect,
    hovered_row: bool,
    dresp: &egui::Response,
    menu_open: bool,
) {
    let p = ui.painter();
    let active = dresp.hovered() || menu_open;
    let fill = if active {
        RULE
    } else if hovered_row {
        RAISED2
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        p.rect_filled(dots, CornerRadius::same(5), fill);
    }
    let dot_col = if active || hovered_row { INK } else { INK2 };
    for k in [-5.0, 0.0, 5.0] {
        p.circle_filled(
            Pos2::new(dots.center().x + k, dots.center().y),
            1.6,
            dot_col,
        );
    }
}

fn conv_menu(
    ui: &mut egui::Ui,
    id: &str,
    t: &Value,
    m: &Value,
    projects: &[String],
    groups_named: &[String],
    acts: &mut Vec<ConvAct>,
) {
    ui.set_min_width(200.0);
    let unread_now = m
        .get("seen")
        .map(|s| s.as_f64() == Some(0.0))
        .unwrap_or(false);
    if ui
        .button(if unread_now {
            "Marcar como leído"
        } else {
            "Marcar como no leído"
        })
        .clicked()
    {
        acts.push(ConvAct::Set(
            id.to_string(),
            "seen",
            json!(if unread_now {
                t.f("updated").unwrap_or(now())
            } else {
                0.0
            }),
        ));
        ui.close();
    }
    ui.separator();
    if ui
        .button(if m.b("pinned", false) {
            "Desfijar"
        } else {
            "Fijar"
        })
        .clicked()
    {
        acts.push(ConvAct::Set(
            id.to_string(),
            "pinned",
            json!(!m.b("pinned", false)),
        ));
        ui.close();
    }
    if ui.button("Cambiar nombre").clicked() {
        acts.push(ConvAct::Rename(id.to_string(), String::new()));
        ui.close();
    }
    ui.menu_button("Agregar al proyecto", |ui| {
        for pj in projects {
            if ui.selectable_label(m.s("project") == *pj, pj).clicked() {
                acts.push(ConvAct::Set(id.to_string(), "project", json!(pj)));
                ui.close();
            }
        }
        if !m.s("project").is_empty() && ui.button("Quitar del proyecto").clicked() {
            acts.push(ConvAct::Set(id.to_string(), "project", json!("")));
            ui.close();
        }
        if ui.button("Nuevo proyecto…").clicked() {
            acts.push(ConvAct::NewName(id.to_string(), true));
            ui.close();
        }
    });
    ui.menu_button("Mover al grupo", |ui| {
        for g in groups_named {
            if ui.selectable_label(m.s("group") == *g, g).clicked() {
                acts.push(ConvAct::Set(id.to_string(), "group", json!(g)));
                ui.close();
            }
        }
        if !m.s("group").is_empty() && ui.button("Sacar del grupo").clicked() {
            acts.push(ConvAct::Set(id.to_string(), "group", json!("")));
            ui.close();
        }
        if ui.button("Nuevo grupo…").clicked() {
            acts.push(ConvAct::NewName(id.to_string(), false));
            ui.close();
        }
    });
    ui.separator();
    if ui
        .button(if m.b("archived", false) {
            "Desarchivar"
        } else {
            "Archivar"
        })
        .clicked()
    {
        acts.push(ConvAct::Set(
            id.to_string(),
            "archived",
            json!(!m.b("archived", false)),
        ));
        ui.close();
    }
    if ui
        .add(egui::Button::new(
            egui::RichText::new("Eliminar").color(ALARM),
        ))
        .clicked()
    {
        acts.push(ConvAct::Delete(id.to_string()));
        ui.close();
    }
}

fn conv_row_interact_dots(
    ui: &mut egui::Ui,
    row: Rect,
    in_project: bool,
    id: &str,
) -> (Rect, egui::Response, bool) {
    let dots = conv_dots_rect(row);
    let dresp = ui
        .interact(
            dots,
            egui::Id::unique(("conv_dots", in_project, id)),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let menu_open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&dresp));
    (dots, dresp, menu_open)
}

fn paint_status_dot(ui: &egui::Ui, center: Pos2, base_r: f32, st: &str) {
    let p = ui.painter();
    if matches!(st, "running" | "verifying") {
        let k = ((crate::util::now() * 3.0).sin() * 0.5 + 0.5) as f32;
        p.circle_filled(center, base_r + k, status_color(st));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(60));
    } else {
        p.circle_filled(center, base_r, status_color(st));
    }
}

fn released_now(ui: &egui::Ui) -> bool {
    ui.input(|i| !i.pointer.any_down())
}

fn subchats_open(app: &App, parent_id: &str, parent_busy: bool, subs: &[SubchatEntry]) -> bool {
    if let Some(&saved) = app.ui.subchat_open.get(parent_id) {
        return saved;
    }
    parent_busy
        || subs
            .iter()
            .any(|s| s.is_live() || (!s.task_id.is_empty() && s.task_id == app.ui.sel_task))
}

#[allow(clippy::too_many_arguments)]
fn render_subchat_tree(
    app: &App,
    ui: &mut egui::Ui,
    snap: &Value,
    parent_id: &str,
    parent_busy: bool,
    subs: &[SubchatEntry],
    indent: f32,
    acts: &mut Vec<ConvAct>,
) {
    if subs.is_empty() {
        return;
    }
    let open = subchats_open(app, parent_id, parent_busy, subs);
    let k = ui.ctx().animate_bool_with_time(
        egui::Id::unique(("subchat_tree_k", parent_id)),
        open,
        0.20,
    );
    if k <= 0.01 {
        return;
    }
    let total = subs.len();
    for (idx, sub) in subs.iter().enumerate() {
        let row_h = 28.0 * k;
        let (r, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
        let p = ui.painter();
        let rail_x = r.min.x + indent;
        let is_last = idx + 1 == total;
        let rail_col = if sub.is_live() {
            RUN.gamma_multiply(0.55 * k)
        } else {
            RULE.gamma_multiply(0.85 * k)
        };
        // Vertical trunk + L-branch connector into the subchat row.
        let top_y = r.min.y;
        let mid_y = r.center().y;
        let bot_y = if is_last { mid_y } else { r.max.y };
        p.line_segment(
            [Pos2::new(rail_x, top_y), Pos2::new(rail_x, bot_y)],
            Stroke::new(1.2, rail_col),
        );
        p.line_segment(
            [Pos2::new(rail_x, mid_y), Pos2::new(rail_x + 9.0, mid_y)],
            Stroke::new(1.2, rail_col),
        );

        if k > 0.55 {
            let alpha = ((k - 0.55) / 0.45).clamp(0.0, 1.0);
            let card_r = Rect::from_min_max(
                Pos2::new(rail_x + 11.0, r.min.y + 1.5),
                Pos2::new(r.max.x, r.max.y - 1.5),
            );
            let sel = !sub.task_id.is_empty() && app.ui.sel_task == sub.task_id;
            let clickable = !sub.task_id.is_empty();
            let hovered = clickable && resp.hovered();
            let accent = brain_accent_color(snap, &sub.brain_id);
            if sel || hovered {
                p.rect_filled(
                    card_r,
                    CornerRadius::same(5),
                    if sel { RAISED2 } else { RAISED },
                );
                if sel {
                    p.rect_filled(
                        Rect::from_min_size(
                            card_r.min + Vec2::new(0.0, 4.0),
                            Vec2::new(2.0, (card_r.height() - 8.0).max(4.0)),
                        ),
                        CornerRadius::same(1),
                        accent,
                    );
                }
            }
            // Pulsing live dot for active subchat agents.
            let dot_pos = Pos2::new(card_r.min.x + 9.0, card_r.center().y);
            if sub.is_live() {
                let pulse = ((now() * 3.4 + idx as f64 * 0.9).sin() * 0.5 + 0.5) as f32;
                p.circle_stroke(
                    dot_pos,
                    3.0 + pulse * 3.0,
                    Stroke::new(
                        1.1,
                        status_color(&sub.status).gamma_multiply((0.65 - 0.45 * pulse) * alpha),
                    ),
                );
                p.circle_filled(
                    dot_pos,
                    2.8,
                    status_color(&sub.status).gamma_multiply(alpha),
                );
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(33));
            } else {
                p.circle_filled(
                    dot_pos,
                    2.6,
                    status_color(&sub.status).gamma_multiply(alpha),
                );
            }
            // Wave pill (O1, O2, ...).
            let wave_txt = format!("O{}", sub.wave);
            let wg = p.layout_no_wrap(wave_txt, mono(9.5), accent.gamma_multiply(alpha));
            let ww = wg.size().x + 6.0;
            let wr = Rect::from_center_size(
                Pos2::new(card_r.min.x + 17.0 + ww * 0.5, card_r.center().y),
                Vec2::new(ww, 14.0),
            );
            p.rect_filled(
                wr,
                CornerRadius::same(4),
                accent.gamma_multiply(0.14 * alpha),
            );
            p.galley(
                Pos2::new(
                    wr.center().x - wg.size().x * 0.5,
                    wr.center().y - wg.size().y * 0.5,
                ),
                wg,
                accent.gamma_multiply(alpha),
            );

            // Title + right-aligned short brain label.
            let short_brain = if sub.brain_name.is_empty() {
                "cola".to_string()
            } else {
                sub.brain_name
                    .split_whitespace()
                    .next()
                    .unwrap_or(&sub.brain_name)
                    .chars()
                    .take(9)
                    .collect()
            };
            let bg = p.layout_no_wrap(short_brain, body(10.0), INK3.gamma_multiply(alpha));
            let bw = bg.size().x;
            p.galley(
                Pos2::new(
                    card_r.max.x - bw - 6.0,
                    card_r.center().y - bg.size().y * 0.5,
                ),
                bg,
                INK3.gamma_multiply(alpha),
            );

            let title_left = wr.max.x + 6.0;
            let title_right = (card_r.max.x - bw - 10.0).max(title_left);
            let sub_title: String = if sub.title.is_empty() {
                sub.role.clone()
            } else {
                sub.title.clone()
            };
            let sub_title: String = sub_title.chars().take(42).collect();
            p.with_clip_rect(Rect::from_min_max(
                Pos2::new(title_left, card_r.min.y),
                Pos2::new(title_right, card_r.max.y),
            ))
            .text(
                Pos2::new(title_left, card_r.center().y),
                egui::Align2::LEFT_CENTER,
                sub_title,
                if sel { medium(11.5) } else { body(11.5) },
                if clickable {
                    INK.gamma_multiply(alpha)
                } else {
                    INK3.gamma_multiply(alpha)
                },
            );

            let tip = format!(
                "Subchat · Ola {} · T{} · {}\nCerebro: {}{}\nEstado: {}",
                sub.wave,
                sub.tier,
                if sub.role.is_empty() {
                    "agente"
                } else {
                    sub.role.as_str()
                },
                if sub.brain_name.is_empty() {
                    "por asignar"
                } else {
                    sub.brain_name.as_str()
                },
                if sub.model.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", crate::gui::home::model_label(&sub.model))
                },
                status_name(&sub.status),
            );
            let resp = resp.on_hover_text(tip);
            if clickable {
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    acts.push(ConvAct::Open(sub.task_id.clone()));
                }
            }
        }
    }
}

fn conversations(
    app: &mut App,
    ui: &mut egui::Ui,
    snap: &Value,
    items: &[Value],
    subchats: &HashMap<String, Vec<SubchatEntry>>,
) {
    if primary(ui, "+  Nueva conversación").clicked() {
        app.ui.sel_task.clear();
        app.ui.focus_composer = true;
    }
    ui.add_space(8.0);
    // The open conversation counts as read.
    if !app.ui.sel_task.is_empty() {
        let id = app.ui.sel_task.clone();
        if let Some(t) = items.iter().find(|t| t.s("id") == id) {
            let upd = t.f("updated").unwrap_or(0.0);
            if conv_meta(app, &id).f("seen").unwrap_or(-1.0) < upd {
                conv_set(app, &id, "seen", json!(upd));
            }
        }
    }
    let groups_named = conv_names(app, "group");
    let mut projects = conv_names(app, "project");
    for p in app
        .ui
        .convs
        .get("_projects")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if let Some(p) = p.as_str().filter(|p| !p.is_empty()) {
            if !projects.iter().any(|x| x == p) {
                projects.push(p.to_string());
            }
        }
    }
    projects.sort();
    // Drag a conversation onto a project folder to file it there.
    let drag_mem = egui::Id::unique("conv_drag");
    let dragging: Option<String> = ui.ctx().data(|d| d.get_temp::<String>(drag_mem));
    let mut folder_rects: Vec<(String, Rect)> = vec![];
    let mut dropped: Option<String> = None;
    let mut acts: Vec<ConvAct> = vec![];
    egui::ScrollArea::vertical()
        .id_salt("convs")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let meta = |id: &str| app.ui.convs.get(id).cloned().unwrap_or_else(|| json!({}));
            let visible: Vec<&Value> = items
                .iter()
                .filter(|t| !meta(&t.s("id")).b("archived", false))
                .collect();
            // Sections: pinned, your groups, then by status.
            // Projects: folders on top, each with its conversations (open/closed by click).
            let has_proj = |t: &&Value| !meta(&t.s("id")).s("project").is_empty();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Proyectos")
                        .font(medium(11.5))
                        .color(INK3),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, Icon::Plus, "Nuevo proyecto", false, true).clicked() {
                        acts.push(ConvAct::NewName(String::new(), true));
                    }
                });
            });
            if projects.is_empty() {
                ui.label(faint(
                    "Crea un proyecto con + y arrastra conversaciones a él.",
                ));
            }
            for pj in &projects {
                let mut inside: Vec<&Value> = visible
                    .iter()
                    .copied()
                    .filter(|t| meta(&t.s("id")).s("project") == *pj)
                    .collect();
                inside.sort_by_key(|t| !meta(&t.s("id")).b("pinned", false));
                let open_id = egui::Id::unique(("proj_open", pj.as_str()));
                let open = ui
                    .ctx()
                    .data(|d| d.get_temp::<bool>(open_id))
                    .unwrap_or(false);
                let (fr, fresp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
                folder_rects.push((pj.clone(), fr));
                let over = dragging.is_some()
                    && ui
                        .input(|i| i.pointer.hover_pos())
                        .map(|pp| fr.contains(pp))
                        .unwrap_or(false);
                let glow = ui.ctx().animate_bool_with_time(
                    egui::Id::unique(("proj_glow", pj.as_str())),
                    over,
                    0.15,
                );
                let p = ui.painter();
                if fresp.hovered() || glow > 0.0 {
                    p.rect_filled(fr, CornerRadius::same(R), RAISED);
                }
                if glow > 0.0 {
                    p.rect_stroke(
                        fr.expand(glow * 2.0),
                        CornerRadius::same(R),
                        Stroke::new(1.5, MASTER.gamma_multiply(glow)),
                        egui::StrokeKind::Inside,
                    );
                }
                // Folder glyph.
                let fx = fr.min.x + 10.0;
                let fy = fr.center().y;
                let col = if glow > 0.0 { MASTER } else { INK2 };
                p.rect_filled(
                    Rect::from_min_size(Pos2::new(fx, fy - 6.0), Vec2::new(16.0, 12.0)),
                    CornerRadius::same(2),
                    col.gamma_multiply(0.35),
                );
                p.rect_filled(
                    Rect::from_min_size(Pos2::new(fx, fy - 8.0), Vec2::new(7.0, 3.0)),
                    CornerRadius::same(1),
                    col.gamma_multiply(0.6),
                );
                p.with_clip_rect(Rect::from_min_max(
                    fr.min,
                    Pos2::new((fr.max.x - 42.0).max(fr.min.x), fr.max.y),
                ))
                .text(
                    Pos2::new(fx + 24.0, fy),
                    egui::Align2::LEFT_CENTER,
                    pj,
                    medium(13.0),
                    INK,
                );
                p.text(
                    Pos2::new(fr.max.x - 10.0, fy),
                    egui::Align2::RIGHT_CENTER,
                    format!("{}  {}", inside.len(), if open { "▾" } else { "▸" }),
                    body(11.5),
                    INK3,
                );
                if fresp.clicked() {
                    ui.ctx().data_mut(|d| d.insert_temp(open_id, !open));
                }
                fresp.context_menu(|ui| {
                    if ui
                        .button("Eliminar proyecto")
                        .on_hover_text("Las conversaciones vuelven a la lista; no se borra nada")
                        .clicked()
                    {
                        acts.push(ConvAct::DropProject(pj.clone()));
                        ui.close();
                    }
                });
                let k = ui.ctx().animate_bool_with_time(
                    egui::Id::unique(("proj_k", pj.as_str())),
                    open,
                    0.18,
                );
                if k > 0.01 {
                    for t in inside.iter().take(40) {
                        let id = t.s("id");
                        let m = meta(&id);
                        let sel = app.ui.sel_task == id;
                        let unread = !sel
                            && t.f("updated").unwrap_or(0.0) > m.f("seen").unwrap_or(0.0)
                            && m.get("seen").is_some();
                        if k > 0.6 {
                            if let Some((rid, buf)) =
                                app.ui.conv_rename.as_mut().filter(|(rid, _)| *rid == id)
                            {
                                let r = ui.add(
                                    egui::TextEdit::singleline(buf)
                                        .desired_width(f32::INFINITY)
                                        .font(body(12.5)),
                                );
                                r.request_focus();
                                if r.lost_focus() {
                                    let done = ui.input(|i| !i.key_pressed(egui::Key::Escape));
                                    acts.push(ConvAct::Rename(
                                        rid.clone(),
                                        if done {
                                            buf.trim().to_string()
                                        } else {
                                            "\u{0}".into()
                                        },
                                    ));
                                }
                                continue;
                            }
                        }
                        let (r, resp) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 30.0 * k),
                            Sense::click_and_drag(),
                        );
                        if resp.drag_started() {
                            ui.ctx().data_mut(|d| d.insert_temp(drag_mem, id.clone()));
                        }
                        let row_subs = subchats.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                        let parent_busy = crate::tasks::turn_busy(&t.s("status"));
                        if k > 0.6 {
                            let (dots, dresp, menu_open) = conv_row_interact_dots(ui, r, true, &id);
                            let hovered = resp.hovered()
                                || dresp.hovered()
                                || resp.context_menu_opened()
                                || menu_open;
                            let show_dots = hovered || sel;
                            let pinned = m.b("pinned", false);
                            if sel || hovered {
                                ui.painter().rect_filled(
                                    r,
                                    CornerRadius::same(R),
                                    if sel { RAISED2 } else { RAISED },
                                );
                            }
                            paint_status_dot(
                                ui,
                                Pos2::new(r.min.x + 24.0, r.center().y),
                                3.0,
                                &t.s("status"),
                            );
                            let right_pad = if show_dots {
                                if pinned {
                                    46.0
                                } else {
                                    32.0
                                }
                            } else if !row_subs.is_empty() {
                                38.0
                            } else if pinned || unread {
                                24.0
                            } else {
                                10.0
                            };
                            let title: String = conv_title(app, t).chars().take(60).collect();
                            ui.painter()
                                .with_clip_rect(Rect::from_min_max(
                                    r.min,
                                    Pos2::new((r.max.x - right_pad).max(r.min.x), r.max.y),
                                ))
                                .text(
                                    Pos2::new(r.min.x + 34.0, r.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    title,
                                    if sel || unread {
                                        bold(12.5)
                                    } else {
                                        body(12.5)
                                    },
                                    INK,
                                );
                            if !row_subs.is_empty() && !show_dots {
                                let is_open = subchats_open(app, &id, parent_busy, row_subs);
                                let live_n = row_subs.iter().filter(|s| s.is_live()).count();
                                let badge_col = if live_n > 0 { RUN } else { INFO };
                                let txt = format!(
                                    "{} {}",
                                    row_subs.len(),
                                    if is_open { "▾" } else { "▸" }
                                );
                                let bg = ui.painter().layout_no_wrap(txt, medium(10.0), badge_col);
                                let br = Rect::from_center_size(
                                    Pos2::new(r.max.x - 16.0, r.center().y),
                                    Vec2::new(bg.size().x + 8.0, 15.0),
                                );
                                ui.painter().rect_filled(
                                    br,
                                    CornerRadius::same(6),
                                    badge_col.gamma_multiply(0.14),
                                );
                                ui.painter().galley(
                                    Pos2::new(
                                        br.center().x - bg.size().x * 0.5,
                                        br.center().y - bg.size().y * 0.5,
                                    ),
                                    bg,
                                    badge_col,
                                );
                            } else if unread && !show_dots {
                                ui.painter().circle_filled(
                                    Pos2::new(r.max.x - 12.0, r.center().y),
                                    3.5,
                                    INFO,
                                );
                            }
                            if pinned {
                                let px = if show_dots {
                                    r.max.x - 36.0
                                } else if unread {
                                    r.max.x - 24.0
                                } else {
                                    r.max.x - 14.0
                                };
                                paint_icon(
                                    ui.painter(),
                                    Rect::from_center_size(
                                        Pos2::new(px, r.center().y),
                                        Vec2::splat(10.0),
                                    ),
                                    Icon::Up,
                                    INK3,
                                );
                            }
                            if show_dots {
                                paint_conv_dots(ui, dots, hovered, &dresp, menu_open);
                            }
                            if resp.clicked() && !dresp.clicked() {
                                acts.push(ConvAct::Open(id.clone()));
                            }
                            resp.context_menu(|ui| {
                                conv_menu(ui, &id, t, &m, &projects, &groups_named, &mut acts)
                            });
                            egui::Popup::menu(&dresp).show(|ui| {
                                conv_menu(ui, &id, t, &m, &projects, &groups_named, &mut acts)
                            });
                            if !row_subs.is_empty() {
                                render_subchat_tree(
                                    app,
                                    ui,
                                    snap,
                                    &id,
                                    parent_busy,
                                    row_subs,
                                    24.0,
                                    &mut acts,
                                );
                            }
                        } else if resp.clicked() {
                            acts.push(ConvAct::Open(id.clone()));
                        }
                    }
                }
            }
            ui.add_space(8.0);
            let mut sections: Vec<(String, Vec<&Value>)> = vec![];
            let visible: Vec<&Value> = visible.into_iter().filter(|t| !has_proj(t)).collect();
            sections.push((
                "Fijadas".into(),
                visible
                    .iter()
                    .copied()
                    .filter(|t| meta(&t.s("id")).b("pinned", false))
                    .collect(),
            ));
            for g in &groups_named {
                sections.push((
                    g.clone(),
                    visible
                        .iter()
                        .copied()
                        .filter(|t| {
                            let m = meta(&t.s("id"));
                            !m.b("pinned", false) && m.s("group") == *g
                        })
                        .collect(),
                ));
            }
            let rest = |t: &&Value| {
                let m = meta(&t.s("id"));
                !m.b("pinned", false) && m.s("group").is_empty()
            };
            let st_groups: [(&str, &[&str]); 3] = [
                (
                    "En curso",
                    &[
                        "planning",
                        "running",
                        "synthesizing",
                        "queued",
                        "verifying",
                        "stopping",
                    ],
                ),
                ("Para revisar", &["review", "failed", "stopped"]),
                ("Anteriores", &["done", "escalated"]),
            ];
            for (title, sts) in st_groups {
                sections.push((
                    title.into(),
                    visible
                        .iter()
                        .copied()
                        .filter(rest)
                        .filter(|t| sts.contains(&t.s("status").as_str()))
                        .collect(),
                ));
            }
            if app.ui.show_archived {
                sections.push((
                    "Archivadas".into(),
                    items
                        .iter()
                        .filter(|t| meta(&t.s("id")).b("archived", false))
                        .collect(),
                ));
            }
            for (title, g) in sections {
                if g.is_empty() {
                    continue;
                }
                ui.label(egui::RichText::new(title).font(medium(11.5)).color(INK3));
                ui.add_space(2.0);
                for t in g.iter().take(60) {
                    let id = t.s("id");
                    let m = meta(&id);
                    let sel = app.ui.sel_task == id;
                    let unread = !sel
                        && t.f("updated").unwrap_or(0.0) > m.f("seen").unwrap_or(0.0)
                        && m.get("seen").is_some();
                    // Inline rename.
                    if let Some((rid, buf)) =
                        app.ui.conv_rename.as_mut().filter(|(rid, _)| *rid == id)
                    {
                        let r = ui.add(
                            egui::TextEdit::singleline(buf)
                                .desired_width(f32::INFINITY)
                                .font(body(13.0)),
                        );
                        r.request_focus();
                        if r.lost_focus() {
                            let done = ui.input(|i| !i.key_pressed(egui::Key::Escape));
                            acts.push(ConvAct::Rename(
                                rid.clone(),
                                if done {
                                    buf.trim().to_string()
                                } else {
                                    "\u{0}".into()
                                },
                            ));
                        }
                        continue;
                    }
                    let (r, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 44.0),
                        Sense::click_and_drag(),
                    );
                    if resp.drag_started() {
                        ui.ctx().data_mut(|d| d.insert_temp(drag_mem, id.clone()));
                    }
                    let row_subs = subchats.get(&id).map(Vec::as_slice).unwrap_or(&[]);
                    let parent_busy = crate::tasks::turn_busy(&t.s("status"));
                    let (dots, dresp, menu_open) = conv_row_interact_dots(ui, r, false, &id);
                    let mut sub_toggle_clicked = false;
                    let mut sub_toggle_hovered = false;
                    let sub_badge_rect = if !row_subs.is_empty() {
                        let is_open = subchats_open(app, &id, parent_busy, row_subs);
                        let live_n = row_subs.iter().filter(|s| s.is_live()).count();
                        let label = if live_n > 0 {
                            format!(
                                "{live_n}/{} {}",
                                row_subs.len(),
                                if is_open { "▾" } else { "▸" }
                            )
                        } else {
                            format!("{} {}", row_subs.len(), if is_open { "▾" } else { "▸" })
                        };
                        let bw = (label.len() as f32 * 5.8 + 12.0).max(28.0);
                        let rx = if sel || resp.hovered() || dresp.hovered() || menu_open {
                            r.max.x - 32.0 - bw * 0.5
                        } else {
                            r.max.x - 8.0 - bw * 0.5
                        };
                        let br = Rect::from_center_size(
                            Pos2::new(rx, r.min.y + 32.0),
                            Vec2::new(bw, 16.0),
                        );
                        let sresp = ui
                            .interact(
                                br,
                                egui::Id::unique(("subchat_tog", id.as_str())),
                                Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(if is_open {
                                "Contraer subchats de agentes"
                            } else {
                                "Ver subchats de agentes"
                            });
                        sub_toggle_hovered = sresp.hovered();
                        if sresp.clicked() {
                            sub_toggle_clicked = true;
                            acts.push(ConvAct::ToggleSubchats(id.clone(), !is_open));
                        }
                        Some((br, label, live_n > 0))
                    } else {
                        None
                    };

                    let hovered = resp.hovered()
                        || dresp.hovered()
                        || sub_toggle_hovered
                        || resp.context_menu_opened()
                        || menu_open;
                    let show_dots = hovered || sel;
                    if sel || hovered {
                        ui.painter().rect_filled(
                            r,
                            CornerRadius::same(R),
                            if sel { RAISED2 } else { RAISED },
                        );
                    }
                    paint_status_dot(
                        ui,
                        Pos2::new(r.min.x + 12.0, r.min.y + 15.0),
                        3.5,
                        &t.s("status"),
                    );
                    let name = if m.s("name").is_empty() {
                        t.s("title")
                    } else {
                        m.s("name")
                    };
                    let title: String = name.chars().take(60).collect();
                    let p = ui.painter();
                    let tp = p.with_clip_rect(Rect::from_min_max(
                        r.min,
                        Pos2::new((r.max.x - 34.0).max(r.min.x), r.max.y),
                    ));
                    tp.text(
                        Pos2::new(r.min.x + 24.0, r.min.y + 15.0),
                        egui::Align2::LEFT_CENTER,
                        title,
                        if sel || unread {
                            bold(13.0)
                        } else {
                            body(13.0)
                        },
                        INK,
                    );
                    let sub_right_pad = sub_badge_rect
                        .as_ref()
                        .map(|(br, _, _)| (r.max.x - br.min.x + 6.0).max(34.0))
                        .unwrap_or(34.0);
                    let sp = p.with_clip_rect(Rect::from_min_max(
                        r.min,
                        Pos2::new((r.max.x - sub_right_pad).max(r.min.x), r.max.y),
                    ));
                    let proj = if m.s("project").is_empty() {
                        String::new()
                    } else {
                        format!("{} · ", m.s("project"))
                    };
                    let model_tag = t
                        .os("model")
                        .filter(|m| !m.is_empty() && m != "motor-paralelo")
                        .map(|m| {
                            let short = m.replace("claude-", "").replace("deepseek-", "ds-");
                            format!(" · {short}")
                        })
                        .unwrap_or_default();
                    sp.text(
                        Pos2::new(r.min.x + 24.0, r.min.y + 32.0),
                        egui::Align2::LEFT_CENTER,
                        format!(
                            "{proj}{}{model_tag} · {}",
                            t.s("brain_name"),
                            ago(now(), t.f("updated").unwrap_or(now()))
                        ),
                        body(11.0),
                        INK3,
                    );
                    if let Some((br, label, has_live)) = sub_badge_rect {
                        let col = if has_live { RUN } else { INFO };
                        p.rect_filled(
                            br,
                            CornerRadius::same(7),
                            col.gamma_multiply(if sub_toggle_hovered { 0.28 } else { 0.14 }),
                        );
                        p.rect_stroke(
                            br,
                            CornerRadius::same(7),
                            Stroke::new(0.8, col.gamma_multiply(0.45)),
                            egui::StrokeKind::Inside,
                        );
                        p.text(
                            br.center(),
                            egui::Align2::CENTER_CENTER,
                            label,
                            medium(10.0),
                            col,
                        );
                    }
                    if unread && !show_dots {
                        p.circle_filled(Pos2::new(r.max.x - 10.0, r.min.y + 15.0), 3.5, INFO);
                    }
                    if m.b("pinned", false) && !show_dots {
                        paint_icon(
                            p,
                            Rect::from_center_size(
                                Pos2::new(r.max.x - 12.0, r.max.y - 12.0),
                                Vec2::splat(10.0),
                            ),
                            Icon::Up,
                            INK3,
                        );
                    }
                    if show_dots {
                        paint_conv_dots(ui, dots, hovered, &dresp, menu_open);
                    }
                    if resp.clicked() && !dresp.clicked() && !sub_toggle_clicked {
                        acts.push(ConvAct::Open(id.clone()));
                    }
                    resp.context_menu(|ui| {
                        conv_menu(ui, &id, t, &m, &projects, &groups_named, &mut acts)
                    });
                    egui::Popup::menu(&dresp)
                        .show(|ui| conv_menu(ui, &id, t, &m, &projects, &groups_named, &mut acts));
                    if !row_subs.is_empty() {
                        render_subchat_tree(
                            app,
                            ui,
                            snap,
                            &id,
                            parent_busy,
                            row_subs,
                            14.0,
                            &mut acts,
                        );
                    }
                }
                ui.add_space(8.0);
            }
            if items.is_empty() {
                ui.label(faint("Tus conversaciones aparecerán aquí."));
            }
            let n_arch = items
                .iter()
                .filter(|t| meta(&t.s("id")).b("archived", false))
                .count();
            if n_arch > 0
                && ui
                    .link(
                        egui::RichText::new(if app.ui.show_archived {
                            "Ocultar archivadas".to_string()
                        } else {
                            format!("Archivadas ({n_arch})")
                        })
                        .font(body(11.5))
                        .color(INK3),
                    )
                    .clicked()
            {
                app.ui.show_archived = !app.ui.show_archived;
            }
        });
    if let Some(did) = &dragging {
        let (pos, released) = ui.input(|i| (i.pointer.hover_pos(), !i.pointer.any_down()));
        if let Some(pp) = pos {
            if released {
                dropped = folder_rects
                    .iter()
                    .find(|(_, r)| r.contains(pp))
                    .map(|(n, _)| n.clone());
            } else {
                // The conversation travels under the pointer.
                let title: String = items
                    .iter()
                    .find(|t| t.s("id") == *did)
                    .map(|t| conv_title(app, t))
                    .unwrap_or_default()
                    .chars()
                    .take(26)
                    .collect();
                let layer =
                    egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("conv_ghost"));
                let p = ui.ctx().layer_painter(layer);
                let gr = Rect::from_min_size(pp + Vec2::new(10.0, 6.0), Vec2::new(210.0, 30.0));
                p.rect_filled(
                    gr.translate(Vec2::new(0.0, 3.0)),
                    CornerRadius::same(R),
                    Color32::from_black_alpha(90),
                );
                p.rect_filled(gr, CornerRadius::same(R), RAISED2);
                p.rect_stroke(
                    gr,
                    CornerRadius::same(R),
                    Stroke::new(1.0, MASTER),
                    egui::StrokeKind::Inside,
                );
                p.text(
                    gr.left_center() + Vec2::new(10.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    title,
                    medium(12.5),
                    INK,
                );
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
        }
        if released_now(ui) {
            if let Some(pj) = dropped.take() {
                acts.push(ConvAct::Set(did.clone(), "project", json!(pj)));
                app.say(format!("Movida a {pj}"), RUN);
            }
            ui.ctx().data_mut(|d| d.remove::<String>(drag_mem));
        }
    }
    for a in acts {
        match a {
            ConvAct::DropProject(pj) => {
                if let Some(m) = app.ui.convs.as_object_mut() {
                    for (_, v) in m.iter_mut() {
                        if v.s("project") == pj {
                            v["project"] = json!("");
                        }
                    }
                    if let Some(l) = m.get_mut("_projects").and_then(|l| l.as_array_mut()) {
                        l.retain(|x| x.as_str() != Some(pj.as_str()));
                    }
                }
                let _ = crate::util::write_json(
                    &crate::util::data_dir().join("convs.json"),
                    &app.ui.convs,
                );
            }
            ConvAct::Open(id) => app.ui.sel_task = id,
            ConvAct::ToggleSubchats(id, open) => {
                app.ui.subchat_open.insert(id, open);
            }
            ConvAct::Set(id, k, v) => conv_set(app, &id, k, v),
            ConvAct::Rename(id, name) if name.is_empty() => {
                let cur = items
                    .iter()
                    .find(|t| t.s("id") == id)
                    .map(|t| conv_title(app, t))
                    .unwrap_or_default();
                app.ui.conv_rename = Some((id, cur));
            }
            ConvAct::Rename(id, name) => {
                if name != "\u{0}" {
                    conv_set(app, &id, "name", json!(name));
                }
                app.ui.conv_rename = None;
            }
            ConvAct::NewName(id, project) => {
                app.ui.conv_newgroup = Some((id, String::new(), project))
            }
            ConvAct::Delete(id) => app.ui.confirm_delete = Some(id),
        }
    }
    conv_dialogs(app, ui.ctx(), items);
}

fn conv_dialogs(app: &mut App, ctx: &egui::Context, items: &[Value]) {
    if let Some((id, mut buf, project)) = app.ui.conv_newgroup.clone() {
        let mut done: Option<bool> = None;
        egui::Window::new(if project {
            "Nuevo proyecto"
        } else {
            "Nuevo grupo"
        })
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .show(ctx, |ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut buf)
                    .hint_text("Nombre")
                    .desired_width(260.0),
            );
            r.request_focus();
            ui.horizontal(|ui| {
                if primary(ui, "Crear").clicked()
                    || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    done = Some(true);
                }
                if quiet(ui, "Cancelar").clicked() {
                    done = Some(false);
                }
            });
        });
        match done {
            Some(true) if !buf.trim().is_empty() && id.is_empty() => {
                if !app.ui.convs.is_object() {
                    app.ui.convs = json!({});
                }
                let name = buf.trim().to_string();
                let mut l = app
                    .ui
                    .convs
                    .get("_projects")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !l.iter().any(|x| x.as_str() == Some(name.as_str())) {
                    l.push(json!(name));
                }
                app.ui.convs["_projects"] = json!(l);
                let _ = crate::util::write_json(
                    &crate::util::data_dir().join("convs.json"),
                    &app.ui.convs,
                );
                app.ui.conv_newgroup = None;
            }
            Some(true) if !buf.trim().is_empty() => {
                conv_set(
                    app,
                    &id,
                    if project { "project" } else { "group" },
                    json!(buf.trim()),
                );
                app.ui.conv_newgroup = None;
            }
            Some(_) => app.ui.conv_newgroup = None,
            None => app.ui.conv_newgroup = Some((id, buf, project)),
        }
    }
    if let Some(id) = app.ui.confirm_delete.clone() {
        let name = items
            .iter()
            .find(|t| t.s("id") == id)
            .map(|t| conv_title(app, t))
            .unwrap_or_default();
        let mut close = false;
        egui::Window::new("Eliminar conversación").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            ui.label(format!("¿Eliminar «{name}»? Desaparece de la lista y no se puede deshacer. Los archivos de tu proyecto no se tocan."));
            ui.horizontal(|ui| {
                if danger(ui, "Eliminar").clicked() {
                    conv_set(app, &id, "deleted", json!(true));
                    if app.ui.sel_task == id {
                        app.ui.sel_task.clear();
                    }
                    close = true;
                }
                if quiet(ui, "Cancelar").clicked() {
                    close = true;
                }
            });
        });
        if close {
            app.ui.confirm_delete = None;
        }
    }
}

/// Empty state: a big centered composer, like starting a chat.
fn new_chat(app: &mut App, ui: &mut egui::Ui, snap: &Value, h: f32) {
    if let Some(pending) = &app.ui.pending_open {
        if pending.origin.is_empty() && pending.body.get("prompt").is_some() {
            reading_column(ui, 760.0, |ui| {
                let files = pending
                    .body
                    .arr("files")
                    .iter()
                    .filter_map(|f| f.as_str().map(PathBuf::from))
                    .collect::<Vec<_>>();
                bubble_user(ui, &pending.body.s("prompt"), &files);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(faint("Iniciando conversación…"));
                });
            });
            return;
        }
    }
    ui.add_space(((h - 400.0) * 0.4).clamp(0.0, h * 0.22));
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new("¿En qué trabajamos?")
                .font(cond(30.0))
                .color(INK),
        );
        ui.add_space(2.0);
        ui.label(dim(
            "MetaBrain elige el cerebro más barato que pueda hacerlo y comprueba el resultado.",
        ));
    });
    ui.add_space(18.0);
    reading_column(ui, 820.0, |ui| {
        composer_box(app, ui, snap, None);
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            for s in [
                "Revisa este proyecto y dime qué mejorar",
                "Escribe pruebas para el módulo principal",
                "Arregla el error que falla al compilar",
                "Resume los cambios de hoy",
            ] {
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new(s).font(body(12.5)).color(INK2))
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, RULE))
                            .corner_radius(CornerRadius::same(14)),
                    )
                    .clicked()
                {
                    app.ui.prompt = s.to_string();
                }
            }
        });
    });
}

fn brain_models(brains: &[Value], brain_id: &str) -> Vec<String> {
    let Some(b) = brains.iter().find(|b| b.s("id") == brain_id) else {
        return Vec::new();
    };
    let mut out: Vec<String> = b
        .arr("models")
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(m) = b.os("model").filter(|s| !s.is_empty()) {
        if !out.iter().any(|x| x == &m) {
            out.push(m);
        }
    }
    out
}

/// Apply live auto-detected model/brain/mode intent from the composer text without fighting manual clicks.
/// Saves the prior `(brain, model, build)` snapshot so editing away the directive cleanly reverts it.
fn sync_live_intent(
    app: &mut App,
    draft_id: &str,
    live_text: &str,
    brains: &[Value],
    chain: &[Value],
    fresh: bool,
) -> Option<ChatIntent> {
    let detected = detect_chat_intent(live_text, brains, chain);
    let Some(intent) = detected else {
        app.ui.auto_intent_dismissed.remove(draft_id);
        app.ui.auto_intent_applied.remove(draft_id);
        if let Some((prev_brain, prev_model, prev_build)) = app.ui.auto_intent_prev.remove(draft_id)
        {
            if fresh {
                app.ui.brain = prev_brain;
                app.ui.model = prev_model;
                app.ui.build = prev_build;
            } else {
                if prev_brain.is_empty() {
                    app.ui.reply_brain.remove(draft_id);
                } else {
                    app.ui.reply_brain.insert(draft_id.to_string(), prev_brain);
                }
                if prev_model.is_empty() {
                    app.ui.reply_model.remove(draft_id);
                } else {
                    app.ui.reply_model.insert(draft_id.to_string(), prev_model);
                }
            }
        }
        return None;
    };
    if app
        .ui
        .auto_intent_dismissed
        .get(draft_id)
        .map(String::as_str)
        == Some(intent.key.as_str())
    {
        return None;
    }
    let already =
        app.ui.auto_intent_applied.get(draft_id).map(String::as_str) == Some(intent.key.as_str());
    if !already && (intent.available || intent.brain_id == "auto") {
        // Capture pre-intent state once so deleting the directive restores the user's original settings.
        app.ui
            .auto_intent_prev
            .entry(draft_id.to_string())
            .or_insert_with(|| {
                if fresh {
                    (app.ui.brain.clone(), app.ui.model.clone(), app.ui.build)
                } else {
                    (
                        app.ui
                            .reply_brain
                            .get(draft_id)
                            .cloned()
                            .unwrap_or_default(),
                        app.ui
                            .reply_model
                            .get(draft_id)
                            .cloned()
                            .unwrap_or_default(),
                        app.ui.build,
                    )
                }
            });
        if fresh {
            app.ui.brain = intent.brain_id.clone();
            app.ui.model = intent.model.clone();
            if let Some(bm) = intent.build_mode {
                app.ui.build = bm;
            }
        } else if intent.brain_id == "auto" {
            // In existing threads, "auto" resets any manual override back to the thread's active brain.
            app.ui.reply_brain.remove(draft_id);
            app.ui.reply_model.remove(draft_id);
        } else {
            app.ui
                .reply_brain
                .insert(draft_id.to_string(), intent.brain_id.clone());
            if intent.model.is_empty() {
                app.ui.reply_model.remove(draft_id);
            } else {
                app.ui
                    .reply_model
                    .insert(draft_id.to_string(), intent.model.clone());
            }
        }
        app.ui
            .auto_intent_applied
            .insert(draft_id.to_string(), intent.key.clone());
        app.ui.auto_switch_at = now();
    }
    Some(intent)
}

/// The rounded composer: text, attachments, live model auto-adaptation, and a tool row.
/// `task` carries the conversation ID and current status for replies.
pub(super) fn composer_box(
    app: &mut App,
    ui: &mut egui::Ui,
    snap: &Value,
    task: Option<(&str, &str)>,
) {
    let fresh = task.is_none();
    let draft_id = task.map_or("", |(id, _)| id).to_string();
    let mut draft = app.ui.drafts.remove(&draft_id).unwrap_or_default();
    if let Some(result) = app
        .ui
        .folder_picker
        .as_ref()
        .and_then(|rx| match rx.try_recv() {
            Ok(folder) => Some(folder),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
        })
    {
        app.ui.folder_picker = None;
        if let Some(folder) = result {
            app.ui.cwd = folder;
            remember_folder(&app.ui.cwd);
        }
    }
    if let Some((target_id, files)) =
        app.ui
            .file_picker
            .as_ref()
            .and_then(|(tid, rx)| match rx.try_recv() {
                Ok(files) => Some((tid.clone(), files)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((tid.clone(), Vec::new())),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            })
    {
        app.ui.file_picker = None;
        if !files.is_empty() {
            if target_id == draft_id {
                for f in files {
                    if !draft.files.contains(&f) {
                        draft.files.push(f);
                    }
                }
            } else {
                let target = app.ui.drafts.entry(target_id).or_default();
                for f in files {
                    if !target.files.contains(&f) {
                        target.files.push(f);
                    }
                }
            }
        }
    }
    let brains = snap.arr("brains");
    let chain = snap.arr("chain");
    let waiting = task.is_some_and(|(_, status)| crate::tasks::turn_busy(status));
    let interruptible = task.is_some_and(|(_, status)| waiting && status != "stopping");
    let interrupt_key = interruptible
        && !app.ui.drawer
        && app.ui.palette.is_none()
        && app.ui.confirm_delete.is_none()
        && app.ui.conv_newgroup.is_none()
        && app.ui.diff.is_none()
        && ui.input_mut(|i| consume_chat_key(i, egui::Key::Escape));
    let edit_id = ui.make_persistent_id(("chat_composer", task.map(|(id, _)| id)));
    let focused = ui.memory(|m| m.has_focus(edit_id));

    // Evaluate live auto-switch intent before drawing border so the frame smoothly adapts its accent.
    let live_text_pre = if fresh {
        app.ui.prompt.clone()
    } else {
        draft.reply.clone()
    };
    let live_intent = sync_live_intent(app, &draft_id, &live_text_pre, &brains, &chain, fresh);
    let active_brain_for_tint = if fresh {
        app.ui.brain.clone()
    } else {
        app.ui
            .reply_brain
            .get(&draft_id)
            .cloned()
            .unwrap_or_default()
    };
    let accent = if let Some(ref det) = live_intent {
        if !det.available {
            ATTN
        } else if det.parallel {
            INFO
        } else {
            det.brain_index.map(series).unwrap_or(MASTER)
        }
    } else {
        brain_accent_color(snap, &active_brain_for_tint)
    };
    let switch_age = (now() - app.ui.auto_switch_at).max(0.0);
    let switch_pulse = if switch_age < 0.75 {
        let p = (1.0 - switch_age / 0.75) as f32;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(20));
        p * p
    } else {
        0.0
    };
    let border = if switch_pulse > 0.01 {
        Stroke::new(
            1.6 + switch_pulse * 1.4,
            accent.gamma_multiply(0.75 + 0.25 * switch_pulse),
        )
    } else if live_intent.is_some() {
        Stroke::new(1.5, accent.gamma_multiply(0.85))
    } else if focused {
        Stroke::new(1.5, MASTER.gamma_multiply(0.85))
    } else {
        Stroke::new(1.0, BORDER_SUBTLE)
    };

    egui::Frame::new()
        .fill(RAISED)
        .stroke(border)
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin { left: 14, right: 10, top: 10, bottom: 8 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Smooth animated auto-switch banner when a model/brain/orchestration directive is detected in chat.
            let banner_k = ui.ctx().animate_bool_with_time(
                egui::Id::unique(("auto_intent_banner", draft_id.as_str())),
                live_intent.is_some(),
                0.18,
            );
            if banner_k > 0.02 {
                if let Some(ref det) = live_intent {
                    let banner_col = if !det.available { ATTN } else { accent };
                    egui::Frame::new()
                        .fill(banner_col.gamma_multiply(0.11 * banner_k))
                        .stroke(Stroke::new(1.0, banner_col.gamma_multiply(0.45 * banner_k)))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(9, 4))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                let (dr, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                                let r_dot = 3.2 + switch_pulse * 1.8;
                                ui.painter().circle_filled(dr.center(), r_dot, banner_col.gamma_multiply(banner_k));
                                let mut desc = if !det.available {
                                    format!("{} no disponible ahora ({})", det.brain_name, det.reason)
                                } else if det.parallel {
                                    "Orquestación paralela activada automáticamente".to_string()
                                } else if det.model.is_empty() {
                                    format!("Cambio automático → {}", det.brain_name)
                                } else {
                                    format!("Cambio automático → {} · {}", det.brain_name, crate::gui::home::model_label(&det.model))
                                };
                                if let Some(bm) = det.build_mode {
                                    desc.push_str(if bm { " · Construir" } else { " · Planear" });
                                }
                                ui.label(egui::RichText::new(desc).font(medium(11.5)).color(banner_col.gamma_multiply(banner_k)));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(egui::RichText::new("Restablecer").font(body(10.5)).color(INK2))
                                                .fill(Color32::TRANSPARENT)
                                                .stroke(Stroke::new(0.8, RULE))
                                                .corner_radius(CornerRadius::same(6)),
                                        )
                                        .clicked()
                                    {
                                        app.ui.auto_intent_dismissed.insert(draft_id.clone(), det.key.clone());
                                        if fresh {
                                            app.ui.brain = "auto".into();
                                            app.ui.model.clear();
                                        } else {
                                            app.ui.reply_brain.remove(&draft_id);
                                            app.ui.reply_model.remove(&draft_id);
                                        }
                                    }
                                });
                            });
                        });
                    ui.add_space(5.0 * banner_k);
                }
            }

            let buf = if fresh { &mut app.ui.prompt } else { &mut draft.reply };
            if std::mem::take(&mut app.ui.focus_composer) {
                ui.memory_mut(|m| m.request_focus(edit_id));
            }
            let send_key = chat_editor(ui, buf, edit_id, fresh);
            // Re-evaluate immediately after keystrokes in this frame so UI responds on the exact same frame.
            let live_text_post = if fresh { app.ui.prompt.clone() } else { draft.reply.clone() };
            let live_intent = if live_text_post != live_text_pre {
                sync_live_intent(app, &draft_id, &live_text_post, &brains, &chain, fresh)
            } else {
                live_intent
            };
            // Ctrl+V with an image or copied files on the clipboard attaches them (text pastes as usual).
            // egui hands a pasted picture over as PasteImage; copied files (Explorer) bring no
            // event at all, so on Windows Ctrl+V is also read from the keyboard directly.
            let images: Vec<std::sync::Arc<egui::ColorImage>> = ui.input(|i| i.events.iter().filter_map(|e| if let egui::Event::PasteImage(im) = e { Some(im.clone()) } else { None }).collect());
            for im in &images {
                if let Some(f) = save_pasted(im) {
                    if !draft.files.contains(&f) {
                        draft.files.push(f);
                    }
                }
            }
            let paste = images.is_empty() && ui.ctx().input(|i| i.focused) && ctrl_v_pressed();
            if paste {
                for f in paste_attachments() {
                    if !draft.files.contains(&f) {
                        draft.files.push(f);
                    }
                }
            }
            if !draft.files.is_empty() {
                let mut drop_i: Option<usize> = None;
                ui.horizontal_wrapped(|ui| {
                    for (i, f) in draft.files.iter().enumerate() {
                        let name = std::path::Path::new(f).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| f.clone());
                        let ext = std::path::Path::new(f).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                        egui::Frame::new().fill(RAISED2).corner_radius(CornerRadius::same(R)).inner_margin(Margin::symmetric(6, 4)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if IMG_EXT.contains(&ext.as_str()) {
                                    let uri = file_uri(f);
                                    ui.add(egui::Image::new(uri).fit_to_exact_size(Vec2::splat(44.0)).corner_radius(CornerRadius::same(5)));
                                }
                                let short: String = name.chars().take(28).collect();
                                ui.label(egui::RichText::new(short).font(body(12.0)).color(INK2)).on_hover_text(f);
                                if icon_button(ui, Icon::Close, "Quitar", false, true).clicked() {
                                    drop_i = Some(i);
                                }
                            });
                        });
                    }
                });
                if let Some(i) = drop_i {
                    draft.files.remove(i);
                }
            }
            ui.add_space(4.0);
            if fresh {
                ui.horizontal_wrapped(|ui| {
                    // Who works: automatic, or one of the coding agents (Claude Code, Codex),
                    // Google, or the cheap API. Picks the first available brain of that kind.
                    let first = |kinds: &[&str], cheap: bool| -> Option<String> {
                        chain
                            .iter()
                            .filter(|e| e.b("available", false))
                            .map(|e| e.s("id"))
                            .find(|id| {
                                brains.iter().any(|b| {
                                    &b.s("id") == id && (kinds.contains(&b.s("kind").as_str()) || (cheap && b.s("kind") == "openai"))
                                })
                            })
                    };
                    let any_avail = chain.iter().any(|e| e.b("available", false));
                    let opts: [(&str, &str, Option<String>); 6] = [
                        ("Auto", "MetaBrain elige el cerebro más barato que pueda hacerlo", Some("auto".to_string())),
                        ("Paralelo", "Motor paralelo (DAG + varios cerebros a la vez): reparte subtareas según cuota y jerarquía, y el Director sintetiza", if any_avail { Some("parallel".to_string()) } else { None }),
                        ("Claude Code", "Agente de programación de Claude: edita archivos, ejecuta y prueba", first(&["claude_code"], false)),
                        ("Codex", "Agente de programación de ChatGPT: edita archivos, ejecuta y prueba", first(&["codex"], false)),
                        ("Gemini", "Tu cuenta de Google (Antigravity)", first(&["antigravity", "gemini_cli"], false)),
                        ("Barato", "API económica (DeepSeek o Gemini por API)", first(&[], true)),
                    ];
                    let kind_of = |id: &str| brains.iter().find(|b| b.s("id") == id).map(|b| b.s("kind")).unwrap_or_default();
                    let cur_kind = kind_of(&app.ui.brain);
                    for (label, tip, target) in opts.iter() {
                        let on = match *label {
                            "Auto" => app.ui.brain == "auto",
                            "Paralelo" => app.ui.brain == "parallel",
                            "Claude Code" => cur_kind == "claude_code",
                            "Codex" => cur_kind == "codex",
                            "Gemini" => cur_kind == "antigravity" || cur_kind == "gemini_cli",
                            _ => cur_kind == "openai",
                        };
                        let enabled = target.is_some();
                        let pill_fill = if on {
                            if *label == "Paralelo" { INFO } else { MASTER }
                        } else {
                            Color32::TRANSPARENT
                        };
                        let b = egui::Button::new(egui::RichText::new(*label).font(if on { medium(12.5) } else { body(12.5) }).color(if on { Color32::from_rgb(0x1C, 0x16, 0x0A) } else if enabled { INK2 } else { INK3 }))
                            .fill(pill_fill)
                            .stroke(Stroke::new(1.0, if on { pill_fill } else { RULE }))
                            .corner_radius(CornerRadius::same(12));
                        let r = ui.add_enabled(enabled, b).on_hover_text(*tip).on_disabled_hover_text("No hay uno disponible ahora: conéctalo en Cerebros o espera a que tenga cuota");
                        if on && switch_pulse > 0.05 {
                            ui.painter().rect_stroke(
                                r.rect.expand(switch_pulse * 3.0),
                                CornerRadius::same(14),
                                Stroke::new(1.5, accent.gamma_multiply(switch_pulse * 0.9)),
                                egui::StrokeKind::Outside,
                            );
                        }
                        if r.clicked() {
                            if let Some(t) = target {
                                if let Some(ref det) = live_intent {
                                    app.ui.auto_intent_dismissed.insert(draft_id.clone(), det.key.clone());
                                }
                                app.ui.brain = t.clone();
                                app.ui.model.clear();
                                remember_brain(&app.ui.brain);
                                if *label == "Claude Code" || *label == "Codex" {
                                    app.ui.build = true;
                                }
                            }
                        }
                    }
                    // Dynamic model selector appears automatically when a concrete brain with models is active.
                    let models = brain_models(&brains, &app.ui.brain);
                    if !models.is_empty() {
                        let sel_lbl = if app.ui.model.is_empty() {
                            "Modelo: auto".to_string()
                        } else {
                            crate::gui::home::model_label(&app.ui.model)
                        };
                        let mut chosen = app.ui.model.clone();
                        egui::ComboBox::from_id_salt("chat_new_model")
                            .selected_text(egui::RichText::new(sel_lbl).font(mono(11.5)).color(if app.ui.model.is_empty() { INK2 } else { accent }))
                            .width(148.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut chosen, String::new(), "Modelo automático");
                                for m in &models {
                                    ui.selectable_value(&mut chosen, m.clone(), crate::gui::home::model_label(m));
                                }
                            });
                        if chosen != app.ui.model {
                            if let Some(ref det) = live_intent {
                                app.ui.auto_intent_dismissed.insert(draft_id.clone(), det.key.clone());
                            }
                            app.ui.model = chosen;
                        }
                    }
                    egui::ComboBox::from_id_salt("chat_mode").selected_text(egui::RichText::new(if app.ui.build { "Construir" } else { "Planear" }).font(body(12.5))).width(100.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut app.ui.build, true, "Construir: hace los cambios");
                        ui.selectable_value(&mut app.ui.build, false, "Planear: solo analiza");
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    let folder = if app.ui.cwd.trim().is_empty() { "Elegir carpeta".to_string() } else { short_path(app.ui.cwd.trim()) };
                    let fr = ui.add(egui::Button::new(egui::RichText::new(folder).font(body(12.5)).color(INK2)).fill(Color32::TRANSPARENT).stroke(Stroke::new(1.0, RULE)));
                    if fr.clicked() {
                        #[cfg(windows)]
                        if app.ui.folder_picker.is_none() {
                            app.ui.folder_picker = Some(pick_folder(app.ui.cwd.clone(), ui.ctx().clone()));
                        }
                        #[cfg(not(windows))]
                        { app.ui.cwd_edit = !app.ui.cwd_edit; }
                    }
                    ui.menu_button("Carpetas recientes", |ui| {
                        let config = crate::util::read_json(&crate::util::data_dir().join("gui.json")).unwrap_or_default();
                        for folder in config.arr("recent_cwd").iter().filter_map(Value::as_str) {
                            if ui.button(short_path(folder)).on_hover_text(folder).clicked() {
                                app.ui.cwd = folder.into();
                                remember_folder(folder);
                                ui.close();
                            }
                        }
                        if ui.button("Escribir ruta").clicked() {
                            app.ui.cwd_edit = true;
                            ui.close();
                        }
                    });
                    ui.checkbox(&mut app.ui.worktree, egui::RichText::new("copia aparte").font(body(12.0)).color(INK3)).on_hover_text("Trabaja en una copia del repositorio (git worktree); apruebas o descartas al final");
                });
                if app.ui.worktree {
                    let cwd_key = app.ui.cwd.trim().to_string();
                    let is_git = match &app.ui.cwd_git_cache {
                        Some((cached, val)) if *cached == cwd_key => *val,
                        _ => {
                            let val = has_git_repo(&cwd_key);
                            app.ui.cwd_git_cache = Some((cwd_key.clone(), val));
                            val
                        }
                    };
                    if !is_git {
                        let msg = if cwd_key.is_empty() {
                            "Elige una carpeta con repositorio Git para usar copia aparte."
                        } else {
                            "Esta carpeta no tiene .git; la tarea se ejecutará directamente en ella."
                        };
                        ui.label(egui::RichText::new(msg).font(body(11.5)).color(ATTN));
                    }
                }
            }
            ui.horizontal(|ui| {
                let attach_btn = ui.add_enabled(
                    app.ui.file_picker.is_none(),
                    egui::Button::new(egui::RichText::new("+ Adjuntar").font(medium(12.0)).color(INK2))
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0, RULE))
                        .corner_radius(CornerRadius::same(R)),
                ).on_hover_text("Adjuntar archivos o imágenes (también puedes pegar con Ctrl+V o arrastrar)");
                if attach_btn.clicked() {
                    #[cfg(windows)]
                    {
                        let init_dir = task
                            .and_then(|(tid, _)| app.sup.tasks.get(tid))
                            .and_then(|t| t.os("cwd"))
                            .filter(|d| !d.is_empty())
                            .unwrap_or_else(|| app.ui.cwd.clone());
                        app.ui.file_picker = Some((draft_id.clone(), pick_files(init_dir, ui.ctx().clone())));
                    }
                    #[cfg(not(windows))]
                    {
                        app.ui.file_attach_edit = !app.ui.file_attach_edit;
                    }
                }
                // In a conversation: which brain + model answers the next message (auto-adapts or manual).
                let mut active_task_cwd = String::new();
                let mut active_task_mode = "build".to_string();
                let mut active_task_kind = String::new();
                if let Some((tid, _)) = task {
                    let tasks_now = task_list(app);
                    let active = tasks_now.iter().find(|t| t.s("id") == tid);
                    let cur_brain = active.map(|t| t.s("brain")).unwrap_or_default();
                    let mode = active.map(|t| t.s("mode")).unwrap_or_default();
                    active_task_cwd = active.map(|t| t.s("cwd")).unwrap_or_default();
                    if !mode.is_empty() {
                        active_task_mode = mode.clone();
                    }
                    active_task_kind = active.map(|t| t.s("kind")).unwrap_or_default();
                    let default_auto = active.is_some_and(|t| !t.s("route_cand").is_empty());
                    let picked = app.ui.reply_brain.get(tid).cloned().unwrap_or_default();
                    let name_of = |id: &str| {
                        if id == "parallel" {
                            "Motor paralelo".to_string()
                        } else {
                            brains.iter().find(|b| b.s("id") == id).map(|b| b.s("name")).unwrap_or_else(|| id.to_string())
                        }
                    };
                    let shown = match picked.as_str() {
                        "auto" => "Auto para el próximo turno".to_string(),
                        "parallel" => "Orquestar en paralelo".to_string(),
                        "" if default_auto => format!("Auto · actual: {}", name_of(&cur_brain)),
                        "" | "current" => format!("Sigue: {}", name_of(&cur_brain)),
                        _ => format!("Cambia a: {}", name_of(&picked)),
                    };
                    let mut choice = picked.clone();
                    let combo_w = (ui.available_width() - 210.0).clamp(110.0, 200.0);
                    egui::ComboBox::from_id_salt(("reply_brain", tid)).selected_text(egui::RichText::new(shown).font(body(12.5))).width(combo_w).show_ui(ui, |ui| {
                        if cur_brain != "parallel" {
                            ui.selectable_value(&mut choice, "auto".into(), "Elegir según el próximo turno");
                            ui.selectable_value(&mut choice, "parallel".into(), "Orquestar en paralelo (agentes)");
                        }
                        ui.selectable_value(&mut choice, "current".into(), format!("Seguir con {}", name_of(&cur_brain)));
                        for b in &brains {
                            let id = b.s("id");
                            if id == cur_brain {
                                continue;
                            }
                            let ok = chain.iter().any(|e| e.s("id") == id && e.b("available", false));
                            let can = crate::tasks::can_run_task_on(&b.s("kind"), &mode);
                            let why = chain.iter().find(|e| e.s("id") == id).map(|e| e.s("reason")).unwrap_or_default();
                            let label = if ok && can { b.s("name") } else if !can { format!("{} (no edita archivos)", b.s("name")) } else { format!("{} ({why})", b.s("name")) };
                            ui.add_enabled_ui(ok && can, |ui| {
                                ui.selectable_value(&mut choice, id.clone(), label);
                            });
                        }
                    });
                    if choice != picked {
                        if let Some(ref det) = live_intent {
                            app.ui.auto_intent_dismissed.insert(draft_id.clone(), det.key.clone());
                        }
                        app.ui.reply_brain.insert(tid.to_string(), choice.clone());
                        app.ui.reply_model.remove(tid);
                    }
                    // Dynamic model selector for the effective target brain of this reply.
                    let eff_brain = if choice.is_empty() || choice == "current" {
                        cur_brain.as_str()
                    } else {
                        choice.as_str()
                    };
                    let models = brain_models(&brains, eff_brain);
                    if !models.is_empty() && eff_brain != "parallel" {
                        let cur_model = app.ui.reply_model.get(tid).cloned().unwrap_or_default();
                        let sel_lbl = if cur_model.is_empty() {
                            "Modelo: igual".to_string()
                        } else {
                            crate::gui::home::model_label(&cur_model)
                        };
                        let mut m_choice = cur_model.clone();
                        egui::ComboBox::from_id_salt(("reply_model", tid))
                            .selected_text(egui::RichText::new(sel_lbl).font(mono(11.5)).color(if cur_model.is_empty() { INK2 } else { accent }))
                            .width(136.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut m_choice, String::new(), "Mantener modelo");
                                for m in &models {
                                    ui.selectable_value(&mut m_choice, m.clone(), crate::gui::home::model_label(m));
                                }
                            });
                        if m_choice != cur_model {
                            if let Some(ref det) = live_intent {
                                app.ui.auto_intent_dismissed.insert(draft_id.clone(), det.key.clone());
                            }
                            if m_choice.is_empty() {
                                app.ui.reply_model.remove(tid);
                            } else {
                                app.ui.reply_model.insert(tid.to_string(), m_choice);
                            }
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let empty = (if fresh { app.ui.prompt.trim().is_empty() } else { draft.reply.trim().is_empty() }) && draft.files.is_empty();
                    let queued_here = task.is_some_and(|(id, _)| app.ui.queued_follow_up.as_ref().is_some_and(|q| q.task_id == id));
                    let dark = Color32::from_rgb(0x1C, 0x16, 0x0A);

                    if waiting {
                        let can_redirect = !empty && !queued_here && app.ui.pending_open.is_none();
                        let mut redirect_clicked = false;
                        if !empty {
                            let (sr, sresp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
                            let sp = ui.painter();
                            sp.circle_filled(sr.center(), 15.0, if can_redirect { MASTER } else { RAISED2 });
                            paint_icon(sp, sr.shrink(9.0), Icon::Up, if can_redirect { dark } else { INK3 });
                            let shint = if queued_here { "Enviando al interrumpir…" } else { "Interrumpir y enviar (Enter)" };
                            sresp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, can_redirect, shint));
                            redirect_clicked = sresp.on_hover_text(shint).clicked();
                        }

                        let (rr, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
                        let p = ui.painter();
                        p.circle_filled(rr.center(), 15.0, if interruptible { ATTN } else { RAISED2 });
                        p.rect_filled(Rect::from_center_size(rr.center(), Vec2::splat(10.0)), 2.0, if interruptible { dark } else { INK3 });
                        let hint = if interruptible { "Interrumpir (Esc)" } else { "Interrumpiendo…" };
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, interruptible, hint));
                        let stop_clicked = resp.on_hover_text(hint).clicked();

                        if let Some((id, _)) = task {
                            if interruptible && (stop_clicked || interrupt_key) {
                                if app.ui.queued_follow_up.as_ref().is_some_and(|q| q.task_id == id) {
                                    app.ui.queued_follow_up = None;
                                }
                                app.ui.focus_composer = true;
                                ui.ctx().request_repaint();
                                app.post(&format!("api/tasks/{id}/stop"), json!({}), |r| (r.os("error").unwrap_or_default(), ALARM));
                            } else if can_redirect && (redirect_clicked || send_key) {
                                let brain = app.ui.reply_brain.remove(id).unwrap_or_default();
                                let model = app.ui.reply_model.remove(id).unwrap_or_default();
                                app.ui.queued_follow_up = Some(QueuedFollowUp {
                                    task_id: id.to_string(),
                                    text: draft.reply.trim().to_string(),
                                    files: draft.files.clone(),
                                    brain,
                                    model,
                                });
                                ui.ctx().request_repaint();
                                if interruptible {
                                    app.post(&format!("api/tasks/{id}/stop"), json!({}), |r| (r.os("error").unwrap_or_default(), ALARM));
                                }
                            }
                        }

                        if ui.available_width() > 210.0 {
                            let label = if !empty {
                                "Enter interrumpe y envía · Esc detiene"
                            } else {
                                "Esc interrumpe · escribe para redirigir"
                            };
                            ui.label(faint(label));
                        }
                    } else {
                        let disabled = empty || app.ui.pending_open.is_some();
                        let (rr, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
                        let p = ui.painter();
                        p.circle_filled(rr.center(), 15.0, if disabled { RAISED2 } else { accent });
                        paint_icon(p, rr.shrink(9.0), Icon::Up, if disabled { INK3 } else { dark });
                        let hint = "Enviar (Enter)";
                        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, !disabled, hint));
                        let clicked = resp.on_hover_text(hint).clicked();
                        let go = (clicked || send_key) && !disabled;
                        if go {
                            ui.ctx().request_repaint();
                            app.ui.auto_intent_applied.remove(&draft_id);
                            app.ui.auto_intent_prev.remove(&draft_id);
                            app.ui.auto_intent_dismissed.remove(&draft_id);
                            match task {
                                None => {
                                    let body = json!({
                                        "prompt": app.ui.prompt.trim(),
                                        "brain": app.ui.brain,
                                        "model": app.ui.model,
                                        "cwd": app.ui.cwd.trim(),
                                        "mode": if app.ui.build { "build" } else { "plan" },
                                        "worktree": app.ui.worktree,
                                        "files": draft.files,
                                    });
                                    submit_chat(app, "api/tasks", body);
                                }
                                Some((id, _)) => {
                                    let brain = app.ui.reply_brain.remove(id).unwrap_or_default();
                                    let model = app.ui.reply_model.remove(id).unwrap_or_default();
                                    if brain == "parallel" && active_task_kind != "parallel" {
                                        let mode = live_intent
                                            .as_ref()
                                            .and_then(|i| i.build_mode)
                                            .map(|b| if b { "build".to_string() } else { "plan".to_string() })
                                            .unwrap_or_else(|| active_task_mode.clone());
                                        let body = json!({
                                            "prompt": draft.reply.trim(),
                                            "brain": "parallel",
                                            "cwd": if active_task_cwd.is_empty() { app.ui.cwd.trim().to_string() } else { active_task_cwd.clone() },
                                            "mode": mode,
                                            "worktree": app.ui.worktree,
                                            "files": draft.files,
                                        });
                                        app.say("Started parallel orchestration in a new thread", INFO);
                                        submit_chat(app, "api/tasks", body);
                                    } else {
                                        let safe_brain = if brain == "auto" { String::new() } else { brain };
                                        let body = json!({"text": draft.reply.trim(), "files": draft.files, "brain": safe_brain, "model": model});
                                        submit_chat(app, &format!("api/tasks/{id}/message"), body);
                                    }
                                }
                            }
                        }
                        if ui.available_width() > 210.0 {
                            ui.label(faint("Enter envía · Shift+Enter salto"));
                        }
                    }
                });
            });
            if let Some((tid, _)) = task {
                if app.ui.queued_follow_up.as_ref().is_some_and(|q| q.task_id == tid) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("Interrumpiendo para enviar tu mensaje…").font(body(12.0)).color(ATTN));
                        if quiet(ui, "Cancelar").clicked() {
                            app.ui.queued_follow_up = None;
                        }
                    });
                }
            }
            if app.ui.file_attach_edit {
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut app.ui.file_attach_buf).hint_text("Ruta del archivo a adjuntar…").desired_width((ui.available_width() - 80.0).max(1.0)).font(mono(12.0)));
                    let add_it = quiet(ui, "Agregar").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if add_it {
                        let p = app.ui.file_attach_buf.trim().to_string();
                        if !p.is_empty() && !draft.files.contains(&p) {
                            draft.files.push(p);
                        }
                        app.ui.file_attach_buf.clear();
                        app.ui.file_attach_edit = false;
                    }
                });
            }
            if fresh && app.ui.cwd_edit {
                ui.label(faint("Carpeta del proyecto"));
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(&mut app.ui.cwd).hint_text("C:\\ruta\\al\\proyecto").desired_width((ui.available_width() - 70.0).max(1.0)).font(mono(12.0)));
                    if r.lost_focus() || quiet(ui, "Listo").clicked() {
                        remember_folder(&app.ui.cwd);
                        app.ui.cwd_edit = false;
                    }
                });
            }
        });
    app.ui.drafts.insert(draft_id, draft);
}

fn remember_brain(brain: &str) {
    let path = crate::util::data_dir().join("gui.json");
    let mut config = crate::util::read_json(&path).unwrap_or_else(|| json!({}));
    config["brain"] = json!(brain);
    let _ = crate::util::write_json(&path, &config);
}

fn remember_folder(folder: &str) {
    let path = crate::util::data_dir().join("gui.json");
    let mut config = crate::util::read_json(&path).unwrap_or_else(|| json!({}));
    config["cwd"] = json!(folder);
    if !folder.trim().is_empty() {
        let previous = config.arr("recent_cwd");
        let recent = std::iter::once(folder)
            .chain(
                previous
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|old| *old != folder),
            )
            .take(8)
            .collect::<Vec<_>>();
        config["recent_cwd"] = json!(recent);
    }
    let _ = crate::util::write_json(&path, &config);
}

#[cfg(windows)]
fn pick_folder(initial: String, ctx: egui::Context) -> std::sync::mpsc::Receiver<Option<String>> {
    use std::os::windows::process::CommandExt;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let script = r#"
            [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
            Add-Type -AssemblyName System.Windows.Forms
            $dialog = New-Object System.Windows.Forms.FolderBrowserDialog
            $dialog.Description = 'Selecciona la carpeta del proyecto'
            if ([IO.Directory]::Exists($env:METABRAIN_PICKER_INITIAL)) {
                $dialog.SelectedPath = $env:METABRAIN_PICKER_INITIAL
            }
            $owner = New-Object System.Windows.Forms.Form
            $owner.TopMost = $true
            try {
                if ($dialog.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) {
                    [Console]::Write($dialog.SelectedPath)
                }
            } finally { $dialog.Dispose(); $owner.Dispose() }
        "#;
        let result = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
            .env("METABRAIN_PICKER_INITIAL", initial)
            .creation_flags(0x08000000)
            .output()
            .ok()
            .filter(|out| out.status.success() && !out.stdout.is_empty())
            .and_then(|out| String::from_utf8(out.stdout).ok());
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    rx
}

#[cfg(windows)]
fn pick_files(initial: String, ctx: egui::Context) -> std::sync::mpsc::Receiver<Vec<String>> {
    use std::os::windows::process::CommandExt;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let script = r#"
            [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
            Add-Type -AssemblyName System.Windows.Forms
            $dialog = New-Object System.Windows.Forms.OpenFileDialog
            $dialog.Title = 'Selecciona archivos para adjuntar'
            $dialog.Multiselect = $true
            $dialog.Filter = 'Todos los archivos (*.*)|*.*|Imágenes|*.png;*.jpg;*.jpeg;*.gif;*.webp;*.bmp'
            if ([IO.Directory]::Exists($env:METABRAIN_PICKER_INITIAL)) {
                $dialog.InitialDirectory = $env:METABRAIN_PICKER_INITIAL
            }
            $owner = New-Object System.Windows.Forms.Form
            $owner.TopMost = $true
            try {
                if ($dialog.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) {
                    foreach ($f in $dialog.FileNames) { [Console]::WriteLine($f) }
                }
            } finally { $dialog.Dispose(); $owner.Dispose() }
        "#;
        let files = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-STA", "-Command", script])
            .env("METABRAIN_PICKER_INITIAL", initial)
            .creation_flags(0x08000000)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|s| {
                s.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let _ = tx.send(files);
        ctx.request_repaint();
    });
    rx
}

fn has_git_repo(dir: &str) -> bool {
    let trimmed = dir.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut cur = Some(Path::new(trimmed));
    while let Some(p) = cur {
        if p.join(".git").exists() {
            return true;
        }
        cur = p.parent();
    }
    false
}

pub(super) struct ChatSubmission {
    origin: String,
    body: Value,
    result: std::sync::mpsc::Receiver<Value>,
}

fn apply_chat_submit(ui: &mut UiState, origin: &str, sent: &Value, result: &Value) {
    let same_chat = ui.sel_task == origin;
    ui.focus_composer = same_chat;
    // Task metadata returned by POST /api/tasks includes `"error": ""`; only a non-empty error failed.
    if result.os("error").is_some() {
        return;
    }
    if let Some(id) = result.os("id") {
        if same_chat {
            ui.sel_task = id;
        }
        if sent.s("prompt") == ui.prompt.trim() {
            ui.prompt.clear();
        }
    }
    if let Some(draft) = ui.drafts.get_mut(origin) {
        if sent.get("text").is_some() && sent.s("text") == draft.reply.trim() {
            draft.reply.clear();
        }
        let files = sent.arr("files");
        draft
            .files
            .retain(|path| !files.iter().any(|file| file.as_str() == Some(path)));
    }
}

fn finish_chat_submit(app: &mut App) {
    use std::sync::mpsc::TryRecvError;
    let Some(pending) = app.ui.pending_open.as_ref() else {
        return;
    };
    let result = match pending.result.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => json!({"error": "Se perdió la conexión al enviar."}),
    };
    let pending = app.ui.pending_open.take().unwrap();
    apply_chat_submit(&mut app.ui, &pending.origin, &pending.body, &result);
}

fn drain_queued_follow_up(app: &mut App, ctx: &egui::Context) {
    if app.ui.pending_open.is_some() {
        return;
    }
    let Some(task_id) = app.ui.queued_follow_up.as_ref().map(|q| q.task_id.clone()) else {
        return;
    };
    let Some(task) = app.sup.tasks.get(&task_id) else {
        app.ui.queued_follow_up = None;
        return;
    };
    if crate::tasks::turn_busy(&task.s("status")) {
        ctx.request_repaint_after(std::time::Duration::from_millis(80));
        return;
    }
    if let Some(queued) = app.ui.queued_follow_up.take() {
        let body = json!({"text": queued.text, "files": queued.files, "brain": queued.brain, "model": queued.model});
        submit_chat_for(
            app,
            queued.task_id.clone(),
            &format!("api/tasks/{}/message", queued.task_id),
            body,
        );
        ctx.request_repaint();
    }
}

/// file:// URI egui's loader understands (Windows needs file:///C:/...).
pub(super) fn file_uri(path: &str) -> String {
    let p = path.replace('\\', "/");
    if p.starts_with('/') {
        format!("file://{p}")
    } else {
        format!("file:///{p}")
    }
}

/// Save a pasted picture as PNG under the data folder.
fn save_pasted(im: &egui::ColorImage) -> Option<String> {
    let dir = crate::util::data_dir().join("pegados");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!(
        "pegado-{}.png",
        (crate::util::now() * 1000.0) as u64
    ));
    let mut raw = Vec::with_capacity(im.pixels.len() * 4);
    for c in &im.pixels {
        raw.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    let buf = image::RgbaImage::from_raw(im.size[0] as u32, im.size[1] as u32, raw)?;
    buf.save(&path).ok()?;
    Some(path.display().to_string())
}

/// True once per Ctrl+V press (edge), read from the keyboard on Windows.
fn ctrl_v_pressed() -> bool {
    #[cfg(windows)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        static WAS: AtomicBool = AtomicBool::new(false);
        let down = unsafe {
            (GetAsyncKeyState(0x11) as u16 & 0x8000) != 0
                && (GetAsyncKeyState(0x56) as u16 & 0x8000) != 0
        };
        let was = WAS.swap(down, Ordering::SeqCst);
        down && !was
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Files copied in Explorer become attachments (by path).
fn paste_attachments() -> Vec<String> {
    let Ok(mut cb) = arboard::Clipboard::new() else {
        return vec![];
    };
    if let Ok(list) = cb.get().file_list() {
        let v: Vec<String> = list.into_iter().map(|p| p.display().to_string()).collect();
        if !v.is_empty() {
            return v;
        }
    }
    // Pictures arrive through egui's own PasteImage event (reading them here too doubled them).
    vec![]
}

pub(super) fn submit_chat(app: &mut App, path: &str, body: Value) {
    let origin = app.ui.sel_task.clone();
    submit_chat_for(app, origin, path, body);
}

fn submit_chat_for(app: &mut App, origin: String, path: &str, body: Value) {
    if app.ui.pending_open.is_some() {
        return;
    }
    let (tx, result) = std::sync::mpsc::channel();
    app.ui.pending_open = Some(ChatSubmission {
        origin,
        body: body.clone(),
        result,
    });
    app.post(path, body, move |r| {
        let _ = tx.send(r.clone());
        (r.os("error").unwrap_or_default(), ALARM)
    });
}

#[cfg(test)]
mod tests {
    use super::super::ChatDraft;
    use super::*;

    #[test]
    fn enter_submits_and_shift_enter_inserts_a_line_break() {
        let ctx = egui::Context::default();
        crate::gui::theme::install(&ctx);
        let id = egui::Id::unique("test_chat_editor");
        let mut draft = String::from("Primer mensaje");
        let mut frame = |input| {
            let mut send = false;
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    send |= chat_editor(ui, &mut draft, id, true);
                });
            });
            output.textures_delta.clear();
            send
        };
        ctx.memory_mut(|m| m.request_focus(id));
        assert!(!frame(egui::RawInput::default()));
        let press_enter = |modifiers| egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        };
        assert!(frame(press_enter(egui::Modifiers::NONE)));
        assert!(!frame(press_enter(egui::Modifiers::SHIFT)));
        assert_eq!(draft, "Primer mensaje\n");
    }

    #[test]
    fn has_git_repo_checks_ancestors_without_panicking() {
        assert!(!has_git_repo(""));
        let root = std::env::temp_dir().join(format!("mb_git_check_{}", std::process::id()));
        let nested = root.join("sub");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&nested).unwrap();
        assert!(!has_git_repo(&nested.to_string_lossy()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert!(has_git_repo(&nested.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn new_chat_submit_opens_thread_and_clears_prompt_even_with_empty_error_field() {
        let mut ui = UiState {
            prompt: "Hola mundo".into(),
            ..Default::default()
        };
        ui.drafts.insert(
            String::new(),
            ChatDraft {
                reply: String::new(),
                files: vec!["C:\\tmp\\a.png".into()],
            },
        );
        let sent = json!({"prompt": "Hola mundo", "files": ["C:\\tmp\\a.png"]});
        // POST /api/tasks returns task metadata where `"error": ""`.
        let result = json!({"id": "abc1234567", "status": "queued", "error": ""});
        apply_chat_submit(&mut ui, "", &sent, &result);
        assert_eq!(ui.sel_task, "abc1234567");
        assert!(ui.prompt.is_empty());
        assert!(ui.drafts.get("").unwrap().files.is_empty());
        assert!(ui.focus_composer);
    }

    #[test]
    fn new_chat_submit_preserves_prompt_on_real_error() {
        let mut ui = UiState {
            prompt: "Hola mundo".into(),
            ..Default::default()
        };
        let sent = json!({"prompt": "Hola mundo", "files": []});
        let result = json!({"error": "Ningún cerebro disponible."});
        apply_chat_submit(&mut ui, "", &sent, &result);
        assert!(ui.sel_task.is_empty());
        assert_eq!(ui.prompt, "Hola mundo");
    }

    #[test]
    fn conv_dots_button_opens_menu_without_triggering_row_open() {
        let ctx = egui::Context::default();
        crate::gui::theme::install(&ctx);
        let task = json!({"id": "t1", "title": "Conversación en proyecto", "status": "done", "updated": 100.0});
        let meta = json!({"project": "METABRAIN"});
        let projects = vec!["METABRAIN".to_string()];
        let groups: Vec<String> = vec![];

        let run_frame = |input: egui::RawInput| -> (Pos2, bool, bool) {
            let mut dots_pos = Pos2::ZERO;
            let mut menu_is_open = false;
            let mut acts: Vec<ConvAct> = vec![];
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.set_width(240.0);
                    let (r, resp) =
                        ui.allocate_exact_size(Vec2::new(240.0, 30.0), Sense::click_and_drag());
                    let (dots, dresp, open) = conv_row_interact_dots(ui, r, true, "t1");
                    dots_pos = dots.center();
                    menu_is_open = open;
                    if resp.clicked() && !dresp.clicked() {
                        acts.push(ConvAct::Open("t1".into()));
                    }
                    egui::Popup::menu(&dresp).show(|ui| {
                        conv_menu(ui, "t1", &task, &meta, &projects, &groups, &mut acts);
                    });
                });
            });
            output.textures_delta.clear();
            let opened_row = acts.iter().any(|a| matches!(a, ConvAct::Open(_)));
            (dots_pos, menu_is_open, opened_row)
        };

        // Frame 1: initial layout to discover dots_pos.
        let (dots_pos, menu_is_open, opened_row) = run_frame(egui::RawInput::default());
        assert!(!menu_is_open);
        assert!(!opened_row);

        // Frame 2: pointer down on dots.
        run_frame(egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(dots_pos),
                egui::Event::PointerButton {
                    pos: dots_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        });
        // Frame 3: pointer up on dots (completes click and opens popup).
        let (_, _, opened_row) = run_frame(egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(dots_pos),
                egui::Event::PointerButton {
                    pos: dots_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        });
        assert!(
            !opened_row,
            "clicking the ⋯ button must not open the conversation"
        );

        // Frame 4: popup should now report open.
        let (_, menu_is_open, _) = run_frame(egui::RawInput {
            events: vec![egui::Event::PointerMoved(dots_pos)],
            ..Default::default()
        });
        assert!(
            menu_is_open,
            "clicking the ⋯ button must open the conversation menu"
        );
    }

    #[test]
    fn subchat_map_orders_waves_and_merges_live_child_tasks() {
        let run = ParallelRun {
            id: "run_p1".into(),
            prompt: "Plan paralelo".into(),
            cwd: String::new(),
            mode: "build".into(),
            worktree: false,
            strategy: "dag".into(),
            director: "claude".into(),
            director_name: "Claude Code".into(),
            status: "running".into(),
            waves_total: 2,
            branches: vec![
                crate::parallel::BranchState {
                    id: "b2".into(),
                    title: "Implementar cambios".into(),
                    role: "implement".into(),
                    tier: 1,
                    mode: "build".into(),
                    deps: vec!["b1".into()],
                    files: vec![],
                    prompt: String::new(),
                    worktree: false,
                    task_id: String::new(),
                    brain: String::new(),
                    brain_name: String::new(),
                    model: String::new(),
                    status: "pending".into(),
                    summary: String::new(),
                    tokens: 0,
                    usd: 0.0,
                    error: String::new(),
                    wave: 1,
                    started_at: 0.0,
                    finished_at: 0.0,
                },
                crate::parallel::BranchState {
                    id: "b1".into(),
                    title: "Explorar módulos".into(),
                    role: "explore".into(),
                    tier: 2,
                    mode: "plan".into(),
                    deps: vec![],
                    files: vec![],
                    prompt: String::new(),
                    worktree: false,
                    task_id: "child_1".into(),
                    brain: "gemini".into(),
                    brain_name: "Gemini".into(),
                    model: "gemini-4-neon-medium".into(),
                    status: "queued".into(),
                    summary: String::new(),
                    tokens: 0,
                    usd: 0.0,
                    error: String::new(),
                    wave: 0,
                    started_at: 0.0,
                    finished_at: 0.0,
                },
            ],
            total_tokens: 0,
            total_usd: 0.0,
            created: 100.0,
            updated: 100.0,
            error: String::new(),
        };
        let child_tasks = vec![json!({
            "id": "child_1",
            "parent_run": "run_p1",
            "title": "Explorar módulos",
            "status": "running",
            "brain": "gemini",
            "brain_name": "Gemini",
            "tokens": 2500.0
        })];
        let map = build_subchat_map(&child_tasks, &[run]);
        let list = map.get("run_p1").expect("subchats for run_p1");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].wave, 1);
        assert_eq!(list[0].task_id, "child_1");
        assert_eq!(list[0].status, "running");
        assert_eq!(list[0].tokens, 2500.0);
        assert_eq!(list[1].wave, 2);
        assert_eq!(list[1].status, "pending");
    }
}
