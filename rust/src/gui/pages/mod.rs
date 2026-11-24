//! Every page besides Inicio, plus the permission inbox that floats over all of them.

mod brains;
mod chat;
pub(crate) mod chat_intent;
mod system;
mod thread;
mod usage;

pub use brains::{Choice, Wizard};
pub use chat::task_list;
pub use system::approvals_overlay;

use super::theme::*;
use super::{App, Page};
use crate::util::J;
use eframe::egui::{self, Vec2};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Form and selection state that has to survive between frames.
pub struct UiState {
    /// Brain chosen for the next turn of each conversation (empty = keep the current one).
    pub reply_brain: HashMap<String, String>,
    /// Model override chosen/auto-detected for the next turn of each conversation.
    pub reply_model: HashMap<String, String>,
    /// Per-conversation metadata (pin, name, group, project, archive, seen), saved in convs.json.
    pub convs: Value,
    pub conv_rename: Option<(String, String)>,
    pub conv_newgroup: Option<(String, String, bool)>,
    pub show_archived: bool,
    pub confirm_delete: Option<String>,
    /// Conversation list folded away in Chat.
    pub conv_closed: bool,
    /// User-toggled expand/collapse state for orchestrated subchat trees (parent task id -> open).
    pub subchat_open: HashMap<String, bool>,
    /// Last auto-detected model/brain intent key applied per draft ("" = new chat, or task id).
    pub auto_intent_applied: HashMap<String, String>,
    /// Previous (brain, model, build) before live auto-intent mutated a draft, so deleting the directive reverts cleanly.
    pub auto_intent_prev: HashMap<String, (String, String, bool)>,
    /// Auto-detected intent key manually dismissed by the user for a draft so we don't fight clicks.
    pub auto_intent_dismissed: HashMap<String, String>,
    /// Timestamp of last automatic model/brain switch (drives smooth highlight pulse animation).
    pub auto_switch_at: f64,
    pub decided: std::collections::HashSet<String>,
    /// Selected node on the Inicio map ("core" = summary).
    pub sel: String,
    pub graph: super::home::Graph,
    pub nav_open: bool,
    pub sys_tab: Page,
    pub drawer: bool,
    pub palette: Option<String>,
    pub pal_sel: usize,
    pub wizard: Option<Wizard>,
    /// Box being dragged in the hierarchy diagram, with the grab offset.
    pub hdrag: Option<(String, eframe::egui::Vec2)>,
    pub cwd_edit: bool,
    folder_picker: Option<std::sync::mpsc::Receiver<Option<String>>>,
    file_picker: Option<(String, std::sync::mpsc::Receiver<Vec<String>>)>,
    file_attach_edit: bool,
    file_attach_buf: String,
    cwd_git_cache: Option<(String, bool)>,
    confirm_discard: Option<String>,
    pending_open: Option<chat::ChatSubmission>,
    queued_follow_up: Option<QueuedFollowUp>,
    focus_composer: bool,
    // tasks
    pub prompt: String,
    pub brain: String,
    /// Explicit model override for a new conversation (empty = brain default / auto).
    pub model: String,
    pub build: bool,
    pub cwd: String,
    pub worktree: bool,
    drafts: std::collections::HashMap<String, ChatDraft>,
    pub sel_task: String,
    pub events: Vec<Value>,
    pub ev_task: String,
    pub ev_off: u64,
    pub diff: Option<DiffView>,
    // router
    pub rt_task: String,
    pub rt_project: String,
    pub rt_result: Value,
    pub rt_pending: Option<std::sync::Arc<std::sync::Mutex<Option<Value>>>>,
    // usage
    pub days: i64,
    // brains
    pub confirm_remove: Option<String>,
    // settings
    pub cfg_text: String,
    pub cfg_open: bool,
    pub cfg_err: String,
    pub succ: Option<(f64, f64, f64)>,
    autostart: Option<crate::app::AutostartStatus>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    pub total_lines: usize,
}

pub enum DiffView {
    Loading {
        task_id: String,
        rx: std::sync::mpsc::Receiver<String>,
    },
    Ready {
        task_id: String,
        text: String,
        stats: DiffStats,
    },
}

#[derive(Default)]
struct ChatDraft {
    reply: String,
    files: Vec<String>,
}

struct QueuedFollowUp {
    task_id: String,
    text: String,
    files: Vec<String>,
    brain: String,
    model: String,
}

impl Default for UiState {
    fn default() -> Self {
        let gui_cfg = crate::util::read_json(&crate::util::data_dir().join("gui.json"));
        let cwd = gui_cfg.as_ref().map(|v| v.s("cwd")).unwrap_or_default();
        let saved_brain = gui_cfg
            .as_ref()
            .map(|v| v.s("brain"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "auto".into());
        UiState {
            reply_brain: HashMap::new(),
            reply_model: HashMap::new(),
            conv_closed: false,
            subchat_open: HashMap::new(),
            auto_intent_applied: HashMap::new(),
            auto_intent_prev: HashMap::new(),
            auto_intent_dismissed: HashMap::new(),
            auto_switch_at: 0.0,
            convs: crate::util::read_json(&crate::util::data_dir().join("convs.json"))
                .unwrap_or_else(|| json!({})),
            conv_rename: None,
            conv_newgroup: None,
            show_archived: false,
            confirm_delete: None,
            decided: Default::default(),
            sel: "core".into(),
            graph: Default::default(),
            nav_open: gui_cfg
                .as_ref()
                .map(|v| v.b("nav_open", true))
                .unwrap_or(true),
            sys_tab: Page::Router,
            drawer: false,
            palette: None,
            pal_sel: 0,
            wizard: None,
            hdrag: None,
            cwd_edit: false,
            folder_picker: None,
            file_picker: None,
            file_attach_edit: false,
            file_attach_buf: String::new(),
            cwd_git_cache: None,
            confirm_discard: None,
            pending_open: None,
            queued_follow_up: None,
            focus_composer: false,
            prompt: String::new(),
            brain: saved_brain,
            model: String::new(),
            build: true,
            cwd,
            worktree: false,
            drafts: Default::default(),
            sel_task: String::new(),
            events: vec![],
            ev_task: String::new(),
            ev_off: 0,
            diff: None,
            rt_task: String::new(),
            rt_project: String::new(),
            rt_result: Value::Null,
            rt_pending: None,
            days: 7,
            confirm_remove: None,
            cfg_text: String::new(),
            cfg_open: false,
            cfg_err: String::new(),
            succ: None,
            autostart: None,
        }
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui, snap: &Value, page: Page) {
    if super::shell::is_system(page) {
        super::shell::system_tabs(app, ui, snap);
        ui.label(dim(page_sub(page)));
        ui.add_space(10.0);
    } else {
        title(ui, page_title(page), page_sub(page));
    }
    match page {
        Page::Tasks => chat::tasks(app, ui, snap),
        Page::Brains => brains::brains(app, ui, snap),
        Page::Usage => usage::usage(app, ui, snap),
        Page::Alarms => system::alarms(app, ui, snap),
        Page::Router => system::router(app, ui),
        Page::Memory => system::memory(app, ui, snap),
        Page::Services => system::services(app, ui, snap),
        Page::Agents => system::agents(app, ui, snap),
        Page::Extensions => system::extensions(app, ui),
        Page::Settings => system::settings(app, ui, snap),
        Page::Home => {}
    }
}

fn page_title(p: Page) -> &'static str {
    match p {
        Page::Home => "Home",
        Page::Tasks => "Chat",
        Page::Brains => "Brains",
        Page::Usage => "Usage & Savings",
        Page::Alarms => "Alarms",
        Page::Router => "Router",
        Page::Memory => "Memory",
        Page::Services => "Services",
        Page::Agents => "Agents",
        Page::Extensions => "Extensions",
        Page::Settings => "Settings",
    }
}

fn page_sub(p: Page) -> &'static str {
    match p {
        Page::Tasks => "",
        Page::Brains => "Connected accounts, CLI agents, and API models",
        Page::Usage => "Token spend and estimated routing savings",
        Page::Alarms => "Active alerts and recent supervisor events",
        Page::Router => "Adaptive model routing learned per project",
        Page::Memory => "Shared file locks, handoff notes, and supervisor diary",
        Page::Services => "Monitored background services and host utilization",
        Page::Agents => "Active sessions and subagents per brain",
        Page::Extensions => "MCP servers and skills installed per brain",
        Page::Settings => "Operation modes, autopilot, budgets, and safety policies",
        Page::Home => "",
    }
}

pub fn title(ui: &mut egui::Ui, t: &str, sub: &str) {
    ui.label(egui::RichText::new(t).font(cond(26.0)).color(INK));
    if !sub.is_empty() {
        ui.label(dim(sub));
    }
    ui.add_space(14.0);
}

fn empty(ui: &mut egui::Ui, text: &str) {
    card(ui, None, |ui| {
        ui.set_width(ui.available_width());
        ui.add_space(6.0);
        ui.label(dim(text));
        ui.add_space(6.0);
    });
}

/// Two-column helper: fixed widths on wide windows, stacked vertically when narrow.
fn columns2(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    let gap = 16.0;
    let avail = ui.available_width();
    if avail < 620.0 {
        left(ui);
        ui.add_space(12.0);
        right(ui);
        return;
    }
    let w = (avail - gap) / 2.0;
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(w, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(w);
                left(ui);
            },
        );
        ui.add_space(gap - 8.0);
        ui.allocate_ui_with_layout(
            Vec2::new(w, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(w);
                right(ui);
            },
        );
    });
}

fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        p.to_string()
    } else {
        format!("…/{}", parts[parts.len() - 2..].join("/"))
    }
}

fn hhmm(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}
