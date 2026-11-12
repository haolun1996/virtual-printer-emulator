# ESC/POS Virtual Printer (macOS)

A virtual thermal receipt printer that listens on **TCP port 9100** (RAW / JetDirect) and shows
every print job live in your browser. Pure Node.js, **no dependencies**, no build step.
Inspired by [Garletz/escpos-virtual-printer-emulator](https://github.com/Garletz/escpos-virtual-printer-emulator) (Rust/egui, Windows/Linux).

## Run

```bash
node server.js --open        # or: npm start
```

- Printer: `tcp://<your-mac-ip>:9100` (listens on all interfaces, so tablets/POS terminals on your LAN can print)
- Viewer: http://localhost:9101 (bound to localhost only)

macOS may ask once whether `node` may accept incoming connections. Allow it if other devices need to print.

> **Port 9100 busy?** Flutter/Dart DevTools also defaults to 9100. Check with
> `lsof -nP -iTCP:9100 -sTCP:LISTEN`, then quit it or run the printer on another port: `node server.js --port 9200`.

## Test it

```bash
printf '\x1b@Hello from macOS\n\x1dV\x00' | nc -w1 localhost 9100
node examples/test-print.js                 # full sample receipt (+ status query)
node examples/test-print.js 192.168.1.20 9100 32   # remote host, 58mm (32 cols)
```

Or click **Print test receipt** in the viewer.

## Options

| Flag | Env | Default | |
|---|---|---|---|
| `--port` | `PORT` | `9100` | printer port |
| `--host` | `HOST` | `0.0.0.0` | use `127.0.0.1` to accept local jobs only |
| `--viewer-port` | `VIEWER_PORT` | `9101` | web viewer |
| `--viewer-host` | `VIEWER_HOST` | `127.0.0.1` | `0.0.0.0` to view from other devices |
| `--save <dir>` | `SAVE_DIR` | off | save each job as a raw `.bin` |
| `--chinese` | `CHINESE_MODE=1` | off | decode non-UTF-8 text as GB18030 (like Chinese-market printers with Chinese mode on by default) |
| `--open` | `OPEN=1` | off | open the viewer in your browser |

## What's supported

- **Transport:** raw TCP (one or many receipts per connection, split on cut), plus HTTP `POST`/`PUT` of raw bytes to the same port with CORS + Private-Network headers (for web POS apps)
- **Status replies:** `DLE EOT n` → `0x12` (online), `GS r n` → `0x00`, `GS I n` (printer ID), `ESC u`/`ESC v`, `GS a` (ASB). This lets POS software that checks the printer before printing work.
- **Text:** `ESC @ ! E G - M a 2 3 J d SP $ \ D t`, `GS ! B L`, `HT`, double width/height (1–8×), Font A/B, underline, inverted, bold
- **Encoding:** UTF-8 auto-detect; `FS &`/`FS .` Chinese mode (GB18030); `ESC t` code pages (CP437, WPC1252, PC866, WPC125x…)
- **Images:** `GS v 0` raster, `ESC *` bit-image bands (merged), `GS ( L` / `GS 8 L` graphics, `GS *` + `GS /`
- **QR codes:** `GS ( k` QR codes are **drawn for real** (scannable from the screen) at the requested module size and error-correction level. Nothing extra is printed on the paper: hover the QR to see its data, click it to open a URL.
- **Other barcodes:** `GS k` (all 1D types) and PDF417/DataMatrix/Aztec are **shown as labelled placeholders with the decoded data**; their bars/modules are not drawn.
- **Cut:** `GS V`, `ESC i`, `ESC m`. **Drawer kick** `ESC p` is accepted.
- Viewer: 58mm/80mm paper, zoom, per-receipt hex dump and raw `.bin` download, activity log

## Printing from Mac apps (optional)

You can add the emulator as a CUPS raw queue:

```bash
lpadmin -p ESC_POS_Virtual -E -v socket://127.0.0.1:9100 -m raw
lp -d ESC_POS_Virtual -o raw receipt.bin
```

A raw queue passes bytes through unchanged. That's useful for apps or scripts that already produce ESC/POS.
Printing a normal document (for example from TextEdit) sends PDF data, which no ESC/POS printer understands, real or virtual.
To remove the queue: `lpadmin -x ESC_POS_Virtual`.
