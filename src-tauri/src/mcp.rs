//! MCP (Model Context Protocol) server so AI agents can read printed receipts.
//!
//! Transport: Streamable HTTP at `POST /mcp` on the viewer server (127.0.0.1 only), answering
//! every request with a plain JSON response (no server-initiated SSE stream). For clients that
//! only launch stdio servers, `--mcp-stdio` bridges stdin/stdout to that endpoint.

use crate::escpos::is_wide;
use crate::server::Shared;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_COLUMNS: usize = 48;
const MAX_HEX_BYTES: usize = 16 * 1024;

const INSTRUCTIONS: &str = "Virtual ESC/POS receipt printer. Point-of-sale apps print to it over TCP port 9100 \
(or the port shown by printer_status). Use list_receipts to see recent print jobs, get_receipt to read one \
(plain text laid out like the paper, the structured JSON with styles, or a hex dump of the raw ESC/POS bytes), \
and wait_for_receipt to block until the next job finishes printing. QR codes and barcodes are reported with \
their encoded data.";

fn tools() -> Value {
    json!([
        {
            "name": "printer_status",
            "title": "Printer status",
            "description": "Where the virtual printer is listening (port, LAN address), whether it is running, and the id of the latest finished receipt (pass it to wait_for_receipt as after_id before triggering a print).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "list_receipts",
            "title": "List receipts",
            "description": "List recently printed receipts, newest first: id, time (UTC), source (tcp/http), client address, size, whether the paper was cut, and the first line of text.",
            "inputSchema": {
                "type": "object",
                "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20 } },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "get_receipt",
            "title": "Read a receipt",
            "description": "Read one printed receipt (the latest if id is omitted). format=text (default) renders it as plain text laid out on the paper width, with images, barcodes and QR codes shown as [ ... ] lines that include their data. format=json returns the structured receipt with styles (bold, underline, double size, alignment, fonts). format=hex returns a hex dump of the raw ESC/POS bytes the app sent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Receipt id from list_receipts. Omit for the latest." },
                    "format": { "type": "string", "enum": ["text", "json", "hex"], "default": "text" },
                    "columns": { "type": "integer", "minimum": 16, "maximum": 96, "default": DEFAULT_COLUMNS,
                        "description": "Paper width in characters for format=text (80mm = 48, 58mm = 32)." }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "wait_for_receipt",
            "title": "Wait for the next receipt",
            "description": "Block until a receipt with an id greater than after_id finishes printing (or the timeout passes), then return it as text. Get after_id from printer_status before triggering the print, to avoid missing a fast job.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "after_id": { "type": "integer", "description": "Defaults to the latest finished receipt at call time." },
                    "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": 300, "default": 30 },
                    "columns": { "type": "integer", "minimum": 16, "maximum": 96, "default": DEFAULT_COLUMNS }
                },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true, "openWorldHint": false }
        },
        {
            "name": "clear_receipts",
            "title": "Clear receipts",
            "description": "Delete all receipts from the emulator (also clears the viewer window).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false }
        }
    ])
}

/// Handle one JSON-RPC message posted to /mcp. Returns (HTTP status, response body).
/// Notifications and responses get 202 with no body.
pub fn handle(shared: &Arc<Shared>, body: &[u8]) -> (&'static str, Option<String>) {
    let msg: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return ("400 Bad Request", Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}")).to_string())),
    };
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return ("202 Accepted", None); // a JSON-RPC response from the client
    };
    let Some(id) = msg.get("id").cloned() else {
        return ("202 Accepted", None); // notification, e.g. notifications/initialized
    };
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let reply = match method {
        "initialize" => {
            let requested = params["protocolVersion"].as_str().unwrap_or("");
            let version = if SUPPORTED_VERSIONS.contains(&requested) { requested } else { SUPPORTED_VERSIONS[0] };
            rpc_result(id, json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "escpos-virtual-printer", "title": "ESC/POS Virtual Printer", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS
            }))
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(shared, name, &args) {
                Ok(text) => rpc_result(id, json!({ "content": [{ "type": "text", "text": text }], "isError": false })),
                Err(ToolError::Unknown) => rpc_error(id, -32602, &format!("unknown tool: {name}")),
                Err(ToolError::Failed(text)) => rpc_result(id, json!({ "content": [{ "type": "text", "text": text }], "isError": true })),
            }
        }
        _ => rpc_error(id, -32601, &format!("method not found: {method}")),
    };
    ("200 OK", Some(reply.to_string()))
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

enum ToolError {
    Unknown,
    Failed(String),
}

fn call_tool(shared: &Arc<Shared>, name: &str, args: &Value) -> Result<String, ToolError> {
    let int = |key: &str| args.get(key).and_then(Value::as_u64);
    let columns = int("columns").map(|c| c.clamp(16, 96) as usize).unwrap_or(DEFAULT_COLUMNS);
    match name {
        "printer_status" => {
            let s = shared.status_json();
            Ok(serde_json::to_string_pretty(&s).unwrap_or_default())
        }
        "list_receipts" => {
            let limit = int("limit").unwrap_or(20).clamp(1, 200) as usize;
            let list = shared.receipts_json();
            if list.is_empty() {
                return Ok("No receipts printed yet.".into());
            }
            let mut out = format!("{} receipt(s), newest first:\n", list.len());
            for (_, j) in list.iter().rev().take(limit) {
                let r: Value = serde_json::from_str(j).unwrap_or_default();
                // title = first line of real text (skip [image …] / [QR …] markers)
                let first = render_lines(&r, columns).into_iter().map(|l| l.trim().to_string())
                    .find(|l| !l.is_empty() && !l.starts_with('[')).unwrap_or_default();
                out.push_str(&format!(
                    "#{} | {} | {} {} | {} bytes | {} | \"{}\"\n",
                    r["id"], iso_utc(r["createdAt"].as_u64().unwrap_or(0)), r["source"].as_str().unwrap_or(""),
                    r["remote"].as_str().unwrap_or(""), r["bytes"],
                    if r["done"] == true { if r["cut"] == true { "cut" } else { "done" } } else { "still printing" }, first
                ));
            }
            Ok(out)
        }
        "get_receipt" => {
            let list = shared.receipts_json();
            let entry = match int("id") {
                Some(id) => list.iter().find(|(rid, _)| *rid == id),
                None => list.last(),
            };
            let Some((id, j)) = entry else {
                return Err(ToolError::Failed(match int("id") {
                    Some(id) => format!("No receipt #{id} (it may have been cleared). Use list_receipts."),
                    None => "No receipts printed yet.".into(),
                }));
            };
            let r: Value = serde_json::from_str(j).unwrap_or_default();
            match args.get("format").and_then(Value::as_str).unwrap_or("text") {
                "json" => Ok(serde_json::to_string_pretty(&r).unwrap_or_default()),
                "hex" => match shared.receipt_raw(*id) {
                    Some(raw) => Ok(hex_dump(&raw)),
                    None => Err(ToolError::Failed(format!("Receipt #{id} is still printing; raw bytes are available once it is done."))),
                },
                _ => Ok(render_receipt(&r, columns)),
            }
        }
        "wait_for_receipt" => {
            let after = int("after_id").unwrap_or_else(|| shared.latest_done_id());
            let timeout = int("timeout_seconds").unwrap_or(30).clamp(1, 300);
            match shared.wait_for_done(after, Duration::from_secs(timeout)) {
                Some(id) => {
                    let list = shared.receipts_json();
                    let j = list.iter().find(|(rid, _)| *rid == id).map(|(_, j)| j.clone()).unwrap_or_default();
                    Ok(render_receipt(&serde_json::from_str(&j).unwrap_or_default(), columns))
                }
                None => Err(ToolError::Failed(format!("No new receipt after #{after} within {timeout} s."))),
            }
        }
        "clear_receipts" => {
            let n = shared.clear_receipts();
            Ok(format!("Cleared {n} receipt(s)."))
        }
        _ => Err(ToolError::Unknown),
    }
}

// ------------------------------------------------------------------ text rendering

fn text_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide(c as u32) { 2 } else { 1 }).sum()
}

fn aligned(s: &str, align: &str, columns: usize) -> String {
    let w = text_width(s);
    let pad = match align {
        "center" => columns.saturating_sub(w) / 2,
        "right" => columns.saturating_sub(w),
        _ => 0,
    };
    format!("{}{}", " ".repeat(pad), s).trim_end().to_string()
}

/// Render receipt items as plain-text lines laid out on `columns` characters.
fn render_lines(r: &Value, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let empty = vec![];
    for it in r["items"].as_array().unwrap_or(&empty) {
        let align = it["align"].as_str().unwrap_or("left");
        match it["type"].as_str().unwrap_or("") {
            "text" => {
                let mut s = String::new();
                for sp in it["spans"].as_array().unwrap_or(&empty) {
                    if let Some(dots) = sp["sp"].as_u64() {
                        s.push_str(&" ".repeat(((dots as f64) / 12.0).round() as usize));
                    } else {
                        s.push_str(sp["t"].as_str().unwrap_or(""));
                    }
                }
                lines.push(aligned(&s, align, columns));
            }
            "feed" => {
                let n = ((it["dots"].as_f64().unwrap_or(0.0)) / 30.0).round() as usize;
                lines.extend(std::iter::repeat_n(String::new(), n.min(5)));
            }
            "image" => {
                let (w, h) = (it["w"].as_u64().unwrap_or(0) * it["sx"].as_u64().unwrap_or(1), it["h"].as_u64().unwrap_or(0) * it["sy"].as_u64().unwrap_or(1));
                lines.push(aligned(&format!("[image {w}x{h} dots]"), align, columns));
            }
            "barcode" => lines.push(aligned(&format!("[{} barcode: {}]", it["kind"].as_str().unwrap_or("?"), it["data"].as_str().unwrap_or("")), align, columns)),
            "code2d" => {
                let kind = it["kind"].as_str().unwrap_or("2D");
                let label = if kind == "QR" { "QR code".to_string() } else { kind.to_string() };
                lines.push(aligned(&format!("[{label}: {}]", it["data"].as_str().unwrap_or("")), align, columns));
            }
            "note" => lines.push(aligned(it["text"].as_str().unwrap_or(""), "center", columns)),
            _ => {}
        }
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

pub fn render_receipt(r: &Value, columns: usize) -> String {
    let status = if r["done"] == true { if r["cut"] == true { "cut" } else { "done" } } else { "still printing" };
    let mut out = format!(
        "Receipt #{} | {} | {} {} | {} bytes | {}\n",
        r["id"], iso_utc(r["createdAt"].as_u64().unwrap_or(0)), r["source"].as_str().unwrap_or(""),
        r["remote"].as_str().unwrap_or(""), r["bytes"], status
    );
    let rule = "=".repeat(columns);
    out.push_str(&rule);
    out.push('\n');
    for l in render_lines(r, columns) {
        out.push_str(&l);
        out.push('\n');
    }
    if r["cut"] == true {
        out.push_str(&aligned("--- cut ---", "center", columns));
        out.push('\n');
    }
    out.push_str(&rule);
    out
}

fn hex_dump(raw: &[u8]) -> String {
    let mut out = format!("{} bytes{}\n", raw.len(), if raw.len() > MAX_HEX_BYTES { format!(" (showing first {MAX_HEX_BYTES})") } else { String::new() });
    for (n, row) in raw[..raw.len().min(MAX_HEX_BYTES)].chunks(16).enumerate() {
        let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
        let asc: String = row.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        out.push_str(&format!("{:06x}  {:<47}  {asc}\n", n * 16, hex.join(" ")));
    }
    out
}

/// Milliseconds since epoch → "YYYY-MM-DDTHH:MM:SSZ" (UTC), without a date crate.
fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ------------------------------------------------------------------ stdio bridge

/// `--mcp-stdio`: newline-delimited JSON-RPC on stdin/stdout, forwarded to the running app's
/// HTTP endpoint. For MCP clients that only launch local (stdio) servers.
pub fn run_stdio_bridge(viewer_port: u16) -> ! {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match post_json(viewer_port, line.as_bytes()) {
            Ok(body) => body,
            Err(e) => {
                // Only requests (with an id) get an answer.
                let msg: Value = serde_json::from_str(&line).unwrap_or_default();
                match msg.get("id") {
                    Some(id) if msg.get("method").is_some() => Some(rpc_error(id.clone(), -32000, &format!(
                        "ESC/POS Virtual Printer app is not reachable at http://127.0.0.1:{viewer_port}/mcp ({e}). Start the app (or pass --viewer-port)."
                    )).to_string()),
                    _ => None,
                }
            }
        };
        if let Some(body) = reply.filter(|b| !b.trim().is_empty()) {
            let _ = writeln!(stdout, "{}", body.trim());
            let _ = stdout.flush();
        }
    }
    std::process::exit(0)
}

fn post_json(port: u16, body: &[u8]) -> std::io::Result<Option<String>> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(330)))?;
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body)?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    let split = resp.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(resp.len());
    let body = String::from_utf8_lossy(&resp[split..]).to_string();
    Ok(if body.is_empty() { None } else { Some(body) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_layout() {
        let r = json!({ "id": 7, "createdAt": 1794457474795u64, "source": "tcp", "remote": "1.2.3.4:5", "bytes": 10,
            "done": true, "cut": true, "items": [
                { "type": "text", "align": "center", "spacing": 30, "spans": [{ "t": "Kedai", "f": "A" }] },
                { "type": "text", "align": "left", "spacing": 30, "spans": [{ "t": "Tea", "f": "A" }, { "sp": 120 }, { "t": "5.90", "f": "A" }] },
                { "type": "text", "align": "right", "spacing": 30, "spans": [{ "t": "谢谢", "f": "A" }] },
                { "type": "feed", "dots": 30 },
                { "type": "code2d", "kind": "QR", "data": "https://x.y/t/1", "align": "center" },
                { "type": "feed", "dots": 120 }
            ] });
        let lines = render_lines(&r, 20);
        assert_eq!(lines, vec![
            "       Kedai".to_string(),
            "Tea          5.90".to_string(),
            "                谢谢".to_string(),
            String::new(),
            "[QR code: https://x.y/t/1]".to_string(),
        ]);
        let full = render_receipt(&r, 20);
        assert!(full.starts_with("Receipt #7 | 2026-11-12T04:24:34Z | tcp 1.2.3.4:5 | 10 bytes | cut\n"));
        assert!(full.contains("--- cut ---"));
    }

    #[test]
    fn utc_dates() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00Z");
    }
}
