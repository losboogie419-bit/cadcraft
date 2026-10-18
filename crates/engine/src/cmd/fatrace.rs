//! Fire alarm wiring: follow a wire (FATRACE) and find wires that stop short (WIRECHECK).
//!
//! The connectivity rules are the usual CAD ones: two pieces join when an end of one touches the
//! other (end to end, or an end landing on the middle of the other, a T). Two wires that only cross
//! do not join. A small circle at a wire's end (a screw, a pin) is where the wire lands; it ends
//! the trace there instead of joining everything that touches the same terminal block.
//!
//! FATRACE selects every piece of the net under a point (or a handle) and reports where its loose
//! ends land: on a screw, or near which label. WIRECHECK lists wire ends that touch nothing (no
//! other line, no screw), the usual sign of a missing or short-drawn wire, and rings each one on
//! layer `FA_CHECK` (amber; replaced on every run).
//!
//! Lines on the PDF reader's hidden layers, closed outlines and very long border lines are left
//! out. Rule checks that need terminal roles (relay common on an SLC, a jumper onto a card) need
//! verified cut-sheet terminals and are not done here.

use std::collections::HashMap;

use cadcraft_color::{Color, Rgb};
use cadcraft_doc::{Circle, Common, EntityKind, Handle, Layer, Space};
use cadcraft_geom::{Vec2, Vec3};
use serde_json::{Value, json};

use super::*;
use crate::{Result, Session};

pub const CHECK_LAYER: &str = "FA_CHECK";
const AMBER: Rgb = Rgb(0xFF, 0xB0, 0x00);
/// Layers never traced (hidden PDF copies, tag and check layers).
const SKIP_LAYERS: &[&str] = &[
    "PDF_PAGES",
    "FA-POINTLIST",
    "PDF_SHX_STROKES",
    "PDF_SHX_GLYPHS",
    "PDF_FILLS",
    "PDF_SHX_SEARCH",
    "PDF_TEXT_HIDDEN",
    "PDF_WHITE",
    "FA_READ",
    CHECK_LAYER,
];
/// Pieces per drawing the tracer looks at.
const MAX_PIECES: usize = 2_000_000;
/// Most loose ends listed.
const MAX_LOOSE: usize = 2000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("fatrace", "Trace Wire", run_fatrace)
            .menu(&["Tools", "Fire Alarm", "Trace Wire"])
            .alias(&["wiretrace", "fat"])
            .params("{point: [x, y] | handle | (nothing: the selected wire), tol?: drawing units (default 0.008), select?: true} → {wires, handles, length, ends: [{at, lands: screw|label|loose, label?}]}")
            .enabled(has_doc),
        CommandSpec::new("wirecheck", "Wire Check", run_wirecheck)
            .menu(&["Tools", "Fire Alarm", "Wire Check (loose wire ends)"])
            .alias(&["wchk"])
            .params("{tol?: drawing units (default 0.008), window?: [[x0,y0],[x1,y1]], mark?: true (rings on layer FA_CHECK), minLength?: shortest piece counted as a wire (default 50 × tol), all?: false (true: every open end, not only ends near a terminal), reach?: how near a terminal an end must be (default 40 × tol), layers?: wire layers to check (default: layers named WIRE/SLC/NAC/CIRCUIT/CABLE, else all)} → {loose: [{at, handle, label?}], count}")
            .enabled(has_doc),
    ]
}

struct Piece {
    handle: Handle,
    pts: Vec<Vec2>,
    /// A very long line (sheet border, cabinet outline): others can end on it, but it isn't traced
    /// or reported.
    border: bool,
}

struct Net {
    pieces: Vec<Piece>,
    /// Small circles (screws, pins): (centre, radius).
    screws: Vec<(Vec2, f64)>,
    texts: Vec<(Vec2, String)>,
    /// Filled shapes (arrowheads, dots) by their bounding boxes.
    fills: Vec<Bbox>,
    grid: HashMap<(i64, i64), Vec<(usize, usize)>>,
    cell: f64,
}

#[derive(Clone, Copy, Debug)]
struct Bbox(f64, f64, f64, f64);

impl Bbox {
    fn contains_or_near(&self, p: Vec2, tol: f64) -> bool {
        p.x >= self.0 - tol && p.x <= self.2 + tol && p.y >= self.1 - tol && p.y <= self.3 + tol
    }
}

fn seg_dist(a: Vec2, b: Vec2, p: Vec2) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let l = dx * dx + dy * dy;
    let t = if l > 0.0 { (((p.x - a.x) * dx + (p.y - a.y) * dy) / l).clamp(0.0, 1.0) } else { 0.0 };
    (p.x - (a.x + t * dx)).hypot(p.y - (a.y + t * dy))
}

fn gather(s: &Session, tol: f64) -> Result<Net> {
    let st = s.state()?;
    let doc = &st.doc;
    let mut n =
        Net { pieces: Vec::new(), screws: Vec::new(), texts: Vec::new(), fills: Vec::new(), grid: HashMap::new(), cell: (tol * 40.0).max(1e-6) };
    let Some(store) = doc.space(&Space::Model) else { return Ok(n) };
    let ext = doc_extent(store.iter().map(|e| &e.kind));
    let long = ext * 0.6;
    for e in store.iter() {
        if SKIP_LAYERS.contains(&e.common.layer.as_str()) || doc.layer(&e.common.layer).is_some_and(|l| !l.on || l.frozen) {
            continue;
        }
        let mut outline = false;
        let pts: Vec<Vec2> = match &e.kind {
            EntityKind::Line(l) => vec![l.a.xy(), l.b.xy()],
            EntityKind::LwPolyline(p) if p.vertices.len() >= 2 => {
                let mut v: Vec<Vec2> = p.vertices.iter().map(|v| v.p).collect();
                if p.closed {
                    // Outlines (boxes, card edges): wires end on them, but they aren't wires.
                    if let Some(f) = v.first().copied() {
                        v.push(f);
                    }
                    outline = true;
                }
                v
            }
            EntityKind::Arc(a) if a.radius.is_finite() && a.radius > 0.0 => {
                let sweep = (a.end - a.start).rem_euclid(std::f64::consts::TAU);
                (0..=8)
                    .map(|i| {
                        let t = a.start + sweep * f64::from(i) / 8.0;
                        Vec2::new(a.center.x + a.radius * t.cos(), a.center.y + a.radius * t.sin())
                    })
                    .collect()
            }
            EntityKind::Circle(c) if c.radius.is_finite() && c.radius > 0.0 => {
                n.screws.push((c.center.xy(), c.radius));
                continue;
            }
            EntityKind::Text(t) => {
                n.texts.push((t.insert.xy(), t.value.trim().to_string()));
                continue;
            }
            EntityKind::Solid(so) => {
                let xs = so.corners.iter().map(|c| c.x);
                let ys = so.corners.iter().map(|c| c.y);
                n.fills.push(Bbox(
                    xs.clone().fold(f64::INFINITY, f64::min),
                    ys.clone().fold(f64::INFINITY, f64::min),
                    xs.fold(f64::NEG_INFINITY, f64::max),
                    ys.fold(f64::NEG_INFINITY, f64::max),
                ));
                continue;
            }
            _ => continue,
        };
        let len: f64 = pts.windows(2).map(|w| if let [a, b] = w { (a.x - b.x).hypot(a.y - b.y) } else { 0.0 }).sum();
        if len <= 0.0 {
            continue;
        }
        n.pieces.push(Piece { handle: e.handle, pts, border: outline || len > long });
        if n.pieces.len() >= MAX_PIECES {
            break;
        }
    }
    for (pi, p) in n.pieces.iter().enumerate() {
        for (si, w) in p.pts.windows(2).enumerate() {
            let [a, b] = w else { continue };
            let (x0, x1) = (((a.x.min(b.x) - tol) / n.cell).floor() as i64, ((a.x.max(b.x) + tol) / n.cell).floor() as i64);
            let (y0, y1) = (((a.y.min(b.y) - tol) / n.cell).floor() as i64, ((a.y.max(b.y) + tol) / n.cell).floor() as i64);
            if (x1 - x0 + 1).saturating_mul(y1 - y0 + 1) > 4000 {
                continue;
            }
            for gx in x0..=x1 {
                for gy in y0..=y1 {
                    n.grid.entry((gx, gy)).or_default().push((pi, si));
                }
            }
        }
    }
    Ok(n)
}

fn doc_extent<'a>(kinds: impl Iterator<Item = &'a EntityKind>) -> f64 {
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for k in kinds {
        let p = match k {
            EntityKind::Line(l) => l.a.xy(),
            EntityKind::Circle(c) => c.center.xy(),
            EntityKind::Text(t) => t.insert.xy(),
            _ => continue,
        };
        b = [b[0].min(p.x), b[1].min(p.y), b[2].max(p.x), b[3].max(p.y)];
    }
    if b[0].is_finite() { (b[2] - b[0]).max(b[3] - b[1]) } else { f64::INFINITY }
}

impl Net {
    fn near(&self, p: Vec2) -> Vec<(usize, usize)> {
        let (gx, gy) = ((p.x / self.cell).floor() as i64, (p.y / self.cell).floor() as i64);
        let mut out = Vec::new();
        for i in -1..=1 {
            for j in -1..=1 {
                if let Some(l) = self.grid.get(&(gx + i, gy + j)) {
                    out.extend_from_slice(l);
                }
            }
        }
        out
    }
    fn seg(&self, pi: usize, si: usize) -> Option<(Vec2, Vec2)> {
        let p = self.pieces.get(pi)?;
        Some((*p.pts.get(si)?, *p.pts.get(si + 1)?))
    }
    fn ends(&self, pi: usize) -> Vec<Vec2> {
        self.pieces.get(pi).map(|p| [p.pts.first(), p.pts.last()].into_iter().flatten().copied().collect()).unwrap_or_default()
    }
    /// The screw a point lands on (a small circle containing it).
    fn screw_at(&self, p: Vec2, tol: f64) -> Option<(Vec2, f64)> {
        self.screws.iter().copied().filter(|(c, r)| *r < tol * 40.0 && (p.x - c.x).hypot(p.y - c.y) <= r + tol).min_by(|a, b| a.1.total_cmp(&b.1))
    }
    /// An arrowhead at end `e` of piece `pi`: a filled shape there, or two or more short strokes
    /// (an outlined head) meeting it.
    fn arrow_at(&self, pi: usize, e: Vec2, tol: f64) -> bool {
        if self.fills.iter().any(|f| f.contains_or_near(e, tol * 2.0)) {
            return true;
        }
        let short = tol * 25.0;
        let mut seen: Vec<usize> = Vec::new();
        for (pj, sj) in self.near(e) {
            if pj == pi || seen.contains(&pj) {
                continue;
            }
            let Some(q) = self.pieces.get(pj) else { continue };
            let len: f64 = q.pts.windows(2).map(|w| if let [a, b] = w { (a.x - b.x).hypot(a.y - b.y) } else { 0.0 }).sum();
            if len <= short && self.seg(pj, sj).is_some_and(|(a, b)| seg_dist(a, b, e) <= tol * 2.0) {
                seen.push(pj);
            }
        }
        seen.len() >= 2 || (seen.len() == 1 && self.pieces.get(seen[0]).is_some_and(|q| q.pts.len() >= 3))
    }

    fn is_short(&self, pi: usize, tol: f64) -> bool {
        self.pieces
            .get(pi)
            .is_some_and(|q| q.pts.windows(2).map(|w| if let [a, b] = w { (a.x - b.x).hypot(a.y - b.y) } else { 0.0 }).sum::<f64>() <= tol * 25.0)
    }

    /// Distance from `p` to the nearest screw (terminal) edge.
    fn screw_gap(&self, p: Vec2) -> f64 {
        self.screws.iter().map(|(c, r)| ((p.x - c.x).hypot(p.y - c.y) - r).max(0.0)).fold(f64::INFINITY, f64::min)
    }

    fn label_near(&self, p: Vec2, reach: f64) -> Option<String> {
        self.texts
            .iter()
            .filter(|(at, v)| !v.is_empty() && (at.x - p.x).hypot(at.y - p.y) < reach)
            .min_by(|a, b| (a.0.x - p.x).hypot(a.0.y - p.y).total_cmp(&(b.0.x - p.x).hypot(b.0.y - p.y)))
            .map(|(_, v)| v.clone())
    }
    /// Pieces this piece's ends touch (end to end, or an end landing on a segment).
    fn touching(&self, pi: usize, tol: f64) -> Vec<(usize, Vec2)> {
        let mut out: Vec<(usize, Vec2)> = Vec::new();
        for e in self.ends(pi) {
            for (pj, sj) in self.near(e) {
                if pj == pi || out.iter().any(|o| o.0 == pj) {
                    continue;
                }
                if let Some((a, b)) = self.seg(pj, sj)
                    && seg_dist(a, b, e) <= tol
                {
                    out.push((pj, e));
                }
            }
        }
        // Others whose ends land on this piece.
        if let Some(p) = self.pieces.get(pi) {
            for w in p.pts.windows(2) {
                let [a, b] = w else { continue };
                for q in [*a, *b, Vec2::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0)] {
                    for (pj, _) in self.near(q) {
                        if pj == pi || out.iter().any(|o| o.0 == pj) {
                            continue;
                        }
                        if let Some(e) = self.ends(pj).into_iter().find(|e| seg_dist(*a, *b, *e) <= tol) {
                            out.push((pj, e));
                        }
                    }
                }
            }
        }
        out
    }
}

fn tol_of(p: &Value) -> f64 {
    p.get("tol").and_then(Value::as_f64).filter(|t| t.is_finite() && *t > 0.0 && *t < 1e6).unwrap_or(0.008)
}

fn run_fatrace(s: &mut Session, p: &Value) -> Result<Value> {
    let tol = tol_of(p);
    let net = gather(s, tol)?;
    let start = if let Some(h) = str_param(p, "handle") {
        net.pieces
            .iter()
            .position(|q| q.handle.hex().eq_ignore_ascii_case(h))
            .ok_or_else(|| bad("fatrace", "that object isn't a wire piece (line, open polyline or arc)"))?
    } else if point_param(p, "point").is_none() {
        // Typed or from the menu: trace the selected wire.
        let sel = s.selection();
        net.pieces.iter().position(|q| sel.contains(&q.handle)).ok_or_else(|| bad("fatrace", "select a wire first (click it), then run FATRACE"))?
    } else {
        let at = point_param(p, "point").ok_or_else(|| bad("fatrace", "`point` [x, y] or `handle` is required"))?;
        let mut best: Option<(f64, usize)> = None;
        for (pi, si) in net.near(at) {
            if let Some((a, b)) = net.seg(pi, si) {
                let d = seg_dist(a, b, at);
                if d <= tol * 6.0 && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, pi));
                }
            }
        }
        best.map(|b| b.1).ok_or_else(|| bad("fatrace", "no wire under that point"))?
    };
    // Flood the net; a screw at a piece's end stops the flood there.
    let mut seen = vec![false; net.pieces.len()];
    let mut stack = vec![start];
    let mut members = Vec::new();
    while let Some(pi) = stack.pop() {
        if seen.get(pi).copied().unwrap_or(true) {
            continue;
        }
        if let Some(v) = seen.get_mut(pi) {
            *v = true;
        }
        members.push(pi);
        if members.len() > 200_000 {
            break;
        }
        for (pj, at) in net.touching(pi, tol) {
            // Two pieces meeting on a screw are each landed there: that's a terminal, not a joint.
            let border = net.pieces.get(pj).is_some_and(|q| q.border);
            // Arrowhead strokes at a wire's end are the end, not more wire.
            let head = net.is_short(pj, tol) && net.arrow_at(pi, at, tol);
            if !border && !head && net.screw_at(at, tol).is_none() && !seen.get(pj).copied().unwrap_or(true) {
                stack.push(pj);
            }
        }
    }
    let handles: Vec<Handle> = members.iter().filter_map(|i| net.pieces.get(*i).map(|q| q.handle)).collect();
    let length: f64 = members
        .iter()
        .filter_map(|i| net.pieces.get(*i))
        .map(|q| q.pts.windows(2).map(|w| if let [a, b] = w { (a.x - b.x).hypot(a.y - b.y) } else { 0.0 }).sum::<f64>())
        .sum();
    // Where it ends: piece ends not touching another member.
    let mut ends = Vec::new();
    for &pi in &members {
        for e in net.ends(pi) {
            let joined = members.iter().any(|&pj| {
                pj != pi
                    && net.pieces.get(pj).is_some_and(|q| q.pts.windows(2).any(|w| if let [a, b] = w { seg_dist(*a, *b, e) <= tol } else { false }))
            });
            if joined {
                continue;
            }
            let v = match net.screw_at(e, tol) {
                Some((c, _)) => json!({ "at": [e.x, e.y], "lands": "screw", "screw": [c.x, c.y], "label": net.label_near(c, tol * 60.0) }),
                None => match net.label_near(e, tol * 30.0) {
                    Some(l) => json!({ "at": [e.x, e.y], "lands": "label", "label": l }),
                    // An arrowhead pointing at a circuit label: a normal end.
                    None if net.arrow_at(pi, e, tol) => match net.label_near(e, tol * 150.0) {
                        Some(l) => json!({ "at": [e.x, e.y], "lands": "label", "arrow": true, "label": l }),
                        None => json!({ "at": [e.x, e.y], "lands": "arrow" }),
                    },
                    None => json!({ "at": [e.x, e.y], "lands": "loose" }),
                },
            };
            ends.push(v);
        }
    }
    if p.get("select").and_then(Value::as_bool).unwrap_or(true) {
        s.set_selection(handles.clone());
    }
    let loose = ends.iter().filter(|e| e["lands"] == "loose").count();
    Ok(json!({
        "wires": handles.len(),
        "handles": handles.iter().map(|h| h.hex()).collect::<Vec<_>>(),
        "length": length,
        "ends": ends,
        "message": format!("Traced {} piece(s), {:.2} long, {} end(s){}", handles.len(), length, ends.len(), if loose > 0 { format!(", {loose} loose") } else { String::new() }),
    }))
}

fn run_wirecheck(s: &mut Session, p: &Value) -> Result<Value> {
    let tol = tol_of(p);
    let window = p.get("window").and_then(Value::as_array).and_then(|a| {
        let a0 = point_value(a.first()?)?;
        let a1 = point_value(a.get(1)?)?;
        Some([a0.x.min(a1.x), a0.y.min(a1.y), a0.x.max(a1.x), a0.y.max(a1.y)])
    });
    let min_len = p.get("minLength").and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(tol * 50.0);
    // By default only ends that come close to a terminal (screw) but miss it are wiring problems;
    // an end out in open space is a drawing detail. `all: true` checks every end.
    let all = p.get("all").and_then(Value::as_bool).unwrap_or(false);
    let reach = p.get("reach").and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(tol * 40.0);
    let net = gather(s, tol)?;
    // Wire layers, when the drawing has them (WIRE, SLC, NAC, CIRCUIT, CABLE in the name), or
    // the `layers` given: only lines on them are checked.
    let wire_layers: Vec<String> = match p.get("layers").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_ascii_uppercase())).collect(),
        None => {
            let st = s.state()?;
            st.doc
                .layers
                .iter()
                .map(|l| l.name.to_ascii_uppercase())
                .filter(|n| ["WIRE", "SLC", "NAC", "CIRCUIT", "CABLE"].iter().any(|k| n.contains(k)))
                .collect()
        }
    };
    let layer_of: HashMap<Handle, String> = if wire_layers.is_empty() {
        HashMap::new()
    } else {
        let st = s.state()?;
        st.doc.space(&Space::Model).map(|m| m.iter().map(|e| (e.handle, e.common.layer.to_ascii_uppercase())).collect()).unwrap_or_default()
    };
    let mut loose = Vec::new();
    'pieces: for (pi, piece) in net.pieces.iter().enumerate() {
        if !wire_layers.is_empty() && !layer_of.get(&piece.handle).is_some_and(|l| wire_layers.contains(l)) {
            continue;
        }
        // Short artwork (glyph strokes, board details, hatching) isn't a wire.
        let len: f64 = piece.pts.windows(2).map(|w| if let [a, b] = w { (a.x - b.x).hypot(a.y - b.y) } else { 0.0 }).sum();
        if len < min_len || piece.border {
            continue;
        }
        for e in net.ends(pi) {
            if window.is_some_and(|w| e.x < w[0] || e.x > w[2] || e.y < w[1] || e.y > w[3]) {
                continue;
            }
            let touches = net.near(e).into_iter().any(|(pj, sj)| pj != pi && net.seg(pj, sj).is_some_and(|(a, b)| seg_dist(a, b, e) <= tol))
                || net.screws.iter().any(|(c, r)| (e.x - c.x).hypot(e.y - c.y) <= r + tol);
            // An end at a label is a leader, an underline or a labelled feed ("TO DAAD #8"): not loose.
            if touches || net.label_near(e, tol * 25.0).is_some() || net.arrow_at(pi, e, tol) {
                continue;
            }
            if !all && net.screw_gap(e) > reach {
                continue;
            }
            loose.push((e, piece.handle, net.label_near(e, tol * 60.0)));
            if loose.len() >= MAX_LOOSE {
                break 'pieces;
            }
        }
    }
    if p.get("mark").and_then(Value::as_bool).unwrap_or(true) {
        let st = s.state()?;
        let old: Vec<Handle> =
            st.doc.space(&Space::Model).map(|m| m.iter().filter(|e| e.common.layer == CHECK_LAYER).map(|e| e.handle).collect()).unwrap_or_default();
        let d = s.doc_mut()?;
        for h in old {
            d.remove_entity(h);
        }
        if d.layer(CHECK_LAYER).is_none() {
            d.layers.push(Layer {
                name: CHECK_LAYER.into(),
                color: Color::True(AMBER),
                description: "Loose wire ends found by WIRECHECK".into(),
                ..Layer::default()
            });
        }
        for (e, _, _) in &loose {
            let common = Common { layer: CHECK_LAYER.into(), ..Common::default() };
            d.add(&Space::Model, common, EntityKind::Circle(Circle { center: Vec3::new(e.x, e.y, 0.0), radius: tol * 6.0 }))?;
        }
    }
    let n = loose.len();
    Ok(json!({
        "count": n,
        "loose": loose.iter().map(|(e, h, l)| json!({ "at": [e.x, e.y], "handle": h.hex(), "label": l })).collect::<Vec<_>>(),
        "message": if n == 0 { "No loose wire ends".to_string() } else { format!("{n} loose wire end(s){} — ringed on layer {CHECK_LAYER}", if n >= MAX_LOOSE { " (first 2000)" } else { "" }) },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::Line;

    fn line(s: &mut Session, a: (f64, f64), b: (f64, f64)) -> Handle {
        s.add_entity(EntityKind::Line(Line { a: Vec3::new(a.0, a.1, 0.0), b: Vec3::new(b.0, b.1, 0.0) })).unwrap_or(Handle(0))
    }

    #[test]
    fn trace_follows_ends_and_tees_not_crossings() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        let a = line(&mut s, (0.0, 0.0), (1.0, 0.0));
        let b = line(&mut s, (1.0, 0.0), (1.0, 1.0)); // end to end
        let c = line(&mut s, (0.5, 0.0), (0.5, -1.0)); // T onto a
        let _x = line(&mut s, (0.25, -0.5), (0.25, 0.5)); // crosses a: not joined
        let _far = line(&mut s, (5.0, 5.0), (6.0, 5.0));
        let r = s.execute("fatrace", &json!({ "point": [0.1, 0.0] })).unwrap_or_default();
        let hs: Vec<String> = r["handles"].as_array().map(|v| v.iter().filter_map(|h| h.as_str().map(String::from)).collect()).unwrap_or_default();
        assert_eq!(hs.len(), 3, "{r}");
        for h in [a, b, c] {
            assert!(hs.contains(&h.hex()), "{r}");
        }
    }

    #[test]
    fn wirecheck_rings_loose_ends_only() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        line(&mut s, (0.0, 0.0), (1.0, 0.0));
        line(&mut s, (1.0, 0.0), (1.0, 1.0));
        // Something far off, so the sheet is bigger than the wires (long border lines are skipped).
        line(&mut s, (20.0, 20.0), (20.5, 20.0));
        // Land the open end at (0,0) on a screw.
        let _ = s.add_entity(EntityKind::Circle(Circle { center: Vec3::new(0.0, 0.0, 0.0), radius: 0.02 }));
        let r = s.execute("wirecheck", &json!({ "all": true })).unwrap_or_default();
        assert_eq!(r["count"], 3, "(1,1) and both ends of the far stub are loose: {r}");
        // By default only an end that comes near a terminal and misses it counts.
        let r = s.execute("wirecheck", &json!({})).unwrap_or_default();
        assert_eq!(r["count"], 0, "no end is near a terminal: {r}");
        let _ = s.add_entity(EntityKind::Circle(Circle { center: Vec3::new(1.0, 1.1, 0.0), radius: 0.02 }));
        let r = s.execute("wirecheck", &json!({})).unwrap_or_default();
        assert_eq!(r["count"], 1, "(1,1) stops short of the screw at (1,1.1): {r}");
    }

    #[test]
    fn arrowhead_end_lands_on_label() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        let _ = s.add_entity(EntityKind::Circle(Circle { center: Vec3::new(0.0, 0.0, 0.0), radius: 0.02 }));
        line(&mut s, (0.0, 0.0), (2.0, 0.0));
        // Outlined arrowhead at (2,0) and the circuit label beyond it.
        line(&mut s, (2.0, 0.0), (1.9, 0.04));
        line(&mut s, (2.0, 0.0), (1.9, -0.04));
        let _ = s.add_entity(EntityKind::Text(cadcraft_doc::Text {
            insert: Vec3::new(2.3, 0.0, 0.0),
            align_pt: None,
            height: 0.1,
            value: "SAMPLE-1".into(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: cadcraft_doc::HAlign::Left,
            valign: cadcraft_doc::VAlign::Baseline,
        }));
        let r = s.execute("fatrace", &json!({ "point": [1.0, 0.0] })).unwrap_or_default();
        let lands: Vec<&str> = r["ends"].as_array().map(|v| v.iter().filter_map(|e| e["lands"].as_str()).collect()).unwrap_or_default();
        assert!(lands.contains(&"screw") && lands.contains(&"label") && !lands.contains(&"loose"), "{r}");
    }
}
