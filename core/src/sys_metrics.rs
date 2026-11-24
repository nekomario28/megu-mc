// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::sys_metrics` — a zero-dependency, fail-safe system and
//! performance telemetry snapshotter powering Megu's opt-in Advanced
//! MiniHUD and the server TUI.
//!
//! Everything here is pure `std`, never panics, never reads a clock, and
//! degrades to `None`/empty on any OS or kernel where the source files do
//! not exist:
//!
//! * Pure parsers for Linux `/proc` and DRM data, Windows CPU counters, and
//!   macOS `vm_stat`/`top` output:
//!   [`parse_proc_self_status_rss_kib`], [`parse_proc_meminfo`],
//!   [`parse_proc_stat_cpu_totals`] + [`cpu_usage_percent`], and
//!   [`parse_drm_gpu_metrics`]. These never touch the filesystem, so every
//!   malformed/truncated-input path is unit-testable.
//! * Live readers — [`sample_host_metrics`], [`sample_gpu_metrics`] and
//!   [`read_drm_gpu_metrics`] — that bind the parsers to platform sources.
//!   Linux reads `/proc` and `/sys/class/drm`; Windows uses kernel32; macOS
//!   uses `sysctl`, `vm_stat` and `top`. Unsupported sensors remain `None`.
//! * [`FrameTimingStats`] — a fixed-capacity ring buffer (default
//!   [`FRAME_TIMING_CAPACITY`] = 120 samples) producing deterministic
//!   `fps_avg`, `fps_one_percent_low`, `frame_ms_avg`, `frame_ms_p95`,
//!   `tps_avg` and `mspt_avg`. NaN, infinite, zero and negative durations
//!   are sanitised before they can poison the statistics.
//! * [`MiniHudConfig`] + [`format_mini_hud_lines`] — the opt-in overlay
//!   contract: disabled by default, anchored to a corner, with Basic
//!   toggles (fps/coords/facing/biome/ping) separate from the Advanced
//!   set (1% low, MSPT/TPS, RAM, CPU, GPU, VRAM, chunks/entities, light
//!   level, shader/audio) that all default off so enabling the HUD never
//!   clutters the vanilla-style display.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Upper bound on a single sysfs/procfs read: these files are kernel
/// generated and small, so anything past this is truncated rather than
/// trusted.
const MAX_PROC_SYSFS_READ: u64 = 64 * 1024;

/// Maximum number of DRM cards enumerated per snapshot — bounds the
/// `Vec<GpuMetrics>` allocation regardless of what `/sys` advertises.
const MAX_DRM_CARDS: usize = 16;

/// Sysfs root for DRM device telemetry.
const DRM_SYSFS_ROOT: &str = "/sys/class/drm";

/// Frame/tick durations below this are clamped up to it, so a zero or
/// negative reading can never divide by zero in an FPS computation.
const MIN_SAMPLE_MS: f64 = 0.01;

/// Durations above this (one minute per frame/tick) — and every non-finite
/// input — are clamped down to it so a single NaN/Inf stall cannot push a
/// percentile to infinity.
const MAX_SAMPLE_MS: f64 = 60_000.0;

/// Default rolling-window capacity for [`FrameTimingStats`]: two seconds
/// of 60 FPS frames.
pub const FRAME_TIMING_CAPACITY: usize = 120;

/// Vanilla server tick rate — `tps_avg` is capped here because a server
/// cannot run faster than real time.
const MAX_TPS: f64 = 20.0;

/// Maximum MiniHUD formatter output lines, keeping the overlay to a
/// narrow strip in its anchor corner.
const MAX_HUD_LINES: usize = 4;

/// Bytes per mebibyte for the sysfs VRAM counters, which are byte exact.
const BYTES_PER_MIB: u64 = 1_048_576;

// ---------------------------------------------------------------------------
// /proc parsers
// ---------------------------------------------------------------------------

/// Extract the KiB value from a `Name: 1234 kB` style `/proc` line.
fn parse_proc_kb_line(line: &str) -> Option<u64> {
    let mut parts = line.split_whitespace();
    let _name = parts.next()?;
    let value: u64 = parts.next()?.parse().ok()?;
    Some(value)
}

/// Parse `/proc/self/status`-style text and return the process resident
/// set size (`VmRSS:`) in kibibytes, or `None` when the field is absent or
/// malformed.
pub fn parse_proc_self_status_rss_kib(text: &str) -> Option<u64> {
    for line in text.lines() {
        if line.starts_with("VmRSS:") {
            return parse_proc_kb_line(line);
        }
    }
    None
}

/// System memory totals in MiB derived from `/proc/meminfo`.
///
/// `used_mib` is `total - available` (saturating). When the kernel does
/// not report `MemAvailable` (pre-3.14), the `MemFree + Buffers + Cached`
/// estimate Linux documented for the same purpose is used instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemInfoMiB {
    /// `MemTotal` in MiB; 0 when absent or malformed.
    pub total_mib: u64,
    /// `MemAvailable` (or the free+buffers+cached fallback) in MiB.
    pub available_mib: u64,
    /// `total - available`, saturated at 0.
    pub used_mib: u64,
}

/// Parse `/proc/meminfo` text into [`MemInfoMiB`]. Missing or malformed
/// fields yield 0 for that field; the function never fails.
pub fn parse_proc_meminfo(text: &str) -> MemInfoMiB {
    let mut total_kib: Option<u64> = None;
    let mut available_kib: Option<u64> = None;
    let mut free_kib: Option<u64> = None;
    let mut buffers_kib: Option<u64> = None;
    let mut cached_kib: Option<u64> = None;
    for line in text.lines() {
        let (slot, value) = if let Some(rest) = line.strip_prefix("MemTotal:") {
            (&mut total_kib, rest)
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            (&mut available_kib, rest)
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            (&mut free_kib, rest)
        } else if let Some(rest) = line.strip_prefix("Buffers:") {
            (&mut buffers_kib, rest)
        } else if let Some(rest) = line.strip_prefix("Cached:") {
            (&mut cached_kib, rest)
        } else {
            continue;
        };
        if let Some(kib) = value
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<u64>().ok())
        {
            *slot = Some(kib);
        }
    }
    let avail = available_kib.or_else(|| {
        // The pre-MemAvailable estimate; needs MemFree to mean anything.
        free_kib.map(|f| {
            f.saturating_add(buffers_kib.unwrap_or(0))
                .saturating_add(cached_kib.unwrap_or(0))
        })
    });
    let total = total_kib.unwrap_or(0) / 1024;
    let available = avail.unwrap_or(0) / 1024;
    MemInfoMiB {
        total_mib: total,
        available_mib: available,
        used_mib: total.saturating_sub(available),
    }
}

/// One cumulative CPU counter reading from the first `cpu ` line of
/// `/proc/stat`.
///
/// `total_jiffies` counts `user + nice + system + idle + iowait + irq +
/// softirq + steal`; the `guest`/`guest_nice` columns are deliberately
/// excluded because the kernel already folds them into `user`/`nice`.
/// `idle_jiffies` is `idle + iowait`, the standard "not busy" pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuSample {
    /// All non-idle plus idle jiffies, excluding guest time.
    pub total_jiffies: u64,
    /// `idle + iowait` jiffies.
    pub idle_jiffies: u64,
}

/// Parse the aggregate `cpu ` line of `/proc/stat` text. Per-core
/// `cpu0`, `cpu1`, … lines are ignored. Returns `None` when the line is
/// absent or carries fewer than `user nice system idle` columns.
pub fn parse_proc_stat_cpu_totals(text: &str) -> Option<CpuSample> {
    for line in text.lines() {
        // "cpu " with a trailing space matches only the aggregate line;
        // "cpu0"… are per-core rows.
        if !line.starts_with("cpu ") {
            continue;
        }
        let fields: Vec<u64> = line[4..]
            .split_whitespace()
            .map(|t| t.parse::<u64>().unwrap_or(0))
            .collect();
        if fields.len() < 4 {
            return None;
        }
        // Columns: user nice system idle iowait irq softirq steal guest
        // guest_nice. Guest time is already inside user/nice, so index 8
        // and 9 are summed out.
        let idle = fields[3].saturating_add(fields.get(4).copied().unwrap_or(0));
        let mut busy = fields[0]
            .saturating_add(fields[1])
            .saturating_add(fields[2]);
        for idx in [5usize, 6, 7] {
            busy = busy.saturating_add(fields.get(idx).copied().unwrap_or(0));
        }
        return Some(CpuSample {
            total_jiffies: busy.saturating_add(idle),
            idle_jiffies: idle,
        });
    }
    None
}

/// CPU busy percentage between two cumulative samples, clamped to
/// `0.0..=100.0`.
///
/// Returns `None` when either counter rolled backwards (kernel counter
/// reset or wrap) or when no time elapsed between the samples, so the
/// caller keeps showing the previous value rather than a garbage spike.
pub fn cpu_usage_percent(prev: &CpuSample, curr: &CpuSample) -> Option<f32> {
    let d_total = curr.total_jiffies.checked_sub(prev.total_jiffies)?;
    let d_idle = curr.idle_jiffies.checked_sub(prev.idle_jiffies)?;
    if d_total == 0 {
        return None;
    }
    let d_busy = d_total.saturating_sub(d_idle);
    let pct = (d_busy as f64) * 100.0 / (d_total as f64);
    Some((pct as f32).clamp(0.0, 100.0))
}

/// Parse the user/kernel/idle values from a Windows `GetSystemTimes` sample.
pub fn parse_windows_cpu_times(idle: u64, kernel: u64, user: u64) -> CpuSample {
    let total = kernel.saturating_add(user);
    CpuSample {
        total_jiffies: total,
        idle_jiffies: idle.min(total),
    }
}

/// Parse macOS `vm_stat` output. Available pages include free, inactive,
/// speculative and purgeable pages; total physical memory comes from sysctl.
pub fn parse_macos_vm_stat(text: &str, total_bytes: u64) -> Option<MemInfoMiB> {
    let page_size = text.lines().find_map(|line| {
        let start = line.find("page size of ")? + "page size of ".len();
        let digits: String = line[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<u64>().ok().filter(|&n| n > 0)
    })?;
    let mut available_pages = 0u64;
    for line in text.lines() {
        let Some((name, count)) = line.split_once(':') else {
            continue;
        };
        let count = count
            .trim()
            .trim_end_matches('.')
            .replace('.', "")
            .parse::<u64>()
            .ok();
        let Some(count) = count else { continue };
        if [
            "Pages free",
            "Pages inactive",
            "Pages speculative",
            "Pages purgeable",
        ]
        .contains(&name.trim())
        {
            available_pages = available_pages.saturating_add(count);
        }
    }
    let total_mib = total_bytes / BYTES_PER_MIB;
    if total_mib == 0 {
        return None;
    }
    let available_mib = available_pages.saturating_mul(page_size) / BYTES_PER_MIB;
    let available_mib = available_mib.min(total_mib);
    Some(MemInfoMiB {
        total_mib,
        available_mib,
        used_mib: total_mib.saturating_sub(available_mib),
    })
}

/// Parse the aggregate CPU line emitted by macOS `top -l 1 -n 0`.
pub fn parse_macos_top_cpu_percent(text: &str) -> Option<f32> {
    let line = text.lines().find(|line| line.contains("CPU usage:"))?;
    let idle = line
        .split(',')
        .find(|part| part.contains("idle"))?
        .split_whitespace()
        .find_map(|part| part.trim_end_matches('%').parse::<f32>().ok())?;
    idle.is_finite().then_some((100.0 - idle).clamp(0.0, 100.0))
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Default)]
struct MemoryStatusEx {
    dw_length: u32,
    dw_memory_load: u32,
    ull_total_phys: u64,
    ull_avail_phys: u64,
    ull_total_page_file: u64,
    ull_avail_page_file: u64,
    ull_total_virtual: u64,
    ull_avail_virtual: u64,
    ull_avail_extended_virtual: u64,
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
#[allow(unsafe_code)]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
}

#[cfg(target_os = "windows")]
fn windows_u64_time(time: &FileTime) -> u64 {
    (u64::from(time.high) << 32) | u64::from(time.low)
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn windows_memory() -> Option<MemInfoMiB> {
    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        ..MemoryStatusEx::default()
    };
    // SAFETY: the repr(C) output structure is valid and sized for this API.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 || status.ull_total_phys == 0 {
        return None;
    }
    let total_mib = status.ull_total_phys / BYTES_PER_MIB;
    let available_mib = (status.ull_avail_phys / BYTES_PER_MIB).min(total_mib);
    Some(MemInfoMiB {
        total_mib,
        available_mib,
        used_mib: total_mib.saturating_sub(available_mib),
    })
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn windows_cpu_sample() -> Option<CpuSample> {
    let (mut idle, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: all three pointers refer to writable FILETIME output values.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    Some(parse_windows_cpu_times(
        windows_u64_time(&idle),
        windows_u64_time(&kernel),
        windows_u64_time(&user),
    ))
}

#[cfg(target_os = "macos")]
fn macos_memory() -> Option<MemInfoMiB> {
    let total = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !total.status.success() {
        return None;
    }
    let total_bytes = std::str::from_utf8(&total.stdout)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let vm_stat = std::process::Command::new("/usr/bin/vm_stat")
        .output()
        .ok()?;
    if !vm_stat.status.success() {
        return None;
    }
    parse_macos_vm_stat(&String::from_utf8_lossy(&vm_stat.stdout), total_bytes)
}

#[cfg(target_os = "macos")]
fn macos_cpu_percent() -> Option<f32> {
    let output = std::process::Command::new("/usr/bin/top")
        .args(["-l", "1", "-n", "0"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_macos_top_cpu_percent(&String::from_utf8_lossy(&output.stdout))
}

// ---------------------------------------------------------------------------
// DRM sysfs GPU telemetry
// ---------------------------------------------------------------------------

/// Which driver family produced a [`GpuMetrics`] reading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuBackend {
    /// AMD (`amdgpu`, PCI vendor `0x1002`).
    Amd,
    /// Intel (`i915`/`xe`, PCI vendor `0x8086`).
    Intel,
    /// NVIDIA (PCI vendor `0x10de`).
    Nvidia,
    /// Qualcomm — Adreno parts on Snapdragon SoCs (PCI vendor `0x5143`).
    Qualcomm,
    /// Any other DRM device — virtio, simpledrm, unknown vendors.
    #[default]
    DrmOther,
}

/// One GPU's telemetry. Every numeric field is independently optional:
/// a driver may expose `gpu_busy_percent` but no VRAM counters (Intel
/// integrated), or vice versa.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuMetrics {
    /// Human-readable label such as `"AMD GPU (0x747e)"`.
    pub name: String,
    /// Driver family inferred from the PCI vendor id.
    pub backend: GpuBackend,
    /// `gpu_busy_percent`, clamped to 0–100.
    pub busy_percent: Option<u8>,
    /// `mem_info_vram_used` converted from bytes to MiB.
    pub vram_used_mib: Option<u64>,
    /// `mem_info_vram_total` converted from bytes to MiB.
    pub vram_total_mib: Option<u64>,
}

/// Map a PCI vendor id string (`"0x1002"`, `"1002"`) to a [`GpuBackend`].
pub fn gpu_backend_for_vendor(vendor_id: &str) -> GpuBackend {
    match vendor_id
        .trim()
        .trim_start_matches("0x")
        .to_ascii_lowercase()
        .as_str()
    {
        "1002" | "1022" => GpuBackend::Amd,
        "8086" => GpuBackend::Intel,
        "10de" => GpuBackend::Nvidia,
        "5143" => GpuBackend::Qualcomm,
        _ => GpuBackend::DrmOther,
    }
}

/// Parse a `gpu_busy_percent` sysfs value into `0..=100`.
pub fn parse_gpu_busy_percent(text: &str) -> Option<u8> {
    let v: i64 = text.trim().parse().ok()?;
    Some(v.clamp(0, 100) as u8)
}

/// Convert a sysfs byte counter (`mem_info_vram_*`) to MiB, rounding to
/// the nearest whole MiB so a 512-byte-over reading still displays its
/// true size.
pub fn parse_vram_bytes_to_mib(text: &str) -> Option<u64> {
    let bytes: u64 = text.trim().parse().ok()?;
    Some(bytes.saturating_add(BYTES_PER_MIB / 2) / BYTES_PER_MIB)
}

/// Pure assembly of one GPU's metrics from the strings its sysfs files
/// would contain. `vendor_id`/`device_id` are the contents of the `vendor`
/// and `device` attribute files (e.g. `"0x1002"`/`"0x747e"`); `driver` is
/// the `DRIVER=` name from `uevent` and is used only for the display name
/// when it names a known family.
///
/// Each `*_raw` is `None` when the attribute file does not exist, and a
/// malformed value degrades that single field to `None` without touching
/// the others.
pub fn parse_drm_gpu_metrics(
    vendor_id: &str,
    device_id: &str,
    driver: &str,
    busy_raw: Option<&str>,
    vram_used_raw: Option<&str>,
    vram_total_raw: Option<&str>,
) -> GpuMetrics {
    let backend = gpu_backend_for_vendor(vendor_id);
    let family = match backend {
        GpuBackend::Amd => "AMD",
        GpuBackend::Intel => "Intel",
        GpuBackend::Nvidia => "NVIDIA",
        GpuBackend::Qualcomm => "Qualcomm",
        GpuBackend::DrmOther => {
            if driver.is_empty() {
                "DRM"
            } else {
                driver
            }
        }
    };
    let device = device_id.trim();
    let name = if device.is_empty() {
        format!("{family} GPU")
    } else {
        format!("{family} GPU ({device})")
    };
    GpuMetrics {
        name,
        backend,
        busy_percent: busy_raw.and_then(parse_gpu_busy_percent),
        vram_used_mib: vram_used_raw.and_then(parse_vram_bytes_to_mib),
        vram_total_mib: vram_total_raw.and_then(parse_vram_bytes_to_mib),
    }
}

/// Bounded, trim-to-content read of one attribute file. Any I/O failure,
/// non-UTF-8 payload, or oversized file yields `None`.
fn read_attr_bounded(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_PROC_SYSFS_READ + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_PROC_SYSFS_READ {
        return None;
    }
    let text = String::from_utf8(buf).ok()?;
    Some(text.trim().to_string())
}

/// Extract `DRIVER=` from a kernel `uevent` attribute blob.
fn uevent_driver(text: &str) -> &str {
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("DRIVER=") {
            return v.trim();
        }
    }
    ""
}

/// Read one GPU's metrics from its DRM sysfs device directory — the
/// `device/` node under `/sys/class/drm/cardN` on Linux. Returns `None`
/// when the directory itself is absent; every individual attribute
/// degrades independently.
pub fn read_drm_gpu_metrics(device_dir: &Path) -> Option<GpuMetrics> {
    if !device_dir.is_dir() {
        return None;
    }
    let read = |name: &str| read_attr_bounded(&device_dir.join(name));
    let vendor = read("vendor").unwrap_or_default();
    let device = read("device").unwrap_or_default();
    let driver = read("uevent")
        .map(|u| uevent_driver(&u).to_string())
        .unwrap_or_default();
    Some(parse_drm_gpu_metrics(
        &vendor,
        &device,
        &driver,
        read("gpu_busy_percent").as_deref(),
        read("mem_info_vram_used").as_deref(),
        read("mem_info_vram_total").as_deref(),
    ))
}

/// Enumerate `/sys/class/drm/cardN/device` directories (at most
/// [`MAX_DRM_CARDS`], sorted by card name for determinism) and read each
/// one. Render nodes (`renderD*`) and connector entries (`cardN-DP-*`)
/// are skipped so every GPU appears exactly once. Empty on non-Linux or
/// when no DRM device exposes telemetry.
pub fn sample_gpu_metrics() -> Vec<GpuMetrics> {
    sample_gpu_metrics_under(Path::new(DRM_SYSFS_ROOT))
}

/// [`sample_gpu_metrics`] with an injectable sysfs root, so tests can
/// point it at a fixture directory.
pub fn sample_gpu_metrics_under(drm_root: &Path) -> Vec<GpuMetrics> {
    let mut cards: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(drm_root) {
        for entry in entries.flatten().take(MAX_DRM_CARDS * 4) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Keep only "card<digits>" — skips cardN-DP-*, cardN-HDMI-*,
            // renderD*, and the "version" file.
            let Some(suffix) = name.strip_prefix("card") else {
                continue;
            };
            if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let device_dir = entry.path().join("device");
            if device_dir.is_dir() {
                cards.push(device_dir);
            }
        }
    }
    cards.sort();
    cards.truncate(MAX_DRM_CARDS);
    cards
        .iter()
        .filter_map(|dir| read_drm_gpu_metrics(dir))
        .collect()
}

// ---------------------------------------------------------------------------
// Host snapshot
// ---------------------------------------------------------------------------

/// Cumulative CPU counter state carried between [`sample_host_metrics`]
/// calls so each snapshot reports the delta since the previous one.
#[derive(Clone, Debug, Default)]
pub struct CpuSampler {
    prev: Option<CpuSample>,
}

impl CpuSampler {
    /// Fresh sampler; the first [`sample_host_metrics`] it feeds reports
    /// `cpu_percent: None` because there is no baseline yet.
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// The most recent raw sample observed, if any.
    pub fn current(&self) -> Option<CpuSample> {
        self.prev
    }

    /// Fold a new cumulative sample in and return the busy percentage
    /// since the previous one (`None` on the first sample, counter
    /// rollback, or zero elapsed jiffies).
    pub fn observe(&mut self, sample: CpuSample) -> Option<f32> {
        let pct = self.prev.and_then(|prev| cpu_usage_percent(&prev, &sample));
        self.prev = Some(sample);
        pct
    }
}

/// One bounded snapshot of host RAM, CPU and GPU telemetry. Every field
/// is optional; on non-Linux hosts every read fails closed to `None` and
/// `gpus` is empty.
#[derive(Clone, Debug, Default)]
pub struct HostMetricsSnapshot {
    /// This process's resident set (`VmRSS`) in MiB.
    pub process_rss_mib: Option<u64>,
    /// System `MemTotal` in MiB.
    pub mem_total_mib: Option<u64>,
    /// `MemAvailable` (or fallback estimate) in MiB.
    pub mem_available_mib: Option<u64>,
    /// `total - available` in MiB.
    pub mem_used_mib: Option<u64>,
    /// Aggregate CPU busy % since the previous snapshot; `None` on the
    /// first sample.
    pub cpu_percent: Option<f32>,
    /// Per-GPU DRM metrics, at most [`MAX_DRM_CARDS`] entries.
    pub gpus: Vec<GpuMetrics>,
}

/// Collect one [`HostMetricsSnapshot`]. Never panics, never reads a
/// clock, and bounds every file read and every allocation. `sampler`
/// carries the CPU baseline — reuse one across frames.
pub fn sample_host_metrics(sampler: &mut CpuSampler) -> HostMetricsSnapshot {
    sample_host_metrics_under(
        sampler,
        Path::new("/proc/self/status"),
        Path::new("/proc/meminfo"),
        Path::new("/proc/stat"),
        Path::new(DRM_SYSFS_ROOT),
    )
}

/// [`sample_host_metrics`] with injectable paths for tests.
pub fn sample_host_metrics_under(
    _sampler: &mut CpuSampler,
    _status_path: &Path,
    _meminfo_path: &Path,
    _stat_path: &Path,
    drm_root: &Path,
) -> HostMetricsSnapshot {
    #[cfg(target_os = "linux")]
    let rss_mib = read_attr_bounded(_status_path)
        .and_then(|t| parse_proc_self_status_rss_kib(&t))
        .map(|kib| kib / 1024);
    #[cfg(not(target_os = "linux"))]
    let rss_mib = None;
    #[cfg(target_os = "linux")]
    let mem = read_attr_bounded(_meminfo_path).map(|t| parse_proc_meminfo(&t));
    #[cfg(target_os = "windows")]
    let mem = windows_memory();
    #[cfg(target_os = "macos")]
    let mem = macos_memory();
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let mem: Option<MemInfoMiB> = None;
    #[cfg(target_os = "linux")]
    let cpu_percent = read_attr_bounded(_stat_path)
        .and_then(|t| parse_proc_stat_cpu_totals(&t))
        .and_then(|sample| _sampler.observe(sample));
    #[cfg(target_os = "windows")]
    let cpu_percent = windows_cpu_sample().and_then(|sample| _sampler.observe(sample));
    #[cfg(target_os = "macos")]
    let cpu_percent = macos_cpu_percent();
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    let cpu_percent = None;
    HostMetricsSnapshot {
        process_rss_mib: rss_mib,
        mem_total_mib: mem.map(|m| m.total_mib),
        mem_available_mib: mem.map(|m| m.available_mib),
        mem_used_mib: mem.map(|m| m.used_mib),
        cpu_percent,
        gpus: sample_gpu_metrics_under(drm_root),
    }
}

// ---------------------------------------------------------------------------
// Rolling frame/tick timing statistics
// ---------------------------------------------------------------------------

/// Fixed-capacity ring of sanitised millisecond samples.
#[derive(Clone, Debug)]
struct SampleRing {
    buf: Vec<f64>,
    pos: usize,
    len: usize,
}

impl SampleRing {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity.max(1)],
            pos: 0,
            len: 0,
        }
    }

    fn push(&mut self, value: f64) {
        self.buf[self.pos] = value;
        self.pos = (self.pos + 1) % self.buf.len();
        self.len = (self.len + 1).min(self.buf.len());
    }

    fn len(&self) -> usize {
        self.len
    }

    /// Valid samples copied out for reduction. Order does not matter for
    /// the statistics below, so the raw ring order is fine.
    fn samples(&self) -> Vec<f64> {
        self.buf[..self.len].to_vec()
    }
}

/// Clamp a duration into a finite, positive, usable range: NaN/±Inf
/// collapse to [`MAX_SAMPLE_MS`] (treated as a stall), values ≤ 0 rise to
/// [`MIN_SAMPLE_MS`], and anything above one minute is capped.
fn sanitize_ms(ms: f64) -> f64 {
    if !ms.is_finite() {
        return MAX_SAMPLE_MS;
    }
    ms.clamp(MIN_SAMPLE_MS, MAX_SAMPLE_MS)
}

/// Rolling frame-time and tick-time statistics over a fixed-capacity
/// window.
///
/// Two independent rings — frame durations for FPS/frametime and tick
/// durations for MSPT/TPS — each capped at `capacity` samples
/// ([`FRAME_TIMING_CAPACITY`] by default), so memory is flat after
/// warmup and a single burst of samples evicts only its own size.
///
/// All getters return `0.0` on an empty window; every input is run
/// through [`sanitize_ms`] first, so no NaN, infinity, or non-positive
/// duration can enter the statistics.
#[derive(Clone, Debug)]
pub struct FrameTimingStats {
    frames: SampleRing,
    ticks: SampleRing,
}

impl Default for FrameTimingStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameTimingStats {
    /// Window of [`FRAME_TIMING_CAPACITY`] samples per ring.
    pub fn new() -> Self {
        Self::with_capacity(FRAME_TIMING_CAPACITY)
    }

    /// Window of `capacity` samples per ring (clamped to ≥ 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            frames: SampleRing::with_capacity(capacity),
            ticks: SampleRing::with_capacity(capacity),
        }
    }

    /// Record one frame duration in milliseconds.
    pub fn push_frame_ms(&mut self, ms: f64) {
        self.frames.push(sanitize_ms(ms));
    }

    /// Record one frame duration as a [`Duration`].
    pub fn push_frame(&mut self, d: Duration) {
        self.push_frame_ms(d.as_secs_f64() * 1_000.0);
    }

    /// Record one server tick duration in milliseconds.
    pub fn push_tick_ms(&mut self, ms: f64) {
        self.ticks.push(sanitize_ms(ms));
    }

    /// Record one server tick duration as a [`Duration`].
    pub fn push_tick(&mut self, d: Duration) {
        self.push_tick_ms(d.as_secs_f64() * 1_000.0);
    }

    /// Number of frame samples currently in the window.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Number of tick samples currently in the window.
    pub fn tick_count(&self) -> usize {
        self.ticks.len()
    }

    /// Mean frame time in milliseconds; `0.0` when empty.
    pub fn frame_ms_avg(&self) -> f64 {
        mean(&self.frames.samples())
    }

    /// 95th-percentile frame time in milliseconds (nearest-rank); `0.0`
    /// when empty.
    pub fn frame_ms_p95(&self) -> f64 {
        crate::stats::nearest_rank_percentile(&self.frames.samples(), 95.0).unwrap_or(0.0)
    }

    /// Mean FPS (`1000 / frame_ms_avg`); `0.0` when empty.
    pub fn fps_avg(&self) -> f64 {
        ms_to_fps(self.frame_ms_avg())
    }

    /// FPS computed from the mean of the worst 1% of frame times — the
    /// "1% low" figure that exposes stutter an average hides. With fewer
    /// than 100 samples the single worst frame stands in for the slice.
    /// `0.0` when empty.
    pub fn fps_one_percent_low(&self) -> f64 {
        let mut sorted = self.frames.samples();
        if sorted.is_empty() {
            return 0.0;
        }
        sorted.sort_by(f64::total_cmp);
        let worst = (sorted.len() / 100).max(1);
        let tail_ms = mean(&sorted[sorted.len() - worst..]);
        ms_to_fps(tail_ms)
    }

    /// Mean tick time (milliseconds per tick); `0.0` when empty.
    pub fn mspt_avg(&self) -> f64 {
        mean(&self.ticks.samples())
    }

    /// Effective ticks per second, capped at [`MAX_TPS`] = 20 because a
    /// vanilla-rate server cannot tick faster than real time; `0.0` when
    /// empty.
    pub fn tps_avg(&self) -> f64 {
        let mspt = self.mspt_avg();
        if mspt <= 0.0 {
            return 0.0;
        }
        (1_000.0 / mspt).min(MAX_TPS)
    }

    /// Drop every frame and tick sample.
    pub fn clear(&mut self) {
        self.frames.len = 0;
        self.frames.pos = 0;
        self.ticks.len = 0;
        self.ticks.pos = 0;
    }
}

fn mean(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

/// Nearest-rank percentile: the smallest sample ≥ `pct`% of the window.
fn ms_to_fps(frame_ms: f64) -> f64 {
    if frame_ms <= 0.0 {
        return 0.0;
    }
    1_000.0 / frame_ms
}

// ---------------------------------------------------------------------------
// MiniHUD configuration and formatter
// ---------------------------------------------------------------------------

/// Corner the MiniHUD overlay is anchored to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MiniHudAnchor {
    /// Above the vanilla debug-free top-left area.
    #[default]
    TopLeft,
    /// Top-right of the window.
    TopRight,
    /// Bottom-left, above the hotbar.
    BottomLeft,
    /// Bottom-right of the window.
    BottomRight,
}

/// Which MiniHUD segments are drawn, and where.
///
/// `enabled` defaults to `false`: the overlay is strictly opt-in and the
/// vanilla HUD is untouched until the player turns it on. The Basic
/// toggles (`show_fps`, `show_coords`, `show_facing`, `show_biome`,
/// `show_ping`) default to `true` so a bare enable yields a useful
/// vanilla-plus readout, while every Advanced toggle defaults to `false`
/// and must be opted into individually.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MiniHudConfig {
    /// Master switch — `false` (the default) renders nothing.
    pub enabled: bool,
    /// Anchor corner.
    pub anchor: MiniHudAnchor,
    /// Basic: current FPS.
    pub show_fps: bool,
    /// Basic: `X Y Z` coordinates.
    pub show_coords: bool,
    /// Basic: cardinal facing.
    pub show_facing: bool,
    /// Basic: biome name.
    pub show_biome: bool,
    /// Basic: server ping.
    pub show_ping: bool,
    /// Advanced: 1% low FPS.
    pub show_one_percent_low: bool,
    /// Advanced: MSPT and TPS.
    pub show_mspt_tps: bool,
    /// Advanced: system RAM usage.
    pub show_ram: bool,
    /// Advanced: CPU busy %.
    pub show_cpu: bool,
    /// Advanced: GPU name and busy %.
    pub show_gpu: bool,
    /// Advanced: VRAM usage.
    pub show_vram: bool,
    /// Advanced: loaded chunks and entity count.
    pub show_chunks_entities: bool,
    /// Advanced: block/sky light level at the player.
    pub show_light_level: bool,
    /// Advanced: active shader pack and audio-engine status.
    pub show_shader_audio_status: bool,
}

impl Default for MiniHudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            anchor: MiniHudAnchor::TopLeft,
            show_fps: true,
            show_coords: true,
            show_facing: true,
            show_biome: true,
            show_ping: true,
            show_one_percent_low: false,
            show_mspt_tps: false,
            show_ram: false,
            show_cpu: false,
            show_gpu: false,
            show_vram: false,
            show_chunks_entities: false,
            show_light_level: false,
            show_shader_audio_status: false,
        }
    }
}

/// Point-in-time data the formatter renders. Fields the config enables
/// but the data leaves as `None` are silently omitted from the output —
/// a dedicated-server MiniHUD simply shows no FPS, and a client shows no
/// MSPT.
#[derive(Clone, Debug, Default)]
pub struct MiniHudData {
    /// Smoothed FPS (e.g. [`FrameTimingStats::fps_avg`]).
    pub fps: Option<f64>,
    /// 1% low FPS ([`FrameTimingStats::fps_one_percent_low`]).
    pub fps_one_percent_low: Option<f64>,
    /// Mean milliseconds per server tick.
    pub mspt: Option<f64>,
    /// Ticks per second.
    pub tps: Option<f64>,
    /// Player position `X Y Z`.
    pub coords: Option<(f64, f64, f64)>,
    /// Cardinal/intercardinal facing label (`"N"`, `"SW"`, …).
    pub facing: Option<String>,
    /// Biome id or display name.
    pub biome: Option<String>,
    /// Server round-trip ping in milliseconds.
    pub ping_ms: Option<u32>,
    /// System RAM used in MiB.
    pub ram_used_mib: Option<u64>,
    /// System RAM total in MiB.
    pub ram_total_mib: Option<u64>,
    /// CPU busy percentage.
    pub cpu_percent: Option<f32>,
    /// GPU display name (e.g. [`GpuMetrics::name`]).
    pub gpu_name: Option<String>,
    /// GPU busy percentage ([`GpuMetrics::busy_percent`]).
    pub gpu_busy_percent: Option<u8>,
    /// VRAM used in MiB.
    pub vram_used_mib: Option<u64>,
    /// VRAM total in MiB.
    pub vram_total_mib: Option<u64>,
    /// Loaded chunk count.
    pub loaded_chunks: Option<u32>,
    /// Rendered/tracked entity count.
    pub entity_count: Option<u32>,
    /// Block light at the player position, 0–15.
    pub block_light: Option<u8>,
    /// Sky light at the player position, 0–15.
    pub sky_light: Option<u8>,
    /// Active shader pack name (`None`/`Some` when the segment is on).
    pub shader_name: Option<String>,
    /// Audio engine health (`Some(true)` ok / `Some(false)` muted or
    /// failed); `None` hides the audio half of the segment.
    pub audio_ok: Option<bool>,
}

impl MiniHudData {
    /// Build a HUD snapshot from live telemetry: pulls RAM/CPU/GPU out
    /// of a [`HostMetricsSnapshot`] and FPS/1%-low out of a
    /// [`FrameTimingStats`]. World-side fields are left `None` for the
    /// caller to fill.
    pub fn from_telemetry(host: &HostMetricsSnapshot, frame: &FrameTimingStats) -> Self {
        let gpu = host.gpus.first();
        Self {
            fps: (frame.frame_count() > 0).then(|| frame.fps_avg()),
            fps_one_percent_low: (frame.frame_count() > 0).then(|| frame.fps_one_percent_low()),
            mspt: (frame.tick_count() > 0).then(|| frame.mspt_avg()),
            tps: (frame.tick_count() > 0).then(|| frame.tps_avg()),
            ram_used_mib: host.mem_used_mib,
            ram_total_mib: host.mem_total_mib,
            cpu_percent: host.cpu_percent,
            gpu_name: gpu.map(|g| g.name.clone()),
            gpu_busy_percent: gpu.and_then(|g| g.busy_percent),
            vram_used_mib: gpu.and_then(|g| g.vram_used_mib),
            vram_total_mib: gpu.and_then(|g| g.vram_total_mib),
            ..Self::default()
        }
    }
}

fn push_segment(line: &mut Vec<String>, segment: Option<String>) {
    if let Some(s) = segment {
        if !s.is_empty() {
            line.push(s);
        }
    }
}

fn join_line(segments: Vec<String>) -> Option<String> {
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("  "))
    }
}

/// Render the configured MiniHUD lines — at most [`MAX_HUD_LINES`]
/// compact, segment-joined strings, in top-to-bottom draw order.
///
/// Returns an empty `Vec` when `config.enabled` is `false`, so an opt-out
/// player sees exactly the vanilla HUD. Lines group loosely by theme:
/// basic readouts first, then frame/tick stats, host hardware, and world
/// state; empty groups disappear entirely so no blank rows are drawn.
pub fn format_mini_hud_lines(config: &MiniHudConfig, data: &MiniHudData) -> Vec<String> {
    if !config.enabled {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::with_capacity(MAX_HUD_LINES);

    // Line 1 — Basic: FPS, position, facing, biome, ping.
    let mut basic = Vec::new();
    if config.show_fps {
        push_segment(&mut basic, data.fps.map(|f| format!("{f:.0} FPS")));
    }
    if config.show_coords {
        push_segment(
            &mut basic,
            data.coords
                .map(|(x, y, z)| format!("XYZ {x:.1} {y:.1} {z:.1}")),
        );
    }
    if config.show_facing {
        push_segment(&mut basic, data.facing.clone());
    }
    if config.show_biome {
        push_segment(&mut basic, data.biome.clone());
    }
    if config.show_ping {
        push_segment(&mut basic, data.ping_ms.map(|p| format!("Ping {p}ms")));
    }
    if let Some(l) = join_line(basic) {
        lines.push(l);
    }

    // Line 2 — Advanced perf: 1% low, MSPT/TPS, RAM, CPU.
    let mut perf = Vec::new();
    if config.show_one_percent_low {
        push_segment(
            &mut perf,
            data.fps_one_percent_low
                .map(|f| format!("1% low {f:.0} FPS")),
        );
    }
    if config.show_mspt_tps {
        let seg = match (data.mspt, data.tps) {
            (Some(m), Some(t)) => Some(format!("{m:.1} mspt  {t:.1} TPS")),
            (Some(m), None) => Some(format!("{m:.1} mspt")),
            (None, Some(t)) => Some(format!("{t:.1} TPS")),
            (None, None) => None,
        };
        push_segment(&mut perf, seg);
    }
    if config.show_ram {
        let seg = match (data.ram_used_mib, data.ram_total_mib) {
            (Some(u), Some(t)) => Some(format!("RAM {u}/{t} MiB")),
            (Some(u), None) => Some(format!("RAM {u} MiB")),
            (None, Some(t)) => Some(format!("RAM /{t} MiB")),
            (None, None) => None,
        };
        push_segment(&mut perf, seg);
    }
    if config.show_cpu {
        push_segment(&mut perf, data.cpu_percent.map(|c| format!("CPU {c:.0}%")));
    }
    if let Some(l) = join_line(perf) {
        lines.push(l);
    }

    // Line 3 — Advanced GPU: name+busy, VRAM.
    let mut gpu = Vec::new();
    if config.show_gpu {
        let seg = match (&data.gpu_name, data.gpu_busy_percent) {
            (Some(n), Some(b)) => Some(format!("GPU {n} {b}%")),
            (Some(n), None) => Some(format!("GPU {n}")),
            (None, Some(b)) => Some(format!("GPU {b}%")),
            (None, None) => None,
        };
        push_segment(&mut gpu, seg);
    }
    if config.show_vram {
        let seg = match (data.vram_used_mib, data.vram_total_mib) {
            (Some(u), Some(t)) => Some(format!("VRAM {u}/{t} MiB")),
            (Some(u), None) => Some(format!("VRAM {u} MiB")),
            (None, Some(t)) => Some(format!("VRAM /{t} MiB")),
            (None, None) => None,
        };
        push_segment(&mut gpu, seg);
    }
    if let Some(l) = join_line(gpu) {
        lines.push(l);
    }

    // Line 4 — Advanced world: chunks/entities, light, shader/audio.
    let mut world = Vec::new();
    if config.show_chunks_entities {
        let seg = match (data.loaded_chunks, data.entity_count) {
            (Some(c), Some(e)) => Some(format!("C {c}  E {e}")),
            (Some(c), None) => Some(format!("C {c}")),
            (None, Some(e)) => Some(format!("E {e}")),
            (None, None) => None,
        };
        push_segment(&mut world, seg);
    }
    if config.show_light_level {
        let seg = match (data.block_light, data.sky_light) {
            (Some(b), Some(s)) => Some(format!("Light B{b} S{s}")),
            (Some(b), None) => Some(format!("Light B{b}")),
            (None, Some(s)) => Some(format!("Light S{s}")),
            (None, None) => None,
        };
        push_segment(&mut world, seg);
    }
    if config.show_shader_audio_status {
        let seg = match (&data.shader_name, data.audio_ok) {
            (Some(s), Some(true)) => Some(format!("Shader {s}  Audio ok")),
            (Some(s), Some(false)) => Some(format!("Shader {s}  Audio off")),
            (Some(s), None) => Some(format!("Shader {s}")),
            (None, Some(true)) => Some("Audio ok".to_string()),
            (None, Some(false)) => Some("Audio off".to_string()),
            (None, None) => None,
        };
        push_segment(&mut world, seg);
    }
    if let Some(l) = join_line(world) {
        lines.push(l);
    }

    lines.truncate(MAX_HUD_LINES);
    lines
}
