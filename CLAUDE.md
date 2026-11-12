# CLAUDE.md

ESC/POS virtual network printer for macOS. It listens on TCP 9100, parses receipt-printer byte streams and shows each job live in a browser viewer (port 9101).
Plain Node.js (>=18) with **zero npm dependencies and no build step**. Keep it that way: no `package.json` dependencies, and no CDN scripts in the viewer.

## Commands

```bash
node server.js --open                                   # run (printer :9100, viewer http://localhost:9101)
node server.js --port 19100 --viewer-port 19101         # run on alternate ports (see "Port 9100" below)
node server.js --help                                   # all flags (each also has an env var)
node examples/test-print.js [host] [port] [cols]        # status query + full sample receipt
printf '\x1b@Hello\n\x1dV\x00' | nc -w1 localhost 9100  # minimal job
```

There is no test suite. To verify changes, see "Verifying changes" below.

## Layout

- `server.js`: two servers in one process.
  - **Printer port** (`net`, `allowHalfOpen: true`) sniffs the first bytes of each connection. `GET/POST/PUT/OPTIONS/HEAD` are handled as HTTP (POST body = raw ESC/POS, CORS + Private-Network headers). Anything else is raw TCP, fed into one `Printer` per connection.
  - **Viewer** (`http`, bound to 127.0.0.1 by default) serves `public/index.html`, an SSE stream at `/events` (`init`, `receipt`, `log`, `clear`) and `/api/receipts`, `/api/receipts/:id/raw`, `/api/clear` and `/api/test`. It keeps the last 200 receipts in memory.
- `src/escpos.js`: the parser and printer model (`Printer`, `Receipt`). Pure: no I/O, and it talks to the outside only through the `onUpdate`, `onLog` and `respond` callbacks.
- `src/qr.js`: hand-written QR encoder (byte mode, v1–40, L/M/Q/H, mask selection). `packMatrix` packs the result into 1-bit rows.
- `src/sample.js`: `buildSampleReceipt(cols)`, the demo job used by the "Print test receipt" button and `examples/test-print.js`.
- `public/index.html`: a single-file viewer (inline CSS/JS) that renders the receipt item model into DOM and canvas.

## Receipt model (contract between `escpos.js` and the viewer)

`Receipt.toJSON()` → `{ id, createdAt, remote, source, bytes, done, cut, items[] }`. Item types:
- `text`: `{ align, margin?, spacing, spans }`. A span is `{ t, f:'A'|'B', b?, u?, i?, inv?, w?, h?, cs? }`, or a spacer `{ sp: dots }`.
- `feed`: `{ dots }`. Consecutive feeds are merged.
- `image`: `{ w, h, sx, sy, align, data: base64 1-bit rows }`.
- `barcode`: `{ kind, data, hri, height, module, align }` (placeholder only).
- `code2d`: `{ kind, data, align, size?, ec?, modules?, matrix? }`. QR codes carry a real matrix.
- `note`: `{ text }`, e.g. a stored NV logo that can't be shown.

Units are printer dots, rendered at 1 dot = 1 CSS px. Font A is 12×24 dots, Font B 9×17, and 80mm paper is 576 dots / 48 columns.
If you add an item type or field, update both `escpos.js` and `renderItem()` in `index.html`.

## Parser conventions (src/escpos.js)

- `parseOne(b, i)` returns **bytes consumed, or 0 if the command is incomplete**. The leftover bytes are kept in `this.pending` until the next chunk arrives. Every new command must check `need(len)` before it reads its arguments, because TCP chunks split commands anywhere.
- Unknown `ESC x` / `GS x` consume 2 bytes. Commands with arguments must consume them all, or the argument bytes get printed as garbage text.
- Call `flushText()` before any style change. The parser already does this for every control byte (<0x20), so text runs are decoded with the style that was active while they were collected.
- Status queries (`DLE EOT`, `GS r`, `GS I`, `ESC u/v`, `GS a`) must `respond()`. POS apps often wait for this reply before printing.
- A cut sets `cutRequested`. The receipt is finalized in `feed()` only after the cut command's own bytes have been added to its raw data (otherwise saved `.bin` files come out short).
- Receipt ids are assigned lazily, in the `visible` getter, so connections that only poll status don't use up ids.
- Text decoding order: ASCII → GB18030 when Chinese mode is on (`FS &` or `--chinese`) → strict UTF-8 → `ESC t` code page → CP437.

## Viewer rule: the paper shows only what the printer would print

Never add debug or helper text onto the paper (the user flagged a decoded-URL label under the QR as wrong).
Put metadata in the `.meta` bar above the receipt, in tooltips (`title`) or in the Activity log. QR data is shown on hover, and a URL opens on click.
Barcode placeholders are the one exception: their bars aren't drawn, so they show their data.

## Verifying changes

- **Parser:** create a `Printer` with `onUpdate` collecting `r.done` receipts. Feed it the job **in small chunks** (e.g. 7 bytes) to test the incomplete-command handling, then call `end()`. Check that the per-receipt `bytes` add up to the input length.
- **End to end:** start the server on 19100/19101, send a job, then check `curl localhost:19101/api/receipts`.
- **Visual:** render the viewer with headless Chrome:
  `"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --disable-gpu --user-data-dir=<tmp> --window-size=1100,1400 --screenshot=<out.png> http://localhost:19101/`
- **QR:** verify that codes really decode. Write a PNG (Node `zlib.deflateSync` + `zlib.crc32`, with a 4-module quiet zone), then decode it with CoreImage from a `swift` script (`CIDetector(ofType: CIDetectorTypeQRCode)` → `CIQRCodeFeature.messageString`). Cover several lengths and all EC levels. Byte-mode capacity at EC H tops out around 1,273 bytes.

## Gotchas

- **Port 9100** is also the default port of Flutter/Dart DevTools, which is often running on this machine. Check with `lsof -nP -iTCP:9100 -sTCP:LISTEN`. Don't kill the user's process; test on other ports instead.
- The printer port binds `0.0.0.0` so LAN devices can print. The viewer binds `127.0.0.1` on purpose, because receipts may contain customer data.
- Editing `public/index.html` needs only a browser refresh (it is read on every request). Changes in `src/` or `server.js` need a server restart.
- The CUPS raw queue in the README (`lpadmin … -m raw`) is documented but hasn't been tested on this machine.
