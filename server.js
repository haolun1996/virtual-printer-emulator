#!/usr/bin/env node
'use strict';
/**
 * ESC/POS virtual network printer.
 *   - Raw TCP (JetDirect / "port 9100") printing, incl. status replies (DLE EOT, GS r, GS I)
 *   - HTTP POST of raw ESC/POS bytes to the same port (for browser-based POS apps, CORS enabled)
 *   - Live receipt viewer at http://localhost:9101
 */
const net = require('node:net');
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { Printer } = require('./src/escpos');
const { buildSampleReceipt } = require('./src/sample');

const args = process.argv.slice(2);
const opt = (name, env, def) => {
  const i = args.indexOf(name);
  if (i >= 0 && args[i + 1] && !args[i + 1].startsWith('--')) return args[i + 1];
  return process.env[env] || def;
};
const flag = (name, env) => args.includes(name) || process.env[env] === '1';

if (flag('--help')) {
  console.log(`Usage: node server.js [options]
  --port <n>          printer TCP port            (PORT, default 9100)
  --host <addr>       printer bind address        (HOST, default 0.0.0.0 = reachable from LAN)
  --viewer-port <n>   web viewer port             (VIEWER_PORT, default 9101)
  --viewer-host <a>   web viewer bind address     (VIEWER_HOST, default 127.0.0.1)
  --save <dir>        save every job as .bin      (SAVE_DIR)
  --chinese           decode non-UTF-8 text as GB18030 by default, like Chinese-market printers (CHINESE_MODE=1)
  --open              open the viewer in the browser`);
  process.exit(0);
}

const PORT = Number(opt('--port', 'PORT', 9100));
const HOST = opt('--host', 'HOST', '0.0.0.0');
const VIEWER_PORT = Number(opt('--viewer-port', 'VIEWER_PORT', 9101));
const VIEWER_HOST = opt('--viewer-host', 'VIEWER_HOST', '127.0.0.1');
const SAVE_DIR = opt('--save', 'SAVE_DIR', '');
const CHINESE = flag('--chinese', 'CHINESE_MODE');
const MAX_RECEIPTS = 200;

// ------------------------------------------------------------------ state + live events

const receipts = new Map(); // id → Receipt (insertion order = arrival order)
const logLines = [];
const clients = new Set();
const pendingBroadcast = new Map();

function broadcast(event, data) {
  if (!clients.size) return;
  const msg = `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  for (const res of clients) res.write(msg);
}

function log(msg) {
  const entry = { t: Date.now(), msg };
  logLines.push(entry);
  if (logLines.length > 500) logLines.shift();
  console.log(`[${new Date(entry.t).toLocaleTimeString()}] ${msg}`);
  broadcast('log', entry);
}

function onUpdate(r) {
  if (!r.visible) return; // e.g. a connection that only polled status
  if (!receipts.has(r.id)) {
    receipts.set(r.id, r);
    while (receipts.size > MAX_RECEIPTS) receipts.delete(receipts.keys().next().value);
  }
  if (r.done) {
    clearTimeout(pendingBroadcast.get(r.id));
    pendingBroadcast.delete(r.id);
    broadcast('receipt', r);
    log(`receipt #${r.id} printed (${r.rawLength} bytes${r.cut ? ', cut' : ''}) from ${r.remote}`);
    saveJob(r);
  } else if (!pendingBroadcast.has(r.id)) {
    pendingBroadcast.set(r.id, setTimeout(() => {
      pendingBroadcast.delete(r.id);
      broadcast('receipt', r);
    }, 100));
  }
}

function saveJob(r) {
  if (!SAVE_DIR) return;
  try {
    fs.mkdirSync(SAVE_DIR, { recursive: true });
    const stamp = new Date(r.createdAt).toISOString().replace(/[:.]/g, '-');
    fs.writeFileSync(path.join(SAVE_DIR, `${stamp}-receipt-${r.id}.bin`), r.raw);
  } catch (e) {
    log(`could not save job: ${e.message}`);
  }
}

function makePrinter(remote, source, sock) {
  return new Printer({
    remote, source, chineseMode: CHINESE, onUpdate,
    onLog: (m) => log(`${remote}: ${m}`),
    respond: (buf) => { if (sock && !sock.destroyed && sock.writable) sock.write(buf); },
  });
}

// ------------------------------------------------------------------ printer port (raw TCP + HTTP)

const CORS = {
  'Access-Control-Allow-Origin': '*',
  'Access-Control-Allow-Methods': 'POST, PUT, GET, OPTIONS',
  'Access-Control-Allow-Headers': '*',
  'Access-Control-Allow-Private-Network': 'true',
  'Access-Control-Max-Age': '86400',
};

function httpReply(sock, status, body = '') {
  const head = [`HTTP/1.1 ${status}`, ...Object.entries(CORS).map(([k, v]) => `${k}: ${v}`),
    'Content-Type: text/plain; charset=utf-8', `Content-Length: ${Buffer.byteLength(body)}`,
    'Connection: close', '', ''].join('\r\n');
  sock.end(head + body);
}

// allowHalfOpen: we close our side ourselves after replying (HTTP bodies without Content-Length).
const printerServer = net.createServer({ allowHalfOpen: true }, (sock) => {
  const remote = `${(sock.remoteAddress || '?').replace(/^::ffff:/, '')}:${sock.remotePort}`;
  let mode = null; // 'raw' | 'http'
  let buf = Buffer.alloc(0);
  let printer = null;
  let total = 0;
  sock.setNoDelay(true);
  log(`connection from ${remote}`);

  const startRaw = () => {
    mode = 'raw';
    printer = makePrinter(remote, 'tcp', sock);
    printer.feed(buf);
    buf = null;
  };

  const tryHttp = (ended) => {
    const headerEnd = buf.indexOf('\r\n\r\n');
    if (headerEnd < 0) {
      if (ended || buf.length > 65536) httpReply(sock, '400 Bad Request', 'bad request');
      return;
    }
    const headers = buf.toString('latin1', 0, headerEnd).split('\r\n');
    const [method, target] = headers[0].split(' ');
    const cl = headers.find((h) => /^content-length:/i.test(h));
    const length = cl ? Number(cl.split(':')[1]) : null;
    const body = buf.subarray(headerEnd + 4);
    if (length !== null ? body.length < length : (!ended && method !== 'GET' && method !== 'OPTIONS' && method !== 'HEAD')) return;
    mode = 'done';
    if (method === 'OPTIONS') return httpReply(sock, '204 No Content');
    if (method === 'POST' || method === 'PUT') {
      const p = makePrinter(remote, 'http', null);
      p.feed(length !== null ? body.subarray(0, length) : body);
      p.end();
      log(`${remote}: HTTP ${method} ${target} (${length ?? body.length} bytes)`);
      return httpReply(sock, '200 OK', 'OK');
    }
    httpReply(sock, '200 OK', `ESC/POS virtual printer. Send raw ESC/POS bytes here (TCP or HTTP POST).\nViewer: http://localhost:${VIEWER_PORT}\n`);
  };

  sock.on('data', (chunk) => {
    total += chunk.length;
    if (mode === 'raw') return printer.feed(chunk);
    if (mode === 'done') return;
    buf = Buffer.concat([buf, chunk]);
    if (mode === null) {
      const head = buf.toString('latin1', 0, 8);
      if (buf.length < 8 && /^[A-Z]*$/.test(head)) return; // could still become an HTTP verb
      if (/^(GET|HEAD|POST|PUT|OPTIONS) /.test(head)) mode = 'http';
      else return startRaw();
    }
    tryHttp(false);
  });

  sock.on('end', () => {
    if (mode === null && buf.length) startRaw();
    if (mode === 'http') tryHttp(true);
    if (printer) printer.end();
    sock.end();
  });

  sock.on('error', (e) => log(`${remote}: socket error ${e.code || e.message}`));
  sock.on('close', () => log(`${remote} disconnected (${total} bytes received)`));
});

// ------------------------------------------------------------------ viewer (web UI + API)

const INDEX_HTML = path.join(__dirname, 'public', 'index.html');

function lanAddresses() {
  return Object.values(os.networkInterfaces()).flat()
    .filter((a) => a && a.family === 'IPv4' && !a.internal).map((a) => a.address);
}

function sendSample(cols) {
  return new Promise((resolve, reject) => {
    const s = net.connect(PORT, '127.0.0.1', () => s.end(buildSampleReceipt(cols)));
    s.on('close', resolve);
    s.on('error', reject);
    s.resume();
  });
}

const viewer = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  const json = (code, data) => { res.writeHead(code, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(data)); };
  try {
    if (req.method === 'GET' && url.pathname === '/') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-cache' });
      return fs.createReadStream(INDEX_HTML).pipe(res);
    }
    if (req.method === 'GET' && url.pathname === '/events') {
      res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' });
      const config = { port: PORT, host: HOST, addresses: lanAddresses(), chinese: CHINESE };
      res.write(`event: init\ndata: ${JSON.stringify({ receipts: [...receipts.values()], log: logLines, config })}\n\n`);
      clients.add(res);
      req.on('close', () => clients.delete(res));
      return;
    }
    if (req.method === 'GET' && url.pathname === '/api/receipts') return json(200, [...receipts.values()]);
    const m = url.pathname.match(/^\/api\/receipts\/(\d+)\/raw$/);
    if (req.method === 'GET' && m) {
      const r = receipts.get(Number(m[1]));
      if (!r) return json(404, { error: 'not found' });
      res.writeHead(200, { 'Content-Type': 'application/octet-stream',
        'Content-Disposition': `attachment; filename="receipt-${r.id}.bin"` });
      return res.end(r.raw);
    }
    if (req.method === 'POST' && url.pathname === '/api/clear') {
      receipts.clear();
      broadcast('clear', {});
      return json(200, { ok: true });
    }
    if (req.method === 'POST' && url.pathname === '/api/test') {
      await sendSample(Number(url.searchParams.get('cols')) || 48);
      return json(200, { ok: true });
    }
    json(404, { error: 'not found' });
  } catch (e) {
    json(500, { error: e.message });
  }
});

setInterval(() => { for (const res of clients) res.write(': ping\n\n'); }, 25000).unref();

// ------------------------------------------------------------------ start

function listen(server, port, host, name) {
  return new Promise((resolve) => {
    server.once('error', (e) => {
      if (e.code === 'EADDRINUSE') {
        console.error(`\n✖ ${name} port ${port} is already in use. Find the process with:\n    lsof -nP -iTCP:${port} -sTCP:LISTEN\n`);
      } else {
        console.error(`\n✖ ${name} failed to listen on ${host}:${port}: ${e.message}\n`);
      }
      process.exit(1);
    });
    server.listen(port, host, resolve);
  });
}

(async () => {
  await listen(printerServer, PORT, HOST, 'Printer');
  await listen(viewer, VIEWER_PORT, VIEWER_HOST, 'Viewer');
  const viewerUrl = `http://localhost:${VIEWER_PORT}`;
  const ips = HOST === '0.0.0.0' ? lanAddresses() : [HOST];
  console.log(`
  🖨  ESC/POS virtual printer is running
     Printer : tcp://${HOST}:${PORT}${ips.length ? `   (LAN: ${ips.map((ip) => `${ip}:${PORT}`).join(', ')})` : ''}
     Viewer  : ${viewerUrl}
     Encoding: ${CHINESE ? 'GB18030 (Chinese mode) for non-UTF-8 text' : 'UTF-8 auto-detect, else ESC t code page (CP437 default)'}${SAVE_DIR ? `\n     Saving  : ${path.resolve(SAVE_DIR)}` : ''}

  Quick test:  printf '\\x1b@Hello from macOS\\n\\x1dV\\x00' | nc -w1 localhost ${PORT}
`);
  if (flag('--open', 'OPEN') && process.platform === 'darwin') spawn('open', [viewerUrl], { stdio: 'ignore', detached: true }).unref();
})();

process.on('SIGINT', () => { console.log('\nbye'); process.exit(0); });
