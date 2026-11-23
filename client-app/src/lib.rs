// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Standalone native Minecraft 1.21.1 (Protocol 767) client library combining
//! `megu-kernel` and `megu-client-mesh`.

pub mod audio_out;
pub mod audio_qol;
pub mod cli;
pub mod crash_reporter;
pub mod frame_pacer;
pub mod friend_client;
pub mod hardware_profile;
pub mod integrated;
pub mod kinetic_sync;
pub mod lod;
pub mod mod_assets_meta;
pub mod online_auth;
pub mod protocol;
pub mod recovery;
pub mod render;
pub mod render_graph;
pub mod server_list;
pub mod session;
pub mod settings;
pub mod shaderpack_runtime;
pub mod shaders;
pub mod shutdown;
pub mod sim;
pub mod sound_assets;
pub mod spatial_audio;
pub mod world;
pub mod zip_entries;
pub mod zsh_command_bar;
