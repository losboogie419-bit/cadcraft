//! Fire alarm point list extractor (Tools → Fire Alarm → Point List).
//!
//! The job (layers, block map, room patterns, ignore areas, panel report points and the owner's
//! room choices) lives in a job JSON file, so nothing about any one job is built in:
//! - FAINVENTORY lists every layer, block (insert count, attribute tags) and text layer, to fill
//!   in the job.
//! - FAJOB loads, changes and saves the job.
//! - FAREPORT reads a VeriFire full panel report (.xls/.xlsx) or a DGP run sheet into the job.
//! - FAPOINTS finds the devices: mapped block inserts (DWG/DXF), else address text (PDF), with
//!   addresses split into pieces merged back (`N1L M` + `1` + `01`). Each device gets the room
//!   whose boundary holds it, else the nearest room number (a close second is NEEDS_REVIEW).
//!   Markers go on layer FA-POINTLIST only: green = matched, amber = check, red = problem,
//!   blue = checked by the owner. Nothing is invented: an unreadable address or room stays blank
//!   and is flagged.
//! - FAPOINTROOM / FAPOINTACCEPT record the owner's room choices and acceptances in the job;
//!   they survive reruns.
//! - FAPOINTSEXPORT writes the Excel point list (one tab per node and loop, gap-filled 1–159,
//!   detectors then modules, the full address plus Node/Loop/Type/Number) and the Workbench JSON.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use cadcraft_color::{Color, Rgb};
use cadcraft_doc::{Circle, Common, Drawing, EntityKind, HAlign, Handle, Layer, Space, Text, VAlign};
use cadcraft_geom::{Mat3, Vec2, Vec3};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::*;
use crate::{Result, Session};

pub const POINT_LAYER: &str = "FA-POINTLIST";
pub const MAX_ADDR: u32 = 159;
const MAX_LABEL: usize = 40;
const MAX_ITEMS: usize = 500_000;
const GREEN: Rgb = Rgb(0x3E, 0xCF, 0x6E);
const AMBER: Rgb = Rgb(0xFF, 0xB0, 0x00);
const RED: Rgb = Rgb(0xEF, 0x44, 0x44);
const BLUE: Rgb = Rgb(0x4A, 0xA8, 0xFF);
/// Layers that are never read as drawing content (our own output, page outlines, strokes that a
/// rebuilt label replaced).
const SKIP_LAYERS: &[&str] = &[POINT_LAYER, "FA_READ", "FA_CHECK", "FA_REV", "PDF_PAGES", "PDF_SHX_STROKES"];

// ---------- spreadsheet hooks (installed by the app: the io layer reads and writes Excel) ----------

/// A sheet as text: name and rows.
pub type Grid = (String, Vec<Vec<String>>);

pub struct SheetHooks {
    /// Read every sheet of a workbook or CSV (bytes, file name).
    pub read: fn(&[u8], &str) -> std::result::Result<Vec<Grid>, String>,
    /// Write sheets as .xlsx: `[{name, rows: [[text]], styles: [[\"plain\"|\"header\"|\"spacer\"|\"green\"|\"amber\"|\"red\"]], widths: [n]}]`.
    pub write: fn(&Value) -> std::result::Result<Vec<u8>, String>,
}

static SHEETS: OnceLock<SheetHooks> = OnceLock::new();

pub fn set_sheets(h: SheetHooks) {
    let _ = SHEETS.set(h);
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec::new("fainventory", "Point List: Drawing Inventory", run_inventory)
            .menu(&["Tools", "Fire Alarm", "Point List", "Drawing Inventory"])
            .params("{} → {layers: [{name, count}], blocks: [{name, inserts, tags}], textLayers: [{layer, count, samples}]}")
            .enabled(has_doc),
        CommandSpec::new("fajob", "Point List: Job Setup", run_job)
            .menu(&["Tools", "Fire Alarm", "Point List", "Job Setup..."])
            .params("{path?: job.json to load, set?: {name, node, deviceLayers, roomLayers, addressLayers, boundaryLayer, blockMap: {block: {type, kind: D|M}}, addrAttr, roomPatterns: [{name, re, on}], exclude, ignoreAreas: [[x0,y0,x1,y1]], nearTextDistance, floor}, save?: path, reset?: true} → {job}")
            .enabled(has_doc),
        CommandSpec::new("fareport", "Point List: Load Panel Report / Run Sheet", run_report)
            .menu(&["Tools", "Fire Alarm", "Point List", "Load Panel Report / Run Sheet..."])
            .params("{path: VeriFire full panel report (.xls/.xlsx) or DGP run sheet} → {kind, node, points}")
            .enabled(has_doc),
        CommandSpec::new("fapoints", "Point List: Find Devices and Rooms", run_points)
            .menu(&["Tools", "Fire Alarm", "Point List", "Find Devices and Rooms"])
            .alias(&["pointlist"])
            .params("{mark?: true, onlyProblems?: false} → {devices: [...], rooms, counts: {green, amber, red, checked}, notPlaced}")
            .enabled(has_doc),
        CommandSpec::new("fapointroom", "Point List: Assign Room", run_assign_room)
            .menu(&["Tools", "Fire Alarm", "Point List", "Assign Room to Selected"])
            .params("{room?: \"101\" (default: a room label in the selection), handles?: markers or devices (default: the selection)} → {assigned}")
            .enabled(has_doc),
        CommandSpec::new("fapointaccept", "Point List: Accept", run_accept)
            .menu(&["Tools", "Fire Alarm", "Point List", "Accept Selected"])
            .params("{handles?: (default: the selection), allGreen?: false, page?: n (with allGreen: that page only), undo?: false (un-accept)} → {accepted}")
            .enabled(has_doc),
        CommandSpec::new("fapointgreens", "Point List: Accept All Greens on This Page", run_accept_greens)
            .menu(&["Tools", "Fire Alarm", "Point List", "Accept All Greens on This Page"])
            .params("{page?: n (default: the page under the cursor)} → {accepted}")
            .enabled(has_doc),
        CommandSpec::new("fapointsreview", "Point List: Show Only What Needs Me", run_review)
            .menu(&["Tools", "Fire Alarm", "Point List", "Show Only What Needs Me"])
            .params("{} → FAPOINTS with markers only on amber and red")
            .enabled(has_doc),
        CommandSpec::new("fapointsexport", "Point List: Export", run_export)
            .menu(&["Tools", "Fire Alarm", "Point List", "Export Point List..."])
            .params("{path: points.xlsx | points.csv | *_drawing_points.json, workbench?: also write the Workbench JSON next to it} → {written}")
            .enabled(has_doc),
    ]
}

// ---------- the job ----------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BlockKind {
    #[serde(rename = "type")]
    pub dtype: String,
    /// D, M, or NAC (notification appliances are listed but not on the SLC point sheet).
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RoomPattern {
    pub name: String,
    pub re: String,
    pub on: bool,
}

impl Default for RoomPattern {
    fn default() -> Self {
        RoomPattern { name: String::new(), re: String::new(), on: true }
    }
}

/// A point from the panel report (or run sheet).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct JobPoint {
    pub label: String,
    #[serde(rename = "type")]
    pub dtype: String,
    pub room: String,
    pub zone: String,
    /// report | runsheet
    pub source: String,
}

/// The owner's decision on one device.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Decision {
    pub room_num: String,
    pub room: String,
    pub accepted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Job {
    pub schema_version: u32,
    pub name: String,
    /// Node used when an address on the drawing has none (`L1D23`).
    pub node: Option<u32>,
    pub floor: String,
    pub device_layers: Vec<String>,
    pub room_layers: Vec<String>,
    /// Layers holding address text (empty: any).
    pub address_layers: Vec<String>,
    pub boundary_layer: String,
    pub block_map: BTreeMap<String, BlockKind>,
    pub addr_attr: String,
    pub room_patterns: Vec<RoomPattern>,
    pub exclude: Vec<String>,
    pub ignore_areas: Vec<[f64; 4]>,
    /// How far from a device its address text may be (drawing units; 0 = auto).
    pub near_text_distance: f64,
    /// Points from the panel report / run sheet, by full address (N5L1D3).
    pub points: BTreeMap<String, JobPoint>,
    /// The owner's choices, by device id.
    pub decisions: BTreeMap<String, Decision>,
    /// Anything else in the file (kept as it was).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Job {
    fn default() -> Self {
        let p = |name: &str, re: &str, on: bool| RoomPattern { name: name.into(), re: re.into(), on };
        Job {
            schema_version: 1,
            name: "New job".into(),
            node: None,
            floor: String::new(),
            device_layers: Vec::new(),
            room_layers: Vec::new(),
            address_layers: Vec::new(),
            boundary_layer: String::new(),
            block_map: BTreeMap::new(),
            addr_attr: String::new(),
            // The FA Drawings defaults.
            room_patterns: vec![
                p("1-234, 1A-123", r"^\d{1,2}[A-Z]?-\d{2,4}[A-Z]?$", true),
                p("B-101, B101", r"^[A-Z]{1,2}-?\d{2,4}[A-Z]?$", true),
                p("2.045", r"^\d{1,2}\.\d{3}[A-Z]?$", true),
                p("Dotted, like 1.2.034 or 2A.1.105", r"^[0-9][0-9A-Z]{0,3}(\.[0-9A-Z]{1,4}){2,3}$", true),
                p("101, 1204A (noisy)", r"^\d{3,4}[A-Z]?$", false),
            ],
            exclude: Vec::new(),
            ignore_areas: Vec::new(),
            near_text_distance: 0.0,
            points: BTreeMap::new(),
            decisions: BTreeMap::new(),
            extra: Map::new(),
        }
    }
}

/// Where the job lives in the session: the job and the file it came from.
#[derive(Clone, Debug, Default)]
pub struct FaState {
    pub job: Job,
    pub path: Option<String>,
    /// The last FAPOINTS result: devices with their marker handles.
    pub devices: Vec<Device>,
    pub rooms: Vec<Room>,
    /// Page outlines of the drawing (PDF import), else one page = the extents.
    pub pages: Vec<[f64; 4]>,
    pub drawing_name: String,
}

// ---------- addresses ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Addr {
    pub node: Option<u32>,
    pub loop_: u32,
    pub kind: char,
    pub num: u32,
}

impl Addr {
    pub fn key(&self) -> String {
        match self.node {
            Some(n) => format!("N{n}L{}{}{}", self.loop_, self.kind, self.num),
            None => format!("L{}{}{}", self.loop_, self.kind, self.num),
        }
    }
}

/// Read an address: `N1L1M01`, `N05-L1-D-01`, `N1L M01`, `L1D23` (no node). The whole text must
/// be the address; separators (space, `-`, `_`, `.`) are allowed between the parts.
pub fn parse_addr(text: &str) -> Option<Addr> {
    let s: Vec<char> = text.trim().to_ascii_uppercase().chars().filter(|c| !matches!(c, ' ' | '-' | '_' | '.' | '\t')).collect();
    let mut i = 0;
    let num = |i: &mut usize| -> Option<u32> {
        let st = *i;
        while *i < s.len() && s[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == st || *i - st > 4 {
            return None;
        }
        s[st..*i].iter().collect::<String>().parse().ok()
    };
    let mut node = None;
    if s.first() == Some(&'N') {
        i = 1;
        node = Some(num(&mut i)?);
    }
    if s.get(i) != Some(&'L') {
        return None;
    }
    i += 1;
    let loop_ = num(&mut i)?;
    let kind = *s.get(i).filter(|c| matches!(c, 'D' | 'M'))?;
    i += 1;
    let n = num(&mut i)?;
    if i != s.len() || loop_ == 0 || n == 0 || node == Some(0) {
        return None;
    }
    Some(Addr { node, loop_, kind, num: n })
}

// ---------- reading the drawing ----------

#[derive(Clone, Debug)]
struct TextItem {
    at: Vec2,
    /// Run direction (unit).
    dir: Vec2,
    height: f64,
    value: String,
    layer: String,
    /// Width along `dir` (estimated).
    width: f64,
}

impl TextItem {
    fn center(&self) -> Vec2 {
        let up = Vec2::new(-self.dir.y, self.dir.x);
        self.at + self.dir * (self.width / 2.0) + up * (self.height / 2.0)
    }
}

#[derive(Clone, Debug)]
struct InsertItem {
    handle: Handle,
    block: String,
    layer: String,
    at: Vec2,
    attribs: Vec<(String, String)>,
}

fn mtext_plain(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' | '}' => {}
            '\\' => match it.next() {
                Some('P') | Some('~') => out.push(' '),
                Some('\\') => out.push('\\'),
                Some('{') => out.push('{'),
                Some('}') => out.push('}'),
                Some(code) if "fFHCcWQTApS".contains(code) => {
                    for x in it.by_ref() {
                        if x == ';' {
                            break;
                        }
                    }
                }
                _ => {}
            },
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_item(t: &Text, m: &Mat3, layer: &str) -> Option<TextItem> {
    let value = t.value.trim().to_string();
    if value.is_empty() || !(t.height.is_finite() && t.height > 0.0) {
        return None;
    }
    let dir0 = Vec2::new(t.rotation.cos(), t.rotation.sin());
    let at = m.apply(t.insert.xy());
    let d = m.apply_vec(dir0);
    let scale = d.len();
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let dir = d * (1.0 / scale);
    let height = t.height * scale;
    let width = match t.align_pt {
        Some(a) if matches!(t.halign, HAlign::Fit | HAlign::Aligned) => (m.apply(a.xy()) - at).len(),
        _ => value.chars().count() as f64 * height * 0.8 * t.width_factor.max(0.1),
    };
    // Centred or right-aligned text: its insert point is the alignment point.
    let at = match t.halign {
        HAlign::Center | HAlign::Middle => at - dir * (width / 2.0),
        HAlign::Right => at - dir * width,
        _ => at,
    };
    Some(TextItem { at, dir, height, value, layer: layer.to_string(), width })
}

struct Scan {
    texts: Vec<TextItem>,
    inserts: Vec<InsertItem>,
    /// Closed polylines on the boundary layer.
    boundaries: Vec<Vec<Vec2>>,
    pages: Vec<[f64; 4]>,
}

fn skip(d: &Drawing, layer: &str) -> bool {
    SKIP_LAYERS.iter().any(|l| l.eq_ignore_ascii_case(layer)) || d.layer(layer).is_some_and(|l| l.frozen)
}

fn scan(d: &Drawing, boundary_layer: &str) -> Scan {
    let mut sc = Scan { texts: Vec::new(), inserts: Vec::new(), boundaries: Vec::new(), pages: Vec::new() };
    let Some(store) = d.space(&Space::Model) else { return sc };
    fn walk(
        d: &Drawing,
        sc: &mut Scan,
        ents: &cadcraft_doc::EntityStore,
        m: &Mat3,
        outer: Option<(Handle, &str)>,
        depth: usize,
        boundary_layer: &str,
    ) {
        for e in ents.iter() {
            if sc.texts.len() + sc.inserts.len() > MAX_ITEMS {
                return;
            }
            let layer = outer.map(|o| o.1).filter(|_| e.common.layer == "0").unwrap_or(e.common.layer.as_str());
            if e.common.layer == "PDF_PAGES" && depth == 0 {
                if let EntityKind::LwPolyline(p) = &e.kind {
                    let xs = p.vertices.iter().map(|v| v.p.x);
                    let ys = p.vertices.iter().map(|v| v.p.y);
                    sc.pages.push([
                        xs.clone().fold(f64::INFINITY, f64::min),
                        ys.clone().fold(f64::INFINITY, f64::min),
                        xs.fold(f64::NEG_INFINITY, f64::max),
                        ys.fold(f64::NEG_INFINITY, f64::max),
                    ]);
                }
                continue;
            }
            if skip(d, layer) {
                continue;
            }
            match &e.kind {
                EntityKind::Text(t) => sc.texts.extend(text_item(t, m, layer)),
                EntityKind::MText(t) => {
                    let txt = Text {
                        insert: t.insert,
                        align_pt: None,
                        height: t.height,
                        value: mtext_plain(&t.contents),
                        rotation: t.rotation,
                        width_factor: 1.0,
                        oblique: 0.0,
                        style: t.style.clone(),
                        halign: HAlign::Left,
                        valign: VAlign::Baseline,
                    };
                    sc.texts.extend(text_item(&txt, m, layer));
                }
                EntityKind::LwPolyline(p) if p.closed && !boundary_layer.is_empty() && layer.eq_ignore_ascii_case(boundary_layer) => {
                    sc.boundaries.push(p.vertices.iter().map(|v| m.apply(v.p)).collect());
                }
                EntityKind::Insert(ins) => {
                    let Some(block) = d.block(&ins.block) else { continue };
                    let mi = m.then_before(ins.transform(block.base.xy()));
                    let handle = outer.map(|o| o.0).unwrap_or(e.handle);
                    let attribs: Vec<(String, String)> =
                        ins.attribs.iter().map(|a| (a.tag.to_ascii_uppercase(), a.text.value.trim().to_string())).collect();
                    for a in ins.attribs.iter().filter(|a| !a.invisible) {
                        // Attribute text is placed in world coordinates already (relative to the outer transform).
                        sc.texts.extend(text_item(&a.text, m, layer));
                    }
                    sc.inserts.push(InsertItem { handle, block: ins.block.clone(), layer: layer.to_string(), at: m.apply(ins.insert.xy()), attribs });
                    if depth < cadcraft_doc::MAX_BLOCK_DEPTH && block.xref_path.is_none() {
                        walk(d, sc, &block.entities, &mi, Some((handle, layer)), depth + 1, boundary_layer);
                    }
                }
                _ => {}
            }
        }
    }
    walk(d, &mut sc, store, &Mat3::IDENTITY, None, 0, boundary_layer);
    sc
}

/// Text pieces on one line merged into runs, left to right: (merged text, the pieces' items).
fn runs(texts: &[TextItem]) -> Vec<Vec<usize>> {
    // Bucket by direction, then by the baseline offset across the direction.
    let mut idx: Vec<usize> = (0..texts.len()).collect();
    let key = |t: &TextItem| {
        let a = t.dir.y.atan2(t.dir.x);
        let up = Vec2::new(-t.dir.y, t.dir.x);
        (((a.to_degrees() / 5.0).round() as i64), up.dot(t.at), t.dir.dot(t.at))
    };
    idx.sort_by(|a, b| {
        let (ka, kb) = (key(&texts[*a]), key(&texts[*b]));
        ka.0.cmp(&kb.0).then(ka.1.total_cmp(&kb.1)).then(ka.2.total_cmp(&kb.2))
    });
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut used = vec![false; texts.len()];
    for (n, &i) in idx.iter().enumerate() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let mut run = vec![i];
        let ki = key(&texts[i]);
        let mut last = i;
        // Look ahead at pieces on the same line (same direction bucket, close baseline).
        for &j in idx.iter().skip(n + 1).take(400) {
            if used[j] {
                continue;
            }
            let (a, b) = (&texts[last], &texts[j]);
            let kj = key(b);
            if kj.0 != ki.0 {
                break;
            }
            let h = a.height.max(b.height);
            // A short piece (a loop digit, a number) may sit a little off the line.
            let h0 = h.max(texts[run[0]].height);
            let tol = if b.value.trim().chars().count() <= 3 { h0 * 0.8 } else { h * 0.45 };
            // Measured from the run's first piece, so a run never drifts onto the next line.
            if (kj.1 - ki.1).abs() > tol {
                if kj.1 - ki.1 > h0 * 1.5 {
                    break;
                }
                continue;
            }
            // Split addresses mix sizes (the loop digit is often smaller) and the loop digit sits
            // inside the first piece's gap ("N9L D" + "1" + "29").
            if (a.height - b.height).abs() > h * 0.5 {
                continue;
            }
            let gap = kj.2 - (key(a).2 + a.width);
            if gap > h0 * 1.5 || kj.2 < key(&texts[run[0]]).2 - h0 * 0.2 {
                continue;
            }
            used[j] = true;
            run.push(j);
            last = j;
            if run.len() >= 8 {
                break;
            }
        }
        out.push(run);
    }
    out
}

#[derive(Clone, Debug)]
struct AddrText {
    addr: Addr,
    raw: String,
    at: Vec2,
    height: f64,
}

/// The start of an address that didn't complete (`N9L M` with its numbers missing).
fn is_partial(v: &str) -> bool {
    let s: String = v.to_ascii_uppercase().chars().filter(|c| !c.is_whitespace()).collect();
    let mut c = s.chars();
    c.next() == Some('N') && {
        let rest: String = c.collect();
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with('L')
    }
}

/// Addresses written as text, with split pieces merged (`N1L M` + `1` + `01` → N1L1M01).
fn address_texts(texts: &[TextItem], layers: &[String], partial: &mut Vec<(Vec2, String)>) -> Vec<AddrText> {
    let ok_layer = |l: &str| layers.is_empty() || layers.iter().any(|x| x.eq_ignore_ascii_case(l));
    // Short pieces (a loop digit, a two-digit number) are sometimes read as turned text: lay them
    // along the line by their centre so they join the rest of the address.
    let flat: Vec<TextItem> = texts
        .iter()
        .map(|t| {
            if t.value.trim().chars().count() <= 3 && t.dir.x.abs() < 0.5 {
                let c = t.center();
                let w = t.value.trim().chars().count() as f64 * t.height * 0.8;
                TextItem { at: c - Vec2::new(w / 2.0, t.height / 2.0), dir: Vec2::new(1.0, 0.0), width: w, ..t.clone() }
            } else {
                t.clone()
            }
        })
        .collect();
    let texts = flat.as_slice();
    let mut out = Vec::new();
    for run in runs(texts) {
        let mut i = 0;
        while i < run.len() {
            let mut hit: Option<(usize, Addr, String)> = None;
            // Longest window starting here that reads as an address.
            'win: for j in (i + 1..=run.len().min(i + 6)).rev() {
                let parts: Vec<&str> = run[i..j].iter().map(|k| texts[*k].value.as_str()).collect();
                let mut tries = vec![parts.concat()];
                // The second piece fills the gap in the first: "N9L D" + "1" + "29" → N9L1D29.
                if let [first, second, rest @ ..] = parts.as_slice()
                    && let Some((a, b)) = first.split_once(' ')
                {
                    tries.insert(0, format!("{a}{second}{b}{}", rest.concat()));
                }
                for raw in tries {
                    if let Some(a) = parse_addr(&raw) {
                        hit = Some((j, a, raw));
                        break 'win;
                    }
                }
            }
            match hit {
                Some((j, addr, raw)) => {
                    let first = &texts[run[i]];
                    if ok_layer(&first.layer) {
                        let pts: Vec<Vec2> = run[i..j].iter().map(|k| texts[*k].center()).collect();
                        let c = pts.iter().fold(Vec2::ZERO, |s, p| s + *p) * (1.0 / pts.len().max(1) as f64);
                        out.push(AddrText { addr, raw, at: c, height: first.height });
                    }
                    i = j;
                }
                None => {
                    let t = &texts[run[i]];
                    if is_partial(&t.value) && ok_layer(&t.layer) {
                        partial.push((t.center(), t.value.clone()));
                    }
                    i += 1;
                }
            }
        }
    }
    // The same address twice at one spot (a hidden copy beside the drawn text) is one.
    let mut keep: Vec<AddrText> = Vec::new();
    for a in out {
        if keep.iter().any(|k| k.addr == a.addr && (k.at - a.at).len() < k.height.max(a.height) * 1.5) {
            continue;
        }
        keep.push(a);
    }
    keep
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Room {
    pub num: String,
    pub name: String,
    #[serde(skip)]
    pub at: Vec2,
    pub page: usize,
    /// text | boundary
    pub source: String,
    #[serde(skip)]
    pub outline: Option<Vec<Vec2>>,
}

fn in_poly(p: Vec2, poly: &[Vec2]) -> bool {
    let mut inside = false;
    let n = poly.len();
    for i in 0..n {
        let (a, b) = (poly[i], poly[(i + n - 1) % n]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
    }
    inside
}

fn page_of(pages: &[[f64; 4]], p: Vec2) -> usize {
    pages.iter().position(|f| p.x >= f[0] && p.x <= f[2] && p.y >= f[1] && p.y <= f[3]).map(|i| i + 1).unwrap_or(1)
}

fn in_areas(areas: &[[f64; 4]], p: Vec2) -> bool {
    areas.iter().any(|a| p.x >= a[0].min(a[2]) && p.x <= a[0].max(a[2]) && p.y >= a[1].min(a[3]) && p.y <= a[1].max(a[3]))
}

fn find_rooms(job: &Job, sc: &Scan) -> Vec<Room> {
    let res: Vec<regex::Regex> = job
        .room_patterns
        .iter()
        .filter(|p| p.on && !p.re.is_empty())
        .filter_map(|p| regex::RegexBuilder::new(&p.re).case_insensitive(true).size_limit(1 << 20).build().ok())
        .collect();
    if res.is_empty() {
        return Vec::new();
    }
    let excl: HashSet<String> = job.exclude.iter().map(|e| e.trim().to_ascii_uppercase()).collect();
    let on_layer = |l: &str| job.room_layers.is_empty() || job.room_layers.iter().any(|x| x.eq_ignore_ascii_case(l));
    // Device addresses (and their pieces) are never room numbers.
    let is_num = |t: &str| !is_partial(t) && parse_addr(t).is_none() && res.iter().any(|r| r.is_match(t.trim()));
    let noise = |t: &str| {
        let t = t.trim();
        // Dates (10.10.2026, 4/30/26) and phone numbers (516.496.2322) aren't rooms.
        let parts: Vec<&str> = t.split(['.', '/', '-']).collect();
        let date_or_phone = parts.len() == 3
            && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
            && (parts.last().is_some_and(|p| p.len() == 4) || (parts[0].len() == 3 && parts[1].len() == 3));
        date_or_phone
            || t.contains('\'')
            || t.contains('"')
            || t.contains('=')
            || t.len() == 1
            || (t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()))
            || excl.contains(&t.to_ascii_uppercase())
    };
    let mut out: Vec<Room> = Vec::new();
    for t in &sc.texts {
        if !on_layer(&t.layer) {
            continue;
        }
        let c = t.center();
        if in_areas(&job.ignore_areas, c) {
            continue;
        }
        // The whole text, else each word ("ROOM 101" → 101 when only the number matches).
        let words: Vec<&str> = t.value.split_whitespace().collect();
        let whole = t.value.split_whitespace().collect::<String>();
        let num = if !noise(&whole) && is_num(&whole) {
            Some(whole.to_ascii_uppercase())
        } else {
            words.iter().map(|w| w.trim_matches(|c: char| "()[],;:".contains(c))).find(|w| !noise(w) && is_num(w)).map(|w| w.to_ascii_uppercase())
        };
        let Some(num) = num else { continue };
        // The name: the rest of this text, else a word line just above or below.
        let rest: Vec<&str> = words.iter().copied().filter(|w| !w.eq_ignore_ascii_case(&num) && !is_num(w)).collect();
        let mut name = rest.join(" ");
        if name.chars().count() > 30 {
            name.clear();
        }
        if name.chars().filter(|c| c.is_ascii_alphabetic()).count() < 2 {
            name.clear();
            let mut best = f64::INFINITY;
            for o in &sc.texts {
                let v = o.value.trim();
                if v.len() < 2 || v.len() > 30 || v.chars().filter(|c| c.is_ascii_alphabetic()).count() < 2 || is_num(v) || noise(v) {
                    continue;
                }
                let oc = o.center();
                let up = Vec2::new(-t.dir.y, t.dir.x);
                let along = (oc - c).dot(t.dir).abs();
                let across = (oc - c).dot(up).abs();
                if along > t.width.max(o.width) * 0.9 || across < t.height * 0.5 || across > t.height.max(o.height) * 3.2 {
                    continue;
                }
                if across < best {
                    best = across;
                    name = v.to_string();
                }
            }
        }
        let page = page_of(&sc.pages, c);
        if out.iter().any(|r| r.num == num && r.page == page && (r.at - c).len() < t.height * 6.0) {
            continue;
        }
        out.push(Room { num, name, at: c, page, source: "text".into(), outline: None });
    }
    // Room outlines: each takes the room number(s) inside it.
    for poly in &sc.boundaries {
        let inside: Vec<usize> = (0..out.len()).filter(|i| out[*i].outline.is_none() && in_poly(out[*i].at, poly)).collect();
        if let [i] = inside.as_slice() {
            out[*i].outline = Some(poly.clone());
            out[*i].source = "boundary".into();
        }
    }
    out
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    /// Stable id for the owner's decisions: `H<handle>` for a block, else `A<address>@<page>`
    /// or `X<x>,<y>`.
    pub id: String,
    pub address: String,
    #[serde(skip)]
    pub addr: Option<Addr>,
    pub raw: String,
    pub node: Option<u32>,
    #[serde(rename = "loop")]
    pub loop_: Option<u32>,
    pub kind: String,
    pub number: Option<u32>,
    #[serde(rename = "type")]
    pub dtype: String,
    pub block: String,
    pub label: String,
    pub room_num: String,
    pub room: String,
    pub page: usize,
    pub at: [f64; 2],
    /// green | amber | red | checked
    pub status: String,
    pub flags: Vec<String>,
    pub source: String,
    #[serde(skip)]
    pub source_handle: Option<Handle>,
    #[serde(skip)]
    pub markers: Vec<Handle>,
    /// Text height near the device (marker size); 0 = use the drawing size.
    #[serde(skip)]
    pub size: f64,
}

fn load_state(s: &Session) -> FaState {
    s.fa.clone().unwrap_or_default()
}

fn extract(s: &Session, st: &mut FaState) -> Result<()> {
    let ds = s.state()?;
    let d = &ds.doc;
    let job = &st.job;
    let sc = scan(d, &job.boundary_layer);
    let ext = d.extents(&Space::Model);
    st.pages = if sc.pages.is_empty() && !ext.is_empty() { vec![[ext.min.x, ext.min.y, ext.max.x, ext.max.y]] } else { sc.pages.clone() };
    st.drawing_name = ds.title.clone();
    let rooms = find_rooms(job, &sc);
    let size = if ext.is_empty() { 1.0 } else { ext.width().max(ext.height()) };
    let near = if job.near_text_distance > 0.0 { job.near_text_distance } else { size * 0.01 };
    let mut devices: Vec<Device> = Vec::new();
    let blank = |id: String, at: Vec2, page: usize, source: &str| Device {
        id,
        address: String::new(),
        addr: None,
        raw: String::new(),
        node: None,
        loop_: None,
        kind: String::new(),
        number: None,
        dtype: String::new(),
        block: String::new(),
        label: String::new(),
        room_num: String::new(),
        room: String::new(),
        page,
        at: [at.x, at.y],
        status: String::new(),
        flags: Vec::new(),
        source: source.into(),
        source_handle: None,
        markers: Vec::new(),
        size: 0.0,
    };
    let mut partial: Vec<(Vec2, String)> = Vec::new();
    let addr_texts = address_texts(&sc.texts, &job.address_layers, &mut partial);
    let mut claimed = vec![false; addr_texts.len()];
    // 1. Mapped block inserts.
    let map: HashMap<String, &BlockKind> = job.block_map.iter().map(|(k, v)| (k.to_ascii_uppercase(), v)).collect();
    let on_dev_layer = |l: &str| job.device_layers.is_empty() || job.device_layers.iter().any(|x| x.eq_ignore_ascii_case(l));
    if !map.is_empty() {
        let mut seen: HashSet<Handle> = HashSet::new();
        for ins in &sc.inserts {
            let Some(bk) = map.get(&ins.block.to_ascii_uppercase()) else { continue };
            if !on_dev_layer(&ins.layer) || in_areas(&job.ignore_areas, ins.at) || !seen.insert(ins.handle) {
                continue;
            }
            let page = page_of(&st.pages, ins.at);
            let mut dv = blank(format!("H{}", ins.handle.hex()), ins.at, page, "block");
            dv.block = ins.block.clone();
            dv.dtype = bk.dtype.clone();
            dv.kind = bk.kind.to_ascii_uppercase();
            dv.source_handle = Some(ins.handle);
            // Address: the mapped attribute, else address text nearby.
            let tag = job.addr_attr.trim().to_ascii_uppercase();
            let from_attr = (!tag.is_empty()).then(|| ins.attribs.iter().find(|(t, _)| *t == tag)).flatten().map(|(_, v)| v.clone());
            if let Some(raw) = from_attr.filter(|v| !v.is_empty()) {
                dv.raw = raw.clone();
                dv.addr = parse_addr(&raw);
                if dv.addr.is_none() {
                    dv.flags.push(format!("address \"{raw}\" can't be read (NEEDS_REVIEW)"));
                }
            } else {
                let best = addr_texts
                    .iter()
                    .enumerate()
                    .filter(|(i, a)| !claimed[*i] && (a.at - ins.at).len() <= near)
                    .min_by(|a, b| (a.1.at - ins.at).len().total_cmp(&(b.1.at - ins.at).len()));
                if let Some((i, a)) = best {
                    claimed[i] = true;
                    dv.raw = a.raw.clone();
                    dv.addr = Some(a.addr);
                }
            }
            devices.push(dv);
        }
    }
    // 2. Address text with no mapped block (PDFs, drawings without device blocks).
    for (i, a) in addr_texts.iter().enumerate() {
        if claimed[i] || in_areas(&job.ignore_areas, a.at) {
            continue;
        }
        let page = page_of(&st.pages, a.at);
        let mut dv = blank(format!("A{}@{page}", a.addr.key()), a.at, page, "text");
        dv.raw = a.raw.clone();
        dv.addr = Some(a.addr);
        dv.kind = a.addr.kind.to_string();
        dv.size = a.height;
        devices.push(dv);
    }
    // Address text that starts like an address but can't be completed: red, never guessed.
    for (at, raw) in &partial {
        if in_areas(&job.ignore_areas, *at) || addr_texts.iter().any(|a| (a.at - *at).len() < a.height * 3.0) {
            continue;
        }
        let page = page_of(&st.pages, *at);
        let mut dv = blank(format!("X{:.2},{:.2}", at.x, at.y), *at, page, "text");
        dv.raw = raw.clone();
        dv.flags.push(format!("address text \"{raw}\" is incomplete (NEEDS_REVIEW)"));
        devices.push(dv);
    }
    // Addresses: the job's node when the drawing gives none.
    for dv in &mut devices {
        if let Some(mut a) = dv.addr {
            if a.node.is_none() {
                match job.node {
                    Some(n) => a.node = Some(n),
                    None => dv.flags.push("no node on the drawing; set the job's node (NEEDS_REVIEW)".into()),
                }
            }
            if !dv.kind.is_empty() && dv.kind != "NAC" && dv.kind != a.kind.to_string() {
                dv.flags.push(format!("block is mapped as {} but the address is {}", dv.kind, a.kind));
            }
            dv.kind = a.kind.to_string();
            dv.addr = Some(a);
            dv.address = a.key();
            dv.node = a.node;
            dv.loop_ = Some(a.loop_);
            dv.number = Some(a.num);
            if a.num > MAX_ADDR {
                dv.flags.push(format!("address number {} is over {MAX_ADDR}", a.num));
            }
        }
    }
    // Rooms.
    for dv in &mut devices {
        let p = Vec2::new(dv.at[0], dv.at[1]);
        if let Some(dec) = job.decisions.get(&dv.id).filter(|x| !x.room_num.is_empty() || !x.room.is_empty()) {
            dv.room_num = dec.room_num.clone();
            dv.room = dec.room.clone();
            continue;
        }
        let here: Vec<&Room> = rooms.iter().filter(|r| r.page == dv.page).collect();
        if let Some(r) = here.iter().find(|r| r.outline.as_ref().is_some_and(|o| in_poly(p, o))) {
            dv.room_num = r.num.clone();
            dv.room = r.name.clone();
            continue;
        }
        let mut by: Vec<(f64, &Room)> = here.iter().map(|r| ((r.at - p).len(), *r)).collect();
        by.sort_by(|a, b| a.0.total_cmp(&b.0));
        match by.as_slice() {
            [] => dv.flags.push("no room found (NEEDS_REVIEW)".into()),
            [(_, r)] => {
                dv.room_num = r.num.clone();
                dv.room = r.name.clone();
            }
            [(d1, r), (d2, r2), ..] => {
                dv.room_num = r.num.clone();
                dv.room = r.name.clone();
                if *d2 <= d1 * 1.15 {
                    dv.flags.push(format!("room close call: {} or {} (NEEDS_REVIEW)", r.num, r2.num));
                }
            }
        }
    }
    // Panel report, duplicates, labels, status.
    let mut count: HashMap<String, usize> = HashMap::new();
    for dv in devices.iter().filter(|d| !d.address.is_empty()) {
        *count.entry(dv.address.clone()).or_default() += 1;
    }
    let have_report = !job.points.is_empty();
    for dv in &mut devices {
        let dec = job.decisions.get(&dv.id).cloned().unwrap_or_default();
        if dv.address.is_empty() {
            dv.flags.push("no address (NEEDS_REVIEW)".into());
        } else if count.get(&dv.address).copied().unwrap_or(0) > 1 {
            dv.flags.push(format!("duplicate: {} appears {} times", dv.address, count.get(&dv.address).copied().unwrap_or(0)));
        }
        if let Some(pt) = job.points.get(&dv.address) {
            dv.label = pt.label.clone();
            if dv.dtype.is_empty() {
                dv.dtype = pt.dtype.clone();
            }
            if pt.source == "runsheet" && job.points.values().any(|p| p.source == "report") {
                dv.flags.push("on the run sheet only, not in the panel program".into());
            }
        } else if have_report && !dv.address.is_empty() {
            dv.flags.push("not in the panel report: new (NEEDS_REVIEW)".into());
        }
        if dv.label.chars().count() > MAX_LABEL {
            dv.flags.push(format!("label is {} characters (over {MAX_LABEL})", dv.label.chars().count()));
        }
        let red = dv.flags.iter().any(|f| {
            f.starts_with("no address")
                || f.starts_with("duplicate")
                || f.starts_with("no room")
                || f.contains("over 159")
                || f.contains("can't be read")
                || f.contains("incomplete")
        });
        dv.status = if dec.accepted && !dv.flags.iter().any(|f| f.starts_with("duplicate")) {
            "checked".into()
        } else if red {
            "red".into()
        } else if !dv.flags.is_empty() {
            "amber".into()
        } else {
            "green".into()
        };
        if !dec.room_num.is_empty() || !dec.room.is_empty() {
            dv.flags.push("room set by owner".into());
            if dv.status == "amber" && dv.flags.iter().all(|f| f.starts_with("room")) {
                dv.status = "green".into();
            }
        }
    }
    devices.sort_by(|a, b| {
        (a.node, a.loop_, a.kind.clone(), a.number, a.page)
            .cmp(&(b.node, b.loop_, b.kind.clone(), b.number, b.page))
            .then(a.at[0].total_cmp(&b.at[0]))
    });
    st.devices = devices;
    st.rooms = rooms;
    Ok(())
}

fn status_color(s: &str) -> Rgb {
    match s {
        "green" => GREEN,
        "amber" => AMBER,
        "checked" => BLUE,
        _ => RED,
    }
}

fn mark(s: &mut Session, st: &mut FaState, only_problems: bool) -> Result<()> {
    let ext = s.state()?.doc.extents(&Space::Model);
    let size = if ext.is_empty() { 1.0 } else { ext.width().max(ext.height()) };
    let old: Vec<Handle> = s
        .state()?
        .doc
        .space(&Space::Model)
        .map(|m| m.iter().filter(|e| e.common.layer == POINT_LAYER).map(|e| e.handle).collect())
        .unwrap_or_default();
    let d = s.doc_mut()?;
    for h in old {
        d.remove_entity(h);
    }
    if d.layer(POINT_LAYER).is_none() {
        d.layers.push(Layer {
            name: POINT_LAYER.into(),
            color: Color::True(AMBER),
            description: "Point list markers (FAPOINTS); the drawing's own layers are never changed".into(),
            ..Layer::default()
        });
    }
    for dv in &mut st.devices {
        let r = if dv.size > 0.0 { dv.size * 1.3 } else { size * 0.002 };
        dv.markers.clear();
        if only_problems && (dv.status == "green" || dv.status == "checked") {
            continue;
        }
        let common = || Common { layer: POINT_LAYER.into(), color: Color::True(status_color(&dv.status)), ..Common::default() };
        let c = Vec3::new(dv.at[0], dv.at[1], 0.0);
        dv.markers.push(d.add(&Space::Model, common(), EntityKind::Circle(Circle { center: c, radius: r }))?);
        let tag: String = if dv.address.is_empty() { "?".into() } else { dv.address.clone() };
        let tag = if dv.room_num.is_empty() { tag } else { format!("{tag} {}", dv.room_num) };
        dv.markers.push(d.add(
            &Space::Model,
            common(),
            EntityKind::Text(Text {
                insert: Vec3::new(dv.at[0] + r * 1.2, dv.at[1] - r * 1.2, 0.0),
                align_pt: None,
                height: r * 0.45,
                value: tag.chars().take(MAX_LABEL).collect(),
                rotation: 0.0,
                width_factor: 1.0,
                oblique: 0.0,
                style: "Standard".into(),
                halign: HAlign::Left,
                valign: VAlign::Baseline,
            }),
        )?);
    }
    Ok(())
}

fn counts(st: &FaState) -> Value {
    let n = |k: &str| st.devices.iter().filter(|d| d.status == k).count();
    json!({ "green": n("green"), "amber": n("amber"), "red": n("red"), "checked": n("checked") })
}

/// Report points not found on the drawing.
fn not_placed(st: &FaState) -> Vec<String> {
    let found: HashSet<&str> = st.devices.iter().map(|d| d.address.as_str()).collect();
    st.job.points.keys().filter(|k| !found.contains(k.as_str())).cloned().collect()
}

fn save_job(st: &FaState) -> Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = &st.path {
        let text = serde_json::to_string_pretty(&st.job).map_err(|e| bad("fajob", e.to_string()))?;
        std::fs::write(path, text).map_err(|e| bad("fajob", format!("{path}: {e}")))?;
    }
    let _ = st;
    Ok(())
}

// ---------- commands ----------

fn run_inventory(s: &mut Session, _p: &Value) -> Result<Value> {
    let d = &s.state()?.doc;
    let mut layers: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocks: BTreeMap<String, (usize, HashSet<String>)> = BTreeMap::new();
    let mut texts: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    if let Some(m) = d.space(&Space::Model) {
        for e in m.iter() {
            *layers.entry(e.common.layer.clone()).or_default() += 1;
            match &e.kind {
                EntityKind::Insert(i) => {
                    let b = blocks.entry(i.block.clone()).or_default();
                    b.0 += 1;
                    b.1.extend(i.attribs.iter().map(|a| a.tag.clone()));
                }
                EntityKind::Text(t) => {
                    let x = texts.entry(e.common.layer.clone()).or_default();
                    x.0 += 1;
                    if x.1.len() < 4 {
                        x.1.push(t.value.chars().take(40).collect());
                    }
                }
                EntityKind::MText(t) => {
                    let x = texts.entry(e.common.layer.clone()).or_default();
                    x.0 += 1;
                    if x.1.len() < 4 {
                        x.1.push(mtext_plain(&t.contents).chars().take(40).collect());
                    }
                }
                _ => {}
            }
        }
    }
    // Attribute definitions in the blocks too (tags an insert may leave empty).
    for (name, b) in blocks.iter_mut() {
        if let Some(def) = d.block(name) {
            for e in def.entities.iter() {
                if let EntityKind::AttDef(a) = &e.kind {
                    b.1.insert(a.tag.clone());
                }
            }
        }
    }
    let mut bl: Vec<Value> = blocks
        .into_iter()
        .map(|(name, (n, tags))| {
            let mut tags: Vec<String> = tags.into_iter().collect();
            tags.sort();
            json!({ "name": name, "inserts": n, "tags": tags })
        })
        .collect();
    bl.sort_by_key(|b| std::cmp::Reverse(b["inserts"].as_u64().unwrap_or(0)));
    let ly: Vec<Value> = layers.iter().map(|(k, v)| json!({ "name": k, "count": v })).collect();
    let tx: Vec<Value> = texts.iter().map(|(k, (n, s))| json!({ "layer": k, "count": n, "samples": s })).collect();
    let mut message = format!("{} layer(s), {} block(s) inserted, {} text layer(s)", ly.len(), bl.len(), tx.len());
    for b in bl.iter().take(15) {
        message.push_str(&format!("\n  block {} × {}{}", b["name"].as_str().unwrap_or(""), b["inserts"], {
            let t: Vec<&str> = b["tags"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if t.is_empty() { String::new() } else { format!(" [{}]", t.join(", ")) }
        }));
    }
    Ok(json!({ "layers": ly, "blocks": bl, "textLayers": tx, "message": message }))
}

fn run_job(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fajob";
    let mut st = load_state(s);
    if p.get("reset").and_then(Value::as_bool).unwrap_or(false) {
        st = FaState::default();
    }
    if let Some(path) = str_param(p, "path") {
        #[cfg(not(target_arch = "wasm32"))]
        {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    st.job = serde_json::from_str(&text).map_err(|e| bad(CMD, format!("{path}: not a job file ({e})")))?;
                }
                // A new job file: start from the defaults and save there.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(bad(CMD, format!("{path}: {e}"))),
            }
            st.path = Some(path.to_string());
        }
        #[cfg(target_arch = "wasm32")]
        return Err(bad(CMD, "job files need the desktop app"));
    }
    if let Some(set) = p.get("set").and_then(Value::as_object) {
        let mut cur = serde_json::to_value(&st.job).map_err(|e| bad(CMD, e.to_string()))?;
        if let Some(o) = cur.as_object_mut() {
            for (k, v) in set {
                o.insert(k.clone(), v.clone());
            }
        }
        st.job = serde_json::from_value(cur).map_err(|e| bad(CMD, format!("bad job setting: {e}")))?;
        for pat in &st.job.room_patterns {
            if pat.on && regex::Regex::new(&pat.re).is_err() {
                return Err(bad(CMD, format!("room pattern \"{}\" has a mistake: {}", pat.name, pat.re)));
            }
        }
    }
    if let Some(path) = str_param(p, "save") {
        st.path = Some(path.to_string());
    }
    save_job(&st)?;
    let job = serde_json::to_value(&st.job).map_err(|e| bad(CMD, e.to_string()))?;
    let message = format!(
        "Job \"{}\"{}: {} mapped block(s), {} room pattern(s) on, {} panel point(s), {} decision(s)",
        st.job.name,
        st.path.as_deref().map(|p| format!(" ({p})")).unwrap_or_default(),
        st.job.block_map.len(),
        st.job.room_patterns.iter().filter(|p| p.on).count(),
        st.job.points.len(),
        st.job.decisions.len()
    );
    s.fa = Some(st);
    Ok(json!({ "job": job, "message": message }))
}

fn cell(row: &[String], i: Option<usize>) -> &str {
    i.and_then(|i| row.get(i)).map(|s| s.trim()).unwrap_or("")
}

fn header(rows: &[Vec<String>], must: &[&str], scan: usize) -> Option<(usize, HashMap<String, usize>)> {
    for (i, r) in rows.iter().enumerate().take(scan) {
        let low: Vec<String> = r.iter().map(|c| c.trim().to_ascii_lowercase()).collect();
        if must.iter().all(|m| low.iter().any(|c| c == m)) {
            let mut map = HashMap::new();
            for (j, c) in low.into_iter().enumerate() {
                map.entry(c).or_insert(j);
            }
            return Some((i, map));
        }
    }
    None
}

fn read_path(cmd: &str, path: &str) -> Result<Vec<u8>> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::read(path).map_err(|e| bad(cmd, format!("{path}: {e}")))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        Err(bad(cmd, "reading files needs the desktop app"))
    }
}

fn run_report(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fareport";
    let path = str_param(p, "path").ok_or_else(|| bad(CMD, "`path` is required (a VeriFire full panel report or a DGP run sheet)"))?;
    let hooks = SHEETS.get().ok_or_else(|| bad(CMD, "spreadsheets are not available in this build"))?;
    let bytes = read_path(CMD, path)?;
    let grids = (hooks.read)(&bytes, path).map_err(|e| bad(CMD, e))?;
    let mut st = load_state(s);
    // VeriFire full panel report: node from "NodeDetails: (Address: 5, …)".
    let mut node: Option<u32> = None;
    for (_, rows) in &grids {
        for r in rows.iter().take(5) {
            let t = r.first().map(String::as_str).unwrap_or("");
            if let Some(i) = t.find("Address:").filter(|_| t.contains("NodeDetails")) {
                node = t[i + 8..].trim_start().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok();
            }
        }
        if node.is_some() {
            break;
        }
    }
    let mut added = 0usize;
    let mut bad_rows: Vec<String> = Vec::new();
    let mut kind_found = "";
    for (name, rows) in &grids {
        let lname = name.to_ascii_lowercase();
        let report_kind = if lname.contains("detector") {
            Some('D')
        } else if lname.contains("module") {
            Some('M')
        } else {
            None
        };
        if let (Some(k), Some((h, c))) = (report_kind, header(rows, &["loop", "device addr"], 12)) {
            let Some(n) = node.or(st.job.node) else {
                return Err(bad(CMD, "the report doesn't say its node (NodeDetails: Address); set the job's node first"));
            };
            kind_found = "panel report";
            for r in rows.iter().skip(h + 1) {
                let (lp, ad) = (cell(r, c.get("loop").copied()), cell(r, c.get("device addr").copied()));
                let (Ok(lp), Ok(ad)) = (lp.parse::<u32>(), ad.parse::<u32>()) else { continue };
                let a = Addr { node: Some(n), loop_: lp, kind: k, num: ad };
                st.job.points.insert(
                    a.key(),
                    JobPoint {
                        label: cell(r, c.get("custom label").copied()).to_string(),
                        dtype: cell(r, c.get("type code label").copied()).to_string(),
                        room: String::new(),
                        zone: String::new(),
                        source: "report".into(),
                    },
                );
                added += 1;
            }
            continue;
        }
        // DGP run sheet: "DEVICE ADDRESS" column, addresses like N05-L1-D-01.
        if let Some((h, c)) = header(rows, &["device address"], 25) {
            kind_found = if kind_found.is_empty() { "run sheet" } else { kind_found };
            for (ri, r) in rows.iter().enumerate().skip(h + 1) {
                let raw = cell(r, c.get("device address").copied());
                if raw.is_empty() {
                    continue;
                }
                let Some(mut a) = parse_addr(raw) else {
                    bad_rows.push(format!("{name} row {}: \"{raw}\"", ri + 1));
                    continue;
                };
                if a.node.is_none() {
                    a.node = st.job.node;
                }
                let rest = [cell(r, c.get("device type").copied()), cell(r, c.get("function").copied()), cell(r, c.get("room #").copied())];
                if rest.iter().all(|x| x.is_empty()) {
                    continue;
                }
                if a.num > MAX_ADDR {
                    bad_rows.push(format!("{name} row {}: {} is over {MAX_ADDR}", ri + 1, a.key()));
                    continue;
                }
                // The panel program is the truth: a run sheet only fills what it doesn't have.
                let e = st.job.points.entry(a.key()).or_insert_with(|| JobPoint { source: "runsheet".into(), ..JobPoint::default() });
                if e.dtype.is_empty() {
                    e.dtype = rest[0].to_string();
                }
                if e.room.is_empty() {
                    e.room = rest[2].to_string();
                }
                added += 1;
            }
        }
    }
    if kind_found.is_empty() {
        return Err(bad(CMD, "no panel report sheets (All Detectors / All Modules with LOOP and Device ADDR) or run sheet (DEVICE ADDRESS) found"));
    }
    save_job(&st)?;
    let total = st.job.points.len();
    s.fa = Some(st);
    let mut message =
        format!("Read {added} point(s) from the {kind_found}{}; {total} in the job", node.map(|n| format!(" (node {n})")).unwrap_or_default());
    for b in bad_rows.iter().take(20) {
        message.push_str(&format!("\n  NEEDS_REVIEW {b}"));
    }
    Ok(json!({ "kind": kind_found, "node": node, "points": added, "total": total, "badRows": bad_rows, "message": message }))
}

fn run_points(s: &mut Session, p: &Value) -> Result<Value> {
    let mut st = load_state(s);
    extract(s, &mut st)?;
    if p.get("mark").and_then(Value::as_bool).unwrap_or(true) {
        mark(s, &mut st, p.get("onlyProblems").and_then(Value::as_bool).unwrap_or(false))?;
    }
    let c = counts(&st);
    let np = not_placed(&st);
    let mut message = format!(
        "{} device(s): {} green, {} amber, {} red, {} checked; {} room(s){}",
        st.devices.len(),
        c["green"],
        c["amber"],
        c["red"],
        c["checked"],
        st.rooms.len(),
        if st.job.points.is_empty() { String::new() } else { format!("; {} panel point(s) not found on the drawing", np.len()) }
    );
    for dv in st.devices.iter().filter(|d| d.status == "red" || d.status == "amber").take(25) {
        message.push_str(&format!(
            "\n  {} {} p{}: {}",
            dv.status.to_uppercase(),
            if dv.address.is_empty() { "?" } else { &dv.address },
            dv.page,
            dv.flags.join("; ")
        ));
    }
    let devices = serde_json::to_value(&st.devices).unwrap_or_default();
    let rooms = serde_json::to_value(&st.rooms).unwrap_or_default();
    s.fa = Some(st);
    Ok(json!({ "devices": devices, "rooms": rooms, "counts": c, "notPlaced": np, "message": message }))
}

/// The devices picked by `handles` (markers or the devices' own blocks), else the selection.
fn picked(s: &Session, st: &FaState, p: &Value) -> Vec<usize> {
    let hs: Vec<Handle> = match p.get("handles").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(|v| v.as_str().and_then(|h| u64::from_str_radix(h, 16).ok()).map(Handle)).collect(),
        None => s.selection(),
    };
    let set: HashSet<Handle> = hs.into_iter().collect();
    (0..st.devices.len())
        .filter(|i| st.devices[*i].markers.iter().any(|m| set.contains(m)) || st.devices[*i].source_handle.is_some_and(|h| set.contains(&h)))
        .collect()
}

fn run_assign_room(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fapointroom";
    let mut st = load_state(s);
    if st.devices.is_empty() {
        return Err(bad(CMD, "run FAPOINTS (Find Devices and Rooms) first"));
    }
    // The room: given, else a room label picked with the markers (box-select the devices, then
    // add the room's number text to the selection).
    let from_sel: Option<String> = {
        let ds = s.state()?;
        s.selection().iter().filter_map(|h| ds.doc.entity(*h)).find_map(|e| match &e.kind {
            EntityKind::Text(t) if e.common.layer != POINT_LAYER => Some(t.value.trim().to_string()),
            EntityKind::MText(t) if e.common.layer != POINT_LAYER => Some(mtext_plain(&t.contents)),
            _ => None,
        })
    };
    let text_owned = str_param(p, "room").or_else(|| str_param(p, "roomNum")).map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).or(from_sel);
    let text_owned = text_owned.ok_or_else(|| bad(CMD, "select the device markers and the room's number text, or give `room` (e.g. 101)"))?;
    let text = text_owned.as_str();
    // A label like "ROOM 101" picks the room number found on the drawing inside it.
    let text = st
        .rooms
        .iter()
        .find(|r| text.split_whitespace().any(|w| w.eq_ignore_ascii_case(&r.num)))
        .map(|r| r.num.clone())
        .unwrap_or_else(|| text.to_string());
    let text = text.as_str();
    // A known room number picks up its name; else the text is the owner's own room.
    let (num, name) = match st.rooms.iter().find(|r| r.num.eq_ignore_ascii_case(text)) {
        Some(r) => (r.num.clone(), r.name.clone()),
        None => {
            let mut w = text.split_whitespace();
            let first = w.next().unwrap_or("").to_string();
            if first.chars().any(|c| c.is_ascii_digit()) { (first, w.collect::<Vec<_>>().join(" ")) } else { (String::new(), text.to_string()) }
        }
    };
    let idx = picked(s, &st, p);
    if idx.is_empty() {
        return Err(bad(CMD, "select the devices' markers (layer FA-POINTLIST) first"));
    }
    for i in &idx {
        let id = st.devices[*i].id.clone();
        let d = st.job.decisions.entry(id).or_default();
        d.room_num = num.clone();
        d.room = name.clone();
        d.accepted = true;
    }
    save_job(&st)?;
    s.fa = Some(st);
    let r = run_points(s, &json!({}))?;
    Ok(
        json!({ "assigned": idx.len(), "counts": r["counts"], "message": format!("{} device(s) set to room {} {} and checked", idx.len(), num, name) }),
    )
}

fn run_accept_greens(s: &mut Session, p: &Value) -> Result<Value> {
    let st = load_state(s);
    let page = p.get("page").and_then(Value::as_u64).map(|v| v as usize).unwrap_or_else(|| page_of(&st.pages, s.cursor));
    run_accept(s, &json!({ "allGreen": true, "page": page }))
}

fn run_review(s: &mut Session, _p: &Value) -> Result<Value> {
    run_points(s, &json!({ "onlyProblems": true }))
}

fn run_accept(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fapointaccept";
    let mut st = load_state(s);
    if st.devices.is_empty() {
        return Err(bad(CMD, "run FAPOINTS (Find Devices and Rooms) first"));
    }
    let undo = p.get("undo").and_then(Value::as_bool).unwrap_or(false);
    let idx: Vec<usize> = if p.get("allGreen").and_then(Value::as_bool).unwrap_or(false) {
        let page = p.get("page").and_then(Value::as_u64).map(|v| v as usize);
        (0..st.devices.len()).filter(|i| st.devices[*i].status == "green" && page.is_none_or(|pg| st.devices[*i].page == pg)).collect()
    } else {
        picked(s, &st, p)
    };
    if idx.is_empty() {
        return Err(bad(CMD, "nothing to accept: select markers, or use allGreen"));
    }
    for i in &idx {
        let id = st.devices[*i].id.clone();
        st.job.decisions.entry(id).or_default().accepted = !undo;
    }
    save_job(&st)?;
    s.fa = Some(st);
    let r = run_points(s, &json!({}))?;
    Ok(
        json!({ "accepted": idx.len(), "counts": r["counts"], "message": format!("{} device(s) {}", idx.len(), if undo { "un-accepted" } else { "accepted (checked)" }) }),
    )
}

// ---------- export ----------

const HEAD: [&str; 15] =
    ["Address", "Node", "Loop", "D/M", "Number", "Label", "Type", "Room #", "Room", "Floor", "Drawing", "Page", "Zone", "Note", "Status"];

fn device_row(st: &FaState, dv: &Device) -> Vec<String> {
    let pt = st.job.points.get(&dv.address);
    vec![
        dv.address.clone(),
        dv.node.map(|n| n.to_string()).unwrap_or_default(),
        dv.loop_.map(|n| n.to_string()).unwrap_or_default(),
        dv.kind.clone(),
        dv.number.map(|n| n.to_string()).unwrap_or_default(),
        dv.label.clone(),
        dv.dtype.clone(),
        dv.room_num.clone(),
        dv.room.clone(),
        st.job.floor.clone(),
        st.drawing_name.clone(),
        dv.page.to_string(),
        pt.map(|p| p.zone.clone()).unwrap_or_default(),
        dv.flags.join("; "),
        dv.status.to_uppercase(),
    ]
}

fn style_of(status: &str) -> &'static str {
    match status {
        "green" | "checked" => "green",
        "amber" => "amber",
        _ => "red",
    }
}

fn sheet(name: &str, head: &[&str], rows: Vec<(Vec<String>, Vec<&str>)>, widths: &[f64]) -> Value {
    let mut r: Vec<Vec<String>> = vec![head.iter().map(|h| h.to_string()).collect()];
    let mut st: Vec<Vec<&str>> = vec![vec!["header"; head.len()]];
    for (row, sty) in rows {
        r.push(row);
        st.push(sty);
    }
    json!({ "name": name, "rows": r, "styles": st, "widths": widths })
}

fn workbook(st: &FaState) -> Value {
    let widths = [12.0, 6.0, 6.0, 5.0, 8.0, 42.0, 18.0, 12.0, 24.0, 8.0, 28.0, 6.0, 8.0, 40.0, 10.0];
    let mut sheets: Vec<Value> = Vec::new();
    let mut summary: Vec<(Vec<String>, Vec<&str>)> = Vec::new();
    // Node + loop tabs, gap-filled 1–159, detectors then modules.
    let mut loops: BTreeMap<(u32, u32), ()> = BTreeMap::new();
    for dv in &st.devices {
        if let (Some(n), Some(l)) = (dv.node, dv.loop_) {
            loops.insert((n, l), ());
        }
    }
    for k in st.job.points.keys() {
        if let Some(Addr { node: Some(n), loop_, .. }) = parse_addr(k) {
            loops.insert((n, loop_), ());
        }
    }
    for (n, l) in loops.keys() {
        let mut rows: Vec<(Vec<String>, Vec<&str>)> = Vec::new();
        for kind in ['D', 'M'] {
            let mut cnt = [0usize; 5];
            for a in 1..=MAX_ADDR {
                let key = Addr { node: Some(*n), loop_: *l, kind, num: a }.key();
                let here: Vec<&Device> = st.devices.iter().filter(|d| d.address == key).collect();
                if here.is_empty() {
                    match st.job.points.get(&key) {
                        // In the panel report but not found on the drawing.
                        Some(pt) => {
                            let mut row =
                                vec![key.clone(), n.to_string(), l.to_string(), kind.to_string(), a.to_string(), pt.label.clone(), pt.dtype.clone()];
                            row.extend([
                                String::new(),
                                pt.room.clone(),
                                st.job.floor.clone(),
                                String::new(),
                                String::new(),
                                pt.zone.clone(),
                                "in the panel report, not found on the drawing".into(),
                                "NOT PLACED".into(),
                            ]);
                            let mut sty = vec!["plain"; HEAD.len()];
                            sty[14] = "amber";
                            if pt.label.chars().count() > MAX_LABEL {
                                sty[5] = "red";
                            }
                            rows.push((row, sty));
                            cnt[4] += 1;
                        }
                        None => {
                            let mut row = vec![key.clone()];
                            row.resize(HEAD.len(), String::new());
                            let mut sty = vec!["plain"; HEAD.len()];
                            sty[0] = "spacer";
                            rows.push((row, sty));
                        }
                    }
                    continue;
                }
                for dv in here {
                    let mut sty = vec!["plain"; HEAD.len()];
                    sty[14] = style_of(&dv.status);
                    if dv.label.chars().count() > MAX_LABEL {
                        sty[5] = "red";
                    }
                    if dv.flags.iter().any(|f| f.starts_with("duplicate")) {
                        sty[0] = "red";
                    }
                    rows.push((device_row(st, dv), sty));
                    match dv.status.as_str() {
                        "green" => cnt[0] += 1,
                        "amber" => cnt[1] += 1,
                        "red" => cnt[2] += 1,
                        _ => cnt[3] += 1,
                    }
                }
            }
            summary.push((
                vec![
                    format!("N{n}"),
                    l.to_string(),
                    if kind == 'D' { "Detectors".into() } else { "Modules".into() },
                    (cnt[0] + cnt[1] + cnt[2] + cnt[3]).to_string(),
                    cnt[0].to_string(),
                    cnt[1].to_string(),
                    cnt[2].to_string(),
                    cnt[3].to_string(),
                    cnt[4].to_string(),
                ],
                vec!["plain"; 9],
            ));
        }
        sheets.push(sheet(&format!("N{n} L{l}"), &HEAD, rows, &widths));
    }
    let mut all = vec![sheet(
        "Summary",
        &["Node", "Loop", "Kind", "Found", "Green", "Amber", "Red", "Checked", "Not placed"],
        summary,
        &[8.0, 6.0, 11.0, 8.0, 8.0, 8.0, 8.0, 9.0, 11.0],
    )];
    all.extend(sheets);
    // Every device found (with or without an address).
    let devs: Vec<(Vec<String>, Vec<&str>)> = st
        .devices
        .iter()
        .map(|dv| {
            let mut sty = vec!["plain"; HEAD.len()];
            sty[14] = style_of(&dv.status);
            (device_row(st, dv), sty)
        })
        .collect();
    all.push(sheet("Devices", &HEAD, devs, &widths));
    // Count by room × type.
    let mut cnt: BTreeMap<(String, String), usize> = BTreeMap::new();
    for dv in &st.devices {
        let room = [dv.room_num.as_str(), dv.room.as_str()].iter().filter(|x| !x.is_empty()).copied().collect::<Vec<_>>().join(" ");
        let t = if dv.dtype.is_empty() {
            if dv.kind == "D" {
                "Detector".to_string()
            } else if dv.kind == "M" {
                "Module".to_string()
            } else {
                "Device".to_string()
            }
        } else {
            dv.dtype.clone()
        };
        *cnt.entry((if room.is_empty() { "(no room)".into() } else { room }, t)).or_default() += 1;
    }
    let crows = cnt.into_iter().map(|((r, t), n)| (vec![r, t, n.to_string()], vec!["plain"; 3])).collect();
    all.push(sheet("Device count", &["Room", "Type", "Count"], crows, &[30.0, 22.0, 8.0]));
    // Unresolved: everything not green or checked, and report points not on the drawing.
    let mut un: Vec<(Vec<String>, Vec<&str>)> = st
        .devices
        .iter()
        .filter(|d| d.status != "green" && d.status != "checked")
        .map(|dv| {
            let mut sty = vec!["plain"; HEAD.len()];
            sty[14] = style_of(&dv.status);
            (device_row(st, dv), sty)
        })
        .collect();
    for k in not_placed(st) {
        let pt = st.job.points.get(&k).cloned().unwrap_or_default();
        let a = parse_addr(&k);
        let mut row = vec![
            k.clone(),
            a.and_then(|a| a.node).map(|n| n.to_string()).unwrap_or_default(),
            a.map(|a| a.loop_.to_string()).unwrap_or_default(),
            a.map(|a| a.kind.to_string()).unwrap_or_default(),
            a.map(|a| a.num.to_string()).unwrap_or_default(),
            pt.label,
            pt.dtype,
        ];
        row.extend([
            String::new(),
            pt.room,
            st.job.floor.clone(),
            String::new(),
            String::new(),
            pt.zone,
            "in the panel report, not found on the drawing".into(),
            "NOT PLACED".into(),
        ]);
        let mut sty = vec!["plain"; HEAD.len()];
        sty[14] = "amber";
        un.push((row, sty));
    }
    all.push(sheet("Unresolved", &HEAD, un, &widths));
    Value::Array(all)
}

fn workbench(st: &FaState) -> Value {
    let frame = |page: usize| st.pages.get(page.saturating_sub(1)).copied().unwrap_or([0.0, 0.0, 1.0, 1.0]);
    let points: Vec<Value> = st
        .devices
        .iter()
        .filter(|d| !d.address.is_empty())
        .map(|dv| {
            let f = frame(dv.page);
            let (w, h) = ((f[2] - f[0]).max(1e-9), (f[3] - f[1]).max(1e-9));
            let x = ((dv.at[0] - f[0]) / w * 10_000.0).round() / 10_000.0;
            // Fractions from the top-left, as the FA Drawings pages are.
            let y = ((f[3] - dv.at[1]) / h * 10_000.0).round() / 10_000.0;
            let status = if st.job.points.contains_key(&dv.address) { "existing" } else { "new" };
            json!({
                "key": dv.address, "node": dv.node, "loop": dv.loop_, "kind": dv.kind, "addr": dv.number,
                "label": dv.label, "type": dv.dtype, "room": dv.room, "roomNum": dv.room_num,
                "drawing": st.drawing_name, "page": dv.page, "x": x, "y": y, "status": status,
                "note": if dv.status == "green" || dv.status == "checked" { String::new() } else { dv.flags.join("; ") },
            })
        })
        .collect();
    let rooms: Vec<Value> =
        st.rooms.iter().map(|r| json!({ "name": r.name, "num": r.num, "abbr": "", "drawing": st.drawing_name, "page": r.page })).collect();
    #[cfg(not(target_arch = "wasm32"))]
    let at = jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string();
    #[cfg(target_arch = "wasm32")]
    let at = String::new();
    json!({ "fa": "drawing", "version": 1, "exportedAt": at, "job": st.job.name, "points": points, "rooms": rooms, "labels": [] })
}

fn csv_line(row: &[String]) -> String {
    row.iter().map(|c| if c.contains([',', '"', '\n']) { format!("\"{}\"", c.replace('"', "\"\"")) } else { c.clone() }).collect::<Vec<_>>().join(",")
}

fn run_export(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fapointsexport";
    let path = str_param(p, "path").ok_or_else(|| bad(CMD, "`path` is required (points.xlsx, points.csv or *_drawing_points.json)"))?.to_string();
    let mut st = load_state(s);
    if st.devices.is_empty() {
        extract(s, &mut st)?;
    }
    let lower = path.to_ascii_lowercase();
    let mut written: Vec<(String, Vec<u8>)> = Vec::new();
    if lower.ends_with(".json") {
        written.push((path.clone(), serde_json::to_vec_pretty(&workbench(&st)).unwrap_or_default()));
    } else if lower.ends_with(".csv") {
        let mut out = csv_line(&HEAD.iter().map(|h| h.to_string()).collect::<Vec<_>>());
        for dv in &st.devices {
            out.push('\n');
            out.push_str(&csv_line(&device_row(&st, dv)));
        }
        out.push('\n');
        written.push((path.clone(), out.into_bytes()));
    } else {
        let hooks = SHEETS.get().ok_or_else(|| bad(CMD, "Excel export is not available in this build; use a .csv path"))?;
        let bytes = (hooks.write)(&workbook(&st)).map_err(|e| bad(CMD, e))?;
        written.push((path.clone(), bytes));
        if p.get("workbench").and_then(Value::as_bool).unwrap_or(true) {
            let stem = path.rsplit_once('.').map(|(a, _)| a).unwrap_or(&path);
            written.push((format!("{stem}_drawing_points.json"), serde_json::to_vec_pretty(&workbench(&st)).unwrap_or_default()));
        }
    }
    let mut names = Vec::new();
    for (name, bytes) in &written {
        #[cfg(not(target_arch = "wasm32"))]
        std::fs::write(name, bytes).map_err(|e| bad(CMD, format!("{name}: {e}")))?;
        #[cfg(target_arch = "wasm32")]
        {
            let deliver = super::file::deliver().ok_or_else(|| bad(CMD, "no way to save files here"))?;
            deliver(name, "application/octet-stream", bytes).map_err(|e| bad(CMD, e))?;
        }
        names.push(name.clone());
    }
    let c = counts(&st);
    let message = format!(
        "Wrote {} ({} device(s): {} green, {} amber, {} red, {} checked)",
        names.join(", "),
        st.devices.len(),
        c["green"],
        c["amber"],
        c["red"],
        c["checked"]
    );
    s.fa = Some(st);
    Ok(json!({ "written": names, "message": message }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadcraft_doc::{Block, Insert, LwPolyline};
    use cadcraft_geom::PolyVertex;

    fn text(s: &mut Session, v: &str, x: f64, y: f64, h: f64) {
        let _ = s.add_entity(EntityKind::Text(Text {
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
        }));
    }

    #[test]
    fn split_address_with_loop_in_the_gap() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        text(&mut s, "N9L D", 0.0, 0.0, 0.076);
        text(&mut s, "1", 0.167, 0.0, 0.049);
        text(&mut s, "29", 0.25, 0.0, 0.076);
        text(&mut s, "N9L M", 5.0, 0.0, 0.076);
        let _ = s.execute("fajob", &json!({ "reset": true }));
        let r = s.execute("fapoints", &json!({ "mark": false })).unwrap_or_default();
        let a: Vec<&str> = r["devices"].as_array().map(|v| v.iter().filter_map(|d| d["address"].as_str()).collect()).unwrap_or_default();
        assert!(a.contains(&"N9L1D29"), "{r}");
        assert!(r.to_string().contains("incomplete"), "the lone N9L M is flagged: {r}");
    }

    #[test]
    fn turned_number_piece_joins_its_address() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        let mut t = |v: &str, x: f64, y: f64, h: f64, r: f64| {
            let _ = s.add_entity(EntityKind::Text(Text {
                insert: Vec3::new(x, y, 0.0),
                align_pt: None,
                height: h,
                value: v.into(),
                rotation: r,
                width_factor: 1.0,
                oblique: 0.0,
                style: "Standard".into(),
                halign: HAlign::Left,
                valign: VAlign::Baseline,
            }));
        };
        t("N9L D", 0.861, 0.736, 0.069, 0.0);
        t("1", 1.0, 0.736, 0.049, 0.0);
        t("32", 1.153, 0.736, 0.042, std::f64::consts::FRAC_PI_2);
        t("EP-9B", 0.903, 0.639, 0.069, 0.0);
        let _ = s.execute("fajob", &json!({ "reset": true }));
        let r = s.execute("fapoints", &json!({ "mark": false })).unwrap_or_default();
        assert!(r.to_string().contains("N9L1D32"), "{r}");
    }

    #[test]
    fn addresses_parse_loosely() {
        assert_eq!(parse_addr("N1L1M01").map(|a| a.key()), Some("N1L1M1".into()));
        assert_eq!(parse_addr("N05-L1-D-01").map(|a| a.key()), Some("N5L1D1".into()));
        assert_eq!(parse_addr("N1L M01"), None, "a missing loop number is not an address");
        assert_eq!(parse_addr("L1D23").map(|a| a.key()), Some("L1D23".into()));
        assert_eq!(parse_addr("TB2"), None);
        assert_eq!(parse_addr("N1L1X01"), None);
    }

    /// Made-up sample: ROOM 101 / ROOM 102, a device address split in three pieces, a lone
    /// device with no address nearby, and a duplicate.
    fn sample() -> Session {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        text(&mut s, "B-101", 0.0, 0.0, 0.2);
        text(&mut s, "STORAGE", -0.1, 0.3, 0.15);
        text(&mut s, "B-102", 10.0, 0.0, 0.2);
        // N1L1M01 split as "N1L M" + "1" + "01"? No: "N1L" + "1M" + "01" reads N1L1M01.
        text(&mut s, "N1L", 1.0, 1.0, 0.1);
        text(&mut s, "1M", 1.25, 1.0, 0.1);
        text(&mut s, "01", 1.45, 1.0, 0.1);
        text(&mut s, "N1L1D5", 9.0, 1.0, 0.1);
        text(&mut s, "N1L1D5", 9.0, 3.0, 0.1);
        s
    }

    #[test]
    fn finds_devices_rooms_and_flags() {
        let mut s = sample();
        let _ = s.execute("fajob", &json!({ "reset": true, "set": { "node": 1 } }));
        let r = s.execute("fapoints", &json!({})).unwrap_or_default();
        let devs = r["devices"].as_array().cloned().unwrap_or_default();
        let m1 = devs.iter().find(|d| d["address"] == "N1L1M1").cloned().unwrap_or_default();
        assert_eq!(m1["roomNum"], "B-101", "{r}");
        assert_eq!(m1["room"], "STORAGE", "{r}");
        assert_eq!(m1["status"], "green", "{r}");
        let d5: Vec<&Value> = devs.iter().filter(|d| d["address"] == "N1L1D5").collect();
        assert_eq!(d5.len(), 2, "{r}");
        assert!(d5.iter().all(|d| d["status"] == "red"), "duplicates are red: {r}");
        // Only FA-POINTLIST was added to.
        let st = s.state().map(|st| st.doc.clone());
        let layers: HashSet<String> =
            st.map(|d| d.space(&Space::Model).map(|m| m.iter().map(|e| e.common.layer.clone()).collect()).unwrap_or_default()).unwrap_or_default();
        assert!(layers.contains(POINT_LAYER));
    }

    #[test]
    fn owner_room_sticks_across_reruns() {
        let mut s = sample();
        let _ = s.execute("fajob", &json!({ "reset": true, "set": { "node": 1 } }));
        let _ = s.execute("fapoints", &json!({}));
        let st = s.fa.clone().unwrap_or_default();
        let m = st.devices.iter().find(|d| d.address == "N1L1M1").map(|d| d.markers.clone()).unwrap_or_default();
        let hs: Vec<String> = m.iter().map(|h| h.hex()).collect();
        let r = s.execute("fapointroom", &json!({ "room": "B-102", "handles": hs })).unwrap_or_default();
        assert_eq!(r["assigned"], 1, "{r}");
        let r = s.execute("fapoints", &json!({})).unwrap_or_default();
        let m1 = r["devices"].as_array().and_then(|a| a.iter().find(|d| d["address"] == "N1L1M1").cloned()).unwrap_or_default();
        assert_eq!(m1["roomNum"], "B-102", "{r}");
        assert_eq!(m1["status"], "checked", "{r}");
    }

    #[test]
    fn mapped_blocks_and_boundaries() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        if let Ok(d) = s.doc_mut() {
            let b = Block::new("SMK");
            d.blocks.insert("SMK".into(), std::sync::Arc::new(b));
        }
        // Room outline with an open side area to its right: device inside the outline goes to it
        // even though another room number is nearer.
        let sq = [Vec2::new(0.0, 0.0), Vec2::new(4.0, 0.0), Vec2::new(4.0, 4.0), Vec2::new(0.0, 4.0)];
        if let Ok(d) = s.doc_mut() {
            d.layers.push(Layer { name: "ROOMS".into(), ..Layer::default() });
            let _ = d.add(
                &Space::Model,
                Common { layer: "ROOMS".into(), ..Common::default() },
                EntityKind::LwPolyline(LwPolyline {
                    vertices: sq.into_iter().map(PolyVertex::new).collect(),
                    closed: true,
                    const_width: 0.0,
                    elevation: 0.0,
                    plinegen: false,
                }),
            );
        }
        text(&mut s, "B-101", 0.2, 0.2, 0.2);
        text(&mut s, "B-102", 4.3, 3.7, 0.2);
        let _ = s.add_entity(EntityKind::Insert(Insert {
            block: "SMK".into(),
            insert: Vec3::new(3.8, 3.8, 0.0),
            scale: Vec3::new(1.0, 1.0, 1.0),
            rotation: 0.0,
            attribs: Vec::new(),
            cols: 1,
            rows: 1,
            col_spacing: 0.0,
            row_spacing: 0.0,
        }));
        let _ = s.execute("fajob", &json!({ "reset": true, "set": { "node": 1, "boundaryLayer": "ROOMS", "blockMap": { "SMK": { "type": "Smoke detector", "kind": "D" } } } }));
        let r = s.execute("fapoints", &json!({})).unwrap_or_default();
        let d0 = r["devices"][0].clone();
        assert_eq!(d0["roomNum"], "B-101", "{r}");
        assert_eq!(d0["status"], "red", "a block with no address is red: {r}");
        assert!(d0["flags"].to_string().contains("no address"), "{r}");
    }

    #[test]
    fn workbook_is_gap_filled() {
        let mut s = sample();
        let _ = s.execute("fajob", &json!({ "reset": true, "set": { "node": 1 } }));
        let _ = s.execute("fapoints", &json!({}));
        let st = s.fa.clone().unwrap_or_default();
        let wb = workbook(&st);
        let tab = wb.as_array().and_then(|a| a.iter().find(|t| t["name"] == "N1 L1").cloned()).unwrap_or_default();
        let rows = tab["rows"].as_array().cloned().unwrap_or_default();
        // Header + 159 detectors (one address twice) + 159 modules.
        assert_eq!(rows.len(), 1 + 160 + 159, "gap-filled");
        assert_eq!(rows[1][0], "N1L1D1");
        assert_eq!(rows[1][5], "", "spacer rows are blank after the address");
    }
}
