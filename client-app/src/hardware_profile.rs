// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Read-only host hardware probe and deterministic auto-tuning recommender.
//!
//! Two layers, kept strictly apart so every recommendation is testable
//! without touching the machine the tests run on:
//!
//! * Pure parsers — [`parse_meminfo_total_kib`], [`parse_cache_size_kib`],
//!   [`parse_pci_id`], [`parse_sysctl_memsize_mib`],
//!   [`parse_sysctl_l3_cache_kib`], [`classify_macos_cpu_brand`],
//!   [`classify_linux_drm_uevent`], and
//!   [`classify_gpu`] turn `/proc` + `/sys` + `sysctl` text into
//!   typed values. They never read files themselves.
//! * [`probe_host_hardware`] — a read-only host probe that feeds those
//!   parsers from `/proc/meminfo`,
//!   `/sys/devices/system/cpu/cpu0/cache/index*/{level,size}`, and
//!   `/sys/class/drm/card*/device/{vendor,device}` (with a `uevent`
//!   fallback for ARM64 platform GPUs that carry no PCI ids) on Linux, from
//!   `sysctl -n hw.memsize` / `hw.l3cachesize` /
//!   `machdep.cpu.brand_string` on macOS, and from the Win32
//!   `GlobalMemoryStatusEx` kernel32 call on Windows. On hosts with no
//!   probe the fields degrade gracefully (`total_ram_mib`/
//!   `l3_cache_kib` become `0`, `gpu_class` becomes
//!   [`GpuVendorClass::Unknown`]).
//!
//! [`recommend_tuning`] maps a [`HostHardwareProfile`] onto a
//! [`HardwareTuningPlan`]: GPU backend preference, meshing worker threads,
//! chunk cache budget, view distance / LOD radius / per-frame rebuild
//! budget, and an opt-in local-LLM translation model tier. Manual user
//! overrides ALWAYS win: apply the plan via [`HardwareTuningPlan::apply_to`]
//! *before* TOML files, `options.txt` imports, or explicit CLI flags are
//! overlaid, never after.
//!
//! Pure `std` Rust; zero dependencies beyond `megu-client-mesh` types.

use megu_client_mesh::gpu_pipeline::RenderBackendPreference;

use crate::settings::{
    ClientSettings, DistantHorizon, QualityPreset, DISTANT_HORIZON_MEDIUM_CHUNKS,
    MAX_VIEW_DISTANCE, MIN_VIEW_DISTANCE,
};

/// PCI vendor ids as written to `/sys/class/drm/card*/device/vendor`.
pub const PCI_VENDOR_NVIDIA: u16 = 0x10de;
pub const PCI_VENDOR_AMD: u16 = 0x1002;
pub const PCI_VENDOR_INTEL: u16 = 0x8086;
/// Microsoft Basic Display Adapter / software display device.
pub const PCI_VENDOR_MICROSOFT: u16 = 0x1414;
/// Qualcomm PCI vendor id (`PCI\VEN_5143`). Windows ARM64 Snapdragon
/// laptops more often report `ACPI\VEN_QCOM` (or the `4D4F4351`
/// little-endian "QCOM" encoding); both fold onto this id.
pub const PCI_VENDOR_QUALCOMM: u16 = 0x5143;
/// ARM Ltd. PCI vendor id (PCIe-attached Mali; platform Mali is detected
/// through the DRM uevent instead).
pub const PCI_VENDOR_ARM: u16 = 0x13b5;
/// Broadcom PCI vendor id (Raspberry Pi VideoCore reports as a platform
/// device, but keep the PCI id mapped for completeness).
pub const PCI_VENDOR_BROADCOM: u16 = 0x14e4;

/// Coarse GPU class the auto-tuner reasons about. Finer distinctions
/// (exact model, driver) deliberately stay out of scope — only what the
/// tuning policy actually branches on is tracked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GpuVendorClass {
    /// A software display adapter or explicit headless/no-GPU profile was
    /// positively identified.
    NoneSoftware,
    /// Legacy Intel integrated GPU (Gen7-Gen11: HD 4000 through UHD 630).
    /// Only pure wgpu core (Vulkan 1.1 / DX12 / Metal) is safe to assume — the
    /// `classic-low-spec` pipeline exists for exactly this class.
    IntelLegacyIntegrated,
    /// Modern Intel integrated GPU (Gen12+/Xe: Tiger Lake and newer).
    IntelModernIntegrated,
    /// Intel Arc discrete GPU (Alchemist / Battlemage PCI device ids).
    IntelArc,
    /// NVIDIA dedicated GPU.
    NvidiaDedicated,
    /// AMD dedicated GPU.
    AmdDedicated,
    /// Apple Silicon unified-memory GPU (probe fallback on macOS).
    AppleSilicon,
    /// Qualcomm Snapdragon Adreno tile-based GPU — Windows ARM64
    /// (`ACPI\VEN_QCOM` / `PCI\VEN_5143`, Snapdragon X Elite/Plus/8cx)
    /// and Linux ARM64 (`DRIVER=msm` / `qcom,adreno` compatible,
    /// Freedreno/Turnip).
    QualcommAdreno,
    /// ARM Mali tile-based GPU on a Linux platform device
    /// (`DRIVER=panfrost`/`panthor`, `arm,mali` compatible).
    ArmMali,
    /// Broadcom VideoCore tile-based GPU (Raspberry Pi `DRIVER=v3d`/`vc4`,
    /// `brcm,*-v3d`/`vc4` compatible).
    BroadcomVideocore,
    /// A GPU device exists but its vendor id is unrecognized.
    #[default]
    Unknown,
}

/// One read-only snapshot of the host's hardware. `0` numeric fields mean
/// "not reported by the host" (non-Linux probe, missing sysfs entries) —
/// recommenders degrade to the conservative lean tier rather than
/// assuming a large machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostHardwareProfile {
    /// Logical CPU count from [`std::thread::available_parallelism`].
    pub logical_cpus: u16,
    /// L3 cache size in KiB summed across `cpu0/cache/index*/level == 3`.
    pub l3_cache_kib: u32,
    /// `MemTotal` from `/proc/meminfo` in MiB.
    pub total_ram_mib: u32,
    /// Strongest GPU class discovered (dedicated wins over integrated).
    pub gpu_class: GpuVendorClass,
    /// Best-effort VRAM estimate in MiB: the driver's real
    /// `mem_info_vram_total` when sysfs reports it (amdgpu), a
    /// conservative shared-memory carve for integrated GPUs, `0` when
    /// unknown.
    pub estimated_vram_mib: u32,
    /// `true` when the GPU draws its memory from main RAM (integrated /
    /// unified-memory parts).
    pub shared_system_ram: bool,
}

/// Opt-in local translation-model size tier. Only consulted when the user
/// opts into local mod translation; the tiers exist to stop an oversized
/// model from OOM-freezing the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalLlmRecommendation {
    /// Under 8 GiB RAM: no local model — stay on the remote path.
    TooLowRam,
    /// 8-16 GiB RAM: small quantized models (2B-4B parameters).
    SmallQuantized2BTo4B,
    /// 16-32 GiB RAM: medium quantized models (8B-12B parameters).
    MediumQuantized8BTo12B,
    /// 32+ GiB RAM, or a dedicated GPU with 16+ GiB VRAM: large local
    /// models are acceptable.
    LargeLocalAllowed,
}

/// The auto-tuner's output: suggested engine budgets for one host.
/// These are *defaults* — anything the user set explicitly (TOML,
/// `options.txt`, CLI flags) overrides them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardwareTuningPlan {
    /// Preferred GPU pipeline for this hardware class.
    pub gpu_backend: RenderBackendPreference,
    /// Meshing/decode worker threads; always leaves at least one core for
    /// the main tick/render loop.
    pub worker_threads: u16,
    /// CPU-side chunk data cache budget in MiB.
    pub chunk_cache_budget_mib: u32,
    /// Suggested full-detail view distance in chunks.
    pub view_distance: u8,
    /// Suggested far-terrain LOD radius in chunks (`0` = off).
    pub lod_radius_chunks: u16,
    /// Dirty-section rebuilds allowed per frame.
    pub max_rebuilds_per_frame: u8,
    /// Opt-in local translation-model tier.
    pub local_llm: LocalLlmRecommendation,
}

impl HardwareTuningPlan {
    /// Seed `settings` with this plan's player-visible fields
    /// (`gpu_backend`, `view_distance`, `distant_horizon`) and mark the
    /// preset `Custom`. Apply *before* any TOML/options.txt/CLI overlay so
    /// explicit user values always win over the recommendation.
    pub fn apply_to(&self, settings: &mut ClientSettings) {
        settings.gpu_backend = self.gpu_backend;
        settings.view_distance = self
            .view_distance
            .clamp(MIN_VIEW_DISTANCE, MAX_VIEW_DISTANCE);
        settings.distant_horizon = if self.lod_radius_chunks == 0 {
            DistantHorizon::Off
        } else if self.lod_radius_chunks <= DISTANT_HORIZON_MEDIUM_CHUNKS as u16 {
            DistantHorizon::Medium
        } else {
            DistantHorizon::Far
        };
        settings.preset = QualityPreset::Custom;
    }
}

// ---------------------------------------------------------------------------
// Pure parsers (deterministic; no I/O)
// ---------------------------------------------------------------------------

/// Extract `MemTotal` (KiB) from `/proc/meminfo` text. `None` when the key
/// is absent or its value is unparseable.
pub fn parse_meminfo_total_kib(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("MemTotal:") else {
            continue;
        };
        let mut tokens = rest.split_whitespace();
        let value: u64 = tokens.next()?.parse().ok()?;
        // The kernel always reports kB; accept an absent unit too but
        // refuse a surprise unit rather than mis-scaling RAM.
        if let Some(unit) = tokens.next() {
            if !unit.eq_ignore_ascii_case("kb") && !unit.eq_ignore_ascii_case("kib") {
                return None;
            }
        }
        return Some(value);
    }
    None
}

/// Parse a sysfs cache `size` file into KiB. Handles the kernel's
/// `K`/`M`/`G` suffixes (`98304K`, `96M`, `6144K`, `1G`) and bare numbers
/// (already KiB). `None` on empty or unparseable input.
pub fn parse_cache_size_kib(text: &str) -> Option<u32> {
    let trimmed = text.trim();
    let (digits, multiplier) = match trimmed.chars().last()? {
        'K' | 'k' => (&trimmed[..trimmed.len() - 1], 1u64),
        'M' | 'm' => (&trimmed[..trimmed.len() - 1], 1024u64),
        'G' | 'g' => (&trimmed[..trimmed.len() - 1], 1024u64 * 1024u64),
        _ => (trimmed, 1u64),
    };
    let value: u64 = digits.trim().parse().ok()?;
    // sysfs text is host-controlled but still untrusted input: saturate
    // instead of overflowing on absurd magnitudes.
    Some(value.saturating_mul(multiplier).min(u64::from(u32::MAX)) as u32)
}

/// Parse a sysfs PCI `vendor`/`device` file (`0x10de\n`) into a hex id.
/// `None` on missing `0x` prefix or unparseable digits.
pub fn parse_pci_id(text: &str) -> Option<u16> {
    let trimmed = text.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    u16::from_str_radix(hex, 16).ok()
}

/// Parse a Windows display adapter's PCI device ID and name. Returns
/// `(vendor_id, device_id)` for PCI adapters, or `(0x1414, 0)` for a
/// confirmed software adapter. `None` means the device was not identified.
/// This parser is platform independent so every CI OS can exercise it.
pub fn parse_windows_display_device_id(device_id: &str, device_string: &str) -> Option<(u16, u16)> {
    let name = device_string.to_ascii_lowercase();
    if [
        "llvmpipe",
        "lavapipe",
        "swiftshader",
        "microsoft basic render",
    ]
    .iter()
    .any(|marker| name.contains(marker))
    {
        return Some((PCI_VENDOR_MICROSOFT, 0));
    }

    let mut vendor_id = None;
    let mut device_id_value = None;
    for component in device_id.split(['&', '\\', '#']) {
        if let Some(rest) = component
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("VEN_"))
            .and_then(|_| component.get(4..))
        {
            // Snapdragon Windows ARM64 devices report ACPI ids rather
            // than PCI: `VEN_QCOM` and the `4D4F4351` little-endian
            // "QCOM" encoding are not 16-bit hex, so map them onto
            // Qualcomm's real PCI vendor id explicitly.
            if rest.eq_ignore_ascii_case("qcom") || rest.eq_ignore_ascii_case("4d4f4351") {
                vendor_id = Some(PCI_VENDOR_QUALCOMM);
            } else if let Ok(value) = u16::from_str_radix(rest, 16) {
                vendor_id = Some(value);
            }
        }
        if let Some(value) = component
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("DEV_"))
            .and_then(|_| component.get(4..))
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
        {
            device_id_value = Some(value);
        }
        // Compact ACPI PNP id form (`ACPI\QCOM0D0A`): vendor prefix plus
        // a 4-hex-digit device part, no `VEN_`/`DEV_` separators.
        if let Some(dev) = component
            .get(..4)
            .filter(|prefix| prefix.eq_ignore_ascii_case("QCOM"))
            .and_then(|_| component.get(4..))
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
        {
            vendor_id = Some(PCI_VENDOR_QUALCOMM);
            device_id_value = Some(dev);
        }
    }
    if let Some(vendor) = vendor_id {
        return Some((vendor, device_id_value.unwrap_or(0)));
    }

    // Some display drivers omit the PCI identifier. Their Windows adapter
    // description still carries a usable vendor name.
    let vendor = if name.contains("nvidia") {
        PCI_VENDOR_NVIDIA
    } else if name.contains("amd") || name.contains("radeon") || name.contains("ati ") {
        PCI_VENDOR_AMD
    } else if name.contains("intel") {
        PCI_VENDOR_INTEL
    } else if name.contains("qualcomm") || name.contains("adreno") || name.contains("snapdragon") {
        PCI_VENDOR_QUALCOMM
    } else {
        return None;
    };
    Some((vendor, device_id_value.unwrap_or(0)))
}

/// Parse `sysctl -n hw.memsize` output — a byte count in decimal — into
/// MiB. `None` on empty or unparseable input; sub-MiB values floor to `0`.
pub fn parse_sysctl_memsize_mib(text: &str) -> Option<u32> {
    let bytes: u64 = text.trim().parse().ok()?;
    Some((bytes / (1024 * 1024)).min(u64::from(u32::MAX)) as u32)
}

/// Parse `sysctl -n hw.l3cachesize` output — a byte count in decimal —
/// into KiB. `None` on empty or unparseable input.
pub fn parse_sysctl_l3_cache_kib(text: &str) -> Option<u32> {
    let bytes: u64 = text.trim().parse().ok()?;
    Some((bytes / 1024).min(u64::from(u32::MAX)) as u32)
}

/// Classify a macOS `machdep.cpu.brand_string` value. Apple Silicon parts
/// report `Apple M<n>[ Pro|Max|Ultra]` and share one unified-memory GPU,
/// so they map to [`GpuVendorClass::AppleSilicon`]. Every other brand —
/// Intel Core parts, empty or garbled output — maps to
/// [`GpuVendorClass::Unknown`]: a Mac always has a GPU, the CPU brand
/// string just cannot name an Intel one's vendor, and `Unknown`
/// negotiates `Auto` rather than claiming the software rasterizer.
pub fn classify_macos_cpu_brand(brand: &str) -> GpuVendorClass {
    let mut words = brand.split_whitespace();
    let apple = words
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("apple"));
    let silicon = words.next().is_some_and(|model| {
        model
            .strip_prefix('M')
            .or_else(|| model.strip_prefix('m'))
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit())
    });
    if apple && silicon {
        GpuVendorClass::AppleSilicon
    } else {
        GpuVendorClass::Unknown
    }
}

/// Map a PCI vendor (+ optional device) id pair onto a [`GpuVendorClass`].
/// `vendor_id == None` means the probe did not identify an adapter →
/// [`GpuVendorClass::Unknown`]; an unrecognized vendor →
/// [`GpuVendorClass::Unknown`].
pub fn classify_gpu(vendor_id: Option<u16>, device_id: Option<u16>) -> GpuVendorClass {
    match vendor_id {
        None => GpuVendorClass::Unknown,
        Some(PCI_VENDOR_MICROSOFT) => GpuVendorClass::NoneSoftware,
        Some(PCI_VENDOR_NVIDIA) => GpuVendorClass::NvidiaDedicated,
        Some(PCI_VENDOR_AMD) => GpuVendorClass::AmdDedicated,
        Some(PCI_VENDOR_QUALCOMM) => GpuVendorClass::QualcommAdreno,
        Some(PCI_VENDOR_ARM) => GpuVendorClass::ArmMali,
        Some(PCI_VENDOR_BROADCOM) => GpuVendorClass::BroadcomVideocore,
        Some(PCI_VENDOR_INTEL) => device_id
            .map(classify_intel_device)
            // A vendor id with no device file still means an Intel GPU;
            // assume the modern path and let capability negotiation
            // downgrade if the hardware is actually old.
            .unwrap_or(GpuVendorClass::IntelModernIntegrated),
        Some(_) => GpuVendorClass::Unknown,
    }
}

/// Refine an Intel GPU by PCI device id: Arc discrete cards, Gen12+/Xe
/// integrated parts, and the Gen7-Gen11 legacy parts
/// `classic-low-spec` was built for. Unrecognized ids take the
/// conservative legacy path — ClassicLowSpec is always available, so a
/// wrong guess degrades performance rather than failing to render.
pub fn classify_intel_device(device_id: u16) -> GpuVendorClass {
    match device_id {
        // Arc Alchemist/DG2 (0x56A0-0x56FF) and Battlemage (0xE200-0xE2FF).
        0x56A0..=0x56FF | 0xE200..=0xE2FF => GpuVendorClass::IntelArc,
        // Gen12+/Xe integrated: Tiger Lake 0x9Axx, DG1/Xe MAX 0x49xx,
        // Alder Lake 0x46xx, Raptor Lake 0xA7xx, Meteor Lake 0x7Dxx,
        // Lunar Lake 0x64xx, Panther Lake 0xB0xx.
        0x4600..=0x46FF
        | 0x4900..=0x49FF
        | 0x6400..=0x64FF
        | 0x7D00..=0x7DFF
        | 0x9A00..=0x9AFF
        | 0xA700..=0xA7FF
        | 0xB000..=0xB0FF => GpuVendorClass::IntelModernIntegrated,
        // Gen7-Gen11 (HD 4000 - UHD 630, 0x01xx-0x9Bxx outside the ranges
        // above) and anything unrecognized.
        _ => GpuVendorClass::IntelLegacyIntegrated,
    }
}

/// Classify a Linux DRM device's `uevent` file when the PCI
/// `vendor`/`device` ids are absent — the common case on ARM64
/// DeviceTree laptops and SBCs, where the GPU is a platform device.
/// Recognizes Qualcomm Adreno (`DRIVER=msm`, `qcom,adreno` DeviceTree
/// compatibles/modaliases — Freedreno/Turnip on Snapdragon laptops),
/// ARM Mali (`DRIVER=panfrost`/`panthor`, `arm,mali` compatibles), and
/// Raspberry Pi VideoCore (`DRIVER=v3d`/`vc4`, `brcm,*-v3d`/`vc4`
/// compatibles). Returns [`GpuVendorClass::Unknown`] when no marker
/// matches.
pub fn classify_linux_drm_uevent(uevent: &str) -> GpuVendorClass {
    for line in uevent.to_ascii_lowercase().lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "driver" => match value.trim() {
                "msm" | "msm_drm" => return GpuVendorClass::QualcommAdreno,
                "panfrost" | "panthor" => return GpuVendorClass::ArmMali,
                "v3d" | "vc4" => return GpuVendorClass::BroadcomVideocore,
                _ => {}
            },
            _ => {
                // OF_COMPATIBLE_*/OF_*/MODALIAS lines carry
                // `vendor,device` tokens like `qcom,adreno-740.0`.
                if value.contains("qcom,adreno") {
                    return GpuVendorClass::QualcommAdreno;
                }
                if value.contains("arm,mali") {
                    return GpuVendorClass::ArmMali;
                }
                if value.contains("brcm,") && (value.contains("v3d") || value.contains("vc4")) {
                    return GpuVendorClass::BroadcomVideocore;
                }
            }
        }
    }
    GpuVendorClass::Unknown
}

/// Selection rank used when a host exposes several DRM cards: dedicated
/// silicon wins over integrated so hybrid laptops tune for the fast GPU.
fn gpu_class_rank(class: GpuVendorClass) -> u8 {
    match class {
        GpuVendorClass::NvidiaDedicated
        | GpuVendorClass::AmdDedicated
        | GpuVendorClass::IntelArc => 4,
        GpuVendorClass::AppleSilicon | GpuVendorClass::QualcommAdreno => 3,
        GpuVendorClass::IntelModernIntegrated
        | GpuVendorClass::ArmMali
        | GpuVendorClass::Unknown => 2,
        GpuVendorClass::IntelLegacyIntegrated | GpuVendorClass::BroadcomVideocore => 1,
        GpuVendorClass::NoneSoftware => 0,
    }
}

// ---------------------------------------------------------------------------
// Read-only host probe
// ---------------------------------------------------------------------------

/// Probe the host for CPU/RAM/GPU facts. Read-only: opens a handful of
/// `/proc` and `/sys` files on Linux, runs `sysctl -n` on macOS and calls
/// the Win32 `GlobalMemoryStatusEx` kernel32 export on Windows — never
/// writes, never panics on missing data; every absent source degrades to
/// `0` / `Unknown` defaults.
pub fn probe_host_hardware() -> HostHardwareProfile {
    let logical_cpus = std::thread::available_parallelism()
        .map(|n| u16::try_from(n.get()).unwrap_or(u16::MAX))
        .unwrap_or(1);
    let total_ram_mib = probe_total_ram_mib();
    let l3_cache_kib = probe_l3_cache_kib();
    let (gpu_class, dedicated_vram_mib) = probe_gpu();
    let shared_system_ram = matches!(
        gpu_class,
        GpuVendorClass::IntelLegacyIntegrated
            | GpuVendorClass::IntelModernIntegrated
            | GpuVendorClass::AppleSilicon
            | GpuVendorClass::QualcommAdreno
            | GpuVendorClass::ArmMali
            | GpuVendorClass::BroadcomVideocore
    );
    // Integrated/unified GPUs have no real VRAM counter; estimate the
    // carve conservatively at a quarter of RAM capped at 4 GiB.
    let estimated_vram_mib = if shared_system_ram {
        (total_ram_mib / 4).min(4096)
    } else {
        dedicated_vram_mib
    };
    HostHardwareProfile {
        logical_cpus,
        l3_cache_kib,
        total_ram_mib,
        gpu_class,
        estimated_vram_mib,
        shared_system_ram,
    }
}

/// `MemTotal` from `/proc/meminfo`, in MiB (`0` when unavailable).
#[cfg(target_os = "linux")]
fn probe_total_ram_mib() -> u32 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| parse_meminfo_total_kib(&text))
        .map(|kib| (kib / 1024).min(u64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

/// `hw.memsize` from `sysctl -n`, in MiB (`0` when sysctl fails).
#[cfg(target_os = "macos")]
fn probe_total_ram_mib() -> u32 {
    sysctl_value("hw.memsize")
        .and_then(|text| parse_sysctl_memsize_mib(&text))
        .unwrap_or(0)
}

/// `ullTotalPhys` from Win32 `GlobalMemoryStatusEx`, in MiB (`0` when the
/// kernel32 call fails). Minimal `extern "system"` FFI — zero crates.
#[cfg(windows)]
fn probe_total_ram_mib() -> u32 {
    /// `MEMORYSTATUSEX`: the Win32 physical-RAM snapshot.
    #[repr(C)]
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

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
    }

    let mut status = MemoryStatusEx {
        dw_length: std::mem::size_of::<MemoryStatusEx>() as u32,
        dw_memory_load: 0,
        ull_total_phys: 0,
        ull_avail_phys: 0,
        ull_total_page_file: 0,
        ull_avail_page_file: 0,
        ull_total_virtual: 0,
        ull_avail_virtual: 0,
        ull_avail_extended_virtual: 0,
    };
    // Returns BOOL: nonzero on success.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return 0;
    }
    (status.ull_total_phys / (1024 * 1024)).min(u64::from(u32::MAX)) as u32
}

/// No RAM probe exists on this platform; report `0` (unknown).
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn probe_total_ram_mib() -> u32 {
    0
}

/// `sysctl -n <name>` on macOS: stdout on success, `None` on any failure.
/// One short-lived child process per call; stderr is silenced so a
/// missing key cannot spam the client's log.
#[cfg(target_os = "macos")]
fn sysctl_value(name: &str) -> Option<String> {
    let output = std::process::Command::new("sysctl")
        .arg("-n")
        .arg(name)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Total L3 cache in KiB across `cpu0`'s `index*/level == 3` sysfs nodes
/// (`0` when sysfs is absent — non-Linux hosts, containers).
#[cfg(target_os = "linux")]
fn probe_l3_cache_kib() -> u32 {
    let cache_dir = std::path::Path::new("/sys/devices/system/cpu/cpu0/cache");
    let mut total_kib: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("index") {
                continue;
            }
            let is_l3 = std::fs::read_to_string(entry.path().join("level"))
                .map(|s| s.trim() == "3")
                .unwrap_or(false);
            if !is_l3 {
                continue;
            }
            if let Some(kib) = std::fs::read_to_string(entry.path().join("size"))
                .ok()
                .and_then(|s| parse_cache_size_kib(&s))
            {
                total_kib += u64::from(kib);
            }
        }
    }
    total_kib.min(u64::from(u32::MAX)) as u32
}

/// `hw.l3cachesize` from `sysctl -n`, in KiB (`0` when sysctl fails).
#[cfg(target_os = "macos")]
fn probe_l3_cache_kib() -> u32 {
    sysctl_value("hw.l3cachesize")
        .and_then(|text| parse_sysctl_l3_cache_kib(&text))
        .unwrap_or(0)
}

/// L3 cache size is not probed on this platform (`0` = unknown).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe_l3_cache_kib() -> u32 {
    0
}

/// Walk `/sys/class/drm/card*` for the strongest GPU. Returns the class
/// plus the driver-reported dedicated VRAM total in MiB (amdgpu's
/// `mem_info_vram_total`; `0` when no driver reports it). Missing probes
/// remain `Unknown` unless a platform probe identifies an adapter.
fn probe_gpu() -> (GpuVendorClass, u32) {
    let mut class: Option<GpuVendorClass> = None;
    let mut vram_mib = 0u32;
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            // Only top-level `card<N>` nodes — connector entries like
            // `card0-HDMI-A-1` resolve to the same device.
            if !name.starts_with("card") || name.contains('-') {
                continue;
            }
            let device_dir = entry.path().join("device");
            let vendor = std::fs::read_to_string(device_dir.join("vendor"))
                .ok()
                .and_then(|s| parse_pci_id(&s));
            let device = std::fs::read_to_string(device_dir.join("device"))
                .ok()
                .and_then(|s| parse_pci_id(&s));
            let mut candidate = classify_gpu(vendor, device);
            if matches!(candidate, GpuVendorClass::Unknown) {
                // ARM64 platform/DT GPUs (Snapdragon `msm`, Mali, Pi
                // VideoCore) carry no PCI id files; the uevent's
                // DRIVER=/OF_COMPATIBLE_*/MODALIAS lines name them.
                if let Ok(uevent) = std::fs::read_to_string(device_dir.join("uevent")) {
                    let from_uevent = classify_linux_drm_uevent(&uevent);
                    if !matches!(from_uevent, GpuVendorClass::Unknown) {
                        candidate = from_uevent;
                    }
                }
            }
            if class.is_none()
                || class.is_some_and(|best| gpu_class_rank(candidate) > gpu_class_rank(best))
            {
                class = Some(candidate);
                vram_mib = std::fs::read_to_string(device_dir.join("mem_info_vram_total"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|bytes| (bytes / (1024 * 1024)).min(u64::from(u32::MAX)) as u32)
                    .unwrap_or(0);
            }
        }
    }
    // macOS exposes no `/sys/class/drm`; classify only when the CPU-brand
    // probe identifies Apple Silicon or confirms a real but unknown GPU.
    #[cfg(target_os = "macos")]
    if class.is_none() {
        class =
            sysctl_value("machdep.cpu.brand_string").map(|brand| classify_macos_cpu_brand(&brand));
    }

    #[cfg(target_os = "windows")]
    if class.is_none() {
        let (windows_class, windows_vram) = probe_windows_gpu();
        if !matches!(windows_class, GpuVendorClass::Unknown) {
            class = Some(windows_class);
            vram_mib = windows_vram;
        }
    }

    (class.unwrap_or(GpuVendorClass::Unknown), vram_mib)
}

/// Enumerate Windows display adapters using the zero-dependency Win32
/// `EnumDisplayDevicesW` API.
#[cfg(target_os = "windows")]
fn probe_windows_gpu() -> (GpuVendorClass, u32) {
    #[repr(C)]
    struct DisplayDeviceW {
        cb: u32,
        device_name: [u16; 32],
        device_string: [u16; 128],
        state_flags: u32,
        device_id: [u16; 128],
        device_key: [u16; 128],
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplayDevicesW(
            device: *const u16,
            device_num: u32,
            display_device: *mut DisplayDeviceW,
            flags: u32,
        ) -> i32;
    }

    fn wide_string(value: &[u16]) -> String {
        let length = value
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(value.len());
        String::from_utf16_lossy(&value[..length])
    }

    let mut best: Option<GpuVendorClass> = None;
    for index in 0..64 {
        let mut device = DisplayDeviceW {
            cb: std::mem::size_of::<DisplayDeviceW>() as u32,
            device_name: [0; 32],
            device_string: [0; 128],
            state_flags: 0,
            device_id: [0; 128],
            device_key: [0; 128],
        };
        if unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut device, 0) } == 0 {
            break;
        }
        let parsed = parse_windows_display_device_id(
            &wide_string(&device.device_id),
            &wide_string(&device.device_string),
        );
        if let Some((vendor, id)) = parsed {
            let candidate = classify_gpu(Some(vendor), Some(id));
            if best.is_none()
                || best.is_some_and(|current| gpu_class_rank(candidate) > gpu_class_rank(current))
            {
                best = Some(candidate);
            }
        }
    }
    (best.unwrap_or(GpuVendorClass::Unknown), 0)
}

// ---------------------------------------------------------------------------
// Auto-tuning recommendation
// ---------------------------------------------------------------------------

/// Meshing/decode worker count for `logical_cpus` host cores. Always
/// reserves at least one core for the main tick/render loop and keeps an
/// extra core free on 2-4 core laptops; capped at 16 so big servers do
/// not spawn absurd pools for a meshing workload.
fn recommend_worker_threads(logical_cpus: u16) -> u16 {
    match logical_cpus {
        0..=2 => 1,
        3..=4 => logical_cpus - 2,
        _ => (logical_cpus - 1).min(16),
    }
}

/// CPU chunk cache budget in MiB, tiered by installed RAM: tight on
/// 8 GiB-class machines so the client cannot OOM the host, widened on
/// 16+ GiB machines with a large L3 (Ryzen X3D-style) that actually feeds
/// bigger working sets, and shaved a quarter when the GPU shares system
/// RAM.
fn recommend_chunk_cache_mib(hw: &HostHardwareProfile) -> u32 {
    const MIB: u32 = 1024;
    let mut budget = match hw.total_ram_mib {
        ram if ram >= 32 * MIB => 2048,
        ram if ram >= 16 * MIB => 1024,
        ram if ram >= 8 * MIB => 512,
        ram if ram >= 4 * MIB => 256,
        _ => 128,
    };
    if hw.total_ram_mib >= 16 * MIB && hw.l3_cache_kib >= 32 * MIB {
        budget += budget / 2;
    }
    if hw.shared_system_ram {
        budget = budget * 3 / 4;
    }
    budget.clamp(64, 4096)
}

/// Map a probed (or hand-built) profile onto a [`HardwareTuningPlan`].
///
/// Backend policy: confirmed software/headless and legacy Intel iGPUs pin
/// `ClassicLowSpec`
/// (pure wgpu Vulkan 1.1 / DX12 / Metal core); NVIDIA/AMD/Intel Arc run `Auto` so
/// capability negotiation can reach the mesh-shader pipeline;
/// Apple Silicon takes `ComputeIndirect` (no `VK_EXT_mesh_shader` on
/// Metal-class hardware). Snapdragon Adreno, ARM Mali, and VideoCore are
/// real tile-based GPUs — `Auto` lets them negotiate DX12 on Windows
/// ARM64 or Vulkan (Turnip/panvk/v3dv) on Linux ARM64 rather than being
/// throttled onto the software path. Everything else negotiates `Auto`.
///
/// Budgets stay lean on low-spec/no-GPU hardware, widen on X3D-class
/// machines (32+ GiB RAM, 32+ MiB L3, dedicated GPU), and always clamp
/// down when RAM is scarce regardless of GPU class.
pub fn recommend_tuning(hw: &HostHardwareProfile) -> HardwareTuningPlan {
    const GIB_MIB: u32 = 1024;
    const KIB_PER_MIB: u32 = 1024;
    let dedicated = matches!(
        hw.gpu_class,
        GpuVendorClass::NvidiaDedicated | GpuVendorClass::AmdDedicated | GpuVendorClass::IntelArc
    );

    let gpu_backend = match hw.gpu_class {
        GpuVendorClass::NoneSoftware | GpuVendorClass::IntelLegacyIntegrated => {
            RenderBackendPreference::ClassicLowSpec
        }
        GpuVendorClass::AppleSilicon => RenderBackendPreference::ComputeIndirect,
        _ => RenderBackendPreference::Auto,
    };

    let (mut view_distance, mut lod_radius_chunks, mut max_rebuilds_per_frame) = match hw.gpu_class
    {
        GpuVendorClass::NoneSoftware => (6u8, 0u16, 2u8),
        GpuVendorClass::IntelLegacyIntegrated | GpuVendorClass::BroadcomVideocore => (8, 32, 2),
        GpuVendorClass::IntelModernIntegrated
        | GpuVendorClass::ArmMali
        | GpuVendorClass::Unknown => (10, 48, 4),
        // Snapdragon Adreno (X1-85/X1-45/7xx/6xx) is a strong tile-based
        // iGPU: a notch below Apple Silicon, well above legacy parts.
        GpuVendorClass::QualcommAdreno => (14, 64, 6),
        GpuVendorClass::AppleSilicon => (16, 64, 6),
        GpuVendorClass::NvidiaDedicated
        | GpuVendorClass::AmdDedicated
        | GpuVendorClass::IntelArc => (16, 96, 8),
    };

    // X3D-class enthusiast tier: big L3 + 32+ GiB RAM + a dedicated GPU
    // earns a wider horizon and the fullest far-terrain ring.
    if dedicated && hw.total_ram_mib >= 32 * GIB_MIB && hw.l3_cache_kib >= 32 * KIB_PER_MIB {
        view_distance = 20;
        lod_radius_chunks = 128;
        max_rebuilds_per_frame = 8;
    }

    // RAM clamps override GPU optimism: a starved host cannot feed a wide
    // window no matter how fast the card is.
    if hw.total_ram_mib < 4 * GIB_MIB {
        view_distance = view_distance.min(6);
        lod_radius_chunks = 0;
        max_rebuilds_per_frame = max_rebuilds_per_frame.min(1);
    } else if hw.total_ram_mib < 8 * GIB_MIB {
        view_distance = view_distance.min(8);
        lod_radius_chunks = lod_radius_chunks.min(32);
        max_rebuilds_per_frame = max_rebuilds_per_frame.min(2);
    }

    HardwareTuningPlan {
        gpu_backend,
        worker_threads: recommend_worker_threads(hw.logical_cpus),
        chunk_cache_budget_mib: recommend_chunk_cache_mib(hw),
        view_distance,
        lod_radius_chunks,
        max_rebuilds_per_frame,
        local_llm: recommend_llm_tier(hw),
    }
}

/// Local translation-model tier from memory headroom. `total_ram_mib`
/// thresholds follow the opt-in flow's safety contract, and a dedicated
/// GPU reporting 16+ GiB VRAM can host a large model even on a
/// RAM-lighter host. Shared-memory GPUs never qualify via the VRAM path —
/// their "VRAM" is the same RAM the game already needs.
pub fn recommend_llm_tier(hw: &HostHardwareProfile) -> LocalLlmRecommendation {
    const GIB_MIB: u32 = 1024;
    if hw.total_ram_mib >= 32 * GIB_MIB
        || (!hw.shared_system_ram && hw.estimated_vram_mib >= 16 * GIB_MIB)
    {
        LocalLlmRecommendation::LargeLocalAllowed
    } else if hw.total_ram_mib >= 16 * GIB_MIB {
        LocalLlmRecommendation::MediumQuantized8BTo12B
    } else if hw.total_ram_mib >= 8 * GIB_MIB {
        LocalLlmRecommendation::SmallQuantized2BTo4B
    } else {
        LocalLlmRecommendation::TooLowRam
    }
}
