// SPDX-License-Identifier: LGPL-3.0-or-later
//! Hardware auto-tuner + `background_fps_limit` tests.
//!
//! Every test is deterministic: the parsers and [`recommend_tuning`] take
//! text/profiles directly, so no assertion depends on the CI machine. The
//! only live-host touch is [`probe_host_hardware`]'s smoke check, which
//! asserts sanity (`logical_cpus >= 1`, Linux RAM reported) and that the
//! probe never panics.

use std::path::PathBuf;

use megu_client::cli::parse_args;
use megu_client::hardware_profile::{
    classify_gpu, classify_intel_device, classify_linux_drm_uevent, classify_macos_cpu_brand,
    parse_cache_size_kib, parse_meminfo_total_kib, parse_pci_id, parse_sysctl_l3_cache_kib,
    parse_sysctl_memsize_mib, parse_windows_display_device_id, probe_host_hardware,
    recommend_tuning, GpuVendorClass, HostHardwareProfile, LocalLlmRecommendation,
};
use megu_client::render::RenderConfig;
use megu_client::settings::{
    ClientSettings, DistantHorizon, QualityPreset, DEFAULT_BACKGROUND_FPS_LIMIT, MAX_BACKGROUND_FPS,
};
use megu_client_mesh::gpu_pipeline::RenderBackendPreference;

fn profile(
    logical_cpus: u16,
    l3_cache_kib: u32,
    total_ram_mib: u32,
    gpu_class: GpuVendorClass,
    estimated_vram_mib: u32,
) -> HostHardwareProfile {
    HostHardwareProfile {
        logical_cpus,
        l3_cache_kib,
        total_ram_mib,
        gpu_class,
        estimated_vram_mib,
        shared_system_ram: matches!(
            gpu_class,
            GpuVendorClass::IntelLegacyIntegrated
                | GpuVendorClass::IntelModernIntegrated
                | GpuVendorClass::AppleSilicon
                | GpuVendorClass::QualcommAdreno
                | GpuVendorClass::ArmMali
                | GpuVendorClass::BroadcomVideocore
        ),
    }
}

/// A scratch directory guaranteed not to contain a `megu-client.toml`
/// auto-load file (the resolver only reads one when the file exists).
fn empty_scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("megu-hw-tuning-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Pure parser tests
// ---------------------------------------------------------------------------

#[test]
fn meminfo_parser_extracts_memtotal() {
    let text = "\
MemTotal:       31937784 kB
MemFree:        12345678 kB
MemAvailable:   23456789 kB
Buffers:          123456 kB
";
    assert_eq!(parse_meminfo_total_kib(text), Some(31_937_784));

    // Minimal file with just the key.
    assert_eq!(parse_meminfo_total_kib("MemTotal: 4096 kB\n"), Some(4096));
    // Missing key -> None.
    assert_eq!(parse_meminfo_total_kib("MemFree: 1 kB\n"), None);
    // Non-numeric value -> None.
    assert_eq!(parse_meminfo_total_kib("MemTotal: banana kB\n"), None);
    // A surprise unit is rejected rather than mis-scaled.
    assert_eq!(parse_meminfo_total_kib("MemTotal: 4096 MB\n"), None);
    // Empty input -> None.
    assert_eq!(parse_meminfo_total_kib(""), None);
}

#[test]
fn cache_size_parser_handles_k_m_g_and_bare_kib() {
    assert_eq!(parse_cache_size_kib("98304K"), Some(98_304));
    assert_eq!(parse_cache_size_kib("96M"), Some(96 * 1024));
    assert_eq!(parse_cache_size_kib("6144K"), Some(6144));
    assert_eq!(parse_cache_size_kib("512K\n"), Some(512));
    assert_eq!(parse_cache_size_kib("1G"), Some(1024 * 1024));
    // Bare numbers are already KiB.
    assert_eq!(parse_cache_size_kib("1024"), Some(1024));
    // Case-insensitive suffixes, surrounding whitespace tolerated.
    assert_eq!(parse_cache_size_kib("  32m  \n"), Some(32 * 1024));
    // Junk -> None.
    assert_eq!(parse_cache_size_kib(""), None);
    assert_eq!(parse_cache_size_kib("K"), None);
    assert_eq!(parse_cache_size_kib("abcK"), None);
    // Absurd magnitudes saturate at u32::MAX instead of overflowing u64.
    assert_eq!(
        parse_cache_size_kib("18446744073709551615G"),
        Some(u32::MAX)
    );
}

#[test]
fn pci_id_parser_and_gpu_classification() {
    assert_eq!(parse_pci_id("0x10de\n"), Some(0x10de));
    assert_eq!(parse_pci_id("0x1002"), Some(0x1002));
    assert_eq!(parse_pci_id("0x8086"), Some(0x8086));
    assert_eq!(parse_pci_id("0x56a0"), Some(0x56a0));
    // Missing 0x prefix or junk -> None.
    assert_eq!(parse_pci_id("10de"), None);
    assert_eq!(parse_pci_id(""), None);
    assert_eq!(parse_pci_id("0xzzzz"), None);

    // Vendor ids -> classes.
    assert_eq!(
        classify_gpu(Some(0x10de), Some(0x2684)),
        GpuVendorClass::NvidiaDedicated
    );
    assert_eq!(
        classify_gpu(Some(0x1002), Some(0x744c)),
        GpuVendorClass::AmdDedicated
    );
    // Intel refines through the device id.
    assert_eq!(
        classify_gpu(Some(0x8086), Some(0x0166)),
        GpuVendorClass::IntelLegacyIntegrated
    );
    assert_eq!(
        classify_gpu(Some(0x8086), Some(0x9a49)),
        GpuVendorClass::IntelModernIntegrated
    );
    assert_eq!(
        classify_gpu(Some(0x8086), Some(0x56a0)),
        GpuVendorClass::IntelArc
    );
    // Vendor present but device file unreadable -> assume modern Intel.
    assert_eq!(
        classify_gpu(Some(0x8086), None),
        GpuVendorClass::IntelModernIntegrated
    );
    // Unknown vendor and unidentified probe both retain the neutral Auto path.
    assert_eq!(
        classify_gpu(Some(0x1234), Some(0x5678)),
        GpuVendorClass::Unknown
    );
    assert_eq!(classify_gpu(None, None), GpuVendorClass::Unknown);
    assert_eq!(
        classify_gpu(Some(0x1414), Some(0)),
        GpuVendorClass::NoneSoftware
    );
}

#[test]
fn windows_display_device_parser_classifies_vendor_and_software_adapters() {
    assert_eq!(
        parse_windows_display_device_id(
            r"PCI\VEN_10DE&DEV_2684&SUBSYS_12345678&REV_A1",
            "NVIDIA GeForce RTX 4090",
        ),
        Some((0x10de, 0x2684))
    );
    assert_eq!(
        parse_windows_display_device_id(
            r"PCI\VEN_1002&DEV_747E&SUBSYS_12345678",
            "AMD Radeon RX 7900 XTX",
        ),
        Some((0x1002, 0x747e))
    );
    assert_eq!(
        parse_windows_display_device_id(r"PCI\VEN_8086&DEV_56A0&SUBSYS_12345678", "Intel Arc A770",),
        Some((0x8086, 0x56a0))
    );
    assert_eq!(
        parse_windows_display_device_id("", "NVIDIA Adapter"),
        Some((0x10de, 0))
    );
    assert_eq!(
        parse_windows_display_device_id("", "AMD Radeon Adapter"),
        Some((0x1002, 0))
    );
    assert_eq!(
        parse_windows_display_device_id("", "Intel Graphics"),
        Some((0x8086, 0))
    );
    assert_eq!(
        parse_windows_display_device_id("ROOT\\BASICRENDER", "Microsoft Basic Render Driver"),
        Some((0x1414, 0))
    );
    assert_eq!(
        parse_windows_display_device_id(r"PCI\VEN_1414&DEV_008C", "Microsoft adapter"),
        Some((0x1414, 0x008c))
    );
    assert_eq!(
        parse_windows_display_device_id("", "llvmpipe (LLVM 18.1.8, 256 bits)"),
        Some((0x1414, 0))
    );
    assert_eq!(
        parse_windows_display_device_id("", "lavapipe"),
        Some((0x1414, 0))
    );
    assert_eq!(
        parse_windows_display_device_id("", "SwiftShader Device"),
        Some((0x1414, 0))
    );
    assert_eq!(
        parse_windows_display_device_id("ROOT\\UNKNOWN", "Unknown"),
        None
    );
}

#[test]
fn windows_display_device_parser_classifies_qualcomm_adreno() {
    // Snapdragon X Elite: ACPI id + adapter name.
    assert_eq!(
        parse_windows_display_device_id(
            r"ACPI\VEN_QCOM&DEV_0D0A",
            "Qualcomm(R) Adreno(TM) X1-85 GPU",
        ),
        Some((0x5143, 0x0d0a))
    );
    // Snapdragon X Plus / 8cx-class ACPI ids.
    assert_eq!(
        parse_windows_display_device_id(
            r"ACPI\VEN_QCOM&DEV_363F&SUBSYS_12345678",
            "Qualcomm(R) Adreno(TM) X1-45 GPU",
        ),
        Some((0x5143, 0x363f))
    );
    // Qualcomm PCI id form.
    assert_eq!(
        parse_windows_display_device_id(
            r"PCI\VEN_5143&DEV_0001&SUBSYS_00010000",
            "Qualcomm(R) Adreno(TM) 690 GPU",
        ),
        Some((0x5143, 0x0001))
    );
    // The 4D4F4351 ("QCOM" little-endian) ACPI vendor encoding.
    assert_eq!(
        parse_windows_display_device_id(r"ACPI\VEN_4D4F4351&DEV_0001", "Adreno"),
        Some((0x5143, 0x0001))
    );
    // Compact ACPI PNP id without VEN_/DEV_ separators.
    assert_eq!(
        parse_windows_display_device_id(r"ACPI\QCOM0D0A", "Qualcomm Adreno"),
        Some((0x5143, 0x0d0a))
    );
    // Adapter name alone still identifies the vendor.
    for adapter_name in [
        "Qualcomm(R) Adreno(TM) X1-85 GPU",
        "Qualcomm(R) Adreno(TM) 690 GPU",
        "Adreno 750",
        "Snapdragon X Elite",
    ] {
        assert_eq!(
            parse_windows_display_device_id("", adapter_name),
            Some((0x5143, 0)),
            "{adapter_name}"
        );
    }
    // Every Qualcomm id lands on the Adreno class — never Unknown or
    // NoneSoftware.
    for (device_id, name) in [
        (
            r"ACPI\VEN_QCOM&DEV_0D0A",
            "Qualcomm(R) Adreno(TM) X1-85 GPU",
        ),
        (r"PCI\VEN_5143&DEV_0001", "Qualcomm(R) Adreno(TM) 690 GPU"),
        (r"ACPI\QCOM363F", "Qualcomm(R) Adreno(TM) X1-45 GPU"),
        ("", "Qualcomm(R) Adreno(TM) X1-85 GPU"),
    ] {
        let (vendor, device) = parse_windows_display_device_id(device_id, name)
            .unwrap_or_else(|| panic!("unparsed: {device_id} / {name}"));
        assert_eq!(
            classify_gpu(Some(vendor), Some(device)),
            GpuVendorClass::QualcommAdreno,
            "{device_id} / {name}"
        );
    }
}

#[test]
fn linux_drm_uevent_classifies_arm64_platform_gpus() {
    // Snapdragon X Elite laptop: platform/DT device, no PCI ids. The
    // DRIVER= line alone is decisive; compatibles/modalias agree.
    let snapdragon_uevent = "\
DRIVER=msm
OF_NAME=gpu
OF_FULLNAME=/soc@0/display-subsystem@ae00000/gpu@3d00000
OF_COMPATIBLE_0=qcom,adreno-740.0
OF_COMPATIBLE_1=qcom,adreno
MODALIAS=of:NgpuT(NULL)Cqcom,adreno-740.0Cqcom,adreno
";
    assert_eq!(
        classify_linux_drm_uevent(snapdragon_uevent),
        GpuVendorClass::QualcommAdreno
    );
    // DRIVER=msm alone is enough (no compatibles bound yet).
    assert_eq!(
        classify_linux_drm_uevent("DRIVER=msm\n"),
        GpuVendorClass::QualcommAdreno
    );
    // A modalias-only uevent still identifies the Adreno GPU.
    assert_eq!(
        classify_linux_drm_uevent("MODALIAS=of:NgpuT(NULL)Cqcom,adreno\n"),
        GpuVendorClass::QualcommAdreno
    );
    // ARM Mali via panthor (newer) and panfrost (older) drivers.
    assert_eq!(
        classify_linux_drm_uevent(
            "DRIVER=panthor\nOF_COMPATIBLE_0=arm,mali-g610\nOF_COMPATIBLE_1=arm,mali\n"
        ),
        GpuVendorClass::ArmMali
    );
    assert_eq!(
        classify_linux_drm_uevent("DRIVER=panfrost\n"),
        GpuVendorClass::ArmMali
    );
    assert_eq!(
        classify_linux_drm_uevent("MODALIAS=of:NgpuT(NULL)Carm,mali-g52\n"),
        GpuVendorClass::ArmMali
    );
    // Raspberry Pi VideoCore via v3d (Pi 4/5 3D block) and vc4.
    assert_eq!(
        classify_linux_drm_uevent(
            "DRIVER=v3d\nOF_COMPATIBLE_0=brcm,2712-v3d\nMODALIAS=of:NgpuT(NULL)Cbrcm,2712-v3d\n"
        ),
        GpuVendorClass::BroadcomVideocore
    );
    assert_eq!(
        classify_linux_drm_uevent("DRIVER=vc4\n"),
        GpuVendorClass::BroadcomVideocore
    );
    // Unrelated drivers and empty input stay Unknown (the PCI path owns
    // amdgpu/nouveau/i915 hosts).
    for text in [
        "DRIVER=amdgpu\nPCI_ID=1002:744C\n",
        "DRIVER=i915\n",
        "DRIVER=nouveau\n",
        "DRIVER=\n",
        "",
        "no equals signs at all\n",
    ] {
        assert_eq!(
            classify_linux_drm_uevent(text),
            GpuVendorClass::Unknown,
            "{text:?}"
        );
    }
}

#[test]
fn sysctl_parsers_handle_macos_byte_counts() {
    // `sysctl -n hw.memsize` prints physical RAM in bytes, decimal.
    assert_eq!(parse_sysctl_memsize_mib("17179869184\n"), Some(16384));
    assert_eq!(parse_sysctl_memsize_mib("  8589934592  "), Some(8192));
    assert_eq!(parse_sysctl_memsize_mib("34359738368"), Some(32768));
    // Sub-MiB values floor to 0; junk and empty input are None.
    assert_eq!(parse_sysctl_memsize_mib("512"), Some(0));
    assert_eq!(parse_sysctl_memsize_mib(""), None);
    assert_eq!(parse_sysctl_memsize_mib("not-a-number"), None);
    assert_eq!(parse_sysctl_memsize_mib("16 GiB"), None);

    // `sysctl -n hw.l3cachesize` prints bytes too -> KiB.
    assert_eq!(parse_sysctl_l3_cache_kib("16777216\n"), Some(16384));
    assert_eq!(parse_sysctl_l3_cache_kib("9437184"), Some(9216));
    assert_eq!(parse_sysctl_l3_cache_kib(""), None);
    assert_eq!(parse_sysctl_l3_cache_kib("abc"), None);
}

#[test]
fn macos_cpu_brand_classification_detects_apple_silicon() {
    // Every shipping Apple Silicon part reports `Apple M<n>[ suffix]`.
    for brand in [
        "Apple M1",
        "Apple M1 Pro",
        "Apple M1 Max",
        "Apple M1 Ultra",
        "Apple M2",
        "Apple M2 Pro",
        "Apple M2 Max",
        "Apple M3",
        "Apple M3 Pro",
        "Apple M3 Max",
        "Apple M4",
        "Apple M4 Pro",
        "Apple M4 Max",
    ] {
        assert_eq!(
            classify_macos_cpu_brand(brand),
            GpuVendorClass::AppleSilicon,
            "{brand}"
        );
    }
    // Case and whitespace tolerance.
    assert_eq!(
        classify_macos_cpu_brand("  apple   m4  "),
        GpuVendorClass::AppleSilicon
    );
    // Intel Macs and unparseable output degrade to Unknown — a real GPU
    // exists, the CPU brand simply cannot name its vendor — never
    // AppleSilicon and never NoneSoftware.
    for brand in [
        "Intel(R) Core(TM) i9-9980HK CPU @ 2.40GHz",
        "Intel(R) Xeon(R) CPU E5-1650 v4 @ 3.60GHz",
        "",
        "   ",
        "not a cpu",
        "Apple",
        "Apple M",
        "M1",
    ] {
        assert_eq!(
            classify_macos_cpu_brand(brand),
            GpuVendorClass::Unknown,
            "{brand:?}"
        );
    }
}

#[test]
fn intel_device_classification_boundaries() {
    // Arc Alchemist/DG2 block and Battlemage block.
    assert_eq!(classify_intel_device(0x56a0), GpuVendorClass::IntelArc);
    assert_eq!(classify_intel_device(0x56b1), GpuVendorClass::IntelArc);
    assert_eq!(classify_intel_device(0xe202), GpuVendorClass::IntelArc);
    // Gen12+ Xe integrated (Tiger Lake, DG1, Alder/Raptor/Meteor Lake).
    assert_eq!(
        classify_intel_device(0x9a49),
        GpuVendorClass::IntelModernIntegrated
    );
    assert_eq!(
        classify_intel_device(0x4905),
        GpuVendorClass::IntelModernIntegrated
    );
    assert_eq!(
        classify_intel_device(0x4680),
        GpuVendorClass::IntelModernIntegrated
    );
    assert_eq!(
        classify_intel_device(0x7d55),
        GpuVendorClass::IntelModernIntegrated
    );
    // Gen7-Gen11 legacy (HD 4000, HD 530, UHD 620, Ice Lake Gen11).
    assert_eq!(
        classify_intel_device(0x0166),
        GpuVendorClass::IntelLegacyIntegrated
    );
    assert_eq!(
        classify_intel_device(0x1912),
        GpuVendorClass::IntelLegacyIntegrated
    );
    assert_eq!(
        classify_intel_device(0x3e92),
        GpuVendorClass::IntelLegacyIntegrated
    );
    assert_eq!(
        classify_intel_device(0x8a52),
        GpuVendorClass::IntelLegacyIntegrated
    );
}

// ---------------------------------------------------------------------------
// Tuning matrix: five realistic machines
// ---------------------------------------------------------------------------

#[test]
fn tuning_no_gpu_4gb_netbook_stays_lean() {
    // (a) No-GPU 4 GiB 2-core netbook.
    let hw = profile(2, 4 * 1024, 4 * 1024, GpuVendorClass::NoneSoftware, 0);
    let plan = recommend_tuning(&hw);
    assert_eq!(plan.gpu_backend, RenderBackendPreference::ClassicLowSpec);
    assert_eq!(plan.worker_threads, 1, "2-core netbook keeps 1 worker");
    assert!(plan.chunk_cache_budget_mib <= 512);
    assert_eq!(plan.chunk_cache_budget_mib, 256);
    assert!(plan.view_distance <= 8);
    assert_eq!(plan.lod_radius_chunks, 0);
    assert!(plan.max_rebuilds_per_frame <= 2);
    assert_eq!(plan.local_llm, LocalLlmRecommendation::TooLowRam);
}

#[test]
fn tuning_legacy_intel_laptop_uses_low_spec_pipeline() {
    // (b) Legacy Intel HD 4000, 8 GiB, 4-core laptop.
    let hw = profile(
        4,
        3 * 1024,
        8 * 1024,
        GpuVendorClass::IntelLegacyIntegrated,
        2 * 1024,
    );
    let plan = recommend_tuning(&hw);
    assert_eq!(plan.gpu_backend, RenderBackendPreference::ClassicLowSpec);
    assert_eq!(plan.worker_threads, 2, "4-core laptop keeps OS headroom");
    // Shared-memory GPU shaves the RAM tier a quarter (512 -> 384).
    assert_eq!(plan.chunk_cache_budget_mib, 384);
    assert!(plan.view_distance <= 8);
    assert_eq!(plan.local_llm, LocalLlmRecommendation::SmallQuantized2BTo4B);
}

#[test]
fn tuning_midrange_dedicated_gpu_balances() {
    // (c) Modern 16 GiB, 8-core, NVIDIA/AMD mid-range with 8 GiB VRAM.
    for gpu in [
        GpuVendorClass::NvidiaDedicated,
        GpuVendorClass::AmdDedicated,
        GpuVendorClass::IntelArc,
    ] {
        let hw = profile(8, 16 * 1024, 16 * 1024, gpu, 8 * 1024);
        let plan = recommend_tuning(&hw);
        assert_eq!(
            plan.gpu_backend,
            RenderBackendPreference::Auto,
            "{gpu:?} negotiates mesh-shader via Auto"
        );
        assert_eq!(plan.worker_threads, 7, "8 cores -> main loop + 7 workers");
        assert_eq!(plan.chunk_cache_budget_mib, 1024);
        assert_eq!(plan.view_distance, 16);
        assert_eq!(plan.lod_radius_chunks, 96);
        assert_eq!(plan.max_rebuilds_per_frame, 8);
        // 16 GiB RAM + only 8 GiB VRAM -> medium tier.
        assert_eq!(
            plan.local_llm,
            LocalLlmRecommendation::MediumQuantized8BTo12B
        );
    }
}

#[test]
fn tuning_x3d_enthusiast_widens_budgets() {
    // (d) 64 GiB Ryzen X3D (96 MiB L3) + RTX-class GPU, 16 GiB VRAM, 32 threads.
    let hw = profile(
        32,
        96 * 1024,
        64 * 1024,
        GpuVendorClass::NvidiaDedicated,
        16 * 1024,
    );
    let plan = recommend_tuning(&hw);
    assert_eq!(plan.gpu_backend, RenderBackendPreference::Auto);
    assert_eq!(plan.worker_threads, 16, "worker pool caps at 16");
    // Base 2048 + 50% for 32+ GiB RAM with 32+ MiB L3.
    assert_eq!(plan.chunk_cache_budget_mib, 3072);
    assert_eq!(plan.view_distance, 20);
    assert_eq!(plan.lod_radius_chunks, 128);
    assert_eq!(plan.max_rebuilds_per_frame, 8);
    // Both the RAM and VRAM routes allow large local models.
    assert_eq!(plan.local_llm, LocalLlmRecommendation::LargeLocalAllowed);
}

#[test]
fn tuning_apple_silicon_prefers_compute_indirect() {
    // (e) Apple Silicon, 16 GiB unified memory, 8-core.
    let hw = profile(
        8,
        24 * 1024,
        16 * 1024,
        GpuVendorClass::AppleSilicon,
        4 * 1024,
    );
    assert!(hw.shared_system_ram);
    let plan = recommend_tuning(&hw);
    assert_eq!(plan.gpu_backend, RenderBackendPreference::ComputeIndirect);
    assert_eq!(plan.worker_threads, 7);
    // 16 GiB tier (1024) shaved a quarter for unified memory.
    assert_eq!(plan.chunk_cache_budget_mib, 768);
    assert_eq!(
        plan.local_llm,
        LocalLlmRecommendation::MediumQuantized8BTo12B
    );
}

#[test]
fn tuning_unknown_gpu_keeps_auto_backend_and_balanced_distance() {
    assert_eq!(
        HostHardwareProfile::default().gpu_class,
        GpuVendorClass::Unknown
    );
    let plan = recommend_tuning(&profile(
        8,
        16 * 1024,
        16 * 1024,
        GpuVendorClass::Unknown,
        0,
    ));
    assert_eq!(plan.gpu_backend, RenderBackendPreference::Auto);
    assert_eq!(plan.view_distance, 10);
    assert_eq!(plan.lod_radius_chunks, 48);
    assert_eq!(plan.max_rebuilds_per_frame, 4);
}

#[test]
fn tuning_snapdragon_adreno_gets_strong_balanced_plan() {
    // Snapdragon X Elite: 12-core, 32 GiB unified memory, Adreno X1-85.
    let hw = profile(
        12,
        8 * 1024,
        32 * 1024,
        GpuVendorClass::QualcommAdreno,
        8 * 1024,
    );
    assert!(hw.shared_system_ram);
    let plan = recommend_tuning(&hw);
    // A real tile-based GPU — DX12 on Windows ARM64, Turnip Vulkan on
    // Linux ARM64 — negotiated through Auto, never the software path.
    assert_eq!(plan.gpu_backend, RenderBackendPreference::Auto);
    assert_eq!(plan.worker_threads, 11);
    // 32 GiB tier (2048) shaved a quarter for shared memory; the small
    // L3 blocks the X3D bonus.
    assert_eq!(plan.chunk_cache_budget_mib, 1536);
    assert_eq!(plan.view_distance, 14);
    assert_eq!(plan.lod_radius_chunks, 64);
    assert_eq!(plan.max_rebuilds_per_frame, 6);
    assert_eq!(plan.local_llm, LocalLlmRecommendation::LargeLocalAllowed);
}

#[test]
fn tuning_mobile_tile_gpus_stay_on_real_gpu_path() {
    // ARM Mali and VideoCore are real integrated/tile GPUs: they keep
    // the Auto backend and integrated budgets — never the software tier.
    for (gpu, view_distance) in [
        (GpuVendorClass::ArmMali, 10u8),
        (GpuVendorClass::BroadcomVideocore, 8),
    ] {
        let hw = profile(8, 8 * 1024, 16 * 1024, gpu, 4 * 1024);
        assert!(hw.shared_system_ram, "{gpu:?}");
        let plan = recommend_tuning(&hw);
        assert_eq!(plan.gpu_backend, RenderBackendPreference::Auto, "{gpu:?}");
        assert_eq!(plan.view_distance, view_distance, "{gpu:?}");
        assert!(plan.lod_radius_chunks > 0, "{gpu:?}");
        assert!(plan.max_rebuilds_per_frame >= 2, "{gpu:?}");
        // 16 GiB tier (1024) shaved a quarter for shared memory.
        assert_eq!(plan.chunk_cache_budget_mib, 768, "{gpu:?}");
    }
}

#[test]
fn llm_tier_boundaries_and_vram_override() {
    let at = |ram_mib, vram_mib, gpu| {
        recommend_tuning(&profile(8, 8 * 1024, ram_mib, gpu, vram_mib)).local_llm
    };
    let nvidia = GpuVendorClass::NvidiaDedicated;
    // RAM tiers.
    assert_eq!(at(7 * 1024, 0, nvidia), LocalLlmRecommendation::TooLowRam);
    assert_eq!(
        at(8 * 1024, 0, nvidia),
        LocalLlmRecommendation::SmallQuantized2BTo4B
    );
    assert_eq!(
        at(16 * 1024, 0, nvidia),
        LocalLlmRecommendation::MediumQuantized8BTo12B
    );
    assert_eq!(
        at(32 * 1024, 0, nvidia),
        LocalLlmRecommendation::LargeLocalAllowed
    );
    // 16+ GiB dedicated VRAM upgrades even a RAM-lighter host.
    assert_eq!(
        at(12 * 1024, 16 * 1024, nvidia),
        LocalLlmRecommendation::LargeLocalAllowed
    );
    // ...but the same VRAM figure on a shared-memory GPU does not count.
    assert_eq!(
        at(16 * 1024, 20 * 1024, GpuVendorClass::AppleSilicon),
        LocalLlmRecommendation::MediumQuantized8BTo12B
    );
}

#[test]
fn worker_threads_always_reserve_a_core() {
    let plan_for = |cpus| {
        recommend_tuning(&profile(cpus, 0, 16 * 1024, GpuVendorClass::Unknown, 0)).worker_threads
    };
    assert_eq!(plan_for(1), 1);
    assert_eq!(plan_for(2), 1);
    assert_eq!(plan_for(4), 2);
    assert_eq!(plan_for(8), 7);
    assert_eq!(plan_for(16), 15);
    assert_eq!(plan_for(64), 16);
    // Every recommendation leaves at least one core for the main loop.
    for cpus in [1u16, 2, 4, 8, 16, 32, 64, 128] {
        assert!(plan_for(cpus) < cpus.max(2));
    }
}

#[test]
fn tuning_plan_seeds_settings_then_user_overrides_win() {
    let hw = profile(
        8,
        16 * 1024,
        16 * 1024,
        GpuVendorClass::NvidiaDedicated,
        8 * 1024,
    );
    let plan = recommend_tuning(&hw);

    // The plan seeds a base settings profile.
    let mut settings = ClientSettings::default();
    plan.apply_to(&mut settings);
    assert_eq!(settings.gpu_backend, RenderBackendPreference::Auto);
    assert_eq!(settings.view_distance, 16);
    assert_eq!(settings.distant_horizon, DistantHorizon::Far);
    assert_eq!(settings.preset, QualityPreset::Custom);

    // TOML keys defined afterward win over the plan.
    settings.apply_toml("view_distance = 22\ngpu_backend = \"classic-low-spec\"\n");
    assert_eq!(settings.view_distance, 22);
    assert_eq!(
        settings.gpu_backend,
        RenderBackendPreference::ClassicLowSpec
    );
    // Keys the file does not define keep the plan's value.
    assert_eq!(settings.distant_horizon, DistantHorizon::Far);
}

// ---------------------------------------------------------------------------
// background_fps_limit: TOML, CLI, render wiring
// ---------------------------------------------------------------------------

#[test]
fn background_fps_toml_roundtrip_clamp_and_default() {
    // Lossless roundtrip of a non-default value.
    let s = ClientSettings {
        background_fps_limit: 45,
        ..ClientSettings::default()
    };
    let (back, warnings) = ClientSettings::from_toml(&s.to_toml());
    assert_eq!(back, s);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // Files written before the key existed stay valid and default to 15.
    let (s, warnings) = ClientSettings::from_toml("view_distance = 20\n");
    assert_eq!(s.background_fps_limit, DEFAULT_BACKGROUND_FPS_LIMIT);
    assert_eq!(s.background_fps_limit, 15);
    assert!(warnings.is_empty());

    // Out-of-range values clamp with a warning.
    let (s, warnings) = ClientSettings::from_toml("background_fps_limit = 999\n");
    assert_eq!(s.background_fps_limit, MAX_BACKGROUND_FPS);
    assert!(warnings.iter().any(|w| w.contains("background_fps_limit")));
    // Negative clamps to 0 (disabled).
    let (s, _) = ClientSettings::from_toml("background_fps_limit = -5\n");
    assert_eq!(s.background_fps_limit, 0);
    // Non-numeric value warns and keeps the default.
    let (s, warnings) = ClientSettings::from_toml("background_fps_limit = \"auto\"\n");
    assert_eq!(s.background_fps_limit, DEFAULT_BACKGROUND_FPS_LIMIT);
    assert!(warnings.iter().any(|w| w.contains("background_fps_limit")));

    // apply_toml overlays only the keys a file defines.
    let mut s = ClientSettings {
        background_fps_limit: 99,
        ..ClientSettings::default()
    };
    s.apply_toml("view_distance = 20\n");
    assert_eq!(s.background_fps_limit, 99, "unset key must survive overlay");
    s.apply_toml("background_fps_limit = 30\n");
    assert_eq!(s.background_fps_limit, 30);
}

#[test]
fn background_fps_cli_override_and_fail_closed() {
    let dir = empty_scratch_dir("bg-fps");

    // `--background-fps N` overrides TOML/preset through resolution.
    let mut cfg = parse_args(&["--background-fps", "30"]).unwrap();
    assert_eq!(cfg.background_fps, Some(30));
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.background_fps_limit, 30);

    // Inline `--background-fps=N` form works too.
    let mut cfg = parse_args(&["--background-fps=60"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.background_fps_limit, 60);

    // `0` = explicitly disabled.
    let mut cfg = parse_args(&["--background-fps", "0"]).unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.background_fps_limit, 0);

    // Invalid values fail closed with an error, never a silent clamp.
    assert!(parse_args(&["--background-fps", "abc"]).is_err());
    assert!(parse_args(&["--background-fps", "241"]).is_err());
    assert!(parse_args(&["--background-fps", "-1"]).is_err());
    // Missing value is an error too.
    assert!(parse_args(&["--background-fps"]).is_err());

    // CLI beats the settings file.
    let toml_path = dir.join("custom.toml");
    std::fs::write(&toml_path, "background_fps_limit = 45\n").unwrap();
    let mut cfg = parse_args(&[
        "--settings",
        toml_path.to_str().unwrap(),
        "--background-fps",
        "10",
    ])
    .unwrap();
    cfg.resolve_client_settings_from(&dir).unwrap();
    assert_eq!(cfg.client_settings.background_fps_limit, 10);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn render_config_focus_gated_fps_cap_and_rebuilds() {
    let mut config = RenderConfig {
        max_fps: Some(144),
        background_fps_limit: 15,
        ..RenderConfig::default()
    };

    // Focused: the normal foreground cap applies.
    assert_eq!(config.effective_fps_cap(true), 144);
    assert_eq!(config.effective_rebuilds_per_frame(true), usize::MAX);

    // Unfocused with throttling enabled: background cap + 1 rebuild/frame.
    assert_eq!(config.effective_fps_cap(false), 15);
    assert_eq!(config.effective_rebuilds_per_frame(false), 1);

    // `0` disables background throttling entirely.
    config.background_fps_limit = 0;
    assert_eq!(config.effective_fps_cap(false), 144);
    assert_eq!(config.effective_rebuilds_per_frame(false), usize::MAX);

    // Uncapped foreground (`None`) reports 0, same convention.
    config.max_fps = None;
    assert_eq!(config.effective_fps_cap(true), 0);
    config.background_fps_limit = 20;
    assert_eq!(config.effective_fps_cap(false), 20);

    // The cap flows settings -> RenderConfig unchanged.
    let settings = ClientSettings {
        background_fps_limit: 45,
        ..ClientSettings::default()
    };
    let config = RenderConfig::from_settings(&settings);
    assert_eq!(config.background_fps_limit, 45);
    assert_eq!(config.effective_fps_cap(false), 45);
}

// ---------------------------------------------------------------------------
// Live probe smoke check (the only host-dependent test)
// ---------------------------------------------------------------------------

#[test]
fn live_probe_returns_sane_profile_without_panicking() {
    let hw = probe_host_hardware();
    assert!(hw.logical_cpus >= 1, "must report at least one CPU");
    // The recommendation must never panic regardless of what was probed.
    let plan = recommend_tuning(&hw);
    assert!(plan.worker_threads >= 1);
    assert!(plan.worker_threads <= hw.logical_cpus.max(1));
    assert!(plan.chunk_cache_budget_mib >= 64);

    #[cfg(target_os = "linux")]
    {
        assert!(hw.total_ram_mib > 0, "Linux probe must read /proc/meminfo");
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Non-Linux hosts degrade gracefully; only the no-panic contract
        // above is asserted.
        let _ = hw.total_ram_mib;
    }
}
