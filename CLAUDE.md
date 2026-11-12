# CLAUDE.md

ESC/POS virtual network printer: a Rust + Tauri v2 desktop app for macOS and Windows. It listens on TCP 9100, parses receipt-printer byte streams and shows each job live.
The window is a webview showing `ui/index.html`, which is served by the app's own HTTP server (also reachable from a browser at `localhost:9101`).
Keep dependencies minimal: `tauri`, `serde_json`, `encoding_rs`. The servers are plain `std::net` + threads (no tokio). The viewer is a single HTML file with no CDN scripts.

## Commands (run in `src-tauri/`)

```bash
cargo test                                            # parser + QR known-answer tests
cargo run -- --headless --port 19100 --viewer-port 19101   # server only, no window
cargo tauri dev                                       # with window
cargo tauri build --bundles app,dmg                   # macOS → target/release/bundle/{macos,dmg}
PATH="/opt/homebrew/opt/llvm/bin:$PATH" cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --bundles nsis
                                                      # Windows installer cross-built on macOS → target/x86_64-pc-windows-msvc/release/bundle/nsis
```

The Windows cross-build needs Homebrew's keg-only `llvm` on `PATH` (for `clang-cl`, `llvm-rc`, `llvm-lib`), plus `makensis` and `cargo-xwin`.
The `LNK4099 … libcmt.amd64.pdb` linker warning is harmless.

## Layout

- `src-tauri/src/main.rs`: CLI/headless switch and Tauri setup.
  - Loads the saved printer port (`app_config_dir/settings.json`) unless `--port`/`PORT` is given.
  - Starts the servers and opens the window on the viewer URL.
  - `on_new_window` sends `window.open` (QR links) to the system browser.
  - `on_download` saves `download .bin` files to `~/Downloads`.
- `src-tauri/src/server.rs`: two servers.
  - **Printer port** (one thread per connection, 100 ms read timeout) sniffs the first bytes. HTTP verbs are handled as HTTP (POST body = raw ESC/POS, CORS + Private-Network headers). Anything else is fed into one `Printer`.
  - **Viewer** (127.0.0.1) serves `ui/index.html` (embedded with `include_str!`), SSE at `/events` (`init`, `receipt`, `log`, `status`, `clear`), the MCP endpoint `/mcp`, and `/api/receipts`, `/api/receipts/:id/raw`, `/api/clear`, `/api/test`, `/api/listen?port=`.
  - If the viewer port is busy, the app picks a free one. A busy printer port is **not** fatal: `status.listening = false`, and the UI shows a banner to start on another port.
- `src-tauri/src/escpos.rs`: parser and printer model (`Printer`, `Receipt`, `Item`). Pure, no I/O. Output is `printer.events`: `Update` (receipt JSON; `raw` attached once done), `Log`, `Respond` (bytes back to the client).
- `src-tauri/src/mcp.rs`: MCP server, so AI agents can read receipts.
  - Streamable HTTP at `POST /mcp` on the viewer server. Every request gets a plain JSON response; there is no server-initiated SSE stream, and `GET /mcp` returns 405. Requests with a non-local `Origin` get 403 (DNS-rebinding guard).
  - Tools: `printer_status`, `list_receipts`, `get_receipt` (text/json/hex), `wait_for_receipt` (Condvar `latest_done` in `Shared`), `clear_receipts`.
  - `render_receipt` turns the receipt JSON into plain text on N columns.
  - `--mcp-stdio` (`run_stdio_bridge`) forwards newline-delimited JSON-RPC from stdin to the running app's `/mcp`.
- `src-tauri/src/qr.rs`: QR encoder (byte mode, v1–40, L/M/Q/H, mask selection). `pack()` produces 1-bit rows.
- `src-tauri/src/sample.rs`: the demo receipt behind the "Print test receipt" button.
- `ui/index.html`: the viewer (inline CSS/JS). It renders the receipt JSON into DOM and canvas.
- `src-tauri/icons/`: generated with `cargo tauri icon <1024px png>`. The source art isn't kept; mobile icon folders were deleted.

## Receipt JSON (contract between `escpos.rs` `to_json` and `renderItem()` in `ui/index.html`)

`{ id, createdAt, updatedAt, remote, source, bytes, done, cut, items[] }`. Item types:
- `text`: `{ align, margin?, spacing, spans }`. A span is `{ t, f:'A'|'B', b?, u?, i?, inv?, w?, h?, cs? }`, or a spacer `{ sp }`.
- `feed`: `{ dots }`.
- `image`: `{ w, h, sx, sy, align, data: base64 1-bit rows }`.
- `barcode`: `{ kind, data, hri, height, module, align }` (placeholder only).
- `code2d`: `{ kind, data, align, size?, ec?, modules?, matrix? }`. QR codes carry a real matrix.
- `note`: `{ text }`.

Units are printer dots, rendered at 1 dot = 1 CSS px. Font A is 12×24 dots, Font B 9×17, and 80mm paper is 576 dots / 48 columns. Change both sides together.

## Parser conventions (escpos.rs)

- `parse_one` returns **bytes consumed, or 0 if the command is incomplete**. The leftover bytes are kept in `pending` until the next chunk arrives. Use `need!(len)` before reading any argument, because TCP chunks split commands anywhere.
- Unknown `ESC x` / `GS x` consume 2 bytes. Commands with arguments must consume them all, or the argument bytes get printed as garbage text.
- `flush_text()` runs before every control byte, so text is decoded with the style active while it was collected.
- Status queries (`DLE EOT`, `GS r`, `GS I`, `ESC u/v`, `GS a`) must push `Event::Respond`. POS apps often wait for this reply before printing.
- A cut sets `cut_requested`. `feed()` finalizes only after the cut command's bytes have been added to `raw`.
- Receipt ids are assigned lazily (`ensure_visible`), so status-only connections don't use up ids.
- Text decoding order: ASCII → GB18030 when Chinese mode is on → strict UTF-8 → `ESC t` code page (`encoding_rs`) → CP437.
- Watch for usize underflow when porting arithmetic: JS tolerated negative intermediates (this already bit `qr.rs`).

## MCP output rule

`get_receipt` text mirrors the paper: no extra commentary inside the ruled block. Images, barcodes and QR codes appear as single `[ … ]` lines that include their data, so agents can verify them.

## Viewer rule: the paper shows only what the printer would print

Never add debug or helper text onto the paper (the user flagged a decoded-URL label under the QR as wrong).
Put metadata in the `.meta` bar above the receipt, in tooltips or in the Activity log. QR data is shown on hover, and a URL opens on click.

## Verifying changes

- Run `cargo test`.
  - `sample_receipt_chunked` feeds the sample in 1-, 7- and 4096-byte chunks and checks the per-receipt raw bytes add up.
  - `qr::matches_verified_reference` holds FNV hashes of matrices from an encoder whose output macOS CoreImage decoded, so any change to the QR output fails it.
- **End to end:** run `cargo run -- --headless --port 19100 --viewer-port 19101`, send jobs with `nc`/`curl`, then check `curl localhost:19101/api/receipts`.
- **MCP:** POST JSON-RPC with curl (`initialize`, `tools/list`, `tools/call`). To test with a real client without touching the user's config, use `claude -p "<prompt>" --strict-mcp-config --mcp-config <tmp.json> --allowedTools "mcp__escpos__get_receipt"`. Put the prompt **before** `--allowedTools`, which takes several values.
- **Visual:** `"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new --disable-gpu --user-data-dir=<tmp> --window-size=1100,900 --screenshot=<out.png> http://localhost:19101/`
- **QR scannability:** decode a screenshot with a `swift` script: `CIDetector(ofType: CIDetectorTypeQRCode)` → `CIQRCodeFeature.messageString`.

## Gotchas

- **Port 9100** is often taken on this machine. Flutter/Dart DevTools binds `127.0.0.1:9100`, and the user may also be running the old Node emulator on `*:9100`. When both are up, `localhost:9100` reaches DevTools while LAN connections reach the emulator. Don't kill the user's processes; test on 19100/19101.
- The printer port binds `0.0.0.0` so LAN devices can print. The viewer binds `127.0.0.1` on purpose, because receipts may contain customer data.
- `ui/index.html` is compiled into the binary, so UI changes need a rebuild (or `cargo run`).
- The builds are unsigned (no Apple Developer ID or Windows code-signing certificate). The Windows build is cross-compiled and hasn't been run on a real Windows machine.
