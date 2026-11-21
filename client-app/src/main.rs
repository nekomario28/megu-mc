// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Native Minecraft 1.21.1 (Protocol 767) client executable for mc-spine.

use std::process::ExitCode;

use mc_spine_client::cli::{help_text, parse_args, redact_from_argv, version_info_text};
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

    let endpoint = match cfg.target_endpoint() {
        Ok(ep) => ep,
        Err(err) => {
            eprintln!("mc-spine-client: {}", cfg.redact(&err));
            return ExitCode::from(2);
        }
    };

    if endpoint.is_none() {
        if let Some(ref world_name) = cfg.quick_play_singleplayer {
            eprintln!(
                "mc-spine-client: --quickPlaySingleplayer '{}' requires an integrated server endpoint via --quickPlayMultiplayer or --server",
                cfg.redact(world_name)
            );
            return ExitCode::from(2);
        }
        eprintln!(
            "mc-spine-client: no server specified (pass --quickPlayMultiplayer <host[:port]> or --server <host>; use --help for usage)"
        );
        return ExitCode::from(2);
    }

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
