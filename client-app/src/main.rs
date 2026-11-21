// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Native Minecraft 1.21.1 (Protocol 767) client executable for mc-spine.

use std::process::ExitCode;

use mc_spine_client::cli::{help_text, parse_args, redact_from_argv, version_info_text};
use mc_spine_client::integrated;
use mc_spine_client::settings::ClientSettings;
use mc_spine_client::sim::run_session;

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse_args(&raw_args) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("mc-spine-client: {}", redact_from_argv(&err, &raw_args));
            return ExitCode::from(2);
        }
    };

    if cfg.show_help {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }

    if cfg.show_version_info {
        println!("{}", version_info_text());
        return ExitCode::SUCCESS;
    }

    let mut cfg = cfg;

    // Resolve player settings: --preset base, settings-file keys, a Vanilla
    // options.txt import, then explicit flags. File problems are warnings,
    // not fatal, unless an explicitly named file cannot be read at all.
    match cfg.resolve_client_settings() {
        Ok(warnings) => {
            for warning in warnings {
                eprintln!("mc-spine-client: {}", cfg.redact(&warning));
            }
        }
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            return ExitCode::from(2);
        }
    }

    // Settings write-out paths are settings-only actions: write the file and
    // exit unless a server/render action was also requested.
    if let Some(path) = cfg.write_default_settings.clone() {
        return match ClientSettings::default().write_toml(&path) {
            Ok(()) => {
                println!("wrote default settings to {}", path.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("mc-spine-client: {}", cfg.redact(&err));
                ExitCode::FAILURE
            }
        };
    }
    if let Some(path) = cfg.write_settings.clone() {
        match cfg.client_settings.write_toml(&path) {
            Ok(()) => println!("wrote settings to {}", path.display()),
            Err(err) => {
                eprintln!("mc-spine-client: {}", cfg.redact(&err));
                return ExitCode::FAILURE;
            }
        }
        if !cfg.wants_session_action() {
            return ExitCode::SUCCESS;
        }
    }

    let endpoint = match cfg.target_endpoint() {
        Ok(ep) => ep,
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            return ExitCode::from(2);
        }
    };

    // Real --quickPlaySingleplayer loopback: launch the integrated mc-spine
    // server on an ephemeral port and keep the guard alive for the session.
    let mut integrated_server = None;
    if endpoint.is_none() {
        if cfg.quick_play_singleplayer.is_some() {
            match integrated::launch(&cfg) {
                Ok(server) => {
                    cfg.server = Some("127.0.0.1".to_string());
                    cfg.port = server.port;
                    integrated_server = Some(server);
                }
                Err(err) => {
                    eprintln!("mc-spine-client: {}", cfg.redact(&err));
                    return ExitCode::from(2);
                }
            }
        } else {
            eprintln!(
                "mc-spine-client: no server specified (pass --quickPlayMultiplayer <host[:port]>, --quickPlaySingleplayer <world>, or --server <host>; use --help for usage)"
            );
            return ExitCode::from(2);
        }
    }
    let _integrated_server = integrated_server;

    match run_session(&cfg) {
        Ok(summary) => {
            if cfg.json_summary {
                println!("{}", summary.to_json());
            } else {
                println!("{}", summary.to_human());
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            ExitCode::FAILURE
        }
    }
}
