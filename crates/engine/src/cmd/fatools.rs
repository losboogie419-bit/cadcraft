//! Fire alarm drawing upkeep: labels from a point list (FALABELS) and dated revisions (FAREV).
//!
//! FALABELS takes rows of (label on the drawing → new label), from a CSV point list or JSON, and
//! changes every TEXT, MTEXT and attribute whose whole value is the old label. It never adds a
//! label: a row whose old label isn't on the drawing is NOT_FOUND (NEEDS_REVIEW). New labels over
//! 40 characters are refused. `dryRun` reports without changing anything.
//!
//! FAREV compares the drawing with the previous revision's file (COMPARE), colours what was added
//! green and what changed amber, rings the changed areas on layer `FA_REV`, and stamps a revision
//! line (40 characters at most) with the date.

use std::collections::HashMap;

use cadcraft_color::{Color, Rgb};
use cadcraft_doc::{Common, EntityKind, HAlign, Handle, Layer, LwPolyline, Text, VAlign};
use cadcraft_geom::{PolyVertex, Vec2, Vec3};
use serde_json::{Value, json};

use super::*;
use crate::{Result, Session};

pub const REV_LAYER: &str = "FA_REV";
const MAX_LABEL: usize = 40;
const MAX_ROWS: usize = 50_000;
const GREEN: Rgb = Rgb(0x4C, 0xD9, 0x7B);
const AMBER: Rgb = Rgb(0xFF, 0xB0, 0x00);

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("falabels", "Update Labels from Point List", run_falabels)
            .menu(&["Tools", "Fire Alarm", "Update Labels from Point List..."])
            .alias(&["pointlabels"])
            .params("{csv: \"text\" | path: \"list.csv\" | rows: [{from, to}], from?: column name (default `from`), to?: column name (default `to`), dryRun?: false} → {rows: [{from, to, status, count}], updated, needsReview}")
            .enabled(has_doc),
        CommandSpec::new("farev", "Stamp Revision", run_farev)
            .menu(&["Tools", "Fire Alarm", "Stamp Revision..."])
            .params("{rev?: \"B\" (default: the next letter), path?: previous revision's DXF/DWG to compare with, author?, note?, date?: YYYY-MM-DD (default today, local time), at?: [x, y] (default lower right of the drawing)} → {stamp, added, changed, removed, regions}")
            .enabled(has_doc),
    ]
}

// ---------- FALABELS ----------

/// Split CSV text into rows of cells (quotes, doubled quotes, commas or tabs).
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let tab = text.lines().next().is_some_and(|l| l.contains('\t') && !l.contains(','));
    let sep = if tab { '\t' } else { ',' };
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' if quoted && it.peek() == Some(&'"') => {
                cell.push('"');
                it.next();
            }
            '"' => quoted = !quoted,
            c if c == sep && !quoted => row.push(std::mem::take(&mut cell)),
            '\r' if !quoted => {}
            '\n' if !quoted => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
                if rows.len() > MAX_ROWS {
                    break;
                }
            }
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    rows
}

fn rows_param(p: &Value) -> Result<Vec<(String, String)>> {
    const CMD: &str = "falabels";
    if let Some(rows) = p.get("rows").and_then(Value::as_array) {
        return Ok(rows
            .iter()
            .take(MAX_ROWS)
            .filter_map(|r| Some((r.get("from")?.as_str()?.trim().to_string(), r.get("to")?.as_str()?.trim().to_string())))
            .collect());
    }
    let text = if let Some(t) = str_param(p, "csv") {
        t.to_string()
    } else if let Some(path) = str_param(p, "path") {
        #[cfg(not(target_arch = "wasm32"))]
        {
            String::from_utf8_lossy(&std::fs::read(path).map_err(|e| bad(CMD, format!("{path}: {e}")))?).to_string()
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = path;
            return Err(bad(CMD, "paths are not available on the web; pass `csv`"));
        }
    } else {
        return Err(bad(CMD, "`csv`, `path` or `rows` is required"));
    };
    let rows = csv_rows(&text);
    let Some(head) = rows.first() else { return Ok(Vec::new()) };
    let col = |key: &str, default: &str| -> Result<usize> {
        let want = str_param(p, key).unwrap_or(default).trim().to_ascii_lowercase();
        head.iter()
            .position(|h| h.trim().to_ascii_lowercase() == want)
            .ok_or_else(|| bad(CMD, format!("no `{want}` column; the columns are: {}", head.iter().map(|h| h.trim()).collect::<Vec<_>>().join(", "))))
    };
    let (f, t) = (col("from", "from")?, col("to", "to")?);
    Ok(rows
        .iter()
        .skip(1)
        .map(|r| (r.get(f).map(|v| v.trim().to_string()).unwrap_or_default(), r.get(t).map(|v| v.trim().to_string()).unwrap_or_default()))
        .collect())
}

/// Every text value on the sheet: handle, attribute tag (for attributes), value.
fn text_values(s: &Session) -> Result<Vec<(Handle, Option<String>, String)>> {
    let st = s.state()?;
    let mut out = Vec::new();
    if let Some(m) = st.doc.space(&st.space) {
        for e in m.iter() {
            match &e.kind {
                EntityKind::Text(t) => out.push((e.handle, None, t.value.trim().to_string())),
                EntityKind::MText(t) => out.push((e.handle, None, t.contents.trim().to_string())),
                EntityKind::Insert(i) => {
                    for a in &i.attribs {
                        out.push((e.handle, Some(a.tag.clone()), a.text.value.trim().to_string()));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

fn run_falabels(s: &mut Session, p: &Value) -> Result<Value> {
    let rows = rows_param(p)?;
    let dry = p.get("dryRun").and_then(Value::as_bool).unwrap_or(false);
    let values = text_values(s)?;
    let mut index: HashMap<&str, Vec<(Handle, Option<String>)>> = HashMap::new();
    for (h, tag, v) in &values {
        index.entry(v.as_str()).or_default().push((*h, tag.clone()));
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut report = Vec::new();
    let mut edits: Vec<(Handle, Option<String>, String)> = Vec::new();
    let (mut updated, mut review) = (0usize, 0usize);
    for (n, (from, to)) in rows.iter().enumerate() {
        if from.is_empty() && to.is_empty() {
            continue;
        }
        let hits = index.get(from.as_str()).map(Vec::len).unwrap_or(0);
        let status = if from.is_empty() {
            "NEEDS_REVIEW: no old label"
        } else if seen.contains_key(from) {
            "DUPLICATE_ROW"
        } else if to.chars().count() > MAX_LABEL {
            "REFUSED_OVER_40_CHARS"
        } else if hits == 0 {
            "NOT_FOUND (NEEDS_REVIEW)"
        } else if from == to {
            "NO_CHANGE"
        } else if index.contains_key(to.as_str()) {
            "UPDATE (WARN: new label already on the drawing)"
        } else {
            "UPDATE"
        };
        seen.entry(from.clone()).or_insert(n);
        if status.starts_with("UPDATE") {
            if let Some(list) = index.get(from.as_str()) {
                for (h, tag) in list {
                    edits.push((*h, tag.clone(), to.clone()));
                }
            }
            updated += hits;
        }
        if status.contains("NEEDS_REVIEW") || status.starts_with("REFUSED") {
            review += 1;
        }
        report.push(json!({ "row": n + 2, "from": from, "to": to, "status": status, "count": hits }));
    }
    if !dry {
        let d = s.doc_mut()?;
        for (h, tag, to) in edits {
            d.modify_entity(h, |e| match (&mut e.kind, &tag) {
                (EntityKind::Text(t), None) => t.value = to.clone(),
                (EntityKind::MText(t), None) => t.contents = to.clone(),
                (EntityKind::Insert(i), Some(tag)) => {
                    if let Some(a) = i.attribs.iter_mut().find(|a| &a.tag == tag) {
                        a.text.value = to.clone();
                    }
                }
                _ => {}
            })?;
        }
    }
    let mut message = format!("{} label(s) {}; {review} row(s) need review", updated, if dry { "would change (dry run)" } else { "changed" });
    for r in &report {
        let st = r["status"].as_str().unwrap_or("");
        if st != "UPDATE" && st != "NO_CHANGE" {
            message.push_str(&format!("\n  row {}: {} → {}: {st}", r["row"], r["from"].as_str().unwrap_or(""), r["to"].as_str().unwrap_or("")));
        }
    }
    Ok(json!({ "rows": report, "updated": updated, "needsReview": review, "message": message }))
}

// ---------- FAREV ----------

/// Today's date in the computer's local time zone, as YYYY-MM-DD (empty on the web).
fn today() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        jiff::Zoned::now().date().strftime("%Y-%m-%d").to_string()
    }
    #[cfg(target_arch = "wasm32")]
    {
        String::new()
    }
}

fn run_farev(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "farev";
    // No rev given (from the menu): the letter after the last stamp on the sheet, A first.
    let auto_rev = {
        let st = s.state()?;
        let last = st.doc.space(&st.space).map(|m| {
            m.iter()
                .filter(|e| e.common.layer == REV_LAYER)
                .filter_map(|e| if let EntityKind::Text(t) = &e.kind { t.value.strip_prefix("REV ").and_then(|r| r.chars().next()) } else { None })
                .filter(char::is_ascii_uppercase)
                .max()
        });
        match last.flatten() {
            Some(c) if c < 'Z' => char::from(c as u8 + 1).to_string(),
            Some(_) => "Z".to_string(),
            None => "A".to_string(),
        }
    };
    let rev = str_param(p, "rev").map(str::trim).filter(|r| !r.is_empty()).map(str::to_string).unwrap_or(auto_rev);
    let rev = rev.as_str();
    let date = str_param(p, "date").map(str::to_string).unwrap_or_else(today);
    let mut stamp = format!("REV {rev} {date}");
    if let Some(a) = str_param(p, "author").map(str::trim).filter(|a| !a.is_empty()) {
        stamp.push(' ');
        stamp.push_str(a);
    }
    if let Some(n) = str_param(p, "note").map(str::trim).filter(|n| !n.is_empty()) {
        stamp.push_str(": ");
        stamp.push_str(n);
    }
    if stamp.chars().count() > MAX_LABEL {
        return Err(bad(CMD, format!("the stamp \"{stamp}\" is {} characters; keep it to {MAX_LABEL} (shorten the note)", stamp.chars().count())));
    }
    let (mut added, mut changed, mut removed, mut regions) = (0usize, 0usize, 0usize, Vec::new());
    if let Some(path) = str_param(p, "path") {
        let compare = find_command("compare").ok_or_else(|| EngineError::UnknownCommand("compare".into()))?;
        (compare.run)(s, &json!({ "path": path }))?;
        let (a, c, r, g) = match s.compare.as_ref() {
            Some(cmp) => (cmp.added.clone(), cmp.changed.iter().map(|x| x.0).collect::<Vec<_>>(), cmp.removed.len(), cmp.regions.clone()),
            None => (Vec::new(), Vec::new(), 0, Vec::new()),
        };
        added = a.len();
        changed = c.len();
        removed = r;
        let d = s.doc_mut()?;
        for h in a {
            let _ = d.modify_entity(h, |e| e.common.color = Color::True(GREEN));
        }
        for h in c {
            let _ = d.modify_entity(h, |e| e.common.color = Color::True(AMBER));
        }
        regions = g;
    }
    let st = s.state()?;
    let ext = st.doc.extents(&st.space);
    let size = if ext.is_empty() { 1.0 } else { ext.width().max(ext.height()) };
    let at =
        point_param(p, "at").unwrap_or_else(|| if ext.is_empty() { Vec2::ZERO } else { Vec2::new(ext.max.x - size * 0.25, ext.min.y - size * 0.02) });
    let space = st.space.clone();
    let d = s.doc_mut()?;
    if d.layer(REV_LAYER).is_none() {
        d.layers.push(Layer {
            name: REV_LAYER.into(),
            color: Color::True(AMBER),
            description: "Revision stamps and changed areas (FAREV)".into(),
            ..Layer::default()
        });
    }
    let common = || Common { layer: REV_LAYER.into(), ..Common::default() };
    for b in &regions {
        let pad = size * 0.004;
        let v = [
            Vec2::new(b.min.x - pad, b.min.y - pad),
            Vec2::new(b.max.x + pad, b.min.y - pad),
            Vec2::new(b.max.x + pad, b.max.y + pad),
            Vec2::new(b.min.x - pad, b.max.y + pad),
        ];
        d.add(
            &space,
            common(),
            EntityKind::LwPolyline(LwPolyline {
                vertices: v.into_iter().map(PolyVertex::new).collect(),
                closed: true,
                const_width: 0.0,
                elevation: 0.0,
                plinegen: false,
            }),
        )?;
    }
    let height = size * 0.006;
    d.add(
        &space,
        common(),
        EntityKind::Text(Text {
            insert: Vec3::new(at.x, at.y, 0.0),
            align_pt: None,
            height,
            value: stamp.clone(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        }),
    )?;
    Ok(json!({
        "stamp": stamp,
        "added": added, "changed": changed, "removed": removed, "regions": regions.len(),
        "message": format!("{stamp} — {added} added (green), {changed} changed (amber), {removed} removed; {} area(s) ringed on {REV_LAYER}", regions.len()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &mut Session, v: &str, x: f64) {
        let _ = s.add_entity(EntityKind::Text(Text {
            insert: Vec3::new(x, 0.0, 0.0),
            align_pt: None,
            height: 1.0,
            value: v.into(),
            rotation: 0.0,
            width_factor: 1.0,
            oblique: 0.0,
            style: "Standard".into(),
            halign: HAlign::Left,
            valign: VAlign::Baseline,
        }));
    }

    #[test]
    fn labels_update_only_whats_there() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        text(&mut s, "A8-1-04", 0.0);
        text(&mut s, "A8-1-04", 5.0);
        text(&mut s, "N95L6D01", 10.0);
        let csv = "from,to\nA8-1-04,A8-1-09\nMISSING-1,X\nN95L6D01,THIS LABEL IS MUCH LONGER THAN FORTY CHARACTERS\n";
        let r = s.execute("falabels", &json!({ "csv": csv })).unwrap_or_default();
        assert_eq!(r["rows"][0]["status"], "UPDATE", "{r}");
        assert_eq!(r["rows"][0]["count"], 2, "{r}");
        assert_eq!(r["rows"][1]["status"], "NOT_FOUND (NEEDS_REVIEW)", "{r}");
        assert_eq!(r["rows"][2]["status"], "REFUSED_OVER_40_CHARS", "{r}");
        let vals: Vec<String> = text_values(&s).unwrap_or_default().into_iter().map(|v| v.2).collect();
        assert_eq!(vals.iter().filter(|v| *v == "A8-1-09").count(), 2);
        assert!(!vals.iter().any(|v| v == "X"), "never adds a label");
        assert!(vals.iter().any(|v| v == "N95L6D01"), "refused row left alone");
    }

    #[test]
    fn stamp_is_limited_to_40_chars() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        text(&mut s, "X", 0.0);
        assert!(
            s.execute("farev", &json!({ "rev": "B", "date": "2026-10-18", "author": "CL", "note": "this note is far too long for a stamp" }))
                .is_err()
        );
        let r = s.execute("farev", &json!({ "rev": "B", "date": "2026-10-18", "author": "CL" })).unwrap_or_default();
        assert_eq!(r["stamp"], "REV B 2026-10-18 CL");
    }

    #[test]
    fn csv_handles_quotes() {
        let r = csv_rows("from,to\n\"A, B\",\"say \"\"hi\"\"\"\n");
        assert_eq!(r[1], vec!["A, B".to_string(), "say \"hi\"".to_string()]);
    }
}
