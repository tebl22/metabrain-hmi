//! Interactive Obsidian-style brain graph map: force-directed orbit, drag/pan/zoom,
//! subagent satellites, active task rings, and delegation arcs.

use super::{
    balance_text_snap, gauge_snap, hhmm, model_label, pct_color, state_of_snap, task_activity,
    BrainSnap, ChainSnap, HomeSnap, TaskSnap,
};
use crate::gui::theme::*;
use crate::gui::App;
use crate::parallel::ParallelRun;
use crate::util::now;
use eframe::egui::{
    self, Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2,
};
use serde_json::Value;
use std::collections::HashSet;
use std::f32::consts::PI;

fn polar(c: Pos2, r: f32, deg: f32) -> Pos2 {
    let a = (deg - 90.0) * PI / 180.0;
    Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())
}

fn arc(p: &egui::Painter, c: Pos2, r: f32, d0: f32, d1: f32, stroke: Stroke) {
    if d1 - d0 < 0.5 {
        return;
    }
    let n = ((d1 - d0) / 4.0).ceil().max(2.0) as usize;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| polar(c, r, d0 + (d1 - d0) * i as f32 / n as f32))
        .collect();
    p.add(Shape::line(pts, stroke));
    // Round caps.
    p.circle_filled(polar(c, r, d0), stroke.width / 2.0, stroke.color);
    p.circle_filled(polar(c, r, d1), stroke.width / 2.0, stroke.color);
}

fn dashed_circle(p: &egui::Painter, c: Pos2, r: f32, stroke: Stroke, dash_deg: f32, gap_deg: f32) {
    let mut d = 0.0;
    while d < 360.0 {
        let e = (d + dash_deg).min(360.0);
        let pts: Vec<Pos2> = (0..=3)
            .map(|i| polar(c, r, d + (e - d) * i as f32 / 3.0))
            .collect();
        p.add(Shape::line(pts, stroke));
        d += dash_deg + gap_deg;
    }
}

const GAP: f32 = 60.0;
const SWEEP: f32 = 360.0 - GAP;
const D0: f32 = 180.0 + GAP / 2.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Core,
    Brain,
    Orchestration,
    Branch,
    Sub,
    Task,
}

#[derive(Clone, Debug)]
enum NodeData {
    Core,
    Brain {
        brain: Box<BrainSnap>,
        chain: Option<ChainSnap>,
    },
    Orchestration {
        run_id: String,
        done: usize,
        total: usize,
        active: usize,
        wave: usize,
        waves_total: usize,
    },
    Branch {
        task_id: String,
        run_id: String,
        wave: usize,
        role: String,
        brain_name: String,
        model: String,
    },
    Sub,
    Task {
        task_id: String,
    },
}

#[derive(Clone, Debug)]
struct GNode {
    id: String,
    kind: Kind,
    parent: String,
    label: String,
    detail: String,
    color: Color32,
    alive: bool,
    /// Smooth 0..1 entrance scale animation so new agents/subchats pop in fluidly.
    spawn: f32,
    pos: Vec2,
    vel: Vec2,
    anchor: Option<Vec2>,
    data: NodeData,
}

#[derive(Default)]
pub struct Graph {
    nodes: Vec<GNode>,
    pan: Vec2,
    zoom: f32,
    orbit: f32,
    drag: Option<usize>,
    hot: f32,
}

/// Evaluate a quadratic Bezier curve at `s` in `0..=1`.
fn quad_bezier(a: Pos2, ctrl: Pos2, b: Pos2, s: f32) -> Pos2 {
    let u = 1.0 - s;
    Pos2::new(
        u * u * a.x + 2.0 * u * s * ctrl.x + s * s * b.x,
        u * u * a.y + 2.0 * u * s * ctrl.y + s * s * b.y,
    )
}

/// Draw a glowing travelling particle along a straight segment.
fn paint_flow_particle(p: &egui::Painter, a: Pos2, b: Pos2, phase: f32, col: Color32, alpha: f32) {
    let s = phase.fract();
    let pos = Pos2::new(a.x + (b.x - a.x) * s, a.y + (b.y - a.y) * s);
    let edge_fade = (s * 4.0).min((1.0 - s) * 4.0).clamp(0.0, 1.0) * alpha;
    p.circle_filled(pos, 4.4, col.gamma_multiply(0.24 * edge_fade));
    p.circle_filled(pos, 2.2, col.gamma_multiply(0.95 * edge_fade));
}

impl Graph {
    /// Rebuild the node set from the snapshot, active tasks, and parallel orchestrations,
    /// keeping physical positions & smooth spawn state of surviving nodes.
    fn sync(
        &mut self,
        snap: &HomeSnap,
        tasks: &[TaskSnap],
        runs: &[ParallelRun],
        convs: &Value,
        orbit: f32,
    ) {
        let resize = if self.orbit > 0.0 {
            orbit / self.orbit
        } else {
            1.0
        };
        self.orbit = orbit;
        let bs = &snap.brains;
        let n = bs.len().max(1) as f32;
        let mut want: Vec<GNode> = Vec::new();
        let mk = |id: String,
                  kind: Kind,
                  parent: &str,
                  label: String,
                  detail: String,
                  color: Color32,
                  alive: bool,
                  anchor: Option<Vec2>,
                  data: NodeData| GNode {
            id,
            kind,
            parent: parent.to_string(),
            label,
            detail,
            color,
            alive,
            spawn: if matches!(kind, Kind::Core | Kind::Brain) {
                1.0
            } else {
                0.05
            },
            pos: Vec2::ZERO,
            vel: Vec2::ZERO,
            anchor,
            data,
        };
        want.push(mk(
            "core".into(),
            Kind::Core,
            "",
            "MetaBrain".into(),
            String::new(),
            RUN,
            false, // computed after gathering live brains/tasks/orchestrations (fixes 30fps idle burn)
            Some(Vec2::ZERO),
            NodeData::Core,
        ));
        let mut branch_task_ids: HashSet<String> = HashSet::new();
        for (i, b) in bs.iter().enumerate() {
            let bid = format!("b:{}", b.id);
            let a = polar(Pos2::ZERO, orbit, 360.0 * i as f32 / n).to_vec2();
            let ch = snap.chain.iter().find(|e| e.id == b.id).cloned();
            want.push(mk(
                bid.clone(),
                Kind::Brain,
                "core",
                b.name.clone(),
                String::new(),
                series(i),
                b.active,
                Some(a),
                NodeData::Brain {
                    brain: Box::new(b.clone()),
                    chain: ch,
                },
            ));
            for s in b.subagents.iter().filter(|s| s.alive).take(10) {
                want.push(mk(
                    format!("s:{}:{}", b.id, s.id),
                    Kind::Sub,
                    &bid,
                    s.desc.clone(),
                    format!(
                        "{} · {} tokens",
                        s.model.replace("claude-", ""),
                        num(s.tokens)
                    ),
                    RUN,
                    true,
                    None,
                    NodeData::Sub,
                ));
            }
            let brain_tasks = tasks
                .iter()
                .filter(|t| t.brain == b.id && t.parent_run.is_empty() && t.kind != "parallel")
                .filter_map(|t| task_activity(t, convs).map(|(label, color)| (t, label, color)));
            for (t, label, color) in brain_tasks.take(6) {
                let model_s = if t.model.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", model_label(&t.model))
                };
                want.push(mk(
                    format!("t:{}", t.id),
                    Kind::Task,
                    &bid,
                    t.title.clone(),
                    format!("Chat · {label}{model_s}"),
                    color,
                    color == RUN,
                    None,
                    NodeData::Task {
                        task_id: t.id.clone(),
                    },
                ));
            }
        }

        // Parallel orchestrations & their agent subchats visualized directly as living circles.
        let now_ts = now();
        let visible_runs: Vec<&ParallelRun> = runs
            .iter()
            .filter(|r| {
                let deleted = convs
                    .get(&r.id)
                    .is_some_and(|m| m.get("deleted").and_then(|v| v.as_bool()).unwrap_or(false));
                if deleted {
                    return false;
                }
                let is_live = matches!(r.status.as_str(), "planning" | "running" | "synthesizing");
                let recent = matches!(r.status.as_str(), "done" | "review")
                    && (now_ts - r.updated).abs() < 90.0;
                is_live || recent
            })
            .take(3)
            .collect();

        for (ri, run) in visible_runs.iter().enumerate() {
            let run_live = matches!(run.status.as_str(), "planning" | "running" | "synthesizing");
            let done_n = run.branches.iter().filter(|b| b.status == "done").count();
            let active_n = run
                .branches
                .iter()
                .filter(|b| b.status == "running")
                .count();
            let total_n = run.branches.len();
            let cur_wave = run
                .branches
                .iter()
                .filter(|b| b.status == "running")
                .map(|b| b.wave + 1)
                .min()
                .unwrap_or_else(|| {
                    if done_n == total_n && total_n > 0 {
                        run.waves_total.max(1)
                    } else {
                        1
                    }
                });
            let oid = format!("o:{}", run.id);
            let dir_node_id = format!("b:{}", run.director);
            let parent_id = if !run.director.is_empty() && want.iter().any(|x| x.id == dir_node_id)
            {
                dir_node_id
            } else {
                "core".to_string()
            };
            let hub_angle = 45.0 + ri as f32 * 120.0;
            let hub_anchor = Some(polar(Pos2::ZERO, orbit * 0.56, hub_angle).to_vec2());
            let run_title: String = run
                .prompt
                .lines()
                .next()
                .unwrap_or("Orchestration")
                .chars()
                .take(40)
                .collect();
            want.push(mk(
                oid.clone(),
                Kind::Orchestration,
                &parent_id,
                run_title,
                format!(
                    "Orchestration · {done_n}/{total_n} agents · Wave {cur_wave}/{}",
                    run.waves_total.max(1)
                ),
                if run_live { INFO } else { MASTER },
                run_live,
                hub_anchor,
                NodeData::Orchestration {
                    run_id: run.id.clone(),
                    done: done_n,
                    total: total_n,
                    active: active_n,
                    wave: cur_wave,
                    waves_total: run.waves_total.max(1),
                },
            ));

            for br in &run.branches {
                if !br.task_id.is_empty() {
                    branch_task_ids.insert(br.task_id.clone());
                }
                let br_live = br.status == "running";
                let br_col = match br.status.as_str() {
                    "running" => RUN,
                    "done" => MASTER,
                    "failed" => ALARM,
                    "stopped" => ATTN,
                    _ => INFO,
                };
                let b_node_id = format!("b:{}", br.brain);
                let br_parent = if !br.brain.is_empty() && want.iter().any(|x| x.id == b_node_id) {
                    b_node_id
                } else {
                    oid.clone()
                };
                let br_id = format!("br:{}:{}", run.id, br.id);
                let role_lbl = br.role.as_str();
                let model_s = if br.model.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", model_label(&br.model))
                };
                want.push(mk(
                    br_id,
                    Kind::Branch,
                    &br_parent,
                    br.title.clone(),
                    format!(
                        "Subchat Wave {} · T{} · {role_lbl} · {}{model_s}",
                        br.wave + 1,
                        br.tier,
                        if br.brain_name.is_empty() {
                            "awaiting brain"
                        } else {
                            br.brain_name.as_str()
                        }
                    ),
                    br_col,
                    br_live,
                    None,
                    NodeData::Branch {
                        task_id: br.task_id.clone(),
                        run_id: run.id.clone(),
                        wave: br.wave + 1,
                        role: role_lbl.into(),
                        brain_name: br.brain_name.clone(),
                        model: br.model.clone(),
                    },
                ));
            }
        }

        // Also surface any active child task not already represented by a ParallelRun branch.
        for t in tasks
            .iter()
            .filter(|t| !t.parent_run.is_empty() && !branch_task_ids.contains(&t.id))
        {
            if let Some((label, color)) = task_activity(t, convs) {
                let b_node_id = format!("b:{}", t.brain);
                if want.iter().any(|x| x.id == b_node_id) {
                    want.push(mk(
                        format!("t:{}", t.id),
                        Kind::Task,
                        &b_node_id,
                        t.title.clone(),
                        format!("Subchat · {label}"),
                        color,
                        color == RUN,
                        None,
                        NodeData::Task {
                            task_id: t.id.clone(),
                        },
                    ));
                }
            }
        }

        // Core is alive only when there is actual live activity or an active alarm.
        let any_live_activity = snap.overall == "ALARM" || want.iter().skip(1).any(|x| x.alive);
        if let Some(core) = want.first_mut() {
            core.alive = any_live_activity;
        }

        // Keep the physical state & animate spawn of nodes we already had.
        let mut out = Vec::with_capacity(want.len());
        for mut w in want {
            if let Some(old) = self.nodes.iter().find(|o| o.id == w.id) {
                w.pos = old.pos * resize;
                w.vel = old.vel * resize;
                w.spawn = (old.spawn + 0.14).min(1.0);
            } else if let Some(a) = w.anchor {
                w.pos = a;
            } else if let Some(p) = out
                .iter()
                .find(|p: &&GNode| p.id == w.parent)
                .or_else(|| self.nodes.iter().find(|p| p.id == w.parent))
            {
                let h = (w
                    .id
                    .bytes()
                    .fold(7u32, |a, c| a.wrapping_mul(31).wrapping_add(c as u32))
                    % 360) as f32;
                w.pos = p.pos + polar(Pos2::ZERO, 56.0, h).to_vec2();
            }
            out.push(w);
        }
        if out.len() != self.nodes.len() || resize != 1.0 {
            self.hot = 1.0;
        }
        self.drag = self.drag.and_then(|i| {
            let id = &self.nodes.get(i)?.id;
            out.iter().position(|node| node.id == *id)
        });
        self.nodes = out;
    }

    fn radius(&self, i: usize, nr: f32) -> f32 {
        let base = match self.nodes[i].kind {
            Kind::Core => nr * 0.95,
            Kind::Brain => nr,
            Kind::Orchestration => (nr * 0.44).clamp(13.0, 19.0),
            Kind::Branch => 10.0,
            Kind::Sub => 6.5,
            Kind::Task => 8.5,
        };
        let s = self.nodes[i].spawn;
        let ease = 1.0 - (1.0 - s).powi(3);
        base * (0.25 + 0.75 * ease)
    }

    /// A few steps of a small force simulation: repulsion between all nodes, springs to
    /// the parent, brains held near their orbit slot. Returns true while still moving.
    fn step(&mut self, nr: f32) -> bool {
        if self.hot < 0.02 && self.drag.is_none() {
            return false;
        }
        let n = self.nodes.len();
        let mut force = vec![Vec2::ZERO; n];
        for i in 0..n {
            for j in (i + 1)..n {
                let d = self.nodes[j].pos - self.nodes[i].pos;
                let dist = d.length().max(1.0);
                let min = self.radius(i, nr) + self.radius(j, nr) + 16.0;
                let mut f = 1800.0 / (dist * dist);
                if dist < min {
                    f += (min - dist) * 0.5;
                }
                let dir = d / dist;
                force[i] -= dir * f;
                force[j] += dir * f;
            }
        }
        for i in 0..n {
            let nd = &self.nodes[i];
            if let Some(a) = nd.anchor {
                force[i] += (a - nd.pos) * 0.12;
            }
            if !nd.parent.is_empty() && nd.kind != Kind::Brain {
                if let Some(pi) = self.nodes.iter().position(|p| p.id == nd.parent) {
                    let rest = match nd.kind {
                        Kind::Orchestration => nr * 1.45,
                        Kind::Branch => nr + 50.0,
                        _ => nr + 46.0,
                    };
                    let d = self.nodes[pi].pos - nd.pos;
                    let dist = d.length().max(1.0);
                    let f = (dist - rest) * 0.06;
                    force[i] += d / dist * f;
                    if self.nodes[pi].kind != Kind::Core {
                        force[pi] -= d / dist * f * 0.15;
                    }
                }
            }
        }
        let mut energy = 0.0;
        for i in 0..n {
            if Some(i) == self.drag || self.nodes[i].kind == Kind::Core {
                self.nodes[i].vel = Vec2::ZERO;
                continue;
            }
            let v = (self.nodes[i].vel + force[i] * self.hot.max(0.3)) * 0.78;
            let v = if v.length() > 30.0 {
                v.normalized() * 30.0
            } else {
                v
            };
            self.nodes[i].vel = v;
            self.nodes[i].pos += v;
            energy += v.length_sq();
        }
        self.hot *= 0.985;
        energy > 0.02
    }
}

/// Choose ring radius and orbit radius so the whole map stays inside the canvas on small windows.
fn map_geometry(size: Vec2) -> (f32, f32) {
    let half = (size.x - 24.0).min(size.y - 48.0).max(160.0) * 0.5;
    let nr = (half * 0.22).clamp(24.0, 42.0);
    let min_orbit = nr * 2.15 + 8.0;
    let max_orbit = (half - nr - 24.0).max(min_orbit);
    let orbit = max_orbit.min(220.0);
    (nr, orbit)
}

pub(super) fn map(
    app: &mut App,
    ui: &mut egui::Ui,
    snap: &HomeSnap,
    size: Vec2,
    drawer_rect: Option<Rect>,
) {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let p = ui.painter_at(rect);
    p.rect(
        rect,
        CornerRadius::same(8),
        Color32::from_rgb(0x1D, 0x22, 0x27),
        Stroke::new(1.0, RULE),
        StrokeKind::Inside,
    );
    let tasks: Vec<TaskSnap> = crate::gui::pages::task_list(app)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let parallel_runs: Vec<ParallelRun> = app
        .data
        .read()
        .map(|d| d.parallel_runs.clone())
        .unwrap_or_default();
    let g = &mut app.ui.graph;
    if g.zoom == 0.0 {
        g.zoom = 1.0;
    }
    let (nr, orbit) = map_geometry(size);
    let scale = nr / 42.0;
    g.sync(snap, &tasks, &parallel_runs, &app.ui.convs, orbit);
    let moving = (0..2).fold(false, |m, _| g.step(nr) || m);

    // Floating zoom bar in the top-left corner so zoom is explicit and separate from page scroll.
    let zoom_bar = Rect::from_min_size(
        Pos2::new(rect.min.x + 12.0, rect.min.y + 12.0),
        Vec2::new(112.0, 26.0),
    );
    let mut zoom_bar_hit = false;
    {
        let mut zui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(zoom_bar)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        egui::Frame::new()
            .fill(PANEL.gamma_multiply(0.92))
            .stroke(Stroke::new(1.0, RULE))
            .corner_radius(CornerRadius::same(R))
            .inner_margin(egui::Margin::symmetric(4, 2))
            .show(&mut zui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let btn = |ui: &mut egui::Ui, txt: &str, w: f32, tip: &str| {
                    ui.add_sized(
                        [w, 20.0],
                        egui::Button::new(egui::RichText::new(txt).font(mono(11.5)).color(INK2))
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(4)),
                    )
                    .on_hover_text(tip)
                };
                if btn(ui, "−", 22.0, "Zoom out (Ctrl + scroll down)").clicked() {
                    g.zoom = (g.zoom / 1.2).clamp(0.45, 2.4);
                    zoom_bar_hit = true;
                }
                let pct_txt = format!("{:.0}%", g.zoom * 100.0);
                if btn(ui, &pct_txt, 46.0, "Reset zoom & center (double-click)").clicked() {
                    g.zoom = 1.0;
                    g.pan = Vec2::ZERO;
                    zoom_bar_hit = true;
                }
                if btn(ui, "+", 22.0, "Zoom in (Ctrl + scroll up)").clicked() {
                    g.zoom = (g.zoom * 1.2).clamp(0.45, 2.4);
                    zoom_bar_hit = true;
                }
            });
    }
    let blocked =
        |pos: Pos2| zoom_bar.contains(pos) || drawer_rect.is_some_and(|dr| dr.contains(pos));

    // egui separates Ctrl+wheel and pinch from the page's ordinary wheel scroll.
    let center = rect.center() - Vec2::new(0.0, 12.0);
    let to_screen = |g: &Graph, v: Vec2| center + (v + g.pan) * g.zoom;
    let hover = resp.hover_pos().filter(|pos| !blocked(*pos));
    if let Some(pointer) = hover {
        let factor = ui.input(|i| i.zoom_delta());
        if factor != 1.0 {
            let z0 = g.zoom;
            g.zoom = (z0 * factor).clamp(0.45, 2.4);
            // Keep the point under the cursor fixed.
            g.pan += (pointer - center) * (1.0 / g.zoom - 1.0 / z0);
        }
    }
    let zoom = g.zoom;
    let hit = |g: &Graph, at: Pos2| -> Option<usize> {
        (0..g.nodes.len()).rev().find(|&i| {
            let r = g.radius(i, nr) * zoom
                + if g.nodes[i].kind == Kind::Brain {
                    10.0
                } else {
                    5.0
                };
            to_screen(g, g.nodes[i].pos).distance(at) <= r
        })
    };
    let hovered = hover.and_then(|h| hit(g, h));
    if resp.drag_started() && !zoom_bar_hit {
        let start = resp.interact_pointer_pos().filter(|pos| !blocked(*pos));
        g.drag = start
            .and_then(|h| hit(g, h))
            .filter(|&i| g.nodes[i].kind != Kind::Core);
    }
    if resp.dragged() && !zoom_bar_hit {
        let over_blocked = resp.interact_pointer_pos().is_some_and(blocked);
        if !over_blocked || g.drag.is_some() {
            let d = resp.drag_delta() / zoom;
            match g.drag {
                Some(i) => {
                    g.nodes[i].pos += d;
                    if let Some(a) = g.nodes[i].anchor.as_mut() {
                        *a += d; // brains stay where you leave them
                    }
                    g.hot = 1.0;
                }
                None => g.pan += d,
            }
        }
    }
    if resp.drag_stopped() {
        g.drag = None;
    }
    if resp.double_clicked() && hovered.is_none() && hover.is_some() && !zoom_bar_hit {
        g.pan = Vec2::ZERO;
        g.zoom = 1.0;
    }

    // Neighbourhood of the hovered node (Obsidian-style focus, including orchestration swarms).
    let focus: Option<HashSet<String>> = hovered.map(|i| {
        let id = g.nodes[i].id.clone();
        let mut s = HashSet::new();
        s.insert(id.clone());
        let par = g.nodes[i].parent.clone();
        s.insert(par.clone());
        let hovered_run: Option<&str> = match &g.nodes[i].data {
            NodeData::Orchestration { run_id, .. } => Some(run_id.as_str()),
            NodeData::Branch { run_id, .. } => Some(run_id.as_str()),
            _ => None,
        };
        for nd in &g.nodes {
            if nd.parent == id
                || (g.nodes[i].kind != Kind::Brain
                    && nd.parent == par
                    && g.nodes[i].kind != Kind::Core)
            {
                s.insert(nd.id.clone());
            }
            if let Some(rid) = hovered_run {
                match &nd.data {
                    NodeData::Orchestration { run_id, .. } if run_id == rid => {
                        s.insert(nd.id.clone());
                        s.insert(nd.parent.clone());
                    }
                    NodeData::Branch { run_id, .. } if run_id == rid => {
                        s.insert(nd.id.clone());
                        s.insert(nd.parent.clone());
                    }
                    _ => {}
                }
            }
        }
        s
    });
    let dim = |id: &str| focus.as_ref().map(|f| !f.contains(id)).unwrap_or(false);
    let t = now();
    let pulse = ((t * 2.4).sin() * 0.5 + 0.5) as f32;

    // Orbit guide.
    dashed_circle(
        &p,
        to_screen(g, Vec2::ZERO),
        orbit * zoom,
        Stroke::new(1.0, RULE.gamma_multiply(0.8)),
        1.2,
        3.8,
    );

    // Edges with animated energy particles along active connections.
    for (ei, nd) in g.nodes.iter().enumerate() {
        if nd.parent.is_empty() {
            continue;
        }
        let Some(par) = g.nodes.iter().find(|x| x.id == nd.parent) else {
            continue;
        };
        let faded = dim(&nd.id) || dim(&par.id);
        let alpha = if faded { 0.25 } else { 1.0 };
        let col = if nd.kind == Kind::Brain {
            if nd.alive {
                RUN.gamma_multiply(0.45 + 0.4 * pulse)
            } else {
                RULE
            }
        } else if nd.kind == Kind::Orchestration {
            if nd.alive {
                INFO.gamma_multiply(0.6 + 0.3 * pulse)
            } else {
                MASTER.gamma_multiply(0.55)
            }
        } else if nd.alive {
            nd.color.gamma_multiply(0.6)
        } else {
            RULE
        };
        let (a_pt, b_pt) = (to_screen(g, par.pos), to_screen(g, nd.pos));
        p.line_segment(
            [a_pt, b_pt],
            Stroke::new(if nd.alive { 1.5 } else { 1.0 }, col.gamma_multiply(alpha)),
        );
        if nd.alive {
            let phase = (t * 0.62 + ei as f64 * 0.23) as f32;
            paint_flow_particle(
                &p,
                a_pt,
                b_pt,
                phase,
                if nd.kind == Kind::Orchestration {
                    INFO
                } else {
                    nd.color
                },
                alpha,
            );
        }
    }
    // Curved orchestration links: Orchestration hub -> every Branch subchat circle (with travelling particles).
    for (bi, nd) in g.nodes.iter().enumerate() {
        let NodeData::Branch { run_id, .. } = &nd.data else {
            continue;
        };
        let hub_id = format!("o:{run_id}");
        if nd.parent == hub_id {
            continue; // already drawn as direct parent edge
        }
        let Some(hub) = g.nodes.iter().find(|x| x.id == hub_id) else {
            continue;
        };
        let faded = dim(&nd.id) || dim(&hub.id);
        let alpha = if faded { 0.22 } else { 1.0 };
        let (a_pt, b_pt) = (to_screen(g, hub.pos), to_screen(g, nd.pos));
        let mid = Pos2::new((a_pt.x + b_pt.x) * 0.5, (a_pt.y + b_pt.y) * 0.5);
        let perp = Vec2::new(-(b_pt.y - a_pt.y), b_pt.x - a_pt.x) * 0.14;
        let ctrl = mid + perp;
        let pts: Vec<Pos2> = (0..=20)
            .map(|k| quad_bezier(a_pt, ctrl, b_pt, k as f32 / 20.0))
            .collect();
        let line_col = if nd.alive {
            nd.color.gamma_multiply(0.55 * alpha)
        } else {
            INFO.gamma_multiply(0.28 * alpha)
        };
        p.add(Shape::line(
            pts,
            Stroke::new(if nd.alive { 1.4 } else { 1.0 }, line_col),
        ));
        if nd.alive {
            let s = ((t * 0.75 + bi as f64 * 0.31) % 1.0) as f32;
            let pt = quad_bezier(a_pt, ctrl, b_pt, s);
            p.circle_filled(pt, 4.5, nd.color.gamma_multiply(0.24 * alpha));
            p.circle_filled(pt, 2.3, nd.color.gamma_multiply(0.95 * alpha));
        }
    }
    // Delegations brain → brain with animated travelling pulse.
    for (di, nd) in g.nodes.iter().enumerate() {
        let NodeData::Brain { brain, .. } = &nd.data else {
            continue;
        };
        for j in brain.jobs.iter().filter(|j| j.alive) {
            let to = j.to.to_lowercase();
            if let Some(o) = g.nodes.iter().find(|o| match &o.data {
                NodeData::Brain { brain: ob, .. } => {
                    ob.id.to_lowercase() == to || o.label.to_lowercase().contains(&to)
                }
                _ => false,
            }) {
                let (a_pt, b_pt) = (to_screen(g, nd.pos), to_screen(g, o.pos));
                let mid = Pos2::new((a_pt.x + b_pt.x) / 2.0, (a_pt.y + b_pt.y) / 2.0);
                let ctrl = mid + (mid - to_screen(g, Vec2::ZERO)) * 0.3;
                let pts: Vec<Pos2> = (0..=24)
                    .map(|k| quad_bezier(a_pt, ctrl, b_pt, k as f32 / 24.0))
                    .collect();
                p.add(Shape::line(pts, Stroke::new(1.5, RUN.gamma_multiply(0.7))));
                let s = ((t * 0.8 + di as f64 * 0.25) % 1.0) as f32;
                let pt = quad_bezier(a_pt, ctrl, b_pt, s);
                p.circle_filled(pt, 4.8, RUN.gamma_multiply(0.26));
                p.circle_filled(pt, 2.4, RUN);
            }
        }
    }

    // Nodes.
    let mut tip: Option<String> = None;
    for (i, nd) in g.nodes.iter().enumerate() {
        let at = to_screen(g, nd.pos);
        let r = g.radius(i, nr) * zoom;
        let faded = dim(&nd.id);
        let a = if faded { 0.28 } else { 1.0 };
        let is_hover = hovered == Some(i);
        match &nd.data {
            NodeData::Core => {
                let bs: Vec<&BrainSnap> = g
                    .nodes
                    .iter()
                    .filter_map(|x| match &x.data {
                        NodeData::Brain { brain, .. } => Some(brain.as_ref()),
                        _ => None,
                    })
                    .collect();
                let n = bs.len().max(1) as f32;
                let span = 360.0 / n;
                let sg = if n > 4.0 { 18.0 } else { 26.0 };
                for (k, b) in bs.iter().enumerate() {
                    let a0 = 360.0 * k as f32 / n - span / 2.0 + sg / 2.0;
                    let (_, col) = state_of_snap(b);
                    arc(
                        &p,
                        at,
                        r * 1.05,
                        a0,
                        a0 + span - sg,
                        Stroke::new(
                            r * 0.16,
                            (if col == INK3 { RAISED2 } else { col }).gamma_multiply(a),
                        ),
                    );
                }
                if app.ui.sel == "core" || is_hover {
                    p.circle_stroke(at, r * 1.05 + 10.0, Stroke::new(1.0, INK3));
                }
                p.circle(at, r * 0.74, RAISED, Stroke::new(1.0, RULE));
                let ovc = match snap.overall.as_str() {
                    "ALARM" => ALARM.gamma_multiply(0.6 + 0.4 * pulse),
                    "WARNING" => ATTN,
                    _ => RUN,
                };
                p.circle_filled(Pos2::new(at.x, at.y - r * 0.24), r * 0.12, ovc);
                p.text(
                    Pos2::new(at.x, at.y + r * 0.14),
                    egui::Align2::CENTER_CENTER,
                    "MetaBrain",
                    bold((11.5 * scale * zoom.clamp(0.8, 1.2)).max(8.5)),
                    INK,
                );
                if is_hover {
                    tip = Some("Overview, alarms, and activity log".into());
                }
            }
            NodeData::Brain {
                brain: b,
                chain: ch,
            } => {
                let (pct, glabel, reset) = gauge_snap(b);
                let (sw, sc) = state_of_snap(b);
                let master = ch.as_ref().is_some_and(|c| c.master);
                let sel = app.ui.sel == b.id;
                if master {
                    p.circle_stroke(
                        at,
                        r + 17.0 * scale * zoom,
                        Stroke::new(1.5, MASTER.gamma_multiply(0.55 * a)),
                    );
                }
                if b.active {
                    let ripple = ((t * 1.15 + i as f64 * 0.4) % 1.0) as f32;
                    let rr = r + (14.0 + ripple * 16.0) * scale * zoom;
                    p.circle_stroke(
                        at,
                        rr,
                        Stroke::new(1.4, RUN.gamma_multiply((1.0 - ripple) * 0.55 * a)),
                    );
                }
                if sel || is_hover {
                    p.circle_filled(
                        at,
                        r + 14.0 * scale * zoom,
                        Color32::from_rgba_unmultiplied(255, 255, 255, 8),
                    );
                    p.circle_stroke(
                        at,
                        r + 14.0 * scale * zoom,
                        Stroke::new(1.0, if sel { INK2 } else { RULE }),
                    );
                }
                let window = |group: &str| {
                    b.quota
                        .iter()
                        .filter(|q| q.group == group)
                        .filter_map(|q| q.pct)
                        .max_by(f64::total_cmp)
                };
                let session = window("session");
                let week = window("weekly");
                let has_windows = b
                    .quota
                    .iter()
                    .any(|q| matches!(q.group.as_str(), "session" | "weekly"));
                let draw_ring = |offset: f32, width: f32, value: Option<f64>, color: Color32| {
                    let radius = r + offset * scale * zoom;
                    let width = width * scale * zoom;
                    arc(
                        &p,
                        at,
                        radius,
                        D0,
                        D0 + SWEEP,
                        Stroke::new(width, RAISED2.gamma_multiply(a)),
                    );
                    if let Some(v) = value {
                        let color = if v >= 100.0 {
                            ALARM
                        } else if v >= 80.0 {
                            ATTN
                        } else {
                            color
                        };
                        arc(
                            &p,
                            at,
                            radius,
                            D0,
                            D0 + SWEEP * (v / 100.0).clamp(0.0, 1.0) as f32,
                            Stroke::new(width, color.gamma_multiply(a)),
                        );
                    }
                };
                if has_windows {
                    draw_ring(9.5, 3.8, session, nd.color);
                    draw_ring(3.5, 3.0, week, INFO);
                } else {
                    draw_ring(7.5, 5.0, pct, nd.color);
                }
                p.circle(
                    at,
                    r,
                    RAISED.gamma_multiply(if faded { 0.7 } else { 1.0 }),
                    Stroke::new(
                        1.0,
                        if master {
                            MASTER.gamma_multiply(0.6)
                        } else {
                            RULE
                        },
                    ),
                );
                let z = (scale * zoom).clamp(0.65, 1.4);
                let short: String = if nd.label.chars().count() > 16 {
                    nd.label.chars().take(15).collect::<String>() + "…"
                } else {
                    nd.label.clone()
                };
                p.text(
                    Pos2::new(at.x, at.y - r * 0.36),
                    egui::Align2::CENTER_CENTER,
                    short,
                    bold((10.5 * z).max(8.0)),
                    INK.gamma_multiply(a),
                );
                let bal_txt = balance_text_snap(b);
                if has_windows {
                    for (offset, label, value) in [(-0.08, "5h", session), (0.21, "Wk", week)] {
                        let amount = value.map_or_else(|| "–".into(), |v| format!("{v:.0}%"));
                        p.text(
                            Pos2::new(at.x, at.y + r * offset),
                            egui::Align2::CENTER_CENTER,
                            format!("{label} {amount}"),
                            mono((r * 0.23).max(7.0)),
                            pct_color(value).gamma_multiply(a),
                        );
                    }
                } else {
                    p.text(
                        Pos2::new(at.x, at.y + r * 0.08),
                        egui::Align2::CENTER_CENTER,
                        bal_txt.clone().unwrap_or_else(|| {
                            pct.map(|v| format!("{v:.0}%"))
                                .unwrap_or_else(|| "–".into())
                        }),
                        mono((r * if bal_txt.is_some() { 0.32 } else { 0.4 }).max(9.0)),
                        (if pct.map(|v| v >= 80.0).unwrap_or(false) {
                            pct_color(pct)
                        } else {
                            INK
                        })
                        .gamma_multiply(a),
                    );
                }
                p.text(
                    Pos2::new(at.x, at.y + r * 0.5),
                    egui::Align2::CENTER_CENTER,
                    sw,
                    body((9.5 * z).max(7.5)),
                    sc.gamma_multiply(a),
                );
                if let Some(rank) = ch.as_ref().and_then(|c| c.rank) {
                    let br = Rect::from_center_size(
                        polar(at, r + 7.5 * scale * zoom, 0.0),
                        Vec2::new(26.0 * scale.max(0.85), 15.0 * scale.max(0.85)),
                    );
                    p.rect(
                        br,
                        CornerRadius::same(7),
                        if master { MASTER } else { RAISED2 },
                        Stroke::new(1.0, if master { MASTER } else { RULE }),
                        StrokeKind::Inside,
                    );
                    p.text(
                        br.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("T{rank}"),
                        medium((10.0 * scale.max(0.85)).max(8.5)),
                        if master {
                            Color32::from_rgb(0x1C, 0x16, 0x0A)
                        } else {
                            INK2
                        },
                    );
                }
                let model = b
                    .effective
                    .model
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or_else(|| b.model.as_deref().filter(|s| !s.is_empty()))
                    .unwrap_or_default()
                    .replace("claude-", "");
                p.text(
                    Pos2::new(at.x, at.y + r + 21.0 * scale * zoom),
                    egui::Align2::CENTER_CENTER,
                    model_label(&model),
                    mono((10.5 * z).max(8.5)),
                    INK2.gamma_multiply(a),
                );
                if is_hover {
                    let mut s = match pct {
                        Some(v) => format!(
                            "{}: {:.0}% used · {:.0}% left ({glabel})",
                            nd.label,
                            v,
                            (100.0 - v).clamp(0.0, 100.0)
                        ),
                        None => format!("{}: – ({glabel})", nd.label),
                    };
                    if let Some(rs) = reset {
                        s += &format!(", resets at {}", hhmm(rs));
                    }
                    if has_windows {
                        s += "\nOuter ring: 5h session · Inner ring: weekly";
                    }
                    tip = Some(s + ". Click to inspect details and hierarchy.");
                }
            }
            NodeData::Orchestration {
                done,
                total,
                active,
                wave,
                waves_total,
                ..
            } => {
                let col = nd.color.gamma_multiply(a);
                if nd.alive {
                    let ripple = ((t * 1.3 + i as f64 * 0.3) % 1.0) as f32;
                    p.circle_stroke(
                        at,
                        r + 3.0 + ripple * 14.0 * zoom,
                        Stroke::new(1.4, col.gamma_multiply((1.0 - ripple) * 0.7)),
                    );
                }
                // Progress arc around the orchestration hub circle.
                let frac = if *total > 0 {
                    (*done as f32 / *total as f32).clamp(0.08, 1.0)
                } else {
                    0.2
                };
                arc(
                    &p,
                    at,
                    r + 3.5 * zoom,
                    0.0,
                    360.0,
                    Stroke::new(2.2 * zoom, RAISED2.gamma_multiply(a)),
                );
                let spin = if nd.alive {
                    ((t * 48.0) % 360.0) as f32
                } else {
                    0.0
                };
                arc(
                    &p,
                    at,
                    r + 3.5 * zoom,
                    spin,
                    spin + 360.0 * frac,
                    Stroke::new(2.4 * zoom, col),
                );
                p.circle(at, r, PANEL.gamma_multiply(a), Stroke::new(1.6, col));
                let badge = if *total > 0 {
                    format!("{done}/{total}")
                } else {
                    format!("W{wave}")
                };
                p.text(
                    at,
                    egui::Align2::CENTER_CENTER,
                    badge,
                    mono((9.5 * zoom).clamp(7.5, 13.0)),
                    INK.gamma_multiply(a),
                );
                if is_hover {
                    p.circle_stroke(at, r + 7.0 * zoom, Stroke::new(1.1, INK2));
                }
                if is_hover || zoom >= 0.95 || (focus.is_some() && !faded) {
                    let s: String = if nd.label.chars().count() > 24 {
                        nd.label.chars().take(23).collect::<String>() + "…"
                    } else {
                        nd.label.clone()
                    };
                    p.text(
                        at + Vec2::new(0.0, r + 11.0 * zoom),
                        egui::Align2::CENTER_CENTER,
                        s,
                        medium(10.5),
                        col,
                    );
                }
                if is_hover {
                    tip = Some(format!(
                        "Parallel orchestration: {}\n{} agents ({} active) · Wave {wave}/{waves_total}\nClick to open the main thread.",
                        nd.label, total, active
                    ));
                }
            }
            NodeData::Branch {
                wave,
                role,
                brain_name,
                model,
                ..
            } => {
                let col = nd.color.gamma_multiply(a);
                if nd.alive {
                    let ripple = ((t * 1.8 + i as f64 * 0.45) % 1.0) as f32;
                    p.circle_stroke(
                        at,
                        r + ripple * 9.0 * zoom,
                        Stroke::new(1.2, col.gamma_multiply((1.0 - ripple) * 0.7)),
                    );
                }
                p.circle(at, r, RAISED.gamma_multiply(a), Stroke::new(2.0, col));
                p.text(
                    at,
                    egui::Align2::CENTER_CENTER,
                    format!("W{wave}"),
                    mono((8.0 * zoom).clamp(6.5, 11.0)),
                    col,
                );
                if is_hover {
                    p.circle_stroke(at, r + 4.0, Stroke::new(1.0, INK2));
                }
                if is_hover || zoom >= 1.15 || (focus.is_some() && !faded) {
                    let s: String = if nd.label.chars().count() > 24 {
                        nd.label.chars().take(23).collect::<String>() + "…"
                    } else {
                        nd.label.clone()
                    };
                    p.text(
                        at + Vec2::new(0.0, r + 9.0),
                        egui::Align2::CENTER_CENTER,
                        s,
                        body(10.0),
                        INK2.gamma_multiply(a),
                    );
                }
                if is_hover {
                    let m = if model.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", model_label(model))
                    };
                    tip = Some(format!(
                        "Subchat · {}\nWave {wave} · {role} · {}{m}\nClick to open this subchat.",
                        nd.label,
                        if brain_name.is_empty() {
                            "awaiting brain"
                        } else {
                            brain_name.as_str()
                        }
                    ));
                }
            }
            NodeData::Sub | NodeData::Task { .. } => {
                let col = nd.color.gamma_multiply(a);
                if nd.alive {
                    let ripple = ((t * 1.9 + i as f64 * 0.5) % 1.0) as f32;
                    p.circle_stroke(
                        at,
                        r + ripple * 8.0 * zoom,
                        Stroke::new(1.1, col.gamma_multiply((1.0 - ripple) * 0.65)),
                    );
                }
                if nd.kind == Kind::Task {
                    p.circle(at, r, RAISED.gamma_multiply(a), Stroke::new(2.0, col));
                } else {
                    let rr = if nd.alive { r + pulse * 1.2 } else { r };
                    p.circle_filled(at, rr, col);
                }
                if is_hover {
                    p.circle_stroke(at, r + 4.0, Stroke::new(1.0, INK2));
                }
                // Labels appear when you look closer (zoom) or point at it.
                if is_hover || zoom >= 1.25 || (focus.is_some() && !faded) {
                    let s: String = if nd.label.chars().count() > 26 {
                        nd.label.chars().take(25).collect::<String>() + "…"
                    } else {
                        nd.label.clone()
                    };
                    p.text(
                        at + Vec2::new(0.0, r + 9.0),
                        egui::Align2::CENTER_CENTER,
                        s,
                        body(10.5),
                        INK2.gamma_multiply(a),
                    );
                }
                if is_hover {
                    tip = Some(format!("{}\n{}", nd.label, nd.detail));
                }
            }
        }
    }

    // Top-right live orchestration status pill (only when an orchestration or subagent swarm is active).
    let active_orch: Option<(String, usize, usize, usize)> = g.nodes.iter().find_map(|nd| {
        if let NodeData::Orchestration {
            run_id,
            done,
            total,
            active,
            ..
        } = &nd.data
        {
            if nd.alive {
                return Some((run_id.clone(), *active, *done, *total));
            }
        }
        None
    });
    let mut hud_goto: Option<String> = None;
    if let Some((run_id, active_n, done_n, total_n)) = active_orch {
        if drawer_rect.is_none() && rect.width() > 360.0 {
            let hud_txt = if active_n > 0 {
                format!("Orchestrating · {active_n} active agents ({done_n}/{total_n})")
            } else {
                format!("Orchestrating · {done_n}/{total_n} subchats")
            };
            let hud_w = (hud_txt.len() as f32 * 6.4 + 34.0)
                .min(rect.width() - 150.0)
                .max(160.0);
            let hud_rect = Rect::from_min_size(
                Pos2::new(rect.max.x - hud_w - 12.0, rect.min.y + 12.0),
                Vec2::new(hud_w, 26.0),
            );
            let hresp = ui
                .interact(
                    hud_rect,
                    egui::Id::unique(("orch_hud", run_id.as_str())),
                    Sense::click(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Click to open orchestration in Chat");
            p.rect(
                hud_rect,
                CornerRadius::same(13),
                PANEL.gamma_multiply(0.94),
                Stroke::new(
                    1.1,
                    INFO.gamma_multiply(if hresp.hovered() { 0.9 } else { 0.55 }),
                ),
                StrokeKind::Inside,
            );
            let dot_c = Pos2::new(hud_rect.min.x + 13.0, hud_rect.center().y);
            p.circle_stroke(
                dot_c,
                3.2 + pulse * 2.4,
                Stroke::new(1.0, RUN.gamma_multiply(0.75 - 0.5 * pulse)),
            );
            p.circle_filled(dot_c, 3.0, RUN);
            p.text(
                Pos2::new(hud_rect.min.x + 24.0, hud_rect.center().y),
                egui::Align2::LEFT_CENTER,
                hud_txt,
                medium(11.0),
                INK,
            );
            if hresp.clicked() {
                hud_goto = Some(run_id);
            }
        }
    }

    // Legend.
    let ly = rect.max.y - 16.0;
    let mut lx = rect.min.x + 14.0;
    for (c, t_lbl, filled) in [
        (RUN, "subagent", true),
        (RUN, "chat", false),
        (INFO, "orchestration", false),
    ] {
        if filled {
            p.circle_filled(Pos2::new(lx + 5.0, ly), 4.5, c);
        } else {
            p.circle_stroke(Pos2::new(lx + 5.0, ly), 5.0, Stroke::new(2.0, c));
        }
        let gl = p.layout_no_wrap(t_lbl.to_string(), body(11.0), INK3);
        let w = gl.size().x;
        p.galley(Pos2::new(lx + 14.0, ly - gl.size().y / 2.0), gl, INK3);
        lx += w + 26.0;
    }
    let hint_text = if rect.width() < 540.0 {
        "Ctrl+scroll: zoom · scroll: page · double-click: center"
    } else {
        "Ctrl+scroll: zoom · scroll: page · drag: pan · double-click: center"
    };
    let hint = p.layout_no_wrap(hint_text.into(), body(11.0), INK3);
    let right = rect.max.x - 14.0;
    let hint_y = if right - hint.size().x < lx {
        ly - 16.0
    } else {
        ly
    };
    p.galley(
        Pos2::new(
            (right - hint.size().x).max(rect.min.x + 14.0),
            hint_y - hint.size().y / 2.0,
        ),
        hint,
        INK3,
    );

    if hovered.is_some() {
        ui.ctx().set_cursor_icon(if g.drag.is_some() {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::PointingHand
        });
    } else if resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }
    let clicked = if resp.clicked() && !zoom_bar_hit {
        hovered.map(|i| (g.nodes[i].data.clone(), g.nodes[i].parent.clone()))
    } else {
        None
    };
    let spawning = g.nodes.iter().any(|n| n.spawn < 0.99);
    let live = moving || spawning || g.nodes.iter().any(|n| n.alive);
    if let Some(tp) = tip {
        resp.on_hover_text(tp);
    }
    if let Some(run_id) = hud_goto {
        app.ui.sel_task = run_id;
        app.page = crate::gui::Page::Tasks;
    } else if let Some((data, parent)) = clicked {
        match data {
            NodeData::Core => app.ui.sel = "core".into(),
            NodeData::Brain { brain, .. } => app.ui.sel = brain.id,
            NodeData::Orchestration { run_id, .. } => {
                app.ui.sel_task = run_id;
                app.page = crate::gui::Page::Tasks;
            }
            NodeData::Branch {
                task_id, run_id, ..
            } => {
                app.ui.sel_task = if task_id.is_empty() { run_id } else { task_id };
                app.page = crate::gui::Page::Tasks;
            }
            NodeData::Sub => app.ui.sel = parent.trim_start_matches("b:").to_string(),
            NodeData::Task { task_id, .. } => {
                app.ui.sel_task = task_id;
                app.page = crate::gui::Page::Tasks;
            }
        }
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(if live {
            22
        } else {
            1000
        }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::home::parse_home_snap;
    use serde_json::json;

    #[test]
    fn graph_removes_inactive_subagents_and_hidden_tasks() {
        let mut graph = Graph::default();
        let mut snap =
            json!({"brains": [{"id": "A", "subagents": [{"id": "sub", "alive": true}]}]});
        let mut tasks = vec![
            TaskSnap {
                id: "hidden".into(),
                brain: "A".into(),
                status: "running".into(),
                ..Default::default()
            },
            TaskSnap {
                id: "completed".into(),
                brain: "A".into(),
                status: "running".into(),
                ..Default::default()
            },
            TaskSnap {
                id: "queued".into(),
                brain: "A".into(),
                status: "queued".into(),
                ..Default::default()
            },
        ];
        let mut convs = json!({});
        graph.sync(&parse_home_snap(&snap), &tasks, &[], &convs, 150.0);
        assert_eq!(graph.nodes.len(), 6);
        assert!(
            graph.nodes[0].alive,
            "core should be alive while subagent/tasks are running"
        );
        graph.drag = graph.nodes.iter().position(|node| node.id == "s:A:sub");

        snap["brains"][0]["subagents"][0]["alive"] = json!(false);
        convs["hidden"] = json!({"deleted": true});
        tasks[1].status = "review".into();
        graph.sync(&parse_home_snap(&snap), &tasks, &[], &convs, 150.0);
        let ids: Vec<_> = graph.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["core", "b:A", "t:queued"]);
        assert_eq!(graph.drag, None);
        assert!(
            !graph.nodes[0].alive,
            "core should be idle when nothing is actively running"
        );
    }

    #[test]
    fn graph_visualizes_parallel_orchestration_and_branches() {
        let mut graph = Graph::default();
        let snap = parse_home_snap(
            &json!({"brains": [{"id": "A", "name": "Claude"}, {"id": "B", "name": "Gemini"}]}),
        );
        let run = ParallelRun {
            id: "run1".into(),
            prompt: "Auditar arquitectura".into(),
            cwd: String::new(),
            mode: "plan".into(),
            worktree: false,
            strategy: "dag".into(),
            director: "A".into(),
            director_name: "Claude".into(),
            status: "running".into(),
            waves_total: 2,
            branches: vec![crate::parallel::BranchState {
                id: "b1".into(),
                title: "Explorar UI".into(),
                role: "explore".into(),
                tier: 2,
                mode: "plan".into(),
                deps: vec![],
                files: vec![],
                prompt: String::new(),
                worktree: false,
                task_id: "t_b1".into(),
                brain: "B".into(),
                brain_name: "Gemini".into(),
                model: "gemini-4-neon-medium".into(),
                status: "running".into(),
                summary: String::new(),
                tokens: 1200,
                usd: 0.0,
                error: String::new(),
                wave: 0,
                started_at: 0.0,
                finished_at: 0.0,
            }],
            total_tokens: 1200,
            total_usd: 0.0,
            created: now(),
            updated: now(),
            error: String::new(),
        };
        graph.sync(&snap, &[], &[run], &json!({}), 180.0);
        assert!(graph
            .nodes
            .iter()
            .any(|n| n.id == "o:run1" && n.kind == Kind::Orchestration));
        assert!(graph
            .nodes
            .iter()
            .any(|n| n.id == "br:run1:b1" && n.kind == Kind::Branch && n.parent == "b:B"));
        assert!(graph.nodes[0].alive);
    }

    #[test]
    fn graph_scales_positions_on_resize() {
        let mut graph = Graph::default();
        let snap = parse_home_snap(&json!({"brains": [{"id": "A"}, {"id": "B"}]}));
        graph.sync(&snap, &[], &[], &json!({}), 200.0);
        let initial_dist = graph
            .nodes
            .iter()
            .find(|n| n.id == "b:A")
            .unwrap()
            .pos
            .length();
        assert!((initial_dist - 200.0).abs() < 0.1);

        graph.sync(&snap, &[], &[], &json!({}), 100.0);
        let resized_dist = graph
            .nodes
            .iter()
            .find(|n| n.id == "b:A")
            .unwrap()
            .pos
            .length();
        assert!((resized_dist - 100.0).abs() < 0.1);
    }

    #[test]
    fn ordinary_wheel_scrolls_without_zooming_and_ctrl_wheel_zooms() {
        let ctx = egui::Context::default();
        let wheel = |modifiers| egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events: vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: Vec2::new(0.0, 2.0),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers,
            }],
            ..Default::default()
        };

        let mut plain_zoom = 0.0;
        let mut plain_scroll = 0.0;
        let mut out = ctx.run_ui(wheel(egui::Modifiers::NONE), |ui| {
            plain_zoom = ui.input(|i| i.zoom_delta());
            plain_scroll = ui.input(|i| i.smooth_scroll_delta.y);
        });
        out.textures_delta.clear();
        assert!(
            (plain_zoom - 1.0).abs() < 1e-5,
            "plain wheel must not zoom: {plain_zoom}"
        );
        assert!(
            plain_scroll.abs() > 0.0,
            "plain wheel must scroll: {plain_scroll}"
        );

        let mut ctrl_zoom = 0.0;
        let mut ctrl_scroll = 0.0;
        let mut out = ctx.run_ui(wheel(egui::Modifiers::CTRL), |ui| {
            ctrl_zoom = ui.input(|i| i.zoom_delta());
            ctrl_scroll = ui.input(|i| i.smooth_scroll_delta.y);
        });
        out.textures_delta.clear();
        assert!(ctrl_zoom > 1.0, "Ctrl+wheel must zoom in: {ctrl_zoom}");
        assert!(
            ctrl_scroll.abs() < 1e-5,
            "Ctrl+wheel must not scroll the page: {ctrl_scroll}"
        );
        assert!(
            (ctx.zoom_factor() - 1.0).abs() < 1e-5,
            "Ctrl+wheel must not rescale the whole UI"
        );
    }

    #[test]
    fn map_geometry_fits_compact_and_wide_viewports_without_core_overlap() {
        for (w, h) in [
            (420.0, 320.0),
            (540.0, 440.0),
            (800.0, 480.0),
            (1150.0, 650.0),
        ] {
            let (nr, orbit) = map_geometry(Vec2::new(w, h));
            let min_sep = nr * 0.95 + nr + 12.0;
            assert!(
                orbit >= min_sep,
                "orbit {orbit} overlaps core for {w}x{h} (nr={nr})"
            );
            let half = (w - 24.0).min(h - 48.0) * 0.5;
            assert!(
                orbit + nr <= half + 2.0,
                "rings clip viewport for {w}x{h}: orbit={orbit} nr={nr} half={half}"
            );
        }
    }
}
