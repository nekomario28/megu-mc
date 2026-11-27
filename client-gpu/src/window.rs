// SPDX-License-Identifier: LGPL-3.0-or-later
//! `winit` 0.30 window + input loop: `ApplicationHandler`, FPS camera
//! controls, pause/settings via `megu_ui`, background-FPS throttling,
//! and `FrameBudgetGovernor` pacing — all driving the real `wgpu` renderer.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

struct BlockInspectorHudCache {
    telemetry: megu_ui::block_inspector_hud::BlockInspectorTelemetry,
    viewport: [u32; 2],
    locale: &'static str,
    list: UiDrawList,
}

use megu_client::audio_qol::{
    DEFAULT_ZOOM_SMOOTHING_RATE, ToolDurability, ZoomState, check_tool_durability_warning,
};
use megu_client::friend_client::{FriendGuestBridge, FriendHostEvent, FriendHostSession};
use megu_client::friends_store::{
    FriendShareMode as StoreShareMode, FriendsStore, load_or_create_identity,
};
use megu_client::integrated;
use megu_client::protocol::{
    ItemComponentPatch, ItemStackComponents, NbtValue, WireReader, decode_item_stack_components,
    encode_item_stack_components, encode_slot_stack,
};
use megu_client::render::{FrameBudgetGovernor, RenderConfig, ThrottleStage};
use megu_client::server_list::{ServerEntry, ServerListStore};
use megu_client::settings::CloudMode;
use megu_client::settings::{
    ClientSettings, CrashReportMode as ClientCrashReportMode, DistantHorizon, HostMinimapMode,
    HostPlayerRadarScope, QualityPreset,
};
use megu_client::spatial_audio::SpatialAudioPreset;
use megu_client::world::{TrackedBlockEntity, TrackedEntity};
use megu_client::zsh_command_bar::ZshCommandBar;
use megu_client_mesh::ChunkUploadBudget;
use megu_client_mesh::ber_instancing::{
    BerBellMount, BerChestKind, BerDyeColor, VanillaBerExtendedState, VanillaBerFacing,
    VanillaBerKind, VanillaBerState, VanillaParticlePayload, VanillaParticleQuad,
    VanillaParticleWorld, build_ber_mesh, build_extended_ber_mesh,
};
use megu_client_mesh::builtin_shadow_wgsl::{
    BuiltinShadowQuality, RenderScalePreset, enchantment_glint_rgb,
};
#[cfg(test)]
use megu_client_mesh::entity_rig::build_entity_rig_mesh_with_mouth_item;
use megu_client_mesh::entity_rig::{
    EntityPose, EntityRigArchetype, EntityRigPose, EntityRigSpec, build_entity_rig_mesh_with_pose,
    classify_entity_rig_or_fallback,
};
use megu_client_mesh::mesher::classify_vanilla_ber;
use megu_client_mesh::tactile_effects::{
    TactileEffectsConfig, TactileParticle, TactileParticleKind, TactilePreset,
};
use megu_client_mesh::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad};
use megu_client_mesh::vanilla_plus_nature::{NatureParticle, NatureParticleKind};
use megu_client_mesh::vfx_engine::{ParticleDraw, ParticleOrientation, VfxFrameOutput};
use megu_core::hash::{FNV1A64_OFFSET_BASIS, FNV1A64_PRIME, Fnv1a64};
use megu_core::math_opt_foundation::exp_decay_exact;
use megu_core::rev::UiLayer;
use megu_core::sys_metrics::{CpuSampler, HostMetricsSnapshot, sample_host_metrics};
use megu_kernel::debug_chords::{
    DebugChordAction, DebugChordContext, DebugChordState, DebugLine, DebugTarget, F3Chord,
    GameMode as DebugGameMode, PlayerDebugInfo, build_chunk_border_lines,
    build_entity_hitbox_lines, finish_gamemode_switcher, format_advanced_tooltip_lines,
    handle_f3_chord,
};
use megu_kernel::entity_interp::SkyFogOutput;
use megu_proto::frl_e2ee::{FriendAllowlist, Frl3GuestToken, Frl3Identity, parse_friend_id};
use megu_proto::seamless_portal::{
    SEAMLESS_PORTAL_CHANNEL, SeamlessDimTransitionState, SeamlessPortalPacket,
    decode_seamless_portal_packet,
};
use megu_ui::container_screen::{
    ContainerScreenState, ContainerSlotHotkey, DragState, InventorySortState, MenuScreenSchema,
    MerchantOfferView, SlotDef, SlotKind, UiItemStack, WorkstationAction, menu_schema,
};
use megu_ui::draw::tessellate_draw_list;
use megu_ui::draw::{UiColor, UiCommand, UiDrawList, UiRect, text_width};
use megu_ui::gui_bridge::{GuiBridgeBatch, lower_bridge_batch_to_draw_list};
use megu_ui::hud_widgets::{F3DebugOverlay, ScoreboardEntry};
use megu_ui::magic_hud::LookingAtBlockState;
use megu_ui::recipe_browser::{
    BrowserTab, DetailMode, ItemEntry, RecipeBrowserAction, RecipeBrowserState,
};
use megu_ui::sign_and_chat::SignEditorState;
use megu_ui::state::{
    CrashReportMode as UiCrashReportMode, DisconnectedScreenState, FriendListEntry,
    FriendShareMode as UiShareMode, FriendsScreenState, GpuBackend, LodMode, MinimapMode,
    MultiplayerScreenState, MultiplayerServerEntry, PlayerRadarScope, Screen, SettingsTab,
    UiAction, UiInputEvent, UiKey, UiSettings, UiState, VideoPreset, VisualStylePreset,
    WorldLaunchOptions,
};
use megu_ui::state::{HudNametag, ScoreboardState, TitleOverlayState, invite_code_display, tr};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{DeviceEvent, DeviceId, ElementState, Ime, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
#[cfg(all(unix, not(target_os = "macos")))]
use winit::platform::wayland::WindowAttributesExtWayland;
#[cfg(all(unix, not(target_os = "macos")))]
use winit::platform::x11::WindowAttributesExtX11;
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowAttributes, WindowId};

use crate::atmosphere::{BiomeAtmosphereBlender, ChunkFadeTracker, blend_dimension_crossfade};
use crate::clipboard::{ClipboardOutcome, SystemClipboard};
use crate::entity_appearance::{EntityAppearanceQuery, RuntimeEntityAppearanceAtlas};
use crate::item_visuals_3d::{
    SpriteRgba, build_winit_window_icon, discover_official_builtin_packs, merged_end_credits_lines,
    upgrade_item_icon_atlas_with_3d_blocks,
};
use crate::live_session::{
    CLOUD_SUBWORLD_ID, CloudDeckMode, CrosshairPick, EntityContactShadowQuad, LiveSession,
    PlayerMovement, PlayerPose, RenderFramePose, SessionPaths, SessionStatus, jar_version_ids,
};
use crate::panorama_pass::vanilla_panorama_angles;
use crate::renderer::{
    FirstPersonUseKind, FirstPersonViewmodel, GpuRenderer, request_gpu_device,
};
use crate::subworld_sync::SubWorldHelmCmd;
use crate::title_branding::{
    TitleBrandingAssets, configured_title_jar, resolve_title_jar, title_mod_jars,
};
use crate::vanilla_official_extras::VanillaOfficialExtras;
use crate::{
    Camera, DisplayPresentationPolicy, GpuCliConfig, MoveInput, build_demo_scene,
    detect_display_server_kind, fingerprint_draw_list, preferred_alpha_mode, present_mode_for,
    select_display_presentation_policy,
};

/// Window title.
pub const WINDOW_TITLE: &str = "Megu 1.21.1";

fn gui_entry(entry: crate::vanilla_gui_sprites::SpriteAtlasEntry) -> megu_ui::draw::GuiSpriteEntry {
    let border = entry.nine_slice_border.unwrap_or_default();
    megu_ui::draw::GuiSpriteEntry {
        uv_min: entry.uv_min,
        uv_max: entry.uv_max,
        width: entry.width,
        height: entry.height,
        nine_slice_border: [border.left, border.top, border.right, border.bottom],
    }
}

/// Re-key the loaded Vanilla HUD rectangles onto Megu's stable logical
/// sprite IDs. `VanillaGuiTextureAtlas::to_gui_sprite_atlas` intentionally
/// keeps only widget chrome in the legacy contract map, so the window —
/// the boundary that owns host asset loading — installs these additional
/// bindings without changing the shared atlas loader.
fn install_logical_hud_sprite_bindings(
    atlas: &mut megu_ui::draw::GuiSpriteAtlas,
    textures: &crate::vanilla_gui_sprites::VanillaGuiTextureAtlas,
) {
    use crate::vanilla_gui_sprites::VanillaGuiSpriteId as V;
    use megu_ui::draw::hud_sprite_ids as hud;
    const TYPED: &[(&str, V)] = &[
        (hud::HOTBAR, V::Hotbar),
        (hud::HOTBAR_SELECTION, V::HotbarSelection),
        (hud::CROSSHAIR, V::Crosshair),
        (hud::EXPERIENCE_BAR_BACKGROUND, V::XpBarBg),
        (hud::EXPERIENCE_BAR_PROGRESS, V::XpBarFill),
        (hud::HEART_CONTAINER, V::HeartContainer),
        (hud::HEART_FULL, V::HeartFull),
        (hud::HEART_HALF, V::HeartHalf),
        (hud::FOOD_EMPTY, V::FoodEmpty),
        (hud::FOOD_FULL, V::FoodFull),
        (hud::ARMOR_EMPTY, V::ArmorEmpty),
        (hud::ARMOR_FULL, V::ArmorFull),
    ];
    for (key, id) in TYPED {
        if let Some(entry) = textures.entry(*id) {
            atlas.insert_named_sprite(*key, gui_entry(*entry));
        }
    }
    for (name, entry) in &textures.named_entries {
        let key = if let Some(rest) = name
            .strip_prefix("sprites/hud/")
            .and_then(|rest| rest.strip_suffix(".png"))
        {
            format!("hud/{rest}")
        } else if name.starts_with(hud::MOB_EFFECT_PREFIX) {
            name.clone()
        } else {
            continue;
        };
        atlas.insert_named_sprite(key, gui_entry(*entry));
    }
}

/// Event-loop wake interval while the surface is occluded: slow enough for
/// zero-GPU sleep, fast enough to keep a live session's network pump alive.
const OCCLUDED_TICK: Duration = Duration::from_millis(50);

/// F3 diagnostic line refresh interval: the overlay reads smoothly at
/// 10 Hz, and a stable actionbar keeps the draw-list fingerprint — and
/// therefore the UI mesh upload — quiet on steady frames.
const F3_REFRESH: Duration = Duration::from_millis(100);
/// Live entity/BER mesh rebuild cadence — the server mutates tracked
/// entity state at 20 Hz, so faster rebuilds re-mesh identical poses.
const LIVE_ENTITY_REFRESH: Duration = Duration::from_millis(50);
/// Stable dispatch slots mirroring the 12 [`VanillaBerKind`] families.
const BER_KIND_SLOTS: usize = 12;

fn sample_live_entity_light(live: &LiveSession, x: f32, y: f32, z: f32) -> (u8, u8) {
    live.sample_block_and_sky_light(x, y, z)
}

#[cfg(test)]
mod entity_lighting_wiring_tests {
    use super::sample_live_entity_light;
    use crate::live_session::LiveSession;

    #[test]
    fn entity_staging_adapter_reads_live_world_light() {
        let live = LiveSession::offline_with_null_audio();
        assert_eq!(sample_live_entity_light(&live, 0.25, 64.0, -0.25), (0, 15));
    }
}

/// Vanilla `Camera.getMaxZoom(4.0)`: the third-person pull-back distance
/// before the corner sweep clips it.
const THIRD_PERSON_DISTANCE: f32 = 4.0;
/// Inflated eye AABB of the `getMaxZoom` corner rays (`±0.1` per axis).
const CAMERA_ZOOM_CORNER_INSET: f32 = 0.1;
const SPRINT_FOV_BOOST: f32 = 0.10;
const SPRINT_FOV_SMOOTHING_RATE: f32 = 12.0;
/// `AbstractClientPlayer.getFieldOfViewModifier` factors, in vanilla order.
const FLYING_FOV_BOOST: f32 = 1.1;
/// `getTicksUsingItem()` a full bow draw takes.
const BOW_FOV_DRAW_TICKS: f32 = 20.0;
/// The bow-draw FOV pull-in at a full draw.
const BOW_FOV_DRAW_SHRINK: f32 = 0.15;
/// A first-person spyglass scope pins the FOV modifier to `0.1`.
const SPYGLASS_FOV_MODIFIER: f32 = 0.1;
/// Vanilla `Options.screenEffectScale` default — `1.0` keeps the full
/// `getFieldOfViewModifier` swing, `0.0` pins the setting FOV.
const DEFAULT_FOV_EFFECT_SCALE: f32 = 1.0;
/// Vanilla `GameRenderer.bobHurt` peak tilt (`f * 14.0F`).
const HURT_TILT_PEAK_DEG: f32 = 14.0;
/// Vanilla `Options.damageTiltStrength` default.
const DEFAULT_DAMAGE_TILT_STRENGTH: f32 = 1.0;
/// Vanilla `Minecraft.rightClickDelay`: four game ticks between held uses.
const RIGHT_CLICK_DELAY_TICKS: u32 = 4;
/// One vanilla game tick in seconds — the frame clock folds into 20 Hz steps.
const GAME_TICK_SECS: f32 = 0.05;
/// Ceiling on the held-use clock; vanilla saturates the bow draw at 20 ticks.
const MAX_USE_HOLD_SECS: f32 = 5.0;

/// Friend-relay session timeout (register, join, revoke round-trips).
const FRIEND_TIMEOUT: Duration = Duration::from_secs(10);
/// `FRL3-FRIEND` host-share poll cadence: the relay link's read timeout
/// doubles as the worker's event poll, so allowlist refreshes (friend
/// removal kicks) apply within about this window.
const FRIEND_SHARE_TIMEOUT: Duration = Duration::from_secs(3);

fn merchant_offer_views(
    offers: &[megu_client::protocol::MerchantOfferEntry],
) -> Vec<MerchantOfferView> {
    offers
        .iter()
        .map(|offer| {
            let item_name = |id: i64| {
                crate::recipe_ingest::item_name_for_id(u32::try_from(id).unwrap_or_default())
            };
            let base_cost = offer.cost_a.count.max(1) as f32;
            let adjustment = (base_cost * offer.demand as f32 * offer.price_multiplier).floor();
            let effective = (base_cost + adjustment + offer.special_price as f32)
                .floor()
                .max(1.0)
                .min(u32::MAX as f32) as u32;
            let output = decode_merchant_result_slot(&offer.result_slot_wire)
                .map(|(item, _count)| item_name(i64::from(item)))
                .unwrap_or_else(|| "minecraft:air".to_owned());
            MerchantOfferView {
                input1: item_name(offer.cost_a.item),
                input2: offer.cost_b.as_ref().map(|cost| item_name(cost.item)),
                output,
                original_cost: offer.cost_a.count.max(0) as u32,
                effective_cost: effective,
                uses: offer.uses.max(0) as u32,
                max_uses: offer.max_uses.max(0) as u32,
                out_of_stock: offer.disabled || offer.uses >= offer.max_uses,
            }
        })
        .collect()
}

/// Decode the Protocol 767 result `Slot` from one merchant offer.
pub fn decode_merchant_result_slot(wire: &[u8]) -> Option<(i32, i32)> {
    let (stack, used) = decode_item_stack_components(wire).ok()?;
    (used == wire.len() && stack.count > 0).then_some((stack.item_id, stack.count))
}

fn protocol_767_table() -> &'static megu_proto::ProtocolTable {
    static TABLE: std::sync::OnceLock<megu_proto::ProtocolTable> = std::sync::OnceLock::new();
    TABLE.get_or_init(megu_proto::ProtocolTable::v767_builtin)
}

fn decode_stack_component(
    stack: &ItemStackComponents,
    component_name: &str,
) -> Option<megu_proto::item_components::VerifiedComponent> {
    let table = protocol_767_table();
    let type_id = table.registry_id("minecraft:data_component_type", component_name)?;
    let patch = stack
        .added
        .iter()
        .find(|component| u32::try_from(component.type_id).ok() == Some(type_id))?;
    megu_proto::item_components::VerifiedComponent::decode(&megu_proto::RawComponent {
        type_id,
        data: patch.data.clone(),
    })
    .ok()
}

/// Convert the window-0 inventory hotbar menu slot to `edit_book`'s
/// Protocol 767 hotbar index.
pub fn book_hotbar_slot(menu_slot: i16) -> Option<i32> {
    (36..=44)
        .contains(&menu_slot)
        .then(|| i32::from(menu_slot - 36))
}

/// An explicit sign Done action sends an update; Escape is a discard path.
pub fn should_send_sign_update(done: bool) -> bool {
    done
}

/// Whether the title UI should reveal the active panorama behind its overlay.
pub fn title_panorama_is_visible(screen: Screen, panorama_active: bool) -> bool {
    screen == Screen::Title && panorama_active
}

/// FNV-1a content fingerprint for arm-skin dedup — cheap enough to run
/// every frame and sensitive enough that a re-skinned player always
/// re-uploads.
fn arm_skin_fingerprint(rgba: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in rgba {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `true` for held items that render as a flat handheld sprite rather
/// than a blocky 3D item — mirrors the classification
/// `LiveSession::first_person_viewmodel` applies to the main hand.
fn viewmodel_held_is_block(name: &str) -> bool {
    ![
        "sword", "pickaxe", "axe", "shovel", "hoe", "bow", "crossbow", "trident", "shield",
        "potion", "apple", "bread", "bucket", "elytra",
    ]
    .iter()
    .any(|kind| name.contains(kind))
}

/// Build the left-hand first-person viewmodel snapshot. `main` is this
/// frame's main-hand snapshot; the offhand reuses its timing, bob and
/// equip fields but substitutes the slot-45 (`OFF_HAND`) item and only
/// animates when the session resolved the swing/use to the off hand —
/// `use_hand()`/`begin_item_use` already map an offhand shield to
/// `ShieldBlock`, so no separate blocking heuristic is needed here.
fn offhand_first_person_viewmodel(
    main: &FirstPersonViewmodel,
    offhand_name: Option<String>,
    swing_hand: i32,
    item_use_hand: i32,
) -> FirstPersonViewmodel {
    let offhand_using = item_use_hand == LiveSession::OFF_HAND;
    FirstPersonViewmodel {
        visible: main.visible,
        held_item_name: offhand_name.clone(),
        held_color_rgb: main.held_color_rgb,
        is_block: offhand_name
            .as_deref()
            .is_some_and(viewmodel_held_is_block),
        swing_progress: if swing_hand == LiveSession::OFF_HAND {
            main.swing_progress
        } else {
            0.0
        },
        use_progress: if offhand_using {
            main.use_progress
        } else {
            0.0
        },
        using_kind: if offhand_using {
            main.using_kind
        } else {
            FirstPersonUseKind::None
        },
        walk_bob_x: main.walk_bob_x,
        walk_bob_y: main.walk_bob_y,
        equip_progress: main.equip_progress,
    }
}

fn merchant_signature(offers: &[megu_client::protocol::MerchantOfferEntry]) -> u64 {
    let mut hash = FNV1A64_OFFSET_BASIS;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    };
    for offer in offers {
        mix(offer.cost_a.item as u64);
        mix(offer.cost_a.count as u64);
        mix(offer.cost_b.as_ref().map_or(0, |cost| cost.item as u64));
        mix(offer.cost_b.as_ref().map_or(0, |cost| cost.count as u64));
        mix(offer.uses as u64);
        mix(offer.max_uses as u64);
        mix(offer.special_price as u64);
        mix(offer.demand as u64);
        mix(offer.price_multiplier.to_bits() as u64);
        mix(u64::from(offer.disabled));
        for byte in &offer.result_slot_wire {
            mix(u64::from(*byte));
        }
    }
    hash
}

/// Vanilla's chat length cap — paste and IME feeds into the chat bar
/// stop at this many characters.
const CHAT_FIELD_MAX_CHARS: usize = 256;

/// Non-registry menu id for the client-side window-0 player inventory —
/// `menu_schema` owns `0..=24` and this sentinel can never collide.
pub const PLAYER_INVENTORY_MENU_ID: u32 = u32::MAX;

/// Vanilla window-0 player inventory layout (46 slots): crafting output +
/// 2x2 grid + 4 armor + offhand as "container" slots, then the standard
/// 27+9 region `inventory_start = 9` describes.
static PLAYER_INVENTORY_SLOTS: [SlotDef; 10] = [
    SlotDef::new(0, 144, 36, SlotKind::Output),
    SlotDef::new(1, 98, 18, SlotKind::Input),
    SlotDef::new(2, 116, 18, SlotKind::Input),
    SlotDef::new(3, 98, 36, SlotKind::Input),
    SlotDef::new(4, 116, 36, SlotKind::Input),
    SlotDef::new(5, 8, 8, SlotKind::Armor),
    SlotDef::new(6, 8, 26, SlotKind::Armor),
    SlotDef::new(7, 8, 44, SlotKind::Armor),
    SlotDef::new(8, 8, 62, SlotKind::Armor),
    SlotDef::new(45, 77, 62, SlotKind::Offhand),
];

/// The `E`-key screen's schema: vanilla's own 46-slot player inventory,
/// rendered by the same [`ContainerScreenState`] every server menu uses.
pub static PLAYER_INVENTORY_SCHEMA: MenuScreenSchema = MenuScreenSchema {
    menu_id: PLAYER_INVENTORY_MENU_ID,
    name: "minecraft:inventory",
    panel_w: 176.0,
    panel_h: 166.0,
    container_slots: &PLAYER_INVENTORY_SLOTS,
    has_player_inventory: true,
    inventory_start: 9,
    slot_count: 46,
    inventory_origin_x: 8,
    inventory_origin_y: 84,
    buttons: &[],
    property_bars: &[],
};

/// Which recipe-browser-style focus the R/U/W hotkeys request on the hovered stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeFocus {
    /// `R` — "recipes producing this item" detail view.
    Recipes,
    /// `U` — "uses consuming this item" detail view.
    Uses,
    /// `W` — Create-MachineTutorial-style 3D scene for the item.
    MachineTutorial,
}

/// Display-independent output of the Minecraft keyboard bindings.
#[derive(Clone, Debug, PartialEq)]
pub enum WindowInputEvent {
    DebugChord(DebugChordAction),
    SetDebugOverlayVisible(bool),
    SelectHotbar(u8),
    SwapOffhand,
    DropItem {
        entire_stack: bool,
    },
    OpenInventory,
    OpenChat {
        slash_prefill: bool,
    },
    SetTabListVisible(bool),
    SortInventory,
    OpenAdvancements,
    /// recipe-browser-style hover hotkey resolved from `R`/`U`/`W` while a container
    /// surface is up — opens the recipe browser focused on the hovered
    /// stack (no-op when nothing resolvable is under the cursor).
    RecipeBrowserFocus(RecipeFocus),
}

/// Pure input router shared by the window loop and display-free tests.
#[derive(Clone, Debug, Default)]
pub struct WindowInputRouter {
    pub debug_chords: DebugChordState,
    pub f3_held: bool,
    pub f3_chord_consumed: bool,
    pub active_hotbar_slot: u8,
    pub tab_list_held: bool,
}

impl WindowInputRouter {
    /// Process a physical key transition. `ctrl` is the current modifier state.
    pub fn route_key(
        &mut self,
        key: KeyCode,
        state: ElementState,
        ctrl: bool,
    ) -> Vec<WindowInputEvent> {
        self.route_key_with_context(key, state, ctrl, None, None, false)
    }

    /// Supply authoritative player/target data when available for F3+C/I/N/F4.
    pub fn route_key_with_context(
        &mut self,
        key: KeyCode,
        state: ElementState,
        ctrl: bool,
        player: Option<PlayerDebugInfo<'_>>,
        target: Option<DebugTarget<'_>>,
        has_permission_level_2: bool,
    ) -> Vec<WindowInputEvent> {
        let pressed = state == ElementState::Pressed;
        if key == KeyCode::F3 {
            if pressed {
                if !self.f3_held {
                    self.f3_chord_consumed = false;
                }
                self.f3_held = true;
                return Vec::new();
            }
            if !self.f3_held {
                return Vec::new();
            }
            self.f3_held = false;
            let consumed = std::mem::take(&mut self.f3_chord_consumed);
            let mut events = Vec::new();
            if let Some(mode) = player.map(|info| info.gamemode) {
                if let Some(action) = finish_gamemode_switcher(&mut self.debug_chords, mode) {
                    events.push(WindowInputEvent::DebugChord(action));
                }
            } else {
                self.debug_chords.gamemode_switcher = None;
            }
            if !consumed {
                self.debug_chords.toggle_debug_overlay();
                events.push(WindowInputEvent::SetDebugOverlayVisible(
                    self.debug_chords.show_debug_overlay,
                ));
            }
            return events;
        }

        if self.f3_held && pressed {
            if let Some(chord) = f3_chord_for_key(key) {
                self.f3_chord_consumed = true;
                let mut context = DebugChordContext::new(&mut self.debug_chords);
                context.player = player;
                context.target = target;
                context.has_permission_level_2 = has_permission_level_2;
                let action = handle_f3_chord(chord, &mut context);
                return vec![WindowInputEvent::DebugChord(action)];
            }
        }

        if key == KeyCode::Tab {
            if self.tab_list_held != pressed {
                self.tab_list_held = pressed;
                return vec![WindowInputEvent::SetTabListVisible(pressed)];
            }
            return Vec::new();
        }
        if !pressed {
            return Vec::new();
        }

        let event = match key {
            KeyCode::Digit1 => WindowInputEvent::SelectHotbar(0),
            KeyCode::Digit2 => WindowInputEvent::SelectHotbar(1),
            KeyCode::Digit3 => WindowInputEvent::SelectHotbar(2),
            KeyCode::Digit4 => WindowInputEvent::SelectHotbar(3),
            KeyCode::Digit5 => WindowInputEvent::SelectHotbar(4),
            KeyCode::Digit6 => WindowInputEvent::SelectHotbar(5),
            KeyCode::Digit7 => WindowInputEvent::SelectHotbar(6),
            KeyCode::Digit8 => WindowInputEvent::SelectHotbar(7),
            KeyCode::Digit9 => WindowInputEvent::SelectHotbar(8),
            KeyCode::KeyF => WindowInputEvent::SwapOffhand,
            KeyCode::KeyQ => WindowInputEvent::DropItem { entire_stack: ctrl },
            KeyCode::KeyE => WindowInputEvent::OpenInventory,
            KeyCode::KeyT => WindowInputEvent::OpenChat {
                slash_prefill: false,
            },
            KeyCode::Slash => WindowInputEvent::OpenChat {
                slash_prefill: true,
            },
            KeyCode::KeyR => WindowInputEvent::SortInventory,
            KeyCode::KeyL => WindowInputEvent::OpenAdvancements,
            _ => return Vec::new(),
        };
        if let WindowInputEvent::SelectHotbar(slot) = event {
            self.active_hotbar_slot = slot;
        }
        vec![event]
    }

    /// A lost focus must not leave F3 or Tab logically held.
    pub fn release_held_keys(&mut self) -> Option<WindowInputEvent> {
        self.f3_held = false;
        self.f3_chord_consumed = false;
        self.debug_chords.gamemode_switcher = None;
        let tab_was_held = std::mem::take(&mut self.tab_list_held);
        tab_was_held.then_some(WindowInputEvent::SetTabListVisible(false))
    }
}

fn f3_chord_for_key(key: KeyCode) -> Option<F3Chord> {
    Some(match key {
        KeyCode::KeyA => F3Chord::A,
        KeyCode::KeyB => F3Chord::B,
        KeyCode::KeyC => F3Chord::C,
        KeyCode::KeyD => F3Chord::D,
        KeyCode::F4 => F3Chord::F4,
        KeyCode::KeyG => F3Chord::G,
        KeyCode::KeyH => F3Chord::H,
        KeyCode::KeyI => F3Chord::I,
        KeyCode::KeyN => F3Chord::N,
        KeyCode::KeyP => F3Chord::P,
        KeyCode::KeyQ => F3Chord::Q,
        KeyCode::KeyT => F3Chord::T,
        KeyCode::Digit1 => F3Chord::Key1,
        KeyCode::Digit2 => F3Chord::Key2,
        KeyCode::Digit3 => F3Chord::Key3,
        _ => return None,
    })
}

/// The three Minecraft camera views in F5 cycling order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraMode {
    #[default]
    FirstPerson,
    ThirdPersonBack,
    ThirdPersonFront,
}

/// `AbstractClientPlayer.getFieldOfViewModifier` inputs, in the order the
/// vanilla bytecode applies them: creative flight, the movement-speed
/// attribute (speed/slowness potions), an in-progress bow draw, the
/// first-person spyglass, and `Options.screenEffectScale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FovModifierInputs {
    /// `Abilities.flying` — the creative-flight `*1.1`.
    pub flying: bool,
    /// `Attributes.MOVEMENT_SPEED` relative to the ability's walking speed:
    /// `1.0` is an unmodified player, `1.3` the vanilla sprint boost.
    pub movement_speed_ratio: f32,
    /// `LivingEntity.getTicksUsingItem()` while [`Self::bow_drawn`] holds.
    pub bow_draw_ticks: f32,
    /// A bow is being drawn right now.
    pub bow_drawn: bool,
    /// First-person spyglass scope.
    pub spyglass_scoped: bool,
    /// `Options.screenEffectScale`; the modifier is lerped from `1.0`.
    pub fov_effect_scale: f32,
}

impl Default for FovModifierInputs {
    fn default() -> Self {
        Self {
            flying: false,
            movement_speed_ratio: 1.0,
            bow_draw_ticks: 0.0,
            bow_drawn: false,
            spyglass_scoped: false,
            fov_effect_scale: DEFAULT_FOV_EFFECT_SCALE,
        }
    }
}

/// Ordered `FovPipeline`: `AbstractClientPlayer.getFieldOfViewModifier`
/// followed by `GameRenderer`'s screen-effect lerp. Every row multiplies
/// into the running modifier; a scoped spyglass returns `0.1` before any
/// other row, and the final `Mth.lerp(fovEffectScale, 1.0, modifier)`
/// folds the accessibility scale in so `0.0` leaves the FOV untouched.
fn compute_vanilla_fov_modifier(inputs: &FovModifierInputs) -> f32 {
    if inputs.spyglass_scoped {
        return SPYGLASS_FOV_MODIFIER;
    }
    let mut modifier = 1.0f32;
    if inputs.flying {
        modifier *= FLYING_FOV_BOOST;
    }
    let speed = if inputs.movement_speed_ratio.is_finite() {
        inputs.movement_speed_ratio.max(0.0)
    } else {
        1.0
    };
    modifier *= (speed + 1.0) * 0.5;
    if inputs.bow_drawn {
        let draw = if inputs.bow_draw_ticks.is_finite() {
            (inputs.bow_draw_ticks / BOW_FOV_DRAW_TICKS).clamp(0.0, 1.0)
        } else {
            0.0
        };
        modifier *= 1.0 - draw * draw * BOW_FOV_DRAW_SHRINK;
    }
    let scale = finite_clamp(inputs.fov_effect_scale, DEFAULT_FOV_EFFECT_SCALE, 0.0, 1.0);
    1.0 + (modifier - 1.0) * scale
}

/// `GameRenderer.bobHurt` camera tilt, split onto the two axes a yaw/pitch
/// camera can carry: a roll around the view axis and a pitch bend.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HurtTilt {
    /// Roll around the view axis, radians.
    pub roll_rad: f32,
    /// Pitch offset added to the view pitch, degrees.
    pub pitch_deg: f32,
}

/// Vanilla `GameRenderer.bobHurt` for the local player: the hurt pose is
/// `Y(-hurtYaw) · Z(-tilt) · Y(hurtYaw)` with `tilt = sin(f⁴ · π) · 14°`
/// over the 10-tick hurt animation and `f = hurtTime / hurtDuration`.
/// Split around the attack direction, the in-plane part becomes a roll and
/// the perpendicular part a pitch bend, so a hit from dead ahead rolls the
/// screen and a side hit tips it.
fn compute_hurt_tilt(
    progress: f32,
    camera_yaw_deg: f32,
    hurt_yaw_deg: f32,
    damage_tilt_strength: f32,
) -> HurtTilt {
    if !progress.is_finite()
        || progress <= 0.0
        || !camera_yaw_deg.is_finite()
        || !hurt_yaw_deg.is_finite()
    {
        return HurtTilt::default();
    }
    let strength = finite_clamp(damage_tilt_strength, DEFAULT_DAMAGE_TILT_STRENGTH, 0.0, 1.0);
    let f = progress.min(1.0);
    let tick = f * f * f * f * std::f32::consts::PI;
    let tilt_deg = tick.sin() * HURT_TILT_PEAK_DEG * strength;
    if strength == 0.0 || !tilt_deg.is_finite() {
        return HurtTilt::default();
    }
    let (sin, cos) = (hurt_yaw_deg - camera_yaw_deg).to_radians().sin_cos();
    HurtTilt {
        // Vanilla rolls `ZP` by `-tilt` about the attack direction.
        roll_rad: (-tilt_deg).to_radians() * cos,
        pitch_deg: (-tilt_deg) * sin,
    }
}

/// Vanilla `Minecraft.rightClickDelay`: a held use re-runs `startUseItem`
/// every four game ticks, measured on the 20 Hz clock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RightClickDelay {
    remaining_ticks: u32,
}

impl RightClickDelay {
    /// Vanilla `startUseItem`'s tail: `this.rightClickDelay = 4`.
    fn on_start_use(&mut self) {
        self.remaining_ticks = RIGHT_CLICK_DELAY_TICKS;
    }

    /// Releasing the button clears the gate so the next press fires at once.
    fn on_release(&mut self) {
        self.remaining_ticks = 0;
    }

    /// One 20 Hz tick of `Minecraft.tick`'s
    /// `if (rightClickDelay > 0) --rightClickDelay;` — `true` means the held
    /// use should run again this tick.
    fn tick(&mut self) -> bool {
        self.remaining_ticks = self.remaining_ticks.saturating_sub(1);
        self.remaining_ticks == 0
    }
}

/// Vanilla `Camera.getMaxZoom`: sweep the eight `±0.1` eye-AABB corner rays
/// along `back_dir` and stop at the nearest solid voxel, so the
/// third-person camera clips against walls instead of phasing through
/// them. Returns a distance in `0..=max_dist`.
fn camera_max_zoom(
    eye: [f32; 3],
    back_dir: [f32; 3],
    max_dist: f32,
    is_solid: impl Fn([i32; 3]) -> bool,
) -> f32 {
    if !max_dist.is_finite() || max_dist <= 0.0 || eye.iter().any(|axis| !axis.is_finite()) {
        return 0.0;
    }
    let length = (back_dir[0] * back_dir[0]
        + back_dir[1] * back_dir[1]
        + back_dir[2] * back_dir[2])
        .sqrt();
    if !length.is_finite() || length <= f32::EPSILON {
        return max_dist;
    }
    let dir = [
        back_dir[0] / length,
        back_dir[1] / length,
        back_dir[2] / length,
    ];
    let mut limit = max_dist;
    // The eight AABB corners: every `±inset` combination of the three axes.
    for sign_x in [-CAMERA_ZOOM_CORNER_INSET, CAMERA_ZOOM_CORNER_INSET] {
        for sign_y in [-CAMERA_ZOOM_CORNER_INSET, CAMERA_ZOOM_CORNER_INSET] {
            for sign_z in [-CAMERA_ZOOM_CORNER_INSET, CAMERA_ZOOM_CORNER_INSET] {
                let start = [eye[0] + sign_x, eye[1] + sign_y, eye[2] + sign_z];
                if let Some(hit) = voxel_ray_distance(start, dir, limit, &is_solid) {
                    limit = hit;
                }
            }
        }
    }
    limit.clamp(0.0, max_dist)
}

/// Amanatides–Woo voxel traversal behind one `getMaxZoom` corner ray: the
/// exact distance to the face of the first solid voxel inside `max_dist`.
fn voxel_ray_distance(
    start: [f32; 3],
    dir: [f32; 3],
    max_dist: f32,
    is_solid: impl Fn([i32; 3]) -> bool,
) -> Option<f32> {
    let mut cell = [
        start[0].floor() as i32,
        start[1].floor() as i32,
        start[2].floor() as i32,
    ];
    let mut step = [0i32; 3];
    let mut t_max = [f32::INFINITY; 3];
    let mut t_delta = [f32::INFINITY; 3];
    for axis in 0..3 {
        let d = dir[axis];
        if d.abs() <= f32::EPSILON {
            continue;
        }
        step[axis] = if d > 0.0 { 1 } else { -1 };
        let boundary = if d > 0.0 {
            (cell[axis] + 1) as f32
        } else {
            cell[axis] as f32
        };
        t_max[axis] = (boundary - start[axis]) / d;
        t_delta[axis] = (1.0 / d).abs();
    }
    let mut t = 0.0f32;
    loop {
        if is_solid(cell) {
            return Some(t.clamp(0.0, max_dist));
        }
        let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
            0
        } else if t_max[1] <= t_max[2] {
            1
        } else {
            2
        };
        if !t_max[axis].is_finite() {
            return None;
        }
        t = t_max[axis];
        if t > max_dist {
            return None;
        }
        t_max[axis] += t_delta[axis];
        cell[axis] += step[axis];
    }
}

/// Window-independent input and projection state. The player's camera stays
/// at the eye position for movement/network updates in every view mode.
#[derive(Clone, Debug)]
pub struct CameraController {
    mode: CameraMode,
    zoom: ZoomState,
    base_fov_deg: f32,
    base_sensitivity: f32,
    sprint_blend: f32,
    fov_inputs: FovModifierInputs,
}

impl Default for CameraController {
    fn default() -> Self {
        Self::new(70.0, 1.0, 4.0)
    }
}

impl CameraController {
    /// `sensitivity` multiplies the shared camera mouse-look speed.
    pub fn new(fov_deg: f32, sensitivity: f32, zoom_divisor: f32) -> Self {
        let mut controller = Self {
            mode: CameraMode::FirstPerson,
            // Keep ZoomState's base constant so changing FOV mid-zoom does
            // not reset the current interpolated zoom factor.
            zoom: ZoomState::with_zoom_divisor(1.0, zoom_divisor),
            base_fov_deg: 70.0,
            base_sensitivity: 1.0,
            sprint_blend: 0.0,
            fov_inputs: FovModifierInputs::default(),
        };
        controller.apply_settings(fov_deg, sensitivity, zoom_divisor);
        controller
    }

    pub fn mode(&self) -> CameraMode {
        self.mode
    }

    pub fn cycle_mode(&mut self) -> CameraMode {
        self.mode = match self.mode {
            CameraMode::FirstPerson => CameraMode::ThirdPersonBack,
            CameraMode::ThirdPersonBack => CameraMode::ThirdPersonFront,
            CameraMode::ThirdPersonFront => CameraMode::FirstPerson,
        };
        self.mode
    }

    /// Apply live settings while retaining the current zoom transition.
    pub fn apply_settings(&mut self, fov_deg: f32, sensitivity: f32, zoom_divisor: f32) {
        self.base_fov_deg = finite_clamp(fov_deg, 70.0, 10.0, 120.0);
        self.base_sensitivity = finite_clamp(sensitivity, 1.0, 0.01, 10.0);
        self.zoom.set_zoom_divisor(zoom_divisor);
    }

    pub fn base_fov_deg(&self) -> f32 {
        self.base_fov_deg
    }

    pub fn base_sensitivity(&self) -> f32 {
        self.base_sensitivity
    }

    pub fn zoom_divisor(&self) -> f32 {
        self.zoom.zoom_divisor()
    }

    pub fn set_zoomed(&mut self, held: bool) {
        self.zoom.set_zoomed(held);
    }

    /// Disabling smoothing makes zoom snap at the next frame update.
    pub fn set_smooth_zoom(&mut self, enabled: bool) {
        self.zoom.set_smoothing_rate(if enabled {
            DEFAULT_ZOOM_SMOOTHING_RATE
        } else {
            10_000.0
        });
    }

    pub fn is_zoomed(&self) -> bool {
        self.zoom.is_zoomed()
    }

    /// Advance smooth zoom and the 10% sprint FOV transition using frame dt.
    pub fn tick(&mut self, dt: f32, sprinting: bool) -> f32 {
        self.zoom.tick(dt);
        let dt = finite_clamp(dt, 0.0, 0.0, 0.25);
        let target = if sprinting { 1.0 } else { 0.0 };
        let blend = 1.0 - (-SPRINT_FOV_SMOOTHING_RATE * dt).exp();
        self.sprint_blend += (target - self.sprint_blend) * blend;
        self.effective_fov_deg()
    }

    /// Effective vertical projection FOV: the zoom factor, the ordered
    /// vanilla `FovPipeline`, and the smoothed sprint layer on top.
    pub fn effective_fov_deg(&self) -> f32 {
        let modifier = compute_vanilla_fov_modifier(&self.fov_inputs)
            * (1.0 + SPRINT_FOV_BOOST * self.sprint_blend);
        let fov = self.base_fov_deg / self.zoom.zoom_factor() * modifier;
        if self.fov_inputs.spyglass_scoped {
            // Vanilla's scoped spyglass sits far below the gameplay FOV
            // floor (`0.1 ×` the setting), so it bypasses the 10°..120°
            // safety rail instead of clipping at 10°.
            return finite_clamp(fov, 10.0, 1.0, 120.0);
        }
        finite_clamp(fov, self.base_fov_deg, 10.0, 120.0)
    }

    /// Refresh the `getFieldOfViewModifier` environment consumed by
    /// [`Self::effective_fov_deg`]; the window samples it once per frame.
    pub fn set_fov_environment(&mut self, inputs: FovModifierInputs) {
        self.fov_inputs = inputs;
    }

    pub fn effective_sensitivity(&self) -> f32 {
        self.zoom.scaled_sensitivity(self.base_sensitivity)
    }

    /// Apply raw device mouse motion to the player's orientation.
    pub fn apply_mouse_look(&self, camera: &mut Camera, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        camera.apply_mouse_look(
            dx * f64::from(self.effective_sensitivity()),
            dy * f64::from(self.effective_sensitivity()),
        );
    }

    /// Derived render camera. Does not change player movement or network pose.
    /// Callers without a world (tests, spectator) get the unclipped
    /// distance; the live frame path routes through
    /// [`Self::clip_view_camera`] instead.
    pub fn view_camera(&self, player: &Camera) -> Camera {
        self.clip_view_camera(player, |_| false)
    }

    /// Vanilla `Camera.setup` for the third-person views: apply the
    /// `thirdPersonReverse` mirror first (180° yaw, negated pitch), then
    /// move backwards along the mirrored view axis by `Camera.getMaxZoom`
    /// so the camera stops at the wall instead of phasing through it.
    pub fn clip_view_camera(
        &self,
        player: &Camera,
        is_solid: impl Fn([i32; 3]) -> bool,
    ) -> Camera {
        let mut camera = *player;
        camera.fov_deg = self.effective_fov_deg();
        if self.mode == CameraMode::FirstPerson {
            return camera;
        }
        if self.mode == CameraMode::ThirdPersonFront {
            camera.yaw_deg = (camera.yaw_deg + 180.0).rem_euclid(360.0);
            camera.pitch_deg = -camera.pitch_deg;
        }
        // `move(-getMaxZoom(4.0), 0, 0)`: the pull-back axis is the camera's
        // own backward direction, which the mirror above already flipped for
        // the front view — the distance sign never changes.
        let look = camera.forward_vector();
        let back = [-look[0], -look[1], -look[2]];
        let zoom = camera_max_zoom(player.position, back, THIRD_PERSON_DISTANCE, is_solid);
        for (eye, direction) in camera.position.iter_mut().zip(back) {
            *eye += direction * zoom;
        }
        camera
    }
}

/// Voxel-cloud stratum count for the active quality ladder: `Fast`
/// trims to a single layer, `Balanced`/untuned settings keep the
/// pack's two-layer look, and `High`/`Ultra` — plus the cinematic
/// `MeguNatural` visual preset — earn a third stratum for parallax
/// depth. Clamped to the pack's `1..=3` contract.
fn cloud_layers_for_settings(settings: &UiSettings) -> u8 {
    if matches!(settings.visual.preset, VisualStylePreset::MeguNatural) {
        return 3;
    }
    match settings.video.preset {
        VideoPreset::Fast => 1,
        VideoPreset::Balanced | VideoPreset::Custom => 2,
        VideoPreset::High | VideoPreset::Ultra => 3,
    }
}

/// `F3 + I` entity target id: the rig registry name for a known entity id
/// and a deterministic placeholder for a registry gap, mirroring the
/// `unknown:item_<n>` convention on the item path.
fn debug_entity_target_name(live: &LiveSession, entity_id: i32) -> String {
    live.entity_rig(entity_id)
        .map(|spec| spec.name.to_owned())
        .unwrap_or_else(|| format!("unknown:entity_{entity_id}"))
}

/// `F3 + B` classification: living rigs draw vanilla's red eye-height
/// rectangle, while projectiles, dropped items, boats, and hanging entities
/// only get the plain white box.
fn entity_hitbox_is_living(archetype: EntityRigArchetype) -> bool {
    !matches!(
        archetype,
        EntityRigArchetype::ProjectileOrItem | EntityRigArchetype::VehicleOrHanging
    )
}

/// Assemble one frame of world-space debug lines: the night-sky drama
/// (when enabled and non-empty), `F3 + G` chunk borders around the player's
/// chunk, `F3 + B` entity hitboxes (vanilla white AABB + red living
/// eye-height rectangle + blue look ray), and the crosshair block outline.
/// Pure over the live session so the layout is testable without a window.
fn build_debug_world_lines(
    live: Option<&LiveSession>,
    drama: Option<&megu_client_mesh::tactile_effects::NightSkyDrama>,
    camera: &Camera,
    block_outline: Option<[i32; 3]>,
    show_hitboxes: bool,
    show_chunk_borders: bool,
    out: &mut Vec<DebugLine>,
) {
    out.clear();
    if let Some(drama) = drama {
        append_night_sky_drama_lines(drama, camera, out);
    }
    if let Some(live) = live {
        if show_chunk_borders {
            if let Some(pose) = live.render_frame_pose() {
                let chunk_x = (pose.feet[0].floor() as i32) >> 4;
                let chunk_z = (pose.feet[2].floor() as i32) >> 4;
                let min_y = live.world.dimension.min_y;
                let max_y = min_y
                    .saturating_add(live.world.dimension.height)
                    .saturating_sub(1);
                out.extend(build_chunk_border_lines(chunk_x, chunk_z, min_y, max_y));
            }
        }
        if show_hitboxes {
            for (eid, tracked) in &live.world.entities {
                if Some(*eid) == live.world.camera_entity_id {
                    continue;
                }
                let Some(spec) = live.entity_rig(*eid) else {
                    continue;
                };
                let (width, height) = (f64::from(spec.width), f64::from(spec.height));
                if width <= 0.0 || height <= 0.0 {
                    continue;
                }
                let aabb = megu_kernel::Aabb {
                    min: [
                        tracked.pos[0] - width / 2.0,
                        tracked.pos[1],
                        tracked.pos[2] - width / 2.0,
                    ],
                    max: [
                        tracked.pos[0] + width / 2.0,
                        tracked.pos[1] + height,
                        tracked.pos[2] + width / 2.0,
                    ],
                };
                let eye_y =
                    tracked.pos[1] + f64::from(live.world.entity_eye_height(*eid).unwrap_or(1.62));
                out.extend(build_entity_hitbox_lines(
                    aabb,
                    eye_y,
                    tracked.yaw_deg,
                    tracked.pitch_deg,
                    entity_hitbox_is_living(spec.archetype),
                ));
            }
        }
    }
    if let Some(pos) = block_outline {
        out.extend(crate::renderer::block_selection_outline_lines(pos));
    }
}

/// Project one tick of `NightSkyDrama` into world-space debug-line
/// segments: shooting stars streak along their start→end arc on a
/// shell inside the far plane, and each aurora band draws a short
/// undulating curtain arc centered on the north horizon, tint lerping
/// primary→secondary across the ribbon.
fn append_night_sky_drama_lines(
    drama: &megu_client_mesh::tactile_effects::NightSkyDrama,
    camera: &Camera,
    lines: &mut Vec<DebugLine>,
) {
    let shell = (camera.far * 0.8).clamp(80.0, 480.0);
    let eye = camera.position;
    let at = |dir: [f32; 3]| {
        [
            eye[0] + dir[0] * shell,
            eye[1] + dir[1] * shell,
            eye[2] + dir[2] * shell,
        ]
    };
    for star in &drama.shooting_stars {
        let head_t = star.progress.clamp(0.0, 1.0);
        let tail_t = (head_t - 0.15).max(0.0);
        let lerp_dir = |t: f32| {
            let mut d = [
                star.start_dir[0] + (star.end_dir[0] - star.start_dir[0]) * t,
                star.start_dir[1] + (star.end_dir[1] - star.start_dir[1]) * t,
                star.start_dir[2] + (star.end_dir[2] - star.start_dir[2]) * t,
            ];
            let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-5);
            d.iter_mut().for_each(|v| *v /= len);
            d
        };
        let alpha = (star.brightness.clamp(0.0, 1.0) * 255.0) as u8;
        let rgb = |scale: f32| {
            [
                (star.tint[0].clamp(0.0, 1.0) * 255.0 * scale) as u8,
                (star.tint[1].clamp(0.0, 1.0) * 255.0 * scale) as u8,
                (star.tint[2].clamp(0.0, 1.0) * 255.0 * scale) as u8,
            ]
        };
        lines.push(DebugLine {
            start: at(lerp_dir(tail_t)),
            end: at(lerp_dir(head_t)),
            rgba: [rgb(1.0)[0], rgb(1.0)[1], rgb(1.0)[2], alpha],
        });
    }
    const AURORA_SEGMENTS: usize = 14;
    for band in &drama.aurora_bands {
        let mut prev: Option<[f32; 3]> = None;
        for i in 0..=AURORA_SEGMENTS {
            let t = i as f32 / AURORA_SEGMENTS as f32;
            // Curtain arc centred on the north sky (-Z), ±55° azimuth,
            // elevation undulating slowly with the band's phase.
            let az = (-55.0 + 110.0 * t).to_radians();
            let alt = (band.altitude_deg + 2.5 * (band.phase + t * 4.0).sin()).to_radians();
            let dir = [az.sin() * alt.cos(), alt.sin(), -az.cos() * alt.cos()];
            let point = at(dir);
            if let Some(start) = prev {
                let mix = |a: f32, b: f32| ((a + (b - a) * t).clamp(0.0, 1.0) * 255.0) as u8;
                lines.push(DebugLine {
                    start,
                    end: point,
                    rgba: [
                        mix(band.primary_rgb[0], band.secondary_rgb[0]),
                        mix(band.primary_rgb[1], band.secondary_rgb[1]),
                        mix(band.primary_rgb[2], band.secondary_rgb[2]),
                        (band.intensity.clamp(0.0, 1.0) * 170.0) as u8,
                    ],
                });
            }
            prev = Some(point);
        }
    }
}

/// Vanilla yaw convention: `0` faces south (+Z), `90` west (−X),
/// `±180` north (−Z), `−90`/`270` east (+X). Returns the F3 cardinal
/// label + axis subtitle vanilla prints.
fn facing_from_yaw(yaw: f32) -> (&'static str, &'static str) {
    let sector = ((yaw % 360.0 + 360.0 + 45.0) % 360.0 / 90.0).floor() as i32;
    match sector {
        0 => ("south", "Towards positive Z"),
        1 => ("west", "Towards negative X"),
        2 => ("north", "Towards negative Z"),
        _ => ("east", "Towards positive X"),
    }
}

/// `VisualStylePreset` x `CloudMode` -> cloud deck layout: the Vanilla
/// preset (and `Clouds: Fast` under every preset) keeps the authentic
/// single cubic deck, while `Clouds: Fancy` under a Megu Natural style
/// opts into the three parallax hybrid decks. `Off` removes them all.
fn cloud_deck_mode(preset: VisualStylePreset, clouds: CloudMode) -> CloudDeckMode {
    match clouds {
        CloudMode::Off => CloudDeckMode::Off,
        CloudMode::Fast => CloudDeckMode::Vanilla,
        CloudMode::Fancy => match preset {
            VisualStylePreset::MeguNatural => CloudDeckMode::Hybrid,
            VisualStylePreset::Vanilla => CloudDeckMode::Vanilla,
        },
    }
}

fn finite_clamp(value: f32, fallback: f32, low: f32, high: f32) -> f32 {
    if value.is_finite() {
        value.clamp(low, high)
    } else {
        fallback
    }
}

/// Movement-speed attribute ratio behind the FOV pipeline: vanilla `speed`
/// adds `+20%` and `slowness` `-15%` per level as total multipliers, so the
/// ratio is `1 + 0.2·speedLevel - 0.15·slownessLevel` relative to the
/// ability's walking speed. Reads the HUD's effect mirror, i.e. the same
/// visible-effect set the rest of the interface reflects.
fn status_effect_speed_ratio(effects: &[megu_ui::state::LiveStatusEffect]) -> f32 {
    let mut ratio = 1.0f32;
    for effect in effects {
        let level = f32::from(effect.amplifier) + 1.0;
        match effect.effect_id {
            1 => ratio += 0.2 * level,
            2 => ratio -= 0.15 * level,
            _ => {}
        }
    }
    if ratio.is_finite() {
        ratio.max(0.0)
    } else {
        1.0
    }
}

/// `LivingEntity.getUseItem()` for the hand `startUseItem` resolves — the
/// stack behind the bow-draw and spyglass FOV rows.
fn session_use_item_name(live: &LiveSession) -> Option<&str> {
    let name = if live.use_hand() == LiveSession::OFF_HAND {
        live.hud.offhand.as_deref()
    } else {
        live.hud
            .hotbar
            .get(live.hud.selected_slot)
            .and_then(|slot| slot.as_deref())
    }?;
    (!name.is_empty()).then_some(name)
}

/// Dirty-only gate for the software-rasterized UI framebuffer: remembers
/// the `(width, height, fingerprint)` of the last uploaded frame so
/// `render_frame` skips `rasterize_draw_list` + the PCIe texture upload on
/// frames where the HUD did not change (the common case at 240+ FPS). The
/// counters let tests prove zero redundant uploads.
#[derive(Default)]
pub struct UiUploadCache {
    last_upload: Option<(u32, u32, u64)>,
    /// Frames that rasterized and uploaded (first frame, HUD change, resize).
    pub ui_uploads_performed: u64,
    /// Frames skipped because `(w, h, fingerprint)` matched the last upload.
    pub ui_uploads_skipped: u64,
}

impl UiUploadCache {
    /// `true` when this `(width, height, fingerprint)` differs from the
    /// last upload (or none has happened yet); records the key and counts
    /// the decision either way.
    pub fn should_upload(&mut self, width: u32, height: u32, fingerprint: u64) -> bool {
        let key = (width, height, fingerprint);
        if self.last_upload == Some(key) {
            self.ui_uploads_skipped += 1;
            return false;
        }
        self.last_upload = Some(key);
        self.ui_uploads_performed += 1;
        true
    }
}

/// GPU-side state that exists only once the window + surface are up.
struct ActiveWindow {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    renderer: GpuRenderer,
    surface_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
    surface_copy_src_supported: bool,
    title_branding_frame: Option<TitleBrandingFrame>,
    debug_overlay: FrameDebugOverlay,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TitleBrandingFrame {
    Hidden,
    Visible {
        viewport: [u32; 2],
        gui_scale: u32,
        font_revision: u64,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameDebugOverlay {
    /// `set_block_selection` owns the ring: only the crosshair outline.
    BlockOutline(Option<[i32; 3]>),
    /// `set_debug_lines` owns the ring: night-sky drama, `F3 + B`
    /// hitboxes, `F3 + G` chunk borders, and the crosshair outline.
    WorldLines,
}

#[derive(PartialEq, Eq)]
struct ModOverlayLayout {
    viewport: [u32; 2],
    scale: u32,
    font_revision: u64,
    sprites_loaded: bool,
}

/// Key set for FPS movement.
#[derive(Default)]
struct Keys {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    sprint: bool,
}

impl Keys {
    fn move_input(&self) -> MoveInput {
        let axis = |pos: bool, neg: bool| match (pos, neg) {
            (true, false) => 1.0,
            (false, true) => -1.0,
            _ => 0.0,
        };
        MoveInput {
            forward: axis(self.forward, self.back),
            strafe: axis(self.right, self.left),
            vertical: axis(self.up, self.down),
            sprint: self.sprint,
        }
    }
}

/// Commands the UI thread hands the friend-share worker.
enum FriendShareCommand {
    /// Rebuild the session allowlist — removed friends are kicked
    /// (their streams terminated) inside `update_allowlist`.
    ApplyAllowlist(FriendAllowlist),
    /// Revoke the invite and end the pump.
    Stop,
}

/// The worker's view back to the UI thread: the registered invite code,
/// currently open guest streams, and a sticky error when the relay
/// drops the session underneath us.
#[derive(Default)]
struct FriendShareShared {
    invite_code: Option<megu_proto::InviteCode>,
    /// `stream_id` → `(guest_name, verified guest megu-id)`.
    guests: BTreeMap<u32, (String, Option<String>)>,
    stopped: bool,
    last_error: Option<String>,
}

/// The `FRL3-FRIEND` host-share pump running on its own thread:
/// `FriendHostSession::recv_event` only returns per relay packet or per
/// `FRIEND_SHARE_TIMEOUT` read window, so between polls the worker
/// drains [`FriendShareCommand`]s and pumps guest↔world bytes over the
/// per-stream loopback TCP pipes it opens to the integrated server.
struct FriendShareWorker {
    commands: mpsc::Sender<FriendShareCommand>,
    shared: Arc<Mutex<FriendShareShared>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// A minted 128-bit one-off [`Frl3GuestToken`] bound to a friend's
/// static key — admits exactly that guest key with that token while the
/// grant lives (share-mode admission still applies to the key itself:
/// `allowlist.allows` checks key *and* token).
struct GuestLinkGrant {
    friend_pk: [u8; 32],
    token: Frl3GuestToken,
}

/// A precipitation hold with eased evaporation after rain stops. Server rain
/// is authoritative while active; the short dry-off ramp avoids instant
/// visual drying when the weather packet changes to clear.
#[derive(Clone, Copy, Debug, Default)]
struct RainWetnessRamp {
    wetness: f32,
    dry_start: f32,
    elapsed: f32,
    was_raining: bool,
}

impl RainWetnessRamp {
    fn update(&mut self, rain_level: f32, dt: f32) -> f32 {
        let rain = if rain_level.is_finite() {
            rain_level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if rain > 0.0 {
            self.wetness = rain;
            self.dry_start = rain;
            self.elapsed = 0.0;
            self.was_raining = true;
        } else {
            if self.was_raining {
                self.dry_start = self.wetness;
                self.elapsed = 0.0;
                self.was_raining = false;
            }
            self.elapsed = (self.elapsed
                + if dt.is_finite() {
                    dt.clamp(0.0, 1.0)
                } else {
                    0.0
                })
            .min(20.0);
            let t = self.elapsed / 20.0;
            let eased = t * t * (3.0 - 2.0 * t);
            self.wetness = self.dry_start * (1.0 - eased);
        }
        self.wetness
    }
}

#[cfg(test)]
mod rain_wetness_ramp_tests {
    use super::RainWetnessRamp;

    #[test]
    fn rain_stays_live_then_drains_smoothly_over_twenty_seconds() {
        let mut ramp = RainWetnessRamp::default();
        assert_eq!(ramp.update(1.0, 1.0 / 60.0), 1.0);
        let after_stop = (0..10).fold(0.0, |_, _| ramp.update(0.0, 1.0));
        assert!((after_stop - 0.5).abs() < 0.001);
        let fully_dry = (0..10).fold(0.0, |_, _| ramp.update(0.0, 1.0));
        assert_eq!(fully_dry, 0.0);
        assert_eq!(ramp.update(0.0, 1.0), 0.0);
    }
}

/// Deterministic content counters captured before/after a soft reload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SoftReloadSummary {
    pub blocks_delta: i64,
    pub recipes_delta: i64,
    pub loot_overrides: usize,
    pub translations_delta: i64,
    pub generation_id: Option<String>,
}

/// Result of an in-process mod/pack reload. `window_preserved` is a
/// contract marker: this path never destroys or recreates the `winit`
/// window, GPU device, surface, panorama or audio mixer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SoftReloadReport {
    pub summary: SoftReloadSummary,
    pub restarted_integrated_server: bool,
    pub window_preserved: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SoftReloadAssetSnapshot {
    block_keys: usize,
    recipes: usize,
    loot_overrides: usize,
    translation_keys: usize,
}

/// Localized one-line Diff UX #4 summary.
pub fn soft_reload_summary_line(
    summary: &SoftReloadSummary,
    lang: megu_ui::state::UiLang,
) -> String {
    let signed = |value: i64| {
        if value >= 0 {
            format!("+{value}")
        } else {
            value.to_string()
        }
    };
    if lang == megu_ui::state::UiLang::Ja {
        format!(
            "再読込: {} ブロック / {} レシピ / {} ja_jpキー",
            signed(summary.blocks_delta),
            signed(summary.recipes_delta),
            signed(summary.translations_delta)
        )
    } else {
        format!(
            "Reloaded: {} blocks / {} recipes / {} ja_jp keys",
            signed(summary.blocks_delta),
            signed(summary.recipes_delta),
            signed(summary.translations_delta)
        )
    }
}

/// Compact transactional profile diff used by rollback/apply toasts.
pub fn profile_activation_diff_line(
    diff: &crate::mod_profile_tx::ProfileActivationDiff,
    lang: megu_ui::state::UiLang,
) -> String {
    let added = diff.added_mods.len();
    let removed = diff.removed_mods.len();
    let updated = diff.updated_mods.len();
    let auto = diff.auto_fetched_deps.len();
    if lang == megu_ui::state::UiLang::Ja {
        format!("プロファイル差分: +{added} / -{removed} / 更新{updated} / 自動取得{auto}")
    } else {
        format!("Profile diff: +{added} / -{removed} / updated {updated} / auto-fetched {auto}")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlamoWorkKind {
    Install,
    Translate,
}

#[derive(Clone, Debug)]
enum PlamoWorkOutcome {
    Installed,
    Translated(crate::mod_translator::TranslationRunSummary),
}

struct PlamoWorker {
    kind: PlamoWorkKind,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<String>>,
    result: Arc<Mutex<Option<Result<PlamoWorkOutcome, String>>>>,
    thread: thread::JoinHandle<()>,
}

impl PlamoWorker {
    fn spawn(
        kind: PlamoWorkKind,
        cache_dir: std::path::PathBuf,
        mods_dir: std::path::PathBuf,
        resourcepacks_dir: std::path::PathBuf,
        preset: crate::mod_translator::Plamo3Preset,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(String::new()));
        let result = Arc::new(Mutex::new(None));
        let worker_cancel = Arc::clone(&cancel);
        let worker_progress = Arc::clone(&progress);
        let worker_result = Arc::clone(&result);
        let thread = thread::spawn(move || {
            let outcome = match kind {
                PlamoWorkKind::Install => crate::mod_translator::install_plamo3_model(
                    &cache_dir,
                    preset,
                    &worker_cancel,
                    |file, done, total| {
                        if let Ok(mut label) = worker_progress.lock() {
                            *label = match total {
                                Some(total) if total > 0 => {
                                    format!("{file}: {} / {} bytes", done.min(total), total)
                                }
                                _ => format!("{file}: {done} bytes"),
                            };
                        }
                    },
                )
                .map(|()| PlamoWorkOutcome::Installed),
                PlamoWorkKind::Translate => crate::mod_translator::translate_mods_with_plamo3(
                    &cache_dir,
                    &mods_dir,
                    &resourcepacks_dir,
                    preset,
                    &worker_cancel,
                    |done, total| {
                        if let Ok(mut label) = worker_progress.lock() {
                            *label = format!("translated entries: {done}/{total}");
                        }
                    },
                )
                .map(PlamoWorkOutcome::Translated),
            };
            if let Ok(mut slot) = worker_result.lock() {
                *slot = Some(outcome);
            }
        });
        Self {
            kind,
            cancel,
            progress,
            result,
            thread,
        }
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct MissingDependencyDownloadWorker {
    progress: Arc<Mutex<crate::mod_sync_auto::ModSyncProgress>>,
    result: Arc<Mutex<Option<Result<crate::mod_profile_tx::ProfileActivationDiff, String>>>>,
    cancel: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl MissingDependencyDownloadWorker {
    fn spawn(
        requirements: Vec<crate::mod_sync_auto::ExactModRequirement>,
        profile_root: std::path::PathBuf,
    ) -> Self {
        let progress = Arc::new(Mutex::new(crate::mod_sync_auto::ModSyncProgress::Resolving));
        let result = Arc::new(Mutex::new(None));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_progress = Arc::clone(&progress);
        let worker_result = Arc::clone(&result);
        let worker_cancel = Arc::clone(&cancel);
        let thread = thread::spawn(move || {
            let outcome = crate::mod_sync_auto::sync_exact_mods(
                &requirements,
                &profile_root,
                &worker_progress,
                &worker_cancel,
            );
            if let Ok(mut slot) = worker_result.lock() {
                *slot = Some(outcome);
            }
        });
        Self {
            progress,
            result,
            cancel,
            thread,
        }
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

struct MissingDependencyConsentWorker {
    result: Arc<Mutex<Option<Result<crate::mod_sync_auto::MissingDependencyConsentPlan, String>>>>,
}

impl MissingDependencyConsentWorker {
    fn spawn(descriptors: Vec<crate::mod_sync_auto::MissingDependencyDescriptor>) -> Self {
        let result = Arc::new(Mutex::new(None));
        let worker_result = Arc::clone(&result);
        thread::spawn(move || {
            let plan = crate::mod_sync_auto::build_missing_dep_consent_plan(&descriptors);
            if let Ok(mut slot) = worker_result.lock() {
                *slot = Some(plan);
            }
        });
        Self { result }
    }
}

/// Delayed asset bootstrap: resolving the per-version metadata URL is itself
/// network I/O, so a daemon thread performs manifest -> version JSON first
/// and only then starts `vanilla_bootstrap`'s bounded asset worker.
struct DeferredVanillaAssetsBootstrap {
    shared: Arc<Mutex<crate::vanilla_bootstrap::AssetBootstrapProgress>>,
}

impl DeferredVanillaAssetsBootstrap {
    fn progress(&self) -> crate::vanilla_bootstrap::AssetBootstrapProgress {
        self.shared
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    fn spawn(version_id: &str, paths: crate::live_session::SessionPaths) -> Option<Self> {
        if version_id.is_empty()
            || version_id.len() > 64
            || !version_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return None;
        }
        let shared = Arc::new(Mutex::new(
            crate::vanilla_bootstrap::AssetBootstrapProgress::FetchingIndex,
        ));
        let worker_shared = Arc::clone(&shared);
        let version = version_id.to_owned();
        thread::Builder::new()
            .name("megu-vanilla-assets-resolve".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    deferred_assets_bootstrap_run(&version, &paths, &worker_shared)
                }));
                if outcome.is_err() {
                    set_deferred_asset_progress(
                        &worker_shared,
                        crate::vanilla_bootstrap::AssetBootstrapProgress::Failed(
                            "asset metadata resolver crashed".into(),
                        ),
                    );
                }
            })
            .ok()?;
        Some(Self { shared })
    }
}

fn set_deferred_asset_progress(
    shared: &Mutex<crate::vanilla_bootstrap::AssetBootstrapProgress>,
    progress: crate::vanilla_bootstrap::AssetBootstrapProgress,
) {
    *shared.lock().unwrap_or_else(|err| err.into_inner()) = progress;
}

fn mojang_metadata_url_allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    matches!(
        host.as_str(),
        "piston-meta.mojang.com" | "launchermeta.mojang.com" | "launcher.mojang.com"
    )
}

fn fetch_mojang_text(url: &str, cap: usize) -> Result<String, String> {
    if !mojang_metadata_url_allowed(url) {
        return Err("refusing non-Mojang metadata URL".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(10)))
        .timeout_recv_body(Some(Duration::from_secs(10)))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| format!("metadata request failed: {e}"))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(cap as u64)
        .read_to_vec()
        .map_err(|e| format!("read metadata: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "metadata is not UTF-8".into())
}

fn manifest_version_url_window(manifest_text: &str, version_id: &str) -> Result<String, String> {
    let json = megu_core::json::parse_json(manifest_text)
        .map_err(|e| format!("invalid version manifest: {e}"))?;
    let versions = json
        .get("versions")
        .and_then(megu_core::json::JsonValue::as_array)
        .ok_or_else(|| "version manifest has no versions array".to_owned())?;
    for entry in versions {
        if entry.get("id").and_then(megu_core::json::JsonValue::as_str) == Some(version_id) {
            let url = entry
                .get("url")
                .and_then(megu_core::json::JsonValue::as_str)
                .ok_or_else(|| "version entry has no url".to_owned())?;
            if !mojang_metadata_url_allowed(url) {
                return Err("version metadata URL is not allowlisted".into());
            }
            return Ok(url.to_owned());
        }
    }
    Err(format!(
        "version {version_id:?} is absent from the Mojang manifest"
    ))
}

fn local_version_json_url(
    paths: &crate::live_session::SessionPaths,
    version_id: &str,
) -> Option<Result<(String, String), String>> {
    let game_dir = paths.game_dir.as_deref()?;
    let path = game_dir
        .join("versions")
        .join(version_id)
        .join(format!("{version_id}.json"));
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        crate::vanilla_bootstrap::parse_asset_index_ref(&text)
            .map(|index| (index.id, "local-version-json".to_owned()))
            .map_err(|err| err.to_string()),
    )
}

fn deferred_assets_bootstrap_run(
    version_id: &str,
    paths: &crate::live_session::SessionPaths,
    shared: &Mutex<crate::vanilla_bootstrap::AssetBootstrapProgress>,
) {
    let resolved = match local_version_json_url(paths, version_id) {
        Some(Ok((index_id, version_url))) => Ok((index_id, version_url)),
        Some(Err(err)) => Err(err),
        None => {
            let manifest = fetch_mojang_text(
                crate::vanilla_bootstrap::VANILLA_MANIFEST_URL,
                4 * 1024 * 1024,
            )
            .and_then(|text| manifest_version_url_window(&text, version_id));
            manifest.and_then(|version_url| {
                fetch_mojang_text(&version_url, 1024 * 1024).and_then(|version_text| {
                    crate::vanilla_bootstrap::parse_asset_index_ref(&version_text)
                        .map(|index| (index.id, version_url))
                })
            })
        }
    };
    let (index_id, version_url) = match resolved {
        Ok(resolved) => resolved,
        Err(err) => {
            set_deferred_asset_progress(
                shared,
                crate::vanilla_bootstrap::AssetBootstrapProgress::Failed(err),
            );
            return;
        }
    };
    // A local launcher JSON has no remote URL to re-read; the public worker
    // requires the canonical URL for provenance, so use the manifest chain in
    // that rare case. Most cache-only installs take the remote path above.
    let version_url = if version_url == "local-version-json" {
        match fetch_mojang_text(
            crate::vanilla_bootstrap::VANILLA_MANIFEST_URL,
            4 * 1024 * 1024,
        )
        .and_then(|text| manifest_version_url_window(&text, version_id))
        {
            Ok(url) => url,
            Err(err) => {
                set_deferred_asset_progress(
                    shared,
                    crate::vanilla_bootstrap::AssetBootstrapProgress::Failed(err),
                );
                return;
            }
        }
    } else {
        version_url
    };
    let Some(inner) = crate::vanilla_bootstrap::spawn_vanilla_assets_bootstrap(
        &index_id,
        &version_url,
        paths,
        crate::vanilla_bootstrap::AssetTierFilter::All,
    ) else {
        set_deferred_asset_progress(
            shared,
            crate::vanilla_bootstrap::AssetBootstrapProgress::Failed(
                "asset bootstrap endpoints failed validation".into(),
            ),
        );
        return;
    };
    loop {
        let progress = inner.progress();
        let terminal = progress.is_terminal();
        set_deferred_asset_progress(shared, progress);
        if terminal {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The `ApplicationHandler` `EventLoop::run_app` drives.
pub struct GpuApp {
    config: GpuCliConfig,
    live: Option<LiveSession>,
    rain_wetness: RainWetnessRamp,
    active: Option<ActiveWindow>,
    camera: Camera,
    camera_controller: CameraController,
    spectator_view: Option<SpectatorView>,
    keys: Keys,
    input_router: WindowInputRouter,
    debug_messages: Vec<String>,
    focused: bool,
    occluded: bool,
    scale_factor: f64,
    cursor_grabbed: bool,
    paused: bool,
    show_diagnostics: bool,
    mouse_pos: (i32, i32),
    left_mouse_held: bool,
    right_mouse_held: bool,
    /// Vanilla `Minecraft.rightClickDelay` — the 4-tick gate that spaces
    /// repeated uses while the right button stays held.
    right_click_delay: RightClickDelay,
    /// Fractional 20 Hz accumulator folding the frame clock into
    /// [`RightClickDelay`] ticks and the held-use clock.
    use_repeat_accum: f32,
    /// Seconds the use button has been held — the
    /// `LivingEntity.getTicksUsingItem()` behind the bow-draw FOV row.
    use_hold_secs: f32,
    /// `GameRenderer.bobHurt` tilt the render camera carries this frame.
    hurt_tilt: HurtTilt,
    gui_modifiers: u8,
    gui_left_held: bool,
    gui_right_held: bool,
    last_container_click: Option<(Instant, i16)>,
    /// `F3 + H` state the container/inventory slots were last filled with —
    /// a toggle has to re-decorate already-rendered stacks, and the
    /// `state_id` gate alone would keep the old tooltips.
    container_tooltip_advanced: bool,
    screenshot_pending: bool,
    screenshot_sequence: u64,
    container_screen: Option<ContainerScreenState>,
    container_suspended_for_recipe: bool,
    creative_inventory: Option<megu_ui::creative_screen::CreativeInventoryState>,
    creative_carried: Option<megu_ui::creative_screen::CreativeItem>,
    book_editor: Option<megu_ui::gameplay_screens::BookEditorState>,
    credits_screen: Option<megu_ui::gameplay_screens::EndCreditsState>,
    credits_ack_pending: bool,
    merchant_offer_signature: Option<(u8, u64)>,
    vanilla_particles: VanillaParticleWorld,
    /// `container_screen` currently holds the client-side window-0 player
    /// inventory opened by `E` (not a server `open_screen`).
    own_inventory_open: bool,
    /// Last `live.dropped_container_clicks` value the container surface was
    /// reconciled against — a bump means a predicted click never reached the
    /// worker and the slots must be restored from authority.
    seen_container_click_drops: u64,
    /// `L`-key advancements overlay; opening announces the tab via
    /// `seen_advancements`, closing retracts it.
    advancements_open: bool,
    chat_bar: Option<ZshCommandBar>,
    /// Latest `complete_suggestions` hit for the open chat bar's `/`-input.
    chat_suggestions: Vec<String>,
    /// `command_suggestion` transaction id the last Tab press sent —
    /// replies carrying any other id are stale and ignored.
    pending_suggestion_tx: Option<i32>,
    /// Revision counters already folded into `chat_bar` from the live
    /// session's suggestion/completion mirrors.
    seen_suggestions_rev: u64,
    seen_completions_rev: u64,
    /// Fractional-tick accumulator driving `LiveSession::tick_item_cooldowns`
    /// at 20 Hz from the frame clock.
    cooldown_tick_accum: f32,
    /// Physical Shift state while the chat bar owns the keyboard —
    /// Shift+Tab cycles completions backwards (kept off `keys.down`
    /// so sneak edge tracking stays untouched by chat input).
    chat_shift_held: bool,
    /// Settings as last loaded/saved through `client-app`
    /// (`megu-client.toml` under `--gameDir`, `.` when unset) — the base
    /// `ui_to_client_settings` overlays the UI's mapped keys onto so
    /// file-only fields (shader pack, report endpoint) survive a save.
    persisted_settings: megu_client::settings::ClientSettings,
    active_shaderpack_path: Option<String>,
    /// Where [`persisted_settings`](Self::persisted_settings) writes.
    settings_path: std::path::PathBuf,
    /// Debounced atomic writer for `persisted_settings` — dirty-only,
    /// byte-compare, temp-file + rename (`SettingsSaveGuard`).
    settings_save_guard: megu_client::settings::SettingsSaveGuard,
    /// OS clipboard backend + in-process fallback (see
    /// [`crate::clipboard`]): `CopyInvite`/`Ctrl+C` writes and `Ctrl+V`
    /// reads land here, degrading to the in-app buffer when no display
    /// or clipboard tool exists.
    clipboard: SystemClipboard,
    /// Last delivered `ModifiersChanged` state — `Ctrl` (and `Cmd` on
    /// macOS) gates the copy/paste chords on text surfaces.
    modifiers: ModifiersState,
    /// Live IME composition state from `Ime::Enabled`/`Preedit`/`Commit`/
    /// `Disabled` — preedit text + clamped cursor byte range plus the
    /// candidate-window anchor rect pushed through
    /// `set_ime_cursor_area`. Reset on disable/focus loss so a stale
    /// composition can never double-insert.
    ime_state: crate::linux_desktop::ImeCompositionState,
    /// Last `set_ime_allowed` pushed to the window: `true` while a text
    /// surface owns typing, `false` during raw WASD gameplay so the IME
    /// can never hijack movement keys.
    ime_allowed: bool,
    /// Platforms that echo `Ime::Commit` again inside the following
    /// `KeyEvent.text` would double-insert; the commit string + instant
    /// is recorded so the keyboard path can swallow the echo.
    last_ime_commit: Option<(String, Instant)>,
    /// Tiling-WM focus/grab recovery policy seeded from the detected
    /// [`crate::linux_desktop::LinuxCompositor`]: focus loss and pointer
    /// exit release the grab cleanly, focus gain / pointer re-entry
    /// re-arm it in the compositor's preferred `CursorGrabMode` order
    /// (Hyprland/Sway/Niri/COSMIC lock first; X11 WMs and Gamescope
    /// confine first).
    tiling_grab_policy: crate::linux_desktop::TilingGrabPolicy,
    /// Last `WindowEvent::DroppedFile` payload, staged for the alpha.4
    /// `DropPayloadClassifier` wiring — recorded plus surfaced as a
    /// localized status notice, never silently executed.
    last_dropped_file: Option<std::path::PathBuf>,
    /// QoL container sorter armed by the `SortInventory` keybind.
    inventory_sort: InventorySortState,
    /// Launcher filesystem paths handed to every `LiveSession` this app
    /// spawns (`--gameDir`/`--assetsDir`/`--assetIndex`/`--version`/
    /// `--resourcePackDir`).
    session_paths: SessionPaths,
    /// Confirmed, non-blocking server-scoped Modrinth synchronization.
    mod_sync_worker: Option<crate::mod_sync_auto::ModSyncWorker>,
    mod_sync_generation: u64,
    shader_options_open: bool,
    /// Exact-candidate missing-dependency resolver; network resolution is
    /// always off-thread and only its result fills the consent UI.
    missing_dep_worker: Option<MissingDependencyConsentWorker>,
    /// Exact resolved candidates backing the visible consent rows. Kept
    /// host-side because the UI deliberately receives only display-safe
    /// fields; approval must reuse these project/version ids rather than
    /// resolving a second, potentially different Modrinth candidate set.
    missing_dep_plan: Option<crate::mod_sync_auto::MissingDependencyConsentPlan>,
    /// Background PLaMo install/translation worker (the model process itself
    /// remains transient inside `mod_translator`).
    plamo_worker: Option<PlamoWorker>,
    /// Foreground transactional download of consent-approved dependencies.
    missing_dep_download_worker: Option<MissingDependencyDownloadWorker>,
    /// Explicit resource-pack stack in bottom-to-top order. Entries are file
    /// names under `resourcepacks/` or official built-in ids; it mirrors
    /// `options.txt` and lets the generated translation pack flip instantly.
    resource_pack_selection: Vec<String>,
    /// Last display locale whose vanilla/lang asset tables were loaded.
    active_ui_locale: String,
    /// Content snapshot used to produce Diff UX #4 counters.
    soft_reload_asset_baseline: Option<SoftReloadAssetSnapshot>,
    /// Singleplayer world currently hosted by this process, when known.
    active_singleplayer_world: Option<String>,
    /// Root fingerprint of the last MSY1 mismatch already surfaced.
    last_mod_sync_root_fp: Option<u64>,
    /// Asset-store bootstrap for cache-only vanilla installs (P1.1).
    vanilla_assets_bootstrap: Option<DeferredVanillaAssetsBootstrap>,
    /// True after a cache-only asset bootstrap has been requested once.
    vanilla_assets_bootstrap_attempted: bool,
    last_server_address: Option<String>,
    /// Boot-time recipe-browser ingest report (mod JARs + vanilla client
    /// JAR + datapacks); empty when no `--gameDir` was supplied.
    recipe_ingest: crate::recipe_ingest::IngestReport,
    /// CPU-side `item_id → uv` map for `UiCommand::ItemIcon`
    /// tessellation; the RGBA payload is uploaded to the renderer once
    /// and dropped (VRAM owns texels, matching the block atlas).
    item_icon_atlas: megu_ui::draw::ItemIconAtlas,
    /// `true` once `item_icon_atlas.rgba` reached the renderer — upload
    /// happens once per asset-bundle build, never per frame.
    item_icon_atlas_uploaded: bool,
    /// `player_inventory_state_id` the browser's `inventory` mirror was
    /// last rebuilt from — `-1` forces a first sync.
    recipe_inventory_state: i32,
    /// Hit regions produced by the in-container recipe overlay's last
    /// `render_overlay` call — click/hover routing reads this each frame.
    recipe_overlay_regions: Vec<megu_ui::recipe_browser::RecipeHitRegion>,
    /// Locale the lang ingest currently covers — a language change
    /// re-runs `ingest_lang_into_ui` once so localized item names and
    /// search follow the new display language.
    lang_ingested_locale: String,
    /// Clean-room runtime `MEGU EDITION` title branding (wordmark +
    /// patched edition strip, merged splash pool) loaded once at boot —
    /// the built-in splash set with no banner when no local client JAR
    /// is discoverable.
    title_branding: TitleBrandingAssets,
    /// Session-deterministic splash pick seed (FNV-1a over the launch
    /// config — never `SystemTime`).
    splash_seed: u64,
    /// Integrated `megu` server guard while a singleplayer world runs.
    singleplayer_server: Option<integrated::SingleplayerServer>,
    /// Friend-relay host session while an invite code is live.
    friend_host: Option<FriendHostSession>,
    /// Friend-relay guest bridge while joined through an invite code.
    friend_bridge: Option<FriendGuestBridge>,
    /// `FRL3` device identity from `<gameDir>/megu-identity.key` — the
    /// 256-bit static key behind the player's `megu-id:`.
    frl_identity: Option<Frl3Identity>,
    /// `megu-friends.toml` mirror: share mode, saved relay, allowlist.
    friends_store: Option<FriendsStore>,
    /// Live `FRL3-FRIEND` host-share worker while Host World Sharing is
    /// on; owns the relay session and the per-guest loopback pipes.
    friend_share: Option<FriendShareWorker>,
    /// The minted 128-bit guest token bound to one friend's allowlist
    /// slot for the current share session; cleared when sharing stops.
    guest_link_grant: Option<GuestLinkGrant>,
    /// `(host megu-id, six-digit SAS)` of the latest friend join, for
    /// the Friends screen's pairing readout.
    friend_join_sas: Option<(String, u32)>,
    /// Background vanilla client-JAR bootstrap (manifest fetch +
    /// SHA-1-verified download) spawned at boot when no local JAR was
    /// found; polled non-blockingly from the event loop.
    vanilla_bootstrap: Option<crate::vanilla_bootstrap::VanillaJarBootstrap>,
    server_blocks_bootstrap: Option<crate::vanilla_bootstrap::VanillaJarBootstrap>,
    /// `true` once the bootstrap was spawned (or deliberately skipped) so
    /// a `Failed` download is not retried on every surface re-creation.
    bootstrap_attempted: bool,
    /// Version id the bootstrap targets — progress/status text.
    bootstrap_version: String,
    /// The exact progress line this app last placed in
    /// `ui.title_status`, so a real `status_line` message is never
    /// overwritten by download progress.
    bootstrap_banner: Option<String>,
    /// Last live `title_revision` applied to `ui.hud.title_overlay`.
    title_seen_rev: u64,
    /// Fractional-tick accumulator driving `TitleOverlayState::tick` at 20/s.
    title_tick_accum: f32,
    /// Packet-revision-gated hotbar and boss-bar styling alongside the
    /// legacy HUD state. Cleared on attach, even if revisions coincide.
    live_hud_metadata: Option<LiveHudMetadata>,
    live_ui_locale: Option<megu_core::i18n::BuiltinLocale>,
    /// Dirty-checked cache for the generic block-inspector HUD card so steady
    /// frames skip string formatting and layout rebuilds completely.
    inspector_hud_cache: Option<Box<BlockInspectorHudCache>>,
    ui: UiState,
    /// Revision bus feeding the retained UI cache: the window bumps a
    /// `UiLayer` only when that layer's draw-list fingerprint moved, so
    /// a chat tick never re-tessellates the container layer.
    ui_layer_bus: megu_core::rev::UiLayerBus,
    /// Per-layer retained tessellation cache — replaces the old
    /// fingerprint-gated whole-list re-tessellate. See
    /// [`UiRetainedCache`](megu_ui::retained_cache::UiRetainedCache) for
    /// the zero-alloc/zero-upload contract.
    ui_retained: megu_ui::retained_cache::UiRetainedCache,
    /// Last fingerprint per `UiLayer` index — the host-side change
    /// detector that drives `ui_layer_bus` bumps.
    ui_layer_fps: [u64; 7],
    /// Retained layer lists composited beside `ui_draw_list` each frame:
    /// mod HUD (goggles, magic HUD, hotbar glint), vanilla overlay
    /// (container, recipe browser sidebar, advancements, container glint) and the
    /// mod-GUI bridge batch.
    ui_modhud_list: UiDrawList,
    ui_overlay_list: UiDrawList,
    ui_modoverlay_list: UiDrawList,
    /// Bridge and manifest setters and sprite-atlas reloads invalidate
    /// the retained ModOverlay command stream.
    mod_overlay_layout: Option<ModOverlayLayout>,
    /// Decoded vanilla `widget/*` sprite atlas re-keyed to the contract
    /// `SpriteId` table — empty on clean-room installs (the
    /// `vanilla_widgets` vector chrome then draws everything). Uploaded
    /// to GPU `ui_quad` binding 5 once per JAR arrival.
    gui_sprite_atlas: megu_ui::draw::GuiSpriteAtlas,
    /// Complete decoded local 1.21.1 asset bundle, retained for retries,
    /// translations, player cosmetics and credits text.
    vanilla_runtime_assets: Option<crate::vanilla_assets_bridge::VanillaRuntimeAssets>,
    vanilla_official_extras: Option<VanillaOfficialExtras>,
    entity_appearance: Option<RuntimeEntityAppearanceAtlas>,
    /// `(uuid, content fingerprint)` of the arm-skin RGBA last pushed to
    /// the first-person viewmodel pass. The fingerprint dedups the
    /// per-frame skin refresh without depending on remote-player
    /// appearance revisions.
    viewmodel_arm_skin_uploaded: Option<([u8; 16], u64)>,
    /// `InteractionHand` ordinal (`LiveSession::MAIN_HAND`/`OFF_HAND`)
    /// of the most recent local arm swing and item use. The session keeps
    /// one shared animation clock, so the window attributes it to the
    /// hand that actually acted and leaves the other arm at rest.
    viewmodel_swing_hand: i32,
    viewmodel_item_use_hand: i32,
    player_skin_cache: Option<crate::player_skins::PlayerSkinCache>,
    /// `true` once the JAR scan for `gui_sprite_atlas` ran — bootstrap
    /// completion re-arms it.
    gui_sprites_loaded: bool,
    render_config: RenderConfig,
    policy: DisplayPresentationPolicy,
    governor: FrameBudgetGovernor,
    /// User-requested shadow quality / render scale, restored when the
    /// governor steps back to `ThrottleStage::Full`.
    user_shadow_quality: BuiltinShadowQuality,
    user_render_scale: RenderScalePreset,
    /// Renderer upload budget captured at init; `TrimRebuilds` swaps in a
    /// 1-section / 32 KiB cap and `Full` restores this baseline.
    baseline_upload_budget: Option<ChunkUploadBudget>,
    /// Throttled host-RAM probe feeding the live OOM guard
    /// (`UiSettings.video.ram_budget_mib`): `sample_host_metrics` is
    /// fail-safe — unsupported platforms simply report `None` fields
    /// and the guard idles.
    host_ram_probe: CpuSampler,
    /// Frame-time accumulator gating the ~2 Hz host-metrics resample.
    host_ram_sample_accum_ms: f32,
    /// Latest F3 overlay contents and when they were built — refreshed
    /// at most every `F3_REFRESH` so steady frames never re-populate
    /// it; the cached copy is mirrored into `ui.hud.debug_overlay`
    /// every frame so `sync_hud_to`'s wholesale clone can't stomp it.
    f3_overlay: F3DebugOverlay,
    f3_overlay_built: Option<Instant>,
    /// Host-metrics probe for the F3 `Mem:` line — the overlay's 10 Hz
    /// cadence keeps `/proc` reads cheap.
    f3_metrics_probe: CpuSampler,
    /// `true` once the local-Minecraft font probe produced a
    /// `GlyphAtlas` components installed; each stays retryable until its
    /// own late bootstrap asset arrives.
    vanilla_ascii_loaded: bool,
    vanilla_unifont_loaded: bool,
    /// Mouse deltas accumulated while the loop slept, applied once
    /// immediately before `render_frame` derives the view camera — the
    /// late-latch: input sampled as close to the draw as possible.
    pending_mouse_delta: (f64, f64),
    /// Armed whenever a session attaches: the next live frame on any
    /// `GpuRenderer` that has not seen a full strategy push applies the
    /// session's connect-time `ComputeStrategy` (GPU indirect/Hi-Z/MDI
    /// on capable adapters, fail-closed elsewhere).
    live_strategy_pending: bool,
    /// Pooled HUD draw list — `UiState::render_into` refills it in place
    /// so steady frames allocate nothing (text payloads recycle inside
    /// the list).
    ui_draw_list: UiDrawList,
    /// Active Tier-2 mod GUI batch (`client-ui` `gui_bridge` ABI): a
    /// client mod records its draw ops here and the HUD pass lowers them
    /// straight into `ui_draw_list`. `Default` is empty, so the hook is
    /// free until a mod host feeds it through [`Self::set_gui_bridge_batch`].
    gui_bridge_batch: GuiBridgeBatch,
    /// Declarative mod-UI manifest (`megu_core::ui_contract::ModUiManifest`)
    /// rendered through `client-ui`'s shared Vanilla widget kit into the
    /// `ModOverlay` retained layer — `None` when no mod manifest is up.
    mod_ui_manifest: Option<megu_core::ui_contract::ModUiManifest>,
    /// Monotonic epoch for time-driven 2D HUD shimmer (enchantment
    /// glint): `Instant`, never `SystemTime`, so every overlay stays
    /// deterministic under replay and headless tests.
    ui_anim_epoch: Instant,
    /// Per-frame live-particle staging scratch (tactile + ambience + VFX1
    /// billboards) — cleared and refilled only while particles exist.
    particle_scratch: Vec<VanillaParticleQuad>,
    subworld_light_scratch: Vec<(u64, u8, u8)>,
    debug_line_scratch: Vec<DebugLine>,
    /// `true` while the renderer's particle ring holds a non-empty
    /// upload — an all-clear frame stages `[]` once, then stays free.
    live_particles_staged: bool,
    /// Live entity + block-entity mesh staging cache (20 Hz throttle —
    /// the server tick cadence positions actually move at).
    entity_render_cache: LiveEntityRenderCache,
    /// Phase-1 seamless dimension transfer (`megu:portal_seamless_v1`)
    /// client state machine: standby prefetch + committed crossfade.
    /// Fed by [`Self::apply_seamless_portal_channel`], advanced by the
    /// monotonic frame delta, and consumed by the render path for the
    /// sky/fog blend and the loading-overlay suppression.
    seamless_portal: SeamlessDimTransitionState,
    /// Timestamp of the previous physical Space press — the vanilla
    /// ~7-tick double-jump window used by `LiveSession::toggle_flight`
    /// (key repeats are filtered upstream, so every `Pressed` is real).
    last_space_press: Option<Instant>,
    /// Last helm frame pushed to the session's worker — dispatch is
    /// change-only (a released stick is one zeroed frame, not a stream)
    /// and unbinding emits a single control release.
    last_subworld_helm: Option<SubWorldHelmCmd>,
    last_frame: Instant,
    frame_start: Instant,
    fatal: Option<String>,
}

/// Live entity + block-entity render cache: rig meshes keyed by entity
/// id + pose fingerprint, one canonical template per BER family, and the
/// throttle/staging bookkeeping that keeps uploads off the hot path.
struct LiveEntityRenderCache {
    /// Pose-fingerprint-keyed rig meshes; rebuilt only when an entity's
    /// spec or pose actually changes.
    entity_meshes: std::collections::HashMap<i32, (u64, ExtrudedMesh)>,
    entity_visuals:
        std::collections::HashMap<i32, (u64, crate::entity_appearance::EntityVisualLayers)>,
    entity_interpolation: std::collections::HashMap<i32, EntityInterpolation>,
    dynamic_ber_meshes:
        std::collections::HashMap<[i32; 3], (VanillaBerExtendedState, ExtrudedMesh)>,
    ber_animation_easing: std::collections::HashMap<[i32; 3], f32>,
    /// Canonical template per [`VanillaBerKind`] — the default states are
    /// per-kind constants, so every placed block entity of a family
    /// shares one mesh.
    ber_meshes: [Option<ExtrudedMesh>; BER_KIND_SLOTS],
    /// Per-refresh scratch holding the converted live contact-shadow
    /// quads (`mesh`, folded rgb tint) so their `&ExtrudedMesh` borrows
    /// outlive the staging call — empty while `entity_shadows` is off.
    contact_shadow_quads: Vec<(ExtrudedMesh, [f32; 3])>,
    /// Earliest next rebuild instant (20 Hz — the server tick cadence).
    next_refresh: Option<Instant>,
    /// `true` while the renderer's entity/BER ring holds a non-empty
    /// upload — a fully empty world still clears the ring once.
    staged: bool,
}

impl Default for LiveEntityRenderCache {
    fn default() -> Self {
        Self {
            entity_meshes: std::collections::HashMap::new(),
            entity_visuals: std::collections::HashMap::new(),
            entity_interpolation: std::collections::HashMap::new(),
            dynamic_ber_meshes: std::collections::HashMap::new(),
            ber_animation_easing: std::collections::HashMap::new(),
            ber_meshes: std::array::from_fn(|_| None),
            contact_shadow_quads: Vec::new(),
            next_refresh: None,
            staged: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct EntityInterpolation {
    initialized: bool,
    position: [f64; 3],
    target_position: [f64; 3],
    body_yaw: f32,
    head_yaw: f32,
    pitch: f32,
    hurt_ticks: f32,
    limb_swing: f32,
    limb_swing_amount: f32,
}

/// Render-only pose. A server camera packet must not move the local
/// simulation, prediction origin, or outbound player pose.
#[derive(Clone, Copy)]
struct SpectatorView {
    entity: (i32, [u8; 16]),
    eye: [f64; 3],
    yaw: f32,
    pitch: f32,
}

impl SpectatorView {
    fn follow(previous: Option<Self>, target: &TrackedEntity, eye_height: f32, dt: f32) -> Self {
        let mut pose = Self {
            entity: (target.eid, target.uuid),
            eye: [
                target.pos[0],
                target.pos[1] + f64::from(eye_height),
                target.pos[2],
            ],
            yaw: target.yaw_deg,
            pitch: (target.pitch_deg + 180.0).rem_euclid(360.0) - 180.0,
        };
        if let Some(previous) = previous.filter(|p| p.entity == pose.entity) {
            let delta = std::array::from_fn::<_, 3, _>(|i| pose.eye[i] - previous.eye[i]);
            // Disconnects, entity switches and large server teleports cut
            // immediately. Regular movement eases over a server tick.
            if delta.iter().map(|v| v * v).sum::<f64>() <= 64.0 {
                let a = 1.0 - (-dt.clamp(0.0, 0.25) / 0.05).exp();
                pose.eye = std::array::from_fn(|i| previous.eye[i] + delta[i] * f64::from(a));
                pose.yaw = interpolate_angle(previous.yaw, pose.yaw, a);
                pose.pitch = previous.pitch + (pose.pitch - previous.pitch) * a;
            }
        }
        pose
    }
}

fn interpolate_angle(current: f32, target: f32, amount: f32) -> f32 {
    let delta = (target - current + 180.0).rem_euclid(360.0) - 180.0;
    (current + delta * amount).rem_euclid(360.0)
}

/// Remote-entity pose smoothing rate in `1/s`. The previous implementation
/// blended a constant `0.5` per 20 Hz refresh; `ln(2) / 0.05 s` keeps that
/// exact feel while making the easing independent of the refresh cadence
/// and frame rate (`exp_decay_exact`).
const ENTITY_SMOOTH_RATE: f64 = std::f64::consts::LN_2 / 0.05;

fn interpolate_entity(
    cache: &mut LiveEntityRenderCache,
    eid: i32,
    target_position: [f64; 3],
    tracked: Option<&TrackedEntity>,
    dt: f32,
) -> EntityInterpolation {
    let sample = cache.entity_interpolation.entry(eid).or_default();
    let target_limb_swing_amount = tracked.map_or(0.0, |entity| {
        let speed = entity.vel[0].hypot(entity.vel[2]);
        if speed.is_finite() {
            (speed as f32 * 4.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    });
    let target = EntityInterpolation {
        initialized: true,
        position: target_position,
        target_position,
        body_yaw: tracked.map_or(0.0, |entity| entity.yaw_deg),
        head_yaw: tracked.map_or(0.0, |entity| entity.head_yaw_deg),
        pitch: tracked.map_or(0.0, |entity| entity.pitch_deg),
        hurt_ticks: tracked.map_or(0.0, |entity| f32::from(entity.hurt_ticks)),
        limb_swing: sample.limb_swing,
        limb_swing_amount: target_limb_swing_amount,
    };
    if !sample.initialized {
        *sample = target;
    } else {
        let dx = target_position[0] - sample.target_position[0];
        let dz = target_position[2] - sample.target_position[2];
        let stride = dx.hypot(dz);
        if stride.is_finite() {
            sample.limb_swing =
                (sample.limb_swing + stride as f32 * 4.0).rem_euclid(std::f32::consts::TAU);
        }
        sample.target_position = target_position;
        // dt-independent exponential decay: the same curve for a 20 Hz
        // refresh, a 240 Hz frame, or a stalled frame that samples late.
        let step = if dt.is_finite() && dt > 0.0 {
            f64::from(dt.min(1.0))
        } else {
            0.0
        };
        let amount = exp_decay_exact(0.0, 1.0, ENTITY_SMOOTH_RATE, step) as f32;
        for axis in 0..3 {
            sample.position[axis] = exp_decay_exact(
                sample.position[axis],
                target.position[axis],
                ENTITY_SMOOTH_RATE,
                step,
            );
        }
        sample.body_yaw = interpolate_angle(sample.body_yaw, target.body_yaw, amount);
        sample.head_yaw = interpolate_angle(sample.head_yaw, target.head_yaw, amount);
        sample.pitch += (target.pitch - sample.pitch) * amount;
        sample.hurt_ticks += (target.hurt_ticks - sample.hurt_ticks) * amount;
        sample.limb_swing_amount +=
            (target.limb_swing_amount - sample.limb_swing_amount) * amount;
    }
    *sample
}

/// Milliseconds between host-RAM guard samples (~2 Hz — cheap enough to
/// ride the frame accumulator, fast enough to beat the OOM killer).
const HOST_RAM_GUARD_INTERVAL_MS: f32 = 500.0;

/// Host-memory pressure verdict for one [`HostMetricsSnapshot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostRamPressure {
    /// Comfortable headroom: no shedding.
    Clear,
    /// Available RAM under the reserve floor, or the process RSS over
    /// its budget: shed the `>view_distance+2` chunk fringe.
    Trim,
    /// Available RAM under half the reserve (or a 256 MiB absolute
    /// floor): pull the keep radius in so only the immediate surround
    /// stays resident.
    Emergency,
}

/// Classify one host snapshot against the RAM budget. `budget_mib == 0`
/// is Auto: the runtime's sane share is a quarter of host RAM clamped
/// to `[1 GiB, 8 GiB]`. The guard never pre-allocates and never trusts
/// the cap — pressure is judged on *available* host RAM plus the
/// process RSS, so a user who slides the cap to the host total still
/// gets protection.
fn host_ram_pressure(
    snap: &HostMetricsSnapshot,
    budget_mib: u32,
    host_total_mib: u32,
) -> HostRamPressure {
    let Some(available) = snap.mem_available_mib else {
        return HostRamPressure::Clear;
    };
    let total_mib = snap
        .mem_total_mib
        .map(|mib| mib.min(u64::from(u32::MAX)))
        .unwrap_or(u64::from(host_total_mib.max(1)));
    // Proportional reserve: 10% of host RAM clamped to [512 MiB, 4 GiB]
    // — small machines keep a real floor, big ones are not forced to
    // hold tens of GiB idle.
    let reserve_mib = (total_mib / 10).clamp(512, 4096);
    if available < 256 || available * 2 < reserve_mib {
        return HostRamPressure::Emergency;
    }
    let budget_mib = if budget_mib == 0 {
        // Auto share: quarter of host RAM, clamped to a sane band.
        (total_mib / 4).clamp(1024, 8192).min(u64::from(u32::MAX)) as u32
    } else {
        budget_mib
    };
    let rss_over_budget = snap
        .process_rss_mib
        .is_some_and(|rss| rss > u64::from(budget_mib));
    if available < reserve_mib || rss_over_budget {
        HostRamPressure::Trim
    } else {
        HostRamPressure::Clear
    }
}

/// Read the complete `resourcePacks` stack, preserving official built-in
/// ids while normalizing `file/<name>` entries to resourcepack directory
/// names. Missing or malformed options yield an empty stack (vanilla only).
pub(crate) fn configured_resource_pack_stack(game_dir: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(game_dir.join("options.txt")) else {
        return Vec::new();
    };
    let Some(value) = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("resourcePacks:"))
    else {
        return Vec::new();
    };
    let Ok(parsed) = megu_core::json::parse_json(value) else {
        return Vec::new();
    };
    let megu_core::json::JsonValue::Array(entries) = parsed else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for raw in entries.iter().filter_map(|entry| entry.as_str()) {
        let normalized = raw.strip_prefix("file/").unwrap_or(raw);
        if normalized == "vanilla"
            || normalized == "fabric"
            || normalized.is_empty()
            || normalized.contains("..")
            || normalized.contains('\\')
            || normalized.starts_with('/')
        {
            continue;
        }
        if !out.iter().any(|existing| existing == normalized) {
            out.push(normalized.to_owned());
        }
    }
    out
}

fn json_string_literal(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn resource_pack_option_json(selection: &[String]) -> String {
    let entries = selection
        .iter()
        .map(|entry| {
            if entry == "programmer_art"
                || entry == "high_contrast"
                || entry.starts_with("minecraft:")
            {
                json_string_literal(entry)
            } else {
                json_string_literal(&format!("file/{entry}"))
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{entries}]")
}

/// Persist the resource-pack stack atomically while preserving every other
/// `options.txt` key. This is small, synchronous user configuration rather
/// than frame-path I/O.
pub(crate) fn persist_resource_pack_stack(
    game_dir: &std::path::Path,
    selection: &[String],
) -> Result<(), String> {
    let path = game_dir.join("options.txt");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let replacement = format!("resourcePacks:{}", resource_pack_option_json(selection));
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in old.lines() {
        if line.trim_start().starts_with("resourcePacks:") {
            if !replaced {
                lines.push(replacement.clone());
                replaced = true;
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    if !replaced {
        lines.push(replacement);
    }
    let mut body = lines.join("\n");
    body.push('\n');
    let tmp = game_dir.join(".options.txt.megu-tmp");
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("replace {}: {e}", path.display()))
}

impl GpuApp {
    /// Build the app state (no GPU or window yet — `resumed` does that).
    pub fn new(config: GpuCliConfig) -> Self {
        let session_paths = SessionPaths {
            game_dir: config.game_dir.clone(),
            assets_dir: config.assets_dir.clone(),
            asset_index: config.asset_index.clone(),
            version_id: config.version_id.clone(),
            resource_pack_dir: config.resource_pack_dir.clone(),
            server_mods_dir: None,
        };
        // Load options before starting quick play so the integrated host
        // receives the same fair-play policy as worlds started from the UI.
        let settings_path = config
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(megu_client::settings::SETTINGS_FILE_NAME);
        let mut startup_status = Vec::new();
        let mut persisted_settings = megu_client::settings::ClientSettings {
            vsync: config.vsync,
            ..megu_client::settings::ClientSettings::default()
        };
        if let Ok(text) = std::fs::read_to_string(&settings_path) {
            for warning in persisted_settings.apply_toml(&text) {
                startup_status.push(format!("{}: {warning}", settings_path.display()));
            }
        } else if !settings_path.exists() {
            // True first launch — no `megu-client.toml` anywhere: seed
            // the probed hardware recommendation under the possible
            // `options.txt` import. The file itself is only ever
            // written by the explicit save path, never here, and an
            // existing user TOML always wins untouched.
            megu_client::hardware_profile::recommend_tuning(
                &megu_client::hardware_profile::probe_host_hardware(),
            )
            .apply_to(&mut persisted_settings);
            let options_path = settings_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("options.txt");
            if let Ok(text) = std::fs::read_to_string(&options_path) {
                persisted_settings =
                    megu_client::settings::ClientSettings::import_vanilla_options_txt(&text);
                startup_status.push(format!("Imported settings from {}", options_path.display()));
            }
        }
        if let Some(path) = &config.shader_pack {
            persisted_settings.shader_pack = Some(path.display().to_string());
            persisted_settings.shaders_enabled = true;
        }
        // `--quickPlaySingleplayer`: launch the integrated server first so
        // the session can bind its loopback endpoint. Failures surface as a
        // title-screen status line, never a silent no-op.
        let mut singleplayer_server = None;
        if !config.offline_demo {
            if let Some(world) = &config.quick_play_singleplayer {
                match Self::launch_singleplayer(
                    &config,
                    world,
                    &persisted_settings,
                    None,
                    None,
                    false,
                ) {
                    Ok(server) => singleplayer_server = Some(server),
                    Err(err) => {
                        startup_status.push(format!("Singleplayer {world:?}: {err}"));
                    }
                }
            }
        }
        let live = if config.offline_demo {
            None
        } else if let Some(server) = &singleplayer_server {
            let addr = server.addr();
            Some(LiveSession::connect_with_paths_and_auth(
                addr.ip().to_string(),
                addr.port(),
                config.username.clone(),
                config.mc_version.clone(),
                session_paths.clone(),
                config.access_token.clone(),
                config.uuid.clone(),
                persisted_settings.view_distance as i8,
            ))
        } else {
            config.connect.as_ref().map(|(host, port)| {
                LiveSession::connect_with_paths_and_auth(
                    host.clone(),
                    *port,
                    config.username.clone(),
                    config.mc_version.clone(),
                    session_paths.clone(),
                    config.access_token.clone(),
                    config.uuid.clone(),
                    persisted_settings.view_distance as i8,
                )
            })
        };
        let render_config = RenderConfig {
            vsync: config.vsync,
            gpu_backend: config.gpu_backend,
            shader_pack: config.shader_pack.as_ref().map(|p| p.display().to_string()),
            ..RenderConfig::default()
        };
        let policy = select_display_presentation_policy(
            detect_display_server_kind(),
            render_config.gpu_backend,
        );
        let governor = FrameBudgetGovernor::new(render_config.max_fps);
        let camera = Camera {
            position: [8.0, 18.0, 26.0],
            yaw_deg: 180.0,
            pitch_deg: -18.0,
            ..Camera::default()
        };
        let splash_seed = splash_seed_for(&config);
        let resource_pack_selection = config
            .game_dir
            .as_deref()
            .map(configured_resource_pack_stack)
            .unwrap_or_default();
        let active_singleplayer_world = config.quick_play_singleplayer.clone();
        let mut ui = UiState::new();
        apply_megu_default_links(&mut ui);
        apply_client_settings_to_ui(&persisted_settings, &mut ui.settings);
        // Straight into gameplay when a world/server is already targeted;
        // the title screen otherwise — like a vanilla launch with no
        // quick-play argument.
        ui.screen = if live.is_some() || config.offline_demo {
            Screen::InGameHud
        } else {
            Screen::Title
        };
        if ui.screen == Screen::InGameHud {
            ui.hud = megu_ui::state::HudState::demo();
        }
        let mut app = Self {
            config,
            live,
            rain_wetness: RainWetnessRamp::default(),
            active: None,
            camera,
            camera_controller: CameraController::new(render_config.fov_deg as f32, 1.0, 4.0),
            spectator_view: None,
            keys: Keys::default(),
            input_router: WindowInputRouter::default(),
            debug_messages: Vec::new(),
            focused: true,
            occluded: false,
            scale_factor: 1.0,
            cursor_grabbed: false,
            paused: false,
            show_diagnostics: false,
            mouse_pos: (0, 0),
            left_mouse_held: false,
            right_mouse_held: false,
            right_click_delay: RightClickDelay::default(),
            use_repeat_accum: 0.0,
            use_hold_secs: 0.0,
            hurt_tilt: HurtTilt::default(),
            gui_modifiers: 0,
            gui_left_held: false,
            gui_right_held: false,
            last_container_click: None,
            container_tooltip_advanced: false,
            screenshot_pending: false,
            screenshot_sequence: 0,
            container_screen: None,
            container_suspended_for_recipe: false,
            creative_inventory: None,
            creative_carried: None,
            book_editor: None,
            credits_screen: None,
            credits_ack_pending: false,
            merchant_offer_signature: None,
            vanilla_particles: VanillaParticleWorld::new(),
            own_inventory_open: false,
            seen_container_click_drops: 0,
            advancements_open: false,
            chat_bar: None,
            chat_suggestions: Vec::new(),
            pending_suggestion_tx: None,
            seen_suggestions_rev: 0,
            seen_completions_rev: 0,
            cooldown_tick_accum: 0.0,
            chat_shift_held: false,
            settings_save_guard: megu_client::settings::SettingsSaveGuard::new(),
            persisted_settings,
            active_shaderpack_path: None,
            settings_path,
            clipboard: SystemClipboard::new(),
            modifiers: ModifiersState::empty(),
            ime_state: crate::linux_desktop::ImeCompositionState::default(),
            ime_allowed: false,
            last_ime_commit: None,
            tiling_grab_policy: crate::linux_desktop::TilingGrabPolicy::new(
                crate::linux_desktop::detect_compositor_from_env(),
            ),
            last_dropped_file: None,
            inventory_sort: InventorySortState::default(),
            session_paths,
            mod_sync_worker: None,
            mod_sync_generation: 0,
            shader_options_open: false,
            missing_dep_worker: None,
            missing_dep_plan: None,
            plamo_worker: None,
            missing_dep_download_worker: None,
            resource_pack_selection,
            active_ui_locale: String::new(),
            soft_reload_asset_baseline: None,
            active_singleplayer_world,
            last_mod_sync_root_fp: None,
            vanilla_assets_bootstrap: None,
            vanilla_assets_bootstrap_attempted: false,
            last_server_address: None,
            recipe_ingest: crate::recipe_ingest::IngestReport::default(),
            item_icon_atlas: megu_ui::draw::ItemIconAtlas::default(),
            item_icon_atlas_uploaded: false,
            recipe_inventory_state: -1,
            recipe_overlay_regions: Vec::new(),
            lang_ingested_locale: String::new(),
            title_branding: TitleBrandingAssets::fallback(),
            splash_seed,
            singleplayer_server,
            friend_host: None,
            friend_bridge: None,
            frl_identity: None,
            friends_store: None,
            friend_share: None,
            guest_link_grant: None,
            friend_join_sas: None,
            vanilla_bootstrap: None,
            server_blocks_bootstrap: None,
            bootstrap_attempted: false,
            bootstrap_version: String::new(),
            bootstrap_banner: None,
            title_seen_rev: 0,
            title_tick_accum: 0.0,
            live_hud_metadata: None,
            live_ui_locale: None,
            inspector_hud_cache: None,
            ui,
            ui_layer_bus: megu_core::rev::UiLayerBus::new(),
            ui_retained: megu_ui::retained_cache::UiRetainedCache::new(),
            ui_layer_fps: [0; 7],
            ui_modhud_list: UiDrawList::new(),
            ui_overlay_list: UiDrawList::new(),
            ui_modoverlay_list: UiDrawList::new(),
            mod_overlay_layout: None,
            gui_sprite_atlas: megu_ui::draw::GuiSpriteAtlas::default(),
            vanilla_runtime_assets: None,
            vanilla_official_extras: None,
            entity_appearance: None,
            viewmodel_arm_skin_uploaded: None,
            viewmodel_swing_hand: LiveSession::MAIN_HAND,
            viewmodel_item_use_hand: LiveSession::MAIN_HAND,
            player_skin_cache: None,
            gui_sprites_loaded: false,
            render_config,
            policy,
            governor,
            user_shadow_quality: BuiltinShadowQuality::Off,
            user_render_scale: RenderScalePreset::Native100,
            baseline_upload_budget: None,
            host_ram_probe: CpuSampler::new(),
            host_ram_sample_accum_ms: 0.0,
            f3_overlay: F3DebugOverlay::default(),
            f3_overlay_built: None,
            f3_metrics_probe: CpuSampler::new(),
            vanilla_ascii_loaded: false,
            vanilla_unifont_loaded: false,
            pending_mouse_delta: (0.0, 0.0),
            last_subworld_helm: None,
            live_strategy_pending: true,
            ui_draw_list: UiDrawList::new(),
            gui_bridge_batch: GuiBridgeBatch::default(),
            mod_ui_manifest: None,
            ui_anim_epoch: Instant::now(),
            particle_scratch: Vec::new(),
            subworld_light_scratch: Vec::new(),
            debug_line_scratch: Vec::new(),
            live_particles_staged: false,
            entity_render_cache: LiveEntityRenderCache::default(),
            seamless_portal: SeamlessDimTransitionState::new(),
            last_space_press: None,
            last_frame: Instant::now(),
            frame_start: Instant::now(),
            fatal: None,
        };
        for message in startup_status {
            app.status_line(message);
        }
        // Lane GPU-RECIPE-TUTORIAL-WIRE: feed `ui.recipe_browser.index` real
        // datapack content — `<gameDir>/mods/*.jar`, the vanilla version
        // JAR, and datapack dirs — all through the lazy ZIP index with
        // zero extraction. The ingest report is available to diagnostics.
        app.recipe_ingest = crate::recipe_ingest::ingest_mod_recipes_into_ui(
            &app.session_paths,
            &mut app.ui.recipe_browser,
        );
        if app.recipe_ingest.any_source() {
            eprintln!(
                "[{}] {}",
                crate::CLIENT_GPU_NAME,
                app.recipe_ingest.summary_line()
            );
        }
        // Localized item names: `en_us` + the active display locale, from
        // every JAR's `assets/*/lang/*.json` (zero extraction). The
        // browser's `active_locale` was already synced from settings
        // during `UiState` construction; switching language re-ingests.
        let _ = crate::recipe_ingest::ingest_lang_into_ui(
            &app.session_paths,
            &mut app.ui.recipe_browser,
        );
        app.lang_ingested_locale = app.ui.recipe_browser.active_locale.clone();
        // Runtime `MEGU EDITION` title branding: when the launcher paths
        // resolve a local client JAR, its `minecraft.png`/`edition.png`/
        // `splashes.txt` are read in memory through the lazy ZIP index —
        // the edition strip patched to `MEGU` — and mod JARs contribute
        // their own splash pools. Zero temp files, graceful fallback.
        if let Some(jar) = configured_title_jar(&app.session_paths) {
            app.load_title_branding(&jar);
        }
        // Activate the loaded settings in one pass (video -> render
        // config, camera values -> controller/FOV, volumes -> live audio
        // mixer when a session already attached at boot).
        let boot_settings = app.ui.settings.clone();
        app.apply_settings(&boot_settings);
        app.active_ui_locale = app.ui.settings.locale.code().to_owned();
        app.refresh_plamo_panel_status();
        app.refresh_singleplayer_worlds();
        // Quick-play boots straight into a hosted world — only that path
        // loads FRL3 state eagerly so a persisted share mode re-arms
        // before any screen opens. A plain boot must not mint
        // `megu-identity.key` the player never asked for; screen entry
        // loads it on demand.
        if app.config.quick_play_singleplayer.is_some() {
            app.load_friends_state();
            app.maybe_start_friend_share();
        }
        app
    }

    fn active_title_jar(&self) -> Option<std::path::PathBuf> {
        self.title_branding
            .source_jar
            .as_ref()
            .filter(|jar| jar.is_file())
            .cloned()
            .or_else(|| resolve_title_jar(&self.session_paths))
    }

    fn load_title_branding(&mut self, jar: &std::path::Path) {
        if self.title_branding.source_jar.as_deref() == Some(jar) {
            return;
        }
        self.title_branding = crate::title_branding::load_title_branding_from_jar(
            jar,
            &title_mod_jars(&self.session_paths),
        );
        self.vanilla_official_extras = None;
        if let Some(active) = self.active.as_mut() {
            active.title_branding_frame = None;
            active.renderer.set_title_branding(
                self.title_branding.logo.as_ref(),
                self.title_branding.edition.as_ref(),
            );
        }
    }

    pub fn ensure_vanilla_official_extras(&mut self) -> bool {
        if self.vanilla_official_extras.is_some() {
            return true;
        }
        let jar = self.active_title_jar();
        let assets_dir = self
            .session_paths
            .assets_dir
            .as_ref()
            .filter(|path| path.is_dir());
        let asset_index = self.session_paths.asset_index.as_deref().unwrap_or("17");
        self.vanilla_official_extras = Some(VanillaOfficialExtras::load(
            jar.as_deref(),
            assets_dir.map(std::path::PathBuf::as_path),
            asset_index,
        ));
        true
    }

    pub fn vanilla_official_extras(&self) -> Option<&VanillaOfficialExtras> {
        self.vanilla_official_extras.as_ref()
    }

    pub fn entity_appearance(&self) -> Option<&RuntimeEntityAppearanceAtlas> {
        self.entity_appearance.as_ref()
    }

    pub fn official_builtin_resource_packs(&self) -> Vec<(String, String, std::path::PathBuf)> {
        self.vanilla_official_extras
            .as_ref()
            .map(discover_official_builtin_packs)
            .unwrap_or_default()
    }

    pub fn upload_title_panorama_visuals(
        paths: &SessionPaths,
        jar: Option<&std::path::Path>,
        resource_packs: &[std::path::PathBuf],
        extras: &VanillaOfficialExtras,
        renderer: &mut GpuRenderer,
    ) -> bool {
        let gui_assets =
            crate::title_branding::load_title_panorama_assets(paths, jar, resource_packs);
        // The overlay participates in the same pack > jar > asset-index
        // resolution as the cubemap faces: `named_textures` already
        // folded every enabled resource pack, so a pack-supplied
        // `panorama_overlay.png` wins. The extras catalog (JAR + object
        // store only, no packs) stays the fallback for installs where
        // the GUI stack did not decode it.
        let overlay = gui_assets
            .named_texture("title/background/panorama_overlay.png")
            .map(|image| (image.rgba.as_slice(), image.width, image.height))
            .or_else(|| {
                extras
                    .gui_sprite_rgba("title/background/panorama_overlay.png")
                    .map(|image| (image.rgba.as_slice(), image.width, image.height))
            });
        renderer.upload_title_panorama(&gui_assets, overlay)
    }

    fn rebuild_entity_appearance(&mut self) {
        let (Some(runtime), Some(extras)) = (
            self.vanilla_runtime_assets.as_ref(),
            self.vanilla_official_extras.as_ref(),
        ) else {
            return;
        };
        self.entity_appearance = Some(RuntimeEntityAppearanceAtlas::new(
            &runtime.entity_atlas,
            extras,
        ));
    }

    pub fn upload_entity_appearance_if_dirty(&mut self) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        let Some(appearance) = self.entity_appearance.as_mut() else {
            return false;
        };
        if !appearance.take_dirty() {
            return false;
        }
        active
            .renderer
            .upload_entity_texture_atlas(appearance.atlas());
        true
    }

    fn apply_official_window_icon(&mut self) {
        let Some(extras) = self.vanilla_official_extras.as_ref() else {
            return;
        };
        let Some(icon) = build_winit_window_icon(extras) else {
            return;
        };
        if let Some(active) = self.active.as_ref() {
            active.window.set_window_icon(Some(icon));
        }
    }

    fn configured_builtin_pack_enabled(&self, id: &str) -> bool {
        let Some(game_dir) = self.session_paths.game_dir.as_deref() else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(game_dir.join("options.txt")) else {
            return false;
        };
        let Some(value) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("resourcePacks:"))
        else {
            return false;
        };
        let Ok(parsed) = megu_core::json::parse_json(value) else {
            return false;
        };
        let megu_core::json::JsonValue::Array(entries) = parsed else {
            return false;
        };
        let namespaced = format!("minecraft:{id}");
        let zipped = format!("file/{id}.zip");
        entries
            .iter()
            .filter_map(|entry| entry.as_str())
            .any(|entry| entry == id || entry == namespaced || entry == zipped)
    }

    pub fn sync_official_builtin_pack_menu_entries(&mut self) -> usize {
        let packs = self.official_builtin_resource_packs();
        let mut present = 0usize;
        for (id, display_name, object_path) in packs {
            let namespaced_id = format!("minecraft:{id}");
            let file_stem = object_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.trim_end_matches(".zip").to_owned());
            let enabled = self.configured_builtin_pack_enabled(&id);
            let existing = self.ui.mod_manager.entries.iter_mut().find(|entry| {
                entry.mod_id == id
                    || entry.mod_id == namespaced_id
                    || entry.name == display_name
                    || file_stem.as_ref().is_some_and(|stem| entry.mod_id == *stem)
            });
            if let Some(entry) = existing {
                entry.name = display_name.clone();
                entry.version = "1.21.1".into();
                entry.enabled |= enabled;
                entry.summary = "Official built-in Minecraft resource pack".into();
            } else {
                let mut entry = megu_ui::mod_manager::ModManagerEntry::new(
                    &namespaced_id,
                    &display_name,
                    "1.21.1",
                );
                entry.enabled = enabled;
                entry.summary = "Official built-in Minecraft resource pack".into();
                entry.loader = megu_ui::mod_manager::ModLoaderTag::MeguNative;
                entry.tier = megu_ui::mod_manager::ModAdaptationBadge::Tier1Native;
                entry.reload_mode = megu_ui::mod_manager::ModReloadBadge::HotReload;
                self.ui.mod_manager.entries.push(entry);
            }
            present += 1;
        }
        present
    }

    /// Decode the local client JAR's GUI sprites into
    /// `gui_sprite_atlas` (widget keys plus stable HUD/effect bindings) and push
    /// the texture to `ui_quad` binding 5 when a renderer is live.
    /// Clean-room hosts keep the empty atlas — `UiCommand::Sprite` then
    /// never emits and procedural widget/HUD chrome remains the fallback.
    /// Runs once per JAR arrival (`gui_sprites_loaded` gates rescans).
    fn load_gui_sprite_atlas(&mut self) {
        if self.vanilla_runtime_assets.is_some() {
            return;
        }
        // A partially bootstrapped `assets/` directory cannot supply the
        // JAR-backed GUI/entity/particle atlases. Keep retrying each frame
        // until the requested local version JAR is actually present.
        let Some(jar) = self.active_title_jar() else {
            return;
        };
        self.load_title_branding(&jar);
        self.ensure_vanilla_official_extras();
        let assets_dir = self
            .session_paths
            .assets_dir
            .as_ref()
            .filter(|path| path.is_dir())
            .cloned();
        let packs = self.runtime_resource_pack_paths();
        let runtime = crate::vanilla_assets_bridge::load_runtime_assets(
            Some(&jar),
            assets_dir.as_deref(),
            self.session_paths.asset_index.as_deref(),
            &packs,
            self.ui.settings.locale.code(),
        );
        self.mod_overlay_layout = None;
        self.gui_sprite_atlas = runtime.gui_atlas.clone();
        install_logical_hud_sprite_bindings(&mut self.gui_sprite_atlas, &runtime.gui_textures);
        self.gui_sprites_loaded = true;
        self.rebuild_entity_appearance();
        self.sync_official_builtin_pack_menu_entries();
        if let Some(active) = self.active.as_mut() {
            Self::upload_runtime_assets_to_renderer(&runtime, &mut active.renderer);
            if let Some(extras) = self.vanilla_official_extras.as_ref() {
                Self::upload_title_panorama_visuals(
                    &self.session_paths,
                    Some(&jar),
                    &packs,
                    extras,
                    &mut active.renderer,
                );
            }
        }
        self.upload_entity_appearance_if_dirty();
        self.apply_official_window_icon();
        if let Some(live) = self.live.as_mut() {
            live.apply_biome_colormaps(&runtime.world_assets);
        }
        let skin_dir = assets_dir.clone().or_else(|| {
            self.session_paths
                .game_dir
                .as_ref()
                .map(|p| p.join("assets"))
        });
        if let Some(skin_dir) = skin_dir {
            self.player_skin_cache = Some(crate::player_skins::PlayerSkinCache::new(skin_dir));
        }
        self.vanilla_runtime_assets = Some(runtime);
        self.rebuild_entity_appearance();
    }

    fn runtime_resource_pack_paths(&self) -> Vec<std::path::PathBuf> {
        let Some(game_dir) = self.session_paths.game_dir.as_deref() else {
            return Vec::new();
        };
        let pack_root = self
            .session_paths
            .resource_pack_dir
            .clone()
            .unwrap_or_else(|| game_dir.join("resourcepacks"));
        let builtin_path = |token: &str| -> Option<std::path::PathBuf> {
            let bare = token
                .strip_prefix("minecraft:")
                .unwrap_or(token)
                .strip_suffix(".zip")
                .unwrap_or(token.strip_prefix("minecraft:").unwrap_or(token));
            if bare != "programmer_art" && bare != "high_contrast" {
                return None;
            }
            self.official_builtin_resource_packs()
                .into_iter()
                .find(|(id, _, _)| id.strip_prefix("minecraft:").unwrap_or(id) == bare)
                .map(|(_, _, path)| path)
        };
        let mut packs: Vec<std::path::PathBuf> = self
            .resource_pack_selection
            .iter()
            .filter_map(|name| {
                if name.is_empty()
                    || name.contains("..")
                    || name.starts_with('/')
                    || name.contains('\\')
                {
                    return None;
                }
                if let Some(path) = builtin_path(name) {
                    return Some(path);
                }
                let path = pack_root.join(name);
                (path.is_file() || path.is_dir()).then_some(path)
            })
            .collect();
        // Reconcile with the canonical `AssetPackChain` parse of
        // `options.txt`: the selection is persisted from the same file,
        // but an externally edited stack (or a builtin id the chain
        // classifies differently) must not silently drop a pack the
        // unified resolver would honor. Missing entries slot back in at
        // their chain position so low→high priority order survives.
        // Cold path only — callers are the atlas/panorama rebuilds.
        let chain = crate::vanilla_bootstrap::AssetPackChain::for_session(&self.session_paths);
        let chain_paths = chain.resource_pack_paths_low_to_high();
        for (index, path) in chain_paths.iter().enumerate() {
            if packs.iter().any(|existing| existing == path) {
                continue;
            }
            let mut position = 0usize;
            for (existing_index, existing) in packs.iter().enumerate() {
                if chain_paths[..index].contains(existing) {
                    position = existing_index + 1;
                }
            }
            packs.insert(position, path.clone());
        }
        packs
    }

    /// Force the runtime bundle to re-scan jar/assets/resource-pack layers
    /// and the active locale on the next load. The window/device/surface are
    /// deliberately untouched: this is the asset half of soft reload.
    fn invalidate_vanilla_runtime_assets(&mut self) {
        self.vanilla_runtime_assets = None;
        self.entity_appearance = None;
        self.gui_sprites_loaded = false;
    }

    /// `F3 + T` (`reloadResourcePacks`): mark every asset layer stale so the
    /// next frame's cold `load_gui_sprite_atlas` pass re-resolves the JAR,
    /// resource packs, and atlases. Input handling never blocks on the
    /// rescan. Returns `false` when nothing was loaded yet — the initial
    /// load owns that case and there is nothing to re-derive.
    fn reload_debug_assets(&mut self) -> bool {
        if self.vanilla_runtime_assets.is_none() {
            return false;
        }
        if let Some(live) = &mut self.live {
            // Re-arm asset discovery (a JAR/pack that appeared after the
            // first pass) and the block-atlas rebuild.
            live.refresh_asset_layers();
            live.request_atlas_reload();
        }
        self.item_icon_atlas_uploaded = false;
        self.invalidate_vanilla_runtime_assets();
        true
    }

    /// Load local vanilla assets without creating a window. This is also the
    /// headless entry point used by asset integration tests.
    pub fn load_vanilla_runtime_assets(&mut self) {
        self.load_gui_sprite_atlas();
    }

    /// CPU-side evidence of which local runtime assets were resolved.
    pub fn vanilla_runtime_assets_summary(
        &self,
    ) -> Option<crate::vanilla_assets_bridge::VanillaRuntimeAssetsSummary> {
        self.vanilla_runtime_assets
            .as_ref()
            .map(|assets| assets.summary())
    }

    /// Retained local asset bundle for renderer adapters and credits/UI.
    pub fn vanilla_runtime_assets(
        &self,
    ) -> Option<&crate::vanilla_assets_bridge::VanillaRuntimeAssets> {
        self.vanilla_runtime_assets.as_ref()
    }

    /// Upload the complete retained local asset bundle into a renderer.
    /// The normal window path and headless GPU integration path share this
    /// exact boundary.
    pub fn upload_runtime_assets_to_renderer(
        assets: &crate::vanilla_assets_bridge::VanillaRuntimeAssets,
        renderer: &mut GpuRenderer,
    ) {
        renderer.upload_gui_sprite_atlas(&assets.gui_atlas);
        renderer.upload_world_env_assets(&assets.world_assets);
        renderer.upload_destroy_stage_textures(&assets.world_assets);
        renderer.upload_entity_texture_atlas(&assets.entity_atlas);
        renderer.upload_particle_texture_atlas(&assets.particle_atlas);
    }

    /// Queue official texture lookups for the current tab-list players.
    /// Requests and result polling are non-blocking; network and disk work
    /// stay on the cache worker thread.
    fn refresh_player_skin_requests(&mut self) {
        let players = self.live.iter().flat_map(|live| live.player_info.values());
        if let Some(cache) = self.player_skin_cache.as_mut() {
            for entry in players.clone() {
                let textures = entry
                    .properties
                    .iter()
                    .find(|(name, _, _)| name == "textures")
                    .map(|(_, value, _)| value.as_str());
                cache.request_skin_for_uuid(entry.uuid, textures);
            }
            cache.poll_completed();
        }
        if let (Some(cache), Some(appearance)) = (
            self.player_skin_cache.as_ref(),
            self.entity_appearance.as_mut(),
        ) {
            for entry in players {
                if let Some(skin) = cache.get_skin(entry.uuid) {
                    appearance.upsert_player_skin(entry.uuid, skin);
                }
            }
        }
        self.upload_entity_appearance_if_dirty();
        self.refresh_viewmodel_arm_skin();
    }

    /// RGBA sheet for the local player's first-person arm: the resolved
    /// session skin when the cache has one, else the official default
    /// Steve/Alex skin for the uuid. `None` keeps the built-in fallback
    /// arm. Remote lookups stay on the cache worker; this only reads
    /// already-resolved pixels.
    fn viewmodel_arm_skin_rgba(&self, uuid: [u8; 16]) -> Option<(u32, u32, &[u8])> {
        if let Some(skin) = self
            .player_skin_cache
            .as_ref()
            .and_then(|cache| cache.get_skin(uuid))
            .filter(|skin| !skin.skin_rgba.is_empty())
        {
            return Some((skin.width, skin.height, skin.skin_rgba.as_slice()));
        }
        let (name, slim) = crate::vanilla_official_extras::default_skin_for_uuid(uuid);
        self.vanilla_official_extras
            .as_ref()?
            .skin_rgba(name, slim)
            .map(|image| (image.width, image.height, image.rgba.as_slice()))
    }

    /// Push the resolved arm skin to the renderer's bare-arm texture.
    /// Dedup is a content fingerprint so remote-player appearance
    /// revisions can neither starve nor spam the upload; only the
    /// 64×64 sheet layout the arm UVs are authored for is accepted.
    fn refresh_viewmodel_arm_skin(&mut self) {
        let uuid = self.local_player_uuid();
        let key = self
            .viewmodel_arm_skin_rgba(uuid)
            .filter(|(width, height, _)| *width == 64 && *height == 64)
            .map(|(width, height, rgba)| (width, height, arm_skin_fingerprint(rgba)));
        let Some((width, height, fingerprint)) = key else {
            return;
        };
        if self.viewmodel_arm_skin_uploaded == Some((uuid, fingerprint)) {
            return;
        }
        let rgba = self
            .viewmodel_arm_skin_rgba(uuid)
            .map(|(_, _, pixels)| pixels.to_vec());
        let Some(rgba) = rgba else {
            return;
        };
        if let Some(active) = self.active.as_mut() {
            active
                .renderer
                .upload_viewmodel_arm_skin(width, height, &rgba);
            self.viewmodel_arm_skin_uploaded = Some((uuid, fingerprint));
        }
    }

    fn local_player_uuid(&self) -> [u8; 16] {
        if self
            .config
            .access_token
            .as_ref()
            .is_some_and(|token| token.is_present())
        {
            if let Some(uuid) = self
                .config
                .uuid
                .as_deref()
                .and_then(|raw| megu_client::protocol::parse_uuid(raw).ok())
            {
                return uuid;
            }
        }
        megu_client::protocol::offline_uuid(&self.config.username)
    }

    /// Non-blocking resolved remote skin lookup for entity render adapters.
    pub fn resolved_player_skin(
        &self,
        uuid: [u8; 16],
    ) -> Option<&crate::player_skins::ResolvedPlayerSkin> {
        self.player_skin_cache.as_ref()?.get_skin(uuid)
    }

    /// Probe until both independent Minecraft font components are ready.
    /// A partial ASCII result must not suppress a later Unifont load.
    fn maybe_load_vanilla_fonts(&mut self) {
        if self.vanilla_ascii_loaded && self.vanilla_unifont_loaded {
            return;
        }
        if let Some(load) =
            crate::vanilla_font::try_load_local_minecraft_fonts_detailed(&self.session_paths)
        {
            self.vanilla_ascii_loaded |= load.ascii_ready();
            self.vanilla_unifont_loaded |= load.unifont_ready();
            megu_ui::font_cjk::install_external_font(load.into_atlas());
        }
    }

    /// Snapshot the live session + renderer into the two-column F3
    /// overlay. Runs at `F3_REFRESH` cadence; every field degrades to
    /// `None` when its source is absent so the overlay stays honest on
    /// title screens and headless CI. An associated fn over disjoint
    /// fields so the `active` window borrow can stay mutable.
    fn build_f3_overlay(
        live: Option<&LiveSession>,
        governor: &FrameBudgetGovernor,
        user_shadow_quality: BuiltinShadowQuality,
        metrics_probe: &mut CpuSampler,
        active: &ActiveWindow,
    ) -> F3DebugOverlay {
        let mut overlay = F3DebugOverlay {
            enabled: true,
            client_version: Some("1.21.1".to_owned()),
            display: Some((active.size.width, active.size.height)),
            backend: Some(
                active
                    .renderer
                    .gpu()
                    .report
                    .resolution
                    .backend
                    .as_str()
                    .to_owned(),
            ),
            gpu: Some(active.renderer.gpu().report.name.clone()),
            os: Some(std::env::consts::OS.to_owned()),
            render_scale: Some(active.renderer.render_scale.percent() as f32 / 100.0),
            shadows: Some(user_shadow_quality != BuiltinShadowQuality::Off),
            visible_sections: Some(active.renderer.live_section_count() as u32),
            total_sections: Some(active.renderer.live_section_count() as u32),
            ..Default::default()
        };
        let host = sample_host_metrics(metrics_probe);
        overlay.memory_used_mb = host.process_rss_mib.or(host.mem_used_mib);
        overlay.memory_total_mb = host.mem_total_mib;
        overlay.frame_ms = Some(governor.frame_ms() as f32);
        let Some(live) = live else {
            return overlay;
        };
        overlay.fps = Some(live.frame_timings.fps_avg() as f32);
        overlay.frame_ms = Some(live.frame_timings.frame_ms_avg() as f32);
        overlay.entity_count = Some(live.world.entities.len() as u32);
        overlay.loaded_chunks = Some(live.world.loaded_chunks() as u32);
        overlay.render_distance = Some(live.client_view_distance.max(0) as u32);
        // `F3DebugOverlay` is owned by client-ui and has no survival-state
        // extension point yet, so these server facts ride the dimension line
        // in a compact truthful suffix instead of inventing renderer-only data.
        let mut dimension_line = live.world.dimension.name.clone();
        if let Some((difficulty, locked)) = live.world.difficulty {
            dimension_line.push_str(match difficulty {
                0 => " peaceful",
                1 => " easy",
                2 => " normal",
                3 => " hard",
                _ => " difficulty?",
            });
            if locked {
                dimension_line.push_str(" locked");
            }
        }
        dimension_line.push_str(&format!(" {:.1}t", live.world.server_tick_rate));
        if live.world.server_ticks_frozen {
            dimension_line.push_str(" frozen");
        }
        if let Some(rtt) = live.pong_rtt_ms {
            dimension_line.push_str(&format!(" {rtt}ms"));
        }
        if live.world.chunk_batch_active {
            dimension_line.push_str(" batch");
        }
        overlay.dimension = Some(dimension_line);
        if let Some(pose) = live.pose {
            overlay.position = Some([
                pose.feet[0] as f32,
                pose.feet[1] as f32,
                pose.feet[2] as f32,
            ]);
            let block = [
                pose.feet[0].floor() as i32,
                pose.feet[1].floor() as i32,
                pose.feet[2].floor() as i32,
            ];
            overlay.block_pos = Some(block);
            overlay.chunk = Some((block[0].div_euclid(16), block[2].div_euclid(16)));
            overlay.section_y = Some(block[1].div_euclid(16));
            let (cardinal, axis) = facing_from_yaw(pose.yaw);
            overlay.facing = Some(cardinal.to_owned());
            overlay.facing_axis = Some(axis.to_owned());
            overlay.look_deg = Some([pose.yaw, pose.pitch]);
        }
        if let Some(([x, y, z], block_id)) = &live.looking_at_block {
            overlay.target_block = Some((*x, *y, *z, block_id.clone()));
        }
        if let Some(entity_id) = live.looking_at_entity {
            overlay.target_entity = live
                .entity_rig(entity_id)
                .map(|rig| rig.name.to_owned())
                .or_else(|| Some(format!("entity #{entity_id}")));
        }
        overlay
    }

    fn vanilla_asset_store_complete(&self) -> bool {
        let index_id = self.session_paths.asset_index.as_deref().unwrap_or("17");
        let Some(root) =
            crate::vanilla_bootstrap::discover_local_assets_dir(index_id, &self.session_paths)
        else {
            return false;
        };
        let index_path = root.join("indexes").join(format!("{index_id}.json"));
        let Ok(text) = std::fs::read_to_string(index_path) else {
            return false;
        };
        let Ok(json) = megu_core::json::parse_json(&text) else {
            return false;
        };
        let Some(objects) = json
            .get("objects")
            .and_then(megu_core::json::JsonValue::as_object)
        else {
            return false;
        };
        // Check indexed panorama faces and representative companion assets
        // that a cached client JAR alone cannot supply.
        let panorama_faces = crate::vanilla_gui_sprites::ALL_GUI_SPRITES
            .into_iter()
            .filter(|id| id.is_panorama_face())
            .map(|id| id.jar_entry().trim_start_matches("assets/"));
        for key in [
            "minecraft/lang/en_us.json",
            "minecraft/font/ascii.png",
            "icons/icon_16x16.png",
            "resourcepacks/programmer_art.zip",
        ]
        .into_iter()
        .chain(panorama_faces)
        {
            let Some(object) = objects
                .iter()
                .find_map(|(name, value)| (name == key).then_some(value))
            else {
                continue;
            };
            let Some(hash) = object
                .get_str("hash")
                .filter(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            else {
                return false;
            };
            let Some(size) = object
                .get("size")
                .and_then(megu_core::json::JsonValue::as_u64)
            else {
                return false;
            };
            let path = root.join("objects").join(&hash[..2]).join(hash);
            let Ok(meta) = std::fs::symlink_metadata(path) else {
                return false;
            };
            if !meta.is_file() || meta.len() != size {
                return false;
            }
        }
        true
    }

    /// Cache-only/bootstrapped vanilla installs often have the client JAR
    /// but no complete Mojang object store. Resolve metadata and fill that
    /// store off-thread, then reload the runtime bundle in-place (P1.1).
    fn ensure_vanilla_assets_bootstrap(&mut self) {
        if self.server_blocks_bootstrap.is_none() && crate::vanilla_bootstrap::auto_fetch_enabled()
        {
            let game_dir = self
                .config
                .game_dir
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            if crate::vanilla_bootstrap::discover_local_server_blocks(&game_dir).is_none() {
                self.server_blocks_bootstrap =
                    crate::vanilla_bootstrap::spawn_server_blocks_bootstrap(&game_dir);
            }
        }
        if self.vanilla_assets_bootstrap.is_some()
            || self.vanilla_assets_bootstrap_attempted
            || !crate::vanilla_bootstrap::auto_fetch_enabled()
            || self.vanilla_asset_store_complete()
        {
            return;
        }
        if self.active_title_jar().is_none() {
            return;
        }
        let Some(version_id) = jar_version_ids(&self.session_paths)
            .last()
            .cloned()
        else {
            return;
        };
        self.vanilla_assets_bootstrap_attempted = true;
        if let Some(handle) =
            DeferredVanillaAssetsBootstrap::spawn(&version_id, self.session_paths.clone())
        {
            self.vanilla_assets_bootstrap = Some(handle);
        }
    }

    fn poll_vanilla_assets_bootstrap(&mut self) {
        let Some(progress) = self
            .vanilla_assets_bootstrap
            .as_ref()
            .map(DeferredVanillaAssetsBootstrap::progress)
        else {
            return;
        };
        use crate::vanilla_bootstrap::AssetBootstrapProgress as P;
        match progress {
            P::Ready { assets_dir, index_id } => {
                self.vanilla_assets_bootstrap = None;
                if self.session_paths.assets_dir.is_none() {
                    self.session_paths.assets_dir = Some(assets_dir);
                }
                self.session_paths.asset_index.get_or_insert(index_id);
                self.vanilla_official_extras = None;
                self.invalidate_vanilla_runtime_assets();
                self.maybe_load_vanilla_fonts();
                self.load_gui_sprite_atlas();
                if let Some(live) = &mut self.live {
                    live.session_paths.assets_dir = self.session_paths.assets_dir.clone();
                    live.session_paths.asset_index = self.session_paths.asset_index.clone();
                    live.refresh_asset_layers();
                }
                self.status_line("Minecraft runtime asset bundle ready");
            }
            P::Failed(reason) => {
                self.vanilla_assets_bootstrap = None;
                self.status_line(format!("Vanilla asset bundle download: {reason}"));
            }
            P::FetchingIndex | P::Downloading { .. } => {}
        }
    }

    /// Non-blocking poll of the background vanilla-JAR bootstrap, called
    /// once per event-loop turn from `about_to_wait`. In-flight progress
    /// rides `ui.title_status` only while that banner still shows OUR
    /// bootstrap text — a real `status_line` message always wins. On
    /// `Ready` the texture layers + title branding hot-reload seamlessly;
    /// on `Failed` a clean, actionable status line replaces the banner —
    /// the user is never told to run `megu-installer` by hand.
    fn poll_vanilla_bootstrap(&mut self) {
        let Some(progress) = self
            .vanilla_bootstrap
            .as_ref()
            .map(crate::vanilla_bootstrap::VanillaJarBootstrap::progress)
        else {
            return;
        };
        use crate::vanilla_bootstrap::BootstrapProgress as P;
        match progress {
            P::Ready(jar) => {
                self.vanilla_bootstrap = None;
                let version = std::mem::take(&mut self.bootstrap_version);
                self.load_title_branding(&jar);
                // Re-arm asset discovery: the JAR now sits inside the
                // candidate set, so the next bundle pass picks up the
                // vanilla textures and re-uploads the atlas seamlessly.
                if let Some(live) = &mut self.live {
                    live.refresh_asset_layers();
                }
                // The downloaded JAR + assets may now satisfy the font
                // probe (it stays silent while nothing local exists).
                self.maybe_load_vanilla_fonts();
                // …and the `widget/*` GUI sprites: decode + upload the
                // chrome atlas now that a real JAR exists.
                self.invalidate_vanilla_runtime_assets();
                self.load_gui_sprite_atlas();
                if self.ui.title_status.as_deref() == self.bootstrap_banner.as_deref() {
                    self.ui.title_status = None;
                }
                self.bootstrap_banner = None;
                self.status_line(format!(
                    "Minecraft {version} assets ready — vanilla textures loaded"
                ));
            }
            P::Failed(reason) => {
                self.vanilla_bootstrap = None;
                let version = std::mem::take(&mut self.bootstrap_version);
                if self.ui.title_status.as_deref() == self.bootstrap_banner.as_deref() {
                    self.ui.title_status = None;
                }
                self.bootstrap_banner = None;
                self.status_line(format!(
                    "Offline: could not auto-download {version} assets ({reason}). \
                     Connect to the internet and restart, or pass --gameDir."
                ));
            }
            _ => {
                let Some(line) = crate::vanilla_bootstrap::bootstrap_progress_label(
                    &self.bootstrap_version,
                    &progress,
                ) else {
                    return;
                };
                if self.ui.title_status.is_none()
                    || self.ui.title_status.as_deref() == self.bootstrap_banner.as_deref()
                {
                    self.ui.title_status = Some(line.clone());
                    self.bootstrap_banner = Some(line);
                }
            }
        }
    }

    /// The fatal setup error, if `resumed` hit one (`run` reports it).
    pub fn fatal(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// The top-level UI screen (Title, Pause, InGameHud, ...).
    pub fn screen(&self) -> Screen {
        self.ui.screen
    }

    /// The active live session, when one was spawned.
    pub fn live(&self) -> Option<&LiveSession> {
        self.live.as_ref()
    }

    /// `true` while `container_screen` is the window-0 player inventory.
    pub fn own_inventory_open(&self) -> bool {
        self.own_inventory_open
    }

    /// `true` while the `L`-key advancements overlay is up.
    pub fn advancements_open(&self) -> bool {
        self.advancements_open
    }

    /// Slot count of the open container surface (own inventory or server
    /// screen), `None` when no surface is up.
    pub fn container_slot_count(&self) -> Option<usize> {
        self.container_screen
            .as_ref()
            .map(ContainerScreenState::slot_count)
    }

    /// One slot of the open container surface.
    pub fn container_slot(&self, index: usize) -> Option<UiItemStack> {
        self.container_screen.as_ref()?.slot(index).cloned()
    }

    /// Status lines surfaced by dispatched actions (join failures, invite
    /// results, statistics summaries) — never silently dropped.
    pub fn status_messages(&self) -> &[String] {
        &self.debug_messages
    }

    /// Feed the frame's Tier-2 mod GUI batch (`gui_bridge` ABI) into the
    /// HUD pass. An empty batch is free: the lowering hook skips it
    /// before touching the draw list.
    pub fn set_gui_bridge_batch(&mut self, batch: GuiBridgeBatch) {
        self.gui_bridge_batch = batch;
        self.mod_overlay_layout = None;
    }

    /// Install (or clear) the declarative mod-UI manifest composited on
    /// the `ModOverlay` layer. The widget kit audits it before drawing
    /// with the current viewport and scale.
    pub fn set_mod_ui_manifest(&mut self, manifest: Option<megu_core::ui_contract::ModUiManifest>) {
        if self.mod_ui_manifest != manifest {
            self.mod_ui_manifest = manifest;
            self.mod_overlay_layout = None;
        }
    }

    /// The recipe/resource browser state (recipe overlay/world-drops data/MachineTutorial index, tabs,
    /// focused detail) — read-only view for host code and tests.
    pub fn recipe_browser(&self) -> &RecipeBrowserState {
        &self.ui.recipe_browser
    }

    /// The QoL `recipe_overlay_enabled` flag — `Ctrl+O` and
    /// Settings→QoL both write this single source of truth.
    pub fn recipe_overlay_enabled(&self) -> bool {
        self.ui.settings.qol.recipe_overlay_enabled
    }

    /// Runtime `MEGU EDITION` title branding assets loaded at boot —
    /// [`TitleBrandingAssets::fallback`] state (built-in splashes, no
    /// banner) when no local client JAR was discoverable.
    pub fn title_branding(&self) -> &TitleBrandingAssets {
        &self.title_branding
    }

    /// The session-deterministic title splash line from the merged
    /// pool (local JAR + mod JARs + built-in Megu set).
    pub fn active_splash(&self) -> &str {
        self.title_branding.select_splash(self.splash_seed)
    }

    /// Boot-time mod/vanilla/datapack ingest report (see
    /// [`crate::recipe_ingest`]); `None` counters when no `--gameDir`.
    pub fn recipe_ingest_report(&self) -> &crate::recipe_ingest::IngestReport {
        &self.recipe_ingest
    }

    /// Set the hovered container slot — the host-side mirror of
    /// [`ContainerScreenState::hover`] (cursor hover for mouse, and the
    /// path gamepad-style focus movement uses).
    pub fn set_container_hover(&mut self, slot: Option<usize>) {
        if let Some(screen) = &mut self.container_screen {
            screen.hover(slot);
        }
    }

    /// Open the client-side player inventory surface.
    pub fn open_player_inventory(&mut self) {
        self.open_own_inventory();
    }

    /// Apply a Vanilla container hotkey to the hovered slot and forward
    /// its click-window action to the live session.
    pub fn handle_container_slot_hotkey(&mut self, key: ContainerSlotHotkey) -> bool {
        let action = self
            .container_screen
            .as_mut()
            .and_then(|screen| screen.handle_slot_hotkey(key));
        if let Some(action) = action {
            if let Some(live) = &mut self.live {
                Self::forward_click_action(live, action);
            }
            true
        } else {
            false
        }
    }

    /// End a server-reported bed overlay through the live stop-sleeping action.
    pub fn leave_bed(&mut self) -> bool {
        let Some(live) = self.live.as_mut().filter(|live| live.is_sleeping()) else {
            return false;
        };
        live.send_stop_sleeping();
        true
    }

    /// Consume the live session's win-game edge and open localized credits.
    pub fn activate_pending_credits(&mut self) -> bool {
        let pending = self
            .live
            .as_mut()
            .is_some_and(LiveSession::take_pending_end_credits);
        if !pending {
            return false;
        }
        let mut credits = megu_ui::gameplay_screens::EndCreditsState::default();
        if let Some(texts) = self
            .vanilla_runtime_assets
            .as_ref()
            .map(|assets| &assets.i18n.texts_and_fonts)
        {
            if let Some(extras) = self.vanilla_official_extras.as_ref() {
                let merged = merged_end_credits_lines(texts, extras);
                if !merged.is_empty() {
                    credits.lines = merged;
                }
            } else {
                credits.load_text(
                    texts.end_text.as_deref().unwrap_or(""),
                    texts.postcredits_text.as_deref().unwrap_or(""),
                );
            }
        }
        self.credits_screen = Some(credits);
        self.paused = true;
        true
    }

    /// Close the credits screen and notify the server that the player is
    /// done. The same `client_command` packet doubles as the end-of-credits
    /// respawn; if the bounded worker queue is momentarily full or not yet
    /// live, `credits_ack_pending` keeps the close retryable on later frames.
    pub fn finish_end_credits_screen(&mut self) -> bool {
        if self.credits_screen.take().is_none() {
            return false;
        }
        self.paused = false;
        self.credits_ack_pending = true;
        self.service_end_credits_ack();
        true
    }

    /// Escape-close the credits overlay through the same acknowledgement
    /// path the natural scroll end uses. Public so platform/input adapters
    /// and integration tests exercise the exact wired behavior.
    pub fn escape_end_credits_screen(&mut self) -> bool {
        let closed = self.finish_end_credits_screen();
        if closed {
            self.maybe_regrab();
        }
        closed
    }

    /// Advance the credits overlay; when the scroll passes the end this
    /// closes through the same server acknowledgement path as Escape.
    /// Returns `true` while the overlay remains open afterwards.
    pub fn tick_end_credits_screen(&mut self, dt: f32, height: u32, fast_forward: bool) -> bool {
        if let Some(credits) = self.credits_screen.as_mut() {
            credits.tick(dt, fast_forward);
            if credits.finished(height) {
                self.finish_end_credits_screen();
            }
        }
        self.credits_screen.is_some()
    }

    fn service_end_credits_ack(&mut self) {
        if !self.credits_ack_pending {
            return;
        }
        let Some(live) = self.live.as_mut() else {
            self.credits_ack_pending = false;
            return;
        };
        if live.send_end_credits_complete() {
            self.credits_ack_pending = false;
        }
    }

    #[doc(hidden)]
    pub fn end_credits_ack_pending(&self) -> bool {
        self.credits_ack_pending
    }

    pub fn credits_screen_open(&self) -> bool {
        self.credits_screen.is_some()
    }

    pub fn live_is_sleeping(&self) -> bool {
        self.live.as_ref().is_some_and(LiveSession::is_sleeping)
    }

    /// Inject a session without a real socket — tests use the
    /// `offline_with_*` fixtures to drive the same window-0/advancements
    /// mirrors the network path fills.
    pub fn attach_live(&mut self, live: LiveSession) {
        self.credits_ack_pending = false;
        self.live = Some(live);
        self.on_live_attached();
    }

    /// Mirror of the `ModifiersChanged` event's state write — headless
    /// tests drive `Ctrl`/`Cmd` chords through the same field the real
    /// event path sets.
    pub fn set_modifiers(&mut self, state: ModifiersState) {
        self.modifiers = state;
    }

    /// One-time per-attach wiring shared by `connect_to`/`attach_live`:
    /// arms the compute-strategy push the next live frame performs and
    /// replays existing chat feedback into the session's chat mirror —
    /// `sync_hud_to` replaces `ui.hud.chat_lines` wholesale on every HUD
    /// revision, so the log lives in `live.hud` where the sync carries it.
    fn on_live_attached(&mut self) {
        self.live_hud_metadata = None;
        self.live_ui_locale = None;
        self.inspector_hud_cache = None;
        self.live_strategy_pending = true;
        let audio_settings = self.ui.settings.audio.clone();
        let world_assets = self
            .vanilla_runtime_assets
            .as_ref()
            .map(|assets| &assets.world_assets);
        let Some(live) = &mut self.live else {
            return;
        };
        if let Some(world_assets) = world_assets {
            live.apply_biome_colormaps(world_assets);
        }
        // The session spawns its own mixer — seed the persisted volume
        // sliders before the first packet can start a voice.
        live.set_audio_volumes(&audio_settings);
        live.set_chat_history_limit(self.ui.settings.qol.chat_history_lines);
        for message in self.ui.hud.chat_lines.iter().cloned() {
            live.push_chat_line(message);
        }
        live.hud_revision += 1;
    }

    /// Push the session's connect-time `ComputeStrategy` onto this
    /// renderer the first time it draws a live frame —
    /// `on_live_attached` arms `live_strategy_pending` and a fresh
    /// `ActiveWindow` re-arms it, so device re-creation re-applies
    /// uploads/workers/indirect policy. The strategy itself fails
    /// closed: GPU indirect/Hi-Z/MDI engage only when
    /// `indirect_execution_supported()` and the unified plan allow it.
    #[doc(hidden)]
    pub fn ensure_live_compute_strategy(
        live: &mut LiveSession,
        pending: &mut bool,
        renderer: &mut GpuRenderer,
    ) {
        if !*pending {
            return;
        }
        *pending = false;
        live.apply_compute_strategy(renderer);
    }

    /// `true` while the next live frame will push the compute strategy —
    /// armed by `on_live_attached` and every fresh `ActiveWindow`.
    #[doc(hidden)]
    pub fn live_strategy_pending(&self) -> bool {
        self.live_strategy_pending
    }

    /// Drive the pending strategy push for the attached session (test +
    /// headless-hook entry point; the render loop calls
    /// [`Self::ensure_live_compute_strategy`] directly so it can borrow
    /// the fields disjointly).
    #[doc(hidden)]
    pub fn push_pending_compute_strategy(&mut self, renderer: &mut GpuRenderer) {
        let pending = &mut self.live_strategy_pending;
        if let Some(live) = &mut self.live {
            Self::ensure_live_compute_strategy(live, pending, renderer);
        }
    }

    /// Feed one raw `megu:portal_seamless_v1` (`MSP1`) channel payload
    /// into the Phase-1 seamless dimension transfer state machine.
    /// `false` for any other channel or a frame the fail-closed codec
    /// rejects — the caller keeps handling those as ordinary unknown
    /// channels. S2C variants drive standby-prefetch/commit bookkeeping;
    /// a `PrefetchAck` seen in-band is ignored (acks flow C2S only).
    #[doc(hidden)]
    pub fn apply_seamless_portal_channel(&mut self, channel: &str, payload: &[u8]) -> bool {
        if channel != SEAMLESS_PORTAL_CHANNEL {
            return false;
        }
        let Ok(packet) = decode_seamless_portal_packet(payload) else {
            return false;
        };
        match packet {
            SeamlessPortalPacket::PrefetchHint(hint) => {
                self.seamless_portal.on_prefetch_hint(&hint);
            }
            SeamlessPortalPacket::PrefetchCancel { transfer_id } => {
                self.seamless_portal.on_prefetch_cancel(transfer_id);
            }
            SeamlessPortalPacket::CommitTransfer(commit) => {
                self.seamless_portal.on_commit_transfer(&commit);
            }
            SeamlessPortalPacket::PrefetchAck { .. } => {}
        }
        true
    }

    /// `true` while the Phase-1 seamless transfer committed the live
    /// session's current dimension — a blocking "Loading Terrain" overlay
    /// (and the per-chunk arrival dissolve) must stay suppressed so the
    /// swap lands in one frame with zero black screen.
    #[doc(hidden)]
    pub fn seamless_loading_suppressed(&self) -> bool {
        self.live.as_ref().is_some_and(|live| {
            self.seamless_portal
                .should_suppress_loading_screen(&live.world.dimension.name)
        })
    }

    /// Rebuild the live entity + block-entity draw set: articulated rig
    /// meshes keyed by entity id + pose fingerprint (rebuilt only when
    /// the spec or pose actually changes), plus one canonical template
    /// per BER family placed at each tracked block-entity position.
    /// Throttled by the caller to the 20 Hz server cadence; an empty
    /// world costs only the two map scans and clears a stale upload
    /// exactly once.
    fn refresh_live_entity_and_ber(
        live: &LiveSession,
        cache: &mut LiveEntityRenderCache,
        renderer: &mut GpuRenderer,
        entity_shadows: bool,
        enchantment_glint: bool,
        appearance: Option<&RuntimeEntityAppearanceAtlas>,
        dt: f32,
    ) {
        cache.entity_meshes.retain(|eid, _| {
            live.entity_rig(*eid).is_some() || live.world.entities.contains_key(eid)
        });
        cache
            .entity_visuals
            .retain(|eid, _| live.world.entities.contains_key(eid));
        cache
            .entity_interpolation
            .retain(|eid, _| live.world.entities.contains_key(eid));
        cache
            .dynamic_ber_meshes
            .retain(|pos, _| live.world.block_entities.contains_key(pos));
        cache
            .ber_animation_easing
            .retain(|pos, _| live.world.block_entities.contains_key(pos));
        // Pass 1 mutates the caches (new/pose-changed meshes only); pass
        // 2 then borrows them immutably to assemble the draw list — the
        // renderer consumes `&ExtrudedMesh` refs so no mesh is cloned.
        let mut entity_refs: Vec<(i32, [f32; 3], [f32; 3])> = Vec::new();
        for (eid, spec, pos, tracked) in live.entity_render_entries() {
            stage_live_entity(
                live,
                cache,
                &mut entity_refs,
                enchantment_glint,
                appearance,
                eid,
                spec,
                pos,
                tracked,
                dt,
            );
        }
        // Never-invisible modded entities: `add_entity` classified only
        // ids inside the vanilla table, so a registry id outside it
        // (modded `> u16::MAX` or unmapped) owns no `entity_rigs` entry
        // and `entity_render_entries` skipped it above — resolve a
        // visible `GenericProxy` rig instead of dropping the entity.
        for (eid, spec, pos, tracked) in unrigged_entity_fallbacks(live) {
            stage_live_entity(
                live,
                cache,
                &mut entity_refs,
                enchantment_glint,
                appearance,
                eid,
                spec,
                pos,
                Some(tracked),
                dt,
            );
        }
        // Do not render the model around its own first-person camera.
        if let Some(camera_id) = live.world.camera_entity_id {
            entity_refs.retain(|(eid, _, _)| *eid != camera_id);
        }
        // Grounded contact shadows ride the same lit entity/BER list —
        // each quad samples world light at its centroid through the same
        // `sample_live_entity_light` closure the rigs use, gated on the
        // `entity_shadows` settings key.
        stage_contact_shadow_quads(live, entity_shadows, &mut cache.contact_shadow_quads);
        let mut ber_refs: Vec<StaticBerRef> = Vec::new();
        let mut animated_ber_refs: Vec<AnimatedBerRef> = Vec::new();
        for (block_pos, be) in &live.world.block_entities {
            let be_visual = live.world.block_entity_visual_state(*block_pos);
            let classified_kind = classify_tracked_block_entity(be);
            let be_uv = classified_kind
                .and_then(|kind| vanilla_ber_appearance_uv(appearance, kind, be_visual.as_ref()));
            if let Some(visual) = be_visual {
                if let Some(state) = live_animated_ber_state(live, cache, *block_pos, visual) {
                    let mesh = cache
                        .dynamic_ber_meshes
                        .entry(*block_pos)
                        .or_insert_with(|| (state, build_extended_ber_mesh(state.kind(), state)));
                    if mesh.0 != state {
                        *mesh = (state, build_extended_ber_mesh(state.kind(), state));
                    }
                    animated_ber_refs.push((*block_pos, [1.0; 3], be_uv));
                    continue;
                }
            }
            let Some(kind) = classified_kind else {
                continue;
            };
            let slot = ber_kind_slot(kind);
            if cache.ber_meshes[slot].is_none() {
                cache.ber_meshes[slot] =
                    Some(build_ber_mesh(kind, default_vanilla_ber_state(kind)));
            }
            ber_refs.push((
                slot,
                [
                    block_pos[0] as f32,
                    block_pos[1] as f32,
                    block_pos[2] as f32,
                ],
                vanilla_ber_base_color_f32(kind),
                be_uv,
            ));
        }
        let mut meshes: Vec<crate::renderer::TexturedEntityBerMesh<'_>> = Vec::with_capacity(
            entity_refs.len()
                + ber_refs.len()
                + animated_ber_refs.len()
                + cache.contact_shadow_quads.len(),
        );
        for (eid, origin, tint) in entity_refs {
            if let Some((_, mesh)) = cache.entity_meshes.get(&eid) {
                let layers = cache.entity_visuals.get(&eid).map(|(_, layers)| layers);
                let base_uv = layers.and_then(|layers| layers.base_uv);
                meshes.push((mesh, origin, tint, base_uv));
                if let Some(layers) = layers {
                    for overlay in &layers.overlays {
                        meshes.push((mesh, origin, overlay.tint, Some(overlay.uv)));
                    }
                }
            }
        }
        for (shadow_mesh, tint) in &cache.contact_shadow_quads {
            // Positions are already world-space (`floor_top + 0.015`), so
            // the staging origin stays at zero.
            meshes.push((shadow_mesh, [0.0, 0.0, 0.0], *tint, None));
        }
        for (slot, origin, tint, uv) in ber_refs {
            if let Some(mesh) = cache.ber_meshes[slot].as_ref() {
                meshes.push((mesh, origin, tint, uv));
            }
        }
        for (pos, tint, uv) in animated_ber_refs {
            if let Some((_, mesh)) = cache.dynamic_ber_meshes.get(&pos) {
                meshes.push((
                    mesh,
                    [pos[0] as f32, pos[1] as f32, pos[2] as f32],
                    tint,
                    uv,
                ));
            }
        }
        if meshes.is_empty() {
            if cache.staged {
                renderer.set_entity_and_ber_meshes_textured(Vec::new(), |x, y, z| {
                    sample_live_entity_light(live, x, y, z)
                });
                cache.staged = false;
            }
            return;
        }
        cache.staged = true;
        renderer.set_entity_and_ber_meshes_textured(meshes, |x, y, z| {
            sample_live_entity_light(live, x, y, z)
        });
    }

    /// Expand every live particle system (tactile sim, nature ambience,
    /// VFX1 particles) into camera-facing [`VanillaParticleQuad`]s in
    /// `scratch`, then stage them via
    /// [`GpuRenderer::set_particle_billboards`]. All systems empty ⇒ the
    /// ring is cleared exactly once and subsequent frames cost a few
    /// `is_empty` checks — zero wasted CPU when particles are off.
    fn stage_live_particles(
        live: &LiveSession,
        vanilla_particles: &VanillaParticleWorld,
        frame: &VfxFrameOutput,
        camera: &Camera,
        scratch: &mut Vec<VanillaParticleQuad>,
        staged: &mut bool,
        renderer: &mut GpuRenderer,
    ) {
        scratch.clear();
        let (right, up) =
            VanillaParticleWorld::billboard_axes(camera.right_vector(), camera.up_vector());
        for particle in live.tactile_sim().particles() {
            push_tactile_particle(scratch, particle, right, up);
        }
        for particle in live.nature_ambience.particles() {
            push_nature_particle(scratch, particle, right, up);
        }
        for particle in &frame.particles {
            push_vfx_particle(scratch, particle, right, up, camera.forward_vector());
        }
        for particle in vanilla_particles.particles() {
            let half = particle.billboard_half_size();
            let color = particle.color_at_age();
            push_particle_quad(
                scratch,
                particle.pos,
                right.map(|axis| axis * half),
                up.map(|axis| axis * half),
                color,
            );
        }
        if scratch.is_empty() {
            if *staged {
                renderer.set_particle_billboards(&[]);
                *staged = false;
            }
            return;
        }
        *staged = true;
        renderer.set_particle_billboards(scratch);
    }

    fn drain_protocol_particles(
        live: &mut LiveSession,
        particles: &mut VanillaParticleWorld,
        dt: f32,
    ) {
        while let Some(spawn) = live.world.level_particles.pop_front() {
            let options = live
                .world
                .level_particle_options
                .pop_front()
                .filter(|(id, _)| *id == spawn.particle_id)
                .map(|(_, options)| options);
            let Some(protocol_id) = u8::try_from(spawn.particle_id).ok() else {
                continue;
            };
            let Some(options) = options else {
                continue;
            };
            let payload = match options {
                megu_client::protocol::DecodedParticleOptions::Empty
                | megu_client::protocol::DecodedParticleOptions::Unknown(_) => {
                    VanillaParticlePayload::None
                }
                megu_client::protocol::DecodedParticleOptions::BlockState(state) => {
                    VanillaParticlePayload::BlockState(state.max(0) as u32)
                }
                megu_client::protocol::DecodedParticleOptions::Dust {
                    red,
                    green,
                    blue,
                    scale,
                } => VanillaParticlePayload::Dust {
                    rgb: [red, green, blue],
                    scale,
                },
                megu_client::protocol::DecodedParticleOptions::DustColorTransition {
                    from,
                    scale,
                    to,
                } => VanillaParticlePayload::DustColorTransition {
                    from_rgb: from,
                    to_rgb: to,
                    scale,
                },
                megu_client::protocol::DecodedParticleOptions::SculkCharge { roll } => {
                    VanillaParticlePayload::SculkCharge { roll }
                }
                megu_client::protocol::DecodedParticleOptions::Item { slot_wire } => {
                    let mut reader = WireReader::new(&slot_wire);
                    if !reader.read_bool().unwrap_or(false) {
                        VanillaParticlePayload::None
                    } else {
                        VanillaParticlePayload::ItemStack(
                            reader.read_varint().unwrap_or_default().max(0) as u32,
                        )
                    }
                }
                megu_client::protocol::DecodedParticleOptions::Vibration {
                    source,
                    arrival_ticks,
                } => {
                    let destination = match source {
                        megu_client::protocol::VibrationSource::Block(pos) => [
                            pos[0] as f32 + 0.5,
                            pos[1] as f32 + 0.5,
                            pos[2] as f32 + 0.5,
                        ],
                        megu_client::protocol::VibrationSource::Entity { .. } => [
                            spawn.pos[0] as f32,
                            spawn.pos[1] as f32,
                            spawn.pos[2] as f32,
                        ],
                    };
                    VanillaParticlePayload::Vibration {
                        origin: spawn.pos.map(|value| value as f32),
                        destination,
                        travel_ticks: arrival_ticks.clamp(1, i32::from(u16::MAX)) as u16,
                    }
                }
                megu_client::protocol::DecodedParticleOptions::Shriek { delay } => {
                    VanillaParticlePayload::Shriek {
                        delay_ticks: delay.max(0) as u32,
                    }
                }
                megu_client::protocol::DecodedParticleOptions::EntityEffect { argb } => {
                    VanillaParticlePayload::EntityEffect {
                        rgba: [
                            ((argb >> 16) & 0xff) as f32 / 255.0,
                            ((argb >> 8) & 0xff) as f32 / 255.0,
                            (argb & 0xff) as f32 / 255.0,
                            ((argb >> 24) & 0xff) as f32 / 255.0,
                        ],
                    }
                }
            };
            let count = if spawn.count == 0 {
                1
            } else {
                spawn.count.clamp(0, 32) as usize
            };
            for index in 0..count {
                let seed = spawn.pos[0].to_bits()
                    ^ (spawn.pos[1].to_bits() << 17)
                    ^ (spawn.pos[2].to_bits() << 37)
                    ^ ((index as u64) << 8)
                    ^ u64::from(protocol_id);
                let spread = |shift: u32| {
                    let bits = seed.rotate_left(shift).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                    ((bits >> 40) as f32 / 16_777_215.0) * 2.0 - 1.0
                };
                let velocity = if spawn.count == 0 {
                    spawn.offset.map(|value| value * spawn.max_speed)
                } else {
                    spawn
                        .offset
                        .map(|value| value * spread(11 + (index as u32 % 19)))
                };
                particles.spawn_protocol(
                    protocol_id,
                    spawn.pos.map(|value| value as f32),
                    velocity,
                    seed,
                    payload,
                );
            }
        }
        live.world.level_particle_options.clear();
        particles.step(dt.clamp(0.0, 0.1));
    }

    /// Drive one [`WindowInputEvent`] through the same handler `handle_key`
    /// uses — the display-free way tests exercise the keybind path.
    pub fn dispatch_input(&mut self, event: WindowInputEvent) {
        self.apply_window_input(event);
    }

    /// Apply a [`UiAction`] that needs no event loop. `QuitGame` is handled
    /// inside `apply_ui_action` instead — it owns `event_loop.exit()`.
    pub fn dispatch_ui_action(&mut self, action: &UiAction) {
        match action {
            UiAction::OpenScreen(screen) => {
                if *screen == Screen::Singleplayer {
                    self.refresh_singleplayer_worlds();
                }
                if *screen == Screen::Multiplayer {
                    self.refresh_multiplayer_servers();
                }
                // Both screens read `megu-friends.toml` + the FRL3
                // identity; reload on entry so a hand-edited file or a
                // late-bound relay address is picked up.
                if matches!(*screen, Screen::Friends | Screen::Multiplayer) {
                    self.load_friends_state();
                }
                self.ui.navigate(*screen);
            }
            UiAction::Back => {
                if self.live.is_some() {
                    self.paused = self.ui.screen != Screen::InGameHud;
                    self.maybe_regrab();
                }
                // Mod Manager Done/Back lands here (`ModManagerAction::Close`
                // flattens to `Back`); a staged or applied
                // non-hot-reload toggle kicks the in-process reload.
                self.maybe_soft_reload_mods();
            }
            UiAction::SettingsChanged => {
                let settings = self.ui.settings.clone();
                // Atomic persistence: overlay the mapped keys onto the
                // last loaded file state (file-only keys survive), then
                // the save guard writes only on real byte changes.
                self.persisted_settings =
                    ui_to_client_settings(&settings, &self.persisted_settings);
                if let Some(selection) = self.ui.shader_packs.pending() {
                    self.persisted_settings.shader_pack = Some(selection.pack_id);
                    self.persisted_settings.shader_profile = selection.profile;
                }
                self.apply_settings(&settings);
                self.reload_locale_assets_if_changed();
                self.settings_save_guard.mark_dirty();
                if let Err(err) = self
                    .settings_save_guard
                    .flush_if_dirty(&self.persisted_settings, &self.settings_path)
                {
                    self.status_line(format!("Settings save: {err}"));
                }
            }
            UiAction::StartSingleplayer => {
                if let Err(err) = self.start_singleplayer() {
                    self.status_line(format!("Singleplayer: {err}"));
                }
            }
            UiAction::PlayWorld { name, seed } => {
                let payload = self.ui.create_world_payload();
                let blank_seed_create = payload.name == name.as_str()
                    && !self
                        .ui
                        .singleplayer
                        .worlds
                        .iter()
                        .any(|world| world == name);
                let create_options =
                    (seed.is_some() || blank_seed_create).then_some(WorldLaunchOptions {
                        game_mode: payload.game_mode,
                        difficulty: payload.difficulty,
                    });
                if let Err(err) = self.start_singleplayer_world(name, *seed, create_options) {
                    self.status_line(format!("Singleplayer {name:?}: {err}"));
                }
            }
            UiAction::StartMultiplayer => {
                if let Err(err) = self.join_multiplayer() {
                    self.status_line(format!("Multiplayer: {err}"));
                }
            }
            UiAction::JoinServer { address, port } => {
                self.connect_to(address.clone(), *port);
            }
            UiAction::ConfirmModSyncDownload => self.start_mod_sync_download(),
            UiAction::CancelModSync => {
                self.mod_sync_generation = self.mod_sync_generation.wrapping_add(1);
                if let Some(worker) = &self.mod_sync_worker {
                    worker.cancel();
                }
                self.mod_sync_worker = None;
                self.ui.go_title();
            }
            UiAction::SaveInstantReplay => self.save_instant_replay(),
            UiAction::SaveServer {
                name,
                address,
                port,
            } => {
                if let Err(err) = self.save_server_entry(name, address, *port) {
                    self.multiplayer_error(&err);
                }
            }
            UiAction::DeleteServer { index } => {
                if let Err(err) = self.delete_server_entry(*index) {
                    self.multiplayer_error(&err);
                }
            }
            UiAction::JoinWithCode(code) => {
                if let Err(err) = self.join_with_code(*code) {
                    self.status_line(format!("Join with code: {err}"));
                }
            }
            UiAction::GenerateInvite => match self.generate_invite() {
                Ok(()) => {
                    self.status_line(format!("Invite ready: {}", self.ui.invite_display()));
                }
                Err(err) => self.status_line(format!("Invite: {err}")),
            },
            UiAction::CopyInvite(code) => {
                // The code stays visible on the Friends screen; the
                // status line says which store actually holds it (a
                // headless/missing-tool session only has the in-app
                // buffer).
                match self.clipboard.set_text(code) {
                    ClipboardOutcome::System => {
                        self.status_line(format!("Invite code {code} copied to clipboard"))
                    }
                    ClipboardOutcome::Fallback => {
                        self.status_line(format!("Invite code {code} copied (in-app buffer)"))
                    }
                }
            }
            UiAction::RevokeInvite => {
                self.ui.active_invite = None;
                match self.friend_host.as_mut().map(|host| host.revoke()) {
                    None => self.status_line("Invite revoked (no live relay session)"),
                    Some(Ok(())) => self.status_line("Invite revoked"),
                    Some(Err(err)) => self.status_line(format!("Revoke invite: {err}")),
                }
                self.friend_host = None;
            }
            UiAction::ShowStatistics => {
                let stats = self.live.as_ref().map_or_else(
                    || "Statistics: no live session".to_owned(),
                    LiveSession::statistics_summary,
                );
                self.status_line(stats);
            }
            UiAction::OpenShaderOptions => {
                self.open_shader_options();
            }
            UiAction::OpenUrl(url) => {
                if let Err(err) = open_url(url) {
                    self.status_line(format!("Open link: {err}"));
                }
            }
            UiAction::SubmitCrashReport => self.handle_submit_crash_report(),
            UiAction::PerformRespawn => {
                // The death screen stays up until the server's
                // respawn/health recovery clears `live.death`; only the
                // packet dispatch happens here. A refused request
                // surfaces on the status line instead of failing silent.
                let sent = self
                    .live
                    .as_mut()
                    .is_some_and(|live| live.request_respawn());
                if !sent {
                    self.status_line("Respawn: request not available");
                }
            }
            UiAction::LeaveWorld => self.leave_world_for_title(),
            UiAction::ModManager(action) => self.dispatch_mod_manager_action(action),
            UiAction::RecipeBrowser(action) => self.dispatch_recipe_browser_action(action),
            // Copy buttons on friend rows/guest links — same honest
            // reporting as `CopyInvite`: system store vs in-app buffer,
            // with a truncated preview so the click reads back what it
            // copied.
            UiAction::CopyText(text) => match self.clipboard.set_text(text) {
                ClipboardOutcome::System => self.status_line(format!(
                    "Copied to clipboard: {}",
                    truncate_for_status(text)
                )),
                ClipboardOutcome::Fallback => self.status_line(format!(
                    "Copied (in-app buffer): {}",
                    truncate_for_status(text)
                )),
            },
            UiAction::AddFriend { name, friend_id } => self.add_friend(name, friend_id),
            UiAction::RemoveFriend { friend_id } => self.remove_friend(friend_id),
            UiAction::SetFriendShareMode(mode) => self.set_friend_share_mode(*mode),
            UiAction::SetFriendSelected {
                friend_id,
                selected,
            } => self.set_friend_selected(friend_id, *selected),
            UiAction::SaveRelayAddr { addr } => self.save_relay_addr(addr),
            UiAction::GenerateGuestLink { friend_id } => self.generate_guest_link(friend_id),
            UiAction::JoinFriendWorld { index } => {
                if let Err(err) = self.join_friend_world(*index) {
                    self.status_line(format!("Friend world: {err}"));
                }
            }
            UiAction::JoinGuestLink { link } => {
                if let Err(err) = self.join_guest_link(link) {
                    self.status_line(format!("Guest link: {err}"));
                }
            }
            UiAction::Claims(action) => {
                // The `ClaimsWorld` sync channel is not wired into the
                // session yet — surface the overlay's request instead of
                // dropping it silently.
                self.status_line(format!("Claims: {action:?} (no claims-capable session)"));
            }
            UiAction::CloseSignEditor => self.finish_sign_editor(true),
            _ => {}
        }
        // Screen navigations change whether IME is allowed — settle the
        // flag here so the headless dispatch path stays consistent.
        self.update_ime();
    }

    fn mod_profile_root(&self) -> std::path::PathBuf {
        if let Some(server_mods) = self.session_paths.server_mods_dir.as_deref() {
            if server_mods
                .parent()
                .and_then(|generations| generations.parent())
                .is_some_and(|profile| {
                    profile.ends_with("server-mods") || profile.join("generations").exists()
                })
            {
                if let Some(profile) = server_mods.parent().and_then(|p| p.parent()) {
                    return profile.to_path_buf();
                }
            }
        }
        self.session_paths
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("mod-profiles")
            .join("default")
    }

    fn scan_soft_reload_assets(&self) -> SoftReloadAssetSnapshot {
        let (layers, _, _) = crate::live_session::discover_asset_layers(&self.session_paths);
        let mut block_keys = std::collections::BTreeSet::new();
        for layer in &layers {
            block_keys.extend(layer.blockstates.keys().cloned());
            block_keys.extend(
                layer
                    .models
                    .keys()
                    .filter_map(|key| key.strip_prefix("block/").map(|_| key.clone())),
            );
        }
        let translations = self
            .resourcepacks_root()
            .and_then(|root| crate::mod_translator::load_auto_translation_pack_ja_jp(&root).ok())
            .map_or(0, |entries| entries.len());
        SoftReloadAssetSnapshot {
            block_keys: block_keys.len(),
            recipes: self.ui.recipe_browser.index.recipes.len(),
            loot_overrides: self.ui.recipe_browser.index.dungeon_loot.len()
                + self.ui.recipe_browser.index.mob_drops.len(),
            translation_keys: translations,
        }
    }

    fn resourcepacks_root(&self) -> Option<std::path::PathBuf> {
        self.session_paths.resource_pack_dir.clone().or_else(|| {
            self.session_paths
                .game_dir
                .as_ref()
                .map(|dir| dir.join("resourcepacks"))
        })
    }

    fn mods_root(&self) -> std::path::PathBuf {
        self.session_paths
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("mods")
    }

    /// Apply generated `ja_jp` translations to the recipe/item catalog when
    /// the pack is enabled. The base JAR ingest supplies every other locale;
    /// this overlays only keys the generated pack actually owns.
    fn apply_auto_translation_overlay(&mut self) -> usize {
        if self
            .ui
            .settings
            .locale
            .code()
            .replace('-', "_")
            .eq_ignore_ascii_case("ja_jp")
            || !self
                .resource_pack_selection
                .iter()
                .any(|pack| pack == crate::mod_translator::AUTO_TRANSLATION_PACK_DIR)
        {
            return 0;
        }
        let Some(root) = self.resourcepacks_root() else {
            return 0;
        };
        let Ok(entries) = crate::mod_translator::load_auto_translation_pack_ja_jp(&root) else {
            return 0;
        };
        let mut applied = 0usize;
        for item in &mut self.ui.recipe_browser.index.items {
            let path = item.id.split(':').nth(1).unwrap_or(item.id.as_str());
            let namespace = item.id.split(':').next().unwrap_or("minecraft");
            for kind in ["item", "block"] {
                let key = format!("{kind}.{namespace}.{path}");
                if let Some(value) = entries.get(&key) {
                    item.localized_names
                        .insert("ja_jp".to_owned(), value.clone());
                    applied += 1;
                    break;
                }
            }
        }
        applied
    }

    fn push_soft_reload_toast(&mut self, summary: &SoftReloadSummary, body: &str) {
        let title = if self.ui.lang() == megu_ui::state::UiLang::Ja {
            "再読込が完了しました"
        } else {
            "Reload complete"
        };
        self.ui.push_toast(
            megu_ui::qol_helpers::ToastKind::Success,
            title,
            &format!(
                "{}{body}",
                soft_reload_summary_line(summary, self.ui.lang())
            ),
        );
    }

    /// Zero-window-close soft reload: stop/restart only the integrated
    /// world worker when one is active, rescan mod/resource layers, reload
    /// texture/GUI/language/recipe tables, and report deterministic content
    /// deltas. This method never touches `self.active`, the `wgpu` device,
    /// the title panorama or the process lifecycle.
    pub fn soft_reload_in_process(&mut self, reason: &str) -> SoftReloadReport {
        let before = self
            .soft_reload_asset_baseline
            .clone()
            .unwrap_or_else(|| self.scan_soft_reload_assets());
        let mut restarted_integrated_server = false;
        if self.singleplayer_server.is_some() {
            if let Some(world) = self.active_singleplayer_world.clone() {
                // Drop the old connection and child guard before relaunching;
                // the OS process/window/renderer remain fully alive.
                self.live = None;
                self.singleplayer_server = None;
                let game_mode = match self.ui.singleplayer.game_mode {
                    megu_ui::state::SingleplayerGameMode::Survival => "survival",
                    megu_ui::state::SingleplayerGameMode::Creative => "creative",
                    megu_ui::state::SingleplayerGameMode::Hardcore => "survival",
                };
                let difficulty = match self.ui.singleplayer.difficulty {
                    megu_ui::state::SingleplayerDifficulty::Peaceful => "peaceful",
                    megu_ui::state::SingleplayerDifficulty::Easy => "easy",
                    megu_ui::state::SingleplayerDifficulty::Normal => "normal",
                    megu_ui::state::SingleplayerDifficulty::Hard => "hard",
                };
                let hardcore = self.ui.singleplayer.game_mode
                    == megu_ui::state::SingleplayerGameMode::Hardcore;
                match Self::launch_singleplayer(
                    &self.config,
                    &world,
                    &self.persisted_settings,
                    Some(game_mode),
                    Some(difficulty),
                    hardcore,
                ) {
                    Ok(server) => {
                        let addr = server.addr();
                        self.singleplayer_server = Some(server);
                        self.connect_to(addr.ip().to_string(), addr.port());
                        restarted_integrated_server = true;
                    }
                    Err(err) => self.status_line(format!("Soft reload server restart: {err}")),
                }
            }
        }
        if let Some(live) = &mut self.live {
            live.refresh_asset_layers();
            let locale = self.ui.settings.locale.code().to_owned();
            live.set_ui_locale("und");
            live.set_ui_locale(&locale);
            self.live_ui_locale = Some(self.ui.settings.locale);
        }
        self.item_icon_atlas_uploaded = false;
        self.invalidate_vanilla_runtime_assets();
        self.load_gui_sprite_atlas();
        self.ui.recipe_browser.index = megu_ui::recipe_browser::RecipeIndex::default();
        self.recipe_ingest = crate::recipe_ingest::ingest_mod_recipes_into_ui(
            &self.session_paths,
            &mut self.ui.recipe_browser,
        );
        let _ = crate::recipe_ingest::ingest_lang_into_ui(
            &self.session_paths,
            &mut self.ui.recipe_browser,
        );
        self.apply_auto_translation_overlay();
        self.lang_ingested_locale = self.ui.recipe_browser.active_locale.clone();
        let after = self.scan_soft_reload_assets();
        let generation_id = crate::mod_profile_tx::ModProfileStore::open(self.mod_profile_root())
            .ok()
            .and_then(|store| store.active_generation_id().ok().flatten());
        let summary = SoftReloadSummary {
            blocks_delta: after.block_keys as i64 - before.block_keys as i64,
            recipes_delta: after.recipes as i64 - before.recipes as i64,
            loot_overrides: after.loot_overrides,
            translations_delta: after.translation_keys as i64 - before.translation_keys as i64,
            generation_id,
        };
        self.soft_reload_asset_baseline = Some(after);
        self.push_soft_reload_toast(&summary, &format!(" — {reason}"));
        self.status_line(soft_reload_summary_line(&summary, self.ui.lang()));
        SoftReloadReport {
            summary,
            restarted_integrated_server,
            window_preserved: true,
        }
    }

    /// Accessory used by integration tests to prove the API is non-destructive
    /// even when no platform window exists in the headless host.
    pub fn soft_reload_preserves_window_contract(&self) -> bool {
        true
    }

    /// Mod Manager close hook: when a non-hot-reload mod was toggled or
    /// applied, refresh mod assets + translations in-process (the window
    /// never leaves the loop), surface the localized status line, then
    /// acknowledge so a clean close stays cheap. Poll-style: the menu
    /// owns the sticky flag, so this is a no-op on every other `Back`.
    fn maybe_soft_reload_mods(&mut self) {
        if !self.ui.mod_manager.needs_soft_reload_on_close() {
            return;
        }
        let status = self.ui.mod_manager.soft_reload_status(self.ui.lang());
        self.status_line(status);
        self.soft_reload_in_process("mod manager");
        self.ui.mod_manager.acknowledge_soft_reload();
    }

    fn normalized_mod_lookup_key(value: &str) -> String {
        value
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric())
            .map(|ch| ch.to_ascii_lowercase())
            .collect()
    }

    fn find_mod_jar_for_id(&self, mod_id: &str) -> Option<std::path::PathBuf> {
        let wanted = Self::normalized_mod_lookup_key(mod_id);
        if wanted.is_empty() {
            return None;
        }
        let mut jars = Vec::new();
        if let Ok(read) = std::fs::read_dir(self.mods_root()) {
            jars.extend(read.flatten().map(|entry| entry.path()).filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
            }));
        }
        if let Some(active) = crate::mod_profile_tx::ModProfileStore::open(self.mod_profile_root())
            .ok()
            .and_then(|store| store.active_generation_dir().ok().flatten())
        {
            if let Ok(read) = std::fs::read_dir(active) {
                jars.extend(read.flatten().map(|entry| entry.path()).filter(|path| {
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
                }));
            }
        }
        jars.sort();
        jars.truncate(512);
        let mut fallback = None;
        for jar in jars {
            let Some(stem) = jar.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let key = Self::normalized_mod_lookup_key(stem);
            if key == wanted {
                return Some(jar);
            }
            if fallback.is_none() && (key.starts_with(&wanted) || wanted.starts_with(&key)) {
                fallback = Some(jar);
            }
        }
        fallback
    }

    fn stage_user_edit_generation(
        &self,
        enabled_ids: &[String],
        disabled_ids: &[String],
    ) -> Result<crate::mod_profile_tx::ProfileActivationDiff, String> {
        let store = crate::mod_profile_tx::ModProfileStore::open(self.mod_profile_root())?;
        let mut staged = store.stage_generation(crate::mod_profile_tx::SOURCE_USER_EDIT)?;
        let mut entries = Vec::new();
        let disabled: std::collections::BTreeSet<&str> =
            disabled_ids.iter().map(String::as_str).collect();
        let mut present = std::collections::BTreeSet::new();
        if let Some(active_id) = store.active_generation_id()? {
            let active = store.manifest(&active_id)?;
            let active_dir = store.generations_root().join(active_id);
            for entry in active.mods {
                if disabled.contains(entry.mod_id.as_str()) {
                    continue;
                }
                let source = active_dir.join(&entry.filename);
                let target = staged.generation_dir.join(&entry.filename);
                std::fs::copy(&source, &target).map_err(|e| {
                    format!("copy {} into staged generation: {e}", source.display())
                })?;
                present.insert(entry.mod_id.clone());
                entries.push(entry);
            }
        }
        // Bootstrap mode (no prior transactional generation) and explicit
        // re-enable both resolve jars from the editable mods directory. A jar
        // that cannot be attributed to a requested mod is skipped fail-closed
        // rather than being smuggled into the signed manifest.
        let bootstrap_ids: Vec<String> = if entries.is_empty() {
            self.ui
                .mod_manager
                .entries
                .iter()
                .filter(|entry| entry.enabled && !disabled.contains(entry.mod_id.as_str()))
                .map(|entry| entry.mod_id.clone())
                .collect()
        } else {
            enabled_ids.to_vec()
        };
        for mod_id in bootstrap_ids {
            if present.contains(&mod_id) || disabled.contains(mod_id.as_str()) {
                continue;
            }
            let Some(source) = self.find_mod_jar_for_id(&mod_id) else {
                continue;
            };
            let Some(filename) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            crate::mod_profile_tx::validate_member_filename(filename)?;
            let bytes =
                std::fs::read(&source).map_err(|e| format!("read {}: {e}", source.display()))?;
            if bytes.len() as u64 > crate::mod_profile_tx::MAX_JAR_BYTES {
                return Err(format!("{} exceeds the mod jar size cap", source.display()));
            }
            let target = staged.generation_dir.join(filename);
            std::fs::write(&target, &bytes)
                .map_err(|e| format!("stage {}: {e}", target.display()))?;
            let version = self
                .ui
                .mod_manager
                .entries
                .iter()
                .find(|entry| entry.mod_id == mod_id)
                .map(|entry| entry.version.clone())
                .unwrap_or_else(|| "unknown".into());
            present.insert(mod_id.clone());
            entries.push(crate::mod_profile_tx::GenerationModEntry {
                mod_id: mod_id.clone(),
                version,
                filename: filename.to_owned(),
                sha512: crate::mod_profile_tx::hash_bytes_sha512_hex(&bytes),
                sha256: Some(megu_core::crypto::sha256_hex(&bytes)),
                source_reason: crate::mod_profile_tx::SOURCE_USER_EDIT.into(),
                modrinth_project_id: None,
                modrinth_version_id: None,
                requested_by: vec![mod_id],
            });
        }
        if entries.is_empty() {
            return Err("mod profile edit would leave an empty/unresolved generation".into());
        }
        store.publish_staged_generation(
            &mut staged,
            crate::mod_profile_tx::SOURCE_USER_EDIT,
            entries,
        )
    }

    fn apply_mod_manager_changes(&mut self, enabled_ids: &[String], disabled_ids: &[String]) {
        self.prime_soft_reload_baseline();
        match self.stage_user_edit_generation(enabled_ids, disabled_ids) {
            Ok(diff) => {
                if let Ok(store) =
                    crate::mod_profile_tx::ModProfileStore::open(self.mod_profile_root())
                {
                    if let Ok(Some(dir)) = store.active_generation_dir() {
                        self.session_paths.server_mods_dir = Some(dir.clone());
                        if let Some(live) = &mut self.live {
                            live.session_paths.server_mods_dir = Some(dir);
                        }
                    }
                }
                let line = profile_activation_diff_line(&diff, self.ui.lang());
                self.ui.push_toast(
                    megu_ui::qol_helpers::ToastKind::Success,
                    if self.ui.lang() == megu_ui::state::UiLang::Ja {
                        "Modプロファイルを適用"
                    } else {
                        "Mod profile applied"
                    },
                    &line,
                );
                self.soft_reload_in_process("mod profile apply");
                self.ui.mod_manager.acknowledge_soft_reload();
            }
            Err(err) => self.status_line(format!("Mod profile apply: {err}")),
        }
    }

    fn rollback_mod_profile(&mut self) {
        self.prime_soft_reload_baseline();
        let root = self.mod_profile_root();
        let result = crate::mod_profile_tx::ModProfileStore::open(&root)
            .and_then(|store| store.rollback_to_last_good_generation());
        match result {
            Ok(result) => {
                if let Ok(store) = crate::mod_profile_tx::ModProfileStore::open(&root) {
                    if let Ok(Some(dir)) = store.active_generation_dir() {
                        self.session_paths.server_mods_dir = Some(dir.clone());
                        if let Some(live) = &mut self.live {
                            live.session_paths.server_mods_dir = Some(dir);
                        }
                    }
                }
                let diff = profile_activation_diff_line(&result.diff, self.ui.lang());
                self.ui.push_toast(
                    megu_ui::qol_helpers::ToastKind::Success,
                    if self.ui.lang() == megu_ui::state::UiLang::Ja {
                        "正常プロファイルへ復元"
                    } else {
                        "Known-good profile restored"
                    },
                    &format!("{} ({})", diff, result.restored_generation_id),
                );
                self.soft_reload_in_process("mod profile rollback");
            }
            Err(err) => self.status_line(format!("Mod profile rollback: {err}")),
        }
    }

    fn missing_dependency_descriptors(
        &self,
        mod_id: &str,
        deps: &[String],
    ) -> Vec<crate::mod_sync_auto::MissingDependencyDescriptor> {
        deps.iter()
            .take(64)
            .map(|dep| crate::mod_sync_auto::MissingDependencyDescriptor {
                mod_id: dep.clone(),
                required_version_range: "*".into(),
                requesting_mods: vec![mod_id.to_owned()],
                loader: crate::mod_sync_auto::ModLoader::NeoForge,
                game_version: self.config.mc_version.clone(),
                side: crate::mod_sync_auto::ModSide::Client,
                project_id: None,
                version_id: None,
                modrinth_slug: None,
            })
            .collect()
    }

    fn start_missing_dependency_resolution(&mut self, mod_id: &str, deps: &[String]) {
        if self.missing_dep_worker.is_some() {
            self.status_line("Missing dependency resolution is already running");
            return;
        }
        let descriptors = self.missing_dependency_descriptors(mod_id, deps);
        if descriptors.is_empty() {
            return;
        }
        self.missing_dep_worker = Some(MissingDependencyConsentWorker::spawn(descriptors));
        self.status_line("Resolving exact missing dependency candidates...");
    }

    fn present_missing_dependency_plan(
        &mut self,
        plan: crate::mod_sync_auto::MissingDependencyConsentPlan,
    ) {
        use megu_ui::mod_manager as ui;
        self.missing_dep_plan = Some(plan.clone());
        let rows = plan
            .candidates
            .iter()
            .map(|row| ui::MissingDependencyConsentRow {
                mod_id: row.mod_id.clone(),
                version_number: row.version_number.clone(),
                filename: row.filename.clone(),
                sha512: row.sha512.clone(),
                size_bytes: row.size_bytes,
                requesting_mods: row.requesting_mods.clone(),
            })
            .collect();
        let unsatisfied = plan
            .unsatisfied_dependencies
            .iter()
            .map(|row| ui::MissingDependencyUnsatisfiedRow {
                mod_id: row.mod_id.clone(),
                required_version_range: row.required_version_range.clone(),
                requesting_mods: row.requesting_mods.clone(),
                reason: row.reason.clone(),
            })
            .collect();
        self.ui
            .mod_manager
            .set_missing_dependency_consent(ui::MissingDependencyConsentUiState {
                rows,
                unsatisfied,
                isolated_mods_if_declined: plan.isolated_mods_if_declined.clone(),
                decision: None,
            });
    }

    fn current_missing_dependency_plan_candidates(
        &self,
    ) -> Vec<crate::mod_sync_auto::ExactModRequirement> {
        let Some(plan) = self.missing_dep_plan.as_ref() else {
            return Vec::new();
        };
        plan.candidates
            .iter()
            .map(|row| crate::mod_sync_auto::ExactModRequirement {
                mod_id: row.mod_id.clone(),
                required_version: row.version_number.clone(),
                sha512: Some(row.sha512.clone()),
                sha256: row.sha256.clone(),
                modrinth_slug: None,
                project_id: Some(row.project_id.clone()),
                version_id: Some(row.version_id.clone()),
                game_version: self.config.mc_version.clone(),
                loader: crate::mod_sync_auto::ModLoader::NeoForge,
                side: crate::mod_sync_auto::ModSide::Client,
                requested_by: row.requesting_mods.clone(),
            })
            .collect()
    }

    fn resolve_missing_dependency_consent(
        &mut self,
        decision: megu_ui::mod_manager::MissingDependencyConsentDecision,
    ) {
        use megu_ui::mod_manager::MissingDependencyConsentDecision as Decision;
        match decision {
            Decision::ApproveAutoFetchAndSoftReload => {
                if let Some(worker) = &self.missing_dep_download_worker {
                    worker.cancel();
                    self.status_line(
                        "Dependency download is already running; cancellation requested",
                    );
                    return;
                }
                let reqs = self.current_missing_dependency_plan_candidates();
                if reqs.is_empty() {
                    self.status_line("No exact dependency candidates are available");
                    return;
                }
                self.missing_dep_download_worker = Some(MissingDependencyDownloadWorker::spawn(
                    reqs,
                    self.mod_profile_root(),
                ));
                self.status_line("Downloading approved missing dependencies...");
            }
            Decision::IsolateUnsatisfiedMods => {
                let isolated = self
                    .ui
                    .mod_manager
                    .missing_dependency_consent
                    .as_ref()
                    .map(|plan| plan.isolated_mods_if_declined.clone())
                    .unwrap_or_default();
                for entry in &mut self.ui.mod_manager.entries {
                    if isolated.iter().any(|id| id == &entry.mod_id) {
                        entry.enabled = false;
                    }
                }
                self.ui.mod_manager.clear_missing_dependency_consent();
                self.missing_dep_plan = None;
                self.ui.push_toast(
                    megu_ui::qol_helpers::ToastKind::Warning,
                    if self.ui.lang() == megu_ui::state::UiLang::Ja {
                        "未充足Modを隔離"
                    } else {
                        "Unsatisfied mods isolated"
                    },
                    &isolated.join(", "),
                );
                self.apply_mod_manager_changes(&[], &isolated);
            }
        }
    }

    fn plamo_cache_dir(&self) -> std::path::PathBuf {
        crate::mod_translator::default_translation_cache_dir()
    }

    fn plamo_preset(
        preset: megu_ui::mod_manager::PlamoTranslatePreset,
    ) -> crate::mod_translator::Plamo3Preset {
        use megu_ui::mod_manager::PlamoTranslatePreset as Ui;
        match preset {
            Ui::Lightweight08B => crate::mod_translator::Plamo3Preset::Lightweight08B,
            Ui::Standard2B => crate::mod_translator::Plamo3Preset::Standard2B,
            Ui::HighQuality8B => crate::mod_translator::Plamo3Preset::HighQuality8B,
        }
    }

    fn refresh_plamo_panel_status(&mut self) {
        let preset = Self::plamo_preset(self.ui.mod_manager.plamo_panel.preset);
        let status = crate::mod_translator::detect_install_status(&self.plamo_cache_dir(), preset);
        let ui_status = match status.state() {
            crate::mod_translator::TranslatorInstallState::ModelNotInstalled => {
                megu_ui::mod_manager::PlamoTranslatorStatus::NotInstalled
            }
            crate::mod_translator::TranslatorInstallState::RuntimeMissing => {
                megu_ui::mod_manager::PlamoTranslatorStatus::RuntimeMissing
            }
            crate::mod_translator::TranslatorInstallState::Ready => {
                megu_ui::mod_manager::PlamoTranslatorStatus::Ready
            }
        };
        let pack_selected = self
            .resource_pack_selection
            .iter()
            .any(|pack| pack == crate::mod_translator::AUTO_TRANSLATION_PACK_DIR);
        let enabled = pack_selected || self.ui.settings.plamo_auto_translation_pack;
        self.ui.settings.plamo_auto_translation_pack = enabled;
        self.ui.mod_manager.set_plamo_status(
            ui_status,
            self.ui.mod_manager.plamo_panel.preset,
            enabled,
        );
        if self.plamo_worker.is_none() {
            self.ui.mod_manager.plamo_panel.busy = false;
        }
    }

    fn start_plamo_work(&mut self, ui_preset: megu_ui::mod_manager::PlamoTranslatePreset) {
        if self.plamo_worker.is_some() {
            self.status_line("PLaMo translator work is already running");
            return;
        }
        let preset = Self::plamo_preset(ui_preset);
        let cache = self.plamo_cache_dir();
        let status = crate::mod_translator::detect_install_status(&cache, preset);
        let kind = match status.state() {
            crate::mod_translator::TranslatorInstallState::ModelNotInstalled => {
                PlamoWorkKind::Install
            }
            crate::mod_translator::TranslatorInstallState::Ready => PlamoWorkKind::Translate,
            crate::mod_translator::TranslatorInstallState::RuntimeMissing => {
                self.status_line(
                    "llama-server was not found; install it or set MEGU_LLAMA_SERVER_BIN",
                );
                return;
            }
        };
        let resourcepacks = self
            .resourcepacks_root()
            .unwrap_or_else(|| std::path::PathBuf::from("resourcepacks"));
        let mods = self.mods_root();
        self.plamo_worker = Some(PlamoWorker::spawn(kind, cache, mods, resourcepacks, preset));
        self.ui.mod_manager.plamo_panel.busy = true;
        self.ui.mod_manager.plamo_panel.progress_label = match kind {
            PlamoWorkKind::Install => "downloading model".into(),
            PlamoWorkKind::Translate => "starting local translation server".into(),
        };
    }

    fn cancel_plamo_work(&mut self) {
        if let Some(worker) = &self.plamo_worker {
            worker.cancel();
            self.ui.mod_manager.plamo_panel.progress_label = "cancelling...".into();
        }
    }

    fn toggle_auto_translation_pack(&mut self, enabled: bool) {
        self.prime_soft_reload_baseline();
        crate::mod_translator::set_auto_translation_pack_enabled(
            &mut self.resource_pack_selection,
            enabled,
        );
        if let Some(game_dir) = self.session_paths.game_dir.as_deref() {
            if let Err(err) = persist_resource_pack_stack(game_dir, &self.resource_pack_selection) {
                self.status_line(format!("Resource pack setting save: {err}"));
            }
        }
        self.refresh_plamo_panel_status();
        self.soft_reload_in_process(if enabled {
            "auto translation pack enabled"
        } else {
            "auto translation pack disabled"
        });
    }

    fn prime_soft_reload_baseline(&mut self) {
        if self.soft_reload_asset_baseline.is_none() {
            self.soft_reload_asset_baseline = Some(self.scan_soft_reload_assets());
        }
    }

    fn poll_missing_dependency_workers(&mut self) {
        let consent_result = self
            .missing_dep_worker
            .as_ref()
            .and_then(|worker| worker.result.lock().ok()?.clone());
        if let Some(result) = consent_result {
            self.missing_dep_worker = None;
            match result {
                Ok(plan) => {
                    let candidates = plan.candidates.len();
                    let unsatisfied = plan.unsatisfied_dependencies.len();
                    self.present_missing_dependency_plan(plan);
                    self.status_line(format!(
                        "Missing dependency plan ready: {candidates} candidates, {unsatisfied} unsatisfied"
                    ));
                }
                Err(err) => {
                    self.missing_dep_plan = None;
                    self.status_line(format!("Missing dependency resolution: {err}"));
                }
            }
        }

        if let Some(worker) = self.missing_dep_download_worker.as_ref() {
            if let Ok(progress) = worker.progress.lock() {
                if let crate::mod_sync_auto::ModSyncProgress::Downloading {
                    file_index,
                    total_files,
                    ..
                } = &*progress
                {
                    let label =
                        format!("Downloading approved dependencies {file_index}/{total_files}");
                    if self.ui.title_status.as_deref() != Some(label.as_str()) {
                        self.ui.title_status = Some(label);
                    }
                }
            }
        }
        let download_result = self
            .missing_dep_download_worker
            .as_ref()
            .and_then(|worker| worker.result.lock().ok()?.clone());
        if let Some(result) = download_result {
            if let Some(worker) = self.missing_dep_download_worker.take() {
                let _ = worker.thread.join();
            }
            match result {
                Ok(diff) => {
                    if let Ok(store) =
                        crate::mod_profile_tx::ModProfileStore::open(self.mod_profile_root())
                    {
                        if let Ok(Some(dir)) = store.active_generation_dir() {
                            self.session_paths.server_mods_dir = Some(dir.clone());
                            if let Some(live) = &mut self.live {
                                live.session_paths.server_mods_dir = Some(dir);
                            }
                        }
                    }
                    self.ui.mod_manager.clear_missing_dependency_consent();
                    self.missing_dep_plan = None;
                    self.ui.push_toast(
                        megu_ui::qol_helpers::ToastKind::Success,
                        if self.ui.lang() == megu_ui::state::UiLang::Ja {
                            "不足依存を取得"
                        } else {
                            "Dependencies fetched"
                        },
                        &profile_activation_diff_line(&diff, self.ui.lang()),
                    );
                    self.soft_reload_in_process("missing dependencies auto-fetch");
                }
                Err(err) => {
                    self.missing_dep_plan = None;
                    self.status_line(format!("Dependency auto-fetch: {err}"));
                }
            }
        }
    }

    fn poll_plamo_worker(&mut self) {
        if let Some(worker) = self.plamo_worker.as_ref() {
            if let Ok(label) = worker.progress.lock() {
                if !label.is_empty() {
                    self.ui.mod_manager.plamo_panel.progress_label = label.clone();
                }
            }
        }
        let outcome = self
            .plamo_worker
            .as_ref()
            .and_then(|worker| worker.result.lock().ok()?.clone());
        let Some(outcome) = outcome else {
            return;
        };
        let Some(worker) = self.plamo_worker.take() else {
            return;
        };
        let kind = worker.kind;
        let _ = worker.thread.join();
        self.ui.mod_manager.plamo_panel.busy = false;
        self.ui.mod_manager.plamo_panel.progress_label.clear();
        match outcome {
            Ok(PlamoWorkOutcome::Installed) => {
                self.refresh_plamo_panel_status();
                self.ui.push_toast(
                    megu_ui::qol_helpers::ToastKind::Success,
                    if self.ui.lang() == megu_ui::state::UiLang::Ja {
                        "翻訳モデルをインストール"
                    } else {
                        "Translation model installed"
                    },
                    crate::mod_translator::PLAMO3_LICENSE_NAME,
                );
            }
            Ok(PlamoWorkOutcome::Translated(summary)) => {
                if !self
                    .resource_pack_selection
                    .iter()
                    .any(|pack| pack == crate::mod_translator::AUTO_TRANSLATION_PACK_DIR)
                {
                    crate::mod_translator::set_auto_translation_pack_enabled(
                        &mut self.resource_pack_selection,
                        true,
                    );
                    if let Some(game_dir) = self.session_paths.game_dir.as_deref() {
                        let _ =
                            persist_resource_pack_stack(game_dir, &self.resource_pack_selection);
                    }
                }
                self.refresh_plamo_panel_status();
                self.ui.push_toast(
                    megu_ui::qol_helpers::ToastKind::Success,
                    if self.ui.lang() == megu_ui::state::UiLang::Ja {
                        "Modを日本語化"
                    } else {
                        "Mods translated"
                    },
                    &format!(
                        "{} entries / {} namespaces",
                        summary.translated_entries, summary.translated_namespaces
                    ),
                );
                self.soft_reload_in_process("PLaMo translation");
            }
            Err(err) => {
                self.refresh_plamo_panel_status();
                self.status_line(match kind {
                    PlamoWorkKind::Install => format!("PLaMo model install: {err}"),
                    PlamoWorkKind::Translate => format!("PLaMo translation: {err}"),
                });
            }
        }
    }

    fn poll_live_mod_sync_diff(&mut self) {
        let Some(diff) = self
            .live
            .as_ref()
            .and_then(|live| live.mod_sync_diff.as_ref())
        else {
            return;
        };
        if diff.matches() {
            return;
        }
        let fingerprint_text = format!(
            "{}|{}|{}|{}",
            diff.added
                .iter()
                .map(|entry| entry.file_name.as_str())
                .collect::<Vec<_>>()
                .join(","),
            diff.updated
                .iter()
                .map(|entry| entry.file_name.as_str())
                .collect::<Vec<_>>()
                .join(","),
            diff.removed.join(","),
            diff.unchanged.len()
        );
        let fp = megu_core::crypto::sha256_hex(fingerprint_text.as_bytes());
        let fp = u64::from_str_radix(&fp[..16], 16).unwrap_or(0);
        if self.last_mod_sync_root_fp == Some(fp) {
            return;
        }
        self.last_mod_sync_root_fp = Some(fp);
        let body = format!(
            "+{} mods / ~{} updates / -{} local mods",
            diff.added.len(),
            diff.updated.len(),
            diff.removed.len()
        );
        self.ui.push_toast(
            megu_ui::qol_helpers::ToastKind::Warning,
            if self.ui.lang() == megu_ui::state::UiLang::Ja {
                "サーバーMod構成が不一致"
            } else {
                "Server mod profile mismatch"
            },
            &body,
        );
        self.status_line(format!("Server mod sync mismatch: {body}"));
    }

    /// Locale changes are immediate: vanilla language tables, the generated
    /// `megu_auto_ja_jp` overlay and live mod display names all reload with
    /// no restart and no window recreation.
    fn reload_locale_assets_if_changed(&mut self) {
        let locale = self.ui.settings.locale.code().to_owned();
        if locale == self.active_ui_locale {
            return;
        }
        self.active_ui_locale = locale.clone();
        self.invalidate_vanilla_runtime_assets();
        self.load_gui_sprite_atlas();
        let _ = crate::recipe_ingest::ingest_lang_into_ui(
            &self.session_paths,
            &mut self.ui.recipe_browser,
        );
        self.apply_auto_translation_overlay();
        self.lang_ingested_locale = self.ui.recipe_browser.active_locale.clone();
        if let Some(live) = &mut self.live {
            live.set_ui_locale("und");
            live.set_ui_locale(&locale);
            self.live_ui_locale = Some(self.ui.settings.locale);
        }
    }

    fn dispatch_mod_manager_action(&mut self, action: &megu_ui::mod_manager::ModManagerAction) {
        use megu_ui::mod_manager::ModManagerAction as Action;
        match action {
            Action::OpenModrinthBrowser => {
                if let Err(err) = open_url("https://modrinth.com/mods") {
                    self.status_line(format!("Open Modrinth: {err}"));
                }
            }
            Action::OpenModsFolder => {
                let dir = self.mods_root();
                if let Err(err) = std::fs::create_dir_all(&dir) {
                    self.status_line(format!("Open mods folder: {err}"));
                } else if let Err(err) = open_url(&dir.display().to_string()) {
                    self.status_line(format!("Open mods folder: {err}"));
                }
            }
            Action::OpenUrl(url) => {
                if let Err(err) = open_url(url) {
                    self.status_line(format!("Open link: {err}"));
                }
            }
            Action::ConfigureMod(mod_id) => {
                self.status_line(format!("Mod configuration for {mod_id} is not available"));
            }
            Action::DownloadMissingFromModrinth {
                mod_id,
                missing_deps,
            } => {
                self.start_missing_dependency_resolution(mod_id, missing_deps);
            }
            Action::ApplyChanges {
                enabled_ids,
                disabled_ids,
                requires_soft_restart: _,
            } => {
                self.apply_mod_manager_changes(enabled_ids, disabled_ids);
            }
            Action::RollbackModProfile => self.rollback_mod_profile(),
            Action::SelectPlamoPreset(_) => self.refresh_plamo_panel_status(),
            Action::InstallOrTranslatePlamo(preset) => self.start_plamo_work(*preset),
            Action::CancelPlamo => self.cancel_plamo_work(),
            Action::ToggleAutoTranslationPack(enabled) => {
                self.toggle_auto_translation_pack(*enabled);
            }
            Action::ResolveMissingDependencyConsent(decision) => {
                self.resolve_missing_dependency_consent(*decision);
            }
            Action::Close => {}
        }
    }

    fn open_shader_options(&mut self) {
        let root = self
            .session_paths
            .game_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("shaderpacks");
        let mut packs = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() && !path.is_dir() {
                    continue;
                }
                let extension = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or_default();
                if path.is_file()
                    && !matches!(extension.to_ascii_lowercase().as_str(), "zip" | "jar")
                {
                    continue;
                }
                let id = path.to_string_lossy().into_owned();
                let title = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Shader Pack")
                    .to_owned();
                packs.push(megu_ui::shader_screen::ShaderPackScreenDefinition {
                    id,
                    title,
                    ..Default::default()
                });
            }
        }
        packs.sort_by_key(|pack| pack.title.to_lowercase());
        self.ui.shader_packs = megu_ui::shader_screen::ShaderPackScreenState::new(packs);
        if let Some(selected) = self.persisted_settings.shader_pack.as_deref() {
            if let Some(index) = self
                .ui
                .shader_packs
                .packs
                .iter()
                .position(|pack| pack.id == selected || pack.title == selected)
            {
                self.ui.shader_packs.selected_pack = Some(index);
            }
        }
        self.shader_options_open = true;
        self.clear_gameplay_controls();
        self.set_cursor_grab(false);
    }

    fn apply_shader_screen_selection(
        &mut self,
        selection: megu_ui::shader_screen::ShaderPackSelection,
    ) {
        self.persisted_settings.shader_pack = Some(selection.pack_id.clone());
        self.persisted_settings.shader_profile = selection.profile;
        self.settings_save_guard.mark_dirty();
        if let Some(active) = &mut self.active {
            active
                .renderer
                .load_shaderpack(std::path::Path::new(&selection.pack_id));
            self.active_shaderpack_path = Some(selection.pack_id);
        }
        if let Err(err) = self
            .settings_save_guard
            .flush_if_dirty(&self.persisted_settings, &self.settings_path)
        {
            self.status_line(format!("Shader options save: {err}"));
        }
    }

    /// Enter the mismatch flow when a caller has a structured server mod manifest.
    pub fn open_mod_sync_prompt(
        &mut self,
        server_address: String,
        required: &[crate::mod_sync_auto::ModRequirement],
        installed: &std::collections::BTreeMap<String, String>,
    ) {
        self.clear_gameplay_controls();
        self.set_cursor_grab(false);
        self.last_server_address = Some(server_address.clone());
        let mismatches = crate::mod_sync_auto::compute_missing_mods(required, installed);
        let rows = mismatches
            .into_iter()
            .filter_map(|entry| {
                let requirement = required
                    .iter()
                    .find(|requirement| requirement.mod_id == entry.mod_id)?;
                let installed_version = match entry.kind {
                    crate::mod_sync_auto::ModMismatchKind::Missing => None,
                    crate::mod_sync_auto::ModMismatchKind::VersionMismatch {
                        installed, ..
                    } => Some(installed),
                };
                Some(megu_ui::state::ModSyncPromptRow {
                    mod_id: entry.mod_id,
                    required_version: requirement.required_version.clone(),
                    installed_version,
                    sha512: requirement.sha512.clone(),
                    sha256: requirement.sha256.clone(),
                    modrinth_slug: requirement.modrinth_slug.clone(),
                })
            })
            .collect();
        self.ui
            .open_mod_sync_prompt(megu_ui::state::ModSyncPromptState {
                server_address,
                rows,
                progress_label: "Waiting for confirmation".into(),
                progress_percent: None,
            });
    }

    /// Vanilla `Minecraft.clearLevel()` for the disconnect / leave-world
    /// paths: the last live frame left its sections resident in the renderer,
    /// so dropping the session alone would keep the disconnected world's
    /// terrain drawn behind the menu. Clear the session's columns and external
    /// payloads, then run one full reconcile against the emptied world —
    /// `update_live_terrain` retires every resident section it can no longer
    /// find, which is exactly the release path an unload takes.
    fn release_world_terrain(&mut self) {
        let (Some(live), Some(active)) = (self.live.as_mut(), self.active.as_mut()) else {
            return;
        };
        live.clear_external_sections();
        active.renderer.sync_external_sections_from(live);
        let dimension = live.world.dimension.clone();
        live.world.reset_dimension(dimension);
        live.discard_external_sections_delta();
        live.dirty_terrain_coords.clear();
        live.terrain_dirty = false;
        live.terrain_full_sync = false;
        active.renderer.update_live_terrain(&live.world);
    }

    /// Death-screen `Title Screen` button (also the only escape on
    /// hardcore deaths): drop the live session — and the integrated
    /// `megu` child when it hosted the world — plus every in-game
    /// overlay. The UI already navigated to `Screen::Title` inside
    /// `activate`.
    fn leave_world_for_title(&mut self) {
        // No world to share: revoke the friend session; the persisted
        // share mode re-arms on the next hosted world.
        self.stop_friend_share();
        self.release_world_terrain();
        self.live = None;
        self.singleplayer_server = None;
        self.active_singleplayer_world = None;
        self.container_screen = None;
        self.own_inventory_open = false;
        self.spectator_view = None;
        self.credits_screen = None;
        self.chat_bar = None;
        self.ui.sign_editor = None;
        self.advancements_open = false;
        self.paused = false;
        self.keys = Keys::default();
        self.update_ime();
        self.maybe_regrab();
    }

    /// Persist a redacted local crash report on explicit user confirmation from
    /// the CrashRecovery screen, strictly honoring `settings.qol.crash_report_mode`.
    fn handle_submit_crash_report(&mut self) {
        if self.ui.settings.qol.crash_report_mode == megu_ui::state::CrashReportMode::Off {
            self.status_line("Crash reporting is off (nothing saved or sent)");
            return;
        }
        let (kind_label, summary) = self
            .ui
            .crash
            .as_ref()
            .map(|c| (c.error_kind.clone(), c.detail_message.clone()))
            .unwrap_or_else(|| ("crash_recovery".to_string(), "client crash".to_string()));
        let crash_dir = self
            .config
            .game_dir
            .as_deref()
            .map(|dir| dir.join("crash-reports"))
            .unwrap_or_else(|| std::path::PathBuf::from("crash-reports"));
        let mut env = megu_client::crash_reporter::ReportEnv::detect();
        env.mc_version = self.config.mc_version.clone();
        env.engine_version = format!("{} {}", crate::CLIENT_GPU_NAME, env!("CARGO_PKG_VERSION"));
        env.subsystem = Some("client-gpu".to_string());
        let report = megu_client::crash_reporter::capture_panic_context(
            &env,
            megu_client::crash_reporter::ReportKind::Crash,
            &kind_label,
            &summary,
            None,
        );
        match megu_client::crash_reporter::write_local_report(&crash_dir, &report) {
            Ok(path) => self.status_line(format!(
                "Redacted crash report saved: {} (fp {})",
                path.display(),
                report.fingerprint
            )),
            Err(err) => {
                self.ui.crash_sent = false;
                self.status_line(format!("Crash report save failed: {err}"));
            }
        }
    }

    /// The recipe browser's only host-facing actions today are the recipe browser `+`
    /// transfer and `Close` (bookmark/toggle mutations are state-internal,
    /// and MachineTutorial opens through the `W` hotkey or the browser's own `P`
    /// key — both feed `browser.machine_tutorial`, no host action is emitted).
    /// `TransferToCraftingGrid` sends the serverbound `place_recipe`
    /// packet on a live session or surfaces an explicit status when
    /// offline — it is never dropped on the floor.
    fn dispatch_recipe_browser_action(&mut self, action: &RecipeBrowserAction) {
        match action {
            RecipeBrowserAction::TransferToCraftingGrid {
                recipe_id,
                max_craft,
            } => {
                let window_id = self
                    .container_screen
                    .as_ref()
                    .map(|screen| screen.window_id)
                    .or_else(|| {
                        self.live.as_ref().and_then(|live| {
                            live.open_screen
                                .as_ref()
                                .map(|s| s.window_id.clamp(0, 255) as u8)
                        })
                    })
                    .unwrap_or(0);
                // `make_all` is vanilla's shift-click fill-all flag: request
                // it whenever the inventory could run the recipe more than
                // once, so `+` fills the grid like a shift-click.
                let make_all = *max_craft > 1;
                match &mut self.live {
                    Some(live) => match live.status {
                        SessionStatus::Playing => {
                            if live.send_place_recipe(window_id, recipe_id, make_all) {
                                self.status_line(format!("Placing {recipe_id} in the grid"));
                            } else {
                                self.status_line(format!(
                                    "Place recipe {recipe_id}: send queue busy"
                                ));
                            }
                        }
                        SessionStatus::Connecting => {
                            self.status_line(format!("Place recipe {recipe_id}: still connecting"))
                        }
                        SessionStatus::Disconnected(_) | SessionStatus::Failed(_) => {
                            self.status_line(format!("Place recipe {recipe_id}: not connected"))
                        }
                    },
                    None => self.status_line(format!("Place recipe {recipe_id}: not connected")),
                }
            }
            RecipeBrowserAction::Close => {
                if self.container_screen.is_some() && self.ui.screen == Screen::InGameHud {
                    self.ui.recipe_browser.detail = None;
                    self.ui.settings.qol.recipe_overlay_enabled = false;
                } else {
                    self.container_suspended_for_recipe = false;
                    self.ui
                        .handle_input(UiInputEvent::KeyPressed(UiKey::Escape));
                }
            }
        }
    }

    pub fn camera_controller(&self) -> &CameraController {
        &self.camera_controller
    }

    pub fn render_config(&self) -> &RenderConfig {
        &self.render_config
    }

    /// Camera values can also be updated by a settings source with FOV,
    /// sensitivity and zoom-divisor controls.
    pub fn apply_camera_settings(&mut self, fov_deg: f32, sensitivity: f32, zoom_divisor: f32) {
        self.camera_controller
            .apply_settings(fov_deg, sensitivity, zoom_divisor);
        self.render_config.fov_deg = f64::from(self.camera_controller.base_fov_deg());
    }

    /// Apply settings immediately, including the active swapchain and GPU.
    /// Video/visual presets ride the render config; camera values (FOV,
    /// sensitivity, zoom divisor) and the audio volume sliders apply here
    /// too, so every `SettingsChanged` action is fully live.
    pub fn apply_settings(&mut self, settings: &UiSettings) {
        let video = &settings.video;
        let smooth_lighting_changed = self.render_config.smooth_lighting != video.smooth_lighting;
        self.ui.ui_scale = settings.gui_scale.min(4);
        self.camera_controller
            .set_smooth_zoom(settings.qol.smooth_zoom);
        self.apply_camera_settings(
            settings.fov_f32(),
            settings.mouse_sensitivity_f32(),
            settings.zoom_divisor_f32(),
        );
        let vsync_changed = self.render_config.vsync != video.vsync;
        let fps_cap = (video.fps_cap > 0).then_some(video.fps_cap);
        if self.render_config.max_fps != fps_cap {
            self.governor = FrameBudgetGovernor::new(fps_cap);
        }
        self.render_config.max_fps = fps_cap;
        self.render_config.background_fps_limit = video.background_fps;
        self.render_config.vsync = video.vsync;
        self.render_config.smooth_lighting = video.smooth_lighting;
        self.render_config.view_distance = Some(i32::from(video.view_distance));
        self.inventory_sort.enabled = settings.builtin_inventory_sort_active(false);
        // The renderer's frustum and section culler read `camera.far`;
        // updating RenderConfig alone does not alter the active GPU path.
        self.camera.far = effective_camera_far(video.view_distance, video.lod);
        if let Some(active) = &self.active {
            let should_fullscreen = settings.fullscreen;
            if active.window.fullscreen().is_some() != should_fullscreen {
                active
                    .window
                    .set_fullscreen(should_fullscreen.then_some(Fullscreen::Borderless(None)));
            }
        }
        let selected_backend = ui_gpu_backend(video.gpu_backend);
        if selected_backend != self.render_config.gpu_backend {
            self.status_line("GPU backend change requires restart".to_owned());
        }

        let desired_shaderpack = settings
            .shader_quick
            .shaders_enabled
            .then(|| {
                self.persisted_settings
                    .shader_pack
                    .as_deref()
                    .map(|pack| resolve_shaderpack_path(self.config.game_dir.as_deref(), pack))
            })
            .flatten();
        if let Some(active) = &mut self.active {
            active.renderer.set_gamma_pct(settings.gamma_pct);
            active.renderer.set_smooth_lighting(video.smooth_lighting);
            if desired_shaderpack != self.active_shaderpack_path {
                if let Some(path) = desired_shaderpack.as_deref() {
                    active.renderer.load_shaderpack(std::path::Path::new(path));
                } else {
                    active.renderer.disable_shaderpack();
                }
                self.active_shaderpack_path = desired_shaderpack;
            }
            // Realistic shadows ride the Megu Natural style only, and
            // only while a shadow consumer is on: the dedicated
            // `Shadows` toggle or the local shadow-light gameplay opt-in.
            // The adaptive frame-time governor can still force `Off`
            // under load on top of this player-facing quality.
            let shadow_quality = match settings.visual.preset {
                VisualStylePreset::MeguNatural
                    if video.entity_shadows || video.directional_shadow_gameplay =>
                {
                    BuiltinShadowQuality::Natural
                }
                _ => BuiltinShadowQuality::Off,
            };
            self.user_shadow_quality = shadow_quality;
            active.renderer.set_shadow_quality(shadow_quality);
            let scale = match video.preset {
                VideoPreset::Fast => RenderScalePreset::Performance67,
                VideoPreset::Balanced | VideoPreset::High | VideoPreset::Ultra => {
                    RenderScalePreset::Native100
                }
                VideoPreset::Custom => active.renderer.render_scale,
            };
            self.user_render_scale = scale;
            active.renderer.set_render_scale(scale);
            // Sky-dome star field + night drama ride the same toggle
            // pair: `World Effects` is the master switch, `Night Sky`
            // the sub-toggle — both must be on for stars to draw.
            active.renderer.set_night_sky(
                settings.visual.world_effects && settings.visual.world_effects_detail.night_sky,
            );
        }
        if vsync_changed {
            if let Some(size) = self.active.as_ref().map(|active| active.size) {
                self.reconfigure(size);
            }
        }

        // Live-session surfaces (QUAL-4): the 3D blocks/skins and nature
        // packs, the tactile weather/touch sim, and the spatial-audio
        // mixer preset all take the same toggles the settings screen
        // exposes.
        if let Some(live) = &mut self.live {
            if smooth_lighting_changed {
                live.rebuild_loaded_sections_for_lighting(
                    self.camera.position,
                    video.view_distance,
                );
            }
            live.set_chat_history_limit(settings.qol.chat_history_lines);
            live.vanilla_plus_cfg.three_d.enabled = settings.visual.blocks_and_skins_3d;
            live.vanilla_plus_cfg.nature.enabled = settings.visual.world_effects;
            live.vanilla_plus_cfg.nature.cloud_layers = cloud_layers_for_settings(settings);
            let detail = &settings.visual.world_effects_detail;
            let tactile = live.tactile_config().clone();
            live.set_tactile_config(TactileEffectsConfig {
                preset: if settings.visual.world_effects {
                    TactilePreset::Fancy
                } else {
                    TactilePreset::Off
                },
                water_and_lava_reactions: detail.water_splash || detail.ripples,
                tree_shake_and_footsteps: detail.cherry_leaves || detail.autumn_leaves,
                cold_breath_and_embers: detail.cold_breath,
                shooting_stars_and_aurora: detail.night_sky,
                subtle_camera_motion: settings.visual.camera_micro_motion,
                ..tactile
            });
            let preset = if settings.visual.spatial_audio_3d {
                SpatialAudioPreset::Realistic
            } else {
                SpatialAudioPreset::Off
            };
            live.set_spatial_audio_preset(preset);
            // After the preset (it replaces the mixer wholesale): master
            // + per-category gains and the limiter ceiling apply live on
            // both the sync mixer and the dedicated mix thread.
            live.set_audio_volumes(&settings.audio);
            // Render-distance changes while connected re-announce
            // `client_information` so the server re-centers the chunk
            // view (no-op unless the value actually moved).
            live.resend_client_information(video.view_distance as i8);
        }
    }

    /// `(try, fallback)` grab modes for mouselook. On Linux the detected
    /// [`crate::linux_desktop::TilingGrabPolicy`] picks the compositor's
    /// preferred order: `Locked`→`Confined` for native Wayland
    /// compositors (`zwp_locked_pointer_v1` + `zwp_relative_pointer_v1`
    /// unaccelerated raw deltas, so the invisible cursor can never hit
    /// the window borders), `Confined`→`Locked` for X11 WMs and
    /// Gamescope, where XI2 `DeviceEvent::MouseMotion` already delivers
    /// unaccelerated raw deltas and pointer lock trips cursor quirks.
    /// Other OSes keep the display-policy order (`Locked` first —
    /// macOS/Windows raw input). The second mode is always the fallback.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn cursor_grab_order(&self) -> (CursorGrabMode, CursorGrabMode) {
        match self.tiling_grab_policy.preferred_grabs()[0] {
            crate::linux_desktop::PointerGrab::Locked => {
                (CursorGrabMode::Locked, CursorGrabMode::Confined)
            }
            crate::linux_desktop::PointerGrab::Confined => {
                (CursorGrabMode::Confined, CursorGrabMode::Locked)
            }
        }
    }

    /// Non-Linux grab order: the display policy's
    /// `prefer_locked_pointer_first` flag (see the Unix sibling).
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    fn cursor_grab_order(&self) -> (CursorGrabMode, CursorGrabMode) {
        if self.policy.prefer_locked_pointer_first {
            (CursorGrabMode::Locked, CursorGrabMode::Confined)
        } else {
            (CursorGrabMode::Confined, CursorGrabMode::Locked)
        }
    }

    fn set_cursor_grab(&mut self, grab: bool) {
        let Some(active) = &self.active else {
            return;
        };
        if grab && !self.cursor_grabbed {
            let (first, second) = self.cursor_grab_order();
            active
                .window
                .set_cursor_grab(first)
                .or_else(|_| active.window.set_cursor_grab(second))
                .ok();
            active.window.set_cursor_visible(false);
            self.cursor_grabbed = true;
        } else if !grab && self.cursor_grabbed {
            active.window.set_cursor_grab(CursorGrabMode::None).ok();
            active.window.set_cursor_visible(true);
            self.cursor_grabbed = false;
        }
    }

    /// True when the pointer should be captured for mouselook: in-game,
    /// unpaused, no chat bar, no sign editor, no container/inventory
    /// screen, no advancements overlay.
    fn wants_cursor_grab(&self) -> bool {
        self.ui.screen == Screen::InGameHud && !self.has_gameplay_modal() && self.focused
    }

    /// Central ownership check for overlays that suspend world input.
    fn has_gameplay_modal(&self) -> bool {
        self.ui.screen != Screen::InGameHud
            || self.paused
            || self.shader_options_open
            || self.chat_bar.is_some()
            || self.container_screen.is_some()
            || self.creative_inventory.is_some()
            || self.book_editor.is_some()
            || self.credits_screen.is_some()
            || self.ui.sign_editor.is_some()
            || self.advancements_open
            || self.ui.claims_ui.is_some()
            || self.live_is_sleeping()
            || matches!(
                self.ui.screen,
                Screen::Settings(SettingsTab::Shaders)
                    | Screen::ModSyncConsent
                    | Screen::ModSyncPrompt
            )
    }

    fn clear_gameplay_controls(&mut self) {
        let stop_sneak = self.keys.down;
        let stop_sprint = self.keys.sprint;
        self.keys = Keys::default();
        self.left_mouse_held = false;
        self.right_mouse_held = false;
        if let Some(live) = &mut self.live {
            live.cancel_mining();
            live.release_item_use();
            if stop_sneak {
                live.player_command(1);
            }
            if stop_sprint {
                live.player_command(4);
            }
        }
    }

    /// Re-apply the grab decision after an overlay/screen transition.
    fn maybe_regrab(&mut self) {
        let grab = self.wants_cursor_grab();
        self.set_cursor_grab(grab);
    }

    /// True when key/mouse input belongs to the UI layer (pause menus and
    /// every non-HUD screen) rather than gameplay movement.
    fn ui_wants_input(&self) -> bool {
        self.paused || self.ui.screen != Screen::InGameHud
    }

    /// `true` while an editable text surface owns typing: the in-game
    /// chat bar, the server-driven sign editor, any focused UI text
    /// field, or a delegated sub-screen's search field. IME enablement
    /// and the clipboard chords key off this — during raw WASD gameplay
    /// it is `false`.
    fn text_input_active(&self) -> bool {
        self.chat_bar.is_some()
            || self.book_editor.is_some()
            || self
                .container_screen
                .as_ref()
                .is_some_and(|s| s.rename_focused)
            || self
                .creative_inventory
                .as_ref()
                .is_some_and(|state| state.tab == megu_ui::creative_screen::CreativeTab::Search)
            || self.ui.sign_editor.is_some()
            || self.ui.fps_field_active()
            || self.ui.join_field_active()
            || self.ui.world_name_field_active()
            || self.ui.world_seed_field_active()
            || self.ui.server_address_field_active()
            || (self.ui.screen == Screen::RecipeBrowser && self.ui.recipe_browser.search_focused())
            || (self.ui.screen == Screen::ModManager && self.ui.mod_manager.search_focused())
            || (self.ui.settings.qol.recipe_overlay_enabled
                && self.container_screen.is_some()
                && self.ui.recipe_browser.search_focused())
    }

    /// Forward device wheel motion to the scrollable menu screens owned by
    /// `UiState`. Winit's positive vertical delta moves content down (the
    /// scroll-up gesture), while `UiState::scroll_menu` positive values move
    /// the menu downward, so the sign flips at this host boundary. Line
    /// deltas use vanilla's 24px row pitch; pixel deltas keep their native
    /// resolution.
    fn scroll_menu_from_wheel(&mut self, delta: winit::event::MouseScrollDelta) -> bool {
        let menu_pixels = match delta {
            winit::event::MouseScrollDelta::LineDelta(_, y) => -y * 24.0,
            winit::event::MouseScrollDelta::PixelDelta(pos) => -pos.y as f32,
        };
        let scroll_px = menu_pixels.round() as i32;
        if scroll_px == 0
            || !matches!(
                self.ui.screen,
                Screen::Settings(_) | Screen::Singleplayer | Screen::Friends
            )
        {
            return false;
        }
        self.ui.scroll_menu(scroll_px);
        true
    }

    /// `Ctrl` (every platform) or `Cmd` (macOS) currently held — the
    /// copy/paste chord modifier.
    fn clipboard_modifier_held(&self) -> bool {
        self.modifiers.control_key() || (cfg!(target_os = "macos") && self.modifiers.super_key())
    }

    /// `true` when a `V`/`C` press is a clipboard chord
    /// (`Ctrl`/`Cmd`+key) with a live text surface to feed.
    fn clipboard_chord(&self, code: KeyCode, state: ElementState) -> bool {
        state == ElementState::Pressed
            && self.clipboard_modifier_held()
            && self.text_input_active()
            && matches!(code, KeyCode::KeyV | KeyCode::KeyC)
    }

    /// Push the current text-input state to the OS's IME: allowed while
    /// a text surface owns typing (chat bar, focused UI field, delegated
    /// search), forbidden during raw 3D gameplay so the compositor can
    /// never hijack WASD movement keys. Only transitions reach
    /// `set_ime_allowed`; while typing is live the candidate-window
    /// anchor (`set_ime_cursor_area`) tracks whichever surface owns the
    /// caret so Fcitx5/IBus popups land beside the input, not the
    /// crosshair. Losing the text surface clears the tracked
    /// composition state so a stale preedit can't corrupt the next
    /// commit.
    fn update_ime(&mut self) {
        let allowed = self.text_input_active();
        if allowed {
            let area = self.ime_cursor_area();
            if area != self.ime_state.ime_cursor_area {
                self.ime_state.set_ime_cursor_area(area);
                if let Some(active) = &self.active {
                    let (x, y, w, h) = area;
                    active
                        .window
                        .set_ime_cursor_area(PhysicalPosition::new(x, y), PhysicalSize::new(w, h));
                }
            }
        }
        if allowed == self.ime_allowed {
            return;
        }
        self.ime_allowed = allowed;
        if let Some(active) = &self.active {
            active.window.set_ime_allowed(allowed);
        }
        if !allowed {
            self.ime_state.on_disabled();
        }
    }

    /// Physical-pixel rect the IME candidate window should hug while a
    /// text surface owns typing: the chat input strip at the bottom-left
    /// (mirroring `hud_widgets::chat_chrome` — EDGE=4,
    /// ACTIONBAR_LIFT=64, CHAT_INPUT_H=10, width cap 280), the focused
    /// widget's hit region (server address, world name/seed, FPS and
    /// join-code fields), a mid-screen band for the sign editor, or a
    /// bottom strip fallback so the popup never lands on the center
    /// crosshair.
    fn ime_cursor_area(&self) -> (i32, i32, u32, u32) {
        let (w, h) = self
            .active
            .as_ref()
            .map(|active| (active.size.width as i32, active.size.height as i32))
            .unwrap_or((0, 0));
        if self.chat_bar.is_some() {
            let box_w = 280.min((w - 8).max(40)).max(1);
            return (4, (h - 76).max(0), box_w as u32, 10);
        }
        if self.ui.sign_editor.is_some() {
            return (w / 4, h / 2, (w / 2).max(1) as u32, 12);
        }
        if let Some(id) = self.ui.focused() {
            if let Some(region) = self.ui.hit_regions.iter().find(|r| r.id == id) {
                let rect = region.rect;
                return (rect.x, rect.y, rect.w.max(1), rect.h.max(8));
            }
        }
        (4, (h - 40).max(0), 280.min(w).max(1) as u32, 12)
    }

    /// Route committed text — an `Ime::Commit` string, a sanitized
    /// paste, or a test dispatch — into the active text surface and
    /// return the [`UiAction`]s the resulting `CharTyped` events
    /// produced. The chat bar consumes text directly (capped at
    /// vanilla's [`CHAT_FIELD_MAX_CHARS`]); UI fields go through
    /// `UiInputEvent::CharTyped` so each field's own filter and cap
    /// applies.
    fn committed_text_actions(&mut self, text: &str) -> Vec<UiAction> {
        // The sign-edit overlay outranks every other surface while open:
        // committed strings land on the active line verbatim (the model
        // strips control chars and caps the vanilla line length).
        if let Some(editor) = &mut self.ui.sign_editor {
            editor.insert_str(text);
            return Vec::new();
        }
        if let Some(bar) = &mut self.chat_bar {
            let room = CHAT_FIELD_MAX_CHARS.saturating_sub(bar.text().chars().count());
            let mut fed = false;
            for ch in text.chars().filter(|c| !c.is_control()).take(room) {
                bar.push_char(ch);
                fed = true;
            }
            if fed {
                self.refresh_chat_suggestions();
            }
            return Vec::new();
        }
        if let Some(book) = &mut self.book_editor {
            for ch in text.chars() {
                if matches!(ch, '\n' | '\r') {
                    book.newline();
                } else {
                    book.type_char(ch);
                }
            }
            return Vec::new();
        }
        if self
            .container_screen
            .as_ref()
            .is_some_and(|screen| screen.rename_focused)
        {
            let used = self
                .container_screen
                .as_ref()
                .map_or(0, |screen| screen.rename_text.chars().count());
            for ch in text
                .chars()
                .filter(|c| !c.is_control())
                .take(50usize.saturating_sub(used))
            {
                let action = self
                    .container_screen
                    .as_mut()
                    .and_then(|screen| screen.handle_rename_char(ch));
                if let Some(action) = action {
                    self.dispatch_workstation_action(action);
                }
            }
            return Vec::new();
        }
        if let Some(creative) = self
            .creative_inventory
            .as_mut()
            .filter(|state| state.tab == megu_ui::creative_screen::CreativeTab::Search)
        {
            let mut search = creative.search.clone();
            search.extend(text.chars().filter(|c| !c.is_control()));
            creative.set_search(search);
            return Vec::new();
        }
        // recipe overlay overlay search: the container keeps `Screen::InGameHud`
        // underneath, so `ui_wants_input` is false — the browser's
        // `CharTyped` arm owns the text surface instead.
        if self.ui.settings.qol.recipe_overlay_enabled
            && self.container_screen.is_some()
            && self.ui.recipe_browser.search_focused()
        {
            let (w, h) = self
                .active
                .as_ref()
                .map(|a| (a.size.width, a.size.height))
                .unwrap_or((0, 0));
            for ch in text.chars().filter(|c| !c.is_control()) {
                for action in self
                    .ui
                    .recipe_browser
                    .handle_input(UiInputEvent::CharTyped(ch), w, h)
                {
                    self.dispatch_recipe_browser_action(&action);
                }
            }
            return Vec::new();
        }
        if !self.ui_wants_input() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        for ch in text.chars().filter(|c| !c.is_control()) {
            actions.extend(self.ui.handle_input(UiInputEvent::CharTyped(ch)));
        }
        actions
    }

    /// The event-loop-free text feed tests use — mirrors how
    /// [`dispatch_input`](Self::dispatch_input) bypasses
    /// `ActiveEventLoop`. IME commits and pastes route identically.
    pub fn dispatch_committed_text(&mut self, text: &str) {
        for action in self.committed_text_actions(text) {
            self.dispatch_ui_action(&action);
        }
    }

    /// `Ctrl`/`Cmd`+`V`: pull the clipboard (system tool or the in-app
    /// fallback), sanitize it for a single-line field, and feed the
    /// result. Returns the actions the text produced.
    fn clipboard_paste_actions(&mut self) -> Vec<UiAction> {
        let Some(raw) = self.clipboard.get_text() else {
            return Vec::new();
        };
        let text = crate::clipboard::sanitize_paste_text(&raw, true);
        if text.is_empty() {
            return Vec::new();
        }
        self.committed_text_actions(&text)
    }

    /// `Ctrl`/`Cmd`+`C`: copy the active text surface's contents to the
    /// OS clipboard (or the in-app fallback) and report which store
    /// took them.
    fn clipboard_copy(&mut self) {
        let text = if let Some(editor) = &self.ui.sign_editor {
            editor.lines()[editor.active_line()].clone()
        } else if let Some(bar) = &self.chat_bar {
            bar.text().to_owned()
        } else {
            match self.active_field_text() {
                Some(text) => text.to_owned(),
                None => return,
            }
        };
        if text.is_empty() {
            return;
        }
        match self.clipboard.set_text(&text) {
            ClipboardOutcome::System => self.status_line("Copied to clipboard"),
            ClipboardOutcome::Fallback => self.status_line("Copied (in-app buffer)"),
        }
    }

    /// Contents of whichever single-line UI field owns typing.
    fn active_field_text(&self) -> Option<&str> {
        if self.ui.fps_field_active() {
            Some(self.ui.fps_field_text())
        } else if self.ui.join_field_active() {
            Some(&self.ui.join_code_text)
        } else if self.ui.world_name_field_active() {
            Some(&self.ui.singleplayer.new_name)
        } else if self.ui.world_seed_field_active() {
            Some(&self.ui.singleplayer.seed_text)
        } else if self.ui.server_address_field_active() {
            Some(&self.ui.multiplayer.address_text)
        } else {
            None
        }
    }

    /// `true` when `text` echoes the `Ime::Commit` delivered moments
    /// ago — some platforms report the same committed string again as
    /// `KeyEvent.text`, which would double-insert. The first matching
    /// echo is swallowed and cleared so only one suppression happens.
    fn swallow_ime_echo(&mut self, text: &str) -> bool {
        let echo = self
            .last_ime_commit
            .as_ref()
            .is_some_and(|(committed, when)| {
                committed == text && when.elapsed() < Duration::from_millis(50)
            });
        if echo {
            self.last_ime_commit = None;
        }
        echo
    }

    /// `WindowEvent::Ime` state update, factored so tests can drive it
    /// without an `ActiveEventLoop`: transitions `ime_state` on
    /// `Enabled`/`Preedit`/`Disabled`, and on `Commit` clears the preedit
    /// and returns the committed string for
    /// [`committed_text_actions`](Self::committed_text_actions) — the
    /// same route `Key::Character` text takes, so multi-byte Japanese
    /// output is never dropped. The commit is also latched for the
    /// `KeyEvent.text` echo swallow. `None` means nothing to insert.
    fn on_ime_event(&mut self, ime: Ime) -> Option<String> {
        match ime {
            Ime::Enabled => {
                self.ime_state.on_enabled();
                None
            }
            Ime::Disabled => {
                self.ime_state.on_disabled();
                None
            }
            Ime::Preedit(text, cursor) => {
                self.ime_state.on_preedit(text, cursor);
                None
            }
            Ime::Commit(text) => {
                let committed = self.ime_state.on_commit(text);
                if committed.is_empty() {
                    None
                } else {
                    self.last_ime_commit = Some((committed.clone(), Instant::now()));
                    Some(committed)
                }
            }
        }
    }

    /// `WindowEvent::DroppedFile` staging hook: record the path, run the
    /// universal payload classifier, and surface its verdict. Nothing
    /// installs from the verdict alone — installable kinds carry a
    /// mandatory bilingual consent prompt the UI must still show.
    fn record_dropped_file(&mut self, path: &std::path::Path) {
        let verdict = megu_ui::seamless_ux::DropPayloadClassifier::classify_path(path);
        let lang = self.ui.lang();
        self.last_dropped_file = Some(path.to_path_buf());
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        if let Some(prompt) = &verdict.prompt {
            self.status_line(format!(
                "{} {} {}{name}",
                prompt.title(lang),
                prompt.message(lang),
                tr(lang, "Import: ", "取り込み: "),
            ));
        } else {
            let reason = match &verdict.rejection {
                Some(r) => format!("{r:?}"),
                None => tr(lang, "unrecognized payload", "未対応のペイロード").to_owned(),
            };
            self.status_line(format!(
                "{}{name}: {reason}",
                tr(
                    lang,
                    "Dropped file rejected: ",
                    "ドロップされたファイルは拒否されました: "
                ),
            ));
        }
    }

    /// Report client status outside the server chat stream.
    fn status_line(&mut self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("[{}] {message}", crate::CLIENT_GPU_NAME);
        self.ui.title_status = Some(message.clone());
        self.debug_messages.push(message.clone());
        if self.debug_messages.len() > 100 {
            let excess = self.debug_messages.len() - 100;
            self.debug_messages.drain(..excess);
        }

    }

    /// Spawn a session against `host:port` and switch the UI into gameplay.
    fn connect_to(&mut self, host: String, port: u16) {
        let endpoint = format!("{host}:{port}");
        if self.last_server_address.as_deref() != Some(endpoint.as_str()) {
            self.session_paths.server_mods_dir = None;
        }
        self.last_server_address = Some(endpoint);
        let retained_chat = retained_chat_on_reconnect(
            self.ui.settings.qol.retain_chat_on_reconnect,
            &self.ui.hud.chat_lines,
        );
        let live = LiveSession::connect_with_paths_and_auth(
            host,
            port,
            self.config.username.clone(),
            self.config.mc_version.clone(),
            self.session_paths.clone(),
            self.config.access_token.clone(),
            self.config.uuid.clone(),
            self.ui.settings.video.view_distance as i8,
        );
        // Windowed path: the dedicated real-time `AudioMixThread` owns the
        // cpal stream; gameplay code enqueues `AudioCmd`s non-blocking.
        // (`attach_live` test fixtures intentionally keep the synchronous
        // `pump()` fallback.)
        // Windowed sessions hand their sink to the real-time mix thread
        // lazily in `render_frame` (first live frame): `AudioMixCore`
        // construction is heavy enough that running it on small test
        // threads overflows their stack, and headless/test sessions keep
        // the synchronous `pump()` path by design.
        self.live = Some(live);
        self.on_live_attached();
        self.ui.screen = Screen::InGameHud;
        self.ui.hud = megu_ui::state::HudState::default();
        if !retained_chat.is_empty() {
            self.ui.hud.chat_lines = retained_chat;
            if let Some(live) = &mut self.live {
                live.clear_chat_log();
                live.set_chat_history_limit(self.ui.settings.qol.chat_history_lines);
                for line in self.ui.hud.chat_lines.iter().cloned() {
                    live.push_chat_line(line);
                }
            }
        }
        self.paused = false;
        self.maybe_regrab();
    }

    fn start_mod_sync_download(&mut self) {
        if self.mod_sync_worker.is_some() {
            return;
        }
        if megu_ui::state::parse_server_address(&self.ui.mod_sync_prompt.server_address).is_none() {
            self.ui.mod_sync_prompt.progress_label = "Invalid server address".into();
            return;
        }
        let requirements = self
            .ui
            .mod_sync_prompt
            .rows
            .iter()
            .map(|row| crate::mod_sync_auto::ModRequirement {
                mod_id: row.mod_id.clone(),
                required_version: row.required_version.clone(),
                sha512: row.sha512.clone(),
                sha256: row.sha256.clone(),
                modrinth_slug: row.modrinth_slug.clone(),
            })
            .collect();
        let game_dir = self
            .session_paths
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let server_key = self.ui.mod_sync_prompt.server_address.clone();
        self.mod_sync_generation = self.mod_sync_generation.wrapping_add(1);
        self.mod_sync_worker = Some(crate::mod_sync_auto::spawn_modrinth_mod_sync(
            requirements,
            game_dir,
            server_key,
        ));
        self.ui.mod_sync_prompt.progress_label = "Resolving mods on Modrinth...".into();
        self.ui.mod_sync_prompt.progress_percent = None;
    }

    fn poll_mod_sync_worker(&mut self) {
        use crate::mod_sync_auto::ModSyncProgress as P;
        let Some(progress) = self
            .mod_sync_worker
            .as_ref()
            .map(|worker| worker.task.progress())
        else {
            return;
        };
        match progress {
            P::Resolving => {
                self.ui.mod_sync_prompt.progress_label = "Resolving mods on Modrinth...".into();
                self.ui.mod_sync_prompt.progress_percent = None;
            }
            P::Downloading {
                mod_id,
                file_index,
                total_files,
                downloaded_bytes,
                total_bytes,
            } => {
                self.ui.mod_sync_prompt.progress_label = format!(
                    "Downloading {mod_id} ({file_index}/{total_files}) - {downloaded_bytes} bytes"
                );
                self.ui.mod_sync_prompt.progress_percent = total_bytes
                    .filter(|size| *size > 0)
                    .map(|size| ((downloaded_bytes.saturating_mul(100) / size).min(100)) as u8);
            }
            P::Verifying { mod_id } => {
                self.ui.mod_sync_prompt.progress_label = format!("Verifying {mod_id}...");
                self.ui.mod_sync_prompt.progress_percent = None;
            }
            P::Failed(reason) => {
                self.mod_sync_worker = None;
                self.ui.mod_sync_prompt.progress_label = reason;
                self.ui.mod_sync_prompt.progress_percent = None;
            }
            P::Ready {
                mods_dir,
                downloaded_files,
            } => {
                self.mod_sync_worker = None;
                self.session_paths.server_mods_dir = Some(mods_dir);
                let endpoint =
                    megu_ui::state::parse_server_address(&self.ui.mod_sync_prompt.server_address);
                self.ui.mod_sync_prompt.progress_label =
                    format!("Verified {downloaded_files} mods; reconnecting...");
                if let Some((host, port)) = endpoint {
                    self.connect_to(host, port);
                } else {
                    self.ui.mod_sync_prompt.progress_label = "Invalid server address".into();
                }
            }
        }
    }

    fn save_instant_replay(&mut self) {
        let game_dir = self
            .session_paths
            .game_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));
        let Some(live) = self.live.as_ref() else {
            self.status_line("No live session is available to save a replay");
            return;
        };
        if live.replay_ring.frames().is_empty() {
            self.status_line("No replay packets have been captured in this session");
            return;
        }
        let replay_dir = game_dir.join("replays");
        let next = std::fs::read_dir(&replay_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_prefix("replay-"))
                    .and_then(|name| name.strip_suffix(".megu-replay"))
                    .and_then(|number| number.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let path = replay_dir.join(format!("replay-{next}.megu-replay"));
        match live.replay_ring.save_to_file(
            &path,
            self.config.mc_version.parse().unwrap_or(767),
            self.last_server_address.as_deref().unwrap_or("local world"),
        ) {
            Ok(summary) => self.status_line(format!(
                "Saved 60s replay ({} frames, {:.1} KB)",
                summary.frame_count,
                summary.payload_bytes as f64 / 1024.0
            )),
            Err(error) => self.status_line(format!("Replay save failed: {error}")),
        }
    }

    /// Launch the integrated server for world `world` (validated by
    /// `integrated::launch`, which also resolves the server binary and
    /// runtime assets) and return the running guard.
    fn launch_singleplayer(
        config: &GpuCliConfig,
        world: &str,
        settings: &ClientSettings,
        game_mode: Option<&str>,
        difficulty: Option<&str>,
        hardcore: bool,
    ) -> Result<integrated::SingleplayerServer, String> {
        let mut cli = megu_client::cli::CliConfig::default();
        cli.quick_play_singleplayer = Some(world.to_owned());
        cli.server_bin = config.server_bin.clone();
        cli.game_dir = config.game_dir.clone();
        cli.seed = config.seed;
        cli.game_mode = game_mode.map(str::to_owned);
        cli.difficulty = difficulty.map(str::to_owned);
        cli.hardcore = hardcore;
        cli.client_settings = settings.clone();
        // The child server reads `MEGU_DIRECTIONAL_SHADOW_LIGHT` at boot
        // ([server] `directional_shadow_light` in server.toml fills the
        // same var). `integrated::launch` never sets it, so the spawned
        // `megu` inherits this process env — the Settings → Video toggle
        // reaches local/hosted worlds with no config-file edit. The same
        // spawn path serves friend-relay hosting, so the policy applies
        // to every world this client hosts.
        std::env::set_var(
            "MEGU_DIRECTIONAL_SHADOW_LIGHT",
            directional_shadow_light_env_value(settings.directional_shadow_gameplay),
        );
        // Same spawn path for the opt-in unloaded-chunk catch-up lane:
        // `MEGU_LAZY_CATCHUP` reaches `server_config_boot` through the
        // inherited environment.
        std::env::set_var(
            "MEGU_LAZY_CATCHUP",
            lazy_catchup_env_value(settings.lazy_catchup_gameplay),
        );
        if integrated::has_complete_runtime(&cli) {
            return integrated::launch(&cli);
        }
        let session_paths = SessionPaths {
            game_dir: config.game_dir.clone(),
            assets_dir: config.assets_dir.clone(),
            asset_index: config.asset_index.clone(),
            version_id: config.version_id.clone(),
            resource_pack_dir: config.resource_pack_dir.clone(),
            server_mods_dir: None,
        };
        let vanilla_jar = crate::vanilla_bootstrap::discover_local_vanilla_jar(&session_paths);
        let game_dir = cli
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let blocks_json =
            crate::vanilla_bootstrap::discover_or_bootstrap_blocks_json(&game_dir).ok();
        integrated::launch_with_runtime_assets(&cli, vanilla_jar.as_deref(), blocks_json.as_deref())
    }

    /// Title Singleplayer: launch the integrated loopback world named by
    /// `--quickPlaySingleplayer` (or `New World`), then connect to it.
    fn start_singleplayer(&mut self) -> Result<(), String> {
        let world = self
            .config
            .quick_play_singleplayer
            .clone()
            .unwrap_or_else(|| "New World".to_owned());
        self.start_singleplayer_world(&world, None, None)
    }

    fn singleplayer_wire_options(
        options: WorldLaunchOptions,
    ) -> (&'static str, &'static str, bool) {
        let game_mode = match options.game_mode {
            megu_ui::state::SingleplayerGameMode::Survival => "survival",
            megu_ui::state::SingleplayerGameMode::Creative => "creative",
            megu_ui::state::SingleplayerGameMode::Hardcore => "survival",
        };
        let difficulty = match options.difficulty {
            megu_ui::state::SingleplayerDifficulty::Peaceful => "peaceful",
            megu_ui::state::SingleplayerDifficulty::Easy => "easy",
            megu_ui::state::SingleplayerDifficulty::Normal => "normal",
            megu_ui::state::SingleplayerDifficulty::Hard => "hard",
        };
        let hardcore = options.game_mode == megu_ui::state::SingleplayerGameMode::Hardcore;
        (game_mode, difficulty, hardcore)
    }

    /// Singleplayer world-select/"Create & Play": launch the integrated
    /// server for `world` (fail-closed `validate_world_name` inside
    /// `integrated::launch`), honoring the UI seed when given and
    /// falling back to `--seed` otherwise, then connect to it. New-world
    /// launches carry the Create World form's game-mode/difficulty payload;
    /// preexisting world-row launches keep the historical UI defaults.
    fn start_singleplayer_world(
        &mut self,
        world: &str,
        seed: Option<i64>,
        create_options: Option<WorldLaunchOptions>,
    ) -> Result<(), String> {
        let mut config = self.config.clone();
        config.seed = seed.or(config.seed);
        let options = create_options.unwrap_or_else(|| self.ui.singleplayer.launch_options());
        let (game_mode, difficulty, hardcore) = Self::singleplayer_wire_options(options);
        let server = Self::launch_singleplayer(
            &config,
            world,
            &self.persisted_settings,
            Some(game_mode),
            Some(difficulty),
            hardcore,
        )?;
        let addr = server.addr();
        self.singleplayer_server = Some(server);
        self.active_singleplayer_world = Some(world.to_owned());
        self.connect_to(addr.ip().to_string(), addr.port());
        self.maybe_start_friend_share();
        Ok(())
    }

    /// Refresh the Singleplayer screen's world list from
    /// `<gameDir>/saves/` (`.` when `--gameDir` is unset), sorted
    /// deterministically.
    fn refresh_singleplayer_worlds(&mut self) {
        self.ui.singleplayer.worlds = list_saved_worlds(self.config.game_dir.as_deref());
    }

    /// The Multiplayer screen's server-list store under `<gameDir>`
    /// (`.` when `--gameDir` is unset) — loaded fresh for every refresh
    /// and every mutation so `SaveServer`/`DeleteServer` always persist
    /// through the store's atomic commit instead of a stale snapshot.
    fn server_list_store(&self) -> Result<ServerListStore, String> {
        let dir = self
            .config
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        ServerListStore::load(dir)
    }

    /// Repopulate `ui.multiplayer.servers` from the store on screen open
    /// and after every mutation; store failures land in the screen's
    /// status banner rather than dropping silently.
    fn refresh_multiplayer_servers(&mut self) {
        let loaded = self.server_list_store();
        let multiplayer: &mut MultiplayerScreenState = &mut self.ui.multiplayer;
        match loaded {
            Ok(store) => {
                multiplayer.servers = store
                    .entries()
                    .iter()
                    .map(|entry| MultiplayerServerEntry {
                        name: entry.name.clone(),
                        address: entry.address.clone(),
                        port: entry.port,
                    })
                    .collect();
                if multiplayer
                    .selected
                    .is_some_and(|index| index >= multiplayer.servers.len())
                {
                    multiplayer.selected = None;
                }
            }
            Err(err) => {
                multiplayer.status = Some(err.clone());
                self.status_line(format!("Multiplayer: {err}"));
            }
        }
    }

    /// `UiAction::SaveServer`: append the direct-connect endpoint through
    /// the store (validation + atomic persistence inside), then refresh
    /// the visible rows so the UI mirrors exactly what was committed.
    fn save_server_entry(&mut self, name: &str, address: &str, port: u16) -> Result<(), String> {
        let mut store = self.server_list_store()?;
        store.add(ServerEntry {
            name: name.to_owned(),
            address: address.to_owned(),
            port,
        })?;
        self.refresh_multiplayer_servers();
        self.ui.multiplayer.status = Some(format!("Saved {name}"));
        Ok(())
    }

    /// `UiAction::DeleteServer`: remove row `index` through the store,
    /// then refresh; a stale row selection clears itself on rebuild.
    fn delete_server_entry(&mut self, index: usize) -> Result<(), String> {
        let mut store = self.server_list_store()?;
        let removed = store.remove(index)?;
        self.refresh_multiplayer_servers();
        self.ui.multiplayer.status = Some(format!("Deleted {}", removed.name));
        Ok(())
    }

    /// Surface a server-list/store failure on the Multiplayer screen's
    /// status banner (and the global status line for logs/chat).
    fn multiplayer_error(&mut self, err: &str) {
        self.ui.multiplayer.status = Some(err.to_owned());
        self.status_line(format!("Multiplayer: {err}"));
    }

    /// Title Multiplayer: connect to the configured server target.
    fn join_multiplayer(&mut self) -> Result<(), String> {
        let (host, port) = self.config.connect.clone().ok_or_else(|| {
            "no server configured — pass --connect host:port or \
             --quickPlayMultiplayer host:port"
                .to_owned()
        })?;
        self.connect_to(host, port);
        Ok(())
    }

    /// Friends "join with code": bind a [`FriendGuestBridge`] to the
    /// configured relay, then run the normal session over its loopback
    /// endpoint — exactly the headless client's relay path.
    fn join_with_code(&mut self, code_value: u64) -> Result<(), String> {
        let relay = self.friend_relay_addr()?;
        let display = invite_code_display(code_value);
        let code = megu_proto::InviteCode::parse(&display)
            .map_err(|e| format!("invite code {display}: {e}"))?;
        let bridge =
            FriendGuestBridge::start(&relay, &code, &self.config.username, FRIEND_TIMEOUT)?;
        let addr = bridge.local_addr();
        self.friend_bridge = Some(bridge);
        self.connect_to(addr.ip().to_string(), addr.port());
        Ok(())
    }

    /// Host side of "Play with Friends": register a live invite code on
    /// the relay while the integrated world is running.
    fn generate_invite(&mut self) -> Result<(), String> {
        if self.singleplayer_server.is_none() {
            return Err("friend invites need a hosted world — start singleplayer first".to_owned());
        }
        let relay = self
            .config
            .relay
            .clone()
            .ok_or_else(|| "friend invites need --relay host:port".to_owned())?;
        let protocol = self.config.mc_version.parse::<i32>().unwrap_or(767);
        let world = self
            .config
            .quick_play_singleplayer
            .clone()
            .unwrap_or_else(|| "New World".to_owned());
        let session = FriendHostSession::register(
            &relay,
            protocol,
            &world,
            8,
            host_secret_bytes(),
            FRIEND_TIMEOUT,
        )?;
        let code = session.invite_code();
        let value = invite_code_value(&code);
        self.friend_host = Some(session);
        self.ui.active_invite = Some(value);
        Ok(())
    }

    // ------------------------------------------------------------------
    // FRL3 friends: identity, allowlist store, host-share worker
    // ------------------------------------------------------------------

    /// `<gameDir>` or `.` — where `megu-identity.key`/`megu-friends.toml`
    /// (and every other launcher-scoped file) live.
    fn game_dir_path(&self) -> std::path::PathBuf {
        self.config
            .game_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// Load `megu-identity.key` + `megu-friends.toml` and mirror them
    /// into `ui.friends`. Runs at boot and every time the Friends or
    /// Multiplayer screens open so a hand-edited toml is picked up; a
    /// corrupt file is reported, never silently replaced.
    fn load_friends_state(&mut self) {
        let dir = self.game_dir_path();
        match load_or_create_identity(&dir) {
            Ok(identity) => self.frl_identity = Some(identity),
            Err(err) => self.status_line(format!("FRL3 identity: {err}")),
        }
        match FriendsStore::load(&dir) {
            Ok(mut store) => {
                if let Some(identity) = &self.frl_identity {
                    store.bind_identity(identity);
                }
                self.friends_store = Some(store);
            }
            Err(err) => self.status_line(format!("megu-friends.toml: {err}")),
        }
        self.refresh_friends_ui();
    }

    /// Mirror `friends_store` + the live share session into
    /// `ui.friends` after every mutation — the screen only ever shows
    /// what is persisted. Text fields the player is typing in are left
    /// alone (relay field check mirrors the address-field pattern).
    fn refresh_friends_ui(&mut self) {
        let ui_friends: &mut FriendsScreenState = &mut self.ui.friends;
        ui_friends.my_friend_id = self
            .frl_identity
            .as_ref()
            .map(Frl3Identity::friend_id)
            .unwrap_or_default();
        let mut rows = Vec::new();
        if let Some(store) = &self.friends_store {
            ui_friends.share_mode = match store.share_mode {
                StoreShareMode::Off => UiShareMode::Off,
                StoreShareMode::FriendsOnly => UiShareMode::FriendsOnly,
                StoreShareMode::SelectedFriends => UiShareMode::SelectedFriends,
            };
            if !ui_friends.relay_field_active() {
                ui_friends.relay_addr_text = store.relay_addr.clone().unwrap_or_default();
            }
            rows = store
                .friends
                .iter()
                .map(|f| FriendListEntry {
                    name: f.name.clone(),
                    friend_id: f.friend_id.clone(),
                    selected_for_share: f.selected_for_share,
                    connected: false,
                    sas_code: None,
                })
                .collect();
        }
        // Live guest streams mark the row of the friend the handshake
        // actually verified — the claim comes from `guest_friend_id`,
        // never the spoofable `guest_name`.
        if let Some(worker) = &self.friend_share {
            if let Ok(shared) = worker.shared.lock() {
                for (_, friend_id) in shared.guests.values() {
                    if let Some(friend_id) = friend_id {
                        if let Some(row) = rows.iter_mut().find(|r| &r.friend_id == friend_id) {
                            row.connected = true;
                        }
                    }
                }
            }
        }
        // The guest-side SAS lands on the joined host's row.
        if let Some((friend_id, sas)) = &self.friend_join_sas {
            if let Some(row) = rows.iter_mut().find(|r| &r.friend_id == friend_id) {
                row.sas_code = Some(*sas);
            }
        }
        ui_friends.friends = rows;
        if ui_friends
            .selected
            .is_some_and(|i| i >= ui_friends.friends.len())
        {
            ui_friends.selected = None;
        }
        ui_friends.scroll = ui_friends
            .scroll
            .min(ui_friends.friends.len().saturating_sub(1));
        ui_friends.sharing_active = self.friend_share.is_some();
    }

    /// The relay `host:port` joins and sharing bind to: an explicit
    /// `--relay` wins, the saved `megu-friends.toml` address is the
    /// fallback — so a saved relay never needs the CLI flag.
    fn friend_relay_addr(&self) -> Result<String, String> {
        if let Some(relay) = &self.config.relay {
            return Ok(relay.clone());
        }
        self.friends_store
            .as_ref()
            .and_then(|store| store.relay_addr.clone())
            .ok_or_else(|| "set a relay address on the Friends screen (or pass --relay)".to_owned())
    }

    /// The allowlist the share session enforces right now: the store's
    /// mode-driven set plus the live 128-bit guest-link grant (a
    /// token-bound entry admits that one guest key with that token).
    fn current_allowlist(&self) -> FriendAllowlist {
        let mut allowlist = self
            .friends_store
            .as_ref()
            .map(FriendsStore::active_allowlist)
            .unwrap_or_default();
        if let Some(grant) = &self.guest_link_grant {
            allowlist.add_with_token(grant.friend_pk, grant.token);
        }
        allowlist
    }

    /// Persist the store, then push the rebuilt allowlist into the live
    /// share worker — `update_allowlist` inside terminates the streams
    /// of friends that fell out of the set.
    fn commit_friends_store(&mut self) -> Result<(), String> {
        let Some(store) = &self.friends_store else {
            return Err("friends store not loaded".to_owned());
        };
        store
            .save(&self.game_dir_path())
            .map_err(|e| format!("megu-friends.toml save: {e}"))?;
        if let Some(worker) = &self.friend_share {
            let _ = worker
                .commands
                .send(FriendShareCommand::ApplyAllowlist(self.current_allowlist()));
        }
        Ok(())
    }

    /// `UiAction::AddFriend` — the store validates fail-closed (checksum,
    /// duplicate key, self-key); rejections land on the inline error
    /// line, never the void.
    fn add_friend(&mut self, name: &str, friend_id: &str) {
        let Some(store) = &mut self.friends_store else {
            self.ui.friends.add_error = Some("friends store not loaded".to_owned());
            return;
        };
        match store.add_friend(name, friend_id) {
            Err(err) => self.ui.friends.add_error = Some(err),
            Ok(entry) => {
                let fid = entry.friend_id.clone();
                match self.commit_friends_store() {
                    Ok(()) => {
                        self.ui.friends.add_error = None;
                        self.ui.friends.add_name_text.clear();
                        self.ui.friends.add_id_text.clear();
                        self.ui.friends.status = Some(format!("Added {fid}"));
                    }
                    Err(err) => self.ui.friends.add_error = Some(err),
                }
            }
        }
        self.refresh_friends_ui();
    }

    /// `UiAction::RemoveFriend` — instant revoke: persists the removal,
    /// then the worker's allowlist refresh terminates any live guest
    /// stream for that key within one `FRIEND_SHARE_TIMEOUT` poll.
    fn remove_friend(&mut self, friend_id: &str) {
        let Some(store) = &mut self.friends_store else {
            self.ui.friends.status = Some("friends store not loaded".to_owned());
            return;
        };
        if !store.remove_friend(friend_id) {
            self.ui.friends.status = Some("friend not in the allowlist".to_owned());
        } else {
            match self.commit_friends_store() {
                Ok(()) => {
                    self.ui.friends.status =
                        Some(format!("Removed {friend_id} — kicked from your world"))
                }
                Err(err) => self.ui.friends.status = Some(err),
            }
        }
        self.refresh_friends_ui();
    }

    /// `UiAction::SetFriendShareMode` — persist the mode, then start or
    /// stop the E2EE share session to match. `Off` revokes; an active
    /// mode registers when a world is hosted (or re-pushes the
    /// allowlist into a running session).
    fn set_friend_share_mode(&mut self, mode: UiShareMode) {
        let store_mode = match mode {
            UiShareMode::Off => StoreShareMode::Off,
            UiShareMode::FriendsOnly => StoreShareMode::FriendsOnly,
            UiShareMode::SelectedFriends => StoreShareMode::SelectedFriends,
        };
        if let Some(store) = &mut self.friends_store {
            store.share_mode = store_mode;
        }
        if let Err(err) = self.commit_friends_store() {
            self.ui.friends.status = Some(err);
        }
        match mode {
            UiShareMode::Off => self.stop_friend_share(),
            _ => {
                if self.friend_share.is_none() && self.singleplayer_server.is_some() {
                    match self.start_friend_share() {
                        Ok(()) => {
                            self.ui.friends.status = Some("Sharing live".to_owned());
                        }
                        Err(err) => {
                            self.ui.friends.status = Some(format!("Sharing: {err}"));
                        }
                    }
                } else if self.friend_share.is_none() {
                    self.ui.friends.status =
                        Some("Sharing armed — starts when you host a world".to_owned());
                }
            }
        }
        self.refresh_friends_ui();
    }

    /// `UiAction::SetFriendSelected` — persist the row's
    /// `selected_for_share` flag and refresh the live allowlist.
    fn set_friend_selected(&mut self, friend_id: &str, selected: bool) {
        let Some(store) = &mut self.friends_store else {
            return;
        };
        if let Some(entry) = store.friends.iter_mut().find(|f| f.friend_id == friend_id) {
            entry.selected_for_share = selected;
            if let Err(err) = self.commit_friends_store() {
                self.ui.friends.status = Some(err);
            }
        }
        self.refresh_friends_ui();
    }

    /// `UiAction::SaveRelayAddr` — persist the typed `host:port`; `""`
    /// clears. Joins and sharing read it back through
    /// [`Self::friend_relay_addr`].
    fn save_relay_addr(&mut self, addr: &str) {
        let Some(store) = &mut self.friends_store else {
            self.ui.friends.status = Some("friends store not loaded".to_owned());
            return;
        };
        store.relay_addr = if addr.is_empty() {
            None
        } else {
            Some(addr.to_owned())
        };
        match self.commit_friends_store() {
            Ok(()) => {
                self.ui.friends.status = Some(if addr.is_empty() {
                    "Relay address cleared".to_owned()
                } else {
                    format!("Relay saved: {addr}")
                });
            }
            Err(err) => self.ui.friends.status = Some(err),
        }
        self.refresh_friends_ui();
    }

    /// Register the E2EE host-share session on the relay and spawn the
    /// pump thread. No-op when sharing already runs.
    fn start_friend_share(&mut self) -> Result<(), String> {
        if self.friend_share.is_some() {
            return Ok(());
        }
        let server = self
            .singleplayer_server
            .as_ref()
            .ok_or_else(|| "start a world first (Singleplayer)".to_owned())?;
        let identity = self
            .frl_identity
            .clone()
            .ok_or_else(|| "no FRL3 identity (megu-identity.key)".to_owned())?;
        // One-click publish goes through `friend_session`'s consent plan:
        // the player's Friends-screen toggle is the explicit
        // `PublishToFriends` consent, and the plan orders every transport
        // it allows. Port mapping is not requested, so the plan is
        // `[ProbeStunAndPunch, OpenRelayTunnel]`; the relay tunnel is the
        // implemented step today and the plan is validated before it.
        let policy = megu_proto::friend_session::FriendPublishPolicy::default();
        let plan = megu_proto::friend_session::plan_publish(
            &policy,
            megu_proto::friend_session::PublishConsent::PublishToFriends {
                allow_port_mapping: false,
            },
        )
        .map_err(|e| format!("publish plan: {e}"))?;
        if !plan.iter().any(|step| {
            matches!(
                step,
                megu_proto::friend_session::PublishStep::OpenRelayTunnel
            )
        }) {
            return Err("publish plan has no reachable transport".to_owned());
        }
        let relay = self.friend_relay_addr()?;
        let protocol = self.config.mc_version.parse::<i32>().unwrap_or(767);
        let world = self
            .config
            .quick_play_singleplayer
            .clone()
            .unwrap_or_else(|| "New World".to_owned());
        let host = FriendHostSession::register_e2ee_friends(
            &relay,
            protocol,
            &world,
            8,
            identity,
            self.current_allowlist(),
            FRIEND_SHARE_TIMEOUT,
        )?;
        let invite_code = host.invite_code();
        let shared = Arc::new(Mutex::new(FriendShareShared {
            invite_code: Some(invite_code),
            ..FriendShareShared::default()
        }));
        let (commands, inbox) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let world_addr = server.addr();
        let thread = std::thread::Builder::new()
            .name("friend-share".to_owned())
            .spawn(move || run_friend_share_worker(host, world_addr, worker_shared, inbox))
            .map_err(|e| format!("friend share worker: {e}"))?;
        self.friend_share = Some(FriendShareWorker {
            commands,
            shared,
            thread: Some(thread),
        });
        self.status_line(format!("Friend sharing live: {invite_code}"));
        Ok(())
    }

    /// Revoke the share session and join the pump thread (bounded by
    /// `FRIEND_SHARE_TIMEOUT` per pending relay read).
    fn stop_friend_share(&mut self) {
        if let Some(mut worker) = self.friend_share.take() {
            let _ = worker.commands.send(FriendShareCommand::Stop);
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
        self.guest_link_grant = None;
        self.ui.friends.guest_link = None;
        self.ui.friends.sharing_active = false;
    }

    /// Start sharing when the persisted mode is armed and a world is
    /// hosted — called after a singleplayer world attaches so the
    /// toggle survives restarts like vanilla's "Open to LAN".
    fn maybe_start_friend_share(&mut self) {
        let armed = self
            .friends_store
            .as_ref()
            .is_some_and(|store| store.share_mode != StoreShareMode::Off);
        if armed && self.friend_share.is_none() && self.singleplayer_server.is_some() {
            if let Err(err) = self.start_friend_share() {
                self.status_line(format!("Friend sharing: {err}"));
            }
            self.refresh_friends_ui();
        }
    }

    /// Reap a worker that exited on its own (relay drop, fatal link
    /// error) and refresh the guest-connected row flags — polled from
    /// the event loop next to the other background workers.
    fn poll_friend_share(&mut self) {
        let dead = self.friend_share.as_ref().is_some_and(|worker| {
            worker
                .shared
                .lock()
                .map(|shared| shared.stopped)
                .unwrap_or(false)
        });
        if dead {
            let mut last_error = None;
            if let Some(mut worker) = self.friend_share.take() {
                if let Ok(shared) = worker.shared.lock() {
                    last_error = shared.last_error.clone();
                }
                if let Some(thread) = worker.thread.take() {
                    let _ = thread.join();
                }
            }
            self.guest_link_grant = None;
            self.ui.friends.guest_link = None;
            self.ui.friends.sharing_active = false;
            match last_error {
                Some(err) => self.status_line(format!("Friend sharing stopped: {err}")),
                None => self.status_line("Friend sharing stopped"),
            }
            self.refresh_friends_ui();
        } else if self.friend_share.is_some() && self.ui.screen == Screen::Friends {
            self.refresh_friends_ui();
        }
    }

    /// `UiAction::GenerateGuestLink` — mint a 128-bit token bound to the
    /// selected friend's key, push it into the live allowlist, and
    /// compose the shareable `megu-friend:` link (host id + invite code
    /// + token).
    fn generate_guest_link(&mut self, friend_id: &str) {
        match self.build_guest_link(friend_id) {
            Ok(link) => {
                self.ui.friends.guest_link = Some(link);
                self.ui.friends.status = Some("Guest link ready".to_owned());
            }
            Err(err) => self.ui.friends.status = Some(format!("Guest link: {err}")),
        }
        self.refresh_friends_ui();
    }

    fn build_guest_link(&mut self, friend_id: &str) -> Result<String, String> {
        let friend_pk = parse_friend_id(friend_id).map_err(|e| format!("friend id: {e}"))?;
        let token = Frl3GuestToken::generate();
        self.guest_link_grant = Some(GuestLinkGrant { friend_pk, token });
        let worker = self
            .friend_share
            .as_ref()
            .ok_or_else(|| "turn Sharing on first".to_owned())?;
        worker
            .commands
            .send(FriendShareCommand::ApplyAllowlist(self.current_allowlist()))
            .map_err(|_| "share worker is gone".to_owned())?;
        let invite_code = worker
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.invite_code)
            .ok_or_else(|| "no invite code yet".to_owned())?;
        let host_id = self
            .frl_identity
            .as_ref()
            .map(FriendsStore::my_friend_id)
            .ok_or_else(|| "no FRL3 identity".to_owned())?;
        Ok(format!(
            "megu-friend:{host_id}:{invite_code}:{}",
            guest_link_b32_encode(&token.as_bytes())
        ))
    }

    /// `UiAction::JoinFriendWorld` — 1-click E2EE join of a friend-hosted
    /// world row: the row's pinned `megu-id:` becomes the expected host
    /// key, its invite code + relay address come from the presence feed.
    fn join_friend_world(&mut self, index: usize) -> Result<(), String> {
        let entry = self
            .ui
            .multiplayer
            .friend_worlds
            .get(index)
            .cloned()
            .ok_or_else(|| "friend world no longer listed".to_owned())?;
        let host_pk = parse_friend_id(&entry.friend_id)
            .map_err(|e| format!("friend id '{}': {e}", entry.friend_id))?;
        let code = megu_proto::InviteCode::parse(&entry.invite_code)
            .map_err(|e| format!("invite code {}: {e}", entry.invite_code))?;
        let relay = if entry.relay_addr.is_empty() {
            self.friend_relay_addr()?
        } else {
            entry.relay_addr.clone()
        };
        let identity = self
            .frl_identity
            .clone()
            .ok_or_else(|| "no FRL3 identity — open the Friends screen once".to_owned())?;
        let bridge = FriendGuestBridge::start_e2ee_friend(
            &relay,
            &code,
            &self.config.username,
            &identity,
            &host_pk,
            FRIEND_TIMEOUT,
        )?;
        if let Some(sas) = bridge.sas_code() {
            self.friend_join_sas = Some((entry.friend_id.clone(), sas));
            self.status_line(format!(
                "Friend join SAS {sas:06} — compare it with {}",
                entry.friend_name
            ));
        }
        let addr = bridge.local_addr();
        self.friend_bridge = Some(bridge);
        self.connect_to(addr.ip().to_string(), addr.port());
        Ok(())
    }

    /// `UiAction::JoinGuestLink` — parse the pasted `megu-friend:` link
    /// (host id + invite code + 128-bit token) and open the E2EE join
    /// with `start_e2ee_friend_with_token`.
    fn join_guest_link(&mut self, link: &str) -> Result<(), String> {
        let (host_pk, host_id, code, token) = parse_guest_link(link)?;
        let relay = self.friend_relay_addr()?;
        let identity = self
            .frl_identity
            .clone()
            .ok_or_else(|| "no FRL3 identity — open the Friends screen once".to_owned())?;
        let bridge = FriendGuestBridge::start_e2ee_friend_with_token(
            &relay,
            &code,
            &self.config.username,
            &identity,
            &host_pk,
            token,
            FRIEND_TIMEOUT,
        )?;
        if let Some(sas) = bridge.sas_code() {
            self.friend_join_sas = Some((host_id, sas));
            self.status_line(format!(
                "Friend join SAS {sas:06} — compare it with your friend"
            ));
        }
        let addr = bridge.local_addr();
        self.friend_bridge = Some(bridge);
        self.connect_to(addr.ip().to_string(), addr.port());
        Ok(())
    }

    /// `E` opens the window-0 player inventory: a 46-slot
    /// [`ContainerScreenState`] fed by `live.player_inventory`, the mirror
    /// the server's window-0 `container_set_content`/`set_slot` packets
    /// keep current even with no `open_screen`.
    fn open_own_inventory(&mut self) {
        let title = self.localized_menu_title(u32::MAX, "Inventory");
        let runtime_assets = self.vanilla_runtime_assets.as_ref();
        let mut screen = ContainerScreenState::new(&PLAYER_INVENTORY_SCHEMA, 0, 0, title);
        if let Some(live) = &self.live {
            screen.state_id = live.player_inventory_state_id;
            for (index, stack) in &live.player_inventory {
                if let Ok(index) = usize::try_from(*index) {
                    screen.set_slot(
                        index,
                        Self::localized_item_stack(runtime_assets, stack.item_id, stack.count),
                    );
                }
            }
            screen.set_cursor(live.player_carried.as_ref().map(|carried| {
                Self::localized_item_stack(runtime_assets, carried.item_id, carried.count)
            }));
        }
        self.container_screen = Some(screen);
        self.own_inventory_open = true;
        self.set_cursor_grab(false);
    }

    fn localized_item_stack(
        assets: Option<&crate::vanilla_assets_bridge::VanillaRuntimeAssets>,
        item_id: i32,
        count: i32,
    ) -> UiItemStack {
        let numeric_id = item_id.max(0) as u32;
        let namespaced_id = crate::recipe_ingest::item_name_for_id(numeric_id);
        let display_name = assets
            .map(|assets| assets.i18n.language.item_display_name(&namespaced_id))
            .unwrap_or_else(|| namespaced_id.clone());
        UiItemStack::new(numeric_id, count.clamp(0, 255) as u8)
            .with_namespaced_id(namespaced_id)
            .with_name(display_name)
    }

    fn localized_menu_title(&self, menu_id: u32, fallback: &str) -> String {
        let key = match menu_id {
            0..=5 => "container.chest",
            6 => "container.dispenser",
            7 => "container.crafter",
            8 => "container.repair",
            9 => "container.beacon",
            10 => "container.blast_furnace",
            11 => "container.brewing",
            12 => "container.crafting",
            13 => "container.enchant",
            14 => "container.furnace",
            15 => "container.grindstone",
            16 => "container.hopper",
            17 => "container.lectern",
            18 => "container.loom",
            19 => "container.villager",
            20 => "container.shulkerBox",
            21 => "container.upgrade",
            22 => "container.smoker",
            23 => "container.cartography_table",
            24 => "container.stonecutter",
            u32::MAX => "container.inventory",
            _ => return fallback.to_owned(),
        };
        self.vanilla_runtime_assets
            .as_ref()
            .and_then(|assets| assets.i18n.language.translate(key))
            .map(str::to_owned)
            .unwrap_or_else(|| fallback.to_owned())
    }

    /// Localized container title with Vanilla key lookup and a stable
    /// English/schema fallback when no language pack was found.
    pub fn translated_container_title(&self, menu_id: u32) -> String {
        self.localized_menu_title(menu_id, "Container")
    }

    fn open_creative_inventory(&mut self) {
        let Some(live) = self.live.as_ref().filter(|live| live.game_mode == 1) else {
            return;
        };
        let mut state = megu_ui::creative_screen::CreativeInventoryState::default();
        let items = vanilla_creative_catalog();
        for (slot, stack) in &live.player_inventory {
            if stack.count <= 0 {
                continue;
            }
            if (36..45).contains(slot) {
                state.hotbar[(slot - 36) as usize] = Some(megu_ui::creative_screen::CreativeItem {
                    item_id: stack.item_id,
                    name: crate::recipe_ingest::item_name_for_id(stack.item_id.max(0) as u32),
                    count: stack.count,
                });
            }
        }
        state.set_items(items);
        self.creative_inventory = Some(state);
        self.own_inventory_open = false;
        self.container_screen = None;
        self.clear_gameplay_controls();
        self.set_cursor_grab(false);
    }

    pub fn dispatch_creative_action(
        &mut self,
        action: megu_ui::creative_screen::CreativeInventoryAction,
    ) {
        match action {
            megu_ui::creative_screen::CreativeInventoryAction::SetCreativeSlot {
                slot,
                item_id,
                count,
            } => {
                if slot == -1 {
                    if let Some(state) = &self.creative_inventory {
                        self.creative_carried = state
                            .filtered_items()
                            .find(|item| item.item_id == item_id)
                            .cloned();
                    }
                    return;
                }
                if let Some(live) = &mut self.live {
                    live.send_creative_mode_slot(slot, item_id, count);
                    live.play_ui_click();
                }
            }
            megu_ui::creative_screen::CreativeInventoryAction::ClearSlot { slot } => {
                if slot == -1 {
                    self.creative_carried = None;
                    return;
                }
                if let Some(live) = &mut self.live {
                    live.send_creative_mode_slot(slot, 0, 0);
                    live.play_ui_click();
                }
            }
            megu_ui::creative_screen::CreativeInventoryAction::Close => {
                self.creative_inventory = None;
                self.creative_carried = None;
                self.maybe_regrab();
            }
        }
    }

    fn creative_mouse_click(&mut self, x: i32, y: i32) -> bool {
        let (w, h) = self
            .active
            .as_ref()
            .map_or((0, 0), |a| (a.size.width, a.size.height));
        let (px, py) = ((w as i32 - 194) / 2, (h as i32 - 166) / 2);
        use megu_ui::creative_screen::{CreativeClick, CreativeTab};
        let target = if (py + 6..py + 21).contains(&y) && (px + 8..px + 188).contains(&x) {
            let tab = ((x - (px + 8)) / 18).clamp(0, 9) as usize;
            Some(CreativeClick::Tab(CreativeTab::ALL[tab]))
        } else if (py + 24..py + 119).contains(&y) && (px + 8..px + 179).contains(&x) {
            let col = ((x - (px + 8)) / 19).clamp(0, 8) as usize;
            let row = ((y - (py + 24)) / 19).clamp(0, 4) as usize;
            Some(CreativeClick::Item(row * 9 + col))
        } else if (py + 132..py + 150).contains(&y) && (px + 8..px + 179).contains(&x) {
            Some(CreativeClick::Hotbar(
                ((x - (px + 8)) / 19).clamp(0, 8) as u8
            ))
        } else if (py + 132..py + 150).contains(&y) && (px + 179..px + 192).contains(&x) {
            Some(CreativeClick::Trash)
        } else {
            None
        };
        let Some(state) = self.creative_inventory.as_mut() else {
            return false;
        };
        if let Some(target) = target {
            if let CreativeClick::Hotbar(index) = target {
                if let Some(item) = self.creative_carried.take() {
                    let slot = 36 + i16::from(index);
                    state.hotbar[index as usize] = Some(item.clone());
                    self.dispatch_creative_action(
                        megu_ui::creative_screen::CreativeInventoryAction::SetCreativeSlot {
                            slot,
                            item_id: item.item_id,
                            count: item.count,
                        },
                    );
                    return true;
                }
            }
            if matches!(target, CreativeClick::Trash) && self.creative_carried.is_some() {
                self.creative_carried = None;
                return true;
            }
            if let Some(action) = state.click(target) {
                self.dispatch_creative_action(action);
            } else if let Some(live) = &mut self.live {
                live.play_ui_click();
            }
        }
        true
    }

    #[doc(hidden)]
    pub fn book_editor_state_for_test(
        &self,
    ) -> Option<&megu_ui::gameplay_screens::BookEditorState> {
        self.book_editor.as_ref()
    }

    /// Open an editor from a server open-book event adapter. The editor
    /// remains owned by the window and saves through the live session.
    pub fn open_book_editor(&mut self, slot: i16, pages: Vec<String>) {
        let mut editor = megu_ui::gameplay_screens::BookEditorState::new(slot);
        if !pages.is_empty() {
            editor.pages = pages;
        }
        self.book_editor = Some(editor);
        self.clear_gameplay_controls();
        self.set_cursor_grab(false);
    }

    /// Drain server `open_book` instructions decoded by the live session and
    /// route them into the matching book screen. Hand 0 resolves through the
    /// selected hotbar stack and hand 1 through window-0 slot 45 (offhand).
    pub fn sync_open_book_events(&mut self) -> bool {
        let hands = self
            .live
            .as_mut()
            .map(LiveSession::drain_open_book_hands)
            .unwrap_or_default();
        let mut opened = false;
        for hand in hands {
            opened |= self.open_book_for_hand(hand);
        }
        opened
    }

    fn open_book_for_hand(&mut self, hand: u8) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        let slot = match hand {
            0 => 36 + live.hud.selected_slot.min(8) as i32,
            1 => 45,
            _ => return false,
        };
        let Some(stack) = live.player_inventory.get(&slot) else {
            return false;
        };
        let table = protocol_767_table();
        let item_name = table.registry_name("minecraft:item", stack.item_id.max(0) as u32);
        let component = |name: &str| decode_stack_component(stack, name);
        match item_name {
            Some("minecraft:written_book") => {
                let Some(megu_proto::item_components::VerifiedComponent::WrittenBookContent(book)) =
                    component("minecraft:written_book_content")
                else {
                    return false;
                };
                let pages = book
                    .pages
                    .iter()
                    .map(|page| {
                        WireReader::new(&page.text)
                            .read_nbt()
                            .map(|nbt| nbt.extract_text())
                            .unwrap_or_default()
                    })
                    .collect();
                self.open_written_book_viewer(slot as i16, pages, book.title, book.author);
                true
            }
            Some("minecraft:writable_book") => {
                let pages = component("minecraft:writable_book_content")
                    .and_then(|component| match component {
                        megu_proto::item_components::VerifiedComponent::WritableBookContent(
                            pages,
                        ) => Some(pages.into_iter().map(|page| page.text).collect()),
                        _ => None,
                    })
                    .unwrap_or_default();
                self.open_book_editor(slot as i16, pages);
                true
            }
            _ => false,
        }
    }

    /// Open a signed book in the read-only page viewer.
    pub fn open_written_book_viewer(
        &mut self,
        slot: i16,
        pages: Vec<String>,
        title: impl Into<String>,
        author: impl Into<String>,
    ) {
        self.book_editor = Some(megu_ui::gameplay_screens::BookEditorState::new_written(
            slot, pages, title, author,
        ));
        self.clear_gameplay_controls();
        self.set_cursor_grab(false);
    }

    /// Open the local writable-book editor for the selected hotbar stack.
    /// Component decoding is bounded by the protocol's verified component
    /// decoder; no network or disk work occurs on the window thread.
    pub fn open_held_writable_book(&mut self) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        let slot = 36 + live.hud.selected_slot.min(8) as i32;
        let Some(stack) = live.player_inventory.get(&slot) else {
            return false;
        };
        let table = protocol_767_table();
        if table.registry_name("minecraft:item", stack.item_id.max(0) as u32)
            != Some("minecraft:writable_book")
        {
            return false;
        }
        let pages = decode_stack_component(stack, "minecraft:writable_book_content")
            .and_then(|component| match component {
                megu_proto::item_components::VerifiedComponent::WritableBookContent(pages) => {
                    Some(pages.into_iter().map(|page| page.text).collect())
                }
                _ => None,
            })
            .unwrap_or_default();
        self.open_book_editor(slot as i16, pages);
        true
    }

    pub fn dispatch_book_action(&mut self, action: megu_ui::gameplay_screens::BookEditorAction) {
        match action {
            megu_ui::gameplay_screens::BookEditorAction::Save { slot, pages, title } => {
                let Some(live) = &mut self.live else {
                    self.status_line("Book save failed: not connected; draft retained".to_owned());
                    return;
                };
                if !matches!(live.status, SessionStatus::Playing) {
                    self.status_line(
                        "Book save failed: session is not playing; draft retained".to_owned(),
                    );
                    return;
                }
                // Inventory menu slots 36..=44 map to Protocol 767's
                // hotbar slot indices 0..=8 in edit_book.
                let Some(hotbar_slot) = book_hotbar_slot(slot) else {
                    self.status_line(
                        "Book save failed: invalid hotbar slot; draft retained".to_owned(),
                    );
                    return;
                };
                let signed = title.is_some();
                if live.send_edit_book(hotbar_slot, pages, title) {
                    self.book_editor = None;
                    self.status_line(if signed {
                        "Book signed and saved".to_owned()
                    } else {
                        "Book saved".to_owned()
                    });
                    self.maybe_regrab();
                } else {
                    self.status_line(
                        "Book save failed: network queue unavailable; draft retained".to_owned(),
                    );
                }
            }
            megu_ui::gameplay_screens::BookEditorAction::Close => {
                self.book_editor = None;
                self.maybe_regrab();
            }
        }
    }

    /// Close the window-0 inventory: `E` or `Escape`, with the vanilla
    /// `container_close(0)` notification when a session is live.
    fn close_own_inventory(&mut self) {
        self.own_inventory_open = false;
        self.container_screen = None;
        if let Some(live) = &mut self.live {
            live.container_close(0);
        }
        self.maybe_regrab();
    }

    /// Close whatever container surface is up: the own inventory, or the
    /// server-driven screen (sending its `container_close`).
    fn close_container_surface(&mut self) -> bool {
        if self.own_inventory_open {
            self.close_own_inventory();
            return true;
        }
        if let Some(screen) = self.container_screen.take() {
            if let Some(live) = &mut self.live {
                live.container_close(screen.window_id);
            }
            self.maybe_regrab();
            return true;
        }
        false
    }

    /// `L` toggles the advancements overlay; vanilla announces the screen
    /// through `seen_advancements` on open (`Some(tab)`) and close (`None`).
    fn toggle_advancements(&mut self) {
        if self.advancements_open {
            self.advancements_open = false;
            if let Some(live) = &mut self.live {
                live.seen_advancements(None);
            }
            self.maybe_regrab();
        } else {
            self.advancements_open = true;
            let tab = self.live.as_ref().and_then(|live| {
                live.advancements_tab
                    .clone()
                    .or_else(|| live.advancements.keys().next().cloned())
            });
            if let Some(live) = &mut self.live {
                live.seen_advancements(tab.as_deref());
            }
            self.set_cursor_grab(false);
        }
    }

    /// The `L` overlay: a parchment-dark panel listing the mirrored
    /// advancement tree with progress, drawn through the normal UI draw
    /// list so it shares HUD compositing.
    fn render_advancements_overlay(
        live: Option<&LiveSession>,
        w: i32,
        h: i32,
        out: &mut megu_ui::draw::UiDrawList,
    ) {
        let panel_w = w.saturating_sub(80).clamp(200, 520);
        let rows = live.map_or(0, |live| live.advancements.len()) as i32;
        let panel_h = (60 + rows * 14).min(h.saturating_sub(80)).max(80);
        let x0 = ((w - panel_w) / 2).max(8);
        let y0 = ((h - panel_h) / 2).max(8);
        let rect = UiRect::new(x0, y0, panel_w as u32, panel_h as u32);
        out.fill_rect(rect, UiColor::rgba(12, 12, 14, 222));
        out.border_rect(rect, UiColor::rgba(84, 84, 96, 255), 2);
        out.text(
            x0 + 12,
            y0 + 10,
            1,
            UiColor::rgba(255, 213, 79, 255),
            "Advancements",
        );
        let Some(live) = live else {
            out.text_str(
                x0 + 12,
                y0 + 30,
                1,
                UiColor::rgba(170, 170, 178, 255),
                "no live session",
            );
            return;
        };
        if live.advancements.is_empty() {
            out.text_str(
                x0 + 12,
                y0 + 30,
                1,
                UiColor::rgba(170, 170, 178, 255),
                "no advancements synced yet",
            );
            return;
        }
        let mut y = y0 + 28;
        let max_y = y0 + panel_h - 12;
        for (key, advancement) in &live.advancements {
            if y >= max_y {
                out.text_str(x0 + 12, y, 1, UiColor::rgba(170, 170, 178, 255), "...");
                break;
            }
            let progress = live.advancement_progress.get(key);
            let done = progress.map_or(0, |p| p.done);
            let total = advancement.criteria_total.max(1);
            let complete = done >= total;
            let color = if complete {
                UiColor::rgba(85, 255, 85, 255)
            } else {
                UiColor::rgba(220, 220, 226, 255)
            };
            let title = if advancement.title.is_empty() {
                key.as_str()
            } else {
                advancement.title.as_str()
            };
            out.text(x0 + 12, y, 1, color, format!("{title}  {done}/{total}"));
            y += 14;
        }
    }

    fn init_gpu(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let local_jar = self.active_title_jar();
        if let Some(jar) = &local_jar {
            self.load_title_branding(jar);
        }
        self.ensure_vanilla_official_extras();
        let size = PhysicalSize::new(
            self.config.width.unwrap_or(1280).max(1),
            self.config.height.unwrap_or(720).max(1),
        );
        let mut attrs = WindowAttributes::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size(size)
            .with_resizable(true);
        // Wayland `app_id` and X11 `WM_CLASS` share one
        // `platform_specific.name` slot in winit 0.30, so the detected
        // compositor picks the `(general, instance)` pair the active
        // backend consumes — `app_id` for `.desktop`/`StartupWMClass`
        // matching on Wayland, `WM_CLASS(STRING)` on X11.
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let compositor = self.tiling_grab_policy.compositor();
            let (general, instance) = crate::linux_desktop::window_name_hints(compositor);
            attrs = if compositor.is_wayland() {
                WindowAttributesExtWayland::with_name(attrs, general, instance)
            } else {
                WindowAttributesExtX11::with_name(attrs, general, instance)
            };
        }
        if self.config.fullscreen {
            attrs = attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| format!("create_window: {e}"))?,
        );
        // Instance bound to this event loop's display, then adapter + device,
        // then a surface the selected adapter can present to.
        let ctx = request_gpu_device(
            self.render_config.gpu_backend,
            Some(event_loop.owned_display_handle()),
            None,
        )
        .map_err(|e| e.to_string())?;
        let surface = ctx
            .instance
            .create_surface(window.clone())
            .map_err(|e| format!("create_surface: {e}"))?;
        let caps = surface.get_capabilities(&ctx.adapter);
        if caps.formats.is_empty() {
            return Err(format!(
                "adapter {:?} cannot present to this surface",
                ctx.report.backend
            ));
        }
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.inner_size();
        let mut active = ActiveWindow {
            window,
            surface,
            renderer: { GpuRenderer::new(ctx, surface_format) },
            surface_format,
            size,
            surface_copy_src_supported: caps.usages.contains(wgpu::TextureUsages::COPY_SRC),
            title_branding_frame: None,
            debug_overlay: FrameDebugOverlay::BlockOutline(None),
        };
        if let Some(icon) = self
            .vanilla_official_extras
            .as_ref()
            .and_then(build_winit_window_icon)
        {
            active.window.set_window_icon(Some(icon));
        }
        let config = surface_config_for(
            self.render_config.vsync,
            self.render_config.effective_fps_cap(self.focused),
            &self.policy,
            active.surface_format,
            active.size,
            &caps,
        );
        active
            .surface
            .configure(&active.renderer.gpu().device, &config);
        self.baseline_upload_budget = Some(active.renderer.upload_budget().clone());
        // Zero-manual-step vanilla JAR bootstrap — kept in `init_gpu`
        // (the real window path) rather than `GpuApp::new` so headless
        // construction never scans host launcher dirs or hits the
        // network. Step 1: an Official/Prism/Modrinth install of the
        // same version (or Megu's own bootstrap cache) can satisfy
        // branding and textures with no download at all. Step 2: still
        // no client JAR anywhere -> manifest fetch + SHA-1 download in
        // the background, like Paper/NeoForge; `poll_vanilla_bootstrap`
        // hot-reloads textures + branding on `Ready`. Skipped while a
        // bootstrap is already running, and attempted only once per
        // process so a failed download isn't retried on every surface
        // re-creation.
        if self.vanilla_bootstrap.is_none() {
            match local_jar {
                Some(_) => {
                    self.bootstrap_attempted = true;
                }
                None if !self.bootstrap_attempted => {
                    self.bootstrap_attempted = true;
                    if crate::vanilla_bootstrap::auto_fetch_enabled() {
                        if let Some(version_id) =
                            crate::live_session::jar_version_ids(&self.session_paths)
                                .last()
                                .cloned()
                        {
                            if let Some(dest) =
                                crate::vanilla_bootstrap::bootstrap_dest_dir(&self.session_paths)
                            {
                                if let Some(handle) =
                                    crate::vanilla_bootstrap::spawn_vanilla_jar_bootstrap(
                                        &version_id,
                                        &dest,
                                    )
                                {
                                    self.status_line(format!(
                                        "Downloading Minecraft {version_id} assets in the background..."
                                    ));
                                    self.bootstrap_version = version_id;
                                    self.vanilla_bootstrap = Some(handle);
                                }
                            }
                        }
                    }
                    if self.vanilla_bootstrap.is_none() {
                        self.status_line("Vanilla client assets not found — using fallback textures");
                    }
                }
                None => {}
            }
        }
        self.ensure_vanilla_assets_bootstrap();
        // Same local-install scan feeds the font pipeline: a real
        // `ascii.png` + Unifont `.hex` upgrade text rendering to the
        // authentic vanilla font when the host has 1.21.1 installed.
        // Silent no-op on offline hosts / CI.
        self.maybe_load_vanilla_fonts();
        // The same JAR feeds the GUI-sprite atlas (`widget/*` chrome →
        // `ui_quad` binding 5). First call decodes; later `init_gpu`s
        // just re-upload the resident atlas after device/surface loss.
        if !self.gui_sprites_loaded {
            self.load_gui_sprite_atlas();
        }
        if let Some(assets) = &self.vanilla_runtime_assets {
            Self::upload_runtime_assets_to_renderer(assets, &mut active.renderer);
            if let Some(extras) = self.vanilla_official_extras.as_ref() {
                let packs = self.runtime_resource_pack_paths();
                Self::upload_title_panorama_visuals(
                    &self.session_paths,
                    self.title_branding.source_jar.as_deref(),
                    &packs,
                    extras,
                    &mut active.renderer,
                );
            }
        }
        // One-time PCIe upload of the runtime `MEGU EDITION` banner
        // textures (wordmark + patched edition strip); the CPU copies
        // stay resident in `title_branding` for a later surface loss.
        if self.title_branding.has_banner() {
            active.renderer.set_title_branding(
                self.title_branding.logo.as_ref(),
                self.title_branding.edition.as_ref(),
            );
        }
        if self.live.is_none() {
            let scene = build_demo_scene(self.camera.position);
            active
                .renderer
                .upload_scene(&scene)
                .map_err(|e| e.to_string())?;
        }
        if self.persisted_settings.shaders_enabled {
            if let Some(pack) = self.persisted_settings.shader_pack.as_deref() {
                let path = resolve_shaderpack_path(self.config.game_dir.as_deref(), pack);
                active.renderer.load_shaderpack(std::path::Path::new(&path));
                self.active_shaderpack_path = Some(path);
            }
        }
        // Fresh `GpuRenderer` (first `resumed`, device loss, surface
        // re-create): re-arm the compute-strategy push so the next live
        // frame re-applies indirect/Hi-Z/MDI policy to the new device.
        self.live_strategy_pending = true;
        self.active = Some(active);
        let settings = self.ui.settings.clone();
        self.apply_settings(&settings);
        self.maybe_regrab();
        Ok(())
    }

    /// Ghost-item guard: a `container_click` the worker queue refused
    /// (`Err(TrySendError::Full)`) never reaches the server, so the slot the
    /// screen already painted is a prediction nobody will confirm. Rebuild
    /// the surface from the authoritative `open_screen` / `player_inventory`
    /// mirror the moment the drop counter moves — before this frame draws —
    /// instead of leaving the stack stranded until the next server
    /// `state_id` bump (which may never come).
    fn resync_container_after_dropped_clicks(&mut self) {
        let dropped = self
            .live
            .as_ref()
            .map_or(0, |live| live.dropped_container_clicks);
        if dropped == self.seen_container_click_drops {
            return;
        }
        self.seen_container_click_drops = dropped;
        if self.own_inventory_open {
            // Window 0 has no server menu: `open_own_inventory` rebuilds the
            // 46 slots from `live.player_inventory`/`player_carried`.
            self.open_own_inventory();
        } else {
            // Dropping the surface makes `sync_container_screen` take its
            // `opened_now` path, which re-creates every slot and the cursor
            // from `live.open_screen` — predictions included, since a fresh
            // `ContainerScreenState` starts empty.
            self.container_screen = None;
        }
    }

    /// Mirror the live session's server-driven `open_screen` into a
    /// `ContainerScreenState`: the schema comes from `menu_schema` (all 25
    /// vanilla `0x33` menus), slots/carried refresh when `state_id` bumps,
    /// and the pointer is released while a screen is up (vanilla mouse
    /// behavior).
    fn sync_container_screen(&mut self) {
        self.resync_container_after_dropped_clicks();
        // `F3 + H` advanced tooltips decorate the slot stacks below; the
        // client-ui tooltip renders lore, so the debug suffix rides there.
        let advanced_tooltips = self.input_router.debug_chords.advanced_tooltips;
        // Phase 1: resolve the schema + snapshot the shared state under an
        // immutable borrow of `self.live`.
        struct Snapshot {
            schema: &'static megu_ui::container_screen::MenuScreenSchema,
            window_id: u8,
            state_id: i32,
            properties: std::collections::BTreeMap<i16, i16>,
            title_nbt: Vec<u8>,
            title_text: Option<String>,
            merchant_signature: Option<u64>,
            merchant_offers: Option<Vec<MerchantOfferView>>,
        }
        let previous_merchant_signature = self.merchant_offer_signature;
        let snap = self.live.as_ref().and_then(|live| {
            let open = live.open_screen.as_ref()?;
            let schema = open.menu_type.try_into().ok().and_then(menu_schema)?;
            let window_id = u8::try_from(open.window_id).ok()?;
            let merchant_panel = live.world.merchant_panels.get(&window_id);
            let merchant_signature = merchant_panel.map(|panel| merchant_signature(&panel.offers));
            let merchant_offers = merchant_panel
                .filter(|_| {
                    previous_merchant_signature
                        != merchant_signature.map(|signature| (window_id, signature))
                })
                .map(|panel| merchant_offer_views(&panel.offers));
            Some(Snapshot {
                schema,
                window_id,
                state_id: open.state_id,
                properties: open.properties.clone(),
                title_nbt: open.title_nbt.clone(),
                title_text: open.title_text.clone(),
                merchant_signature,
                merchant_offers,
            })
        });
        // Phase 2: mutate `self.container_screen` / grab state freely.
        match snap {
            Some(Snapshot {
                schema,
                window_id,
                state_id,
                properties,
                title_nbt,
                title_text,
                merchant_signature,
                merchant_offers,
            }) => {
                let localized_title = title_text
                    .filter(|title| !title.trim().is_empty())
                    .or_else(|| {
                        WireReader::new(&title_nbt)
                            .read_nbt()
                            .ok()
                            .and_then(|nbt| component_label_from_nbt(&nbt))
                            .filter(|title| !title.trim().is_empty())
                    })
                    .unwrap_or_else(|| self.localized_menu_title(schema.menu_id, schema.name));
                // A server-opened window always replaces the local
                // inventory surface.
                self.own_inventory_open = false;
                let opened_now = !matches!(
                    &self.container_screen,
                    Some(screen) if screen.window_id == window_id
                );
                let rename_initial = if opened_now && schema.menu_id == 8 {
                    self.live
                        .as_ref()
                        .and_then(|live| live.open_screen.as_ref())
                        .and_then(|open| open.slots.get(&0))
                        .filter(|stack| stack.item_id != 0)
                        .map(|stack| {
                            crate::recipe_ingest::item_name_for_id(stack.item_id.max(0) as u32)
                        })
                } else {
                    None
                };
                if opened_now {
                    self.container_screen = Some(ContainerScreenState::new(
                        schema,
                        window_id,
                        state_id,
                        localized_title.clone(),
                    ));
                    self.set_cursor_grab(false);
                }
                let runtime_assets = self.vanilla_runtime_assets.as_ref();
                let Some(screen) = self.container_screen.as_mut() else {
                    return;
                };
                if let Some(name) = rename_initial {
                    screen.rename_text = name.chars().take(50).collect();
                }
                screen.title.clone_from(&localized_title);
                // `container_set_data` updates (furnace burn/cook bars,
                // brewing progress, enchant hints) carry no `state_id` —
                // mirror the whole property map every frame so a mid-open
                // update reaches the rendered bars.
                screen.properties.clone_from(&properties);
                if let Some(offers) = merchant_offers {
                    screen.set_merchant_offers(offers);
                }
                self.merchant_offer_signature =
                    merchant_signature.map(|signature| (window_id, signature));
                if opened_now
                    || screen.state_id != state_id
                    || self.container_tooltip_advanced != advanced_tooltips
                {
                    screen.state_id = state_id;
                    self.container_tooltip_advanced = advanced_tooltips;
                    if let Some(open) = self
                        .live
                        .as_ref()
                        .and_then(|live| live.open_screen.as_ref())
                    {
                        for (index, stack) in &open.slots {
                            if let Ok(index) = usize::try_from(*index) {
                                let mut localized = Self::localized_item_stack(
                                    runtime_assets,
                                    stack.item_id,
                                    stack.count,
                                );
                                if advanced_tooltips {
                                    localized = apply_advanced_tooltips(localized, Some(stack));
                                }
                                screen.set_slot(index, localized);
                            }
                        }
                        screen.set_cursor(open.carried.as_ref().map(|carried| {
                            let localized = Self::localized_item_stack(
                                runtime_assets,
                                carried.item_id,
                                carried.count,
                            );
                            if advanced_tooltips {
                                apply_advanced_tooltips(localized, Some(carried))
                            } else {
                                localized
                            }
                        }));
                    }
                }
            }
            None => {
                self.merchant_offer_signature = None;
                if self.own_inventory_open {
                    // Window-0 sync without an `open_screen`: refresh the
                    // local inventory's slots when `state_id` bumps.
                    let runtime_assets = self.vanilla_runtime_assets.as_ref();
                    if let (Some(live), Some(screen)) = (&self.live, &mut self.container_screen) {
                        if screen.state_id != live.player_inventory_state_id
                            || self.container_tooltip_advanced != advanced_tooltips
                        {
                            screen.state_id = live.player_inventory_state_id;
                            self.container_tooltip_advanced = advanced_tooltips;
                            for (index, stack) in &live.player_inventory {
                                if let Ok(index) = usize::try_from(*index) {
                                    let mut localized = Self::localized_item_stack(
                                        runtime_assets,
                                        stack.item_id,
                                        stack.count,
                                    );
                                    if advanced_tooltips {
                                        localized = apply_advanced_tooltips(localized, Some(stack));
                                    }
                                    screen.set_slot(index, localized);
                                }
                            }
                            screen.set_cursor(live.player_carried.as_ref().map(|carried| {
                                let localized = Self::localized_item_stack(
                                    runtime_assets,
                                    carried.item_id,
                                    carried.count,
                                );
                                if advanced_tooltips {
                                    apply_advanced_tooltips(localized, Some(carried))
                                } else {
                                    localized
                                }
                            }));
                        }
                    }
                } else if self.container_screen.take().is_some() {
                    self.maybe_regrab();
                }
            }
        }
    }

    /// Forward one UI click action as the matching `container_click` /
    /// `container_button_click` serverbound packet on the worker.
    fn forward_click_action(
        live: &mut LiveSession,
        action: megu_ui::container_screen::ClickWindowAction,
    ) {
        let changed: Vec<(i16, Vec<u8>)> = action
            .changed_slots
            .iter()
            .map(|change| {
                (
                    change.slot,
                    Self::encode_clicked_stack(
                        live,
                        action.window_id,
                        change.slot,
                        change.stack.item_id as i32,
                        i32::from(change.stack.count),
                    ),
                )
            })
            .collect();
        let carried = match action.carried_item.as_ref() {
            Some(stack) => Self::encode_clicked_stack(
                live,
                action.window_id,
                -1,
                stack.item_id as i32,
                i32::from(stack.count),
            ),
            None => encode_slot_stack(0, 0),
        };
        live.container_click(
            action.window_id,
            action.state_id,
            action.slot,
            action.button,
            i32::from(action.mode),
            changed,
            carried,
        );
    }

    /// Encode one clicked `Slot`. The UI owns the predicted count, but the
    /// authoritative stack (window 0 inventory or the open container) owns
    /// the component patch — copying it keeps enchantments, custom names,
    /// and durability intact instead of emitting an empty patch list.
    /// `slot == -1` addresses the carried (cursor) stack.
    fn encode_clicked_stack(
        live: &LiveSession,
        window_id: u8,
        slot: i16,
        item_id: i32,
        count: i32,
    ) -> Vec<u8> {
        if count <= 0 {
            return encode_slot_stack(0, 0);
        }
        let authoritative = if slot < 0 {
            live.authoritative_carried_stack(window_id)
        } else {
            live.authoritative_container_stack(window_id, slot)
        };
        if let Some(stack) = authoritative.filter(|stack| stack.item_id == item_id) {
            let mut patched = stack.clone();
            patched.count = count;
            if let Ok(wire) = encode_item_stack_components(&patched) {
                return wire;
            }
        }
        encode_slot_stack(item_id, count)
    }

    /// Route a press/release to the open container screen when one is up.
    /// Returns `true` when the event was consumed by the screen.
    /// Grabbed in-game world clicks — vanilla `startAttack`/`startUseItem`
    /// crosshair dispatch: an entity hit wins only when nearer than the
    /// traced block, then dig/interact falls back to block/air.
    fn world_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        if self.has_gameplay_modal() {
            return;
        }
        if button == MouseButton::Right
            && state == ElementState::Pressed
            && self
                .live
                .as_ref()
                .is_some_and(|live| live.crosshair_pick().is_none())
            && self.open_held_writable_book()
        {
            return;
        }
        let warn_percent = self.ui.settings.qol.tool_warn_percent;
        let prevent_tool_break = self.ui.settings.qol.prevent_tool_break;
        let Some(live) = &mut self.live else { return };
        if live.is_dead() {
            return;
        }
        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => {
                if live.game_mode == 3 {
                    return;
                }
                match live.crosshair_pick() {
                    Some(CrosshairPick::Entity(hit)) => {
                        live.attack_entity(hit.entity_id);
                    }
                    Some(CrosshairPick::Block(hit)) => {
                        if let Some((damage, max_damage)) =
                            held_tool_durability(live, LiveSession::MAIN_HAND)
                        {
                            match check_tool_durability_warning(
                                damage,
                                max_damage,
                                warn_percent,
                                prevent_tool_break,
                            ) {
                                ToolDurability::BlockLastUse => {
                                    live.hud.actionbar = "Tool protected from breaking".to_owned();
                                    live.hud_revision += 1;
                                    return;
                                }
                                ToolDurability::LowWarning { remaining } => {
                                    live.hud.actionbar =
                                        format!("Tool durability low ({remaining})");
                                    live.hud_revision += 1;
                                }
                                ToolDurability::Ok => {}
                            }
                        }
                        live.begin_mining(hit.block_pos, hit.face as i8);
                        live.trigger_arm_swing();
                        self.viewmodel_swing_hand = LiveSession::MAIN_HAND;
                    }
                    None => {}
                }
                live.swing(0);
                self.viewmodel_swing_hand = LiveSession::MAIN_HAND;
            }
            (MouseButton::Left, ElementState::Released) => {
                live.cancel_mining();
            }
            (MouseButton::Right, ElementState::Pressed) => {
                // Vanilla `Minecraft.startUseItem` arms the held-repeat gate
                // (`rightClickDelay = 4`) after the interaction runs.
                self.right_click_delay.on_start_use();
                self.use_repeat_accum = 0.0;
                self.start_use_item();
            }
            (MouseButton::Right, ElementState::Released) => {
                self.right_click_delay.on_release();
                self.use_repeat_accum = 0.0;
                live.release_item_use();
            }
            (MouseButton::Middle, ElementState::Pressed) => {
                if let Some(CrosshairPick::Block(hit)) = live.crosshair_pick() {
                    let name = live
                        .looking_at_block
                        .as_ref()
                        .filter(|(pos, _)| *pos == hit.block_pos)
                        .map(|(_, name)| name.clone())
                        .unwrap_or_default();
                    if !name.is_empty() {
                        live.send_pick_item_for_block(&name);
                    }
                }
            }
            _ => {}
        }
    }

    /// Vanilla `Minecraft.startUseItem`: resolve one hand with the
    /// `[MAIN_HAND, OFF_HAND]` fallback and carry it through the target
    /// interaction, the held-item use, and the arm swing. The press edge and
    /// the `rightClickDelay` held repeat share this dispatch, so a held
    /// right-click keeps placing/interacting every four ticks.
    fn start_use_item(&mut self) {
        let warn_percent = self.ui.settings.qol.tool_warn_percent;
        let prevent_tool_break = self.ui.settings.qol.prevent_tool_break;
        let Some(live) = &mut self.live else { return };
        if live.is_dead() {
            return;
        }
        let hand = live.use_hand();
        self.viewmodel_swing_hand = hand;
        match live.crosshair_pick() {
            Some(CrosshairPick::Entity(hit)) => {
                let entity_origin = live
                    .world
                    .entities
                    .get(&hit.entity_id)
                    .map(|entity| entity.pos)
                    .unwrap_or([0.0; 3]);
                let at = Some([
                    (hit.hit_pos[0] - entity_origin[0]) as f32,
                    (hit.hit_pos[1] - entity_origin[1]) as f32,
                    (hit.hit_pos[2] - entity_origin[2]) as f32,
                ]);
                // `interact_at` then plain `interact`, both on the
                // resolved hand.
                live.interact_entity(hit.entity_id, at, hand);
                live.interact_entity(hit.entity_id, None, hand);
                live.swing(hand);
            }
            Some(CrosshairPick::Block(hit)) => {
                if let Some((damage, max_damage)) = held_tool_durability(live, hand) {
                    match check_tool_durability_warning(
                        damage,
                        max_damage,
                        warn_percent,
                        prevent_tool_break,
                    ) {
                        ToolDurability::BlockLastUse => {
                            live.hud.actionbar = "Tool protected from breaking".to_owned();
                            live.hud_revision += 1;
                            return;
                        }
                        ToolDurability::LowWarning { remaining } => {
                            live.hud.actionbar = format!("Tool durability low ({remaining})");
                            live.hud_revision += 1;
                        }
                        ToolDurability::Ok => {}
                    }
                }
                // The packet always rides the wire: the server owns the
                // arm of `performUseItemOn`, and its Spectator container
                // peek is checked before `skip_block_use`. `use_item_on`
                // drops only the block-interaction prediction while
                // `skip_block_use` holds.
                live.use_item_on(
                    hand,
                    hit.block_pos,
                    i32::from(hit.face as u8),
                    hit.cursor_in_block,
                    hit.inside,
                );
                // Starts (and swings) the held-item use for this hand.
                live.begin_item_use(hand, true);
                self.viewmodel_item_use_hand = hand;
            }
            None => {
                live.begin_item_use(hand, false);
                live.swing(hand);
                self.viewmodel_item_use_hand = hand;
            }
        }
    }

    /// Vanilla `Minecraft.tick`: while the use button stays held and the
    /// `rightClickDelay` gate has run out, `startUseItem` runs again — four
    /// game ticks after the press edge and every four ticks after that.
    fn advance_held_use_repeat(&mut self, dt: f32, gameplay_active: bool) {
        if !gameplay_active || !self.right_mouse_held {
            self.use_repeat_accum = 0.0;
            self.right_click_delay.on_release();
            return;
        }
        self.use_repeat_accum += dt.clamp(0.0, 0.25);
        let mut repeat = false;
        let mut steps = 0;
        while self.use_repeat_accum >= GAME_TICK_SECS && steps < RIGHT_CLICK_DELAY_TICKS {
            self.use_repeat_accum -= GAME_TICK_SECS;
            steps += 1;
            if self.right_click_delay.tick() {
                self.right_click_delay.on_start_use();
                repeat = true;
            }
        }
        if repeat {
            self.start_use_item();
        }
    }

    fn container_mouse_input(&mut self, state: ElementState, button: MouseButton) -> bool {
        let Some(screen) = &mut self.container_screen else {
            return false;
        };
        let (mx, my) = self.mouse_pos;
        let (w, h) = self
            .active
            .as_ref()
            .map(|a| (a.size.width as f32, a.size.height as f32))
            .unwrap_or((0.0, 0.0));
        let button_id: i8 = match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            _ => return true,
        };
        match state {
            ElementState::Pressed => {
                // recipe overlay overlay widgets (search, grid, bookmarks,
                // detail slots) outrank container slots — same rule as
                // recipe browser: sidebar clicks never touch the hovered slot.
                if self.ui.settings.qol.recipe_overlay_enabled {
                    if let Some(id) = self
                        .recipe_overlay_regions
                        .iter()
                        .find(|r| r.contains(mx, my))
                        .map(|r| r.id)
                    {
                        let actions = self.ui.recipe_browser.activate_region(id);
                        for action in &actions {
                            self.dispatch_recipe_browser_action(action);
                        }
                        return true;
                    }
                }
                let panel = screen.layout(w, h).panel;
                let before = (
                    screen.rename_focused,
                    screen.beacon_primary,
                    screen.beacon_secondary,
                    screen.selected_trade_index,
                );
                let workstation_action = screen.click_at(mx, my, w, h);
                let after = (
                    screen.rename_focused,
                    screen.beacon_primary,
                    screen.beacon_secondary,
                    screen.selected_trade_index,
                );
                let menu_id = screen.schema().menu_id;
                let rename_hit = screen
                    .rename_input_rect(w, h)
                    .is_some_and(|rect| rect.contains(mx, my));
                let merchant_row_hit = menu_id == 19
                    && (panel.x + 12..panel.x + 108).contains(&mx)
                    && (panel.y + 22..panel.y + 134).contains(&my);
                let beacon_control_hit = menu_id == 9
                    && (panel.x + 4..panel.x + 204).contains(&mx)
                    && (panel.y + 20..panel.y + 142).contains(&my);
                if let Some(action) = workstation_action {
                    self.dispatch_workstation_action(action);
                    return true;
                }
                if before != after || rename_hit || merchant_row_hit || beacon_control_hit {
                    return true;
                }
                let layout = screen.layout(w, h);
                if let Some(hit) = layout.slots.iter().find(|slot| slot.rect.contains(mx, my)) {
                    let slot_index = hit.index;
                    if button_id == 2 {
                        if self.live.as_ref().is_some_and(|live| live.game_mode == 1) {
                            if let (Some(live), Some(stack)) = (
                                self.live.as_mut(),
                                screen
                                    .slot(slot_index as usize)
                                    .filter(|stack| !stack.is_empty()),
                            ) {
                                Self::forward_click_action(
                                    live,
                                    megu_ui::container_screen::ClickWindowAction {
                                        window_id: screen.window_id,
                                        state_id: screen.state_id,
                                        slot: slot_index,
                                        button: 2,
                                        mode: 3,
                                        changed_slots: Vec::new(),
                                        carried_item: Some(stack.clone()),
                                    },
                                );
                            }
                        }
                        return true;
                    }
                    if screen.schema().menu_id == 7
                        && (0..9).contains(&slot_index)
                        && screen.cursor.is_none()
                        && screen
                            .slot(slot_index as usize)
                            .is_some_and(|stack| stack.is_empty())
                    {
                        if let Some(live) = &mut self.live {
                            live.toggle_crafter_slot(screen.window_id, i32::from(slot_index));
                        }
                        return true;
                    }
                    if screen
                        .cursor
                        .as_ref()
                        .is_some_and(|stack| !stack.is_empty())
                        && self.gui_modifiers & 3 == 0
                        && matches!(button_id, 0 | 1)
                    {
                        if let Some(action) = screen.drag_start(slot_index, button_id) {
                            if let Some(live) = &mut self.live {
                                Self::forward_click_action(live, action);
                            }
                        }
                    } else if screen
                        .cursor
                        .as_ref()
                        .is_some_and(|stack| !stack.is_empty())
                        && self.gui_modifiers & 3 == 0
                        && !panel.contains(mx, my)
                    {
                        if let Some(action) = screen.drag_start(-999, button_id) {
                            if let Some(live) = &mut self.live {
                                Self::forward_click_action(live, action);
                            }
                        }
                    } else if button_id == 0
                        && self.gui_modifiers & 3 == 0
                        && self.last_container_click.is_some_and(|(at, previous)| {
                            previous == slot_index && at.elapsed() <= Duration::from_millis(300)
                        })
                    {
                        self.last_container_click = None;
                        if let Some(action) = screen.click_mode(slot_index, 0, 6) {
                            if let Some(live) = &mut self.live {
                                Self::forward_click_action(live, action);
                            }
                        }
                    } else if let Some(action) =
                        screen.click_with_shift(slot_index, button_id, self.gui_modifiers & 3 != 0)
                    {
                        self.last_container_click = Some((Instant::now(), slot_index));
                        if let Some(live) = &mut self.live {
                            Self::forward_click_action(live, action);
                        }
                    }
                } else if let Some(hit) = layout.buttons.iter().find(|b| b.rect.contains(mx, my)) {
                    let id = hit.id;
                    if let Some(action) = screen.button_click(id) {
                        if let Some(live) = &mut self.live {
                            let _ = live.container_button_click(
                                i32::from(action.window_id),
                                action.button_id,
                            );
                            live.play_ui_click();
                        }
                    }
                } else if !panel.contains(mx, my) {
                    let action = if screen
                        .cursor
                        .as_ref()
                        .is_some_and(|stack| !stack.is_empty())
                        && self.gui_modifiers & 3 == 0
                        && matches!(button_id, 0 | 1)
                    {
                        screen.drag_start(-999, button_id)
                    } else {
                        screen.click_mode(-999, button_id, 0)
                    };
                    if let Some(action) = action {
                        if let Some(live) = &mut self.live {
                            Self::forward_click_action(live, action);
                        }
                    }
                }
            }
            ElementState::Released => {
                if let Some(action) = screen.drag_end() {
                    if let Some(live) = &mut self.live {
                        Self::forward_click_action(live, action);
                    }
                }
            }
        }
        true
    }

    fn dispatch_workstation_action(&mut self, action: WorkstationAction) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.dispatch_workstation_action(&action);
    }

    /// Inventory Drag Controls style wheel transfer: while a container owns the pointer,
    /// each wheel notch quick-moves the hovered stack. Vanilla grabbed-wheel
    /// hotbar selection remains the fallback when the feature is disabled.
    fn container_scroll_tweak(&mut self, steps: i32) -> bool {
        if steps == 0
            || !self
                .ui
                .settings
                .builtin_inventory_drag_controls_active(false)
        {
            return false;
        }
        let Some(screen) = &mut self.container_screen else {
            return false;
        };
        let Some(slot) = screen.hovered_slot else {
            return false;
        };
        let Some(live) = &mut self.live else {
            return false;
        };
        for _ in 0..steps.unsigned_abs().min(64) {
            if let Some(action) = screen.click_mode(slot as i16, 0, 1) {
                Self::forward_click_action(live, action);
            } else {
                break;
            }
        }
        true
    }

    fn update_spectator_view(&mut self, dt: f32) {
        self.spectator_view = self.live.as_ref().and_then(|live| {
            let id = live.world.camera_entity_id?;
            let target = live.world.entities.get(&id)?;
            let eye_height = live.world.entity_eye_height(id)?;
            Some(SpectatorView::follow(
                self.spectator_view,
                target,
                eye_height,
                dt,
            ))
        });
    }

    fn session_view_camera(&self) -> Camera {
        let Some(view) = self.spectator_view else {
            // Third-person distance clips against solid blocks, and the
            // `bobHurt` tilt rides the derived camera only — the player
            // camera (movement/network pose) never picks it up.
            let mut camera = self
                .camera_controller
                .clip_view_camera(&self.camera, |cell| self.camera_cell_is_solid(cell));
            camera.roll_rad += self.hurt_tilt.roll_rad;
            camera.pitch_deg = (camera.pitch_deg + self.hurt_tilt.pitch_deg).clamp(-89.9, 89.9);
            return camera;
        };
        Camera {
            position: view.eye.map(|axis| axis as f32),
            yaw_deg: (view.yaw + 180.0).rem_euclid(360.0),
            pitch_deg: (-view.pitch).clamp(-89.9, 89.9),
            fov_deg: self.camera_controller.effective_fov_deg(),
            roll_rad: 0.0,
            ..self.camera
        }
    }

    /// Solid-block probe behind `Camera.getMaxZoom`: unloaded columns and
    /// fluid cells read as empty so the third-person view never collapses
    /// against terrain the client has not streamed in.
    fn camera_cell_is_solid(&self, cell: [i32; 3]) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        match live.world.get_block_state(cell[0], cell[1], cell[2]) {
            Some(0) | None => false,
            Some(_) => live
                .world
                .fluid_cell_at(cell[0], cell[1], cell[2])
                .kind
                .is_none(),
        }
    }

    /// Resolve this frame's vanilla camera-effect state before any consumer
    /// derives the render camera: the held-use clock that drives the
    /// bow-draw FOV row, the ordered `FovPipeline` environment, and the
    /// `GameRenderer.bobHurt` tilt.
    fn advance_camera_effects(&mut self, dt: f32, gameplay_modal_open: bool) {
        let use_held = self.right_mouse_held
            && self.ui.screen == Screen::InGameHud
            && !gameplay_modal_open;
        self.use_hold_secs = if use_held {
            (self.use_hold_secs + dt.clamp(0.0, 0.25)).min(MAX_USE_HOLD_SECS)
        } else {
            0.0
        };
        let inputs = self.fov_modifier_inputs();
        self.camera_controller.set_fov_environment(inputs);
        self.hurt_tilt = self.current_hurt_tilt();
    }

    /// `AbstractClientPlayer.getFieldOfViewModifier` inputs the live session
    /// can supply: creative flight, the speed/slowness effects behind the
    /// movement-speed attribute, a bow draw, and a first-person spyglass.
    fn fov_modifier_inputs(&self) -> FovModifierInputs {
        let mut inputs = FovModifierInputs::default();
        let Some(live) = self.live.as_ref() else {
            return inputs;
        };
        if live.is_dead() {
            return inputs;
        }
        inputs.flying = live.player_abilities.flying();
        // Speed/slowness attribute modifiers only — the sprint attribute
        // boost rides the smoothed `SPRINT_FOV_BOOST` layer in
        // `CameraController::effective_fov_deg`, so it is not counted twice.
        inputs.movement_speed_ratio = status_effect_speed_ratio(&live.hud.status_effects);
        let Some(item) = session_use_item_name(live) else {
            return inputs;
        };
        let using = self.right_mouse_held && self.use_hold_secs > 0.0;
        inputs.bow_drawn = using && item.ends_with(":bow");
        if inputs.bow_drawn {
            inputs.bow_draw_ticks = self.use_hold_secs / GAME_TICK_SECS;
        }
        inputs.spyglass_scoped = using
            && self.camera_controller.mode() == CameraMode::FirstPerson
            && item.ends_with(":spyglass");
        inputs
    }

    /// Vanilla `GameRenderer.bobHurt` for the local player: the attack
    /// direction of the last `HurtAnimation` play event and its
    /// `hurtTime / hurtDuration` phase.
    fn current_hurt_tilt(&self) -> HurtTilt {
        let Some(live) = self.live.as_ref() else {
            return HurtTilt::default();
        };
        let Some(hurt) = live.local_hurt_animation() else {
            return HurtTilt::default();
        };
        compute_hurt_tilt(
            hurt.progress(),
            // `hurt_animation` carries the vanilla (server) yaw while the
            // window camera runs the protocol-767 pose yaw.
            (self.camera.yaw_deg - 180.0).rem_euclid(360.0),
            hurt.yaw_deg,
            DEFAULT_DAMAGE_TILT_STRENGTH,
        )
    }

    /// Use the same camera for rendering and mesh-priority culling.
    fn frustum_planes_for(
        camera: &Camera,
        size: Option<PhysicalSize<u32>>,
    ) -> Option<[[f32; 4]; 6]> {
        let size = size?;
        let aspect = size.width.max(1) as f32 / size.height.max(1) as f32;
        Some(crate::renderer::frustum_planes_from_view_proj(
            &camera.view_projection(aspect),
        ))
    }

    fn poll_live_in_background(&mut self) {
        let camera = self.session_view_camera();
        let planes = Self::frustum_planes_for(&camera, self.active.as_ref().map(|a| a.size));
        if let Some(live) = &mut self.live {
            live.poll(camera.position, planes.as_ref().map_or(&[][..], |p| &p[..]));
        }
        self.update_spectator_view(0.05);
        self.sync_disconnected_screen();
    }

    /// The hovered container stack's namespaced item id — the numeric
    /// `minecraft:item` registry id resolved through the built-in protocol
    /// table; empty (`0`) slots return `None`, and unknown ids degrade to
    /// `unknown:item_<n>` so a modded stack still opens a deterministic
    /// focus target.
    fn hovered_item_name(&self) -> Option<String> {
        let screen = self.container_screen.as_ref()?;
        let stack = screen.slot(screen.hovered_slot?)?;
        (stack.item_id != 0).then(|| crate::recipe_ingest::item_name_for_id(stack.item_id))
    }

    /// Item under the cursor across both surfaces: a container slot, or
    /// a sidebar/detail slot in the recipe overlay overlay's hit regions.
    fn overlay_focused_item(&self) -> Option<String> {
        self.hovered_item_name()
            .or_else(|| self.ui.recipe_browser.hovered_item())
    }

    /// recipe overlay side-overlay key chord — `true` when the press was
    /// consumed:
    /// `Ctrl`/`Cmd`+`O` toggles `recipe_overlay_enabled` in place;
    /// `A` bookmarks the hovered stack (container or sidebar slot);
    /// `R`/`U` open the recipe detail inside the overlay; `Backspace`
    /// pops search text then the detail history; `Escape` backs out of
    /// detail while leaving the container itself open.
    pub fn handle_overlay_key(&mut self, code: KeyCode, pressed: bool) -> bool {
        if pressed
            && self.container_screen.is_some()
            && !self.container_suspended_for_recipe
            && self.clipboard_modifier_held()
            && code == KeyCode::KeyO
        {
            self.ui.settings.qol.recipe_overlay_enabled =
                !self.ui.settings.qol.recipe_overlay_enabled;
            self.dispatch_ui_action(&UiAction::SettingsChanged);
            return true;
        }
        if !(pressed
            && !self.paused
            && self.container_screen.is_some()
            && !self.container_suspended_for_recipe
            && self.ui.settings.qol.recipe_overlay_enabled)
        {
            return false;
        }
        let (w, h) = self
            .active
            .as_ref()
            .map(|a| (a.size.width, a.size.height))
            .unwrap_or((0, 0));
        match code {
            KeyCode::KeyA => {
                if let Some(item) = self.overlay_focused_item() {
                    self.ui.recipe_browser.toggle_bookmark(&item);
                    return true;
                }
            }
            KeyCode::KeyR | KeyCode::KeyU => {
                if let Some(item) = self.overlay_focused_item() {
                    let mode = if code == KeyCode::KeyR {
                        DetailMode::Recipes
                    } else {
                        DetailMode::Uses
                    };
                    if self.ui.recipe_browser.index.item(&item).is_none() {
                        let name = crate::recipe_ingest::humanize_id(&item);
                        self.ui
                            .recipe_browser
                            .index
                            .add_item(ItemEntry::new(&item, &name));
                    }
                    self.ui.recipe_browser.open_detail(&item, mode);
                    return true;
                }
            }
            KeyCode::Backspace => {
                let actions = self.ui.recipe_browser.handle_input(
                    UiInputEvent::KeyPressed(UiKey::Backspace),
                    w,
                    h,
                );
                for action in &actions {
                    self.dispatch_recipe_browser_action(action);
                }
                return true;
            }
            KeyCode::Escape if self.ui.recipe_browser.detail.is_some() => {
                let actions = self.ui.recipe_browser.back_or_close();
                for action in &actions {
                    self.dispatch_recipe_browser_action(action);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// Recipe-browser `R`/`U`/`W` open: navigate to the recipe browser focused
    /// on `item` (recipes/uses detail, or a machine-tutorial scene on the Machine
    /// Tutorials tab). A hovered stack is ground truth, so the id lands in the
    /// catalog even when no JAR declared it.
    fn open_recipe_browser_focused(&mut self, item: &str, focus: RecipeFocus) {
        if self.ui.recipe_browser.index.item(item).is_none() {
            let name = crate::recipe_ingest::humanize_id(item);
            self.ui
                .recipe_browser
                .index
                .add_item(ItemEntry::new(item, &name));
        }
        match focus {
            RecipeFocus::Recipes => self
                .ui
                .recipe_browser
                .open_detail(item, DetailMode::Recipes),
            RecipeFocus::Uses => self.ui.recipe_browser.open_detail(item, DetailMode::Uses),
            RecipeFocus::MachineTutorial => {
                let scene = crate::recipe_ingest::machine_tutorial_scene_for(
                    &self.ui.recipe_browser.index,
                    item,
                );
                self.ui.recipe_browser.machine_tutorial = Some(scene);
                self.ui.recipe_browser.tab = BrowserTab::MachineTutorials;
            }
        }
        self.container_suspended_for_recipe = self.container_screen.is_some();
        self.clear_gameplay_controls();
        for action in self.ui.open_recipe_browser() {
            self.dispatch_ui_action(&action);
        }
        self.maybe_regrab();
    }

    /// Per-frame feed for the recipe browser's live inputs: the 20 Hz
    /// clock driving tag-slot cycling, the player-inventory mirror the
    /// craftable-only filter + autofill coloring read (rebuilt only when
    /// `player_inventory_state_id` moves), and a one-shot lang re-ingest
    /// when the display locale changed.
    fn sync_recipe_browser_side_state(&mut self) {
        let browser = &mut self.ui.recipe_browser;
        browser.clock_ticks = self
            .frame_start
            .saturating_duration_since(self.ui_anim_epoch)
            .as_millis() as u64
            / 50;
        if let Some(live) = &self.live {
            if live.player_inventory_state_id != self.recipe_inventory_state {
                self.recipe_inventory_state = live.player_inventory_state_id;
                browser.inventory.clear();
                for stack in live.player_inventory.values() {
                    if stack.item_id != 0 && stack.count > 0 {
                        let id =
                            crate::recipe_ingest::item_name_for_id(stack.item_id.max(0) as u32);
                        *browser.inventory.entry(id).or_insert(0) += stack.count as u64;
                    }
                }
            }
        } else if self.recipe_inventory_state != 0 {
            self.recipe_inventory_state = 0;
            browser.inventory.clear();
        }
        if browser.active_locale != self.lang_ingested_locale {
            let _ = crate::recipe_ingest::ingest_lang_into_ui(&self.session_paths, browser);
            self.lang_ingested_locale = browser.active_locale.clone();
        }
    }

    /// While a container screen is open: hover highlight, plus vanilla
    /// quick-craft (`mode = 5`) slot painting while the left button is held
    /// after a `DragState` has been armed.
    fn container_cursor_moved(&mut self) {
        if self.container_suspended_for_recipe {
            return;
        }
        let Some(screen) = &mut self.container_screen else {
            return;
        };
        let (mx, my) = self.mouse_pos;
        let (w, h) = self
            .active
            .as_ref()
            .map(|a| (a.size.width as f32, a.size.height as f32))
            .unwrap_or((0.0, 0.0));
        let layout = screen.layout(w, h);
        let hovered = layout
            .slots
            .iter()
            .find(|slot| slot.rect.contains(mx, my))
            .map(|slot| (slot.index, usize::from(slot.index.max(0) as u16)));
        screen.hover(hovered.map(|(_, index)| index));
        if !self.container_suspended_for_recipe && (self.gui_left_held || self.gui_right_held) {
            if let Some(DragState { slots, .. }) = screen.drag_mode.as_ref() {
                // The drag is armed: entering a new slot paints it into the
                // quick-craft set (`click_mode` mode 5 handles dedup).
                if let Some(&(slot_index, _)) = hovered.as_ref() {
                    if !slots.contains(&slot_index) {
                        if let Some(action) =
                            screen.drag_over(slot_index, if self.gui_right_held { 1 } else { 0 })
                        {
                            if let Some(live) = &mut self.live {
                                Self::forward_click_action(live, action);
                            }
                        }
                    }
                }
            }
        }
        // recipe overlay overlay hover: the tooltip anchor and `hovered_item`
        // read `overlay_mouse`/`hover`, fed from the last frame's hit
        // regions (stale-rect clicks are rejected in `activate` anyway).
        let overlay_on = self.ui.settings.qol.recipe_overlay_enabled;
        let browser = &mut self.ui.recipe_browser;
        browser.overlay_mouse = overlay_on.then_some((mx, my));
        browser.hover = overlay_on
            .then(|| {
                self.recipe_overlay_regions
                    .iter()
                    .find(|r| r.contains(mx, my))
                    .map(|r| r.id)
            })
            .flatten();
    }

    /// Recompute the chat bar's `/` suggestions through the session's
    /// `CommandDispatcher` (`complete_suggestions`); plain chat gets none.
    fn refresh_chat_suggestions(&mut self) {
        self.chat_suggestions.clear();
        let (Some(bar), Some(live)) = (&self.chat_bar, &self.live) else {
            return;
        };
        let text = bar.text();
        let Some(input) = text.strip_prefix('/') else {
            return;
        };
        let cursor = bar.cursor().saturating_sub(1);
        if let Ok(suggestions) = live.commands.complete_suggestions(input, cursor) {
            for index in 0..suggestions.len() {
                if let Some(item) = suggestions.get(index) {
                    self.chat_suggestions.push(item.to_string());
                }
            }
        }
    }

    /// Route a key event into the open chat bar: text from `event.text`,
    /// editing keys, Tab cycles badge completions, Enter sends through
    /// `send_chat`, Escape closes and re-grabs the pointer.
    fn chat_bar_key(&mut self, event: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        // Shift presses/releases must track even through the early
        // release-return so Shift+Tab cycles completions backwards.
        if matches!(
            event.physical_key,
            PhysicalKey::Code(KeyCode::ShiftLeft | KeyCode::ShiftRight)
        ) {
            self.chat_shift_held = event.state == ElementState::Pressed;
            return;
        }
        if event.state != ElementState::Pressed {
            return;
        }
        // Clipboard chords reach the chat bar before the per-key match:
        // `Ctrl`/`Cmd`+`V` pastes sanitized single-line text, `+C` copies
        // the draft — and neither leaks a control char into the buffer.
        if self.clipboard_modifier_held() {
            match event.physical_key {
                PhysicalKey::Code(KeyCode::KeyV) => {
                    for action in self.clipboard_paste_actions() {
                        self.apply_ui_action(&action, event_loop);
                    }
                    return;
                }
                PhysicalKey::Code(KeyCode::KeyC) => {
                    self.clipboard_copy();
                    return;
                }
                _ => {}
            }
        }
        let mut close = false;
        match event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => close = true,
            PhysicalKey::Code(KeyCode::Enter) | PhysicalKey::Code(KeyCode::NumpadEnter) => {
                if let Some(bar) = &mut self.chat_bar {
                    let text = bar.text().to_string();
                    if !text.is_empty() {
                        bar.push_history_entry(&text);
                        if let Some(live) = &mut self.live {
                            live.send_chat(&text);
                        }
                    }
                }
                close = true;
            }
            PhysicalKey::Code(KeyCode::Backspace) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.delete_backward();
                }
                self.refresh_chat_suggestions();
            }
            PhysicalKey::Code(KeyCode::Delete) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.delete_forward();
                }
                self.refresh_chat_suggestions();
            }
            PhysicalKey::Code(KeyCode::Tab) => {
                if let Some(bar) = &mut self.chat_bar {
                    // Slash-commands ask the server first
                    // (`command_suggestion` C2S) — the reply lands as
                    // `command_suggestions` and replaces the menu when
                    // its transaction id matches the pending request.
                    // Local completion still answers immediately so Tab
                    // never stalls on latency.
                    if bar.text().starts_with('/') {
                        if let Some(live) = &mut self.live {
                            self.pending_suggestion_tx =
                                live.request_command_suggestion(bar.text());
                        }
                    }
                    bar.cycle_completion(!self.chat_shift_held);
                }
            }
            PhysicalKey::Code(KeyCode::ArrowLeft) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.move_cursor_left();
                }
            }
            PhysicalKey::Code(KeyCode::ArrowRight) => {
                if let Some(bar) = &mut self.chat_bar {
                    bar.move_cursor_right();
                }
            }
            _ => {
                if let Some(text) = &event.text {
                    if !self.swallow_ime_echo(text) {
                        for action in self.committed_text_actions(text) {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                    self.refresh_chat_suggestions();
                }
            }
        }
        if close {
            self.chat_bar = None;
            self.chat_suggestions.clear();
            self.pending_suggestion_tx = None;
            self.maybe_regrab();
        }
        sync_chat_input_hud(
            &mut self.ui.hud.chat_input,
            self.chat_bar.as_ref(),
            self.ui.settings.zsh_command_bar,
        );
    }

    /// Fold the live session's server-driven completion channels into
    /// the open chat bar: a `command_suggestions` reply whose
    /// `transaction_id` matches the pending request replaces the bar's
    /// menu verbatim; `custom_chat_completions` re-seeds the word set on
    /// every revision. Cooldowns tick at 20 Hz on the frame clock.
    fn sync_chat_completion_state(&mut self, dt: f32) {
        let Some(live) = &mut self.live else {
            return;
        };
        if live.command_suggestions_rev != self.seen_suggestions_rev {
            self.seen_suggestions_rev = live.command_suggestions_rev;
            if let Some(reply) = &live.command_suggestions {
                if Some(reply.transaction_id) == self.pending_suggestion_tx {
                    if let Some(bar) = &mut self.chat_bar {
                        if bar.apply_server_suggestions(
                            reply.start.max(0) as usize,
                            reply.length.max(0) as usize,
                            &reply.matches,
                        ) {
                            self.chat_suggestions = reply.matches.clone();
                        }
                    }
                }
            }
        }
        if live.custom_chat_completions_rev != self.seen_completions_rev {
            self.seen_completions_rev = live.custom_chat_completions_rev;
            if let Some(bar) = &mut self.chat_bar {
                bar.set_custom_chat_completions(live.custom_chat_completions.iter().cloned());
            }
        }
        // Cooldowns decrement on the client tick (20 Hz), not per frame.
        self.cooldown_tick_accum = (self.cooldown_tick_accum + dt).min(1.0);
        while self.cooldown_tick_accum >= 0.05 {
            self.cooldown_tick_accum -= 0.05;
            live.tick_item_cooldowns();
        }
    }

    /// Lift the server's pending `open_sign_editor` instruction into
    /// `ui.sign_editor`: the overlay takes over keyboard + IME input like
    /// the chat bar does. Called after `live.poll` each frame.
    fn sync_sign_editor(&mut self) {
        let pending = self
            .live
            .as_mut()
            .and_then(|live| live.take_pending_sign_editor());
        let Some((pos, is_front)) = pending else {
            return;
        };
        // A fresh editor replaces the chat bar and drops every held
        // movement key so WASD can't leak into the session while typing.
        self.chat_bar = None;
        self.chat_suggestions.clear();
        self.keys = Keys::default();
        if let Some(event) = self.input_router.release_held_keys() {
            self.apply_window_input(event);
        }
        let initial = self
            .live
            .as_ref()
            .and_then(|live| live.world.block_entities.get(&[pos.0, pos.1, pos.2]))
            .and_then(|entity| WireReader::new(&entity.nbt_bytes).read_nbt().ok())
            .and_then(|nbt| sign_face_lines(&nbt, is_front));
        self.ui.sign_editor = Some(SignEditorState::new(
            pos,
            is_front,
            initial.unwrap_or_default(),
        ));
        self.set_cursor_grab(false);
        self.update_ime();
    }

    /// Route a key event into the open sign editor: arrows/editing keys
    /// drive [`SignEditorState`], Enter advances a line (closing on the
    /// last, like vanilla's sign screen), Escape closes, the
    /// `Ctrl`/`Cmd` clipboard chords paste/copy, and all other text
    /// feeds [`Self::committed_text_actions`] (Japanese text also
    /// arrives via `WindowEvent::Ime::Commit`).
    fn sign_editor_key(&mut self, event: &winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        if event.state != ElementState::Pressed {
            return;
        }
        if self.clipboard_modifier_held() {
            match event.physical_key {
                PhysicalKey::Code(KeyCode::KeyV) => {
                    for action in self.clipboard_paste_actions() {
                        self.apply_ui_action(&action, event_loop);
                    }
                    return;
                }
                PhysicalKey::Code(KeyCode::KeyC) => {
                    self.clipboard_copy();
                    return;
                }
                _ => {}
            }
        }
        let mut close = false;
        let mut save = false;
        match event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => close = true,
            PhysicalKey::Code(KeyCode::Enter) | PhysicalKey::Code(KeyCode::NumpadEnter) => {
                if let Some(editor) = &mut self.ui.sign_editor {
                    close = editor.enter();
                    save = close;
                }
            }
            PhysicalKey::Code(KeyCode::Backspace) => {
                if let Some(editor) = &mut self.ui.sign_editor {
                    editor.backspace();
                }
            }
            PhysicalKey::Code(KeyCode::ArrowUp) => {
                if let Some(editor) = &mut self.ui.sign_editor {
                    editor.move_up();
                }
            }
            PhysicalKey::Code(KeyCode::ArrowDown) => {
                if let Some(editor) = &mut self.ui.sign_editor {
                    editor.move_down();
                }
            }
            PhysicalKey::Code(KeyCode::ArrowLeft) => {
                if let Some(editor) = &mut self.ui.sign_editor {
                    editor.move_left();
                }
            }
            PhysicalKey::Code(KeyCode::ArrowRight) => {
                if let Some(editor) = &mut self.ui.sign_editor {
                    editor.move_right();
                }
            }
            _ => {
                // Typed text — an `Ime::Commit` echo is swallowed so a
                // composed string can never insert twice.
                if let Some(text) = &event.text {
                    if !self.swallow_ime_echo(text) {
                        for action in self.committed_text_actions(text) {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                }
            }
        }
        if close {
            self.finish_sign_editor(save);
        }
    }

    /// Close the sign editor and ship the result: vanilla's sign screen
    /// sends `sign_update` on every exit path (Done, Enter on the last
    /// line and Escape alike), then the pointer + IME return to gameplay
    /// state.
    fn finish_sign_editor(&mut self, save: bool) {
        let Some(editor) = self.ui.sign_editor.take() else {
            return;
        };
        if should_send_sign_update(save) {
            if let Some(live) = &mut self.live {
                live.send_update_sign(editor.pos, editor.is_front, editor.lines().clone());
            }
        }
        self.update_ime();
        self.maybe_regrab();
    }

    /// Death loop: `player_combat_kill`/`set_health(<=0)` flipped
    /// `live.death` dead -> put the death screen up (refreshing the
    /// cause/score line while it stays up); `respawn`/`set_health(>0)`
    /// clears it back onto the HUD. The screen replaces
    /// pause/inventory/chat overlays — those belong to the living
    /// player. Called from `render_frame` before the occlusion
    /// early-return so a hidden frame can never miss the transition.
    fn sync_death_screen(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        match live.death_screen_state() {
            Some(state) => {
                if self.ui.screen == Screen::Death {
                    self.ui.update_death(state);
                } else {
                    self.ui.show_death(state);
                    self.paused = false;
                    self.container_screen = None;
                    self.chat_bar = None;
                    self.ui.sign_editor = None;
                    self.advancements_open = false;
                    self.keys = Keys::default();
                    self.update_ime();
                    self.maybe_regrab();
                }
            }
            None if self.ui.screen == Screen::Death => {
                self.ui.clear_death();
                self.maybe_regrab();
            }
            None => {}
        }
    }

    /// `PlayEvent::Disconnect` / an unexpected session drop: route a
    /// recognizable missing-mod manifest to the explicit Modrinth prompt,
    /// otherwise show the exact disconnect reason.
    /// Called after `live.poll` each frame so kicks surface within one
    /// frame; the pause/container/chat/cursor state cannot survive it.
    fn sync_disconnected_screen(&mut self) {
        let reason = match self.live.as_ref().map(|live| &live.status) {
            Some(SessionStatus::Disconnected(reason)) => reason.clone(),
            Some(SessionStatus::Failed(reason)) => {
                let context = megu_core::crash_report::CrashContext::new(
                    "session_error",
                    "client-gpu",
                    &self.config.mc_version,
                    reason,
                )
                .redacted();
                // Abandon the broken simulation before presenting a menu;
                // the back stack must not reopen it. The GPU stays alive for UI.
                self.ui.go_title();
                self.ui.show_crash(context);
                self.leave_world_for_title();
                debug_assert_eq!(self.ui.screen, Screen::CrashRecovery);
                return;
            }
            _ => return,
        };
        let missing_mods = parse_missing_mod_disconnect(&reason);
        self.spectator_view = None;
        self.release_world_terrain();
        self.live = None;
        self.singleplayer_server = None;
        self.container_screen = None;
        self.own_inventory_open = false;
        self.chat_bar = None;
        self.ui.sign_editor = None;
        self.advancements_open = false;
        self.paused = false;
        self.keys = Keys::default();
        self.update_ime();
        if let Some(requirements) = missing_mods.filter(|items| !items.is_empty()) {
            let address = self.last_server_address.clone().unwrap_or_default();
            self.open_mod_sync_prompt(address, &requirements, &std::collections::BTreeMap::new());
            debug_assert_eq!(self.ui.screen, Screen::ModSyncPrompt);
        } else {
            self.ui.show_disconnected(DisconnectedScreenState {
                title: "Disconnected".to_owned(),
                reason,
            });
            debug_assert_eq!(self.ui.screen, Screen::Disconnected);
        }
        self.maybe_regrab();
    }

    fn sync_live_scoreboard(ui: &mut UiState, live: &LiveSession) {
        let scoreboard = &live.scoreboard;
        ui.hud.scoreboard = match scoreboard.display_slots.get(&1) {
            Some(objective) => ScoreboardState {
                objective_title: scoreboard
                    .objectives
                    .get(objective)
                    .and_then(|(nbt, _, _)| nbt.as_ref())
                    .and_then(|bytes| {
                        megu_client::protocol::WireReader::new(bytes)
                            .read_nbt()
                            .ok()
                            .map(|value| value.extract_text())
                    }),
                entries: scoreboard
                    .scores
                    .iter()
                    .filter(|((name, _), _)| name == objective)
                    .map(|((_, owner), score)| ScoreboardEntry {
                        label: owner.clone(),
                        score: *score,
                    })
                    .collect(),
                visible: true,
            },
            None => ScoreboardState::default(),
        };
    }

    fn tick_live_title(
        ui: &mut UiState,
        live: &LiveSession,
        title_seen_rev: &mut u64,
        title_tick_accum: &mut f32,
        dt: f32,
    ) {
        if live.title_revision != *title_seen_rev {
            *title_seen_rev = live.title_revision;
            let [fade_in, stay, fade_out] = live.title_times.unwrap_or([10, 70, 20]);
            let mut overlay = TitleOverlayState::vanilla();
            overlay.show(
                live.title_text.clone().unwrap_or_default(),
                live.subtitle_text.clone().unwrap_or_default(),
                fade_in.max(0) as u32,
                stay.max(0) as u32,
                fade_out.max(0) as u32,
            );
            ui.hud.title_overlay = overlay;
            *title_tick_accum = 0.0;
        }
        *title_tick_accum += dt;
        while *title_tick_accum >= 0.05 {
            ui.hud.title_overlay.tick();
            *title_tick_accum -= 0.05;
        }
    }

    fn sync_live_hud_items(ui: &mut UiState, live: &LiveSession) -> LiveHudMetadata {
        let hotbar = live.live_hotbar_snapshot();
        let offhand = live.live_offhand_snapshot();
        let bossbars = live.live_bossbar_snapshot();
        let overlay_stack = |stack: &crate::live_session::LiveHotbarStack| LiveHotbarOverlayMeta {
            count: stack.count,
            damage: stack.damage,
            max_damage: stack.max_damage,
        };
        let metadata = (
            std::array::from_fn(|i| hotbar[i].as_ref().map(overlay_stack)),
            offhand.as_ref().map(overlay_stack),
            std::array::from_fn(|i| {
                bossbars.get(i).map(|bar| LiveBossBarStyle {
                    color: bar.color,
                    divisions: bar.divisions,
                })
            }),
        );

        ui.hud.hotbar = hotbar.map(|stack| stack.map(|stack| stack.item_name));
        ui.hud.offhand = offhand.map(|stack| stack.item_name);
        ui.hud.bossbar_colors.clear();
        ui.hud.bossbars.clear();
        for bar in bossbars {
            let colors = megu_ui::state::BossBarColor::ALL;
            let index = bar.color.clamp(0, (colors.len() - 1) as i32) as usize;
            ui.hud.bossbar_colors.push(colors[index]);
            ui.hud
                .bossbars
                .push((bar.title, bar.health.clamp(0.0, 1.0)));
        }
        metadata
    }

    fn sync_live_hud_effects(ui: &mut UiState, live: &LiveSession) {
        let effects = live.live_status_effect_snapshot();
        ui.hud.max_health = ui.hud.max_health.max(ui.hud.health).max(20.0);
        ui.hud.heart_style = if effects.iter().any(|e| e.effect_id == 20) {
            megu_ui::state::HeartStyle::Withered
        } else if effects.iter().any(|e| e.effect_id == 19) {
            megu_ui::state::HeartStyle::Poisoned
        } else if ui.hud.hardcore {
            megu_ui::state::HeartStyle::Hardcore
        } else {
            megu_ui::state::HeartStyle::Normal
        };
        ui.hud.status_effects.clear();
        ui.hud.status_effects.extend(
            effects
                .iter()
                .filter(|effect| effect.visible_in_hud())
                .map(|effect| effect.as_hud_state()),
        );
    }

    fn sync_looking_at_hud(ui: &mut UiState, live: &LiveSession) {
        let target = live
            .looking_at_block
            .as_ref()
            .filter(|_| ui.settings.builtin_looking_at_hud_active(false))
            .and_then(|(position, _)| {
                let registry = live.mod_registry.as_ref()?;
                let [x, y, z] = *position;
                let name = registry.block_state_name(live.world.get_block_state(x, y, z)?)?;
                Some((position, name))
            });
        let looking_at = &mut ui.hud.looking_at;
        if let Some((position, name)) = target {
            if !looking_at.enabled
                || looking_at.position != Some(*position)
                || looking_at.block_id != name
            {
                *looking_at = LookingAtBlockState {
                    enabled: true,
                    block_id: name.to_owned(),
                    display_name: name.to_owned(),
                    position: Some(*position),
                    ..Default::default()
                };
            }
        } else {
            *looking_at = LookingAtBlockState::default();
        }
    }

    fn render_frame(&mut self) {
        let frame_begin = Instant::now();
        self.frame_start = frame_begin;
        let dt = frame_begin.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = frame_begin;

        let (rain_level, rain_wetness) = self.advance_frame(dt);
        let Some(viewport) = self.render_viewport() else {
            return;
        };
        let elapsed_secs = frame_begin
            .saturating_duration_since(self.ui_anim_epoch)
            .as_secs_f32();

        self.update_scene_settings(elapsed_secs);
        self.sync_live_scene(rain_level, elapsed_secs);
        let live_hud_metadata = self.sync_live_hud(dt);
        let goggles_draw = self.build_block_inspector_overlay(viewport);
        self.stage_live_scene_assets(frame_begin, dt);

        self.sync_frame_hud();
        let render_camera = self.prepare_frame_camera();
        self.sync_entity_nametags(viewport, &render_camera);
        self.sync_held_map_overlay(viewport);
        let font_revision = crate::vanilla_font::font_revision();
        self.build_frame_ui(
            viewport,
            elapsed_secs,
            font_revision,
            live_hud_metadata.as_ref(),
            goggles_draw,
        );
        let mod_overlay_rebuilt = self.refresh_mod_overlay(viewport, font_revision);
        self.upload_retained_ui(viewport, font_revision, mod_overlay_rebuilt);

        self.submit_surface_frame(viewport, &render_camera, rain_wetness);
        self.observe_frame_budget(frame_begin);
    }

    fn advance_frame(&mut self, dt: f32) -> (f32, f32) {
        if self.vanilla_runtime_assets.is_none() {
            self.load_gui_sprite_atlas();
        }
        self.refresh_player_skin_requests();

        // JIT late-latch: mouse deltas that arrived while the loop slept
        // apply once, right here at the top of the frame, so the camera
        // pose read below is the freshest input the OS delivered.
        let (mdx, mdy) = std::mem::take(&mut self.pending_mouse_delta);
        if (mdx != 0.0 || mdy != 0.0) && !self.has_gameplay_modal() {
            self.camera_controller.apply_mouse_look(
                &mut self.camera,
                mdx,
                mouse_pitch_delta(mdy, self.ui.settings.mouse_invert_y),
            );
        }

        let gameplay_modal_open = self.has_gameplay_modal();
        // Camera effects first: `poll_live_frame` derives the render camera
        // (FOV environment and `bobHurt` tilt ride it) before the movement
        // step below ticks the smoothing.
        self.advance_camera_effects(dt, gameplay_modal_open);
        let seamless_portal_frames = self.poll_live_frame(dt, gameplay_modal_open);
        self.update_spectator_view(dt);
        self.activate_pending_credits();
        self.tick_end_credits_screen(
            dt,
            self.active.as_ref().map_or(600, |a| a.size.height),
            self.keys.up,
        );
        if self.credits_screen.is_none() {
            self.service_end_credits_ack();
        }
        // Live OOM guard (`ram_budget_mib`): ~2 Hz host-RAM resample that
        // sheds distant chunk columns early when the OS runs low — runs
        // even while occluded so a minimized client still protects
        // itself, and never pre-allocates anything.
        self.poll_host_ram_guard(dt * 1000.0);
        // Forward this frame's drained MSP1 payloads into the seamless
        // dimension-transfer state machine so live server prefetch hints
        // and commit transfers drive the zero-loading-screen crossfade.
        for bytes in seamless_portal_frames {
            self.apply_seamless_portal_channel(SEAMLESS_PORTAL_CHANNEL, &bytes);
        }
        let rain_level = self.live.as_ref().map_or(0.0, |live| live.world.rain_level);
        let rain_wetness = self.rain_wetness.update(rain_level, dt);
        self.sync_disconnected_screen();
        self.sync_death_screen();
        self.sync_container_screen();
        self.sync_chat_completion_state(dt);
        self.sync_sign_editor();
        self.sync_open_book_events();

        self.advance_frame_movement(dt, gameplay_modal_open);
        // Interaction ticks run after the physics/look update so the
        // mining target is picked against the exact `RenderFramePose` this
        // frame renders with.
        self.tick_live_interactions(dt, gameplay_modal_open);
        self.sync_recipe_browser_side_state();
        (rain_level, rain_wetness)
    }

    /// Left/right held-button gameplay ticks against the frame pose. Runs
    /// after [`Self::advance_frame_movement`], so the crosshair pick and the
    /// rendered camera share one eye origin and look vector.
    fn tick_live_interactions(&mut self, dt: f32, gameplay_modal_open: bool) {
        let gameplay_active = self.ui.screen == Screen::InGameHud && !gameplay_modal_open;
        if let Some(live) = &mut self.live {
            if gameplay_active {
                if self.left_mouse_held {
                    let target = match live.crosshair_pick() {
                        Some(CrosshairPick::Block(hit)) => Some((hit.block_pos, hit.face as i8)),
                        _ => None,
                    };
                    live.tick_mining(dt, target);
                } else {
                    live.cancel_mining();
                }
                if self.right_mouse_held {
                    // Advances the use clock (eat/draw progress and the arm
                    // animation); the repeat cadence itself is the vanilla
                    // `rightClickDelay` gate below.
                    live.tick_item_use(dt);
                }
            } else {
                live.cancel_mining();
                live.release_item_use();
            }
        }
        self.advance_held_use_repeat(dt, gameplay_active);
    }

    fn poll_live_frame(&mut self, dt: f32, gameplay_modal_open: bool) -> Vec<Vec<u8>> {
        let camera = self.session_view_camera();
        let planes = Self::frustum_planes_for(&camera, self.active.as_ref().map(|a| a.size));
        if gameplay_modal_open {
            self.clear_gameplay_controls();
        }

        let Some(live) = &mut self.live else {
            return Vec::new();
        };
        live.poll(camera.position, planes.as_ref().map_or(&[][..], |p| &p[..]));
        if matches!(live.status, SessionStatus::Failed(_)) {
            return Vec::new();
        }
        live.tick_gameplay_frame(dt);
        Self::drain_protocol_particles(live, &mut self.vanilla_particles, dt);
        // `megu:portal_seamless_v1` (`MSP1`) frames captured by the
        // session's custom-channel dispatch; applied below once the
        // `live` borrow ends.
        let seamless_portal_frames = live.drain_seamless_portal_frames();
        if let Some(pose) = live.camera_sync.take() {
            self.camera.position = [
                pose.feet[0] as f32,
                pose.feet[1] as f32 + 1.62,
                pose.feet[2] as f32,
            ];
            self.camera.yaw_deg = (pose.yaw + 180.0).rem_euclid(360.0);
            self.camera.pitch_deg = (-pose.pitch).clamp(-89.9, 89.9);
        }
        // Phase-1 seamless transfer: the committed crossfade advances
        // on the monotonic frame delta (Instant — zero SystemTime),
        // and the state machine's "current" atmosphere stays pinned
        // to what the biome blender actually shows so the next
        // commit eases from real sky/fog colors, never stale defaults.
        self.seamless_portal.advance_ms(dt * 1000.0);
        let atmo = live.atmosphere_blender.current();
        self.seamless_portal
            .set_current_atmosphere(atmo.sky_horizon_rgb, atmo.fog_rgb);

        seamless_portal_frames
    }

    fn advance_frame_movement(&mut self, dt: f32, gameplay_modal_open: bool) {
        if self.live.is_none() && !self.paused {
            self.camera.apply_move(self.keys.move_input(), dt.min(0.1));
        }
        self.camera_controller
            .tick(dt, !self.paused && self.keys.sprint);

        if let Some(live) = &mut self.live {
            // Server `player_look_at` cinematics steer the actual camera
            // smoothly; the movement prediction below then inherits the new
            // yaw/pitch through the ordinary camera-control path.
            let pose_yaw = (self.camera.yaw_deg - 180.0).rem_euclid(360.0);
            let pose_pitch = -self.camera.pitch_deg;
            if let Some((yaw, pitch)) = live.poll_player_look_at(pose_yaw, pose_pitch, dt) {
                self.camera.yaw_deg = (yaw + 180.0).rem_euclid(360.0);
                self.camera.pitch_deg = (-pitch).clamp(-89.9, 89.9);
            }
            // This frame's look basis, after every camera/look update: the
            // physics step, the render camera, both crosshair raycasts, the
            // mining target, and the block outline all consume these values.
            let frame_yaw = (self.camera.yaw_deg - 180.0).rem_euclid(360.0);
            let frame_pitch = -self.camera.pitch_deg;
            let input = self.keys.move_input();
            // Dead players keep the death pose until the respawn teleport.
            let can_move = !gameplay_modal_open && !live.is_dead();
            if !live.is_dead() && live.game_mode != 3 {
                if let Some(movement) = live.step_local_player_physics(
                    dt,
                    if can_move { input.forward } else { 0.0 },
                    if can_move { input.strafe } else { 0.0 },
                    can_move && input.vertical > 0.0,
                    can_move && self.keys.down,
                    can_move && self.keys.sprint,
                    frame_yaw,
                    frame_pitch,
                ) {
                    // THE frame pose: interpolated feet + interpolated pose
                    // eye height + this frame's look basis.
                    let pose = RenderFramePose::new(
                        movement.pose.feet,
                        movement.eye_height,
                        movement.pose.yaw,
                        movement.pose.pitch,
                    );
                    self.camera.position = pose.eye.map(|axis| axis as f32);
                    live.set_render_pose(pose);
                }
            } else {
                if can_move {
                    self.camera.apply_move(input, dt.min(0.1));
                    live.send_movement(PlayerMovement {
                        pose: PlayerPose {
                            feet: [
                                self.camera.position[0] as f64,
                                (self.camera.position[1] - 1.62) as f64,
                                self.camera.position[2] as f64,
                            ],
                            yaw: frame_yaw,
                            pitch: frame_pitch,
                        },
                        strafe: if gameplay_modal_open {
                            0.0
                        } else {
                            input.strafe
                        },
                        forward: if gameplay_modal_open {
                            0.0
                        } else {
                            input.forward
                        },
                        jump: !gameplay_modal_open && input.vertical > 0.0,
                        sneak: !gameplay_modal_open && self.keys.down,
                    });
                }
                // Spectator/death: the camera itself is the eye, so the
                // same pose keeps the pick aligned with the view.
                let eye = self.camera.position;
                live.set_render_pose(RenderFramePose::new(
                    [
                        f64::from(eye[0]),
                        f64::from(eye[1]) - 1.62,
                        f64::from(eye[2]),
                    ],
                    1.62,
                    frame_yaw,
                    frame_pitch,
                ));
            }
            self.ui.hud.hardcore = live.is_hardcore();
            if can_move {
                if let Some(vpos) = live.riding_vehicle_position() {
                    let yaw = (self.camera.yaw_deg - 180.0).rem_euclid(360.0);
                    let pitch = -self.camera.pitch_deg;
                    let yaw_rad = yaw.to_radians() as f64;
                    let speed = 6.0 * f64::from(dt.min(0.1));
                    let fwd = f64::from(input.forward) * speed;
                    let strafe = f64::from(input.strafe) * speed;
                    let vx = vpos[0] - yaw_rad.sin() * fwd + yaw_rad.cos() * strafe;
                    let vz = vpos[2] + yaw_rad.cos() * fwd + yaw_rad.sin() * strafe;
                    live.send_move_vehicle(vx, vpos[1], vz, yaw, pitch);
                }
            }
            // 6-DOF subworld helm: while an SW6D snapshot binds the player
            // to a sub-world, the same movement keys steer it.
            let helm = live
                .player_subworld()
                .map(|id| subworld_helm_cmd(id, input, gameplay_modal_open));
            dispatch_subworld_helm(live, helm, &mut self.last_subworld_helm);
            if live.riding_vehicle_id().is_some() {
                // `move_vehicle` carries an absolute world pose. The
                // movement axes must never be sent in its XYZ fields.
                live.send_paddle_boat(
                    !gameplay_modal_open && self.keys.left,
                    !gameplay_modal_open && self.keys.right,
                );
            }
        }
    }

    fn render_viewport(&self) -> Option<[u32; 2]> {
        // Occluded (minimized / fully covered): zero GPU work. The live
        // session above still polls so the connection stays alive.
        if self.occluded && std::env::var_os("MEGU_DISABLE_OCCLUSION_SLEEP").is_none() {
            return None;
        }

        let active = self.active.as_ref()?;
        // Zero extent means a minimized window (Windows reports `0x0`):
        // acquiring or presenting a zero-sized swapchain panics inside
        // DXGI `ResizeBuffers`/`vkCreateSwapchainKHR`, so the policy skips
        // all GPU work here — the live-session poll above stays alive.
        if self.policy.skip_zero_extent_present
            && (active.size.width == 0 || active.size.height == 0)
        {
            return None;
        }

        Some([active.size.width.max(1), active.size.height.max(1)])
    }

    fn update_scene_settings(&mut self, elapsed_secs: f32) {
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        // Settings -> Video lighting toggles ride the shared shadow
        // uniform's `camera_forward.w` lane; refreshing every frame makes
        // each switch live on the same frame it flips.
        active.renderer.set_lighting_comfort_flags(
            self.ui.settings.video.comfort_exposure,
            self.ui.settings.video.indirect_light_bounce,
        );
        let (panorama_yaw, panorama_pitch) = vanilla_panorama_angles(elapsed_secs);
        let panorama_active =
            title_panorama_is_visible(self.ui.screen, active.renderer.has_title_panorama());
        active.renderer.set_title_panorama_state(
            panorama_active,
            panorama_yaw,
            panorama_pitch,
            1.0,
        );
        self.ui.title_panorama_active = panorama_active;
    }

    fn sync_live_scene(&mut self, rain_level: f32, elapsed_secs: f32) {
        let Some(live) = &mut self.live else {
            // No session: never leave a stale first-person hand on screen.
            if let Some(active) = self.active.as_mut() {
                active
                    .renderer
                    .set_first_person_viewmodel(FirstPersonViewmodel::default());
                active
                    .renderer
                    .set_offhand_viewmodel(FirstPersonViewmodel::default());
            }
            return;
        };
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        active.renderer.set_player_position(self.camera.position);
        active
            .renderer
            .set_block_break_overlay(live.active_block_break_overlay());
        active
            .renderer
            .set_gameplay_screen_overlay(live.gameplay_screen_overlay());
        let dimension_kind = match live.world.dimension.name.as_str() {
            "minecraft:the_nether" => 1,
            "minecraft:the_end" => 2,
            _ => 0,
        };
        let world_time = live.world_time_ticks();
        let sun_dir = megu_client_mesh::builtin_shadow_wgsl::compute_sun_direction(world_time);
        let moon_phase = ((live.world.world_age.div_euclid(24_000)).rem_euclid(8)) as u8;
        active.renderer.set_world_env_frame(
            dimension_kind,
            rain_level,
            live.season_ambience.snow_amount() > 0.5,
            moon_phase,
            sun_dir,
            elapsed_secs,
        );
        let vm = live.first_person_viewmodel(
            self.ui.screen == Screen::InGameHud
                && self.container_screen.is_none()
                && !self.paused
                && self.camera_controller.mode == CameraMode::FirstPerson
                && self.spectator_view.is_none(),
            (0.0, 0.0),
        );
        // The session keeps one swing/use clock for both hands; hand it
        // to the viewmodel of the hand that actually acted so the other
        // arm stays at rest. The offhand snapshot is built from the
        // un-gated values, then the main hand is masked below.
        let offhand_name = live.hud.offhand.clone();
        let offhand_vm = offhand_first_person_viewmodel(
            &vm,
            offhand_name.clone(),
            self.viewmodel_swing_hand,
            self.viewmodel_item_use_hand,
        );
        let mut vm = vm;
        if self.viewmodel_swing_hand == LiveSession::OFF_HAND {
            vm.swing_progress = 0.0;
        }
        if self.viewmodel_item_use_hand == LiveSession::OFF_HAND {
            vm.using_kind = FirstPersonUseKind::None;
            vm.use_progress = 0.0;
        }
        let uv_rect = vm
            .held_item_name
            .as_deref()
            .and_then(|name| self.item_icon_atlas.uv(name));
        let held_slot = 36 + live.hud.selected_slot.min(8) as i32;
        let enchanted = live
            .player_inventory
            .get(&held_slot)
            .is_some_and(crate::live_session::item_stack_has_glint);
        let (block_light, sky_light) = live.sample_block_and_sky_light(
            self.camera.position[0],
            self.camera.position[1],
            self.camera.position[2],
        );
        let light = (f32::from(block_light.max(sky_light)) / 15.0).clamp(0.2, 1.0);
        active.renderer.set_viewmodel_extras(
            uv_rect,
            enchanted,
            [light, light, light],
            elapsed_secs,
        );
        let spyglass_active = vm.visible
            && self.right_mouse_held
            && (vm
                .held_item_name
                .as_deref()
                .is_some_and(|name| name.contains("spyglass"))
                || live.hud.offhand.as_deref().is_some_and(|name| name.contains("spyglass")));
        active
            .renderer
            .set_spyglass_alpha(if spyglass_active { 1.0 } else { 0.0 });
        active.renderer.set_first_person_viewmodel(vm);
        // Slot-45 (`OFF_HAND`) mirror of the main-hand extras: same icon
        // atlas UVs and the same glint rule, keyed to the offhand stack.
        let offhand_uv_rect = offhand_name
            .as_deref()
            .and_then(|name| self.item_icon_atlas.uv(name));
        let offhand_enchanted = live
            .player_inventory
            .get(&45)
            .is_some_and(crate::live_session::item_stack_has_glint);
        active
            .renderer
            .set_offhand_viewmodel_extras(offhand_uv_rect, offhand_enchanted);
        active.renderer.set_offhand_viewmodel(offhand_vm);
        // Off-thread/priority-0 mesh payloads the session published
        // this poll ride the reconcile below — the renderer treats
        // them as authoritative until the world derives its own. The
        // revision-gated delta sync replaces the old full-map clone:
        // steady frames compare one u64 and apply nothing.
        active.renderer.sync_external_sections_from(live);
        if live.terrain_full_sync {
            // First sync / dimension change: full reconcile once.
            active.renderer.update_live_terrain(&live.world);
            live.terrain_full_sync = false;
            live.dirty_terrain_coords.clear();
            live.terrain_order_sweep = false;
            live.terrain_dirty = false;
        } else if live.terrain_dirty
            || !live.dirty_terrain_coords.is_empty()
            || live.terrain_order_sweep
        {
            // O(1) steady path: only the coords `LiveSession` marked
            // per play event plus, when the camera re-sorted the
            // translucent stream, a resident-order sweep. No
            // full-world mesh scan.
            let dirty = std::mem::take(&mut live.dirty_terrain_coords);
            let order_sweep = std::mem::take(&mut live.terrain_order_sweep);
            active
                .renderer
                .update_live_terrain_tracked(&live.world, &dirty, order_sweep);
            live.terrain_dirty = false;
        }
    }

    fn sync_live_hud(&mut self, dt: f32) -> Option<LiveHudMetadata> {
        let live = self.live.as_mut()?;
        // Revision-gated HUD sync: unchanged server state skips the
        // `HudState` clone entirely (`live.hud_sync_skips` counts hits).
        let hud_replaced = live.sync_hud_to(&mut self.ui.hud);
        let compact_debug_hud = &mut self.ui.settings.qol.compact_debug_hud;
        let compact_debug_hud_segments = self.persisted_settings.compact_debug_hud_segments;
        compact_debug_hud.enabled = self.persisted_settings.compact_debug_hud_enabled;
        compact_debug_hud.show_fps = compact_debug_hud_segments & (1 << 0) != 0;
        compact_debug_hud.show_coords = compact_debug_hud_segments & (1 << 1) != 0;
        compact_debug_hud.show_biome = compact_debug_hud_segments & (1 << 2) != 0;
        compact_debug_hud.show_facing = compact_debug_hud_segments & (1 << 3) != 0;
        compact_debug_hud.show_ping = compact_debug_hud_segments & (1 << 4) != 0;
        compact_debug_hud.show_mspt_tps = compact_debug_hud_segments & (1 << 5) != 0;
        compact_debug_hud.show_light_level = compact_debug_hud_segments & (1 << 6) != 0;
        compact_debug_hud.show_chunks_entities = compact_debug_hud_segments & (1 << 7) != 0;
        compact_debug_hud.show_ram = compact_debug_hud_segments & (1 << 8) != 0;
        // Visual-style preset x `Clouds` decides the deck layout
        // every frame: Vanilla stays one authentic cubic deck, Megu
        // Natural + Fancy opts into the hybrid parallax set.
        live.cloud_deck_mode =
            cloud_deck_mode(self.ui.settings.visual.preset, self.render_config.clouds);
        live.record_frame_and_refresh_compact_debug_hud(dt, &mut self.ui);
        let locale = self.ui.settings.locale;
        if self.live_ui_locale != Some(locale) {
            live.set_ui_locale(locale.code());
            self.live_ui_locale = Some(locale);
        }

        // Inventory, boss-bar and scoreboard packets advance the HUD
        // revision. Rebuild their projections when the base mirror replaces
        // them. Titles, effects and crosshair data can change independently.
        if hud_replaced || self.live_hud_metadata.is_none() {
            Self::sync_live_scoreboard(&mut self.ui, live);
            self.live_hud_metadata = Some(Self::sync_live_hud_items(&mut self.ui, live));
        }
        Self::tick_live_title(
            &mut self.ui,
            live,
            &mut self.title_seen_rev,
            &mut self.title_tick_accum,
            dt,
        );
        Self::sync_live_hud_effects(&mut self.ui, live);
        Self::sync_looking_at_hud(&mut self.ui, live);
        self.live_hud_metadata
    }

    fn build_block_inspector_overlay(&mut self, [w, h]: [u32; 2]) -> Option<UiDrawList> {
        // Generic block-inspector card: when the crosshair rests on a block
        // exposing live telemetry (rotary/kinetic, processing, fluid, or
        // custom metrics), reuse the cached draw list while inputs match so
        // steady frames pay zero formatting or layout cost.
        if self.ui.screen != Screen::InGameHud || self.ui.hud.hud_hidden {
            self.inspector_hud_cache = None;
            return None;
        }
        let telemetry = {
            let live = self.live.as_mut()?;
            live.looking_at_block
                .as_ref()
                .map(|(pos, _)| *pos)
                .and_then(|pos| live.block_inspector_telemetry_at((pos[0], pos[1], pos[2])))
        };
        let Some(telemetry) = telemetry else {
            self.inspector_hud_cache = None;
            return None;
        };
        let locale = self.ui.settings.locale.code();
        if let Some(cache) = self.inspector_hud_cache.as_deref() {
            if cache.viewport == [w, h] && cache.locale == locale && cache.telemetry == telemetry {
                return Some(cache.list.clone());
            }
        }
        let list = megu_ui::block_inspector_hud::BlockInspectorHudLayout::build(
            &telemetry, w, h, locale,
        );
        self.inspector_hud_cache = Some(Box::new(BlockInspectorHudCache {
            telemetry,
            viewport: [w, h],
            locale,
            list: list.clone(),
        }));
        Some(list)
    }

    fn stage_live_scene_assets(&mut self, frame_begin: Instant, dt: f32) {
        let Some(live) = &mut self.live else {
            return;
        };
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        if live.asset_bundle_pending_upload {
            // Dropping the CPU pixel payloads is part of the upload:
            // after this call VRAM owns the sole copy of the atlas and
            // `live.pending_atlas_pixels` is `None` (`atlas_meta`
            // stays resident).
            live.upload_pending_atlas(&mut active.renderer);
        }
        if !self.item_icon_atlas_uploaded {
            if let Some(mut atlas) = live.item_icon_atlas.take() {
                upgrade_ui_item_icon_atlas_with_3d_blocks(&mut atlas);
                // Same RAM/VRAM split as the block atlas: `rgba`
                // uploads once, the uv map stays CPU-side for
                // `UiCommand::ItemIcon` tessellation.
                active.renderer.upload_item_icon_atlas(&atlas);
                // Both first-person hands sample held-item sprites from
                // this same atlas through `set_viewmodel_extras` /
                // `set_offhand_viewmodel_extras` UV rects, so each hand
                // pass gets its own copy of the pixels.
                active.renderer.upload_viewmodel_surface_texture(
                    atlas.width,
                    atlas.height,
                    &atlas.rgba,
                );
                active.renderer.upload_offhand_viewmodel_surface_texture(
                    atlas.width,
                    atlas.height,
                    &atlas.rgba,
                );
                self.item_icon_atlas = atlas;
                self.item_icon_atlas.rgba = Vec::new();
                self.item_icon_atlas_uploaded = true;
            }
        }
        // `.mcmeta` playback: one `SpriteFrame` rewrite per animated
        // sprite, and only when the client-side 20 Hz clock advanced
        // inside `poll` — steady frames cost one compare.
        active
            .renderer
            .tick_atlas_animations(live.atlas_anim_tick());
        // Moving contraptions and airships shade against the world
        // they currently occupy: sample the cached chunk light at
        // each sub-world anchor so cave transits darken and
        // torch-side or daylight flight brightens the staged mesh.
        // The reserved voxel-cloud sheet keeps its baked vertex
        // lighting — a world-light word would double-shade it.
        self.subworld_light_scratch.clear();
        self.subworld_light_scratch.extend(
            live.subworlds
                .entries()
                .filter(|entry| entry.id != CLOUD_SUBWORLD_ID)
                .map(|entry| {
                    let p = entry.transform.position;
                    let (block_light, sky_light) =
                        live.sample_block_and_sky_light(p[0], p[1], p[2]);
                    (entry.id, block_light, sky_light)
                }),
        );
        for &(id, block_light, sky_light) in &self.subworld_light_scratch {
            live.subworlds
                .set_subworld_light(id, block_light, sky_light);
        }
        active.renderer.sync_subworlds(&mut live.subworlds);
        // First live frame on this renderer (attach order or
        // surface/device re-creation): push the session's
        // connect-time `ComputeStrategy`. `apply_compute_strategy`
        // engages GPU indirect/Hi-Z/MDI only where the adapter
        // proves indirect+compute support and the unified plan
        // allows it; everything else fails closed to the
        // classic/bundle path.
        Self::ensure_live_compute_strategy(
            live,
            &mut self.live_strategy_pending,
            &mut active.renderer,
        );
        // Live entities + block-entity meshes ride the dynamic ring;
        // re-staged at most at the 20 Hz cadence the server actually
        // mutates tracked state, skipped outright while nothing is
        // renderable.
        if self
            .entity_render_cache
            .next_refresh
            .is_none_or(|t| frame_begin >= t)
        {
            self.entity_render_cache.next_refresh = Some(frame_begin + LIVE_ENTITY_REFRESH);
            Self::refresh_live_entity_and_ber(
                live,
                &mut self.entity_render_cache,
                &mut active.renderer,
                // `entity_shadows` is the Video tab `Shadows`
                // toggle — read it live like `enchantment_glint`
                // does on the 2D path.
                self.ui.settings.video.entity_shadows,
                self.ui.settings.video.enchantment_glint,
                self.entity_appearance.as_ref(),
                dt,
            );
        }
        // First live frame on the windowed path: hand the sink to the
        // dedicated real-time `AudioMixThread` so gameplay code
        // submits `AudioCmd`s non-blocking (headless/test sessions
        // keep the synchronous `pump()` fallback).
        if !live.dedicated_audio_mix() {
            live.enable_dedicated_audio_mix();
        }
        // Upload admission finished above: drop the CPU mesh copies
        // of the sections VRAM now owns (keeps `packed_mesh_count()`
        // at the working-set bound, V2).
        live.release_uploaded_cpu_copies(&mut active.renderer);
        // Tactile weather/touch sim ticks on the render clock.
        let submerged = live.player_eye_submerged();
        live.step_tactile_frame(
            dt,
            live.world_time_ticks(),
            live.world.rain_level <= 0.0,
            submerged,
            15,
            0.0,
            live.world.rain_level > 0.0,
        );
    }

    fn sync_frame_hud(&mut self) {
        // HUD/diagnostics through the real client-ui state machine. The
        // F3 overlay's DATA refreshes at 10 Hz (`F3_REFRESH`) — steady
        // frames reuse the cached contents, which also keeps the
        // draw-list fingerprint stable so the UI mesh rebuild stays
        // skipped — and the cached overlay is re-mirrored every frame so
        // `sync_hud_to`'s revision-gated wholesale clone can't stomp it.
        let stale = self
            .f3_overlay_built
            .is_none_or(|built| built.elapsed() >= F3_REFRESH);
        if stale {
            self.f3_overlay = Self::build_f3_overlay(
                self.live.as_ref(),
                &self.governor,
                self.user_shadow_quality,
                &mut self.f3_metrics_probe,
                self.active
                    .as_ref()
                    .expect("visible frame has an active window"),
            );
            self.f3_overlay_built = Some(Instant::now());
        }
        self.f3_overlay.enabled = self.show_diagnostics;
        if self.ui.hud.debug_overlay != self.f3_overlay {
            self.ui.hud.debug_overlay.clone_from(&self.f3_overlay);
        }
        // Actionbar mirror: always the real game actionbar / connection
        // status — the F3 overlay lives in `hud.debug_overlay` and never
        // hijacks this channel again.
        let actionbar = &mut self.ui.hud.actionbar;
        if let Some(live) = &self.live {
            let (prefix, message) = match &live.status {
                SessionStatus::Playing => ("", live.hud.actionbar.as_str()),
                SessionStatus::Connecting => ("", "Connecting to server..."),
                SessionStatus::Disconnected(reason) => ("Disconnected: ", reason.as_str()),
                SessionStatus::Failed(_) => ("Session error", ""),
            };
            if actionbar.strip_prefix(prefix) != Some(message) {
                actionbar.clear();
                actionbar.push_str(prefix);
                actionbar.push_str(message);
            }
        } else {
            actionbar.clear();
        }
        // Chat-bar mirror lands after `sync_hud_to` (which clones
        // `live.hud` wholesale) and before the draw list rebuild — the
        // input strip + suggestion popup read it this frame.
        sync_chat_input_hud(
            &mut self.ui.hud.chat_input,
            self.chat_bar.as_ref(),
            self.ui.settings.zsh_command_bar,
        );
    }

    fn build_frame_ui(
        &mut self,
        viewport: [u32; 2],
        elapsed_secs: f32,
        font_revision: u64,
        live_hud_metadata: Option<&LiveHudMetadata>,
        goggles_draw: Option<UiDrawList>,
    ) {
        self.build_screen_ui(viewport, live_hud_metadata);
        self.update_frame_title_branding(viewport, font_revision);
        let glint_on = self.ui.settings.video.enchantment_glint && !self.ui.hud.hud_hidden;
        self.build_mod_hud(viewport, glint_on, elapsed_secs, goggles_draw);
        self.build_gameplay_overlay(viewport, glint_on, elapsed_secs);
    }

    fn build_screen_ui(&mut self, [w, h]: [u32; 2], metadata: Option<&LiveHudMetadata>) {
        // Pooled draw list (C4): `render_into` clears + refills
        // `self.ui_draw_list` in place so steady frames allocate
        // nothing (text payloads recycle inside the list). The command
        // stream is identical to `self.ui.render(w, h)` — the
        // fingerprint below feeds the same dirty-check either way.
        self.ui.render_into_with_gui_atlas(
            w,
            h,
            &mut self.ui_draw_list,
            self.gui_sprites_loaded.then_some(&self.gui_sprite_atlas),
        );
        if self.ui.screen == Screen::InGameHud && !self.ui.hud.hud_hidden {
            if let Some((hotbar, offhand, bossbars)) = metadata {
                apply_live_hotbar_metadata(
                    w,
                    h,
                    self.ui.effective_scale(h),
                    hotbar,
                    offhand.as_ref(),
                    self.ui.hud.game_mode,
                    &mut self.ui_draw_list,
                );
                apply_live_bossbar_metadata(w, bossbars, &mut self.ui_draw_list);
            }
        }
    }

    fn update_frame_title_branding(&mut self, viewport: [u32; 2], font_revision: u64) {
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        let [w, h] = viewport;
        let gui_scale = self.ui.effective_scale(h);
        // The screen list is freshly built, so remove its fallback text
        // even when the renderer already holds the matching banner mesh.
        let banner = self
            .title_branding
            .logo
            .as_ref()
            .zip(self.title_branding.edition.as_ref())
            .filter(|_| self.ui.screen == Screen::Title);
        let key = if banner.is_some() {
            self.ui_draw_list
                .commands
                .retain(|c| !is_builtin_title_text(c));
            TitleBrandingFrame::Visible {
                viewport,
                gui_scale,
                font_revision,
            }
        } else {
            TitleBrandingFrame::Hidden
        };
        if active.title_branding_frame == Some(key) {
            return;
        }
        active.title_branding_frame = Some(key);

        if banner.is_some() {
            let layout = crate::title_branding::title_brand_layout(w, h, gui_scale);
            let splash = self.title_branding.select_splash(self.splash_seed);
            let splash = megu_ui::draw::first_px(
                splash,
                layout.splash_anchor_x as usize / gui_scale as usize,
            );
            let mut splash_list = UiDrawList::new();
            splash_list.text(
                layout.splash_anchor_x - text_width(splash, gui_scale) as i32,
                layout.splash_y,
                gui_scale,
                UiColor::rgb(255, 255, 0),
                splash,
            );
            let splash_mesh = tessellate_draw_list(w, h, &splash_list);
            active.renderer.update_title_branding(
                Some(layout.logo_rect),
                Some(layout.edition_rect),
                Some(&splash_mesh),
            );
        } else {
            active.renderer.update_title_branding(None, None, None);
        }
    }

    fn build_mod_hud(
        &mut self,
        [w, h]: [u32; 2],
        glint_on: bool,
        glint_elapsed: f32,
        goggles_draw: Option<UiDrawList>,
    ) {
        self.ui_modhud_list.clear();
        // Live goggles telemetry composites beside the crosshair on the
        // in-game HUD only (hidden behind menus/containers by the
        // `Screen::InGameHud` gate where `goggles_draw` was produced).
        if let Some(goggles) = goggles_draw {
            self.ui_modhud_list.commands.extend(goggles.commands);
        }
        // Enchantment glint shimmer over enchanted 2D slots: the same
        // `enchantment_glint_rgb` the 3D path runs, composited as a
        // subtle translucent wash inside each slot cell. Hotbar/offhand
        // read window-0 components directly (36..45 = hotbar, 45 =
        // offhand) and only while the bare HUD is up — an open container
        // re-draws every slot through its own overlay pass below.
        if glint_on
            && self.ui.screen == Screen::InGameHud
            && self.ui.hud.game_mode != 3
            && self.container_screen.is_none()
        {
            if let Some(live) = &self.live {
                let scale = self.ui.effective_scale(h);
                let slots = (-1..9).map(|index| (index, if index < 0 { 45 } else { 36 + index }));
                for (index, stack_slot) in slots {
                    if let Some(stack) = live.player_inventory.get(&stack_slot) {
                        let rect = hotbar_slot_rect(w, h, scale, index);
                        if let Some(cmd) =
                            enchant_glint_overlay_cmd(true, stack, rect, w, h, glint_elapsed)
                        {
                            self.ui_modhud_list.commands.push(cmd);
                        }
                    }
                }
            }
        }
    }

    fn build_gameplay_overlay(&mut self, viewport: [u32; 2], glint_on: bool, elapsed_secs: f32) {
        let [w, h] = viewport;
        self.ui_overlay_list.clear();
        self.build_container_overlay(viewport, glint_on, elapsed_secs);
        if self.live.as_ref().is_some_and(LiveSession::is_sleeping) {
            megu_ui::gameplay_screens::BedSleepOverlay { visible: true }.render(
                w,
                h,
                &mut self.ui_overlay_list,
            );
        }
        if let Some(book) = &self.book_editor {
            book.render(w, h, &mut self.ui_overlay_list);
        }
        if let Some(creative) = &self.creative_inventory {
            creative.render(w, h, &mut self.ui_overlay_list);
        }
        if let Some(credits) = &self.credits_screen {
            credits.render(w, h, &mut self.ui_overlay_list);
        }
        if self.shader_options_open {
            let (list, _) = self.ui.shader_packs.render(w, h);
            self.ui_overlay_list.commands.extend(list.commands);
        }
        if self.advancements_open {
            Self::render_advancements_overlay(
                self.live.as_ref(),
                w as i32,
                h as i32,
                &mut self.ui_overlay_list,
            );
        }
    }

    fn build_container_overlay(&mut self, [w, h]: [u32; 2], glint_on: bool, glint_elapsed: f32) {
        // Server-driven container screen draws above the HUD — the
        // `Overlay` layer in the retained cache.
        if let Some(screen) = self
            .container_screen
            .as_ref()
            .filter(|_| !self.container_suspended_for_recipe)
        {
            screen.render_with_atlas(
                w as f32,
                h as f32,
                self.own_inventory_open.then_some(u32::MAX),
                &self.gui_sprite_atlas,
                &mut self.ui_overlay_list,
            );
            // recipe overlay side overlay: bookmark column left, search+grid
            // right — only when the QoL toggle is on and both columns
            // fit without overlapping the container panel.
            if self.ui.settings.qol.recipe_overlay_enabled {
                let panel = screen.panel_rect(w as f32, h as f32);
                if let Some((overlay, regions)) = self.ui.recipe_browser.render_overlay(w, h, panel)
                {
                    self.ui_overlay_list.commands.extend(overlay.commands);
                    self.recipe_overlay_regions = regions;
                } else {
                    self.recipe_overlay_regions.clear();
                }
            } else {
                self.recipe_overlay_regions.clear();
            }
            if glint_on {
                // The window-0 inventory feeds slots straight from
                // `live.player_inventory`; a server screen renders
                // `live.open_screen.slots` instead. Every rendered cell
                // has a `slot_rect`, so the pass emits one overlay per
                // enchanted stack and skips empty/unmapped entries.
                let components: Option<&std::collections::BTreeMap<i32, ItemStackComponents>> =
                    if self.own_inventory_open {
                        self.live.as_ref().map(|live| &live.player_inventory)
                    } else {
                        self.live
                            .as_ref()
                            .and_then(|live| live.open_screen.as_ref())
                            .map(|open| &open.slots)
                    };
                if let Some(components) = components {
                    let panel = screen.panel_rect(w as f32, h as f32);
                    for (index, stack) in components {
                        if let Some(rect) = i16::try_from(*index)
                            .ok()
                            .and_then(|index| screen.slot_rect(index, panel))
                        {
                            if let Some(cmd) =
                                enchant_glint_overlay_cmd(true, stack, rect, w, h, glint_elapsed)
                            {
                                self.ui_overlay_list.commands.push(cmd);
                            }
                        }
                    }
                }
            }
        }
    }

    fn refresh_mod_overlay(&mut self, viewport: [u32; 2], font_revision: u64) -> bool {
        let [w, h] = viewport;
        let layout = ModOverlayLayout {
            viewport,
            scale: self.ui.effective_scale(h),
            font_revision,
            sprites_loaded: self.gui_sprites_loaded,
        };
        if self.mod_overlay_layout.as_ref() == Some(&layout) {
            return false;
        }

        self.ui_modoverlay_list.clear();
        // Tier-2 mod GUI bridge: the active batch lowers straight into
        // the mod-overlay layer list (scissor-safe inside the frame's
        // clip), zero cost while no mod recorded ops this frame.
        if !self.gui_bridge_batch.ops.is_empty() {
            let mut lowered = lower_bridge_batch_to_draw_list(&self.gui_bridge_batch, w, h);
            for (index, command) in std::mem::take(&mut lowered.commands)
                .into_iter()
                .enumerate()
            {
                self.ui_modoverlay_list
                    .push_with_clip(command, lowered.command_clip(index));
            }
        }
        // Declarative mod-UI manifest: the shared Vanilla widget kit
        // renders it on the same layer (Vanilla 1.21.1 sprite atlas when
        // the client JAR supplied one, pixel-authentic bevels otherwise).
        if let Some(manifest) = &self.mod_ui_manifest {
            megu_ui::vanilla_widgets::draw_mod_ui_manifest_sprites(
                &mut self.ui_modoverlay_list,
                manifest,
                layout.scale,
                UiRect::new(0, 0, w, h),
                layout.sprites_loaded.then_some(&self.gui_sprite_atlas),
            );
        }

        self.mod_overlay_layout = Some(layout);
        true
    }

    fn upload_retained_ui(
        &mut self,
        [w, h]: [u32; 2],
        font_revision: u64,
        mod_overlay_rebuilt: bool,
    ) {
        // Retained per-layer upload: fingerprint each layer's list and
        // bump its `UiLayer` revision only when content moved — the
        // retained cache then re-tessellates just the dirty layers into
        // warm buffers and uploads the single merged batch only when the
        // composite actually differs (`renderer.ui_cpu_raster_uploads`
        // stays at zero either way).
        let layer_lists: [(UiLayer, &UiDrawList); 4] = [
            (UiLayer::Screen, &self.ui_draw_list),
            (UiLayer::ModHud, &self.ui_modhud_list),
            (UiLayer::Overlay, &self.ui_overlay_list),
            (UiLayer::ModOverlay, &self.ui_modoverlay_list),
        ];
        for (layer, list) in layer_lists {
            if layer == UiLayer::ModOverlay && !mod_overlay_rebuilt {
                continue;
            }
            let mut fp = fingerprint_draw_list(w, h, list);
            if layer == UiLayer::Screen {
                fp ^= font_revision.wrapping_mul(FNV1A64_PRIME);
            }
            if self.ui_layer_fps[layer.index()] != fp {
                self.ui_layer_fps[layer.index()] = fp;
                layer.bump(&mut self.ui_layer_bus);
            }
        }
        let dirty = self.ui_retained.begin_frame(&self.ui_layer_bus);
        if dirty.is_empty() && !self.ui_retained.gpu_upload_needed() {
            return;
        }
        for index in dirty.iter() {
            let Some((layer, list)) = layer_lists.iter().find(|(layer, _)| layer.index() == index)
            else {
                continue;
            };
            self.ui_retained.update_layer(
                *layer,
                list,
                w,
                h,
                &self.item_icon_atlas,
                &self.gui_sprite_atlas,
            );
        }
        let _ = self.ui_retained.composite();
        if self.ui_retained.gpu_upload_needed() {
            let (vertices, indices) = self.ui_retained.composite();
            self.active
                .as_mut()
                .expect("visible frame has an active window")
                .renderer
                .update_ui_mesh_parts(w, h, vertices, indices);
            self.ui_retained.mark_uploaded();
        }
    }

    fn sync_held_map_overlay(&mut self, [w, h]: [u32; 2]) {
        let overlay = self
            .ui
            .hud
            .held_map
            .as_ref()
            .zip(megu_ui::layout::held_map_viewport(&self.ui, w, h));
        if let Some(active) = &mut self.active {
            let assets = self
                .vanilla_runtime_assets
                .as_ref()
                .map(|a| &a.atlas_extras)
                .filter(|atlas| atlas.from_jar_count() > 0);
            active.renderer.set_held_map_overlay(overlay, assets);
        }
    }

    fn sync_entity_nametags(&mut self, [w, h]: [u32; 2], camera: &Camera) {
        // Crowded-server cap on how many labels are painted at once.
        const MAX_NAMETAGS: usize = 32;
        self.ui.hud.entity_nametags.clear();
        let Some(live) = self
            .live
            .as_ref()
            .filter(|_| self.ui.screen == Screen::InGameHud && !self.ui.hud.hud_hidden)
        else {
            return;
        };
        let viewer = live
            .player_info
            .get(&self.local_player_uuid())
            .and_then(|p| p.name.as_deref())
            .unwrap_or(&self.config.username);
        let matrix = camera.view_projection(w.max(1) as f32 / h.max(1) as f32);
        let candidates = live.world.entities.iter().filter_map(|(&eid, entity)| {
            let feet = self
                .entity_render_cache
                .entity_interpolation
                .get(&eid)
                .map_or(entity.pos, |sample| sample.position);
            let distance_sq: f64 = feet
                .iter()
                .zip(camera.position)
                .map(|(a, b)| (a - f64::from(b)).powi(2))
                .sum();
            let crouched = live.entity_metadata.get(&eid).is_some_and(|meta| {
                meta.pose == Some(5) || meta.flags.is_some_and(|flags| flags & 0x02 != 0)
            });
            let range = if crouched { 32.0 } else { 64.0 };
            if distance_sq > range * range {
                return None;
            }
            let (text, color) = live.entity_nametag(eid, viewer)?;
            let y = feet[1] + f64::from(live.world.entity_eye_height(eid)?) + 0.45;
            let anchor = project_nametag(&matrix, [feet[0] as f32, y as f32, feet[2] as f32])?;
            Some((distance_sq, anchor, text, color))
        });
        let mut tags: Vec<_> = candidates.collect();
        // Bounded selection on crowded servers: one O(n) partition keeps the
        // nearest labels, so a world with hundreds of entities never pays a
        // full sort — and only the labels actually painted become `String`.
        if tags.len() > MAX_NAMETAGS {
            tags.select_nth_unstable_by(MAX_NAMETAGS, |a, b| a.0.total_cmp(&b.0));
            tags.truncate(MAX_NAMETAGS);
        }
        // Paint far to near.
        tags.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let painted = tags
            .into_iter()
            .rev()
            .map(|(_, anchor, text, color)| HudNametag {
                anchor,
                text: text.into_owned(),
                color,
            });
        self.ui.hud.entity_nametags.extend(painted);
    }

    fn prepare_frame_camera(&mut self) -> Camera {
        let mut render_camera = self.session_view_camera();
        // Tactile micro-motion: transient turn-roll + landing dip from
        // the live session's camera sim. The state machine emits an
        // exact zero offset while the setting is off, so this never
        // perturbs the view when disabled.
        if let Some(live) = self.live.as_ref().filter(|_| self.spectator_view.is_none()) {
            let micro = live.latest_camera_micro_offset();
            render_camera.roll_rad += micro.roll_rad;
            render_camera.position[1] -= micro.landing_dip_blocks;
        }
        // Environment audio hint: the tactile sim's underwater low-pass
        // doubles as the `submerged` flag on the mix thread's
        // `UpdateListener` command — real underwater acoustics on the
        // dedicated audio path, not just the synchronous mixer.
        if let Some(live) = &mut self.live {
            let submerged = live.latest_audio_hint().underwater_lowpass > 0.3;
            live.set_listener_submerged(submerged);
        }
        self.update_frame_debug_overlay(&render_camera);
        render_camera
    }

    fn update_frame_debug_overlay(&mut self, camera: &Camera) {
        let night_sky_on = self.ui.settings.visual.world_effects
            && self.ui.settings.visual.world_effects_detail.night_sky;
        let drama = self
            .live
            .as_ref()
            .filter(|_| night_sky_on)
            .map(|live| live.latest_night_sky_drama())
            .filter(|drama| !drama.is_empty());
        let block_outline = match self.live.as_ref().and_then(LiveSession::crosshair_pick) {
            Some(CrosshairPick::Block(hit)) => Some(hit.block_pos),
            _ => None,
        };
        // `F3 + B` hitboxes and `F3 + G` chunk borders ride the same
        // world-space debug-line ring as the night-sky drama.
        let show_hitboxes = self.input_router.debug_chords.show_hitboxes;
        let show_chunk_borders = self.input_router.debug_chords.show_chunk_borders;
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");

        if drama.is_none() && !show_hitboxes && !show_chunk_borders {
            let overlay = FrameDebugOverlay::BlockOutline(block_outline);
            if active.debug_overlay != overlay {
                active.renderer.set_block_selection(block_outline);
                active.debug_overlay = overlay;
            }
            return;
        }

        build_debug_world_lines(
            self.live.as_ref(),
            drama,
            camera,
            block_outline,
            show_hitboxes,
            show_chunk_borders,
            &mut self.debug_line_scratch,
        );
        active.renderer.set_debug_lines(&self.debug_line_scratch);
        active.debug_overlay = FrameDebugOverlay::WorldLines;
    }

    fn submit_surface_frame(&mut self, viewport: [u32; 2], camera: &Camera, rain_wetness: f32) {
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");

        let [w, h] = viewport;
        let sky = active.renderer.set_camera(camera, w as f32 / h as f32);
        match active.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());

                self.draw_surface_frame(viewport, &view, camera, &sky, rain_wetness);
                self.present_surface_frame(viewport, frame);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {}
            surface_kind @ (wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation) => {
                let caps = active
                    .surface
                    .get_capabilities(&active.renderer.gpu().adapter);
                let cfg = surface_config_for(
                    self.render_config.vsync,
                    self.render_config.effective_fps_cap(self.focused),
                    &self.policy,
                    active.surface_format,
                    active.size,
                    &caps,
                );
                active
                    .surface
                    .configure(&active.renderer.gpu().device, &cfg);
                if matches!(surface_kind, wgpu::CurrentSurfaceTexture::Lost) {
                    // The atlas pixel payloads were dropped after upload;
                    // re-derive them on demand instead of keeping a CPU
                    // duplicate resident for a loss that may never come.
                    if let Some(live) = &mut self.live {
                        live.request_atlas_reload();
                    }
                }
            }
        }
    }

    fn draw_surface_frame(
        &mut self,
        [w, h]: [u32; 2],
        view: &wgpu::TextureView,
        render_camera: &Camera,
        sky: &SkyFogOutput,
        rain_wetness: f32,
    ) {
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        match &self.live {
            Some(live) => {
                // One `frame_output` snapshot feeds both the
                // light UBO (`dynamic_colored_lights` did the
                // same call and discarded the rest) and the
                // particle staging below.
                let mut vfx_frame = live.vfx.frame_output(render_camera.forward_vector());
                // Gameplay dynamic lights (held torches, glowing
                // mobs, dropped glow items) share the 64-light
                // WGSL UBO with the server VFX1 lights —
                // nearest-first, budget-capped inside
                // `collect_dynamic_lights`, empty while the
                // world-effects setting is Off. The
                // `video.dynamic_lights` switch skips the collect
                // outright so the toggle kills every gameplay
                // light the same frame it flips.
                if self.ui.settings.video.dynamic_lights {
                    vfx_frame
                        .lights
                        .extend(live.collect_dynamic_lights(render_camera.position));
                }
                Self::stage_live_particles(
                    live,
                    &self.vanilla_particles,
                    &vfx_frame,
                    render_camera,
                    &mut self.particle_scratch,
                    &mut self.live_particles_staged,
                    &mut active.renderer,
                );
                // Phase-1 seamless dimension transfer: while the
                // committed crossfade is in flight, fold its eased
                // sky/fog into this frame's atmosphere so the swap
                // glides instead of hard-cutting; while the
                // committed dimension's loading screen stays
                // suppressed, the per-chunk arrival dissolve is
                // swapped for an instantly-settled tracker so
                // prefetched terrain pops in with zero black beat.
                let portal_atmosphere = self.seamless_portal.blended_sky_and_fog().map(
                    |(sky_rgb, fog_rgb, progress)| {
                        let from = live.atmosphere_blender.current();
                        let mut to = from;
                        to.sky_zenith_rgb = sky_rgb;
                        to.sky_horizon_rgb = sky_rgb;
                        to.fog_rgb = fog_rgb;
                        BiomeAtmosphereBlender::new(blend_dimension_crossfade(&from, &to, progress))
                    },
                );
                let atmosphere = portal_atmosphere
                    .as_ref()
                    .unwrap_or(&live.atmosphere_blender);
                let empty_fades;
                let chunk_fades = if self
                    .seamless_portal
                    .should_suppress_loading_screen(&live.world.dimension.name)
                {
                    empty_fades = ChunkFadeTracker::new();
                    &empty_fades
                } else {
                    &live.chunk_fades
                };
                active.renderer.render_to_view_with_env_and_rain(
                    view,
                    w,
                    h,
                    sky,
                    atmosphere,
                    chunk_fades,
                    live.world_time_ticks(),
                    &vfx_frame.lights,
                    rain_wetness,
                );
            }
            None => active.renderer.render_to_view(view, w, h, sky),
        }
    }

    fn present_surface_frame(&mut self, viewport: [u32; 2], frame: wgpu::SurfaceTexture) {
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        // Wayland `wl_surface.frame` pacing: notify the compositor
        // right before present so frame callbacks align with the
        // actual swap (a no-op everywhere else; the policy gates
        // it). `--vsync off` skips the notify entirely — an
        // `Immediate`/`Mailbox` swap must not wait on a frame
        // callback the compositor only schedules for vsync clients.
        if self.policy.call_pre_present_notify
            && crate::should_call_pre_present_notify(self.config.vsync)
        {
            active.window.pre_present_notify();
        }

        Self::dump_surface_frame(&active.renderer, viewport, &frame.texture);
        self.save_pending_screenshot(viewport, &frame.texture);
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        active.renderer.gpu().queue.present(frame);
        if std::env::var_os("MEGU_FPS_LOG").is_some() {
            static FPS: std::sync::OnceLock<std::sync::Mutex<(std::time::Instant, u32)>> =
                std::sync::OnceLock::new();
            let st = FPS.get_or_init(|| std::sync::Mutex::new((std::time::Instant::now(), 0)));
            let mut g = st.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            g.1 += 1;
            let el = g.0.elapsed();
            if el >= std::time::Duration::from_secs(5) {
                eprintln!(
                    "[fps] {:.1} fps ({} frames / {:.1}s)",
                    g.1 as f64 / el.as_secs_f64(),
                    g.1,
                    el.as_secs_f64()
                );
                *g = (std::time::Instant::now(), 0);
            }
        }
    }

    fn dump_surface_frame(renderer: &GpuRenderer, [w, h]: [u32; 2], texture: &wgpu::Texture) {
        // One-shot ground-truth readback for capture debugging:
        // MEGU_FRAME_DUMP=/path.ppm copies the presented surface into
        // a CPU buffer on present #30 and writes a binary PPM.
        if let Ok(path) = std::env::var("MEGU_FRAME_DUMP") {
            static DUMPED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            static PN0: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let pn = PN0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if pn >= 30 && !DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let dev = &renderer.gpu().device;
                let queue = &renderer.gpu().queue;
                let row = w * 4;
                let padded = row.div_ceil(256) * 256;
                let buf = dev.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("frame-dump"),
                    size: (padded * h) as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("frame-dump"),
                });
                enc.copy_texture_to_buffer(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::TexelCopyBufferInfo {
                        buffer: &buf,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(padded),
                            rows_per_image: Some(h),
                        },
                    },
                    wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                );
                queue.submit([enc.finish()]);
                let slice = buf.slice(..);
                let (tx2, rx2) = std::sync::mpsc::channel();
                slice.map_async(wgpu::MapMode::Read, move |r| {
                    let _ = tx2.send(r);
                });
                let _ = dev.poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(std::time::Duration::from_secs(10)),
                });
                if let Ok(Ok(())) = rx2.recv() {
                    match slice.get_mapped_range() {
                        Ok(data) => {
                            let mut img = format!("P6\n{} {}\n255\n", w, h).into_bytes();
                            for y in 0..h as usize {
                                let start = y * padded as usize;
                                img.extend_from_slice(&data[start..start + row as usize]);
                            }
                            drop(data);
                            buf.unmap();
                            let _ = std::fs::write(&path, &img);
                            eprintln!("[dump] wrote {} ({} bytes)", path, img.len());
                        }
                        Err(err) => {
                            buf.unmap();
                            eprintln!("[dump] mapped frame buffer was unavailable: {err}");
                        }
                    }
                }
            }
        }
    }

    fn save_pending_screenshot(&mut self, [w, h]: [u32; 2], texture: &wgpu::Texture) {
        let active = self
            .active
            .as_mut()
            .expect("visible frame has an active window");
        if self.screenshot_pending {
            self.screenshot_pending = false;
            let game_dir = self
                .config
                .game_dir
                .as_deref()
                .unwrap_or(std::path::Path::new("."));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut sequence = self.screenshot_sequence;
            let mut path =
                crate::renderer::screenshot_output_path(game_dir, now.saturating_add(sequence));
            while path.exists() && sequence < u64::MAX - now {
                sequence += 1;
                path =
                    crate::renderer::screenshot_output_path(game_dir, now.saturating_add(sequence));
            }
            self.screenshot_sequence = sequence.saturating_add(1);
            let result = (|| {
                if !active.surface_copy_src_supported {
                    return Err("surface does not support screenshot readback".to_owned());
                }
                let bgra = matches!(
                    active.surface_format,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
                );
                if !bgra
                    && !matches!(
                        active.surface_format,
                        wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb
                    )
                {
                    return Err(format!(
                        "unsupported screenshot surface format {:?}",
                        active.surface_format
                    ));
                }
                let mut rgba = active.renderer.read_texture_rgba(texture, w, h)?;
                if bgra {
                    for pixel in rgba.as_chunks_mut::<4>().0 {
                        pixel.swap(0, 2);
                    }
                }
                crate::renderer::spawn_rgba_png_write(path, w, h, rgba)
            })();
            if let Err(error) = result {
                eprintln!("[screenshot] failed: {error}");
            }
        }
    }

    fn observe_frame_budget(&mut self, frame_begin: Instant) {
        let stage = self.governor.observe_frame(frame_begin.elapsed());
        // Live P95 feedback edge: the session's real measured
        // frame-time window feeds the renderer's adaptive upload-budget
        // controller — `upload_budget_p95_observations` counts each
        // observation so the loop is provably live, not dead code. The
        // Jain–Chlamtac P² streaming P95 is the primary source once its
        // five markers bootstrap; the ring-window P95 covers startup.
        if let (Some(live), Some(active)) = (&self.live, &mut self.active) {
            let p95_ms = live
                .p95_frame_ms()
                .unwrap_or_else(|| live.frame_timings.frame_ms_p95());
            if p95_ms.is_finite() && p95_ms > 0.0 {
                active.renderer.observe_frame_p95_ms(p95_ms);
            }
        }
        self.apply_governor_stage(stage);
    }

    /// Live OOM guard driven by the `ram_budget_mib` setting: resample
    /// host memory at ~2 Hz and, when available RAM drops under the
    /// reserve floor (or the process outgrows its budget), shed distant
    /// chunk columns early — RAM-only eviction, chunks re-stream on
    /// demand — so the kernel OOM killer never picks Megu even when the
    /// user slides the cap to the host total.
    fn poll_host_ram_guard(&mut self, dt_ms: f32) {
        self.host_ram_sample_accum_ms += dt_ms.max(0.0);
        if self.host_ram_sample_accum_ms < HOST_RAM_GUARD_INTERVAL_MS {
            return;
        }
        self.host_ram_sample_accum_ms = 0.0;
        let snap = sample_host_metrics(&mut self.host_ram_probe);
        let budget_mib = self.ui.settings.video.ram_budget_mib;
        let host_total_mib = self.ui.settings.video.host_total_ram_mib;
        let pressure = host_ram_pressure(&snap, budget_mib, host_total_mib);
        if pressure == HostRamPressure::Clear {
            return;
        }
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let center_x = (self.camera.position[0] / 16.0).floor() as i32;
        let center_z = (self.camera.position[2] / 16.0).floor() as i32;
        let view = i32::from(live.client_view_distance.clamp(6, 32));
        // Normal pressure sheds only the `>view_distance+2` fringe (the
        // same hysteresis band vanilla keeps); emergency pressure pulls
        // the keep radius in a few chunks so the nearest surround stays.
        let effective_view = if pressure == HostRamPressure::Emergency {
            view.saturating_sub(4).max(2)
        } else {
            view
        };
        live.world
            .evict_far_chunks(center_x, center_z, effective_view);
        if pressure == HostRamPressure::Emergency {
            // CPU-side mesh copies VRAM already owns are the cheapest
            // RAM to give back under real duress.
            if let Some(active) = self.active.as_mut() {
                live.release_uploaded_cpu_copies(&mut active.renderer);
            }
        }
    }

    /// Apply the governor's throttle stage live: `Full` restores the
    /// baseline upload budget, user shadow quality and user render
    /// scale; `TrimRebuilds` caps uploads at one section / 32 KiB per
    /// frame and clamps the dirty rebuild allowance; `TrimLod` adds
    /// shadow shutdown; `MinBudget` drops the 3D viewport to the 50%
    /// rung of the render-scale ladder (UI still renders at native).
    fn apply_governor_stage(&mut self, stage: ThrottleStage) {
        if stage == ThrottleStage::Full {
            if let Some(active) = &mut self.active {
                if let Some(budget) = self.baseline_upload_budget.clone() {
                    active.renderer.set_upload_budget(budget);
                }
                active.renderer.set_shadow_quality(self.user_shadow_quality);
                active.renderer.set_render_scale(self.user_render_scale);
            }
            if let Some(live) = &mut self.live {
                live.governor_rebuild_cap = usize::MAX;
            }
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        active.renderer.set_upload_budget(
            ChunkUploadBudget::with_limits(1, 32 * 1024, 150).with_target_frame_us(1_000),
        );
        if let Some(live) = &mut self.live {
            live.governor_rebuild_cap = 1;
        }
        if stage == ThrottleStage::TrimRebuilds {
            return;
        }
        active
            .renderer
            .set_shadow_quality(BuiltinShadowQuality::Off);
        if stage == ThrottleStage::TrimLod {
            return;
        }
        active
            .renderer
            .set_render_scale(RenderScalePreset::UltraPerf50);
    }

    /// `F3 + P` (`pauseOnLostFocus`): pause a singleplayer game when the
    /// window loses focus. Multiplayer stays unpaused — the server keeps
    /// ticking — and an already-paused game is left alone.
    fn pause_for_lost_focus(&mut self) {
        if !self.input_router.debug_chords.pause_on_lost_focus
            || self.paused
            || self.singleplayer_server.is_none()
            || self.ui.screen != Screen::InGameHud
        {
            return;
        }
        self.paused = true;
        self.camera_controller.set_zoomed(false);
        if let Some(event) = self.input_router.release_held_keys() {
            self.apply_window_input(event);
        }
        self.ui.navigate(Screen::Pause);
    }

    fn update_gui_key(&mut self, code: KeyCode, state: ElementState) {
        let mask = match code {
            KeyCode::ShiftLeft => 1,
            KeyCode::ShiftRight => 2,
            KeyCode::ControlLeft => 4,
            KeyCode::ControlRight => 8,
            _ => 0,
        };
        if state == ElementState::Pressed {
            self.gui_modifiers |= mask;
        } else {
            self.gui_modifiers &= !mask;
        }
    }

    fn handle_key(&mut self, code: KeyCode, state: ElementState, event_loop: &ActiveEventLoop) {
        let pressed = state == ElementState::Pressed;
        if pressed && code == KeyCode::Escape && self.ui.screen == Screen::ModSyncPrompt {
            self.dispatch_ui_action(&UiAction::CancelModSync);
            return;
        }
        if self.ui.claims_ui.is_some() {
            if pressed {
                if let Some(key) = ui_key(code) {
                    for action in self.ui.handle_input(UiInputEvent::KeyPressed(key)) {
                        self.dispatch_ui_action(&action);
                    }
                }
            }
            return;
        }
        if self.shader_options_open {
            if !pressed {
                return;
            }
            use megu_ui::shader_screen::ShaderScreenKey as K;
            let key = match code {
                KeyCode::ArrowUp => Some(K::Up),
                KeyCode::ArrowDown => Some(K::Down),
                KeyCode::ArrowLeft => Some(K::Left),
                KeyCode::ArrowRight => Some(K::Right),
                KeyCode::Enter => Some(K::Enter),
                KeyCode::Escape => Some(K::Escape),
                KeyCode::KeyR => Some(K::Reset),
                KeyCode::KeyA => Some(K::Apply),
                _ => None,
            };
            if let Some(key) = key {
                let selection = self.ui.shader_packs.handle_key(key);
                if let Some(selection) = selection {
                    self.apply_shader_screen_selection(selection);
                }
                if code == KeyCode::Escape && self.ui.shader_packs.active_screen().is_none() {
                    self.shader_options_open = false;
                    self.maybe_regrab();
                }
            }
            return;
        }
        if let Some(index) = self.ui.capturing_key_binding {
            if pressed {
                if code == KeyCode::Escape {
                    self.ui.capturing_key_binding = None;
                } else if self.ui.settings.keys.set(index, format!("{code:?}")) {
                    self.ui.capturing_key_binding = None;
                    self.dispatch_ui_action(&UiAction::SettingsChanged);
                }
            }
            return;
        }
        if pressed && code == KeyCode::F8 {
            self.save_instant_replay();
            return;
        }
        let physical_code = code;
        let code = remap_bound_key(code, &self.ui.settings.keys);
        if let Some(book) = self.book_editor.as_mut() {
            if pressed {
                use megu_ui::gameplay_screens::BookEditorClick;
                match code {
                    KeyCode::Escape => {
                        let action = book.close();
                        self.dispatch_book_action(action);
                    }
                    KeyCode::ArrowLeft => {
                        book.click(BookEditorClick::Previous);
                    }
                    KeyCode::ArrowRight => {
                        book.click(BookEditorClick::Next);
                    }
                    KeyCode::Backspace => book.backspace(),
                    KeyCode::Enter if book.title_entry => {
                        let action = book.submit_title();
                        self.dispatch_book_action(action);
                    }
                    KeyCode::Enter => book.newline(),
                    _ => {}
                }
            }
            return;
        }
        if pressed && code == KeyCode::Escape && self.credits_screen.is_some() {
            self.escape_end_credits_screen();
            return;
        }
        if pressed && code == KeyCode::Escape && self.creative_inventory.is_some() {
            self.dispatch_creative_action(megu_ui::creative_screen::CreativeInventoryAction::Close);
            return;
        }
        if pressed && code == KeyCode::Escape && self.live_is_sleeping() {
            self.leave_bed();
            return;
        }
        if self.text_input_active()
            && !matches!(
                code,
                KeyCode::Escape
                    | KeyCode::Backspace
                    | KeyCode::Enter
                    | KeyCode::ArrowLeft
                    | KeyCode::ArrowRight
            )
        {
            return;
        }
        if pressed
            && code == KeyCode::KeyE
            && inventory_key_matches(physical_code, &self.ui.settings.keys)
            && self.live.as_ref().is_some_and(|live| live.game_mode == 1)
            && self.container_screen.is_none()
            && self.chat_bar.is_none()
            && self.book_editor.is_none()
            && !self.text_input_active()
        {
            if self.creative_inventory.is_some() {
                self.creative_inventory = None;
                self.maybe_regrab();
            } else {
                self.open_creative_inventory();
            }
            return;
        }
        if pressed
            && !self.text_input_active()
            && !self
                .container_screen
                .as_ref()
                .is_some_and(|screen| screen.rename_focused)
            && !self.container_suspended_for_recipe
        {
            let container_hotkey = match code {
                KeyCode::Digit1 => Some(ContainerSlotHotkey::HotbarSwap(0)),
                KeyCode::Digit2 => Some(ContainerSlotHotkey::HotbarSwap(1)),
                KeyCode::Digit3 => Some(ContainerSlotHotkey::HotbarSwap(2)),
                KeyCode::Digit4 => Some(ContainerSlotHotkey::HotbarSwap(3)),
                KeyCode::Digit5 => Some(ContainerSlotHotkey::HotbarSwap(4)),
                KeyCode::Digit6 => Some(ContainerSlotHotkey::HotbarSwap(5)),
                KeyCode::Digit7 => Some(ContainerSlotHotkey::HotbarSwap(6)),
                KeyCode::Digit8 => Some(ContainerSlotHotkey::HotbarSwap(7)),
                KeyCode::Digit9 => Some(ContainerSlotHotkey::HotbarSwap(8)),
                KeyCode::KeyF => Some(ContainerSlotHotkey::OffhandSwap),
                KeyCode::KeyQ if self.gui_modifiers & 12 != 0 => {
                    Some(ContainerSlotHotkey::DropStack)
                }
                KeyCode::KeyQ => Some(ContainerSlotHotkey::DropOne),
                _ => None,
            };
            if let (Some(key), Some(screen)) = (container_hotkey, self.container_screen.as_mut()) {
                if let Some(action) = screen.handle_slot_hotkey(key) {
                    if let Some(live) = &mut self.live {
                        Self::forward_click_action(live, action);
                    }
                }
                return;
            }
        }
        if self.has_gameplay_modal()
            && self.ui.screen == Screen::InGameHud
            && code != KeyCode::Escape
        {
            return;
        }
        let f3_chord_pressed =
            pressed && self.input_router.f3_held && f3_chord_for_key(code).is_some();
        // OS clipboard chords on text surfaces — before the input
        // router so `Ctrl`/`Cmd`+`V`/`C` reaches the focused field
        // instead of gameplay bindings (zoom, drop-stack, debug chords).
        if self.clipboard_chord(code, state) {
            match code {
                KeyCode::KeyV => {
                    for action in self.clipboard_paste_actions() {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
                _ => self.clipboard_copy(),
            }
            return;
        }
        if pressed
            && code == KeyCode::Backspace
            && self
                .container_screen
                .as_ref()
                .is_some_and(|screen| screen.rename_focused)
        {
            let action = self
                .container_screen
                .as_mut()
                .and_then(ContainerScreenState::handle_rename_backspace);
            if let Some(action) = action {
                self.dispatch_workstation_action(action);
            }
            return;
        }
        if pressed
            && code == KeyCode::Backspace
            && self
                .creative_inventory
                .as_ref()
                .is_some_and(|state| state.tab == megu_ui::creative_screen::CreativeTab::Search)
        {
            if let Some(creative) = &mut self.creative_inventory {
                let mut search = creative.search.clone();
                search.pop();
                creative.set_search(search);
            }
            return;
        }
        // recipe overlay side overlay hotkeys (`Ctrl+O` toggle, `A` bookmark,
        // `R`/`U` detail, `Backspace`/`Esc` history) — the whole chord
        // lives in `handle_overlay_key` so headless tests drive the same
        // code path without `ActiveEventLoop`.
        if self.handle_overlay_key(code, pressed) {
            return;
        }
        // recipe browser hover hotkeys while a container/inventory surface is up:
        // `R` shows recipes, `U` uses, `W` the MachineTutorial scene — but only when
        // a resolvable stack is under the cursor. Otherwise the key keeps
        // its usual binding (`R` sorts, `W` moves forward).
        if pressed
            && !self.paused
            && self.container_screen.is_some()
            && !self.container_suspended_for_recipe
            && matches!(code, KeyCode::KeyR | KeyCode::KeyU | KeyCode::KeyW)
            && self.overlay_focused_item().is_some()
        {
            let focus = match code {
                KeyCode::KeyR => RecipeFocus::Recipes,
                KeyCode::KeyU => RecipeFocus::Uses,
                _ => RecipeFocus::MachineTutorial,
            };
            self.apply_window_input(WindowInputEvent::RecipeBrowserFocus(focus));
            return;
        }
        let player = self.live.as_ref().and_then(|live| {
            let pose = live.pose?;
            let gamemode = match live.game_mode {
                0 => DebugGameMode::Survival,
                1 => DebugGameMode::Creative,
                2 => DebugGameMode::Adventure,
                3 => DebugGameMode::Spectator,
                _ => return None,
            };
            Some(PlayerDebugInfo {
                dimension: &live.world.dimension.name,
                position: pose.feet,
                yaw_deg: f64::from(pose.yaw),
                pitch_deg: f64::from(pose.pitch),
                gamemode,
            })
        });
        // `F3 + I` copies the crosshair target: the traced block's
        // `/setblock` command or the entity's `/summon` command. Entities
        // resolve through the rig registry name; registry gaps keep a
        // deterministic placeholder like the item path does.
        let picked = self.live.as_ref().and_then(LiveSession::crosshair_pick);
        let entity_target_name = match &picked {
            Some(CrosshairPick::Entity(hit)) => self
                .live
                .as_ref()
                .map(|live| debug_entity_target_name(live, hit.entity_id)),
            _ => None,
        };
        let target = match (&picked, entity_target_name.as_deref()) {
            (Some(CrosshairPick::Block(hit)), _) => self.live.as_ref().and_then(|live| {
                let (position, block_state) = live.looking_at_block.as_ref()?;
                (*position == hit.block_pos).then_some(DebugTarget::Block {
                    position: *position,
                    block_state,
                    nbt: None,
                })
            }),
            (Some(CrosshairPick::Entity(hit)), Some(entity_id)) => {
                self.live.as_ref().and_then(|live| {
                    let tracked = live.world.entities.get(&hit.entity_id)?;
                    Some(DebugTarget::Entity {
                        entity_id,
                        position: tracked.pos,
                        nbt: None,
                    })
                })
            }
            _ => None,
        };
        let permitted = self.live.as_ref().is_some_and(|live| live.permission_level >= 2);
        for event in self.input_router.route_key_with_context(
            code, state, self.keys.sprint, player, target, permitted,
        ) {
            if !self.paused
                || matches!(
                    event,
                    WindowInputEvent::DebugChord(_)
                        | WindowInputEvent::SetDebugOverlayVisible(_)
                        | WindowInputEvent::SetTabListVisible(false)
                )
            {
                self.apply_window_input(event);
            }
        }
        if code == KeyCode::F3 || f3_chord_pressed {
            return;
        }
        match code {
            KeyCode::KeyW => self.keys.forward = pressed,
            KeyCode::KeyS => self.keys.back = pressed,
            KeyCode::KeyA => self.keys.left = pressed,
            KeyCode::KeyD => self.keys.right = pressed,
            KeyCode::Space => {
                self.keys.up = pressed;
                if pressed && !self.paused {
                    if let Some(live) = &mut self.live {
                        // Vanilla creative double-jump: a second Space
                        // press inside ~7 ticks toggles flight instead of
                        // re-jumping/elytra. `toggle_flight` no-ops
                        // without the server's `may_fly` grant, so normal
                        // jump behavior is preserved outside creative.
                        let double_tap = self
                            .last_space_press
                            .is_some_and(|t| t.elapsed() <= Duration::from_millis(350));
                        self.last_space_press = Some(Instant::now());
                        if !(double_tap && live.toggle_flight()) {
                            live.try_start_elytra();
                        }
                    }
                }
            }
            KeyCode::ShiftLeft | KeyCode::ShiftRight => {
                // Sneak transitions ride `player_command` (0 start /
                // 1 stop) — emitted once per edge, not per frame.
                if pressed != self.keys.down {
                    self.keys.down = pressed;
                    if !self.paused {
                        if let Some(live) = &mut self.live {
                            live.player_command(if pressed { 0 } else { 1 });
                        }
                    }
                }
            }
            KeyCode::ControlLeft | KeyCode::ControlRight => {
                // Sprint transitions ride `player_command` (3 start /
                // 4 stop) — emitted once per edge.
                if pressed != self.keys.sprint {
                    self.keys.sprint = pressed;
                    if !self.paused {
                        if let Some(live) = &mut self.live {
                            live.player_command(if pressed { 3 } else { 4 });
                        }
                    }
                }
            }
            KeyCode::KeyC => self.camera_controller.set_zoomed(pressed && !self.paused),
            KeyCode::F1 if pressed && !self.paused => {
                self.ui.hud.hud_hidden = !self.ui.hud.hud_hidden;
            }
            KeyCode::F2 if pressed => self.screenshot_pending = true,
            KeyCode::F11 if pressed => {
                toggle_fullscreen_setting(&mut self.ui.settings);
                self.dispatch_ui_action(&UiAction::SettingsChanged);
            }
            KeyCode::F5 if pressed && !self.paused => {
                self.camera_controller.cycle_mode();
            }
            KeyCode::Escape if pressed => {
                // Vanilla close order: chat (handled before this point) ->
                // container/inventory screen -> advancements overlay ->
                // pause menu -> UI back-navigation on non-HUD screens.
                if self.close_container_surface() {
                    return;
                }
                if self.advancements_open {
                    self.toggle_advancements();
                    return;
                }
                if self.ui.screen == Screen::InGameHud || self.paused {
                    self.paused = !self.paused;
                    if self.paused {
                        self.camera_controller.set_zoomed(false);
                        if let Some(event) = self.input_router.release_held_keys() {
                            self.apply_window_input(event);
                        }
                        self.ui.navigate(Screen::Pause);
                    } else {
                        for action in self
                            .ui
                            .handle_input(UiInputEvent::KeyPressed(UiKey::Escape))
                        {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                    self.maybe_regrab();
                } else {
                    for action in self
                        .ui
                        .handle_input(UiInputEvent::KeyPressed(UiKey::Escape))
                    {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
            }
            _ => {
                if pressed && self.ui_wants_input() {
                    if let Some(key) = ui_key(code) {
                        let actions = self.ui.handle_input(UiInputEvent::KeyPressed(key));
                        for action in actions {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                }
            }
        }
    }

    fn apply_window_input(&mut self, event: WindowInputEvent) {
        match event {
            WindowInputEvent::SetDebugOverlayVisible(visible) => {
                self.show_diagnostics = visible;
            }
            WindowInputEvent::DebugChord(action) => {
                self.show_diagnostics = self.input_router.debug_chords.show_debug_overlay;
                match action {
                    DebugChordAction::ChatFeedback(message) => {
                        if let Some(live) = &mut self.live {
                            live.push_chat_line(message.clone());
                        } else {
                            self.ui.hud.chat_lines.push(message.clone());
                        }
                        self.debug_messages.push(message);
                    }
                    DebugChordAction::HelpLines(lines) => {
                        if let Some(live) = &mut self.live {
                            for line in &lines {
                                live.push_chat_line(line.clone());
                            }
                        } else {
                            self.ui.hud.chat_lines.extend(lines.iter().cloned());
                        }
                        self.debug_messages.extend(lines);
                    }
                    DebugChordAction::ClearChat => {
                        self.debug_messages.clear();
                        self.ui.hud.chat_lines.clear();
                        if let Some(live) = &mut self.live {
                            live.clear_chat_log();
                        }
                    }
                    DebugChordAction::ClipboardCommand(command) => {
                        self.clipboard.set_text(&command);
                    }
                    DebugChordAction::SendGameModeCommand(mode) => {
                        if let Some(live) = &mut self.live {
                            live.send_chat(&format!("/gamemode {}", mode.command_name()));
                        }
                    }
                    DebugChordAction::ReloadChunks => {
                        if let Some(live) = &mut self.live {
                            live.rebuild_loaded_sections_for_lighting(
                                self.camera.position,
                                self.ui.settings.video.view_distance,
                            );
                        }
                    }
                    DebugChordAction::ReloadResourcePacks => {
                        // Vanilla `F3 + T` (`reloadResourcePacks`): re-scan the
                        // JAR/assets/resource-pack layers and re-upload the
                        // block/GUI atlases without touching the window,
                        // device, or the live connection.
                        let scheduled = self.reload_debug_assets();
                        self.debug_messages.push(
                            if scheduled {
                                "Resource reload scheduled"
                            } else {
                                "Resource reload unavailable"
                            }
                            .to_owned(),
                        );
                    }
                }
                if self.debug_messages.len() > 100 {
                    let excess = self.debug_messages.len() - 100;
                    self.debug_messages.drain(..excess);
                }
                let chat_limit = self.ui.settings.qol.chat_history_lines.clamp(100, 5000);
                if self.ui.hud.chat_lines.len() > chat_limit {
                    let excess = self.ui.hud.chat_lines.len() - chat_limit;
                    self.ui.hud.chat_lines.drain(..excess);
                }
            }
            WindowInputEvent::SelectHotbar(slot) => {
                self.ui.hud.selected_slot = usize::from(slot);
                if let Some(live) = &mut self.live {
                    live.hud.selected_slot = usize::from(slot);
                    live.sync_hud_map();
                    live.hud_revision += 1;
                    live.set_carried_item(i32::from(slot));
                }
            }
            WindowInputEvent::SetTabListVisible(visible) => {
                self.ui.hud.show_player_list = visible;
                if let Some(live) = &mut self.live {
                    live.hud.show_player_list = visible;
                    live.hud_revision += 1;
                }
            }
            WindowInputEvent::SwapOffhand => {
                if let Some(live) = &mut self.live {
                    live.swap_offhand();
                }
            }
            WindowInputEvent::DropItem { entire_stack } => {
                if let Some(live) = &mut self.live {
                    live.drop_item(entire_stack);
                }
            }
            WindowInputEvent::OpenChat { slash_prefill } => {
                // Vanilla `T`/`/` open the chat/command bar; the pointer is
                // released for text entry like a server screen.
                self.close_container_surface();
                let mut bar = ZshCommandBar::new();
                if slash_prefill {
                    bar.push_char('/');
                }
                self.chat_bar = Some(bar);
                self.clear_gameplay_controls();
                self.refresh_chat_suggestions();
                self.set_cursor_grab(false);
            }
            WindowInputEvent::SortInventory => {
                // InventoryTweaks-style QoL sort: hovered slot picks its
                // region (container vs player inventory), otherwise the
                // schema default applies. Local-only — the next server
                // `container_set_content` refresh owns the truth.
                if self.live.is_some() && self.container_screen.is_some() {
                    self.status_line("Sort unavailable: server inventory is authoritative");
                } else if self.inventory_sort.enabled {
                    if let Some(screen) = &mut self.container_screen {
                        let range = match screen.hovered_slot {
                            Some(index) => self.inventory_sort.slot_range_for(screen, index),
                            None => self.inventory_sort.default_sort_range(screen),
                        };
                        self.inventory_sort.sort_container_range(screen, range);
                    }
                }
            }
            WindowInputEvent::OpenInventory => {
                if self.own_inventory_open {
                    self.close_own_inventory();
                } else if !self.close_container_surface() {
                    self.open_own_inventory();
                }
            }
            WindowInputEvent::OpenAdvancements => {
                self.toggle_advancements();
            }
            WindowInputEvent::RecipeBrowserFocus(focus) => {
                // Re-verify the hovered stack here — a stale event can
                // never open a detail view for an item that is not under
                // the cursor (the key path checks before emitting too).
                if let Some(item) = self.overlay_focused_item() {
                    self.open_recipe_browser_focused(&item, focus);
                }
            }
        }
        // `OpenChat` (and any future input event that opens a text
        // surface) toggles IME allow state — keep the headless
        // `dispatch_input` path consistent with the windowed one.
        self.update_ime();
    }

    fn apply_ui_action(&mut self, action: &UiAction, event_loop: &ActiveEventLoop) {
        match action {
            UiAction::QuitGame => event_loop.exit(),
            other => self.dispatch_ui_action(other),
        }
    }

    fn reconfigure(&mut self, size: PhysicalSize<u32>) {
        let Some(active) = &mut self.active else {
            return;
        };
        if size.width == 0 || size.height == 0 {
            return;
        }
        active.size = size;
        let caps = active
            .surface
            .get_capabilities(&active.renderer.gpu().adapter);
        let cfg = surface_config_for(
            self.render_config.vsync,
            self.render_config.effective_fps_cap(self.focused),
            &self.policy,
            active.surface_format,
            active.size,
            &caps,
        );
        active
            .surface
            .configure(&active.renderer.gpu().device, &cfg);
    }
}

pub fn sign_face_lines(nbt: &NbtValue, is_front: bool) -> Option<[String; 4]> {
    fn flatten(value: &megu_core::json::JsonValue, out: &mut String) {
        use megu_core::json::JsonValue;
        match value {
            JsonValue::String(text) => out.push_str(text),
            JsonValue::Array(values) => {
                for value in values {
                    flatten(value, out);
                }
            }
            JsonValue::Object(fields) => {
                if let Some((_, JsonValue::String(text))) =
                    fields.iter().find(|(key, _)| key == "text")
                {
                    out.push_str(text);
                }
                if let Some((_, extra)) = fields.iter().find(|(key, _)| key == "extra") {
                    flatten(extra, out);
                }
            }
            _ => {}
        }
    }
    let NbtValue::Compound(root) = nbt else {
        return None;
    };
    let face_name = if is_front { "front_text" } else { "back_text" };
    let NbtValue::Compound(face) = root.get(face_name)? else {
        return None;
    };
    let NbtValue::List(messages) = face.get("messages")? else {
        return None;
    };
    let mut lines = [String::new(), String::new(), String::new(), String::new()];
    for (line, message) in lines.iter_mut().zip(messages.iter()) {
        let NbtValue::String(json) = message else {
            continue;
        };
        if let Ok(value) = megu_core::json::parse_json(json) {
            flatten(&value, line);
        } else {
            *line = json.clone();
        }
    }
    Some(lines)
}

#[derive(Clone, Copy)]
struct LiveHotbarOverlayMeta {
    count: i32,
    damage: Option<i32>,
    max_damage: Option<i32>,
}

#[derive(Clone, Copy)]
struct LiveBossBarStyle {
    color: i32,
    divisions: i32,
}

type LiveHudMetadata = (
    [Option<LiveHotbarOverlayMeta>; 9],
    Option<LiveHotbarOverlayMeta>,
    [Option<LiveBossBarStyle>; 3],
);

fn live_bossbar_fill(color: i32) -> UiColor {
    match color {
        0 => megu_ui::draw::palette::HEALTH,
        1 => megu_ui::draw::palette::AIR,
        2 => megu_ui::draw::palette::DANGER,
        3 => megu_ui::draw::palette::XP,
        4 => megu_ui::draw::palette::WARNING,
        5 => megu_ui::draw::palette::ACCENT,
        6 => megu_ui::draw::palette::TEXT,
        _ => UiColor::rgb(170, 60, 200),
    }
}

fn bossbar_segments(divisions: i32) -> i32 {
    match divisions {
        1 => 6,
        2 => 10,
        3 => 12,
        4 => 20,
        _ => 1,
    }
}

/// Apply the typed bossbar metadata that the legacy `(title, fraction)`
/// `HudState` cannot carry. `client-ui` owns the base layout; this pass
/// recolors the exact progress commands it emitted and adds the protocol's
/// notched overlay ticks without changing hit layout or occupancy.
fn apply_live_bossbar_metadata(
    width: u32,
    bars: &[Option<LiveBossBarStyle>; 3],
    out: &mut UiDrawList,
) {
    let width = width as i32;
    let cx = width / 2;
    let bw = (width - 40).clamp(8, 200);
    for (index, bar) in bars.iter().enumerate() {
        let Some(bar) = bar else {
            continue;
        };
        let rect = UiRect::new(cx - bw / 2, 6 + index as i32 * 16 + 9, bw as u32, 5);
        let mut painted = false;
        for command in &mut out.commands {
            if let UiCommand::ProgressBar {
                rect: command_rect,
                fill,
                ..
            } = command
            {
                if *command_rect == rect {
                    *fill = live_bossbar_fill(bar.color);
                    painted = true;
                    break;
                }
            }
        }
        if !painted {
            continue;
        }
        let segments = bossbar_segments(bar.divisions);
        for notch in 1..segments {
            let x = rect.x + (bw * notch).div_euclid(segments);
            out.fill_rect(
                UiRect::new(x, rect.y, 1, rect.h),
                megu_ui::draw::palette::SLOT_INNER_DARK,
            );
        }
    }
}

fn durability_color(fraction: f32) -> UiColor {
    let fraction = fraction.clamp(0.0, 1.0);
    let red = (255.0 * (1.0 - fraction)).round() as u8;
    let green = (255.0 * fraction).round() as u8;
    UiColor::rgb(red, green, 32)
}

fn overlay_hotbar_stack_metadata(
    x: i32,
    y: i32,
    stack: &LiveHotbarOverlayMeta,
    scale: u32,
    out: &mut UiDrawList,
) {
    let s = scale.max(1) as i32;
    if stack.count > 1 {
        let mut digits = [0u8; 10];
        let mut value = stack.count as u32;
        let mut start = digits.len();
        while value > 0 {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
        }
        let count = std::str::from_utf8(&digits[start..]).unwrap_or("1");
        let tx = x + 20 * s - text_width(count, scale.max(1)) as i32;
        out.text_str(
            tx + s,
            y + 13 * s,
            scale.max(1),
            megu_ui::draw::palette::TEXT_SHADOW,
            count,
        );
        out.text_str(tx, y + 12 * s, scale.max(1), megu_ui::draw::palette::TEXT, count);
    }
    let (Some(damage), Some(max_damage)) = (stack.damage, stack.max_damage) else {
        return;
    };
    if damage <= 0 || max_damage <= 0 {
        return;
    }
    let remaining_ticks = (i64::from(max_damage) - i64::from(damage)).max(0);
    let remaining = (remaining_ticks as f32 / max_damage as f32).clamp(0.0, 1.0);
    let bar_w = 13;
    let fill_w = ((bar_w as f32) * remaining).round().max(1.0) as u32;
    out.fill_rect(
        UiRect::new(x + 4 * s, y + 18 * s, bar_w * scale.max(1), scale.max(1)),
        UiColor::rgb(20, 20, 20),
    );
    out.fill_rect(
        UiRect::new(x + 4 * s, y + 18 * s, fill_w * scale.max(1), scale.max(1)),
        durability_color(remaining),
    );
}

/// Composite the component-preserving stack metadata the legacy hotbar
/// strings cannot represent: stack counts and damage bars use the same
/// window-0 slot geometry as `client-ui`'s hotbar, while the base renderer
/// keeps ownership of slot frames, icons, selection and cooldown wipes.
fn apply_live_hotbar_metadata(
    width: u32,
    height: u32,
    scale: u32,
    hotbar: &[Option<LiveHotbarOverlayMeta>; 9],
    offhand: Option<&LiveHotbarOverlayMeta>,
    game_mode: i32,
    out: &mut UiDrawList,
) {
    if game_mode == 3 {
        return;
    }
    if let Some(stack) = offhand {
        let rect = hotbar_slot_rect(width, height, scale, -1);
        overlay_hotbar_stack_metadata(rect.x, rect.y, stack, scale, out);
    }
    for (index, stack) in hotbar.iter().enumerate() {
        if let Some(stack) = stack {
            let rect = hotbar_slot_rect(width, height, scale, index as i32);
            overlay_hotbar_stack_metadata(rect.x, rect.y, stack, scale, out);
        }
    }
}

fn component_label_from_nbt(nbt: &NbtValue) -> Option<String> {
    fn flatten_json(value: &megu_core::json::JsonValue, out: &mut String) {
        use megu_core::json::JsonValue;
        match value {
            JsonValue::String(text) => out.push_str(text),
            JsonValue::Array(values) => values.iter().for_each(|value| flatten_json(value, out)),
            JsonValue::Object(fields) => {
                if let Some((_, megu_core::json::JsonValue::String(text))) =
                    fields.iter().find(|(key, _)| key == "text")
                {
                    out.push_str(text);
                }
                if let Some((_, extra)) = fields.iter().find(|(key, _)| key == "extra") {
                    flatten_json(extra, out);
                }
            }
            _ => {}
        }
    }
    fn flatten_nbt(value: &NbtValue, out: &mut String) {
        match value {
            NbtValue::String(text) => {
                if let Ok(json) = megu_core::json::parse_json(text) {
                    flatten_json(&json, out);
                } else {
                    out.push_str(text);
                }
            }
            NbtValue::Compound(fields) => {
                if let Some(NbtValue::String(text)) = fields.get("text") {
                    out.push_str(text);
                }
                if let Some(extra) = fields.get("extra") {
                    flatten_nbt(extra, out);
                }
            }
            NbtValue::List(values) => values.iter().for_each(|value| flatten_nbt(value, out)),
            _ => {}
        }
    }
    let mut text = String::new();
    flatten_nbt(nbt, &mut text);
    (!text.is_empty()).then_some(text)
}

/// The friend-share pump. `recv_event` returns once per relay packet or
/// per `FRIEND_SHARE_TIMEOUT` read window; between windows the worker
/// drains UI commands (allowlist refresh = instant friend kicks, Stop)
/// and forwards each guest stream to a loopback TCP pipe into the
/// integrated server — the relay only ever sees AEAD records.
fn run_friend_share_worker(
    mut host: FriendHostSession,
    world_addr: SocketAddr,
    shared: Arc<Mutex<FriendShareShared>>,
    commands: mpsc::Receiver<FriendShareCommand>,
) {
    let mut pipes: BTreeMap<u32, TcpStream> = BTreeMap::new();
    let mut stop = false;
    while !stop {
        match host.recv_event() {
            Ok(FriendHostEvent::GuestOpened {
                stream_id,
                guest_name,
            }) => {
                let friend_id = host.guest_friend_id(stream_id);
                match TcpStream::connect_timeout(&world_addr, FRIEND_SHARE_TIMEOUT) {
                    Ok(tcp) => {
                        let _ = tcp.set_nonblocking(true);
                        pipes.insert(stream_id, tcp);
                    }
                    Err(_) => {
                        let _ = host.close_stream(stream_id, "local world unreachable");
                    }
                }
                if let Ok(mut s) = shared.lock() {
                    s.guests.insert(stream_id, (guest_name, friend_id));
                }
            }
            Ok(FriendHostEvent::GameData { stream_id, payload }) => {
                let dead = pipes
                    .get_mut(&stream_id)
                    .is_some_and(|tcp| tcp.write_all(&payload).is_err());
                if dead {
                    pipes.remove(&stream_id);
                    let _ = host.close_stream(stream_id, "local link closed");
                }
            }
            Ok(FriendHostEvent::GuestClosed { stream_id, .. }) => {
                pipes.remove(&stream_id);
                if let Ok(mut s) = shared.lock() {
                    s.guests.remove(&stream_id);
                }
            }
            // A timed-out poll keeps partial frames buffered inside the
            // link — retrying loses nothing.
            Err(err) if err.contains("timed out") => {}
            Err(err) => {
                if let Ok(mut s) = shared.lock() {
                    s.last_error = Some(err);
                }
                break;
            }
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                FriendShareCommand::ApplyAllowlist(allowlist) => {
                    if let Err(err) = host.update_allowlist(allowlist) {
                        if let Ok(mut s) = shared.lock() {
                            s.last_error = Some(err);
                        }
                    }
                }
                FriendShareCommand::Stop => stop = true,
            }
        }
        // World → guest: each pipe is nonblocking; a dead pipe closes
        // the stream so a crashed world can never wedge a guest.
        let mut dead = Vec::new();
        for (stream_id, tcp) in pipes.iter_mut() {
            let mut buf = [0u8; 8192];
            match tcp.read(&mut buf) {
                Ok(0) => dead.push(*stream_id),
                Ok(n) => {
                    if host.send_game_bytes(*stream_id, &buf[..n]).is_err() {
                        dead.push(*stream_id);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => dead.push(*stream_id),
            }
        }
        for stream_id in dead {
            pipes.remove(&stream_id);
            let _ = host.close_stream(stream_id, "local link closed");
        }
    }
    let _ = host.revoke();
    if let Ok(mut s) = shared.lock() {
        s.stopped = true;
        s.guests.clear();
    }
}

/// One status-line line's worth of `text` — char-boundary safe, so a
/// pasted `megu-friend:` link or `megu-id:` never splits mid-grapheme.
fn truncate_for_status(text: &str) -> &str {
    const LIMIT: usize = 64;
    if text.len() <= LIMIT {
        return text;
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Crockford Base32 — the same alphabet `InviteCode` and `megu-id:`
/// render with — for the 128-bit token segment of `megu-friend:` links.
const GUEST_LINK_B32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn guest_link_b32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(GUEST_LINK_B32[((acc >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(
            GUEST_LINK_B32[((acc << (5 - bits)) & 31) as usize],
        ));
    }
    out
}

fn guest_link_b32_decode(text: &str) -> Result<Vec<u8>, String> {
    let mut acc = 0u64;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    for raw in text.bytes() {
        // Homoglyph normalization matches `InviteCode::parse`.
        let digit = match raw.to_ascii_uppercase() {
            b'O' => b'0',
            b'I' | b'L' => b'1',
            d => d,
        };
        let Some(v) = GUEST_LINK_B32.iter().position(|&c| c == digit) else {
            return Err("invalid base32 character in guest link".to_owned());
        };
        acc = (acc << 5) | v as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

/// `megu-friend:megu-id:<58>:MEGU-XXXX-XXXX:<26>` — host static id +
/// invite code + 128-bit one-off token, `:`-separated so the embedded
/// `megu-id:`/`MEGU-…` hyphens can't confuse the split. Fail-closed:
/// every segment is re-validated (id checksum, code shape, token size).
fn parse_guest_link(
    link: &str,
) -> Result<([u8; 32], String, megu_proto::InviteCode, Frl3GuestToken), String> {
    let segs: Vec<&str> = link.trim().split(':').collect();
    if segs.len() != 5
        || !segs[0].eq_ignore_ascii_case("megu-friend")
        || !segs[1].eq_ignore_ascii_case("megu-id")
    {
        return Err("expected megu-friend:megu-id:<id>:<code>:<token>".to_owned());
    }
    let host_id = format!("megu-id:{}", segs[2]);
    let host_pk = parse_friend_id(&host_id).map_err(|e| format!("host id: {e}"))?;
    let code = megu_proto::InviteCode::parse(segs[3]).map_err(|e| format!("invite code: {e}"))?;
    let token = guest_link_b32_decode(segs[4])?;
    let token: [u8; 16] = token
        .try_into()
        .map_err(|_| "guest token must be 128 bits".to_owned())?;
    Ok((host_pk, host_id, code, Frl3GuestToken::from_bytes(token)))
}

/// Parse the simple `missing mods: id@version, ...` kick form used by
/// server-side dependency gates. Unknown prose stays on the ordinary
/// disconnect screen instead of being guessed into install requests.
fn parse_missing_mod_disconnect(reason: &str) -> Option<Vec<crate::mod_sync_auto::ModRequirement>> {
    let lower = reason.to_ascii_lowercase();
    let marker = ["missing required mods:", "missing mods:", "mod mismatch:"]
        .iter()
        .find_map(|marker| lower.find(marker).map(|at| (at, marker.len())))?;
    let tail = reason.get(marker.0 + marker.1..)?;
    let tail = tail.split(['\n', '\r']).next().unwrap_or(tail);
    let mut requirements = Vec::new();
    for entry in tail.split([',', ';']) {
        let entry = entry
            .trim()
            .trim_matches(|c: char| matches!(c, '.' | ')' | ']' | '}' | '"' | '\''));
        let Some((mod_id, version)) = entry.split_once('@').or_else(|| entry.split_once('='))
        else {
            continue;
        };
        let mod_id = mod_id.trim();
        let version = version
            .trim()
            .trim_matches(|c: char| matches!(c, '.' | ')' | ']' | '}' | '"' | '\''));
        if mod_id.is_empty()
            || mod_id.len() > 128
            || version.is_empty()
            || version.len() > 128
            || !mod_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
        {
            continue;
        }
        let slug = mod_id
            .rsplit(':')
            .next()
            .filter(|slug| !slug.is_empty())
            .map(str::to_owned);
        requirements.push(crate::mod_sync_auto::ModRequirement {
            mod_id: mod_id.to_owned(),
            required_version: version.to_owned(),
            sha512: None,
            sha256: None,
            modrinth_slug: slug,
        });
        if requirements.len() >= crate::mod_sync_auto::MAX_MODS {
            break;
        }
    }
    (!requirements.is_empty()).then_some(requirements)
}

/// The effective surface configuration for the current size/caps under the
/// detected display server's [`DisplayPresentationPolicy`].
fn surface_config_for(
    vsync: bool,
    fps_cap: u32,
    policy: &DisplayPresentationPolicy,
    surface_format: wgpu::TextureFormat,
    size: PhysicalSize<u32>,
    caps: &wgpu::SurfaceCapabilities,
) -> wgpu::SurfaceConfiguration {
    wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | if caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            },
        format: surface_format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: present_mode_for(vsync, &caps.present_modes),
        // Policy frame latency: `1` uncapped on Wayland/X11/Windows DX12
        // keeps `Mailbox`/`Immediate` input->photon time minimal; `2`
        // under vsync and on macOS Metal, where the extra buffered frame
        // avoids present stalls. An active FPS cap always drops to `1`
        // so the JIT pacing loop never queues a frame behind present.
        desired_maximum_frame_latency: if fps_cap > 0 {
            policy.frame_latency_capped()
        } else {
            policy.frame_latency(vsync)
        },
        // `Opaque` unlocks direct scanout + VRR wherever the policy asks
        // for it (Wayland, X11, Windows, macOS all do); otherwise honor
        // the surface's own first choice.
        alpha_mode: if policy.prefer_opaque_direct_scanout {
            preferred_alpha_mode(&caps.alpha_modes)
        } else {
            caps.alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto)
        },
        view_formats: vec![],
    }
}

/// The 40-bit value inside an [`megu_proto::InviteCode`], matching
/// `InviteCode`'s private `value()` — `as_bytes` is the code big-endian.
fn invite_code_value(code: &megu_proto::InviteCode) -> u64 {
    let b = code.as_bytes();
    u64::from_be_bytes([0, 0, 0, b[0], b[1], b[2], b[3], b[4]])
}

/// Entropy for the friend-host session secret: `/dev/urandom` where it
/// exists, with the same pid/ASLR/monotonic-jitter mixer the relay crate
/// falls back to when it does not.
fn host_secret_bytes() -> [u8; 16] {
    let mut buf = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .is_ok()
        {
            return buf;
        }
    }
    let marker = Box::new(0u8);
    let mut x = (u64::from(std::process::id()) << 32)
        ^ (&*marker as *const u8 as usize) as u64
        ^ 0x9E37_79B9_7F4A_7C15;
    let t0 = Instant::now();
    for chunk in buf.chunks_mut(8) {
        x ^= t0.elapsed().as_nanos() as u64;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
    }
    buf
}

/// `UiAction::OpenUrl` — hand the link to the platform's URL handler and
/// report spawn failures back through the status line (the UI only emits
/// configured `https?` links).
fn open_url(url: &str) -> Result<(), String> {
    let spawn = |program: &str, args: &[&str]| {
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("{program}: {e}"))
    };
    #[cfg(target_os = "macos")]
    {
        return spawn("open", &[url]);
    }
    #[cfg(target_os = "windows")]
    {
        return spawn("rundll32", &["url.dll,FileProtocolHandler", url]);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        spawn("xdg-open", &[url])
    }
}

fn remap_bound_key(code: KeyCode, bindings: &megu_ui::state::KeyBindings) -> KeyCode {
    let physical = format!("{code:?}");
    [
        (&bindings.forward, KeyCode::KeyW),
        (&bindings.back, KeyCode::KeyS),
        (&bindings.left, KeyCode::KeyA),
        (&bindings.right, KeyCode::KeyD),
        (&bindings.jump, KeyCode::Space),
        (&bindings.sneak, KeyCode::ShiftLeft),
        (&bindings.sprint, KeyCode::ControlLeft),
        (&bindings.inventory, KeyCode::KeyE),
        (&bindings.drop, KeyCode::KeyQ),
        (&bindings.swap_offhand, KeyCode::KeyF),
        (&bindings.chat, KeyCode::KeyT),
        (&bindings.command, KeyCode::Slash),
        (&bindings.zoom, KeyCode::KeyC),
        (&bindings.camera_toggle, KeyCode::F5),
        (&bindings.fullscreen, KeyCode::F11),
        (&bindings.advancements, KeyCode::KeyL),
        (&bindings.sort_inventory, KeyCode::KeyR),
    ]
    .into_iter()
    .find_map(|(binding, action)| (binding == &physical).then_some(action))
    .unwrap_or(code)
}

/// Check the physical key selected for the Inventory action without a legacy
/// hard-coded `E` fallback.
pub fn vanilla_creative_catalog() -> Vec<megu_ui::creative_screen::CreativeItem> {
    static TABLE: std::sync::OnceLock<megu_proto::ProtocolTable> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(megu_proto::ProtocolTable::v767_builtin);
    let mut items = Vec::new();
    for item_id in 0.. {
        let Some(name) = table.registry_name("minecraft:item", item_id) else {
            break;
        };
        items.push(megu_ui::creative_screen::CreativeItem {
            item_id: item_id as i32,
            name: crate::recipe_ingest::humanize_id(name),
            count: 1,
        });
    }
    items
}

pub fn inventory_key_matches(code: KeyCode, bindings: &megu_ui::state::KeyBindings) -> bool {
    bindings.inventory == format!("{code:?}")
}

fn toggle_fullscreen_setting(settings: &mut UiSettings) {
    settings.fullscreen = !settings.fullscreen;
}

fn mouse_pitch_delta(delta_y: f64, invert_y: bool) -> f64 {
    if invert_y { -delta_y } else { delta_y }
}

fn effective_camera_far(view_distance: u8, lod: LodMode) -> f32 {
    let chunks = match lod {
        LodMode::Off => i32::from(view_distance),
        LodMode::Medium => i32::from(view_distance).max(64),
        LodMode::Far => i32::from(view_distance).max(128),
    };
    chunks as f32 * 16.0
}

fn resolve_shaderpack_path(game_dir: Option<&std::path::Path>, pack: &str) -> String {
    let path = std::path::Path::new(pack);
    if path.is_absolute() {
        return path.display().to_string();
    }
    let root = game_dir.unwrap_or_else(|| std::path::Path::new("."));
    let joined = if path.components().count() == 1 {
        root.join("shaderpacks").join(path)
    } else {
        root.join(path)
    };
    joined.display().to_string()
}

fn retained_chat_on_reconnect(retain: bool, chat: &[String]) -> Vec<String> {
    if retain { chat.to_vec() } else { Vec::new() }
}

fn ui_key(code: KeyCode) -> Option<UiKey> {
    Some(match code {
        KeyCode::Tab => UiKey::Tab,
        KeyCode::ArrowUp => UiKey::Up,
        KeyCode::ArrowDown => UiKey::Down,
        KeyCode::ArrowLeft => UiKey::Left,
        KeyCode::ArrowRight => UiKey::Right,
        KeyCode::Enter => UiKey::Enter,
        KeyCode::Backspace => UiKey::Backspace,
        _ => return None,
    })
}

impl ApplicationHandler for GpuApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_none() && self.fatal.is_none() {
            if let Err(err) = self.init_gpu(event_loop) {
                self.fatal = Some(err);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.reconfigure(size),
            WindowEvent::Occluded(occluded) => self.occluded = occluded,
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Fractional HiDPI (e.g. Wayland 125%/150%): clamp into a
                // sane range and reconfigure — a `Resized` follows on
                // platforms where the physical size actually moved, and a
                // no-op reconfigure is cheap where it did not.
                self.scale_factor = scale_factor.clamp(0.5, 4.0);
                let size = self
                    .active
                    .as_ref()
                    .map(|active| active.window.inner_size());
                if let Some(size) = size {
                    self.reconfigure(size);
                }
            }
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if !focused {
                    // Tiling-WM workspace switch (Hyprland/Sway/Niri/i3):
                    // the policy forgets the grab request, the grab and
                    // hidden cursor release cleanly, and any live
                    // composition state drops so a cancelled preedit can
                    // never corrupt input on return.
                    self.tiling_grab_policy.focus_lost();
                    self.keys = Keys::default();
                    self.left_mouse_held = false;
                    self.right_mouse_held = false;
                    self.gui_modifiers = 0;
                    self.gui_left_held = false;
                    self.gui_right_held = false;
                    if let Some(live) = &mut self.live {
                        live.cancel_mining();
                        live.release_item_use();
                    }
                    self.modifiers = ModifiersState::empty();
                    if let Some(event) = self.input_router.release_held_keys() {
                        self.apply_window_input(event);
                    }
                    self.camera_controller.set_zoomed(false);
                    self.set_cursor_grab(false);
                    self.ime_state.on_disabled();
                    // Vanilla `Options.pauseOnLostFocus` (toggled by `F3 + P`):
                    // a singleplayer session opens the pause screen, while a
                    // server session keeps running. Toggled off, focus loss
                    // leaves the world exactly as it was.
                    self.pause_for_lost_focus();
                } else {
                    self.tiling_grab_policy.focus_gained();
                    // Re-arm only while the policy agrees the pointer is
                    // inside — tiling WMs can focus a window without
                    // moving the pointer into it, and grabbing then
                    // would swallow the desktop's cursor.
                    if self.tiling_grab_policy.request_grab() {
                        self.maybe_regrab();
                    }
                }
            }
            WindowEvent::CursorEntered { .. } => {
                self.tiling_grab_policy.pointer_entered();
                // Pointer re-entry onto a focused game window (tiling WM
                // mouse-follows-focus, Alt+Tab back onto the window)
                // re-arms the grab exactly like focus gain does.
                if self.tiling_grab_policy.request_grab() {
                    self.maybe_regrab();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.tiling_grab_policy.pointer_left();
                self.set_cursor_grab(false);
            }
            WindowEvent::DroppedFile(path) => {
                self.record_dropped_file(&path);
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::Ime(ime) => {
                // The composed string routes through the same text
                // surface as `Key::Character` input — a platform echo
                // of the commit as `KeyEvent.text` is swallowed
                // downstream. Preedit/cursor state is tracked in
                // `ime_state` so cancels and focus loss stay clean.
                if let Some(committed) = self.on_ime_event(ime) {
                    for action in self.committed_text_actions(&committed) {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.update_gui_key(code, event.state);
                }
                if self.ui.sign_editor.is_some() {
                    self.sign_editor_key(&event, event_loop);
                } else if self.chat_bar.is_some() {
                    self.chat_bar_key(&event, event_loop);
                } else if let PhysicalKey::Code(code) = event.physical_key {
                    if !event.repeat {
                        self.handle_key(code, event.state, event_loop);
                        // Text entry for UI screens (the recipe browser's
                        // search field and its `r`/`u`/`p`/`+` item keys
                        // all ride `CharTyped`, which winit reports via
                        // `event.text`). A payload that only echoes the
                        // `Ime::Commit` delivered moments ago must not
                        // insert twice.
                        if event.state == ElementState::Pressed
                            && (self.ui_wants_input()
                                || self.text_input_active()
                                || self.book_editor.is_some()
                                || self
                                    .container_screen
                                    .as_ref()
                                    .is_some_and(|screen| screen.rename_focused))
                        {
                            if let Some(text) = &event.text {
                                if !self.swallow_ime_echo(text) {
                                    if self
                                        .container_screen
                                        .as_ref()
                                        .is_some_and(|screen| screen.rename_focused)
                                    {
                                        for action in self.committed_text_actions(text) {
                                            self.apply_ui_action(&action, event_loop);
                                        }
                                    } else if let Some(book) = self.book_editor.as_mut() {
                                        for ch in text.chars().filter(|c| !c.is_control()) {
                                            book.type_char(ch);
                                        }
                                    } else if self.creative_inventory.is_some() {
                                        for action in self.committed_text_actions(text) {
                                            self.apply_ui_action(&action, event_loop);
                                        }
                                    } else {
                                        for ch in text.chars().filter(|c| !c.is_control()) {
                                            for action in
                                                self.ui.handle_input(UiInputEvent::CharTyped(ch))
                                            {
                                                self.apply_ui_action(&action, event_loop);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x as i32, position.y as i32);
                if self.ui_wants_input() {
                    let actions = self.ui.handle_input(UiInputEvent::MouseMove {
                        x: position.x as i32,
                        y: position.y as i32,
                    });
                    for action in actions {
                        self.apply_ui_action(&action, event_loop);
                    }
                }
                self.container_cursor_moved();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    self.gui_left_held = state == ElementState::Pressed;
                } else if button == MouseButton::Right {
                    self.gui_right_held = state == ElementState::Pressed;
                }
                if button == MouseButton::Left {
                    self.left_mouse_held = state == ElementState::Pressed;
                }
                if button == MouseButton::Right {
                    self.right_mouse_held = state == ElementState::Pressed;
                }
                if self.shader_options_open
                    && button == MouseButton::Left
                    && state == ElementState::Pressed
                {
                    let (w, h) = self
                        .active
                        .as_ref()
                        .map_or((0, 0), |a| (a.size.width, a.size.height));
                    let action = self.ui.shader_packs.pointer_action(
                        w,
                        h,
                        self.mouse_pos.0,
                        self.mouse_pos.1,
                    );
                    if let Some(action) = action {
                        let selection = self.ui.shader_packs.apply_action(action);
                        if let Some(selection) = selection {
                            self.apply_shader_screen_selection(selection);
                        }
                    }
                } else if self.ui.sign_editor.is_some()
                    && button == MouseButton::Left
                    && state == ElementState::Pressed
                {
                    let (w, h) = self
                        .active
                        .as_ref()
                        .map_or((0, 0), |a| (a.size.width, a.size.height));
                    let scale = ((h as f32 / 360.0).floor() as i32)
                        .clamp(1, 4)
                        .min(((w as f32 / 320.0).floor() as i32).max(1));
                    let done_x = (self.mouse_pos.0 - w as i32 / 2).abs() <= 28 * scale;
                    let done_y = self.mouse_pos.1 >= h as i32 / 2 + 15 * scale
                        && self.mouse_pos.1 <= h as i32 / 2 + 33 * scale;
                    if done_x && done_y {
                        self.finish_sign_editor(true);
                    }
                } else if self.book_editor.is_some()
                    && button == MouseButton::Left
                    && state == ElementState::Pressed
                {
                    let (w, h) = self
                        .active
                        .as_ref()
                        .map_or((0, 0), |a| (a.size.width, a.size.height));
                    let (x, y) = self.mouse_pos;
                    let rel_x = x - w as i32 / 2;
                    let title_entry = self
                        .book_editor
                        .as_ref()
                        .is_some_and(|book| book.title_entry);
                    if title_entry && y > h as i32 / 2 + 70 && rel_x > 24 {
                        if let Some(book) = self.book_editor.as_ref() {
                            let action = book.close();
                            self.dispatch_book_action(action);
                        }
                        self.update_ime();
                        return;
                    }
                    let click = if y > h as i32 / 2 + 70 {
                        Some(if rel_x < -45 {
                            megu_ui::gameplay_screens::BookEditorClick::Previous
                        } else if rel_x < 5 {
                            megu_ui::gameplay_screens::BookEditorClick::Next
                        } else if rel_x < 50 {
                            megu_ui::gameplay_screens::BookEditorClick::Done
                        } else {
                            megu_ui::gameplay_screens::BookEditorClick::SignAndClose
                        })
                    } else {
                        None
                    };
                    if let (Some(editor), Some(click)) = (self.book_editor.as_mut(), click) {
                        if let Some(action) = editor.click(click) {
                            self.dispatch_book_action(action);
                        }
                    }
                } else if self.creative_inventory.is_some()
                    && button == MouseButton::Left
                    && state == ElementState::Pressed
                {
                    let (x, y) = self.mouse_pos;
                    self.creative_mouse_click(x, y);
                } else if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && self.live.as_ref().is_some_and(LiveSession::is_sleeping)
                {
                    let (x, y) = self.mouse_pos;
                    let (w, h) = self
                        .active
                        .as_ref()
                        .map_or((0, 0), |a| (a.size.width, a.size.height));
                    let overlay = megu_ui::gameplay_screens::BedSleepOverlay { visible: true };
                    if overlay.click(x, y, w, h) {
                        self.leave_bed();
                    }
                } else if self.container_screen.is_some() && !self.container_suspended_for_recipe {
                    self.container_mouse_input(state, button);
                } else if self.cursor_grabbed && !self.has_gameplay_modal() {
                    // Grabbed in-game clicks: crosshair dig/place/attack
                    // dispatch into the live session.
                    self.world_mouse_input(state, button);
                } else if state == ElementState::Pressed && button == MouseButton::Left {
                    if self.ui_wants_input() {
                        let (x, y) = self.mouse_pos;
                        let actions = self.ui.handle_input(UiInputEvent::Click { x, y });
                        if !actions.is_empty() {
                            if let Some(live) = &mut self.live {
                                live.play_ui_click();
                            }
                        }
                        for action in actions {
                            self.apply_ui_action(&action, event_loop);
                        }
                    } else {
                        self.maybe_regrab();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(pos) => (pos.y / 40.0) as f32,
                };
                let steps = lines.round() as i32;
                if steps != 0 && self.chat_bar.is_some() {
                    if steps > 0 {
                        self.ui.chat_scroll.scroll_back(steps as usize);
                    } else {
                        self.ui.chat_scroll.scroll_by(steps.unsigned_abs() as usize);
                    }
                    return;
                }
                if steps != 0
                    && matches!(self.ui.screen, Screen::RecipeBrowser | Screen::ModManager)
                {
                    let key = if steps > 0 { UiKey::Up } else { UiKey::Down };
                    for _ in 0..steps.unsigned_abs().min(64) {
                        for action in self.ui.handle_input(UiInputEvent::KeyPressed(key)) {
                            self.apply_ui_action(&action, event_loop);
                        }
                    }
                    return;
                }
                if self.scroll_menu_from_wheel(delta) {
                    return;
                }
                if steps != 0
                    && self.container_screen.is_some()
                    && self
                        .recipe_overlay_regions
                        .iter()
                        .any(|region| region.contains(self.mouse_pos.0, self.mouse_pos.1))
                {
                    let (w, h) = self
                        .active
                        .as_ref()
                        .map_or((0, 0), |active| (active.size.width, active.size.height));
                    let key = if steps > 0 { UiKey::Up } else { UiKey::Down };
                    for _ in 0..steps.unsigned_abs().min(64) {
                        for action in
                            self.ui
                                .recipe_browser
                                .handle_input(UiInputEvent::KeyPressed(key), w, h)
                        {
                            self.dispatch_recipe_browser_action(&action);
                        }
                    }
                    return;
                }
                if steps != 0 {
                    if let Some(creative) = self.creative_inventory.as_mut() {
                        creative.scroll_by(-steps * 9);
                        return;
                    }
                }
                if steps != 0 {
                    if let Some(screen) = &mut self.container_screen {
                        if screen.schema().menu_id == 19 {
                            screen.scroll_merchant(-steps);
                            return;
                        } else if screen.schema().menu_id == 18 {
                            let max = screen.loom_patterns.len().div_ceil(3).saturating_sub(4);
                            screen.loom_scroll_row =
                                (screen.loom_scroll_row as i64 - i64::from(steps))
                                    .clamp(0, max as i64) as usize;
                            return;
                        } else if screen.schema().menu_id == 24 {
                            let max = screen
                                .stonecutter_recipes
                                .len()
                                .div_ceil(4)
                                .saturating_sub(3);
                            screen.stonecutter_scroll_row =
                                (screen.stonecutter_scroll_row as i64 - i64::from(steps))
                                    .clamp(0, max as i64) as usize;
                            return;
                        }
                    }
                }
                if !self.cursor_grabbed && !self.paused && self.container_scroll_tweak(steps) {
                    return;
                }
                // Grabbed scroll cycles the held hotbar slot (vanilla
                // `mouseScrolling$scrollHotbar`).
                if self.cursor_grabbed && !self.paused && self.live.is_some() && steps != 0 {
                    let current = i32::from(self.input_router.active_hotbar_slot);
                    let next = (current - steps).rem_euclid(9) as u8;
                    self.apply_window_input(WindowInputEvent::SelectHotbar(next));
                }
            }
            WindowEvent::RedrawRequested => self.render_frame(),
            _ => {}
        }
        // Text-entry state changes arrive through this single funnel —
        // clicks focusing fields, Escape closing the chat bar, screen
        // navigations — so the IME allow flag always settles here.
        self.update_ime();
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        // Raw mouse deltas drive the camera while grabbed — unaffected by
        // cursor clamping at screen edges. Deltas accumulate rather than
        // applying per event: `render_frame` latches the summed delta at
        // the top of the frame, so a 1000 Hz motion stream during a
        // WaitUntil sleep costs zero camera math until the pose is read.
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.cursor_grabbed && !self.paused {
                self.pending_mouse_delta.0 += delta.0;
                self.pending_mouse_delta.1 += delta.1;
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.fatal.is_some() {
            event_loop.exit();
            return;
        }
        self.poll_mod_sync_worker();
        // Vanilla-JAR bootstrap poll is cheap and non-blocking; it runs
        // even while occluded so a download finishing in the background
        // still lands `Ready` and re-arms asset discovery.
        self.poll_vanilla_bootstrap();
        self.poll_vanilla_assets_bootstrap();
        self.poll_missing_dependency_workers();
        self.poll_plamo_worker();
        self.poll_live_mod_sync_diff();
        // Reap a dead share worker + keep guest-connected flags fresh.
        self.poll_friend_share();
        if self.occluded && std::env::var_os("MEGU_DISABLE_OCCLUSION_SLEEP").is_none() {
            // Zero-GPU sleep: no redraw requests at all. A slow tick keeps
            // the live session polled until the surface un-occludes.
            self.poll_live_in_background();
            let wake = Instant::now() + OCCLUDED_TICK;
            event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
            return;
        }
        // The effective cap drops to `background_fps_limit` while unfocused
        // (the RenderConfig rule); WaitUntil holds the loop to it. Calling
        // `request_redraw` before the deadline would let a 1000 Hz input
        // event stream bypass the cap and storm redundant redraws.
        let cap = self.render_config.effective_fps_cap(self.focused);
        if cap > 0 {
            let deadline = self.frame_start + Duration::from_secs_f64(1.0 / f64::from(cap));
            if Instant::now() < deadline {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
                return;
            }
            // Deadline reached: request one redraw and sleep until it
            // fires — a capped loop never enters `Poll` spin.
            if let Some(active) = &self.active {
                active.window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        if let Some(active) = &self.active {
            active.window.request_redraw();
        }
        // Wayland frame-callback starvation: a requested redraw that never
        // gets dispatched means the compositor is throttling this surface
        // (fully occluded/unmapped — winit only re-emits RedrawRequested
        // after a wl_surface.frame callback). Spinning `Poll` here burns a
        // core at ~1.5M `about_to_wait` iterations/sec doing nothing, so
        // after 500 ms without a delivered frame fall back to a slow wake
        // tick; when callbacks resume, `frame_start` refreshes and Poll
        // pacing returns automatically.
        if self.frame_start.elapsed() > Duration::from_millis(500) {
            self.poll_live_in_background();
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(33),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Poll);
        }
    }
}

fn project_nametag(matrix: &crate::Mat4, [x, y, z]: [f32; 3]) -> Option<[f32; 2]> {
    let point = [x, y, z, 1.0];
    let [x, y, depth, w] = std::array::from_fn::<_, 4, _>(|row| {
        matrix
            .iter()
            .zip(point)
            .map(|(col, v)| col[row] * v)
            .sum::<f32>()
    });
    if w <= 0.0 || !(0.0..=w).contains(&depth) || !(-w..=w).contains(&x) || !(-w..=w).contains(&y) {
        return None;
    }
    Some([0.5 + x / w * 0.5, 0.5 - y / w * 0.5])
}

/// World folder names under `<gameDir>/saves/` — the Singleplayer
/// screen's world list. `game_dir` defaults to `.` like the launcher's
/// `--gameDir` fallback; missing/unreadable `saves/` yields an empty
/// list (the "Create New World" path still works). Sorted by name for
/// deterministic presentation.
fn list_saved_worlds(game_dir: Option<&std::path::Path>) -> Vec<String> {
    let saves = game_dir
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("saves");
    let mut worlds: Vec<String> = match std::fs::read_dir(&saves) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    worlds.sort();
    worlds
}

/// Mirror the open `ZshCommandBar` into `hud.chat_input` — the HUD
/// draws the vanilla-style input strip + suggestion popup from that
/// mirror each frame (`sync_hud_to` clones `live.hud` over `ui.hud`,
/// so callers run this after the clone in `render_frame` and on every
/// key the bar consumes).
fn sync_chat_input_hud(
    input: &mut megu_ui::hud_widgets::ChatInputOverlay,
    bar: Option<&ZshCommandBar>,
    show_suggestions: bool,
) {
    match bar {
        Some(bar) => {
            input.open = true;
            if input.text != bar.text() {
                input.text.clear();
                input.text.push_str(bar.text());
            }
            input.cursor = bar.text()[..bar.cursor()].chars().count();
            if !show_suggestions {
                input.completions.clear();
                input.completion_index = megu_ui::hud_widgets::NO_COMPLETION;
            } else if let Some((list, selected)) = bar.completion_menu_list() {
                input.completions = list;
                input.completion_index = selected;
            } else {
                input.completions.clear();
                input.completions.extend(
                    bar.completions()
                        .into_iter()
                        .map(|candidate| candidate.text),
                );
                input.completion_index = megu_ui::hud_widgets::NO_COMPLETION;
            }
        }
        None => {
            input.open = false;
            input.text.clear();
            input.completions.clear();
            input.completion_index = megu_ui::hud_widgets::NO_COMPLETION;
        }
    }
}

/// Read the held hotbar stack's explicit 1.21.1 `max_damage` (component 2)
/// and `damage` (component 3) patches. Items whose prototype carries these
/// components implicitly are left alone because the client has no registry
/// prototype table to resolve them safely.
fn held_tool_durability(live: &LiveSession, hand: i32) -> Option<(u32, u32)> {
    // Window slot 36..=44 is the hotbar and 45 the off hand (vanilla
    // `Inventory` layout).
    let slot = if hand == LiveSession::OFF_HAND {
        45
    } else {
        36 + live.hud.selected_slot.min(8) as i32
    };
    stack_durability(live.player_inventory.get(&slot)?)
}

/// Read a decoded stack's explicit 1.21.1 `max_damage` (component 2) and
/// `damage` (component 3) patches as `(damage, max_damage)`.
fn stack_durability(stack: &ItemStackComponents) -> Option<(u32, u32)> {
    let component_value = |type_id| {
        stack
            .added
            .iter()
            .find(|component| component.type_id == type_id)
            .and_then(|component| parse_component_varint(&component.data))
    };
    let max_damage = component_value(2)?;
    let damage = component_value(3).unwrap_or(0);
    (max_damage > 0).then_some((damage, max_damage))
}

/// Vanilla `F3 + H` metadata lines for one stack, as tooltip lore. The
/// kernel formatter owns the exact text; its `§8` dark-gray marker is
/// dropped because the tooltip already draws lore in that gray.
fn advanced_tooltip_lore(
    stack: &megu_ui::container_screen::UiItemStack,
    components: Option<&ItemStackComponents>,
) -> Vec<String> {
    let (damage, max_damage) = components.and_then(stack_durability).unwrap_or((0, 0));
    let component_count = components.map_or(0, |stack| stack.added.len() + stack.removed.len());
    format_advanced_tooltip_lines(&stack.namespaced_id, damage, max_damage, component_count)
        .into_iter()
        .map(|line| line.strip_prefix("§8").unwrap_or(&line).to_owned())
        .collect()
}

/// Append the `F3 + H` suffix to a localized stack's lore.
fn apply_advanced_tooltips(
    mut stack: megu_ui::container_screen::UiItemStack,
    components: Option<&ItemStackComponents>,
) -> megu_ui::container_screen::UiItemStack {
    let lines = advanced_tooltip_lore(&stack, components);
    stack.lore.extend(lines);
    stack
}

fn parse_component_varint(bytes: &[u8]) -> Option<u32> {
    let mut result = 0u32;
    for (index, byte) in bytes.iter().copied().enumerate().take(5) {
        result |= u32::from(byte & 0x7f).checked_shl((index * 7) as u32)?;
        if byte & 0x80 == 0 {
            return (index + 1 == bytes.len()).then_some(result);
        }
    }
    None
}

/// Mirror the persisted `client-app` settings into `UiSettings` — every
/// UI-visible key the file defines (video/audio/camera/QoL/fair-play)
/// lands on its widget state so `apply_settings` activates it. UI-only
/// surfaces (visual style, locale, shader quick-toggle, language) stay
/// untouched.
fn apply_client_settings_to_ui(src: &ClientSettings, ui: &mut UiSettings) {
    ui.fullscreen = src.fullscreen;
    ui.mouse_invert_y = src.mouse_invert_y;
    ui.gamma_pct = src.gamma_pct.clamp(50, 200);
    ui.gui_scale = src.gui_scale.min(4);
    ui.keys = megu_ui::state::KeyBindings {
        forward: src.keys.forward.clone(),
        back: src.keys.back.clone(),
        left: src.keys.left.clone(),
        right: src.keys.right.clone(),
        jump: src.keys.jump.clone(),
        sneak: src.keys.sneak.clone(),
        sprint: src.keys.sprint.clone(),
        inventory: src.keys.inventory.clone(),
        drop: src.keys.drop.clone(),
        swap_offhand: src.keys.swap_offhand.clone(),
        chat: src.keys.chat.clone(),
        command: src.keys.command.clone(),
        zoom: src.keys.zoom.clone(),
        camera_toggle: src.keys.camera_toggle.clone(),
        fullscreen: src.keys.fullscreen.clone(),
        advancements: src.keys.advancements.clone(),
        sort_inventory: src.keys.sort_inventory.clone(),
    };
    ui.shader_quick.shaders_enabled = src.shaders_enabled;
    ui.qol_looking_at_hud = src.builtin_looking_at_hud;
    ui.inventory_drag_controls = src.inventory_drag_controls;
    ui.qol_inventory_sort = src.builtin_inventory_sort;
    ui.zsh_command_bar = src.zsh_command_bar;
    ui.qol.compact_debug_hud.enabled = src.compact_debug_hud_enabled;
    ui.qol.compact_debug_hud.show_fps = src.compact_debug_hud_segments & (1 << 0) != 0;
    ui.qol.compact_debug_hud.show_coords = src.compact_debug_hud_segments & (1 << 1) != 0;
    ui.qol.compact_debug_hud.show_biome = src.compact_debug_hud_segments & (1 << 2) != 0;
    ui.qol.compact_debug_hud.show_facing = src.compact_debug_hud_segments & (1 << 3) != 0;
    ui.qol.compact_debug_hud.show_ping = src.compact_debug_hud_segments & (1 << 4) != 0;
    ui.qol.compact_debug_hud.show_mspt_tps = src.compact_debug_hud_segments & (1 << 5) != 0;
    ui.qol.compact_debug_hud.show_light_level = src.compact_debug_hud_segments & (1 << 6) != 0;
    ui.qol.compact_debug_hud.show_chunks_entities = src.compact_debug_hud_segments & (1 << 7) != 0;
    ui.qol.compact_debug_hud.show_ram = src.compact_debug_hud_segments & (1 << 8) != 0;
    ui.video.preset = match src.preset {
        QualityPreset::Fast => VideoPreset::Fast,
        QualityPreset::Balanced => VideoPreset::Balanced,
        QualityPreset::High => VideoPreset::High,
        QualityPreset::Ultra => VideoPreset::Ultra,
        QualityPreset::Custom => VideoPreset::Custom,
    };
    ui.video.view_distance = src.view_distance;
    ui.video.lod = match src.distant_horizon {
        DistantHorizon::Off => LodMode::Off,
        DistantHorizon::Medium => LodMode::Medium,
        DistantHorizon::Far => LodMode::Far,
    };
    ui.video.gpu_backend = match src.gpu_backend {
        megu_client_mesh::RenderBackendPreference::Auto => GpuBackend::Auto,
        megu_client_mesh::RenderBackendPreference::MeshShader => GpuBackend::MeshShader,
        megu_client_mesh::RenderBackendPreference::ComputeIndirect => GpuBackend::ComputeIndirect,
        megu_client_mesh::RenderBackendPreference::Classic => GpuBackend::Classic,
        megu_client_mesh::RenderBackendPreference::ClassicLowSpec => GpuBackend::ClassicLowSpec,
    };
    ui.video.fps_cap = src.max_fps.unwrap_or(0);
    ui.video.background_fps = src.background_fps_limit;
    ui.video.vsync = src.vsync;
    ui.video.smooth_lighting = src.smooth_lighting;
    ui.video.clouds = match src.clouds {
        CloudMode::Off => megu_ui::state::CloudSetting::Off,
        CloudMode::Fast => megu_ui::state::CloudSetting::Fast,
        CloudMode::Fancy => megu_ui::state::CloudSetting::Fancy,
    };
    ui.video.entity_shadows = src.entity_shadows;
    ui.video.comfort_exposure = src.comfort_exposure;
    ui.video.dynamic_lights = src.dynamic_lights;
    ui.video.enchantment_glint = src.enchantment_glint;
    ui.video.indirect_light_bounce = src.indirect_light_bounce;
    ui.video.directional_shadow_gameplay = src.directional_shadow_gameplay;
    ui.video.lazy_catchup_gameplay = src.lazy_catchup_gameplay;
    ui.video.ram_budget_mib = src.ram_budget_mib;
    // Refresh the detected host-RAM bound so the budget slider always
    // reflects the real machine, not a stale boot-time probe.
    ui.video.host_total_ram_mib = megu_ui::state::detect_host_total_ram_mib();
    ui.audio.master = src.volume_master;
    ui.audio.music = src.volume_music;
    ui.audio.blocks = src.volume_blocks;
    ui.audio.hostile = src.volume_hostile;
    ui.audio.friendly = src.volume_friendly;
    ui.audio.max_volume_cap = (src.qol.max_volume_cap * 100.0).round() as u8;
    ui.audio.max_concurrent_same_sound = src.qol.max_concurrent_same_sound;
    ui.fov_deg = u16::from(src.fov);
    ui.mouse_sensitivity_pct = src.mouse_sensitivity_pct;
    ui.zoom_divisor_tenths = (src.qol.zoom_divisor * 10.0).round() as u16;
    ui.qol.tool_warn_percent = src.qol.tool_warn_percent;
    ui.qol.prevent_tool_break = src.qol.prevent_tool_break;
    ui.qol.chat_history_lines = src.qol.chat_history_lines;
    ui.qol.retain_chat_on_reconnect = src.qol.retain_chat_on_reconnect;
    ui.qol.recipe_overlay_enabled = src.seamless_qol.recipe_overlay_enabled;
    ui.qol.crash_report_mode = match src.crash_report_mode {
        ClientCrashReportMode::Ask => UiCrashReportMode::Ask,
        ClientCrashReportMode::AutoCrashOnly => UiCrashReportMode::AutoCrashOnly,
        ClientCrashReportMode::Off => UiCrashReportMode::Off,
    };
    ui.fair_play.minimap_mode = match src.map_fair_play.minimap_mode {
        HostMinimapMode::Unrestricted => MinimapMode::Unrestricted,
        HostMinimapMode::FairPlay => MinimapMode::FairPlay,
        HostMinimapMode::Disabled => MinimapMode::Disabled,
    };
    ui.fair_play.allow_cave_map = src.map_fair_play.allow_cave_map;
    ui.fair_play.allow_mob_radar = src.map_fair_play.allow_mob_radar;
    ui.fair_play.player_radar_scope = match src.map_fair_play.player_radar_scope {
        HostPlayerRadarScope::All => PlayerRadarScope::All,
        HostPlayerRadarScope::PartyOnly => PlayerRadarScope::PartyOnly,
        HostPlayerRadarScope::None => PlayerRadarScope::None,
    };
    ui.fair_play.allow_waypoint_teleport = src.map_fair_play.allow_waypoint_teleport;
}

/// `MEGU_DIRECTIONAL_SHADOW_LIGHT` value for the integrated/hosted
/// server spawn: the server-side `ops_bool_str` reader accepts the full
/// `1/true/yes/on` alphabet, but the env pair stays canonical `"1"` /
/// `"0"` like the other boot switches.
fn directional_shadow_light_env_value(on: bool) -> &'static str {
    if on { "1" } else { "0" }
}

/// `MEGU_LAZY_CATCHUP` value for the integrated/hosted server spawn —
/// the same canonical `"1"` / `"0"` pair the server-side `ops_bool_str`
/// reader resolves.
fn lazy_catchup_env_value(on: bool) -> &'static str {
    if on { "1" } else { "0" }
}

/// Overlay alpha for the 2D enchant-glint shimmer — subtle (~0.24) so
/// item icons stay fully readable under the tint.
const GLINT_OVERLAY_ALPHA: u8 = 61;

/// Logical-px hotbar slot edge, matching `client-ui`'s `layout.rs`
/// hotbar grid (`slot = 22`, offhand + 4 px gap + 9 slots).
const HOTBAR_SLOT_PX: i32 = 22;

/// Vanilla `ItemStack.hasFoil` on raw 767 component patches: a non-empty
/// `minecraft:enchantments` (9) or `minecraft:stored_enchantments` (23)
/// list — both wire a VarInt entry count first — or a set
/// `minecraft:enchantment_glint_override` (18).
pub fn stack_shows_enchant_glint(stack: &ItemStackComponents) -> bool {
    if let Some(override_patch) = stack.added.iter().find(|patch| patch.type_id == 18) {
        return override_patch.data.first().is_some_and(|flag| *flag != 0);
    }
    stack.added.iter().any(|patch| match patch.type_id {
        9 | 23 => patch.data.first().is_some_and(|count| *count != 0),
        _ => false,
    })
}

/// One 2D slot's subtle violet-lavender glint overlay, or `None` while
/// the Settings → Video `enchantment_glint` switch is off or the stack
/// shows no foil. The tint samples the same
/// [`enchantment_glint_rgb`] the 3D WGSL path evaluates per fragment,
/// keyed by the slot's normalized `[u, v]` (rect center over the
/// viewport) at `elapsed_sec`, with [`GLINT_OVERLAY_ALPHA`] pinned so
/// the icon below keeps 100% of its contrast.
fn enchant_glint_overlay_cmd(
    enabled: bool,
    stack: &ItemStackComponents,
    rect: UiRect,
    viewport_w: u32,
    viewport_h: u32,
    elapsed_sec: f32,
) -> Option<UiCommand> {
    if !enabled || !stack_shows_enchant_glint(stack) {
        return None;
    }
    let u = (rect.x as f32 + rect.w as f32 * 0.5) / viewport_w.max(1) as f32;
    let v = (rect.y as f32 + rect.h as f32 * 0.5) / viewport_h.max(1) as f32;
    let rgb = enchantment_glint_rgb([u, v], elapsed_sec);
    let channel = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some(UiCommand::FillRect {
        rect,
        color: UiColor::rgba(
            channel(rgb[0]),
            channel(rgb[1]),
            channel(rgb[2]),
            GLINT_OVERLAY_ALPHA,
        ),
    })
}

/// Physical-pixel rect of one bottom-row hotbar cell — `index` `0..=8`
/// is a hotbar slot, `-1` the offhand cell — mirroring the `hud()`
/// layout math in `client-ui`'s `layout.rs` so the glint lands exactly
/// on the slot chrome that screen already drew.
fn hotbar_slot_rect(viewport_w: u32, viewport_h: u32, scale: u32, index: i32) -> UiRect {
    let s = scale.max(1) as i32;
    let logical_w = viewport_w.min(i32::MAX as u32) as i32 / s;
    let logical_h = viewport_h.min(i32::MAX as u32) as i32 / s;
    let slot = HOTBAR_SLOT_PX;
    let hx = logical_w / 2 - (9 * slot + slot + 4) / 2;
    let hy = logical_h - slot - 4;
    let x = if index < 0 {
        hx
    } else {
        hx + slot + 4 + index * slot
    };
    UiRect::new(x * s, hy * s, (slot * s) as u32, (slot * s) as u32)
}

fn ui_gpu_backend(backend: GpuBackend) -> megu_client_mesh::RenderBackendPreference {
    match backend {
        GpuBackend::Auto => megu_client_mesh::RenderBackendPreference::Auto,
        GpuBackend::MeshShader => megu_client_mesh::RenderBackendPreference::MeshShader,
        GpuBackend::ComputeIndirect => megu_client_mesh::RenderBackendPreference::ComputeIndirect,
        GpuBackend::Classic => megu_client_mesh::RenderBackendPreference::Classic,
        GpuBackend::ClassicLowSpec => megu_client_mesh::RenderBackendPreference::ClassicLowSpec,
    }
}

/// Overlay the UI-visible settings onto `base` (the last loaded file
/// state): every widget-owned key writes back, and file-only fields
/// (`shader_pack`, `shader_profile`, `report_endpoint`, `particles`)
/// survive untouched.
fn ui_to_client_settings(ui: &UiSettings, base: &ClientSettings) -> ClientSettings {
    let mut out = base.clone();
    out.shaders_enabled = ui.shader_quick.shaders_enabled;
    out.builtin_looking_at_hud = ui.qol_looking_at_hud;
    out.inventory_drag_controls = ui.inventory_drag_controls;
    out.builtin_inventory_sort = ui.qol_inventory_sort;
    out.zsh_command_bar = ui.zsh_command_bar;
    let compact_debug_hud = &ui.qol.compact_debug_hud;
    out.compact_debug_hud_enabled = compact_debug_hud.enabled;
    out.compact_debug_hud_segments = u16::from(compact_debug_hud.show_fps)
        | (u16::from(compact_debug_hud.show_coords) << 1)
        | (u16::from(compact_debug_hud.show_biome) << 2)
        | (u16::from(compact_debug_hud.show_facing) << 3)
        | (u16::from(compact_debug_hud.show_ping) << 4)
        | (u16::from(compact_debug_hud.show_mspt_tps) << 5)
        | (u16::from(compact_debug_hud.show_light_level) << 6)
        | (u16::from(compact_debug_hud.show_chunks_entities) << 7)
        | (u16::from(compact_debug_hud.show_ram) << 8);
    out.fullscreen = ui.fullscreen;
    out.mouse_invert_y = ui.mouse_invert_y;
    out.gamma_pct = ui.gamma_pct.clamp(50, 200);
    out.gui_scale = ui.gui_scale.min(4);
    out.keys = megu_client::settings::KeyBindings {
        forward: ui.keys.forward.clone(),
        back: ui.keys.back.clone(),
        left: ui.keys.left.clone(),
        right: ui.keys.right.clone(),
        jump: ui.keys.jump.clone(),
        sneak: ui.keys.sneak.clone(),
        sprint: ui.keys.sprint.clone(),
        inventory: ui.keys.inventory.clone(),
        drop: ui.keys.drop.clone(),
        swap_offhand: ui.keys.swap_offhand.clone(),
        chat: ui.keys.chat.clone(),
        command: ui.keys.command.clone(),
        zoom: ui.keys.zoom.clone(),
        camera_toggle: ui.keys.camera_toggle.clone(),
        fullscreen: ui.keys.fullscreen.clone(),
        advancements: ui.keys.advancements.clone(),
        sort_inventory: ui.keys.sort_inventory.clone(),
    };
    out.preset = match ui.video.preset {
        VideoPreset::Fast => QualityPreset::Fast,
        VideoPreset::Balanced => QualityPreset::Balanced,
        VideoPreset::High => QualityPreset::High,
        VideoPreset::Ultra => QualityPreset::Ultra,
        VideoPreset::Custom => QualityPreset::Custom,
    };
    out.view_distance = ui.video.view_distance;
    out.distant_horizon = match ui.video.lod {
        LodMode::Off => DistantHorizon::Off,
        LodMode::Medium => DistantHorizon::Medium,
        LodMode::Far => DistantHorizon::Far,
    };
    out.gpu_backend = match ui.video.gpu_backend {
        GpuBackend::Auto => megu_client_mesh::RenderBackendPreference::Auto,
        GpuBackend::MeshShader => megu_client_mesh::RenderBackendPreference::MeshShader,
        GpuBackend::ComputeIndirect => megu_client_mesh::RenderBackendPreference::ComputeIndirect,
        GpuBackend::Classic => megu_client_mesh::RenderBackendPreference::Classic,
        GpuBackend::ClassicLowSpec => megu_client_mesh::RenderBackendPreference::ClassicLowSpec,
    };
    out.max_fps = (ui.video.fps_cap > 0).then_some(ui.video.fps_cap);
    out.background_fps_limit = ui.video.background_fps;
    out.vsync = ui.video.vsync;
    out.smooth_lighting = ui.video.smooth_lighting;
    out.clouds = match ui.video.clouds {
        megu_ui::state::CloudSetting::Off => CloudMode::Off,
        megu_ui::state::CloudSetting::Fast => CloudMode::Fast,
        megu_ui::state::CloudSetting::Fancy => CloudMode::Fancy,
    };
    out.entity_shadows = ui.video.entity_shadows;
    out.comfort_exposure = ui.video.comfort_exposure;
    out.dynamic_lights = ui.video.dynamic_lights;
    out.enchantment_glint = ui.video.enchantment_glint;
    out.indirect_light_bounce = ui.video.indirect_light_bounce;
    out.directional_shadow_gameplay = ui.video.directional_shadow_gameplay;
    out.lazy_catchup_gameplay = ui.video.lazy_catchup_gameplay;
    out.ram_budget_mib = ui.video.ram_budget_mib;
    out.volume_master = ui.audio.master;
    out.volume_music = ui.audio.music;
    out.volume_blocks = ui.audio.blocks;
    out.volume_hostile = ui.audio.hostile;
    out.volume_friendly = ui.audio.friendly;
    out.qol.max_volume_cap = f32::from(ui.audio.max_volume_cap.min(100)) / 100.0;
    out.qol.max_concurrent_same_sound = ui.audio.max_concurrent_same_sound;
    out.fov = ui.fov_deg.clamp(30, 110) as u8;
    out.mouse_sensitivity_pct = ui.mouse_sensitivity_pct;
    out.qol.zoom_divisor = f32::from(ui.zoom_divisor_tenths) / 10.0;
    out.qol.tool_warn_percent = ui.qol.tool_warn_percent;
    out.qol.prevent_tool_break = ui.qol.prevent_tool_break;
    out.qol.chat_history_lines = ui.qol.chat_history_lines;
    out.qol.retain_chat_on_reconnect = ui.qol.retain_chat_on_reconnect;
    out.seamless_qol.recipe_overlay_enabled = ui.qol.recipe_overlay_enabled;
    out.crash_report_mode = match ui.qol.crash_report_mode {
        UiCrashReportMode::Ask => ClientCrashReportMode::Ask,
        UiCrashReportMode::AutoCrashOnly => ClientCrashReportMode::AutoCrashOnly,
        UiCrashReportMode::Off => ClientCrashReportMode::Off,
    };
    out.map_fair_play.minimap_mode = match ui.fair_play.minimap_mode {
        MinimapMode::Unrestricted => HostMinimapMode::Unrestricted,
        MinimapMode::FairPlay => HostMinimapMode::FairPlay,
        MinimapMode::Disabled => HostMinimapMode::Disabled,
    };
    out.map_fair_play.minimap_fair_play = ui.fair_play.minimap_mode != MinimapMode::Unrestricted;
    out.map_fair_play.allow_cave_map = ui.fair_play.allow_cave_map;
    out.map_fair_play.allow_mob_radar = ui.fair_play.allow_mob_radar;
    // `allow_entity_radar` mirrors `allow_mob_radar` (same restriction).
    out.map_fair_play.allow_entity_radar = ui.fair_play.allow_mob_radar;
    out.map_fair_play.player_radar_scope = match ui.fair_play.player_radar_scope {
        PlayerRadarScope::All => HostPlayerRadarScope::All,
        PlayerRadarScope::PartyOnly => HostPlayerRadarScope::PartyOnly,
        PlayerRadarScope::None => HostPlayerRadarScope::None,
    };
    out.map_fair_play.allow_waypoint_teleport = ui.fair_play.allow_waypoint_teleport;
    out
}

/// Deterministic per-session splash seed: FNV-1a over the launch
/// config's identity fields (`username`/`--version`/`--gameDir`) — zero
/// `SystemTime`, stable across the whole run.
fn splash_seed_for(config: &GpuCliConfig) -> u64 {
    let mut hash = Fnv1a64::new();
    hash.update(config.username.as_bytes());
    if let Some(id) = config.version_id.as_deref() {
        hash.update_str(id);
    }
    if let Some(dir) = config.game_dir.as_deref() {
        hash.update(dir.to_string_lossy().as_bytes());
    }
    hash.finish()
}

/// The built-in `client-ui` title wordmark/tagline `Text` commands the
/// GPU banner replaces while the local JAR supplies real branding.
/// Matched by payload (never position) so a client-ui layout change
/// degrades to "both drawn", never to a panic.
fn is_builtin_title_text(command: &UiCommand) -> bool {
    matches!(
        command,
        UiCommand::Text { text, .. }
            if matches!(
                text.as_str(),
                "MINECRAFT" | "MEGU EDITION" | "clean-room native client"
                    | "クリーンルーム ネイティブクライアント"
            )
    )
}

pub const MEGU_WEBSITE_URL: &str = "https://megu.megumin.tech/";
pub const MEGU_SUPPORT_URL: &str = "https://megu.megumin.tech/#support";
pub const MEGU_CONTRIBUTE_URL: &str = "https://github.com/nekomario28/megu-mc";
pub const MEGU_REPORT_URL: &str = "https://megu.megumin.tech/#report";

// ---------------------------------------------------------------------------
// Live entity/BER + particle staging helpers
// ---------------------------------------------------------------------------

/// Stable dispatch slot per [`VanillaBerKind`] for the template cache.
fn ber_kind_slot(kind: VanillaBerKind) -> usize {
    match kind {
        VanillaBerKind::Chest => 0,
        VanillaBerKind::Sign => 1,
        VanillaBerKind::Banner => 2,
        VanillaBerKind::Beacon => 3,
        VanillaBerKind::Bed => 4,
        VanillaBerKind::Skull => 5,
        VanillaBerKind::ItemFrame => 6,
        VanillaBerKind::Campfire => 7,
        VanillaBerKind::BrewingStand => 8,
        VanillaBerKind::Lectern => 9,
        VanillaBerKind::DecoratedPot => 10,
        VanillaBerKind::ShulkerBox => 11,
    }
}

/// Classify a tracked block entity for the BER pass: the NBT `id`
/// string is authoritative (chunk payloads carry it), falling back to
/// the protocol-767 `BuiltInRegistries.BLOCK_ENTITY_TYPE` wire id when
/// the payload omits it (bare `block_entity_data` updates).
fn classify_tracked_block_entity(be: &TrackedBlockEntity) -> Option<VanillaBerKind> {
    if let Ok(NbtValue::Compound(map)) = WireReader::new(&be.nbt_bytes).read_nbt() {
        if let Some(NbtValue::String(id)) = map.get("id") {
            if let Some(kind) = classify_vanilla_ber(id) {
                return Some(kind);
            }
        }
    }
    ber_kind_for_wire_id(be.kind)
}

/// Wire-id fallback — `BuiltInRegistries.BLOCK_ENTITY_TYPE` ids in
/// 1.21.1 for the renderable families (the same numbering the server's
/// `live_block_entity_wire_id` pins: sign=7, beacon=14, lectern=29,
/// campfire=32, decorated_pot=40). Item frames are entities, not block
/// entities, so that family only ever arrives via the NBT name path.
fn ber_kind_for_wire_id(kind: i32) -> Option<VanillaBerKind> {
    Some(match kind {
        1..=3 => VanillaBerKind::Chest,
        7 | 8 => VanillaBerKind::Sign,
        19 => VanillaBerKind::Banner,
        14 => VanillaBerKind::Beacon,
        24 => VanillaBerKind::Bed,
        15 => VanillaBerKind::Skull,
        32 => VanillaBerKind::Campfire,
        11 => VanillaBerKind::BrewingStand,
        29 => VanillaBerKind::Lectern,
        40 => VanillaBerKind::DecoratedPot,
        23 => VanillaBerKind::ShulkerBox,
        _ => return None,
    })
}

/// Canonical idle state per BER family — the same defaults the software
/// renderer's `default_vanilla_ber_state` uses, so block entities look
/// identical across the two paths.
fn default_vanilla_ber_state(kind: VanillaBerKind) -> VanillaBerState {
    match kind {
        VanillaBerKind::Chest => VanillaBerState::Chest {
            lid_open: 0.0,
            double: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::Sign => VanillaBerState::Sign {
            wall: false,
            hanging: false,
            glowing: false,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Banner => VanillaBerState::Banner {
            wall: false,
            sway_rad: 0.0,
            facing_yaw: 0.0,
        },
        VanillaBerKind::Beacon => VanillaBerState::Beacon {
            beam_height: 8.0,
            active: true,
            rgb: [0.35, 0.6, 1.0],
        },
        VanillaBerKind::Bed => VanillaBerState::Bed {
            is_head: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::Skull => VanillaBerState::Skull {
            wall: false,
            yaw_deg: 0.0,
        },
        VanillaBerKind::ItemFrame => VanillaBerState::ItemFrame {
            facing: VanillaBerFacing::South,
            glow: false,
            rotation: 0,
        },
        VanillaBerKind::Campfire => VanillaBerState::Campfire {
            lit: true,
            item_count: 2,
        },
        VanillaBerKind::BrewingStand => VanillaBerState::BrewingStand { bottle_mask: 0b111 },
        VanillaBerKind::Lectern => VanillaBerState::Lectern {
            has_book: false,
            facing: VanillaBerFacing::South,
        },
        VanillaBerKind::DecoratedPot => VanillaBerState::DecoratedPot {
            facing: VanillaBerFacing::South,
            wobble: 0.0,
        },
        VanillaBerKind::ShulkerBox => VanillaBerState::ShulkerBox {
            open_progress: 0.0,
            facing: VanillaBerFacing::South,
        },
    }
}

/// Deterministic albedo per BER family — the software renderer's
/// `vanilla_ber_base_color` byte palette in `0..=1` tint form.
fn vanilla_ber_base_color_f32(kind: VanillaBerKind) -> [f32; 3] {
    let [r, g, b]: [u8; 3] = match kind {
        VanillaBerKind::Chest => [162, 116, 63],
        VanillaBerKind::Sign => [172, 134, 82],
        VanillaBerKind::Banner => [196, 62, 62],
        VanillaBerKind::Beacon => [214, 214, 214],
        VanillaBerKind::Bed => [178, 58, 58],
        VanillaBerKind::Skull => [216, 216, 208],
        VanillaBerKind::ItemFrame => [146, 104, 58],
        VanillaBerKind::Campfire => [104, 72, 48],
        VanillaBerKind::BrewingStand => [88, 70, 108],
        VanillaBerKind::Lectern => [138, 98, 58],
        VanillaBerKind::DecoratedPot => [168, 138, 108],
        VanillaBerKind::ShulkerBox => [138, 92, 158],
    };
    [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    ]
}

fn ease_ber_animation(cache: &mut LiveEntityRenderCache, pos: [i32; 3], target: f32) -> f32 {
    let current = cache.ber_animation_easing.entry(pos).or_insert(target);
    *current += (target - *current) * 0.35;
    if (*current - target).abs() < 0.001 {
        *current = target;
    }
    *current
}

fn live_animated_ber_state(
    live: &LiveSession,
    cache: &mut LiveEntityRenderCache,
    pos: [i32; 3],
    visual: megu_client::world::BlockEntityVisualState,
) -> Option<VanillaBerExtendedState> {
    let facing = VanillaBerFacing::South;
    let phase = (live.world.world_age.rem_euclid(24_000) as f32 / 24_000.0) * std::f32::consts::TAU;
    Some(match visual {
        megu_client::world::BlockEntityVisualState::Chest {
            open_count,
            trapped,
            ender,
        } => VanillaBerExtendedState::Chest {
            lid_open: ease_ber_animation(cache, pos, if open_count > 0 { 1.0 } else { 0.0 }),
            double: false,
            facing,
            chest: if ender {
                BerChestKind::Ender
            } else if trapped {
                BerChestKind::Trapped
            } else {
                BerChestKind::Normal
            },
        },
        megu_client::world::BlockEntityVisualState::ShulkerBox { open_count, color } => {
            let dye = BerDyeColor::ALL[usize::from(color.min(15))];
            VanillaBerExtendedState::ShulkerBox {
                open_progress: ease_ber_animation(
                    cache,
                    pos,
                    (f32::from(open_count) / 100.0).clamp(0.0, 1.0),
                ),
                facing,
                dye,
            }
        }
        megu_client::world::BlockEntityVisualState::Bell {
            ring_dir,
            ringing_ticks,
        } => {
            let elapsed = f32::from(50u16.saturating_sub(ringing_ticks));
            let envelope = (f32::from(ringing_ticks) / 50.0).clamp(0.0, 1.0);
            VanillaBerExtendedState::Bell {
                mount: BerBellMount::Floor,
                facing: match ring_dir % 4 {
                    0 => VanillaBerFacing::South,
                    1 => VanillaBerFacing::West,
                    2 => VanillaBerFacing::North,
                    _ => VanillaBerFacing::East,
                },
                swing_rad: (elapsed * 0.24).sin() * envelope * 0.45,
            }
        }
        megu_client::world::BlockEntityVisualState::EnchantingTable { book_open } => {
            VanillaBerExtendedState::EnchantingTable {
                facing_yaw: 0.0,
                book_yaw_rad: phase + pos[0] as f32 * 0.17 + pos[2] as f32 * 0.11,
                page_open: ease_ber_animation(cache, pos, if book_open { 1.0 } else { 0.0 }),
            }
        }
        megu_client::world::BlockEntityVisualState::Beacon { level, .. } => {
            VanillaBerExtendedState::Beacon {
                beam_height: level.clamp(0, 4) as f32,
                active: level > 0,
                rgb: [0.35, 0.75, 1.0],
                beam_yaw: phase,
            }
        }
        megu_client::world::BlockEntityVisualState::TrialSpawner { active, ominous } => {
            VanillaBerExtendedState::TrialSpawner {
                spin_rad: phase * 2.0,
                active,
                ominous,
                mob_scale: if active { 0.8 } else { 0.0 },
            }
        }
        megu_client::world::BlockEntityVisualState::Vault { ominous, state } => {
            VanillaBerExtendedState::Vault {
                spin_rad: phase * 1.5,
                active: state != 0,
                ominous,
            }
        }
        _ => return None,
    })
}

/// Pose for one live entity: relative head yaw from the tracked packet
/// state, hurt flash while `hurt_ticks` is hot, and the metadata-driven
/// body poses vanilla exposes (`sneak`/`swim`/`elytra`).
fn live_entity_pose(
    live: &LiveSession,
    entity_id: i32,
    tracked: Option<&TrackedEntity>,
) -> EntityPose {
    let mut pose = EntityPose {
        head_yaw_deg: tracked.map_or(0.0, |t| t.head_yaw_deg - t.yaw_deg),
        head_pitch_deg: tracked.map_or(0.0, |t| t.pitch_deg),
        hurt_flash: tracked.is_some_and(|t| t.hurt_ticks > 0),
        ..EntityPose::default()
    };
    if let Some(meta) = live.entity_metadata.get(&entity_id) {
        // Vanilla pose ids: 1 fall-flying, 3 swimming, 5 sneaking; the
        // shared flags byte keeps the sneak/elytra bits too.
        pose.swimming = meta.pose == Some(3);
        pose.sneaking = meta.pose == Some(5) || meta.flags.is_some_and(|f| f & 0x02 != 0);
        pose.elytra_flying = meta.pose == Some(1) || meta.flags.is_some_and(|f| f & 0x80 != 0);
    }
    pose
}

/// Helm frame for `subworld_id` built from the shared FPS move input:
/// W/S throttle, A/D yaw rate, Space/Shift altitude delta; a paused app
/// emits a fully released frame.
fn subworld_helm_cmd(subworld_id: u64, input: MoveInput, paused: bool) -> SubWorldHelmCmd {
    let input = if paused { MoveInput::default() } else { input };
    SubWorldHelmCmd {
        subworld_id,
        throttle: input.forward,
        yaw_rate: input.strafe,
        pitch_rate: 0.0,
        altitude_delta: input.vertical,
        flags: 0,
    }
}

/// Push the current helm frame to the session's worker on change only:
/// an unchanged command is not re-sent (a released stick is one zeroed
/// frame, not a stream of zeroes), and losing the binding sends a single
/// control release to the last helm target so a parked craft does not
/// keep thrusting under a stale input.
fn dispatch_subworld_helm(
    live: &LiveSession,
    next: Option<SubWorldHelmCmd>,
    last: &mut Option<SubWorldHelmCmd>,
) {
    if next == *last {
        return;
    }
    if let Some(cmd) = next.or_else(|| {
        last.filter(subworld_helm_cmd_active)
            .map(|prev| SubWorldHelmCmd {
                subworld_id: prev.subworld_id,
                ..SubWorldHelmCmd::default()
            })
    }) {
        live.send_subworld_helm(&cmd);
    }
    *last = next;
}

/// `true` while any helm control is off neutral — a release frame only
/// matters when the last dispatched command actually moved something.
fn subworld_helm_cmd_active(cmd: &SubWorldHelmCmd) -> bool {
    cmd.throttle != 0.0
        || cmd.yaw_rate != 0.0
        || cmd.pitch_rate != 0.0
        || cmd.altitude_delta != 0.0
        || cmd.flags != 0
}

/// Vanilla fox entity type id — the only rig with a held mouth slot.
const FOX_ENTITY_TYPE_ID: u16 = 42;

/// Fingerprint of `(spec, pose, held mouth item)` so rig meshes rebuild
/// only on real change — floats quantize to 1/64 to keep jitter from
/// thrashing the cache, and the item id joins so the fox's mouth prism
/// follows pick-up/drop without waiting on a pose delta.
fn entity_pose_key(spec: EntityRigSpec, pose: &EntityPose, held_item_id: Option<i32>) -> u64 {
    // Word-wise FNV-1a step (`hash ^= word; hash *= prime`) — not the
    // byte-wise `Fnv1a64` stream, kept bit-identical to the historical
    // digest; only the offset basis and prime come from `megu_core`.
    let mut hash = FNV1A64_OFFSET_BASIS;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    };
    mix(u64::from(spec.entity_type_id));
    for v in [
        pose.limb_swing,
        pose.limb_swing_amount,
        pose.head_yaw_deg,
        pose.head_pitch_deg,
        pose.swing_progress,
        pose.age_ticks,
    ] {
        mix((f64::from(v) * 64.0).round() as i64 as u64);
    }
    mix(u64::from(pose.sneaking)
        | u64::from(pose.swimming) << 1
        | u64::from(pose.elytra_flying) << 2
        | u64::from(pose.hurt_flash) << 3
        | u64::from(pose.baby) << 4
        | u64::from(pose.enable_skin_3d_layers) << 5);
    mix((held_item_id.map_or(0, |id| i64::from(id) + 1)) as u64);
    hash
}

fn entity_rig_pose(live: &LiveSession, eid: i32, tracked: Option<&TrackedEntity>) -> EntityRigPose {
    let mut pose = EntityRigPose::from(live_entity_pose(live, eid, tracked));
    let Some(visual) = live.world.entity_visual_state(eid) else {
        return pose;
    };
    pose.base.baby = visual.is_baby;
    pose.base.sneaking |= visual.crouching;
    pose.base.swimming |= visual.swimming;
    pose.sitting = visual.pose == Some(10);
    pose.sleeping = visual.pose == Some(2);
    pose.wither_armored = visual.wither_invul_ticks.is_some_and(|ticks| ticks > 0)
        || visual.health.is_some_and(|health| health < 150.0);
    pose.carried_block = visual.enderman_carried_block.is_some_and(|block| block > 0);
    pose.open_amount = visual
        .shulker_peek
        .map_or(0.0, |peek| f32::from(peek) / 100.0);
    pose.swell_amount = visual
        .creeper_state
        .map_or(0.0, |state| (state.max(0) as f32 / 2.0).clamp(0.0, 1.0));
    pose.main_hand_item = tracked.is_some_and(|entity| entity.equipment_slots & 1 != 0);
    pose.offhand_item = tracked.is_some_and(|entity| entity.equipment_slots & 2 != 0);
    pose.using_item = tracked.is_some_and(|entity| entity.last_animation == Some(0));
    pose
}

fn probable_cube_block_icon(name: &str) -> bool {
    let short = name.rsplit(':').next().unwrap_or(name);
    const ITEM_TOKENS: &[&str] = &[
        "sword",
        "pickaxe",
        "axe",
        "shovel",
        "hoe",
        "bow",
        "crossbow",
        "trident",
        "shield",
        "potion",
        "apple",
        "bread",
        "bucket",
        "elytra",
        "stick",
        "ingot",
        "nugget",
        "gem",
        "diamond",
        "emerald",
        "coal",
        "charcoal",
        "quartz",
        "shard",
        "crystal",
        "pearl",
        "eye_of",
        "book",
        "paper",
        "map",
        "compass",
        "clock",
        "torch",
        "dust",
        "powder",
        "egg",
        "seed",
        "bean",
        "berry",
        "carrot",
        "potato",
        "beetroot",
        "wheat",
        "sugar",
        "bone",
        "feather",
        "leather",
        "hide",
        "meat",
        "beef",
        "porkchop",
        "chicken",
        "mutton",
        "rabbit",
        "cod",
        "salmon",
        "fish",
        "stew",
        "soup",
        "pie",
        "cookie",
        "melon_slice",
        "blade",
        "shell",
        "star",
        "saddle",
        "helmet",
        "chestplate",
        "leggings",
        "boots",
        "horse_armor",
        "minecart",
        "boat",
        "sign",
        "bed",
        "door",
        "banner",
        "dye",
        "arrow",
        "firework",
        "music_disc",
        "disc",
        "template",
        "key",
        "brush",
        "shears",
        "flint_and_steel",
        "rod",
        "lead",
        "nametag",
        "chain",
        "flower",
        "sapling",
        "vine",
        "lily_pad",
        "coral_fan",
        "coral",
        "seagrass",
        "kelp",
        "grass",
        "fern",
        "bush",
        "stem",
        "roots",
        "fungus",
        "mushroom",
        "candle",
        "pane",
        "slab",
        "stairs",
        "wall",
        "fence",
        "button",
        "pressure_plate",
        "rail",
        "wire",
        "repeater",
        "comparator",
        "ladder",
        "scaffolding",
        "azalea",
        "dripleaf",
        "lichen",
        "amethyst_bud",
    ];
    !ITEM_TOKENS.iter().any(|token| short.contains(token))
}

fn extract_item_icon_cell(atlas: &megu_ui::draw::ItemIconAtlas, name: &str) -> Option<Vec<u8>> {
    let [u0, v0, u1, v1] = atlas.uv(name)?;
    let x0 = (u0 * atlas.width as f32).floor() as u32;
    let y0 = (v0 * atlas.height as f32).floor() as u32;
    let x1 = (u1 * atlas.width as f32).ceil() as u32;
    let y1 = (v1 * atlas.height as f32).ceil() as u32;
    if x1 <= x0 || y1 <= y0 || x1 > atlas.width || y1 > atlas.height {
        return None;
    }
    let width = (x1 - x0) as usize;
    let height = (y1 - y0) as usize;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in y0..y1 {
        let start = ((y * atlas.width + x0) * 4) as usize;
        rgba.extend_from_slice(&atlas.rgba[start..start + width * 4]);
    }
    Some(rgba)
}

pub fn upgrade_ui_item_icon_atlas_with_3d_blocks(
    atlas: &mut megu_ui::draw::ItemIconAtlas,
) -> usize {
    const CELL: u32 = megu_ui::draw::ITEM_ICON_CELL;
    let mut extracted = Vec::new();
    for item_id in 0..2048u32 {
        let name = crate::recipe_ingest::item_name_for_id(item_id);
        if !probable_cube_block_icon(&name) || !name.contains(':') {
            continue;
        }
        if let Some(rgba) = extract_item_icon_cell(atlas, &name) {
            if !extracted
                .iter()
                .any(|(existing, _): &(String, Vec<u8>)| *existing == name)
            {
                extracted.push((name, rgba));
            }
        }
    }
    let mut inputs = std::collections::BTreeMap::new();
    for (name, rgba) in &extracted {
        if rgba.len() == (CELL * CELL * 4) as usize {
            let sprite = SpriteRgba::new(CELL, CELL, rgba);
            inputs.insert(name.clone(), (sprite, sprite, None));
        }
    }
    let upgraded = upgrade_item_icon_atlas_with_3d_blocks(&inputs, CELL);
    let mut replaced = 0usize;
    for (name, rgba) in &upgraded {
        let Some([u0, v0, u1, v1]) = atlas.uv(name) else {
            continue;
        };
        let x0 = (u0 * atlas.width as f32).floor() as u32;
        let y0 = (v0 * atlas.height as f32).floor() as u32;
        let x1 = (u1 * atlas.width as f32).ceil() as u32;
        let y1 = (v1 * atlas.height as f32).ceil() as u32;
        if x1 - x0 != CELL || y1 - y0 != CELL {
            continue;
        }
        for row in 0..CELL {
            let dst = (((y0 + row) * atlas.width + x0) * 4) as usize;
            let src = (row * CELL * 4) as usize;
            atlas.rgba[dst..dst + (CELL * 4) as usize]
                .copy_from_slice(&rgba[src..src + (CELL * 4) as usize]);
        }
        replaced += 1;
    }
    replaced
}

fn vanilla_dye_color_name(color: u8) -> &'static str {
    [
        "white",
        "orange",
        "magenta",
        "light_blue",
        "yellow",
        "lime",
        "pink",
        "gray",
        "light_gray",
        "cyan",
        "purple",
        "blue",
        "brown",
        "green",
        "red",
        "black",
    ][usize::from(color.min(15))]
}

fn vanilla_villager_biome_name(index: i32) -> Option<&'static str> {
    [
        "desert", "jungle", "plains", "savanna", "snow", "swamp", "taiga",
    ]
    .get(usize::try_from(index).ok()?)
    .copied()
}

fn vanilla_villager_profession_name(index: i32) -> Option<&'static str> {
    [
        "none",
        "armorer",
        "butcher",
        "cartographer",
        "cleric",
        "farmer",
        "fisherman",
        "fletcher",
        "leatherworker",
        "librarian",
        "mason",
        "nitwit",
        "shepherd",
        "toolsmith",
        "weaponsmith",
    ]
    .get(usize::try_from(index).ok()?)
    .copied()
}

fn entity_appearance_query<'a>(
    live: &LiveSession,
    eid: i32,
    spec: EntityRigSpec,
    tracked: Option<&TrackedEntity>,
) -> EntityAppearanceQuery<'a> {
    let visual = live.world.entity_visual_state(eid);
    let villager = visual.as_ref().and_then(|state| state.villager_data);
    EntityAppearanceQuery {
        entity_type_id: Some(spec.entity_type_id),
        entity_name: Some(spec.name),
        uuid: tracked.map(|entity| entity.uuid),
        color: visual
            .as_ref()
            .and_then(|state| state.sheep_color)
            .map(vanilla_dye_color_name),
        sheared: visual
            .as_ref()
            .and_then(|state| state.sheep_sheared)
            .unwrap_or(false),
        powered: visual
            .as_ref()
            .and_then(|state| state.creeper_charged)
            .unwrap_or(false),
        angry: visual
            .as_ref()
            .and_then(|state| state.warden_anger)
            .is_some_and(|anger| anger > 0),
        villager_biome: villager.and_then(|(biome, _, _)| vanilla_villager_biome_name(biome)),
        villager_profession: villager
            .and_then(|(_, profession, _)| vanilla_villager_profession_name(profession)),
        villager_level: villager
            .map(|(_, _, level)| u8::try_from(level.clamp(0, 5)).unwrap_or(0))
            .unwrap_or(0),
        ..EntityAppearanceQuery::default()
    }
}

fn entity_appearance_cache_key(
    live: &LiveSession,
    eid: i32,
    spec: EntityRigSpec,
    tracked: Option<&TrackedEntity>,
    revision: u64,
) -> u64 {
    let mut hash = FNV1A64_OFFSET_BASIS;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    };
    mix(revision);
    mix(u64::from(spec.entity_type_id));
    if let Some(uuid) = tracked.map(|entity| entity.uuid) {
        for byte in uuid {
            mix(u64::from(byte));
        }
    }
    if let Some(visual) = live.world.entity_visual_state(eid) {
        mix(u64::from(visual.sheep_color.unwrap_or(255)));
        mix(u64::from(visual.sheep_sheared.unwrap_or(false)));
        mix(u64::from(visual.creeper_charged.unwrap_or(false)));
        if let Some((biome, profession, level)) = visual.villager_data {
            mix((biome as u64) << 32 | (profession as u64) << 8 | level as u64);
        }
        mix(visual.warden_anger.unwrap_or(0).max(0) as u64);
    }
    if let Some(tracked) = tracked {
        mix(u64::from(tracked.equipment_slots));
    }
    mix(live.entity_mainhand_item_id(eid).unwrap_or(0).max(0) as u64);
    hash
}

fn extended_entity_pose_key(mut key: u64, pose: &EntityRigPose) -> u64 {
    for value in [pose.open_amount, pose.swell_amount] {
        key ^= (f64::from(value) * 64.0).round() as i64 as u64;
        key = key.wrapping_mul(FNV1A64_PRIME);
    }
    let flags = u64::from(pose.sitting)
        | u64::from(pose.sleeping) << 1
        | u64::from(pose.spellcasting) << 2
        | u64::from(pose.using_item) << 3
        | u64::from(pose.armor_visible) << 4
        | u64::from(pose.main_hand_item) << 5
        | u64::from(pose.offhand_item) << 6
        | u64::from(pose.carried_block) << 7
        | u64::from(pose.wither_armored) << 8
        | u64::from(pose.rolled_up) << 9
        | u64::from(pose.screaming) << 10;
    (key ^ flags).wrapping_mul(FNV1A64_PRIME)
}

/// Per-entity tint: hurt flash overrides, otherwise the golden-ratio
/// hue keyed on the rig's type id (the same convention the software
/// renderer uses for its entity palette).
fn live_entity_tint(spec: EntityRigSpec, tracked: Option<&TrackedEntity>) -> [f32; 3] {
    if tracked.is_some_and(|t| t.hurt_ticks > 0) {
        return [1.0, 0.45, 0.45];
    }
    let hue = (f64::from(spec.entity_type_id) * 0.618_033_988_749_895).fract() as f32;
    hsv_to_rgb_f32(hue, 0.5, 0.85)
}

fn live_entity_tint_interpolated(spec: EntityRigSpec, hurt_ticks: f32) -> [f32; 3] {
    let base = live_entity_tint(spec, None);
    let hurt = [1.0, 0.45, 0.45];
    let amount = (hurt_ticks / 10.0).clamp(0.0, 1.0);
    std::array::from_fn(|channel| base[channel] + (hurt[channel] - base[channel]) * amount)
}

/// Deterministic vibrant colour for the fox's held mouth item — the
/// golden-ratio hue walk [`live_entity_tint`] uses, keyed on the item id
/// so distinct pickups tint the mouth prism differently.
#[cfg(test)]
fn mouth_item_rgb(item_id: i32) -> [u8; 3] {
    let hue = (f64::from(item_id as u32) * 0.618_033_988_749_895).fract() as f32;
    hsv_to_rgb_f32(hue, 0.85, 0.95).map(|c| (c * 255.0).round() as u8)
}

/// Quad-center UV the whole-mesh 3D glint tint samples — the scroll
/// comes from [`LiveSession::enchanted_item_quad_tint`]'s shimmer clock,
/// so one representative texel coordinate per entity is enough.
const GLINT_SAMPLE_UV: [f32; 2] = [0.5, 0.5];

/// Minimal enchanted stack standing in for the session-private equipment
/// slots: [`LiveSession::entity_enchanted_item_glint`] already proved a
/// glinted held/armor stack exists, but the decoded
/// [`ItemStackComponents`] for equipment never leaves `live_session`, so
/// this proxy unlocks the shared
/// [`LiveSession::enchanted_item_quad_tint`] blend. Component id `9` is
/// `minecraft:enchantments` with a non-empty entry count.
fn equipment_glint_proxy() -> &'static ItemStackComponents {
    static PROXY: std::sync::OnceLock<ItemStackComponents> = std::sync::OnceLock::new();
    PROXY.get_or_init(|| ItemStackComponents {
        count: 1,
        item_id: 1,
        added: vec![ItemComponentPatch {
            type_id: 9,
            data: vec![1],
        }],
        removed: Vec::new(),
    })
}

/// One live entity staged into the entity/BER draw set: the rig mesh
/// rebuilds only when the pose fingerprint changes, then
/// `(eid, world origin, tint)` joins the draw refs. While the
/// `enchantment_glint` video toggle is on, an entity carrying an
/// enchanted dropped/held/armor stack gets the violet 3D shimmer folded
/// into its tint through [`LiveSession::enchanted_item_quad_tint`].
#[allow(clippy::too_many_arguments)]
fn stage_live_entity(
    live: &LiveSession,
    cache: &mut LiveEntityRenderCache,
    entity_refs: &mut Vec<(i32, [f32; 3], [f32; 3])>,
    glint_on: bool,
    appearance: Option<&RuntimeEntityAppearanceAtlas>,
    eid: i32,
    spec: EntityRigSpec,
    pos: [f64; 3],
    tracked: Option<&TrackedEntity>,
    dt: f32,
) {
    let sample = interpolate_entity(cache, eid, pos, tracked, dt);
    let mut pose = entity_rig_pose(live, eid, tracked);
    pose.base.head_yaw_deg = sample.head_yaw - sample.body_yaw;
    pose.base.head_pitch_deg = sample.pitch;
    pose.base.hurt_flash = sample.hurt_ticks > 0.5;
    pose.base.limb_swing = sample.limb_swing;
    pose.base.limb_swing_amount = sample.limb_swing_amount;
    // The fox (id 42) carries pickups in its mouth slot: the held
    // item id joins the cache key so pick-up/drop rebuilds the
    // mesh immediately, without waiting on a pose change.
    let mouth_item_id = if spec.entity_type_id == FOX_ENTITY_TYPE_ID {
        live.entity_mainhand_item_id(eid).filter(|id| *id != 0)
    } else {
        None
    };
    let mut key = extended_entity_pose_key(entity_pose_key(spec, &pose.base, mouth_item_id), &pose);
    key ^= (f64::from(sample.body_yaw) * 64.0).round() as i64 as u64;
    key = key.wrapping_mul(FNV1A64_PRIME);
    if cache.entity_meshes.get(&eid).is_none_or(|(k, _)| *k != key) {
        let mut mesh = build_entity_rig_mesh_with_pose(spec, pose);
        let angle = sample.body_yaw.to_radians();
        let (sin, cos) = angle.sin_cos();
        mesh.affine(
            [[cos, 0.0, sin], [0.0, 1.0, 0.0], [-sin, 0.0, cos]],
            [0.0; 3],
            [0.0; 3],
        );
        cache.entity_meshes.insert(eid, (key, mesh));
    }
    if let Some(appearance) = appearance {
        let visual_key =
            entity_appearance_cache_key(live, eid, spec, tracked, appearance.revision());
        if cache
            .entity_visuals
            .get(&eid)
            .is_none_or(|(key, _)| *key != visual_key)
        {
            let query = entity_appearance_query(live, eid, spec, tracked);
            let resolved = appearance.resolve_entity(&query);
            cache.entity_visuals.insert(eid, (visual_key, resolved));
        }
    }
    let mut tint = live_entity_tint_interpolated(spec, sample.hurt_ticks);
    if glint_on && live.entity_enchanted_item_glint(eid) {
        tint = match live
            .entity_metadata
            .get(&eid)
            .and_then(|meta| meta.item_stack.as_ref())
        {
            // Dropped enchanted stack: its real components drive the tint.
            Some(stack) => live.enchanted_item_quad_tint(stack, tint, GLINT_SAMPLE_UV),
            None => live.enchanted_item_quad_tint(equipment_glint_proxy(), tint, GLINT_SAMPLE_UV),
        };
    }
    entity_refs.push((
        eid,
        [
            sample.position[0] as f32,
            sample.position[1] as f32,
            sample.position[2] as f32,
        ],
        tint,
    ));
}

/// Tracked entities the vanilla rig table left unclassified — modded or
/// out-of-range registry ids `add_entity` could not resolve — each paired
/// with the visible `GenericProxy` fallback spec from
/// [`classify_entity_rig_or_fallback`] so nothing modded ever renders
/// invisible.
fn unrigged_entity_fallbacks(
    live: &LiveSession,
) -> impl Iterator<Item = (i32, EntityRigSpec, [f64; 3], &TrackedEntity)> + '_ {
    live.world
        .entities
        .iter()
        .filter(|(eid, _)| live.entity_rig(**eid).is_none())
        .map(|(eid, tracked)| {
            let spec = classify_entity_rig_or_fallback(
                u32::try_from(tracked.entity_type.max(0)).unwrap_or(u32::MAX),
                None,
            );
            (*eid, spec, tracked.pos, tracked)
        })
}

type StaticBerRef = (usize, [f32; 3], [f32; 3], Option<[f32; 4]>);
type AnimatedBerRef = ([i32; 3], [f32; 3], Option<[f32; 4]>);

fn vanilla_ber_appearance_uv(
    appearance: Option<&RuntimeEntityAppearanceAtlas>,
    kind: VanillaBerKind,
    visual: Option<&megu_client::world::BlockEntityVisualState>,
) -> Option<[f32; 4]> {
    let appearance = appearance?;
    let kind_name = match kind {
        VanillaBerKind::Bed => "bed",
        VanillaBerKind::Chest => "chest",
        VanillaBerKind::ShulkerBox => "shulker_box",
        VanillaBerKind::Sign => "sign",
        VanillaBerKind::Banner => "banner",
        _ => return None,
    };
    let variant = match visual {
        Some(megu_client::world::BlockEntityVisualState::ShulkerBox { color, .. })
        | Some(megu_client::world::BlockEntityVisualState::Banner {
            base_color: color, ..
        }) => vanilla_dye_color_name(*color),
        _ => "",
    };
    appearance.resolve_ber(kind_name, variant)
}

/// The live contact shadows to stage this refresh — the session's
/// grounded-mob quads converted for the entity/BER lit list, or empty
/// while the `entity_shadows` gate is off so the toggle kills every
/// contact shadow the same refresh it flips.
fn stage_contact_shadow_quads(
    live: &LiveSession,
    enabled: bool,
    out: &mut Vec<(ExtrudedMesh, [f32; 3])>,
) {
    out.clear();
    if !enabled {
        return;
    }
    out.extend(
        live.entity_contact_shadow_quads()
            .into_iter()
            .map(contact_shadow_quad_mesh),
    );
}

/// Convert one live [`EntityContactShadowQuad`] into entity/BER staging
/// form: a single-quad [`ExtrudedMesh`] (world-space positions, `[0,1,0]`
/// normal, unit-square UVs, untinted tag) plus the shadow's rgba folded
/// to an rgb tint. The lit pipeline has no alpha lane, so alpha lerps
/// the tint from white down to the shadow color — grounded mobs get the
/// full ~30% darkening and the quad fades toward invisible as the entity
/// leaves the floor, mirroring the shadow's alpha ramp.
fn contact_shadow_quad_mesh(quad: EntityContactShadowQuad) -> (ExtrudedMesh, [f32; 3]) {
    let alpha = quad.rgba[3].clamp(0.0, 1.0);
    let tint = [
        alpha.mul_add(quad.rgba[0] - 1.0, 1.0),
        alpha.mul_add(quad.rgba[1] - 1.0, 1.0),
        alpha.mul_add(quad.rgba[2] - 1.0, 1.0),
    ];
    let mesh = ExtrudedMesh {
        quads: vec![ExtrudedQuad {
            positions: quad.positions,
            normal: quad.normal,
            uv: [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            tint_index: -1,
        }],
    };
    (mesh, tint)
}

/// `hsv_to_rgb` for `0..=1` float inputs.
fn hsv_to_rgb_f32(h: f32, s: f32, v: f32) -> [f32; 3] {
    let i = (h * 6.0).floor() as i32;
    let f = h * 6.0 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r, g, b]
}

/// Push one quad — center + two half-edge vectors — in the corner order
/// and unit-square UV basis [`VanillaParticleWorld::billboard_quads`]
/// emits, so instanced packing (`pack_particle_billboard`) accepts it.
/// `axis_u`/`axis_v` may be any parallelogram edges: camera billboards,
/// flat ground rings, or velocity-stretched streaks.
fn push_particle_quad(
    quads: &mut Vec<VanillaParticleQuad>,
    pos: [f32; 3],
    axis_u: [f32; 3],
    axis_v: [f32; 3],
    color: [f32; 4],
) {
    if !pos.iter().all(|v| v.is_finite()) || !color.iter().all(|v| v.is_finite()) {
        return;
    }
    let positions = [
        [
            pos[0] - axis_u[0] - axis_v[0],
            pos[1] - axis_u[1] - axis_v[1],
            pos[2] - axis_u[2] - axis_v[2],
        ],
        [
            pos[0] + axis_u[0] - axis_v[0],
            pos[1] + axis_u[1] - axis_v[1],
            pos[2] + axis_u[2] - axis_v[2],
        ],
        [
            pos[0] + axis_u[0] + axis_v[0],
            pos[1] + axis_u[1] + axis_v[1],
            pos[2] + axis_u[2] + axis_v[2],
        ],
        [
            pos[0] - axis_u[0] + axis_v[0],
            pos[1] - axis_u[1] + axis_v[1],
            pos[2] - axis_u[2] + axis_v[2],
        ],
    ];
    if positions.iter().flatten().all(|v| v.is_finite()) {
        quads.push(VanillaParticleQuad {
            kind: megu_client_mesh::ber_instancing::VanillaParticleKind::PotionSwirl,
            positions,
            uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            color,
        });
    }
}

/// Camera-facing quad for a point particle of half-size `half`.
fn push_billboard(
    quads: &mut Vec<VanillaParticleQuad>,
    pos: [f32; 3],
    right: [f32; 3],
    up: [f32; 3],
    half: f32,
    color: [f32; 4],
) {
    if !half.is_finite() || half <= 0.0 {
        return;
    }
    push_particle_quad(
        quads,
        pos,
        right.map(|r| r * half),
        up.map(|u| u * half),
        color,
    );
}

/// Horizontal quad lying on the XZ plane (water ripples, dust rings,
/// ground decals).
fn push_flat_ring(quads: &mut Vec<VanillaParticleQuad>, pos: [f32; 3], half: f32, color: [f32; 4]) {
    if !half.is_finite() || half <= 0.0 {
        return;
    }
    push_particle_quad(quads, pos, [half, 0.0, 0.0], [0.0, 0.0, half], color);
}

/// Tactile sim particle → quad. `size` is the sim's radius; ring kinds
/// lie flat on XZ (they expand on water/ground surfaces), everything
/// else is a camera billboard.
fn push_tactile_particle(
    quads: &mut Vec<VanillaParticleQuad>,
    particle: &TactileParticle,
    right: [f32; 3],
    up: [f32; 3],
) {
    let half = particle.size.max(0.005);
    let flat = matches!(
        particle.kind,
        TactileParticleKind::WaterRippleRing { .. }
            | TactileParticleKind::BubblePopRing { .. }
            | TactileParticleKind::LandingDustRing { .. }
            | TactileParticleKind::DripLandingRing { .. }
            | TactileParticleKind::ShockRing { .. }
    );
    if flat {
        push_flat_ring(quads, particle.pos, half, particle.rgba);
    } else {
        push_billboard(quads, particle.pos, right, up, half, particle.rgba);
    }
}

fn push_nature_particle(
    quads: &mut Vec<VanillaParticleQuad>,
    particle: &NatureParticle,
    right: [f32; 3],
    up: [f32; 3],
) {
    // Gentle tail fade so expiring particles do not pop; the sim owns
    // TTL expiry either way.
    let life = (particle.age_sec / particle.ttl_sec.max(1e-3)).clamp(0.0, 1.0);
    let fade = 1.0 - life * life;
    let (mut color, half, flat) = match particle.kind {
        NatureParticleKind::FallingLeaf { rgb, .. } => {
            ([rgb[0], rgb[1], rgb[2], 0.85], 0.05, false)
        }
        NatureParticleKind::SakuraPetal { rgb, .. } => {
            ([rgb[0], rgb[1], rgb[2], 0.85], 0.045, false)
        }
        NatureParticleKind::Firefly { glow_rgb, .. } => {
            ([glow_rgb[0], glow_rgb[1], glow_rgb[2], 0.9], 0.03, false)
        }
        NatureParticleKind::RainRipple { radius, max_radius } => {
            let alpha = 0.45 * (1.0 - radius / max_radius.max(1e-3)).clamp(0.0, 1.0);
            ([0.70, 0.82, 0.95, alpha], radius.max(0.01), true)
        }
        NatureParticleKind::WaterfallMist { alpha } => {
            ([0.9, 0.93, 1.0, alpha.clamp(0.0, 1.0) * 0.6], 0.15, false)
        }
        NatureParticleKind::SnowMote { .. } => ([0.92, 0.95, 1.0, 0.8], 0.03, false),
    };
    color[3] *= fade;
    if flat {
        push_flat_ring(quads, particle.pos, half, color);
    } else {
        push_billboard(quads, particle.pos, right, up, half, color);
    }
}

/// VFX1 `ParticleDraw` → quad honoring the recorded orientation:
/// camera billboards apply the in-plane `rotation`, velocity-stretched
/// particles become streaks along their motion, and flat decals lie on
/// XZ. `AxisAligned` has no fixed axis stored on the draw, so it shares
/// the camera billboard path.
fn push_vfx_particle(
    quads: &mut Vec<VanillaParticleQuad>,
    particle: &ParticleDraw,
    right: [f32; 3],
    up: [f32; 3],
    forward: [f32; 3],
) {
    let half = particle.size.max(0.005);
    match particle.orientation {
        ParticleOrientation::FlatGroundDecal => {
            push_flat_ring(quads, particle.pos, half, particle.rgba);
        }
        ParticleOrientation::VelocityStretched => {
            let speed = (particle.velocity[0] * particle.velocity[0]
                + particle.velocity[1] * particle.velocity[1]
                + particle.velocity[2] * particle.velocity[2])
                .sqrt();
            if speed > 1e-4 {
                // Streak axis along the velocity (longer when faster);
                // the cross axis stays as camera-perpendicular as the
                // motion allows.
                let axis_u = particle.velocity.map(|v| v / speed * (half + speed * 0.5));
                let cx = axis_u[1] * forward[2] - axis_u[2] * forward[1];
                let cy = axis_u[2] * forward[0] - axis_u[0] * forward[2];
                let cz = axis_u[0] * forward[1] - axis_u[1] * forward[0];
                let len = (cx * cx + cy * cy + cz * cz).sqrt();
                let axis_v = if len > 1e-6 {
                    [cx / len * half, cy / len * half, cz / len * half]
                } else {
                    up.map(|u| u * half)
                };
                push_particle_quad(quads, particle.pos, axis_u, axis_v, particle.rgba);
            } else {
                push_billboard(quads, particle.pos, right, up, half, particle.rgba);
            }
        }
        _ => {
            // CameraBillboard + AxisAligned: rotate the billboard axes
            // in-plane by `rotation`.
            let (sin, cos) = particle.rotation.sin_cos();
            let r2 = [
                right[0] * cos + up[0] * sin,
                right[1] * cos + up[1] * sin,
                right[2] * cos + up[2] * sin,
            ];
            let u2 = [
                up[0] * cos - right[0] * sin,
                up[1] * cos - right[1] * sin,
                up[2] * cos - right[2] * sin,
            ];
            push_particle_quad(
                quads,
                particle.pos,
                r2.map(|v| v * half),
                u2.map(|v| v * half),
                particle.rgba,
            );
        }
    }
}

/// Populate the official Megu links on a windowed [`UiState`] without
/// altering [`UiState::default`] (which stays URL-empty for `client-ui` tests).
pub fn apply_megu_default_links(ui: &mut UiState) {
    ui.website_url = MEGU_WEBSITE_URL.to_owned();
    ui.support_url = MEGU_SUPPORT_URL.to_owned();
    ui.contribute_url = MEGU_CONTRIBUTE_URL.to_owned();
    ui.report_url = MEGU_REPORT_URL.to_owned();
}

/// Run the windowed client. Returns `Err` text when the event loop or GPU
/// setup fails (e.g. no display — use `--headless-probe` there).
pub fn run(config: GpuCliConfig) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("EventLoop::new: {e}"))?;
    let mut app = GpuApp::new(config);
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("run_app: {e}"))?;
    match app.fatal {
        Some(fatal) => Err(fatal),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_overlay_reuses_commands_until_layout_or_batch_changes() {
        use megu_ui::gui_bridge::GuiBridgeDrawOp;

        let mut app = GpuApp::new(GpuCliConfig::default());
        app.set_gui_bridge_batch(GuiBridgeBatch::new(vec![
            GuiBridgeDrawOp::PushScissor {
                x: 20,
                y: 20,
                w: 120,
                h: 120,
            },
            GuiBridgeDrawOp::FillRect {
                x: 0,
                y: 0,
                w: 256,
                h: 256,
                rgba: [255; 4],
            },
            GuiBridgeDrawOp::PopScissor,
        ]));

        assert!(app.refresh_mod_overlay([160, 160], 1));
        assert_eq!(
            app.ui_modoverlay_list.command_clip(0),
            Some(UiRect::new(20, 20, 120, 120))
        );
        assert!(!app.refresh_mod_overlay([160, 160], 1));

        assert!(app.refresh_mod_overlay([100, 160], 1));
        assert_eq!(
            app.ui_modoverlay_list.command_clip(0),
            Some(UiRect::new(20, 20, 80, 120))
        );
        assert!(app.refresh_mod_overlay([100, 160], 2));
        assert!(!app.refresh_mod_overlay([100, 160], 2));

        app.set_gui_bridge_batch(GuiBridgeBatch::default());
        app.refresh_mod_overlay([100, 160], 2);
        assert!(app.ui_modoverlay_list.commands.is_empty());
    }

    #[test]
    fn entity_interpolation_smooths_pose_locomotion_and_hurt_flash() {
        let mut live = LiveSession::offline_with_null_audio();
        window_test_add_entity(&mut live, 90, 124, [0.0, 64.0, 0.0]);
        let mut cache = LiveEntityRenderCache::default();
        {
            let tracked = live.world.entities.get_mut(&90).unwrap();
            tracked.yaw_deg = 350.0;
            tracked.head_yaw_deg = 350.0;
            tracked.pitch_deg = 10.0;
        }
        // One server refresh interval (50 ms) reproduces the legacy
        // constant-0.5 blend exactly, so the assertions still pin the curve.
        let dt = 0.05;
        let first = interpolate_entity(
            &mut cache,
            90,
            [0.0, 64.0, 0.0],
            live.world.entities.get(&90),
            dt,
        );
        assert_eq!(first.body_yaw, 350.0);

        {
            let tracked = live.world.entities.get_mut(&90).unwrap();
            tracked.yaw_deg = 10.0;
            tracked.head_yaw_deg = 10.0;
            tracked.pitch_deg = 20.0;
            tracked.hurt_ticks = 10;
            tracked.vel = [0.25, 0.0, 0.0];
        }
        let next = interpolate_entity(
            &mut cache,
            90,
            [2.0, 64.0, 0.0],
            live.world.entities.get(&90),
            dt,
        );
        // `exp_decay_exact` reproduces the legacy half-blend; the frame delta
        // arrives as f32, so the curve lands within a few ulps of 0.5.
        assert!((next.position[0] - 1.0).abs() < 1.0e-6);
        assert!(next.body_yaw.abs() < 1.0e-4);
        assert!(next.head_yaw.abs() < 1.0e-4);
        assert!((next.pitch - 15.0).abs() < 1.0e-5);
        assert!((next.hurt_ticks - 5.0).abs() < 1.0e-5);
        assert!((next.limb_swing - 8.0f32.rem_euclid(std::f32::consts::TAU)).abs() < 1.0e-5);
        assert!((next.limb_swing_amount - 0.5).abs() < 1.0e-5);
        assert_ne!(
            live_entity_tint_interpolated(*live.entity_rig(90).unwrap(), next.hurt_ticks),
            live_entity_tint(*live.entity_rig(90).unwrap(), None),
        );
    }

    #[test]
    fn missing_mod_disconnect_text_opens_structured_prompt() {
        let requirements =
            parse_missing_mod_disconnect("Missing required mods: fabric-api@0.102.0, sodium@0.6.5")
                .expect("recognizable dependency kick");
        assert_eq!(requirements.len(), 2);
        assert_eq!(requirements[0].mod_id, "fabric-api");
        assert_eq!(requirements[0].required_version, "0.102.0");
        assert_eq!(requirements[1].modrinth_slug.as_deref(), Some("sodium"));
        assert!(parse_missing_mod_disconnect("Kicked by an operator.").is_none());

        let mut app = GpuApp::new(GpuCliConfig::default());
        app.last_server_address = Some("example.test:25565".into());
        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Disconnected("Missing mods: example@1.2.3".into());
        app.live = Some(live);
        app.sync_disconnected_screen();
        assert_eq!(app.ui.screen, Screen::ModSyncPrompt);
        assert_eq!(app.ui.mod_sync_prompt.server_address, "example.test:25565");
        assert_eq!(app.ui.mod_sync_prompt.rows.len(), 1);
    }

    #[test]
    fn remapped_key_binding_routes_to_vanilla_action_and_persists_capture_name() {
        let mut bindings = megu_ui::state::KeyBindings {
            forward: "KeyI".to_owned(),
            ..megu_ui::state::KeyBindings::default()
        };
        assert_eq!(remap_bound_key(KeyCode::KeyI, &bindings), KeyCode::KeyW);
        assert_eq!(remap_bound_key(KeyCode::KeyW, &bindings), KeyCode::KeyW);
        assert!(bindings.set(0, format!("{:?}", KeyCode::KeyJ)));
        assert_eq!(bindings.forward, "KeyJ");
    }

    #[test]
    fn held_tool_components_drive_warning_and_final_use_guard() {
        let (_tx, incoming) = std::sync::mpsc::sync_channel(1);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(1);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.player_inventory.insert(
            36,
            megu_client::protocol::ItemStackComponents {
                count: 1,
                item_id: 1,
                added: vec![
                    megu_client::protocol::ItemComponentPatch {
                        type_id: 2,
                        data: vec![10],
                    },
                    megu_client::protocol::ItemComponentPatch {
                        type_id: 3,
                        data: vec![9],
                    },
                ],
                removed: Vec::new(),
            },
        );
        let (damage, max_damage) =
            held_tool_durability(&live, LiveSession::MAIN_HAND).expect("durability components");
        assert_eq!(
            check_tool_durability_warning(damage, max_damage, 10, false),
            ToolDurability::LowWarning { remaining: 1 }
        );
        assert_eq!(
            check_tool_durability_warning(damage, max_damage, 10, true),
            ToolDurability::BlockLastUse
        );
    }

    #[test]
    fn fullscreen_and_invert_y_helpers_change_runtime_input_state() {
        let mut settings = UiSettings::default();
        toggle_fullscreen_setting(&mut settings);
        assert!(settings.fullscreen);
        toggle_fullscreen_setting(&mut settings);
        assert!(!settings.fullscreen);
        assert_eq!(mouse_pitch_delta(3.5, false), 3.5);
        assert_eq!(mouse_pitch_delta(3.5, true), -3.5);
        assert_eq!(effective_camera_far(8, LodMode::Off), 128.0);
        assert_eq!(effective_camera_far(8, LodMode::Medium), 1024.0);
        assert_eq!(effective_camera_far(8, LodMode::Far), 2048.0);
        let chat = vec!["old line".to_owned()];
        assert_eq!(retained_chat_on_reconnect(true, &chat), chat);
        assert!(retained_chat_on_reconnect(false, &chat).is_empty());
    }

    #[test]
    fn first_run_options_txt_import_seeds_the_live_settings_model() {
        let dir =
            std::env::temp_dir().join(format!("megu-gpu-options-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create game dir");
        std::fs::write(
            dir.join("options.txt"),
            "renderDistance:8\nfov:0.75\nenableVsync:false\nao:0\ninvertYMouse:true\ngamma:0.5\nguiScale:2\nfullscreen:true\n",
        )
        .expect("write vanilla options");
        let app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            offline_demo: true,
            ..GpuCliConfig::default()
        });
        assert_eq!(app.persisted_settings.view_distance, 8);
        assert_eq!(app.ui.settings.fov_deg, 90);
        assert!(!app.ui.settings.video.vsync);
        assert!(!app.ui.settings.video.smooth_lighting);
        assert!(app.ui.settings.mouse_invert_y);
        assert_eq!(app.ui.settings.gamma_pct, 150);
        assert_eq!(app.ui.settings.gui_scale, 2);
        assert!(app.ui.settings.fullscreen);
        assert!(!dir.join(megu_client::settings::SETTINGS_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn megu_default_links_wire_into_gpu_app_via_about_screen() {
        // `UiState::default()` stays empty so `client-ui` default-URL tests stay intact.
        let bare = UiState::default();
        assert!(bare.website_url.is_empty());
        assert!(bare.support_url.is_empty());
        assert!(bare.contribute_url.is_empty());
        assert!(bare.report_url.is_empty());

        let mut app = GpuApp::new(GpuCliConfig::default());
        assert_eq!(app.ui.website_url, "https://megu.megumin.tech/");
        assert_eq!(app.ui.support_url, "https://megu.megumin.tech/#support");
        assert_eq!(
            app.ui.contribute_url,
            "https://github.com/nekomario28/megu-mc"
        );
        assert_eq!(app.ui.report_url, "https://megu.megumin.tech/#report");
        assert_eq!(app.ui.screen, Screen::Title);
        // Vanilla-faithful QoL defaults keep the support-link row off.
        assert!(!app.ui.settings.qol.show_support_links);

        // Vanilla-faithful title: no link row ever reaches Title, with the
        // support-links toggle on or off — every link lives on About Megu.
        for support in [true, false] {
            app.ui.settings.qol.show_support_links = support;
            let _ = app.ui.render(1280, 720);
            for id in [
                megu_ui::state::WidgetId::WebsiteLink,
                megu_ui::state::WidgetId::SupportLink,
                megu_ui::state::WidgetId::ContributeLink,
                megu_ui::state::WidgetId::ReportLink,
            ] {
                assert!(
                    !app.ui.hit_regions.iter().any(|h| h.id == id),
                    "{id:?} must never render on Title (show_support_links={support})"
                );
            }
        }

        // The `?` chip navigates to About Megu where all 4 links open Megu URLs.
        let _ = app.ui.render(1280, 720);
        let about = app
            .ui
            .hit_regions
            .iter()
            .find(|h| h.id == megu_ui::state::WidgetId::TitleAbout)
            .cloned()
            .expect("expected TitleAbout chip on Title");
        let acts = app.ui.handle_input(megu_ui::state::UiInputEvent::Click {
            x: about.rect.x + 2,
            y: about.rect.y + 2,
        });
        assert_eq!(
            acts,
            vec![UiAction::OpenScreen(megu_ui::state::Screen::AboutMegu)]
        );
        assert_eq!(app.ui.screen, megu_ui::state::Screen::AboutMegu);

        let _ = app.ui.render(1280, 720);
        for (id, expected) in [
            (
                megu_ui::state::WidgetId::WebsiteLink,
                "https://megu.megumin.tech/",
            ),
            (
                megu_ui::state::WidgetId::SupportLink,
                "https://megu.megumin.tech/#support",
            ),
            (
                megu_ui::state::WidgetId::ContributeLink,
                "https://github.com/nekomario28/megu-mc",
            ),
            (
                megu_ui::state::WidgetId::ReportLink,
                "https://megu.megumin.tech/#report",
            ),
        ] {
            let hit = app
                .ui
                .hit_regions
                .iter()
                .find(|h| h.id == id)
                .cloned()
                .expect("expected link hit region on About Megu");
            let acts = app.ui.handle_input(megu_ui::state::UiInputEvent::Click {
                x: hit.rect.x + 2,
                y: hit.rect.y + 2,
            });
            assert_eq!(acts, vec![UiAction::OpenUrl(expected.to_owned())]);
        }
    }

    #[test]
    fn submit_crash_report_honors_off_mode_and_writes_redacted_local_report() {
        let dir = std::env::temp_dir().join(format!("megu-gpu-crash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        };
        let mut app = GpuApp::new(cfg);
        app.ui.crash = Some(megu_core::crash_report::CrashContext {
            error_kind: "panic".to_string(),
            culprit_mod_or_module: "client-gpu".to_string(),
            mc_version: "1.21.1".to_string(),
            detail_message: "panic at /home/alice/secret with 192.168.1.9".to_string(),
        });

        // Mode Off: writes nothing.
        app.ui.settings.qol.crash_report_mode = megu_ui::state::CrashReportMode::Off;
        app.handle_submit_crash_report();
        assert!(
            !dir.join("crash-reports").exists(),
            "CrashReportMode::Off must not create crash-reports/"
        );

        // Mode Ask (after explicit UI confirmation emits SubmitCrashReport): writes redacted report.
        app.ui.settings.qol.crash_report_mode = megu_ui::state::CrashReportMode::Ask;
        app.handle_submit_crash_report();
        let reports: Vec<_> = std::fs::read_dir(dir.join("crash-reports"))
            .expect("crash-reports dir created")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(reports.len(), 1);
        let body = std::fs::read_to_string(reports[0].path()).unwrap();
        assert!(!body.contains("/home/alice"), "home path must be redacted");
        assert!(!body.contains("192.168.1.9"), "IP must be redacted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_saved_worlds_lists_only_directories_sorted() {
        let dir = std::env::temp_dir().join(format!("megu-gpu-saves-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saves = dir.join("saves");
        std::fs::create_dir_all(saves.join("Beta World")).unwrap();
        std::fs::create_dir_all(saves.join("alpha world")).unwrap();
        std::fs::write(saves.join("options.txt"), b"not a world").unwrap();

        assert_eq!(
            list_saved_worlds(Some(&dir)),
            vec!["Beta World".to_string(), "alpha world".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);

        // A missing saves/ dir yields an empty list, never a panic.
        assert!(list_saved_worlds(Some(&dir)).is_empty());
    }

    #[test]
    fn singleplayer_screen_populates_worlds_from_game_dir() {
        let dir = std::env::temp_dir().join(format!("megu-gpu-sp-worlds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("saves").join("Test World")).unwrap();

        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });
        assert_eq!(app.ui.singleplayer.worlds, vec!["Test World".to_string()]);

        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Singleplayer));
        assert_eq!(app.ui.screen, Screen::Singleplayer);
        assert_eq!(app.ui.singleplayer.worlds, vec!["Test World".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn play_world_launch_failure_surfaces_visible_status() {
        let mut app = GpuApp::new(GpuCliConfig {
            server_bin: Some(std::path::PathBuf::from("/nonexistent/megu-integrated")),
            ..GpuCliConfig::default()
        });
        app.dispatch_ui_action(&UiAction::PlayWorld {
            name: "New World".to_string(),
            seed: Some(7),
        });
        // The failure is visible on Title/Singleplayer screens, not just
        // the HUD chat line those screens never draw (BUG-SP2).
        let status = app.ui.title_status.as_deref().unwrap_or_default();
        assert!(
            status.contains("Singleplayer") && status.contains("New World"),
            "title_status did not capture the failure: {status:?}"
        );
        assert!(
            app.status_messages()
            .iter()
                .any(|m| m.contains("Singleplayer"))
        );
    }

    #[test]
    fn host_ram_pressure_trims_on_low_available_and_ignores_missing_data() {
        let snap = |total: Option<u64>, avail: Option<u64>, rss: Option<u64>| HostMetricsSnapshot {
            mem_total_mib: total,
            mem_available_mib: avail,
            process_rss_mib: rss,
            ..HostMetricsSnapshot::default()
        };
        // No memory data at all -> the guard idles (fail-safe).
        assert_eq!(
            host_ram_pressure(&HostMetricsSnapshot::default(), 0, 8192),
            HostRamPressure::Clear
        );
        // Comfortable headroom on an 8 GiB host -> Clear.
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(6000), Some(1500)), 0, 8192),
            HostRamPressure::Clear
        );
        // Available under the 10% reserve (800 MiB) -> Trim.
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(700), Some(1500)), 0, 8192),
            HostRamPressure::Trim
        );
        // Under half the reserve (or the 256 MiB floor) -> Emergency,
        // even with a huge user cap — the cap never disables the guard.
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(300), Some(1500)), u32::MAX, 8192),
            HostRamPressure::Emergency
        );
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(200), Some(1500)), 0, 8192),
            HostRamPressure::Emergency
        );
        // Auto budget = total/4 clamped [1 GiB, 8 GiB]: RSS over it -> Trim.
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(6000), Some(3000)), 0, 8192),
            HostRamPressure::Trim
        );
        // A user cap above the Auto share trims sooner; below it, later.
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(6000), Some(2500)), 2048, 8192),
            HostRamPressure::Trim
        );
        assert_eq!(
            host_ram_pressure(&snap(Some(8192), Some(6000), Some(2500)), 4096, 8192),
            HostRamPressure::Clear
        );
        // Missing total falls back to the UI-detected host total.
        assert_eq!(
            host_ram_pressure(&snap(None, Some(6000), Some(9000)), 0, 8192),
            HostRamPressure::Trim
        );
    }

    #[test]
    fn ui_upload_cache_uploads_first_frame_and_skips_identical() {
        let mut cache = UiUploadCache::default();
        // First frame always uploads.
        assert!(cache.should_upload(1280, 720, 0xaaaa));
        assert_eq!(cache.ui_uploads_performed, 1);
        assert_eq!(cache.ui_uploads_skipped, 0);
        // An identical second frame (same size + fingerprint) is skipped.
        assert!(!cache.should_upload(1280, 720, 0xaaaa));
        assert!(!cache.should_upload(1280, 720, 0xaaaa));
        assert_eq!(cache.ui_uploads_performed, 1);
        assert_eq!(cache.ui_uploads_skipped, 2);
    }

    #[test]
    fn ui_upload_cache_reuploads_on_hud_change_and_resize() {
        let mut cache = UiUploadCache::default();
        assert!(cache.should_upload(1280, 720, 0xaaaa));
        // HUD text updated -> new fingerprint -> upload.
        assert!(cache.should_upload(1280, 720, 0xbbbb));
        // Resize -> upload even when the fingerprint collides.
        assert!(cache.should_upload(1920, 1080, 0xbbbb));
        assert_eq!(cache.ui_uploads_performed, 3);
        // Steady state resumes skipping.
        assert!(!cache.should_upload(1920, 1080, 0xbbbb));
        assert_eq!(cache.ui_uploads_skipped, 1);
    }

    /// The window half of the death loop, driven through the same
    /// `sync_death_screen`/`dispatch_ui_action` path `render_frame` and
    /// the event handlers use: kill packet -> `Screen::Death` -> Respawn
    /// click -> `client_command(0)` queued -> server recovery -> HUD.
    #[test]
    fn death_to_respawn_drives_screen_death_then_hud() {
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let mut app = GpuApp::new(GpuCliConfig::default());
        // A real outgoing channel pair: the receiver end must stay alive
        // or `request_respawn`'s `try_send` fails as disconnected.
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.pose = Some(PlayerPose {
            feet: [0.0; 3],
            yaw: 0.0,
            pitch: 0.0,
        });
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;

        // Anonymous-NBT `{"text":"..."}` — the kill packet's message.
        let mut kill_nbt = vec![10u8, 8];
        kill_nbt.extend_from_slice(&4u16.to_be_bytes());
        kill_nbt.extend_from_slice(b"text");
        let msg = "Tester was slain by Creeper";
        kill_nbt.extend_from_slice(&(msg.len() as u16).to_be_bytes());
        kill_nbt.extend_from_slice(msg.as_bytes());
        kill_nbt.push(0);
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::PlayerCombatKill {
                player_id: 11,
                message_nbt: kill_nbt,
            }))
            .unwrap();

        // The per-frame sync turns death into Screen::Death with the
        // decoded cause; the cursor policy un-grabs (not InGameHud).
        app.sync_death_screen();
        assert_eq!(app.ui.screen, Screen::Death);
        assert_eq!(app.ui.death.cause_message.as_deref(), Some(msg));
        assert!(!app.wants_cursor_grab());
        assert!(app.ui_wants_input());

        // Respawn click -> UiAction::PerformRespawn -> the host queues
        // `client_command(0)` (`respawn_pending` proves dispatch; the
        // WorkerCommand variant itself is covered in live_session tests).
        // Vanilla 1.21.1 locks the primary death button for the first 20 ticks
        // to prevent accidental clicks on death.
        for _ in 0..megu_ui::vanilla_metrics::DEATH_ENABLE_DELAY_TICKS {
            app.ui.tick_death();
        }
        app.ui.render(1280, 720);
        let region = app
            .ui
            .hit_regions
            .iter()
            .find(|r| r.id == megu_ui::state::WidgetId::DeathRespawn)
            .copied()
            .expect("respawn button region");
        let actions = app.ui.handle_input(UiInputEvent::Click {
            x: region.rect.x + 2,
            y: region.rect.y + 2,
        });
        assert_eq!(actions, vec![UiAction::PerformRespawn]);
        for action in &actions {
            app.dispatch_ui_action(action);
        }
        assert!(
            app.live.as_ref().unwrap().death.respawn_pending(),
            "PerformRespawn dispatch must queue the packet"
        );
        // Still dead — nothing marked the player alive yet.
        app.sync_death_screen();
        assert_eq!(app.ui.screen, Screen::Death);

        // Server recovery clears death; the next sync returns to the HUD.
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::Respawn {
                dimension: DimensionMeta::default(),
            }))
            .unwrap();
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
                health: 20.0,
                food: 20,
                saturation: 5.0,
            }))
            .unwrap();
        app.sync_death_screen();
        assert_eq!(app.ui.screen, Screen::InGameHud);
        assert!(app.ui.death.cause_message.is_none());
    }

    /// `Title Screen` on the death leaf drops the live session — the
    /// `LeaveWorld` action's whole job.
    #[test]
    fn death_screen_title_button_leaves_world() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui
            .show_death(megu_ui::death_screen::DeathScreenState::default());

        // The real click path: the widget emits `LeaveWorld` while the
        // state machine navigates to Title; dispatch drops the session.
        app.ui.render(1280, 720);
        let region = app
            .ui
            .hit_regions
            .iter()
            .find(|r| r.id == megu_ui::state::WidgetId::DeathTitleScreen)
            .copied()
            .expect("title button region");
        let actions = app.ui.handle_input(UiInputEvent::Click {
            x: region.rect.x + 2,
            y: region.rect.y + 2,
        });
        assert_eq!(actions, vec![UiAction::LeaveWorld]);
        for action in &actions {
            app.dispatch_ui_action(action);
        }
        assert!(app.live.is_none(), "LeaveWorld must drop the session");
        assert_eq!(app.ui.screen, Screen::Title);
    }

    /// Pause-menu `Save and Quit to Title` must drop the live session the
    /// same way — the leak this fixes let the socket survive to Title.
    #[test]
    fn pause_save_quit_leaves_world() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.navigate(Screen::Pause);

        app.ui.render(1280, 720);
        let region = app
            .ui
            .hit_regions
            .iter()
            .find(|r| r.id == megu_ui::state::WidgetId::PauseSaveQuit)
            .copied()
            .expect("save & quit region");
        let actions = app.ui.handle_input(UiInputEvent::Click {
            x: region.rect.x + 2,
            y: region.rect.y + 2,
        });
        assert_eq!(actions, vec![UiAction::LeaveWorld]);
        for action in &actions {
            app.dispatch_ui_action(action);
        }
        assert!(app.live.is_none(), "Save & Quit must drop the session");
        assert_eq!(app.ui.screen, Screen::Title);
    }

    /// Grabbed in-game clicks dispatch the vanilla crosshair order:
    /// entity-nearer-than-block -> `interact` attack/`interact_at`, then
    /// `swing`; air right-click falls back to `use_item`.
    #[test]
    fn grabbed_world_clicks_dispatch_attack_and_interact() {
        use crate::live_session::WorkerCommand;
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let (tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(16);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid: 42,
            uuid: [3; 16],
            entity_type: 124,
            x: 0.0,
            y: 64.0,
            z: 3.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);
        app.ui.navigate(Screen::InGameHud);

        // Left click on the entity ahead: attack + swing.
        app.world_mouse_input(ElementState::Pressed, MouseButton::Left);
        // Right click on it: interact_at (hit position) + swing.
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);

        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        assert!(
            commands.iter().any(|c| matches!(
                c,
                WorkerCommand::Interact {
                    target: 42,
                    action: 1,
                    ..
                }
            )),
            "left click on an entity must enqueue attack"
        );
        assert!(commands.iter().any(|c| matches!(
            c,
            WorkerCommand::Interact {
                target: 42,
                action: 2,
                at: Some(_),
                ..
            }
        )));
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, WorkerCommand::Swing { hand: 0 }))
                .count(),
            2,
            "each click pairs with a main-hand swing"
        );

        // Off-entity right click on air falls back to `use_item`.
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::RemoveEntities {
                eids: vec![42],
            }))
            .unwrap();
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, WorkerCommand::UseItem { .. })),
            "air right-click must enqueue use_item"
        );

        // Dead players send nothing.
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::SetHealth {
                health: 0.0,
                food: 20,
                saturation: 5.0,
            }))
            .unwrap();
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        assert!(
            rx.try_recv().is_err(),
            "dead player input must not reach the wire"
        );
    }

    /// Hotbar slot keys (and the scroll handler that reuses the same
    /// event) echo the held slot to the server.
    #[test]
    fn hotbar_selection_sends_set_carried_item() {
        use crate::live_session::WorkerCommand;

        let (tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(8);
        let _ = tx;
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);

        app.dispatch_input(WindowInputEvent::SelectHotbar(5));
        assert_eq!(app.ui.hud.selected_slot, 5);
        assert!(
            matches!(rx.try_recv(), Ok(WorkerCommand::SetCarriedItem { slot: 5 })),
            "slot change must echo set_carried_item to the server"
        );
    }

    /// C3 regression: debug-chord chat lands in `live.hud` once, the
    /// per-frame HUD sync mirrors it wholesale, and repeated "frames"
    /// never duplicate or grow the line list past the 100-line cap.
    #[test]
    fn chat_lines_bounded_and_never_duplicated_across_frames() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(LiveSession::offline_with_null_audio());
        app.ui.settings.qol.chat_history_lines = 100;
        let settings = app.ui.settings.clone();
        app.apply_settings(&settings);
        for i in 0..140 {
            app.dispatch_input(WindowInputEvent::DebugChord(
                DebugChordAction::ChatFeedback(format!("line {i}")),
            ));
        }
        // Simulate 300 rendered frames worth of HUD syncs.
        for _ in 0..300 {
            app.live.as_mut().unwrap().sync_hud_to(&mut app.ui.hud);
        }
        assert!(
            app.ui.hud.chat_lines.len() <= 100,
            "chat_lines must stay bounded, got {}",
            app.ui.hud.chat_lines.len()
        );
        assert_eq!(
            app.ui
                .hud
                .chat_lines
                .iter()
                .filter(|l| l.as_str() == "line 139")
                .count(),
            1,
            "each chat line must appear exactly once no matter how many frames render"
        );
        assert!(
            app.ui.hud.chat_lines.iter().all(|l| l.starts_with("line ")),
            "no stale/duplicated lines may ride along: {:?}",
            app.ui.hud.chat_lines
        );
    }

    /// C3: the offline (no live session) path keeps the same bound.
    #[test]
    fn offline_chat_lines_stay_bounded() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.ui.settings.qol.chat_history_lines = 100;
        let settings = app.ui.settings.clone();
        app.apply_settings(&settings);
        for i in 0..140 {
            app.dispatch_input(WindowInputEvent::DebugChord(
                DebugChordAction::ChatFeedback(format!("line {i}")),
            ));
        }
        assert!(app.ui.hud.chat_lines.len() <= 100);
        assert_eq!(app.debug_messages.len(), 100);
        assert_eq!(
            app.ui.hud.chat_lines.first().map(String::as_str),
            Some("line 40")
        );
    }

    #[test]
    fn f3_copy_and_gamemode_actions_reach_clipboard_and_network() {
        use crate::live_session::WorkerCommand;
        let (_tx, incoming) = std::sync::mpsc::sync_channel(1);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(4);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.clipboard = SystemClipboard::in_memory();
        app.attach_live(live);
        app.dispatch_input(WindowInputEvent::DebugChord(DebugChordAction::ClipboardCommand(
            "/tp @s 1 2 3".to_owned(),
        )));
        assert_eq!(app.clipboard.get_text().as_deref(), Some("/tp @s 1 2 3"));
        app.dispatch_input(WindowInputEvent::DebugChord(DebugChordAction::SendGameModeCommand(
            DebugGameMode::Spectator,
        )));
        assert!(matches!(rx.try_recv(), Ok(WorkerCommand::Chat(text)) if text == "/gamemode spectator"));
    }

    #[test]
    fn gui_physical_input_survives_gameplay_control_clearing() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.update_gui_key(KeyCode::ShiftLeft, ElementState::Pressed);
        app.update_gui_key(KeyCode::ShiftRight, ElementState::Pressed);
        app.update_gui_key(KeyCode::ControlLeft, ElementState::Pressed);
        app.gui_left_held = true;
        app.gui_right_held = true;
        app.clear_gameplay_controls();
        assert_ne!(app.gui_modifiers & 3, 0);
        assert_ne!(app.gui_modifiers & 12, 0);
        assert!(app.gui_left_held && app.gui_right_held);
        app.update_gui_key(KeyCode::ShiftLeft, ElementState::Released);
        assert_ne!(app.gui_modifiers & 3, 0);
        app.update_gui_key(KeyCode::ShiftRight, ElementState::Released);
        assert_eq!(app.gui_modifiers & 3, 0);
    }

    #[test]
    fn modal_does_not_feed_camera_height_into_physics() {
        use megu_client::protocol::{ClientPlayEvent, PlayEvent, TeleportSync};
        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
            x: 0.0, y: 64.0, z: 0.0, yaw: 0.0, pitch: 0.0, flags: 0, teleport_id: 1,
        }))).unwrap();
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);
        app.camera.position = [0.0, 65.27, 0.0];
        app.advance_frame_movement(0.01, true);
        let feet = app.live.as_ref().unwrap().pose.unwrap().feet;
        assert!((feet[1] - 64.0).abs() < 0.01, "modal changed simulation feet: {feet:?}");
    }

    #[test]
    fn status_messages_do_not_enter_chat_before_or_after_attach() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.status_line("asset loading");
        app.attach_live(LiveSession::offline_with_null_audio());
        app.status_line("settings saved");
        assert!(app.ui.hud.chat_lines.is_empty());
        assert!(app.live.as_ref().unwrap().hud.chat_lines.is_empty());
        assert!(app.debug_messages.iter().any(|line| line == "asset loading"));
    }

    #[test]
    fn hotbar_metadata_scales_with_slot_geometry() {
        use megu_ui::draw::UiCommand;
        let stack = LiveHotbarOverlayMeta { count: 12, damage: Some(3), max_damage: Some(10) };
        for scale in 1..=4 {
            let mut list = UiDrawList::new();
            let mut hotbar = [None; 9];
            hotbar[0] = Some(stack);
            apply_live_hotbar_metadata(1280, 720, scale, &hotbar, Some(&stack), 0, &mut list);
            for index in [-1, 0] {
                let slot = hotbar_slot_rect(1280, 720, scale, index);
                assert!(list.commands.iter().any(|command| matches!(command,
                    UiCommand::FillRect { rect, .. } if *rect == UiRect::new(slot.x + 4 * scale as i32, slot.y + 18 * scale as i32, 13 * scale, scale)
                )));
                assert!(list.commands.iter().any(|command| matches!(command,
                    UiCommand::Text { x, y, scale: text_scale, text, .. }
                    if *text_scale == scale && text == "12" && *x >= slot.x && *x < slot.right() && *y == slot.y + 12 * scale as i32
                )));
            }
        }
    }

    /// C3: attaching a session replays the pre-connect debug log into
    /// `live.hud` exactly once — the HUD mirror then owns the lines.
    #[test]
    fn attach_live_replays_debug_log_once() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.dispatch_input(WindowInputEvent::DebugChord(
            DebugChordAction::ChatFeedback("pre-connect".to_owned()),
        ));
        app.attach_live(LiveSession::offline_with_null_audio());
        let live = app.live.as_ref().unwrap();
        assert_eq!(
            live.hud
                .chat_lines
                .iter()
                .filter(|l| l.as_str() == "pre-connect")
                .count(),
            1
        );
        assert!(app.live_strategy_pending());
    }

    /// C5: wire-id fallback maps the 1.21.1
    /// `BuiltInRegistries.BLOCK_ENTITY_TYPE` ids for the renderable
    /// families and rejects the rest (spawner=9 has no BER mesh).
    #[test]
    fn block_entity_wire_ids_map_to_ber_families() {
        let be = |kind: i32| TrackedBlockEntity {
            kind,
            nbt_bytes: Vec::new(),
        };
        assert_eq!(
            classify_tracked_block_entity(&be(1)),
            Some(VanillaBerKind::Chest)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(7)),
            Some(VanillaBerKind::Sign)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(14)),
            Some(VanillaBerKind::Beacon)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(32)),
            Some(VanillaBerKind::Campfire)
        );
        assert_eq!(
            classify_tracked_block_entity(&be(40)),
            Some(VanillaBerKind::DecoratedPot)
        );
        assert_eq!(classify_tracked_block_entity(&be(9)), None);
        assert_eq!(classify_tracked_block_entity(&be(0)), None);
    }

    /// C5: the pose fingerprint changes on real pose deltas but ignores
    /// sub-quantization jitter, so rig meshes are not rebuilt per frame.
    #[test]
    fn entity_pose_key_detects_only_real_pose_changes() {
        let spec = megu_client_mesh::entity_rig::classify_entity_rig(114) // zombie
            .expect("zombie rig spec");
        let base = EntityPose::default();
        let key = entity_pose_key(spec, &base, None);
        let jitter = EntityPose {
            head_yaw_deg: 0.005, // < 1/64 quantization step
            ..EntityPose::default()
        };
        assert_eq!(entity_pose_key(spec, &jitter, None), key);
        let turned = EntityPose {
            head_yaw_deg: 45.0,
            ..EntityPose::default()
        };
        assert_ne!(entity_pose_key(spec, &turned, None), key);
        let sneaking = EntityPose {
            sneaking: true,
            ..EntityPose::default()
        };
        assert_ne!(entity_pose_key(spec, &sneaking, None), key);
    }

    /// 6-DOF subworld helm: an SW6D pose flagged player-aboard binds the
    /// session, player input drives change-only SHLM frames, and losing
    /// the binding emits one zeroed release.
    #[test]
    fn subworld_helm_dispatch_binds_drives_and_releases() {
        use crate::live_session::WorkerCommand;
        let (_incoming_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, outgoing_rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);

        let sw6d = |id: u64, flags: u8| {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"SW6D");
            bytes.push(1);
            bytes.extend_from_slice(&1u16.to_be_bytes());
            bytes.extend_from_slice(&id.to_be_bytes());
            for value in [4.0f64, 5.0, 6.0] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            for value in [0.0f32, 0.0, 0.0, 1.0] {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            for _ in 0..6 {
                bytes.extend_from_slice(&0.0f32.to_be_bytes());
            }
            bytes.push(flags);
            bytes
        };
        let sync_event = |payload: Vec<u8>| {
            megu_client::protocol::ClientPlayEvent::CustomChannel(
                megu_client::protocol::ClientCustomChannelEvent::Unknown {
                    channel: crate::subworld_sync::SUBWORLD_SYNC_CHANNEL.to_owned(),
                    payload,
                },
            )
        };
        let recv_helm = |rx: &std::sync::mpsc::Receiver<WorkerCommand>| match rx.try_recv() {
            Ok(WorkerCommand::SendCustomPayload { channel, payload }) => {
                assert_eq!(channel, crate::subworld_sync::SUBWORLD_HELM_CHANNEL);
                crate::subworld_sync::decode_helm_cmd(&payload).expect("SHLM v1 decode")
            }
            _ => panic!("expected a helm SendCustomPayload"),
        };

        // Unbound input dispatches nothing.
        let mut last = None;
        let input = MoveInput {
            forward: 1.0,
            ..MoveInput::default()
        };
        let unbound = live
            .player_subworld()
            .map(|id| subworld_helm_cmd(id, input, false));
        dispatch_subworld_helm(&live, unbound, &mut last);
        assert!(outgoing_rx.try_recv().is_err());

        // A flagged SW6D pose binds the player and lands in the render bank.
        live.apply_event(sync_event(sw6d(77, 0x01))).unwrap();
        assert_eq!(live.player_subworld(), Some(77));
        assert_eq!(
            live.subworlds.get(77).unwrap().transform.position,
            [4.0, 5.0, 6.0]
        );

        // First bound frame sends SHLM; identical repeats are suppressed.
        let cmd = subworld_helm_cmd(77, input, false);
        assert_eq!(cmd.throttle, 1.0);
        dispatch_subworld_helm(&live, Some(cmd), &mut last);
        assert_eq!(recv_helm(&outgoing_rx), cmd);
        dispatch_subworld_helm(&live, Some(cmd), &mut last);
        assert!(
            outgoing_rx.try_recv().is_err(),
            "unchanged helm frame must not resend"
        );

        // A released stick sends one zeroed frame; staying released sends none.
        let released = subworld_helm_cmd(77, MoveInput::default(), false);
        dispatch_subworld_helm(&live, Some(released), &mut last);
        assert_eq!(recv_helm(&outgoing_rx), released);
        dispatch_subworld_helm(&live, Some(released), &mut last);
        assert!(outgoing_rx.try_recv().is_err());

        // Dropping the binding after an active command releases controls once.
        dispatch_subworld_helm(&live, Some(cmd), &mut last);
        assert_eq!(recv_helm(&outgoing_rx), cmd);
        live.apply_event(sync_event(sw6d(77, 0))).unwrap();
        assert_eq!(live.player_subworld(), None);
        dispatch_subworld_helm(&live, None, &mut last);
        let release = recv_helm(&outgoing_rx);
        assert_eq!(release.subworld_id, 77);
        assert!(!subworld_helm_cmd_active(&release));
        dispatch_subworld_helm(&live, None, &mut last);
        assert!(outgoing_rx.try_recv().is_err(), "release is sent once");
    }

    /// Fox mouth slot: the held main-hand item id joins the mesh cache
    /// key and paints the mouth prism, so pick-up/drop rebuilds the mesh
    /// immediately instead of waiting on a pose delta.
    #[test]
    fn fox_mouth_item_cache_key_and_mesh_follow_mainhand_item() {
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.apply_event(megu_client::protocol::ClientPlayEvent::Play(
            megu_client::protocol::PlayEvent::AddEntity {
                eid: 9,
                uuid: [1; 16],
                entity_type: i32::from(FOX_ENTITY_TYPE_ID),
                x: 1.0,
                y: 64.0,
                z: 2.0,
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                data: 0,
                vx: 0.0,
                vy: 0.0,
                vz: 0.0,
            },
        ))
        .unwrap();
        let spec = *live.entity_rig(9).expect("fox rig");
        let pose = EntityPose::default();

        // Bare-handed fox: no mouth item, base fingerprint and mesh.
        assert_eq!(live.entity_mainhand_item_id(9), None);
        let bare_key = entity_pose_key(spec, &pose, None);
        let bare = build_entity_rig_mesh_with_mouth_item(spec, pose, None);

        live.apply_event(megu_client::protocol::ClientPlayEvent::Play(
            megu_client::protocol::PlayEvent::SetEquipment {
                eid: 9,
                entries: vec![megu_client::protocol::EquipmentEntry {
                    slot: 0,
                    item_wire: vec![1, 5, 0, 0],
                }],
            },
        ))
        .unwrap();
        assert_eq!(live.entity_mainhand_item_id(9), Some(5));
        let held_key = entity_pose_key(spec, &pose, Some(5));
        assert_ne!(
            held_key, bare_key,
            "held item id must invalidate the mesh cache"
        );
        let held = build_entity_rig_mesh_with_mouth_item(spec, pose, Some(mouth_item_rgb(5)));
        assert_eq!(
            held.quads.len(),
            bare.quads.len() + 6,
            "mouth prism adds six quads"
        );

        // Distinct items re-key and re-tint; drops return to the bare key.
        assert_ne!(entity_pose_key(spec, &pose, Some(7)), held_key);
        assert_eq!(entity_pose_key(spec, &pose, None), bare_key);
        assert_eq!(mouth_item_rgb(5), mouth_item_rgb(5));
        assert_ne!(mouth_item_rgb(5), mouth_item_rgb(7));
    }

    /// C5: quads pushed for live particles are axis-consistent
    /// parallelograms — the shape `pack_particle_billboard` requires for
    /// the instanced path — and degenerate input is skipped.
    #[test]
    fn particle_quad_push_builds_valid_parallelograms() {
        let mut quads = Vec::new();
        let right = [1.0, 0.0, 0.0];
        let up = [0.0, 1.0, 0.0];
        push_billboard(&mut quads, [1.0, 2.0, 3.0], right, up, 0.5, [1.0; 4]);
        push_flat_ring(&mut quads, [0.0, 0.0, 0.0], 0.25, [0.5, 0.5, 0.5, 1.0]);
        assert_eq!(quads.len(), 2);
        let quad = &quads[0];
        let u = [
            quad.positions[1][0] - quad.positions[0][0],
            quad.positions[1][1] - quad.positions[0][1],
            quad.positions[1][2] - quad.positions[0][2],
        ];
        let v = [
            quad.positions[3][0] - quad.positions[0][0],
            quad.positions[3][1] - quad.positions[0][1],
            quad.positions[3][2] - quad.positions[0][2],
        ];
        for axis in 0..3 {
            let expected = quad.positions[0][axis] + u[axis] + v[axis];
            assert!(
                (quad.positions[2][axis] - expected).abs() < 1e-5,
                "corner 2 must close the parallelogram on axis {axis}"
            );
        }
        // Degenerate/NaN input is dropped, not staged.
        push_billboard(&mut quads, [f32::NAN; 3], right, up, 0.5, [1.0; 4]);
        push_billboard(&mut quads, [0.0; 3], right, up, 0.0, [1.0; 4]);
        assert_eq!(quads.len(), 2);
    }

    /// Multiplayer screen `<->` `ServerListStore` roundtrip through the
    /// same `dispatch_ui_action` path the click handlers use: opening
    /// the screen loads `<gameDir>/megu_servers.toml`, `SaveServer`
    /// persists through the store, `DeleteServer` removes it.
    #[test]
    fn multiplayer_screen_loads_and_persists_server_list() {
        let dir =
            std::env::temp_dir().join(format!("megu-gpu-servers-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });

        // Opening Multiplayer loads the (empty) store.
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Multiplayer));
        assert_eq!(app.ui.screen, Screen::Multiplayer);
        assert!(app.ui.multiplayer.servers.is_empty());

        // Save persists through the store and refreshes the visible rows.
        app.dispatch_ui_action(&UiAction::SaveServer {
            name: "Hub".to_string(),
            address: "hub.example.org".to_string(),
            port: 25566,
        });
        assert_eq!(app.ui.multiplayer.servers.len(), 1);
        assert_eq!(app.ui.multiplayer.servers[0].name, "Hub");
        assert_eq!(app.ui.multiplayer.servers[0].port, 25566);
        assert!(dir.join("megu_servers.toml").exists());

        // A fresh load sees the same entry (on-disk roundtrip).
        let store = ServerListStore::load(&dir).unwrap();
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].address, "hub.example.org");

        // Delete removes the row and the persisted entry.
        app.ui.multiplayer.selected = Some(0);
        app.dispatch_ui_action(&UiAction::DeleteServer { index: 0 });
        assert!(app.ui.multiplayer.servers.is_empty());
        let store = ServerListStore::load(&dir).unwrap();
        assert!(store.entries().is_empty());

        // Store-level rejection (port 0 can't arrive from the UI, but a
        // bad index surfaces on the status banner, never silently).
        app.dispatch_ui_action(&UiAction::DeleteServer { index: 9 });
        assert!(app.ui.multiplayer.status.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_processing_failure_opens_crash_recovery_and_returns_to_title() {
        use crate::live_session::WorkerMessage;
        use megu_client::protocol::{ClientPlayEvent, PlayEvent};
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        app.attach_live(LiveSession::with_channels(incoming, outgoing));
        app.ui.screen = Screen::InGameHud;
        app.paused = true;
        tx.send(WorkerMessage::Play(ClientPlayEvent::Play(
            PlayEvent::MapItemData {
                map_id: 1,
                scale: 0,
                locked: false,
                icons: None,
                columns: 2,
                rows: 1,
                x: 127,
                y: 0,
                colors: vec![0; 2],
            },
        )))
        .unwrap();
        app.poll_live_in_background();
        assert_eq!(app.ui.screen, Screen::CrashRecovery);
        assert!(app.ui.crash_preview().contains("map patch outside"));
        assert!(app.live.is_none());
        assert!(!app.paused);
        app.clipboard = SystemClipboard::in_memory();
        let mut click = |id| {
            app.ui.render(850, 480);
            let rect = app.ui.hit_regions.iter().find(|r| r.id == id).unwrap().rect;
            for action in app.ui.handle_input(UiInputEvent::Click {
                x: rect.x + 2,
                y: rect.y + 2,
            }) {
                app.dispatch_ui_action(&action);
            }
        };
        click(megu_ui::state::WidgetId::CrashCopyReport);
        click(megu_ui::state::WidgetId::CrashReturnTitle);
        assert!(
            app.clipboard
            .get_text()
            .unwrap()
                .contains("map patch outside")
        );
        assert_eq!(app.ui.screen, Screen::Title);
    }

    #[test]
    fn spectator_camera_follows_tracked_eye_without_changing_local_camera() {
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 1,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        window_test_add_entity(&mut live, 2, 23, [1.0, 64.0, 3.0]); // creeper
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetCamera { eid: 2 }))
            .unwrap();
        app.attach_live(live);
        let local = app.camera.position;
        app.update_spectator_view(0.016);
        let first = app.session_view_camera();
        assert_eq!(first.position, [1.0, 64.0 + 1.445, 3.0]);
        assert_eq!(app.camera.position, local);
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::MoveEntityPosRot {
                eid: 2,
                dx: 1.0,
                dy: 0.0,
                dz: 0.0,
                yaw: 90.0,
                pitch: 30.0,
                on_ground: true,
            }))
            .unwrap();
        app.update_spectator_view(0.035);
        let second = app.session_view_camera();
        assert!((1.4..1.6).contains(&second.position[0]));
        assert!((224.0..226.0).contains(&second.yaw_deg));
        assert!((-16.0..-14.0).contains(&second.pitch_deg));
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::SetCamera { eid: 1 }))
            .unwrap();
        app.update_spectator_view(0.016);
        assert_eq!(app.session_view_camera().position, local);
        assert!(app.live.as_ref().unwrap().world.camera_entity_id.is_none());
    }

    /// `SessionStatus::Disconnected` → `Screen::Disconnected` with the
    /// verbatim reason; the live session and every overlay are dropped
    /// and the pointer is released (the same cleanup
    /// `leave_world_for_title` performs).
    #[test]
    fn disconnected_transition_drops_session_and_shows_reason() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Disconnected("Kicked by an operator.".to_string());
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;
        app.paused = true;
        app.chat_bar = Some(ZshCommandBar::new());

        app.sync_disconnected_screen();
        assert!(app.live.is_none());
        assert_eq!(app.ui.screen, Screen::Disconnected);
        assert_eq!(app.ui.disconnected.reason, "Kicked by an operator.");
        assert!(!app.paused);
        assert!(app.chat_bar.is_none());
        assert!(app.container_screen.is_none());

        // Playing sessions are untouched.
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;
        app.sync_disconnected_screen();
        assert!(app.live.is_some());
        assert_eq!(app.ui.screen, Screen::InGameHud);
    }

    /// `container_set_data` properties recorded on `live.open_screen`
    /// reach the rendered `ContainerScreenState.properties` every frame —
    /// they carry no `state_id`, so the mirror must not wait for a bump.
    #[test]
    fn sync_container_screen_mirrors_live_properties() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;

        // Furnace (menu 14) opens, then container_set_data lands without
        // a state_id bump.
        use megu_client::protocol::{ClientPlayEvent, PlayEvent};
        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::OpenScreen {
                window_id: 1,
                menu_type: 14,
                title_nbt: megu_proto::item_components::plain_text_component("Server Furnace")
                    .expect("plain text component"),
            }))
            .unwrap();
        app.sync_container_screen();
        assert!(app.container_screen.is_some());
        assert_eq!(
            app.container_screen.as_ref().unwrap().title,
            "Server Furnace"
        );
        assert!(app.container_screen.as_ref().unwrap().properties.is_empty());

        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::ContainerSetData {
                window_id: 1,
                property: 2, // furnace cook progress
                value: 42,
            }))
            .unwrap();
        app.sync_container_screen();
        assert_eq!(app.container_screen.as_ref().unwrap().property(2), 42);
    }

    /// Opening `Screen::Friends` mints `megu-identity.key` +
    /// `megu-friends.toml` under `<gameDir>` and mirrors them into
    /// `ui.friends` — the row set is exactly what the store holds.
    #[test]
    fn friends_screen_loads_identity_and_store_into_ui() {
        let dir =
            std::env::temp_dir().join(format!("megu-gpu-friends-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });
        // A plain boot does not mint identity files (screen entry owns that).
        assert!(!dir.join("megu-identity.key").exists());

        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Friends));
        assert_eq!(app.ui.screen, Screen::Friends);
        assert!(dir.join("megu-identity.key").exists());
        assert!(app.ui.friends.my_friend_id.starts_with("megu-id:"));
        // The persisted identity and the displayed id agree.
        let store = FriendsStore::load(&dir).unwrap();
        let identity = load_or_create_identity(&dir).unwrap();
        assert_eq!(
            app.ui.friends.my_friend_id,
            FriendsStore::my_friend_id(&identity)
        );
        assert!(store.friends.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `AddFriend`/`RemoveFriend` through `dispatch_ui_action` persist to
    /// `megu-friends.toml`, mirror the rows, and surface store rejections
    /// (bad checksum, self-key, duplicate) on `add_error` — never silent.
    #[test]
    fn friends_add_remove_persist_through_dispatch() {
        let dir =
            std::env::temp_dir().join(format!("megu-gpu-friends-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Friends));

        // Rejections land inline: garbage id, then the player's own key.
        app.dispatch_ui_action(&UiAction::AddFriend {
            name: "Bad".into(),
            friend_id: "megu-id:NOTAREALID".into(),
        });
        assert!(app.ui.friends.add_error.is_some());
        let own = app.ui.friends.my_friend_id.clone();
        app.dispatch_ui_action(&UiAction::AddFriend {
            name: "Self".into(),
            friend_id: own,
        });
        assert!(app.ui.friends.add_error.is_some());
        assert!(app.ui.friends.friends.is_empty());

        // A valid `megu-id:` adds + persists + clears the input fields.
        let friend_id = Frl3Identity::from_secret([9u8; 32]).friend_id();
        app.ui.friends.add_name_text = "Pal".into();
        app.ui.friends.add_id_text = friend_id.clone();
        app.dispatch_ui_action(&UiAction::AddFriend {
            name: "Pal".into(),
            friend_id: friend_id.clone(),
        });
        assert!(app.ui.friends.add_error.is_none());
        assert_eq!(app.ui.friends.friends.len(), 1);
        assert_eq!(app.ui.friends.friends[0].friend_id, friend_id);
        assert!(app.ui.friends.add_name_text.is_empty());
        assert!(app.ui.friends.add_id_text.is_empty());
        let store = FriendsStore::load(&dir).unwrap();
        assert_eq!(store.friends.len(), 1);
        assert_eq!(store.friends[0].name, "Pal");

        // Duplicates are rejected too.
        app.dispatch_ui_action(&UiAction::AddFriend {
            name: "Pal2".into(),
            friend_id: friend_id.clone(),
        });
        assert!(app.ui.friends.add_error.is_some());

        // Remove persists and drops the row.
        app.dispatch_ui_action(&UiAction::RemoveFriend {
            friend_id: friend_id.clone(),
        });
        assert!(app.ui.friends.friends.is_empty());
        let store = FriendsStore::load(&dir).unwrap();
        assert!(store.friends.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Share-mode cycling + the relay field persist to
    /// `megu-friends.toml`; a non-`Off` mode without a hosted world is
    /// armed (not silently dropped) and `Off` stops a live share.
    #[test]
    fn friends_share_mode_and_relay_persist_to_toml() {
        let dir =
            std::env::temp_dir().join(format!("megu-gpu-friends-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Friends));

        app.dispatch_ui_action(&UiAction::SetFriendShareMode(UiShareMode::SelectedFriends));
        let store = FriendsStore::load(&dir).unwrap();
        assert_eq!(store.share_mode, StoreShareMode::SelectedFriends);
        // No hosted world → armed, not a bogus "live" claim.
        assert!(!app.ui.friends.sharing_active);
        assert!(app.friend_share.is_none());
        assert!(
            app.ui
            .friends
            .status
            .as_deref()
                .is_some_and(|s| s.contains("armed"))
        );

        app.dispatch_ui_action(&UiAction::SaveRelayAddr {
            addr: "relay.example.org:7777".into(),
        });
        let store = FriendsStore::load(&dir).unwrap();
        assert_eq!(store.relay_addr.as_deref(), Some("relay.example.org:7777"));
        // The saved relay is the join/share fallback once persisted.
        assert_eq!(app.friend_relay_addr().unwrap(), "relay.example.org:7777");
        // An explicit `--relay` still wins over the saved one.
        let mut app2 = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            relay: Some("cli-relay:1".into()),
            ..GpuCliConfig::default()
        });
        app2.dispatch_ui_action(&UiAction::OpenScreen(Screen::Friends));
        assert_eq!(app2.friend_relay_addr().unwrap(), "cli-relay:1");

        // Back to Off — persisted.
        app.dispatch_ui_action(&UiAction::SetFriendShareMode(UiShareMode::Off));
        let store = FriendsStore::load(&dir).unwrap();
        assert_eq!(store.share_mode, StoreShareMode::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `megu-friend:` guest-link codec round-trips host id + invite
    /// code + 128-bit token and rejects every malformed surface form —
    /// a bad id checksum, a truncated token, garbage — fail-closed.
    #[test]
    fn guest_link_codec_roundtrip_and_rejects_garbage() {
        let host_id = Frl3Identity::from_secret([7u8; 32]).friend_id();
        let code = megu_proto::InviteCode::from_bytes([0x12, 0x34, 0x56, 0x78, 0x9A]);
        let token = Frl3GuestToken::from_bytes([0xAB; 16]);
        let link = format!(
            "megu-friend:{host_id}:{code}:{}",
            guest_link_b32_encode(&token.as_bytes())
        );
        let (pk, parsed_id, parsed_code, parsed_token) = parse_guest_link(&link).unwrap();
        assert_eq!(parsed_id, host_id);
        assert_eq!(pk, parse_friend_id(&host_id).unwrap());
        assert_eq!(parsed_code.as_bytes(), code.as_bytes());
        assert_eq!(parsed_token.as_bytes(), token.as_bytes());

        assert!(parse_guest_link("nonsense").is_err());
        assert!(parse_guest_link("megu-friend:megu-id:BADID:MEGU-0000-0000:AAAA").is_err());
        // Token must decode to exactly 16 bytes.
        assert!(parse_guest_link(&format!("megu-friend:{host_id}:{code}:AAAA")).is_err());
        // A missing segment can't be salvaged.
        assert!(parse_guest_link(&format!("megu-friend:{host_id}")).is_err());
    }

    /// `JoinFriendWorld` resolves the row and surfaces every failure on
    /// the status line — an empty list, a bad id, an unreachable relay —
    /// without panicking or leaving a bridge behind.
    #[test]
    fn join_friend_world_surfaces_errors_not_panics() {
        let dir = std::env::temp_dir().join(format!("megu-gpu-friend-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut app = GpuApp::new(GpuCliConfig {
            game_dir: Some(dir.clone()),
            ..GpuCliConfig::default()
        });
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Multiplayer));

        // Empty row list → out-of-range is an error, not a panic.
        app.dispatch_ui_action(&UiAction::JoinFriendWorld { index: 0 });
        assert!(app.ui.title_status.is_some());
        assert!(app.friend_bridge.is_none());

        // A well-formed row pointing at a closed relay port fails the
        // handshake fast and leaves no half-attached bridge.
        app.ui.multiplayer.friend_worlds = vec![megu_ui::state::FriendWorldEntry {
            friend_name: "Pal".into(),
            friend_id: Frl3Identity::from_secret([3u8; 32]).friend_id(),
            world_name: "Sky".into(),
            online_players: 1,
            max_players: 8,
            relay_addr: "127.0.0.1:1".into(),
            invite_code: "MEGU-0000-0000".into(),
        }];
        app.dispatch_ui_action(&UiAction::JoinFriendWorld { index: 0 });
        assert!(app.ui.title_status.is_some());
        assert!(app.friend_bridge.is_none());

        // Same for a pasted guest link with a dead relay.
        app.dispatch_ui_action(&UiAction::JoinGuestLink {
            link: "megu-friend:megu-id:NOTREAL:MEGU-0000-0000:AAAA".into(),
        });
        assert!(app.friend_bridge.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `CopyText` writes through the clipboard backend — the dispatch
    /// lands the text in the store and reports which one (the in-app
    /// buffer here, pinned so the test is deterministic everywhere).
    #[test]
    fn copy_text_reports_on_status_line() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.clipboard = SystemClipboard::in_memory();
        app.dispatch_ui_action(&UiAction::CopyText("megu-id:ABCDEF".into()));
        assert_eq!(app.clipboard.get_text().as_deref(), Some("megu-id:ABCDEF"));
        assert!(
            app.ui
            .title_status
            .as_deref()
                .is_some_and(|s| s.starts_with("Copied"))
        );
    }
    /// `UiAction::CopyInvite` writes through the clipboard backend and
    /// the status line honestly reports which store took the text —
    /// `SystemClipboard::in_memory` pins the fallback path so the test
    /// is deterministic on both desktops and headless CI.
    #[test]
    fn copy_invite_writes_clipboard_and_reports_store() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.clipboard = SystemClipboard::in_memory();

        app.dispatch_ui_action(&UiAction::CopyInvite("MEGU-ABCD-1234".to_string()));
        assert_eq!(app.clipboard.get_text().as_deref(), Some("MEGU-ABCD-1234"));
        assert!(
            app.status_messages()
                .iter()
                .any(|m| m.contains("MEGU-ABCD-1234") && m.contains("in-app buffer")),
            "fallback store must be reported honestly: {:?}",
            app.status_messages()
        );
    }

    /// `Ctrl+C` on the chat bar copies the draft and reports which
    /// store actually holds the text.
    #[test]
    fn copy_reports_clipboard_outcome() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.clipboard = SystemClipboard::in_memory();
        let mut bar = ZshCommandBar::new();
        bar.set_text("/say hi");
        app.chat_bar = Some(bar);

        app.clipboard_copy();
        assert_eq!(app.clipboard.get_text().as_deref(), Some("/say hi"));
        assert!(
            app.status_messages()
            .iter()
                .any(|m| m.contains("in-app buffer"))
        );
    }

    /// `Ime::Commit` text routes into the active text surface exactly
    /// like typed characters: a focused UI field takes the full UTF-8
    /// string (the world-name field accepts CJK), the chat bar takes it
    /// byte-for-byte, and no character is dropped.
    #[test]
    fn ime_commit_feeds_japanese_text_into_focused_inputs() {
        let mut app = GpuApp::new(GpuCliConfig::default());

        // World-name field on the Singleplayer screen.
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Singleplayer));
        let _ = app.ui.render(1280, 720);
        let hit = app
            .ui
            .hit_regions
            .iter()
            .find(|h| h.id == megu_ui::state::WidgetId::SingleplayerNameField)
            .cloned()
            .expect("name field hit region");
        for action in app.ui.handle_input(UiInputEvent::Click {
            x: hit.rect.x + 2,
            y: hit.rect.y + 2,
        }) {
            app.dispatch_ui_action(&action);
        }
        assert!(app.ui.world_name_field_active());
        app.ui.singleplayer.new_name.clear();
        app.dispatch_committed_text("こんにちは");
        assert_eq!(app.ui.singleplayer.new_name, "こんにちは");

        // The in-game chat bar takes the same commit verbatim.
        app.chat_bar = Some(ZshCommandBar::new());
        app.dispatch_committed_text("こんにちは世界");
        assert_eq!(
            app.chat_bar.as_ref().map(|b| b.text()),
            Some("こんにちは世界")
        );

        // Control characters inside a commit never reach the buffer.
        app.chat_bar.as_mut().unwrap().clear();
        app.dispatch_committed_text("ab\ncd\x07ef");
        assert_eq!(app.chat_bar.as_ref().map(|b| b.text()), Some("abcdef"));
    }

    /// `set_ime_allowed` tracks text-entry state: off during raw WASD
    /// gameplay (the IME can never hijack movement keys), on for the
    /// chat bar and focused UI fields, off again once they close.
    #[test]
    fn ime_allowed_tracks_text_entry_not_gameplay() {
        let mut app = GpuApp::new(GpuCliConfig::default());

        // Live 3D gameplay: IME stays off.
        app.ui.screen = Screen::InGameHud;
        app.update_ime();
        assert!(!app.ime_allowed);

        // Opening the chat bar enables it; closing disables it again.
        app.chat_bar = Some(ZshCommandBar::new());
        app.update_ime();
        assert!(app.ime_allowed);
        app.chat_bar = None;
        app.update_ime();
        assert!(!app.ime_allowed);

        // A focused UI text field also enables it.
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Multiplayer));
        let _ = app.ui.render(1280, 720);
        let hit = app
            .ui
            .hit_regions
            .iter()
            .find(|h| h.id == megu_ui::state::WidgetId::MultiplayerAddressField)
            .cloned()
            .expect("address field hit region");
        for action in app.ui.handle_input(UiInputEvent::Click {
            x: hit.rect.x + 2,
            y: hit.rect.y + 2,
        }) {
            app.dispatch_ui_action(&action);
        }
        assert!(app.ui.server_address_field_active());
        app.update_ime();
        assert!(app.ime_allowed);

        // Escape out of the field -> back to no-IME.
        for action in app.ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)) {
            app.dispatch_ui_action(&action);
        }
        app.update_ime();
        assert!(!app.ime_allowed);
    }

    /// `Ctrl+V` path: clipboard text is sanitized for the single-line
    /// chat bar (`\r\n` stripped) and capped at the vanilla chat
    /// length — never a mid-commit panic or a doubled paste.
    #[test]
    fn paste_sanitizes_newlines_and_caps_chat_length() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.clipboard = SystemClipboard::in_memory();
        app.chat_bar = Some(ZshCommandBar::new());

        app.clipboard.set_text("megu.example.com\r\n:25565");
        let actions = app.clipboard_paste_actions();
        assert!(actions.is_empty());
        assert_eq!(
            app.chat_bar.as_ref().map(|b| b.text()),
            Some("megu.example.com:25565")
        );

        // The vanilla 256-char cap truncates, never overflows.
        let long = "x".repeat(CHAT_FIELD_MAX_CHARS + 40);
        app.clipboard.set_text(&long);
        let actions = app.clipboard_paste_actions();
        assert!(actions.is_empty());
        assert_eq!(
            app.chat_bar.as_ref().unwrap().text().chars().count(),
            CHAT_FIELD_MAX_CHARS
        );
    }

    /// The `Ctrl`/`Cmd` chord only fires while a text surface owns
    /// typing — raw gameplay keeps `V`/`C` free for their real
    /// bindings.
    #[test]
    fn clipboard_chord_requires_text_surface_and_modifier() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.modifiers = ModifiersState::CONTROL;
        app.ui.screen = Screen::InGameHud;
        // No text surface in raw gameplay.
        assert!(!app.clipboard_chord(KeyCode::KeyV, ElementState::Pressed));
        app.chat_bar = Some(ZshCommandBar::new());
        assert!(app.clipboard_chord(KeyCode::KeyV, ElementState::Pressed));
        assert!(app.clipboard_chord(KeyCode::KeyC, ElementState::Pressed));
        // A released modifier drops the chord; a release event never
        // counts as a press.
        app.modifiers = ModifiersState::empty();
        assert!(!app.clipboard_chord(KeyCode::KeyV, ElementState::Pressed));
        app.modifiers = ModifiersState::CONTROL;
        assert!(!app.clipboard_chord(KeyCode::KeyV, ElementState::Released));
    }

    /// A platform that re-reports the committed string as
    /// `KeyEvent.text` triggers the swallow exactly once — a second
    /// identical echo (or a genuinely typed repeat) is never
    /// suppressed.
    #[test]
    fn ime_echo_swallow_is_single_shot() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.last_ime_commit = Some(("あ".to_string(), Instant::now()));
        assert!(app.swallow_ime_echo("あ"));
        assert!(!app.swallow_ime_echo("あ"));
        assert!(!app.swallow_ime_echo("い"));
    }

    /// The `WindowEvent::Ime` state update tracks the full composition
    /// lifecycle in `ime_state`: `Enabled`/`Preedit`/`Commit`/`Disabled`
    /// transition deterministically, committed text is returned exactly
    /// once for the shared text-insertion route (multi-byte Japanese
    /// survives byte-for-byte), and an empty commit inserts nothing and
    /// arms no echo swallow.
    #[test]
    fn ime_event_state_tracks_composition_and_commit() {
        let mut app = GpuApp::new(GpuCliConfig::default());

        assert_eq!(app.on_ime_event(Ime::Enabled), None);
        assert!(app.ime_state.active);

        assert_eq!(
            app.on_ime_event(Ime::Preedit("にほん".to_string(), Some((1, 4)))),
            None
        );
        assert_eq!(app.ime_state.preedit, "にほん");
        assert_eq!(app.ime_state.cursor_byte_range, Some((0, 3)));

        // A cleared preedit arrives as an empty string.
        assert_eq!(app.on_ime_event(Ime::Preedit(String::new(), None)), None);
        assert_eq!(app.ime_state.preedit, "");

        // Commit returns the composed string for the text surface and
        // latches it for the `KeyEvent.text` echo swallow.
        assert_eq!(
            app.on_ime_event(Ime::Commit("日本".to_string())),
            Some("日本".to_string())
        );
        assert_eq!(app.ime_state.preedit, "");
        assert!(app.swallow_ime_echo("日本"));
        assert!(!app.swallow_ime_echo("日本"));

        // An empty commit inserts nothing and arms no swallow.
        app.last_ime_commit = None;
        assert_eq!(app.on_ime_event(Ime::Commit(String::new())), None);
        assert!(app.last_ime_commit.is_none());

        app.on_ime_event(Ime::Preedit("かな".to_string(), Some((0, 3))));
        assert_eq!(app.on_ime_event(Ime::Disabled), None);
        assert!(!app.ime_state.active);
        assert_eq!(app.ime_state.preedit, "");
    }

    /// `update_ime` pushes a candidate-window anchor that tracks the
    /// text surface: the chat strip when the chat bar is open, the
    /// focused widget's hit region for UI fields — and dropping the
    /// text surface clears composition state.
    #[test]
    fn ime_cursor_area_tracks_active_text_surface() {
        let mut app = GpuApp::new(GpuCliConfig::default());

        // No window: anchors compute from a 0-size surface but never panic.
        app.chat_bar = Some(ZshCommandBar::new());
        let (x, _y, w, _h) = app.ime_cursor_area();
        assert_eq!(x, 4);
        assert_eq!(w, 40); // minimum width clamp
        app.chat_bar = None;

        // A focused UI text field anchors to its own hit region.
        app.dispatch_ui_action(&UiAction::OpenScreen(Screen::Multiplayer));
        let _ = app.ui.render(1280, 720);
        let hit = app
            .ui
            .hit_regions
            .iter()
            .find(|h| h.id == megu_ui::state::WidgetId::MultiplayerAddressField)
            .cloned()
            .expect("address field hit region");
        for action in app.ui.handle_input(UiInputEvent::Click {
            x: hit.rect.x + 2,
            y: hit.rect.y + 2,
        }) {
            app.dispatch_ui_action(&action);
        }
        assert!(app.ui.server_address_field_active());
        let area = app.ime_cursor_area();
        assert_eq!(area.0, hit.rect.x);
        assert_eq!(area.1, hit.rect.y);
        assert_eq!(area.2, hit.rect.w);

        // `update_ime` records the same anchor into `ime_state`.
        app.update_ime();
        assert!(app.ime_allowed);
        assert_eq!(app.ime_state.ime_cursor_area, area);

        // Leaving the text surface clears the tracked composition.
        app.ime_state.on_enabled();
        app.ime_state.on_preedit("かな", Some((0, 3)));
        for action in app.ui.handle_input(UiInputEvent::KeyPressed(UiKey::Escape)) {
            app.dispatch_ui_action(&action);
        }
        app.update_ime();
        assert!(!app.ime_allowed);
        assert!(!app.ime_state.active);
        assert_eq!(app.ime_state.preedit, "");
    }

    /// `DroppedFile` records the payload and surfaces a non-fatal
    /// localized status notice carrying the universal classifier's
    /// verdict — a nonexistent path is rejected, nothing executes.
    #[test]
    fn dropped_file_is_recorded_and_announced() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        assert!(app.last_dropped_file.is_none());

        let path = std::path::Path::new("/tmp/worlds/cool-pack.zip");
        app.record_dropped_file(path);
        assert_eq!(app.last_dropped_file.as_deref(), Some(path));
        assert!(
            app.status_messages()
            .iter()
                .any(|m| m.contains("cool-pack.zip") && m.contains("rejected"))
        );

        // A second drop replaces the staged payload.
        let other = std::path::Path::new("/tmp/shader");
        app.record_dropped_file(other);
        assert_eq!(app.last_dropped_file.as_deref(), Some(other));
    }

    /// The grab policy is seeded from live compositor detection and the
    /// focus/pointer transitions gate re-arming exactly like the
    /// `Focused`/`CursorEntered` event arms do.
    #[test]
    fn tiling_grab_policy_tracks_focus_and_pointer_for_rearm() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        assert_eq!(
            app.tiling_grab_policy.compositor(),
            crate::linux_desktop::detect_compositor_from_env()
        );
        assert!(!app.tiling_grab_policy.should_grab());

        // Pointer entry while focused arms the request; focus loss
        // releases it; focus gain re-arms only while the pointer is
        // still inside, and pointer exit releases again.
        app.tiling_grab_policy.pointer_entered();
        assert!(app.tiling_grab_policy.request_grab());
        app.tiling_grab_policy.focus_lost();
        assert!(!app.tiling_grab_policy.request_grab());
        app.tiling_grab_policy.focus_gained();
        assert!(app.tiling_grab_policy.request_grab());
        app.tiling_grab_policy.pointer_left();
        assert!(!app.tiling_grab_policy.request_grab());
    }

    /// `open_sign_editor` lifts into `ui.sign_editor` via
    /// `sync_sign_editor`, which also turns `text_input_active` on so
    /// IME stays enabled and WASD is not routed to `self.keys`.
    #[test]
    fn sign_editor_opens_on_server_packet_and_claims_text_input() {
        use megu_client::protocol::{ClientPlayEvent, PlayEvent};
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;
        assert!(!app.text_input_active());

        app.live
            .as_mut()
            .unwrap()
            .apply_event(ClientPlayEvent::Play(PlayEvent::OpenSignEditor {
                pos: [10, 64, -20],
                front_side: true,
            }))
            .unwrap();
        app.sync_sign_editor();

        let editor = app.ui.sign_editor.as_ref().expect("sign editor open");
        assert_eq!(editor.pos, (10, 64, -20));
        assert!(editor.is_front);
        assert!(app.text_input_active());
        assert!(
            !app.keys.forward && !app.keys.sprint,
            "movement keys must be released while the editor owns input"
        );
    }

    /// A second `open_sign_editor` re-arms the pending slot even while a
    /// first editor is still open (server-side re-open).
    #[test]
    fn sign_editor_reopen_replaces_pending() {
        use megu_client::protocol::{ClientPlayEvent, PlayEvent};
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        for pos in [[1, 2, 3], [4, 5, 6]] {
            app.live
                .as_mut()
                .unwrap()
                .apply_event(ClientPlayEvent::Play(PlayEvent::OpenSignEditor {
                    pos,
                    front_side: false,
                }))
                .unwrap();
        }
        app.sync_sign_editor();
        assert_eq!(app.ui.sign_editor.as_ref().unwrap().pos, (4, 5, 6));
    }

    /// IME-committed Japanese text lands in the active sign line, then
    /// close ships `sign_update` with the full 4-line payload and drops
    /// `text_input_active` again.
    #[test]
    fn sign_editor_ime_commit_and_finish_sends_update() {
        use crate::live_session::WorkerCommand;
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.screen = Screen::InGameHud;
        app.ui.sign_editor = Some(SignEditorState::new((1, 64, 1), true, Default::default()));

        // Ime::Commit path: multi-byte UTF-8 inserts intact.
        app.dispatch_committed_text("看板");
        app.dispatch_committed_text("テキスト");
        {
            let editor = app.ui.sign_editor.as_ref().unwrap();
            assert_eq!(editor.lines()[0], "看板テキスト");
            assert_eq!(editor.cursor_col(), 6);
        }
        // Paste path: `insert_str` feeds the active line (control chars
        // like `\n` are stripped by the state model).
        {
            let editor = app.ui.sign_editor.as_mut().unwrap();
            editor.move_down();
            editor.insert_str("売店");
            editor.move_down();
            editor.insert_str("open");
            editor.move_down();
            editor.insert_str("24h");
            assert_eq!(editor.lines()[1], "売店");
            assert_eq!(editor.lines()[2], "open");
            assert_eq!(editor.lines()[3], "24h");
        }

        app.finish_sign_editor(true);
        assert!(app.ui.sign_editor.is_none());
        assert!(!app.text_input_active());
        match rx.try_recv() {
            Ok(WorkerCommand::UpdateSign {
                pos,
                is_front,
                lines,
            }) => {
                assert_eq!(pos, (1, 64, 1));
                assert!(is_front);
                assert_eq!(
                    lines,
                    [
                        "看板テキスト".to_owned(),
                        "売店".to_owned(),
                        "open".to_owned(),
                        "24h".to_owned(),
                    ]
                );
            }
            _ => panic!("expected UpdateSign command"),
        }
    }

    /// The UI-side Done button reaches the host as a notification action and
    /// uses the same save-and-close path as Enter-on-last-line.
    #[test]
    fn close_sign_editor_action_ships_done_payload() {
        use crate::live_session::WorkerCommand;
        let mut app = GpuApp::new(GpuCliConfig::default());
        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(8);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        app.attach_live(live);
        app.ui.sign_editor = Some(SignEditorState::new((2, 64, 3), false, Default::default()));
        app.ui
            .sign_editor
            .as_mut()
            .expect("sign editor")
            .insert_str("done");

        app.dispatch_ui_action(&UiAction::CloseSignEditor);

        assert!(app.ui.sign_editor.is_none());
        match rx.try_recv() {
            Ok(WorkerCommand::UpdateSign {
                pos,
                is_front,
                lines,
            }) => {
                assert_eq!(pos, (2, 64, 3));
                assert!(!is_front);
                assert_eq!(lines[0], "done");
            }
            _ => panic!("CloseSignEditor must queue the sign update"),
        }
    }

    /// Scrollable menu screens consume wheel input through `UiState::scroll_menu`;
    /// the host sign-flips winit's positive scroll-up delta and keeps gameplay
    /// screens on the hotbar/container paths.
    #[test]
    fn menu_wheel_forwarding_scrolls_settings_create_world_and_friends() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        let _ = app.ui.render(320, 100);
        let wheel_down = winit::event::MouseScrollDelta::LineDelta(0.0, -1.0);
        let wheel_up = winit::event::MouseScrollDelta::LineDelta(0.0, 1.0);

        app.ui.navigate(Screen::Settings(SettingsTab::Controls));
        assert!(app.scroll_menu_from_wheel(wheel_down));
        assert!(app.ui.menu_scroll.settings_controls > 0);
        let scrolled = app.ui.menu_scroll.settings_controls;
        assert!(app.scroll_menu_from_wheel(wheel_up));
        assert!(app.ui.menu_scroll.settings_controls < scrolled);

        app.ui.navigate(Screen::Singleplayer);
        assert!(app.scroll_menu_from_wheel(wheel_down));
        assert!(app.ui.menu_scroll.singleplayer_create > 0);

        app.ui.navigate(Screen::Friends);
        assert!(
            app.scroll_menu_from_wheel(winit::event::MouseScrollDelta::PixelDelta(
                PhysicalPosition::new(0.0, -18.0)
            ))
        );
        assert!(app.ui.menu_scroll.friends > 0);

        app.ui.navigate(Screen::InGameHud);
        let before = app.ui.menu_scroll.friends;
        assert!(!app.scroll_menu_from_wheel(wheel_down));
        assert_eq!(app.ui.menu_scroll.friends, before);
    }

    /// The `sign_update` WorkerCommand encodes through to a wire body the
    /// worker's `send_raw` path writes verbatim.
    #[test]
    fn update_sign_command_encodes_wire_payload() {
        let payload = crate::live_session::encode_sign_update_payload(
            (-300, 64, 12345),
            false,
            &[
                "看板".to_owned(),
                String::new(),
                "l3".to_owned(),
                String::new(),
            ],
        );
        let mut r = megu_client::protocol::WireReader::new(&payload);
        assert_eq!(r.read_position().unwrap(), [-300, 64, 12345]);
        assert!(!r.read_bool().unwrap());
        assert_eq!(r.read_string(384).unwrap(), "看板");
        assert_eq!(r.read_string(384).unwrap(), "");
        assert_eq!(r.read_string(384).unwrap(), "l3");
        assert_eq!(r.read_string(384).unwrap(), "");
        assert_eq!(r.remaining(), 0);
    }

    /// REALISM-DYNAMIC-LIGHTS-LIVE: the quality ladder drives the
    /// layered voxel-cloud stratum count through `apply_settings` —
    /// `Balanced` keeps 2 layers, `High`/`Ultra` and the cinematic
    /// `MeguNatural` preset earn 3, `Fast` trims to 1.
    #[test]
    fn apply_settings_scales_voxel_cloud_layers_with_the_quality_ladder() {
        fn layers(video: VideoPreset, visual: VisualStylePreset) -> u8 {
            let mut app = GpuApp::new(GpuCliConfig::default());
            let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
            let (outgoing, _rx) = std::sync::mpsc::sync_channel(8);
            app.attach_live(LiveSession::with_channels(incoming, outgoing));
            let mut settings = UiSettings::default();
            settings.video.preset = video;
            settings.visual.preset = visual;
            app.apply_settings(&settings);
            app.live
                .as_ref()
                .unwrap()
                .vanilla_plus_cfg
                .nature
                .cloud_layers
        }

        assert_eq!(layers(VideoPreset::Fast, VisualStylePreset::Vanilla), 1);
        assert_eq!(layers(VideoPreset::Balanced, VisualStylePreset::Vanilla), 2);
        assert_eq!(layers(VideoPreset::High, VisualStylePreset::Vanilla), 3);
        assert_eq!(layers(VideoPreset::Ultra, VisualStylePreset::Vanilla), 3);
        // Cinematic visual preset wins the third stratum even on the
        // balanced video preset.
        assert_eq!(
            layers(VideoPreset::Balanced, VisualStylePreset::MeguNatural),
            3
        );
    }

    /// CLIENT-GPU-SETTINGS-GLINT-RELOAD-WIRE: the five lighting/gameplay
    /// switches ride `ClientSettings` -> `UiSettings` on load and
    /// `UiSettings` -> `ClientSettings` on apply/save, so a TOML value
    /// reaches the Video page and a page toggle persists.
    #[test]
    fn lighting_gameplay_settings_sync_bidirectionally() {
        let client = ClientSettings {
            comfort_exposure: false,
            dynamic_lights: false,
            enchantment_glint: false,
            indirect_light_bounce: false,
            directional_shadow_gameplay: true,
            lazy_catchup_gameplay: true,
            ..ClientSettings::default()
        };

        let mut ui = UiSettings::default();
        apply_client_settings_to_ui(&client, &mut ui);
        assert!(!ui.video.comfort_exposure);
        assert!(!ui.video.dynamic_lights);
        assert!(!ui.video.enchantment_glint);
        assert!(!ui.video.indirect_light_bounce);
        assert!(ui.video.directional_shadow_gameplay);
        assert!(ui.video.lazy_catchup_gameplay);

        ui.video.comfort_exposure = true;
        ui.video.dynamic_lights = true;
        ui.video.enchantment_glint = true;
        ui.video.indirect_light_bounce = true;
        ui.video.directional_shadow_gameplay = false;
        ui.video.lazy_catchup_gameplay = false;
        let out = ui_to_client_settings(&ui, &client);
        assert!(out.comfort_exposure);
        assert!(out.dynamic_lights);
        assert!(out.enchantment_glint);
        assert!(out.indirect_light_bounce);
        assert!(!out.directional_shadow_gameplay);
        assert!(!out.lazy_catchup_gameplay);
        // File-only fields ride `base` untouched through the overlay.
        assert_eq!(out.shader_pack, client.shader_pack);
        assert_eq!(out.report_endpoint, client.report_endpoint);
    }

    /// The hosted-server env pair stays canonical: `true` -> `"1"`,
    /// `false` -> `"0"` (`ops_bool_str` accepts the wider alphabet, but
    /// the spawn contract pins the short form).
    #[test]
    fn directional_shadow_gameplay_maps_to_env_flag() {
        assert_eq!(directional_shadow_light_env_value(true), "1");
        assert_eq!(directional_shadow_light_env_value(false), "0");
    }

    /// The 2D glint overlay emits a subtle non-zero shimmer over an
    /// enchanted stack while enabled, and `None` when the switch is off
    /// or the stack carries no foil — `enchantments`/`stored_enchantments`
    /// need a non-empty VarInt count, `enchantment_glint_override` a set
    /// byte.
    #[test]
    fn enchant_glint_overlay_is_subtle_shimmer_only_when_enabled_and_enchanted() {
        fn stack(patch: Option<(i32, Vec<u8>)>) -> ItemStackComponents {
            ItemStackComponents {
                count: 1,
                item_id: 5,
                added: patch
                    .map(
                        |(type_id, data)| megu_client::protocol::ItemComponentPatch {
                            type_id,
                            data,
                        },
                    )
                    .into_iter()
                    .collect(),
                removed: Vec::new(),
            }
        }

        let rect = UiRect::new(4, 6, 22, 22);
        let enchanted = stack(Some((9, vec![1, 1, 1])));
        let stored_enchanted = stack(Some((23, vec![1, 1, 1])));
        let glint_override = stack(Some((18, vec![1])));
        let empty_enchantments = stack(Some((9, vec![0, 1])));
        let plain = stack(None);

        // Enabled + enchanted: a FillRect at the slot with the pinned
        // subtle alpha and the exact `enchantment_glint_rgb` tint at the
        // slot's normalized center.
        let cmd = enchant_glint_overlay_cmd(true, &enchanted, rect, 800, 600, 1.25)
            .expect("enchanted stack must shimmer while enabled");
        let expected = enchantment_glint_rgb([(4.0 + 11.0) / 800.0, (6.0 + 11.0) / 600.0], 1.25);
        let channel = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        assert_eq!(
            cmd,
            UiCommand::FillRect {
                rect,
                color: UiColor::rgba(
                    channel(expected[0]),
                    channel(expected[1]),
                    channel(expected[2]),
                    GLINT_OVERLAY_ALPHA,
                ),
            }
        );
        let UiCommand::FillRect { color, .. } = cmd else {
            unreachable!()
        };
        assert_eq!(color.a, GLINT_OVERLAY_ALPHA);
        assert!(color.r > 0 || color.g > 0 || color.b > 0);

        // Stored enchantments and the explicit override shimmer too.
        assert!(enchant_glint_overlay_cmd(true, &stored_enchanted, rect, 800, 600, 0.5).is_some());
        assert!(enchant_glint_overlay_cmd(true, &glint_override, rect, 800, 600, 0.5).is_some());

        // Disabled, unenchanted, and empty-list stacks emit nothing.
        assert!(enchant_glint_overlay_cmd(false, &enchanted, rect, 800, 600, 1.25).is_none());
        assert!(enchant_glint_overlay_cmd(true, &plain, rect, 800, 600, 1.25).is_none());
        assert!(
            enchant_glint_overlay_cmd(true, &empty_enchantments, rect, 800, 600, 1.25).is_none()
        );
    }

    /// Test-local `add_entity` wire path — mirrors the live_session
    /// fixture so entity/BER staging tests exercise the real session
    /// tracker instead of poking private maps.
    fn window_test_add_entity(live: &mut LiveSession, eid: i32, entity_type: i32, pos: [f64; 3]) {
        use megu_client::protocol::{ClientPlayEvent, PlayEvent};
        live.apply_event(ClientPlayEvent::Play(PlayEvent::AddEntity {
            eid,
            uuid: [eid as u8; 16],
            entity_type,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        }))
        .unwrap();
    }

    /// Air column at `(0, 0)` with one solid floor section under y=64 so
    /// the contact-shadow collision probe reports a top of 64.0.
    pub(super) fn window_test_seed_floor_column(live: &mut LiveSession) {
        use megu_client::world::{ChunkColumn, StoredSection};
        let state_count = live.world.state_count;
        let mut sections: Vec<StoredSection> = (0..live.world.dimension.section_count())
            .map(|_| StoredSection {
                palette: megu_kernel::PalettedSection::new_single_bounded(0, state_count)
                    .expect("air state must fit the bound"),
                block_light: [0; megu_client_mesh::LIGHT_BYTES],
                sky_light: [0xff; megu_client_mesh::LIGHT_BYTES],
                biomes: [0; 64],
                has_non_air: false,
            })
            .collect();
        sections[7] = StoredSection {
            palette: megu_kernel::PalettedSection::new_single_bounded(5, state_count).unwrap(),
            block_light: [0; megu_client_mesh::LIGHT_BYTES],
            sky_light: [0x00; megu_client_mesh::LIGHT_BYTES],
            biomes: [0; 64],
            has_non_air: true,
        };
        live.world.columns.insert(
            (0, 0),
            ChunkColumn {
                cx: 0,
                cz: 0,
                sections,
            },
        );
    }

    /// The `entity_shadows` toggle gates live contact-shadow staging:
    /// grounded mobs emit converted quads while enabled and the staged
    /// list is empty the same refresh the toggle is off.
    #[test]
    fn contact_shadow_staging_follows_entity_shadows_gate() {
        let mut live = LiveSession::offline_with_null_audio();
        live.vanilla_plus_cfg.nature.enabled = true;
        super::tests::window_test_seed_floor_column(&mut live);
        // Zombie (type 124, biped) grounded on the floor; the item drop
        // (type 58) never grounds a shadow.
        window_test_add_entity(&mut live, 30, 124, [0.0, 64.0, 0.0]);
        window_test_add_entity(&mut live, 31, 58, [2.0, 64.0, 0.0]);

        let mut staged = Vec::new();
        stage_contact_shadow_quads(&live, false, &mut staged);
        assert!(
            staged.is_empty(),
            "disabled gate must stage zero shadow quads"
        );
        stage_contact_shadow_quads(&live, true, &mut staged);
        assert_eq!(staged.len(), 1, "only the grounded mob shadows");
        let (mesh, tint) = &staged[0];
        assert_eq!(mesh.quads.len(), 1);
        assert!(mesh.quads[0].positions.iter().all(|p| p[1] == 64.015));
        // Grounded alpha 0.30 folded over near-black rgb lands the tint
        // just under white — the lit pipeline's alpha-less fade.
        for channel in tint {
            assert!(
                (*channel - 0.7).abs() < 1e-3,
                "grounded shadow tint must darken ~30%: {tint:?}"
            );
        }
        // Toggling off mid-flight clears the staged list next refresh.
        stage_contact_shadow_quads(&live, false, &mut staged);
        assert!(staged.is_empty());
    }

    /// The `enchantment_glint` toggle decides whether a staged entity's
    /// tint picks up the 3D shimmer: enchanted drops shimmer while on
    /// and fall back to the deterministic per-entity hue while off.
    #[test]
    fn staged_entity_glint_follows_enchantment_glint_gate() {
        use megu_client::protocol::{ClientPlayEvent, PlayEvent, write_varint};
        let mut live = LiveSession::offline_with_null_audio();
        // Dropped enchanted stack: `minecraft:item` (type 58) carrying a
        // component-patched slot at metadata index 8.
        window_test_add_entity(&mut live, 40, 58, [0.0, 64.0, 0.0]);
        let item_id = megu_proto::table::ProtocolTable::v767_builtin()
            .registry_id("minecraft:item", "minecraft:diamond_sword")
            .unwrap() as i32;
        let mut item_meta = vec![8u8, 7];
        item_meta.extend_from_slice(&{
            let mut wire = Vec::new();
            write_varint(&mut wire, 1); // stack count
            write_varint(&mut wire, item_id);
            write_varint(&mut wire, 1); // added component count
            write_varint(&mut wire, 0); // removed component count
            write_varint(&mut wire, 9); // enchantments component id
            wire.extend_from_slice(&[1, 0x1d, 0x05, 0x00]);
            wire
        });
        item_meta.push(0xFF);
        live.apply_event(ClientPlayEvent::Play(PlayEvent::SetEntityData {
            eid: 40,
            raw_entries: item_meta,
        }))
        .unwrap();
        assert!(live.entity_enchanted_item_glint(40));

        let spec = *live.entity_rig(40).unwrap();
        let tracked = live.world.entities.get(&40);
        let pos = [0.0, 64.0, 0.0];
        let mut cache = LiveEntityRenderCache::default();
        let mut refs = Vec::new();
        stage_live_entity(
            &live,
            &mut cache,
            &mut refs,
            false,
            None,
            40,
            spec,
            pos,
            tracked,
            0.05,
        );
        let base = refs[0].2;
        assert_eq!(base, live_entity_tint(spec, tracked));

        let mut refs = Vec::new();
        stage_live_entity(
            &live,
            &mut cache,
            &mut refs,
            true,
            None,
            40,
            spec,
            pos,
            tracked,
            0.05,
        );
        let glinted = refs[0].2;
        assert_ne!(
            glinted, base,
            "enchanted drop must shimmer while the toggle is on"
        );

        // A plain unenchanted drop keeps the base tint either way.
        window_test_add_entity(&mut live, 41, 58, [1.0, 64.0, 0.0]);
        let spec41 = *live.entity_rig(41).unwrap();
        let mut refs = Vec::new();
        stage_live_entity(
            &live,
            &mut cache,
            &mut refs,
            true,
            None,
            41,
            spec41,
            [1.0, 64.0, 0.0],
            live.world.entities.get(&41),
            0.05,
        );
        assert_eq!(
            refs[0].2,
            live_entity_tint(spec41, live.world.entities.get(&41)),
            "unenchanted drop must never shimmer"
        );
    }

    /// Modded or out-of-range entity-type ids never render invisible:
    /// `add_entity` left them unrigged, so the fallback iterator pairs
    /// them with a visible `GenericProxy` spec.
    #[test]
    fn unrigged_entities_resolve_generic_proxy_fallback() {
        use megu_client_mesh::entity_rig::EntityRigArchetype;
        let mut live = LiveSession::offline_with_null_audio();
        // Past the u16 vanilla table — `add_entity` tracks it but never
        // classifies a rig.
        window_test_add_entity(&mut live, 50, 70_000, [3.0, 64.0, 5.0]);
        assert!(live.entity_rig(50).is_none());
        // u16-range but unmapped — same hole through the second gate.
        window_test_add_entity(&mut live, 51, 9_999, [1.0, 64.0, 1.0]);
        assert!(live.entity_rig(51).is_none());
        // A classified zombie must NOT appear in the fallback set.
        window_test_add_entity(&mut live, 52, 124, [0.0, 64.0, 0.0]);

        let fallbacks: std::collections::BTreeMap<i32, EntityRigSpec> =
            unrigged_entity_fallbacks(&live)
                .map(|(eid, spec, _, _)| (eid, spec))
                .collect();
        assert_eq!(fallbacks.len(), 2, "rigged zombie must be excluded");
        for eid in [50, 51] {
            assert_eq!(
                fallbacks[&eid].archetype,
                EntityRigArchetype::GenericProxy,
                "unrigged entity {eid} must get the visible proxy"
            );
        }
        // Staging one end-to-end lands it in the draw refs and the mesh
        // cache — the never-invisible contract, not just a spec.
        let mut cache = LiveEntityRenderCache::default();
        let mut refs = Vec::new();
        for (eid, spec, pos, tracked) in unrigged_entity_fallbacks(&live) {
            stage_live_entity(
                &live,
                &mut cache,
                &mut refs,
                false,
                None,
                eid,
                spec,
                pos,
                Some(tracked),
                0.05,
            );
        }
        assert_eq!(refs.len(), 2);
        assert!(cache.entity_meshes.contains_key(&50));
        assert!(cache.entity_meshes.contains_key(&51));
    }

    /// Phase-1 seamless transfer: the MSP1 channel decoder feeds the
    /// transition state machine — a commit starts the eased crossfade and
    /// suppresses the blocking loading screen only when its committed
    /// dimension matches the live session's.
    #[test]
    fn seamless_portal_channel_drives_crossfade_and_suppression() {
        use megu_proto::seamless_portal::{
            PortalPrefetchHint, SeamlessDimCommit, encode_seamless_portal_packet,
        };
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(LiveSession::offline_with_null_audio());
        assert_eq!(
            app.live.as_ref().unwrap().world.dimension.name,
            "minecraft:overworld"
        );

        // Wrong channel and undecodable payloads are handed back.
        assert!(!app.apply_seamless_portal_channel("megu:other", &[]));
        assert!(!app.apply_seamless_portal_channel(SEAMLESS_PORTAL_CHANNEL, b"garbage"));
        assert!(!app.seamless_loading_suppressed());

        let hint = SeamlessPortalPacket::PrefetchHint(PortalPrefetchHint {
            transfer_id: 7,
            target_dim_id: "minecraft:the_nether".into(),
            target_center_chunk: [0, 0],
            prefetch_radius: 1,
            portal_pos: [0.0, 64.0, 0.0],
            target_spawn: [0.0, 64.0, 0.0],
            target_yaw: 0.0,
            target_pitch: 0.0,
            target_sky_rgb: [0.2, 0.05, 0.05],
            target_fog_rgb: [0.3, 0.08, 0.08],
        });
        assert!(app.apply_seamless_portal_channel(
            SEAMLESS_PORTAL_CHANNEL,
            &encode_seamless_portal_packet(&hint),
        ));
        // A committed transfer starts the crossfade and suppresses the
        // loading screen for the live dimension when ids line up.
        let commit = SeamlessPortalPacket::CommitTransfer(SeamlessDimCommit {
            transfer_id: 7,
            target_dim_id: "minecraft:overworld".into(),
            landing_pos: [0.0, 64.0, 0.0],
            landing_vel: [0.0; 3],
            landing_yaw: 0.0,
            landing_pitch: 0.0,
            crossfade_ms: 280,
            keep_momentum: true,
        });
        assert!(app.apply_seamless_portal_channel(
            SEAMLESS_PORTAL_CHANNEL,
            &encode_seamless_portal_packet(&commit),
        ));
        assert!(
            app.seamless_loading_suppressed(),
            "committed dimension matching the live session must suppress loading"
        );
        assert!(app.seamless_portal.blended_sky_and_fog().is_some());

        // The eased crossfade finishes on monotonic advance; suppression
        // stays latched to the committed dimension.
        app.seamless_portal.advance_ms(10_000.0);
        assert!(app.seamless_portal.blended_sky_and_fog().is_none());
        assert!(app.seamless_loading_suppressed());

        // A commit for a different dimension stops suppressing the live
        // session's loading screen.
        let other = SeamlessPortalPacket::CommitTransfer(SeamlessDimCommit {
            target_dim_id: "minecraft:the_nether".into(),
            ..SeamlessDimCommit {
                transfer_id: 8,
                target_dim_id: String::new(),
                landing_pos: [0.0; 3],
                landing_vel: [0.0; 3],
                landing_yaw: 0.0,
                landing_pitch: 0.0,
                crossfade_ms: 0,
                keep_momentum: false,
            }
        });
        assert!(app.apply_seamless_portal_channel(
            SEAMLESS_PORTAL_CHANNEL,
            &encode_seamless_portal_packet(&other),
        ));
        assert!(
            !app.seamless_loading_suppressed(),
            "a dimension id that is not the live session's must not suppress"
        );
    }

    /// Vanilla default must stay the authentic single-deck cubic cloud;
    /// only Megu Natural + Fancy opts into the hybrid parallax decks.
    #[test]
    fn cloud_deck_mode_maps_preset_and_cloud_mode() {
        // Vanilla preset (the default): every cloud mode stays vanilla.
        for clouds in [CloudMode::Fast, CloudMode::Fancy] {
            assert_eq!(
                cloud_deck_mode(VisualStylePreset::Vanilla, clouds),
                CloudDeckMode::Vanilla,
                "Vanilla preset + {clouds:?} must keep the single cubic deck"
            );
        }
        assert_eq!(
            cloud_deck_mode(VisualStylePreset::Vanilla, CloudMode::Off),
            CloudDeckMode::Off
        );
        // Megu Natural style: Fancy unlocks the hybrid decks, Fast
        // stays one lightweight greedy deck, Off stays empty.
        {
            let preset = VisualStylePreset::MeguNatural;
            assert_eq!(
                cloud_deck_mode(preset, CloudMode::Fancy),
                CloudDeckMode::Hybrid,
                "{preset:?} + Fancy must enable the hybrid decks"
            );
            assert_eq!(
                cloud_deck_mode(preset, CloudMode::Fast),
                CloudDeckMode::Vanilla,
                "{preset:?} + Fast must stay a single greedy deck"
            );
            assert_eq!(cloud_deck_mode(preset, CloudMode::Off), CloudDeckMode::Off);
        }
    }
    /// Right-click resolution: `startUseItem`'s `[MAIN_HAND, OFF_HAND]`
    /// fallback picks the off hand when only it carries a stack, and every
    /// server-bound interaction — target, held-item use, and the arm swing —
    /// carries that same hand.
    #[test]
    fn right_click_carries_the_resolved_offhand_through_every_packet() {
        use crate::live_session::WorkerCommand;
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(16);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        // Empty main hand, shield in the off hand.
        live.hud.hotbar = std::array::from_fn(|_| None);
        live.hud.offhand = Some("minecraft:shield".to_owned());
        assert_eq!(live.use_hand(), LiveSession::OFF_HAND);
        assert!(!live.hands_empty());
        super::tests::window_test_seed_floor_column(&mut live);
        live.world.apply_block_update([0, 65, 3], 5).unwrap();

        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);
        app.ui.navigate(Screen::InGameHud);

        // Block right-click: `use_item_on` and the swing both ride the off
        // hand, and the block branch swings exactly once.
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, WorkerCommand::UseItemOn { hand: 1, .. })),
            "block right-click must carry the off hand"
        );
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, WorkerCommand::Swing { hand: 1 })),
            "block use must swing the acting hand"
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, WorkerCommand::Swing { .. }))
                .count(),
            1,
            "the swing must not be duplicated"
        );

        // Entity right-click: `interact_at` + `interact` + the swing all
        // carry the same off hand.
        app.live
            .as_mut()
            .unwrap()
            .world
            .apply_block_update([0, 65, 3], 0)
            .unwrap();
        window_test_add_entity(app.live.as_mut().unwrap(), 42, 124, [0.0, 64.0, 3.5]);
        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        assert!(
            commands.iter().any(|command| matches!(
                command,
                WorkerCommand::Interact {
                    target: 42,
                    hand: 1,
                    ..
                }
            )),
            "entity right-click must carry the off hand"
        );
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, WorkerCommand::Swing { hand: 1 })),
            "the swing must match the acting hand"
        );
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, WorkerCommand::Swing { hand: 0 })),
            "the empty main hand must not swing"
        );
    }

    /// `F3 + B` / `F3 + G` build the vanilla world-space segment set, and
    /// `F3 + I` names the entity it copies.
    #[test]
    fn f3_hitboxes_borders_and_entity_copy_are_wired() {
        let mut live = LiveSession::offline_with_null_audio();
        live.pose = Some(PlayerPose {
            feet: [1.5, 64.0, 1.5],
            yaw: 0.0,
            pitch: 0.0,
        });
        window_test_add_entity(&mut live, 90, 124, [1.5, 64.0, 4.5]);
        let camera = Camera {
            position: [1.5, 65.62, 1.5],
            ..Camera::default()
        };

        let mut lines = Vec::new();
        build_debug_world_lines(Some(&live), None, &camera, None, false, false, &mut lines);
        assert!(lines.is_empty(), "both overlays off must emit nothing");

        build_debug_world_lines(Some(&live), None, &camera, None, true, false, &mut lines);
        // Living biped: 12 white box edges + 4 red eye-height edges + blue ray.
        assert_eq!(lines.len(), 17, "one zombie hitbox: {lines:?}");
        assert!(lines.iter().any(|line| line.rgba == [255, 0, 0, 255]));
        assert!(lines.iter().any(|line| line.rgba == [0, 0, 255, 255]));
        assert!(lines.iter().any(|line| line.rgba == [255, 255, 255, 255]));

        build_debug_world_lines(Some(&live), None, &camera, None, false, true, &mut lines);
        assert!(!lines.is_empty(), "F3+G must emit chunk borders");
        assert!(lines.iter().any(|line| line.rgba == [255, 0, 0, 128]));
        // Borders are clamped to the dimension's Y span.
        assert!(lines.iter().all(|line| {
            [line.start[1], line.end[1]]
                .iter()
                .all(|y| (live.world.dimension.min_y as f32) <= *y
                    && *y <= (live.world.dimension.min_y + live.world.dimension.height) as f32)
        }));

        assert_eq!(
            debug_entity_target_name(&live, 90),
            "minecraft:zombie",
            "F3+I copies the rig registry name"
        );
        assert_eq!(
            debug_entity_target_name(&live, 31_337),
            "unknown:entity_31337"
        );
        assert!(entity_hitbox_is_living(EntityRigArchetype::Biped));
        assert!(!entity_hitbox_is_living(
            EntityRigArchetype::ProjectileOrItem
        ));
    }

    /// `F3 + H` appends the vanilla metadata suffix: durability, the
    /// namespaced id, and the component count.
    #[test]
    fn advanced_tooltips_append_durability_id_and_component_count() {
        use megu_ui::container_screen::UiItemStack;

        let components = ItemStackComponents {
            count: 1,
            item_id: 840,
            added: vec![
                ItemComponentPatch {
                    // 1561 as a protocol varint: 0x99 (0x19 | continue), 0x0c.
                    type_id: 2,
                    data: vec![0x99, 0x0c],
                },
                ItemComponentPatch {
                    type_id: 3,
                    data: vec![10],
                },
            ],
            removed: Vec::new(),
        };
        let stack = UiItemStack::new(840, 1)
            .with_namespaced_id("minecraft:diamond_pickaxe")
            .with_name("Diamond Pickaxe");
        let decorated = apply_advanced_tooltips(stack, Some(&components));
        assert_eq!(
            decorated.lore,
            [
                "Durability: 1551 / 1561".to_owned(),
                "minecraft:diamond_pickaxe".to_owned(),
                "2 component(s)".to_owned(),
            ],
            "advanced tooltip lines must match the kernel formatter text"
        );

        let plain = UiItemStack::new(1, 1).with_namespaced_id("minecraft:stone");
        assert_eq!(
            apply_advanced_tooltips(plain, None).lore,
            ["minecraft:stone".to_owned()]
        );
    }

    /// Container clicks forward the authoritative component patch instead of
    /// an empty added/removed pair, so enchantments and durability survive.
    #[test]
    fn container_clicks_preserve_authoritative_components() {
        use megu_client::protocol::decode_item_stack_components;

        let mut live = LiveSession::offline_with_null_audio();
        live.player_inventory.insert(
            36,
            ItemStackComponents {
                count: 4,
                item_id: 840,
                added: vec![ItemComponentPatch {
                    // `max_damage` (type 2) as a protocol varint.
                    type_id: 2,
                    data: vec![0x99, 0x0c],
                }],
                removed: vec![12],
            },
        );

        let wire = GpuApp::encode_clicked_stack(&live, 0, 36, 840, 3);
        let (decoded, consumed) = decode_item_stack_components(&wire).expect("decodable slot");
        assert_eq!(consumed, wire.len());
        assert_eq!(decoded.item_id, 840);
        assert_eq!(decoded.count, 3, "the UI's predicted count wins");
        assert_eq!(decoded.added.len(), 1, "component patches are preserved");
        assert_eq!(decoded.added[0].type_id, 2);
        assert_eq!(decoded.removed, vec![12]);

        // Unknown slot / mismatched item fall back to the bare stack.
        let plain = GpuApp::encode_clicked_stack(&live, 0, 37, 1, 5);
        assert_eq!(plain, encode_slot_stack(1, 5));
        assert_eq!(
            GpuApp::encode_clicked_stack(&live, 0, 36, 1, 0),
            encode_slot_stack(0, 0)
        );
    }

    /// `F3 + P` only pauses a singleplayer session that lost focus; a server
    /// session keeps running and an already-paused game is untouched.
    #[test]
    fn lost_focus_pause_respects_singleplayer_and_toggle() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(LiveSession::offline_with_null_audio());
        app.ui.navigate(Screen::InGameHud);
        assert!(app.input_router.debug_chords.pause_on_lost_focus);
        app.pause_for_lost_focus();
        assert!(!app.paused, "multiplayer focus loss must not pause");
        assert_eq!(app.ui.screen, Screen::InGameHud);

        app.input_router.debug_chords.pause_on_lost_focus = false;
        app.pause_for_lost_focus();
        assert!(!app.paused, "F3+P off must never pause on focus loss");
    }

    /// `F3 + T` reports the deterministic value of a headless asset reload
    /// instead of claiming success when no client JAR exists.
    #[test]
    fn f3_t_reload_reports_a_deterministic_outcome() {
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.dispatch_input(WindowInputEvent::DebugChord(
            DebugChordAction::ReloadResourcePacks,
        ));
        let reload_lines: Vec<&str> = app
            .debug_messages
            .iter()
            .map(String::as_str)
            .filter(|line| line.starts_with("Resource reload"))
            .collect();
        assert_eq!(
            reload_lines.len(),
            1,
            "F3+T must report exactly one reload line: {:?}",
            app.debug_messages
        );
        // A headless app never loaded assets, so the chord reports the
        // deterministic unavailable outcome instead of claiming a rescan.
        assert_eq!(reload_lines[0], "Resource reload unavailable");
        assert!(!app.reload_debug_assets());
        assert!(app.vanilla_runtime_assets.is_none());
    }

    /// One `RenderFramePose` drives the camera, the crosshair pick, and the
    /// mining target: a changed pose eye height changes what is picked.
    #[test]
    fn render_frame_pose_drives_camera_and_crosshair_pick() {
        use megu_client::protocol::{ClientPlayEvent, PlayEvent, TeleportSync};

        let mut live = LiveSession::offline_with_null_audio();
        super::tests::window_test_seed_floor_column(&mut live);
        live.world.apply_block_update([0, 65, 3], 5).unwrap();
        live.pose = Some(PlayerPose {
            feet: [0.5, 64.0, 0.5],
            yaw: 0.0,
            pitch: 0.0,
        });
        // Standing eye height sees the stone ahead.
        assert!(matches!(
            live.crosshair_pick(),
            Some(CrosshairPick::Block(hit)) if hit.block_pos == [0, 65, 3]
        ));
        // An installed frame pose owns the eye origin: raised above the
        // block's top face, the same look no longer hits it.
        let raised = RenderFramePose::new([0.5, 64.0, 0.5], 2.5, 0.0, 0.0);
        live.set_render_pose(raised);
        assert!(live.crosshair_pick().is_none());
        assert_eq!(live.render_frame_pose(), Some(raised));
        assert_eq!(raised.eye, [0.5, 66.5, 0.5]);
        assert!((raised.look[2] - 1.0).abs() < f64::EPSILON);

        // The window installs the physics pose as the frame pose and copies
        // exactly its eye origin into the render camera.
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(LiveSession::offline_with_null_audio());
        let mut live = app.live.take().unwrap();
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
            x: 0.5,
            y: 64.0,
            z: 0.5,
            yaw: 0.0,
            pitch: 0.0,
            flags: 0,
            teleport_id: 1,
        })))
        .unwrap();
        app.attach_live(live);
        app.advance_frame_movement(0.05, false);
        let pose = app
            .live
            .as_ref()
            .and_then(LiveSession::render_frame_pose)
            .expect("frame pose installed");
        let expected: [f32; 3] = pose.eye.map(|axis| axis as f32);
        assert_eq!(app.camera.position, expected);
        assert_eq!(pose.yaw, app.camera.yaw_deg - 180.0);
    }

    /// Remote-entity smoothing is dt-independent: two half steps land on the
    /// same pose as one full step through `exp_decay_exact`.
    #[test]
    fn entity_smoothing_is_dt_independent() {
        let mut live = LiveSession::offline_with_null_audio();
        window_test_add_entity(&mut live, 91, 124, [0.0, 64.0, 0.0]);
        let mut one_step = LiveEntityRenderCache::default();
        let mut two_steps = LiveEntityRenderCache::default();

        let first = interpolate_entity(
            &mut one_step,
            91,
            [0.0, 64.0, 0.0],
            live.world.entities.get(&91),
            0.05,
        );
        assert_eq!(first.position, [0.0, 64.0, 0.0]);
        interpolate_entity(
            &mut two_steps,
            91,
            [0.0, 64.0, 0.0],
            live.world.entities.get(&91),
            0.05,
        );

        let full = interpolate_entity(
            &mut one_step,
            91,
            [4.0, 64.0, 0.0],
            live.world.entities.get(&91),
            0.05,
        );
        let half = interpolate_entity(
            &mut two_steps,
            91,
            [4.0, 64.0, 0.0],
            live.world.entities.get(&91),
            0.025,
        );
        let second_half = interpolate_entity(
            &mut two_steps,
            91,
            [4.0, 64.0, 0.0],
            live.world.entities.get(&91),
            0.025,
        );
        assert!((half.position[0] - full.position[0]).abs() > 0.1);
        assert!(
            (second_half.position[0] - full.position[0]).abs() < 1.0e-9,
            "{} vs {}",
            second_half.position[0],
            full.position[0]
        );
    }

}

#[cfg(test)]
mod title_text_filter_tests {
    use super::*;

    #[test]
    fn boot_recipe_ingest_stays_out_of_title_status_and_hud_chat() {
        let game = std::env::temp_dir().join(format!("megu-recipe-status-{}", std::process::id()));
        let recipes = game.join("saves/world/datapacks/test/data/test/recipe");
        std::fs::create_dir_all(&recipes).unwrap();
        std::fs::write(
            recipes.join("press.json"),
            br#"{"type":"test:pressing","ingredients":[{"item":"test:raw"}],"results":[{"id":"test:plate"}]}"#,
        )
        .unwrap();
        let app = GpuApp::new(GpuCliConfig {
            game_dir: Some(game.clone()),
            ..Default::default()
        });
        std::fs::remove_dir_all(game).unwrap();

        // The vanilla JAR resolves through the unified resolver, so hosts
        // with a real launcher install legitimately ingest the full vanilla
        // recipe set on top of the one datapack recipe.
        assert!(app.recipe_ingest_report().recipes_loaded >= 1);
        assert!(app.ui.title_status.as_deref().is_none_or(|line| !line.starts_with("Recipe ingest:")));
        assert!(app.ui.hud.chat_lines.iter().all(|line| !line.starts_with("Recipe ingest:")));
    }

    #[test]
    fn vanilla_crosshair_target_has_no_look_at_card() {
        let mut ui = UiState::default();
        ui.screen = Screen::InGameHud;
        let mut live = LiveSession::offline_with_null_audio();
        live.looking_at_block = Some(([0, 0, 0], "minecraft:stone".to_owned()));

        GpuApp::sync_looking_at_hud(&mut ui, &live);
        assert!(!ui.hud.looking_at.is_visible());
    }

    #[test]
    fn builtin_title_text_filter_strips_wordmark_and_edition() {
        let text_cmd = |scale: u32, text: &str| UiCommand::Text {
            x: 0,
            y: 0,
            scale,
            color: UiColor::rgb(255, 255, 255),
            text: text.to_owned(),
        };
        for scale in 1..=4 {
            assert!(is_builtin_title_text(&text_cmd(3 * scale, "MINECRAFT")));
            for label in [
                "MINECRAFT",
                "MEGU EDITION",
                "clean-room native client",
                "クリーンルーム ネイティブクライアント",
            ] {
                assert!(is_builtin_title_text(&text_cmd(scale, label)));
            }
            assert!(!is_builtin_title_text(&text_cmd(scale, "Options...")));
        }
    }
}

#[cfg(test)]
mod hud_sprite_binding_tests {
    use super::*;
    use crate::vanilla_gui_sprites::{
        SpriteAtlasEntry, VanillaGuiSpriteId, VanillaGuiTextureAtlas,
    };
    use megu_ui::draw::hud_sprite_ids;

    fn placement(width: u32, height: u32) -> SpriteAtlasEntry {
        SpriteAtlasEntry {
            uv_min: [0.25, 0.5],
            uv_max: [0.75, 1.0],
            width,
            height,
            nine_slice_border: None,
        }
    }

    #[test]
    fn logical_hud_bindings_include_typed_named_and_effect_sprites() {
        let mut textures = VanillaGuiTextureAtlas::default();
        textures
            .entries
            .push((VanillaGuiSpriteId::Hotbar, placement(182, 22)));
        textures
            .named_entries
            .push(("sprites/hud/food_half.png".to_string(), placement(9, 9)));
        textures.named_entries.push((
            "sprites/hud/heart/poisoned_full.png".to_string(),
            placement(9, 9),
        ));
        textures
            .named_entries
            .push(("mob_effect/speed".to_string(), placement(18, 18)));
        textures.named_entries.push((
            "sprites/container/furnace/burn_progress.png".to_string(),
            placement(16, 16),
        ));

        let mut atlas = megu_ui::draw::GuiSpriteAtlas::default();
        install_logical_hud_sprite_bindings(&mut atlas, &textures);

        for key in [
            hud_sprite_ids::HOTBAR,
            hud_sprite_ids::FOOD_HALF,
            "hud/heart/poisoned_full",
            "mob_effect/speed",
        ] {
            let bound = atlas
                .named_sprite(key)
                .unwrap_or_else(|| panic!("missing logical HUD binding {key}"));
            assert_eq!(bound.uv_min, [0.25, 0.5], "{key}");
            assert_eq!(bound.uv_max, [0.75, 1.0], "{key}");
        }
        assert!(
            atlas
                .named_sprite("hud/container/furnace/burn_progress")
                .is_none(),
            "non-HUD atlas rectangles must not become stable HUD bindings"
        );
    }
}

#[cfg(test)]
mod viewmodel_wiring_tests {
    use super::*;

    fn main_vm() -> FirstPersonViewmodel {
        FirstPersonViewmodel {
            visible: true,
            held_item_name: Some("minecraft:diamond_sword".to_owned()),
            held_color_rgb: [1.0; 3],
            is_block: false,
            swing_progress: 0.4,
            use_progress: 0.5,
            using_kind: FirstPersonUseKind::Eat,
            walk_bob_x: 0.1,
            walk_bob_y: -0.05,
            equip_progress: 0.75,
        }
    }

    #[test]
    fn offhand_viewmodel_substitutes_slot45_item_and_block_class() {
        let vm = offhand_first_person_viewmodel(
            &main_vm(),
            Some("minecraft:torch".to_owned()),
            LiveSession::MAIN_HAND,
            LiveSession::MAIN_HAND,
        );
        assert!(vm.visible);
        assert_eq!(vm.held_item_name.as_deref(), Some("minecraft:torch"));
        assert!(vm.is_block, "torch is a blocky held item");
        assert_eq!(vm.swing_progress, 0.0, "main-hand swing stays home");
        assert_eq!(vm.use_progress, 0.0);
        assert_eq!(vm.using_kind, FirstPersonUseKind::None);
        // Shared timing/bob/equip fields carry over from the main snapshot.
        assert_eq!(vm.walk_bob_x, 0.1);
        assert_eq!(vm.walk_bob_y, -0.05);
        assert_eq!(vm.equip_progress, 0.75);
    }

    #[test]
    fn offhand_viewmodel_animates_only_for_the_off_hand() {
        let swung = offhand_first_person_viewmodel(
            &main_vm(),
            Some("minecraft:shield".to_owned()),
            LiveSession::OFF_HAND,
            LiveSession::OFF_HAND,
        );
        assert_eq!(swung.swing_progress, 0.4);
        assert_eq!(swung.use_progress, 0.5);
        assert_eq!(swung.using_kind, FirstPersonUseKind::Eat);
        assert!(!swung.is_block, "shield is a flat held item");

        // A main-hand swing/use masks the offhand back to rest while the
        // main snapshot keeps its own animation.
        let masked = {
            let mut vm = main_vm();
            if LiveSession::MAIN_HAND == LiveSession::OFF_HAND {
                vm.swing_progress = 0.0;
            }
            vm
        };
        assert_eq!(masked.swing_progress, 0.4);
    }

    #[test]
    fn offhand_viewmodel_empty_hand_still_tracks_visibility() {
        let vm = offhand_first_person_viewmodel(
            &main_vm(),
            None,
            LiveSession::MAIN_HAND,
            LiveSession::MAIN_HAND,
        );
        assert!(vm.visible, "visibility mirrors the first-person gate");
        assert!(vm.held_item_name.is_none());
        assert!(!vm.is_block);
    }

    #[test]
    fn held_item_block_classification_matches_session_list() {
        for name in [
            "minecraft:torch",
            "minecraft:oak_planks",
            "minecraft:diamond",
        ] {
            assert!(viewmodel_held_is_block(name), "{name} should be a block");
        }
        for name in [
            "minecraft:diamond_sword",
            "minecraft:shield",
            "minecraft:bow",
            "minecraft:potion",
            "minecraft:bread",
        ] {
            assert!(!viewmodel_held_is_block(name), "{name} should be flat");
        }
    }

    #[test]
    fn arm_skin_fingerprint_is_content_sensitive() {
        let mut a = vec![0u8; 64 * 64 * 4];
        let mut b = a.clone();
        assert_eq!(arm_skin_fingerprint(&a), arm_skin_fingerprint(&b));
        b[128] ^= 0xFF;
        assert_ne!(arm_skin_fingerprint(&a), arm_skin_fingerprint(&b));
        a.resize(65 * 64 * 4, 0);
        assert_ne!(arm_skin_fingerprint(&a), arm_skin_fingerprint(&b));
    }

    /// Vanilla `Camera.getMaxZoom(4.0)`: the third-person camera stops just
    /// before the wall face instead of phasing through it, and the
    /// `thirdPersonReverse` mirror pulls along the flipped axis rather than
    /// negating the distance.
    #[test]
    fn third_person_camera_clips_before_a_wall_at_two_and_a_half_blocks() {
        let eye = [0.5, 65.62, 0.5];
        let back = [0.0, 0.0, 1.0];
        assert_eq!(
            camera_max_zoom(eye, back, THIRD_PERSON_DISTANCE, |_| false),
            THIRD_PERSON_DISTANCE,
            "open air keeps the full 4-block pull-back"
        );
        // Solid voxels from z = 3 upward: the closest face sits 2.5 blocks
        // behind the eye centre, and the `+0.1` corner probe reaches it first.
        let wall = |cell: [i32; 3]| cell[2] >= 3;
        let clipped = camera_max_zoom(eye, back, THIRD_PERSON_DISTANCE, wall);
        assert!(clipped < 2.5, "sweep must stop before the wall: {clipped}");
        assert!(
            (clipped - 2.4).abs() < 1e-4,
            "corner ray must stop at the wall face: {clipped}"
        );
        // A block behind the sweep must never shorten it.
        assert_eq!(
            camera_max_zoom(eye, back, THIRD_PERSON_DISTANCE, |cell| cell[2] < 0),
            THIRD_PERSON_DISTANCE
        );
        // Degenerate input stays bounded rather than producing NaN.
        assert_eq!(camera_max_zoom(eye, [0.0; 3], 4.0, |_| false), 4.0);
        assert_eq!(camera_max_zoom(eye, back, f32::NAN, |_| false), 0.0);
        assert_eq!(
            camera_max_zoom([f32::NAN; 3], back, 4.0, |_| false),
            0.0
        );

        let mut controller = CameraController::default();
        let player = Camera {
            position: eye,
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            ..Camera::default()
        };
        // A wall on both sides of the eye so the mirrored front view clips
        // against its own face.
        let walls = |cell: [i32; 3]| cell[2].abs() >= 3;
        assert_eq!(controller.cycle_mode(), CameraMode::ThirdPersonBack);
        assert!(
            (controller.view_camera(&player).position[2] - 4.5).abs() < 1e-4,
            "the world-free view camera keeps the vanilla 4-block offset"
        );
        let clipped_back = controller.clip_view_camera(&player, walls);
        assert!((clipped_back.position[2] - 2.9).abs() < 1e-4);
        // Mirror first, then pull back: the front view lands 2.4 blocks in
        // front of the eye on the same axis, without a negated distance.
        assert_eq!(controller.cycle_mode(), CameraMode::ThirdPersonFront);
        let clipped_front = controller.clip_view_camera(&player, walls);
        assert!((clipped_front.position[2] + 1.9).abs() < 1e-4);
        assert!((clipped_front.pitch_deg + player.pitch_deg).abs() < f32::EPSILON);
    }

    /// Vanilla `AbstractClientPlayer.getFieldOfViewModifier` rows, in order:
    /// normal, sprint (speed attribute), speed/slowness, bow draw, scoped
    /// spyglass, then the `screenEffectScale` lerp.
    #[test]
    fn fov_pipeline_matches_the_five_vanilla_rows() {
        let rows = [
            ("normal", FovModifierInputs::default(), 1.0f32),
            (
                "sprint",
                FovModifierInputs {
                    movement_speed_ratio: 1.3,
                    ..FovModifierInputs::default()
                },
                1.15,
            ),
            (
                "speed",
                FovModifierInputs {
                    movement_speed_ratio: 1.2,
                    ..FovModifierInputs::default()
                },
                1.1,
            ),
            (
                "slowness",
                FovModifierInputs {
                    movement_speed_ratio: 0.85,
                    ..FovModifierInputs::default()
                },
                0.925,
            ),
            (
                "bow",
                FovModifierInputs {
                    bow_drawn: true,
                    bow_draw_ticks: BOW_FOV_DRAW_TICKS,
                    ..FovModifierInputs::default()
                },
                0.85,
            ),
            (
                "spyglass",
                FovModifierInputs {
                    spyglass_scoped: true,
                    flying: true,
                    movement_speed_ratio: 1.3,
                    bow_drawn: true,
                    bow_draw_ticks: BOW_FOV_DRAW_TICKS,
                    ..FovModifierInputs::default()
                },
                SPYGLASS_FOV_MODIFIER,
            ),
        ];
        for (row, inputs, expected) in rows {
            let modifier = compute_vanilla_fov_modifier(&inputs);
            assert!(
                (modifier - expected).abs() < 1e-6,
                "{row} row: {modifier} != {expected}"
            );
        }

        // Flying multiplies on top of the speed row (`*1.1`).
        let flying = compute_vanilla_fov_modifier(&FovModifierInputs {
            flying: true,
            ..FovModifierInputs::default()
        });
        assert!((flying - FLYING_FOV_BOOST).abs() < 1e-6);

        // Half-drawn bow sits between the full draw and no draw.
        let half_draw = compute_vanilla_fov_modifier(&FovModifierInputs {
            bow_drawn: true,
            bow_draw_ticks: BOW_FOV_DRAW_TICKS * 0.5,
            ..FovModifierInputs::default()
        });
        assert!(half_draw > 0.85 && half_draw < 1.0);

        // `fovEffectScale` lerps the whole modifier toward 1.0.
        let pinned = compute_vanilla_fov_modifier(&FovModifierInputs {
            movement_speed_ratio: 1.3,
            fov_effect_scale: 0.0,
            ..FovModifierInputs::default()
        });
        assert!((pinned - 1.0).abs() < 1e-6);
        let half_scale = compute_vanilla_fov_modifier(&FovModifierInputs {
            movement_speed_ratio: 1.3,
            fov_effect_scale: 0.5,
            ..FovModifierInputs::default()
        });
        assert!((half_scale - 1.075).abs() < 1e-6);

        // Degenerate inputs stay finite and neutral.
        let nan = compute_vanilla_fov_modifier(&FovModifierInputs {
            movement_speed_ratio: f32::NAN,
            bow_drawn: true,
            bow_draw_ticks: f32::INFINITY,
            fov_effect_scale: f32::NAN,
            ..FovModifierInputs::default()
        });
        assert!((nan - 1.0).abs() < 1e-6, "degenerate row: {nan}");

        let mut controller = CameraController::new(70.0, 1.0, 4.0);
        assert_eq!(controller.effective_fov_deg(), 70.0);
        controller.set_fov_environment(FovModifierInputs {
            flying: true,
            ..FovModifierInputs::default()
        });
        assert!((controller.effective_fov_deg() - 77.0).abs() < 1e-4);
        controller.set_fov_environment(FovModifierInputs {
            spyglass_scoped: true,
            ..FovModifierInputs::default()
        });
        assert!(
            (controller.effective_fov_deg() - 7.0).abs() < 1e-4,
            "the scoped spyglass must reach vanilla's 0.1x, not the 10 degree floor"
        );
    }

    /// Vanilla `GameRenderer.bobHurt`: `sin(f⁴ · π) · 14°` rises out of the
    /// hit, peaks shortly after, and decays back to zero; a side hit tips the
    /// pitch instead of rolling the view.
    #[test]
    fn bob_hurt_tilt_peaks_and_decays_cleanly() {
        // Zero at the instant of the hit (`f = 1`) and once it ends.
        assert!(compute_hurt_tilt(1.0, 0.0, 0.0, 1.0).roll_rad.abs() < 1e-4);
        assert_eq!(
            compute_hurt_tilt(0.0, 0.0, 0.0, 1.0),
            HurtTilt::default()
        );
        let peak_progress = 0.5f32.powf(0.25);
        let peak = compute_hurt_tilt(peak_progress, 0.0, 0.0, 1.0);
        assert!(
            (peak.roll_rad - (-HURT_TILT_PEAK_DEG).to_radians()).abs() < 1e-3,
            "peak roll: {}",
            peak.roll_rad
        );
        assert!(peak.pitch_deg.abs() < 1e-3);
        let rising = compute_hurt_tilt(0.95, 0.0, 0.0, 1.0).roll_rad;
        let falling = compute_hurt_tilt(0.6, 0.0, 0.0, 1.0).roll_rad;
        assert!(peak.roll_rad < rising && rising < 0.0);
        assert!(peak.roll_rad < falling && falling < 0.0);
        // Attack from the side: the tilt becomes a pitch bend, not a roll.
        let side = compute_hurt_tilt(peak_progress, 90.0, 0.0, 1.0);
        assert!(side.roll_rad.abs() < 1e-3, "side roll: {}", side.roll_rad);
        assert!((side.pitch_deg - HURT_TILT_PEAK_DEG).abs() < 1e-3);
        // `damageTiltStrength` scales linearly and 0 disables the effect.
        let half = compute_hurt_tilt(peak_progress, 0.0, 0.0, 0.5);
        assert!((half.roll_rad - peak.roll_rad * 0.5).abs() < 1e-4);
        assert_eq!(compute_hurt_tilt(peak_progress, 0.0, 0.0, 0.0), HurtTilt::default());
        // Degenerate inputs never reach the camera.
        assert_eq!(compute_hurt_tilt(f32::NAN, 0.0, 0.0, 1.0), HurtTilt::default());
        assert_eq!(compute_hurt_tilt(-1.0, 0.0, 0.0, 1.0), HurtTilt::default());
        assert_eq!(
            compute_hurt_tilt(peak_progress, 0.0, f32::INFINITY, 1.0),
            HurtTilt::default()
        );
    }

    /// `hurt_animation` for the local player reaches the render camera: the
    /// session forwards the attack yaw and its 10-tick phase, the window
    /// derives the `bobHurt` tilt, and the tilt clears when the animation
    /// runs out.
    #[test]
    fn hurt_animation_forwards_a_bob_hurt_tilt_into_the_view_camera() {
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let mut live = LiveSession::offline_with_null_audio();
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.apply_event(ClientPlayEvent::Play(PlayEvent::HurtAnimation {
            eid: 11,
            yaw: 0.0,
        }))
        .unwrap();
        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);
        app.ui.navigate(Screen::InGameHud);
        // `sin(f⁴π)` is zero at the instant of the hit, so the tilt has to
        // rise over the first ticks rather than snap on.
        for _ in 0..3 {
            app.live.as_mut().unwrap().tick_gameplay_frame(0.05);
        }
        app.advance_camera_effects(0.0, false);
        assert!(
            app.hurt_tilt.roll_rad < -0.05,
            "hurt tilt must rise after the hit: {}",
            app.hurt_tilt.roll_rad
        );
        assert!(
            app.hurt_tilt.roll_rad > (-HURT_TILT_PEAK_DEG).to_radians(),
            "hurt tilt must stay under the vanilla 14° peak"
        );
        assert!(app.session_view_camera().roll_rad < 0.0);
        // The 10-tick animation expires and the camera returns to level.
        for _ in 0..12 {
            app.live.as_mut().unwrap().tick_gameplay_frame(0.05);
        }
        app.advance_camera_effects(0.0, false);
        assert_eq!(app.hurt_tilt, HurtTilt::default());
        assert_eq!(app.session_view_camera().roll_rad, 0.0);
        assert!(app.live.as_ref().unwrap().local_hurt_animation().is_none());
    }

    /// Vanilla `Minecraft.rightClickDelay = 4`: the press edge fires
    /// `startUseItem` on tick 0 and the held repeat fires again on tick 4.
    #[test]
    fn right_click_delay_fires_on_ticks_zero_and_four() {
        let mut delay = RightClickDelay::default();
        delay.on_start_use();
        assert_eq!(delay.remaining_ticks, RIGHT_CLICK_DELAY_TICKS);
        let mut fired = Vec::new();
        for tick in 1..=8u32 {
            if delay.tick() {
                fired.push(tick);
                delay.on_start_use();
            }
        }
        assert_eq!(fired, vec![4, 8], "4-tick held-use cadence: {fired:?}");
        delay.on_release();
        assert_eq!(delay, RightClickDelay::default());
    }

    /// Holding right-click keeps placing: the frame clock folds into 20 Hz
    /// ticks and `startUseItem` re-runs on tick 4, not before.
    #[test]
    fn holding_right_click_repeats_the_block_use_every_four_ticks() {
        use crate::live_session::WorkerCommand;
        use megu_client::protocol::{ClientPlayEvent, DimensionMeta, PlayEvent};

        let (_tx, incoming) = std::sync::mpsc::sync_channel(8);
        let (outgoing, rx) = std::sync::mpsc::sync_channel(64);
        let mut live = LiveSession::with_channels(incoming, outgoing);
        live.status = SessionStatus::Playing;
        live.apply_event(ClientPlayEvent::Play(PlayEvent::JoinGame {
            entity_id: 11,
            dimension: DimensionMeta::default(),
            hardcore: false,
        }))
        .unwrap();
        live.pose = Some(PlayerPose {
            feet: [0.0, 64.0, 0.0],
            yaw: 0.0,
            pitch: 0.0,
        });
        live.hud.hotbar = std::array::from_fn(|_| None);
        live.hud.hotbar[0] = Some("minecraft:stone".to_owned());
        live.hud.selected_slot = 0;
        super::tests::window_test_seed_floor_column(&mut live);
        live.world.apply_block_update([0, 65, 3], 5).unwrap();

        let mut app = GpuApp::new(GpuCliConfig::default());
        app.attach_live(live);
        app.ui.navigate(Screen::InGameHud);

        let drain_block_uses = |rx: &std::sync::mpsc::Receiver<WorkerCommand>| {
            let mut count = 0;
            while let Ok(command) = rx.try_recv() {
                if matches!(command, WorkerCommand::UseItemOn { .. }) {
                    count += 1;
                }
            }
            count
        };

        app.world_mouse_input(ElementState::Pressed, MouseButton::Right);
        assert_eq!(
            drain_block_uses(&rx),
            1,
            "the press edge must place immediately"
        );
        app.right_mouse_held = true;
        for _ in 0..3 {
            app.tick_live_interactions(GAME_TICK_SECS, false);
        }
        assert_eq!(
            drain_block_uses(&rx),
            0,
            "rightClickDelay must not fire before tick 4"
        );
        app.tick_live_interactions(GAME_TICK_SECS, false);
        assert_eq!(
            drain_block_uses(&rx),
            1,
            "the held repeat must re-run startUseItem on tick 4"
        );
        // Releasing disarms the gate: no further repeats without a press.
        app.right_mouse_held = false;
        for _ in 0..8 {
            app.tick_live_interactions(GAME_TICK_SECS, false);
        }
        assert_eq!(drain_block_uses(&rx), 0, "released use must stop repeating");
    }
}
