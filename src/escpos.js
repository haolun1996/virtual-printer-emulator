'use strict';
/**
 * ESC/POS stream parser + virtual printer state.
 *
 * Bytes are fed in arbitrary chunks (TCP); incomplete commands are buffered
 * until the rest arrives. Output is a list of "receipts", each a list of items:
 *   { type: 'text',    align, margin, spacing, spans: [...] }
 *   { type: 'feed',    dots }
 *   { type: 'image',   w, h, sx, sy, align, data (1-bit packed rows) }
 *   { type: 'barcode', kind, data, hri, height, module, align }
 *   { type: 'code2d',  kind, data, size, ec, align }
 *   { type: 'note',    text }
 * Text spans: { t, f: 'A'|'B', b, u, i, inv, w, h, cs } or a spacer { sp: dots }.
 */

const { encodeQR, packMatrix } = require('./qr');

const CP437_HIGH =
  'ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒ' +
  'áíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐' +
  '└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀' +
  'αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■ ';
if (CP437_HIGH.length !== 128) throw new Error('CP437 table must have 128 entries');

// ESC t n → TextDecoder label. Anything missing falls back to CP437.
const CODEPAGES = {
  16: 'windows-1252', 17: 'ibm866', 18: 'iso-8859-2', 21: 'windows-874',
  45: 'windows-1250', 46: 'windows-1251', 47: 'windows-1253', 48: 'windows-1254',
  49: 'windows-1255', 50: 'windows-1256', 51: 'windows-1257', 52: 'windows-1258',
};

const FONTS = { A: { w: 12, h: 24 }, B: { w: 9, h: 17 } };

const BARCODES = {
  0: 'UPC-A', 1: 'UPC-E', 2: 'EAN13', 3: 'EAN8', 4: 'CODE39', 5: 'ITF', 6: 'CODABAR',
  65: 'UPC-A', 66: 'UPC-E', 67: 'EAN13', 68: 'EAN8', 69: 'CODE39', 70: 'ITF', 71: 'CODABAR',
  72: 'CODE93', 73: 'CODE128', 74: 'GS1-128', 75: 'GS1 DataBar', 76: 'GS1 DataBar',
  77: 'GS1 DataBar', 78: 'GS1 DataBar',
};
const CODES_2D = { 48: 'PDF417', 49: 'QR', 50: 'MaxiCode', 51: 'GS1 DataBar', 52: 'Composite', 53: 'Aztec', 54: 'DataMatrix' };

const decoders = new Map();
function decoder(label) {
  if (!decoders.has(label)) {
    try { decoders.set(label, new TextDecoder(label)); } catch { decoders.set(label, null); }
  }
  return decoders.get(label);
}
const utf8Strict = new TextDecoder('utf-8', { fatal: true });

/**
 * Decode a text run. Order: plain ASCII → Chinese (GB18030) when FS & is active →
 * valid UTF-8 (many apps send UTF-8 to emulators) → selected code page → CP437.
 */
function decodeText(bytes, codepage, kanji) {
  if (!bytes.some((b) => b >= 0x80)) return bytes.toString('latin1');
  if (kanji) return decoder('gb18030').decode(bytes);
  try { return utf8Strict.decode(bytes); } catch { /* not UTF-8 */ }
  const d = CODEPAGES[codepage] && decoder(CODEPAGES[codepage]);
  if (d) return d.decode(bytes);
  let s = '';
  for (const b of bytes) s += b < 0x80 ? String.fromCharCode(b) : CP437_HIGH[b - 0x80];
  return s;
}

function decodeData(bytes) {
  try { return utf8Strict.decode(bytes); } catch { return Buffer.from(bytes).toString('latin1'); }
}

function isWide(cp) {
  return cp >= 0x1100 && (
    cp <= 0x115f || (cp >= 0x2e80 && cp <= 0xa4cf) || (cp >= 0xac00 && cp <= 0xd7a3) ||
    (cp >= 0xf900 && cp <= 0xfaff) || (cp >= 0xfe30 && cp <= 0xfe4f) ||
    (cp >= 0xff00 && cp <= 0xff60) || (cp >= 0xffe0 && cp <= 0xffe6) || cp >= 0x20000);
}

/** CODE128 data in GS k carries code-set selectors ({A {B {C ...); make it human readable. */
function code128Text(bytes) {
  let out = '';
  let setC = false;
  for (let i = 0; i < bytes.length; i++) {
    const c = bytes[i];
    if (c === 0x7b && i + 1 < bytes.length) { // '{'
      const n = String.fromCharCode(bytes[++i]);
      if (n === 'C') setC = true;
      else if (n === 'A' || n === 'B') setC = false;
      else if (n === '{') out += '{';
      continue;
    }
    out += setC ? String(c).padStart(2, '0') : String.fromCharCode(c);
  }
  return out;
}

let nextId = 1;

class Receipt {
  constructor({ remote, source }) {
    this.id = null; // assigned on first content, so empty connections don't burn ids
    this.createdAt = Date.now();
    this.updatedAt = this.createdAt;
    this.remote = remote;
    this.source = source;
    this.items = [];
    this.preview = null; // in-progress line, shown live but not yet committed
    this.rawChunks = [];
    this.rawLength = 0;
    this.done = false;
    this.cut = false;
  }

  addRaw(buf) {
    this.rawChunks.push(Buffer.from(buf));
    this.rawLength += buf.length;
  }

  get raw() {
    if (this.rawChunks.length > 1) this.rawChunks = [Buffer.concat(this.rawChunks)];
    return this.rawChunks[0] || Buffer.alloc(0);
  }

  get visible() {
    const v = this.items.length > 0 || !!(this.preview && this.preview.length);
    if (v && this.id === null) this.id = nextId++;
    return v;
  }

  toJSON() {
    const items = this.preview ? this.items.concat(this.preview) : this.items;
    return {
      id: this.id, createdAt: this.createdAt, updatedAt: this.updatedAt,
      remote: this.remote, source: this.source, bytes: this.rawLength,
      done: this.done, cut: this.cut, items: items.map(serializeItem),
    };
  }
}

function serializeItem(it) {
  if (it.type !== 'image') return it;
  if (!it._b64) it._b64 = it.data.toString('base64');
  const { data, _b64, _band, ...rest } = it;
  return { ...rest, data: _b64 };
}

const MAX_PENDING = 8 * 1024 * 1024;

class Printer {
  constructor({ remote = '?', source = 'tcp', chineseMode = false,
    onUpdate = () => {}, onLog = () => {}, respond = () => {} } = {}) {
    Object.assign(this, { remote, source, chineseMode, onUpdate, onLog, respond });
    this.pending = Buffer.alloc(0);
    this.textChunks = [];
    this.graphics = null;   // GS ( L stored graphics
    this.downloaded = null; // GS * downloaded bit image
    this.code2d = {};       // GS ( k stored symbol data per cn
    this.reset();
    this.receipt = new Receipt({ remote, source });
    this.newLine();
  }

  reset() {
    this.s = {
      font: 'A', bold: false, underline: 0, italic: false, invert: false, w: 1, h: 1,
      charSpacing: 0, align: 'left', lineSpacing: 30, codepage: 0, kanji: this.chineseMode,
      leftMargin: 0, tabs: Array.from({ length: 32 }, (_, k) => (k + 1) * 8 * FONTS.A.w),
      hri: 0, barcodeHeight: 162, barcodeWidth: 3, qrSize: 3, qrEc: 'L',
    };
    this.bandPending = false;
  }

  // ---------------------------------------------------------------- input

  feed(chunk) {
    let b = this.pending.length ? Buffer.concat([this.pending, chunk]) : chunk;
    let i = 0;
    for (;;) {
      while (i < b.length) {
        const r = this.receipt; // a cut may swap receipts; raw bytes belong to the old one
        const n = this.parseOne(b, i);
        if (n === 0) break;
        r.addRaw(b.subarray(i, i + n));
        i += n;
        if (this.cutRequested) { // finalize only after the cut command's bytes are attached
          this.cutRequested = false;
          this.finalize(true);
        }
      }
      if (b.length - i <= MAX_PENDING) break;
      this.onLog(`dropping a byte: command at offset ${i} never completed`);
      i += 1;
    }
    this.pending = Buffer.from(b.subarray(i));
    this.emit();
  }

  end() {
    if (this.pending.length) {
      this.onLog(`${this.pending.length} trailing byte(s) of an incomplete command were ignored`);
      this.receipt.addRaw(this.pending);
      this.pending = Buffer.alloc(0);
    }
    this.finalize(false);
  }

  /** Parse one command/text run at b[i]. Returns bytes consumed, or 0 if incomplete. */
  parseOne(b, i) {
    const c = b[i];
    if (c >= 0x20) {
      let j = i;
      while (j < b.length && b[j] >= 0x20) j++;
      this.textChunks.push(Buffer.from(b.subarray(i, j)));
      return j - i;
    }
    this.flushText();
    switch (c) {
      case 0x0a: this.commitLine(); return 1;            // LF
      case 0x0c: this.commitLine(); return 1;            // FF (page mode print) ≈ LF
      case 0x09: this.tab(); return 1;                   // HT
      case 0x1b: return this.esc(b, i);
      case 0x1d: return this.gs(b, i);
      case 0x1c: return this.fs(b, i);
      case 0x10: return this.dle(b, i);
      default: return 1;                                 // CR and other controls: ignored
    }
  }

  // ---------------------------------------------------------------- ESC

  esc(b, i) {
    const rem = b.length - i;
    if (rem < 2) return 0;
    const s = this.s;
    const n = b[i + 2];
    const need = (len) => rem >= len;
    switch (b[i + 1]) {
      case 0x40: // ESC @ initialize (clears the print buffer)
        this.textChunks = [];
        this.newLine();
        this.reset();
        return 2;
      case 0x20: if (!need(3)) return 0; s.charSpacing = n; return 3;           // ESC SP
      case 0x21: if (!need(3)) return 0;                                         // ESC !
        s.font = n & 1 ? 'B' : 'A'; s.bold = !!(n & 8);
        s.h = n & 16 ? 2 : 1; s.w = n & 32 ? 2 : 1; s.underline = n & 128 ? 1 : 0;
        return 3;
      case 0x24: if (!need(4)) return 0; this.moveTo(n + b[i + 3] * 256); return 4; // ESC $
      case 0x5c: if (!need(4)) return 0;                                          // ESC \
        this.moveTo(this.line.x + ((n + b[i + 3] * 256) << 16 >> 16)); return 4;
      case 0x25: case 0x3d: case 0x3f: case 0x52: case 0x54: case 0x56: case 0x72:
      case 0x7b: case 0x43: case 0x6c: case 0x4b: case 0x65:
        return need(3) ? 3 : 0;                                                  // 1-arg, no visible effect
      case 0x2d: if (!need(3)) return 0; s.underline = n === 1 || n === 49 ? 1 : n === 2 || n === 50 ? 2 : 0; return 3;
      case 0x32: s.lineSpacing = 30; return 2;                                    // ESC 2
      case 0x33: if (!need(3)) return 0; s.lineSpacing = n; return 3;             // ESC 3
      case 0x34: if (!need(3)) return 0; s.italic = !!(n & 1); return 3;          // ESC 4 (italic, non-Epson)
      case 0x35: if (!need(3)) return 0; s.italic = false; return 3;
      case 0x45: case 0x47: if (!need(3)) return 0; s.bold = !!(n & 1); return 3; // ESC E / ESC G
      case 0x4a: if (!need(3)) return 0; this.commitLine(n); return 3;           // ESC J
      case 0x64: if (!need(3)) return 0; this.feedLines(n); return 3;            // ESC d
      case 0x4d: if (!need(3)) return 0; s.font = (n & 0x0f) ? 'B' : 'A'; return 3; // ESC M
      case 0x61: if (!need(3)) return 0; s.align = ['left', 'center', 'right'][n % 48] || 'left'; return 3;
      case 0x74: if (!need(3)) return 0; s.codepage = n; return 3;               // ESC t
      case 0x69: case 0x6d: this.cut(); return 2;                                // ESC i / ESC m
      case 0x70: return need(5) ? 5 : 0;                                         // ESC p drawer kick
      case 0x63: return need(4) ? 4 : 0;                                         // ESC c x n
      case 0x42: return need(4) ? 4 : 0;                                         // ESC B n t (beeper)
      case 0x57: return need(10) ? 10 : 0;                                       // ESC W (page mode area)
      case 0x75: if (!need(3)) return 0; this.respond(Buffer.from([0x00])); return 3; // ESC u drawer status
      case 0x76: this.respond(Buffer.from([0x00])); return 2;                    // ESC v paper status
      case 0x44: { // ESC D n1..nk NUL — tab stops (in character widths)
        const end = b.indexOf(0, i + 2);
        if (end < 0 || end - i > 34) return rem > 34 ? 34 : 0;
        const stops = [...b.subarray(i + 2, end)];
        s.tabs = stops.map((c) => c * (FONTS[s.font].w + s.charSpacing));
        return end - i + 1;
      }
      case 0x2a: { // ESC * m nL nH d… — bit image band (column format)
        if (!need(5)) return 0;
        const m = n, cols = b[i + 3] + b[i + 4] * 256;
        const bpc = m >= 32 ? 3 : 1;
        const len = 5 + cols * bpc;
        if (!need(len)) return 0;
        this.bitImageBand(m, cols, b.subarray(i + 5, i + len));
        return len;
      }
      case 0x26: { // ESC & y c1 c2 [x d…]… — user-defined characters (skipped)
        if (!need(5)) return 0;
        const y = b[i + 2], c1 = b[i + 3], c2 = b[i + 4];
        let p = i + 5;
        for (let ch = c1; ch <= c2; ch++) {
          if (p >= b.length) return 0;
          p += 1 + y * b[p];
        }
        return p <= b.length ? p - i : 0;
      }
      case 0x28: { // ESC ( X pL pH … (beeper etc.)
        if (!need(5)) return 0;
        const len = 5 + b[i + 3] + b[i + 4] * 256;
        return need(len) ? len : 0;
      }
      default:
        return 2;
    }
  }

  // ---------------------------------------------------------------- GS

  gs(b, i) {
    const rem = b.length - i;
    if (rem < 2) return 0;
    const s = this.s;
    const n = b[i + 2];
    const need = (len) => rem >= len;
    switch (b[i + 1]) {
      case 0x21: if (!need(3)) return 0; s.w = ((n >> 4) & 7) + 1; s.h = (n & 7) + 1; return 3; // GS !
      case 0x42: if (!need(3)) return 0; s.invert = !!(n & 1); return 3;          // GS B
      case 0x48: if (!need(3)) return 0; s.hri = n % 48; return 3;                // GS H
      case 0x68: if (!need(3)) return 0; s.barcodeHeight = n || 1; return 3;      // GS h
      case 0x77: if (!need(3)) return 0; s.barcodeWidth = n; return 3;            // GS w
      case 0x4c: if (!need(4)) return 0; s.leftMargin = n + b[i + 3] * 256; return 4; // GS L
      case 0x66: case 0x62: case 0x54: case 0x45: case 0x6a: case 0x5a: case 0x63:
        return need(3) ? 3 : 0;                                                  // 1-arg settings
      case 0x57: case 0x50: case 0x24: case 0x5c: return need(4) ? 4 : 0;        // 2-arg settings
      case 0x5e: return need(5) ? 5 : 0;                                         // GS ^ macro
      case 0x3a: return 2;                                                        // GS : macro def
      case 0x56: { // GS V — cut
        if (!need(3)) return 0;
        const len = [65, 66, 97, 98, 103, 104].includes(n) ? 4 : 3;
        if (!need(len)) return 0;
        this.cut();
        return len;
      }
      case 0x76: { // GS v 0 m xL xH yL yH d… — raster image
        if (!need(8)) return 0;
        const m = b[i + 3];
        const xb = b[i + 4] + b[i + 5] * 256, h = b[i + 6] + b[i + 7] * 256;
        const len = 8 + xb * h;
        if (!need(len)) return 0;
        this.printImage({ w: xb * 8, h, sx: m & 1 ? 2 : 1, sy: m & 2 ? 2 : 1,
          data: Buffer.from(b.subarray(i + 8, i + len)) });
        return len;
      }
      case 0x2a: { // GS * x y d… — define downloaded bit image
        if (!need(4)) return 0;
        const x = b[i + 2], y = b[i + 3];
        const len = 4 + x * y * 8;
        if (!need(len)) return 0;
        this.downloaded = columnsToRows(b.subarray(i + 4, i + len), x * 8, y);
        return len;
      }
      case 0x2f: // GS / m — print downloaded bit image
        if (!need(3)) return 0;
        if (this.downloaded) {
          const m = n % 48;
          this.printImage({ ...this.downloaded, sx: m & 1 ? 2 : 1, sy: m & 2 ? 2 : 1 });
        }
        return 3;
      case 0x6b: { // GS k — barcode
        if (!need(3)) return 0;
        const m = n;
        let data, len;
        if (m <= 6) {
          const end = b.indexOf(0, i + 3);
          if (end < 0) return rem > 3 + 255 ? 3 : 0;
          data = b.subarray(i + 3, end);
          len = end - i + 1;
        } else {
          if (!need(4)) return 0;
          len = 4 + b[i + 3];
          if (!need(len)) return 0;
          data = b.subarray(i + 4, len + i);
        }
        this.barcode(m, data);
        return len;
      }
      case 0x28: { // GS ( X pL pH …
        if (!need(5)) return 0;
        const p = b[i + 3] + b[i + 4] * 256;
        if (!need(5 + p)) return 0;
        if (n === 0x4c) this.graphicsCmd(b, i + 5, p);       // GS ( L
        else if (n === 0x6b) this.code2dCmd(b, i + 5, p);    // GS ( k
        return 5 + p;
      }
      case 0x38: { // GS 8 L p1 p2 p3 p4 … — graphics (large)
        if (!need(7)) return 0;
        const p = b.readUInt32LE(i + 3);
        if (!need(7 + p)) return 0;
        if (n === 0x4c) this.graphicsCmd(b, i + 7, p);
        return 7 + p;
      }
      case 0x49: { // GS I n — printer ID
        if (!need(3)) return 0;
        const str = (t) => [...Buffer.from(`_${t}`), 0];
        const ids = { 1: [0x20], 2: [0x02], 3: [0x64], 65: str('1.00'), 66: str('VIRTUAL'),
          67: str('ESCPOS-EMULATOR'), 68: str('00000001'), 69: str('ANK') };
        const reply = ids[n] || ids[n - 48];
        if (reply) this.respond(Buffer.from(reply));
        this.onLog(`GS I ${n} (printer ID query) answered`);
        return 3;
      }
      case 0x72: // GS r n — status
        if (!need(3)) return 0;
        this.respond(Buffer.from([0x00]));
        this.onLog(`GS r ${n} (status query) → 0x00 (paper OK)`);
        return 3;
      case 0x61: // GS a n — automatic status back
        if (!need(3)) return 0;
        if (n) this.respond(Buffer.from([0x10, 0x00, 0x00, 0x00]));
        return 3;
      default:
        return 2;
    }
  }

  // ---------------------------------------------------------------- FS

  fs(b, i) {
    const rem = b.length - i;
    if (rem < 2) return 0;
    const need = (len) => rem >= len;
    switch (b[i + 1]) {
      case 0x26: this.s.kanji = true; return 2;                     // FS & — Chinese/Kanji mode on
      case 0x2e: this.s.kanji = false; return 2;                    // FS . — off
      case 0x21: case 0x2d: case 0x43: case 0x57: return need(3) ? 3 : 0;
      case 0x53: return need(4) ? 4 : 0;
      case 0x32: return need(76) ? 76 : 0;                          // FS 2 — user kanji 24x24
      case 0x70: // FS p n m — print NV bit image (logo stored in printer)
        if (!need(4)) return 0;
        this.flushLineIfAny();
        this.receipt.items.push({ type: 'note', text: `[stored NV logo #${b[i + 2]}]` });
        return 4;
      case 0x71: { // FS q n [xL xH yL yH d…]… — define NV images (skipped)
        if (!need(3)) return 0;
        let p = i + 3;
        for (let k = 0; k < b[i + 2]; k++) {
          if (p + 4 > b.length) return 0;
          p += 4 + (b[p] + b[p + 1] * 256) * (b[p + 2] + b[p + 3] * 256) * 8;
        }
        return p <= b.length ? p - i : 0;
      }
      case 0x28: {
        if (!need(5)) return 0;
        const len = 5 + b[i + 3] + b[i + 4] * 256;
        return need(len) ? len : 0;
      }
      default:
        return 2;
    }
  }

  // ---------------------------------------------------------------- DLE (real-time)

  dle(b, i) {
    const rem = b.length - i;
    if (rem < 2) return 0;
    switch (b[i + 1]) {
      case 0x04: { // DLE EOT n — real-time status
        if (rem < 3) return 0;
        this.respond(Buffer.from([0x12]));
        this.onLog(`DLE EOT ${b[i + 2]} (status query) → 0x12 (online, no errors)`);
        return 3;
      }
      case 0x05: return rem >= 3 ? 3 : 0;  // DLE ENQ n
      case 0x14: {                          // DLE DC4 fn …
        if (rem < 3) return 0;
        const len = b[i + 2] === 8 ? 10 : 5;
        return rem >= len ? len : 0;
      }
      default: return 1;
    }
  }

  // ---------------------------------------------------------------- printing model

  newLine() {
    this.line = { spans: [], x: 0, align: null, margin: 0 };
  }

  touchLine() {
    if (this.line.align === null) {
      this.line.align = this.s.align;
      this.line.margin = this.s.leftMargin;
    }
  }

  makeSpan(t) {
    const s = this.s;
    const f = FONTS[s.font];
    const span = { t, f: s.font };
    if (s.bold) span.b = 1;
    if (s.underline) span.u = s.underline;
    if (s.italic) span.i = 1;
    if (s.invert) span.inv = 1;
    if (s.w > 1) span.w = s.w;
    if (s.h > 1) span.h = s.h;
    if (s.charSpacing) span.cs = s.charSpacing;
    let width = 0;
    for (const ch of t) width += ((isWide(ch.codePointAt(0)) ? 2 : 1) * f.w + s.charSpacing) * s.w;
    return { span, width };
  }

  flushText() {
    if (!this.textChunks.length) return;
    const bytes = Buffer.concat(this.textChunks);
    this.textChunks = [];
    const { span, width } = this.makeSpan(decodeText(bytes, this.s.codepage, this.s.kanji));
    this.touchLine();
    this.line.spans.push(span);
    this.line.x += width;
  }

  moveTo(target) {
    this.flushText();
    this.touchLine();
    if (target > this.line.x) {
      this.line.spans.push({ sp: target - this.line.x });
      this.line.x = target;
    }
  }

  tab() {
    const next = this.s.tabs.find((t) => t > this.line.x);
    if (next !== undefined) this.moveTo(next);
  }

  pushFeed(dots) {
    if (dots <= 0) return;
    const items = this.receipt.items;
    const last = items[items.length - 1];
    if (last && last.type === 'feed') last.dots += dots;
    else items.push({ type: 'feed', dots });
  }

  /** Print the current line and advance `spacing` dots (default: line spacing). */
  commitLine(spacing = this.s.lineSpacing) {
    this.flushText();
    if (this.line.spans.length) {
      const it = { type: 'text', align: this.line.align, spacing, spans: this.line.spans };
      if (this.line.margin) it.margin = this.line.margin;
      this.receipt.items.push(it);
      this.bandPending = false;
    } else if (this.bandPending) {
      // LF right after an ESC * band: the band itself already used up its height.
      this.bandPending = false;
      this.pushFeed(spacing - this.bandHeight);
    } else {
      this.pushFeed(spacing);
    }
    this.newLine();
  }

  feedLines(n) {
    const sp = this.s.lineSpacing;
    if (this.line.spans.length || this.bandPending) {
      this.commitLine(n === 0 ? 0 : sp);
      this.pushFeed(Math.max(0, n - 1) * sp);
    } else {
      this.pushFeed(n * sp);
    }
  }

  flushLineIfAny() {
    this.flushText();
    if (this.line.spans.length) this.commitLine();
    this.bandPending = false;
  }

  printImage({ w, h, sx = 1, sy = 1, data }, band = false) {
    if (!w || !h) return;
    const items = this.receipt.items;
    const last = items[items.length - 1];
    const align = this.s.align;
    if (this.line.spans.length || this.textChunks.length) this.commitLine();
    if (band && !this.bandPending && last && last._band && last.w === w &&
        last.sx === sx && last.sy === sy && last.align === align) {
      last.data = Buffer.concat([last.data, data]); // stack consecutive bands into one image
      last.h += h;
      last._b64 = null;
    } else {
      items.push({ type: 'image', w, h, sx, sy, align, data, _band: band });
    }
    this.bandPending = band;
    this.bandHeight = h * sy;
  }

  bitImageBand(m, cols, data) {
    const bpc = m >= 32 ? 3 : 1;
    const img = columnsToRows(data, cols, bpc);
    img.sx = m === 0 || m === 32 ? 2 : 1;
    img.sy = m < 32 ? 3 : 1;
    this.printImage(img, true);
  }

  graphicsCmd(b, o, p) {
    if (p < 2) return;
    const fn = b[o + 1];
    if (fn === 112) { // store raster graphics in print buffer
      if (p < 10) return;
      const tone = b[o + 2], bx = b[o + 3], by = b[o + 4], color = b[o + 5];
      const w = b[o + 6] + b[o + 7] * 256, h = b[o + 8] + b[o + 9] * 256;
      const len = Math.ceil(w / 8) * h;
      if (10 + len > p) return;
      if (tone !== 48) { this.onLog('multi-tone graphics are not supported'); return; }
      if (color === 49 || color === 48 || !this.graphics) {
        this.graphics = { w, h, sx: bx === 2 ? 2 : 1, sy: by === 2 ? 2 : 1,
          data: Buffer.from(b.subarray(o + 10, o + 10 + len)) };
      }
    } else if (fn === 50 || fn === 2) { // print buffered graphics
      if (this.graphics) {
        this.printImage(this.graphics);
        this.graphics = null;
      }
    } else if (fn === 69 || fn === 85) { // print NV / download graphics by key
      this.flushLineIfAny();
      this.receipt.items.push({ type: 'note', text: '[stored graphics]' });
    }
  }

  code2dCmd(b, o, p) {
    if (p < 2) return;
    const cn = b[o], fn = b[o + 1];
    const kind = CODES_2D[cn] || `2D code ${cn}`;
    if (cn === 49 && fn === 67) this.s.qrSize = b[o + 2];
    else if (cn === 49 && fn === 69) this.s.qrEc = { 48: 'L', 49: 'M', 50: 'Q', 51: 'H' }[b[o + 2]] || 'L';
    else if (fn === 80) this.code2d[cn] = Buffer.from(b.subarray(o + 3, o + p));
    else if (fn === 81) {
      this.flushLineIfAny();
      const it = { type: 'code2d', kind, data: decodeData(this.code2d[cn] || Buffer.alloc(0)), align: this.s.align };
      if (cn === 49) {
        it.size = this.s.qrSize;
        it.ec = this.s.qrEc;
        try { // draw the real symbol: n×n modules, each `size` dots
          const q = encodeQR(this.code2d[cn] || Buffer.alloc(0), it.ec);
          it.modules = q.size;
          it.matrix = packMatrix(q).toString('base64');
        } catch (e) {
          this.onLog(`QR not rendered: ${e.message}`);
        }
      }
      this.receipt.items.push(it);
    }
  }

  barcode(m, bytes) {
    this.flushLineIfAny();
    const kind = BARCODES[m] || `barcode ${m}`;
    const data = kind === 'CODE128' || kind === 'GS1-128' ? code128Text(bytes) : Buffer.from(bytes).toString('latin1');
    this.receipt.items.push({ type: 'barcode', kind, data, hri: this.s.hri,
      height: this.s.barcodeHeight, module: this.s.barcodeWidth, align: this.s.align });
  }

  cut() {
    this.flushText();
    if (this.receipt.items.length || this.line.spans.length) this.cutRequested = true;
  }

  finalize(cut) {
    this.flushText();
    if (this.line.spans.length) this.commitLine();
    const r = this.receipt;
    r.cut = cut;
    r.done = true;
    r.preview = null;
    r.updatedAt = Date.now();
    this.onUpdate(r);
    this.receipt = new Receipt({ remote: this.remote, source: this.source });
    this.newLine();
    this.bandPending = false;
  }

  /** Publish the in-progress receipt, including the not-yet-terminated line. */
  emit() {
    const r = this.receipt;
    const spans = this.line.spans.slice();
    if (this.textChunks.length) {
      spans.push(this.makeSpan(decodeText(Buffer.concat(this.textChunks), this.s.codepage, this.s.kanji)).span);
    }
    r.preview = spans.length
      ? [{ type: 'text', align: this.line.align ?? this.s.align, spacing: this.s.lineSpacing, spans }]
      : null;
    r.updatedAt = Date.now();
    this.onUpdate(r);
  }
}

/** Convert column-format bit image (bpc bytes per column, MSB = top) to packed rows. */
function columnsToRows(data, cols, bpc) {
  const h = bpc * 8;
  const bpr = Math.ceil(cols / 8);
  const out = Buffer.alloc(bpr * h);
  for (let x = 0; x < cols; x++) {
    for (let k = 0; k < bpc; k++) {
      const byte = data[x * bpc + k];
      if (!byte) continue;
      for (let j = 0; j < 8; j++) {
        if (byte & (0x80 >> j)) out[(k * 8 + j) * bpr + (x >> 3)] |= 0x80 >> (x & 7);
      }
    }
  }
  return { w: cols, h, data: out };
}

module.exports = { Printer, Receipt, decodeText };
