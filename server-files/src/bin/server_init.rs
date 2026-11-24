// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu-admin`: the standalone offline admin CLI — a thin entrypoint over
//! [`megu_server_files::admin_cli`], the implementation `megu-init` shares
//! for its legacy subcommands, so the 1,800-line command surface compiles
//! once into the library instead of twice into the binaries.

use std::process::ExitCode;

fn main() -> ExitCode {
    megu_server_files::admin_cli::run(std::env::args().skip(1).collect())
}
