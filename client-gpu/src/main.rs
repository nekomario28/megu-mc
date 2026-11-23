// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu-client-gpu` binary: `--headless-probe` adapter report, or the
//! real winit + wgpu window.

use std::process::ExitCode;

use megu_client_gpu::renderer::{format_adapter_report, probe_adapters};
use megu_client_gpu::{gpu_help_text, parse_gpu_args, window, CLIENT_GPU_NAME};
use megu_client_mesh::RenderBackendPreference;

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse_gpu_args(&raw_args) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{CLIENT_GPU_NAME}: {err}");
            return ExitCode::from(2);
        }
    };

    if cfg.show_help {
        print!("{}", gpu_help_text());
        return ExitCode::SUCCESS;
    }
    if cfg.show_version {
        println!("{CLIENT_GPU_NAME} {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if cfg.headless_probe {
        return headless_probe(cfg.gpu_backend);
    }

    let crash_dir = cfg
        .game_dir
        .as_deref()
        .map(|dir| std::path::Path::new(dir).join("crash-reports"))
        .unwrap_or_else(|| std::path::PathBuf::from("crash-reports"));
    let mut env = megu_client::crash_reporter::ReportEnv::detect();
    env.mc_version = cfg.mc_version.clone();
    env.engine_version = format!("{CLIENT_GPU_NAME} {}", env!("CARGO_PKG_VERSION"));
    env.subsystem = Some("client-gpu".to_string());
    megu_client::crash_reporter::install_panic_hook(crash_dir, env);

    if let Some((host, port)) = &cfg.connect {
        if !cfg.offline_demo {
            println!(
                "{CLIENT_GPU_NAME}: connecting to {host}:{port} as {} ({})",
                cfg.username, cfg.mc_version
            );
        }
    }

    match window::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{CLIENT_GPU_NAME}: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `--headless-probe`: enumerate adapters, print the negotiated pipeline for
/// each, exit 0 even with zero adapters (headless machines probe cleanly).
fn headless_probe(pref: RenderBackendPreference) -> ExitCode {
    println!(
        "{CLIENT_GPU_NAME}: probing wgpu adapters (backend preference: {})",
        pref.as_str()
    );
    let reports = probe_adapters(pref);
    if reports.is_empty() {
        println!("{CLIENT_GPU_NAME}: no GPU adapters found — headless environment is OK");
        return ExitCode::SUCCESS;
    }
    for (i, report) in reports.iter().enumerate() {
        println!("{}", format_adapter_report(report, i));
    }
    ExitCode::SUCCESS
}
