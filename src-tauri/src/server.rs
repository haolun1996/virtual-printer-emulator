//! Two servers in one process, std-only (no async runtime):
//!   - printer port (default 9100): raw TCP ESC/POS, or HTTP POST of raw bytes (CORS enabled)
//!   - viewer (default 127.0.0.1:9101): ui/index.html, SSE live updates at /events, small JSON API

use crate::escpos::{now_ms, Event, Printer};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const INDEX_HTML: &str = include_str!("../../ui/index.html");
const MAX_RECEIPTS: usize = 200;
const MAX_LOG: usize = 500;
const EMIT_EVERY: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub port_from_cli: bool,
    pub host: String,
    pub viewer_port: u16,
    pub viewer_host: String,
    pub save_dir: Option<PathBuf>,
    pub chinese: bool,
    pub settings_file: Option<PathBuf>,
}

pub const HELP: &str = "Usage: escpos-virtual-printer [options]
  --headless          run without a window (server only)
  --port <n>          printer TCP port            (PORT, default 9100)
  --host <addr>       printer bind address        (HOST, default 0.0.0.0 = reachable from LAN)
  --viewer-port <n>   web viewer port             (VIEWER_PORT, default 9101)
  --viewer-host <a>   web viewer bind address     (VIEWER_HOST, default 127.0.0.1)
  --save <dir>        save every job as .bin      (SAVE_DIR)
  --chinese           decode non-UTF-8 text as GB18030 by default (CHINESE_MODE=1)";

impl Config {
    /// Parse CLI flags (with env var fallbacks). Returns the config and whether to run headless.
    pub fn from_args() -> (Config, bool) {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let opt = |name: &str, env: &str| -> Option<String> {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .filter(|v| !v.starts_with("--"))
                .cloned()
                .or_else(|| std::env::var(env).ok().filter(|v| !v.is_empty()))
        };
        let flag = |name: &str, env: &str| args.iter().any(|a| a == name) || std::env::var(env).map(|v| v == "1").unwrap_or(false);
        if flag("--help", "__NONE__") {
            println!("{HELP}");
            std::process::exit(0);
        }
        let port_opt = opt("--port", "PORT").and_then(|v| v.parse().ok());
        let cfg = Config {
            port: port_opt.unwrap_or(9100),
            port_from_cli: port_opt.is_some(),
            host: opt("--host", "HOST").unwrap_or_else(|| "0.0.0.0".into()),
            viewer_port: opt("--viewer-port", "VIEWER_PORT").and_then(|v| v.parse().ok()).unwrap_or(9101),
            viewer_host: opt("--viewer-host", "VIEWER_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            save_dir: opt("--save", "SAVE_DIR").map(PathBuf::from),
            chinese: flag("--chinese", "CHINESE_MODE"),
            settings_file: None,
        };
        (cfg, flag("--headless", "HEADLESS"))
    }
}

pub fn load_saved_port(file: &PathBuf) -> Option<u16> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    v["port"].as_u64().and_then(|p| u16::try_from(p).ok())
}

struct Stored {
    id: u64,
    json: String,
    raw: Vec<u8>,
}

struct Status {
    port: u16,
    listening: bool,
    error: Option<String>,
}

pub struct Shared {
    cfg: Config,
    pub viewer_port: u16,
    receipts: Mutex<Vec<Stored>>,
    log: Mutex<VecDeque<Value>>,
    clients: Mutex<Vec<TcpStream>>,
    status: Mutex<Status>,
}

/// Start the viewer and (try to) start the printer port. A busy printer port is not
/// fatal: the viewer shows it and lets the user pick another port.
pub fn start(cfg: Config, viewer_fallback: bool) -> Result<Arc<Shared>, String> {
    let viewer = TcpListener::bind((cfg.viewer_host.as_str(), cfg.viewer_port))
        .or_else(|e| if viewer_fallback { TcpListener::bind((cfg.viewer_host.as_str(), 0)) } else { Err(e) })
        .map_err(|e| format!("Viewer could not listen on {}:{}: {e}", cfg.viewer_host, cfg.viewer_port))?;
    let viewer_port = viewer.local_addr().map(|a| a.port()).unwrap_or(cfg.viewer_port);
    let shared = Arc::new(Shared {
        status: Mutex::new(Status { port: cfg.port, listening: false, error: None }),
        cfg,
        viewer_port,
        receipts: Mutex::new(Vec::new()),
        log: Mutex::new(VecDeque::new()),
        clients: Mutex::new(Vec::new()),
    });

    let s = shared.clone();
    thread::spawn(move || {
        for stream in viewer.incoming().flatten() {
            let s = s.clone();
            thread::spawn(move || s.handle_viewer(stream));
        }
    });
    let s = shared.clone();
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(20));
        s.broadcast_raw(": ping\n\n");
    });

    let port = shared.cfg.port;
    if let Err(e) = shared.listen_printer(port) {
        shared.log(e);
    }
    Ok(shared)
}

fn lan_address() -> Option<String> {
    // Picks the interface used for outbound traffic; connect() on UDP sends nothing.
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

impl Shared {
    pub fn print_banner(&self) {
        let st = self.status.lock().unwrap();
        let lan = lan_address().map(|ip| format!("   (LAN: {ip}:{})", st.port)).unwrap_or_default();
        println!(
            "\n  ESC/POS virtual printer\n     Printer : {}\n     Viewer  : http://localhost:{}\n",
            if st.listening { format!("tcp://{}:{}{lan}", self.cfg.host, st.port) } else { format!("NOT LISTENING: {}", st.error.clone().unwrap_or_default()) },
            self.viewer_port
        );
    }

    pub fn log(&self, msg: impl Into<String>) {
        let msg = msg.into();
        let entry = json!({ "t": now_ms(), "msg": msg });
        {
            let mut log = self.log.lock().unwrap();
            log.push_back(entry.clone());
            while log.len() > MAX_LOG {
                log.pop_front();
            }
        }
        let _ = writeln!(std::io::stdout(), "{msg}");
        self.broadcast("log", &entry.to_string());
    }

    fn broadcast_raw(&self, msg: &str) {
        let mut clients = self.clients.lock().unwrap();
        clients.retain(|c| {
            let mut w: &TcpStream = c;
            w.write_all(msg.as_bytes()).and_then(|_| w.flush()).is_ok()
        });
    }

    fn broadcast(&self, event: &str, data: &str) {
        self.broadcast_raw(&format!("event: {event}\ndata: {data}\n\n"));
    }

    fn config_json(&self) -> Value {
        let st = self.status.lock().unwrap();
        json!({ "port": st.port, "host": self.cfg.host, "addresses": lan_address().into_iter().collect::<Vec<_>>(),
            "chinese": self.cfg.chinese, "listening": st.listening, "error": st.error })
    }

    fn upsert(&self, id: u64, json: String, raw: Option<Vec<u8>>) {
        {
            let mut list = self.receipts.lock().unwrap();
            if let Some(r) = list.iter_mut().find(|r| r.id == id) {
                r.json = json.clone();
                if let Some(raw) = raw {
                    r.raw = raw;
                }
            } else {
                list.push(Stored { id, json: json.clone(), raw: raw.unwrap_or_default() });
                if list.len() > MAX_RECEIPTS {
                    list.remove(0);
                }
            }
        }
        self.broadcast("receipt", &json);
    }

    fn save_job(&self, id: u64, raw: &[u8]) {
        let Some(dir) = &self.cfg.save_dir else { return };
        let path = dir.join(format!("{}-receipt-{id}.bin", now_ms()));
        if let Err(e) = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, raw)) {
            self.log(format!("could not save job: {e}"));
        }
    }

    // ------------------------------------------------------------------ printer port

    pub fn listen_printer(self: &Arc<Self>, port: u16) -> Result<(), String> {
        if self.status.lock().unwrap().listening {
            return Err("the printer port is already listening; restart the app to change it".into());
        }
        let result = TcpListener::bind((self.cfg.host.as_str(), port));
        let outcome = match result {
            Ok(listener) => {
                let s = self.clone();
                thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let s = s.clone();
                        thread::spawn(move || s.handle_printer_conn(stream));
                    }
                });
                *self.status.lock().unwrap() = Status { port, listening: true, error: None };
                if let Some(file) = &self.cfg.settings_file {
                    let _ = std::fs::create_dir_all(file.parent().unwrap_or(file));
                    let _ = std::fs::write(file, json!({ "port": port }).to_string());
                }
                self.log(format!("printer listening on {}:{port}", self.cfg.host));
                Ok(())
            }
            Err(e) => {
                let msg = if e.kind() == ErrorKind::AddrInUse {
                    format!("Port {port} is already in use by another app (Flutter/Dart DevTools uses 9100 by default).")
                } else {
                    format!("Could not listen on port {port}: {e}")
                };
                *self.status.lock().unwrap() = Status { port, listening: false, error: Some(msg.clone()) };
                Err(msg)
            }
        };
        self.broadcast("status", &self.config_json().to_string());
        outcome
    }

    fn handle_printer_conn(self: Arc<Self>, mut stream: TcpStream) {
        let remote = stream.peer_addr().map(|a| a.to_string().replace("[::ffff:", "").replace("]:", ":")).unwrap_or_else(|_| "?".into());
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(EMIT_EVERY));
        self.log(format!("connection from {remote}"));

        let mut chunk = vec![0u8; 64 * 1024];
        let mut head: Vec<u8> = Vec::new();
        let mut printer: Option<Printer> = None;
        let mut total = 0usize;
        let mut last_emit = Instant::now();
        let mut dirty = false;

        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if printer.is_none() {
                        head.extend_from_slice(&chunk[..n]);
                        let probe = &head[..head.len().min(8)];
                        if head.len() < 8 && probe.iter().all(|c| c.is_ascii_uppercase()) {
                            continue; // could still become an HTTP verb
                        }
                        if [&b"GET "[..], b"HEAD ", b"POST ", b"PUT ", b"OPTIONS "].iter().any(|v| head.starts_with(v)) {
                            let head = std::mem::take(&mut head);
                            self.handle_printer_http(&mut stream, head, &remote);
                            self.log(format!("{remote} disconnected ({total} bytes received)"));
                            return;
                        }
                        let mut p = Printer::new(&remote, "tcp", self.cfg.chinese);
                        p.feed(&std::mem::take(&mut head));
                        printer = Some(p);
                    } else if let Some(p) = printer.as_mut() {
                        p.feed(&chunk[..n]);
                    }
                    dirty = true;
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                Err(e) => {
                    self.log(format!("{remote}: socket error {e}"));
                    break;
                }
            }
            if let Some(p) = printer.as_mut() {
                if dirty && last_emit.elapsed() >= EMIT_EVERY {
                    p.emit();
                    last_emit = Instant::now();
                    dirty = false;
                }
                self.process_events(p, &mut stream, &remote);
            }
        }
        if printer.is_none() && !head.is_empty() {
            let mut p = Printer::new(&remote, "tcp", self.cfg.chinese);
            p.feed(&head);
            printer = Some(p);
        }
        if let Some(p) = printer.as_mut() {
            p.end();
            self.process_events(p, &mut stream, &remote);
        }
        let _ = stream.shutdown(Shutdown::Both);
        self.log(format!("{remote} disconnected ({total} bytes received)"));
    }

    fn process_events(&self, p: &mut Printer, stream: &mut TcpStream, remote: &str) {
        for e in std::mem::take(&mut p.events) {
            match e {
                Event::Respond(b) => {
                    let _ = stream.write_all(&b);
                }
                Event::Log(m) => self.log(format!("{remote}: {m}")),
                Event::Update { id, json, done, cut, raw } => {
                    if done {
                        let bytes = raw.as_ref().map(|r| r.len()).unwrap_or(0);
                        if let Some(r) = &raw {
                            self.save_job(id, r);
                        }
                        self.upsert(id, json, raw);
                        self.log(format!("receipt #{id} printed ({bytes} bytes{}) from {remote}", if cut { ", cut" } else { "" }));
                    } else {
                        self.upsert(id, json, None);
                    }
                }
            }
        }
    }

    fn handle_printer_http(&self, stream: &mut TcpStream, head: Vec<u8>, remote: &str) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let Some(req) = read_request(stream, head) else {
            return reply(stream, "400 Bad Request", "text/plain", b"bad request", &[]);
        };
        let cors = [
            ("Access-Control-Allow-Origin", "*"),
            ("Access-Control-Allow-Methods", "POST, PUT, GET, OPTIONS"),
            ("Access-Control-Allow-Headers", "*"),
            ("Access-Control-Allow-Private-Network", "true"),
            ("Access-Control-Max-Age", "86400"),
        ];
        match req.method.as_str() {
            "OPTIONS" => reply(stream, "204 No Content", "text/plain", b"", &cors),
            "POST" | "PUT" => {
                let mut p = Printer::new(remote, "http", self.cfg.chinese);
                p.feed(&req.body);
                p.end();
                // status replies make no sense over HTTP: drop them, keep the rest
                p.events.retain(|e| !matches!(e, Event::Respond(_)));
                self.process_events(&mut p, stream, remote);
                self.log(format!("{remote}: HTTP {} {} ({} bytes)", req.method, req.target, req.body.len()));
                reply(stream, "200 OK", "text/plain", b"OK", &cors);
            }
            _ => {
                let body = format!("ESC/POS virtual printer. Send raw ESC/POS bytes here (TCP or HTTP POST).\nViewer: http://localhost:{}\n", self.viewer_port);
                reply(stream, "200 OK", "text/plain; charset=utf-8", body.as_bytes(), &cors);
            }
        }
    }

    // ------------------------------------------------------------------ viewer

    fn handle_viewer(self: Arc<Self>, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let Some(req) = read_request(&mut stream, Vec::new()) else { return };
        let (path, query) = req.target.split_once('?').unwrap_or((req.target.as_str(), ""));
        let param = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{name}="))).and_then(|v| v.parse::<u32>().ok());
        let json_reply = |stream: &mut TcpStream, status: &str, v: Value| reply(stream, status, "application/json", v.to_string().as_bytes(), &[]);

        match (req.method.as_str(), path) {
            ("GET", "/") | ("GET", "/index.html") => {
                reply(&mut stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes(), &[("Cache-Control", "no-cache")])
            }
            ("GET", "/events") => {
                let receipts: Vec<String> = self.receipts.lock().unwrap().iter().map(|r| r.json.clone()).collect();
                let log: Vec<Value> = self.log.lock().unwrap().iter().cloned().collect();
                let init = format!(
                    "{{\"receipts\":[{}],\"log\":{},\"config\":{}}}",
                    receipts.join(","),
                    Value::Array(log),
                    self.config_json()
                );
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                if stream.write_all(format!("{head}event: init\ndata: {init}\n\n").as_bytes()).is_ok() {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    self.clients.lock().unwrap().push(stream);
                }
            }
            ("GET", "/api/receipts") => {
                let body = format!("[{}]", self.receipts.lock().unwrap().iter().map(|r| r.json.as_str()).collect::<Vec<_>>().join(","));
                reply(&mut stream, "200 OK", "application/json", body.as_bytes(), &[]);
            }
            ("GET", p) if p.starts_with("/api/receipts/") && p.ends_with("/raw") => {
                let id: Option<u64> = p.trim_start_matches("/api/receipts/").trim_end_matches("/raw").parse().ok();
                let raw = id.and_then(|id| self.receipts.lock().unwrap().iter().find(|r| r.id == id && !r.raw.is_empty()).map(|r| r.raw.clone()));
                match (id, raw) {
                    (Some(id), Some(raw)) => {
                        let disp = format!("attachment; filename=\"receipt-{id}.bin\"");
                        reply(&mut stream, "200 OK", "application/octet-stream", &raw, &[("Content-Disposition", disp.as_str())]);
                    }
                    _ => json_reply(&mut stream, "404 Not Found", json!({ "error": "not found (or still printing)" })),
                }
            }
            ("POST", "/api/clear") => {
                self.receipts.lock().unwrap().clear();
                self.broadcast("clear", "{}");
                json_reply(&mut stream, "200 OK", json!({ "ok": true }));
            }
            ("POST", "/api/test") => {
                let st = self.status.lock().unwrap();
                let (listening, port) = (st.listening, st.port);
                drop(st);
                if !listening {
                    return json_reply(&mut stream, "409 Conflict", json!({ "error": "printer port is not listening" }));
                }
                let cols = param("cols").unwrap_or(48) as usize;
                let sent = TcpStream::connect(("127.0.0.1", port)).and_then(|mut s| {
                    s.write_all(&crate::sample::build_sample_receipt(cols))?;
                    s.shutdown(Shutdown::Write)?;
                    let mut sink = Vec::new();
                    let _ = s.read_to_end(&mut sink);
                    Ok(())
                });
                match sent {
                    Ok(()) => json_reply(&mut stream, "200 OK", json!({ "ok": true })),
                    Err(e) => json_reply(&mut stream, "500 Internal Server Error", json!({ "error": e.to_string() })),
                }
            }
            ("POST", "/api/listen") => match param("port").and_then(|p| u16::try_from(p).ok()).filter(|&p| p > 0) {
                None => json_reply(&mut stream, "400 Bad Request", json!({ "error": "invalid port" })),
                Some(port) => match self.listen_printer(port) {
                    Ok(()) => json_reply(&mut stream, "200 OK", json!({ "ok": true })),
                    Err(e) => json_reply(&mut stream, "409 Conflict", json!({ "error": e })),
                },
            },
            _ => json_reply(&mut stream, "404 Not Found", json!({ "error": "not found" })),
        }
    }
}

// ------------------------------------------------------------------ tiny HTTP helpers

struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
}

/// Read one HTTP/1.x request (headers + Content-Length body; without a length the body
/// runs to EOF). `buf` holds bytes already read from the stream.
fn read_request(stream: &mut TcpStream, mut buf: Vec<u8>) -> Option<Request> {
    let mut chunk = [0u8; 16 * 1024];
    let header_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let length = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok());
    let mut body = buf[header_end + 4..].to_vec();
    let expects_body = matches!(method.as_str(), "POST" | "PUT");
    match length {
        Some(len) => {
            while body.len() < len {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                }
            }
            body.truncate(len);
        }
        None if expects_body => loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        },
        None => {}
    }
    Some(Request { method, target, body })
}

fn reply(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8], headers: &[(&str, &str)]) {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}
