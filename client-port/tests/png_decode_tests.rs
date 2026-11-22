// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration tests for `png_decode`: real zlib-compressed PNG fixtures
//! (generated independently with Python's `zlib`, covering stored, fixed
//! and dynamic DEFLATE blocks plus all five scanline filters), stored-zlib
//! PNGs built in-test for the sub-byte indexed depths, an end-to-end
//! `ResourcePackLayer::from_dir` load, and hostile fail-closed cases.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use mc_spine_client_mesh::asset_pipeline::{CompiledAssetBundle, ResourcePackLayer};
use mc_spine_client_mesh::atlas::AtlasTileRect;
use mc_spine_client_mesh::png_decode::{decode_png_rgba, PngDecodeError, MAX_PNG_DIM};

// ---------------------------------------------------------------------------
// Real PNG fixtures (Python zlib.compress, level 9)
// ---------------------------------------------------------------------------

// FIX_RGB8: 8x8 RGB, stored DEFLATE block.
static FIX_RGB8_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x02, 0x00, 0x00, 0x00, 0x4b, 0x6d, 0x29,
    0xdc, 0x00, 0x00, 0x00, 0xd3, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x01, 0xc8, 0x00, 0x37, 0xff,
    0x00, 0x00, 0x00, 0x00, 0x20, 0x05, 0x01, 0x40, 0x0a, 0x04, 0x60, 0x0f, 0x09, 0x80, 0x14, 0x10,
    0xa0, 0x19, 0x19, 0xc0, 0x1e, 0x24, 0xe0, 0x23, 0x31, 0x00, 0x03, 0x20, 0x07, 0x23, 0x25, 0x08,
    0x43, 0x2a, 0x0b, 0x63, 0x2f, 0x10, 0x83, 0x34, 0x17, 0xa3, 0x39, 0x20, 0xc3, 0x3e, 0x2b, 0xe3,
    0x43, 0x38, 0x00, 0x06, 0x40, 0x1c, 0x26, 0x45, 0x1d, 0x46, 0x4a, 0x20, 0x66, 0x4f, 0x25, 0x86,
    0x54, 0x2c, 0xa6, 0x59, 0x35, 0xc6, 0x5e, 0x40, 0xe6, 0x63, 0x4d, 0x00, 0x09, 0x60, 0x3f, 0x29,
    0x65, 0x40, 0x49, 0x6a, 0x43, 0x69, 0x6f, 0x48, 0x89, 0x74, 0x4f, 0xa9, 0x79, 0x58, 0xc9, 0x7e,
    0x63, 0xe9, 0x83, 0x70, 0x00, 0x0c, 0x80, 0x70, 0x2c, 0x85, 0x71, 0x4c, 0x8a, 0x74, 0x6c, 0x8f,
    0x79, 0x8c, 0x94, 0x80, 0xac, 0x99, 0x89, 0xcc, 0x9e, 0x94, 0xec, 0xa3, 0xa1, 0x00, 0x0f, 0xa0,
    0xaf, 0x2f, 0xa5, 0xb0, 0x4f, 0xaa, 0xb3, 0x6f, 0xaf, 0xb8, 0x8f, 0xb4, 0xbf, 0xaf, 0xb9, 0xc8,
    0xcf, 0xbe, 0xd3, 0xef, 0xc3, 0xe0, 0x00, 0x12, 0xc0, 0xfc, 0x32, 0xc5, 0xfd, 0x52, 0xca, 0x00,
    0x72, 0xcf, 0x05, 0x92, 0xd4, 0x0c, 0xb2, 0xd9, 0x15, 0xd2, 0xde, 0x20, 0xf2, 0xe3, 0x2d, 0x00,
    0x15, 0xe0, 0x57, 0x35, 0xe5, 0x58, 0x55, 0xea, 0x5b, 0x75, 0xef, 0x60, 0x95, 0xf4, 0x67, 0xb5,
    0xf9, 0x70, 0xd5, 0xfe, 0x7b, 0xf5, 0x03, 0x88, 0xad, 0x5f, 0x53, 0x01, 0x04, 0x9a, 0x90, 0xd6,
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
static FIX_RGB8_RGBA: &[u8] = &[
    0, 0, 0, 32, 5, 1, 64, 10, 4, 96, 15, 9, 128, 20, 16, 160, 25, 25, 192, 30, 36, 224, 35, 49,
    3, 32, 7, 35, 37, 8, 67, 42, 11, 99, 47, 16, 131, 52, 23, 163, 57, 32, 195, 62, 43, 227, 67, 56,
    6, 64, 28, 38, 69, 29, 70, 74, 32, 102, 79, 37, 134, 84, 44, 166, 89, 53, 198, 94, 64, 230, 99, 77,
    9, 96, 63, 41, 101, 64, 73, 106, 67, 105, 111, 72, 137, 116, 79, 169, 121, 88, 201, 126, 99, 233, 131, 112,
    12, 128, 112, 44, 133, 113, 76, 138, 116, 108, 143, 121, 140, 148, 128, 172, 153, 137, 204, 158, 148, 236, 163, 161,
    15, 160, 175, 47, 165, 176, 79, 170, 179, 111, 175, 184, 143, 180, 191, 175, 185, 200, 207, 190, 211, 239, 195, 224,
    18, 192, 252, 50, 197, 253, 82, 202, 0, 114, 207, 5, 146, 212, 12, 178, 217, 21, 210, 222, 32, 242, 227, 45,
    21, 224, 87, 53, 229, 88, 85, 234, 91, 117, 239, 96, 149, 244, 103, 181, 249, 112, 213, 254, 123, 245, 3, 136,
];

// FIX_RGBA8_FILTERS: 8x8 RGBA, fixed-Huffman DEFLATE, rows use filters
// 0,1,2,3,4,4,3,2 — every PNG scanline filter through a real compressor.
static FIX_RGBA8_FILTERS_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x06, 0x00, 0x00, 0x00, 0xc4, 0x0f, 0xbe,
    0x8b, 0x00, 0x00, 0x00, 0x58, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x60, 0x60, 0xf8,
    0x2f, 0xcb, 0xce, 0xfb, 0xde, 0x8a, 0x4f, 0xea, 0x7e, 0xb8, 0xa8, 0xfa, 0xf9, 0x12, 0x19, 0x93,
    0xfd, 0x13, 0x95, 0x1d, 0xd7, 0xaf, 0xd3, 0xf2, 0x9b, 0x7f, 0xda, 0x30, 0xba, 0x9f, 0x91, 0x51,
    0x5e, 0xf0, 0x3d, 0x50, 0xc1, 0x07, 0x5c, 0x98, 0x09, 0xa8, 0xe0, 0x03, 0x3e, 0xcc, 0xcc, 0x64,
    0xa7, 0x94, 0xc0, 0x2f, 0xcc, 0xff, 0x01, 0x17, 0x66, 0x01, 0xab, 0x04, 0x1a, 0x85, 0x0b, 0x13,
    0x54, 0xc0, 0xcc, 0x92, 0x6b, 0xe3, 0x81, 0xcf, 0x0a, 0x82, 0x8e, 0x04, 0x00, 0x82, 0xb4, 0x48,
    0xcc, 0x6f, 0x0e, 0x17, 0x62, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60,
    0x82,
];
static FIX_RGBA8_FILTERS_RGBA: &[u8] = &[
    0, 0, 0, 255, 29, 7, 13, 239, 58, 14, 26, 223, 87, 21, 39, 207, 116, 28, 52, 191, 145, 35, 65, 175,
    174, 42, 78, 159, 203, 49, 91, 143, 1, 31, 17, 239, 30, 38, 30, 223, 59, 45, 43, 207, 88, 52, 56, 191,
    117, 59, 69, 175, 146, 66, 82, 159, 175, 73, 95, 143, 204, 80, 108, 127, 2, 62, 34, 223, 31, 69, 47, 207,
    60, 76, 60, 191, 89, 83, 73, 175, 118, 90, 86, 159, 147, 97, 99, 143, 176, 104, 112, 127, 205, 111, 125, 111,
    3, 93, 51, 207, 32, 100, 64, 191, 61, 107, 77, 175, 90, 114, 90, 159, 119, 121, 103, 143, 148, 128, 116, 127,
    177, 135, 129, 111, 206, 142, 142, 95, 4, 124, 68, 191, 33, 131, 81, 175, 62, 138, 94, 159, 91, 145, 107, 143,
    120, 152, 120, 127, 149, 159, 133, 111, 178, 166, 146, 95, 207, 173, 159, 79, 5, 155, 85, 175, 34, 162, 98, 159,
    63, 169, 111, 143, 92, 176, 124, 127, 121, 183, 137, 111, 150, 190, 150, 95, 179, 197, 163, 79, 208, 204, 176, 63,
    6, 186, 102, 159, 35, 193, 115, 143, 64, 200, 128, 127, 93, 207, 141, 111, 122, 214, 154, 95, 151, 221, 167, 79,
    180, 228, 180, 63, 209, 235, 193, 47, 7, 217, 119, 143, 36, 224, 132, 127, 65, 231, 145, 111, 94, 238, 158, 95,
    123, 245, 171, 79, 152, 252, 184, 63, 181, 3, 197, 47, 210, 10, 210, 31,
];

// FIX_INDEXED: 8x8 indexed, PLTE (8 entries) + tRNS (8 alphas),
// dynamic-Huffman DEFLATE — the only fixture exercising that block type.
static FIX_INDEXED_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x03, 0x00, 0x00, 0x00, 0xf3, 0xd1, 0x4e,
    0xb9, 0x00, 0x00, 0x00, 0x18, 0x50, 0x4c, 0x54, 0x45, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00,
    0x00, 0xff, 0xff, 0xff, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0xff, 0x80, 0x80, 0x80, 0xff, 0xff,
    0xff, 0x88, 0x6e, 0x14, 0x86, 0x00, 0x00, 0x00, 0x08, 0x74, 0x52, 0x4e, 0x53, 0xff, 0x80, 0x40,
    0xff, 0x20, 0xff, 0x00, 0xff, 0xef, 0x2e, 0x07, 0x0b, 0x00, 0x00, 0x00, 0x33, 0x49, 0x44, 0x41,
    0x54, 0x78, 0xda, 0x1d, 0x8b, 0x49, 0x0a, 0x00, 0x40, 0x08, 0xc3, 0xea, 0x52, 0xe7, 0xff, 0xef,
    0x55, 0x61, 0x54, 0x08, 0xbd, 0x24, 0x05, 0x2c, 0xc4, 0x9f, 0x52, 0x98, 0x66, 0x8b, 0x36, 0xfb,
    0xb0, 0x28, 0x2f, 0x2c, 0xcd, 0x64, 0x62, 0xb8, 0x72, 0x1e, 0x7a, 0xc9, 0xcc, 0x2a, 0xac, 0xfd,
    0xff, 0xc1, 0x18, 0x47, 0x2b, 0xda, 0x26, 0x5d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];
static FIX_INDEXED_RGBA: &[u8] = &[
    255, 0, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0, 0, 255, 0, 128, 255, 0, 255, 32, 255, 255, 255, 255,
    0, 0, 255, 64, 0, 255, 255, 255, 0, 255, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0,
    0, 255, 0, 128, 255, 0, 255, 32, 255, 255, 255, 255, 0, 0, 255, 64, 0, 0, 255, 64, 0, 255, 255, 255,
    255, 0, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0, 0, 255, 0, 128, 255, 0, 255, 32, 255, 255, 255, 255,
    255, 255, 255, 255, 0, 0, 255, 64, 0, 255, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0,
    0, 255, 0, 128, 255, 0, 255, 32, 255, 0, 255, 32, 255, 255, 255, 255, 0, 0, 255, 64, 0, 255, 255, 255,
    255, 0, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0, 0, 255, 0, 128, 0, 255, 0, 128, 255, 0, 255, 32,
    255, 255, 255, 255, 0, 0, 255, 64, 0, 255, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0,
    128, 128, 128, 0, 0, 255, 0, 128, 255, 0, 255, 32, 255, 255, 255, 255, 0, 0, 255, 64, 0, 255, 255, 255,
    255, 0, 0, 255, 255, 255, 0, 255, 255, 255, 0, 255, 128, 128, 128, 0, 0, 255, 0, 128, 255, 0, 255, 32,
    255, 255, 255, 255, 0, 0, 255, 64, 0, 255, 255, 255, 255, 0, 0, 255,
];

// FIX_GRAYA: 8x8 grayscale+alpha, fixed-Huffman DEFLATE.
static FIX_GRAYA_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x04, 0x00, 0x00, 0x00, 0x6e, 0x06, 0x76,
    0x00, 0x00, 0x00, 0x00, 0x3d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x61, 0xf8, 0x2f, 0xf0,
    0x1d, 0x19, 0x32, 0x73, 0x57, 0xf3, 0xfd, 0x44, 0x86, 0x4c, 0xdc, 0xbf, 0x51, 0x21, 0xa3, 0xe2,
    0x07, 0x54, 0x2d, 0x0c, 0x3a, 0xaf, 0x6d, 0x1e, 0xf9, 0xdc, 0x8c, 0xb9, 0x90, 0x73, 0xbc, 0x66,
    0x5f, 0xcf, 0xd6, 0x39, 0x6b, 0x58, 0x30, 0xb4, 0x38, 0x3d, 0x44, 0xb3, 0x45, 0x27, 0x07, 0xd5,
    0x16, 0x00, 0x69, 0x2c, 0x42, 0x35, 0xd6, 0x05, 0xf6, 0x4d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
static FIX_GRAYA_RGBA: &[u8] = &[
    0, 0, 0, 255, 16, 16, 16, 246, 32, 32, 32, 237, 48, 48, 48, 228, 64, 64, 64, 219, 80, 80, 80, 210,
    96, 96, 96, 201, 112, 112, 112, 192, 11, 11, 11, 250, 27, 27, 27, 241, 43, 43, 43, 232, 59, 59, 59, 223,
    75, 75, 75, 214, 91, 91, 91, 205, 107, 107, 107, 196, 123, 123, 123, 187, 22, 22, 22, 245, 38, 38, 38, 236,
    54, 54, 54, 227, 70, 70, 70, 218, 86, 86, 86, 209, 102, 102, 102, 200, 118, 118, 118, 191, 134, 134, 134, 182,
    33, 33, 33, 240, 49, 49, 49, 231, 65, 65, 65, 222, 81, 81, 81, 213, 97, 97, 97, 204, 113, 113, 113, 195,
    129, 129, 129, 186, 145, 145, 145, 177, 44, 44, 44, 235, 60, 60, 60, 226, 76, 76, 76, 217, 92, 92, 92, 208,
    108, 108, 108, 199, 124, 124, 124, 190, 140, 140, 140, 181, 156, 156, 156, 172, 55, 55, 55, 230, 71, 71, 71, 221,
    87, 87, 87, 212, 103, 103, 103, 203, 119, 119, 119, 194, 135, 135, 135, 185, 151, 151, 151, 176, 167, 167, 167, 167,
    66, 66, 66, 225, 82, 82, 82, 216, 98, 98, 98, 207, 114, 114, 114, 198, 130, 130, 130, 189, 146, 146, 146, 180,
    162, 162, 162, 171, 178, 178, 178, 162, 77, 77, 77, 220, 93, 93, 93, 211, 109, 109, 109, 202, 125, 125, 125, 193,
    141, 141, 141, 184, 157, 157, 157, 175, 173, 173, 173, 166, 189, 189, 189, 157,
];

// FIX_GRAY_TRNS: 8x8 grayscale + tRNS (gray 100 -> alpha 0).
static FIX_GRAY_TRNS_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x00, 0x00, 0x00, 0x00, 0xe1, 0x64, 0xe1,
    0x57, 0x00, 0x00, 0x00, 0x02, 0x74, 0x52, 0x4e, 0x53, 0x00, 0x64, 0x3c, 0x4c, 0x68, 0x79, 0x00,
    0x00, 0x00, 0x2b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x10, 0xd1, 0xb0, 0x09, 0x48,
    0xa9, 0xe8, 0x61, 0x62, 0x87, 0x02, 0x16, 0x18, 0x83, 0x51, 0x54, 0x04, 0x02, 0x98, 0x85, 0xf8,
    0x20, 0x80, 0x41, 0xd9, 0xdc, 0x3b, 0xbe, 0xb8, 0x7d, 0xf6, 0x7a, 0x4c, 0xc5, 0x00, 0x1c, 0xda,
    0x07, 0x7e, 0xc6, 0x31, 0x8c, 0x53, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42,
    0x60, 0x82,
];
static FIX_GRAY_TRNS_RGBA: &[u8] = &[
    0, 0, 0, 255, 20, 20, 20, 255, 40, 40, 40, 255, 60, 60, 60, 255, 80, 80, 80, 255, 100, 100, 100, 0,
    120, 120, 120, 255, 140, 140, 140, 255, 7, 7, 7, 255, 27, 27, 27, 255, 47, 47, 47, 255, 67, 67, 67, 255,
    87, 87, 87, 255, 107, 107, 107, 255, 127, 127, 127, 255, 147, 147, 147, 255, 14, 14, 14, 255, 34, 34, 34, 255,
    54, 54, 54, 255, 74, 74, 74, 255, 94, 94, 94, 255, 114, 114, 114, 255, 134, 134, 134, 255, 154, 154, 154, 255,
    21, 21, 21, 255, 41, 41, 41, 255, 61, 61, 61, 255, 81, 81, 81, 255, 101, 101, 101, 255, 121, 121, 121, 255,
    141, 141, 141, 255, 161, 161, 161, 255, 28, 28, 28, 255, 48, 48, 48, 255, 68, 68, 68, 255, 88, 88, 88, 255,
    108, 108, 108, 255, 128, 128, 128, 255, 148, 148, 148, 255, 168, 168, 168, 255, 35, 35, 35, 255, 55, 55, 55, 255,
    75, 75, 75, 255, 95, 95, 95, 255, 115, 115, 115, 255, 135, 135, 135, 255, 155, 155, 155, 255, 175, 175, 175, 255,
    42, 42, 42, 255, 62, 62, 62, 255, 82, 82, 82, 255, 102, 102, 102, 255, 122, 122, 122, 255, 142, 142, 142, 255,
    162, 162, 162, 255, 182, 182, 182, 255, 49, 49, 49, 255, 69, 69, 69, 255, 89, 89, 89, 255, 109, 109, 109, 255,
    129, 129, 129, 255, 149, 149, 149, 255, 169, 169, 169, 255, 189, 189, 189, 255,
];

// FIX_RGB_TRNS: 8x8 RGB + tRNS (one exact RGB -> alpha 0).
static FIX_RGB_TRNS_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x08, 0x02, 0x00, 0x00, 0x00, 0x4b, 0x6d, 0x29,
    0xdc, 0x00, 0x00, 0x00, 0x06, 0x74, 0x52, 0x4e, 0x53, 0x00, 0x3c, 0x00, 0x5a, 0x00, 0xc8, 0xfe,
    0xe3, 0xb3, 0xf2, 0x00, 0x00, 0x00, 0x4b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60,
    0x60, 0x90, 0x63, 0x10, 0xc1, 0x44, 0xcc, 0x0c, 0x72, 0x22, 0xfc, 0xfc, 0x58, 0x10, 0x0b, 0x50,
    0x82, 0x81, 0x01, 0x2b, 0x8a, 0xb2, 0x91, 0x8b, 0x0a, 0xb0, 0x89, 0x4a, 0x89, 0x8a, 0xaa, 0xa8,
    0x88, 0xea, 0x99, 0x16, 0xb5, 0x60, 0x4b, 0xd4, 0x96, 0x4b, 0x51, 0x27, 0x98, 0x40, 0x3a, 0xb0,
    0x21, 0x9c, 0x46, 0x31, 0x33, 0x64, 0xba, 0x61, 0xb5, 0x9c, 0x91, 0xe1, 0x52, 0x0f, 0x56, 0xe7,
    0x02, 0x00, 0x00, 0x16, 0x14, 0xfe, 0x8a, 0x65, 0x53, 0x40, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
static FIX_RGB_TRNS_RGBA: &[u8] = &[
    0, 0, 0, 255, 30, 0, 20, 255, 60, 0, 40, 255, 90, 0, 60, 255, 120, 0, 80, 255, 150, 0, 100, 255,
    180, 0, 120, 255, 210, 0, 140, 255, 0, 30, 20, 255, 30, 30, 40, 255, 60, 30, 60, 255, 90, 30, 80, 255,
    120, 30, 100, 255, 150, 30, 120, 255, 180, 30, 140, 255, 210, 30, 160, 255, 0, 60, 40, 255, 30, 60, 60, 255,
    60, 60, 80, 255, 90, 60, 100, 255, 120, 60, 120, 255, 150, 60, 140, 255, 180, 60, 160, 255, 210, 60, 180, 255,
    0, 90, 60, 255, 30, 90, 80, 255, 60, 90, 100, 255, 90, 90, 120, 255, 120, 90, 140, 255, 150, 90, 160, 255,
    180, 90, 180, 255, 210, 90, 200, 255, 0, 120, 80, 255, 30, 120, 100, 255, 60, 120, 120, 255, 90, 120, 140, 255,
    120, 120, 160, 255, 150, 120, 180, 255, 180, 120, 200, 255, 210, 120, 220, 255, 0, 150, 100, 255, 30, 150, 120, 255,
    60, 150, 140, 255, 90, 150, 160, 255, 120, 150, 180, 255, 150, 150, 200, 255, 180, 150, 220, 255, 210, 150, 240, 255,
    0, 180, 120, 255, 30, 180, 140, 255, 60, 180, 160, 255, 90, 180, 180, 255, 120, 180, 200, 255, 150, 180, 220, 255,
    180, 180, 240, 255, 210, 180, 4, 255, 0, 210, 140, 255, 30, 210, 160, 255, 60, 210, 180, 255, 90, 210, 200, 255,
    120, 210, 220, 255, 150, 210, 240, 255, 180, 210, 4, 255, 210, 210, 24, 255,
];

// FIX_RGBA8_MULTIBLOCK: 16x16 RGBA whose zlib stream was split with
// Z_FULL_FLUSH — a multi-block DEFLATE stream (BFINAL=0 then BFINAL=1).
static FIX_RGBA8_MULTIBLOCK_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x10, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0xf3, 0xff,
    0x61, 0x00, 0x00, 0x01, 0x81, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x62, 0x60, 0x00, 0x02, 0x5e,
    0x59, 0x46, 0x6e, 0x29, 0x2b, 0x26, 0x31, 0xf5, 0x70, 0x66, 0x45, 0x93, 0x12, 0x16, 0x1d, 0xc7,
    0x89, 0xac, 0xe6, 0x7e, 0xeb, 0xd8, 0x9c, 0xa2, 0x4f, 0xb3, 0xfb, 0x66, 0xbc, 0xe0, 0x88, 0x28,
    0x65, 0xe5, 0x4c, 0x6e, 0x52, 0xe2, 0xca, 0xeb, 0xb7, 0xe7, 0xae, 0x9c, 0x13, 0xc3, 0xd3, 0xb2,
    0xb2, 0x92, 0xb7, 0x7f, 0xdb, 0x34, 0xbe, 0x59, 0x87, 0x37, 0xf3, 0x2f, 0x65, 0x61, 0x67, 0x16,
    0x14, 0x67, 0x67, 0x66, 0xe4, 0x26, 0x17, 0x33, 0x41, 0x0c, 0x20, 0x1f, 0x33, 0x8a, 0x72, 0x1a,
    0xbb, 0x82, 0xbc, 0x40, 0x2e, 0x66, 0x16, 0xe2, 0xd0, 0xb6, 0xe2, 0x12, 0xe0, 0x14, 0xc4, 0x8b,
    0x27, 0xe0, 0x96, 0x63, 0x50, 0xe6, 0x0f, 0x2d, 0x36, 0xd0, 0x09, 0xab, 0xb3, 0xf5, 0x0c, 0xef,
    0xf4, 0x4a, 0x8b, 0x98, 0x12, 0xde, 0x1c, 0x39, 0x3f, 0x65, 0x41, 0xd4, 0xaa, 0xc2, 0xbd, 0xd1,
    0x5b, 0xeb, 0x6e, 0xc5, 0x1c, 0xe8, 0xfe, 0x1e, 0x7b, 0x7a, 0x86, 0x48, 0xdc, 0xb5, 0xa5, 0x86,
    0xf1, 0x0f, 0x37, 0xf9, 0x25, 0xbc, 0xd9, 0x9f, 0x9d, 0xf8, 0xfd, 0x4c, 0x47, 0x12, 0xd3, 0xcd,
    0xa5, 0xc9, 0xbc, 0xcf, 0x0e, 0xa5, 0x48, 0x90, 0x1b, 0x88, 0xe2, 0x54, 0x0b, 0x44, 0x00, 0x00,
    0x00, 0x00, 0xff, 0xff, 0x63, 0xb0, 0x90, 0xe8, 0xd8, 0xe1, 0x6a, 0xda, 0x79, 0x38, 0x28, 0xa8,
    0xeb, 0x5c, 0x7c, 0x7e, 0xf7, 0xcd, 0x9c, 0x9e, 0x9e, 0x27, 0x95, 0x2b, 0x7b, 0xdf, 0xb7, 0x1d,
    0xeb, 0xfb, 0x35, 0xf9, 0x71, 0x3f, 0xeb, 0x02, 0x86, 0x09, 0x02, 0x6b, 0x65, 0x27, 0x4a, 0xef,
    0xb2, 0x9a, 0xa4, 0x76, 0x3c, 0x7c, 0xb2, 0xe1, 0x95, 0x92, 0x29, 0x36, 0x0f, 0x27, 0x4e, 0x75,
    0x7f, 0xb7, 0x6e, 0x5a, 0xd0, 0xef, 0xd3, 0xd3, 0x63, 0x59, 0xd8, 0x99, 0x05, 0xc5, 0xd9, 0x99,
    0x19, 0xb9, 0x71, 0x60, 0x7c, 0x72, 0x60, 0xcc, 0x04, 0x31, 0x80, 0x7c, 0xcc, 0xe8, 0xab, 0xb8,
    0xfb, 0x2f, 0xaf, 0x2c, 0x23, 0x37, 0xb9, 0x98, 0x59, 0x4f, 0x24, 0x7f, 0x1a, 0x97, 0x00, 0xa7,
    0x20, 0x4e, 0x3c, 0x01, 0xbb, 0x78, 0x17, 0x94, 0x66, 0x88, 0x56, 0xbf, 0xab, 0x9d, 0xe1, 0x72,
    0xcf, 0xac, 0x34, 0xf1, 0xbe, 0x63, 0x53, 0xdd, 0x03, 0x9f, 0xfe, 0xd9, 0x0f, 0xc3, 0xe7, 0xec,
    0x78, 0x94, 0xb4, 0xf2, 0xea, 0xe3, 0xdc, 0x6d, 0x9f, 0x9e, 0x54, 0x1c, 0xe6, 0x7f, 0xda, 0x7c,
    0x41, 0xe7, 0x59, 0xdf, 0x5d, 0xcf, 0xe7, 0x33, 0x5f, 0xa5, 0xbd, 0x58, 0xf2, 0xbd, 0xf9, 0xe5,
    0x7a, 0x96, 0x05, 0xaf, 0x76, 0x09, 0xee, 0x7d, 0x7d, 0x54, 0xee, 0xd6, 0x9b, 0x0b, 0x84, 0x02,
    0x91, 0x9b, 0xe6, 0x81, 0x08, 0x00, 0x04, 0xc3, 0x9e, 0x97, 0x19, 0xcf, 0x94, 0xf8, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// PNG construction helpers (test-side encoder: stored zlib blocks)
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
        crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u64, 0u64);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u64::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    ((b << 16) | a) as u32
}

/// RFC 1950 zlib wrapper around RFC 1951 *stored* (uncompressed) deflate
/// blocks — same minimal encoder shape as `crop_contact_sheet`'s writer.
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

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32_update(crc32_update(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF;
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Everything [`png_bytes`] needs: IHDR fields plus optional PLTE/tRNS and
/// the raw zlib payload for the single IDAT.
struct PngSpec<'a> {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    interlace: u8,
    plte: Option<&'a [u8]>,
    trns: Option<&'a [u8]>,
    idat: &'a [u8],
}

impl<'a> PngSpec<'a> {
    /// Defaults: non-interlaced, no PLTE/tRNS.
    fn new(width: u32, height: u32, bit_depth: u8, color_type: u8, idat: &'a [u8]) -> Self {
        Self {
            width,
            height,
            bit_depth,
            color_type,
            interlace: 0,
            plte: None,
            trns: None,
            idat,
        }
    }
}

/// Builds a complete PNG: signature + IHDR + optional PLTE/tRNS + one IDAT
/// + IEND, every CRC correct. `spec.idat` is the raw zlib payload.
fn png_bytes(spec: &PngSpec<'_>) -> Vec<u8> {
    let mut out = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&spec.width.to_be_bytes());
    ihdr.extend_from_slice(&spec.height.to_be_bytes());
    ihdr.extend_from_slice(&[spec.bit_depth, spec.color_type, 0, 0, spec.interlace]);
    push_chunk(&mut out, b"IHDR", &ihdr);
    if let Some(palette) = spec.plte {
        push_chunk(&mut out, b"PLTE", palette);
    }
    if let Some(alpha) = spec.trns {
        push_chunk(&mut out, b"tRNS", alpha);
    }
    push_chunk(&mut out, b"IDAT", spec.idat);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

/// Convenience: PNG whose IDAT is `data` wrapped in stored zlib blocks.
fn png_stored(
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    plte: Option<&[u8]>,
    trns: Option<&[u8]>,
    filtered_scanlines: &[u8],
) -> Vec<u8> {
    png_bytes(&PngSpec {
        plte,
        trns,
        ..PngSpec::new(width, height, bit_depth, color_type, &zlib_stored(filtered_scanlines))
    })
}

// ---------------------------------------------------------------------------
// Scanline filter + sub-byte packing helpers (test-side encoder)
// ---------------------------------------------------------------------------

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (i32::from(a), i32::from(b), i32::from(c));
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Forward PNG filter: `cur` is one packed scanline, `prev` the previous
/// *unfiltered* scanline (None on the first row), `bpp` the filter byte
/// offset. Returns the filtered bytes for one row.
fn filter_row(filter: u8, cur: &[u8], prev: Option<&[u8]>, bpp: usize) -> Vec<u8> {
    let mut out = vec![0u8; cur.len()];
    for (i, &x) in cur.iter().enumerate() {
        let left = if i >= bpp { cur[i - bpp] } else { 0 };
        let up = prev.map_or(0, |p| p[i]);
        let up_left = if i >= bpp {
            prev.map_or(0, |p| p[i - bpp])
        } else {
            0
        };
        let pred = match filter {
            0 => 0,
            1 => left,
            2 => up,
            3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
            _ => paeth(left, up, up_left),
        };
        out[i] = x.wrapping_sub(pred);
    }
    out
}

/// Packs `filter_type || filtered_row` for every row into the IDAT payload
/// layout. `rows` are the *unfiltered* packed scanlines.
fn filtered_stream(rows: &[&[u8]], filters: &[u8], bpp: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut prev: Option<&[u8]> = None;
    for (y, row) in rows.iter().enumerate() {
        let filter = filters[y % filters.len()];
        out.push(filter);
        out.extend_from_slice(&filter_row(filter, row, prev, bpp));
        prev = Some(row);
    }
    out
}

/// Packs `depth`-bit palette indices MSB-first into scanline bytes.
fn pack_indices(indices: &[u8], depth: u8) -> Vec<u8> {
    let depth = usize::from(depth);
    let mut out = vec![0u8; (indices.len() * depth).div_ceil(8)];
    for (i, &v) in indices.iter().enumerate() {
        let bitpos = i * depth;
        let shift = 8 - depth - (bitpos % 8);
        out[bitpos / 8] |= v << shift;
    }
    out
}

// ---------------------------------------------------------------------------
// Decode roundtrips
// ---------------------------------------------------------------------------

#[test]
fn decodes_real_zlib_png_fixtures() {
    // FIX_RGB8 is stored-block DEFLATE over packed RGB; the decoder must
    // expand it to RGBA with alpha 255.
    let (w, h, rgba) = decode_png_rgba(FIX_RGB8_PNG).expect("rgb8 fixture decodes");
    assert_eq!((w, h), (8, 8));
    let mut expected = Vec::with_capacity(8 * 8 * 4);
    for px in FIX_RGB8_RGBA.as_chunks::<3>().0 {
        expected.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    assert_eq!(rgba, expected, "rgb8 pixel mismatch");

    // (png, expected rgba, width, height) — each fixture exercises a
    // different DEFLATE block type / colour type combination.
    let cases: &[(&[u8], &[u8], u32, u32)] = &[
        (FIX_RGBA8_FILTERS_PNG, FIX_RGBA8_FILTERS_RGBA, 8, 8), // fixed Huffman, all 5 filters
        (FIX_INDEXED_PNG, FIX_INDEXED_RGBA, 8, 8), // dynamic Huffman, PLTE+tRNS
        (FIX_GRAYA_PNG, FIX_GRAYA_RGBA, 8, 8),     // fixed, grayscale+alpha
        (FIX_GRAY_TRNS_PNG, FIX_GRAY_TRNS_RGBA, 8, 8), // fixed, gray + tRNS
        (FIX_RGB_TRNS_PNG, FIX_RGB_TRNS_RGBA, 8, 8), // fixed, RGB + tRNS
    ];
    for &(png, expected, width, height) in cases {
        let (w, h, rgba) = decode_png_rgba(png).expect("fixture decodes");
        assert_eq!((w, h), (width, height));
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert_eq!(rgba, expected, "pixel mismatch");
    }
}

#[test]
fn decodes_multiblock_deflate_stream() {
    let (w, h, rgba) = decode_png_rgba(FIX_RGBA8_MULTIBLOCK_PNG).expect("multiblock decodes");
    assert_eq!((w, h), (16, 16));
    // Expected pixels follow the generator formula px(x,y).
    let mut expected = Vec::with_capacity(16 * 16 * 4);
    for y in 0..16u32 {
        for x in 0..16u32 {
            expected.extend_from_slice(&[
                ((x * 13 + y * 7) % 256) as u8,
                ((x * 29 + y * 3) % 256) as u8,
                ((y * 17 + x) % 256) as u8,
                ((x * 11 + y * 23) % 256) as u8,
            ]);
        }
    }
    assert_eq!(rgba, expected);
}

#[test]
fn decodes_all_filters_through_stored_zlib() {
    // Same five filter types, but through *stored* DEFLATE blocks so the
    // unfilter path is exercised independently of Huffman decoding.
    let width = 8u32;
    let height = 8u32;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[
                (x as u8).wrapping_mul(31).wrapping_add(y as u8),
                (y as u8).wrapping_mul(47).wrapping_add(x as u8),
                (x as u8).wrapping_mul(13).wrapping_add(y as u8).wrapping_mul(7),
                255u8.wrapping_sub((x + y) as u8),
            ]);
        }
    }
    let rows: Vec<&[u8]> = pixels.chunks_exact((width * 4) as usize).collect();
    let filters = [0u8, 1, 2, 3, 4, 4, 3, 2];
    let stream = filtered_stream(&rows, &filters, 4);
    let png = png_stored(width, height, 8, 6, None, None, &stream);
    let (w, h, rgba) = decode_png_rgba(&png).expect("stored-zlib PNG decodes");
    assert_eq!((w, h), (width, height));
    assert_eq!(rgba, pixels);
}

#[test]
fn decodes_sub_byte_indexed_depths() {
    // Bit depths 1, 2 and 4 pack several palette indices per byte, MSB
    // first, with the row tail padded — the decoder must unpack exactly
    // `width` indices per row.
    let plte: &[u8] = &[
        10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120, // 4 entries
    ];
    let trns: &[u8] = &[255, 128, 0, 64];

    // depth 4, 13x5 -> rowbytes = ceil(13*4/8) = 7
    let (w, h, depth) = (13u32, 5u32, 4u8);
    let indices: Vec<u8> = (0..w * h)
        .map(|i| ((i * 5 + i / 7) % 4) as u8)
        .collect();
    let mut rows = Vec::new();
    for y in 0..h as usize {
        rows.push(pack_indices(&indices[y * w as usize..(y + 1) * w as usize], depth));
    }
    let row_refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    let stream = filtered_stream(&row_refs, &[0, 1, 2, 3, 4], 1);
    let png = png_stored(w, h, depth, 3, Some(plte), Some(trns), &stream);
    let (dw, dh, rgba) = decode_png_rgba(&png).expect("depth-4 indexed decodes");
    assert_eq!((dw, dh), (w, h));
    for (i, &index) in indices.iter().enumerate() {
        let px = &rgba[i * 4..i * 4 + 4];
        let e = usize::from(index) * 3;
        assert_eq!(
            px,
            &[
                plte[e],
                plte[e + 1],
                plte[e + 2],
                trns[usize::from(index)],
            ],
            "depth-4 pixel {i}"
        );
    }

    // depth 2, 10x3 -> rowbytes = ceil(10*2/8) = 3
    let (w, h, depth) = (10u32, 3u32, 2u8);
    let indices: Vec<u8> = (0..w * h).map(|i| (i % 4) as u8).collect();
    let mut rows = Vec::new();
    for y in 0..h as usize {
        rows.push(pack_indices(&indices[y * w as usize..(y + 1) * w as usize], depth));
    }
    let row_refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    let stream = filtered_stream(&row_refs, &[4, 3, 2], 1);
    let png = png_stored(w, h, depth, 3, Some(plte), Some(trns), &stream);
    let (dw, dh, rgba) = decode_png_rgba(&png).expect("depth-2 indexed decodes");
    assert_eq!((dw, dh), (w, h));
    for (i, &index) in indices.iter().enumerate() {
        let e = usize::from(index) * 3;
        assert_eq!(
            &rgba[i * 4..i * 4 + 4],
            &[plte[e], plte[e + 1], plte[e + 2], trns[usize::from(index)]],
            "depth-2 pixel {i}"
        );
    }

    // depth 1, 9x2 -> rowbytes = ceil(9*1/8) = 2
    let plte1: &[u8] = &[200, 100, 50, 10, 20, 30];
    let (w, h, depth) = (9u32, 2u32, 1u8);
    let indices: Vec<u8> = (0..w * h).map(|i| ((i * 7) % 2) as u8).collect();
    let mut rows = Vec::new();
    for y in 0..h as usize {
        rows.push(pack_indices(&indices[y * w as usize..(y + 1) * w as usize], depth));
    }
    let row_refs: Vec<&[u8]> = rows.iter().map(Vec::as_slice).collect();
    let stream = filtered_stream(&row_refs, &[0, 4], 1);
    let png = png_stored(w, h, depth, 3, Some(plte1), None, &stream);
    let (dw, dh, rgba) = decode_png_rgba(&png).expect("depth-1 indexed decodes");
    assert_eq!((dw, dh), (w, h));
    for (i, &index) in indices.iter().enumerate() {
        let e = usize::from(index) * 3;
        assert_eq!(
            &rgba[i * 4..i * 4 + 4],
            &[plte1[e], plte1[e + 1], plte1[e + 2], 255],
            "depth-1 pixel {i}"
        );
    }
}

#[test]
fn decodes_at_dimension_bound_and_rejects_beyond() {
    // MAX_PNG_DIM itself is legal: a 4096x1 grayscale strip.
    let w = MAX_PNG_DIM;
    let rowbytes = w as usize;
    let mut stream = vec![0x5Au8; rowbytes + 1];
    stream[0] = 0; // filter type None on the single row
    let png = png_stored(w, 1, 8, 0, None, None, &stream);
    let (dw, dh, rgba) = decode_png_rgba(&png).expect("max-dim strip decodes");
    assert_eq!((dw, dh), (w, 1));
    assert_eq!(rgba.len(), rowbytes * 4);
    assert!(rgba.as_chunks::<4>().0.iter().all(|px| *px == [0x5A, 0x5A, 0x5A, 255]));

    // One texel over the bound fails closed in IHDR.
    let png = png_stored(w + 1, 1, 8, 0, None, None, &stream);
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::BadHeader { .. })
    ));

    // Zero dimension is likewise rejected.
    let png = png_stored(0, 4, 8, 0, None, None, &[0, 0, 0, 0]);
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::BadHeader { .. })
    ));
}

// ---------------------------------------------------------------------------
// Hostile / fail-closed cases
// ---------------------------------------------------------------------------

#[test]
fn rejects_bad_signature() {
    for bad in [
        &[][..],
        &[0x89, 0x50][..],
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0B][..],
        b"this is not a png at all, just text".as_slice(),
    ] {
        assert!(
            matches!(decode_png_rgba(bad), Err(PngDecodeError::BadSignature)),
            "input {bad:?} must fail BadSignature"
        );
    }
}

#[test]
fn rejects_crc_mismatches() {
    // Corrupt one IDAT payload byte: the chunk CRC no longer matches.
    let mut bad = FIX_RGB8_PNG.to_vec();
    let idat_pos = bad
        .windows(4)
        .position(|w| w == b"IDAT")
        .expect("fixture has IDAT");
    bad[idat_pos + 8] ^= 0xFF; // a byte inside the IDAT payload
    assert!(matches!(
        decode_png_rgba(&bad),
        Err(PngDecodeError::BadCrc { .. })
    ));

    // Corrupt the IHDR CRC field itself.
    let mut bad = FIX_RGB8_PNG.to_vec();
    // IHDR chunk: sig(8) + len(4) + type(4) + data(13) + crc(4); crc at 8+12+13.
    let crc_pos = 8 + 4 + 4 + 13;
    bad[crc_pos] ^= 0x01;
    assert!(matches!(
        decode_png_rgba(&bad),
        Err(PngDecodeError::BadCrc { .. })
    ));
}

#[test]
fn rejects_truncated_input() {
    // Cut the file inside the IDAT chunk payload.
    let cut = &FIX_RGB8_PNG[..FIX_RGB8_PNG.len() - 30];
    assert!(matches!(
        decode_png_rgba(cut),
        Err(PngDecodeError::Truncated)
    ));

    // A chunk header whose declared length runs far past EOF.
    let mut bad = FIX_RGB8_PNG[..33].to_vec(); // through IHDR
    bad.extend_from_slice(&[0x7F, 0xFF, 0xFF, 0xFF]); // huge length
    bad.extend_from_slice(b"IDAT");
    bad.extend_from_slice(&[0; 8]); // nowhere near the declared payload
    assert!(matches!(
        decode_png_rgba(&bad),
        Err(PngDecodeError::Truncated)
    ));
}

#[test]
fn rejects_truncated_and_bad_adler_streams() {
    // Valid chunk CRCs but the zlib payload is cut mid-stream: rebuild the
    // PNG around a truncated IDAT payload.
    let raw = [0xABu8; 64];
    let mut idat = zlib_stored(&raw);
    idat.truncate(idat.len() - 10); // drops part of the Adler-32 trailer
    let png = png_bytes(&PngSpec::new(8, 8, 8, 6, &idat));
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Inflate { .. })
    ));

    // Intact deflate stream but corrupted Adler-32: inflate succeeds, the
    // trailer check must catch it.
    let mut idat = zlib_stored(&raw);
    let last = idat.len() - 1;
    idat[last] ^= 0xFF;
    let png = png_bytes(&PngSpec::new(8, 8, 8, 6, &idat));
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Inflate { .. })
    ));
}

#[test]
fn rejects_decompression_bomb() {
    // 1x1 RGBA8 needs exactly 5 filtered bytes; a zlib stream that expands
    // to 1 MiB of zeros must be cut off at the expected size.
    let bomb = zlib_stored(&vec![0u8; 1024 * 1024]);
    let png = png_bytes(&PngSpec::new(1, 1, 8, 6, &bomb));
    let err = decode_png_rgba(&png).expect_err("bomb must fail");
    assert!(
        matches!(err, PngDecodeError::Inflate { .. }),
        "expected Inflate, got {err:?}"
    );
}

#[test]
fn rejects_declared_size_over_decoded_budget() {
    // 4096x4096 RGBA8 declares a filtered stream of 4096*16385 = 67,112,960
    // bytes — 4 KiB over MAX_DECODED_BYTES. The bound trips on the declared
    // geometry before any inflate work happens.
    let png = png_bytes(&PngSpec::new(4096, 4096, 8, 6, &[]));
    let err = decode_png_rgba(&png).expect_err("oversized declaration must fail");
    assert!(
        matches!(err, PngDecodeError::TooLarge { .. }),
        "expected TooLarge, got {err:?}"
    );
}

#[test]
fn rejects_unsupported_features() {
    let raw = zlib_stored(&[0u8; 8 * (8 * 4 + 1)]);

    // Adam7 interlace.
    let mut spec = PngSpec::new(8, 8, 8, 6, &raw);
    spec.interlace = 1;
    let png = png_bytes(&spec);
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Unsupported { .. })
    ));

    // 16-bit samples are out of the supported subset.
    let png = png_bytes(&PngSpec::new(8, 8, 16, 6, &raw));
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Unsupported { .. })
    ));

    // Indexed without PLTE.
    let png = png_bytes(&PngSpec::new(8, 8, 8, 3, &raw));
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::BadHeader { .. })
    ));

    // tRNS on an already-alpha colour type is forbidden.
    let png = png_bytes(&PngSpec {
        trns: Some(&[0, 0]),
        ..PngSpec::new(8, 8, 8, 6, &raw)
    });
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Unsupported { .. })
    ));

    // PLTE on grayscale is forbidden.
    let png = png_bytes(&PngSpec {
        plte: Some(&[1, 2, 3]),
        ..PngSpec::new(8, 8, 8, 0, &raw)
    });
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Unsupported { .. })
    ));

    // An unknown *critical* chunk (first byte has bit 5 clear) between
    // IHDR and IDAT must be rejected; build the PNG by hand.
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&8u32.to_be_bytes());
    ihdr.extend_from_slice(&8u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"ABCD", b"payload"); // 'A' = critical, unknown
    push_chunk(&mut png, b"IDAT", &raw);
    push_chunk(&mut png, b"IEND", &[]);
    assert!(matches!(
        decode_png_rgba(&png),
        Err(PngDecodeError::Unsupported { .. })
    ));

    // An unknown *ancillary* chunk (first byte has bit 5 set) is skipped
    // after CRC verification — the image still decodes.
    let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    push_chunk(&mut png, b"IHDR", &ihdr);
    push_chunk(&mut png, b"vpAg", b"generator");
    push_chunk(&mut png, b"IDAT", &raw);
    push_chunk(&mut png, b"IEND", &[]);
    // raw was sized for 8x8 RGBA8 filtered rows: 8*(32+1) = 264 bytes.
    assert!(decode_png_rgba(&png).is_ok());
}

// ---------------------------------------------------------------------------
// End-to-end: ResourcePackLayer::from_dir with real .png textures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "png-decode-test-{}-{}-{tag}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir creates");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 16x32 RGBA8 PNG (two 16x16 animation frames), filter 0, stored zlib.
fn animated_strip_png() -> Vec<u8> {
    let (w, h) = (16u32, 32u32);
    let mut stream = Vec::with_capacity((h * (w * 4 + 1)) as usize);
    for y in 0..h {
        stream.push(0);
        for x in 0..w {
            // Frame 0: warm red; frame 1: cool green. Slight x gradient so
            // the frames are not a single flat colour.
            let (r, g, b) = if y < 16 {
                (200 + (x as u8 % 30), 40, 40)
            } else {
                (40, 190 + (x as u8 % 30), 60)
            };
            stream.extend_from_slice(&[r, g, b, 255]);
        }
    }
    png_stored(w, h, 8, 6, None, None, &stream)
}

#[test]
fn from_dir_loads_png_sprites_and_animates_them() {
    let tmp = TempDir::new("pack");
    let root = tmp.path().join("png-pack");
    for dir in [
        "assets/minecraft/blockstates",
        "assets/minecraft/models/block",
        "assets/minecraft/textures/block",
    ] {
        fs::create_dir_all(root.join(dir)).expect("pack dirs create");
    }
    fs::write(
        root.join("assets/minecraft/blockstates/ore.json"),
        r##"{"variants":{"":{"model":"block/ore"}}}"##,
    )
    .unwrap();
    fs::write(
        root.join("assets/minecraft/models/block/ore.json"),
        r##"{"textures":{"t":"minecraft:block/ore_tex"},
"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{
"up":{"texture":"#t","uv":[2,2,14,14]}}}]}"##,
    )
    .unwrap();
    // A real .png sprite: 16x32 strip = two 16x16 animation frames.
    fs::write(
        root.join("assets/minecraft/textures/block/ore_tex.png"),
        animated_strip_png(),
    )
    .unwrap();
    fs::write(
        root.join("assets/minecraft/textures/block/ore_tex.png.mcmeta"),
        r##"{"animation":{"frametime":2,"interpolate":true}}"##,
    )
    .unwrap();
    // A second .png with no animation, plus a .rgba sibling proving the two
    // formats coexist in one layer.
    let mut row = vec![0u8; 1 + 8 * 4];
    for px in row[1..].as_chunks_mut::<4>().0 {
        *px = [10, 200, 90, 255];
    }
    let mut flat_rows = Vec::with_capacity(8 * row.len());
    for _ in 0..8 {
        flat_rows.extend_from_slice(&row);
    }
    let flat = png_stored(8, 8, 8, 6, None, None, &flat_rows);
    fs::write(
        root.join("assets/minecraft/textures/block/plain.png"),
        flat,
    )
    .unwrap();
    // .rgba sprite for coexistence coverage.
    let mut rgba_file = Vec::with_capacity(8 + 4);
    rgba_file.extend_from_slice(&1u32.to_le_bytes());
    rgba_file.extend_from_slice(&1u32.to_le_bytes());
    rgba_file.extend_from_slice(&[1, 2, 3, 255]);
    fs::write(
        root.join("assets/minecraft/textures/block/tiny.rgba"),
        rgba_file,
    )
    .unwrap();

    let layer = ResourcePackLayer::from_dir(&root).expect("pack dir loads");
    assert!(layer.blockstates.contains_key("minecraft:ore"));
    assert!(layer.models.contains_key("minecraft:block/ore"));
    let (w, h, rgba) = layer
        .sprites
        .get("minecraft:block/ore_tex")
        .expect("png sprite decoded into sprites");
    assert_eq!((*w, *h), (16, 32));
    assert_eq!(rgba.len(), 16 * 32 * 4);
    // Frame 0 top-left texel is warm red, frame 1 bottom-left is green.
    assert_eq!(&rgba[0..4], &[200, 40, 40, 255]);
    let bottom = (31 * 16) * 4;
    assert_eq!(&rgba[bottom..bottom + 4], &[40, 190, 60, 255]);
    assert!(layer.sprites.contains_key("minecraft:block/plain"));
    assert!(layer.sprites.contains_key("minecraft:block/tiny"));
    let animation = layer
        .animations
        .get("minecraft:block/ore_tex")
        .expect("png.mcmeta produced an animation");
    assert_eq!(animation.frame_count(), 2);
    assert_eq!(animation.frames()[0].ticks, 2);
    assert!(animation.interpolate());

    // The disk-loaded layer feeds the whole pipeline end to end.
    let bundle = CompiledAssetBundle::build(&[layer]).expect("bundle builds");
    assert!(
        bundle.skipped_errors.is_empty(),
        "unexpected skipped entries: {:?}",
        bundle.skipped_errors
    );
    let quads = bundle
        .bake_block_with_uvs("ore", &[], [0, 0, 0])
        .expect("disk-loaded block bakes");
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0].texture, "minecraft:block/ore_tex");
    let tile = bundle
        .atlas
        .tile_rect("minecraft:block/ore_tex")
        .expect("png sprite packed into atlas");
    assert_uvs_inside(&quads[0].uv, &tile, "png quad");
    assert!(bundle.animation_for("minecraft:block/ore_tex").is_some());
}

#[test]
fn from_dir_keeps_corrupt_png_as_marker_and_build_skips_it() {
    let tmp = TempDir::new("badpng");
    let root = tmp.path().join("pack");
    fs::create_dir_all(root.join("assets/minecraft/textures/block")).unwrap();
    // Valid PNG structure but one IDAT payload byte flipped -> BadCrc.
    let mut bad = FIX_RGB8_PNG.to_vec();
    let idat_pos = bad
        .windows(4)
        .position(|w| w == b"IDAT")
        .expect("fixture has IDAT");
    bad[idat_pos + 12] ^= 0xFF; // a byte inside the IDAT payload
    fs::write(root.join("assets/minecraft/textures/block/bad.png"), bad).unwrap();
    // A valid sibling keeps the atlas non-empty: `build` only aborts on
    // atlas-level failures like an empty atlas.
    write_tiny_rgba(&root);

    // The load itself stays alive: the corrupt texture is kept as a
    // (0, 0, bytes) marker so `build` can report the real decode reason.
    let layer = ResourcePackLayer::from_dir(&root).expect("pack dir loads");
    let (w, h, _) = layer
        .sprites
        .get("minecraft:block/bad")
        .expect("corrupt png kept as marker");
    assert_eq!((*w, *h), (0, 0));

    let bundle = CompiledAssetBundle::build(&[layer]).expect("bundle builds");
    assert_eq!(bundle.skipped_errors.len(), 1);
    let (id, reason) = &bundle.skipped_errors[0];
    assert_eq!(id, "minecraft:block/bad");
    assert!(reason.contains("CRC-32"), "unexpected reason: {reason}");
    assert!(bundle.atlas.tile_rect("minecraft:block/bad").is_none());
}

#[test]
fn from_dir_skips_animation_for_corrupt_png_marker() {
    let tmp = TempDir::new("badpngmeta");
    let root = tmp.path().join("pack");
    fs::create_dir_all(root.join("assets/minecraft/textures/block")).unwrap();
    fs::write(
        root.join("assets/minecraft/textures/block/bad.png"),
        b"not a png at all",
    )
    .unwrap();
    fs::write(
        root.join("assets/minecraft/textures/block/bad.png.mcmeta"),
        r##"{"animation":{"frametime":2}}"##,
    )
    .unwrap();
    write_tiny_rgba(&root);

    // The mcmeta must not resurrect geometry for a marker sprite: the
    // whole entry degrades to skipped_errors, not a load failure.
    let layer = ResourcePackLayer::from_dir(&root).expect("pack dir loads");
    assert!(!layer.animations.contains_key("minecraft:block/bad"));
    let bundle = CompiledAssetBundle::build(&[layer]).expect("bundle builds");
    assert_eq!(bundle.skipped_errors.len(), 1);
    assert_eq!(bundle.skipped_errors[0].0, "minecraft:block/bad");
}

/// Writes a minimal valid 1x1 `.rgba` sprite into `root`'s texture dir.
fn write_tiny_rgba(root: &std::path::Path) {
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 255]);
    fs::write(
        root.join("assets/minecraft/textures/block/tiny.rgba"),
        bytes,
    )
    .unwrap();
}

/// Every vertex UV must land strictly inside the sprite's tile rect.
fn assert_uvs_inside(quad_uv: &[[f32; 2]; 4], tile: &AtlasTileRect, what: &str) {
    for (corner, uv) in quad_uv.iter().enumerate() {
        assert!(
            uv[0] > tile.u0 + 1e-6 && uv[0] < tile.u0 + tile.u_scale - 1e-6,
            "{what} corner {corner}: u {} outside ({}, {})",
            uv[0],
            tile.u0,
            tile.u0 + tile.u_scale
        );
        assert!(
            uv[1] > tile.v0 + 1e-6 && uv[1] < tile.v0 + tile.v_scale - 1e-6,
            "{what} corner {corner}: v {} outside ({}, {})",
            uv[1],
            tile.v0,
            tile.v0 + tile.v_scale
        );
    }
}
