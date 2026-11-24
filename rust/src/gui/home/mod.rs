//! Inicio: the brain map (each brain a ring showing how much quota it has used, live subagents
//! as satellites, delegations as arcs), a detail panel for the selected node with its hierarchy
//! controls, and the command hierarchy strip.

mod detail;
mod hierarchy;
mod map;

pub use map::Graph;

use super::theme::*;
use super::App;
use crate::util::J;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

fn de_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct HomeSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub overall: String,
    #[serde(default, deserialize_with = "de_default")]
    pub brains: Vec<BrainSnap>,
    #[serde(default, deserialize_with = "de_default")]
    pub chain: Vec<ChainSnap>,
    #[serde(default, deserialize_with = "de_default")]
    pub alarms: Vec<AlarmSnap>,
    #[serde(default, deserialize_with = "de_default")]
    pub diary: Vec<DiarySnap>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct BrainSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub id: String,
    #[serde(default, deserialize_with = "de_default")]
    pub name: String,
    #[serde(default, deserialize_with = "de_default")]
    pub active: bool,
    #[serde(default, deserialize_with = "de_default")]
    pub warning: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, deserialize_with = "de_default")]
    pub models: Vec<String>,
    #[serde(default)]
    pub balance: Option<BalanceSnap>,
    #[serde(default, deserialize_with = "de_default")]
    pub quota: Vec<QuotaSnap>,
    #[serde(default, deserialize_with = "de_default")]
    pub effective: EffectiveSnap,
    #[serde(default, deserialize_with = "de_default")]
    pub state: StateSnap,
    #[serde(default, deserialize_with = "de_default")]
    pub usage: UsageSnap,
    #[serde(default, deserialize_with = "de_default")]
    pub subagents: Vec<SubagentSnap>,
    #[serde(default, deserialize_with = "de_default")]
    pub jobs: Vec<JobSnap>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct BalanceSnap {
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default, deserialize_with = "de_default")]
    pub currency: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct QuotaSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub group: String,
    #[serde(default)]
    pub pct: Option<f64>,
    #[serde(default)]
    pub reset: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct EffectiveSnap {
    #[serde(default)]
    pub ratio: Option<f64>,
    #[serde(default, deserialize_with = "de_default")]
    pub paused: bool,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct StateSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub paused: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct UsageSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub today: f64,
    #[serde(default, deserialize_with = "de_default")]
    pub h5: f64,
    #[serde(default, deserialize_with = "de_default")]
    pub d7: f64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct SubagentSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub id: String,
    #[serde(default, deserialize_with = "de_default")]
    pub desc: String,
    #[serde(default, deserialize_with = "de_default")]
    pub model: String,
    #[serde(default, deserialize_with = "de_default")]
    pub tokens: f64,
    #[serde(default, deserialize_with = "de_default")]
    pub alive: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct JobSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub to: String,
    #[serde(default, deserialize_with = "de_default")]
    pub task: String,
    #[serde(default, deserialize_with = "de_default")]
    pub alive: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct ChainSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub id: String,
    #[serde(default, deserialize_with = "de_default")]
    pub name: String,
    #[serde(default, deserialize_with = "de_default")]
    pub available: bool,
    #[serde(default, deserialize_with = "de_default")]
    pub master: bool,
    #[serde(default, deserialize_with = "de_default")]
    pub reason: String,
    #[serde(default)]
    pub rank: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct AlarmSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub level: String,
    #[serde(default, deserialize_with = "de_default")]
    pub text: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct DiarySnap {
    #[serde(default, deserialize_with = "de_default")]
    pub ts: f64,
    #[serde(default, deserialize_with = "de_default")]
    pub text: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(super) struct TaskSnap {
    #[serde(default, deserialize_with = "de_default")]
    pub id: String,
    #[serde(default, deserialize_with = "de_default")]
    pub brain: String,
    #[serde(default, deserialize_with = "de_default")]
    pub title: String,
    #[serde(default, deserialize_with = "de_default")]
    pub status: String,
    #[serde(default, deserialize_with = "de_default")]
    pub parent_run: String,
    #[serde(default, deserialize_with = "de_default")]
    pub kind: String,
    #[serde(default, deserialize_with = "de_default")]
    pub model: String,
}

pub(super) fn parse_home_snap(v: &Value) -> HomeSnap {
    serde_json::from_value(v.clone()).unwrap_or_default()
}

pub fn show(app: &mut App, ui: &mut egui::Ui, snap_val: &Value) {
    let snap = parse_home_snap(snap_val);
    header(app, ui, &snap);
    ui.add_space(10.0);

    // The circles get the whole width; a brain's detail slides in from the right over them.
    let total = ui.available_width().max(240.0);
    let side = (total * 0.34)
        .clamp(240.0, 420.0)
        .min((total - 24.0).max(200.0));
    let map_h = (ui.clip_rect().height() - 90.0).clamp(320.0, 1400.0);
    let (map_rect, _) = ui.allocate_exact_size(Vec2::new(total, map_h), Sense::hover());

    let open_id = Some(app.ui.sel.clone())
        .filter(|id| id != "core" && snap.brains.iter().any(|b| b.id == *id));
    let mem = egui::Id::unique("drawer_brain");
    if let Some(id) = &open_id {
        ui.ctx().data_mut(|d| d.insert_temp(mem, id.clone()));
    }
    let t =
        ui.ctx()
            .animate_bool_with_time(egui::Id::unique("drawer_open"), open_id.is_some(), 0.24);
    let shown: Option<String> = open_id
        .clone()
        .or_else(|| ui.ctx().data(|d| d.get_temp::<String>(mem)));
    let drawer_rect = if t > 0.001 && shown.is_some() {
        let ease = 1.0 - (1.0 - t).powi(3);
        let x = map_rect.max.x - (side + 12.0) * ease;
        Some(Rect::from_min_size(
            Pos2::new(x, map_rect.min.y + 8.0),
            Vec2::new(side, map_h - 16.0),
        ))
    } else {
        None
    };

    {
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(map_rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        map::map(app, &mut child, &snap, Vec2::new(total, map_h), drawer_rect);
    }
    if let (Some(id), Some(r)) = (shown, drawer_rect) {
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(r)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        child.shrink_clip_rect(map_rect);
        child.painter().rect_filled(
            r.translate(Vec2::new(0.0, 4.0)).expand(2.0),
            CornerRadius::same(R + 2),
            Color32::from_black_alpha((70.0 * t) as u8),
        );
        egui::Frame::new()
            .fill(FIELD)
            .stroke(Stroke::new(1.0, RULE))
            .corner_radius(CornerRadius::same(R + 2))
            .inner_margin(egui::Margin::same(6))
            .show(&mut child, |ui| {
                ui.set_min_size(r.size() - Vec2::splat(12.0));
                egui::ScrollArea::vertical()
                    .id_salt("detail")
                    .max_height(map_h - 30.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width((side - 24.0).max(160.0));
                        detail::brain_detail(app, ui, &snap, &id);
                    });
            });
    }
    ui.add_space(16.0);
    detail::core_detail(app, ui, &snap);
    ui.add_space(16.0);
    hierarchy::hierarchy(app, ui, &snap);
}

fn header(app: &mut App, ui: &mut egui::Ui, snap: &HomeSnap) {
    let masters: Vec<&str> = snap
        .chain
        .iter()
        .filter(|e| e.master)
        .map(|e| e.name.as_str())
        .collect();
    let any_avail = snap.chain.iter().any(|e| e.available);
    let line = if !any_avail {
        "Ningún cerebro disponible: todos en pausa o sin cuota".to_string()
    } else {
        match masters.len() {
            0 => "Ningún cerebro al mando".to_string(),
            1 => format!("Manda {}", masters[0]),
            _ => format!("Mandan juntos {}", masters.join(" y ")),
        }
    };
    let buttons = |app: &mut App, ui: &mut egui::Ui| {
        if quiet(ui, "Pausar todo").clicked() {
            app.act(json!({"a": "pause_all"}));
            app.say("Todos los cerebros en pausa", ATTN);
        }
        if quiet(ui, "Reanudar todo").clicked() {
            app.act(json!({"a": "resume_all"}));
            app.say("Todos los cerebros reanudados", RUN);
        }
    };
    if ui.available_width() < 560.0 {
        ui.label(egui::RichText::new("Inicio").font(cond(26.0)).color(INK));
        ui.add(egui::Label::new(dim(line)).wrap());
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| buttons(app, ui));
    } else {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Inicio").font(cond(26.0)).color(INK));
                ui.label(dim(line));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if quiet(ui, "Reanudar todo").clicked() {
                    app.act(json!({"a": "resume_all"}));
                    app.say("Todos los cerebros reanudados", RUN);
                }
                if quiet(ui, "Pausar todo").clicked() {
                    app.act(json!({"a": "pause_all"}));
                    app.say("Todos los cerebros en pausa", ATTN);
                }
            });
        });
    }
}

// =====================================================================================
// Gauges & shared visual helpers
// =====================================================================================

pub(super) fn gauge_snap(b: &BrainSnap) -> (Option<f64>, &'static str, Option<f64>) {
    // The ring shows whatever limits the brain right now: the fullest of its quotas (5 h
    // session, weekly) and its daily budget; an API brain with no balance is full.
    if b.balance
        .as_ref()
        .and_then(|x| x.amount)
        .is_some_and(|a| a <= 0.0)
    {
        return (Some(100.0), "sin saldo", None);
    }
    let mut best: Option<(f64, &'static str, Option<f64>)> = None;
    for q in &b.quota {
        if let Some(p) = q.pct {
            let label = match q.group.as_str() {
                "session" => "sesión 5 h",
                "weekly" => "semana",
                _ => "cuota",
            };
            if best.map(|x| p > x.0).unwrap_or(true) {
                best = Some((p, label, q.reset));
            }
        }
    }
    if let Some(r) = b.effective.ratio.map(|r| r * 100.0) {
        if best.map(|x| r > x.0).unwrap_or(true) {
            best = Some((r, "presupuesto del día", None));
        }
    }
    match best {
        // A share can't pass 100 %: going over the daily budget just means "full".
        Some((p, l, r)) if p >= 100.0 => (
            Some(100.0),
            if l == "presupuesto del día" {
                "presupuesto del día agotado"
            } else {
                l
            },
            r,
        ),
        Some((p, l, r)) => (Some(p), l, r),
        None => (None, "sin datos", None),
    }
}

/// What the ring shows: the real 5 h session quota when the brain has one, otherwise the
/// biggest quota, otherwise how much of its daily budget is used.
pub fn gauge(b: &Value) -> (Option<f64>, &'static str, Option<f64>) {
    let parsed: BrainSnap = serde_json::from_value(b.clone()).unwrap_or_default();
    gauge_snap(&parsed)
}

pub(super) fn balance_text_snap(b: &BrainSnap) -> Option<String> {
    let bal = b.balance.as_ref()?;
    let a = bal.amount?;
    let cur = bal.currency.as_str();
    Some(if cur == "USD" || cur.is_empty() {
        format!("${a:.2}")
    } else if cur == "CNY" {
        format!("¥{a:.2}")
    } else {
        format!("{a:.2} {cur}")
    })
}

/// API brains that bill a prepaid balance (DeepSeek) show what is left instead of a %.
pub fn balance_text(b: &Value) -> Option<String> {
    let parsed: BrainSnap = serde_json::from_value(b.clone()).unwrap_or_default();
    balance_text_snap(&parsed)
}

pub(super) fn state_of_snap(b: &BrainSnap) -> (&'static str, Color32) {
    if b.state.paused || b.effective.paused {
        ("en pausa", ATTN)
    } else if b.active {
        ("trabajando", RUN)
    } else {
        ("en espera", INK3)
    }
}

pub(super) fn pct_color(p: Option<f64>) -> Color32 {
    match p {
        Some(p) if p >= 100.0 => ALARM,
        Some(p) if p >= 80.0 => ATTN,
        Some(_) => INK2,
        None => INK3,
    }
}

pub(super) fn hhmm(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

pub(super) fn task_activity(task: &TaskSnap, convs: &Value) -> Option<(&'static str, Color32)> {
    if convs
        .get(&task.id)
        .is_some_and(|meta| meta.b("deleted", false))
    {
        return None;
    }
    match task.status.as_str() {
        "running" | "planning" | "synthesizing" => Some(("trabajando", RUN)),
        "queued" => Some(("en cola", INFO)),
        "verifying" => Some(("verificando", RUN)),
        _ => None,
    }
}

/// "deepseek-flash" is DeepSeek V4.1 Flash; say so.
pub fn model_label(m: &str) -> String {
    match m {
        "" => "–".into(),
        "deepseek-flash" => "deepseek-flash (V4.1)".into(),
        x => x.to_string(),
    }
}

pub(super) fn panel<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let w = ui.available_width();
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, RULE))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width((w - 28.0).max(1.0));
            add(ui)
        })
        .inner
}

pub(super) fn stat(ui: &mut egui::Ui, v: &str, l: &str) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(v).font(cond(21.0)).color(INK));
        ui.label(faint(l));
    });
}
