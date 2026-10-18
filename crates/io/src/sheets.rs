//! Spreadsheets for the fire alarm point list: read Excel workbooks (.xls, .xlsx, .xlsm, .ods)
//! and CSV as text grids, and write .xlsx workbooks.
//!
//! Native only (the web build has no spreadsheet support yet).

/// A sheet as text: its name and rows of cells.
pub type Grid = (String, Vec<Vec<String>>);

/// How a written cell looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    /// Header row: bold on amber.
    Header,
    /// A gap-filled spacer row's address: grey.
    Spacer,
    Green,
    Amber,
    Red,
}

/// A sheet to write: name, rows, and a style per cell (missing = plain).
pub struct OutSheet {
    pub name: String,
    pub rows: Vec<Vec<String>>,
    pub styles: Vec<Vec<Style>>,
    pub widths: Vec<f64>,
}

const MAX_ROWS: usize = 200_000;
const MAX_COLS: usize = 200;

/// Read every sheet of a workbook (or one CSV) as text.
#[cfg(not(target_arch = "wasm32"))]
pub fn read(bytes: &[u8], name: &str) -> Result<Vec<Grid>, String> {
    use calamine::{Data, Reader};
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".csv") || lower.ends_with(".txt") || lower.ends_with(".tsv") {
        let text = String::from_utf8_lossy(bytes);
        return Ok(vec![("Sheet1".into(), csv_rows(&text))]);
    }
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut wb = calamine::open_workbook_auto_from_rs(cursor).map_err(|e| format!("{name}: {e}"))?;
    let mut out = Vec::new();
    for sheet in wb.sheet_names() {
        let Ok(range) = wb.worksheet_range(&sheet) else { continue };
        let rows: Vec<Vec<String>> = range
            .rows()
            .take(MAX_ROWS)
            .map(|r| {
                r.iter()
                    .take(MAX_COLS)
                    .map(|c| match c {
                        Data::Empty => String::new(),
                        Data::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .collect();
        out.push((sheet, rows));
    }
    Ok(out)
}

#[cfg(target_arch = "wasm32")]
pub fn read(bytes: &[u8], name: &str) -> Result<Vec<Grid>, String> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".csv") || lower.ends_with(".txt") || lower.ends_with(".tsv") {
        return Ok(vec![("Sheet1".into(), csv_rows(&String::from_utf8_lossy(bytes)))]);
    }
    Err("Excel files can only be read in the desktop app; use a CSV here".into())
}

/// CSV (or tab-separated) text as rows: quotes and doubled quotes handled.
pub fn csv_rows(text: &str) -> Vec<Vec<String>> {
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
                if rows.len() >= MAX_ROWS {
                    return rows;
                }
            }
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

/// Write sheets as an .xlsx workbook. The first row of each sheet is frozen.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_xlsx(sheets: &[OutSheet]) -> Result<Vec<u8>, String> {
    use rust_xlsxwriter::{Color, Format, FormatBorder, Workbook};
    let mut wb = Workbook::new();
    let fmt = |s: Style| -> Format {
        match s {
            Style::Plain => Format::new(),
            Style::Header => Format::new().set_bold().set_background_color(Color::RGB(0xFFB000)).set_border_bottom(FormatBorder::Thin),
            Style::Spacer => Format::new().set_font_color(Color::RGB(0x9A9A9A)),
            Style::Green => Format::new().set_background_color(Color::RGB(0xC8F0D2)),
            Style::Amber => Format::new().set_background_color(Color::RGB(0xFFE3A6)),
            Style::Red => Format::new().set_background_color(Color::RGB(0xF8C4C4)),
        }
    };
    let mut used = std::collections::HashSet::new();
    for sh in sheets {
        // Sheet names: 31 characters, no []:*?/\ and unique.
        let mut name: String = sh.name.chars().filter(|c| !"[]:*?/\\".contains(*c)).take(31).collect();
        if name.trim().is_empty() {
            name = "Sheet".into();
        }
        let base = name.clone();
        let mut k = 2;
        while !used.insert(name.to_ascii_lowercase()) {
            let suffix = format!(" ({k})");
            name = format!("{}{suffix}", base.chars().take(31 - suffix.len()).collect::<String>());
            k += 1;
        }
        let ws = wb.add_worksheet();
        ws.set_name(&name).map_err(|e| e.to_string())?;
        for (c, w) in sh.widths.iter().enumerate() {
            let Ok(c) = u16::try_from(c) else { break };
            let _ = ws.set_column_width(c, *w);
        }
        for (r, row) in sh.rows.iter().enumerate().take(1_000_000) {
            let Ok(r) = u32::try_from(r) else { break };
            for (c, v) in row.iter().enumerate().take(16_000) {
                let Ok(c16) = u16::try_from(c) else { break };
                let st = sh.styles.get(r as usize).and_then(|s| s.get(c)).copied().unwrap_or(Style::Plain);
                ws.write_string_with_format(r, c16, v, &fmt(st)).map_err(|e| e.to_string())?;
            }
        }
        let _ = ws.set_freeze_panes(1, 0);
    }
    wb.save_to_buffer().map_err(|e| e.to_string())
}

#[cfg(target_arch = "wasm32")]
pub fn write_xlsx(_sheets: &[OutSheet]) -> Result<Vec<u8>, String> {
    Err("Excel export is only available in the desktop app".into())
}

/// [`write_xlsx`] from JSON: `[{name, rows: [[text]], styles: [[style]], widths: [n]}]`, styles
/// `plain`, `header`, `spacer`, `green`, `amber`, `red`.
pub fn write_json(v: &serde_json::Value) -> Result<Vec<u8>, String> {
    let style = |s: &str| match s {
        "header" => Style::Header,
        "spacer" => Style::Spacer,
        "green" => Style::Green,
        "amber" => Style::Amber,
        "red" => Style::Red,
        _ => Style::Plain,
    };
    let text = |v: &serde_json::Value| -> String {
        match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            o => o.to_string(),
        }
    };
    let sheets: Vec<OutSheet> = v
        .as_array()
        .ok_or("expected a list of sheets")?
        .iter()
        .map(|s| OutSheet {
            name: s.get("name").and_then(|n| n.as_str()).unwrap_or("Sheet").to_string(),
            rows: s
                .get("rows")
                .and_then(|r| r.as_array())
                .map(|r| r.iter().map(|row| row.as_array().map(|c| c.iter().map(text).collect()).unwrap_or_default()).collect())
                .unwrap_or_default(),
            styles: s
                .get("styles")
                .and_then(|r| r.as_array())
                .map(|r| {
                    r.iter()
                        .map(|row| row.as_array().map(|c| c.iter().map(|x| style(x.as_str().unwrap_or(""))).collect()).unwrap_or_default())
                        .collect()
                })
                .unwrap_or_default(),
            widths: s.get("widths").and_then(|w| w.as_array()).map(|w| w.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default(),
        })
        .collect();
    write_xlsx(&sheets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes() {
        let r = csv_rows("a,b\n\"x, y\",\"say \"\"hi\"\"\"\n");
        assert_eq!(r[1], vec!["x, y".to_string(), "say \"hi\"".to_string()]);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn xlsx_round_trip() {
        let sheets = vec![OutSheet {
            name: "N1 L1".into(),
            rows: vec![vec!["Address".into(), "Label".into()], vec!["N1L1D1".into(), "SAMPLE".into()]],
            styles: vec![vec![Style::Header, Style::Header]],
            widths: vec![12.0, 30.0],
        }];
        let bytes = write_xlsx(&sheets).unwrap_or_default();
        let back = read(&bytes, "x.xlsx").unwrap_or_default();
        assert_eq!(back.first().map(|g| g.0.as_str()), Some("N1 L1"));
        assert_eq!(back.first().and_then(|g| g.1.get(1)).and_then(|r| r.get(1)).map(String::as_str), Some("SAMPLE"));
    }
}
