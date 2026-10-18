//! Fire alarm panel builds: read what a drawing says about its Notifier XP6-C cards and HPFF12
//! power supplies, and keep it as editable blocks.
//!
//! FAPANEL looks at an imported panel build (see the io PDF reader):
//! - **XP6-C cards** are found by their own "ENABLE POWER SUPPLY MONITOR" text, which also gives
//!   the sheet scale. The card address is the **first** (leftmost) circuit address label above the
//!   card. The T11–T15 power jumpers are IN when an arc runs from pin 1 to pin 3 with both ends on
//!   the pin row, OUT when no arc is drawn, and NEEDS_REVIEW when only part of an arc is drawn or
//!   the pins can't be found. A card whose labels sit only on +0, +2, +4 is Class A, and Class A
//!   needs T11, T13 and T15 in.
//! - **HPFF12 power supplies** are found by their label. A "SET DIP SWITCHES" callout next to one
//!   (a row of eight switch squares) is read. Strobe power supplies take DIP 5 ON, so a callout
//!   showing DIP 5 off is flagged, and a supply with no callout gets DIP 5 = ON (rule) to set.
//!
//! Every card and supply gets a tag block on layer `FA_READ` (amber; turn it off to hide it):
//! `FA_XP6C` with attributes CARD, ADDRESS, CLASS, T11–T15, LABELS, and `FA_HPFF12` with SUPPLY,
//! DIP, DIP5. Change a value with ATTEDIT or the Properties palette; DATAEXTRACTION lists them all.
//! Running FAPANEL again keeps the tag blocks you already have (your edits win) and flags any
//! value the drawing now reads differently.
//!
//! Nothing is guessed: anything unclear is NEEDS_REVIEW, and labels that disagree (the same
//! number twice on one card, one number on two cards) are flagged, not fixed.

use std::collections::HashMap;
use std::sync::Arc;

use cadcraft_color::{Color, Rgb};
use cadcraft_doc::{Attrib, Block, Common, Entity, EntityKind, HAlign, Layer, Space, Text, VAlign};
use cadcraft_geom::{Vec2, Vec3};
use serde_json::{Map, Value, json};

use super::*;
use crate::{Result, Session};

/// The layer FAPANEL puts its tag blocks on.
pub const FA_LAYER: &str = "FA_READ";
/// Tag block names.
pub const CARD_BLOCK: &str = "FA_XP6C";
pub const SUPPLY_BLOCK: &str = "FA_HPFF12";
const AMBER: Rgb = Rgb(0xFF, 0xB0, 0x00);
/// Font size (PDF points) of the ENABLE text on the reference sheet; card offsets below are in
/// points at that scale, x from the text's left end, y downwards from its baseline.
const ENABLE_FS: f64 = 4.16;
/// Font size (points) of the HPFF12 label on the reference sheet.
const HPFF_FS: f64 = 14.49;
/// TEXT height = font size × this (the PDF reader's cap-height factor).
const CAP: f64 = 0.7;
/// T11–T15 jumper blocks: [x from, x to, pin-row y] from the ENABLE text.
const QJ_BLK: [(&str, f64, f64, f64); 5] =
    [("T11", -45.5, -28.8, -61.5), ("T12", -14.0, 2.6, -78.0), ("T13", 13.6, 30.2, -78.0), ("T14", 41.2, 57.8, -78.0), ("T15", 68.7, 85.3, -78.0)];
/// Where a card's circuit address labels sit (x range, y range) from the ENABLE text.
const ADDR_X: (f64, f64) = (-80.0, 75.0);
const ADDR_Y: (f64, f64) = (-135.0, -98.0);
/// Most cards or supplies read from one drawing.
const MAX_FOUND: usize = 2000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("fapanel", "Read Panel Build", run_fapanel)
            .menu(&["Tools", "Fire Alarm", "Read Panel Build (addresses, jumpers, DIP 5)"])
            .alias(&["xp6read", "faread"])
            .params("{tags?: true (place/keep the FA_XP6C and FA_HPFF12 tag blocks on layer FA_READ)} → {cards: [{card, at, address, labels, class, jumpers: {T11..T15: IN|OUT|NEEDS_REVIEW}}], supplies: [{supply, at, dip, dip5}], flags: [..]}")
            .enabled(has_doc),
    ]
}

// ---------- what the drawing holds ----------

#[derive(Clone, Debug)]
struct Txt {
    value: String,
    at: Vec2,
    height: f64,
}

struct Geo {
    texts: Vec<Txt>,
    /// (centre, diameter), drawing units.
    circles: Vec<(Vec2, f64)>,
    /// Open curves as points, drawing units.
    curves: Vec<Vec<Vec2>>,
    /// Short straight pieces (switch hatching), drawing units.
    short: Vec<(Vec2, Vec2)>,
}

fn collect(s: &Session) -> Result<Geo> {
    let st = s.state()?;
    let mut g = Geo { texts: Vec::new(), circles: Vec::new(), curves: Vec::new(), short: Vec::new() };
    let Some(store) = st.doc.space(&Space::Model) else { return Ok(g) };
    let short = |a: Vec2, b: Vec2| (a.x - b.x).abs() < 0.25 && (a.y - b.y).abs() < 0.25;
    for e in store.iter() {
        if e.common.layer == FA_LAYER {
            continue;
        }
        match &e.kind {
            EntityKind::Text(t) => {
                let v = t.value.trim();
                if !v.is_empty() && t.height.is_finite() && t.height > 0.0 {
                    g.texts.push(Txt { value: v.to_string(), at: t.insert.xy(), height: t.height });
                }
            }
            EntityKind::Circle(c) if c.radius.is_finite() && c.radius > 0.0 => g.circles.push((c.center.xy(), c.radius * 2.0)),
            EntityKind::Arc(a) if a.radius.is_finite() && a.radius > 0.0 => {
                let mut sweep = (a.end - a.start).rem_euclid(std::f64::consts::TAU);
                if sweep <= 1e-9 {
                    sweep = std::f64::consts::TAU;
                }
                g.curves.push(
                    (0..=16)
                        .map(|i| {
                            let t = a.start + sweep * f64::from(i) / 16.0;
                            Vec2::new(a.center.x + a.radius * t.cos(), a.center.y + a.radius * t.sin())
                        })
                        .collect(),
                );
            }
            EntityKind::Line(l) if short(l.a.xy(), l.b.xy()) => g.short.push((l.a.xy(), l.b.xy())),
            EntityKind::LwPolyline(p) => {
                if !p.closed && p.vertices.len() >= 3 && p.vertices.len() <= 64 {
                    g.curves.push(p.vertices.iter().map(|v| v.p).collect());
                }
                for w in p.vertices.windows(2) {
                    if let [a, b] = w
                        && short(a.p, b.p)
                    {
                        g.short.push((a.p, b.p));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(g)
}

/// Something found on the sheet, with its own scale: drawing units per reference point.
#[derive(Clone, Debug)]
struct Found {
    at: Vec2,
    k: f64,
}

impl Found {
    fn local(&self, p: Vec2) -> Vec2 {
        Vec2::new((p.x - self.at.x) / self.k, (self.at.y - p.y) / self.k)
    }
}

fn find_by_text(g: &Geo, matches: impl Fn(&str) -> bool, ref_fs: f64) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    for t in &g.texts {
        if !matches(&t.value.to_ascii_uppercase()) {
            continue;
        }
        let k = t.height / CAP / ref_fs;
        if !(k.is_finite() && k > 0.0) {
            continue;
        }
        if out.iter().any(|c| (c.at.x - t.at.x).abs() < 6.0 * k && (c.at.y - t.at.y).abs() < 6.0 * k) {
            continue;
        }
        out.push(Found { at: t.at, k });
        if out.len() >= MAX_FOUND {
            break;
        }
    }
    // Rows top to bottom, then left to right.
    out.sort_by(|a, b| if (a.at.y - b.at.y).abs() > 40.0 * a.k { b.at.y.total_cmp(&a.at.y) } else { a.at.x.total_cmp(&b.at.x) });
    out
}

// ---------- XP6-C ----------

#[derive(Clone, Debug)]
struct Curve {
    s: Vec2,
    e: Vec2,
    x0: f64,
    x1: f64,
    top: f64,
    bottom: f64,
}

fn read_jumpers(c: &Found, g: &Geo) -> Vec<(&'static str, &'static str)> {
    let near = 200.0 * c.k;
    let close = |p: Vec2| (p.x - c.at.x).abs() < near && (p.y - c.at.y).abs() < near;
    let pins: Vec<Vec2> = g.circles.iter().filter(|(p, d)| close(*p) && (d / c.k - 2.46).abs() < 0.35).map(|(p, _)| c.local(*p)).collect();
    let curves: Vec<Curve> = g
        .curves
        .iter()
        .filter_map(|pts| {
            let first = *pts.first()?;
            let last = *pts.last()?;
            if !close(first) {
                return None;
            }
            let l: Vec<Vec2> = pts.iter().map(|p| c.local(*p)).collect();
            Some(Curve {
                s: c.local(first),
                e: c.local(last),
                x0: l.iter().map(|p| p.x).fold(f64::INFINITY, f64::min),
                x1: l.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max),
                top: l.iter().map(|p| p.y).fold(f64::INFINITY, f64::min),
                bottom: l.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max),
            })
        })
        .collect();
    QJ_BLK
        .iter()
        .map(|&(name, bx0, bx1, py)| {
            let row: Vec<&Vec2> = pins.iter().filter(|p| p.x > bx0 - 1.5 && p.x < bx1 + 1.5 && p.y > py - 16.0 && p.y < py + 10.0).collect();
            if row.len() < 2 {
                return (name, "NEEDS_REVIEW");
            }
            let mut ys: Vec<f64> = row.iter().map(|p| p.y).collect();
            ys.sort_by(f64::total_cmp);
            let pin_y = ys.get(ys.len() / 2).copied().unwrap_or(py) - 1.1;
            let x0 = bx0.min(row.iter().map(|p| p.x).fold(f64::INFINITY, f64::min) - 2.5);
            let x1 = bx1.max(row.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max) + 2.5);
            let (mut main, mut part) = (0, 0);
            for a in &curves {
                if a.x0 < x0 - 1.5 || a.x1 > x1 + 1.5 || a.top < pin_y - 14.0 || a.bottom > pin_y + 1.2 {
                    continue;
                }
                let lo = a.s.y.max(a.e.y);
                if lo - a.top < 2.5 || !(lo > pin_y - 5.0 && lo < pin_y + 0.8) {
                    continue;
                }
                if (a.s.y - a.e.y).abs() < 0.8 && (9.0..=13.5).contains(&(a.e.x - a.s.x).abs()) {
                    main += 1;
                } else {
                    part += 1;
                }
            }
            (
                name,
                if main > 0 {
                    "IN"
                } else if part > 0 {
                    "NEEDS_REVIEW"
                } else {
                    "OUT"
                },
            )
        })
        .collect()
}

/// The circuit address labels above a card, left to right.
fn read_labels(c: &Found, g: &Geo) -> Vec<(f64, String)> {
    let mut v: Vec<(f64, String)> = g
        .texts
        .iter()
        .filter(|t| t.value.len() <= 4 && t.value.chars().all(|ch| ch.is_ascii_digit()))
        .filter_map(|t| {
            let l = c.local(t.at);
            (l.x >= ADDR_X.0 && l.x <= ADDR_X.1 && l.y >= ADDR_Y.0 && l.y <= ADDR_Y.1).then(|| (l.x, t.value.clone()))
        })
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    // The same label read twice at one spot (a text and its searchable copy) counts once.
    v.dedup_by(|a, b| a.1 == b.1 && (a.0 - b.0).abs() < 3.0);
    v
}

// ---------- HPFF12 DIP switches ----------

/// "SET DIP SWITCHES" callouts: rows of eight hatched squares; a square drawn high is ON.
/// Returns (left square centre, [8 × Some(on) | None when the row is flat]), in drawing units.
fn dip_callouts(g: &Geo, k: f64) -> Vec<(Vec2, [Option<bool>; 8])> {
    let f = k;
    let hatch: Vec<&(Vec2, Vec2)> = g
        .short
        .iter()
        .filter(|(a, b)| {
            let (dx, dy) = ((a.x - b.x).abs(), (a.y - b.y).abs());
            dx > 0.15 * f && dy > 0.15 * f && dx < 6.5 * f && dy < 6.5 * f && (dx - dy).abs() < 0.35 * dx.max(dy)
        })
        .collect();
    let cell = 7.0 * f;
    let mut cl: HashMap<(i64, i64), Vec<&(Vec2, Vec2)>> = HashMap::new();
    for s in hatch {
        let c = Vec2::new((s.0.x + s.1.x) / 2.0, (s.0.y + s.1.y) / 2.0);
        cl.entry(((c.x / cell).floor() as i64, (c.y / cell).floor() as i64)).or_default().push(s);
    }
    let mut blocks: Vec<Vec2> = Vec::new();
    for list in cl.values() {
        if list.len() < 6 {
            continue;
        }
        let xs = list.iter().flat_map(|s| [s.0.x, s.1.x]);
        let ys = list.iter().flat_map(|s| [s.0.y, s.1.y]);
        let (x1, x2) = (xs.clone().fold(f64::INFINITY, f64::min), xs.fold(f64::NEG_INFINITY, f64::max));
        let (y1, y2) = (ys.clone().fold(f64::INFINITY, f64::min), ys.fold(f64::NEG_INFINITY, f64::max));
        if x2 - x1 > 3.5 * f && x2 - x1 < 8.0 * f && y2 - y1 > 3.5 * f && y2 - y1 < 8.0 * f {
            let c = Vec2::new((x1 + x2) / 2.0, (y1 + y2) / 2.0);
            if !blocks.iter().any(|m| (m.x - c.x).hypot(m.y - c.y) < 3.0 * f) {
                blocks.push(c);
            }
        }
    }
    blocks.sort_by(|a, b| a.x.total_cmp(&b.x));
    let mut used = vec![false; blocks.len()];
    let mut out = Vec::new();
    for i in 0..blocks.len() {
        if used.get(i).copied().unwrap_or(true) {
            continue;
        }
        let mut row = vec![i];
        let mut cur = i;
        for _ in 1..8 {
            let Some(c) = blocks.get(cur).copied() else { break };
            let next = (0..blocks.len()).find(|&j| {
                !used.get(j).copied().unwrap_or(true)
                    && j != cur
                    && !row.contains(&j)
                    && blocks.get(j).is_some_and(|b| (b.x - (c.x + 9.8 * f)).abs() < 1.6 * f && (b.y - c.y).abs() < 9.0 * f)
            });
            match next {
                Some(j) => {
                    row.push(j);
                    cur = j;
                }
                None => break,
            }
        }
        if row.len() != 8 {
            continue;
        }
        for &j in &row {
            if let Some(u) = used.get_mut(j) {
                *u = true;
            }
        }
        let ys: Vec<f64> = row.iter().filter_map(|j| blocks.get(*j).map(|b| b.y)).collect();
        let (lo, hi) = (ys.iter().copied().fold(f64::INFINITY, f64::min), ys.iter().copied().fold(f64::NEG_INFINITY, f64::max));
        let mid = (lo + hi) / 2.0;
        let mut on = [None; 8];
        for (n, y) in ys.iter().enumerate() {
            if let Some(o) = on.get_mut(n) {
                *o = (hi - lo > 2.0 * f).then_some(*y > mid);
            }
        }
        if let Some(first) = row.first().and_then(|j| blocks.get(*j)) {
            out.push((Vec2::new(first.x, mid), on));
        }
    }
    out
}

// ---------- tag blocks ----------

fn attdef(tag: &str, at: Vec2, height: f64, default: &str, invisible: bool) -> EntityKind {
    EntityKind::AttDef(Attrib {
        tag: tag.into(),
        text: Text {
            insert: Vec3::new(at.x, at.y, 0.0),
            align_pt: None,
            height,
            value: default.into(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        },
        invisible,
        constant: false,
        prompt: tag.into(),
        props: Default::default(),
        multiline: None,
        modes: Default::default(),
    })
}

/// Define a tag block (in reference points, y up) unless the drawing already has one.
fn ensure_block(s: &mut Session, name: &str, defs: Vec<EntityKind>, description: &str) -> Result<()> {
    let d = s.doc_mut()?;
    if d.block(name).is_some() {
        return Ok(());
    }
    let mut b = Block::new(name);
    b.description = description.into();
    for k in defs {
        let h = d.new_handle();
        b.entities.push(Entity { handle: h, common: Common { layer: "0".into(), ..Common::default() }, kind: k });
    }
    d.blocks.insert(name.to_string(), Arc::new(b));
    Ok(())
}

fn card_block_defs() -> Vec<EntityKind> {
    // Positions in points from the ENABLE text (block y up = −card y).
    let mut v = vec![
        attdef("CARD", Vec2::new(-2.0, 26.0), 4.0, "", false),
        attdef("ADDRESS", Vec2::new(-2.0, 18.0), 6.0, "", false),
        attdef("CLASS", Vec2::new(40.0, 18.0), 4.0, "", false),
        attdef("LABELS", Vec2::new(-2.0, 12.0), 3.0, "", true),
    ];
    for (t, _, bx1, py) in QJ_BLK {
        v.push(attdef(t, Vec2::new(bx1 - 0.5, -(py + 12.5)), 3.0, "", false));
    }
    v
}

fn supply_block_defs() -> Vec<EntityKind> {
    vec![
        attdef("SUPPLY", Vec2::new(0.0, -8.0), 6.0, "", false),
        attdef("DIP", Vec2::new(0.0, -16.0), 4.0, "", false),
        attdef("DIP5", Vec2::new(0.0, -23.0), 4.0, "", false),
    ]
}

/// The tag blocks already on the sheet: (block name, insert point, handle, attribute values).
fn existing_tags(s: &Session) -> Result<Vec<(String, Vec2, cadcraft_doc::Handle, Map<String, Value>)>> {
    let st = s.state()?;
    let mut out = Vec::new();
    if let Some(m) = st.doc.space(&Space::Model) {
        for e in m.iter() {
            if let EntityKind::Insert(ins) = &e.kind
                && (ins.block == CARD_BLOCK || ins.block == SUPPLY_BLOCK)
            {
                let vals: Map<String, Value> = ins.attribs.iter().map(|a| (a.tag.clone(), Value::String(a.text.value.clone()))).collect();
                out.push((ins.block.clone(), ins.insert.xy(), e.handle, vals));
            }
        }
    }
    Ok(out)
}

fn place_tag(s: &mut Session, name: &str, at: Vec2, k: f64, vals: &Map<String, Value>) -> Result<()> {
    let h = super::blocks::insert_with(s, name, at, Vec2::new(k, k), 0.0, None, vals)?;
    s.doc_mut()?.modify_entity(h, |e| {
        e.common.layer = FA_LAYER.into();
        e.common.color = Color::ByLayer;
    })?;
    Ok(())
}

// ---------- the command ----------

fn run_fapanel(s: &mut Session, p: &Value) -> Result<Value> {
    let tags = p.get("tags").and_then(Value::as_bool).unwrap_or(true);
    let g = collect(s)?;
    let cards = find_by_text(&g, |v| v.starts_with("ENABLE POWER SUPPLY"), ENABLE_FS);
    let supplies = find_by_text(&g, |v| v == "HPFF12" || v.starts_with("HPFF12 "), HPFF_FS);
    let mut flags: Vec<String> = Vec::new();

    // XP6-C cards.
    let mut card_rows = Vec::new();
    let mut card_vals: Vec<(Vec2, f64, Map<String, Value>)> = Vec::new();
    let mut seen: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in cards.iter().enumerate() {
        let n = i + 1;
        let labels = read_labels(c, &g);
        let address = labels.first().map(|l| l.1.clone());
        let jumpers = read_jumpers(c, &g);
        match &address {
            Some(a) => seen.entry(a.clone()).or_default().push(n),
            None => flags.push(format!("XP6-C #{n}: no address label found above the card — NEEDS_REVIEW")),
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for l in &labels {
            *counts.entry(l.1.as_str()).or_default() += 1;
        }
        let mut twice: Vec<&str> = counts.iter().filter(|(_, n)| **n > 1).map(|(v, _)| *v).collect();
        twice.sort_unstable();
        if !twice.is_empty() {
            flags.push(format!(
                "XP6-C #{n}: label {} drawn twice on this card (labels {}) — address kept as the first label",
                twice.join(", "),
                labels.iter().map(|l| l.1.as_str()).collect::<Vec<_>>().join("/")
            ));
        }
        // Class A: two or more labels, all an even step from the first (+0, +2, +4).
        let base = address.as_deref().and_then(|a| a.parse::<i64>().ok());
        let offsets: Vec<i64> = labels.iter().filter_map(|l| Some(l.1.parse::<i64>().ok()? - base?)).collect();
        let class = if offsets.len() >= 2 && offsets.iter().all(|o| o % 2 == 0) { "A" } else { "B" };
        let jget = |t: &str| jumpers.iter().find(|j| j.0 == t).map(|j| j.1).unwrap_or("NEEDS_REVIEW");
        if class == "A" {
            let missing: Vec<&str> = ["T11", "T13", "T15"].into_iter().filter(|t| jget(t) == "OUT").collect();
            if !missing.is_empty() {
                flags.push(format!(
                    "XP6-C #{n} (addr {}): Class A (labels on +0/+2/+4) needs T11, T13 and T15 in — drawing shows {} out",
                    address.as_deref().unwrap_or("?"),
                    missing.join(", ")
                ));
            }
        }
        let review: Vec<&str> = jumpers.iter().filter(|j| j.1 == "NEEDS_REVIEW").map(|j| j.0).collect();
        if !review.is_empty() {
            flags.push(format!("XP6-C #{n}: {} jumper not clear on the drawing — NEEDS_REVIEW", review.join(", ")));
        }
        let mut vals = Map::new();
        vals.insert("CARD".into(), json!(format!("XP6-C #{n}")));
        vals.insert("ADDRESS".into(), json!(address.clone().unwrap_or_else(|| "NEEDS_REVIEW".into())));
        vals.insert("CLASS".into(), json!(format!("CLASS {class}")));
        vals.insert("LABELS".into(), json!(labels.iter().map(|l| l.1.as_str()).collect::<Vec<_>>().join("/")));
        for (t, v) in &jumpers {
            vals.insert((*t).into(), json!(v));
        }
        card_rows.push(json!({
            "card": n,
            "at": [c.at.x, c.at.y],
            "address": address.unwrap_or_else(|| "NEEDS_REVIEW".into()),
            "labels": labels.iter().map(|l| l.1.clone()).collect::<Vec<_>>(),
            "class": class,
            "jumpers": jumpers.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>(),
        }));
        card_vals.push((c.at, c.k, vals));
    }
    let mut dup: Vec<(&String, &Vec<usize>)> = seen.iter().filter(|(_, v)| v.len() > 1).collect();
    dup.sort();
    for (a, v) in dup {
        flags.push(format!(
            "Address {a} is the first label on {} cards ({}) — NEEDS_REVIEW",
            v.len(),
            v.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ")
        ));
    }

    // HPFF12 supplies and their DIP callouts.
    let mut supply_rows = Vec::new();
    let mut supply_vals: Vec<(Vec2, f64, Map<String, Value>)> = Vec::new();
    let callouts = supplies.first().map(|s0| dip_callouts(&g, s0.k)).unwrap_or_default();
    let mut taken = vec![false; callouts.len()];
    let mut dip5_overruled: Vec<usize> = Vec::new();
    for (i, sp) in supplies.iter().enumerate() {
        let n = i + 1;
        // TB1 to the right of the label = the board is turned 180°, and its callout reads 1→8.
        let tb1 = g.texts.iter().filter(|t| t.value.to_ascii_uppercase().starts_with("TB1")).min_by(|a, b| {
            let da = (a.at.x - sp.at.x).hypot(a.at.y - sp.at.y);
            let db = (b.at.x - sp.at.x).hypot(b.at.y - sp.at.y);
            da.total_cmp(&db)
        });
        let flipped = tb1.is_some_and(|t| (t.at.x - sp.at.x).hypot(t.at.y - sp.at.y) < 220.0 * sp.k && t.at.x > sp.at.x);
        let best = callouts
            .iter()
            .enumerate()
            .filter(|(j, (at, _))| !taken.get(*j).copied().unwrap_or(true) && (at.x - sp.at.x).hypot(at.y - sp.at.y) < 300.0 * sp.k)
            .min_by(|a, b| {
                let da = (a.1.0.x - sp.at.x).hypot(a.1.0.y - sp.at.y);
                let db = (b.1.0.x - sp.at.x).hypot(b.1.0.y - sp.at.y);
                da.total_cmp(&db)
            });
        let (dip, dip5) = match best {
            Some((j, (_, on))) => {
                if let Some(t) = taken.get_mut(j) {
                    *t = true;
                }
                // Switch numbers left to right: 8→1 on an upright board, 1→8 when it's turned.
                let mut by_num = [None; 8];
                for (pos, v) in on.iter().enumerate() {
                    let num = if flipped { pos } else { 7 - pos };
                    if let Some(slot) = by_num.get_mut(num) {
                        *slot = *v;
                    }
                }
                let text: String = by_num
                    .iter()
                    .map(|v| match v {
                        Some(true) => '1',
                        Some(false) => '0',
                        None => '?',
                    })
                    .collect();
                let d5 = by_num.get(4).copied().flatten();
                match d5 {
                    Some(true) => (text, "ON".to_string()),
                    Some(false) => {
                        dip5_overruled.push(n);
                        let fixed: String = text.chars().enumerate().map(|(i, c)| if i == 4 { '1' } else { c }).collect();
                        (fixed, "ON (your rule; drawing shows OFF)".to_string())
                    }
                    None => (text, "NEEDS_REVIEW".to_string()),
                }
            }
            None => ("no callout".to_string(), "ON (rule: strobe supply)".to_string()),
        };
        let mut vals = Map::new();
        vals.insert("SUPPLY".into(), json!(format!("HPFF12 #{n}")));
        vals.insert("DIP".into(), json!(format!("DIP 1-8: {dip}")));
        vals.insert("DIP5".into(), json!(format!("DIP 5: {dip5}")));
        supply_rows.push(json!({ "supply": n, "at": [sp.at.x, sp.at.y], "dip": dip, "dip5": dip5 }));
        supply_vals.push((sp.at, sp.k, vals));
    }

    if !dip5_overruled.is_empty() {
        flags.push(format!(
            "DIP 5 set ON by your rule (strobe power supplies) on HPFF12 {} — the drawing's callout shows it OFF",
            dip5_overruled.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(", ")
        ));
    }

    // Tag blocks: keep the ones already placed (the user's edits win), flag what now reads differently.
    if tags && (!card_vals.is_empty() || !supply_vals.is_empty()) {
        let have = existing_tags(s)?;
        {
            let d = s.doc_mut()?;
            if d.layer(FA_LAYER).is_none() {
                d.layers.push(Layer {
                    name: FA_LAYER.into(),
                    color: Color::True(AMBER),
                    description: "Fire alarm tags read by FAPANEL".into(),
                    ..Layer::default()
                });
            }
        }
        ensure_block(s, CARD_BLOCK, card_block_defs(), "XP6-C card tag: address, class, T11–T15 jumpers")?;
        ensure_block(s, SUPPLY_BLOCK, supply_block_defs(), "HPFF12 power supply tag: DIP switches")?;
        for (block, list) in [(CARD_BLOCK, &card_vals), (SUPPLY_BLOCK, &supply_vals)] {
            for (at, k, vals) in list.iter() {
                let mine = have.iter().find(|(b, p, _, _)| b == block && (p.x - at.x).abs() < 3.0 * k && (p.y - at.y).abs() < 3.0 * k);
                match mine {
                    Some((_, _, _, old)) => {
                        for (tag, v) in vals {
                            if matches!(tag.as_str(), "CARD" | "SUPPLY" | "LABELS") {
                                continue;
                            }
                            let was = old.get(tag).and_then(Value::as_str).unwrap_or("");
                            let now = v.as_str().unwrap_or("");
                            if was != now {
                                let who = old.get("CARD").or_else(|| old.get("SUPPLY")).and_then(Value::as_str).unwrap_or(block);
                                flags.push(format!("{who}: {tag} is \"{was}\" on your tag but the drawing reads \"{now}\" — kept yours"));
                            }
                        }
                    }
                    None => place_tag(s, block, *at, *k, vals)?,
                }
            }
        }
    }

    let message = if cards.is_empty() && supplies.is_empty() {
        "No XP6-C cards or HPFF12 supplies found (cards are found by their ENABLE POWER SUPPLY MONITOR text). Import the panel build PDF first."
            .to_string()
    } else {
        format!(
            "{} XP6-C card(s), {} HPFF12 supply(ies) read; {} flag(s){}",
            cards.len(),
            supplies.len(),
            flags.len(),
            if tags { format!(" — tags on layer {FA_LAYER} (ATTEDIT to change)") } else { String::new() }
        )
    };
    let mut message = message;
    for f in &flags {
        message.push_str("\n  FLAG: ");
        message.push_str(f);
    }
    Ok(json!({ "cards": card_rows, "supplies": supply_rows, "flags": flags, "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Arc as DArc, Circle};

    /// Drawing units per point: panel builds import in sheet inches.
    const UNIT_PT: f64 = 72.0;

    fn text(s: &mut Session, v: &str, x: f64, y: f64, h: f64) {
        let k = EntityKind::Text(Text {
            insert: Vec3::new(x, y, 0.0),
            align_pt: None,
            height: h,
            value: v.into(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        });
        let _ = s.add_entity(k);
    }

    /// A synthetic XP6-C at reference scale: ENABLE text, labels 104/106/108 (Class A), T12 pins
    /// with a pin 1–3 arc (IN), T13 pins with no arc (OUT), no T11/T14/T15 pins (NEEDS_REVIEW).
    fn card(s: &mut Session) {
        let _ = s.execute("new", &json!({}));
        let k = 1.0 / UNIT_PT;
        let (ax, ay) = (10.0, 10.0);
        let w = |x: f64, y: f64| (ax + x * k, ay - y * k);
        text(s, "ENABLE POWER SUPPLY MONITOR", ax, ay, ENABLE_FS * CAP * k);
        for (lx, v) in [(-66.0, "104"), (-7.0, "106"), (51.0, "108")] {
            let (x, y) = w(lx, -117.0);
            text(s, v, x, y, 8.0 * k);
        }
        for (bx, arc) in [(-10.0, true), (17.6, false)] {
            for i in 0..4 {
                let (x, y) = w(bx + f64::from(i) * 5.6, -77.0);
                let _ = s.add_entity(EntityKind::Circle(Circle { center: Vec3::new(x, y, 0.0), radius: 1.23 * k }));
            }
            if arc {
                let (cx, cy) = w(bx + 5.6, -78.0);
                let _ =
                    s.add_entity(EntityKind::Arc(DArc { center: Vec3::new(cx, cy, 0.0), radius: 5.6 * k, start: 0.0, end: std::f64::consts::PI }));
            }
        }
    }

    #[test]
    fn reads_address_jumpers_and_class() {
        let mut s = Session::new();
        card(&mut s);
        let r = s.execute("fapanel", &json!({})).unwrap_or_default();
        let c = &r["cards"][0];
        assert_eq!(c["address"], "104", "{r}");
        assert_eq!(c["class"], "A", "{r}");
        assert_eq!(c["jumpers"]["T12"], "IN", "{r}");
        assert_eq!(c["jumpers"]["T13"], "OUT", "{r}");
        assert_eq!(c["jumpers"]["T11"], "NEEDS_REVIEW", "{r}");
        // Class A with T13 out is flagged, not fixed.
        assert!(r["flags"].as_array().is_some_and(|f| f.iter().any(|x| x.as_str().is_some_and(|t| t.contains("Class A")))), "{r}");
    }

    #[test]
    fn tag_block_placed_once_and_user_edits_win() {
        let mut s = Session::new();
        card(&mut s);
        let _ = s.execute("fapanel", &json!({}));
        let tags = existing_tags(&s).unwrap_or_default();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].3.get("ADDRESS").and_then(Value::as_str), Some("104"));
        // The user changes the address on the tag; a second run keeps it and flags the difference.
        let h = tags[0].2;
        let _ = s.doc_mut().map(|d| {
            d.modify_entity(h, |e| {
                if let EntityKind::Insert(ins) = &mut e.kind
                    && let Some(a) = ins.attribs.iter_mut().find(|a| a.tag == "ADDRESS")
                {
                    a.text.value = "200".into();
                }
            })
        });
        let r = s.execute("fapanel", &json!({})).unwrap_or_default();
        let tags = existing_tags(&s).unwrap_or_default();
        assert_eq!(tags.len(), 1, "no second tag");
        assert_eq!(tags[0].3.get("ADDRESS").and_then(Value::as_str), Some("200"));
        assert!(r["flags"].to_string().contains("kept yours"), "{r}");
    }

    #[test]
    fn empty_drawing_reports_nothing_found() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        let r = s.execute("fapanel", &json!({})).unwrap_or_default();
        assert_eq!(r["cards"].as_array().map(Vec::len), Some(0));
        assert_eq!(r["supplies"].as_array().map(Vec::len), Some(0));
    }
}
