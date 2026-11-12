#!/usr/bin/env node
'use strict';
// Usage: node examples/test-print.js [host=127.0.0.1] [port=9100] [columns=48]
// Asks the printer for its status (DLE EOT 1), then prints the sample receipt.
const net = require('node:net');
const { buildSampleReceipt } = require('../src/sample');

const [host = '127.0.0.1', port = '9100', cols = '48'] = process.argv.slice(2);
const sock = net.connect(Number(port), host, () => {
  sock.write(Buffer.from([0x10, 0x04, 0x01])); // DLE EOT 1 — real-time status
});
sock.once('data', (d) => {
  console.log(`status reply: 0x${d[0].toString(16).padStart(2, '0')} ${d[0] & 0x08 ? '(offline)' : '(online)'}`);
  sock.end(buildSampleReceipt(Number(cols)));
});
sock.on('close', () => console.log(`sent sample receipt to ${host}:${port}`));
sock.on('error', (e) => { console.error(`cannot reach printer at ${host}:${port}: ${e.message}`); process.exit(1); });
