// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic visual contact sheet for the procedural 3D crop archetypes
//! and the 0.5-texel ore relief.
//!
//! Renders a fixed grid — the three [`Crop3dArchetype`] families across
//! growth stages `0..=7`, then the ore relief from four yaw viewpoints in
//! both all-faces and single-face variants — into one PNG using only `std`:
//! a small orthographic isometric z-buffered triangle rasterizer plus a
//! minimal RFC 2083 PNG writer (stored zlib blocks, no compression). Every
//! cell's mesh fingerprint and a whole-image pixel hash are printed on
//! stdout so the sheet doubles as a golden-hash pin board.
//!
//! Usage: `crop_contact_sheet [--out <path>]` (default
//! `target/tmp/crop_ore_contact_sheet.png`).

use mc_spine_client_mesh::vanilla_plus_3d::{
    build_3d_crop_mesh, build_3d_ore_relief, mesh_fingerprint, Crop3dArchetype, Crop3dSpec,
    ExtrudedMesh, ExtrudedQuad,
};
use mc_spine_client_mesh::Direction;

// ---------------------------------------------------------------------------
// Sheet layout
// ---------------------------------------------------------------------------

const CELL: usize = 120;
const GAP: usize = 4;
const MARGIN: usize = 8;
const COLS: usize = 8;
const ROWS: usize = 4;
const PITCH: usize = CELL + GAP;
const IMG_W: usize = 2 * MARGIN + COLS * CELL + (COLS - 1) * GAP;
const IMG_H: usize = 2 * MARGIN + ROWS * CELL + (ROWS - 1) * GAP;
/// Empty border kept around the projected mesh inside each cell.
const PAD: f32 = 10.0;

/// Isometric projection: 30-degree ground axes, `y` up, camera on the
/// `(1, 1, 1)` diagonal so `depth = x + y + z` grows toward the viewer.
const ISO_C: f32 = 0.866_025_4; // cos(30°)
const ISO_S: f32 = 0.5; // sin(30°)

/// Matches the golden-fingerprint fixture in `crops_and_ores_3d_tests`, so
/// the printed crop hashes are directly comparable to those pins.
const SEED: u64 = 0x5eed_cafe_1234;
const BLOCK_POS: [i32; 3] = [3, 64, -7];

const BG: [u8; 3] = [22, 24, 30];

// ---------------------------------------------------------------------------
// Image buffer
// ---------------------------------------------------------------------------

struct Image {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Image {
    fn new(w: usize, h: usize, fill: [u8; 3]) -> Self {
        let mut px = vec![0u8; w * h * 3];
        for chunk in px.as_chunks_mut::<3>().0 {
            chunk.copy_from_slice(&fill);
        }
        Self { w, h, px }
    }

    fn set(&mut self, x: usize, y: usize, c: [u8; 3]) {
        if x < self.w && y < self.h {
            let i = (y * self.w + x) * 3;
            self.px[i..i + 3].copy_from_slice(&c);
        }
    }
}

// ---------------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Palette {
    /// Crop meshes: tint -1 foliage, 0 grain/bulb, 1 fruit pods.
    Plant,
    /// Ore relief + context cube: tint -2 cube faces, -1 vein flecks.
    Mineral,
}

struct Cell {
    label: String,
    mesh: ExtrudedMesh,
    /// Quarter turns about +Y applied before projection (0..=3).
    turns: u32,
    palette: Palette,
    /// FNV-1a fingerprint of the canonical (unrotated, cube-free) mesh.
    fingerprint: u64,
    /// Quad count of that same canonical mesh.
    quads: usize,
}

fn crop_cell(label: &str, archetype: Crop3dArchetype, stage: u8) -> Cell {
    let mesh = build_3d_crop_mesh(&Crop3dSpec {
        archetype,
        stage,
        max_stage: 7,
        seed: SEED,
        block_pos: BLOCK_POS,
    });
    Cell {
        label: label.to_string(),
        quads: mesh.quads.len(),
        fingerprint: mesh_fingerprint(&mesh),
        mesh,
        turns: 0,
        palette: Palette::Plant,
    }
}

/// Same vein fixture as `crops_and_ores_3d_tests`: two disconnected blobs.
fn vein_mask() -> [u8; 256] {
    let mut m = [0u8; 256];
    for y in 2..6 {
        for x in 3..8 {
            m[y * 16 + x] = 200;
        }
    }
    for y in 9..13 {
        for x in 10..14 {
            m[y * 16 + x] = 255;
        }
    }
    m
}

/// Plain unit cube used as context behind the ore relief so the 0.5-texel
/// flecks read as proud of a block face. tint_index -2 marks the cube.
fn unit_cube() -> ExtrudedMesh {
    let q = |positions: [[f32; 3]; 4], normal: [f32; 3]| ExtrudedQuad {
        positions,
        normal,
        uv: [[0.0; 2]; 4],
        tint_index: -2,
    };
    let mut m = ExtrudedMesh::new();
    m.quads.push(q(
        [
            [0.0, 1.0, 0.0],
            [0.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, 0.0],
        ],
        [0.0, 1.0, 0.0],
    ));
    m.quads.push(q(
        [
            [0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 1.0],
        ],
        [0.0, -1.0, 0.0],
    ));
    m.quads.push(q(
        [
            [0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
            [1.0, 0.0, 0.0],
        ],
        [0.0, 0.0, -1.0],
    ));
    m.quads.push(q(
        [
            [1.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
            [0.0, 1.0, 1.0],
            [0.0, 0.0, 1.0],
        ],
        [0.0, 0.0, 1.0],
    ));
    m.quads.push(q(
        [
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0],
        ],
        [-1.0, 0.0, 0.0],
    ));
    m.quads.push(q(
        [
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [1.0, 1.0, 1.0],
            [1.0, 0.0, 1.0],
        ],
        [1.0, 0.0, 0.0],
    ));
    m
}

fn ore_cell(label: &str, relief: &ExtrudedMesh, turns: u32) -> Cell {
    let mut mesh = unit_cube();
    mesh.append(relief);
    Cell {
        label: label.to_string(),
        quads: relief.quads.len(),
        fingerprint: mesh_fingerprint(relief),
        mesh,
        turns,
        palette: Palette::Mineral,
    }
}

fn build_cells() -> Vec<Cell> {
    let mut cells = Vec::with_capacity(COLS * ROWS);
    for (prefix, arch) in [
        ("S", Crop3dArchetype::SheafStalk),
        ("B", Crop3dArchetype::BulbLeaf),
        ("V", Crop3dArchetype::VineFruit),
    ] {
        for stage in 0..=7u8 {
            cells.push(crop_cell(&format!("{prefix}{stage}"), arch, stage));
        }
    }
    let mask = vein_mask();
    let relief_all = build_3d_ore_relief(&mask, 0x3F);
    let relief_up = build_3d_ore_relief(&mask, Direction::Up.bit());
    // Row 3, cols 0..4: all-faces relief from four yaw viewpoints.
    for turns in 0..4u32 {
        cells.push(ore_cell(&format!("A{turns}"), &relief_all, turns));
    }
    // Row 3, cols 4..8: single Up-face relief from the same viewpoints.
    for turns in 0..4u32 {
        cells.push(ore_cell(&format!("U{turns}"), &relief_up, turns));
    }
    cells
}

// ---------------------------------------------------------------------------
// Shading + rasterizer
// ---------------------------------------------------------------------------

fn base_color(palette: Palette, tint: i32) -> [f32; 3] {
    match (palette, tint) {
        (Palette::Plant, 0) => [222.0, 186.0, 60.0], // grain heads / bulbs
        (Palette::Plant, 1) => [205.0, 70.0, 52.0],  // fruit pods
        (Palette::Plant, _) => [96.0, 168.0, 76.0],  // stalks / foliage
        (Palette::Mineral, -2) => [62.0, 66.0, 82.0], // context cube
        (Palette::Mineral, _) => [152.0, 160.0, 182.0], // vein flecks
    }
}

/// Lambert-ish flat shade: fixed key light plus ambient floor so back
/// faces stay readable but clearly darker.
fn shade(base: [f32; 3], n: [f32; 3], light: [f32; 3]) -> [u8; 3] {
    let d = (n[0] * light[0] + n[1] * light[1] + n[2] * light[2]).clamp(0.0, 1.0);
    let b = 0.40 + 0.60 * d;
    [
        (base[0] * b).clamp(0.0, 255.0) as u8,
        (base[1] * b).clamp(0.0, 255.0) as u8,
        (base[2] * b).clamp(0.0, 255.0) as u8,
    ]
}

/// Proper 90-degree rotations about +Y through the origin.
fn yaw_matrix(turns: u32) -> [[f32; 3]; 3] {
    match turns % 4 {
        0 => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        1 => [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]],
        2 => [[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, -1.0]],
        _ => [[0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]],
    }
}

struct ScreenVert {
    x: f32,
    y: f32,
    d: f32,
}

struct ScreenTri {
    xs: [f32; 3],
    ys: [f32; 3],
    ds: [f32; 3],
    c: [u8; 3],
}

fn render_cell(img: &mut Image, ox: usize, oy: usize, cell: &Cell, light: [f32; 3]) {
    let mut mesh = cell.mesh.clone();
    mesh.affine(yaw_matrix(cell.turns), [0.0; 3], [0.0; 3]);

    let mut tris = Vec::with_capacity(mesh.quads.len() * 2);
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
    );
    for q in &mesh.quads {
        let c = shade(base_color(cell.palette, q.tint_index), q.normal, light);
        for corner in [[0usize, 1, 2], [0, 2, 3]] {
            let mut v = [
                ScreenVert {
                    x: 0.0,
                    y: 0.0,
                    d: 0.0,
                },
                ScreenVert {
                    x: 0.0,
                    y: 0.0,
                    d: 0.0,
                },
                ScreenVert {
                    x: 0.0,
                    y: 0.0,
                    d: 0.0,
                },
            ];
            for (i, &ci) in corner.iter().enumerate() {
                let p = q.positions[ci];
                let sx = (p[0] - p[2]) * ISO_C;
                let sy = (p[0] + p[2]) * ISO_S - p[1];
                let d = p[0] + p[1] + p[2];
                v[i] = ScreenVert { x: sx, y: sy, d };
                min_x = min_x.min(sx);
                max_x = max_x.max(sx);
                min_y = min_y.min(sy);
                max_y = max_y.max(sy);
            }
            tris.push(ScreenTri {
                xs: [v[0].x, v[1].x, v[2].x],
                ys: [v[0].y, v[1].y, v[2].y],
                ds: [v[0].d, v[1].d, v[2].d],
                c,
            });
        }
    }

    if !tris.is_empty() {
        let cx = (min_x + max_x) * 0.5;
        let cy = (min_y + max_y) * 0.5;
        let rx = (max_x - min_x).max(1e-6);
        let ry = (max_y - min_y).max(1e-6);
        let k = ((CELL as f32 - 2.0 * PAD) / rx).min((CELL as f32 - 2.0 * PAD) / ry);
        let (hx, hy) = (CELL as f32 * 0.5, CELL as f32 * 0.5);
        for t in &mut tris {
            for i in 0..3 {
                t.xs[i] = (t.xs[i] - cx) * k + hx;
                t.ys[i] = (t.ys[i] - cy) * k + hy;
            }
        }
        let mut zb = vec![f32::NEG_INFINITY; CELL * CELL];
        for t in &tris {
            rasterize_tri(img, ox, oy, t, &mut zb);
        }
    }

    draw_text(img, ox + 4, oy + CELL - 15, &cell.label, 2, [228, 230, 238]);
}

fn rasterize_tri(img: &mut Image, ox: usize, oy: usize, t: &ScreenTri, zb: &mut [f32]) {
    let xs = &t.xs;
    let ys = &t.ys;
    let ds = &t.ds;
    let c = t.c;
    let x_lo = xs
        .iter()
        .fold(f32::INFINITY, |a, &b| a.min(b))
        .floor()
        .max(0.0) as usize;
    let x_hi = xs
        .iter()
        .fold(f32::NEG_INFINITY, |a, &b| a.max(b))
        .ceil()
        .min((CELL - 1) as f32) as usize;
    let y_lo = ys
        .iter()
        .fold(f32::INFINITY, |a, &b| a.min(b))
        .floor()
        .max(0.0) as usize;
    let y_hi = ys
        .iter()
        .fold(f32::NEG_INFINITY, |a, &b| a.max(b))
        .ceil()
        .min((CELL - 1) as f32) as usize;
    if x_lo > x_hi || y_lo > y_hi {
        return;
    }

    // Twice the signed area; barycentric weights stay correct for either
    // winding because all three are divided by the same denominator.
    let d = (ys[1] - ys[2]) * (xs[0] - xs[2]) + (xs[2] - xs[1]) * (ys[0] - ys[2]);
    if d.abs() < 1e-9 {
        return;
    }
    for py in y_lo..=y_hi {
        for px in x_lo..=x_hi {
            let fx = px as f32 + 0.5;
            let fy = py as f32 + 0.5;
            let w0 = ((ys[1] - ys[2]) * (fx - xs[2]) + (xs[2] - xs[1]) * (fy - ys[2])) / d;
            let w1 = ((ys[2] - ys[0]) * (fx - xs[2]) + (xs[0] - xs[2]) * (fy - ys[2])) / d;
            let w2 = 1.0 - w0 - w1;
            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                continue;
            }
            let depth = w0 * ds[0] + w1 * ds[1] + w2 * ds[2];
            let i = py * CELL + px;
            if depth > zb[i] {
                zb[i] = depth;
                img.set(ox + px, oy + py, c);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tiny 3x5 bitmap font for cell labels
// ---------------------------------------------------------------------------

/// Five rows of three pixels; bit 0 is the leftmost column.
fn glyph(ch: char) -> [u8; 5] {
    match ch {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'S' => [0b111, 0b100, 0b111, 0b001, 0b111],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'V' => [0b101, 0b101, 0b101, 0b101, 0b010],
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'U' => [0b101, 0b101, 0b101, 0b101, 0b111],
        _ => [0; 5],
    }
}

fn draw_text_raw(img: &mut Image, x: usize, y: usize, text: &str, scale: usize, c: [u8; 3]) {
    let mut cx = x;
    for ch in text.chars() {
        for (r, &row) in glyph(ch).iter().enumerate() {
            for col in 0..3usize {
                if row & (1 << col) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            img.set(cx + col * scale + dx, y + r * scale + dy, c);
                        }
                    }
                }
            }
        }
        cx += 4 * scale;
    }
}

fn draw_text(img: &mut Image, x: usize, y: usize, text: &str, scale: usize, c: [u8; 3]) {
    draw_text_raw(img, x + scale, y + scale, text, scale, [26, 28, 36]);
    draw_text_raw(img, x, y, text, scale, c);
}

// ---------------------------------------------------------------------------
// Minimal PNG writer: signature + IHDR + IDAT (zlib stored blocks) + IEND
// ---------------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// RFC 1950 zlib wrapper around RFC 1951 *stored* (uncompressed) deflate
/// blocks: header `0x78 0x01`, then `BFINAL`/`LEN`/`NLEN`/literal chunks of
/// at most 65535 bytes, then the Adler-32 of the raw payload.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut pos = 0usize;
    loop {
        let end = (pos + 65535).min(data.len());
        let last = end == data.len();
        out.push(u8::from(last));
        let len = (end - pos) as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[pos..end]);
        pos = end;
        if last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32_update(crc32_update(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF;
    out.extend_from_slice(&crc.to_be_bytes());
}

/// RFC 2083 PNG, 8-bit truecolour (colour type 2), filter 0 on every
/// scanline. Deterministic byte output for a given pixel buffer.
fn png_encode(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let stride = w * 3;
    let mut raw = Vec::with_capacity(h * (stride + 1));
    for row in rgb.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let z = zlib_stored(&raw);

    let mut out = Vec::with_capacity(8 + 25 + z.len() + 12);
    out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, RGB, deflate, no filter, no interlace
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &z);
    write_chunk(&mut out, b"IEND", &[]);
    out
}

// ---------------------------------------------------------------------------
// Hashing + entry point
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit — same constants as `vanilla_plus_3d::mesh_fingerprint`.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

fn out_path() -> String {
    let mut out = "target/tmp/crop_ore_contact_sheet.png".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => match args.next() {
                Some(v) => out = v,
                None => {
                    eprintln!("crop_contact_sheet: --out needs a path");
                    std::process::exit(2);
                }
            },
            "--help" | "-h" => {
                println!("usage: crop_contact_sheet [--out <path>]");
                std::process::exit(0);
            }
            other => {
                eprintln!("crop_contact_sheet: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    out
}

fn main() {
    let out = out_path();
    let cells = build_cells();
    assert_eq!(cells.len(), COLS * ROWS, "sheet layout drifted");

    let l = (0.35f32 * 0.35 + 0.85 * 0.85 + 0.40 * 0.40).sqrt();
    let light = [0.35 / l, 0.85 / l, 0.40 / l];

    let mut img = Image::new(IMG_W, IMG_H, BG);
    for (i, cell) in cells.iter().enumerate() {
        let (col, row) = (i % COLS, i / COLS);
        render_cell(
            &mut img,
            MARGIN + col * PITCH,
            MARGIN + row * PITCH,
            cell,
            light,
        );
    }

    let png = png_encode(&img.px, img.w, img.h);
    if let Some(parent) = std::path::Path::new(&out).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).expect("create output directory");
        }
    }
    std::fs::write(&out, &png).expect("write contact sheet");

    for cell in &cells {
        println!(
            "{}: quads={} fingerprint=0x{:016x}",
            cell.label, cell.quads, cell.fingerprint
        );
    }
    println!("pixel_hash=0x{:016x}", fnv1a64(&img.px));
    println!("size={}x{}", img.w, img.h);
    println!("png_bytes={}", png.len());
    println!("out={}", out);
}
