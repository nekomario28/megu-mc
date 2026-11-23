// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Unit and smoke tests for `megu_core::sys_metrics`.

use megu_core::sys_metrics::{
    cpu_usage_percent, format_mini_hud_lines, gpu_backend_for_vendor, parse_drm_gpu_metrics,
    parse_gpu_busy_percent, parse_macos_top_cpu_percent, parse_macos_vm_stat, parse_proc_meminfo,
    parse_proc_self_status_rss_kib, parse_proc_stat_cpu_totals, parse_vram_bytes_to_mib,
    parse_windows_cpu_times, sample_gpu_metrics, sample_host_metrics, CpuSample, CpuSampler,
    FrameTimingStats, GpuBackend, MiniHudAnchor, MiniHudConfig, MiniHudData, FRAME_TIMING_CAPACITY,
};

const MEMINFO: &str = "\
MemTotal:       31937784 kB
MemFree:         7974076 kB
MemAvailable:   10104052 kB
Buffers:           60588 kB
Cached:          5100028 kB
SwapCached:            0 kB
";

const SELF_STATUS: &str = "\
Name:\tmegu
Umask:\t0022
State:\tR (running)
VmPeak:\t  512000 kB
VmRSS:\t    2416 kB
VmData:\t  102400 kB
";

const PROC_STAT: &str = "\
cpu  1000 200 300 8000 400 0 100 0 0 0
cpu0 500 100 150 4000 200 0 50 0 0 0
intr 12345
";

// ---------------------------------------------------------------------------
// /proc parsers
// ---------------------------------------------------------------------------

#[test]
fn status_rss_parses_the_vm_rss_line() {
    assert_eq!(parse_proc_self_status_rss_kib(SELF_STATUS), Some(2416));
}

#[test]
fn windows_cpu_times_use_kernel_including_idle_without_double_counting() {
    let before = parse_windows_cpu_times(100, 150, 50);
    let after = parse_windows_cpu_times(140, 230, 70);
    assert_eq!(
        before,
        CpuSample {
            total_jiffies: 200,
            idle_jiffies: 100
        }
    );
    assert_eq!(cpu_usage_percent(&before, &after), Some(60.0));
}

#[test]
fn macos_vm_stat_parser_converts_pages_and_handles_bad_or_missing_memory() {
    let sample = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\n\
Pages free: 100.\nPages inactive: 200.\nPages speculative: 10.\nPages purgeable: 5.\n";
    let mem = parse_macos_vm_stat(sample, 1_073_741_824).expect("valid vm_stat");
    assert_eq!(mem.total_mib, 1024);
    assert_eq!(mem.available_mib, 1);
    assert_eq!(mem.used_mib, 1023);
    assert_eq!(parse_macos_vm_stat("garbage", 1024), None);
    assert_eq!(parse_macos_vm_stat(sample, 0), None);
}

#[test]
fn macos_top_parser_returns_busy_percent_only_for_valid_idle_reading() {
    assert_eq!(
        parse_macos_top_cpu_percent("CPU usage: 10.00% user, 5.00% sys, 85.00% idle"),
        Some(15.0)
    );
    assert_eq!(
        parse_macos_top_cpu_percent("CPU usage: 99% user, 1% sys"),
        None
    );
    assert_eq!(
        parse_macos_top_cpu_percent("CPU usage: 100% idle"),
        Some(0.0)
    );
}

#[test]
fn status_rss_missing_or_malformed_is_none() {
    assert_eq!(parse_proc_self_status_rss_kib(""), None);
    assert_eq!(parse_proc_self_status_rss_kib("Name:\tx\n"), None);
    assert_eq!(parse_proc_self_status_rss_kib("VmRSS:\tnotanum kB\n"), None);
    assert_eq!(parse_proc_self_status_rss_kib("VmRSS:\n"), None);
    // A VmRSS-shaped prefix on a different field must not match.
    assert_eq!(parse_proc_self_status_rss_kib("VmRSSExtra:\t99 kB\n"), None);
}

#[test]
fn meminfo_extracts_total_available_used() {
    let m = parse_proc_meminfo(MEMINFO);
    assert_eq!(m.total_mib, 31937784 / 1024);
    assert_eq!(m.available_mib, 10104052 / 1024);
    assert_eq!(m.used_mib, m.total_mib - m.available_mib);
}

#[test]
fn meminfo_falls_back_to_free_plus_buffers_cached() {
    let legacy = "MemTotal: 1048576 kB\nMemFree: 102400 kB\nBuffers: 51200 kB\nCached: 204800 kB\n";
    let m = parse_proc_meminfo(legacy);
    assert_eq!(m.total_mib, 1024);
    assert_eq!(m.available_mib, (102400 + 51200 + 204800) / 1024);
    assert_eq!(m.used_mib, m.total_mib - m.available_mib);
}

#[test]
fn meminfo_malformed_and_empty_inputs_are_zeroed() {
    let empty = parse_proc_meminfo("");
    assert_eq!(empty.total_mib, 0);
    assert_eq!(empty.available_mib, 0);
    assert_eq!(empty.used_mib, 0);

    let garbage = parse_proc_meminfo("MemTotal: banana\nMemAvailable:\nCached: 12x kB\n");
    assert_eq!(garbage.total_mib, 0);
    assert_eq!(garbage.available_mib, 0);
    assert_eq!(garbage.used_mib, 0);
}

#[test]
fn meminfo_used_saturates_when_available_exceeds_total() {
    let weird = parse_proc_meminfo("MemTotal: 1024 kB\nMemAvailable: 2048 kB\n");
    assert_eq!(weird.used_mib, 0);
}

#[test]
fn stat_cpu_totals_reads_aggregate_line_only() {
    let s = parse_proc_stat_cpu_totals(PROC_STAT).expect("aggregate cpu line");
    // busy = user+nice+system+irq+softirq+steal = 1000+200+300+0+100+0
    // idle = idle+iowait = 8000+400; guest columns excluded entirely.
    assert_eq!(s.idle_jiffies, 8400);
    assert_eq!(s.total_jiffies, 1600 + 8400);
}

#[test]
fn stat_cpu_totals_handles_short_and_missing_lines() {
    assert_eq!(parse_proc_stat_cpu_totals(""), None);
    assert_eq!(parse_proc_stat_cpu_totals("cpu0 1 2 3 4\n"), None);
    // Fewer than four columns is unusable.
    assert_eq!(parse_proc_stat_cpu_totals("cpu 1 2 3\n"), None);
    // Non-numeric fields parse as 0 rather than failing.
    let s = parse_proc_stat_cpu_totals("cpu 10 20 30 40 junk\n").unwrap();
    assert_eq!(s.idle_jiffies, 40);
    assert_eq!(s.total_jiffies, 100);
}

#[test]
fn cpu_usage_percent_computes_busy_delta() {
    let a = parse_proc_stat_cpu_totals(PROC_STAT).unwrap();
    // Advance: +100 busy jiffies, +100 idle jiffies → 50%.
    let b = CpuSample {
        total_jiffies: a.total_jiffies + 200,
        idle_jiffies: a.idle_jiffies + 100,
    };
    let pct = cpu_usage_percent(&a, &b).unwrap();
    assert!((pct - 50.0).abs() < 0.01, "got {pct}");
}

#[test]
fn cpu_usage_percent_is_safe_on_zero_delta_and_rollback() {
    let a = CpuSample {
        total_jiffies: 1000,
        idle_jiffies: 400,
    };
    // No elapsed time: must not divide by zero.
    assert_eq!(cpu_usage_percent(&a, &a), None);
    // Counter rollback (kernel reset / wrap): None, not a garbage spike.
    let rolled = CpuSample {
        total_jiffies: 10,
        idle_jiffies: 5,
    };
    assert_eq!(cpu_usage_percent(&a, &rolled), None);
    // Idle ahead of total (inconsistent pair): clamps to 0%, not negative.
    let all_idle = CpuSample {
        total_jiffies: a.total_jiffies + 50,
        idle_jiffies: a.idle_jiffies + 200,
    };
    assert_eq!(cpu_usage_percent(&a, &all_idle), Some(0.0));
    // All-busy delta clamps at 100%.
    let all_busy = CpuSample {
        total_jiffies: a.total_jiffies + 50,
        idle_jiffies: a.idle_jiffies,
    };
    assert_eq!(cpu_usage_percent(&a, &all_busy), Some(100.0));
}

#[test]
fn cpu_sampler_reports_none_on_first_sample_then_deltas() {
    let mut s = CpuSampler::new();
    let a = CpuSample {
        total_jiffies: 500,
        idle_jiffies: 250,
    };
    assert_eq!(s.observe(a), None);
    assert_eq!(s.current(), Some(a));
    let b = CpuSample {
        total_jiffies: 600,
        idle_jiffies: 290,
    };
    let pct = s.observe(b).unwrap();
    assert!(
        (pct - 60.0).abs() < 0.01,
        "40/100 idle → 60% busy, got {pct}"
    );
}

// ---------------------------------------------------------------------------
// DRM GPU parsers
// ---------------------------------------------------------------------------

#[test]
fn gpu_backend_maps_pci_vendors() {
    assert_eq!(gpu_backend_for_vendor("0x1002"), GpuBackend::Amd);
    assert_eq!(gpu_backend_for_vendor("0x8086"), GpuBackend::Intel);
    assert_eq!(gpu_backend_for_vendor("0x10de"), GpuBackend::Nvidia);
    assert_eq!(gpu_backend_for_vendor("0x1af4"), GpuBackend::DrmOther);
    assert_eq!(gpu_backend_for_vendor(""), GpuBackend::DrmOther);
}

#[test]
fn gpu_busy_percent_parses_and_clamps() {
    assert_eq!(parse_gpu_busy_percent("42\n"), Some(42));
    assert_eq!(parse_gpu_busy_percent("0"), Some(0));
    assert_eq!(parse_gpu_busy_percent("137"), Some(100));
    assert_eq!(parse_gpu_busy_percent("-5"), Some(0));
    assert_eq!(parse_gpu_busy_percent(""), None);
    assert_eq!(parse_gpu_busy_percent("busy"), None);
}

#[test]
fn vram_bytes_convert_to_mib() {
    assert_eq!(parse_vram_bytes_to_mib("17163091968"), Some(16368));
    assert_eq!(parse_vram_bytes_to_mib("0"), Some(0));
    assert_eq!(parse_vram_bytes_to_mib("junk"), None);
    assert_eq!(parse_vram_bytes_to_mib(""), None);
}

#[test]
fn amd_drm_metrics_assemble_all_fields() {
    let g = parse_drm_gpu_metrics(
        "0x1002",
        "0x747e",
        "amdgpu",
        Some("37"),
        Some("1048576"),
        Some("17163091968"),
    );
    assert_eq!(g.backend, GpuBackend::Amd);
    assert_eq!(g.name, "AMD GPU (0x747e)");
    assert_eq!(g.busy_percent, Some(37));
    assert_eq!(g.vram_used_mib, Some(1));
    assert_eq!(g.vram_total_mib, Some(16368));
}

#[test]
fn drm_metrics_degrade_field_by_field() {
    // Intel iGPU: busy% exposed, no VRAM counters, empty device id.
    let g = parse_drm_gpu_metrics("0x8086", "", "i915", Some("12"), None, None);
    assert_eq!(g.backend, GpuBackend::Intel);
    assert_eq!(g.name, "Intel GPU");
    assert_eq!(g.busy_percent, Some(12));
    assert_eq!(g.vram_used_mib, None);
    assert_eq!(g.vram_total_mib, None);

    // Malformed busy value only kills that field.
    let g2 = parse_drm_gpu_metrics(
        "0x1002",
        "0x13c0",
        "amdgpu",
        Some("NaN"),
        Some("512"),
        Some("1048576"),
    );
    assert_eq!(g2.busy_percent, None);
    assert_eq!(g2.vram_total_mib, Some(1));

    // Unknown vendor falls back to the driver name for the label.
    let g3 = parse_drm_gpu_metrics("0x1af4", "", "virtio_gpu", None, None, None);
    assert_eq!(g3.backend, GpuBackend::DrmOther);
    assert_eq!(g3.name, "virtio_gpu GPU");
}

// ---------------------------------------------------------------------------
// FrameTimingStats
// ---------------------------------------------------------------------------

#[test]
fn steady_60fps_reports_accurate_stats() {
    let mut st = FrameTimingStats::new();
    for _ in 0..120 {
        st.push_frame_ms(16.6667);
    }
    assert_eq!(st.frame_count(), 120);
    assert!((st.frame_ms_avg() - 16.6667).abs() < 0.01);
    assert!((st.fps_avg() - 60.0).abs() < 0.1, "fps={}", st.fps_avg());
    assert!(
        (st.fps_one_percent_low() - 60.0).abs() < 0.5,
        "1% low {}",
        st.fps_one_percent_low()
    );
    assert!((st.frame_ms_p95() - 16.6667).abs() < 0.01);
}

#[test]
fn steady_144fps_reports_accurate_stats() {
    let mut st = FrameTimingStats::new();
    for _ in 0..144 {
        st.push_frame_ms(1_000.0 / 144.0);
    }
    assert!((st.fps_avg() - 144.0).abs() < 0.5, "fps={}", st.fps_avg());
}

#[test]
fn one_percent_low_reflects_stutter_not_average() {
    let mut st = FrameTimingStats::with_capacity(200);
    for _ in 0..99 {
        st.push_frame_ms(1_000.0 / 60.0);
    }
    // One 100ms hitch in a 100-sample window: the worst-1% slice is
    // exactly this frame alone.
    st.push_frame_ms(100.0);
    let low = st.fps_one_percent_low();
    assert!(
        (low - 10.0).abs() < 0.1,
        "single stutter should pin 1% low near 10 FPS, got {low}"
    );
    // Average FPS stays near 60 — the low metric is what exposes the hitch.
    assert!(st.fps_avg() > 55.0, "avg {}", st.fps_avg());
}

#[test]
fn p95_ignores_the_slow_tail() {
    let mut st = FrameTimingStats::with_capacity(100);
    for i in 0..100 {
        st.push_frame_ms(if i < 95 { 10.0 } else { 50.0 });
    }
    assert!((st.frame_ms_p95() - 10.0).abs() < 0.001);
}

#[test]
fn mspt_and_tps_track_tick_durations() {
    let mut st = FrameTimingStats::new();
    for _ in 0..50 {
        st.push_tick_ms(40.0);
    }
    assert!((st.mspt_avg() - 40.0).abs() < 0.001);
    // 1000/40 = 25 TPS → capped at the vanilla 20.
    assert!((st.tps_avg() - 20.0).abs() < 0.001);

    let mut slow = FrameTimingStats::new();
    for _ in 0..10 {
        slow.push_tick_ms(100.0);
    }
    assert!((slow.tps_avg() - 10.0).abs() < 0.01);
}

#[test]
fn empty_and_pathological_inputs_are_safe() {
    let st = FrameTimingStats::new();
    assert_eq!(st.fps_avg(), 0.0);
    assert_eq!(st.fps_one_percent_low(), 0.0);
    assert_eq!(st.frame_ms_avg(), 0.0);
    assert_eq!(st.frame_ms_p95(), 0.0);
    assert_eq!(st.mspt_avg(), 0.0);
    assert_eq!(st.tps_avg(), 0.0);

    let mut bad = FrameTimingStats::new();
    bad.push_frame_ms(f64::NAN);
    bad.push_frame_ms(f64::INFINITY);
    bad.push_frame_ms(f64::NEG_INFINITY);
    bad.push_frame_ms(0.0);
    bad.push_frame_ms(-30.0);
    for v in [
        bad.frame_ms_avg(),
        bad.frame_ms_p95(),
        bad.fps_avg(),
        bad.fps_one_percent_low(),
    ] {
        assert!(v.is_finite(), "stat must stay finite, got {v}");
    }
    // NaN/Inf collapse to the 60s stall cap → fps floor is ~0.0167, not a
    // divide-by-zero or a NaN leak.
    assert!(bad.frame_ms_avg() > 0.0);
}

#[test]
fn ring_evicts_oldest_past_capacity() {
    let mut st = FrameTimingStats::with_capacity(10);
    for _ in 0..10 {
        st.push_frame_ms(10.0);
    }
    for _ in 0..10 {
        st.push_frame_ms(20.0);
    }
    assert_eq!(st.frame_count(), 10);
    assert!((st.frame_ms_avg() - 20.0).abs() < 0.001);
    assert_eq!(FRAME_TIMING_CAPACITY, 120);
}

#[test]
fn duration_inputs_and_clear_work() {
    let mut st = FrameTimingStats::new();
    st.push_frame(std::time::Duration::from_millis(16));
    st.push_tick(std::time::Duration::from_millis(50));
    assert_eq!(st.frame_count(), 1);
    assert!((st.frame_ms_avg() - 16.0).abs() < 0.001);
    assert!((st.mspt_avg() - 50.0).abs() < 0.001);
    st.clear();
    assert_eq!(st.frame_count(), 0);
    assert_eq!(st.tick_count(), 0);
    assert_eq!(st.fps_avg(), 0.0);
}

// ---------------------------------------------------------------------------
// MiniHudConfig + formatter
// ---------------------------------------------------------------------------

#[test]
fn default_config_is_disabled_and_advanced_free() {
    let c = MiniHudConfig::default();
    assert!(!c.enabled, "MiniHUD must be opt-in");
    assert_eq!(c.anchor, MiniHudAnchor::TopLeft);
    // Basic toggles on for a useful bare enable...
    assert!(c.show_fps && c.show_coords && c.show_facing && c.show_biome && c.show_ping);
    // ...every Advanced toggle off until individually opted in.
    assert!(
        !c.show_one_percent_low
            && !c.show_mspt_tps
            && !c.show_ram
            && !c.show_cpu
            && !c.show_gpu
            && !c.show_vram
            && !c.show_chunks_entities
            && !c.show_light_level
            && !c.show_shader_audio_status
    );
}

#[test]
fn disabled_config_formats_to_nothing() {
    let c = MiniHudConfig::default();
    let d = MiniHudData {
        fps: Some(144.0),
        ..MiniHudData::default()
    };
    assert!(format_mini_hud_lines(&c, &d).is_empty());
}

#[test]
fn enabled_basic_layout_is_compact() {
    let c = MiniHudConfig {
        enabled: true,
        ..MiniHudConfig::default()
    };
    let d = MiniHudData {
        fps: Some(143.6),
        coords: Some((-12.55, 64.0, 340.24)),
        facing: Some("NW".into()),
        biome: Some("minecraft:plains".into()),
        ping_ms: Some(42),
        ..MiniHudData::default()
    };
    let lines = format_mini_hud_lines(&c, &d);
    assert_eq!(lines.len(), 1, "basic toggles → one clean line: {lines:?}");
    let l = &lines[0];
    assert!(l.contains("144 FPS"), "{l}");
    assert!(l.contains("XYZ -12.6 64.0 340.2"), "{l}");
    assert!(l.contains("NW"), "{l}");
    assert!(l.contains("minecraft:plains"), "{l}");
    assert!(l.contains("Ping 42ms"), "{l}");
}

#[test]
fn advanced_toggles_add_grouped_lines() {
    let c = MiniHudConfig {
        enabled: true,
        show_coords: false,
        show_facing: false,
        show_biome: false,
        show_ping: false,
        show_one_percent_low: true,
        show_mspt_tps: true,
        show_ram: true,
        show_cpu: true,
        show_gpu: true,
        show_vram: true,
        show_chunks_entities: true,
        show_light_level: true,
        show_shader_audio_status: true,
        ..MiniHudConfig::default()
    };
    let d = MiniHudData {
        fps: Some(60.0),
        fps_one_percent_low: Some(41.2),
        mspt: Some(43.5),
        tps: Some(20.0),
        ram_used_mib: Some(12_000),
        ram_total_mib: Some(31_237),
        cpu_percent: Some(23.4),
        gpu_name: Some("AMD GPU (0x747e)".into()),
        gpu_busy_percent: Some(37),
        vram_used_mib: Some(1_024),
        vram_total_mib: Some(16_368),
        loaded_chunks: Some(441),
        entity_count: Some(88),
        block_light: Some(0),
        sky_light: Some(15),
        shader_name: Some("Complementary".into()),
        audio_ok: Some(true),
        ..MiniHudData::default()
    };
    let lines = format_mini_hud_lines(&c, &d);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "60 FPS");
    assert!(lines[1].contains("1% low 41 FPS"), "{}", lines[1]);
    assert!(lines[1].contains("43.5 mspt"), "{}", lines[1]);
    assert!(lines[1].contains("20.0 TPS"), "{}", lines[1]);
    assert!(lines[1].contains("RAM 12000/31237 MiB"), "{}", lines[1]);
    assert!(lines[1].contains("CPU 23%"), "{}", lines[1]);
    assert!(
        lines[2].contains("GPU AMD GPU (0x747e) 37%"),
        "{}",
        lines[2]
    );
    assert!(lines[2].contains("VRAM 1024/16368 MiB"), "{}", lines[2]);
    assert!(lines[3].contains("C 441"), "{}", lines[3]);
    assert!(lines[3].contains("E 88"), "{}", lines[3]);
    assert!(lines[3].contains("Light B0 S15"), "{}", lines[3]);
    assert!(lines[3].contains("Shader Complementary"), "{}", lines[3]);
    assert!(lines[3].contains("Audio ok"), "{}", lines[3]);
}

#[test]
fn enabled_toggles_with_missing_data_emit_no_dangling_segments() {
    let c = MiniHudConfig {
        enabled: true,
        show_ram: true,
        show_cpu: true,
        show_gpu: true,
        show_vram: true,
        show_mspt_tps: true,
        show_chunks_entities: true,
        show_light_level: true,
        show_shader_audio_status: true,
        ..MiniHudConfig::default()
    };
    // All data None: only the toggles that have data appear — here, none
    // of the advanced ones do, and basic has no data either.
    let lines = format_mini_hud_lines(&c, &MiniHudData::default());
    assert!(lines.is_empty(), "no data → no lines, got {lines:?}");
}

#[test]
fn anchor_choice_is_stored_not_reordered() {
    let c = MiniHudConfig {
        enabled: true,
        anchor: MiniHudAnchor::BottomRight,
        ..MiniHudConfig::default()
    };
    assert_eq!(c.anchor, MiniHudAnchor::BottomRight);
}

// ---------------------------------------------------------------------------
// Live host smoke tests (Linux)
// ---------------------------------------------------------------------------

#[test]
fn live_sample_reports_nonzero_ram_and_rss() {
    let mut sampler = CpuSampler::new();
    let snap = sample_host_metrics(&mut sampler);
    if cfg!(target_os = "linux") {
        let total = snap.mem_total_mib.expect("linux /proc/meminfo");
        assert!(total > 0, "MemTotal must be non-zero on Linux");
        let rss = snap.process_rss_mib;
        // RSS rounds down to whole MiB — only assert presence, not value.
        assert!(rss.is_some(), "VmRSS must parse on Linux");
        assert!(snap.mem_used_mib.unwrap() <= total);
        // First sample has no CPU baseline yet.
        assert_eq!(snap.cpu_percent, None);
        // /proc/stat counters only advance at jiffy granularity, so a
        // back-to-back second sample can legitimately see a zero delta and
        // report None. Poll a bounded number of times until real jiffies
        // elapse, then require the delta to be present and in range.
        let mut cpu = None;
        for _ in 0..1_000 {
            if let Some(p) = sample_host_metrics(&mut sampler).cpu_percent {
                cpu = Some(p);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let cpu = cpu.expect("CPU jiffies should advance within the bound");
        assert!((0.0..=100.0).contains(&cpu), "cpu={cpu}");
    } else {
        // Supported desktop platforms report system RAM; RSS and GPU remain
        // unavailable where no portable source is implemented.
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert!(snap.mem_total_mib.is_some_and(|mib| mib > 0));
        }
        assert!(snap.process_rss_mib.is_none());
        assert!(snap.gpus.is_empty());
    }
}

#[test]
fn live_gpu_enumeration_never_panics() {
    let gpus = sample_gpu_metrics();
    // Whatever the host has, every reported GPU is well-formed: names are
    // non-empty and any busy% is inside 0..=100 by construction.
    for g in &gpus {
        assert!(!g.name.is_empty());
        if let Some(b) = g.busy_percent {
            assert!(b <= 100);
        }
    }
}
