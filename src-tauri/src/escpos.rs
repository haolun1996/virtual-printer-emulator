//! ESC/POS stream parser + virtual printer state.
//!
//! Bytes are fed in arbitrary chunks (TCP); incomplete commands are buffered until the
//! rest arrives. The printer produces [`Event`]s: receipt updates (JSON, the contract with
//! `ui/index.html`), log lines and bytes to send back to the client (status replies).

use crate::qr;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const CP437_HIGH: &str = concat!(
    "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒ",
    "áíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐",
    "└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀",
    "αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}"
);

fn cp437(b: u8) -> char {
    static TABLE: OnceLock<Vec<char>> = OnceLock::new();
    let t = TABLE.get_or_init(|| CP437_HIGH.chars().collect());
    if b < 0x80 { b as char } else { t[(b - 0x80) as usize] }
}

/// ESC t n → WHATWG encoding label. Anything missing falls back to CP437.
fn codepage_label(n: u8) -> Option<&'static str> {
    Some(match n {
        16 => "windows-1252",
        17 => "ibm866",
        18 => "iso-8859-2",
        21 => "windows-874",
        45 => "windows-1250",
        46 => "windows-1251",
        47 => "windows-1253",
        48 => "windows-1254",
        49 => "windows-1255",
        50 => "windows-1256",
        51 => "windows-1257",
        52 => "windows-1258",
        _ => return None,
    })
}

/// Decode a text run: ASCII → GB18030 (Chinese mode) → valid UTF-8 → ESC t code page → CP437.
pub fn decode_text(bytes: &[u8], codepage: u8, kanji: bool) -> String {
    if bytes.iter().all(|&b| b < 0x80) {
        return bytes.iter().map(|&b| b as char).collect();
    }
    if kanji {
        return encoding_rs::GB18030.decode_without_bom_handling(bytes).0.into_owned();
    }
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    if let Some(enc) = codepage_label(codepage).and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        return enc.decode_without_bom_handling(bytes).0.into_owned();
    }
    bytes.iter().map(|&b| cp437(b)).collect()
}

fn decode_data(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

pub fn is_wide(cp: u32) -> bool {
    cp >= 0x1100
        && (cp <= 0x115f
            || (0x2e80..=0xa4cf).contains(&cp)
            || (0xac00..=0xd7a3).contains(&cp)
            || (0xf900..=0xfaff).contains(&cp)
            || (0xfe30..=0xfe4f).contains(&cp)
            || (0xff00..=0xff60).contains(&cp)
            || (0xffe0..=0xffe6).contains(&cp)
            || cp >= 0x20000)
}

/// CODE128 data in GS k carries code-set selectors ({A {B {C …); make it human readable.
fn code128_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut set_c = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' && i + 1 < bytes.len() {
            i += 1;
            match bytes[i] {
                b'C' => set_c = true,
                b'A' | b'B' => set_c = false,
                b'{' => out.push('{'),
                _ => {}
            }
        } else if set_c {
            out.push_str(&format!("{:02}", c));
        } else {
            out.push(c as char);
        }
        i += 1;
    }
    out
}

fn barcode_kind(m: u8) -> String {
    match m {
        0 | 65 => "UPC-A",
        1 | 66 => "UPC-E",
        2 | 67 => "EAN13",
        3 | 68 => "EAN8",
        4 | 69 => "CODE39",
        5 | 70 => "ITF",
        6 | 71 => "CODABAR",
        72 => "CODE93",
        73 => "CODE128",
        74 => "GS1-128",
        75..=78 => "GS1 DataBar",
        _ => return format!("barcode {m}"),
    }
    .to_string()
}

fn code2d_kind(cn: u8) -> String {
    match cn {
        48 => "PDF417",
        49 => "QR",
        50 => "MaxiCode",
        51 => "GS1 DataBar",
        52 => "Composite",
        53 => "Aztec",
        54 => "DataMatrix",
        _ => return format!("2D code {cn}"),
    }
    .to_string()
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

// ------------------------------------------------------------------ model

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    fn as_str(self) -> &'static str {
        match self {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Span {
    Text { t: String, font_b: bool, b: bool, u: u8, i: bool, inv: bool, w: u8, h: u8, cs: u8 },
    Space(u32),
}

#[derive(Clone, Debug)]
pub struct QrSymbol {
    pub size: u8,
    pub ec: char,
    pub modules: Option<(usize, Vec<u8>)>,
}

#[derive(Clone, Debug)]
pub enum Item {
    Text { align: Align, margin: u32, spacing: u32, spans: Vec<Span> },
    Feed { dots: u32 },
    Image { w: u32, h: u32, sx: u8, sy: u8, align: Align, data: Vec<u8>, band: bool },
    Barcode { kind: String, data: String, hri: u8, height: u32, module: u8, align: Align },
    Code2d { kind: String, data: String, align: Align, qr: Option<QrSymbol> },
    Note { text: String },
}

impl Span {
    fn to_json(&self) -> Value {
        match self {
            Span::Space(sp) => json!({ "sp": sp }),
            Span::Text { t, font_b, b, u, i, inv, w, h, cs } => {
                let mut o = Map::new();
                o.insert("t".into(), json!(t));
                o.insert("f".into(), json!(if *font_b { "B" } else { "A" }));
                if *b { o.insert("b".into(), json!(1)); }
                if *u > 0 { o.insert("u".into(), json!(u)); }
                if *i { o.insert("i".into(), json!(1)); }
                if *inv { o.insert("inv".into(), json!(1)); }
                if *w > 1 { o.insert("w".into(), json!(w)); }
                if *h > 1 { o.insert("h".into(), json!(h)); }
                if *cs > 0 { o.insert("cs".into(), json!(cs)); }
                Value::Object(o)
            }
        }
    }
}

impl Item {
    pub fn to_json(&self) -> Value {
        match self {
            Item::Text { align, margin, spacing, spans } => {
                let mut v = json!({ "type": "text", "align": align.as_str(), "spacing": spacing,
                    "spans": spans.iter().map(Span::to_json).collect::<Vec<_>>() });
                if *margin > 0 { v["margin"] = json!(margin); }
                v
            }
            Item::Feed { dots } => json!({ "type": "feed", "dots": dots }),
            Item::Image { w, h, sx, sy, align, data, .. } => json!({ "type": "image", "w": w, "h": h,
                "sx": sx, "sy": sy, "align": align.as_str(), "data": base64(data) }),
            Item::Barcode { kind, data, hri, height, module, align } => json!({ "type": "barcode", "kind": kind,
                "data": data, "hri": hri, "height": height, "module": module, "align": align.as_str() }),
            Item::Code2d { kind, data, align, qr } => {
                let mut v = json!({ "type": "code2d", "kind": kind, "data": data, "align": align.as_str() });
                if let Some(q) = qr {
                    v["size"] = json!(q.size);
                    v["ec"] = json!(q.ec.to_string());
                    if let Some((n, bits)) = &q.modules {
                        v["modules"] = json!(n);
                        v["matrix"] = json!(base64(bits));
                    }
                }
                v
            }
            Item::Note { text } => json!({ "type": "note", "text": text }),
        }
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Receipt {
    pub id: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub remote: String,
    pub source: &'static str,
    pub items: Vec<Item>,
    pub preview: Option<Item>,
    pub raw: Vec<u8>,
    pub done: bool,
    pub cut: bool,
}

impl Receipt {
    fn new(remote: &str, source: &'static str) -> Self {
        let now = now_ms();
        Receipt { id: None, created_at: now, updated_at: now, remote: remote.to_string(), source,
            items: Vec::new(), preview: None, raw: Vec::new(), done: false, cut: false }
    }

    /// True once there is something to show; assigns the id lazily so connections
    /// that only poll status don't burn ids.
    fn ensure_visible(&mut self) -> Option<u64> {
        if self.items.is_empty() && self.preview.is_none() {
            return None;
        }
        Some(*self.id.get_or_insert_with(|| NEXT_ID.fetch_add(1, Ordering::Relaxed)))
    }

    pub fn to_json(&self) -> Value {
        let mut items: Vec<Value> = self.items.iter().map(Item::to_json).collect();
        if let Some(p) = &self.preview {
            items.push(p.to_json());
        }
        json!({ "id": self.id, "createdAt": self.created_at, "updatedAt": self.updated_at,
            "remote": self.remote, "source": self.source, "bytes": self.raw.len(),
            "done": self.done, "cut": self.cut, "items": items })
    }
}

pub enum Event {
    /// Receipt changed. `raw` is only attached once the receipt is done.
    Update { id: u64, json: String, done: bool, cut: bool, raw: Option<Vec<u8>> },
    Log(String),
    Respond(Vec<u8>),
}

#[derive(Clone)]
struct State {
    font_b: bool,
    bold: bool,
    underline: u8,
    italic: bool,
    invert: bool,
    w: u8,
    h: u8,
    char_spacing: u8,
    align: Align,
    line_spacing: u32,
    codepage: u8,
    kanji: bool,
    left_margin: u32,
    tabs: Vec<u32>,
    hri: u8,
    barcode_height: u32,
    barcode_width: u8,
    qr_size: u8,
    qr_ec: char,
}

struct Line {
    spans: Vec<Span>,
    x: i64,
    align: Option<Align>,
    margin: u32,
}

impl Line {
    fn new() -> Self {
        Line { spans: Vec::new(), x: 0, align: None, margin: 0 }
    }
}

struct Bitmap {
    w: u32,
    h: u32,
    sx: u8,
    sy: u8,
    data: Vec<u8>,
}

const FONT_A_W: u32 = 12;
const FONT_B_W: u32 = 9;
const MAX_PENDING: usize = 8 * 1024 * 1024;

pub struct Printer {
    remote: String,
    source: &'static str,
    chinese_mode: bool,
    pending: Vec<u8>,
    text: Vec<u8>,
    s: State,
    line: Line,
    receipt: Receipt,
    band_pending: bool,
    band_height: u32,
    cut_requested: bool,
    graphics: Option<Bitmap>,
    downloaded: Option<Bitmap>,
    code2d: [Option<Vec<u8>>; 8],
    pub events: Vec<Event>,
}

impl Printer {
    pub fn new(remote: &str, source: &'static str, chinese_mode: bool) -> Self {
        let mut p = Printer {
            remote: remote.to_string(),
            source,
            chinese_mode,
            pending: Vec::new(),
            text: Vec::new(),
            s: Self::default_state(chinese_mode),
            line: Line::new(),
            receipt: Receipt::new(remote, source),
            band_pending: false,
            band_height: 0,
            cut_requested: false,
            graphics: None,
            downloaded: None,
            code2d: Default::default(),
            events: Vec::new(),
        };
        p.reset();
        p
    }

    fn default_state(chinese_mode: bool) -> State {
        State {
            font_b: false, bold: false, underline: 0, italic: false, invert: false, w: 1, h: 1,
            char_spacing: 0, align: Align::Left, line_spacing: 30, codepage: 0, kanji: chinese_mode,
            left_margin: 0, tabs: (1..=32).map(|k| k * 8 * FONT_A_W).collect(),
            hri: 0, barcode_height: 162, barcode_width: 3, qr_size: 3, qr_ec: 'L',
        }
    }

    fn reset(&mut self) {
        self.s = Self::default_state(self.chinese_mode);
        self.band_pending = false;
    }

    fn log(&mut self, msg: impl Into<String>) {
        self.events.push(Event::Log(msg.into()));
    }

    fn respond(&mut self, bytes: &[u8]) {
        self.events.push(Event::Respond(bytes.to_vec()));
    }

    // ---------------------------------------------------------------- input

    pub fn feed(&mut self, chunk: &[u8]) {
        let mut b = std::mem::take(&mut self.pending);
        b.extend_from_slice(chunk);
        let mut i = 0;
        loop {
            while i < b.len() {
                let n = self.parse_one(&b, i);
                if n == 0 {
                    break;
                }
                // Raw bytes belong to the receipt that was current *before* this command.
                self.receipt.raw.extend_from_slice(&b[i..i + n]);
                i += n;
                if self.cut_requested {
                    // finalize only after the cut command's bytes are attached
                    self.cut_requested = false;
                    self.finalize(true);
                }
            }
            if b.len() - i <= MAX_PENDING {
                break;
            }
            self.log(format!("dropping a byte: command at offset {i} never completed"));
            i += 1;
        }
        self.pending = b[i..].to_vec();
    }

    pub fn end(&mut self) {
        if !self.pending.is_empty() {
            let n = self.pending.len();
            self.log(format!("{n} trailing byte(s) of an incomplete command were ignored"));
            let p = std::mem::take(&mut self.pending);
            self.receipt.raw.extend_from_slice(&p);
        }
        self.finalize(false);
    }

    /// Parse one command/text run at b[i]. Returns bytes consumed, or 0 if incomplete.
    fn parse_one(&mut self, b: &[u8], i: usize) -> usize {
        let c = b[i];
        if c >= 0x20 {
            let mut j = i;
            while j < b.len() && b[j] >= 0x20 {
                j += 1;
            }
            self.text.extend_from_slice(&b[i..j]);
            return j - i;
        }
        self.flush_text();
        match c {
            0x0a | 0x0c => {
                // LF, FF (page mode print) ≈ LF
                self.commit_line(None);
                1
            }
            0x09 => {
                self.tab();
                1
            }
            0x1b => self.esc(b, i),
            0x1d => self.gs(b, i),
            0x1c => self.fs(b, i),
            0x10 => self.dle(b, i),
            _ => 1, // CR and other controls: ignored
        }
    }

    // ---------------------------------------------------------------- ESC

    fn esc(&mut self, b: &[u8], i: usize) -> usize {
        let rem = b.len() - i;
        if rem < 2 {
            return 0;
        }
        macro_rules! need {
            ($n:expr) => {
                if rem < $n {
                    return 0;
                }
            };
        }
        let n = if rem > 2 { b[i + 2] } else { 0 };
        match b[i + 1] {
            0x40 => {
                // ESC @ initialize (clears the print buffer)
                self.text.clear();
                self.line = Line::new();
                self.reset();
                2
            }
            0x20 => { need!(3); self.s.char_spacing = n; 3 }
            0x21 => {
                need!(3);
                self.s.font_b = n & 1 != 0;
                self.s.bold = n & 8 != 0;
                self.s.h = if n & 16 != 0 { 2 } else { 1 };
                self.s.w = if n & 32 != 0 { 2 } else { 1 };
                self.s.underline = if n & 128 != 0 { 1 } else { 0 };
                3
            }
            0x24 => { need!(4); self.move_to(n as i64 + b[i + 3] as i64 * 256); 4 }
            0x5c => {
                need!(4);
                let rel = (n as u16 | (b[i + 3] as u16) << 8) as i16 as i64;
                self.move_to(self.line.x + rel);
                4
            }
            // 1-arg commands without visible effect
            0x25 | 0x3d | 0x3f | 0x52 | 0x54 | 0x56 | 0x72 | 0x7b | 0x43 | 0x6c | 0x4b | 0x65 => { need!(3); 3 }
            0x2d => { need!(3); self.s.underline = match n { 1 | 49 => 1, 2 | 50 => 2, _ => 0 }; 3 }
            0x32 => { self.s.line_spacing = 30; 2 }
            0x33 => { need!(3); self.s.line_spacing = n as u32; 3 }
            0x34 => { need!(3); self.s.italic = n & 1 != 0; 3 } // ESC 4 (italic, non-Epson)
            0x35 => { need!(3); self.s.italic = false; 3 }
            0x45 | 0x47 => { need!(3); self.s.bold = n & 1 != 0; 3 }
            0x4a => { need!(3); self.commit_line(Some(n as u32)); 3 }
            0x64 => { need!(3); self.feed_lines(n as u32); 3 }
            0x4d => { need!(3); self.s.font_b = n & 0x0f != 0; 3 }
            0x61 => {
                need!(3);
                self.s.align = match n % 48 { 1 => Align::Center, 2 => Align::Right, _ => Align::Left };
                3
            }
            0x74 => { need!(3); self.s.codepage = n; 3 }
            0x69 | 0x6d => { self.cut(); 2 }
            0x70 => { need!(5); 5 } // drawer kick
            0x63 | 0x42 => { need!(4); 4 }
            0x57 => { need!(10); 10 }
            0x75 => { need!(3); self.respond(&[0x00]); 3 }
            0x76 => { self.respond(&[0x00]); 2 }
            0x44 => {
                // ESC D n1..nk NUL — tab stops (in character widths)
                match b[i + 2..].iter().position(|&c| c == 0) {
                    Some(p) if p <= 32 => {
                        let cw = if self.s.font_b { FONT_B_W } else { FONT_A_W } + self.s.char_spacing as u32;
                        self.s.tabs = b[i + 2..i + 2 + p].iter().map(|&c| c as u32 * cw).collect();
                        p + 3
                    }
                    _ => if rem > 34 { 34 } else { 0 },
                }
            }
            0x2a => {
                // ESC * m nL nH d… — bit image band (column format)
                need!(5);
                let m = n;
                let cols = b[i + 3] as usize + b[i + 4] as usize * 256;
                let bpc = if m >= 32 { 3 } else { 1 };
                let len = 5 + cols * bpc;
                need!(len);
                let data = b[i + 5..i + len].to_vec();
                self.bit_image_band(m, cols, &data);
                len
            }
            0x26 => {
                // ESC & y c1 c2 [x d…]… — user-defined characters (skipped)
                need!(5);
                let (y, c1, c2) = (b[i + 2] as usize, b[i + 3], b[i + 4]);
                let mut p = i + 5;
                for _ in c1..=c2 {
                    if p >= b.len() {
                        return 0;
                    }
                    p += 1 + y * b[p] as usize;
                }
                if p <= b.len() { p - i } else { 0 }
            }
            0x28 => {
                need!(5);
                let len = 5 + b[i + 3] as usize + b[i + 4] as usize * 256;
                need!(len);
                len
            }
            _ => 2,
        }
    }

    // ---------------------------------------------------------------- GS

    fn gs(&mut self, b: &[u8], i: usize) -> usize {
        let rem = b.len() - i;
        if rem < 2 {
            return 0;
        }
        macro_rules! need {
            ($n:expr) => {
                if rem < $n {
                    return 0;
                }
            };
        }
        let n = if rem > 2 { b[i + 2] } else { 0 };
        match b[i + 1] {
            0x21 => { need!(3); self.s.w = ((n >> 4) & 7) + 1; self.s.h = (n & 7) + 1; 3 }
            0x42 => { need!(3); self.s.invert = n & 1 != 0; 3 }
            0x48 => { need!(3); self.s.hri = n % 48; 3 }
            0x68 => { need!(3); self.s.barcode_height = (n as u32).max(1); 3 }
            0x77 => { need!(3); self.s.barcode_width = n; 3 }
            0x4c => { need!(4); self.s.left_margin = n as u32 + b[i + 3] as u32 * 256; 4 }
            0x66 | 0x62 | 0x54 | 0x45 | 0x6a | 0x5a | 0x63 => { need!(3); 3 }
            0x57 | 0x50 | 0x24 | 0x5c => { need!(4); 4 }
            0x5e => { need!(5); 5 }
            0x3a => 2,
            0x56 => {
                // GS V — cut
                need!(3);
                let len = if matches!(n, 65 | 66 | 97 | 98 | 103 | 104) { 4 } else { 3 };
                need!(len);
                self.cut();
                len
            }
            0x76 => {
                // GS v 0 m xL xH yL yH d… — raster image
                need!(8);
                let m = b[i + 3];
                let xb = b[i + 4] as usize + b[i + 5] as usize * 256;
                let h = b[i + 6] as usize + b[i + 7] as usize * 256;
                let len = 8 + xb * h;
                need!(len);
                self.print_image(Bitmap { w: (xb * 8) as u32, h: h as u32,
                    sx: if m & 1 != 0 { 2 } else { 1 }, sy: if m & 2 != 0 { 2 } else { 1 },
                    data: b[i + 8..i + len].to_vec() }, false);
                len
            }
            0x2a => {
                // GS * x y d… — define downloaded bit image
                need!(4);
                let (x, y) = (b[i + 2] as usize, b[i + 3] as usize);
                let len = 4 + x * y * 8;
                need!(len);
                self.downloaded = Some(columns_to_rows(&b[i + 4..i + len], x * 8, y));
                len
            }
            0x2f => {
                // GS / m — print downloaded bit image
                need!(3);
                if let Some(d) = &self.downloaded {
                    let m = n % 48;
                    let img = Bitmap { w: d.w, h: d.h, sx: if m & 1 != 0 { 2 } else { 1 },
                        sy: if m & 2 != 0 { 2 } else { 1 }, data: d.data.clone() };
                    self.print_image(img, false);
                }
                3
            }
            0x6b => {
                // GS k — barcode
                need!(3);
                let m = n;
                let (data, len) = if m <= 6 {
                    match b[i + 3..].iter().position(|&c| c == 0) {
                        Some(p) => (b[i + 3..i + 3 + p].to_vec(), p + 4),
                        None => return if rem > 3 + 255 { 3 } else { 0 },
                    }
                } else {
                    need!(4);
                    let len = 4 + b[i + 3] as usize;
                    need!(len);
                    (b[i + 4..i + len].to_vec(), len)
                };
                self.barcode(m, &data);
                len
            }
            0x28 => {
                // GS ( X pL pH …
                need!(5);
                let p = b[i + 3] as usize + b[i + 4] as usize * 256;
                need!(5 + p);
                let body = b[i + 5..i + 5 + p].to_vec();
                match n {
                    0x4c => self.graphics_cmd(&body), // GS ( L
                    0x6b => self.code2d_cmd(&body),   // GS ( k
                    _ => {}
                }
                5 + p
            }
            0x38 => {
                // GS 8 L p1 p2 p3 p4 … — graphics (large)
                need!(7);
                let p = u32::from_le_bytes([b[i + 3], b[i + 4], b[i + 5], b[i + 6]]) as usize;
                need!(7 + p);
                if n == 0x4c {
                    let body = b[i + 7..i + 7 + p].to_vec();
                    self.graphics_cmd(&body);
                }
                7 + p
            }
            0x49 => {
                // GS I n — printer ID
                need!(3);
                let s = |t: &str| {
                    let mut v = vec![b'_'];
                    v.extend_from_slice(t.as_bytes());
                    v.push(0);
                    v
                };
                let reply: Option<Vec<u8>> = match if n >= 48 && n <= 51 { n - 48 } else { n } {
                    1 => Some(vec![0x20]),
                    2 => Some(vec![0x02]),
                    3 => Some(vec![0x64]),
                    65 => Some(s("1.00")),
                    66 => Some(s("VIRTUAL")),
                    67 => Some(s("ESCPOS-EMULATOR")),
                    68 => Some(s("00000001")),
                    69 => Some(s("ANK")),
                    _ => None,
                };
                if let Some(r) = reply {
                    self.respond(&r);
                }
                self.log(format!("GS I {n} (printer ID query) answered"));
                3
            }
            0x72 => {
                need!(3);
                self.respond(&[0x00]);
                self.log(format!("GS r {n} (status query) → 0x00 (paper OK)"));
                3
            }
            0x61 => {
                need!(3);
                if n != 0 {
                    self.respond(&[0x10, 0x00, 0x00, 0x00]);
                }
                3
            }
            _ => 2,
        }
    }

    // ---------------------------------------------------------------- FS

    fn fs(&mut self, b: &[u8], i: usize) -> usize {
        let rem = b.len() - i;
        if rem < 2 {
            return 0;
        }
        macro_rules! need {
            ($n:expr) => {
                if rem < $n {
                    return 0;
                }
            };
        }
        match b[i + 1] {
            0x26 => { self.s.kanji = true; 2 }  // FS & — Chinese/Kanji mode on
            0x2e => { self.s.kanji = false; 2 } // FS . — off
            0x21 | 0x2d | 0x43 | 0x57 => { need!(3); 3 }
            0x53 => { need!(4); 4 }
            0x32 => { need!(76); 76 } // FS 2 — user kanji 24x24
            0x70 => {
                // FS p n m — print NV bit image (logo stored in printer)
                need!(4);
                self.flush_line_if_any();
                let n = b[i + 2];
                self.receipt.items.push(Item::Note { text: format!("[stored NV logo #{n}]") });
                4
            }
            0x71 => {
                // FS q n [xL xH yL yH d…]… — define NV images (skipped)
                need!(3);
                let mut p = i + 3;
                for _ in 0..b[i + 2] {
                    if p + 4 > b.len() {
                        return 0;
                    }
                    p += 4 + (b[p] as usize + b[p + 1] as usize * 256) * (b[p + 2] as usize + b[p + 3] as usize * 256) * 8;
                }
                if p <= b.len() { p - i } else { 0 }
            }
            0x28 => {
                need!(5);
                let len = 5 + b[i + 3] as usize + b[i + 4] as usize * 256;
                need!(len);
                len
            }
            _ => 2,
        }
    }

    // ---------------------------------------------------------------- DLE (real-time)

    fn dle(&mut self, b: &[u8], i: usize) -> usize {
        let rem = b.len() - i;
        if rem < 2 {
            return 0;
        }
        match b[i + 1] {
            0x04 => {
                // DLE EOT n — real-time status
                if rem < 3 {
                    return 0;
                }
                self.respond(&[0x12]);
                self.log(format!("DLE EOT {} (status query) → 0x12 (online, no errors)", b[i + 2]));
                3
            }
            0x05 => if rem >= 3 { 3 } else { 0 },
            0x14 => {
                if rem < 3 {
                    return 0;
                }
                let len = if b[i + 2] == 8 { 10 } else { 5 };
                if rem >= len { len } else { 0 }
            }
            _ => 1,
        }
    }

    // ---------------------------------------------------------------- printing model

    fn touch_line(&mut self) {
        if self.line.align.is_none() {
            self.line.align = Some(self.s.align);
            self.line.margin = self.s.left_margin;
        }
    }

    fn make_span(&self, t: String) -> (Span, i64) {
        let s = &self.s;
        let fw = if s.font_b { FONT_B_W } else { FONT_A_W } as i64;
        let width: i64 = t
            .chars()
            .map(|ch| ((if is_wide(ch as u32) { 2 } else { 1 }) * fw + s.char_spacing as i64) * s.w as i64)
            .sum();
        (Span::Text { t, font_b: s.font_b, b: s.bold, u: s.underline, i: s.italic, inv: s.invert,
            w: s.w, h: s.h, cs: s.char_spacing }, width)
    }

    fn flush_text(&mut self) {
        if self.text.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.text);
        let (span, width) = self.make_span(decode_text(&bytes, self.s.codepage, self.s.kanji));
        self.touch_line();
        self.line.spans.push(span);
        self.line.x += width;
    }

    fn move_to(&mut self, target: i64) {
        self.flush_text();
        self.touch_line();
        if target > self.line.x {
            self.line.spans.push(Span::Space((target - self.line.x) as u32));
            self.line.x = target;
        }
    }

    fn tab(&mut self) {
        if let Some(&next) = self.s.tabs.iter().find(|&&t| t as i64 > self.line.x) {
            self.move_to(next as i64);
        }
    }

    fn push_feed(&mut self, dots: i64) {
        if dots <= 0 {
            return;
        }
        if let Some(Item::Feed { dots: d }) = self.receipt.items.last_mut() {
            *d += dots as u32;
        } else {
            self.receipt.items.push(Item::Feed { dots: dots as u32 });
        }
    }

    /// Print the current line and advance `spacing` dots (default: line spacing).
    fn commit_line(&mut self, spacing: Option<u32>) {
        let spacing = spacing.unwrap_or(self.s.line_spacing);
        self.flush_text();
        if !self.line.spans.is_empty() {
            let line = std::mem::replace(&mut self.line, Line::new());
            self.receipt.items.push(Item::Text { align: line.align.unwrap_or(self.s.align),
                margin: line.margin, spacing, spans: line.spans });
            self.band_pending = false;
        } else if self.band_pending {
            // LF right after an ESC * band: the band itself already used up its height.
            self.band_pending = false;
            self.push_feed(spacing as i64 - self.band_height as i64);
        } else {
            self.push_feed(spacing as i64);
        }
        self.line = Line::new();
    }

    fn feed_lines(&mut self, n: u32) {
        let sp = self.s.line_spacing;
        if !self.line.spans.is_empty() || self.band_pending {
            self.commit_line(Some(if n == 0 { 0 } else { sp }));
            self.push_feed(n.saturating_sub(1) as i64 * sp as i64);
        } else {
            self.push_feed(n as i64 * sp as i64);
        }
    }

    fn flush_line_if_any(&mut self) {
        self.flush_text();
        if !self.line.spans.is_empty() {
            self.commit_line(None);
        }
        self.band_pending = false;
    }

    fn print_image(&mut self, img: Bitmap, band: bool) {
        if img.w == 0 || img.h == 0 {
            return;
        }
        let align = self.s.align;
        if !self.line.spans.is_empty() || !self.text.is_empty() {
            self.commit_line(None);
        }
        let can_merge = band && !self.band_pending;
        let merged = match self.receipt.items.last_mut() {
            Some(Item::Image { w, h, sx, sy, align: a, data, band: true })
                if can_merge && *w == img.w && *sx == img.sx && *sy == img.sy && *a == align =>
            {
                data.extend_from_slice(&img.data); // stack consecutive bands into one image
                *h += img.h;
                true
            }
            _ => false,
        };
        if !merged {
            self.receipt.items.push(Item::Image { w: img.w, h: img.h, sx: img.sx, sy: img.sy, align,
                data: img.data, band });
        }
        self.band_pending = band;
        self.band_height = img.h * img.sy as u32;
    }

    fn bit_image_band(&mut self, m: u8, cols: usize, data: &[u8]) {
        let bpc = if m >= 32 { 3 } else { 1 };
        let mut img = columns_to_rows(data, cols, bpc);
        img.sx = if m == 0 || m == 32 { 2 } else { 1 };
        img.sy = if m < 32 { 3 } else { 1 };
        self.print_image(img, true);
    }

    fn graphics_cmd(&mut self, body: &[u8]) {
        let p = body.len();
        if p < 2 {
            return;
        }
        match body[1] {
            112 => {
                // store raster graphics in print buffer
                if p < 10 {
                    return;
                }
                let (tone, bx, by, color) = (body[2], body[3], body[4], body[5]);
                let w = body[6] as usize + body[7] as usize * 256;
                let h = body[8] as usize + body[9] as usize * 256;
                let len = w.div_ceil(8) * h;
                if 10 + len > p {
                    return;
                }
                if tone != 48 {
                    self.log("multi-tone graphics are not supported");
                    return;
                }
                if color == 49 || color == 48 || self.graphics.is_none() {
                    self.graphics = Some(Bitmap { w: w as u32, h: h as u32, sx: if bx == 2 { 2 } else { 1 },
                        sy: if by == 2 { 2 } else { 1 }, data: body[10..10 + len].to_vec() });
                }
            }
            50 | 2 => {
                // print buffered graphics
                if let Some(g) = self.graphics.take() {
                    self.print_image(g, false);
                }
            }
            69 | 85 => {
                // print NV / download graphics by key
                self.flush_line_if_any();
                self.receipt.items.push(Item::Note { text: "[stored graphics]".into() });
            }
            _ => {}
        }
    }

    fn code2d_cmd(&mut self, body: &[u8]) {
        let p = body.len();
        if p < 2 {
            return;
        }
        let (cn, fnc) = (body[0], body[1]);
        let slot = (cn as usize).wrapping_sub(48);
        match (cn, fnc) {
            (49, 67) if p > 2 => self.s.qr_size = body[2],
            (49, 69) if p > 2 => self.s.qr_ec = match body[2] { 49 => 'M', 50 => 'Q', 51 => 'H', _ => 'L' },
            (_, 80) if slot < 8 => self.code2d[slot] = Some(body.get(3..).unwrap_or(&[]).to_vec()),
            (_, 81) => {
                self.flush_line_if_any();
                let bytes = if slot < 8 { self.code2d[slot].clone().unwrap_or_default() } else { Vec::new() };
                let mut qr = None;
                if cn == 49 {
                    let mut sym = QrSymbol { size: self.s.qr_size, ec: self.s.qr_ec, modules: None };
                    match qr::encode(&bytes, sym.ec) {
                        Ok(q) => sym.modules = Some((q.size, qr::pack(&q))),
                        Err(e) => self.log(format!("QR not rendered: {e}")),
                    }
                    qr = Some(sym);
                }
                self.receipt.items.push(Item::Code2d { kind: code2d_kind(cn), data: decode_data(&bytes),
                    align: self.s.align, qr });
            }
            _ => {}
        }
    }

    fn barcode(&mut self, m: u8, bytes: &[u8]) {
        self.flush_line_if_any();
        let kind = barcode_kind(m);
        let data = if kind == "CODE128" || kind == "GS1-128" {
            code128_text(bytes)
        } else {
            bytes.iter().map(|&b| b as char).collect()
        };
        self.receipt.items.push(Item::Barcode { kind, data, hri: self.s.hri, height: self.s.barcode_height,
            module: self.s.barcode_width, align: self.s.align });
    }

    fn cut(&mut self) {
        self.flush_text();
        if !self.receipt.items.is_empty() || !self.line.spans.is_empty() {
            self.cut_requested = true;
        }
    }

    fn finalize(&mut self, cut: bool) {
        self.flush_text();
        if !self.line.spans.is_empty() {
            self.commit_line(None);
        }
        let mut r = std::mem::replace(&mut self.receipt, Receipt::new(&self.remote, self.source));
        r.cut = cut;
        r.done = true;
        r.preview = None;
        r.updated_at = now_ms();
        if let Some(id) = r.ensure_visible() {
            let json = r.to_json().to_string();
            self.events.push(Event::Update { id, json, done: true, cut, raw: Some(r.raw) });
        }
        self.line = Line::new();
        self.band_pending = false;
    }

    /// Publish the in-progress receipt, including the not-yet-terminated line.
    pub fn emit(&mut self) {
        let mut spans = self.line.spans.clone();
        if !self.text.is_empty() {
            let t = decode_text(&self.text, self.s.codepage, self.s.kanji);
            spans.push(self.make_span(t).0);
        }
        self.receipt.preview = if spans.is_empty() {
            None
        } else {
            Some(Item::Text { align: self.line.align.unwrap_or(self.s.align), margin: self.line.margin,
                spacing: self.s.line_spacing, spans })
        };
        self.receipt.updated_at = now_ms();
        if let Some(id) = self.receipt.ensure_visible() {
            let json = self.receipt.to_json().to_string();
            self.events.push(Event::Update { id, json, done: false, cut: false, raw: None });
        }
    }
}

/// Convert column-format bit image (bpc bytes per column, MSB = top) to packed rows.
fn columns_to_rows(data: &[u8], cols: usize, bpc: usize) -> Bitmap {
    let h = bpc * 8;
    let bpr = cols.div_ceil(8);
    let mut out = vec![0u8; bpr * h];
    for x in 0..cols {
        for k in 0..bpc {
            let byte = *data.get(x * bpc + k).unwrap_or(&0);
            if byte == 0 {
                continue;
            }
            for j in 0..8 {
                if byte & (0x80 >> j) != 0 {
                    out[(k * 8 + j) * bpr + (x >> 3)] |= 0x80 >> (x & 7);
                }
            }
        }
    }
    Bitmap { w: cols as u32, h: h as u32, sx: 1, sy: 1, data: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(job: &[u8], chunk: usize) -> (Vec<serde_json::Value>, Vec<Vec<u8>>, usize) {
        let mut p = Printer::new("test", "tcp", false);
        let mut done = Vec::new();
        let mut replies = Vec::new();
        let mut raw_total = 0;
        let mut drain = |p: &mut Printer, done: &mut Vec<Value>, raw_total: &mut usize| {
            for e in p.events.drain(..) {
                match e {
                    Event::Update { json, done: true, raw, .. } => {
                        *raw_total += raw.map(|r| r.len()).unwrap_or(0);
                        done.push(serde_json::from_str(&json).unwrap());
                    }
                    Event::Respond(b) => replies.push(b),
                    _ => {}
                }
            }
        };
        for c in job.chunks(chunk) {
            p.feed(c);
            p.emit();
            drain(&mut p, &mut done, &mut raw_total);
        }
        p.end();
        drain(&mut p, &mut done, &mut raw_total);
        (done, replies, raw_total)
    }

    #[test]
    fn sample_receipt_chunked() {
        let mut job = vec![0x10, 0x04, 0x01];
        job.extend(crate::sample::build_sample_receipt(48));
        job.extend(b"second receipt\n");
        job.extend([0x1b, 0x2a, 33, 2, 0, 0xff, 0, 0x0f, 0x80, 0x80, 0x80, b'\n']);
        for chunk in [1, 7, 4096] {
            let (done, replies, raw_total) = run(&job, chunk);
            assert_eq!(replies, vec![vec![0x12]]);
            assert_eq!(done.len(), 2, "chunk {chunk}");
            assert_eq!(raw_total, job.len(), "raw bytes must add up (chunk {chunk})");
            let first = &done[0];
            assert_eq!(first["cut"], true);
            let kinds: Vec<&str> = first["items"].as_array().unwrap().iter().map(|i| i["type"].as_str().unwrap()).collect();
            assert!(kinds.contains(&"image") && kinds.contains(&"barcode") && kinds.contains(&"code2d"));
            let qr = first["items"].as_array().unwrap().iter().find(|i| i["type"] == "code2d").unwrap();
            assert_eq!(qr["data"], "https://example.com/receipt/1042");
            assert!(qr["matrix"].is_string());
            let band = done[1]["items"].as_array().unwrap().iter().find(|i| i["type"] == "image").unwrap();
            assert_eq!((band["w"].as_u64(), band["h"].as_u64()), (Some(2), Some(24)));
        }
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text("谢谢".as_bytes(), 0, false), "谢谢");
        assert_eq!(decode_text(&[0xd0, 0xbb, 0xd0, 0xbb], 0, true), "谢谢"); // GB18030
        assert_eq!(decode_text(&[0x80, 0x9c], 0, false), "Ç£"); // CP437
        assert_eq!(decode_text(&[0x80], 16, false), "€"); // WPC1252
        assert_eq!(base64(b"hello!?"), "aGVsbG8hPw==");
    }
}
