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
        CommandSpec::new("farunsheet", "Point List: Export Run Sheet", run_runsheet)
            .menu(&["Tools", "Fire Alarm", "Point List", "Export Run Sheet..."])
            .params("{path: runsheet.xlsx} → {written} (DEVICE ADDRESS, DEVICE TYPE, FUNCTION, ROOM #, INTERFACE EQUIPMENT, COMMENTS; a tab per loop, gap-filled; your comments in an earlier file are kept)")
            .enabled(has_doc),
        CommandSpec::new("fapointset", "Point List: Set Device Fields", run_set)
            .params("{id: device id, function?, equipment?, room?, roomNum?, note?, accepted?: bool} → {device} (the owner's values win over the drawing's and are kept in the job)")
            .enabled(has_doc),
        CommandSpec::new("fasheet", "Point List: Sheet Status", run_sheet)
            .params("{sheet: \"73\" | page: n, status: not started | in progress | done, initials?} → {sheets} (done writes a dated snapshot: the sheet's run sheet rows and a marked-up PDF; reopening is logged)")
            .enabled(has_doc),
        CommandSpec::new("fafield", "Point List: Field Change", run_field)
            .params("{change: moved | swapped | dual address | removed | other, text?, ids? | handles? (default: the selection), clear?: true} → {changed} (shown in Comments and on the As-built tab)")
            .enabled(has_doc),
        CommandSpec::new("faindex", "Point List: Index a Drawing Set", run_index)
            .menu(&["Tools", "Fire Alarm", "Point List", "Index Drawing Set..."])
            .params("{folder} → {files, entries} (every address, room, equipment tag and label in the folder's PDF/DXF/DWG files, saved as .fa_set_index.json there)"),
        CommandSpec::new("fasearch", "Point List: Search the Drawing Set", run_search)
            .params("{text, folder? (default: the last indexed)} → {hits: [{file, page, sheet, kind, value, at}]}"),
        CommandSpec::new("fapointnote", "Point List: Note on Selected", run_note)
            .params("{note: text (empty clears), handles?: (default: the selection)} → {noted}")
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
    /// The owner's own function / equipment (they win over what the drawing suggests).
    pub function: String,
    pub equipment: String,
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
    /// The owner's notes, by full address (N1L1M1) or device id; kept across reruns and exports.
    pub notes: BTreeMap<String, String>,
    /// Words near a device that say what it does (whole words; longer phrases anywhere).
    pub functions: Vec<FunctionRule>,
    /// Each sheet's review status, by sheet number (or "page N").
    pub sheets: BTreeMap<String, SheetState>,
    /// Field changes (moved, swapped, dual address, removed), by full address or device id.
    pub field: BTreeMap<String, Vec<FieldChange>>,
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
            notes: BTreeMap::new(),
            functions: default_functions(),
            sheets: BTreeMap::new(),
            field: BTreeMap::new(),
            extra: Map::new(),
        }
    }
}

/// A sheet's review status: not started, in progress or done, with who and when.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SheetState {
    pub status: String,
    pub date: String,
    pub initials: String,
    /// Every change of status, oldest first ("2026-10-19 14:02 AB done").
    pub history: Vec<String>,
}

/// A change found in the field (moved, swapped, dual address, removed, other).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FieldChange {
    pub change: String,
    pub text: String,
    pub date: String,
    pub by: String,
}

/// Now, local time: "YYYY-MM-DD HH:MM".
fn stamp() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        jiff::Zoned::now().strftime("%Y-%m-%d %H:%M").to_string()
    }
    #[cfg(target_arch = "wasm32")]
    {
        String::new()
    }
}

/// Who is working: the computer's user name.
fn user() -> String {
    std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default()
}

/// Append to the job's audit log (`<job>.log.csv`): when, who, what, device, before, after.
/// The log is only ever added to.
fn audit(st: &FaState, rows: &[(String, String, String, String)]) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = &st.path {
        use std::io::Write;
        let log = format!("{path}.log.csv");
        let new = !std::path::Path::new(&log).exists();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            if new {
                let _ = writeln!(f, "When,Who,What,Device,Before,After");
            }
            let (when, who) = (stamp(), user());
            for (what, dev, before, after) in rows {
                let line = [when.clone(), who.clone(), what.clone(), dev.clone(), before.clone(), after.clone()];
                let _ = writeln!(f, "{}", csv_line(&line));
            }
        }
    }
    let _ = (st, rows);
}

/// The key for a device's notes and field changes.
fn note_key(d: &Device) -> String {
    if d.key.is_empty() { d.id.clone() } else { d.key.clone() }
}

/// A sheet's key for its status: the title-block sheet number, else "page N".
fn sheet_key(d: &Device) -> String {
    if d.sheet.is_empty() { format!("page {}", d.page) } else { d.sheet.clone() }
}

/// Words near a device that say what it does.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FunctionRule {
    pub function: String,
    pub words: Vec<String>,
    /// `app` (what the device serves: waterflow, damper…), `module` (monitor / control), or
    /// `fallback` (used only when nothing else is found, e.g. the S in a smoke symbol).
    pub group: String,
    /// Only for this address type (D or M); empty: both.
    pub kind: String,
}

/// Common fire alarm terms. Each job can change or add to them in its job file.
pub fn default_functions() -> Vec<FunctionRule> {
    let r = |f: &str, g: &str, k: &str, w: &[&str]| FunctionRule {
        function: f.into(),
        words: w.iter().map(|x| x.to_string()).collect(),
        group: g.into(),
        kind: k.into(),
    };
    vec![
        r("WATERFLOW", "app", "", &["WATERFLOW", "WATER FLOW", "FLOW SWITCH", "WF"]),
        r("TAMPER", "app", "", &["TAMPER", "VALVE SUPERVISORY", "TS", "VS"]),
        r("FIRE/SMOKE DAMPER", "app", "", &["FIRE/SMOKE DAMPER", "FIRE SMOKE DAMPER", "SMOKE DAMPER", "FSD"]),
        r("FIRE DAMPER", "app", "", &["FIRE DAMPER"]),
        r("DUCT DETECTOR", "app", "", &["DUCT DETECTOR", "DUCT SMOKE", "DSD", "DD"]),
        r("HEAT DETECTOR", "app", "D", &["HEAT DETECTOR", "HEAT", "HD"]),
        r("AREA DETECTOR", "app", "D", &["AREA SMOKE", "ASD"]),
        r("PULL STATION", "app", "", &["PULL STATION", "MANUAL STATION"]),
        r("BEAM DETECTOR", "app", "D", &["BEAM"]),
        r("ELEVATOR", "app", "", &["RECALL", "ELEVATOR"]),
        r("DOOR RELEASE", "app", "", &["DOOR HOLDER", "DOOR RELEASE"]),
        r("MONITOR", "module", "M", &["AIM", "IM", "MONITOR MODULE"]),
        r("CONTROL", "module", "M", &["AOM", "CONTROL MODULE"]),
        r("RELAY", "module", "M", &["RL", "RLY", "RELAY"]),
        r("INPUT/OUTPUT", "module", "M", &["AIO"]),
        r("DUCT DETECTOR", "fallback", "D", &["RTS"]),
        r("AREA DETECTOR", "fallback", "D", &["S"]),
    ]
}

/// The address as drawn: upper case, spaces removed (`N1L M` + `1` + `01` → N1L1M01).
fn drawn(raw: &str) -> String {
    raw.split_whitespace().collect::<String>().to_ascii_uppercase()
}

/// A title block field's value: the text right of its label on the same line, else just below.
fn field_value(texts: &[TextItem], page: [f64; 4], labels: &[&str]) -> String {
    let norm = |v: &str| v.trim().trim_end_matches(':').trim().to_ascii_uppercase().replace("NO.", "NO").replace("NUMBER", "NO");
    let inside = |p: Vec2| p.x >= page[0] && p.x <= page[2] && p.y >= page[1] && p.y <= page[3];
    let mut best: Option<(f64, String)> = None;
    for l in texts.iter().filter(|t| inside(t.at) && labels.contains(&norm(&t.value).as_str())) {
        let h = l.height.max(1e-9);
        let lc = l.center();
        for t in texts.iter().filter(|t| inside(t.at)) {
            if std::ptr::eq(t, l) || t.value.trim().ends_with(':') || t.value.trim().is_empty() {
                continue;
            }
            let c = t.center();
            let (dx, dy) = (c.x - (l.at.x + l.width), lc.y - c.y);
            // Same line, to the right; else below, starting near the label's left edge.
            let score = if dy.abs() <= h && dx > -h && dx < h * 30.0 {
                dx.max(0.0)
            } else if dy > 0.0 && dy <= h * 10.0 && t.at.x > l.at.x - h * 3.0 && t.at.x < l.at.x + h * 25.0 {
                dy * 3.0 + h * 30.0
            } else {
                continue;
            };
            if best.as_ref().is_none_or(|b| score < b.0) {
                best = Some((score, t.value.trim().to_string()));
            }
        }
    }
    best.map(|b| b.1).unwrap_or_default()
}

fn title_fields(texts: &[TextItem], pages: &[[f64; 4]], page: usize) -> (String, String) {
    let Some(f) = pages.get(page.saturating_sub(1)).copied() else { return (String::new(), String::new()) };
    (field_value(texts, f, &["SHEET #", "SHEET NO", "SHEET"]), field_value(texts, f, &["DRAWING NO", "DWG NO", "DWG #", "DRAWING #"]))
}

/// A callout line naming a module by its number ("58 - STATUS"), near the device on its page:
/// (what it does, the equipment tag heading the callout).
fn callout(texts: &[TextItem], pages: &[[f64; 4]], page: usize, p: Vec2, num: u32, h: f64) -> Option<(String, String)> {
    let reach = h * 40.0;
    let mut best: Option<(f64, &TextItem, String)> = None;
    for t in texts {
        let v = t.value.trim();
        let Some((a, b)) = v.split_once('-') else { continue };
        let (a, b) = (a.trim(), b.trim().trim_matches('"').trim());
        if a.parse::<u32>().ok() != Some(num) || b.is_empty() || b.len() > 30 || !b.chars().all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '/') {
            continue;
        }
        let d = (t.center() - p).len();
        if d <= reach && page_of(pages, t.at) == page && best.as_ref().is_none_or(|x| d < x.0) {
            best = Some((d, t, b.to_ascii_uppercase()));
        }
    }
    let (_, line, act) = best?;
    // The tag heading the callout: a tag just above the line, over the same column.
    let c = line.center();
    let tag = texts
        .iter()
        .filter(|t| {
            let tc = t.center();
            let dy = tc.y - c.y;
            dy > 0.0 && dy < line.height * 6.0 && (t.at.x - line.at.x).abs() < line.height * 8.0 && is_tag(&t.value)
        })
        .min_by(|a, b| (a.center().y - c.y).total_cmp(&(b.center().y - c.y)))
        .map(|t| t.value.trim().to_string())
        .unwrap_or_default();
    Some((act, tag))
}

/// Text around devices: what each does and what it serves.
struct Context<'a> {
    texts: &'a [TextItem],
    /// Texts that can describe a device: not an address, a room number or name, or a tag
    /// repeated all over its page (a panel name above every address).
    usable: Vec<usize>,
    /// Upper-case words of each text, for whole-word matching.
    words: Vec<Vec<String>>,
    /// Panel tags: the text written just above device addresses (never equipment).
    panels: HashSet<String>,
}

fn is_tag(v: &str) -> bool {
    let v = v.trim();
    v.len() >= 4
        && v.len() <= 40
        && !v.contains(' ')
        && v.chars().any(|c| c.is_ascii_digit())
        && v.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2
        && (v.contains('-') || v.contains('.'))
}

impl<'a> Context<'a> {
    fn new(texts: &'a [TextItem], pages: &[[f64; 4]], rooms: &[Room], address_pieces: &HashSet<usize>) -> Self {
        let mut freq: HashMap<(usize, String), usize> = HashMap::new();
        for t in texts {
            *freq.entry((page_of(pages, t.at), t.value.trim().to_ascii_uppercase())).or_default() += 1;
        }
        let room_words: HashSet<String> = rooms.iter().flat_map(|r| [r.num.to_ascii_uppercase(), r.name.to_ascii_uppercase()]).collect();
        let usable = (0..texts.len())
            .filter(|i| {
                let t = &texts[*i];
                let v = t.value.trim().to_ascii_uppercase();
                !address_pieces.contains(i)
                    && !room_words.contains(&v)
                    && v.len() <= 120
                    && !(is_tag(&v) && freq.get(&(page_of(pages, t.at), v.clone())).copied().unwrap_or(0) >= 5)
                    && parse_addr(&v).is_none()
                    && !is_partial(&v)
            })
            .collect();
        let words = texts
            .iter()
            .map(|t| {
                t.value
                    .to_ascii_uppercase()
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '/')
                    .filter(|w| !w.is_empty())
                    .map(String::from)
                    .collect()
            })
            .collect();
        // The line right above an address is its panel tag.
        let mut panels = HashSet::new();
        for i in address_pieces {
            let Some(a) = texts.get(*i) else { continue };
            let c = a.center();
            for t in texts {
                let tc = t.center();
                let dy = tc.y - c.y;
                if dy > a.height * 0.6 && dy < a.height * 2.2 && (tc.x - c.x).abs() < a.width.max(t.width) && is_tag(&t.value) {
                    panels.insert(t.value.trim().to_ascii_uppercase());
                }
            }
        }
        Context { texts, usable, words, panels }
    }

    /// (function, equipment, notes for the Comments column). Nothing is guessed: when the text
    /// around a device points two ways at about the same distance, the field stays blank and the
    /// note says why.
    fn describe(&self, p: Vec2, kind: &str, reach: f64, rules: &[FunctionRule]) -> (String, String, Vec<String>) {
        let mut near: Vec<(f64, usize)> = self
            .usable
            .iter()
            .filter_map(|i| {
                let d = (self.texts[*i].center() - p).len();
                (d <= reach).then_some((d, *i))
            })
            .collect();
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        // The nearest hit per function, by group.
        let mut hits: Vec<(String, String, f64)> = Vec::new();
        let mut equipment: Vec<(String, f64)> = Vec::new();
        let mut action: Vec<(String, f64)> = Vec::new();
        for (d, i) in &near {
            let t = &self.texts[*i];
            let up = t.value.to_ascii_uppercase();
            let words = &self.words[*i];
            for r in rules.iter().filter(|r| r.kind.is_empty() || r.kind.eq_ignore_ascii_case(kind)) {
                let hit = r.words.iter().any(|w| {
                    let w = w.to_ascii_uppercase();
                    if w.contains(' ') || w.contains('/') { up.contains(&w) } else { words.contains(&w) }
                });
                let g = if r.group.is_empty() { "app".to_string() } else { r.group.to_ascii_lowercase() };
                if hit && !hits.iter().any(|h| h.0 == g && h.1 == r.function) {
                    hits.push((g, r.function.clone(), *d));
                }
            }
            // `TAG: "STOP"` lines: the equipment and what the module does to it.
            if let Some((tag, rest)) = t.value.split_once(':')
                && let Some(q) = rest.split('"').nth(1)
            {
                let tag = tag.trim().to_string();
                if !tag.is_empty() && !equipment.iter().any(|e| e.0 == tag) {
                    equipment.push((tag, *d));
                }
                let q = q.trim().to_ascii_uppercase();
                if !q.is_empty() && !action.iter().any(|a| a.0 == q) {
                    action.push((q, *d));
                }
            } else if is_tag(&t.value) && !self.panels.contains(&t.value.trim().to_ascii_uppercase()) {
                let tag = t.value.trim().to_string();
                if !equipment.iter().any(|e| e.0 == tag) {
                    equipment.push((tag, *d));
                }
            }
        }
        let mut notes = Vec::new();
        // The nearest one wins when it's clearly nearest; two at about the same distance: unsure.
        let pick = |list: Vec<(String, f64)>, what: &str, notes: &mut Vec<String>| -> String {
            match list.as_slice() {
                [] => String::new(),
                [(a, _)] => a.clone(),
                [(a, d1), (b, d2), ..] => {
                    if *d2 > d1 * 1.5 {
                        a.clone()
                    } else {
                        notes.push(format!("{what} unsure: {a} or {b} (NEEDS_REVIEW)"));
                        String::new()
                    }
                }
            }
        };
        let group = |g: &str| -> Vec<(String, f64)> { hits.iter().filter(|h| h.0 == g).map(|h| (h.1.clone(), h.2)).collect() };
        let mut function = pick(group("app"), "function", &mut notes);
        if function.is_empty() && !notes.iter().any(|n| n.starts_with("function")) {
            function = pick(group("fallback"), "function", &mut notes);
        }
        let module = pick(group("module"), "function", &mut notes);
        // Only a module carries out a STOP / STATUS.
        let act = if kind.eq_ignore_ascii_case("M") { pick(action, "function", &mut notes) } else { String::new() };
        let extra: Vec<String> = [module, act].into_iter().filter(|x| !x.is_empty()).collect();
        let function = match (function.is_empty(), extra.is_empty()) {
            (_, true) => function,
            (true, false) => extra.join(" "),
            (false, false) => format!("{function} ({})", extra.join(" ")),
        };
        let equipment = pick(equipment, "equipment", &mut notes);
        (function, equipment, notes)
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
    /// Text that looks like part of an address but wasn't read as one: (page, box, text).
    pub missed: Vec<(usize, [f64; 4], String)>,
    /// Markers only on amber and red ("show only what needs me"), kept across reruns.
    pub only_problems: bool,
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
        if *i == st || *i - st > 3 {
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
    /// The text in world coordinates, for drawing a highlight copy over it.
    src: Text,
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
    let src = Text {
        insert: Vec3::new(m.apply(t.insert.xy()).x, m.apply(t.insert.xy()).y, 0.0),
        align_pt: t.align_pt.map(|a| {
            let q = m.apply(a.xy());
            Vec3::new(q.x, q.y, 0.0)
        }),
        height,
        value: t.value.clone(),
        rotation: dir.y.atan2(dir.x),
        width_factor: t.width_factor,
        oblique: t.oblique,
        style: t.style.clone(),
        halign: t.halign,
        valign: t.valign,
    };
    Some(TextItem { at, dir, height, value, layer: layer.to_string(), width, src })
}

struct Scan {
    texts: Vec<TextItem>,
    /// Line-drawn label strokes (PDF_SHX_GLYPHS): box and geometry, for highlighting a label
    /// that is drawn rather than written.
    glyphs: Vec<([f64; 4], EntityKind)>,
    /// Linework for room spaces (walls): segments longer than a short stroke.
    walls: Vec<(Vec2, Vec2)>,
    inserts: Vec<InsertItem>,
    /// Closed polylines on the boundary layer.
    boundaries: Vec<Vec<Vec2>>,
    pages: Vec<[f64; 4]>,
}

fn skip(d: &Drawing, layer: &str) -> bool {
    SKIP_LAYERS.iter().any(|l| l.eq_ignore_ascii_case(layer)) || d.layer(layer).is_some_and(|l| l.frozen)
}

fn scan(d: &Drawing, boundary_layer: &str) -> Scan {
    let mut sc = Scan { walls: Vec::new(), glyphs: Vec::new(), texts: Vec::new(), inserts: Vec::new(), boundaries: Vec::new(), pages: Vec::new() };
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
            if depth == 0 && e.common.layer == "PDF_SHX_GLYPHS" && sc.glyphs.len() < 2_000_000 {
                let pts: Vec<Vec2> = match &e.kind {
                    EntityKind::Line(l) => vec![l.a.xy(), l.b.xy()],
                    EntityKind::LwPolyline(p) => p.vertices.iter().map(|v| v.p).collect(),
                    _ => Vec::new(),
                };
                if !pts.is_empty() {
                    let xs = pts.iter().map(|p| p.x);
                    let ys = pts.iter().map(|p| p.y);
                    let b = [
                        xs.clone().fold(f64::INFINITY, f64::min),
                        ys.clone().fold(f64::INFINITY, f64::min),
                        xs.fold(f64::NEG_INFINITY, f64::max),
                        ys.fold(f64::NEG_INFINITY, f64::max),
                    ];
                    sc.glyphs.push((b, e.kind.clone()));
                }
                continue;
            }
            if depth == 0
                && !matches!(e.common.layer.as_str(), "PDF_FILLS" | "PDF_WHITE" | "PDF_SHX_GLYPHS" | "PDF_SHX_SEARCH" | "PDF_TEXT_HIDDEN")
                && sc.walls.len() < 3_000_000
            {
                let pts: Vec<Vec2> = match &e.kind {
                    EntityKind::Line(l) => vec![l.a.xy(), l.b.xy()],
                    EntityKind::LwPolyline(p) => {
                        let mut v: Vec<Vec2> = p.vertices.iter().map(|v| v.p).collect();
                        if p.closed
                            && let Some(f) = v.first().copied()
                        {
                            v.push(f);
                        }
                        v
                    }
                    _ => Vec::new(),
                };
                for w in pts.windows(2) {
                    if let [a, b] = w {
                        sc.walls.push((*a, *b));
                    }
                }
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
    /// The text pieces it was read from (indices into the scanned texts).
    pieces: Vec<usize>,
}

/// An address that starts but doesn't complete.
#[derive(Clone, Debug)]
struct Partial {
    at: Vec2,
    raw: String,
    height: f64,
    piece: usize,
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
fn address_texts(texts: &[TextItem], layers: &[String], partial: &mut Vec<Partial>) -> Vec<AddrText> {
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
                        out.push(AddrText { addr, raw, at: c, height: first.height, pieces: run[i..j].to_vec() });
                    }
                    i = j;
                }
                None => {
                    let t = &texts[run[i]];
                    if is_partial(&t.value) && ok_layer(&t.layer) {
                        partial.push(Partial { at: t.center(), raw: t.value.clone(), height: t.height, piece: run[i] });
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
    /// The room number and name text on the drawing (lit up when a device is hovered).
    pub bbox: [f64; 4],
    pub page: usize,
    /// text | boundary
    pub source: String,
    #[serde(skip)]
    pub outline: Option<Vec<Vec2>>,
}

/// A text's box on the drawing.
fn text_box(t: &TextItem) -> [f64; 4] {
    let up = Vec2::new(-t.dir.y, t.dir.x);
    let pts = [t.at, t.at + t.dir * t.width, t.at + up * t.height, t.at + t.dir * t.width + up * t.height];
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for q in pts {
        b = [b[0].min(q.x), b[1].min(q.y), b[2].max(q.x), b[3].max(q.y)];
    }
    b
}

/// Rooms as wall regions: the page's linework is drawn on a grid and each room number floods
/// the space around it until it meets a wall. A device then belongs to the room(s) whose space it
/// is in. Spaces that leak out over much of the page are not used.
struct Regions {
    /// Per page: grid origin, cell size, columns, rows, cell labels (0 = wall/unset).
    grids: Vec<([f64; 2], f64, usize, usize, Vec<u32>)>,
    /// Region label → room indices.
    rooms: HashMap<(usize, u32), Vec<usize>>,
}

impl Regions {
    fn build(sc: &Scan, pages: &[[f64; 4]], rooms: &[Room]) -> Regions {
        let mut out = Regions { grids: Vec::new(), rooms: HashMap::new() };
        for (pi, f) in pages.iter().enumerate() {
            let (w, h) = (f[2] - f[0], f[3] - f[1]);
            if !(w > 0.0 && h > 0.0) {
                out.grids.push(([0.0, 0.0], 1.0, 0, 0, Vec::new()));
                continue;
            }
            // About 1000 cells across the page, and no more than 2 million in all.
            let cell = (w.max(h) / 1000.0).max((w * h / 2.0e6).sqrt());
            let (cols, rows) = ((w / cell).ceil() as usize + 1, (h / cell).ceil() as usize + 1);
            const WALL: u32 = u32::MAX;
            let mut g = vec![0u32; cols * rows];
            let at = |x: f64, y: f64| -> Option<usize> {
                let (cx, cy) = ((x - f[0]) / cell, (y - f[1]) / cell);
                if cx < 0.0 || cy < 0.0 {
                    return None;
                }
                let (cx, cy) = (cx as usize, cy as usize);
                (cx < cols && cy < rows).then_some(cy * cols + cx)
            };
            for (a, b) in &sc.walls {
                let mid = (*a + *b) * 0.5;
                if mid.x < f[0] || mid.x > f[2] || mid.y < f[1] || mid.y > f[3] {
                    continue;
                }
                let len = (*b - *a).len();
                let n = ((len / (cell * 0.5)).ceil() as usize).clamp(1, 20_000);
                for k in 0..=n {
                    let q = *a + (*b - *a) * (k as f64 / n as f64);
                    if let Some(i) = at(q.x, q.y) {
                        g[i] = WALL;
                    }
                }
            }
            let limit = cols * rows / 5;
            let mut label = 0u32;
            for (ri, r) in rooms.iter().enumerate().filter(|(_, r)| r.page == pi + 1) {
                let Some(start) = at(r.at.x, r.at.y) else { continue };
                // The number text itself may sit on linework: start from the nearest open cell.
                let start = [0isize, 1, -1, 2, -2]
                    .iter()
                    .flat_map(|dy| [0isize, 1, -1, 2, -2].iter().map(move |dx| (*dx, *dy)))
                    .map(|(dx, dy)| start as isize + dy * cols as isize + dx)
                    .find(|i| *i >= 0 && (*i as usize) < g.len() && g[*i as usize] != WALL);
                let Some(start) = start.map(|i| i as usize) else { continue };
                if g[start] != 0 {
                    out.rooms.entry((pi, g[start])).or_default().push(ri);
                    continue;
                }
                label += 1;
                let mut stack = vec![start];
                let mut filled = Vec::new();
                g[start] = label;
                while let Some(i) = stack.pop() {
                    filled.push(i);
                    if filled.len() > limit {
                        break;
                    }
                    let (x, y) = (i % cols, i / cols);
                    for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                        if nx < cols && ny < rows {
                            let j = ny * cols + nx;
                            if g[j] == 0 {
                                g[j] = label;
                                stack.push(j);
                            }
                        }
                    }
                }
                if filled.len() > limit {
                    // Leaked: this space isn't a room. Mark it so later rooms don't refill it.
                    for i in filled.into_iter().chain(stack) {
                        g[i] = WALL - 1;
                    }
                    continue;
                }
                out.rooms.entry((pi, label)).or_default().push(ri);
            }
            out.grids.push(([f[0], f[1]], cell, cols, rows, g));
        }
        out
    }

    /// The rooms whose space holds `p` on `page`: None when it isn't in a room's space.
    fn rooms_at(&self, page: usize, p: Vec2) -> Option<&Vec<usize>> {
        let (o, cell, cols, rows, g) = self.grids.get(page.checked_sub(1)?)?;
        let (cx, cy) = (((p.x - o[0]) / cell).floor(), ((p.y - o[1]) / cell).floor());
        if cx < 0.0 || cy < 0.0 {
            return None;
        }
        let (cx, cy) = (cx as usize, cy as usize);
        // On a line: the nearest labelled cell close by.
        for r in 0..3usize {
            for dy in -(r as isize)..=(r as isize) {
                for dx in -(r as isize)..=(r as isize) {
                    let (x, y) = (cx as isize + dx, cy as isize + dy);
                    if x < 0 || y < 0 || x as usize >= *cols || y as usize >= *rows {
                        continue;
                    }
                    let l = g[y as usize * cols + x as usize];
                    if l != 0 && l < u32::MAX - 1 {
                        return self.rooms.get(&(page - 1, l));
                    }
                }
            }
        }
        None
    }
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
        // A room name has a real word in it (not a panel or power tag like EP-1A).
        let has_word = |n: &str| n.split(|c: char| !c.is_ascii_alphabetic()).any(|w| w.len() >= 3) && !is_tag(n);
        if name.chars().count() > 30 || !has_word(&name) {
            name.clear();
        }
        if name.chars().filter(|c| c.is_ascii_alphabetic()).count() < 2 {
            name.clear();
            let mut best = f64::INFINITY;
            for o in &sc.texts {
                let v = o.value.trim();
                if v.len() < 2 || v.len() > 30 || !has_word(v) || is_num(v) || noise(v) || parse_addr(v).is_some() || is_partial(v) {
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
        let mut bbox = text_box(t);
        if let Some(o) = sc.texts.iter().find(|o| !name.is_empty() && o.value.trim() == name && (o.center() - c).len() < t.height.max(o.height) * 4.0)
        {
            let b2 = text_box(o);
            bbox = [bbox[0].min(b2[0]), bbox[1].min(b2[1]), bbox[2].max(b2[2]), bbox[3].max(b2[3])];
        }
        out.push(Room { num, name, at: c, bbox, page, source: "text".into(), outline: None });
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
    /// The full address for matching (N1L1M1): duplicates, the panel report.
    pub key: String,
    /// The address exactly as drawn (N1L1M01).
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
    /// The sheet number and drawing number from the page's title block.
    pub sheet: String,
    pub drawing_no: String,
    /// What the device does (waterflow, fire/smoke damper, duct detector…), from the drawing.
    pub function: String,
    /// The equipment it serves (damper, fan, elevator tag), from the drawing.
    pub equipment: String,
    /// The owner's own note (kept across reruns and exports).
    pub note: String,
    /// The address text pieces (world-space copies) to highlight.
    #[serde(skip)]
    pub pieces: Vec<Text>,
    /// The address's drawn strokes (a line-drawn label), highlighted instead of the text.
    #[serde(skip)]
    pub strokes: Vec<EntityKind>,
    /// The address on the drawing, and the room text it was matched to (for hover).
    pub bbox: [f64; 4],
    pub room_bbox: Option<[f64; 4]>,
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
        key: String::new(),
        sheet: String::new(),
        drawing_no: String::new(),
        function: String::new(),
        equipment: String::new(),
        note: String::new(),
        pieces: Vec::new(),
        strokes: Vec::new(),
        bbox: [at.x, at.y, at.x, at.y],
        room_bbox: None,
    };
    let mut partial: Vec<Partial> = Vec::new();
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
                    dv.pieces = a.pieces.iter().filter_map(|k| sc.texts.get(*k).map(|t| t.src.clone())).collect();
                    dv.size = a.height;
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
        dv.pieces = a.pieces.iter().filter_map(|k| sc.texts.get(*k).map(|t| t.src.clone())).collect();
        devices.push(dv);
    }
    // Address text that starts like an address but can't be completed: red, never guessed.
    for pa in &partial {
        if in_areas(&job.ignore_areas, pa.at) || addr_texts.iter().any(|a| (a.at - pa.at).len() < a.height * 3.0) {
            continue;
        }
        let page = page_of(&st.pages, pa.at);
        let mut dv = blank(format!("X{:.2},{:.2}", pa.at.x, pa.at.y), pa.at, page, "text");
        dv.raw = pa.raw.clone();
        dv.address = drawn(&pa.raw);
        dv.size = pa.height;
        dv.pieces = sc.texts.get(pa.piece).map(|t| vec![t.src.clone()]).unwrap_or_default();
        dv.flags.push(format!("address text \"{}\" is incomplete (NEEDS_REVIEW)", pa.raw));
        devices.push(dv);
    }
    // A label kept as strokes is highlighted by its strokes; its searchable text copy only
    // approximates where it sits.
    for dv in &mut devices {
        if dv.pieces.is_empty() {
            continue;
        }
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        let mut h = 0.0f64;
        for t in &dv.pieces {
            let dir = Vec2::new(t.rotation.cos(), t.rotation.sin());
            let up = Vec2::new(-dir.y, dir.x);
            let w =
                t.align_pt.map(|a| (a.xy() - t.insert.xy()).len()).filter(|w| *w > 0.0).unwrap_or(t.value.chars().count() as f64 * t.height * 0.8);
            for q in [t.insert.xy(), t.insert.xy() + dir * w, t.insert.xy() + up * t.height, t.insert.xy() + dir * w + up * t.height] {
                b = [b[0].min(q.x), b[1].min(q.y), b[2].max(q.x), b[3].max(q.y)];
            }
            h = h.max(t.height);
        }
        dv.bbox = b;
        let pad = h * 0.6;
        let strokes: Vec<EntityKind> = sc
            .glyphs
            .iter()
            .filter(|(g, _)| {
                // The stroke's centre inside the label box, and the stroke no bigger than a character.
                let (cx, cy) = ((g[0] + g[2]) / 2.0, (g[1] + g[3]) / 2.0);
                cx >= b[0] - pad && cx <= b[2] + pad && cy >= b[1] - pad && cy <= b[3] + pad && g[3] - g[1] <= h * 1.6 && g[2] - g[0] <= h * 2.5
            })
            .map(|(_, k)| k.clone())
            .take(400)
            .collect();
        if !strokes.is_empty() {
            dv.strokes = strokes;
        }
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
            dv.key = a.key();
            dv.address = drawn(&dv.raw);
            dv.node = a.node;
            dv.loop_ = Some(a.loop_);
            dv.number = Some(a.num);
            if a.num > MAX_ADDR {
                dv.flags.push(format!("address number {} is over {MAX_ADDR}", a.num));
            }
        }
    }
    // Rooms.
    let regions = Regions::build(&sc, &st.pages, &rooms);
    for dv in &mut devices {
        let p = Vec2::new(dv.at[0], dv.at[1]);
        if let Some(dec) = job.decisions.get(&dv.id).filter(|x| !x.room_num.is_empty() || !x.room.is_empty()) {
            dv.room_num = dec.room_num.clone();
            dv.room = dec.room.clone();
            dv.room_bbox = rooms.iter().find(|r| r.page == dv.page && !dec.room_num.is_empty() && r.num == dec.room_num).map(|r| r.bbox);
            continue;
        }
        let here: Vec<&Room> = rooms.iter().filter(|r| r.page == dv.page).collect();
        if let Some(r) = here.iter().find(|r| r.outline.as_ref().is_some_and(|o| in_poly(p, o))) {
            dv.room_num = r.num.clone();
            dv.room = r.name.clone();
            dv.room_bbox = Some(r.bbox);
            continue;
        }
        // The room whose walls hold the device. Two or more room numbers in one space (an open
        // area): the nearest of them, flagged.
        if let Some(ids) = regions.rooms_at(dv.page, p) {
            let mut inside: Vec<(f64, &Room)> = ids.iter().filter_map(|i| rooms.get(*i)).map(|r| ((r.at - p).len(), r)).collect();
            inside.sort_by(|a, b| a.0.total_cmp(&b.0));
            if let Some((_, r)) = inside.first() {
                dv.room_num = r.num.clone();
                dv.room = r.name.clone();
                dv.room_bbox = Some(r.bbox);
                if inside.len() > 1 {
                    let all: Vec<&str> = inside.iter().take(3).map(|(_, r)| r.num.as_str()).collect();
                    dv.flags.push(format!("room close call: open area with {} (NEEDS_REVIEW)", all.join(", ")));
                }
                continue;
            }
        }
        let mut by: Vec<(f64, &Room)> = here.iter().map(|r| ((r.at - p).len(), *r)).collect();
        by.sort_by(|a, b| a.0.total_cmp(&b.0));
        match by.as_slice() {
            [] => dv.flags.push("no room found (NEEDS_REVIEW)".into()),
            [(_, r)] => {
                dv.room_num = r.num.clone();
                dv.room = r.name.clone();
                dv.room_bbox = Some(r.bbox);
            }
            [(d1, r), (d2, r2), ..] => {
                dv.room_num = r.num.clone();
                dv.room = r.name.clone();
                dv.room_bbox = Some(r.bbox);
                if *d2 <= d1 * 1.15 {
                    dv.flags.push(format!("room close call: {} or {} (NEEDS_REVIEW)", r.num, r2.num));
                }
            }
        }
    }
    // Title block: sheet number and drawing number of each page.
    let titles: Vec<(String, String)> = (1..=st.pages.len().max(1)).map(|pg| title_fields(&sc.texts, &st.pages, pg)).collect();
    // What each device does and what it serves, from the text around it.
    let address_pieces: HashSet<usize> = addr_texts.iter().flat_map(|a| a.pieces.iter().copied()).chain(partial.iter().map(|p| p.piece)).collect();
    let ctx = Context::new(&sc.texts, &st.pages, &rooms, &address_pieces);
    // Address-like text that wasn't read: shown by "Show missed", never counted.
    let address_like = |v: &str| {
        let u: String = v.to_ascii_uppercase().chars().filter(|c| !c.is_whitespace()).collect();
        let b = u.as_bytes();
        (1..b.len()).any(|i| {
            let (prev, cur) = (b[i - 1], b[i]);
            (cur == b'L' && prev.is_ascii_digit() && b.first() == Some(&b'N'))
                || ((cur == b'D' || cur == b'M') && prev.is_ascii_digit() && u.contains('L'))
        })
    };
    st.missed = sc
        .texts
        .iter()
        .enumerate()
        .filter(|(i, t)| !address_pieces.contains(i) && t.value.len() <= 20 && address_like(&t.value) && parse_addr(&t.value).is_none())
        .map(|(_, t)| (page_of(&st.pages, t.at), text_box(t), t.value.trim().to_string()))
        .take(5000)
        .collect();
    for dv in &mut devices {
        if let Some((sheet, dwg)) = titles.get(dv.page.saturating_sub(1)) {
            dv.sheet = sheet.clone();
            dv.drawing_no = dwg.clone();
        }
        let p = Vec2::new(dv.at[0], dv.at[1]);
        let reach = if dv.size > 0.0 { dv.size * 7.0 } else { near };
        let (function, equipment, notes) = ctx.describe(p, &dv.kind, reach, &job.functions);
        dv.function = function;
        dv.equipment = equipment;
        dv.flags.extend(notes);
        // A callout box listing the module numbers ("58 - STATUS" under "…-FCU-40").
        if dv.kind == "M"
            && let Some(n) = dv.number
            && let Some((act, tag)) = callout(&sc.texts, &st.pages, dv.page, p, n, dv.size.max(near / 7.0))
        {
            if dv.equipment.is_empty() && !tag.is_empty() {
                dv.equipment = tag;
                dv.flags.retain(|f| !f.starts_with("equipment"));
            }
            if !dv.function.contains(&act) {
                dv.function = if dv.function.is_empty() { act } else { format!("{} {act}", dv.function) };
            }
        }
        if let Some(dec) = job.decisions.get(&dv.id) {
            if !dec.function.is_empty() {
                dv.function = dec.function.clone();
                dv.flags.retain(|f| !f.starts_with("function"));
            }
            if !dec.equipment.is_empty() {
                dv.equipment = dec.equipment.clone();
                dv.flags.retain(|f| !f.starts_with("equipment"));
            }
        }
        if dv.dtype.is_empty() {
            dv.dtype = match dv.kind.as_str() {
                "D" if dv.function == "HEAT DETECTOR" => "HEAT".into(),
                "D" => "SMOKE".into(),
                "M" => "MODULE".into(),
                _ => String::new(),
            };
        }
        if let Some(n) = job.notes.get(&dv.key).filter(|_| !dv.key.is_empty()).or_else(|| job.notes.get(&dv.id)) {
            dv.note = n.clone();
        }
        let fk = if dv.key.is_empty() { dv.id.clone() } else { dv.key.clone() };
        if let Some(list) = job.field.get(&fk) {
            for c in list {
                dv.flags.push(format!(
                    "FIELD {}: {}{}",
                    c.change,
                    c.text,
                    if c.date.is_empty() { String::new() } else { format!(" ({} {})", c.date, c.by) }
                ));
            }
        }
    }
    // Panel report, duplicates, labels, status.
    let mut count: HashMap<String, usize> = HashMap::new();
    for dv in devices.iter().filter(|d| !d.key.is_empty()) {
        *count.entry(dv.key.clone()).or_default() += 1;
    }
    let have_report = !job.points.is_empty();
    for dv in &mut devices {
        let dec = job.decisions.get(&dv.id).cloned().unwrap_or_default();
        if dv.key.is_empty() {
            dv.flags.push("no address (NEEDS_REVIEW)".into());
        } else if count.get(&dv.key).copied().unwrap_or(0) > 1 {
            dv.flags.push(format!("duplicate: {} appears {} times", dv.key, count.get(&dv.key).copied().unwrap_or(0)));
        }
        if let Some(pt) = job.points.get(&dv.key) {
            dv.label = pt.label.clone();
            if dv.dtype.is_empty() {
                dv.dtype = pt.dtype.clone();
            }
            if pt.source == "runsheet" && job.points.values().any(|p| p.source == "report") {
                dv.flags.push("on the run sheet only, not in the panel program".into());
            }
        } else if have_report && !dv.key.is_empty() {
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
        } else if dv.flags.iter().any(|f| !f.starts_with("function") && !f.starts_with("equipment") && !f.starts_with("FIELD")) {
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
        // The device's own address text, drawn over in its status colour. A block with no
        // address text gets a small ring instead.
        if !dv.strokes.is_empty() {
            for k in &dv.strokes {
                dv.markers.push(d.add(&Space::Model, common(), k.clone())?);
            }
        } else if dv.pieces.is_empty() {
            let c = Vec3::new(dv.at[0], dv.at[1], 0.0);
            dv.markers.push(d.add(&Space::Model, common(), EntityKind::Circle(Circle { center: c, radius: r }))?);
        } else {
            for t in &dv.pieces {
                dv.markers.push(d.add(&Space::Model, common(), EntityKind::Text(t.clone()))?);
            }
        }
    }
    Ok(())
}

fn counts(st: &FaState) -> Value {
    let n = |k: &str| st.devices.iter().filter(|d| d.status == k).count();
    json!({ "green": n("green"), "amber": n("amber"), "red": n("red"), "checked": n("checked") })
}

/// Report points not found on the drawing.
fn not_placed(st: &FaState) -> Vec<String> {
    let found: HashSet<&str> = st.devices.iter().map(|d| d.key.as_str()).collect();
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
    // No job file yet: keep the review next to the drawing (`<drawing>.pointlist.json`), and pick
    // up an earlier review of it when there is one.
    #[cfg(not(target_arch = "wasm32"))]
    if st.path.is_none()
        && let Some(dp) = s.state().ok().and_then(|ds| ds.path.clone().or_else(|| ds.source.clone()))
    {
        let jp = format!("{dp}.pointlist.json");
        if let Ok(text) = std::fs::read_to_string(&jp)
            && let Ok(job) = serde_json::from_str::<Job>(&text)
            && st.job.decisions.is_empty()
            && st.job.notes.is_empty()
        {
            st.job = job;
        }
        st.path = Some(jp);
    }
    extract(s, &mut st)?;
    if let Some(o) = p.get("onlyProblems").and_then(Value::as_bool) {
        st.only_problems = o;
    }
    if p.get("mark").and_then(Value::as_bool).unwrap_or(true) {
        let only = st.only_problems;
        mark(s, &mut st, only)?;
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
    if let Some(ids) = p.get("ids").and_then(Value::as_array) {
        let want: HashSet<&str> = ids.iter().filter_map(Value::as_str).collect();
        return (0..st.devices.len()).filter(|i| want.contains(st.devices[*i].id.as_str())).collect();
    }
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
    let mut log = Vec::new();
    for i in &idx {
        let dv = st.devices[*i].clone();
        let d = st.job.decisions.entry(dv.id.clone()).or_default();
        d.room_num = num.clone();
        d.room = name.clone();
        d.accepted = true;
        log.push((
            "room".to_string(),
            dv.address.clone(),
            format!("{} {}", dv.room_num, dv.room).trim().to_string(),
            format!("{num} {name}").trim().to_string(),
        ));
    }
    audit(&st, &log);
    save_job(&st)?;
    s.fa = Some(st);
    let r = run_points(s, &json!({}))?;
    Ok(
        json!({ "assigned": idx.len(), "counts": r["counts"], "message": format!("{} device(s) set to room {} {} and checked", idx.len(), num, name) }),
    )
}

fn run_runsheet(s: &mut Session, p: &Value) -> Result<Value> {
    let mut q = p.clone();
    if let Some(o) = q.as_object_mut() {
        o.insert("format".into(), json!("runsheet"));
    }
    if str_param(&q, "path").is_none() {
        return Err(bad("farunsheet", "`path` is required (runsheet.xlsx)"));
    }
    run_export(s, &q)
}

fn run_set(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fapointset";
    let mut st = load_state(s);
    let id = str_param(p, "id").ok_or_else(|| bad(CMD, "`id` (a device id from FAPOINTS) is required"))?.to_string();
    let Some(dv) = st.devices.iter().find(|d| d.id == id).cloned() else {
        return Err(bad(CMD, format!("no device {id}; run FAPOINTS first")));
    };
    let before = serde_json::to_string(st.job.decisions.get(&id).unwrap_or(&Decision::default())).unwrap_or_default();
    let note_before = st.job.notes.get(&note_key(&dv)).cloned().unwrap_or_default();
    let dec = st.job.decisions.entry(id.clone()).or_default();
    let take = |k: &str| str_param(p, k).map(|v| v.trim().to_string());
    if let Some(v) = take("function") {
        dec.function = v;
    }
    if let Some(v) = take("equipment") {
        dec.equipment = v;
    }
    if let Some(v) = take("roomNum") {
        dec.room_num = v;
    }
    if let Some(v) = take("room") {
        dec.room = v;
    }
    if let Some(a) = p.get("accepted").and_then(Value::as_bool) {
        dec.accepted = a;
    }
    if let Some(v) = take("note") {
        let k = note_key(&dv);
        if v.is_empty() {
            st.job.notes.remove(&k);
        } else {
            st.job.notes.insert(k, v);
        }
    }
    let after = serde_json::to_string(st.job.decisions.get(&id).unwrap_or(&Decision::default())).unwrap_or_default();
    let note_after = st.job.notes.get(&note_key(&dv)).cloned().unwrap_or_default();
    let mut log = Vec::new();
    if before != after {
        log.push(("set".to_string(), dv.address.clone(), before, after));
    }
    if note_before != note_after {
        log.push(("note".to_string(), dv.address.clone(), note_before, note_after));
    }
    audit(&st, &log);
    save_job(&st)?;
    s.fa = Some(st);
    let _ = run_points(s, &json!({}))?;
    let dv = s.fa.as_ref().and_then(|st| st.devices.iter().find(|d| d.id == id).cloned());
    Ok(json!({ "device": dv, "message": format!("{} updated", dv.as_ref().map(|d| d.address.clone()).unwrap_or(id)) }))
}

fn run_sheet(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fasheet";
    let mut st = load_state(s);
    if st.devices.is_empty() {
        extract(s, &mut st)?;
    }
    let key = match (str_param(p, "sheet"), p.get("page").and_then(Value::as_u64)) {
        (Some(k), _) => k.trim().to_string(),
        (None, Some(pg)) => st.devices.iter().find(|d| d.page as u64 == pg).map(sheet_key).unwrap_or(format!("page {pg}")),
        (None, None) => return Err(bad(CMD, "`sheet` (its number) or `page` is required")),
    };
    let status = str_param(p, "status").map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default();
    if !matches!(status.as_str(), "not started" | "in progress" | "done") {
        return Err(bad(CMD, "`status` must be not started, in progress or done"));
    }
    let initials = str_param(p, "initials").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).unwrap_or_else(user);
    let now = stamp();
    let e = st.job.sheets.entry(key.clone()).or_default();
    let before = e.status.clone();
    let reopened = before == "done" && status != "done";
    e.status = status.clone();
    e.date = now.clone();
    e.initials = initials.clone();
    e.history.push(format!("{now} {initials} {}{status}", if reopened { "reopened: " } else { "" }));
    audit(&st, &[("sheet".into(), key.clone(), before, status.clone())]);
    save_job(&st)?;
    // Done: a locked, dated copy of the sheet's rows and a marked-up PDF of the page.
    let mut written = Vec::new();
    #[cfg(not(target_arch = "wasm32"))]
    if status == "done"
        && let Some(jp) = st.path.clone()
    {
        let stem = jp.trim_end_matches(".json").trim_end_matches(".pointlist").to_string();
        let day = now.get(..10).unwrap_or("").to_string();
        let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let mut sub = st.clone();
        sub.devices.retain(|d| sheet_key(d) == key);
        if let Some(h) = SHEETS.get() {
            let name = unique(&format!("{stem}_Sheet{safe}_{day}_runsheet.xlsx"));
            if let Ok(bytes) = (h.write)(&runsheet_workbook(&sub))
                && std::fs::write(&name, bytes).is_ok()
            {
                written.push(name);
            }
        }
        let page = sub.devices.first().map(|d| d.page).unwrap_or(1);
        if let (Some(io), Some(f)) = (super::file::io(), st.pages.get(page.saturating_sub(1)))
            && let Some(plot) = io.plot
        {
            let ds = s.state()?;
            let opts = json!({ "window": [[f[0], f[1]], [f[2], f[3]]], "fit": true });
            if let Ok(bytes) = plot(&ds.doc, &Space::Model, &opts) {
                let name = unique(&format!("{stem}_Sheet{safe}_{day}_markup.pdf"));
                if std::fs::write(&name, bytes).is_ok() {
                    written.push(name);
                }
            }
        }
    }
    let sheets = serde_json::to_value(&st.job.sheets).unwrap_or_default();
    s.fa = Some(st);
    Ok(
        json!({ "sheets": sheets, "written": written, "message": format!("Sheet {key}: {status}{}", if written.is_empty() { String::new() } else { format!(" — snapshot {}", written.join(", ")) }) }),
    )
}

/// A file name not used yet (adds -2, -3… before the extension).
#[cfg(not(target_arch = "wasm32"))]
fn unique(name: &str) -> String {
    if !std::path::Path::new(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    (2..1000).map(|k| format!("{stem}-{k}.{ext}")).find(|n| !std::path::Path::new(n).exists()).unwrap_or_else(|| name.to_string())
}

fn run_field(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fafield";
    let mut st = load_state(s);
    if st.devices.is_empty() {
        return Err(bad(CMD, "run FAPOINTS (Find Devices and Rooms) first"));
    }
    let idx = picked(s, &st, p);
    if idx.is_empty() {
        return Err(bad(CMD, "pick the devices (their highlighted address, or `ids`)"));
    }
    let clear = p.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let change = str_param(p, "change").map(|v| v.trim().to_ascii_lowercase()).unwrap_or_default();
    if !clear && !matches!(change.as_str(), "moved" | "swapped" | "dual address" | "removed" | "other") {
        return Err(bad(CMD, "`change` must be moved, swapped, dual address, removed or other"));
    }
    let text = str_param(p, "text").map(|v| v.trim().to_string()).unwrap_or_default();
    let mut log = Vec::new();
    for i in &idx {
        let dv = st.devices[*i].clone();
        let k = note_key(&dv);
        if clear {
            st.job.field.remove(&k);
            log.push(("field cleared".to_string(), dv.address.clone(), String::new(), String::new()));
        } else {
            st.job.field.entry(k).or_default().push(FieldChange { change: change.clone(), text: text.clone(), date: stamp(), by: user() });
            log.push((format!("field {change}"), dv.address.clone(), String::new(), text.clone()));
        }
    }
    audit(&st, &log);
    save_job(&st)?;
    s.fa = Some(st);
    let _ = run_points(s, &json!({}))?;
    Ok(
        json!({ "changed": idx.len(), "message": format!("{} device(s): field change {}", idx.len(), if clear { "cleared".to_string() } else { change }) }),
    )
}

/// One searchable thing in a drawing set.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetEntry {
    pub file: String,
    pub page: usize,
    pub sheet: String,
    /// address | room | equipment | text
    pub kind: String,
    pub value: String,
    pub at: [f64; 2],
}

fn run_index(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "faindex";
    let folder = str_param(p, "folder").ok_or_else(|| bad(CMD, "`folder` is required"))?.to_string();
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (s, folder);
        Err(bad(CMD, "indexing a folder needs the desktop app"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let io = super::file::io().ok_or_else(|| bad(CMD, "file formats are not available in this build"))?;
        let job = load_state(s).job;
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&folder)
            .map_err(|e| bad(CMD, format!("{folder}: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|x| x.to_str()).is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "pdf" | "dxf" | "dwg")))
            .collect();
        files.sort();
        files.truncate(500);
        let mut entries: Vec<SetEntry> = Vec::new();
        let mut failed = Vec::new();
        for f in &files {
            let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            let Ok(bytes) = std::fs::read(f) else {
                failed.push(name);
                continue;
            };
            let Ok(d) = (io.read)(&bytes, &name) else {
                failed.push(name);
                continue;
            };
            let sc = scan(&d, &job.boundary_layer);
            let ext = d.extents(&Space::Model);
            let pages = if sc.pages.is_empty() && !ext.is_empty() { vec![[ext.min.x, ext.min.y, ext.max.x, ext.max.y]] } else { sc.pages.clone() };
            let titles: Vec<String> = (1..=pages.len().max(1)).map(|pg| title_fields(&sc.texts, &pages, pg).0).collect();
            let mut partial = Vec::new();
            let addrs = address_texts(&sc.texts, &job.address_layers, &mut partial);
            let used: HashSet<usize> = addrs.iter().flat_map(|a| a.pieces.iter().copied()).collect();
            let rooms = find_rooms(&job, &sc);
            let mut push = |kind: &str, value: String, at: Vec2| {
                let page = page_of(&pages, at);
                entries.push(SetEntry {
                    file: f.to_string_lossy().to_string(),
                    page,
                    sheet: titles.get(page - 1).cloned().unwrap_or_default(),
                    kind: kind.into(),
                    value,
                    at: [at.x, at.y],
                });
            };
            for a in &addrs {
                push("address", drawn(&a.raw), a.at);
            }
            for r in &rooms {
                push("room", if r.name.is_empty() { r.num.clone() } else { format!("{} ({})", r.name, r.num) }, r.at);
            }
            for (i, t) in sc.texts.iter().enumerate() {
                if used.contains(&i) || t.value.trim().len() < 2 {
                    continue;
                }
                push(if is_tag(&t.value) { "equipment" } else { "text" }, t.value.trim().chars().take(120).collect(), t.center());
            }
            if entries.len() > 2_000_000 {
                break;
            }
        }
        let out = std::path::Path::new(&folder).join(".fa_set_index.json");
        let text = serde_json::to_string(&json!({ "folder": folder, "built": stamp(), "entries": entries })).map_err(|e| bad(CMD, e.to_string()))?;
        std::fs::write(&out, text).map_err(|e| bad(CMD, format!("{}: {e}", out.display())))?;
        let mut st = load_state(s);
        st.job.extra.insert("setFolder".into(), json!(folder));
        s.fa = Some(st);
        Ok(json!({
            "files": files.len(), "entries": entries.len(), "failed": failed,
            "message": format!("Indexed {} file(s), {} entries{}", files.len(), entries.len(), if failed.is_empty() { String::new() } else { format!("; couldn't read {}", failed.join(", ")) }),
        }))
    }
}

fn run_search(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fasearch";
    let text = str_param(p, "text").map(|t| t.trim().to_ascii_uppercase()).filter(|t| !t.is_empty()).ok_or_else(|| bad(CMD, "`text` is required"))?;
    let folder =
        str_param(p, "folder").map(String::from).or_else(|| load_state(s).job.extra.get("setFolder").and_then(Value::as_str).map(String::from));
    let folder = folder.ok_or_else(|| bad(CMD, "index a drawing set first (FAINDEX), or give `folder`"))?;
    let path = std::path::Path::new(&folder).join(".fa_set_index.json");
    let bytes = read_path(CMD, &path.to_string_lossy())?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| bad(CMD, format!("{}: {e}", path.display())))?;
    let squash = |x: &str| x.to_ascii_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>();
    let want = squash(&text);
    let mut hits: Vec<Value> = v["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| {
                    let val = e["value"].as_str().unwrap_or("");
                    val.to_ascii_uppercase().contains(&text) || (!want.is_empty() && squash(val).contains(&want))
                })
                .take(500)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // Addresses first, then rooms, equipment, other text.
    let rank = |e: &Value| match e["kind"].as_str().unwrap_or("") {
        "address" => 0,
        "room" => 1,
        "equipment" => 2,
        _ => 3,
    };
    hits.sort_by_key(rank);
    let mut message = format!("{} hit(s) for {text}", hits.len());
    for h in hits.iter().take(15) {
        let file = std::path::Path::new(h["file"].as_str().unwrap_or("")).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        message.push_str(&format!(
            "\n  {} {} — {} sheet {}",
            h["kind"].as_str().unwrap_or(""),
            h["value"].as_str().unwrap_or(""),
            file,
            h["sheet"].as_str().unwrap_or("")
        ));
    }
    Ok(json!({ "hits": hits, "message": message }))
}

fn run_note(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "fapointnote";
    let mut st = load_state(s);
    if st.devices.is_empty() {
        return Err(bad(CMD, "run FAPOINTS (Find Devices and Rooms) first"));
    }
    let note = str_param(p, "note").ok_or_else(|| bad(CMD, "`note` is required (empty text clears it)"))?.trim().to_string();
    let idx = picked(s, &st, p);
    if idx.is_empty() {
        return Err(bad(CMD, "select the devices' highlighted address text (layer FA-POINTLIST) first"));
    }
    for i in &idx {
        let dv = &st.devices[*i];
        let k = if dv.key.is_empty() { dv.id.clone() } else { dv.key.clone() };
        if note.is_empty() {
            st.job.notes.remove(&k);
        } else {
            st.job.notes.insert(k, note.clone());
        }
    }
    save_job(&st)?;
    s.fa = Some(st);
    let _ = run_points(s, &json!({}))?;
    Ok(json!({ "noted": idx.len(), "message": format!("Note {} on {} device(s)", if note.is_empty() { "cleared" } else { "saved" }, idx.len()) }))
}

fn run_accept_greens(s: &mut Session, p: &Value) -> Result<Value> {
    let st = load_state(s);
    let page = p.get("page").and_then(Value::as_u64).map(|v| v as usize).unwrap_or_else(|| page_of(&st.pages, s.cursor));
    run_accept(s, &json!({ "allGreen": true, "page": page }))
}

fn run_review(s: &mut Session, _p: &Value) -> Result<Value> {
    // A toggle: show only what needs review, or everything again.
    let now = s.fa.as_ref().is_some_and(|st| st.only_problems);
    run_points(s, &json!({ "onlyProblems": !now }))
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
    let mut log = Vec::new();
    for i in &idx {
        let dv = st.devices[*i].clone();
        st.job.decisions.entry(dv.id.clone()).or_default().accepted = !undo;
        log.push((
            if undo { "un-accept" } else { "accept" }.to_string(),
            dv.address.clone(),
            dv.status.clone(),
            if undo { String::new() } else { "checked".into() },
        ));
    }
    audit(&st, &log);
    save_job(&st)?;
    s.fa = Some(st);
    let r = run_points(s, &json!({}))?;
    Ok(
        json!({ "accepted": idx.len(), "counts": r["counts"], "message": format!("{} device(s) {}", idx.len(), if undo { "un-accepted" } else { "accepted (checked)" }) }),
    )
}

// ---------- export ----------

const HEAD: [&str; 17] = [
    "Address",
    "Node",
    "Loop",
    "D/M",
    "Number",
    "Label",
    "Type",
    "Function",
    "Interface Equipment",
    "Room #",
    "Room",
    "Floor",
    "Sheet #",
    "Drawing No",
    "Zone",
    "Comments",
    "Status",
];
const C_ADDR: usize = 0;
const C_LABEL: usize = 5;
const C_COMMENTS: usize = 15;
const C_STATUS: usize = 16;
const WIDTHS: [f64; 17] = [12.0, 6.0, 6.0, 5.0, 8.0, 40.0, 10.0, 22.0, 26.0, 12.0, 26.0, 8.0, 8.0, 18.0, 8.0, 50.0, 10.0];

/// The Comments cell: the owner's note first, then the app's concerns after "APP:".
fn comments(note: &str, flags: &[String]) -> String {
    let app: Vec<&str> = flags.iter().map(String::as_str).collect();
    match (note.trim().is_empty(), app.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("APP: {}", app.join("; ")),
        (false, true) => note.trim().to_string(),
        (false, false) => format!("{} | APP: {}", note.trim(), app.join("; ")),
    }
}

/// The owner's part of a Comments cell (what's before "APP:").
fn owner_part(cell: &str) -> String {
    let c = cell.trim();
    match c.find("APP:") {
        Some(i) => c[..i].trim().trim_end_matches('|').trim().to_string(),
        None => c.to_string(),
    }
}

fn device_row(st: &FaState, dv: &Device) -> Vec<String> {
    let pt = st.job.points.get(&dv.key);
    vec![
        if dv.address.is_empty() { dv.key.clone() } else { dv.address.clone() },
        dv.node.map(|n| n.to_string()).unwrap_or_default(),
        dv.loop_.map(|n| n.to_string()).unwrap_or_default(),
        dv.kind.clone(),
        dv.number.map(|n| n.to_string()).unwrap_or_default(),
        dv.label.clone(),
        dv.dtype.clone(),
        dv.function.clone(),
        dv.equipment.clone(),
        dv.room_num.clone(),
        dv.room.clone(),
        st.job.floor.clone(),
        if dv.sheet.is_empty() { dv.page.to_string() } else { dv.sheet.clone() },
        dv.drawing_no.clone(),
        pt.map(|p| p.zone.clone()).unwrap_or_default(),
        comments(&dv.note, &dv.flags),
        dv.status.to_uppercase(),
    ]
}

/// A row for an address with no device on the drawing: in the panel report (not placed), or a
/// blank gap-fill spacer.
fn missing_row(st: &FaState, shown: &str, a: &Addr, key: &str) -> (Vec<String>, Vec<&'static str>, bool) {
    let mut row = vec![String::new(); HEAD.len()];
    let mut sty = vec!["plain"; HEAD.len()];
    row[C_ADDR] = shown.to_string();
    match st.job.points.get(key) {
        Some(pt) => {
            row[1] = a.node.map(|n| n.to_string()).unwrap_or_default();
            row[2] = a.loop_.to_string();
            row[3] = a.kind.to_string();
            row[4] = a.num.to_string();
            row[C_LABEL] = pt.label.clone();
            row[6] = pt.dtype.clone();
            row[10] = pt.room.clone();
            row[11] = st.job.floor.clone();
            row[14] = pt.zone.clone();
            row[C_COMMENTS] =
                comments(st.job.notes.get(key).map(String::as_str).unwrap_or(""), &["in the panel report, not found on the drawing".to_string()]);
            row[C_STATUS] = "NOT PLACED".into();
            sty[C_STATUS] = "amber";
            if pt.label.chars().count() > MAX_LABEL {
                sty[C_LABEL] = "red";
            }
            (row, sty, true)
        }
        None => {
            sty[C_ADDR] = "spacer";
            (row, sty, false)
        }
    }
}

/// Whether a loop's addresses are drawn with zero-padded numbers (N1L1M01): gap rows follow suit.
fn padded(st: &FaState, n: u32, l: u32) -> bool {
    st.devices.iter().filter(|d| d.node == Some(n) && d.loop_ == Some(l)).any(|d| {
        let digits: String = d.address.chars().rev().take_while(char::is_ascii_digit).collect();
        digits.len() >= 2 && digits.ends_with('0')
    })
}

fn loops_of(st: &FaState) -> Vec<(u32, u32)> {
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
    loops.into_keys().collect()
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

fn styled(st: &FaState, dv: &Device) -> (Vec<String>, Vec<&'static str>) {
    let mut sty = vec!["plain"; HEAD.len()];
    sty[C_STATUS] = style_of(&dv.status);
    if dv.label.chars().count() > MAX_LABEL {
        sty[C_LABEL] = "red";
    }
    if dv.flags.iter().any(|f| f.starts_with("duplicate")) {
        sty[C_ADDR] = "red";
    }
    (device_row(st, dv), sty)
}

fn workbook(st: &FaState) -> Value {
    let mut sheets: Vec<Value> = Vec::new();
    let mut summary: Vec<(Vec<String>, Vec<&str>)> = Vec::new();
    // Node + loop tabs, gap-filled 1–159, detectors then modules.
    for (n, l) in loops_of(st) {
        let pad = padded(st, n, l);
        let mut rows: Vec<(Vec<String>, Vec<&str>)> = Vec::new();
        for kind in ['D', 'M'] {
            let mut cnt = [0usize; 5];
            for a in 1..=MAX_ADDR {
                let addr = Addr { node: Some(n), loop_: l, kind, num: a };
                let key = addr.key();
                let here: Vec<&Device> = st.devices.iter().filter(|d| d.key == key).collect();
                if here.is_empty() {
                    let shown = if pad { format!("N{n}L{l}{kind}{a:02}") } else { key.clone() };
                    let (row, sty, placed) = missing_row(st, &shown, &addr, &key);
                    if placed {
                        cnt[4] += 1;
                    }
                    rows.push((row, sty));
                    continue;
                }
                for dv in here {
                    rows.push(styled(st, dv));
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
        sheets.push(sheet(&format!("N{n} L{l}"), &HEAD, rows, &WIDTHS));
    }
    let mut all = vec![sheet(
        "Summary",
        &["Node", "Loop", "Kind", "Found", "Green", "Amber", "Red", "Checked", "Not placed"],
        summary,
        &[8.0, 6.0, 11.0, 8.0, 8.0, 8.0, 8.0, 9.0, 11.0],
    )];
    all.extend(sheets);
    // Every device found (with or without an address).
    let devs: Vec<(Vec<String>, Vec<&str>)> = st.devices.iter().map(|dv| styled(st, dv)).collect();
    all.push(sheet("Devices", &HEAD, devs, &WIDTHS));
    // Count by room × type.
    let mut cnt: BTreeMap<(String, String), usize> = BTreeMap::new();
    for dv in &st.devices {
        let room = room_cell(dv);
        let t = if dv.function.is_empty() { if dv.dtype.is_empty() { "Device".to_string() } else { dv.dtype.clone() } } else { dv.function.clone() };
        *cnt.entry((if room.is_empty() { "(no room)".into() } else { room }, t)).or_default() += 1;
    }
    let crows = cnt.into_iter().map(|((r, t), n)| (vec![r, t, n.to_string()], vec!["plain"; 3])).collect();
    all.push(sheet("Device count", &["Room", "Type / Function", "Count"], crows, &[34.0, 24.0, 8.0]));
    // Unresolved: everything not green or checked, and report points not on the drawing.
    let mut un: Vec<(Vec<String>, Vec<&str>)> =
        st.devices.iter().filter(|d| d.status != "green" && d.status != "checked").map(|dv| styled(st, dv)).collect();
    for k in not_placed(st) {
        if let Some(a) = parse_addr(&k) {
            let (row, sty, _) = missing_row(st, &k, &a, &k);
            un.push((row, sty));
        }
    }
    all.push(sheet("Unresolved", &HEAD, un, &WIDTHS));
    // Field changes, for the as-built record.
    let mut ab: Vec<(Vec<String>, Vec<&str>)> = Vec::new();
    for (k, list) in &st.job.field {
        let dv = st.devices.iter().find(|d| note_key(d) == *k);
        for c in list {
            ab.push((
                vec![
                    dv.map(|d| d.address.clone()).unwrap_or_else(|| k.clone()),
                    c.change.to_ascii_uppercase(),
                    c.text.clone(),
                    dv.map(room_cell).unwrap_or_default(),
                    dv.map(|d| d.sheet.clone()).unwrap_or_default(),
                    c.date.clone(),
                    c.by.clone(),
                ],
                vec!["plain"; 7],
            ));
        }
    }
    all.push(sheet("As-built", &["Address", "Change", "Details", "Room", "Sheet #", "Date", "By"], ab, &[14.0, 14.0, 40.0, 30.0, 8.0, 16.0, 10.0]));
    // Sheet status.
    let ss: Vec<(Vec<String>, Vec<&str>)> = st
        .job
        .sheets
        .iter()
        .map(|(k, v)| (vec![k.clone(), v.status.to_ascii_uppercase(), v.date.clone(), v.initials.clone(), v.history.join(" | ")], vec!["plain"; 5]))
        .collect();
    all.push(sheet("Sheet status", &["Sheet #", "Status", "Date", "Initials", "History"], ss, &[10.0, 14.0, 18.0, 10.0, 80.0]));
    Value::Array(all)
}

/// The room as the run sheet writes it: `NAME (NUMBER)`.
fn room_cell(dv: &Device) -> String {
    match (dv.room.trim().is_empty(), dv.room_num.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => dv.room_num.trim().to_string(),
        (false, true) => dv.room.trim().to_string(),
        (false, false) => format!("{} ({})", dv.room.trim(), dv.room_num.trim()),
    }
}

/// The run sheet's address style: N05-L1-D-01.
fn runsheet_address(n: u32, l: u32, kind: char, a: u32) -> String {
    format!("N{n:02}-L{l}-{kind}-{a:02}")
}

const RS_HEAD: [&str; 6] = ["DEVICE ADDRESS", "DEVICE TYPE", "FUNCTION", "ROOM #", "INTERFACE EQUIPMENT", "COMMENTS"];
const RS_WIDTHS: [f64; 6] = [16.0, 12.0, 26.0, 40.0, 30.0, 60.0];

/// The owner's run sheet layout: a tab per loop (per node and loop when there are several
/// nodes), every address 1–159 (detectors, then modules), and the concerns in COMMENTS.
fn runsheet_workbook(st: &FaState) -> Value {
    let loops = loops_of(st);
    let nodes: HashSet<u32> = loops.iter().map(|(n, _)| *n).collect();
    let mut all: Vec<Value> = Vec::new();
    for (n, l) in &loops {
        let mut rows: Vec<(Vec<String>, Vec<&str>)> = Vec::new();
        for kind in ['D', 'M'] {
            for a in 1..=MAX_ADDR {
                let key = Addr { node: Some(*n), loop_: *l, kind, num: a }.key();
                let shown = runsheet_address(*n, *l, kind, a);
                let here: Vec<&Device> = st.devices.iter().filter(|d| d.key == key).collect();
                if here.is_empty() {
                    let mut row = vec![shown, String::new(), String::new(), String::new(), String::new(), String::new()];
                    let mut sty = vec!["plain"; 6];
                    match st.job.points.get(&key) {
                        Some(pt) => {
                            row[3] = pt.room.clone();
                            row[5] = comments(
                                st.job.notes.get(&key).map(String::as_str).unwrap_or(""),
                                &["in the panel report, not found on the drawing".to_string()],
                            );
                            sty[5] = "amber";
                        }
                        None => {
                            if let Some(note) = st.job.notes.get(&key) {
                                row[5] = note.clone();
                            }
                            sty[0] = "spacer";
                        }
                    }
                    rows.push((row, sty));
                    continue;
                }
                for dv in here {
                    let mut sty = vec!["plain"; 6];
                    sty[5] = match dv.status.as_str() {
                        "green" | "checked" => "plain",
                        "amber" => "amber",
                        _ => "red",
                    };
                    if dv.flags.iter().any(|f| f.starts_with("duplicate")) {
                        sty[0] = "red";
                    }
                    rows.push((
                        vec![
                            shown.clone(),
                            dv.dtype.clone(),
                            dv.function.clone(),
                            room_cell(dv),
                            dv.equipment.clone(),
                            comments(&dv.note, &dv.flags),
                        ],
                        sty,
                    ));
                }
            }
        }
        let name = if nodes.len() > 1 { format!("N{n} Loop {l}") } else { format!("Loop {l}") };
        all.push(sheet(&name, &RS_HEAD, rows, &RS_WIDTHS));
    }
    // Devices whose address couldn't be read: listed so nothing is dropped.
    let lost: Vec<(Vec<String>, Vec<&str>)> = st
        .devices
        .iter()
        .filter(|d| d.key.is_empty())
        .map(|dv| {
            let mut sty = vec!["plain"; 6];
            sty[5] = "red";
            (vec![dv.address.clone(), dv.dtype.clone(), dv.function.clone(), room_cell(dv), dv.equipment.clone(), comments(&dv.note, &dv.flags)], sty)
        })
        .collect();
    if !lost.is_empty() {
        all.push(sheet("No address", &RS_HEAD, lost, &RS_WIDTHS));
    }
    Value::Array(all)
}

/// The owner's notes in an earlier export (point list or run sheet): kept, keyed by address.
fn read_owner_notes(bytes: &[u8], name: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(hooks) = SHEETS.get() else { return out };
    let Ok(grids) = (hooks.read)(bytes, name) else { return out };
    for (_, rows) in grids {
        let Some(head) = rows.first() else { continue };
        let up: Vec<String> = head.iter().map(|h| h.trim().to_ascii_uppercase()).collect();
        let (Some(ca), Some(cc)) = (up.iter().position(|h| h == "DEVICE ADDRESS" || h == "ADDRESS"), up.iter().position(|h| h == "COMMENTS")) else {
            continue;
        };
        for r in rows.iter().skip(1) {
            let (Some(a), Some(c)) = (r.get(ca), r.get(cc)) else { continue };
            let Some(addr) = parse_addr(a) else { continue };
            let owner = owner_part(c);
            if !owner.is_empty() {
                out.insert(addr.key(), owner);
            }
        }
    }
    out
}

fn workbench(st: &FaState) -> Value {
    let frame = |page: usize| st.pages.get(page.saturating_sub(1)).copied().unwrap_or([0.0, 0.0, 1.0, 1.0]);
    let points: Vec<Value> = st
        .devices
        .iter()
        .filter(|d| !d.key.is_empty())
        .map(|dv| {
            let f = frame(dv.page);
            let (w, h) = ((f[2] - f[0]).max(1e-9), (f[3] - f[1]).max(1e-9));
            let x = ((dv.at[0] - f[0]) / w * 10_000.0).round() / 10_000.0;
            // Fractions from the top-left, as the FA Drawings pages are.
            let y = ((f[3] - dv.at[1]) / h * 10_000.0).round() / 10_000.0;
            let status = if st.job.points.contains_key(&dv.key) { "existing" } else { "new" };
            json!({
                "key": dv.key, "node": dv.node, "loop": dv.loop_, "kind": dv.kind, "addr": dv.number,
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
    let lower = path.to_ascii_lowercase();
    let runsheet = str_param(p, "format").is_some_and(|f| f.eq_ignore_ascii_case("runsheet"));
    // Exporting over an earlier export: the owner's comments in it are kept.
    let mut kept = 0usize;
    #[cfg(not(target_arch = "wasm32"))]
    if (lower.ends_with(".xlsx") || lower.ends_with(".csv"))
        && let Ok(old) = std::fs::read(&path)
    {
        for (k, v) in read_owner_notes(&old, &path) {
            if st.job.notes.get(&k) != Some(&v) {
                st.job.notes.insert(k, v);
                kept += 1;
            }
        }
        if kept > 0 {
            save_job(&st)?;
        }
    }
    extract(s, &mut st)?;
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
        let book = if runsheet { runsheet_workbook(&st) } else { workbook(&st) };
        let bytes = (hooks.write)(&book).map_err(|e| bad(CMD, e))?;
        written.push((path.clone(), bytes));
        if !runsheet && p.get("workbench").and_then(Value::as_bool).unwrap_or(true) {
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
        "Wrote {}{} ({} device(s): {} green, {} amber, {} red, {} checked)",
        names.join(", "),
        if kept > 0 { format!("; kept {kept} of your comments from the earlier file") } else { String::new() },
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
        let a: Vec<&str> = r["devices"].as_array().map(|v| v.iter().filter_map(|d| d["key"].as_str()).collect()).unwrap_or_default();
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
    fn runsheet_layout_function_and_title_block() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        text(&mut s, "B-101", 0.0, 0.0, 0.2);
        text(&mut s, "STORAGE", -0.1, 0.3, 0.15);
        text(&mut s, "N3L1M07", 1.0, 1.0, 0.1);
        text(&mut s, "WF", 1.0, 1.25, 0.1);
        text(&mut s, "SAMPLE-SP-01", 1.0, 0.85, 0.1);
        text(&mut s, "SHEET #:", 20.0, 0.5, 0.1);
        text(&mut s, "12", 20.8, 0.5, 0.1);
        let _ = s.execute("fajob", &json!({ "reset": true }));
        let r = s.execute("fapoints", &json!({ "mark": false })).unwrap_or_default();
        let d = r["devices"][0].clone();
        assert_eq!(d["address"], "N3L1M07", "as drawn: {r}");
        assert_eq!(d["key"], "N3L1M7", "{r}");
        assert_eq!(d["function"], "WATERFLOW", "{r}");
        assert_eq!(d["equipment"], "SAMPLE-SP-01", "{r}");
        assert_eq!(d["type"], "MODULE", "{r}");
        assert_eq!(d["sheet"], "12", "{r}");
        let st = s.fa.clone().unwrap_or_default();
        let wb = runsheet_workbook(&st);
        let tab = wb[0].clone();
        assert_eq!(tab["name"], "Loop 1");
        assert_eq!(tab["rows"][0], json!(["DEVICE ADDRESS", "DEVICE TYPE", "FUNCTION", "ROOM #", "INTERFACE EQUIPMENT", "COMMENTS"]));
        // Header + 159 detectors + 159 modules; M07 is row 1 + 159 + 7.
        assert_eq!(tab["rows"].as_array().map(Vec::len), Some(1 + 159 + 159));
        let m7 = tab["rows"][159 + 7].clone();
        assert_eq!(m7[0], "N03-L1-M-07");
        assert_eq!(m7[3], "STORAGE (B-101)");
    }

    #[test]
    fn owner_comments_survive_a_new_export() {
        assert_eq!(owner_part("check in field | APP: room close call"), "check in field");
        assert_eq!(owner_part("APP: no room found"), "");
        assert_eq!(comments("mine", &["dup".into()]), "mine | APP: dup");
    }

    #[test]
    fn walls_decide_the_room_and_field_changes_show() {
        let mut s = Session::new();
        let _ = s.execute("new", &json!({}));
        let line = |s: &mut Session, a: (f64, f64), b: (f64, f64)| {
            let _ = s.add_entity(EntityKind::Line(cadcraft_doc::Line { a: Vec3::new(a.0, a.1, 0.0), b: Vec3::new(b.0, b.1, 0.0) }));
        };
        // Two closed rooms side by side, sharing the wall at x = 10.
        for (a, b) in [
            ((0.0, 0.0), (20.0, 0.0)),
            ((20.0, 0.0), (20.0, 10.0)),
            ((20.0, 10.0), (0.0, 10.0)),
            ((0.0, 10.0), (0.0, 0.0)),
            ((10.0, 0.0), (10.0, 10.0)),
        ] {
            line(&mut s, a, b);
        }
        // Something far away so the rooms are a small part of the page.
        line(&mut s, (0.0, 60.0), (0.1, 60.0));
        line(&mut s, (80.0, 0.0), (80.1, 0.0));
        text(&mut s, "B-101", 2.0, 5.0, 0.3);
        text(&mut s, "B-102", 10.6, 5.0, 0.3);
        // In room B-101, but nearer to B-102's number.
        text(&mut s, "N1L1D07", 8.0, 5.0, 0.2);
        let _ = s.execute("fajob", &json!({ "reset": true }));
        let r = s.execute("fapoints", &json!({ "mark": false })).unwrap_or_default();
        assert_eq!(r["devices"][0]["roomNum"], "B-101", "the wall wins over distance: {r}");
        let id = r["devices"][0]["id"].as_str().unwrap_or("").to_string();
        let f = s.execute("fafield", &json!({ "ids": [id], "change": "moved", "text": "to the corridor" })).unwrap_or_default();
        assert_eq!(f["changed"], 1, "{f}");
        let st = s.fa.clone().unwrap_or_default();
        assert!(st.devices[0].flags.iter().any(|x| x.starts_with("FIELD moved")), "{:?}", st.devices[0].flags);
        assert_eq!(st.devices[0].status, "green", "a field note doesn't change the status");
        let sh = s.execute("fasheet", &json!({ "page": 1, "status": "done", "initials": "AB" })).unwrap_or_default();
        assert!(sh["message"].as_str().unwrap_or("").contains("done"), "{sh}");
        let sh = s.execute("fasheet", &json!({ "page": 1, "status": "in progress", "initials": "AB" })).unwrap_or_default();
        assert!(sh["sheets"].to_string().contains("reopened"), "reopening is logged: {sh}");
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
        let m1 = devs.iter().find(|d| d["key"] == "N1L1M1").cloned().unwrap_or_default();
        assert_eq!(m1["roomNum"], "B-101", "{r}");
        assert_eq!(m1["room"], "STORAGE", "{r}");
        assert_eq!(m1["status"], "green", "{r}");
        let d5: Vec<&Value> = devs.iter().filter(|d| d["key"] == "N1L1D5").collect();
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
        let m = st.devices.iter().find(|d| d.key == "N1L1M1").map(|d| d.markers.clone()).unwrap_or_default();
        let hs: Vec<String> = m.iter().map(|h| h.hex()).collect();
        let r = s.execute("fapointroom", &json!({ "room": "B-102", "handles": hs })).unwrap_or_default();
        assert_eq!(r["assigned"], 1, "{r}");
        let r = s.execute("fapoints", &json!({})).unwrap_or_default();
        let m1 = r["devices"].as_array().and_then(|a| a.iter().find(|d| d["key"] == "N1L1M1").cloned()).unwrap_or_default();
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
        assert_eq!(rows[1][0], "N1L1D01", "gap rows follow the drawing's zero padding");
        assert_eq!(rows[1][5], "", "spacer rows are blank after the address");
    }
}
