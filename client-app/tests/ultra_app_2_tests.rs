// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lane ULTRA-APP-2 coverage: `[MEM-1 §3.3]` 32 MiB decoded-PCM LRU cap on
//! `SoundLibrary`, `[DISK-1 §5.2 ④]` dirty-only debounced settings
//! persistence, and `[MEM-1 §3.2]` client chunk distance eviction.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use megu_client::protocol::{
    DecodedBlockEntity, DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta,
};
use megu_client::settings::{
    ClientSettings, SettingsFlush, SettingsSaveGuard, MAX_FOV, MIN_FOV,
};
use megu_client::sound_assets::{SoundLibrary, DEFAULT_SOUND_PCM_CACHE_CAP_BYTES};
use megu_client::world::{BlockTable, WorldState};

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mc-ultra-app2-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_empty_asset_index(dir: &Path) {
    std::fs::create_dir_all(dir.join("indexes")).unwrap();
    std::fs::write(dir.join("indexes/test.json"), r#"{"objects":{}}"#).unwrap();
}

/// PCM vectors of `bytes` (f32 = 4 B/sample) — content is irrelevant to
/// cache accounting, only the size matters.
fn pcm_of_bytes(bytes: usize) -> Arc<Vec<f32>> {
    assert_eq!(bytes % 4, 0);
    Arc::new(vec![0.0f32; bytes / 4])
}

#[test]
fn sound_library_lru_strictly_caps_decoded_pcm_bytes_and_evicts_oldest() {
    let dir = scratch_dir("snd");
    write_empty_asset_index(&dir);
    let cap = 160 * 1024;
    let mut lib = SoundLibrary::load(&dir, "test")
        .unwrap()
        .with_pcm_cache_cap_bytes(cap);
    assert_eq!(lib.pcm_cache_cap_bytes(), cap);
    assert_eq!(DEFAULT_SOUND_PCM_CACHE_CAP_BYTES, 32 * 1024 * 1024);

    // Five distinct 32 KiB sounds exactly fill the 160 KiB budget.
    let clip_bytes = 32 * 1024;
    let paths: Vec<PathBuf> = (0..5)
        .map(|i| dir.join(format!("sound{i}.ogg")))
        .collect();
    for (i, path) in paths.iter().enumerate() {
        assert!(
            lib.insert_decoded_pcm(path, pcm_of_bytes(clip_bytes)),
            "clip {i} fits the budget"
        );
    }
    assert_eq!(lib.pcm_cache_bytes(), 5 * clip_bytes);
    assert_eq!(lib.pcm_cache_evictions(), 0);
    assert!(paths.iter().all(|p| lib.is_cached(p)));

    // Touching sound #0 refreshes its recency without touching disk: the
    // cache serves the hit even though no real OGG exists at that path.
    let hit = lib.decode(&paths[0]).unwrap();
    assert_eq!(hit.len() * 4, clip_bytes);
    assert_eq!(lib.pcm_cache_hits(), 1);

    // Inserting a sixth clip overflows by exactly one entry: the oldest
    // *untouched* clip (#1) is evicted first, never the just-refreshed #0.
    let extra = dir.join("sound5.ogg");
    assert!(lib.insert_decoded_pcm(&extra, pcm_of_bytes(clip_bytes)));
    assert!(lib.pcm_cache_bytes() <= cap);
    assert_eq!(lib.pcm_cache_evictions(), 1);
    assert!(!lib.is_cached(&paths[1]), "oldest untouched clip evicted");
    assert!(lib.is_cached(&paths[0]), "refreshed clip survives");

    // Strict LRU order: one more insert evicts the next-oldest (#2).
    let extra2 = dir.join("sound6.ogg");
    assert!(lib.insert_decoded_pcm(&extra2, pcm_of_bytes(clip_bytes)));
    assert!(lib.pcm_cache_bytes() <= cap);
    assert_eq!(lib.pcm_cache_evictions(), 2);
    assert!(!lib.is_cached(&paths[2]));
    assert!(lib.is_cached(&paths[0]));

    // A single clip larger than the entire cap plays but is never cached —
    // resident decoded PCM can never exceed the budget.
    let giant = dir.join("giant.ogg");
    let before = lib.pcm_cache_bytes();
    assert!(!lib.insert_decoded_pcm(&giant, pcm_of_bytes(2 * cap)));
    assert_eq!(lib.pcm_cache_bytes(), before);
    assert!(!lib.is_cached(&giant));
    assert!(lib.pcm_cache_bytes() <= cap);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn settings_flush_skips_unchanged_and_writes_once_after_slider_burst() {
    let dir = scratch_dir("settings");
    let path = dir.join("megu-client.toml");
    let mut settings = ClientSettings::default();
    let mut guard = SettingsSaveGuard::new();

    // A burst of 100 slider drags lives entirely in RAM.
    for i in 0..100u8 {
        settings.fov = MIN_FOV + (i % (MAX_FOV - MIN_FOV));
        guard.mark_dirty();
        assert_eq!(guard.disk_writes(), 0, "drag {i} wrote to disk");
        assert!(!path.exists(), "drag {i} created the settings file");
    }
    assert!(guard.is_dirty());

    // Five repeated flushes over identical content: exactly one atomic
    // write total; every later flush is a zero-byte no-op.
    let outcomes: Vec<SettingsFlush> = (0..5)
        .map(|_| guard.flush_if_dirty(&settings, &path).unwrap())
        .collect();
    assert_eq!(outcomes[0], SettingsFlush::Written);
    assert!(outcomes[1..].iter().all(|o| *o == SettingsFlush::Unchanged));
    assert_eq!(guard.disk_writes(), 1);
    assert!(!guard.is_dirty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), settings.to_toml());
    assert!(
        !dir.join("megu-client.toml.tmp").exists(),
        "atomic temp file was renamed, not left behind"
    );

    // An A->B->A roundtrip marks dirty but serializes to identical bytes,
    // so the digest gate skips the write entirely.
    guard.mark_dirty();
    assert_eq!(
        guard.flush_if_dirty(&settings, &path).unwrap(),
        SettingsFlush::Unchanged
    );
    assert_eq!(guard.disk_writes(), 1);
    assert_eq!(guard.skipped_writes(), 1);

    // A real change writes exactly once more — and a clean guard opening
    // the file it just wrote never rewrites identical content.
    settings.view_distance = 20;
    guard.mark_dirty();
    assert_eq!(
        guard.flush_if_dirty(&settings, &path).unwrap(),
        SettingsFlush::Written
    );
    assert_eq!(guard.disk_writes(), 2);

    let mut reopen = SettingsSaveGuard::new();
    reopen.mark_dirty();
    assert_eq!(
        reopen.flush_if_dirty(&settings, &path).unwrap(),
        SettingsFlush::Unchanged,
        "on-disk digest seed makes the reopen flush a no-op"
    );
    assert_eq!(reopen.disk_writes(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

fn one_section_chunk(cx: i32, cz: i32) -> DecodedChunk {
    DecodedChunk {
        cx,
        cz,
        sections: vec![DecodedSection {
            non_air: 0,
            states: Box::new([0u32; 4096]),
            biomes: [0; 64],
        }],
        light: DecodedLightData {
            sky_mask: Vec::new(),
            block_mask: Vec::new(),
            empty_sky_mask: Vec::new(),
            empty_block_mask: Vec::new(),
            sky_arrays: Vec::new(),
            block_arrays: Vec::new(),
        },
        block_entities: Vec::new(),
    }
}

#[test]
fn client_world_evicts_chunks_beyond_view_distance_plus_two() {
    let dimension = DimensionMeta {
        min_y: 0,
        height: 16,
        has_skylight: false,
        ..DimensionMeta::default()
    };
    let mut world = WorldState::new(dimension, BlockTable::builtin());

    // Columns along +X at Chebyshev distances 0..=20; chunk 12 also carries
    // a block entity to prove per-chunk entity caches drop with the column.
    for d in 0..=20 {
        let mut chunk = one_section_chunk(d, 0);
        if d == 12 {
            chunk.block_entities.push(DecodedBlockEntity {
                packed_xz: 0x00,
                y: 5,
                kind: 1,
                nbt_bytes: Vec::new(),
            });
        }
        world.apply_chunk(chunk).unwrap();
    }
    assert_eq!(world.loaded_chunks(), 21);
    assert!(world.block_entities.contains_key(&[12 * 16, 5, 0]));

    // view_distance 6 keeps the +2 hysteresis margin: columns at distance
    // <= 8 stay, everything beyond is evicted from RAM with zero disk I/O.
    let evicted = world.evict_far_chunks(0, 0, 6);
    assert_eq!(evicted, 12);
    assert_eq!(world.loaded_chunks(), 9);
    for d in 0..=8 {
        assert!(world.columns.contains_key(&(d, 0)), "chunk {d} evicted");
        assert!(world.get_block_state(d * 16, 0, 0).is_some());
    }
    for d in 9..=20 {
        assert!(!world.columns.contains_key(&(d, 0)), "chunk {d} retained");
        assert!(world.get_block_state(d * 16, 0, 0).is_none());
    }
    assert!(
        !world.block_entities.contains_key(&[12 * 16, 5, 0]),
        "evicted chunk's block entity stayed behind"
    );
    assert!(world.chunks_unloaded >= 12);

    // Idempotent: a second pass over the same center evicts nothing, while
    // a moved center (20,0) leaves every survivor outside its own margin.
    assert_eq!(world.evict_far_chunks(0, 0, 6), 0);
    assert_eq!(world.evict_far_chunks(20, 0, 6), 9);
    assert_eq!(world.loaded_chunks(), 0);
}
