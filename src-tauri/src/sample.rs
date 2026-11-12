//! Demo ESC/POS receipt exercising text styles, raster image, barcode, QR and cut.

const ESC: u8 = 0x1b;
const GS: u8 = 0x1d;

pub fn build_sample_receipt(cols: usize) -> Vec<u8> {
    let cols = cols.clamp(24, 64);
    let mut out: Vec<u8> = Vec::new();
    let raw = |out: &mut Vec<u8>, bytes: &[u8]| out.extend_from_slice(bytes);
    let text = |out: &mut Vec<u8>, s: &str| out.extend_from_slice(s.as_bytes());
    let row = |out: &mut Vec<u8>, left: &str, right: &str| {
        let room = cols - right.len() - 1;
        let l: String = left.chars().take(room).collect();
        out.extend_from_slice(format!("{l:<width$}{right}\n", width = cols - right.len()).as_bytes());
    };
    let rule = |out: &mut Vec<u8>, ch: char| out.extend_from_slice(format!("{}\n", ch.to_string().repeat(cols)).as_bytes());

    raw(&mut out, &[ESC, 0x40]); // initialize
    raw(&mut out, &[ESC, 0x61, 1]); // center

    // Raster logo (GS v 0): 160x56 frame with a diagonal stripe pattern.
    let (w, h) = (160usize, 56usize);
    let bpr = w / 8;
    let mut img = vec![0u8; bpr * h];
    for y in 0..h {
        for x in 0..w {
            let edge = x < 4 || x >= w - 4 || y < 4 || y >= h - 4;
            let stripe = x > 10 && x < w - 10 && y > 10 && y < h - 10 && (x + y) % 12 < 4;
            if edge || stripe {
                img[y * bpr + (x >> 3)] |= 0x80 >> (x & 7);
            }
        }
    }
    raw(&mut out, &[GS, 0x76, 0x30, 0, bpr as u8, 0, h as u8, 0]);
    raw(&mut out, &img);
    text(&mut out, "\n");

    raw(&mut out, &[GS, 0x21, 0x11]);
    text(&mut out, "VIRTUAL CAFE\n");
    raw(&mut out, &[GS, 0x21, 0x00]);
    text(&mut out, "1 Sample Street\nTel: 000-000 0000\n");
    raw(&mut out, &[ESC, 0x61, 0]); // left
    rule(&mut out, '-');
    row(&mut out, "Order #1042", "Table 5");
    rule(&mut out, '-');
    row(&mut out, "2 x Flat White", "13.00");
    row(&mut out, "1 x Croissant", "6.50");
    row(&mut out, "1 x Iced Lemon Tea (less ice)", "5.90");
    rule(&mut out, '-');
    row(&mut out, "Subtotal", "25.40");
    row(&mut out, "Service tax 6%", "1.52");
    raw(&mut out, &[ESC, 0x45, 1, ESC, 0x21, 0x18]); // bold + double height
    row(&mut out, "TOTAL", "26.92");
    raw(&mut out, &[ESC, 0x21, 0x00, ESC, 0x45, 0]);
    rule(&mut out, '=');

    text(&mut out, "Styles: ");
    raw(&mut out, &[ESC, 0x2d, 1]);
    text(&mut out, "underline");
    raw(&mut out, &[ESC, 0x2d, 0]);
    text(&mut out, " ");
    raw(&mut out, &[GS, 0x42, 1]);
    text(&mut out, " inverted ");
    raw(&mut out, &[GS, 0x42, 0]);
    text(&mut out, " ");
    raw(&mut out, &[ESC, 0x45, 1]);
    text(&mut out, "bold");
    raw(&mut out, &[ESC, 0x45, 0]);
    text(&mut out, "\n");
    raw(&mut out, &[ESC, 0x4d, 1]);
    text(&mut out, "Font B is the small font used for fine print and footers.\n");
    raw(&mut out, &[ESC, 0x4d, 0, GS, 0x21, 0x10]);
    text(&mut out, "WIDE");
    raw(&mut out, &[GS, 0x21, 0x00]);
    text(&mut out, " and ");
    raw(&mut out, &[GS, 0x21, 0x11]);
    text(&mut out, "BIG");
    raw(&mut out, &[GS, 0x21, 0x00]);
    text(&mut out, "\nUTF-8: Terima kasih · 谢谢 · ありがとう\n\n");

    raw(&mut out, &[ESC, 0x61, 1]);
    raw(&mut out, &[GS, 0x68, 80, GS, 0x77, 2, GS, 0x48, 2]); // barcode height, width, HRI below
    let code = b"{BORDER-1042";
    raw(&mut out, &[GS, 0x6b, 73, code.len() as u8]);
    raw(&mut out, code);
    text(&mut out, "\n");

    let qr = b"https://example.com/receipt/1042";
    let n = qr.len() + 3;
    raw(&mut out, &[GS, 0x28, 0x6b, 4, 0, 49, 65, 50, 0]); // QR model 2
    raw(&mut out, &[GS, 0x28, 0x6b, 3, 0, 49, 67, 6]); // module size 6
    raw(&mut out, &[GS, 0x28, 0x6b, 3, 0, 49, 69, 49]); // error correction M
    raw(&mut out, &[GS, 0x28, 0x6b, (n & 0xff) as u8, (n >> 8) as u8, 49, 80, 48]);
    raw(&mut out, qr);
    raw(&mut out, &[GS, 0x28, 0x6b, 3, 0, 49, 81, 48]); // print
    text(&mut out, "\nThank you, come again!\n");

    raw(&mut out, &[ESC, 0x64, 4]); // feed 4 lines
    raw(&mut out, &[GS, 0x56, 66, 0]); // partial cut
    out
}
