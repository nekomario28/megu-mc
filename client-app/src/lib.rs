// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Standalone native Minecraft 1.21.1 (Protocol 767) client library combining
//! `mc-spine-kernel` and `mc-spine-client-mesh`.

pub mod audio_out;
pub mod audio_qol;
pub mod cli;
pub mod crash_reporter;
pub mod frame_pacer;
pub mod friend_client;
pub mod hardware_profile;
pub mod integrated;
pub mod lod;
pub mod protocol;
pub mod recovery;
pub mod render;
pub mod render_graph;
pub mod session;
pub mod settings;
pub mod shaderpack_runtime;
pub mod shaders;
pub mod sim;
pub mod sound_assets;
pub mod spatial_audio;
pub mod world;
pub mod zsh_command_bar;
