# ESC/POS Virtual Printer

A virtual thermal receipt printer for **macOS and Windows**. It listens on **TCP port 9100** (RAW / JetDirect), like a network receipt printer, and shows every print job live in its window.
Built with Rust + [Tauri](https://tauri.app): a ~3 MB native app with nothing else to install.
Inspired by [Garletz/escpos-virtual-printer-emulator](https://github.com/Garletz/escpos-virtual-printer-emulator).

## Install

| Platform | File (from `src-tauri/target/…/bundle/`) |
|---|---|
| macOS (Apple Silicon) | `dmg/ESC POS Virtual Printer_1.0.0_aarch64.dmg` |
| Windows (x64) | `nsis/ESC POS Virtual Printer_1.0.0_x64-setup.exe` |

The builds are **not code-signed**, so each OS warns the first time you open them:

- **macOS:** right-click the app, choose **Open**, then confirm. If macOS says the app "is damaged", run `xattr -dr com.apple.quarantine "/Applications/ESC POS Virtual Printer.app"`.
- **Windows:** in the SmartScreen dialog, click **More info**, then **Run anyway**.

Allow incoming connections when the firewall asks, so tablets and POS terminals on your network can print to it.

## Use

Point your POS / app at this computer as a **network printer**: `<this computer's IP>:9100`. The window shows the IP.

- **Port 9100 busy?** Flutter/Dart DevTools also uses 9100 by default. The app shows a banner where you can start the printer on another port, and it remembers that port.
- **Test it:** click **Print test receipt**, or run `printf '\x1b@Hello\n\x1dV\x00' | nc -w1 localhost 9100`.
- **Download a job:** **download .bin** saves the raw bytes of a receipt to your Downloads folder.
- **Open a QR link:** click a QR code to open its URL in your browser.

The viewer is also served at http://localhost:9101 (local only), so you can open it in a browser too.

## Let AI agents read receipts (MCP)

The app includes an [MCP](https://modelcontextprotocol.io) server at **`http://localhost:9101/mcp`** (also shown in the window header). AI agents can use it to read what your POS printed.
It is local-only and works while the app (or `--headless`) is running.

| Tool | What it does |
|---|---|
| `printer_status` | Printer port, LAN address, running or not, and the latest receipt id |
| `list_receipts` | Recent receipts: id, time, source, size, first line |
| `get_receipt` | One receipt (latest by default). `text` lays it out like the paper, with `[QR code: …]` / `[CODE128 barcode: …]` lines. `json` returns the structure with styles, `hex` the raw ESC/POS bytes |
| `wait_for_receipt` | Blocks until the next receipt finishes printing, then returns it. Lets an agent trigger a print in your POS and check the result |
| `clear_receipts` | Deletes all receipts |

**Claude Code:**

```bash
claude mcp add --transport http escpos-printer http://localhost:9101/mcp
```

**Cursor / other clients with HTTP support:** `{ "mcpServers": { "escpos-printer": { "url": "http://localhost:9101/mcp" } } }`

**Claude Desktop and other stdio-only clients:** run the app binary with `--mcp-stdio`. It forwards to the running app.

```json
{ "mcpServers": { "escpos-printer": {
    "command": "/Applications/ESC POS Virtual Printer.app/Contents/MacOS/escpos-virtual-printer",
    "args": ["--mcp-stdio"] } } }
```

On Windows the default install path is `%LOCALAPPDATA%\ESC POS Virtual Printer\escpos-virtual-printer.exe`.

If port 9101 was taken when the app started, it picks another viewer port; check the window header. In that case use that URL, or `--mcp-stdio --viewer-port <n>`.

## What's supported

- **Transport:** raw TCP (one or many receipts per connection, split on cut), plus HTTP `POST`/`PUT` of raw bytes to the same port with CORS + Private-Network headers (for web POS apps)
- **Status replies:** `DLE EOT n` → `0x12` (online), `GS r n` → `0x00`, `GS I n` (printer ID), `ESC u`/`ESC v`, `GS a` (ASB). This lets POS software that checks the printer before printing work.
- **Text:** `ESC @ ! E G - M a 2 3 J d SP $ \ D t`, `GS ! B L`, `HT`, double width/height (1–8×), Font A/B, underline, inverted, bold
- **Encoding:** UTF-8 auto-detect; `FS &`/`FS .` Chinese mode (GB18030); `ESC t` code pages (CP437, WPC1252, PC866, WPC125x…)
- **Images:** `GS v 0` raster, `ESC *` bit-image bands (merged), `GS ( L` / `GS 8 L` graphics, `GS *` + `GS /`
- **QR codes:** `GS ( k` QR codes are **drawn for real** (scannable from the screen) at the requested module size and error-correction level. Nothing extra is printed on the paper: hover the QR to see its data, click it to open a URL.
- **Other barcodes:** `GS k` (all 1D types) and PDF417/DataMatrix/Aztec are **shown as labelled placeholders with the decoded data**; their bars/modules are not drawn.
- **Cut:** `GS V`, `ESC i`, `ESC m`. **Drawer kick** `ESC p` is accepted.
- **Viewer:** 58mm/80mm paper, zoom, per-receipt hex dump and raw `.bin` download, activity log

## Command line / headless

The app binary also runs without a window (for example on a server):

```bash
escpos-virtual-printer --headless [--port 9100] [--host 0.0.0.0] [--viewer-port 9101] [--viewer-host 127.0.0.1] [--save <dir>] [--chinese]
```

Each flag also has an env var: `PORT`, `HOST`, `VIEWER_PORT`, `VIEWER_HOST`, `SAVE_DIR`, `CHINESE_MODE=1`, `HEADLESS=1`.

- `--chinese` decodes non-UTF-8 text as GB18030, like Chinese-market printers that have Chinese mode on by default.
- `--save` writes every job to `<dir>` as a raw `.bin` file.

On macOS the binary is at `ESC POS Virtual Printer.app/Contents/MacOS/escpos-virtual-printer`.

## Build from source

Requires Rust and the Tauri CLI (`cargo install tauri-cli`).

```bash
cd src-tauri
cargo test                                   # parser + QR known-answer tests
cargo run -- --headless --port 19100         # dev run without a window
cargo tauri dev                              # dev run with the window
cargo tauri build --bundles app,dmg          # macOS .app + .dmg

# Windows installer, cross-compiled on a Mac (needs `brew install llvm nsis` and `cargo install cargo-xwin`)
PATH="/opt/homebrew/opt/llvm/bin:$PATH" cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis
```

## Printing from Mac apps (optional)

You can add the emulator as a CUPS raw queue:

```bash
lpadmin -p ESC_POS_Virtual -E -v socket://127.0.0.1:9100 -m raw
lp -d ESC_POS_Virtual -o raw receipt.bin
```

A raw queue passes bytes through unchanged. That's useful for apps or scripts that already produce ESC/POS.
Printing a normal document (for example from TextEdit) sends PDF data, which no ESC/POS printer understands, real or virtual.
To remove the queue: `lpadmin -x ESC_POS_Virtual`.
