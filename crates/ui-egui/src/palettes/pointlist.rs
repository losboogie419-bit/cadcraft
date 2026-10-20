//! The Point List review panel: every device the point list will export, with its status,
//! function, equipment, room and comments. Click a row to zoom to the device and light up its
//! room; edit cells in place; filter to what needs review; accept a sheet; pick a room on the
//! drawing for a device; export.

use cadcraft_engine::cmd::fapoints::{Device, FaState};
use egui::{Color32, RichText, vec2};
use serde_json::{Value, json};

use crate::CadApp;
use crate::theme::Tokens;

/// Panel state kept between frames.
#[derive(Clone, Default)]
pub struct State {
    pub filter: String,
    pub status: Status,
    pub only_concerns: bool,
    /// The device the panel points at (row clicked): its id.
    pub focus: Option<String>,
    /// Waiting for a room to be clicked on the drawing for these devices.
    pub picking_room: Vec<String>,
    /// Cell being edited: (device id, column, text).
    pub editing: Option<(String, Col, String)>,
    /// The edit box was just opened: give it the keyboard once.
    pub edit_new: bool,
    /// Until when (ms) to flash missed address text on the drawing.
    pub missed_until: f64,
    /// Selection seen last frame (to notice a room picked on the drawing).
    last_sel: Vec<u64>,
    /// Initials for sheet status.
    pub initials: String,
    /// Field change note being typed for the focused device.
    pub field_text: String,
    /// Drawing set search.
    pub set_query: String,
    pub set_hits: Vec<Value>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Status {
    #[default]
    All,
    Green,
    Amber,
    Red,
    Checked,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Col {
    Function,
    Equipment,
    Room,
    Note,
}

pub fn id() -> egui::Id {
    egui::Id::new("cadcraft-pointlist-panel")
}

pub fn state(ctx: &egui::Context) -> State {
    ctx.data_mut(|d| d.get_temp(id())).unwrap_or_default()
}

pub fn set_state(ctx: &egui::Context, st: State) {
    ctx.data_mut(|d| d.insert_temp(id(), st));
}

/// Open the panel, docked under the drawing the first time.
pub fn open(app: &mut CadApp) -> Result<Value, String> {
    let panel = crate::docking::Panel::PointList;
    let first = !app.ui.docking.layout.contains(&panel) && !app.ui.docking.hidden.iter().any(|(p, _)| *p == panel);
    let r = crate::docking::command(app, false, &json!({ "operation": "open", "panel": panel.id() }))?;
    if first {
        let _ = crate::docking::command(app, false, &json!({ "operation": "move", "panel": panel.id(), "target": "canvas", "zone": "bottom" }));
    }
    Ok(r)
}

pub fn status_color(s: &str) -> Color32 {
    match s {
        "green" => Color32::from_rgb(0x3E, 0xCF, 0x6E),
        "amber" => Color32::from_rgb(0xFF, 0xB0, 0x00),
        "checked" => Color32::from_rgb(0x4A, 0xA8, 0xFF),
        _ => Color32::from_rgb(0xEF, 0x44, 0x44),
    }
}

fn room_text(d: &Device) -> String {
    match (d.room.trim().is_empty(), d.room_num.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => d.room_num.clone(),
        (false, true) => d.room.clone(),
        (false, false) => format!("{} ({})", d.room, d.room_num),
    }
}

fn comments(d: &Device) -> String {
    let app = d.flags.join("; ");
    match (d.note.is_empty(), app.is_empty()) {
        (true, _) => app,
        (false, true) => d.note.clone(),
        (false, false) => format!("{} | APP: {app}", d.note),
    }
}

fn shows(st: &State, d: &Device) -> bool {
    let status_ok = match st.status {
        Status::All => true,
        Status::Green => d.status == "green",
        Status::Amber => d.status == "amber",
        Status::Red => d.status == "red",
        Status::Checked => d.status == "checked",
    };
    let concern_ok = !st.only_concerns || (d.status != "green" && d.status != "checked") || !d.flags.is_empty();
    let f = st.filter.trim().to_ascii_uppercase();
    let text_ok = f.is_empty()
        || [&d.address, &d.function, &d.equipment, &d.room, &d.room_num, &d.sheet, &d.note].iter().any(|v| v.to_ascii_uppercase().contains(&f))
        || d.flags.iter().any(|v| v.to_ascii_uppercase().contains(&f));
    status_ok && concern_ok && text_ok
}

/// Zoom the drawing to a device (and its room when they're near).
pub fn zoom_to(app: &mut CadApp, d: &Device) {
    let mut b = d.bbox;
    if let Some(r) = d.room_bbox {
        let joined = [b[0].min(r[0]), b[1].min(r[1]), b[2].max(r[2]), b[3].max(r[3])];
        if (joined[2] - joined[0]).max(joined[3] - joined[1]) < 12.0 * (b[2] - b[0]).max(b[3] - b[1]).max(0.1) {
            b = joined;
        }
    }
    let pad = ((b[2] - b[0]).max(b[3] - b[1]) * 1.5).max(0.6);
    let _ = app.run("zoom", json!({ "mode": "window", "p1": [b[0] - pad, b[1] - pad], "p2": [b[2] + pad, b[3] + pad] }));
}

fn sheet_label(d: &Device) -> String {
    if d.sheet.is_empty() { format!("Page {}", d.page) } else { format!("Sheet {}", d.sheet) }
}

pub fn show(app: &mut CadApp, ui: &mut egui::Ui) {
    let t = Tokens::get();
    let ctx = ui.ctx().clone();
    let mut st = state(&ctx);
    let fa: Option<FaState> = app.session.fa.clone();
    let devices: Vec<Device> = fa.as_ref().map(|f| f.devices.clone()).unwrap_or_default();

    // A room picked on the drawing for the waiting devices: the first text newly selected.
    let sel: Vec<u64> = app.session.selection().iter().map(|h| h.0).collect();
    if !st.picking_room.is_empty() && sel != st.last_sel && !sel.is_empty() {
        let ids = std::mem::take(&mut st.picking_room);
        let r = app.run("fapointroom", json!({ "ids": ids }));
        if let Err(e) = r {
            app.set_status(e);
        }
    }
    st.last_sel = sel;

    ui.add_space(4.0);
    // Toolbar.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 4.0);
        if ui.button(crate::tl!("Find devices and rooms")).clicked()
            && let Err(e) = app.run("fapoints", json!({}))
        {
            app.set_status(e);
        }
        let only = fa.as_ref().is_some_and(|f| f.only_problems);
        if ui.selectable_label(only, crate::tl!("Show only what needs me")).clicked() {
            let _ = app.run("fapointsreview", json!({}));
        }
        if ui.button(crate::tl!("Show missed")).on_hover_text(crate::tl!("Flash address-like text that wasn't read as a device")).clicked() {
            st.missed_until = crate::now_ms() + 4000.0;
        }
        ui.separator();
        ui.label(crate::tl!("Status"));
        egui::ComboBox::from_id_salt("pl-status").selected_text(format!("{:?}", st.status)).show_ui(ui, |ui| {
            for s in [Status::All, Status::Green, Status::Amber, Status::Red, Status::Checked] {
                ui.selectable_value(&mut st.status, s, format!("{s:?}"));
            }
        });
        ui.checkbox(&mut st.only_concerns, crate::tl!("Only rows with a concern"));
        ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text(crate::tl!("Search")).desired_width(140.0));
        ui.separator();
        if ui.button(crate::tl!("Export point list…")).clicked() {
            let _ = crate::menus::run_ui_command(app, "fapointsexport", &Value::Null);
        }
        if ui.button(crate::tl!("Export run sheet…")).clicked() {
            let _ = crate::menus::run_ui_command(app, "farunsheet", &Value::Null);
        }
        ui.separator();
        ui.label(crate::tl!("Initials"));
        ui.add(egui::TextEdit::singleline(&mut st.initials).desired_width(36.0));
    });
    // Search the whole drawing set.
    ui.horizontal_wrapped(|ui| {
        if ui.button(crate::tl!("Index drawing set…")).on_hover_text(crate::tl!("Pick any drawing in the set's folder")).clicked() {
            let picked = app.services.pick_file.as_ref().and_then(|f| f("Any drawing in the set (PDF, DXF, DWG)", &["pdf", "dxf", "dwg"]));
            if let Some(p) = picked {
                let folder = std::path::Path::new(&p).parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
                match app.run("faindex", json!({ "folder": folder })) {
                    Ok(_) => {}
                    Err(e) => app.set_status(e),
                }
            }
        }
        let r =
            ui.add(egui::TextEdit::singleline(&mut st.set_query).hint_text(crate::tl!("Search the set: address, room, tag")).desired_width(240.0));
        if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || ui.button(crate::tl!("Search")).clicked() {
            match app.run("fasearch", json!({ "text": st.set_query })) {
                Ok(v) => st.set_hits = v["hits"].as_array().cloned().unwrap_or_default(),
                Err(e) => {
                    st.set_hits.clear();
                    app.set_status(e);
                }
            }
        }
        if !st.set_hits.is_empty() && ui.small_button(crate::tl!("Clear results")).clicked() {
            st.set_hits.clear();
        }
    });
    if !st.set_hits.is_empty() {
        egui::ScrollArea::vertical().id_salt("pl-set-hits").max_height(120.0).show(ui, |ui| {
            for h in st.set_hits.clone().iter().take(200) {
                let file = h["file"].as_str().unwrap_or("").to_string();
                let short = std::path::Path::new(&file).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
                let label = format!(
                    "{}  {}  —  {}  sheet {}",
                    h["kind"].as_str().unwrap_or(""),
                    h["value"].as_str().unwrap_or(""),
                    short,
                    h["sheet"].as_str().unwrap_or("")
                );
                if ui.selectable_label(false, label).clicked() {
                    let here = app.session.state().ok().and_then(|d| d.path.clone()).is_some_and(|p| p == file);
                    if !here && let Err(e) = app.run("open", json!({ "path": file })) {
                        app.set_status(e);
                        continue;
                    }
                    let (x, y) = (h["at"][0].as_f64().unwrap_or(0.0), h["at"][1].as_f64().unwrap_or(0.0));
                    let _ = app.run("zoom", json!({ "mode": "window", "p1": [x - 1.5, y - 1.0], "p2": [x + 1.5, y + 1.0] }));
                }
            }
        });
    }

    if devices.is_empty() {
        ui.add_space(8.0);
        ui.label(RichText::new(crate::tl!("No devices yet: open a floor plan and click Find devices and rooms.")).color(t.text_dim));
        set_state(&ctx, st);
        return;
    }

    // Per-sheet counts, with Accept greens on each sheet.
    let mut sheets: Vec<(usize, String)> = Vec::new();
    for d in &devices {
        if !sheets.iter().any(|(p, _)| *p == d.page) {
            sheets.push((d.page, sheet_label(d)));
        }
    }
    sheets.sort();
    ui.horizontal_wrapped(|ui| {
        for (page, label) in &sheets {
            let here: Vec<&Device> = devices.iter().filter(|d| d.page == *page).collect();
            let n = |s: &str| here.iter().filter(|d| d.status == s).count();
            let missed = fa.as_ref().map(|f| f.missed.iter().filter(|m| m.0 == *page).count()).unwrap_or(0);
            ui.label(RichText::new(label).strong());
            ui.label(format!("{} found", here.len()));
            ui.label(RichText::new(format!("{} green", n("green"))).color(status_color("green")));
            ui.label(RichText::new(format!("{} amber", n("amber"))).color(status_color("amber")));
            ui.label(RichText::new(format!("{} red", n("red"))).color(status_color("red")));
            ui.label(RichText::new(format!("{} checked", n("checked"))).color(status_color("checked")));
            if missed > 0 {
                ui.label(RichText::new(format!("{missed} missed")).color(Color32::from_rgb(0xE0, 0x4C, 0xE0)));
            }
            if n("green") > 0 && ui.small_button(crate::tl!("Accept greens")).clicked() {
                let _ = app.run("fapointaccept", json!({ "allGreen": true, "page": page }));
            }
            let key = here.first().map(|d| if d.sheet.is_empty() { format!("page {}", d.page) } else { d.sheet.clone() }).unwrap_or_default();
            let cur = fa.as_ref().and_then(|f| f.job.sheets.get(&key)).cloned().unwrap_or_default();
            let shown = if cur.status.is_empty() { "not started".to_string() } else { cur.status.clone() };
            egui::ComboBox::from_id_salt(("pl-sheet", *page)).selected_text(&shown).width(100.0).show_ui(ui, |ui| {
                for s in ["not started", "in progress", "done"] {
                    if ui.selectable_label(shown == s, s).clicked() && shown != s {
                        let r = app.run("fasheet", json!({ "sheet": key, "status": s, "initials": st.initials }));
                        if let Err(e) = r {
                            app.set_status(e);
                        }
                    }
                }
            });
            if !cur.date.is_empty() {
                ui.label(RichText::new(format!("{} {}", cur.date, cur.initials)).color(t.text_dim));
            }
            ui.separator();
        }
    });
    if !st.picking_room.is_empty() {
        ui.label(RichText::new(crate::tl!("Click the room's number or name on the drawing (Esc: cancel)")).color(t.accent));
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            st.picking_room.clear();
        }
    }

    // Field changes for the focused device.
    if let Some(d) = st.focus.as_ref().and_then(|id| devices.iter().find(|d| &d.id == id)).cloned() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}:", if d.address.is_empty() { "?" } else { &d.address })).strong());
            ui.label(crate::tl!("Field change"));
            ui.add(egui::TextEdit::singleline(&mut st.field_text).hint_text(crate::tl!("details")).desired_width(180.0));
            for (label, change) in
                [("Moved", "moved"), ("Swapped", "swapped"), ("Dual address", "dual address"), ("Removed", "removed"), ("Other", "other")]
            {
                if ui.small_button(crate::i18n::t(label)).clicked() {
                    match app.run("fafield", json!({ "ids": [d.id], "change": change, "text": st.field_text })) {
                        Ok(_) => st.field_text.clear(),
                        Err(e) => app.set_status(e),
                    }
                }
            }
            if ui.small_button(crate::tl!("Clear field changes")).clicked() {
                let _ = app.run("fafield", json!({ "ids": [d.id], "clear": true }));
            }
        });
    }
    // The table.
    let rows: Vec<&Device> = devices.iter().filter(|d| shows(&st, d)).collect();
    let widths = [26.0, 110.0, 70.0, 170.0, 170.0, 220.0, 70.0, 70.0, 360.0];
    let head = ["✓", "Address", "Type", "Function", "Interface equipment", "Room", "Sheet", "Status", "Comments"];
    let row_h = 22.0;
    egui::ScrollArea::horizontal().id_salt("pl-h").auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (w, h) in widths.iter().zip(head) {
                ui.add_sized([*w, row_h], egui::Label::new(RichText::new(crate::i18n::t(h)).strong().color(t.accent)));
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().id_salt("pl-v").auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            for d in rows.iter().skip(range.start).take(range.end - range.start) {
                let focused = st.focus.as_deref() == Some(d.id.as_str());
                let dup = d.flags.iter().any(|f| f.starts_with("duplicate"));
                let resp = ui
                    .horizontal(|ui| {
                        // Accepted.
                        let mut acc = d.status == "checked";
                        if ui.add_sized([widths[0], row_h], egui::Checkbox::without_text(&mut acc)).changed() {
                            let _ = app.run("fapointset", json!({ "id": d.id, "accepted": acc }));
                        }
                        let addr =
                            RichText::new(if d.address.is_empty() { "?" } else { &d.address }).color(if dup { status_color("red") } else { t.text });
                        let a = ui.add_sized(
                            [widths[1], row_h],
                            egui::Label::new(if focused { addr.strong().underline() } else { addr }).sense(egui::Sense::click()),
                        );
                        if a.clicked() {
                            st.focus = Some(d.id.clone());
                            zoom_to(app, d);
                        }
                        ui.add_sized([widths[2], row_h], egui::Label::new(&d.dtype));
                        edit_cell(app, ui, &mut st, d, Col::Function, &d.function, widths[3], row_h);
                        edit_cell(app, ui, &mut st, d, Col::Equipment, &d.equipment, widths[4], row_h);
                        ui.horizontal(|ui| {
                            ui.set_width(widths[5]);
                            edit_cell(app, ui, &mut st, d, Col::Room, &room_text(d), widths[5] - 28.0, row_h);
                            if ui.small_button("⌖").on_hover_text(crate::tl!("Pick the room on the drawing")).clicked() {
                                st.picking_room = vec![d.id.clone()];
                                st.focus = Some(d.id.clone());
                                zoom_to(app, d);
                            }
                        });
                        ui.add_sized([widths[6], row_h], egui::Label::new(if d.sheet.is_empty() { d.page.to_string() } else { d.sheet.clone() }));
                        ui.add_sized([widths[7], row_h], egui::Label::new(RichText::new(d.status.to_uppercase()).color(status_color(&d.status))));
                        edit_cell(app, ui, &mut st, d, Col::Note, &comments(d), widths[8], row_h);
                    })
                    .response;
                if resp.hovered() {
                    let pass = ctx.cumulative_pass_nr();
                    ctx.data_mut(|m| m.insert_temp(egui::Id::new("cadcraft-pointlist-hover"), (d.id.clone(), pass)));
                }
            }
        });
    });
    set_state(&ctx, st);
}

/// A cell that turns into a text box when clicked; Enter or clicking away saves it.
#[allow(clippy::too_many_arguments)]
fn edit_cell(app: &mut CadApp, ui: &mut egui::Ui, st: &mut State, d: &Device, col: Col, value: &str, w: f32, h: f32) {
    let editing = st.editing.as_ref().is_some_and(|(id, c, _)| *id == d.id && *c == col);
    if editing {
        let Some((_, _, text)) = st.editing.as_mut() else { return };
        let r = ui.add_sized([w, h], egui::TextEdit::singleline(text));
        if st.edit_new {
            r.request_focus();
            st.edit_new = false;
        }
        let enter = r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let done = r.lost_focus() || enter;
        if done {
            let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if let Some((id, c, text)) = st.editing.take()
                && !cancel
            {
                let params = match c {
                    Col::Function => json!({ "id": id, "function": text }),
                    Col::Equipment => json!({ "id": id, "equipment": text }),
                    Col::Note => json!({ "id": id, "note": crate::palettes::pointlist::owner_part(&text) }),
                    Col::Room => {
                        // "NAME (NUMBER)" or a number.
                        let t = text.trim();
                        match t.rsplit_once('(') {
                            Some((name, num)) => json!({ "id": id, "room": name.trim(), "roomNum": num.trim_end_matches(')').trim() }),
                            None if t.chars().any(|c| c.is_ascii_digit()) && !t.contains(' ') => json!({ "id": id, "roomNum": t, "room": "" }),
                            None => json!({ "id": id, "room": t }),
                        }
                    }
                };
                if let Err(e) = app.run("fapointset", params) {
                    app.set_status(e);
                }
            }
        }
        return;
    }
    let r = ui.add_sized([w, h], egui::Label::new(value).truncate().sense(egui::Sense::click())).on_hover_text(value);
    if r.clicked() {
        let start = if col == Col::Note { d.note.clone() } else { value.to_string() };
        st.editing = Some((d.id.clone(), col, start));
        st.edit_new = true;
    }
}

/// The owner's part of a Comments cell (what's before "APP:").
pub fn owner_part(cell: &str) -> String {
    let c = cell.trim();
    match c.find("APP:") {
        Some(i) => c[..i].trim().trim_end_matches('|').trim().to_string(),
        None => c.to_string(),
    }
}
