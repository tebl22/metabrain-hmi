//! Drag-and-drop command hierarchy strip (levels, round tables, swaps, and rank saving).

use super::{ChainSnap, HomeSnap};
use crate::gui::theme::*;
use crate::gui::App;
use eframe::egui::{self, Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Levels from the chain: consecutive entries with the same rank share a level.
pub(super) fn levels(chain: &[ChainSnap]) -> Vec<Vec<ChainSnap>> {
    let mut out: Vec<Vec<ChainSnap>> = Vec::new();
    let mut last: Option<i64> = None;
    for e in chain {
        let r = e.rank;
        if r.is_some() && r == last {
            if let Some(l) = out.last_mut() {
                l.push(e.clone());
                continue;
            }
        }
        out.push(vec![e.clone()]);
        last = r;
    }
    out
}

/// Move one brain up (-1) or down (+1) a level; out of a shared level it gets its own.
pub(super) fn move_brain(lv: &[Vec<ChainSnap>], id: &str, dir: i32) -> Vec<Vec<ChainSnap>> {
    let mut v = lv.to_vec();
    let Some(li) = v.iter().position(|l| l.iter().any(|e| e.id == id)) else {
        return v;
    };
    let pos = v[li].iter().position(|e| e.id == id).unwrap_or(0);
    if v[li].len() > 1 {
        let x = v[li].remove(pos);
        let at = if dir < 0 { li } else { li + 1 };
        v.insert(at, vec![x]);
    } else {
        let to = li as i32 + dir;
        if to >= 0 && (to as usize) < v.len() {
            v.swap(li, to as usize);
        }
    }
    v.retain(|l| !l.is_empty());
    v
}

pub(super) fn send(app: &App, lv: &[Vec<ChainSnap>]) {
    let mut order: Vec<Value> = Vec::new();
    let mut ranks = Map::new();
    for (i, l) in lv.iter().enumerate() {
        for e in l {
            order.push(json!(e.id));
            ranks.insert(e.id.clone(), json!(i as i64 + 1));
        }
    }
    app.act(json!({"a": "priority", "order": order, "ranks": ranks}));
}

/// Command hierarchy as a diagram: one lane per level, top commands first. Drag a brain's
/// box onto another lane to share that level, or into the gap between lanes to give it a
/// level of its own there. Boxes glide to their new place.
pub(super) fn hierarchy(app: &mut App, ui: &mut egui::Ui, snap: &HomeSnap) {
    let lv = levels(&snap.chain);
    section(
        ui,
        "Command Hierarchy",
        Some("drag onto another brain to swap priority, into a shared tier for round-table execution, or between tiers for a dedicated level"),
    );
    let colors: HashMap<&str, Color32> = snap
        .brains
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id.as_str(), series(i)))
        .collect();

    let gap = 24.0;
    let box_h = 52.0;
    let row_gap = 8.0;
    let box_gap = 10.0;
    let width = ui.available_width().max(260.0);
    let label_w = if width < 560.0 { 104.0 } else { 156.0 };
    let usable_w = (width - label_w - 14.0).max(140.0);
    let cols_per_row = (((usable_w + box_gap) / (155.0 + box_gap)).floor() as usize).max(1);
    let box_w = ((usable_w - box_gap * (cols_per_row.saturating_sub(1) as f32))
        / cols_per_row as f32)
        .clamp(130.0, 190.0);
    let n = lv.len().max(1);
    let lane_heights: Vec<f32> = lv
        .iter()
        .map(|l| {
            let rows = l.len().max(1).div_ceil(cols_per_row);
            22.0 + rows as f32 * box_h + rows.saturating_sub(1) as f32 * row_gap
        })
        .collect();
    let mut lane_ys: Vec<f32> = Vec::with_capacity(n);
    let mut cur_y = 0.0;
    for (i, lh) in lane_heights.iter().enumerate() {
        lane_ys.push(cur_y);
        cur_y += *lh + if i + 1 < n { gap } else { 0.0 };
    }
    let height = cur_y + 8.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    let p = ui.painter_at(rect.expand(4.0));
    let ctx = ui.ctx().clone();
    let pointer = ctx.pointer_interact_pos();

    let lane_rect = |i: usize| {
        Rect::from_min_size(
            Pos2::new(rect.min.x, rect.min.y + lane_ys[i]),
            Vec2::new(width, lane_heights[i]),
        )
    };
    // Box rectangles, final positions.
    let mut boxes: Vec<(String, usize, Rect)> = vec![];
    for (li, l) in lv.iter().enumerate() {
        let lr = lane_rect(li);
        for (k, e) in l.iter().enumerate() {
            let col = k % cols_per_row;
            let row = k / cols_per_row;
            let x = lr.min.x + label_w + col as f32 * (box_w + box_gap);
            let y = lr.min.y + 11.0 + row as f32 * (box_h + row_gap);
            let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(box_w, box_h));
            boxes.push((e.id.clone(), li, r));
        }
    }

    // Start / continue / finish a drag.
    if resp.drag_started() {
        if let Some(pp) = pointer {
            app.ui.hdrag = boxes
                .iter()
                .find(|(_, _, r)| r.contains(pp))
                .map(|(id, _, r)| (id.clone(), pp - r.min));
        }
    }
    let dragging = app.ui.hdrag.clone().filter(|_| resp.dragged());
    // The drop frame no longer reports `dragged`, so the target is also computed then.
    let active = app
        .ui
        .hdrag
        .clone()
        .filter(|_| resp.dragged() || resp.drag_stopped());
    // Drop target from the pointer: a lane (share) or the gap above/below a lane (own level).
    enum Drop {
        Join(usize),
        NewAt(usize),
        Swap(String),
    }
    let target = active
        .as_ref()
        .and_then(|d| pointer.map(|pp| (d.0.clone(), pp)))
        .map(|(me, pp)| {
            if let Some((other, _, _)) = boxes
                .iter()
                .find(|(id, _, r)| *id != me && r.expand(4.0).contains(pp))
            {
                return Drop::Swap(other.clone());
            }
            let rel = pp.y - rect.min.y;
            if rel < 0.0 {
                return Drop::NewAt(0);
            }
            for i in 0..n {
                let y0 = lane_ys[i];
                let y1 = y0 + lane_heights[i];
                if rel < y0 {
                    return Drop::NewAt(i);
                }
                if rel <= y1 {
                    let within = rel - y0;
                    return if within <= 14.0 {
                        Drop::NewAt(i)
                    } else if within >= lane_heights[i] - 14.0 {
                        Drop::NewAt(i + 1)
                    } else {
                        Drop::Join(i)
                    };
                }
            }
            Drop::NewAt(n)
        });

    // Lanes.
    for li in 0..n {
        let lr = lane_rect(li);
        let commanding = lv.get(li).is_some_and(|l| l.iter().any(|e| e.master));
        let hot = matches!(target, Some(Drop::Join(i)) if i == li);
        p.rect(
            lr,
            CornerRadius::same(8),
            if hot {
                Color32::from_rgb(0x2E, 0x2C, 0x25)
            } else if commanding {
                Color32::from_rgb(0x28, 0x27, 0x22)
            } else {
                PANEL
            },
            Stroke::new(
                1.0,
                if hot {
                    MASTER
                } else if commanding {
                    MASTER.gamma_multiply(0.45)
                } else {
                    RULE
                },
            ),
            StrokeKind::Inside,
        );
        p.text(
            Pos2::new(lr.min.x + 16.0, lr.center().y - 8.0),
            egui::Align2::LEFT_CENTER,
            format!("Tier {}", li + 1),
            cond(15.0),
            if commanding { MASTER } else { INK2 },
        );
        let sub = if commanding {
            if lv.get(li).is_some_and(|l| l.len() > 1) {
                "round table: shared command"
            } else {
                "in command"
            }
        } else if li == 0 {
            "primary"
        } else {
            "backup"
        };
        p.text(
            Pos2::new(lr.min.x + 16.0, lr.center().y + 10.0),
            egui::Align2::LEFT_CENTER,
            sub,
            body(11.0),
            INK3,
        );
        if hot {
            p.text(
                Pos2::new(lr.max.x - 16.0, lr.center().y),
                egui::Align2::RIGHT_CENTER,
                "round table: shared command",
                medium(11.0),
                MASTER,
            );
        }
        // Down arrow to the next level.
        if li + 1 < n {
            let x = lr.min.x + 40.0;
            let (y0, y1) = (lr.max.y + 4.0, lr.max.y + gap - 4.0);
            p.line_segment([Pos2::new(x, y0), Pos2::new(x, y1)], Stroke::new(1.2, INK3));
            p.line_segment(
                [Pos2::new(x - 4.0, y1 - 5.0), Pos2::new(x, y1)],
                Stroke::new(1.2, INK3),
            );
            p.line_segment(
                [Pos2::new(x + 4.0, y1 - 5.0), Pos2::new(x, y1)],
                Stroke::new(1.2, INK3),
            );
        }
    }
    // Insertion line for "own level here".
    if let Some(Drop::NewAt(i)) = target {
        let y = if i == 0 {
            rect.min.y - 6.0
        } else if i >= n {
            rect.max.y + 2.0
        } else {
            lane_rect(i).min.y - gap / 2.0
        };
        p.line_segment(
            [
                Pos2::new(rect.min.x + 8.0, y),
                Pos2::new(rect.max.x - 8.0, y),
            ],
            Stroke::new(2.5, MASTER),
        );
        p.text(
            Pos2::new(rect.max.x - 12.0, y - 9.0),
            egui::Align2::RIGHT_CENTER,
            "dedicated tier here",
            medium(11.0),
            MASTER,
        );
    }

    // Boxes glide to their place (animated), the dragged one follows the pointer.
    let mut hover_box: Option<String> = None;
    for (id, li, r) in &boxes {
        let e = lv[*li]
            .iter()
            .find(|e| &e.id == id)
            .cloned()
            .unwrap_or_default();
        let is_drag = dragging.as_ref().map(|d| &d.0 == id).unwrap_or(false);
        let pos = if is_drag {
            pointer
                .map(|pp| pp - dragging.as_ref().map(|d| d.1).unwrap_or(Vec2::ZERO))
                .unwrap_or(r.min)
        } else {
            let x =
                ctx.animate_value_with_time(egui::Id::unique(("hx", id.as_str())), r.min.x, 0.25);
            let y =
                ctx.animate_value_with_time(egui::Id::unique(("hy", id.as_str())), r.min.y, 0.25);
            Pos2::new(x, y)
        };
        let br = Rect::from_min_size(pos, r.size());
        if pointer.map(|pp| br.contains(pp)).unwrap_or(false) && dragging.is_none() {
            hover_box = Some(id.clone());
        }
        let col = colors.get(id.as_str()).copied().unwrap_or(INK3);
        let lift = if is_drag { 1.0 } else { 0.0 };
        if is_drag {
            p.rect_filled(
                br.translate(Vec2::new(0.0, 6.0)),
                CornerRadius::same(R),
                Color32::from_black_alpha(90),
            );
        }
        p.rect(
            br,
            CornerRadius::same(R),
            if is_drag || hover_box.as_deref() == Some(id) {
                RAISED2
            } else {
                RAISED
            },
            Stroke::new(
                1.0 + lift,
                if is_drag || matches!(&target, Some(Drop::Swap(o)) if o == id) {
                    MASTER
                } else if app.ui.sel == *id {
                    INK3
                } else {
                    RULE
                },
            ),
            StrokeKind::Inside,
        );
        p.rect_filled(
            Rect::from_min_max(br.min, Pos2::new(br.min.x + 3.0, br.max.y)),
            CornerRadius {
                nw: R,
                sw: R,
                ne: 0,
                se: 0,
            },
            if e.master { MASTER } else { col },
        );
        let tp = p.with_clip_rect(Rect::from_min_max(
            br.min,
            Pos2::new(br.max.x - 24.0, br.max.y),
        ));
        p.circle_filled(Pos2::new(br.min.x + 16.0, br.min.y + 17.0), 4.0, col);
        tp.text(
            Pos2::new(br.min.x + 26.0, br.min.y + 17.0),
            egui::Align2::LEFT_CENTER,
            &e.name,
            bold(13.0),
            if e.available { INK } else { INK2 },
        );
        let reason = &e.reason;
        tp.text(
            Pos2::new(br.min.x + 14.0, br.min.y + 36.0),
            egui::Align2::LEFT_CENTER,
            reason,
            body(11.5),
            if e.available { INK3 } else { ATTN },
        );
        // Grip dots on the right say "you can drag me".
        paint_icon(
            &p,
            Rect::from_center_size(Pos2::new(br.max.x - 14.0, br.center().y), Vec2::splat(14.0)),
            Icon::Grip,
            INK3,
        );
    }
    if dragging.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        ctx.request_repaint();
    } else if hover_box.is_some() {
        ctx.set_cursor_icon(egui::CursorIcon::Grab);
    }
    // Animations need frames until they land.
    if ctx.input(|i| i.time) < 0.0
        || boxes.iter().any(|(id, _, r)| {
            (ctx.animate_value_with_time(egui::Id::unique(("hx", id.as_str())), r.min.x, 0.25)
                - r.min.x)
                .abs()
                > 0.5
                || (ctx.animate_value_with_time(
                    egui::Id::unique(("hy", id.as_str())),
                    r.min.y,
                    0.25,
                ) - r.min.y)
                    .abs()
                    > 0.5
        })
    {
        ctx.request_repaint();
    }

    if resp.clicked() {
        if let Some(h) = hover_box.clone() {
            app.ui.sel = h;
        }
    }
    if resp.drag_stopped() {
        if let (Some((id, _)), Some(t)) = (app.ui.hdrag.take(), target) {
            let mut v: Vec<Vec<ChainSnap>> = lv.clone();
            let from = v.iter().position(|l| l.iter().any(|e| e.id == id));
            if let Some(fi) = from {
                let pos = v[fi].iter().position(|e| e.id == id).unwrap_or(0);
                let item = v[fi].remove(pos);
                let was_alone = v[fi].is_empty();
                match t {
                    Drop::Swap(other) => {
                        v[fi].insert(pos, item);
                        let a = (fi, pos);
                        let b = v.iter().enumerate().find_map(|(li, l)| {
                            l.iter().position(|e| e.id == other).map(|k| (li, k))
                        });
                        if let Some(b) = b {
                            let ea = v[a.0][a.1].clone();
                            let eb = v[b.0][b.1].clone();
                            v[a.0][a.1] = eb;
                            v[b.0][b.1] = ea;
                        }
                    }
                    Drop::Join(ti) => {
                        if ti < v.len() {
                            v[ti].push(item);
                        }
                    }
                    Drop::NewAt(mut ti) => {
                        // Removing an emptied lane above shifts the target up by one.
                        if was_alone && fi < ti {
                            ti -= 1;
                        }
                        v.retain(|l| !l.is_empty());
                        v.insert(ti.min(v.len()), vec![item]);
                    }
                }
                v.retain(|l| !l.is_empty());
                let shape = |rows: &[Vec<ChainSnap>]| -> Vec<Vec<String>> {
                    rows.iter()
                        .map(|l| l.iter().map(|e| e.id.clone()).collect())
                        .collect()
                };
                if shape(&v) != shape(&lv) {
                    send(app, &v);
                    app.say("Hierarchy saved", MASTER);
                }
            }
        }
        app.ui.hdrag = None;
    }
}
