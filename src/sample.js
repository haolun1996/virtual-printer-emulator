'use strict';
/** Builds a demo ESC/POS receipt exercising text styles, raster image, barcode, QR and cut. */

const ESC = 0x1b, GS = 0x1d;

function buildSampleReceipt(cols = 48) {
  const parts = [];
  const raw = (...bytes) => parts.push(Buffer.from(bytes));
  const text = (s) => parts.push(Buffer.from(s, 'utf8'));
  const row = (left, right) => text(left.slice(0, cols - right.length - 1).padEnd(cols - right.length) + right + '\n');
  const rule = (ch = '-') => text(ch.repeat(cols) + '\n');

  raw(ESC, 0x40);                          // initialize
  raw(ESC, 0x61, 1);                       // center

  // Raster logo (GS v 0): 160x56 rounded frame with a diagonal stripe pattern.
  const w = 160, h = 56, bpr = w / 8;
  const img = Buffer.alloc(bpr * h);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const edge = x < 4 || x >= w - 4 || y < 4 || y >= h - 4;
      const stripe = x > 10 && x < w - 10 && y > 10 && y < h - 10 && (x + y) % 12 < 4;
      if (edge || stripe) img[y * bpr + (x >> 3)] |= 0x80 >> (x & 7);
    }
  }
  raw(GS, 0x76, 0x30, 0, bpr & 0xff, bpr >> 8, h & 0xff, h >> 8);
  parts.push(img);
  text('\n');

  raw(GS, 0x21, 0x11); text('VIRTUAL CAFE\n'); raw(GS, 0x21, 0x00);
  text('1 Sample Street\nTel: 000-000 0000\n');
  raw(ESC, 0x61, 0);                       // left
  rule();
  row('Order #1042', new Date().toLocaleString('en-GB').slice(0, cols - 13));
  rule();
  row('2 x Flat White', '13.00');
  row('1 x Croissant', '6.50');
  row('1 x Iced Lemon Tea (less ice)', '5.90');
  rule();
  row('Subtotal', '25.40');
  row('Service tax 6%', '1.52');
  raw(ESC, 0x45, 1); raw(ESC, 0x21, 0x18); // bold + double height
  row('TOTAL', '26.92');
  raw(ESC, 0x21, 0x00); raw(ESC, 0x45, 0);
  rule('=');

  text('Styles: ');
  raw(ESC, 0x2d, 1); text('underline'); raw(ESC, 0x2d, 0); text(' ');
  raw(GS, 0x42, 1); text(' inverted '); raw(GS, 0x42, 0); text(' ');
  raw(ESC, 0x45, 1); text('bold'); raw(ESC, 0x45, 0); text('\n');
  raw(ESC, 0x4d, 1); text('Font B is the small font used for fine print and footers.\n'); raw(ESC, 0x4d, 0);
  raw(GS, 0x21, 0x10); text('WIDE'); raw(GS, 0x21, 0x00); text(' and ');
  raw(GS, 0x21, 0x11); text('BIG'); raw(GS, 0x21, 0x00); text('\n');
  text('UTF-8: Terima kasih · 谢谢 · ありがとう\n\n');

  raw(ESC, 0x61, 1);
  raw(GS, 0x68, 80); raw(GS, 0x77, 2); raw(GS, 0x48, 2);        // barcode height, width, HRI below
  const code = Buffer.from('{BORDER-1042');
  raw(GS, 0x6b, 73, code.length); parts.push(code);
  text('\n');

  const qr = Buffer.from('https://example.com/receipt/1042');
  raw(GS, 0x28, 0x6b, 4, 0, 49, 65, 50, 0);                   // QR model 2
  raw(GS, 0x28, 0x6b, 3, 0, 49, 67, 6);                       // module size 6
  raw(GS, 0x28, 0x6b, 3, 0, 49, 69, 49);                      // error correction M
  raw(GS, 0x28, 0x6b, (qr.length + 3) & 0xff, (qr.length + 3) >> 8, 49, 80, 48); parts.push(qr);
  raw(GS, 0x28, 0x6b, 3, 0, 49, 81, 48);                      // print
  text('\nThank you, come again!\n');

  raw(ESC, 0x64, 4);                       // feed 4 lines
  raw(GS, 0x56, 66, 0);                    // partial cut
  return Buffer.concat(parts);
}

module.exports = { buildSampleReceipt };
