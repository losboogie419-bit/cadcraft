//! PDF → Drawing: vector import of CAD-exported PDFs (fire alarm panel builds, risers, plans).
//!
//! Every page's content stream is walked with the full graphics state (`q`/`Q`, `cm`, form
//! XObjects with their `/Matrix`), so lines, polylines, circles and arcs land at their true
//! positions. Page `/Rotate` is applied, so the drawing comes in upright. Units are inches on
//! the sheet (1 PDF point = 1/72 in).
//!
//! What becomes what:
//! - Straight runs become LINE (two points) or LWPOLYLINE. Bezier circles (four cubics that keep
//!   one radius) become CIRCLE, and single-radius bezier runs become ARC. Other curves are
//!   flattened into polylines.
//! - Filled triangles and quadrilaterals become SOLID. Other fills keep their outline. White
//!   fills (paper-coloured masks) are skipped.
//! - Real PDF text (`Tj`/`TJ`) becomes TEXT on `PDF_TEXT`. Invisible text (render mode 3) goes
//!   on `PDF_TEXT_HIDDEN`, which is off.
//! - AutoCAD SHX text comments (Square annotations titled "AutoCAD SHX Text") become real TEXT
//!   on `PDF_SHX_TEXT`, placed and sized from the stroked glyphs inside the comment's box. Those
//!   glyph strokes move to `PDF_SHX_STROKES` (off), so the label reads cleanly and can be
//!   searched. Labels over 40 characters or wrapped over several lines keep their strokes, and
//!   a searchable copy goes on `PDF_SHX_SEARCH` (off).
//! - Black ink is ByLayer (white on the dark canvas). Other colours keep their true colour.
//!
//! Not read: raster images, dash patterns, line widths, clipping, shading and patterns.

use cadcraft_doc::Drawing;

use crate::Result;

/// Read every page of a PDF into one drawing (pages side by side, left to right).
#[cfg(not(target_arch = "wasm32"))]
pub fn read(bytes: &[u8]) -> Result<Drawing> {
    native::read(bytes)
}

#[cfg(target_arch = "wasm32")]
pub fn read(_bytes: &[u8]) -> Result<Drawing> {
    Err(crate::IoError::Format("PDF import is not available in the web version; use the desktop app".into()))
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::HashMap;

    use cadcraft_color::{Color, Rgb};
    use cadcraft_doc::*;
    use cadcraft_geom::{PolyVertex, Vec2, Vec3};
    use lopdf::content::Content;
    use lopdf::{Dictionary, Document, Object, ObjectId};

    use crate::{IoError, Result};

    /// Drawing objects per file.
    const MAX_ITEMS: usize = 3_000_000;
    /// Content operations per file (pages and forms together).
    const MAX_OPS: usize = 30_000_000;
    /// Nested form XObjects.
    const MAX_FORM_DEPTH: usize = 16;
    /// Decompressed content per stream.
    const MAX_STREAM: usize = 256 << 20;
    /// Pages read from one file.
    const MAX_PAGES: usize = 200;
    /// Chords per flattened bezier.
    const BEZ_STEPS: usize = 8;
    /// Longest SHX label turned into TEXT; longer notes keep their strokes.
    const MAX_LABEL: usize = 40;
    /// Gap between pages laid side by side, inches.
    const PAGE_GAP: f64 = 2.0;
    /// PDF points per drawing unit (inch).
    const PT: f64 = 72.0;

    pub const L_LINES: &str = "PDF_LINES";
    pub const L_FILLS: &str = "PDF_FILLS";
    pub const L_TEXT: &str = "PDF_TEXT";
    pub const L_TEXT_HIDDEN: &str = "PDF_TEXT_HIDDEN";
    pub const L_SHX_TEXT: &str = "PDF_SHX_TEXT";
    pub const L_SHX_STROKES: &str = "PDF_SHX_STROKES";
    /// One closed rectangle per page (off), in page order: tools find a point's page with it.
    pub const L_PAGES: &str = "PDF_PAGES";
    pub const L_SHX_GLYPHS: &str = "PDF_SHX_GLYPHS";
    pub const L_SHX_SEARCH: &str = "PDF_SHX_SEARCH";
    pub const L_WHITE: &str = "PDF_WHITE";

    fn bad(msg: impl Into<String>) -> IoError {
        IoError::Format(format!("PDF: {}", msg.into()))
    }

    // ---------- matrices (PDF row-vector convention: p' = p × M) ----------

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct M([f64; 6]);

    impl M {
        const I: M = M([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        /// `self` then `o`.
        fn then(self, o: M) -> M {
            let [a, b, c, d, e, f] = self.0;
            let [a2, b2, c2, d2, e2, f2] = o.0;
            M([a * a2 + b * c2, a * b2 + b * d2, c * a2 + d * c2, c * b2 + d * d2, e * a2 + f * c2 + e2, e * b2 + f * d2 + f2])
        }
        fn ap(self, x: f64, y: f64) -> Vec2 {
            let [a, b, c, d, e, f] = self.0;
            Vec2::new(a * x + c * y + e, b * x + d * y + f)
        }
        fn finite(self) -> bool {
            self.0.iter().all(|v| v.is_finite())
        }
    }

    fn num(o: &Object) -> Option<f64> {
        match o {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(f64::from(*r)),
            _ => None,
        }
        .filter(|v| v.is_finite())
    }

    fn nums(ops: &[Object]) -> Vec<f64> {
        ops.iter().filter_map(num).collect()
    }

    fn matrix(o: Option<&Object>) -> Option<M> {
        let a = o?.as_array().ok()?;
        let v: Vec<f64> = a.iter().filter_map(num).collect();
        (v.len() == 6).then(|| M([v[0], v[1], v[2], v[3], v[4], v[5]])).filter(|m| m.finite())
    }

    // ---------- collected items ----------

    #[derive(Clone, Debug)]
    enum Shape {
        Line(Vec2, Vec2),
        Poly(Vec<Vec2>, bool),
        Circle(Vec2, f64),
        Arc(Vec2, f64, f64, f64),
        Solid([Vec2; 4]),
        /// A filled polygon with more than four corners (diamonds with a closing point, stars,
        /// arrowheads): a solid hatch, so it stays filled.
        Fill(Vec<Vec2>),
        Text {
            at: Vec2,
            end: Vec2,
            height: f64,
            rot: f64,
            value: String,
        },
    }

    #[derive(Clone, Debug)]
    struct Item {
        shape: Shape,
        color: Option<Rgb>,
        layer: &'static str,
        /// Stroked geometry (the only kind that can be an SHX glyph).
        stroke: bool,
    }

    fn bbox(s: &Shape) -> [f64; 4] {
        let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        let mut add = |p: Vec2| {
            b[0] = b[0].min(p.x);
            b[1] = b[1].min(p.y);
            b[2] = b[2].max(p.x);
            b[3] = b[3].max(p.y);
        };
        match s {
            Shape::Line(a, c) => {
                add(*a);
                add(*c);
            }
            Shape::Poly(v, _) => v.iter().for_each(|p| add(*p)),
            Shape::Circle(c, r) | Shape::Arc(c, r, _, _) => {
                add(Vec2::new(c.x - r, c.y - r));
                add(Vec2::new(c.x + r, c.y + r));
            }
            Shape::Solid(v) => v.iter().for_each(|p| add(*p)),
            Shape::Fill(v) => v.iter().for_each(|p| add(*p)),
            Shape::Text { at, end, .. } => {
                add(*at);
                add(*end);
            }
        }
        b
    }

    // ---------- paths ----------

    #[derive(Clone, Copy, Debug)]
    enum Seg {
        L(Vec2),
        C(Vec2, Vec2, Vec2),
    }

    #[derive(Clone, Debug, Default)]
    struct Sub {
        start: Vec2,
        segs: Vec<Seg>,
        closed: bool,
    }

    fn bez(p0: Vec2, p1: Vec2, p2: Vec2, p3: Vec2, t: f64) -> Vec2 {
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        Vec2::new(a * p0.x + b * p1.x + c * p2.x + d * p3.x, a * p0.y + b * p1.y + c * p2.y + d * p3.y)
    }

    fn dist(a: Vec2, b: Vec2) -> f64 {
        (a.x - b.x).hypot(a.y - b.y)
    }

    /// The circle through three points.
    fn circle3(a: Vec2, b: Vec2, c: Vec2) -> Option<(Vec2, f64)> {
        let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
        if d.abs() < 1e-12 {
            return None;
        }
        let (a2, b2, c2) = (a.x * a.x + a.y * a.y, b.x * b.x + b.y * b.y, c.x * c.x + c.y * c.y);
        let ux = (a2 * (b.y - c.y) + b2 * (c.y - a.y) + c2 * (a.y - b.y)) / d;
        let uy = (a2 * (c.x - b.x) + b2 * (a.x - c.x) + c2 * (b.x - a.x)) / d;
        let cen = Vec2::new(ux, uy);
        let r = dist(cen, a);
        (r.is_finite() && r > 0.0).then_some((cen, r))
    }

    /// Points along a sub-path, beziers sampled (`BEZ_STEPS` chords each).
    fn flatten(sp: &Sub) -> Vec<Vec2> {
        let mut out = vec![sp.start];
        let mut cur = sp.start;
        for s in &sp.segs {
            match *s {
                Seg::L(p) => {
                    out.push(p);
                    cur = p;
                }
                Seg::C(a, b, c) => {
                    for i in 1..=BEZ_STEPS {
                        out.push(bez(cur, a, b, c, i as f64 / BEZ_STEPS as f64));
                    }
                    cur = c;
                }
            }
        }
        out
    }

    /// A sub-path that is all beziers keeping one radius: a full circle or an arc.
    fn as_round(sp: &Sub) -> Option<Shape> {
        if sp.segs.is_empty() || sp.segs.len() > 8 || !sp.segs.iter().all(|s| matches!(s, Seg::C(..))) {
            return None;
        }
        let pts = flatten(sp);
        let n = pts.len();
        let first = *pts.first()?;
        let last = *pts.last()?;
        let mid = *pts.get(n / 2)?;
        let closed = dist(first, last) < 1e-3 * (1.0 + dist(first, mid));
        let (cen, r) = if closed {
            let q = *pts.get(n / 4)?;
            circle3(first, q, mid)?
        } else {
            circle3(first, mid, last)?
        };
        let tol = (r * 0.02).max(1e-4);
        if !pts.iter().all(|p| (dist(*p, cen) - r).abs() <= tol) {
            return None;
        }
        if closed {
            return Some(Shape::Circle(cen, r));
        }
        // Arc direction: CCW from start to end through the middle, else swap.
        let ang = |p: Vec2| (p.y - cen.y).atan2(p.x - cen.x);
        let (a0, am, a1) = (ang(first), ang(mid), ang(last));
        let norm = |a: f64| a.rem_euclid(std::f64::consts::TAU);
        let ccw = norm(am - a0) < norm(a1 - a0);
        let (s, e) = if ccw { (a0, a1) } else { (a1, a0) };
        Some(Shape::Arc(cen, r, norm(s), norm(e)))
    }

    // ---------- fonts and text ----------

    #[derive(Clone, Debug, Default)]
    struct Font {
        two_byte: bool,
        first: i64,
        widths: Vec<f64>,
        cid_widths: HashMap<u32, f64>,
        default_w: f64,
        to_uni: HashMap<u32, String>,
    }

    impl Font {
        fn width(&self, code: u32) -> f64 {
            if self.two_byte {
                return self.cid_widths.get(&code).copied().unwrap_or(self.default_w) / 1000.0;
            }
            let i = i64::from(code) - self.first;
            usize::try_from(i).ok().and_then(|i| self.widths.get(i)).copied().unwrap_or(self.default_w) / 1000.0
        }
        fn chars(&self, code: u32) -> String {
            if let Some(s) = self.to_uni.get(&code) {
                return s.clone();
            }
            if self.two_byte {
                return char::from_u32(code).filter(|c| !c.is_control()).map(String::from).unwrap_or_default();
            }
            let c = match code {
                0x91 | 0x92 => '\'',
                0x93 | 0x94 => '"',
                0x96 | 0x97 => '-',
                0x95 => '•',
                c => char::from_u32(c).unwrap_or('?'),
            };
            if c.is_control() { String::new() } else { c.to_string() }
        }
    }

    fn hex_u32(s: &[u8]) -> Option<u32> {
        let t = std::str::from_utf8(s).ok()?;
        u32::from_str_radix(t, 16).ok()
    }

    fn utf16_hex(s: &[u8]) -> String {
        let units: Vec<u16> = s.chunks(4).filter_map(|c| hex_u32(c).and_then(|v| u16::try_from(v).ok())).collect();
        String::from_utf16_lossy(&units)
    }

    /// The hex tokens `<…>` and `[` `]` in a CMap section, in order.
    fn cmap_tokens(s: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.len() {
            match s.get(i) {
                Some(b'<') => {
                    let st = i + 1;
                    let mut j = st;
                    while j < s.len() && s.get(j) != Some(&b'>') {
                        j += 1;
                    }
                    out.push(s.get(st..j).unwrap_or_default().iter().copied().filter(u8::is_ascii_hexdigit).collect());
                    i = j + 1;
                }
                Some(b'[') => {
                    out.push(b"[".to_vec());
                    i += 1;
                }
                Some(b']') => {
                    out.push(b"]".to_vec());
                    i += 1;
                }
                _ => i += 1,
            }
        }
        out
    }

    fn sections<'a>(s: &'a [u8], open: &[u8], close: &[u8]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(a) = find(rest, open) {
            let after = rest.get(a + open.len()..).unwrap_or_default();
            let Some(b) = find(after, close) else { break };
            out.push(after.get(..b).unwrap_or_default());
            rest = after.get(b + close.len()..).unwrap_or_default();
        }
        out
    }

    fn find(h: &[u8], n: &[u8]) -> Option<usize> {
        if n.is_empty() || h.len() < n.len() {
            return None;
        }
        h.windows(n.len()).position(|w| w == n)
    }

    fn parse_to_unicode(data: &[u8]) -> HashMap<u32, String> {
        let mut map = HashMap::new();
        for sec in sections(data, b"beginbfchar", b"endbfchar") {
            let t = cmap_tokens(sec);
            for pair in t.chunks(2) {
                if let [a, b] = pair
                    && let Some(code) = hex_u32(a)
                {
                    map.insert(code, utf16_hex(b));
                }
            }
        }
        for sec in sections(data, b"beginbfrange", b"endbfrange") {
            let t = cmap_tokens(sec);
            let mut i = 0;
            while i + 2 < t.len() && map.len() < 70_000 {
                let (Some(lo), Some(hi)) = (t.get(i).and_then(|v| hex_u32(v)), t.get(i + 1).and_then(|v| hex_u32(v))) else {
                    i += 1;
                    continue;
                };
                if t.get(i + 2).map(Vec::as_slice) == Some(b"[") {
                    let mut j = i + 3;
                    let mut code = lo;
                    while j < t.len() && t.get(j).map(Vec::as_slice) != Some(b"]") {
                        if let Some(v) = t.get(j) {
                            map.insert(code, utf16_hex(v));
                        }
                        code = code.saturating_add(1);
                        j += 1;
                    }
                    i = j + 1;
                } else {
                    let dst = t.get(i + 2).map(|v| utf16_hex(v)).unwrap_or_default();
                    let mut units: Vec<u16> = dst.encode_utf16().collect();
                    if hi >= lo && hi - lo < 65_536 {
                        for code in lo..=hi {
                            map.insert(code, String::from_utf16_lossy(&units));
                            if let Some(l) = units.last_mut() {
                                *l = l.wrapping_add(1);
                            }
                        }
                    }
                    i += 3;
                }
            }
        }
        map
    }

    // ---------- the walker ----------

    #[derive(Clone)]
    struct Gs {
        ctm: M,
        stroke: Option<Rgb>,
        fill: Option<Rgb>,
        stroke_white: bool,
        fill_white: bool,
        // text state
        font: Option<String>,
        size: f64,
        tc: f64,
        tw: f64,
        th: f64,
        tl: f64,
        rise: f64,
        mode: i64,
    }

    impl Gs {
        fn new(ctm: M) -> Gs {
            Gs {
                ctm,
                stroke: None,
                fill: None,
                stroke_white: false,
                fill_white: false,
                font: None,
                size: 0.0,
                tc: 0.0,
                tw: 0.0,
                th: 1.0,
                tl: 0.0,
                rise: 0.0,
                mode: 0,
            }
        }
    }

    /// Colour components → (ByLayer for black | true colour, is white).
    fn color_of(c: &[f64]) -> (Option<Rgb>, bool) {
        let rgb = match c.len() {
            1 => [c[0], c[0], c[0]],
            3 => [c[0], c[1], c[2]],
            4 => {
                let k = c[3];
                [(1.0 - c[0]) * (1.0 - k), (1.0 - c[1]) * (1.0 - k), (1.0 - c[2]) * (1.0 - k)]
            }
            _ => return (None, false),
        };
        let rgb = rgb.map(|v| v.clamp(0.0, 1.0));
        if rgb.iter().all(|v| *v < 0.15) {
            return (None, false);
        }
        let white = rgb.iter().all(|v| *v > 0.97);
        let to = |v: f64| (v * 255.0).round() as u8;
        (Some(Rgb(to(rgb[0]), to(rgb[1]), to(rgb[2]))), white)
    }

    struct Walker<'a> {
        doc: &'a Document,
        /// User space → upright sheet inches for the current page.
        page: M,
        items: Vec<Item>,
        ops: usize,
        fonts: HashMap<ObjectId, Font>,
        images: usize,
    }

    impl Walker<'_> {
        fn push(&mut self, it: Item) -> Result<()> {
            if self.items.len() >= MAX_ITEMS {
                return Err(bad(format!("more than {MAX_ITEMS} objects")));
            }
            self.items.push(it);
            Ok(())
        }

        fn deref<'b>(&'b self, o: &'b Object) -> Option<&'b Object> {
            self.doc.dereference(o).ok().map(|(_, o)| o)
        }

        fn dict_in<'b>(&'b self, d: &'b Dictionary, key: &[u8]) -> Option<&'b Dictionary> {
            self.deref(d.get(key).ok()?)?.as_dict().ok()
        }

        fn load_font(&mut self, res: Option<&Dictionary>, name: &str) -> Option<Font> {
            let fonts = self.dict_in(res?, b"Font")?;
            let obj = fonts.get(name.as_bytes()).ok()?;
            let id = obj.as_reference().ok();
            if let Some(id) = id
                && let Some(f) = self.fonts.get(&id)
            {
                return Some(f.clone());
            }
            let fd = self.deref(obj)?.as_dict().ok()?;
            let mut f = Font { default_w: 500.0, ..Font::default() };
            let subtype = fd.get(b"Subtype").ok().and_then(|o| o.as_name().ok()).unwrap_or_default();
            if subtype == b"Type0" {
                f.two_byte = true;
                f.default_w = 1000.0;
                if let Some(desc) = fd
                    .get(b"DescendantFonts")
                    .ok()
                    .and_then(|o| self.deref(o))
                    .and_then(|o| o.as_array().ok())
                    .and_then(|a| a.first())
                    .and_then(|o| self.deref(o))
                    .and_then(|o| o.as_dict().ok())
                {
                    if let Some(dw) = desc.get(b"DW").ok().and_then(num) {
                        f.default_w = dw;
                    }
                    if let Some(w) = desc.get(b"W").ok().and_then(|o| self.deref(o)).and_then(|o| o.as_array().ok()) {
                        let mut i = 0;
                        while i + 1 < w.len() && f.cid_widths.len() < 70_000 {
                            let Some(c0) = w.get(i).and_then(num) else { break };
                            match w.get(i + 1).and_then(|o| self.deref(o)) {
                                Some(Object::Array(ws)) => {
                                    for (k, v) in ws.iter().filter_map(num).enumerate() {
                                        f.cid_widths.insert(c0 as u32 + k as u32, v);
                                    }
                                    i += 2;
                                }
                                Some(o) => {
                                    let (Some(c1), Some(v)) = (num(o), w.get(i + 2).and_then(num)) else { break };
                                    if c1 >= c0 && c1 - c0 < 65_536.0 {
                                        for c in (c0 as u32)..=(c1 as u32) {
                                            f.cid_widths.insert(c, v);
                                        }
                                    }
                                    i += 3;
                                }
                                None => break,
                            }
                        }
                    }
                }
            } else {
                f.first = fd.get(b"FirstChar").ok().and_then(num).map(|v| v as i64).unwrap_or(0);
                if let Some(w) = fd.get(b"Widths").ok().and_then(|o| self.deref(o)).and_then(|o| o.as_array().ok()) {
                    f.widths = w.iter().map(|o| self.deref(o).and_then(num).unwrap_or(0.0)).collect();
                }
                if let Some(mw) = fd
                    .get(b"FontDescriptor")
                    .ok()
                    .and_then(|o| self.deref(o))
                    .and_then(|o| o.as_dict().ok())
                    .and_then(|d| d.get(b"MissingWidth").ok())
                    .and_then(num)
                {
                    f.default_w = mw;
                }
            }
            if let Some(tu) = fd.get(b"ToUnicode").ok().and_then(|o| self.deref(o)).and_then(|o| o.as_stream().ok())
                && let Ok(data) = tu.decompressed_content()
            {
                f.to_uni = parse_to_unicode(&data);
            }
            if let Some(id) = id {
                self.fonts.insert(id, f.clone());
            }
            Some(f)
        }

        fn run(&mut self, content: &[u8], res: Option<&Dictionary>, ctm: M, depth: usize) -> Result<()> {
            let ops = Content::decode(content).map_err(|e| bad(format!("content stream: {e}")))?.operations;
            self.ops = self.ops.saturating_add(ops.len());
            if self.ops > MAX_OPS {
                return Err(bad(format!("more than {MAX_OPS} drawing operations")));
            }
            let mut gs = Gs::new(ctm);
            let mut stack: Vec<Gs> = Vec::new();
            let mut subs: Vec<Sub> = Vec::new();
            let mut cur: Option<Sub> = None;
            let mut pt = Vec2::ZERO;
            let mut font: Option<Font> = None;
            let mut tm = M::I;
            let mut tlm = M::I;
            for op in &ops {
                let o = op.operands.as_slice();
                let n = nums(o);
                let user = |x: f64, y: f64, g: &Gs| g.ctm.ap(x, y);
                match op.operator.as_str() {
                    "q" => {
                        if stack.len() < 256 {
                            stack.push(gs.clone());
                        }
                    }
                    "Q" => {
                        if let Some(g) = stack.pop() {
                            gs = g;
                        }
                    }
                    "cm" if n.len() == 6 => {
                        let m = M([n[0], n[1], n[2], n[3], n[4], n[5]]);
                        if m.finite() {
                            gs.ctm = m.then(gs.ctm);
                        }
                    }
                    "m" if n.len() == 2 => {
                        if let Some(s) = cur.take()
                            && !s.segs.is_empty()
                        {
                            subs.push(s);
                        }
                        pt = user(n[0], n[1], &gs);
                        cur = Some(Sub { start: pt, ..Sub::default() });
                    }
                    "l" if n.len() == 2 => {
                        let p = user(n[0], n[1], &gs);
                        cur.get_or_insert_with(|| Sub { start: pt, ..Sub::default() }).segs.push(Seg::L(p));
                        pt = p;
                    }
                    "c" if n.len() == 6 => {
                        let (a, b, c) = (user(n[0], n[1], &gs), user(n[2], n[3], &gs), user(n[4], n[5], &gs));
                        cur.get_or_insert_with(|| Sub { start: pt, ..Sub::default() }).segs.push(Seg::C(a, b, c));
                        pt = c;
                    }
                    "v" if n.len() == 4 => {
                        let (b, c) = (user(n[0], n[1], &gs), user(n[2], n[3], &gs));
                        cur.get_or_insert_with(|| Sub { start: pt, ..Sub::default() }).segs.push(Seg::C(pt, b, c));
                        pt = c;
                    }
                    "y" if n.len() == 4 => {
                        let (a, c) = (user(n[0], n[1], &gs), user(n[2], n[3], &gs));
                        cur.get_or_insert_with(|| Sub { start: pt, ..Sub::default() }).segs.push(Seg::C(a, c, c));
                        pt = c;
                    }
                    "h" => {
                        if let Some(s) = cur.as_mut() {
                            s.closed = true;
                            pt = s.start;
                        }
                    }
                    "re" if n.len() == 4 => {
                        if let Some(s) = cur.take()
                            && !s.segs.is_empty()
                        {
                            subs.push(s);
                        }
                        let (x, y, w, h) = (n[0], n[1], n[2], n[3]);
                        let p0 = user(x, y, &gs);
                        subs.push(Sub {
                            start: p0,
                            segs: vec![Seg::L(user(x + w, y, &gs)), Seg::L(user(x + w, y + h, &gs)), Seg::L(user(x, y + h, &gs))],
                            closed: true,
                        });
                        pt = p0;
                    }
                    "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                        if let Some(mut s) = cur.take() {
                            if matches!(op.operator.as_str(), "s" | "b" | "b*") {
                                s.closed = true;
                            }
                            if !s.segs.is_empty() {
                                subs.push(s);
                            }
                        }
                        let paths = std::mem::take(&mut subs);
                        let opn = op.operator.as_str();
                        // B, B*, b, b*: fill, then stroke the same path.
                        if matches!(opn, "f" | "F" | "f*" | "B" | "B*" | "b" | "b*") {
                            self.paint(&paths, &gs, false)?;
                        }
                        if matches!(opn, "S" | "s" | "B" | "B*" | "b" | "b*") {
                            self.paint(&paths, &gs, true)?;
                        }
                    }
                    "W" | "W*" => {}
                    "G" => (gs.stroke, gs.stroke_white) = color_of(&n),
                    "g" => (gs.fill, gs.fill_white) = color_of(&n),
                    "RG" => (gs.stroke, gs.stroke_white) = color_of(&n),
                    "rg" => (gs.fill, gs.fill_white) = color_of(&n),
                    "K" => (gs.stroke, gs.stroke_white) = color_of(&n),
                    "k" => (gs.fill, gs.fill_white) = color_of(&n),
                    "SC" | "SCN" => (gs.stroke, gs.stroke_white) = color_of(&n),
                    "sc" | "scn" => (gs.fill, gs.fill_white) = color_of(&n),
                    "CS" => (gs.stroke, gs.stroke_white) = (None, false),
                    "cs" => (gs.fill, gs.fill_white) = (None, false),
                    "Do" => {
                        let Some(name) = o.first().and_then(|v| v.as_name().ok()) else { continue };
                        let Some(xo) = res.and_then(|r| self.dict_in(r, b"XObject")).and_then(|x| x.get(name).ok()).and_then(|v| self.deref(v))
                        else {
                            continue;
                        };
                        let Ok(st) = xo.as_stream() else { continue };
                        let sub = st.dict.get(b"Subtype").ok().and_then(|v| v.as_name().ok()).unwrap_or_default();
                        if sub == b"Image" {
                            self.images += 1;
                        } else if sub == b"Form" && depth < MAX_FORM_DEPTH {
                            let m = matrix(st.dict.get(b"Matrix").ok()).unwrap_or(M::I);
                            let data = st.decompressed_content().unwrap_or_else(|_| st.content.clone());
                            if data.len() > MAX_STREAM {
                                continue;
                            }
                            let form_res = st.dict.get(b"Resources").ok().and_then(|v| self.deref(v)).and_then(|v| v.as_dict().ok()).cloned();
                            let r = form_res.as_ref().or(res);
                            self.run(&data, r, m.then(gs.ctm), depth + 1)?;
                        }
                    }
                    // ---- text ----
                    "BT" => {
                        tm = M::I;
                        tlm = M::I;
                    }
                    "ET" => {}
                    "Tf" if o.len() == 2 => {
                        if let Some(name) = o.first().and_then(|v| v.as_name().ok()) {
                            let name = String::from_utf8_lossy(name).to_string();
                            font = self.load_font(res, &name);
                            gs.font = Some(name);
                        }
                        gs.size = o.get(1).and_then(num).unwrap_or(gs.size);
                    }
                    "Tc" if n.len() == 1 => gs.tc = n[0],
                    "Tw" if n.len() == 1 => gs.tw = n[0],
                    "Tz" if n.len() == 1 => gs.th = n[0] / 100.0,
                    "TL" if n.len() == 1 => gs.tl = n[0],
                    "Ts" if n.len() == 1 => gs.rise = n[0],
                    "Tr" if n.len() == 1 => gs.mode = n[0] as i64,
                    "Td" if n.len() == 2 => {
                        tlm = M([1.0, 0.0, 0.0, 1.0, n[0], n[1]]).then(tlm);
                        tm = tlm;
                    }
                    "TD" if n.len() == 2 => {
                        gs.tl = -n[1];
                        tlm = M([1.0, 0.0, 0.0, 1.0, n[0], n[1]]).then(tlm);
                        tm = tlm;
                    }
                    "Tm" if n.len() == 6 => {
                        let m = M([n[0], n[1], n[2], n[3], n[4], n[5]]);
                        if m.finite() {
                            tlm = m;
                            tm = m;
                        }
                    }
                    "T*" => {
                        tlm = M([1.0, 0.0, 0.0, 1.0, 0.0, -gs.tl]).then(tlm);
                        tm = tlm;
                    }
                    "Tj" | "'" | "\"" | "TJ" => {
                        if op.operator == "'" || op.operator == "\"" {
                            if op.operator == "\"" && n.len() >= 2 {
                                gs.tw = n[0];
                                gs.tc = n[1];
                            }
                            tlm = M([1.0, 0.0, 0.0, 1.0, 0.0, -gs.tl]).then(tlm);
                            tm = tlm;
                        }
                        let Some(f) = font.as_ref() else { continue };
                        let mut parts: Vec<(Option<&[u8]>, f64)> = Vec::new();
                        for v in o {
                            match v {
                                Object::String(s, _) => parts.push((Some(s.as_slice()), 0.0)),
                                Object::Array(a) => {
                                    for e in a {
                                        match e {
                                            Object::String(s, _) => parts.push((Some(s.as_slice()), 0.0)),
                                            e => {
                                                if let Some(k) = num(e) {
                                                    parts.push((None, k));
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        let start_tm = tm;
                        let mut value = String::new();
                        for (s, k) in parts {
                            match s {
                                Some(s) => {
                                    let codes: Vec<u32> = if f.two_byte {
                                        s.chunks(2).map(|c| c.iter().fold(0u32, |a, b| (a << 8) | u32::from(*b))).collect()
                                    } else {
                                        s.iter().map(|b| u32::from(*b)).collect()
                                    };
                                    for code in codes {
                                        let ch = f.chars(code);
                                        let space = !f.two_byte && code == 32;
                                        let adv = (f.width(code) * gs.size + gs.tc + if space { gs.tw } else { 0.0 }) * gs.th;
                                        value.push_str(&ch);
                                        tm = M([1.0, 0.0, 0.0, 1.0, adv, 0.0]).then(tm);
                                    }
                                }
                                None => {
                                    let adv = -k / 1000.0 * gs.size * gs.th;
                                    if k < -250.0 && !value.ends_with(' ') {
                                        value.push(' ');
                                    }
                                    tm = M([1.0, 0.0, 0.0, 1.0, adv, 0.0]).then(tm);
                                }
                            }
                        }
                        let value = value.trim().to_string();
                        if value.is_empty() || gs.size <= 0.0 {
                            continue;
                        }
                        let full = |t: M| M([gs.size * gs.th, 0.0, 0.0, gs.size, 0.0, gs.rise]).then(t).then(gs.ctm).then(self.page);
                        let (t0, t1) = (full(start_tm), full(tm));
                        let at = t0.ap(0.0, 0.0);
                        let end = t1.ap(0.0, 0.0);
                        let up = t0.ap(0.0, 1.0);
                        let size = dist(up, at);
                        if !size.is_finite() || size <= 0.0 {
                            continue;
                        }
                        let rot = (end.y - at.y).atan2(end.x - at.x);
                        let rot = if dist(at, end) > 1e-9 { rot } else { 0.0 };
                        let layer = if gs.mode == 3 || gs.mode == 7 { L_TEXT_HIDDEN } else { L_TEXT };
                        let color = if gs.mode == 1 || gs.mode == 5 { gs.stroke } else { gs.fill };
                        self.push(Item { shape: Shape::Text { at, end, height: size * 0.7, rot, value }, color, layer, stroke: false })?;
                    }
                    _ => {}
                }
            }
            Ok(())
        }

        fn paint(&mut self, paths: &[Sub], gs: &Gs, stroke: bool) -> Result<()> {
            let (color, white) = if stroke { (gs.stroke, gs.stroke_white) } else { (gs.fill, gs.fill_white) };
            if !stroke && white {
                return Ok(());
            }
            for sp in paths {
                let pts: Vec<Vec2> = flatten(sp).into_iter().map(|p| self.page.ap(p.x, p.y)).collect();
                if pts.len() < 2 || !pts.iter().all(|p| p.x.is_finite() && p.y.is_finite()) {
                    continue;
                }
                let disp = Sub {
                    start: self.page.ap(sp.start.x, sp.start.y),
                    segs: sp
                        .segs
                        .iter()
                        .map(|s| match *s {
                            Seg::L(p) => Seg::L(self.page.ap(p.x, p.y)),
                            Seg::C(a, b, c) => Seg::C(self.page.ap(a.x, a.y), self.page.ap(b.x, b.y), self.page.ap(c.x, c.y)),
                        })
                        .collect(),
                    closed: sp.closed,
                };
                let layer = if white {
                    L_WHITE
                } else if stroke {
                    L_LINES
                } else {
                    L_FILLS
                };
                let straight = disp.segs.iter().all(|s| matches!(s, Seg::L(_)));
                let shape = if let Some(round) = as_round(&disp) {
                    round
                } else if !stroke && straight && (3..=4).contains(&pts.len()) || !stroke && straight && pts.len() == 5 && dist(pts[0], pts[4]) < 1e-9
                {
                    let p = |i: usize| pts.get(i).copied().unwrap_or(pts[0]);
                    // SOLID corner order is 1, 2, 4, 3.
                    if pts.len() == 3 { Shape::Solid([p(0), p(1), p(2), p(2)]) } else { Shape::Solid([p(0), p(1), p(3), p(2)]) }
                } else if !stroke && pts.len() >= 3 && pts.len() <= 4000 {
                    let mut v = pts;
                    while v.len() > 3 && v.first().zip(v.last()).is_some_and(|(a, b)| dist(*a, *b) < 1e-9) {
                        v.pop();
                    }
                    v.dedup_by(|a, b| dist(*a, *b) < 1e-9);
                    if v.len() == 4 {
                        Shape::Solid([v[0], v[1], v[3], v[2]])
                    } else if v.len() == 3 {
                        Shape::Solid([v[0], v[1], v[2], v[2]])
                    } else if v.len() > 4 {
                        Shape::Fill(v)
                    } else {
                        Shape::Poly(v, true)
                    }
                } else if pts.len() == 2 && !disp.closed {
                    Shape::Line(pts[0], pts[1])
                } else {
                    let mut v = pts;
                    let closed = disp.closed || v.len() > 2 && v.first().zip(v.last()).is_some_and(|(a, b)| dist(*a, *b) < 1e-9);
                    if closed && v.len() > 2 && v.first().zip(v.last()).is_some_and(|(a, b)| dist(*a, *b) < 1e-9) {
                        v.pop();
                    }
                    Shape::Poly(v, closed)
                };
                self.push(Item { shape, color, layer, stroke })?;
            }
            Ok(())
        }
    }

    // ---------- SHX labels ----------

    struct Label {
        text: String,
        r: [f64; 4],
    }

    /// AutoCAD control codes in label text: %%U / %%O / %%K (underline, overline, strike) are
    /// dropped; %%D, %%P, %%C become °, ±, ⌀; %%% is %.
    pub(crate) fn acad_codes(t: &str) -> String {
        if !t.contains("%%") {
            return t.to_string();
        }
        let mut out = String::with_capacity(t.len());
        let mut rest = t;
        while let Some(i) = rest.find("%%") {
            out.push_str(&rest[..i]);
            let after = &rest[i + 2..];
            let mut c = after.chars();
            match c.next().map(|c| c.to_ascii_uppercase()) {
                Some('U' | 'O' | 'K') => rest = c.as_str(),
                Some('D') => {
                    out.push('°');
                    rest = c.as_str();
                }
                Some('P') => {
                    out.push('±');
                    rest = c.as_str();
                }
                Some('C') => {
                    out.push('⌀');
                    rest = c.as_str();
                }
                Some('%') => {
                    out.push('%');
                    rest = c.as_str();
                }
                _ => {
                    out.push_str("%%");
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    fn pdf_text(o: &Object) -> Option<String> {
        let s = o.as_str().ok()?;
        if s.starts_with(&[0xFE, 0xFF]) {
            let u: Vec<u16> = s.get(2..)?.chunks(2).map(|c| (u16::from(c[0]) << 8) | u16::from(*c.get(1).unwrap_or(&0))).collect();
            Some(String::from_utf16_lossy(&u))
        } else if s.starts_with(&[0xEF, 0xBB, 0xBF]) {
            Some(String::from_utf8_lossy(s.get(3..)?).to_string())
        } else {
            Some(s.iter().map(|b| char::from(*b)).collect())
        }
    }

    /// Labels whose box overlaps another label's box by more than a quarter of the smaller one.
    fn label_overlaps(labels: &[Label]) -> HashMap<usize, bool> {
        let cell = 0.5;
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (i, l) in labels.iter().enumerate() {
            let [x0, y0, x1, y1] = l.r;
            if (x1 - x0) > 20.0 || (y1 - y0) > 20.0 {
                continue;
            }
            for gx in (x0 / cell).floor() as i64..=(x1 / cell).floor() as i64 {
                for gy in (y0 / cell).floor() as i64..=(y1 / cell).floor() as i64 {
                    grid.entry((gx, gy)).or_default().push(i);
                }
            }
        }
        let mut out = HashMap::new();
        for cell in grid.values() {
            for (a, &i) in cell.iter().enumerate() {
                for &j in cell.iter().skip(a + 1) {
                    let (Some(p), Some(q)) = (labels.get(i), labels.get(j)) else { continue };
                    let ix = (p.r[2].min(q.r[2]) - p.r[0].max(q.r[0])).max(0.0);
                    let iy = (p.r[3].min(q.r[3]) - p.r[1].max(q.r[1])).max(0.0);
                    let area = |r: &[f64; 4]| (r[2] - r[0]) * (r[3] - r[1]);
                    if ix * iy > 0.25 * area(&p.r).min(area(&q.r)) {
                        out.insert(i, true);
                        out.insert(j, true);
                    }
                }
            }
        }
        out
    }

    /// Turn SHX comments into TEXT, moving their glyph strokes off. Returns (converted, kept).
    fn apply_labels(items: &mut Vec<Item>, labels: &[Label]) -> Result<(usize, usize)> {
        // Grid of stroked items by bbox.
        let cell = 0.25;
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let boxes: Vec<[f64; 4]> = items.iter().map(|i| bbox(&i.shape)).collect();
        for (i, (it, b)) in items.iter().zip(&boxes).enumerate() {
            if !it.stroke || it.layer != L_LINES || matches!(it.shape, Shape::Text { .. }) {
                continue;
            }
            let (w, h) = (b[2] - b[0], b[3] - b[1]);
            if !(w.is_finite() && h.is_finite()) || w.max(h) > 2.0 {
                continue;
            }
            let (cx, cy) = (((b[0] + b[2]) / 2.0 / cell).floor() as i64, ((b[1] + b[3]) / 2.0 / cell).floor() as i64);
            grid.entry((cx, cy)).or_default().push(i);
        }
        let mut taken = vec![false; items.len()];
        // Glyph strokes of labels that keep their strokes: still drawn, on their own layer.
        let mut glyph_kept = vec![false; items.len()];
        let (mut conv, mut kept) = (0, 0);
        let mut out: Vec<Item> = Vec::new();
        let overlaps = label_overlaps(labels);
        for (li, lab) in labels.iter().enumerate() {
            let [x0, y0, x1, y1] = lab.r;
            let (w, h) = (x1 - x0, y1 - y0);
            if !(w > 0.0 && h > 0.0) {
                continue;
            }
            let text: String = lab.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                continue;
            }
            let n = text.chars().count();
            let short = w.min(h);
            let pad = short * 0.12;
            let horizontal = w >= h * 0.9 || n <= 1;
            // Glyph strokes: small stroked pieces whose centre lies in the comment's box.
            let mut glyphs = Vec::new();
            let (gx0, gy0) = (((x0 - pad) / cell).floor() as i64 - 1, ((y0 - pad) / cell).floor() as i64 - 1);
            let (gx1, gy1) = (((x1 + pad) / cell).floor() as i64 + 1, ((y1 + pad) / cell).floor() as i64 + 1);
            if (gx1 - gx0).saturating_mul(gy1 - gy0) > 40_000 {
                continue;
            }
            for gx in gx0..=gx1 {
                for gy in gy0..=gy1 {
                    for &i in grid.get(&(gx, gy)).map(Vec::as_slice).unwrap_or_default() {
                        let Some(b) = boxes.get(i) else { continue };
                        if taken.get(i).copied().unwrap_or(true) {
                            continue;
                        }
                        let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
                        let inside = cx >= x0 && cx <= x1 && cy >= y0 && cy <= y1;
                        let near = b[0] >= x0 - pad && b[2] <= x1 + pad && b[1] >= y0 - pad && b[3] <= y1 + pad;
                        if inside && near && (b[2] - b[0]).max(b[3] - b[1]) <= short * 1.2 {
                            glyphs.push(i);
                        }
                    }
                }
            }
            glyphs.sort_unstable();
            // Cap height = the tallest glyph across the reading direction; lines = bands of glyph centres.
            let across = |b: &[f64; 4]| if horizontal { b[3] - b[1] } else { b[2] - b[0] };
            let centre = |b: &[f64; 4]| if horizontal { (b[1] + b[3]) / 2.0 } else { (b[0] + b[2]) / 2.0 };
            let along = |b: &[f64; 4]| if horizontal { (b[0] + b[2]) / 2.0 } else { (b[1] + b[3]) / 2.0 };
            let gb: Vec<[f64; 4]> = glyphs.iter().filter_map(|i| boxes.get(*i).copied()).collect();
            let cap = gb.iter().map(across).fold(0.0_f64, f64::max);
            let mut cs: Vec<f64> = gb.iter().filter(|b| across(b) > cap * 0.5).map(centre).collect();
            cs.sort_by(f64::total_cmp);
            let bands = 1 + cs.windows(2).filter(|p| p[1] - p[0] > cap * 1.1).count();
            let mut g = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
            for b in &gb {
                g = [g[0].min(b[0]), g[1].min(b[1]), g[2].max(b[2]), g[3].max(b[3])];
            }
            let height = if cap > short * 0.2 { cap } else { short * 0.6 };
            if !g[0].is_finite() || cap <= short * 0.2 {
                // No usable glyphs (a dash, a dot): centre the text in the box.
                let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                let half = height / 2.0;
                g = if horizontal {
                    [g[0].min(x1).max(x0), cy - half, g[2].max(x0).min(x1), cy + half]
                } else {
                    [cx - half, g[1].min(y1).max(y0), cx + half, g[3].max(y0).min(y1)]
                };
                if !g[0].is_finite() || !g[1].is_finite() {
                    g = [x0, y0, x1, y1];
                }
            }
            // Only rebuild when the glyphs clearly are this label: one line, filling the box, and no
            // other comment overlapping it. Anything else keeps its exact strokes.
            let (glen, gcap) = if horizontal { (g[2] - g[0], g[3] - g[1]) } else { (g[3] - g[1], g[2] - g[0]) };
            let long = w.max(h);
            let fills = glyphs.is_empty() || (glen >= long * 0.45 && gcap >= short * 0.3) || n <= 2;
            let crowded = overlaps.get(&li).copied().unwrap_or(false);
            if n > MAX_LABEL || bands > 1 || !fills || crowded {
                // Keep the strokes; add an invisible searchable copy.
                let (at, end, rot) = if horizontal {
                    (Vec2::new(x0, y0), Vec2::new(x1, y0), 0.0)
                } else {
                    (Vec2::new(x1, y0), Vec2::new(x1, y1), std::f64::consts::FRAC_PI_2)
                };
                out.push(Item {
                    shape: Shape::Text { at, end, height: short.min(0.5) * 0.5, rot, value: text },
                    color: None,
                    layer: L_SHX_SEARCH,
                    stroke: false,
                });
                for &i in &glyphs {
                    if let Some(t) = glyph_kept.get_mut(i) {
                        *t = true;
                    }
                }
                kept += 1;
                continue;
            }
            // Reading direction from the order the glyphs were drawn in.
            let backwards = match (gb.first(), gb.last()) {
                (Some(f), Some(l)) if gb.len() >= 2 => along(l) < along(f) - cap * 0.2,
                _ => false,
            };
            let color = glyphs.first().and_then(|i| items.get(*i)).and_then(|it| it.color);
            for &i in &glyphs {
                if let Some(t) = taken.get_mut(i) {
                    *t = true;
                }
            }
            let (at, end, rot) = match (horizontal, backwards) {
                (true, false) => (Vec2::new(g[0], g[1]), Vec2::new(g[2], g[1]), 0.0),
                (true, true) => (Vec2::new(g[2], g[3]), Vec2::new(g[0], g[3]), std::f64::consts::PI),
                (false, false) => (Vec2::new(g[2], g[1]), Vec2::new(g[2], g[3]), std::f64::consts::FRAC_PI_2),
                (false, true) => (Vec2::new(g[0], g[3]), Vec2::new(g[0], g[1]), -std::f64::consts::FRAC_PI_2),
            };
            out.push(Item { shape: Shape::Text { at, end, height, rot, value: text }, color, layer: L_SHX_TEXT, stroke: false });
            conv += 1;
        }
        for (i, it) in items.iter_mut().enumerate() {
            if taken.get(i).copied().unwrap_or(false) {
                it.layer = L_SHX_STROKES;
            } else if glyph_kept.get(i).copied().unwrap_or(false) {
                it.layer = L_SHX_GLYPHS;
            }
        }
        if items.len().saturating_add(out.len()) > MAX_ITEMS {
            return Err(bad(format!("more than {MAX_ITEMS} objects")));
        }
        items.extend(out);
        Ok((conv, kept))
    }

    // ---------- pages ----------

    fn page_box(doc: &Document, page: &Dictionary) -> [f64; 4] {
        let mut d = Some(page);
        let mut guard = 0;
        while let Some(dict) = d {
            for key in [&b"CropBox"[..], b"MediaBox"] {
                if let Some(a) = dict.get(key).ok().and_then(|o| doc.dereference(o).ok()).and_then(|(_, o)| o.as_array().ok()) {
                    let v: Vec<f64> = a.iter().filter_map(|o| doc.dereference(o).ok().and_then(|(_, o)| num(o))).collect();
                    if v.len() == 4 {
                        return [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])];
                    }
                }
            }
            guard += 1;
            if guard > 32 {
                break;
            }
            d = dict.get(b"Parent").ok().and_then(|o| doc.dereference(o).ok()).and_then(|(_, o)| o.as_dict().ok());
        }
        [0.0, 0.0, 612.0, 792.0]
    }

    fn page_rotate(doc: &Document, page: &Dictionary) -> i64 {
        let mut d = Some(page);
        let mut guard = 0;
        while let Some(dict) = d {
            if let Some(r) = dict.get(b"Rotate").ok().and_then(num) {
                return (r as i64).rem_euclid(360);
            }
            guard += 1;
            if guard > 32 {
                break;
            }
            d = dict.get(b"Parent").ok().and_then(|o| doc.dereference(o).ok()).and_then(|(_, o)| o.as_dict().ok());
        }
        0
    }

    /// User space → upright sheet inches, the sheet's lower-left corner at (`x_off`, 0).
    fn page_matrix(b: [f64; 4], rot: i64, x_off: f64) -> (M, f64) {
        let (w, h) = (b[2] - b[0], b[3] - b[1]);
        let t = M([1.0, 0.0, 0.0, 1.0, -b[0], -b[1]]);
        let (r, disp_w) = match rot {
            90 => (M([0.0, -1.0, 1.0, 0.0, 0.0, w]), h),
            180 => (M([-1.0, 0.0, 0.0, -1.0, w, h]), w),
            270 => (M([0.0, 1.0, -1.0, 0.0, h, 0.0]), h),
            _ => (M::I, w),
        };
        let s = M([1.0 / PT, 0.0, 0.0, 1.0 / PT, x_off, 0.0]);
        (t.then(r).then(s), disp_w / PT)
    }

    pub fn read(bytes: &[u8]) -> Result<Drawing> {
        let doc = Document::load_mem(bytes).map_err(|e| bad(format!("can't read this file ({e})")))?;
        if doc.is_encrypted() {
            return Err(bad("this PDF is password-protected"));
        }
        let pages = doc.get_pages();
        if pages.is_empty() {
            return Err(bad("no pages"));
        }
        let mut w = Walker { doc: &doc, page: M::I, items: Vec::new(), ops: 0, fonts: HashMap::new(), images: 0 };
        let mut labels = Vec::new();
        let mut x_off = 0.0;
        let mut n_pages = 0;
        let mut frames: Vec<[f64; 4]> = Vec::new();
        for (_, &pid) in pages.iter().take(MAX_PAGES) {
            let Ok(pdict) = doc.get_dictionary(pid) else { continue };
            let b = page_box(&doc, pdict);
            let rot = page_rotate(&doc, pdict);
            let (pm, width) = page_matrix(b, rot, x_off);
            w.page = pm;
            let content = doc.get_page_content_with_limit(pid, MAX_STREAM).unwrap_or_default();
            let res: Option<Dictionary> = match doc.get_page_resources(pid) {
                Ok((Some(d), _)) => Some(d.clone()),
                Ok((None, ids)) => ids.first().and_then(|id| doc.get_dictionary(*id).ok()).cloned(),
                Err(_) => None,
            };
            w.run(&content, res.as_ref(), M::I, 0)?;
            for a in doc.get_page_annotations(pid).unwrap_or_default() {
                let title = a.get(b"T").ok().and_then(pdf_text).unwrap_or_default();
                if !title.contains("SHX") {
                    continue;
                }
                let Some(text) = a.get(b"Contents").ok().and_then(pdf_text).map(|t| acad_codes(&t)) else { continue };
                let Some(r) = a.get(b"Rect").ok().and_then(|o| o.as_array().ok()).map(|v| v.iter().filter_map(num).collect::<Vec<_>>()) else {
                    continue;
                };
                if r.len() != 4 {
                    continue;
                }
                let (p, q) = (pm.ap(r[0], r[1]), pm.ap(r[2], r[3]));
                labels.push(Label { text, r: [p.x.min(q.x), p.y.min(q.y), p.x.max(q.x), p.y.max(q.y)] });
            }
            let height = match rot {
                90 | 270 => (b[2] - b[0]) / PT,
                _ => (b[3] - b[1]) / PT,
            };
            frames.push([x_off, 0.0, x_off + width, height]);
            x_off += width + PAGE_GAP;
            n_pages += 1;
        }
        let images = w.images;
        let mut items = w.items;
        let (conv, kept) = apply_labels(&mut items, &labels)?;
        build(items, &frames, n_pages, conv, kept, images)
    }

    fn build(items: Vec<Item>, frames: &[[f64; 4]], pages: usize, conv: usize, kept: usize, images: usize) -> Result<Drawing> {
        let mut d = Drawing::new_imperial();
        for (name, on, desc) in [
            (L_LINES, true, "Lines from the PDF"),
            (L_FILLS, true, "Filled shapes from the PDF"),
            (L_TEXT, true, "Text from the PDF's text layer"),
            (L_SHX_TEXT, true, "AutoCAD SHX labels rebuilt as real text"),
            (L_TEXT_HIDDEN, false, "Invisible (searchable) text from the PDF"),
            (L_SHX_GLYPHS, true, "Line-drawn (SHX) labels kept as drawn: wrapped, long or overlapping"),
            (L_SHX_STROKES, false, "The line strokes the SHX labels were drawn with"),
            (L_SHX_SEARCH, false, "Searchable copies of long SHX notes"),
            (L_WHITE, false, "White (paper-coloured) linework"),
            (L_PAGES, false, "Page outlines, in page order"),
        ] {
            d.layers.push(Layer { name: name.into(), color: Color::Index(7), on, description: desc.into(), ..Layer::default() });
        }
        for f in frames {
            let v = [Vec2::new(f[0], f[1]), Vec2::new(f[2], f[1]), Vec2::new(f[2], f[3]), Vec2::new(f[0], f[3])];
            let kind = EntityKind::LwPolyline(LwPolyline {
                vertices: v.into_iter().map(PolyVertex::new).collect(),
                closed: true,
                const_width: 0.0,
                elevation: 0.0,
                plinegen: false,
            });
            d.add(&Space::Model, Common { layer: L_PAGES.into(), ..Common::default() }, kind).map_err(|e| bad(e.to_string()))?;
        }
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for it in items {
            let kind = match it.shape {
                Shape::Line(a, b) => EntityKind::Line(Line { a: v3(a), b: v3(b) }),
                Shape::Poly(v, closed) => EntityKind::LwPolyline(LwPolyline {
                    vertices: v.into_iter().map(PolyVertex::new).collect(),
                    closed,
                    const_width: 0.0,
                    elevation: 0.0,
                    plinegen: false,
                }),
                Shape::Circle(c, r) => EntityKind::Circle(Circle { center: v3(c), radius: r }),
                Shape::Arc(c, r, s, e) => EntityKind::Arc(Arc { center: v3(c), radius: r, start: s, end: e }),
                Shape::Solid(v) => EntityKind::Solid(Solid { corners: v.map(v3) }),
                Shape::Fill(v) => EntityKind::Hatch(cadcraft_doc::Hatch {
                    pattern: "SOLID".into(),
                    solid: true,
                    loops: vec![cadcraft_doc::HatchLoop { vertices: v.into_iter().map(PolyVertex::new).collect(), outer: true, sources: Vec::new() }],
                    scale: 1.0,
                    angle: 0.0,
                    associative: false,
                    style: 0,
                    elevation: 0.0,
                    gradient: None,
                    origin: Vec2::ZERO,
                    background: None,
                    pattern_lines: Vec::new(),
                }),
                Shape::Text { at, end, height, rot, value } => {
                    let fit = dist(at, end) > height * 0.3;
                    EntityKind::Text(Text {
                        insert: v3(at),
                        align_pt: fit.then(|| v3(end)),
                        height,
                        value,
                        rotation: rot,
                        width_factor: 1.0,
                        oblique: 0.0,
                        style: "Standard".into(),
                        halign: if fit { HAlign::Fit } else { HAlign::Left },
                        valign: VAlign::Baseline,
                    })
                }
            };
            *counts.entry(kind.type_name()).or_default() += 1;
            let common = Common { layer: it.layer.into(), color: it.color.map(Color::True).unwrap_or(Color::ByLayer), ..Common::default() };
            d.add(&Space::Model, common, kind).map_err(|e| bad(e.to_string()))?;
        }
        let mut c: Vec<_> = counts.into_iter().collect();
        c.sort();
        let list = c.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ");
        d.read_notes.push(format!("PDF import: {pages} page(s); {list}; {conv} SHX labels rebuilt as text, {kept} long notes kept as strokes (searchable copy on {L_SHX_SEARCH})"));
        if images > 0 {
            d.read_notes.push(format!("PDF import: {images} raster image(s) not imported"));
        }
        Ok(d)
    }

    fn v3(p: Vec2) -> Vec3 {
        Vec3::new(p.x, p.y, 0.0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn acad_codes_are_cleaned() {
            assert_eq!(acad_codes("%%USHEET #:"), "SHEET #:");
            assert_eq!(acad_codes("45%%D 50%%% %%C2"), "45° 50% ⌀2");
        }

        #[test]
        fn rotate_270_is_upright() {
            let (m, w) = page_matrix([0.0, 0.0, 2592.0, 3456.0], 270, 0.0);
            assert!((w - 48.0).abs() < 1e-9);
            // Top-left of the unrotated page → bottom-left of the sheet.
            let p = m.ap(0.0, 3456.0);
            assert!(p.x.abs() < 1e-9 && p.y.abs() < 1e-9, "{p:?}");
            let q = m.ap(2592.0, 3456.0);
            assert!(q.x.abs() < 1e-9 && (q.y - 36.0).abs() < 1e-9, "{q:?}");
        }

        #[test]
        fn bezier_circle_detected() {
            let k = 0.552_284_75;
            let c = |x: f64, y: f64| Vec2::new(x, y);
            let sp = Sub {
                start: c(1.0, 0.0),
                segs: vec![
                    Seg::C(c(1.0, k), c(k, 1.0), c(0.0, 1.0)),
                    Seg::C(c(-k, 1.0), c(-1.0, k), c(-1.0, 0.0)),
                    Seg::C(c(-1.0, -k), c(-k, -1.0), c(0.0, -1.0)),
                    Seg::C(c(k, -1.0), c(1.0, -k), c(1.0, 0.0)),
                ],
                closed: true,
            };
            match as_round(&sp) {
                Some(Shape::Circle(cen, r)) => assert!(cen.x.abs() < 1e-3 && (r - 1.0).abs() < 1e-3),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn to_unicode_ranges() {
            let m = parse_to_unicode(b"1 beginbfchar <01> <0041> endbfchar 1 beginbfrange <02> <04> <0061> endbfrange");
            assert_eq!(m.get(&1).map(String::as_str), Some("A"));
            assert_eq!(m.get(&3).map(String::as_str), Some("b"));
        }

        #[test]
        fn garbage_is_an_error_not_a_panic() {
            assert!(read(b"%PDF-1.4 garbage").is_err());
            assert!(read(&[0u8; 64]).is_err());
        }
    }
}
