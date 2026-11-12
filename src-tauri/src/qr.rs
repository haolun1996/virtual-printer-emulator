//! Minimal QR Code (model 2) encoder: byte mode, versions 1–40, EC levels L/M/Q/H,
//! automatic mask selection. Follows ISO/IEC 18004; structure modelled on
//! Project Nayuki's reference design.

// Indexed [ecl][version]; ecl order L, M, Q, H. Index 0 unused.
const ECC_PER_BLOCK: [[i16; 41]; 4] = [
    [-1, 7, 10, 15, 20, 26, 18, 20, 24, 30, 18, 20, 24, 26, 30, 22, 24, 28, 30, 28, 28, 28, 28, 30, 30, 26, 28, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30],
    [-1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28],
    [-1, 13, 22, 18, 26, 18, 24, 18, 22, 20, 24, 28, 26, 24, 20, 30, 24, 28, 28, 26, 30, 28, 30, 30, 30, 30, 28, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30],
    [-1, 17, 28, 22, 16, 22, 28, 26, 26, 24, 28, 24, 28, 22, 24, 24, 30, 28, 28, 26, 28, 30, 24, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30, 30],
];
const NUM_BLOCKS: [[i16; 41]; 4] = [
    [-1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 4, 4, 4, 4, 4, 6, 6, 6, 6, 7, 8, 8, 9, 9, 10, 12, 12, 12, 13, 14, 15, 16, 17, 18, 19, 19, 20, 21, 22, 24, 25],
    [-1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21, 23, 25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49],
    [-1, 1, 1, 2, 2, 4, 4, 6, 6, 8, 8, 8, 10, 12, 16, 12, 17, 16, 18, 21, 20, 23, 23, 25, 27, 29, 34, 34, 35, 38, 40, 43, 45, 48, 51, 53, 56, 59, 62, 65, 68],
    [-1, 1, 1, 2, 4, 4, 4, 5, 6, 8, 8, 11, 11, 16, 16, 18, 16, 19, 21, 25, 25, 25, 34, 30, 32, 35, 37, 40, 42, 45, 48, 51, 54, 57, 60, 63, 66, 70, 74, 77, 81],
];
const FORMAT_BITS: [u32; 4] = [1, 0, 3, 2];

pub struct QrCode {
    #[cfg_attr(not(test), allow(dead_code))]
    pub version: usize,
    pub size: usize,
    pub modules: Vec<Vec<bool>>,
}

fn ecl_index(ec: char) -> usize {
    match ec {
        'L' => 0,
        'Q' => 2,
        'H' => 3,
        _ => 1,
    }
}

fn bit(x: u32, i: u32) -> bool {
    (x >> i) & 1 != 0
}

fn raw_data_modules(ver: usize) -> usize {
    let mut r = (16 * ver + 128) * ver + 64;
    if ver >= 2 {
        let na = ver / 7 + 2;
        r -= (25 * na - 10) * na - 55;
        if ver >= 7 {
            r -= 36;
        }
    }
    r
}

fn data_codewords(ver: usize, ecl: usize) -> usize {
    raw_data_modules(ver) / 8 - (ECC_PER_BLOCK[ecl][ver] * NUM_BLOCKS[ecl][ver]) as usize
}

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11d);
        z ^= ((y as u32 >> i) & 1) * x as u32;
    }
    z as u8
}

fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut r = vec![0u8; degree - 1];
    r.push(1);
    let mut root: u8 = 1;
    for _ in 0..degree {
        for j in 0..r.len() {
            r[j] = gf_mul(r[j], root);
            if j + 1 < r.len() {
                r[j] ^= r[j + 1];
            }
        }
        root = gf_mul(root, 0x02);
    }
    r
}

fn rs_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; divisor.len()];
    for &b in data {
        let factor = b ^ r.remove(0);
        r.push(0);
        for (i, &coef) in divisor.iter().enumerate() {
            r[i] ^= gf_mul(coef, factor);
        }
    }
    r
}

fn add_ecc_and_interleave(data: &[u8], ver: usize, ecl: usize) -> Vec<u8> {
    let num_blocks = NUM_BLOCKS[ecl][ver] as usize;
    let ecc_len = ECC_PER_BLOCK[ecl][ver] as usize;
    let raw = raw_data_modules(ver) / 8;
    let num_short = num_blocks - raw % num_blocks;
    let short_len = raw / num_blocks;
    let div = rs_divisor(ecc_len);
    let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(num_blocks);
    let mut k = 0;
    for i in 0..num_blocks {
        let len = short_len - ecc_len + if i < num_short { 0 } else { 1 };
        let mut dat = data[k..k + len].to_vec();
        k += len;
        let ecc = rs_remainder(&dat, &div);
        if i < num_short {
            dat.push(0);
        }
        dat.extend(ecc);
        blocks.push(dat);
    }
    let mut out = Vec::with_capacity(raw);
    for i in 0..blocks[0].len() {
        for (j, b) in blocks.iter().enumerate() {
            if i != short_len - ecc_len || j >= num_short {
                out.push(b[i]);
            }
        }
    }
    out
}

pub fn encode(input: &[u8], ec: char) -> Result<QrCode, String> {
    let ecl = ecl_index(ec);

    let mut ver = 1;
    while ver <= 40 {
        let cc_bits = if ver < 10 { 8 } else { 16 };
        if 4 + cc_bits + input.len() * 8 <= data_codewords(ver, ecl) * 8 {
            break;
        }
        ver += 1;
    }
    if ver > 40 {
        return Err(format!("QR data too long ({} bytes)", input.len()));
    }

    // --- data bit stream (byte mode)
    let mut bits: Vec<u8> = Vec::new();
    let put = |bits: &mut Vec<u8>, val: u32, len: usize| {
        for i in (0..len).rev() {
            bits.push(((val >> i) & 1) as u8);
        }
    };
    put(&mut bits, 0b0100, 4);
    put(&mut bits, input.len() as u32, if ver < 10 { 8 } else { 16 });
    for &b in input {
        put(&mut bits, b as u32, 8);
    }
    let cap = data_codewords(ver, ecl) * 8;
    let term = 4.min(cap - bits.len());
    put(&mut bits, 0, term);
    let pad_bits = (8 - bits.len() % 8) % 8;
    put(&mut bits, 0, pad_bits);
    let mut pad: u32 = 0xec;
    while bits.len() < cap {
        put(&mut bits, pad, 8);
        pad ^= 0xec ^ 0x11;
    }
    let codewords: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |v, &b| (v << 1) | b)).collect();

    // --- matrix
    let size = ver * 4 + 17;
    let mut m = vec![vec![false; size]; size];
    let mut func = vec![vec![false; size]; size];
    fn set(m: &mut [Vec<bool>], func: &mut [Vec<bool>], x: usize, y: usize, dark: bool) {
        m[y][x] = dark;
        func[y][x] = true;
    }

    for i in 0..size {
        set(&mut m, &mut func, 6, i, i % 2 == 0);
        set(&mut m, &mut func, i, 6, i % 2 == 0);
    }

    for &(cx, cy) in &[(3i32, 3i32), (size as i32 - 4, 3), (3, size as i32 - 4)] {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let d = dx.abs().max(dy.abs());
                let (x, y) = (cx + dx, cy + dy);
                if x >= 0 && (x as usize) < size && y >= 0 && (y as usize) < size {
                    set(&mut m, &mut func, x as usize, y as usize, d != 2 && d != 4);
                }
            }
        }
    }

    if ver > 1 {
        let na = ver / 7 + 2;
        let step = (ver * 8 + na * 3 + 5) / (na * 4 - 4) * 2;
        let mut pos = vec![6usize];
        let mut p = size - 7;
        while pos.len() < na {
            pos.insert(1, p);
            p = p.saturating_sub(step); // the last subtraction is unused (may go below 0)
        }
        for i in 0..na {
            for j in 0..na {
                if (i == 0 && j == 0) || (i == 0 && j == na - 1) || (i == na - 1 && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        let x = (pos[i] as i32 + dx) as usize;
                        let y = (pos[j] as i32 + dy) as usize;
                        set(&mut m, &mut func, x, y, dx.abs().max(dy.abs()) != 1);
                    }
                }
            }
        }
    }

    let draw_format = |m: &mut Vec<Vec<bool>>, func: &mut Vec<Vec<bool>>, mask: u32| {
        let data = (FORMAT_BITS[ecl] << 3) | mask;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let f = ((data << 10) | rem) ^ 0x5412;
        for i in 0..=5 {
            set(m, func, 8, i, bit(f, i as u32));
        }
        set(m, func, 8, 7, bit(f, 6));
        set(m, func, 8, 8, bit(f, 7));
        set(m, func, 7, 8, bit(f, 8));
        for i in 9..15 {
            set(m, func, 14 - i, 8, bit(f, i as u32));
        }
        for i in 0..8 {
            set(m, func, size - 1 - i, 8, bit(f, i as u32));
        }
        for i in 8..15 {
            set(m, func, 8, size - 15 + i, bit(f, i as u32));
        }
        set(m, func, 8, size - 8, true); // dark module
    };
    draw_format(&mut m, &mut func, 0); // reserve the area

    if ver >= 7 {
        let mut rem = ver as u32;
        for _ in 0..12 {
            rem = (rem << 1) ^ ((rem >> 11) * 0x1f25);
        }
        let v = ((ver as u32) << 12) | rem;
        for i in 0..18 {
            let a = size - 11 + i % 3;
            let b = i / 3;
            set(&mut m, &mut func, a, b, bit(v, i as u32));
            set(&mut m, &mut func, b, a, bit(v, i as u32));
        }
    }

    // --- place data (zig-zag, right to left, skipping the vertical timing column)
    let all = add_ecc_and_interleave(&codewords, ver, ecl);
    let mut k = 0usize;
    let mut right = size as i32 - 1;
    while right >= 1 {
        if right == 6 {
            right = 5;
        }
        for vert in 0..size {
            for j in 0..2 {
                let x = (right - j) as usize;
                let upward = ((right + 1) & 2) == 0;
                let y = if upward { size - 1 - vert } else { vert };
                if !func[y][x] && k < all.len() * 8 {
                    m[y][x] = bit(all[k >> 3] as u32, 7 - (k & 7) as u32);
                    k += 1;
                }
            }
        }
        right -= 2;
    }

    // --- mask selection
    let mask_fn = |mask: u32, x: usize, y: usize| -> bool {
        match mask {
            0 => (x + y) % 2 == 0,
            1 => y % 2 == 0,
            2 => x % 3 == 0,
            3 => (x + y) % 3 == 0,
            4 => (x / 3 + y / 2) % 2 == 0,
            5 => (x * y) % 2 + (x * y) % 3 == 0,
            6 => ((x * y) % 2 + (x * y) % 3) % 2 == 0,
            _ => ((x + y) % 2 + (x * y) % 3) % 2 == 0,
        }
    };
    let apply_mask = |m: &mut Vec<Vec<bool>>, func: &Vec<Vec<bool>>, mask: u32| {
        for y in 0..size {
            for x in 0..size {
                if !func[y][x] && mask_fn(mask, x, y) {
                    m[y][x] = !m[y][x];
                }
            }
        }
    };
    let mut best = 0;
    let mut best_score = usize::MAX;
    for mask in 0..8 {
        apply_mask(&mut m, &func, mask);
        draw_format(&mut m, &mut func, mask);
        let s = penalty(&m, size);
        if s < best_score {
            best = mask;
            best_score = s;
        }
        apply_mask(&mut m, &func, mask); // XOR again = undo
    }
    apply_mask(&mut m, &func, best);
    draw_format(&mut m, &mut func, best);
    Ok(QrCode { version: ver, size, modules: m })
}

fn penalty(m: &[Vec<bool>], size: usize) -> usize {
    const P1: [bool; 11] = [true, false, true, true, true, false, true, false, false, false, false];
    const P2: [bool; 11] = [false, false, false, false, true, false, true, true, true, false, true];
    let mut score = 0;
    for pass in 0..2 {
        for a in 0..size {
            let line: Vec<bool> = (0..size).map(|b| if pass == 1 { m[b][a] } else { m[a][b] }).collect();
            let mut run = 1;
            for b in 1..=size {
                if b < size && line[b] == line[b - 1] {
                    run += 1;
                    continue;
                }
                if run >= 5 {
                    score += 3 + (run - 5);
                }
                run = 1;
            }
            for b in 0..=size.saturating_sub(11) {
                if b + 11 <= size && (line[b..b + 11] == P1 || line[b..b + 11] == P2) {
                    score += 40;
                }
            }
        }
    }
    let mut dark = 0;
    for y in 0..size {
        for x in 0..size {
            if m[y][x] {
                dark += 1;
            }
            if x < size - 1 && y < size - 1 && m[y][x] == m[y][x + 1] && m[y][x] == m[y + 1][x] && m[y][x] == m[y + 1][x + 1] {
                score += 3;
            }
        }
    }
    let pct = dark as f64 * 100.0 / (size * size) as f64;
    score += 10 * ((pct - 50.0).abs() / 5.0).floor() as usize;
    score
}

/// Pack the matrix into 1-bit rows (MSB first), same format as raster images.
pub fn pack(q: &QrCode) -> Vec<u8> {
    let bpr = q.size.div_ceil(8);
    let mut out = vec![0u8; bpr * q.size];
    for y in 0..q.size {
        for x in 0..q.size {
            if q.modules[y][x] {
                out[y * bpr + (x >> 3)] |= 0x80 >> (x & 7);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fnv1a(data: &[u8]) -> u64 {
        data.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    }

    /// Known answers from the JS encoder whose output was decoded by macOS CoreImage
    /// (versions 1–39, all EC levels). Any change to the matrix changes the hash.
    #[test]
    fn matches_verified_reference() {
        const CASES: &[(usize, char, usize, u64)] = &[
        (1, 'L', 1, 0x8aa2a3e63dd4e9eb),
        (1, 'M', 1, 0xb7c7a3d0873c0d9f),
        (1, 'Q', 1, 0x05868b8a7c92c54e),
        (1, 'H', 1, 0xe18e1b3e6eeeaa7e),
        (17, 'L', 1, 0x998a2de91eb5cf7d),
        (17, 'M', 2, 0x4bb64f06883a8704),
        (17, 'Q', 2, 0x473b4be02d59d56c),
        (17, 'H', 3, 0x472514d2ac276ce7),
        (40, 'L', 3, 0x820c15c611791a33),
        (40, 'M', 3, 0x0f8f4c5257126a9a),
        (40, 'Q', 4, 0xcb8f293999c1a089),
        (40, 'H', 5, 0xb46eeb9e9e053c8b),
        (100, 'L', 5, 0xb8228fa87feab341),
        (100, 'M', 6, 0xb6f52426b6f03fc8),
        (100, 'Q', 8, 0x8735a57499e9bb1f),
        (100, 'H', 10, 0x9ba5fc2df96826a1),
        (180, 'L', 8, 0xa1ef18f3bb729bfe),
        (180, 'M', 9, 0xfe7ef3c5124cbc42),
        (180, 'Q', 12, 0x519806d50a13ebb1),
        (180, 'H', 14, 0xe89a6bb9f3763e15),
        (300, 'L', 11, 0x14e6d87de97b406a),
        (300, 'M', 13, 0x46803f7321f4ff51),
        (300, 'Q', 16, 0x21d91951cf0260ae),
        (300, 'H', 18, 0x16f2683a7ab0013a),
        (500, 'L', 15, 0xcdc874bcef617f05),
        (500, 'M', 17, 0xb7347ad703366abc),
        (500, 'Q', 21, 0xbf6c21386a6f931b),
        (500, 'H', 24, 0x38f9f30929c62bef),
        (900, 'L', 21, 0x267e073033536c43),
        (900, 'M', 24, 0x0b2410ccf6258f26),
        (900, 'Q', 29, 0x630b3672c9a1b990),
        (900, 'H', 34, 0x0045695410d11ad7),
        (1200, 'L', 25, 0xc2fd783ff41c7898),
        (1200, 'M', 29, 0x82e72fb115d3ba2d),
        (1200, 'Q', 34, 0xc80d9812cc5c947d),
        (1200, 'H', 39, 0x0fc8afa476829b28),
        ];
        for &(len, ec, version, hash) in CASES {
            let data: Vec<u8> = (0..len).map(|i| b"abcdefghijklmnopqrstuvwxyz0123456789"[(i * 7) % 36]).collect();
            let q = encode(&data, ec).unwrap();
            assert_eq!(q.version, version, "len {len} ec {ec}");
            assert_eq!(fnv1a(&pack(&q)), hash, "len {len} ec {ec}");
        }
        assert!(encode(&[b'x'; 1300], 'H').is_err());
    }
}
