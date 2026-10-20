//! Point list on the drawing: hovering a device (on the drawing or in the review panel) lights up
//! its address and the room it was matched to in its status colour, joined by a line, with the
//! room's name and number. "Show missed" flashes address-like text that wasn't read.

use cadcraft_geom::Vec2;
use egui::{Color32, Rect, Stroke, pos2};

use super::Xf;
use crate::CadApp;
use crate::palettes::pointlist::{state, status_color};

pub(super) fn paint(app: &mut CadApp, ctx: &egui::Context, p: &egui::Painter, xf: &Xf, rect: Rect) {
    let Some(fa) = app.session.fa.as_ref() else { return };
    if fa.devices.is_empty() {
        return;
    }
    let st = state(ctx);
    let world_rect = |b: [f64; 4]| Rect::from_two_pos(xf.to_screen(Vec2::new(b[0], b[1])), xf.to_screen(Vec2::new(b[2], b[3])));
    // Missed address text, flashing.
    if crate::now_ms() < st.missed_until {
        let on = ((crate::now_ms() / 300.0) as i64) % 2 == 0;
        let c = Color32::from_rgb(0xE0, 0x4C, 0xE0);
        for (_, b, _) in &fa.missed {
            let r = world_rect(*b).expand(3.0);
            if rect.intersects(r) {
                p.rect_stroke(r, 2.0, Stroke::new(if on { 2.5 } else { 1.0 }, c), egui::StrokeKind::Outside);
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    // Which device: the panel row under the mouse, the drawing under the cursor, the focused row.
    let pass = ctx.cumulative_pass_nr();
    let panel_hover: Option<String> = ctx
        .data(|d| d.get_temp::<(String, u64)>(egui::Id::new("cadcraft-pointlist-hover")))
        .filter(|(_, n)| pass.saturating_sub(*n) <= 1)
        .map(|(id, _)| id);
    let cursor = app.canvas.cursor;
    let hover_handle = app.canvas.hover;
    let under_cursor = cursor.and_then(|c| {
        fa.devices.iter().find(|d| {
            hover_handle.is_some_and(|h| d.markers.contains(&h)) || {
                let pad = ((d.bbox[3] - d.bbox[1]) * 0.3).max(1e-6);
                c.x >= d.bbox[0] - pad && c.x <= d.bbox[2] + pad && c.y >= d.bbox[1] - pad && c.y <= d.bbox[3] + pad
            }
        })
    });
    let dev = panel_hover
        .and_then(|id| fa.devices.iter().find(|d| d.id == id))
        .or(under_cursor)
        .or_else(|| st.focus.as_ref().and_then(|id| fa.devices.iter().find(|d| &d.id == id)));
    let Some(d) = dev else { return };
    let c = status_color(&d.status);
    let dr = world_rect(d.bbox).expand(3.0);
    p.rect(dr, 2.0, c.gamma_multiply(0.18), Stroke::new(2.0, c), egui::StrokeKind::Outside);
    let font = egui::FontId::proportional(13.0);
    let label = if d.address.is_empty() { "?".to_string() } else { d.address.clone() };
    if let Some(rb) = d.room_bbox {
        let rr = world_rect(rb).expand(3.0);
        p.rect(rr, 2.0, c.gamma_multiply(0.18), Stroke::new(2.0, c), egui::StrokeKind::Outside);
        p.add(egui::Shape::dashed_line(&[dr.center(), rr.center()], Stroke::new(1.5, c), 6.0, 4.0));
    }
    let room = match (d.room.is_empty(), d.room_num.is_empty()) {
        (true, true) => "no room".to_string(),
        (true, false) => d.room_num.clone(),
        (false, true) => d.room.clone(),
        (false, false) => format!("{} ({})", d.room, d.room_num),
    };
    let text = if d.function.is_empty() { format!("{label} → {room}") } else { format!("{label} · {} → {room}", d.function) };
    let at = pos2(dr.right() + 6.0, dr.top() - 18.0);
    let galley = p.layout_no_wrap(text, font, Color32::WHITE);
    let bg = Rect::from_min_size(at, galley.size()).expand(4.0);
    p.rect_filled(bg, 3.0, Color32::from_black_alpha(220));
    p.rect_stroke(bg, 3.0, Stroke::new(1.0, c), egui::StrokeKind::Outside);
    p.galley(at, galley, Color32::WHITE);
}
