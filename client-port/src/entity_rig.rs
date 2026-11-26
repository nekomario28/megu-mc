// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic articulated entity meshes for Minecraft Java 1.21.1 (protocol 767).
//! Geometry uses entity-local block units, with feet at y = 0 and the origin
//! centered horizontally. The 130 registered dimensions are collision sizes,
//! not promises of exact Vanilla renderer model geometry.

use std::f32::consts::{FRAC_PI_2, PI, TAU};

use crate::vanilla_plus_3d::{cuboid_face_uvs, ExtrudedMesh, ExtrudedQuad, MAX_UPGRADE_QUADS};
use crate::Direction;

/// Broad families of reusable procedural rigs. The first eight cover the
/// canonical Vanilla registry; [`EntityRigArchetype::GenericProxy`] is the
/// never-invisible fallback [`classify_entity_rig_named`] assigns to modded
/// entity names no archetype token matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityRigArchetype {
    Biped,
    Quadruped,
    Avian,
    Arthropod,
    SlimeCube,
    Aquatic,
    ProjectileOrItem,
    VehicleOrHanging,
    /// Compact name-hash-tinted proxy for fully novel modded entity names:
    /// deterministic, always drawable, and unreachable from the protocol-767
    /// `0..=129` classification path.
    GenericProxy,
}

/// Relative head, torso, and limb sizing for procedural archetype templates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RigProportions {
    pub head: f32,
    pub torso: f32,
    pub limbs: f32,
}

impl EntityRigArchetype {
    pub const fn proportions(self) -> RigProportions {
        match self {
            Self::Biped => RigProportions {
                head: 0.25,
                torso: 0.40,
                limbs: 0.35,
            },
            Self::Quadruped => RigProportions {
                head: 0.32,
                torso: 0.50,
                limbs: 0.42,
            },
            Self::Avian => RigProportions {
                head: 0.28,
                torso: 0.58,
                limbs: 0.08,
            },
            Self::Arthropod => RigProportions {
                head: 0.42,
                torso: 0.36,
                limbs: 0.13,
            },
            Self::SlimeCube => RigProportions {
                head: 0.0,
                torso: 1.0,
                limbs: 0.0,
            },
            Self::Aquatic => RigProportions {
                head: 0.46,
                torso: 0.66,
                limbs: 0.12,
            },
            Self::ProjectileOrItem | Self::VehicleOrHanging | Self::GenericProxy => {
                RigProportions {
                    head: 0.0,
                    torso: 1.0,
                    limbs: 0.0,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityRigSpec {
    pub entity_type_id: u16,
    pub name: &'static str,
    pub archetype: EntityRigArchetype,
    pub width: f32,
    pub height: f32,
    pub proportions: RigProportions,
    pub is_player: bool,
    pub translucent_shell: bool,
}

// Canonical 1.21.1 EntityType registry rows, protocol id = array index.
// Dimensions are sourced from src/entity_registry.rs; keep in sync on upgrades.
const ENTITIES: [(&str, f32, f32); 130] = [
    ("minecraft:allay", 0.35, 0.6),                   // 0
    ("minecraft:area_effect_cloud", 6.0, 0.5),        // 1
    ("minecraft:armadillo", 0.7, 0.65),               // 2
    ("minecraft:armor_stand", 0.5, 1.975),            // 3
    ("minecraft:arrow", 0.5, 0.5),                    // 4
    ("minecraft:axolotl", 0.75, 0.42),                // 5
    ("minecraft:bat", 0.5, 0.9),                      // 6
    ("minecraft:bee", 0.7, 0.6),                      // 7
    ("minecraft:blaze", 0.6, 1.8),                    // 8
    ("minecraft:block_display", 0.0, 0.0),            // 9
    ("minecraft:boat", 1.375, 0.5625),                // 10
    ("minecraft:bogged", 0.6, 1.99),                  // 11
    ("minecraft:breeze", 0.6, 1.77),                  // 12
    ("minecraft:breeze_wind_charge", 0.3125, 0.3125), // 13
    ("minecraft:camel", 1.7, 2.375),                  // 14
    ("minecraft:cat", 0.6, 0.7),                      // 15
    ("minecraft:cave_spider", 0.7, 0.5),              // 16
    ("minecraft:chest_boat", 1.375, 0.5625),          // 17
    ("minecraft:chest_minecart", 0.98, 0.7),          // 18
    ("minecraft:chicken", 0.4, 0.7),                  // 19
    ("minecraft:cod", 0.5, 0.3),                      // 20
    ("minecraft:command_block_minecart", 0.98, 0.7),  // 21
    ("minecraft:cow", 0.9, 1.4),                      // 22
    ("minecraft:creeper", 0.6, 1.7),                  // 23
    ("minecraft:dolphin", 0.9, 0.6),                  // 24
    ("minecraft:donkey", 1.3964844, 1.5),             // 25
    ("minecraft:dragon_fireball", 1.0, 1.0),          // 26
    ("minecraft:drowned", 0.6, 1.95),                 // 27
    ("minecraft:egg", 0.25, 0.25),                    // 28
    ("minecraft:elder_guardian", 1.9975, 1.9975),     // 29
    ("minecraft:end_crystal", 2.0, 2.0),              // 30
    ("minecraft:ender_dragon", 16.0, 8.0),            // 31
    ("minecraft:ender_pearl", 0.25, 0.25),            // 32
    ("minecraft:enderman", 0.6, 2.9),                 // 33
    ("minecraft:endermite", 0.4, 0.3),                // 34
    ("minecraft:evoker", 0.6, 1.95),                  // 35
    ("minecraft:evoker_fangs", 0.5, 0.8),             // 36
    ("minecraft:experience_bottle", 0.25, 0.25),      // 37
    ("minecraft:experience_orb", 0.5, 0.5),           // 38
    ("minecraft:eye_of_ender", 0.25, 0.25),           // 39
    ("minecraft:falling_block", 0.98, 0.98),          // 40
    ("minecraft:firework_rocket", 0.25, 0.25),        // 41
    ("minecraft:fox", 0.6, 0.7),                      // 42
    ("minecraft:frog", 0.5, 0.5),                     // 43
    ("minecraft:furnace_minecart", 0.98, 0.7),        // 44
    ("minecraft:ghast", 4.0, 4.0),                    // 45
    ("minecraft:giant", 3.6, 12.0),                   // 46
    ("minecraft:glow_item_frame", 0.5, 0.5),          // 47
    ("minecraft:glow_squid", 0.8, 0.8),               // 48
    ("minecraft:goat", 0.9, 1.3),                     // 49
    ("minecraft:guardian", 0.85, 0.85),               // 50
    ("minecraft:hoglin", 1.3964844, 1.4),             // 51
    ("minecraft:hopper_minecart", 0.98, 0.7),         // 52
    ("minecraft:horse", 1.3964844, 1.6),              // 53
    ("minecraft:husk", 0.6, 1.95),                    // 54
    ("minecraft:illusioner", 0.6, 1.95),              // 55
    ("minecraft:interaction", 0.0, 0.0),              // 56
    ("minecraft:iron_golem", 1.4, 2.7),               // 57
    ("minecraft:item", 0.25, 0.25),                   // 58
    ("minecraft:item_display", 0.0, 0.0),             // 59
    ("minecraft:item_frame", 0.5, 0.5),               // 60
    ("minecraft:ominous_item_spawner", 0.25, 0.25),   // 61
    ("minecraft:fireball", 1.0, 1.0),                 // 62
    ("minecraft:leash_knot", 0.375, 0.5),             // 63
    ("minecraft:lightning_bolt", 0.0, 0.0),           // 64
    ("minecraft:llama", 0.9, 1.87),                   // 65
    ("minecraft:llama_spit", 0.25, 0.25),             // 66
    ("minecraft:magma_cube", 0.52, 0.52),             // 67
    ("minecraft:marker", 0.0, 0.0),                   // 68
    ("minecraft:minecart", 0.98, 0.7),                // 69
    ("minecraft:mooshroom", 0.9, 1.4),                // 70
    ("minecraft:mule", 1.3964844, 1.6),               // 71
    ("minecraft:ocelot", 0.6, 0.7),                   // 72
    ("minecraft:painting", 0.5, 0.5),                 // 73
    ("minecraft:panda", 1.3, 1.25),                   // 74
    ("minecraft:parrot", 0.5, 0.9),                   // 75
    ("minecraft:phantom", 0.9, 0.5),                  // 76
    ("minecraft:pig", 0.9, 0.9),                      // 77
    ("minecraft:piglin", 0.6, 1.95),                  // 78
    ("minecraft:piglin_brute", 0.6, 1.95),            // 79
    ("minecraft:pillager", 0.6, 1.95),                // 80
    ("minecraft:polar_bear", 1.4, 1.4),               // 81
    ("minecraft:potion", 0.25, 0.25),                 // 82
    ("minecraft:pufferfish", 0.7, 0.7),               // 83
    ("minecraft:rabbit", 0.4, 0.5),                   // 84
    ("minecraft:ravager", 1.95, 2.2),                 // 85
    ("minecraft:salmon", 0.7, 0.4),                   // 86
    ("minecraft:sheep", 0.9, 1.3),                    // 87
    ("minecraft:shulker", 1.0, 1.0),                  // 88
    ("minecraft:shulker_bullet", 0.3125, 0.3125),     // 89
    ("minecraft:silverfish", 0.4, 0.3),               // 90
    ("minecraft:skeleton", 0.6, 1.99),                // 91
    ("minecraft:skeleton_horse", 1.3964844, 1.6),     // 92
    ("minecraft:slime", 0.52, 0.52),                  // 93
    ("minecraft:small_fireball", 0.3125, 0.3125),     // 94
    ("minecraft:sniffer", 1.9, 1.75),                 // 95
    ("minecraft:snow_golem", 0.7, 1.9),               // 96
    ("minecraft:snowball", 0.25, 0.25),               // 97
    ("minecraft:spawner_minecart", 0.98, 0.7),        // 98
    ("minecraft:spectral_arrow", 0.5, 0.5),           // 99
    ("minecraft:spider", 1.4, 0.9),                   // 100
    ("minecraft:squid", 0.8, 0.8),                    // 101
    ("minecraft:stray", 0.6, 1.99),                   // 102
    ("minecraft:strider", 0.9, 1.7),                  // 103
    ("minecraft:tadpole", 0.4, 0.3),                  // 104
    ("minecraft:text_display", 0.0, 0.0),             // 105
    ("minecraft:tnt", 0.98, 0.98),                    // 106
    ("minecraft:tnt_minecart", 0.98, 0.7),            // 107
    ("minecraft:trader_llama", 0.9, 1.87),            // 108
    ("minecraft:trident", 0.5, 0.5),                  // 109
    ("minecraft:tropical_fish", 0.5, 0.4),            // 110
    ("minecraft:turtle", 1.2, 0.4),                   // 111
    ("minecraft:vex", 0.4, 0.8),                      // 112
    ("minecraft:villager", 0.6, 1.95),                // 113
    ("minecraft:vindicator", 0.6, 1.95),              // 114
    ("minecraft:wandering_trader", 0.6, 1.95),        // 115
    ("minecraft:warden", 0.9, 2.9),                   // 116
    ("minecraft:wind_charge", 0.3125, 0.3125),        // 117
    ("minecraft:witch", 0.6, 1.95),                   // 118
    ("minecraft:wither", 0.9, 3.5),                   // 119
    ("minecraft:wither_skeleton", 0.7, 2.4),          // 120
    ("minecraft:wither_skull", 0.3125, 0.3125),       // 121
    ("minecraft:wolf", 0.6, 0.85),                    // 122
    ("minecraft:zoglin", 1.3964844, 1.4),             // 123
    ("minecraft:zombie", 0.6, 1.95),                  // 124
    ("minecraft:zombie_horse", 1.3964844, 1.6),       // 125
    ("minecraft:zombie_villager", 0.6, 1.95),         // 126
    ("minecraft:zombified_piglin", 0.6, 1.95),        // 127
    ("minecraft:player", 0.6, 1.8),                   // 128
    ("minecraft:fishing_bobber", 0.25, 0.25),         // 129
];

/// Exact protocol-767 ID classification; all unlisted effect/display IDs use
/// the drawable item/projectile proxy template.
pub fn classify_entity_rig(entity_type_id: u16) -> Option<EntityRigSpec> {
    use EntityRigArchetype::*;
    let &(name, width, height) = ENTITIES.get(usize::from(entity_type_id))?;
    let archetype = match entity_type_id {
        3 | 11 | 12 | 27 | 33 | 35 | 46 | 54 | 55 | 57 | 78 | 79 | 80 | 91 | 96 | 102 | 113
        | 114 | 115 | 116 | 118 | 119 | 120 | 124 | 126 | 127 | 128 => Biped,
        2 | 14 | 15 | 22 | 23 | 25 | 42 | 43 | 49 | 51 | 53 | 65 | 70 | 71 | 72 | 74 | 77 | 81
        | 84 | 85 | 87 | 92 | 95 | 103 | 108 | 122 | 123 | 125 => Quadruped,
        0 | 6 | 7 | 8 | 19 | 31 | 45 | 75 | 76 | 112 => Avian,
        16 | 34 | 90 | 100 => Arthropod,
        67 | 88 | 93 => SlimeCube,
        5 | 20 | 24 | 29 | 48 | 50 | 83 | 86 | 101 | 104 | 110 | 111 => Aquatic,
        10 | 17 | 18 | 21 | 30 | 44 | 47 | 52 | 60 | 63 | 69 | 73 | 98 | 107 => VehicleOrHanging,
        _ => ProjectileOrItem,
    };
    Some(EntityRigSpec {
        entity_type_id,
        name,
        archetype,
        width,
        height,
        proportions: archetype.proportions(),
        is_player: entity_type_id == 128,
        translucent_shell: entity_type_id == 93,
    })
}

/// Synthetic entity-type id assigned to every rig spec resolved from a modded
/// registry name. Outside the protocol-767 `0..=129` range so the canonical
/// vanilla table stays byte-identical and a modded spec can never alias a
/// vanilla template.
pub const MODDED_ENTITY_TYPE_ID: u16 = u16::MAX;

/// One [`classify_entity_rig_named`] archetype family. `tokens` are
/// `*token*` substring tests and `suffixes` are `*suffix` tail tests, both
/// applied to the entity *path* — never the namespace — so `iceandfire:`,
/// `twilightforest:` and any other mod id resolve identically.
struct ModdedFamily {
    tokens: &'static [&'static str],
    suffixes: &'static [&'static str],
    archetype: EntityRigArchetype,
    default_width: f32,
    default_height: f32,
}

/// Ordered modded-name families for [`classify_entity_rig_named`]. The first
/// matching family wins, so specific creatures (dragons, serpents) outrank
/// the generic biped/quadruped buckets further down.
const MODDED_FAMILIES: &[ModdedFamily] = &[
    ModdedFamily {
        // Large winged quadruped: dragons, wyverns, drakes, griffins.
        tokens: &[
            "dragon",
            "wyvern",
            "drake",
            "griffin",
            "hippogriff",
            "manticore",
        ],
        suffixes: &[],
        archetype: EntityRigArchetype::Avian,
        default_width: 2.4,
        default_height: 2.2,
    },
    ModdedFamily {
        // Elongated aquatic serpent: sea serpents, leviathans, hydras.
        tokens: &["serpent", "leviathan", "eel", "worm", "hydra"],
        suffixes: &[],
        archetype: EntityRigArchetype::Aquatic,
        default_width: 1.4,
        default_height: 1.0,
    },
    ModdedFamily {
        tokens: &["shark", "whale", "squid", "dolphin", "crab", "turtle"],
        suffixes: &["_fish"],
        archetype: EntityRigArchetype::Aquatic,
        default_width: 0.8,
        default_height: 0.6,
    },
    ModdedFamily {
        tokens: &[
            "bird", "owl", "eagle", "crow", "phoenix", "bat", "fairy", "pixie", "bee",
        ],
        suffixes: &[],
        archetype: EntityRigArchetype::Avian,
        default_width: 0.7,
        default_height: 0.9,
    },
    ModdedFamily {
        tokens: &[
            "spider", "insect", "ant", "beetle", "scorpion", "mantis", "wasp",
        ],
        suffixes: &[],
        archetype: EntityRigArchetype::Arthropod,
        default_width: 0.9,
        default_height: 0.5,
    },
    ModdedFamily {
        tokens: &["slime", "cube", "jelly", "ooze"],
        suffixes: &[],
        archetype: EntityRigArchetype::SlimeCube,
        default_width: 0.8,
        default_height: 0.8,
    },
    ModdedFamily {
        tokens: &[
            "golem", "knight", "wizard", "mage", "goblin", "orc", "elf", "bandit", "guard",
            "villager", "zombie", "skeleton", "witch", "lich", "boss", "human", "npc",
        ],
        suffixes: &[],
        archetype: EntityRigArchetype::Biped,
        default_width: 0.7,
        default_height: 1.9,
    },
    ModdedFamily {
        tokens: &[
            "wolf", "fox", "bear", "deer", "boar", "lion", "tiger", "horse", "mount", "hound",
            "beast",
        ],
        suffixes: &[],
        archetype: EntityRigArchetype::Quadruped,
        default_width: 1.0,
        default_height: 1.2,
    },
    ModdedFamily {
        tokens: &[
            "arrow",
            "bolt",
            "fireball",
            "orb",
            "spell",
            "projectile",
            "snowball",
            "bullet",
        ],
        suffixes: &[],
        archetype: EntityRigArchetype::ProjectileOrItem,
        default_width: 0.4,
        default_height: 0.4,
    },
    ModdedFamily {
        tokens: &["boat", "cart", "ship", "airship", "vehicle", "seat"],
        suffixes: &[],
        archetype: EntityRigArchetype::VehicleOrHanging,
        default_width: 1.3,
        default_height: 0.6,
    },
];

/// Bounded interner for modded registry names so [`EntityRigSpec::name`]
/// keeps its `&'static str` contract without leaking per call. A poisoned
/// lock is recovered, not propagated: the interner still serves correct
/// names afterwards, it just may re-leak one entry.
fn intern_entity_name(name: &str) -> &'static str {
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    let names = NAMES.get_or_init(|| Mutex::new(Vec::new()));
    let mut guard = names
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&interned) = guard.iter().find(|&&entry| entry == name) {
        return interned;
    }
    let interned: &'static str = Box::leak(name.to_owned().into_boxed_str());
    guard.push(interned);
    interned
}

/// Build an [`EntityRigSpec`] for a modded registry name: synthetic
/// [`MODDED_ENTITY_TYPE_ID`], interned name, family archetype, and the same
/// translucent-shell flag vanilla slime gets for [`EntityRigArchetype::SlimeCube`].
fn modded_spec(
    name: &str,
    archetype: EntityRigArchetype,
    width: f32,
    height: f32,
) -> EntityRigSpec {
    EntityRigSpec {
        entity_type_id: MODDED_ENTITY_TYPE_ID,
        name: intern_entity_name(name),
        archetype,
        width,
        height,
        proportions: archetype.proportions(),
        is_player: false,
        translucent_shell: matches!(archetype, EntityRigArchetype::SlimeCube),
    }
}

/// Infallible rig classification for any registry name — vanilla or modded.
///
/// A name whose path matches a canonical Vanilla row (either `minecraft:path`
/// or bare `path`) resolves to that exact [`classify_entity_rig`] spec, so
/// all 130 vanilla rigs stay byte-identical. Every other name runs the
/// namespace-agnostic [`MODDED_FAMILIES`] token table, and a fully novel
/// name falls through to a [`EntityRigArchetype::GenericProxy`] spec — this
/// function never returns `None`, so no modded entity is ever invisible.
/// `width_hint`/`height_hint` override each family's default collision-size
/// estimate when they are finite and positive.
pub fn classify_entity_rig_named(
    name: &str,
    width_hint: Option<f32>,
    height_hint: Option<f32>,
) -> EntityRigSpec {
    let path = name.rsplit(':').next().unwrap_or(name);
    if let Some(spec) = ENTITIES
        .iter()
        .position(|&(registered, _, _)| registered.rsplit(':').next() == Some(path))
        .and_then(|index| classify_entity_rig(index as u16))
    {
        return spec;
    }
    let hint_or = |hint: Option<f32>, default: f32| match hint {
        Some(value) if value.is_finite() && value > 0.0 => value,
        _ => default,
    };
    for family in MODDED_FAMILIES {
        let matched = family.tokens.iter().any(|token| path.contains(token))
            || family.suffixes.iter().any(|suffix| path.ends_with(suffix));
        if matched {
            return modded_spec(
                name,
                family.archetype,
                hint_or(width_hint, family.default_width),
                hint_or(height_hint, family.default_height),
            );
        }
    }
    modded_spec(
        name,
        EntityRigArchetype::GenericProxy,
        hint_or(width_hint, 0.8),
        hint_or(height_hint, 1.2),
    )
}

/// Total rig resolution for callers holding a wire entity-type id plus an
/// optional registry name: the canonical vanilla spec first,
/// [`classify_entity_rig_named`] second, and a visible
/// [`EntityRigArchetype::GenericProxy`] spec third — modded entities can
/// never go invisible.
pub fn classify_entity_rig_or_fallback(
    entity_type_id: u32,
    registry_name: Option<&str>,
) -> EntityRigSpec {
    if let Ok(id) = u16::try_from(entity_type_id) {
        if let Some(spec) = classify_entity_rig(id) {
            return spec;
        }
    }
    if let Some(name) = registry_name.filter(|n| !n.is_empty()) {
        return classify_entity_rig_named(name, None, None);
    }
    modded_spec(
        "modded:unresolved",
        EntityRigArchetype::GenericProxy,
        0.8,
        1.2,
    )
}

/// Deterministic `(primary, accent)` packed-RGB tint pair for
/// [`EntityRigArchetype::GenericProxy`] rigs: FNV-1a of the registry name
/// spread across two byte mixes, biased bright so every unresolved modded
/// entity stays visible instead of collapsing to black.
fn modded_proxy_tints(name: &str) -> (i32, i32) {
    let hash = megu_core::perf_kit::fnv1a64(name.as_bytes());
    let channel = |shift: u32| 64 + (((hash >> shift) & 0xFF) as u8) % 160;
    (
        colossal_tint([channel(0), channel(8), channel(16)]),
        colossal_tint([channel(24), channel(32), channel(40)]),
    )
}

/// Animation inputs. Angles are degrees except limb_swing, which is radians.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EntityPose {
    pub limb_swing: f32,
    pub limb_swing_amount: f32,
    pub head_yaw_deg: f32,
    pub head_pitch_deg: f32,
    pub swing_progress: f32,
    pub sneaking: bool,
    pub swimming: bool,
    pub elytra_flying: bool,
    pub hurt_flash: bool,
    pub age_ticks: f32,
    pub baby: bool,
    pub enable_skin_3d_layers: bool,
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

impl EntityPose {
    fn sanitized(self) -> Self {
        Self {
            limb_swing: finite_or_zero(self.limb_swing).rem_euclid(TAU),
            limb_swing_amount: finite_or_zero(self.limb_swing_amount).clamp(0.0, 1.0),
            head_yaw_deg: finite_or_zero(self.head_yaw_deg).clamp(-180.0, 180.0),
            head_pitch_deg: finite_or_zero(self.head_pitch_deg).clamp(-90.0, 90.0),
            swing_progress: finite_or_zero(self.swing_progress).clamp(0.0, 1.0),
            age_ticks: finite_or_zero(self.age_ticks).rem_euclid(10_000.0),
            ..self
        }
    }
}

/// Extended optional pose state for the detailed vanilla rigs.
///
/// [`EntityPose`] intentionally remains the small stable payload consumed by
/// existing renderers and cache keys. `EntityRigPose` layers less common
/// entity-state bits on top without changing any existing entrypoint: callers
/// that only know [`EntityPose`] get the same animation as before, while
/// callers that track Vanilla metadata can opt into sitting/sleeping,
/// spellcasting, armor/held-item attachments, and entity-specific shell or
/// jaw states through [`build_entity_rig_mesh_with_pose`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityRigPose {
    /// Stable locomotion/head/hurt/baby state used by the original API.
    pub base: EntityPose,
    /// Legs fold forward and the torso settles downward (cats, wolves,
    /// villagers in seats, and similar poses).
    pub sitting: bool,
    /// Body pitches into a recumbent pose and head motion is damped.
    pub sleeping: bool,
    /// Illager-style arms unfold for a spell/casting animation.
    pub spellcasting: bool,
    /// The entity is actively using an item (bow, crossbow, food, potion).
    pub using_item: bool,
    /// Emit simple armor-layer shells around supported humanoid bodies.
    pub armor_visible: bool,
    /// Attach a generic main-hand item using the transform below.
    pub main_hand_item: bool,
    /// Attach a generic off-hand item using the transform below.
    pub offhand_item: bool,
    /// Attach a carried block (notably endermen) at the hands.
    pub carried_block: bool,
    /// Wither armor shell toggle used below half health in Vanilla.
    pub wither_armored: bool,
    /// Armadillo rolled-ball pose toggle.
    pub rolled_up: bool,
    /// Enderman screaming/jaw-drop pose toggle.
    pub screaming: bool,
    /// Normalized opening amount for shulker lids, guardian spikes, jaws,
    /// and similar shell/mouth mechanisms (`0..=1`).
    pub open_amount: f32,
    /// Normalized swelling/inflation amount for creepers and pufferfish
    /// (`0..=1`).
    pub swell_amount: f32,
    /// Local translation applied to the generated main-hand item prism.
    pub main_hand_offset: [f32; 3],
    /// Local XYZ rotation in degrees applied to the main-hand item prism.
    pub main_hand_rotation_deg: [f32; 3],
    /// Local translation applied to the generated off-hand item prism.
    pub offhand_offset: [f32; 3],
    /// Local XYZ rotation in degrees applied to the off-hand item prism.
    pub offhand_rotation_deg: [f32; 3],
}

impl Default for EntityRigPose {
    fn default() -> Self {
        Self::from(EntityPose::default())
    }
}

impl From<EntityPose> for EntityRigPose {
    fn from(base: EntityPose) -> Self {
        Self {
            base,
            sitting: false,
            sleeping: false,
            spellcasting: false,
            using_item: false,
            armor_visible: false,
            main_hand_item: false,
            offhand_item: false,
            carried_block: false,
            wither_armored: false,
            rolled_up: false,
            screaming: false,
            open_amount: 0.0,
            swell_amount: 0.0,
            main_hand_offset: [0.0; 3],
            main_hand_rotation_deg: [0.0; 3],
            offhand_offset: [0.0; 3],
            offhand_rotation_deg: [0.0; 3],
        }
    }
}

impl EntityRigPose {
    fn sanitized(self) -> Self {
        let sanitize_vec = |values: [f32; 3]| values.map(finite_or_zero);
        Self {
            base: self.base.sanitized(),
            open_amount: finite_or_zero(self.open_amount).clamp(0.0, 1.0),
            swell_amount: finite_or_zero(self.swell_amount).clamp(0.0, 1.0),
            main_hand_offset: sanitize_vec(self.main_hand_offset),
            main_hand_rotation_deg: sanitize_vec(self.main_hand_rotation_deg),
            offhand_offset: sanitize_vec(self.offhand_offset),
            offhand_rotation_deg: sanitize_vec(self.offhand_rotation_deg),
            ..self
        }
    }
}

/// Body-part binding for resolved CEM geometry and procedural articulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityRigPart {
    Head,
    Torso,
    LeftArm,
    RightArm,
    LeftLeg,
    RightLeg,
    LeftWing,
    RightWing,
    Tail,
    Core,
    Shell,
    Other,
}

/// One parsed EMF-CEM-1 / .jem / .jpm box override.
///
/// Coordinates and pivots are in entity-local block units (Y up), sizes are
/// positive, and UV is a normalized rectangle [u0, v0, u1, v1]. The caller
/// resolves CEM model-part hierarchy, texture layout and coordinate axes.
/// The part label selects the corresponding procedural pose animation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CemBox {
    pub part: EntityRigPart,
    pub origin: [f32; 3],
    pub size: [f32; 3],
    pub pivot: [f32; 3],
    pub rotation_deg: [f32; 3],
    pub uv: [f32; 4],
    pub inflate: f32,
}

impl CemBox {
    /// Convert normalized Y-up entity-space pixel positions and pixel UVs
    /// into a box override. One block equals 16 model pixels.
    pub fn from_model_pixels(
        part: EntityRigPart,
        origin: [f32; 3],
        size: [f32; 3],
        pivot: [f32; 3],
        rotation_deg: [f32; 3],
        uv_pixels: [f32; 4],
        texture_size: [f32; 2],
    ) -> Option<Self> {
        if texture_size.iter().any(|v| !v.is_finite() || *v <= 0.0) {
            return None;
        }
        let candidate = Self {
            part,
            origin: origin.map(|v| v / 16.0),
            size: size.map(|v| v / 16.0),
            pivot: pivot.map(|v| v / 16.0),
            rotation_deg,
            uv: [
                uv_pixels[0] / texture_size[0],
                uv_pixels[1] / texture_size[1],
                uv_pixels[2] / texture_size[0],
                uv_pixels[3] / texture_size[1],
            ],
            inflate: 0.0,
        };
        candidate.is_valid().then_some(candidate)
    }

    fn is_valid(&self) -> bool {
        self.origin
            .iter()
            .chain(self.pivot.iter())
            .chain(self.size.iter())
            .chain(self.rotation_deg.iter())
            .chain(self.uv.iter())
            .chain(std::iter::once(&self.inflate))
            .all(|v| v.is_finite() && v.abs() <= 1024.0)
            && self.size.iter().all(|v| *v > 0.0)
            && self.size.iter().all(|v| *v + self.inflate * 2.0 > 0.0)
            && self.uv.iter().all(|v| (0.0..=1.0).contains(v))
            && self.uv[0] <= self.uv[2]
            && self.uv[1] <= self.uv[3]
    }
}

/// Sentinel [`RigBox::tint`] meaning "default entity tint": `-1`, or `1`
/// while `hurt_flash` is set. Any other value is emitted verbatim as the
/// quad `tint_index`, letting procedural details (pumpkin head, held mouth
/// item, projectile accents) carry a distinct palette slot.
const RIG_TINT_DEFAULT: i32 = i32::MIN;

/// Packed mouth-item tint: bit 30 marks the slot, the low 24 bits carry
/// the held item's RGB color. Positive and far above the small palette
/// indices used by rails (`0`/`1`) and hurt flash (`1`).
const MOUTH_ITEM_TINT_FLAG: i32 = 1 << 30;

fn mouth_item_tint(rgb: [u8; 3]) -> i32 {
    MOUTH_ITEM_TINT_FLAG | (i32::from(rgb[0]) << 16) | (i32::from(rgb[1]) << 8) | i32::from(rgb[2])
}

#[derive(Clone, Copy)]
struct RigBox {
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    uv: Option<[f32; 4]>,
    inflate: f32,
    tint: i32,
}

impl From<&CemBox> for RigBox {
    fn from(value: &CemBox) -> Self {
        Self {
            part: value.part,
            center: [
                value.origin[0] + value.size[0] * 0.5,
                value.origin[1] + value.size[1] * 0.5,
                value.origin[2] + value.size[2] * 0.5,
            ],
            size: value.size,
            pivot: value.pivot,
            rotation: value.rotation_deg.map(f32::to_radians),
            uv: Some(value.uv),
            inflate: value.inflate,
            tint: RIG_TINT_DEFAULT,
        }
    }
}

fn articulation(part: EntityRigPart, pose: EntityRigPose) -> [f32; 3] {
    use EntityRigPart::*;
    let base = pose.base;
    let stride = base.limb_swing.cos() * base.limb_swing_amount * 0.8;
    let attack = (base.swing_progress * PI).sin() * 1.2;
    let flight = if base.elytra_flying {
        -1.15
    } else if base.swimming {
        -0.75
    } else {
        0.0
    };
    let crouch = if base.sneaking { 0.32 } else { 0.0 };
    let flap = (base.age_ticks * 0.7 + base.limb_swing).sin() * 0.5;
    let tail = (base.age_ticks * 0.35 + base.limb_swing).sin() * 0.35;
    let sitting = if pose.sitting { 1.0 } else { 0.0 };
    let sleeping = if pose.sleeping { 1.0 } else { 0.0 };
    let head_scale = if pose.sleeping { 0.35 } else { 1.0 };
    match part {
        Head => [
            base.head_pitch_deg.to_radians() * head_scale + sleeping * 0.35,
            base.head_yaw_deg.to_radians() * head_scale,
            0.0,
        ],
        Torso => [
            crouch
                + sleeping * 1.25
                + sitting * 0.12
                + if base.swimming || base.elytra_flying {
                    0.25
                } else {
                    0.0
                },
            0.0,
            0.0,
        ],
        LeftArm => [-stride + flight + sleeping * 0.7, 0.0, sitting * 0.18],
        RightArm => [
            stride - attack + flight + sleeping * 0.7,
            0.0,
            -sitting * 0.18,
        ],
        LeftLeg => [
            stride * 0.85 + crouch * 0.4 - sitting * 1.05 + sleeping * 0.9,
            0.0,
            0.0,
        ],
        RightLeg => [
            -stride * 0.85 + crouch * 0.4 - sitting * 1.05 + sleeping * 0.9,
            0.0,
            0.0,
        ],
        LeftWing => [0.0, 0.0, -flap - 0.12],
        RightWing => [0.0, 0.0, flap + 0.12],
        Tail => [0.0, tail, 0.0],
        Core | Shell | Other => [0.0; 3],
    }
}

/// Right-handed rotations around X then Y then Z.
fn rotate(point: [f32; 3], angles: [f32; 3]) -> [f32; 3] {
    let (sx, cx) = angles[0].sin_cos();
    let (sy, cy) = angles[1].sin_cos();
    let (sz, cz) = angles[2].sin_cos();
    let x = point[0];
    let y = point[1] * cx - point[2] * sx;
    let z = point[1] * sx + point[2] * cx;
    let x2 = x * cy + z * sy;
    let z2 = -x * sy + z * cy;
    [x2 * cz - y * sz, x2 * sz + y * cz, z2]
}

fn baby_scale(part: EntityRigPart, point: [f32; 3], height: f32, baby: bool) -> [f32; 3] {
    if !baby {
        return point;
    }
    if part == EntityRigPart::Head {
        // Vanilla babies use half-scale bodies with effectively full-size
        // heads (the familiar 2:1 head-to-body ratio). Keep the head geometry
        // unscaled, but lower it so its neck joint lands on the half-scale body.
        let joint_y = height * 0.70;
        [point[0], joint_y * 0.5 + (point[1] - joint_y), point[2]]
    } else {
        [point[0] * 0.5, point[1] * 0.5, point[2] * 0.5]
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn outward_normal(positions: &[[f32; 3]; 4]) -> Option<[f32; 3]> {
    let a = positions[0];
    let b = positions[1];
    let c = positions[2];
    let normal = cross(
        [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
        [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
    );
    let length = normal.iter().map(|v| v * v).sum::<f32>().sqrt();
    (length.is_finite() && length > 1e-10).then(|| normal.map(|v| v / length))
}

fn emit_box(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose, shape: RigBox) {
    if mesh.quads.len() + 6 > MAX_UPGRADE_QUADS {
        return;
    }
    let half = shape.size.map(|v| v * 0.5 + shape.inflate);
    if half.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return;
    }
    let lo = [
        shape.center[0] - half[0],
        shape.center[1] - half[1],
        shape.center[2] - half[2],
    ];
    let hi = [
        shape.center[0] + half[0],
        shape.center[1] + half[1],
        shape.center[2] + half[2],
    ];
    let vertices = [
        [lo[0], lo[1], lo[2]],
        [hi[0], lo[1], lo[2]],
        [lo[0], hi[1], lo[2]],
        [hi[0], hi[1], lo[2]],
        [lo[0], lo[1], hi[2]],
        [hi[0], lo[1], hi[2]],
        [lo[0], hi[1], hi[2]],
        [hi[0], hi[1], hi[2]],
    ];
    // Face vertex order is outward CCW for each side.
    const FACES: [[usize; 4]; 6] = [
        [0, 4, 6, 2],
        [5, 1, 3, 7],
        [4, 0, 1, 5],
        [2, 6, 7, 3],
        [1, 0, 2, 3],
        [4, 5, 7, 6],
    ];
    let face_uvs = if let Some([u0, v0, u1, v1]) = shape.uv {
        [[[u0, v1], [u1, v1], [u1, v0], [u0, v0]]; 6]
    } else {
        cuboid_face_uvs(half)
    };
    const DIRECTIONS: [Direction; 6] = [
        Direction::West,
        Direction::East,
        Direction::Down,
        Direction::Up,
        Direction::North,
        Direction::South,
    ];
    let animation = articulation(shape.part, pose);
    for (indices, direction) in FACES.into_iter().zip(DIRECTIONS) {
        let mut positions = [[0.0; 3]; 4];
        for (dest, index) in positions.iter_mut().zip(indices) {
            let original = vertices[index];
            let relative = [
                original[0] - shape.pivot[0],
                original[1] - shape.pivot[1],
                original[2] - shape.pivot[2],
            ];
            let model_rotated = rotate(relative, shape.rotation);
            let animated = rotate(model_rotated, animation);
            let p = [
                shape.pivot[0] + animated[0],
                shape.pivot[1] + animated[1],
                shape.pivot[2] + animated[2],
            ];
            *dest = baby_scale(shape.part, p, spec.height, pose.base.baby);
        }
        if let Some(normal) = outward_normal(&positions) {
            if positions.iter().flatten().all(|v| v.is_finite()) {
                mesh.quads.push(ExtrudedQuad {
                    positions,
                    normal,
                    uv: face_uvs[direction as usize],
                    tint_index: if pose.base.hurt_flash {
                        1
                    } else if shape.tint != RIG_TINT_DEFAULT {
                        shape.tint
                    } else {
                        -1
                    },
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // Flat box coordinates keep template declarations readable.
fn add(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    inflate: f32,
) {
    emit_box(
        mesh,
        spec,
        pose,
        RigBox {
            part,
            center,
            size,
            pivot,
            rotation,
            uv: None,
            inflate,
            tint: RIG_TINT_DEFAULT,
        },
    );
}

/// [`add`] with an explicit `tint_index` palette slot for the emitted quads.
#[allow(clippy::too_many_arguments)] // Mirrors `add`; see its allow note.
fn add_tinted(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    inflate: f32,
    tint: i32,
) {
    emit_box(
        mesh,
        spec,
        pose,
        RigBox {
            part,
            center,
            size,
            pivot,
            rotation,
            uv: None,
            inflate,
            tint,
        },
    );
}

/// Fox (id 42): a slim quadruped with a distinct snout, two pointed ears,
/// four legs, and a bushy two-segment tail. Forward is `-Z`. When
/// `mouth_item_rgb` is `Some`, a small tilted prism is attached at the
/// snout tip, tinted by the packed RGB value.
fn add_fox(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    mouth_item_rgb: Option<[u8; 3]>,
) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let head_pivot = [0.0, h * 0.74, -w * 0.26];

    add(mesh, spec, pose, Torso, [0.0, h * 0.58, w * 0.06], [w * 0.56, h * 0.50, w * 0.82],
        [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.80, -w * 0.36], [w * 0.48, h * 0.32, w * 0.40],
        head_pivot, [0.0; 3], 0.0);
    // Snout: a narrower muzzle box protruding forward from the head.
    add(mesh, spec, pose, Head, [0.0, h * 0.74, -w * 0.60], [w * 0.26, h * 0.18, w * 0.34],
        head_pivot, [0.0; 3], 0.0);
    // Two pointed ears: a tilted base slab plus a smaller, more tilted tip.
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Head, [side * w * 0.16, h * 0.95, -w * 0.36],
            [w * 0.10, h * 0.16, w * 0.08], head_pivot, [0.0, 0.0, -side * 0.45], 0.0);
        add(mesh, spec, pose, Head, [side * w * 0.23, h * 1.00, -w * 0.36],
            [w * 0.07, h * 0.12, w * 0.06], head_pivot, [0.0, 0.0, -side * 0.95], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for z in [-0.28_f32, 0.30] {
            let pivot = [side * w * 0.23, h * 0.40, z * w];
            add(mesh, spec, pose, part, [pivot[0], h * 0.21, pivot[2]],
                [w * 0.15, h * 0.42, w * 0.17], pivot, [0.0; 3], 0.0);
        }
    }
    // Bushy tail: a thick angled base segment and a smaller raised tip,
    // both bound to `Tail` so it sways with the pose.
    let tail_pivot = [0.0, h * 0.60, w * 0.42];
    add(mesh, spec, pose, Tail, [0.0, h * 0.64, w * 0.58], [w * 0.20, h * 0.18, w * 0.36],
        tail_pivot, [-0.55, 0.0, 0.0], 0.0);
    add(mesh, spec, pose, Tail, [0.0, h * 0.76, w * 0.72], [w * 0.14, h * 0.14, w * 0.24],
        tail_pivot, [-0.95, 0.0, 0.0], 0.0);
    if let Some(rgb) = mouth_item_rgb {
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.70, -w * 0.80],
            [w * 0.14, h * 0.09, w * 0.26], head_pivot, [0.5, 0.0, 0.0], 0.0, mouth_item_tint(rgb));
    }
}

/// Snow golem (id 96): stacked lower/upper snow cubes, a pumpkin head,
/// and two angled stick arms.
fn add_snow_golem(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;

    add(mesh, spec, pose, Torso, [0.0, h * 0.27, 0.0], [w * 0.92, h * 0.54, w * 0.92],
        [0.0, h * 0.55, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Torso, [0.0, h * 0.60, 0.0], [w * 0.78, h * 0.34, w * 0.78],
        [0.0, h * 0.55, 0.0], [0.0; 3], 0.0);
    // Pumpkin head: same box geometry, distinct accent tint slot.
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.83, 0.0], [w * 0.56, h * 0.30, w * 0.56],
        [0.0, h * 0.68, 0.0], [0.0; 3], 0.0, 7);
    // Long thin arms angled outward from the shoulders.
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.40, h * 0.70, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.50, h * 0.44, 0.0],
            [w * 0.11, h * 0.52, w * 0.11], pivot, [0.0, 0.0, side * 0.35], 0.0);
    }
}

/// Thrown spherical projectile proxy: an outer shell cube sized to the
/// collision box, a smaller inner cube yawed 45 degrees for a faceted
/// silhouette, and a compact core. `tint` selects a small procedural
/// accent palette slot per projectile family.
fn add_thrown_projectile(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    entity_type_id: u16,
) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let tint = match entity_type_id {
        97 => 2,       // snowball: crisp ice white
        28 => 3,       // egg
        32 => 4,       // ender_pearl
        117 | 13 => 5, // wind_charge / breeze_wind_charge
        _ => 6,        // fireball / small_fireball: flame accent
    };
    let center = [0.0, h * 0.5, 0.0];
    add_tinted(mesh, spec, pose, Core, center, [w * 0.94, h * 0.94, w * 0.94], [0.0; 3], [0.0; 3],
        0.0, tint);
    add_tinted(mesh, spec, pose, Shell, center, [w * 0.70, h * 0.70, w * 0.70], [0.0; 3],
        [0.0, PI * 0.25, 0.0], 0.0, tint + 1);
    add_tinted(mesh, spec, pose, Core, center, [w * 0.38, h * 0.38, w * 0.38], [0.0; 3], [0.0; 3],
        0.0, tint);
}

// ---------------------------------------------------------------------------
// Dedicated vanilla rigs
// ---------------------------------------------------------------------------

/// Emit a box authored in a normalized unit template, scaling positions and
/// dimensions but leaving rotations in radians. This keeps large rigs such as
/// the ender dragon readable without repeating scale factors at every vertex.
#[allow(clippy::too_many_arguments)]
fn add_unit_tinted(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    scale: f32,
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    tint: i32,
) {
    add_tinted(mesh, spec, pose, part, center.map(|v| v * scale), size.map(|v| v * scale),
        pivot.map(|v| v * scale), rotation, 0.0, tint);
}

#[allow(clippy::too_many_arguments)]
fn add_unit(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    scale: f32,
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
) {
    add_unit_tinted(mesh, spec, pose, scale, part, center, size, pivot, rotation, RIG_TINT_DEFAULT);
}

/// Ender dragon accent palette slot shared by the sub-builders.
const DRAGON_ACCENT: i32 = 8;

/// Pose-derived motion values shared by the dragon body-part builders.
#[derive(Clone, Copy)]
struct DragonMotion {
    scale: f32,
    flap: f32,
    tail_drive: f32,
    yaw: f32,
    pitch: f32,
}

/// Torso, chest keel, dorsal spine ridge, and the five-segment neck rising
/// toward the head.
fn dragon_torso_neck(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    m: DragonMotion,
) {
    use EntityRigPart::*;
    let s = m.scale;
    let accent = DRAGON_ACCENT;
    add_unit(mesh, spec, pose, s, Torso, [0.0, 3.25, 0.2], [2.3, 1.35, 4.3], [0.0, 3.3, 0.0],
        [0.0; 3]);
    add_unit_tinted(mesh, spec, pose, s, Torso, [0.0, 2.58, 0.15], [1.55, 0.20, 3.8],
        [0.0, 3.3, 0.0], [0.0; 3], accent);
    for z in [-1.45, -0.55, 0.35, 1.25] {
        add_unit_tinted(mesh, spec, pose, s, Torso, [0.0, 4.08, z], [0.18, 0.55, 0.24],
            [0.0, 3.9, z], [-0.16, 0.0, 0.0], accent);
    }
    let neck = [
        ([0.0, 3.75, -1.75], [0.58, 0.62, 0.92], -0.46),
        ([0.0, 4.08, -2.28], [0.55, 0.58, 0.86], -0.50),
        ([0.0, 4.38, -2.78], [0.51, 0.54, 0.80], -0.54),
        ([0.0, 4.66, -3.25], [0.47, 0.50, 0.74], -0.58),
        ([0.0, 4.90, -3.70], [0.43, 0.46, 0.68], -0.62),
    ];
    for (center, size, neck_pitch) in neck {
        add_unit(mesh, spec, pose, s, Other, center, size, center,
            [neck_pitch + m.pitch * 0.15, m.yaw * 0.12, 0.0]);
    }
}

/// Skull, long snout, articulated jaw, teeth, eyes, and swept horns.
fn dragon_head(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    m: DragonMotion,
) {
    use EntityRigPart::*;
    let s = m.scale;
    let accent = DRAGON_ACCENT;
    let base = pose.base;
    let head_pivot = [0.0, 5.02, -4.12];
    let head_rot = [m.pitch - 0.08, m.yaw, 0.0];
    add_unit(mesh, spec, pose, s, Head, [0.0, 5.12, -4.45], [0.72, 0.62, 0.98], head_pivot,
        head_rot);
    add_unit(mesh, spec, pose, s, Head, [0.0, 5.02, -5.12], [0.52, 0.38, 0.72], head_pivot,
        head_rot);
    let jaw_open = 0.18 + pose.open_amount * 0.72 + (base.swing_progress * PI).sin() * 0.25;
    add_unit_tinted(mesh, spec, pose, s, Head, [0.0, 4.76, -4.98], [0.56, 0.16, 0.78],
        [0.0, 4.92, -4.35], [m.pitch + jaw_open, m.yaw, 0.0], accent);
    for side in [-1.0_f32, 1.0] {
        add_unit_tinted(mesh, spec, pose, s, Head, [side * 0.25, 5.18, -5.45], [0.08, 0.12, 0.24],
            head_pivot, head_rot, accent);
        add_unit_tinted(mesh, spec, pose, s, Head, [side * 0.28, 5.52, -4.08], [0.14, 0.18, 0.72],
            head_pivot, [-0.55, side * 0.38, side * 0.18], accent);
    }
}

/// Four legs, each with thigh, shin, and clawed foot. The dragon is often
/// airborne, but the folded legs remain part of the Vanilla silhouette.
fn dragon_legs(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    m: DragonMotion,
) {
    use EntityRigPart::*;
    let s = m.scale;
    let accent = DRAGON_ACCENT;
    let base = pose.base;
    for (side, z, phase) in [
        (-1.0_f32, -1.15_f32, 0.0_f32),
        (1.0, -1.15, PI),
        (-1.0, 1.35, PI),
        (1.0, 1.35, 0.0),
    ] {
        let swing = (base.limb_swing + phase).cos() * base.limb_swing_amount * 0.28;
        let hip = [side * 0.86, 2.88, z];
        let leg = if side < 0.0 { LeftLeg } else { RightLeg };
        add_unit(mesh, spec, pose, s, leg,
            [side * 0.92, 2.38, z + 0.04], [0.44, 0.88, 0.52], hip, [swing + 0.18, 0.0, 0.0]);
        add_unit(mesh, spec, pose, s, leg,
            [side * 0.98, 1.78, z - 0.03], [0.34, 0.66, 0.38], hip, [swing - 0.12, 0.0, 0.0]);
        add_unit_tinted(mesh, spec, pose, s, leg,
            [side * 0.99, 1.40, z - 0.18], [0.43, 0.18, 0.62], hip, [swing, 0.0, 0.0], accent);
    }
}

/// Eight tapering tail bones with alternating lateral sway and dorsal spikes.
fn dragon_tail(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    m: DragonMotion,
) {
    use EntityRigPart::*;
    let s = m.scale;
    let accent = DRAGON_ACCENT;
    for i in 0..8 {
        let f = i as f32;
        let radius = 0.54 - f * 0.045;
        let phase = m.tail_drive - f * 0.52;
        let x = phase.sin() * 0.10 * (f + 1.0);
        let y = 3.20 - f * 0.07 + phase.cos() * 0.035 * f;
        let z = 2.10 + f * 0.78;
        let pivot = [x, y, z - 0.34];
        let rot = [0.03 * f, phase.cos() * 0.10 * (0.5 + f * 0.12), 0.0];
        add_unit(mesh, spec, pose, s, Tail, [x, y, z], [radius * 1.55, radius * 1.25, 0.86], pivot,
            rot);
        add_unit_tinted(mesh, spec, pose, s, Tail, [x, y + radius * 0.70, z - 0.04],
            [0.13, 0.34, 0.22], pivot, [-0.18 + rot[0], rot[1], 0.0], accent);
    }
}

/// Dual three-panel wings. Inner bones are opaque body material; the broad
/// outer membranes are thin accent plates with progressive flap angles.
fn dragon_wings(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    m: DragonMotion,
) {
    use EntityRigPart::*;
    let s = m.scale;
    let accent = DRAGON_ACCENT;
    for side in [-1.0_f32, 1.0] {
        let wing_part = if side < 0.0 { LeftWing } else { RightWing };
        let root = [side * 1.02, 3.82, -0.72];
        let inner_rot = [0.03, -side * 0.18, side * (m.flap * 0.35 - 0.10)];
        add_unit(mesh, spec, pose, s, wing_part, [side * 2.25, 3.85, -0.52], [2.55, 0.20, 0.55],
            root, inner_rot);
        add_unit_tinted(mesh, spec, pose, s, wing_part, [side * 2.45, 3.68, -0.05],
            [2.55, 0.07, 1.25], root, inner_rot, accent);
        let elbow = [side * 3.40, 3.82, -0.66];
        let mid_rot = [0.04, -side * 0.38, side * (m.flap * 0.78 - 0.16)];
        add_unit_tinted(mesh, spec, pose, s, wing_part, [side * 4.85, 3.72, -0.05],
            [3.10, 0.075, 1.72], elbow, mid_rot, accent);
        add_unit(mesh, spec, pose, s, wing_part, [side * 4.30, 3.88, -0.70], [1.85, 0.16, 0.30],
            elbow, mid_rot);
        let tip_pivot = [side * 6.10, 3.70, 0.20];
        let tip_rot = [0.02, -side * 0.58, side * (m.flap * 1.05 - 0.20)];
        add_unit_tinted(mesh, spec, pose, s, wing_part, [side * 6.85, 3.62, 0.95],
            [1.72, 0.065, 1.42], tip_pivot, tip_rot, accent);
    }
}

/// Ender dragon (id 31), authored in a 16-block-wingspan unit template.
/// Forward is -Z. The rig has the Vanilla silhouette cues that the old avian
/// proxy lacked: jawed horned head, five neck bones, broad torso, two
/// three-panel bat wings, four legs, and an eight-segment whip tail.
fn add_vanilla_ender_dragon(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    let base = pose.base;
    let m = DragonMotion {
        scale: (spec.width / 16.0).min(spec.height / 8.0),
        flap: (base.age_ticks * 0.32 + base.limb_swing).sin()
            * (0.28 + base.limb_swing_amount * 0.35),
        tail_drive: base.age_ticks * 0.22 + base.limb_swing,
        yaw: base.head_yaw_deg.to_radians(),
        pitch: base.head_pitch_deg.to_radians(),
    };
    dragon_torso_neck(mesh, spec, pose, m);
    dragon_head(mesh, spec, pose, m);
    dragon_legs(mesh, spec, pose, m);
    dragon_tail(mesh, spec, pose, m);
    dragon_wings(mesh, spec, pose, m);
}

/// Wither (id 119): three independent skulls, ribcage, spine/tail bones, and
/// an optional translucent armor shell for the half-health armored state.
fn add_vanilla_wither(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let yaw = pose.base.head_yaw_deg.to_radians();
    let pitch = pose.base.head_pitch_deg.to_radians();
    let center_pivot = [0.0, h * 0.72, 0.0];
    let head_size = [w * 0.72, h * 0.18, w * 0.72];

    add(mesh, spec, pose, Torso, [0.0, h * 0.55, 0.0], [w * 0.56, h * 0.32, w * 0.48], center_pivot,
        [0.0; 3], 0.0);
    // Ribcage slats and the tapering spine/tail beneath the center head.
    for i in 0..4 {
        let f = i as f32;
        add(mesh, spec, pose, Torso, [0.0, h * (0.49 - f * 0.065), 0.0],
            [w * (0.86 - f * 0.12), h * 0.035, w * 0.42], center_pivot,
            [0.0, 0.0, (f - 1.5) * 0.035], 0.0);
    }
    let body_sway = pose.base.limb_swing.sin() * pose.base.limb_swing_amount * 0.12
        + (pose.base.swing_progress * PI).sin() * 0.10;
    for i in 0..3 {
        let f = i as f32;
        add(mesh, spec, pose, Tail, [0.0, h * (0.31 - f * 0.09), w * 0.03],
            [w * (0.22 - f * 0.04), h * 0.12, w * (0.22 - f * 0.04)], [0.0, h * 0.38, 0.0],
            [0.12 * f, body_sway * (f + 1.0), 0.0], 0.0);
    }

    add(mesh, spec, pose, Head, [0.0, h * 0.82, -w * 0.02], head_size, center_pivot,
        [pitch, yaw, 0.0], 0.0);
    // Side heads tilt outward and scan independently with a small age-driven nod.
    for side in [-1.0_f32, 1.0] {
        let nod = (pose.base.age_ticks * 0.35 + side).sin() * 0.08 + body_sway * side;
        let pivot = [side * w * 0.58, h * 0.70, 0.0];
        add(mesh, spec, pose, Head, [side * w * 0.76, h * 0.74, 0.0],
            [w * 0.54, h * 0.14, w * 0.54], pivot,
            [pitch * 0.5 + nod, yaw * 0.35 + side * 0.42, side * 0.10], 0.0);
    }
    if pose.wither_armored {
        // Transparent armor is represented by slightly inflated shell plates;
        // the renderer resolves translucency from the shell part/material.
        add(mesh, spec, pose, Shell, [0.0, h * 0.61, 0.0], [w * 0.92, h * 0.46, w * 0.72],
            center_pivot, [0.0; 3], w * 0.025);
        add(mesh, spec, pose, Shell, [0.0, h * 0.82, -w * 0.02], head_size, center_pivot,
            [pitch, yaw, 0.0], w * 0.025);
    }
}

/// Warden (id 116): wide horned head with vibrating tendrils, barrel chest,
/// pulsing soul core, and heavy articulated arms/legs.
fn add_vanilla_warden(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let pulse = 1.0 + (pose.base.age_ticks * 0.45).sin() * 0.10;

    add(mesh, spec, pose, Torso, [0.0, h * 0.58, 0.0], [w * 1.12, h * 0.38, w * 0.72],
        [0.0, h * 0.68, 0.0], [0.0; 3], 0.0);
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.62, -w * 0.39],
        [w * 0.34 * pulse, h * 0.12 * pulse, w * 0.08], [0.0, h * 0.62, -w * 0.30], [0.0; 3], 0.0,
        9);
    let head_pivot = [0.0, h * 0.78, 0.0];
    add(mesh, spec, pose, Head, [0.0, h * 0.86, -w * 0.02], [w * 1.12, h * 0.22, w * 0.86],
        head_pivot, [0.0; 3], 0.0);
    // Wide antler-like horns.
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Head, [side * w * 0.70, h * 0.91, 0.0],
            [w * 0.46, h * 0.07, w * 0.16], head_pivot, [0.0, 0.0, side * 0.35], 0.0);
        add(mesh, spec, pose, Head, [side * w * 0.91, h * 0.98, 0.0],
            [w * 0.15, h * 0.16, w * 0.13], head_pivot, [0.0, 0.0, side * 0.18], 0.0);
        // Four independently vibrating tendrils (two per side).
        for index in 0..2 {
            let f = index as f32;
            let vibration = (pose.base.age_ticks * 1.7 + side * 0.8 + f * 1.9).sin() * 0.18;
            add_tinted(mesh, spec, pose, Head,
                [ side * w * (0.28 + f * 0.20), h * (0.76 - f * 0.035), -w * 0.34, ],
                [w * 0.055, h * 0.18, w * 0.055], head_pivot,
                [vibration, 0.0, side * (0.15 + f * 0.10)], 0.0, 9);
        }
    }
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let shoulder = [side * w * 0.70, h * 0.70, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.88, h * 0.47, 0.0],
            [w * 0.34, h * 0.48, w * 0.38], shoulder, [0.0; 3], 0.0);
        add(mesh, spec, pose, part, [side * w * 0.91, h * 0.20, -w * 0.02],
            [w * 0.40, h * 0.17, w * 0.44], shoulder, [0.0; 3], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let hip = [side * w * 0.28, h * 0.39, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.30, h * 0.20, 0.0],
            [w * 0.34, h * 0.40, w * 0.38], hip, [0.0; 3], 0.0);
    }
}

/// Guardian/elder guardian: cube body, eye pupil, twelve retractable spikes,
/// and a three-piece tail fin. `elder` only changes proportions through the
/// registry dimensions already present on `spec`.
fn add_vanilla_guardian(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let body_y = h * 0.53;
    add(mesh, spec, pose, Core, [0.0, body_y, 0.0], [w * 0.72, h * 0.72, w * 0.72],
        [0.0, body_y, 0.0], [0.0; 3], 0.0);
    // Large eye and dark pupil on the forward (-Z) face.
    add_tinted(mesh, spec, pose, Head, [0.0, body_y, -w * 0.39], [w * 0.30, h * 0.24, w * 0.055],
        [0.0, body_y, -w * 0.25], [0.0; 3], 0.0, 10);
    add_tinted(mesh, spec, pose, Head, [0.0, body_y, -w * 0.43], [w * 0.13, h * 0.11, w * 0.04],
        [0.0, body_y, -w * 0.25], [0.0; 3], 0.0, 11);
    // Twelve spikes: four top/bottom diagonal directions and eight around the
    // sides. Extension breathes with age/open_amount instead of popping.
    for i in 0..12 {
        let f = i as f32;
        let ring = i % 4;
        let tier = i / 4;
        let angle = f32::from(ring as u8) * FRAC_PI_2 + f32::from(tier as u8) * 0.32;
        let x = angle.cos() * w * 0.43;
        let z = angle.sin() * w * 0.43;
        let y = body_y + (f32::from(tier as u8) - 1.0) * h * 0.25;
        let extension =
            0.72 + pose.open_amount * 0.35 + (pose.base.age_ticks * 0.4 + f).sin() * 0.08;
        add_tinted(mesh, spec, pose, Shell,
            [ x * (1.0 + extension * 0.18), y, z * (1.0 + extension * 0.18), ],
            [w * 0.07, h * 0.25 * extension, w * 0.07], [x, y, z],
            [0.45 * z.signum(), angle, -0.45 * x.signum()], 0.0, 12);
    }
    // Three tail-fin segments trail behind +Z and undulate independently.
    for i in 0..3 {
        let f = i as f32;
        let wave = (pose.base.age_ticks * 0.5 + pose.base.limb_swing - f * 0.7).sin() * 0.18;
        add(mesh, spec, pose, Tail, [0.0, body_y - f * h * 0.03, w * (0.43 + f * 0.20)],
            [w * (0.30 - f * 0.05), h * 0.13, w * 0.23], [0.0, body_y, w * 0.34],
            [wave, wave * 0.5, 0.0], 0.0);
    }
}

/// Ghast (id 45): large floating cube head/body and nine independently waving
/// tentacles hanging below it.
fn add_vanilla_ghast(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.69, 0.0], [w * 0.88, h * 0.55, w * 0.88],
        [0.0, h * 0.82, 0.0], [0.0; 3], 0.0);
    for ix in -1..=1 {
        for iz in -1..=1 {
            let index = (ix + 1) * 3 + (iz + 1);
            let phase = pose.base.age_ticks * 0.55 + f32::from(index as u8) * 0.73;
            let x = ix as f32 * w * 0.24;
            let z = iz as f32 * w * 0.24;
            let pivot = [x, h * 0.43, z];
            add(mesh, spec, pose, Tail,
                [ x + phase.sin() * w * 0.025, h * 0.25, z + phase.cos() * w * 0.018, ],
                [w * 0.11, h * 0.38, w * 0.11], pivot,
                [phase.sin() * 0.15, 0.0, phase.cos() * 0.13], 0.0);
        }
    }
}

/// Blaze (id 8): floating head plus twelve rods orbiting in three tiers.
fn add_vanilla_blaze(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let head_pivot = [0.0, h * 0.72, 0.0];
    add(mesh, spec, pose, Head, [0.0, h * 0.82, 0.0], [w * 0.62, h * 0.28, w * 0.62], head_pivot,
        [0.0; 3], 0.0);
    for tier in 0..3 {
        for index in 0..4 {
            let rod = tier * 4 + index;
            let angle = pose.base.age_ticks * 0.72 + rod as f32 * TAU / 12.0 + tier as f32 * 0.28;
            let radius = w * (0.58 + tier as f32 * 0.08);
            let x = angle.cos() * radius;
            let z = angle.sin() * radius;
            let y = h * (0.35 + tier as f32 * 0.17);
            add_tinted(mesh, spec, pose, Other, [x, y, z], [w * 0.10, h * 0.43, w * 0.10],
                [x, y - h * 0.18, z], [0.08 * angle.sin(), angle, 0.08 * angle.cos()], 0.0, 6);
        }
    }
}

/// Breeze (id 12): floating head, two orbiting wind-rod rings, and a tapered
/// wind-cloak cone. Rods counter-rotate so even a static pose reads as moving air.
fn add_vanilla_breeze(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Head, [0.0, h * 0.83, 0.0], [w * 0.58, h * 0.24, w * 0.58],
        [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    for tier in 0..2 {
        let direction = if tier == 0 { 1.0_f32 } else { -1.0 };
        for index in 0..6 {
            let angle = direction * pose.base.age_ticks * 0.85
                + index as f32 * TAU / 6.0
                + tier as f32 * 0.4;
            let radius = w * (0.48 + tier as f32 * 0.18);
            let x = angle.cos() * radius;
            let z = angle.sin() * radius;
            let y = h * (0.54 + tier as f32 * 0.13);
            add_tinted(mesh, spec, pose, Other, [x, y, z], [w * 0.075, h * 0.28, w * 0.075],
                [x, y - h * 0.12, z], [0.2, angle, direction * 0.15], 0.0, 5);
        }
    }
    // Cloak cone: stacked, yawed thin plates tapering downward.
    for i in 0..4 {
        let f = i as f32;
        add(mesh, spec, pose, Torso, [0.0, h * (0.47 - f * 0.10), 0.0],
            [w * (0.58 - f * 0.09), h * 0.09, w * (0.58 - f * 0.09)], [0.0, h * 0.58, 0.0],
            [0.0, pose.base.age_ticks * 0.35 + f * 0.35, 0.0], 0.0);
    }
}

/// Shulker (id 88): box shell base, hinged rotating lid, and a small inner
/// head that rises as the shell opens.
fn add_vanilla_shulker(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let auto_open = ((pose.base.age_ticks * 0.18).sin() + 1.0) * 0.20;
    let open = pose.open_amount.max(auto_open).clamp(0.0, 1.0);
    add(mesh, spec, pose, Shell, [0.0, h * 0.28, 0.0], [w * 0.86, h * 0.56, w * 0.86], [0.0; 3],
        [0.0; 3], 0.0);
    let hinge = [0.0, h * 0.57, w * 0.42];
    add(mesh, spec, pose, Shell, [0.0, h * 0.72, 0.0], [w * 0.90, h * 0.28, w * 0.90], hinge,
        [-open * 1.15, 0.0, 0.0], 0.0);
    add_tinted(mesh, spec, pose, Head, [0.0, h * (0.53 + open * 0.18), -w * 0.03],
        [w * 0.40, h * 0.30, w * 0.40], [0.0, h * 0.47, 0.0], [0.0; 3], 0.0, 13);
}

/// Slime/magma cube shared layered body. Slime gets a translucent outer gel
/// cube, inner core, eyes, and mouth; magma cube gets stacked dark slices with
/// a glowing core. Size pulses with `swell_amount` and locomotion squash.
fn add_vanilla_slime_like(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let squash = 1.0
        + pose.swell_amount * 0.25
        + pose.base.limb_swing_amount * pose.base.limb_swing.sin().abs() * 0.06;
    if spec.entity_type_id == 67 {
        // Magma cube: separated horizontal slices sliding subtly out of phase.
        for i in 0..4 {
            let f = i as f32;
            let offset = (pose.base.age_ticks * 0.5 + f).sin() * 0.018 * f;
            add_tinted(mesh, spec, pose, Shell, [offset, h * (0.15 + f * 0.22), 0.0],
                [ w * (0.92 - f * 0.07) * squash, h * 0.19, w * (0.92 - f * 0.07) * squash, ],
                [0.0; 3], [0.0; 3], 0.0, 14);
        }
        add_tinted(mesh, spec, pose, Core, [0.0, h * 0.43, 0.0], [w * 0.48, h * 0.42, w * 0.48],
            [0.0; 3], [0.0; 3], 0.0, 6);
    } else {
        add(mesh, spec, pose, Shell, [0.0, h * 0.5, 0.0], [w * squash, h, w * squash], [0.0; 3],
            [0.0; 3], 0.0);
        add_tinted(mesh, spec, pose, Core, [0.0, h * 0.45, 0.0], [w * 0.54, h * 0.50, w * 0.54],
            [0.0; 3], [0.0; 3], 0.0, 15);
        for side in [-1.0_f32, 1.0] {
            add_tinted(mesh, spec, pose, Head, [side * w * 0.17, h * 0.58, -w * 0.47],
                [w * 0.12, h * 0.12, w * 0.035], [0.0, h * 0.55, 0.0], [0.0; 3], 0.0, 16);
        }
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.39, -w * 0.47],
            [w * 0.22, h * 0.06, w * 0.035], [0.0, h * 0.45, 0.0], [0.0; 3], 0.0, 16);
    }
}

/// Phantom (id 76): flat head/body, broad flapping wings, and tapering tail.
fn add_vanilla_phantom(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.55, 0.0], [w * 0.44, h * 0.25, w * 0.76],
        [0.0, h * 0.55, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.57, -w * 0.48], [w * 0.38, h * 0.28, w * 0.32],
        [0.0, h * 0.55, -w * 0.20], [0.0; 3], 0.0);
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.18, h * 0.58, -w * 0.02];
        add(mesh, spec, pose, part, [side * w * 0.72, h * 0.56, 0.0],
            [w * 0.98, h * 0.055, w * 0.54], pivot, [0.0; 3], 0.0);
        add(mesh, spec, pose, part, [side * w * 1.20, h * 0.53, w * 0.10],
            [w * 0.56, h * 0.045, w * 0.38], pivot, [0.0, -side * 0.20, 0.0], 0.0);
    }
    for i in 0..3 {
        let f = i as f32;
        add(mesh, spec, pose, Tail, [0.0, h * (0.54 - f * 0.02), w * (0.48 + f * 0.20)],
            [w * (0.18 - f * 0.035), h * 0.08, w * 0.25], [0.0, h * 0.54, w * 0.35], [0.0; 3], 0.0);
    }
}

/// Humanoid silhouette families used by the dedicated vanilla rigs. These are
/// internal modeling tags, not public archetypes: the public eight-family
/// classification remains stable for compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HumanoidStyle {
    Player,
    Zombie,
    Skeleton,
    Bogged,
    Stray,
    Illager,
    IllagerArmed,
    IllagerHooded,
    Witch,
    Piglin,
    ZombieVillager,
    ZombifiedPiglin,
    WitherSkeleton,
}

fn radians3(degrees: [f32; 3]) -> [f32; 3] {
    degrees.map(f32::to_radians)
}

/// Generic hand-item and armor attachment shells. The item prisms are
/// deliberately simple; their importance is that they bind to the correct
/// hand pivots and inherit walk/attack articulation.
fn add_humanoid_attachments(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    if pose.main_hand_item {
        let offset = pose.main_hand_offset;
        let rotation = radians3(pose.main_hand_rotation_deg);
        add_tinted(mesh, spec, pose, RightArm,
            [ w * 0.48 + offset[0], h * 0.43 + offset[1], -w * 0.22 + offset[2], ],
            [w * 0.09, h * 0.34, w * 0.09], [w * 0.43, h * 0.73, 0.0], rotation, 0.0, 18);
    }
    if pose.offhand_item {
        let offset = pose.offhand_offset;
        let rotation = radians3(pose.offhand_rotation_deg);
        add_tinted(mesh, spec, pose, LeftArm,
            [ -w * 0.48 + offset[0], h * 0.43 + offset[1], -w * 0.22 + offset[2], ],
            [w * 0.09, h * 0.30, w * 0.09], [-w * 0.43, h * 0.73, 0.0], rotation, 0.0, 19);
    }
    if pose.armor_visible {
        add_tinted(mesh, spec, pose, Torso, [0.0, h * 0.57, 0.0], [w * 0.70, h * 0.38, w * 0.43],
            [0.0, h * 0.75, 0.0], [0.0; 3], w * 0.018, 20);
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.875, 0.0], [w * 0.58, h * 0.21, w * 0.58],
            [0.0, h * 0.75, 0.0], [0.0; 3], w * 0.018, 20);
    }
}

impl HumanoidStyle {
    fn skeletal(self) -> bool {
        matches!(
            self,
            HumanoidStyle::Skeleton
                | HumanoidStyle::Bogged
                | HumanoidStyle::Stray
                | HumanoidStyle::WitherSkeleton
        )
    }

    fn illager(self) -> bool {
        matches!(
            self,
            HumanoidStyle::Illager
                | HumanoidStyle::IllagerArmed
                | HumanoidStyle::IllagerHooded
                | HumanoidStyle::Witch
                | HumanoidStyle::ZombieVillager
        )
    }

    fn piglin(self) -> bool {
        matches!(self, HumanoidStyle::Piglin | HumanoidStyle::ZombifiedPiglin)
    }

    fn zombie_forward(self) -> bool {
        matches!(
            self,
            HumanoidStyle::Zombie | HumanoidStyle::ZombieVillager | HumanoidStyle::ZombifiedPiglin
        )
    }
}

/// Precomputed humanoid proportions shared by the body-part sub-builders.
#[derive(Clone, Copy)]
struct HumanoidDims {
    w: f32,
    h: f32,
    limb_w: f32,
    limb_d: f32,
    leg_h: f32,
    torso_h: f32,
    torso_depth: f32,
    head_size: [f32; 3],
    head_pivot: [f32; 3],
    torso_pivot: [f32; 3],
}

impl HumanoidDims {
    fn new(spec: EntityRigSpec, style: HumanoidStyle) -> Self {
        let w = spec.width;
        let h = spec.height;
        let skeletal = style.skeletal();
        let illager = style.illager();
        let limb_w = if skeletal { 0.13 } else { 0.22 };
        let limb_d = if skeletal { 0.16 } else { 0.30 };
        let leg_h = if skeletal { 0.43 } else { 0.40 };
        let torso_h = if illager { 0.43 } else { 0.38 };
        let torso_depth = if illager { 0.48 } else { 0.40 };
        let head_factor = (spec.proportions.head / 0.25).clamp(0.5, 2.0);
        let head_size = if skeletal {
            [w * 0.58, h * 0.21 * head_factor, w * 0.58]
        } else {
            [w * 0.62, h * 0.22 * head_factor, w * 0.62]
        };
        Self {
            w,
            h,
            limb_w,
            limb_d,
            leg_h,
            torso_h,
            torso_depth,
            head_size,
            head_pivot: [0.0, h * 0.76, 0.0],
            torso_pivot: [0.0, h * 0.68, 0.0],
        }
    }
}

/// Legs and torso. Illager robes extend below the belt as a skirt; other
/// humanoids keep the familiar two-leg silhouette.
fn add_humanoid_legs_torso(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    dims: &HumanoidDims,
    illager: bool,
) {
    use EntityRigPart::*;
    let (w, h) = (dims.w, dims.h);
    let (limb_w, limb_d, leg_h) = (dims.limb_w, dims.limb_d, dims.leg_h);
    let (torso_h, torso_depth, torso_pivot) = (dims.torso_h, dims.torso_depth, dims.torso_pivot);
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.18, h * leg_h, 0.0];
        add(mesh, spec, pose, part, [pivot[0], h * leg_h * 0.5, 0.0],
            [w * (limb_w + 0.04), h * leg_h, w * (limb_d + 0.03)], pivot, [0.0; 3], 0.0);
    }
    add(mesh, spec, pose, Torso, [0.0, h * (leg_h + torso_h * 0.5), 0.0],
        [w * 0.66, h * torso_h, w * torso_depth], torso_pivot, [0.0; 3], 0.0);
    if illager {
        add(mesh, spec, pose, Torso, [0.0, h * 0.39, 0.0], [w * 0.72, h * 0.24, w * 0.50],
            torso_pivot, [0.0; 3], 0.0);
    }
}

/// Head box plus the family-specific facial silhouette: illager nose and
/// unibrow, piglin snout/ears, witch hat, rogue hood, bogged moss cap, and
/// stray icy strips.
fn add_humanoid_head(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    style: HumanoidStyle,
    dims: &HumanoidDims,
) {
    use EntityRigPart::*;
    let (w, h) = (dims.w, dims.h);
    let (head_size, head_pivot, torso_pivot) = (dims.head_size, dims.head_pivot, dims.torso_pivot);
    let illager = style.illager();
    let piglin = style.piglin();
    add(mesh, spec, pose, Head, [0.0, h * 0.875, 0.0], head_size, head_pivot, [0.0; 3], 0.0);
    if illager {
        add(mesh, spec, pose, Head, [0.0, h * 0.82, -w * 0.43], [w * 0.16, h * 0.11, w * 0.23],
            head_pivot, [0.0; 3], 0.0);
        // Heavy unibrow silhouette shared by villagers and illagers.
        add(mesh, spec, pose, Head, [0.0, h * 0.91, -w * 0.34], [w * 0.44, h * 0.045, w * 0.08],
            head_pivot, [0.0; 3], 0.0);
    }
    if piglin {
        add(mesh, spec, pose, Head, [0.0, h * 0.82, -w * 0.42], [w * 0.35, h * 0.10, w * 0.24],
            head_pivot, [0.0; 3], 0.0);
        for side in [-1.0_f32, 1.0] {
            add(mesh, spec, pose, Head, [side * w * 0.34, h * 0.94, -w * 0.02],
                [w * 0.12, h * 0.12, w * 0.07], head_pivot, [0.0, 0.0, side * 0.35], 0.0);
        }
    }
    if matches!(style, HumanoidStyle::Witch) {
        // Wide brim plus three shrinking hat tiers, tilted backwards.
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.985, 0.0], [w * 0.98, h * 0.035, w * 0.98],
            head_pivot, [-0.08, 0.0, 0.0], 0.0, 21);
        for i in 0..3 {
            let f = i as f32;
            add_tinted(mesh, spec, pose, Head, [0.0, h * (1.02 + f * 0.055), w * 0.02],
                [w * (0.58 - f * 0.13), h * 0.07, w * (0.58 - f * 0.13)], head_pivot,
                [-0.12 - f * 0.04, 0.0, 0.0], 0.0, 21);
        }
    }
    if matches!(style, HumanoidStyle::IllagerHooded) {
        add(mesh, spec, pose, Head, [0.0, h * 0.90, 0.0], [w * 0.69, h * 0.25, w * 0.69],
            head_pivot, [0.0; 3], w * 0.018);
    }
    if matches!(style, HumanoidStyle::Bogged) {
        // Mossy cap and two tiny mushrooms make bogged distinct from stray.
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.98, 0.0], [w * 0.64, h * 0.06, w * 0.64],
            head_pivot, [0.0; 3], 0.0, 22);
        for side in [-1.0_f32, 1.0] {
            add_tinted(mesh, spec, pose, Head, [side * w * 0.22, h * 1.03, -w * 0.08],
                [w * 0.14, h * 0.06, w * 0.14], head_pivot, [0.0; 3], 0.0, 23);
        }
    }
    if matches!(style, HumanoidStyle::Stray) {
        // Ragged icy hood/strips.
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.93, 0.0], [w * 0.67, h * 0.11, w * 0.67],
            head_pivot, [0.0; 3], 0.0, 24);
        add_tinted(mesh, spec, pose, Torso, [0.0, h * 0.63, -w * 0.23],
            [w * 0.70, h * 0.08, w * 0.06], torso_pivot, [0.0; 3], 0.0, 24);
    }
}

/// Arms. Villager-line mobs cross their arms inside the robe until a cast,
/// attack, or item-use state unfolds them. Zombies hold both arms forward;
/// skeletons keep thinner limbs for the bow silhouette.
fn add_humanoid_arms(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    style: HumanoidStyle,
    dims: &HumanoidDims,
) {
    use EntityRigPart::*;
    let (w, h) = (dims.w, dims.h);
    let (limb_w, limb_d) = (dims.limb_w, dims.limb_d);
    let unfold = pose.spellcasting
        || pose.using_item
        || pose.main_hand_item
        || pose.offhand_item
        || pose.base.swing_progress > 0.0;
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.43, h * 0.72, 0.0];
        if style.illager() && !unfold && !matches!(style, HumanoidStyle::ZombieVillager) {
            add(mesh, spec, pose, part, [side * w * 0.12, h * 0.61, -w * 0.29],
                [w * 0.52, h * 0.075, w * 0.16], pivot, [0.0, -side * 0.42, side * 0.10], 0.0);
        } else {
            let forward = if style.zombie_forward() { -1.25 } else { 0.0 };
            add(mesh, spec, pose, part,
                [ pivot[0], h * 0.50, if style.zombie_forward() { -w * 0.27 } else { 0.0 }, ],
                [w * limb_w, h * 0.40, w * limb_d], pivot, [forward, 0.0, 0.0], 0.0);
        }
    }
}

/// Family weapons/tools baked into the default silhouette where Vanilla
/// almost always carries them; the generic attachment state can add more.
fn add_humanoid_weapons(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    style: HumanoidStyle,
    dims: &HumanoidDims,
) {
    use EntityRigPart::*;
    let (w, h) = (dims.w, dims.h);
    if style.skeletal() {
        // Bow arcs: three thin tinted rods on the left hand.
        for i in 0..3 {
            let f = i as f32 - 1.0;
            add_tinted(mesh, spec, pose, LeftArm, [-w * 0.51, h * (0.43 + f * 0.08), -w * 0.22],
                [w * 0.045, h * 0.17, w * 0.045], [-w * 0.43, h * 0.72, 0.0], [0.35, 0.0, f * 0.28],
                0.0, 18);
        }
    }
    if matches!(style, HumanoidStyle::IllagerArmed) {
        add_tinted(mesh, spec, pose, RightArm, [w * 0.50, h * 0.39, -w * 0.25],
            [w * 0.07, h * 0.34, w * 0.07], [w * 0.43, h * 0.72, 0.0], [-0.55, 0.0, 0.12], 0.0, 18);
    }
    if matches!(style, HumanoidStyle::WitherSkeleton) {
        add_tinted(mesh, spec, pose, RightArm, [w * 0.50, h * 0.42, -w * 0.32],
            [w * 0.08, h * 0.48, w * 0.08], [w * 0.43, h * 0.72, 0.0], [-0.70, 0.0, 0.10], 0.0, 18);
    }
}

/// Optional inflated second shell for every body part when the 3D skin-layer
/// flag is set.
fn add_humanoid_skin_layers(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    dims: &HumanoidDims,
) {
    use EntityRigPart::*;
    if !pose.base.enable_skin_3d_layers {
        return;
    }
    let (w, h) = (dims.w, dims.h);
    let (limb_w, limb_d, leg_h) = (dims.limb_w, dims.limb_d, dims.leg_h);
    let (torso_h, torso_depth) = (dims.torso_h, dims.torso_depth);
    let (head_size, head_pivot, torso_pivot) = (dims.head_size, dims.head_pivot, dims.torso_pivot);
    add(mesh, spec, pose, Head, [0.0, h * 0.875, 0.0], head_size, head_pivot, [0.0; 3],
        w * 0.018);
    add(mesh, spec, pose, Torso, [0.0, h * (leg_h + torso_h * 0.5), 0.0],
        [w * 0.66, h * torso_h, w * torso_depth], torso_pivot, [0.0; 3], w * 0.012);
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.43, h * 0.72, 0.0];
        add(mesh, spec, pose, part, [pivot[0], h * 0.50, 0.0],
            [w * limb_w, h * 0.40, w * limb_d], pivot, [0.0; 3], w * 0.010);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.18, h * leg_h, 0.0];
        add(mesh, spec, pose, part, [pivot[0], h * leg_h * 0.5, 0.0],
            [w * (limb_w + 0.04), h * leg_h, w * (limb_d + 0.03)], pivot, [0.0; 3], w * 0.010);
    }
}

/// Shared articulated humanoid body. Style controls the parts that make the
/// vanilla families legible at a glance: illager noses and crossed arms,
/// witch hats, piglin snouts/ears, skeletal thin limbs, zombie forward arms,
/// and the extended armor/held-item attachment state.
fn add_humanoid_rig(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    style: HumanoidStyle,
) {
    let dims = HumanoidDims::new(spec, style);
    add_humanoid_legs_torso(mesh, spec, pose, &dims, style.illager());
    add_humanoid_head(mesh, spec, pose, style, &dims);
    add_humanoid_arms(mesh, spec, pose, style, &dims);
    add_humanoid_weapons(mesh, spec, pose, style, &dims);
    add_humanoid_skin_layers(mesh, spec, pose, &dims);
    add_humanoid_attachments(mesh, spec, pose);
}

/// Iron golem (id 57): broad torso, villager-like nose, very long arms, and
/// heavy legs. The proportions intentionally dwarf the standard biped instead
/// of merely scaling it uniformly.
fn add_vanilla_iron_golem(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let head_pivot = [0.0, h * 0.79, 0.0];
    add(mesh, spec, pose, Torso, [0.0, h * 0.61, 0.0], [w * 0.95, h * 0.36, w * 0.55],
        [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Torso, [0.0, h * 0.38, 0.0], [w * 0.70, h * 0.16, w * 0.44],
        [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.86, -w * 0.02], [w * 0.55, h * 0.16, w * 0.50],
        head_pivot, [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.81, -w * 0.36], [w * 0.14, h * 0.10, w * 0.20],
        head_pivot, [0.0; 3], 0.0);
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.55, h * 0.73, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.66, h * 0.43, 0.0],
            [w * 0.22, h * 0.62, w * 0.28], pivot, [0.0; 3], 0.0);
        add(mesh, spec, pose, part, [side * w * 0.66, h * 0.12, -w * 0.01],
            [w * 0.28, h * 0.13, w * 0.32], pivot, [0.0; 3], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.24, h * 0.34, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.25, h * 0.17, 0.0],
            [w * 0.28, h * 0.34, w * 0.34], pivot, [0.0; 3], 0.0);
    }
    if pose.main_hand_item {
        // The golem's poppy: stem and blossom attached to the right hand.
        add_tinted(mesh, spec, pose, RightArm, [w * 0.68, h * 0.21, -w * 0.21],
            [w * 0.035, h * 0.20, w * 0.035], [w * 0.55, h * 0.73, 0.0], [-0.35, 0.0, 0.0], 0.0,
            25);
        add_tinted(mesh, spec, pose, RightArm, [w * 0.68, h * 0.31, -w * 0.25],
            [w * 0.12, h * 0.07, w * 0.12], [w * 0.55, h * 0.73, 0.0], [-0.35, 0.0, 0.0], 0.0, 26);
    }
}

/// Enderman (id 33): very tall thin limbs, cuboid head, dropping jaw when
/// screaming, and an optional carried block mounted between the long hands.
fn add_vanilla_enderman(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let head_pivot = [0.0, h * 0.82, 0.0];
    add(mesh, spec, pose, Torso, [0.0, h * 0.61, 0.0], [w * 0.55, h * 0.28, w * 0.36],
        [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.91, 0.0], [w * 0.62, h * 0.17, w * 0.62], head_pivot,
        [0.0; 3], 0.0);
    let jaw_drop = if pose.screaming {
        0.55
    } else {
        pose.open_amount * 0.55
    };
    add_tinted(mesh, spec, pose, Head, [0.0, h * (0.84 - jaw_drop * 0.05), -w * 0.18],
        [w * 0.44, h * 0.045 + jaw_drop * h * 0.02, w * 0.36], head_pivot, [jaw_drop, 0.0, 0.0],
        0.0, 27);
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.38, h * 0.73, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.43, h * 0.43, 0.0],
            [w * 0.13, h * 0.62, w * 0.16], pivot, [0.0; 3], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.16, h * 0.43, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.17, h * 0.215, 0.0],
            [w * 0.15, h * 0.43, w * 0.18], pivot, [0.0; 3], 0.0);
    }
    if pose.carried_block {
        add_tinted(mesh, spec, pose, Other, [0.0, h * 0.49, -w * 0.52],
            [w * 0.52, w * 0.52, w * 0.52], [0.0, h * 0.55, -w * 0.25], [0.15, 0.25, 0.0], 0.0, 28);
    }
}

/// Creeper (id 23): tall headless-neck torso, four stubby diagonal-gait legs,
/// and a swelling factor used by the primed flashing animation.
fn add_vanilla_creeper(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let swell = 1.0 + pose.swell_amount * 0.28;
    add(mesh, spec, pose, Torso, [0.0, h * 0.48, 0.0],
        [w * 0.58 * swell, h * 0.58 * swell, w * 0.45 * swell], [0.0, h * 0.60, 0.0], [0.0; 3],
        0.0);
    // Creeper head is fused directly atop the torso: no visible neck.
    add(mesh, spec, pose, Head, [0.0, h * 0.86, -w * 0.01],
        [w * 0.66 * swell, h * 0.24 * swell, w * 0.66 * swell], [0.0, h * 0.72, 0.0], [0.0; 3],
        0.0);
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for (z, phase) in [(-0.20_f32, 0.0_f32), (0.22, PI)] {
            let pivot = [side * w * 0.20, h * 0.24, z * w];
            let gait = (pose.base.limb_swing + phase + if side < 0.0 { 0.0 } else { PI }).cos()
                * pose.base.limb_swing_amount
                * 0.35;
            add(mesh, spec, pose, part, [pivot[0], h * 0.11, pivot[2]],
                [w * 0.22, h * 0.22, w * 0.24], pivot, [gait, 0.0, side * 0.06], 0.0);
        }
    }
}

/// Armor stand (id 3): stone base plate, central post, and a posed wooden
/// mannequin frame rather than a living biped.
fn add_vanilla_armor_stand(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.025, 0.0], [w * 1.15, h * 0.05, w * 1.15],
        [0.0; 3], [0.0; 3], 0.0, 29);
    add(mesh, spec, pose, Torso, [0.0, h * 0.45, 0.0], [w * 0.10, h * 0.78, w * 0.10],
        [0.0, h * 0.45, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Torso, [0.0, h * 0.66, 0.0], [w * 0.66, h * 0.08, w * 0.10],
        [0.0, h * 0.66, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.90, 0.0], [w * 0.42, h * 0.20, w * 0.42],
        [0.0, h * 0.78, 0.0], [0.0; 3], 0.0);
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.35, h * 0.66, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.43, h * 0.43, 0.0],
            [w * 0.08, h * 0.44, w * 0.08], pivot, [0.0, 0.0, side * 0.08], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.14, h * 0.39, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.18, h * 0.20, 0.0],
            [w * 0.09, h * 0.36, w * 0.09], pivot, [0.0, 0.0, side * 0.06], 0.0);
    }
}

/// Evoker fangs (id 36): paired triangular-looking upper/lower tooth rows.
/// Cuboids are rotated rather than smoothed so the shape stays voxel-clean.
fn add_vanilla_evoker_fangs(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let snap = (pose.base.swing_progress * PI).sin() * 0.35 + pose.open_amount * 0.35;
    for side in [-1.0_f32, 1.0] {
        for i in 0..3 {
            let f = i as f32;
            add_tinted(mesh, spec, pose, Other,
                [ side * w * (0.10 + f * 0.10), h * (0.30 + f * 0.10), -w * 0.08, ],
                [w * 0.10, h * (0.40 - f * 0.06), w * 0.10], [0.0, h * 0.15, 0.0],
                [snap, 0.0, side * (0.22 + f * 0.08)], 0.0, 30);
        }
    }
}

/// Feature switches for the parameterized mammal/quadruped rig. Forward is
/// -Z; rows of legs are articulated in diagonal pairs. The switches cover the
/// major silhouette-carrying anatomy without introducing per-species public
/// types or changing the stable archetype enum.
#[derive(Clone, Copy, Debug, Default)]
struct QuadrupedFeatures {
    long_neck: bool,
    long_ears: bool,
    hump: bool,
    snout: bool,
    pointed_ears: bool,
    horns: bool,
    bearded: bool,
    wool: bool,
    shell: bool,
    mane: bool,
    udder: bool,
    mushrooms: bool,
    long_tail: bool,
    bushy_tail: bool,
    six_legs: bool,
    stocky: bool,
    slim: bool,
}

/// Parameterized articulated quadruped used by horses, camelids, cattle,
/// pigs, sheep, canines/felines, bears, goats, hoglins, and sniffer-like
/// mammals. Dimensions derive from the registry collision box while anatomy
/// remains independently proportioned rather than a uniform cube scale.
/// Precomputed quadruped proportions shared by the body-part sub-builders.
#[derive(Clone, Copy)]
struct QuadrupedDims {
    w: f32,
    h: f32,
    body_len: f32,
    body_w: f32,
    body_h: f32,
    leg_h: f32,
    body_y: f32,
    torso_pivot: [f32; 3],
    head_w: f32,
    head_h: f32,
    head_l: f32,
    neck_len: f32,
    head_y: f32,
    head_z: f32,
    head_pivot: [f32; 3],
}

impl QuadrupedDims {
    fn new(spec: EntityRigSpec, features: QuadrupedFeatures) -> Self {
        let w = spec.width;
        let h = spec.height;
        let body_len = w * if features.stocky {
            1.02
        } else if features.slim {
            0.76
        } else {
            0.88
        };
        let body_w = w * if features.stocky {
            0.82
        } else if features.slim {
            0.55
        } else {
            0.68
        };
        let body_h = h * if features.stocky { 0.42 } else { 0.36 };
        let leg_h = h * if features.long_neck { 0.48 } else { 0.43 };
        let body_y = leg_h + body_h * 0.48;
        // Neck/head. Long-neck species use an inclined neck column; stockier
        // species keep the head mounted directly on the front torso edge.
        let head_w = w * if features.stocky { 0.52 } else { 0.44 };
        let head_h = h * if features.long_neck { 0.20 } else { 0.25 };
        let head_l = w * if features.stocky { 0.46 } else { 0.40 };
        let neck_len = if features.long_neck { h * 0.28 } else { 0.0 };
        let head_y = body_y + body_h * 0.18 + neck_len * 0.70;
        let head_z = -body_len * 0.50 - head_l * 0.30;
        Self {
            w,
            h,
            body_len,
            body_w,
            body_h,
            leg_h,
            body_y,
            torso_pivot: [0.0, leg_h + body_h * 0.75, 0.0],
            head_w,
            head_h,
            head_l,
            neck_len,
            head_y,
            head_z,
            head_pivot: [0.0, head_y - head_h * 0.30, head_z + head_l * 0.35],
        }
    }
}

/// Torso barrel plus the feature-driven body additions: wool fleece, turtle
/// shell, camelid hump, and cow udder.
fn add_quadruped_body(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    features: QuadrupedFeatures,
    dims: &QuadrupedDims,
) {
    use EntityRigPart::*;
    let (w, h) = (dims.w, dims.h);
    let (body_len, body_w, body_h) = (dims.body_len, dims.body_w, dims.body_h);
    let (leg_h, body_y, torso_pivot) = (dims.leg_h, dims.body_y, dims.torso_pivot);
    add(mesh, spec, pose, Torso, [0.0, body_y, 0.0], [body_w, body_h, body_len], torso_pivot,
        [0.0; 3], 0.0);
    if features.wool {
        add(mesh, spec, pose, Shell, [0.0, body_y + body_h * 0.04, 0.0], [body_w, body_h, body_len],
            torso_pivot, [0.0; 3], w * 0.035);
    }
    if features.shell {
        add_tinted(mesh, spec, pose, Shell, [0.0, body_y + body_h * 0.25, 0.0],
            [body_w * 1.04, body_h * 0.55, body_len * 0.94], torso_pivot, [0.0; 3], 0.0, 31);
    }
    if features.hump {
        add(mesh, spec, pose, Torso, [0.0, body_y + body_h * 0.54, body_len * 0.02],
            [body_w * 0.58, body_h * 0.55, body_len * 0.38], torso_pivot, [0.0; 3], 0.0);
    }
    if features.udder {
        add_tinted(mesh, spec, pose, Torso, [0.0, leg_h * 0.88, body_len * 0.18],
            [body_w * 0.34, h * 0.12, body_len * 0.20], torso_pivot, [0.0; 3], 0.0, 32);
    }
}

/// Neck column, skull, snout, ears, horns, beard, and mane.
fn add_quadruped_neck_head(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    features: QuadrupedFeatures,
    dims: &QuadrupedDims,
) {
    use EntityRigPart::*;
    let (body_len, body_w, body_h) = (dims.body_len, dims.body_w, dims.body_h);
    let (body_y, torso_pivot) = (dims.body_y, dims.torso_pivot);
    let (head_w, head_h, head_l) = (dims.head_w, dims.head_h, dims.head_l);
    let (neck_len, head_y, head_z, head_pivot) =
        (dims.neck_len, dims.head_y, dims.head_z, dims.head_pivot);
    if features.long_neck {
        add(mesh, spec, pose, Torso, [0.0, body_y + neck_len * 0.38, -body_len * 0.44],
            [body_w * 0.38, neck_len + head_h * 0.35, body_w * 0.34],
            [0.0, body_y, -body_len * 0.34], [-0.25, 0.0, 0.0], 0.0);
    }
    add(mesh, spec, pose, Head, [0.0, head_y, head_z], [head_w, head_h, head_l], head_pivot,
        [0.0; 3], 0.0);
    if features.snout {
        add(mesh, spec, pose, Head, [0.0, head_y - head_h * 0.12, head_z - head_l * 0.58],
            [head_w * 0.62, head_h * 0.55, head_l * 0.58], head_pivot, [0.0; 3], 0.0);
    }
    if features.pointed_ears || features.long_ears {
        let ear_len = if features.long_ears { 1.9 } else { 1.0 };
        for side in [-1.0_f32, 1.0] {
            add(mesh, spec, pose, Head,
                [ side * head_w * 0.32, head_y + head_h * (0.62 + ear_len * 0.20), head_z + head_l * 0.05, ],
                [head_w * 0.16, head_h * 0.52 * ear_len, head_l * 0.12], head_pivot,
                [0.08, 0.0, -side * 0.28], 0.0);
        }
    }
    if features.horns {
        for side in [-1.0_f32, 1.0] {
            add_tinted(mesh, spec, pose, Head,
                [ side * head_w * 0.42, head_y + head_h * 0.58, head_z + head_l * 0.02, ],
                [head_w * 0.13, head_h * 0.55, head_l * 0.13], head_pivot,
                [-0.30, 0.0, side * 0.42], 0.0, 33);
        }
    }
    if features.bearded {
        add(mesh, spec, pose, Head, [0.0, head_y - head_h * 0.52, head_z - head_l * 0.35],
            [head_w * 0.24, head_h * 0.35, head_l * 0.18], head_pivot, [0.18, 0.0, 0.0], 0.0);
    }
    if features.mane {
        for i in 0..3 {
            let f = i as f32;
            add_tinted(mesh, spec, pose, Torso,
                [ 0.0, body_y + body_h * (0.48 + f * 0.06), -body_len * (0.18 + f * 0.10), ],
                [body_w * 0.22, body_h * 0.28, body_len * 0.16], torso_pivot, [-0.22, 0.0, 0.0],
                0.0, 34);
        }
    }
}

/// Legs in diagonal gait. Six-legged species insert a middle row; the part
/// binding still uses left/right so walk articulation stays coherent.
fn add_quadruped_legs(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    features: QuadrupedFeatures,
    dims: &QuadrupedDims,
) {
    use EntityRigPart::*;
    let (w, body_len, body_w, leg_h) = (dims.w, dims.body_len, dims.body_w, dims.leg_h);
    let rows: &[f32] = if features.six_legs {
        &[-0.34, 0.0, 0.34]
    } else {
        &[-0.32, 0.34]
    };
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for (row_index, z_factor) in rows.iter().copied().enumerate() {
            let z = z_factor * body_len;
            let pivot = [side * body_w * 0.34, leg_h, z];
            let diagonal_phase = if (row_index + usize::from(side > 0.0)) % 2 == 0 {
                0.0
            } else {
                PI
            };
            let gait =
                (pose.base.limb_swing + diagonal_phase).cos() * pose.base.limb_swing_amount * 0.28;
            add(mesh, spec, pose, part, [pivot[0], leg_h * 0.50, z],
                [ w * if features.stocky { 0.22 } else { 0.16 }, leg_h, w * 0.18, ], pivot,
                [gait, 0.0, 0.0], 0.0);
        }
    }
}

/// Tails: bushy tails are thick and raised, long tails are thin whips, and
/// otherwise a short rear nub keeps the silhouette readable.
fn add_quadruped_tail(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    features: QuadrupedFeatures,
    dims: &QuadrupedDims,
) {
    use EntityRigPart::*;
    let (body_len, body_w, body_h) = (dims.body_len, dims.body_w, dims.body_h);
    let body_y = dims.body_y;
    let tail_pivot = [0.0, body_y + body_h * 0.12, body_len * 0.52];
    if features.bushy_tail {
        add(mesh, spec, pose, Tail, [0.0, body_y + body_h * 0.24, body_len * 0.66],
            [body_w * 0.25, body_h * 0.28, body_len * 0.34], tail_pivot, [-0.65, 0.0, 0.0], 0.0);
        add(mesh, spec, pose, Tail, [0.0, body_y + body_h * 0.46, body_len * 0.78],
            [body_w * 0.18, body_h * 0.22, body_len * 0.24], tail_pivot, [-0.95, 0.0, 0.0], 0.0);
    } else if features.long_tail {
        add(mesh, spec, pose, Tail, [0.0, body_y, body_len * 0.69],
            [body_w * 0.10, body_h * 0.12, body_len * 0.42], tail_pivot, [-0.30, 0.0, 0.0], 0.0);
    } else {
        add(mesh, spec, pose, Tail, [0.0, body_y + body_h * 0.08, body_len * 0.57],
            [body_w * 0.12, body_h * 0.13, body_len * 0.18], tail_pivot, [-0.30, 0.0, 0.0], 0.0);
    }
}

/// Feature extras: mooshroom caps and the armor shell overlay.
fn add_quadruped_extras(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    features: QuadrupedFeatures,
    dims: &QuadrupedDims,
) {
    use EntityRigPart::*;
    let (w, body_len, body_w, body_h) = (dims.w, dims.body_len, dims.body_w, dims.body_h);
    let (body_y, torso_pivot) = (dims.body_y, dims.torso_pivot);
    if features.mushrooms {
        for (x, z) in [(-0.24_f32, -0.08_f32), (0.18, 0.10), (0.0, 0.30)] {
            add_tinted(mesh, spec, pose, Torso, [x * body_w, body_y + body_h * 0.62, z * body_len],
                [body_w * 0.20, body_h * 0.16, body_len * 0.15], torso_pivot, [0.0; 3], 0.0, 35);
        }
    }
    if pose.armor_visible {
        add_tinted(mesh, spec, pose, Shell, [0.0, body_y + body_h * 0.03, 0.0],
            [body_w, body_h, body_len], torso_pivot, [0.0; 3], w * 0.025, 20);
    }
}

/// Parameterized articulated quadruped used by horses, camelids, cattle,
/// pigs, sheep, canines/felines, bears, goats, hoglins, and sniffer-like
/// mammals. Dimensions derive from the registry collision box while anatomy
/// remains independently proportioned rather than a uniform cube scale.
fn add_quadruped_rig(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    features: QuadrupedFeatures,
) {
    let dims = QuadrupedDims::new(spec, features);
    add_quadruped_body(mesh, spec, pose, features, &dims);
    add_quadruped_neck_head(mesh, spec, pose, features, &dims);
    add_quadruped_legs(mesh, spec, pose, features, &dims);
    add_quadruped_tail(mesh, spec, pose, features, &dims);
    add_quadruped_extras(mesh, spec, pose, features, &dims);
}

/// Armadillo (id 2), including the rolled-ball defensive pose. Walking form
/// is a low shell-plated quadruped; rolled form retracts the head, legs, and
/// tail into a banded ball.
fn add_vanilla_armadillo(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    if pose.rolled_up {
        add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.48, 0.0], [w * 0.88, h * 0.82, w * 0.88],
            [0.0; 3], [0.0, 0.35, 0.0], 0.0, 31);
        for i in 0..3 {
            let f = i as f32 - 1.0;
            add_tinted(mesh, spec, pose, Shell, [0.0, h * (0.48 + f * 0.16), -w * 0.03],
                [w * 0.92, h * 0.055, w * 0.92], [0.0; 3], [0.0, f * 0.4, 0.0], 0.0, 36);
        }
        return;
    }
    add_quadruped_rig(
        mesh,
        spec,
        pose,
        QuadrupedFeatures {
            snout: true,
            pointed_ears: true,
            shell: true,
            long_tail: true,
            slim: true,
            ..QuadrupedFeatures::default()
        },
    );
    // Shell bands across the dome.
    for i in 0..4 {
        let f = i as f32 - 1.5;
        add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.68, f * w * 0.17],
            [w * 0.66, h * 0.055, w * 0.045], [0.0, h * 0.55, 0.0], [0.0; 3], 0.0, 36);
    }
}

/// Frog (id 43): wide head/jaw, squat torso, long folded hind legs, and
/// webbed feet.
fn add_vanilla_frog(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.42, w * 0.05], [w * 0.72, h * 0.38, w * 0.62],
        [0.0, h * 0.50, 0.0], [0.0; 3], 0.0);
    let head_pivot = [0.0, h * 0.58, -w * 0.18];
    add(mesh, spec, pose, Head, [0.0, h * 0.61, -w * 0.28], [w * 0.82, h * 0.30, w * 0.42],
        head_pivot, [0.0; 3], 0.0);
    let jaw = pose.open_amount * 0.5 + (pose.base.swing_progress * PI).sin() * 0.2;
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.50, -w * 0.43], [w * 0.74, h * 0.08, w * 0.38],
        head_pivot, [jaw, 0.0, 0.0], 0.0, 37);
    for side in [-1.0_f32, 1.0] {
        add_tinted(mesh, spec, pose, Head, [side * w * 0.24, h * 0.80, -w * 0.31],
            [w * 0.14, h * 0.13, w * 0.14], head_pivot, [0.0; 3], 0.0, 38);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for z in [-0.20_f32, 0.22] {
            let pivot = [side * w * 0.31, h * 0.34, z * w];
            add(mesh, spec, pose, part, [side * w * 0.42, h * 0.22, z * w],
                [w * 0.28, h * 0.18, w * 0.18], pivot, [0.0, side * 0.35, side * 0.20], 0.0);
            add_tinted(mesh, spec, pose, part, [side * w * 0.52, h * 0.08, z * w - w * 0.04],
                [w * 0.34, h * 0.055, w * 0.27], pivot, [0.0, side * 0.18, 0.0], 0.0, 39);
        }
    }
}

/// Turtle (id 111): broad domed shell, small head, and four flippers.
fn add_vanilla_turtle(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Shell, [0.0, h * 0.45, 0.0], [w * 0.88, h * 0.48, w * 0.78],
        [0.0, h * 0.36, 0.0], [0.0; 3], 0.0);
    add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.68, 0.0], [w * 0.68, h * 0.18, w * 0.60],
        [0.0, h * 0.42, 0.0], [0.0; 3], 0.0, 31);
    add(mesh, spec, pose, Head, [0.0, h * 0.42, -w * 0.50], [w * 0.24, h * 0.23, w * 0.28],
        [0.0, h * 0.40, -w * 0.34], [0.0; 3], 0.0);
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for (z, front) in [(-0.24_f32, true), (0.24, false)] {
            let pivot = [side * w * 0.38, h * 0.30, z * w];
            add(mesh, spec, pose, part, [side * w * 0.52, h * 0.20, z * w],
                [w * if front { 0.36 } else { 0.28 }, h * 0.08, w * 0.24], pivot,
                [0.0, side * if front { 0.30 } else { -0.20 }, 0.0], 0.0);
        }
    }
}

/// Ravager (id 85): huge bull-like head with horns, heavy jaw, armored torso,
/// and four thick legs.
fn add_vanilla_ravager(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.57, w * 0.08], [w * 0.65, h * 0.38, w * 0.72],
        [0.0, h * 0.66, 0.0], [0.0; 3], 0.0);
    let head_pivot = [0.0, h * 0.70, -w * 0.28];
    add(mesh, spec, pose, Head, [0.0, h * 0.72, -w * 0.46], [w * 0.55, h * 0.32, w * 0.36],
        head_pivot, [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.63, -w * 0.68], [w * 0.43, h * 0.15, w * 0.25],
        head_pivot, [pose.open_amount * 0.35, 0.0, 0.0], 0.0);
    for side in [-1.0_f32, 1.0] {
        add_tinted(mesh, spec, pose, Head, [side * w * 0.34, h * 0.84, -w * 0.40],
            [w * 0.10, h * 0.20, w * 0.10], head_pivot, [-0.45, 0.0, side * 0.55], 0.0, 33);
        for z in [-0.22_f32, 0.28] {
            let pivot = [side * w * 0.25, h * 0.40, z * w];
            add(mesh, spec, pose, if side < 0.0 { LeftLeg } else { RightLeg },
                [pivot[0], h * 0.20, pivot[2]], [w * 0.18, h * 0.40, w * 0.18], pivot, [0.0; 3],
                0.0);
        }
    }
}

/// Sniffer (id 95): long low torso, very wide sniffing snout, ear/frill fins,
/// six short legs, and a mossy dorsal ridge.
fn add_vanilla_sniffer(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    add_quadruped_rig(
        mesh,
        spec,
        pose,
        QuadrupedFeatures {
            snout: true,
            long_tail: true,
            six_legs: true,
            stocky: true,
            ..QuadrupedFeatures::default()
        },
    );
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    // Broad nose pad and two side frills are the clearest sniffer silhouette.
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.72, -w * 0.67], [w * 0.40, h * 0.10, w * 0.17],
        [0.0, h * 0.66, -w * 0.25], [0.0; 3], 0.0, 40);
    for side in [-1.0_f32, 1.0] {
        add_tinted(mesh, spec, pose, Head, [side * w * 0.30, h * 0.83, -w * 0.30],
            [w * 0.18, h * 0.06, w * 0.28], [0.0, h * 0.72, -w * 0.18],
            [0.0, side * 0.35, side * 0.35], 0.0, 41);
    }
    for i in 0..4 {
        let f = i as f32;
        add_tinted(mesh, spec, pose, Torso, [0.0, h * (0.82 + f * 0.015), w * (-0.22 + f * 0.15)],
            [w * 0.12, h * 0.12, w * 0.10], [0.0, h * 0.70, 0.0], [0.0; 3], 0.0, 42);
    }
}

/// Rabbit (id 84): compact body, long upright ears, powerful hind legs, small
/// forelegs, and a round tail nub.
fn add_vanilla_rabbit(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.43, w * 0.03], [w * 0.62, h * 0.46, w * 0.70],
        [0.0, h * 0.52, 0.0], [0.0; 3], 0.0);
    let head_pivot = [0.0, h * 0.60, -w * 0.25];
    add(mesh, spec, pose, Head, [0.0, h * 0.67, -w * 0.35], [w * 0.52, h * 0.38, w * 0.42],
        head_pivot, [0.0; 3], 0.0);
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Head, [side * w * 0.15, h * 1.08, -w * 0.34],
            [w * 0.13, h * 0.70, w * 0.09], head_pivot, [0.10, 0.0, -side * 0.12], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        add(mesh, spec, pose, part, [side * w * 0.22, h * 0.16, w * 0.20],
            [w * 0.20, h * 0.30, w * 0.28], [side * w * 0.20, h * 0.36, w * 0.18], [0.0; 3], 0.0);
        add(mesh, spec, pose, part, [side * w * 0.19, h * 0.14, -w * 0.21],
            [w * 0.13, h * 0.25, w * 0.13], [side * w * 0.18, h * 0.34, -w * 0.20], [0.0; 3], 0.0);
    }
    add(mesh, spec, pose, Tail, [0.0, h * 0.43, w * 0.43], [w * 0.20, h * 0.18, w * 0.16],
        [0.0, h * 0.43, w * 0.30], [0.0; 3], 0.0);
}

/// Spider/cave spider: cephalothorax, abdomen, eye cluster, and eight long
/// jointed legs with alternating lift phase.
fn add_vanilla_spider(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.58, w * 0.18], [w * 0.52, h * 0.48, w * 0.48],
        [0.0, h * 0.55, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Torso, [0.0, h * 0.52, -w * 0.17], [w * 0.44, h * 0.36, w * 0.36],
        [0.0, h * 0.55, 0.0], [0.0; 3], 0.0);
    for side in [-1.0_f32, 1.0] {
        for eye in 0..3 {
            add_tinted(mesh, spec, pose, Head,
                [ side * w * (0.06 + eye as f32 * 0.055), h * (0.58 - eye as f32 * 0.04), -w * 0.37, ],
                [w * 0.045, h * 0.045, w * 0.025], [0.0, h * 0.54, -w * 0.20], [0.0; 3], 0.0, 43);
        }
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for index in 0..4 {
            let f = index as f32;
            let z = (f - 1.5) * w * 0.18;
            let pivot = [side * w * 0.22, h * 0.52, z];
            let lift = (pose.base.limb_swing + f * FRAC_PI_2 + if side < 0.0 { 0.0 } else { PI })
                .cos()
                * pose.base.limb_swing_amount
                * 0.18;
            add(mesh, spec, pose, part, [side * w * 0.43, h * 0.43, z],
                [w * 0.48, h * 0.09, w * 0.07], pivot, [lift, (f - 1.5) * 0.14, side * 0.42], 0.0);
            add(mesh, spec, pose, part, [side * w * 0.65, h * 0.25, z],
                [w * 0.08, h * 0.38, w * 0.06], pivot, [lift, (f - 1.5) * 0.14, side * 0.15], 0.0);
        }
    }
}

/// Silverfish/endermite segmented crawler with tiny side feet.
fn add_vanilla_segmented_pest(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    for i in 0..4 {
        let f = i as f32;
        let z = (f - 1.5) * w * 0.22;
        let wave = (pose.base.age_ticks * 0.8 + pose.base.limb_swing - f * 0.7).sin() * 0.06;
        add(mesh, spec, pose, if i == 0 { Head } else { Core }, [wave * w, h * 0.50, z],
            [w * (0.52 - f * 0.04), h * (0.55 - f * 0.03), w * 0.24], [0.0, h * 0.50, 0.0],
            [0.0, wave, 0.0], 0.0);
        for side in [-1.0_f32, 1.0] {
            add(mesh, spec, pose, if side < 0.0 { LeftLeg } else { RightLeg },
                [side * w * 0.34, h * 0.26, z], [w * 0.18, h * 0.08, w * 0.055],
                [side * w * 0.22, h * 0.38, z], [0.0, 0.0, side * 0.28], 0.0);
        }
    }
}

/// Bee (id 7): striped plump body, two independent wings, antennae, legs,
/// and rear stinger.
fn add_vanilla_bee(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.50, 0.0], [w * 0.68, h * 0.58, w * 0.82],
        [0.0, h * 0.50, 0.0], [0.0; 3], 0.0);
    for z in [-0.22_f32, 0.12, 0.38] {
        add_tinted(mesh, spec, pose, Torso, [0.0, h * 0.50, z * w], [w * 0.70, h * 0.60, w * 0.10],
            [0.0, h * 0.50, 0.0], [0.0; 3], 0.0, 44);
    }
    add(mesh, spec, pose, Head, [0.0, h * 0.55, -w * 0.46], [w * 0.46, h * 0.42, w * 0.26],
        [0.0, h * 0.50, -w * 0.30], [0.0; 3], 0.0);
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.18, h * 0.76, -w * 0.02];
        add(mesh, spec, pose, part, [side * w * 0.43, h * 0.78, w * 0.06],
            [w * 0.48, h * 0.045, w * 0.40], pivot, [0.0, -side * 0.25, 0.0], 0.0);
    }
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Head, [side * w * 0.15, h * 0.84, -w * 0.54],
            [w * 0.045, h * 0.22, w * 0.045], [0.0, h * 0.62, -w * 0.44], [0.45, 0.0, -side * 0.35],
            0.0);
        for z in [-0.18_f32, 0.15] {
            add(mesh, spec, pose, if side < 0.0 { LeftLeg } else { RightLeg },
                [side * w * 0.25, h * 0.18, z * w], [w * 0.06, h * 0.26, w * 0.06],
                [side * w * 0.20, h * 0.36, z * w], [0.0; 3], 0.0);
        }
    }
    add_tinted(mesh, spec, pose, Tail, [0.0, h * 0.50, w * 0.52], [w * 0.10, h * 0.10, w * 0.22],
        [0.0, h * 0.50, w * 0.38], [0.0; 3], 0.0, 45);
}

/// Bat (id 6): small body/head, tall ears, and wide membraned wings.
fn add_vanilla_bat(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.45, 0.0], [w * 0.42, h * 0.48, w * 0.36],
        [0.0, h * 0.55, 0.0], [0.0; 3], 0.0);
    let head_pivot = [0.0, h * 0.67, -w * 0.03];
    add(mesh, spec, pose, Head, [0.0, h * 0.73, -w * 0.04], [w * 0.44, h * 0.28, w * 0.38],
        head_pivot, [0.0; 3], 0.0);
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Head, [side * w * 0.16, h * 0.94, -w * 0.03],
            [w * 0.10, h * 0.28, w * 0.08], head_pivot, [0.0, 0.0, -side * 0.20], 0.0);
    }
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.18, h * 0.57, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.67, h * 0.52, 0.0],
            [w * 0.92, h * 0.07, w * 0.52], pivot, [0.0, -side * 0.12, 0.0], 0.0);
        add(mesh, spec, pose, part, [side * w * 0.82, h * 0.39, w * 0.16],
            [w * 0.55, h * 0.055, w * 0.32], pivot, [0.0, -side * 0.34, 0.0], 0.0);
    }
}

/// Small bird body shared by chicken/parrot. `long_tail` selects the parrot
/// silhouette; chickens receive comb, wattles, and a shorter fan tail.
fn add_vanilla_bird(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    long_tail: bool,
) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.43, 0.0], [w * 0.62, h * 0.48, w * 0.62],
        [0.0, h * 0.60, 0.0], [0.0; 3], 0.0);
    let head_pivot = [0.0, h * 0.68, -w * 0.18];
    add(mesh, spec, pose, Head, [0.0, h * 0.75, -w * 0.25], [w * 0.46, h * 0.34, w * 0.42],
        head_pivot, [0.0; 3], 0.0);
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.72, -w * 0.55], [w * 0.22, h * 0.12, w * 0.22],
        head_pivot, [0.0; 3], 0.0, 46);
    if long_tail {
        add(mesh, spec, pose, Tail, [0.0, h * 0.42, w * 0.52], [w * 0.26, h * 0.10, w * 0.62],
            [0.0, h * 0.43, w * 0.28], [-0.28, 0.0, 0.0], 0.0);
    } else {
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.96, -w * 0.23],
            [w * 0.14, h * 0.17, w * 0.09], head_pivot, [0.0; 3], 0.0, 47);
        add_tinted(mesh, spec, pose, Head, [0.0, h * 0.57, -w * 0.47],
            [w * 0.14, h * 0.18, w * 0.08], head_pivot, [0.0; 3], 0.0, 47);
        for side in [-1.0_f32, 1.0] {
            add(mesh, spec, pose, Tail, [side * w * 0.12, h * 0.55, w * 0.44],
                [w * 0.14, h * 0.28, w * 0.34], [0.0, h * 0.45, w * 0.28],
                [-0.55, side * 0.18, 0.0], 0.0);
        }
    }
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.29, h * 0.55, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.42, h * 0.44, 0.0],
            [w * 0.18, h * 0.38, w * 0.52], pivot, [0.0; 3], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        add(mesh, spec, pose, part, [side * w * 0.16, h * 0.12, -w * 0.02],
            [w * 0.07, h * 0.24, w * 0.07], [side * w * 0.16, h * 0.25, 0.0], [0.0; 3], 0.0);
    }
}

/// Allay/vex fairy-like body with glowing or dark wing blades and small arms.
fn add_vanilla_spirit(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let hostile = spec.entity_type_id == 112;
    add(mesh, spec, pose, Torso, [0.0, h * 0.43, 0.0], [w * 0.55, h * 0.42, w * 0.42],
        [0.0, h * 0.62, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.79, -w * 0.02], [w * 0.62, h * 0.32, w * 0.56],
        [0.0, h * 0.62, 0.0], [0.0; 3], 0.0);
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        add(mesh, spec, pose, part, [side * w * 0.36, h * 0.42, -w * 0.03],
            [w * 0.10, h * 0.38, w * 0.10], [side * w * 0.30, h * 0.62, 0.0], [0.0; 3], 0.0);
    }
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.18, h * 0.60, w * 0.14];
        add_tinted(mesh, spec, pose, part, [side * w * 0.48, h * 0.64, w * 0.20],
            [w * 0.55, h * 0.055, w * 0.36], pivot, [0.0, -side * 0.28, 0.0], 0.0,
            if hostile { 48 } else { 49 });
    }
    if hostile {
        add_tinted(mesh, spec, pose, RightArm, [w * 0.45, h * 0.30, -w * 0.18],
            [w * 0.07, h * 0.38, w * 0.07], [w * 0.30, h * 0.62, 0.0], [-0.55, 0.0, 0.12], 0.0, 18);
    }
}

/// Generic fish body for cod/salmon/tropical fish, with tail, dorsal, and
/// pectoral fins. Width/height differences in the registry preserve species.
fn add_vanilla_fish(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.52, -w * 0.03], [w * 0.46, h * 0.56, w * 0.82],
        [0.0, h * 0.52, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.53, -w * 0.43], [w * 0.38, h * 0.44, w * 0.24],
        [0.0, h * 0.52, -w * 0.25], [0.0; 3], 0.0);
    let tail_wave = (pose.base.age_ticks * 0.8 + pose.base.limb_swing).sin() * 0.35;
    add(mesh, spec, pose, Tail, [0.0, h * 0.52, w * 0.55], [w * 0.10, h * 0.52, w * 0.32],
        [0.0, h * 0.52, w * 0.34], [0.0, tail_wave, 0.0], 0.0);
    add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.84, -w * 0.02], [w * 0.08, h * 0.24, w * 0.34],
        [0.0, h * 0.62, 0.0], [0.0; 3], 0.0, 50);
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, if side < 0.0 { LeftWing } else { RightWing },
            [side * w * 0.28, h * 0.43, -w * 0.08], [w * 0.18, h * 0.06, w * 0.24],
            [side * w * 0.20, h * 0.48, -w * 0.04], [0.0, side * 0.35, side * 0.30], 0.0);
    }
}

/// Dolphin (id 24): elongated torso, beak snout, dorsal fin, side flippers,
/// and horizontal tail fluke.
fn add_vanilla_dolphin(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.52, 0.0], [w * 0.46, h * 0.55, w * 1.05],
        [0.0, h * 0.52, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.55, -w * 0.58], [w * 0.42, h * 0.45, w * 0.30],
        [0.0, h * 0.54, -w * 0.40], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.49, -w * 0.83], [w * 0.20, h * 0.16, w * 0.34],
        [0.0, h * 0.54, -w * 0.40], [0.0; 3], 0.0);
    add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.86, -w * 0.02], [w * 0.10, h * 0.34, w * 0.30],
        [0.0, h * 0.62, 0.0], [-0.20, 0.0, 0.0], 0.0, 50);
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, if side < 0.0 { LeftWing } else { RightWing },
            [side * w * 0.34, h * 0.32, -w * 0.12], [w * 0.28, h * 0.07, w * 0.34],
            [side * w * 0.20, h * 0.43, -w * 0.10], [0.0, side * 0.25, side * 0.40], 0.0);
    }
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Tail, [side * w * 0.17, h * 0.52, w * 0.65],
            [w * 0.32, h * 0.07, w * 0.32], [0.0, h * 0.52, w * 0.48], [0.0, side * 0.35, 0.0],
            0.0);
    }
}

/// Axolotl (id 5): broad smiling head, three gill frills per side, small legs,
/// and a vertical fin tail.
fn add_vanilla_axolotl(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.43, w * 0.06], [w * 0.54, h * 0.42, w * 0.70],
        [0.0, h * 0.46, 0.0], [0.0; 3], 0.0);
    let head_pivot = [0.0, h * 0.50, -w * 0.25];
    add(mesh, spec, pose, Head, [0.0, h * 0.52, -w * 0.38], [w * 0.62, h * 0.38, w * 0.36],
        head_pivot, [0.0; 3], 0.0);
    for side in [-1.0_f32, 1.0] {
        for i in 0..3 {
            let f = i as f32 - 1.0;
            add_tinted(mesh, spec, pose, Head, [side * w * 0.40, h * (0.52 + f * 0.15), -w * 0.30],
                [w * 0.18, h * 0.055, w * 0.10], head_pivot,
                [0.0, side * 0.25, side * (0.35 + f * 0.12)], 0.0, 51);
        }
        for z in [-0.18_f32, 0.22] {
            add(mesh, spec, pose, if side < 0.0 { LeftLeg } else { RightLeg },
                [side * w * 0.28, h * 0.16, z * w], [w * 0.12, h * 0.25, w * 0.10],
                [side * w * 0.22, h * 0.30, z * w], [0.0; 3], 0.0);
        }
    }
    add(mesh, spec, pose, Tail, [0.0, h * 0.44, w * 0.52], [w * 0.12, h * 0.38, w * 0.42],
        [0.0, h * 0.45, w * 0.34], [0.0; 3], 0.0);
}

/// Squid/glow squid: mantle/head and eight individually waving tentacles.
fn add_vanilla_squid(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Head, [0.0, h * 0.68, -w * 0.04], [w * 0.72, h * 0.55, w * 0.72],
        [0.0, h * 0.62, 0.0], [0.0; 3], 0.0);
    for i in 0..8 {
        let f = i as f32;
        let angle = f * TAU / 8.0;
        let x = angle.cos() * w * 0.20;
        let z = angle.sin() * w * 0.20;
        let wave = (pose.base.age_ticks * 0.65 + f * 0.7).sin() * 0.22;
        add(mesh, spec, pose, Tail, [x + wave * w * 0.05, h * 0.23, z + wave * w * 0.03],
            [w * 0.10, h * 0.48, w * 0.10], [x, h * 0.43, z], [wave * 0.45, angle, -wave * 0.25],
            0.0);
    }
}

/// Pufferfish: cubic body, fins, tail, and retractable radial spikes controlled
/// by `swell_amount`.
fn add_vanilla_pufferfish(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let inflate = 1.0 + pose.swell_amount * 0.35;
    add(mesh, spec, pose, Core, [0.0, h * 0.50, 0.0],
        [w * 0.58 * inflate, h * 0.58 * inflate, w * 0.58 * inflate], [0.0, h * 0.50, 0.0],
        [0.0; 3], 0.0);
    for i in 0..12 {
        let f = i as f32;
        let angle = f * TAU / 12.0;
        let x = angle.cos() * w * 0.34 * inflate;
        let z = angle.sin() * w * 0.34 * inflate;
        add_tinted(mesh, spec, pose, Shell, [x, h * (0.50 + (i % 3) as f32 * 0.10 - 0.10), z],
            [w * 0.045, h * 0.17 * (0.6 + pose.swell_amount), w * 0.045],
            [x * 0.75, h * 0.50, z * 0.75], [0.45 * z.signum(), angle, -0.45 * x.signum()], 0.0,
            52);
    }
    add(mesh, spec, pose, Tail, [0.0, h * 0.50, w * 0.48], [w * 0.10, h * 0.34, w * 0.24],
        [0.0, h * 0.50, w * 0.32], [0.0; 3], 0.0);
}

/// Tadpole (id 104): tiny round body and large flexible tail fin.
fn add_vanilla_tadpole(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Head, [0.0, h * 0.52, -w * 0.15], [w * 0.55, h * 0.55, w * 0.55],
        [0.0, h * 0.50, 0.0], [0.0; 3], 0.0);
    let wave = (pose.base.age_ticks * 0.9 + pose.base.limb_swing).sin() * 0.4;
    add(mesh, spec, pose, Tail, [0.0, h * 0.50, w * 0.34], [w * 0.16, h * 0.36, w * 0.65],
        [0.0, h * 0.50, w * 0.05], [0.0, wave, 0.0], 0.0);
}

/// Strider (id 103): broad flat head/body, bristle crown, and two very tall
/// thin legs for lava walking.
fn add_vanilla_strider(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.72, 0.0], [w * 0.82, h * 0.24, w * 0.72],
        [0.0, h * 0.76, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.69, -w * 0.32], [w * 0.64, h * 0.20, w * 0.26],
        [0.0, h * 0.72, -w * 0.15], [0.0; 3], 0.0);
    for i in 0..7 {
        let f = i as f32 - 3.0;
        add_tinted(mesh, spec, pose, Shell,
            [f * w * 0.09, h * 0.89 + f.abs() * -h * 0.015, w * 0.02],
            [w * 0.045, h * 0.18, w * 0.045], [0.0, h * 0.78, 0.0], [0.15, 0.0, f * 0.08], 0.0, 53);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.18, h * 0.62, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.20, h * 0.31, 0.0],
            [w * 0.10, h * 0.62, w * 0.10], pivot, [0.0; 3], 0.0);
    }
}

/// Emit the twelve thin edges of a rectangular frame. Every edge shares the
/// same pivot/rotation, so the whole cage can rotate as one rigid object.
#[allow(clippy::too_many_arguments)]
fn add_cube_frame(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    tint: i32,
) {
    use EntityRigPart::Shell;
    let sx = size[0];
    let sy = size[1];
    let sz = size[2];
    let t = sx.min(sy).min(sz) * 0.055;
    for y in [-0.5_f32, 0.5] {
        for z in [-0.5_f32, 0.5] {
            add_tinted(mesh, spec, pose, Shell, [center[0], center[1] + y * sy, center[2] + z * sz],
                [sx, t, t], pivot, rotation, 0.0, tint);
        }
    }
    for x in [-0.5_f32, 0.5] {
        for z in [-0.5_f32, 0.5] {
            add_tinted(mesh, spec, pose, Shell, [center[0] + x * sx, center[1], center[2] + z * sz],
                [t, sy, t], pivot, rotation, 0.0, tint);
        }
    }
    for x in [-0.5_f32, 0.5] {
        for y in [-0.5_f32, 0.5] {
            add_tinted(mesh, spec, pose, Shell, [center[0] + x * sx, center[1] + y * sy, center[2]],
                [t, t, sz], pivot, rotation, 0.0, tint);
        }
    }
}

/// Boat/chest boat: open hull with bottom, side walls, bow/stern transoms,
/// seat slats, paddles, and an optional centered chest.
fn add_vanilla_boat(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.16, 0.0], [w * 0.78, h * 0.16, w * 0.82], [0.0; 3],
        [0.0; 3], 0.0);
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Shell, [side * w * 0.43, h * 0.43, 0.0],
            [w * 0.10, h * 0.52, w * 0.84], [0.0; 3], [0.0, 0.0, -side * 0.12], 0.0);
        add(mesh, spec, pose, Shell, [side * w * 0.28, h * 0.49, -w * 0.43],
            [w * 0.34, h * 0.45, w * 0.10], [0.0; 3], [0.0, side * 0.35, 0.0], 0.0);
        // Paddles stowed along the gunwales.
        add_tinted(mesh, spec, pose, Other, [side * w * 0.48, h * 0.70, w * 0.06],
            [w * 0.055, h * 0.08, w * 0.62], [0.0; 3], [0.0, side * 0.08, 0.0], 0.0, 54);
        add_tinted(mesh, spec, pose, Other, [side * w * 0.48, h * 0.70, -w * 0.32],
            [w * 0.16, h * 0.10, w * 0.20], [0.0; 3], [0.0, side * 0.08, 0.0], 0.0, 54);
    }
    add(mesh, spec, pose, Shell, [0.0, h * 0.43, w * 0.43], [w * 0.74, h * 0.48, w * 0.10],
        [0.0; 3], [0.10, 0.0, 0.0], 0.0);
    for z in [-0.16_f32, 0.18] {
        add_tinted(mesh, spec, pose, Other, [0.0, h * 0.48, z * w], [w * 0.64, h * 0.08, w * 0.10],
            [0.0; 3], [0.0; 3], 0.0, 55);
    }
    if spec.entity_type_id == 17 {
        add_tinted(mesh, spec, pose, Core, [0.0, h * 0.72, w * 0.18],
            [w * 0.42, h * 0.48, w * 0.34], [0.0; 3], [0.0; 3], 0.0, 56);
    }
}

/// Minecart tub plus optional chest/furnace/TNT/hopper/spawner/command-block
/// cargo. The tub is five walls: bottom plus four side plates.
fn add_vanilla_minecart(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.18, 0.0], [w * 0.78, h * 0.12, w * 0.70], [0.0; 3],
        [0.0; 3], 0.0);
    for side in [-1.0_f32, 1.0] {
        add(mesh, spec, pose, Shell, [side * w * 0.43, h * 0.43, 0.0],
            [w * 0.10, h * 0.48, w * 0.72], [0.0; 3], [0.0; 3], 0.0);
        add(mesh, spec, pose, Shell, [0.0, h * 0.43, side * w * 0.39],
            [w * 0.78, h * 0.48, w * 0.10], [0.0; 3], [0.0; 3], 0.0);
    }
    let cargo_y = h * 0.72;
    match spec.entity_type_id {
        18 => add_tinted(mesh, spec, pose, Core, [0.0, cargo_y, 0.0], [w * 0.56, h * 0.52, w * 0.48],
            [0.0; 3], [0.0; 3], 0.0, 56),
        21 => add_tinted(mesh, spec, pose, Core, [0.0, cargo_y, 0.0], [w * 0.52, h * 0.48, w * 0.48],
            [0.0; 3], [0.0; 3], 0.0, 57),
        44 => {
            add_tinted(mesh, spec, pose, Core, [0.0, cargo_y, 0.0], [w * 0.54, h * 0.52, w * 0.50],
                [0.0; 3], [0.0; 3], 0.0, 58);
            add_tinted(mesh, spec, pose, Core, [0.0, cargo_y, -w * 0.26],
                [w * 0.30, h * 0.28, w * 0.035], [0.0; 3], [0.0; 3], 0.0, 59);
        }
        52 => {
            for i in 0..3 {
                let f = i as f32;
                add_tinted(mesh, spec, pose, Core, [0.0, h * (0.64 + f * 0.12), 0.0],
                    [w * (0.62 - f * 0.14), h * 0.12, w * (0.56 - f * 0.12)], [0.0; 3], [0.0; 3],
                    0.0, 60);
            }
        }
        98 => {
            add_cube_frame(mesh, spec, pose, [0.0, cargo_y, 0.0], [w * 0.58, h * 0.56, w * 0.52],
                [0.0, cargo_y, 0.0], [0.0; 3], 61);
            add_tinted(mesh, spec, pose, Core, [0.0, cargo_y, 0.0], [w * 0.18, h * 0.18, w * 0.18],
                [0.0, cargo_y, 0.0], [0.0, pose.base.age_ticks * 0.8, 0.0], 0.0, 62);
        }
        107 => {
            add_tinted(mesh, spec, pose, Core, [0.0, cargo_y, 0.0], [w * 0.54, h * 0.52, w * 0.50],
                [0.0; 3], [0.0; 3], 0.0, 63);
            add_tinted(mesh, spec, pose, Core, [0.0, cargo_y + h * 0.04, 0.0],
                [w * 0.56, h * 0.09, w * 0.52], [0.0; 3], [0.0; 3], 0.0, 64);
        }
        _ => {}
    }
}

/// End crystal (id 30): bright inner core inside two tilted counter-rotating
/// glass cube frames.
fn add_vanilla_end_crystal(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let center = [0.0, h * 0.52, 0.0];
    let spin = pose.base.age_ticks * 0.35;
    add_tinted(mesh, spec, pose, Core, center, [w * 0.20, h * 0.20, w * 0.20], center,
        [0.45, spin * 1.7, 0.45], 0.0, 65);
    add_cube_frame(mesh, spec, pose, center, [w * 0.48, h * 0.48, w * 0.48], center,
        [0.35, spin, 0.18], 66);
    add_cube_frame(mesh, spec, pose, center, [w * 0.62, h * 0.62, w * 0.62], center,
        [-0.25, -spin * 0.75, 0.42], 67);
    // Small bedrock-like base fragment keeps the crystal grounded.
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.08, 0.0], [w * 0.34, h * 0.14, w * 0.34],
        [0.0; 3], [0.0; 3], 0.0, 68);
}

/// Arrow/spectral arrow: long shaft, flint head, and three fletching vanes.
fn add_vanilla_arrow(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let glow = spec.entity_type_id == 99;
    let tint = if glow { 69 } else { 70 };
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.50, 0.0], [w * 0.08, h * 0.08, w * 0.92],
        [0.0; 3], [0.0; 3], 0.0, tint);
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.50, -w * 0.49], [w * 0.16, h * 0.16, w * 0.18],
        [0.0; 3], [0.0; 3], 0.0, tint + 1);
    for i in 0..3 {
        let angle = i as f32 * TAU / 3.0;
        add_tinted(mesh, spec, pose, Tail,
            [ angle.cos() * w * 0.08, h * 0.50 + angle.sin() * w * 0.08, w * 0.36, ],
            [w * 0.035, h * 0.15, w * 0.23], [0.0, h * 0.50, w * 0.20], [angle, 0.0, 0.0], 0.0,
            tint + 2);
    }
}

/// Trident (id 109): long shaft, crossbar, and three forward prongs.
fn add_vanilla_trident(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.50, 0.0], [w * 0.08, h * 0.08, w * 1.05],
        [0.0; 3], [0.0; 3], 0.0, 71);
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.50, -w * 0.42], [w * 0.45, h * 0.07, w * 0.08],
        [0.0; 3], [0.0; 3], 0.0, 72);
    for x in [-0.18_f32, 0.0, 0.18] {
        add_tinted(mesh, spec, pose, Core, [x * w, h * 0.50, -w * 0.58],
            [w * 0.065, h * 0.065, w * 0.34], [0.0; 3], [0.0; 3], 0.0, 72);
    }
}

/// Falling block / primed TNT full cube. TNT receives the white center band
/// and black cap strips that make it distinct from a generic block drop.
fn add_vanilla_block_entity(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let spin = if spec.entity_type_id == 106 {
        pose.base.age_ticks * 0.25
    } else {
        0.0
    };
    add(mesh, spec, pose, Core, [0.0, h * 0.50, 0.0], [w * 0.92, h * 0.92, w * 0.92],
        [0.0, h * 0.5, 0.0], [0.0, spin, 0.0], 0.0);
    if spec.entity_type_id == 106 {
        add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.50, 0.0], [w * 0.94, h * 0.18, w * 0.94],
            [0.0, h * 0.5, 0.0], [0.0, spin, 0.0], 0.0, 64);
        add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.90, 0.0], [w * 0.94, h * 0.08, w * 0.94],
            [0.0, h * 0.5, 0.0], [0.0, spin, 0.0], 0.0, 63);
    }
}

/// Firework rocket: paper tube, nose cone approximation, fuse, and fins.
fn add_vanilla_firework(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.45, 0.0], [w * 0.30, h * 0.70, w * 0.30],
        [0.0, h * 0.45, 0.0], [0.35, pose.base.age_ticks * 0.5, 0.0], 0.0);
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.86, 0.0], [w * 0.22, h * 0.18, w * 0.22],
        [0.0, h * 0.70, 0.0], [0.35, 0.0, 0.0], 0.0, 73);
    for side in [-1.0_f32, 1.0] {
        add_tinted(mesh, spec, pose, Tail, [side * w * 0.20, h * 0.19, 0.0],
            [w * 0.16, h * 0.25, w * 0.05], [0.0, h * 0.35, 0.0], [0.25, 0.0, side * 0.35], 0.0,
            74);
    }
}

/// Area effect cloud: nested flat translucent voxel puffs spanning the large
/// effect radius rather than a single invisible collision box.
fn add_vanilla_area_cloud(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height.max(0.5);
    for ring in 0..4 {
        let f = ring as f32;
        let spin =
            pose.base.age_ticks * (0.08 + f * 0.025) * if ring % 2 == 0 { 1.0 } else { -1.0 };
        let radius = w * (0.16 + f * 0.12);
        for i in 0..5 {
            let angle = spin + i as f32 * TAU / 5.0 + f * 0.35;
            add_tinted(mesh, spec, pose, Other,
                [ angle.cos() * radius, h * (0.18 + f * 0.12), angle.sin() * radius, ],
                [w * 0.11, h * 0.10, w * 0.11], [0.0; 3], [0.0, angle, 0.0], 0.0, 75);
        }
    }
}

/// Lightning bolt placeholder: a tall jagged chain of thin bright segments.
/// The registry collision size is zero, so this rig intentionally uses fixed
/// visual units rather than inheriting the clamped 0.125 placeholder size.
fn add_vanilla_lightning(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let visual_h = 8.0_f32;
    for i in 0..7 {
        let f = i as f32;
        let y0 = visual_h * (0.08 + f * 0.13);
        let jitter = ((i as u32 * 37 + 11) % 5) as f32 - 2.0;
        add_tinted(mesh, spec, pose, Other,
            [jitter * 0.07, y0, ((i as u32 * 19) % 3) as f32 * 0.05], [0.10, visual_h * 0.17, 0.10],
            [0.0, y0 - visual_h * 0.06, 0.0], [0.10 * jitter, 0.0, -0.16 * jitter], 0.0, 76);
    }
}

/// Small item/frame/painting/plate rigs used by item frames, glow frames,
/// paintings, text displays, and dropped/display items.
fn add_vanilla_flat_hanging(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width.max(0.5);
    let h = spec.height.max(0.5);
    let painting = spec.entity_type_id == 73;
    let frame_tint = if painting { 77 } else { 78 };
    let width = if painting { w * 1.8 } else { w };
    let height = if painting { h * 1.5 } else { h };
    add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.5, 0.0], [width, height, w * 0.06], [0.0; 3],
        [0.0; 3], 0.0, frame_tint);
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.5, -w * 0.04],
        [width * 0.78, height * 0.78, w * 0.035], [0.0; 3], [0.0; 3], 0.0,
        if painting { 79 } else { 80 });
    if spec.entity_type_id == 47 {
        add_tinted(mesh, spec, pose, Core, [0.0, h * 0.5, -w * 0.07],
            [width * 0.34, height * 0.34, w * 0.025], [0.0; 3],
            [0.0, pose.base.age_ticks * 0.4, 0.0], 0.0, 81);
    }
}

/// Dropped item / item display / generic throwable item placeholder: a small
/// bobbing, rotating item prism with a bright edge cap.
fn add_vanilla_item_entity(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width.max(0.25);
    let h = spec.height.max(0.25);
    let bob = (pose.base.age_ticks * 0.45).sin() * h * 0.08;
    let spin = pose.base.age_ticks * 0.55;
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.55 + bob, 0.0], [w * 0.62, h * 0.20, w * 0.62],
        [0.0, h * 0.55, 0.0], [0.20, spin, 0.0], 0.0, 82);
    add_tinted(mesh, spec, pose, Shell, [0.0, h * 0.58 + bob, 0.0], [w * 0.68, h * 0.055, w * 0.68],
        [0.0, h * 0.55, 0.0], [0.20, spin, 0.0], 0.0, 83);
}

/// Ominous item spawner: rotating tiny cage with a glowing item core.
fn add_vanilla_ominous_spawner(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width.max(0.3125);
    let h = spec.height.max(0.3125);
    let center = [0.0, h * 0.55, 0.0];
    let spin = pose.base.age_ticks * 0.75;
    add_cube_frame(mesh, spec, pose, center, [w, h, w], center, [0.2, spin, 0.0], 84);
    add_tinted(mesh, spec, pose, Core, center, [w * 0.38, h * 0.38, w * 0.38], center,
        [0.0, -spin * 1.3, 0.0], 0.0, 85);
}

/// Wither skull projectile: compact black skull with brow/jaw accents.
fn add_vanilla_wither_skull(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.56, 0.0], [w * 0.86, h * 0.74, w * 0.82],
        [0.0; 3], [0.0, pose.base.age_ticks * 0.7, 0.0], 0.0, 86);
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.45, -w * 0.35], [w * 0.68, h * 0.18, w * 0.18],
        [0.0; 3], [0.0; 3], 0.0, 87);
}

/// Fishing bobber: red/white float and a tiny hook below it.
fn add_vanilla_fishing_bobber(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let bob = (pose.base.age_ticks * 0.6).sin() * h * 0.08;
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.60 + bob, 0.0], [w * 0.55, h * 0.34, w * 0.55],
        [0.0; 3], [0.0; 3], 0.0, 88);
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.83 + bob, 0.0], [w * 0.44, h * 0.16, w * 0.44],
        [0.0; 3], [0.0; 3], 0.0, 89);
    add_tinted(mesh, spec, pose, Tail, [w * 0.10, h * 0.20 + bob, 0.0],
        [w * 0.08, h * 0.42, w * 0.08], [0.0, h * 0.45, 0.0], [0.0, 0.0, 0.45], 0.0, 90);
}

/// Tiny visible placeholder for logical zero-size entities (display entities,
/// interaction, marker). These are deliberately small translucent frames so
/// debug/freecam inspection can distinguish them without pretending that the
/// entities have a Vanilla creature model.
fn add_vanilla_logic_placeholder(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
) {
    use EntityRigPart::*;
    let size = 0.22_f32;
    let center = [0.0, size * 0.5, 0.0];
    if spec.entity_type_id == 105 {
        add_tinted(mesh, spec, pose, Other, center, [size * 1.8, size, size * 0.08], center,
            [0.0; 3], 0.0, 91);
    } else {
        add_cube_frame(mesh, spec, pose, center, [size, size, size], center,
            [0.0, pose.base.age_ticks * 0.2, 0.0], 91);
    }
}

/// Dispatch the dedicated vanilla rigs implemented above. Returning `false`
/// means the caller should use the reusable archetype fallback (still required
/// for modded names and for canonical rows whose dedicated rig is scheduled
/// in a later slice).
/// Quadruped feature presets for the parameterized mammal rig, keyed by
/// vanilla entity type id. Returns `None` for non-quadruped ids.
fn vanilla_quadruped_features(entity_type_id: u16) -> Option<QuadrupedFeatures> {
    let features = match entity_type_id {
        14 => QuadrupedFeatures {
            long_neck: true, hump: true, snout: true, pointed_ears: true, long_tail: true,
            ..QuadrupedFeatures::default()
        },
        15 | 72 => QuadrupedFeatures {
            snout: true, pointed_ears: true, long_tail: true, slim: true,
            ..QuadrupedFeatures::default()
        },
        22 => QuadrupedFeatures {
            snout: true, pointed_ears: true, horns: true, udder: true, long_tail: true,
            ..QuadrupedFeatures::default()
        },
        25 | 71 => QuadrupedFeatures {
            long_neck: true, long_ears: true, snout: true, mane: true, long_tail: true,
            ..QuadrupedFeatures::default()
        },
        49 => QuadrupedFeatures {
            snout: true, pointed_ears: true, horns: true, bearded: true, long_tail: true,
            ..QuadrupedFeatures::default()
        },
        51 | 123 => QuadrupedFeatures {
            snout: true, pointed_ears: true, horns: true, mane: true, stocky: true,
            ..QuadrupedFeatures::default()
        },
        53 | 92 | 125 => QuadrupedFeatures {
            long_neck: true, pointed_ears: true, snout: true, mane: true, long_tail: true,
            ..QuadrupedFeatures::default()
        },
        65 | 108 => QuadrupedFeatures {
            long_neck: true, long_ears: true, snout: true, wool: true, long_tail: true, slim: true,
            ..QuadrupedFeatures::default()
        },
        70 => QuadrupedFeatures {
            snout: true, pointed_ears: true, horns: true, udder: true, mushrooms: true,
            long_tail: true, ..QuadrupedFeatures::default()
        },
        74 | 81 | 77 => QuadrupedFeatures {
            snout: true, pointed_ears: true, stocky: true, ..QuadrupedFeatures::default()
        },
        87 => QuadrupedFeatures {
            snout: true, pointed_ears: true, wool: true, ..QuadrupedFeatures::default()
        },
        122 => QuadrupedFeatures {
            snout: true, pointed_ears: true, bushy_tail: true, slim: true,
            ..QuadrupedFeatures::default()
        },
        _ => return None,
    };
    Some(features)
}

/// Humanoid silhouette family for each vanilla entity type id that uses the
/// shared articulated biped rig. Returns `None` for non-humanoid ids.
fn vanilla_humanoid_style(entity_type_id: u16) -> Option<HumanoidStyle> {
    let style = match entity_type_id {
        11 => HumanoidStyle::Bogged,
        27 | 46 | 54 | 124 => HumanoidStyle::Zombie,
        35 | 55 => HumanoidStyle::IllagerHooded,
        78 | 79 => HumanoidStyle::Piglin,
        80 | 114 => HumanoidStyle::IllagerArmed,
        91 => HumanoidStyle::Skeleton,
        102 => HumanoidStyle::Stray,
        113 | 115 => HumanoidStyle::Illager,
        118 => HumanoidStyle::Witch,
        120 => HumanoidStyle::WitherSkeleton,
        126 => HumanoidStyle::ZombieVillager,
        127 => HumanoidStyle::ZombifiedPiglin,
        128 => HumanoidStyle::Player,
        _ => return None,
    };
    Some(style)
}

fn build_dedicated_vanilla_rig(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityRigPose,
    mouth_item_rgb: Option<[u8; 3]>,
) -> bool {
    if let Some(features) = vanilla_quadruped_features(spec.entity_type_id) {
        add_quadruped_rig(mesh, spec, pose, features);
        return true;
    }
    if let Some(style) = vanilla_humanoid_style(spec.entity_type_id) {
        add_humanoid_rig(mesh, spec, pose, style);
        return true;
    }
    match spec.entity_type_id {
        0 | 112 => add_vanilla_spirit(mesh, spec, pose),
        1 => add_vanilla_area_cloud(mesh, spec, pose),
        2 => add_vanilla_armadillo(mesh, spec, pose),
        3 => add_vanilla_armor_stand(mesh, spec, pose),
        4 | 99 => add_vanilla_arrow(mesh, spec, pose),
        5 => add_vanilla_axolotl(mesh, spec, pose),
        6 => add_vanilla_bat(mesh, spec, pose),
        7 => add_vanilla_bee(mesh, spec, pose),
        8 => add_vanilla_blaze(mesh, spec, pose),
        9 | 56 | 59 | 63 | 68 | 105 => add_vanilla_logic_placeholder(mesh, spec, pose),
        10 | 17 => add_vanilla_boat(mesh, spec, pose),
        12 => add_vanilla_breeze(mesh, spec, pose),
        13 | 26 | 28 | 32 | 37 | 39 | 62 | 66 | 82 | 89 | 94 | 97 | 117 => {
            add_thrown_projectile(mesh, spec, pose, spec.entity_type_id)
        }
        16 | 100 => add_vanilla_spider(mesh, spec, pose),
        18 | 21 | 44 | 52 | 69 | 98 | 107 => add_vanilla_minecart(mesh, spec, pose),
        19 => add_vanilla_bird(mesh, spec, pose, false),
        20 | 86 | 110 => add_vanilla_fish(mesh, spec, pose),
        23 => add_vanilla_creeper(mesh, spec, pose),
        24 => add_vanilla_dolphin(mesh, spec, pose),
        29 | 50 => add_vanilla_guardian(mesh, spec, pose),
        30 => add_vanilla_end_crystal(mesh, spec, pose),
        31 => add_vanilla_ender_dragon(mesh, spec, pose),
        33 => add_vanilla_enderman(mesh, spec, pose),
        34 | 90 => add_vanilla_segmented_pest(mesh, spec, pose),
        36 => add_vanilla_evoker_fangs(mesh, spec, pose),
        38 | 58 => add_vanilla_item_entity(mesh, spec, pose),
        40 | 106 => add_vanilla_block_entity(mesh, spec, pose),
        41 => add_vanilla_firework(mesh, spec, pose),
        42 => add_fox(mesh, spec, pose, mouth_item_rgb),
        43 => add_vanilla_frog(mesh, spec, pose),
        45 => add_vanilla_ghast(mesh, spec, pose),
        47 | 60 | 73 => add_vanilla_flat_hanging(mesh, spec, pose),
        48 | 101 => add_vanilla_squid(mesh, spec, pose),
        57 => add_vanilla_iron_golem(mesh, spec, pose),
        61 => add_vanilla_ominous_spawner(mesh, spec, pose),
        64 => add_vanilla_lightning(mesh, spec, pose),
        67 | 93 => add_vanilla_slime_like(mesh, spec, pose),
        75 => add_vanilla_bird(mesh, spec, pose, true),
        76 => add_vanilla_phantom(mesh, spec, pose),
        83 => add_vanilla_pufferfish(mesh, spec, pose),
        84 => add_vanilla_rabbit(mesh, spec, pose),
        85 => add_vanilla_ravager(mesh, spec, pose),
        88 => add_vanilla_shulker(mesh, spec, pose),
        95 => add_vanilla_sniffer(mesh, spec, pose),
        96 => add_snow_golem(mesh, spec, pose),
        103 => add_vanilla_strider(mesh, spec, pose),
        104 => add_vanilla_tadpole(mesh, spec, pose),
        109 => add_vanilla_trident(mesh, spec, pose),
        111 => add_vanilla_turtle(mesh, spec, pose),
        116 => add_vanilla_warden(mesh, spec, pose),
        119 => add_vanilla_wither(mesh, spec, pose),
        121 => add_vanilla_wither_skull(mesh, spec, pose),
        129 => add_vanilla_fishing_bobber(mesh, spec, pose),
        _ => return false,
    }
    true
}

/// Build the shared procedural template for all 130 vanilla protocol IDs.
/// Zero-size collision entities receive small drawable placeholders; skin
/// overlay alpha/blending is a responsibility of the consuming renderer.
pub fn build_entity_rig_mesh(spec: EntityRigSpec, pose: EntityPose) -> ExtrudedMesh {
    build_entity_rig_mesh_inner(spec, EntityRigPose::from(pose), None)
}

/// Build a detailed rig with the extended optional pose state. This is an
/// additive entrypoint: the legacy [`build_entity_rig_mesh`] API is unchanged
/// and behaves exactly as if `EntityRigPose::from(pose)` were passed here.
pub fn build_entity_rig_mesh_with_pose(spec: EntityRigSpec, pose: EntityRigPose) -> ExtrudedMesh {
    build_entity_rig_mesh_inner(spec, pose, None)
}

/// [`build_entity_rig_mesh`] plus an optional held mouth item for the fox
/// rig (id 42). When `mouth_item_rgb` is `Some`, a small tilted prism is
/// attached at the snout tip and its quads carry the RGB packed into
/// `tint_index` (see [`mouth_item_tint`]). Other entities ignore the item.
pub fn build_entity_rig_mesh_with_mouth_item(
    spec: EntityRigSpec,
    pose: EntityPose,
    mouth_item_rgb: Option<[u8; 3]>,
) -> ExtrudedMesh {
    build_entity_rig_mesh_with_pose_and_mouth_item(spec, EntityRigPose::from(pose), mouth_item_rgb)
}

/// Use the extended pose and the fox's held mouth item in the same build.
pub fn build_entity_rig_mesh_with_pose_and_mouth_item(
    spec: EntityRigSpec,
    pose: EntityRigPose,
    mouth_item_rgb: Option<[u8; 3]>,
) -> ExtrudedMesh {
    build_entity_rig_mesh_inner(spec, pose, mouth_item_rgb)
}

fn build_biped_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let head = (
        [0.0, h * 0.875, 0.0],
        [w * 0.70, h * spec.proportions.head, w * 0.70],
        [0.0, h * 0.75, 0.0],
    );
    let torso = (
        [0.0, h * 0.55, 0.0],
        [w * 0.66, h * spec.proportions.torso, w * 0.43],
        [0.0, h * 0.75, 0.0],
    );
    add(mesh, spec, pose, Head, head.0, head.1, head.2, [0.0; 3], 0.0);
    add(mesh, spec, pose, Torso, torso.0, torso.1, torso.2, [0.0; 3], 0.0);
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.43, h * 0.735, 0.0];
        let size = [w * 0.22, h * 0.37, w * 0.30];
        let center = [pivot[0], h * 0.55, 0.0];
        add(mesh, spec, pose, part, center, size, pivot, [0.0; 3], 0.0);
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        let pivot = [side * w * 0.18, h * 0.35, 0.0];
        let size = [w * 0.27, h * spec.proportions.limbs, w * 0.34];
        let center = [pivot[0], h * 0.175, 0.0];
        add(mesh, spec, pose, part, center, size, pivot, [0.0; 3], 0.0);
    }
    if pose.base.enable_skin_3d_layers {
        add(mesh, spec, pose, Head, head.0, head.1, head.2, [0.0; 3], w * 0.035);
        add(mesh, spec, pose, Torso, torso.0, torso.1, torso.2, [0.0; 3], w * 0.015);
        for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
            let pivot = [side * w * 0.43, h * 0.735, 0.0];
            add(mesh, spec, pose, part, [pivot[0], h * 0.55, 0.0],
                [w * 0.22, h * 0.37, w * 0.30], pivot, [0.0; 3], w * 0.02);
        }
        for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
            let pivot = [side * w * 0.18, h * 0.35, 0.0];
            add(mesh, spec, pose, part, [pivot[0], h * 0.175, 0.0],
                [w * 0.27, h * spec.proportions.limbs, w * 0.34], pivot, [0.0; 3], w * 0.02);
        }
    }
}

fn build_quadruped_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.65, w * 0.08],
        [w * 0.75, h * spec.proportions.torso, w * 0.80], [0.0, h * 0.75, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.77, -w * 0.34],
        [w * 0.56, h * spec.proportions.head, w * 0.37], [0.0, h * 0.76, -w * 0.24], [0.0; 3],
        0.0);
    for (part, x) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for z in [-0.25_f32, 0.30] {
            let pivot = [x * w * 0.27, h * 0.42, z * w];
            add(mesh, spec, pose, part, [pivot[0], h * 0.21, pivot[2]],
                [w * 0.20, h * spec.proportions.limbs, w * 0.21], pivot, [0.0; 3], 0.0);
        }
    }
}

fn build_avian_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.43, 0.0],
        [w * 0.50, h * spec.proportions.torso, w * 0.62], [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.82, -w * 0.10],
        [w * 0.42, h * spec.proportions.head, w * 0.45], [0.0, h * 0.72, 0.0], [0.0; 3], 0.0);
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.20, h * 0.60, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.34, h * 0.55, 0.0],
            [w * 0.30, h * spec.proportions.limbs, w * 0.57], pivot, [0.0; 3], 0.0);
    }
}

fn build_arthropod_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Head, [0.0, h * 0.53, -w * 0.30],
        [w * 0.38, h * spec.proportions.head, w * 0.27], [0.0, h * 0.52, -w * 0.2], [0.0; 3],
        0.0);
    add(mesh, spec, pose, Torso, [0.0, h * 0.50, 0.0],
        [w * 0.44, h * spec.proportions.torso, w * 0.39], [0.0, h * 0.60, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Core, [0.0, h * 0.58, w * 0.27],
        [w * 0.56, h * 0.53, w * 0.44], [0.0, h * 0.60, 0.0], [0.0; 3], 0.0);
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for index in 0..4 {
            let z = (index as f32 - 1.5) * w * 0.16;
            let pivot = [side * w * 0.24, h * 0.54, z];
            add(mesh, spec, pose, part, [side * w * 0.34, h * 0.36, z],
                [w * 0.40, h * spec.proportions.limbs, w * 0.10], pivot,
                [0.0, (index as f32 - 1.5) * 0.20, side * 0.40], 0.0);
        }
    }
}

fn build_aquatic_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Torso, [0.0, h * 0.53, 0.0],
        [w * 0.65, h * spec.proportions.torso, w * 0.67], [0.0, h * 0.53, 0.0], [0.0; 3], 0.0);
    add(mesh, spec, pose, Head, [0.0, h * 0.55, -w * 0.30],
        [w * 0.50, h * spec.proportions.head, w * 0.33], [0.0, h * 0.54, -w * 0.18],
        [0.0; 3], 0.0);
    add(mesh, spec, pose, Tail, [0.0, h * 0.56, w * 0.37],
        [w * 0.23, h * 0.48, w * 0.27], [0.0, h * 0.54, w * 0.24], [0.0; 3], 0.0);
    for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
        let pivot = [side * w * 0.29, h * 0.52, 0.0];
        add(mesh, spec, pose, part, [side * w * 0.40, h * 0.44, 0.0],
            [w * 0.15, h * spec.proportions.limbs, w * 0.35], pivot, [0.0; 3], 0.0);
    }
}

fn build_projectile_or_item_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    match spec.entity_type_id {
        // Thrown spherical projectiles get a layered faceted proxy
        // instead of the bare collision box. Arrow (4) and the other
        // item-like ids keep the generic template.
        13 | 28 | 32 | 62 | 94 | 97 | 117 => {
            add_thrown_projectile(mesh, spec, pose, spec.entity_type_id);
        }
        _ => {
            add(mesh, spec, pose, Core, [0.0, h * 0.50, 0.0], [w, h, w], [0.0; 3], [0.0; 3],
                0.0);
        }
    }
}

fn build_vehicle_or_hanging_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.36, 0.0], [w, h * 0.45, w], [0.0; 3], [0.0; 3],
        0.0);
    add(mesh, spec, pose, Shell, [0.0, h * 0.66, 0.0], [w * 0.80, h * 0.22, w * 0.80],
        [0.0; 3], [0.0; 3], 0.0);
}

fn build_generic_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    // Never-invisible modded fallback: a name-hash-tinted torso box
    // plus a brighter head cap so the silhouette reads as a creature
    // instead of a stray cube.
    let (primary, accent) = modded_proxy_tints(spec.name);
    add_tinted(mesh, spec, pose, Core, [0.0, h * 0.42, 0.0], [w * 0.85, h * 0.72, w * 0.85],
        [0.0; 3], [0.0; 3], 0.0, primary);
    add_tinted(mesh, spec, pose, Head, [0.0, h * 0.80, -w * 0.08], [w * 0.50, h * 0.26, w * 0.50],
        [0.0, h * 0.66, 0.0], [0.0; 3], 0.0, accent);
}

fn build_slime_cube_proxy(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityRigPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    add(mesh, spec, pose, Core, [0.0, h * 0.5, 0.0], [w * 0.60, h * 0.60, w * 0.60], [0.0; 3],
        [0.0; 3], 0.0);
    add(mesh, spec, pose, Shell, [0.0, h * 0.5, 0.0], [w, h, w], [0.0; 3], [0.0; 3], 0.0);
}

fn build_entity_rig_mesh_inner(
    spec: EntityRigSpec,
    pose: EntityRigPose,
    mouth_item_rgb: Option<[u8; 3]>,
) -> ExtrudedMesh {
    use EntityRigArchetype::*;
    let pose = pose.sanitized();
    let mut mesh = ExtrudedMesh::new();
    let w = finite_or_zero(spec.width).clamp(0.125, 32.0);
    let h = finite_or_zero(spec.height).clamp(0.125, 32.0);
    let spec = EntityRigSpec {
        height: h,
        width: w,
        ..spec
    };
    if build_dedicated_vanilla_rig(&mut mesh, spec, pose, mouth_item_rgb) {
        return mesh;
    }
    match spec.archetype {
        Biped if spec.entity_type_id == 96 => add_snow_golem(&mut mesh, spec, pose),
        Biped => build_biped_proxy(&mut mesh, spec, pose),
        Quadruped if spec.entity_type_id == 42 => add_fox(&mut mesh, spec, pose, mouth_item_rgb),
        Quadruped => build_quadruped_proxy(&mut mesh, spec, pose),
        Avian => build_avian_proxy(&mut mesh, spec, pose),
        Arthropod => build_arthropod_proxy(&mut mesh, spec, pose),
        SlimeCube => build_slime_cube_proxy(&mut mesh, spec, pose),
        Aquatic => build_aquatic_proxy(&mut mesh, spec, pose),
        ProjectileOrItem => build_projectile_or_item_proxy(&mut mesh, spec, pose),
        VehicleOrHanging => build_vehicle_or_hanging_proxy(&mut mesh, spec, pose),
        GenericProxy => build_generic_proxy(&mut mesh, spec, pose),
    }
    mesh
}

/// Build geometry from parsed .jem/.jpm boxes with part-aware animation.
/// Valid boxes replace the base rig. Empty or invalid overrides fall back to
/// the procedural template. Processing is bounded to 256 boxes.
pub fn build_entity_from_cem_boxes(
    spec: EntityRigSpec,
    pose: EntityPose,
    boxes: &[CemBox],
) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    let pose = EntityRigPose::from(pose).sanitized();
    let safe_height = finite_or_zero(spec.height).clamp(0.125, 32.0);
    let safe_spec = EntityRigSpec {
        height: safe_height,
        ..spec
    };
    for candidate in boxes.iter().take(256) {
        if candidate.is_valid() {
            emit_box(&mut mesh, safe_spec, pose, RigBox::from(candidate));
        }
    }
    if mesh.quads.is_empty() {
        build_entity_rig_mesh_inner(spec, pose, None)
    } else {
        mesh
    }
}

// ---------------------------------------------------------------------------
// Colossal articulated rigs (dragon / sea serpent)
// ---------------------------------------------------------------------------

/// Standalone colossal rig families. These are intentionally separate from
/// [`EntityRigArchetype`]: they are built on demand by mod/content callers via
/// [`build_colossal_rig_mesh`] and never participate in the 130-entity
/// protocol classification, so no existing match arm or golden fingerprint
/// is affected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColossalRigKind {
    /// Articulated winged dragon / wyvern: horned head, four legs, spiked
    /// tail, and two multi-segment flapping wings.
    WingedDragon,
    /// Undulating sea serpent / hydra: fanged head, dorsal crest, forked tail.
    SeaSerpent,
}

impl ColossalRigKind {
    /// Suggested `(primary, accent)` sRGB palettes per family.
    pub const fn palette(self) -> ([u8; 3], [u8; 3]) {
        match self {
            Self::WingedDragon => ([178, 42, 46], [240, 178, 60]),
            Self::SeaSerpent => ([28, 92, 112], [90, 200, 190]),
        }
    }
}

/// Encode an sRGB triple as an [`ExtrudedQuad::tint_index`] albedo. The high
/// flag bit keeps dark colors above the `0xFF` material-tag threshold; the
/// renderer masks each channel with `0xFF`, so the flag is ignored on decode.
fn colossal_tint(rgb: [u8; 3]) -> i32 {
    0x0100_0000 | (i32::from(rgb[0]) << 16) | (i32::from(rgb[1]) << 8) | i32::from(rgb[2])
}

/// Outward-CCW face table shared by [`emit_colossal_box`]. Identical winding
/// and vertex ordering to `emit_box`, so rotated boxes stay outward-facing.
const COLOSSAL_FACES: [[usize; 4]; 6] = [
    [0, 4, 6, 2],
    [5, 1, 3, 7],
    [4, 0, 1, 5],
    [2, 6, 7, 3],
    [1, 0, 2, 3],
    [4, 5, 7, 6],
];

/// Emit one articulated box for a colossal rig. `pivot`, `center`, and `size`
/// are in unit (unscaled) entity-local space; `rotation` is radians applied
/// about `pivot` via the shared X-then-Y-then-Z [`rotate`] convention.
/// Everything is multiplied by `stage_scale` so Stage 1 hatchlings through
/// Stage 5 adults share one template.
#[allow(clippy::too_many_arguments)]
fn emit_colossal_box(
    mesh: &mut ExtrudedMesh,
    stage_scale: f32,
    pivot: [f32; 3],
    center: [f32; 3],
    size: [f32; 3],
    rotation: [f32; 3],
    tint_index: i32,
) {
    if mesh.quads.len() + 6 > MAX_UPGRADE_QUADS {
        return;
    }
    let half = size.map(|v| v * 0.5 * stage_scale);
    if half.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return;
    }
    let center = center.map(|v| v * stage_scale);
    let pivot = pivot.map(|v| v * stage_scale);
    let lo = [
        center[0] - half[0],
        center[1] - half[1],
        center[2] - half[2],
    ];
    let hi = [
        center[0] + half[0],
        center[1] + half[1],
        center[2] + half[2],
    ];
    let vertices = [
        [lo[0], lo[1], lo[2]],
        [hi[0], lo[1], lo[2]],
        [lo[0], hi[1], lo[2]],
        [hi[0], hi[1], lo[2]],
        [lo[0], lo[1], hi[2]],
        [hi[0], lo[1], hi[2]],
        [lo[0], hi[1], hi[2]],
        [hi[0], hi[1], hi[2]],
    ];
    let uv = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
    for indices in COLOSSAL_FACES {
        let mut positions = [[0.0; 3]; 4];
        for (dest, index) in positions.iter_mut().zip(indices) {
            let original = vertices[index];
            let relative = [
                original[0] - pivot[0],
                original[1] - pivot[1],
                original[2] - pivot[2],
            ];
            let animated = rotate(relative, rotation);
            *dest = [
                pivot[0] + animated[0],
                pivot[1] + animated[1],
                pivot[2] + animated[2],
            ];
        }
        if let Some(normal) = outward_normal(&positions) {
            if positions.iter().flatten().all(|v| v.is_finite()) {
                mesh.quads.push(ExtrudedQuad {
                    positions,
                    normal,
                    uv,
                    tint_index,
                });
            }
        }
    }
}

/// Winged dragon / wyvern template. Unit space: front is -Z, feet at y = 0,
/// origin centered horizontally. `stage_scale` is applied inside
/// [`emit_colossal_box`].
/// Four-segment tapering tail with per-segment sinusoidal sway, plus the
/// dorsal spike row along the torso.
fn winged_dragon_tail(
    mesh: &mut ExtrudedMesh,
    s: f32,
    pose: EntityPose,
    primary: i32,
    accent: i32,
) {
    for i in 0..4 {
        let f = i as f32;
        let radius = 0.4 * (1.0 - f * 0.16);
        let y = 1.15 + f * 0.18 - f * f * 0.02;
        let z = 2.15 + f * 0.85;
        let pivot = if i == 0 {
            [0.0, 1.15, 2.15]
        } else {
            [
                0.0,
                1.15 + (f - 1.0) * 0.18 - (f - 1.0) * (f - 1.0) * 0.02,
                z - 0.85,
            ]
        };
        let sway = (pose.limb_swing - f * 0.55).sin() * 0.14 * (f + 1.0) * 0.45;
        emit_colossal_box(mesh, s, pivot, [0.0, y, z + 0.45], [radius * 2.0, radius * 2.0, 0.95],
            [0.06 * f, sway, 0.0], primary);
        emit_colossal_box(mesh, s, [0.0, y, z + 0.35], [0.0, y + radius + 0.1, z + 0.3],
            [0.12, 0.28, 0.22], [-0.2, 0.0, 0.0], accent);
    }
    for z in [-1.0, -0.2, 0.6, 1.4] {
        emit_colossal_box(mesh, s, [0.0, 1.58, z], [0.0, 1.58, z], [0.16, 0.4, 0.2],
            [-0.15, 0.0, 0.0], accent);
    }
}

/// Two multi-segment wings: inner arm + swept membrane + wingtip, with flap
/// and fold driven by the pose.
fn winged_dragon_wings(
    mesh: &mut ExtrudedMesh,
    s: f32,
    pose: EntityPose,
    primary: i32,
    accent: i32,
) {
    let flap = pose.limb_swing.sin() * pose.limb_swing_amount * 0.9
        + pose.limb_swing.cos() * pose.limb_swing_amount * 0.15;
    let fold = if pose.swimming || pose.elytra_flying {
        1.1
    } else {
        0.0
    };
    for side in [-1.0_f32, 1.0] {
        let shoulder = [side * 0.62, 1.5, -0.7];
        let inner_rot = [0.0, -side * 0.18, side * (flap * 0.45 + fold * 0.3)];
        emit_colossal_box(mesh, s, shoulder, [side * 1.45, 1.52, -0.55], [1.7, 0.2, 0.5], inner_rot,
            primary);
        emit_colossal_box(mesh, s, [side * 1.45, 1.64, -0.6], [side * 1.45, 1.64, -0.6],
            [1.6, 0.06, 0.12], inner_rot, accent);
        let elbow = [side * 2.15, 1.5, -0.85];
        let mid_rot = [0.0, -side * 0.42, side * (flap * 0.9 + fold * 0.7)];
        emit_colossal_box(mesh, s, elbow, [side * 3.3, 1.42, -0.35], [2.4, 0.09, 1.5], mid_rot,
            accent);
        let tip_pivot = [side * 4.3, 1.4, -0.15];
        let tip_rot = [0.0, -side * 0.6, side * (flap * 1.15 + fold * 0.9)];
        emit_colossal_box(mesh, s, tip_pivot, [side * 4.9, 1.38, 0.45], [1.5, 0.08, 1.0], tip_rot,
            accent);
        emit_colossal_box(mesh, s, [side * 5.4, 1.36, 0.75], [side * 5.4, 1.36, 0.75],
            [0.5, 0.07, 0.35], tip_rot, accent);
    }
}

fn build_winged_dragon(
    mesh: &mut ExtrudedMesh,
    s: f32,
    pose: EntityPose,
    primary: i32,
    accent: i32,
) {
    let stride = pose.limb_swing.cos() * pose.limb_swing_amount * 0.55;
    let head_pitch = pose.head_pitch_deg.to_radians();
    let head_yaw = pose.head_yaw_deg.to_radians();

    // Elongated torso plus a lighter belly plate.
    emit_colossal_box(mesh, s, [0.0, 1.05, 0.4], [0.0, 1.05, 0.4], [1.2, 1.0, 3.4], [0.0; 3],
        primary);
    emit_colossal_box(mesh, s, [0.0, 0.6, 0.4], [0.0, 0.6, 0.4], [0.85, 0.14, 3.2], [0.0; 3],
        accent);

    // Three-segment neck rising toward the horned head.
    for (pivot, center, size, pitch) in [
        ([0.0, 1.5, -1.2], [0.0, 1.72, -1.5], [0.6, 0.7, 0.8], -0.55),
        ([0.0, 1.9, -1.75], [0.0, 2.1, -2.0], [0.55, 0.62, 0.7], -0.6),
        (
            [0.0, 2.35, -2.2],
            [0.0, 2.5, -2.45],
            [0.5, 0.55, 0.6],
            -0.65,
        ),
    ] {
        emit_colossal_box(mesh, s, pivot, center, size, [pitch, 0.0, 0.0], primary);
    }

    // Head, snout, and jaw share the head yaw/pitch articulation.
    let head_pivot = [0.0, 2.62, -2.6];
    let head_rot = [head_pitch - 0.1, head_yaw, 0.0];
    emit_colossal_box(mesh, s, head_pivot, [0.0, 2.78, -2.75], [0.58, 0.52, 0.85], head_rot,
        primary);
    emit_colossal_box(mesh, s, head_pivot, [0.0, 2.7, -3.25], [0.4, 0.34, 0.55], head_rot, primary);
    emit_colossal_box(mesh, s, [0.0, 2.58, -2.7], [0.0, 2.52, -3.0], [0.44, 0.18, 0.7],
        [head_pitch + 0.4, head_yaw, 0.0], accent);
    for side in [-1.0_f32, 1.0] {
        emit_colossal_box(mesh, s, [side * 0.2, 3.0, -2.5], [side * 0.2, 3.0, -2.5],
            [0.12, 0.45, 0.14], [0.7, 0.0, -side * 0.25], accent);
    }

    // Four sturdy legs; diagonal pairs swing in opposition.
    for (side, z, phase) in [
        (-1.0_f32, -1.0_f32, 0.0_f32),
        (1.0, -1.0, PI),
        (-1.0, 1.5, PI),
        (1.0, 1.5, 0.0),
    ] {
        let hip = [side * 0.55, 1.0, z];
        let swing = (pose.limb_swing + phase).cos() * stride;
        for (center, size) in [
            ([side * 0.55, 0.72, z], [0.45, 0.62, 0.55]),
            ([side * 0.6, 0.32, z + 0.05], [0.36, 0.5, 0.45]),
            ([side * 0.6, 0.09, z - 0.08], [0.4, 0.18, 0.6]),
        ] {
            emit_colossal_box(mesh, s, hip, center, size, [swing, 0.0, 0.0], primary);
        }
    }

    winged_dragon_tail(mesh, s, pose, primary, accent);
    winged_dragon_wings(mesh, s, pose, primary, accent);
}

/// Sea serpent / hydra template. Unit space: head toward -Z, body floats
/// around y = 0.8 so the silhouette stays near the origin, `stage_scale`
/// applied inside [`emit_colossal_box`].
fn build_sea_serpent(mesh: &mut ExtrudedMesh, s: f32, pose: EntityPose, primary: i32, accent: i32) {
    let head_pitch = pose.head_pitch_deg.to_radians();
    let head_yaw = pose.head_yaw_deg.to_radians();
    let head_rot = [head_pitch - 0.05, head_yaw, 0.0];

    // Fanged aquatic head with crest and jaw.
    let head_pivot = [0.0, 0.85, -2.7];
    emit_colossal_box(mesh, s, head_pivot, [0.0, 0.85, -2.95], [0.75, 0.65, 1.0], head_rot,
        primary);
    emit_colossal_box(mesh, s, head_pivot, [0.0, 0.78, -3.55], [0.5, 0.4, 0.6], head_rot, primary);
    emit_colossal_box(mesh, s, [0.0, 0.62, -2.85], [0.0, 0.55, -3.3], [0.55, 0.16, 0.7],
        [head_pitch + 0.3, head_yaw, 0.0], accent);
    for side in [-1.0_f32, 1.0] {
        emit_colossal_box(mesh, s, [side * 0.14, 0.52, -3.45], [side * 0.14, 0.52, -3.45],
            [0.07, 0.22, 0.07], head_rot, accent);
        emit_colossal_box(mesh, s, [side * 0.42, 0.72, -2.55], [side * 0.62, 0.68, -2.5],
            [0.5, 0.06, 0.35], [head_pitch, head_yaw, side * 0.5], accent);
    }
    emit_colossal_box(mesh, s, [0.0, 1.22, -2.9], [0.0, 1.22, -2.9], [0.12, 0.4, 0.7],
        [0.0, head_yaw, 0.0], accent);

    // Eight undulating body segments: lateral/vertical sine wave whose
    // amplitude and phase lag grow toward the tail.
    for i in 0..8 {
        let f = i as f32;
        let phase = pose.limb_swing - f * 0.65;
        let lat = phase.sin() * (0.12 + f * 0.13);
        let vert = phase.cos() * (0.06 + f * 0.05);
        let radius = 0.55 - f * 0.045;
        let z = -1.9 + f * 0.8;
        let center = [lat, 0.8 + vert, z];
        let sway = phase.cos() * 0.15;
        emit_colossal_box(mesh, s, center, center, [radius * 2.0, radius * 1.8, 0.85],
            [0.0, sway, 0.0], primary);
        // Dorsal fin crest along the mid-body segments.
        if (1..=6).contains(&i) {
            emit_colossal_box(mesh, s, center, [lat, 0.8 + vert + radius * 0.9 + 0.12, z],
                [0.1, 0.3 + f * 0.04, 0.4], [0.0, sway, 0.0], accent);
        }
    }

    // Forked tail fin: two accent blades angled into a V at the tip.
    let tail_z = -1.9 + 7.0 * 0.8 + 0.5;
    let tail_y = 0.8 + (pose.limb_swing - 7.0 * 0.65).cos() * (0.06 + 7.0 * 0.05);
    for (dy, pitch) in [(0.45, -0.5), (-0.4, 0.5)] {
        emit_colossal_box(mesh, s, [0.0, tail_y, tail_z - 0.3], [0.0, tail_y + dy, tail_z + 0.5],
            [0.1, 1.0, 0.6], [pitch, 0.0, 0.0], accent);
    }
}

/// Build a deterministic articulated colossal voxel mesh (dragon or sea
/// serpent) for mod/content callers. `stage_scale` selects the growth stage:
/// it is sanitized and clamped to `0.35..=3.0` (Stage 1 hatchling through
/// Stage 5 colossal adult); non-finite input collapses to the minimum.
/// `primary_rgb`/`accent_rgb` are packed into quad tint indices so the
/// renderer resolves them as sRGB albedo tints.
pub fn build_colossal_rig_mesh(
    kind: ColossalRigKind,
    pose: EntityPose,
    stage_scale: f32,
    primary_rgb: [u8; 3],
    accent_rgb: [u8; 3],
) -> ExtrudedMesh {
    let s = finite_or_zero(stage_scale).clamp(0.35, 3.0);
    let pose = pose.sanitized();
    let primary = colossal_tint(primary_rgb);
    let accent = colossal_tint(accent_rgb);
    let mut mesh = ExtrudedMesh::new();
    match kind {
        ColossalRigKind::WingedDragon => build_winged_dragon(&mut mesh, s, pose, primary, accent),
        ColossalRigKind::SeaSerpent => build_sea_serpent(&mut mesh, s, pose, primary, accent),
    }
    mesh
}

/// Stable FNV-1a hash of ordered quad topology, floating-point bits, UVs,
/// normals, and tint. Uses no platform hasher or timing information.
pub fn entity_mesh_fingerprint(mesh: &ExtrudedMesh) -> u64 {
    let mut buf = Vec::with_capacity(mesh.quads.len() * 100);
    buf.extend_from_slice(&(mesh.quads.len() as u64).to_le_bytes());
    for quad in &mesh.quads {
        for value in quad
            .positions
            .iter()
            .flatten()
            .chain(quad.normal.iter())
            .chain(quad.uv.iter().flatten())
        {
            buf.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        buf.extend_from_slice(&quad.tint_index.to_le_bytes());
    }
    megu_core::perf_kit::fnv1a64(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_bounds(mesh: &ExtrudedMesh) -> ([f32; 3], [f32; 3]) {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for quad in &mesh.quads {
            for p in &quad.positions {
                for axis in 0..3 {
                    lo[axis] = lo[axis].min(p[axis]);
                    hi[axis] = hi[axis].max(p[axis]);
                }
            }
        }
        (lo, hi)
    }

    /// Same outward-CCW / finite / unit-normal contract the integration
    /// suite enforces on every one of the 130 rigs.
    fn assert_valid_mesh(mesh: &ExtrudedMesh) {
        assert!(!mesh.quads.is_empty());
        for quad in &mesh.quads {
            assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
            assert!(quad.normal.iter().all(|v| v.is_finite()));
            assert!(quad
                .uv
                .iter()
                .flatten()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
            let [a, b, c, _] = quad.positions;
            let x = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let y = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let cross = [
                x[1] * y[2] - x[2] * y[1],
                x[2] * y[0] - x[0] * y[2],
                x[0] * y[1] - x[1] * y[0],
            ];
            let dot = cross
                .iter()
                .zip(quad.normal)
                .map(|(a, b)| a * b)
                .sum::<f32>();
            assert!(dot > 0.0, "quad is not outward CCW");
            let normal_len = quad.normal.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((normal_len - 1.0).abs() < 1e-4, "non-unit normal");
        }
    }

    #[test]
    fn fox_snow_golem_and_projectiles_classify_and_build_valid_meshes() {
        for (id, name) in [
            (42_u16, "minecraft:fox"),
            (96, "minecraft:snow_golem"),
            (97, "minecraft:snowball"),
            (28, "minecraft:egg"),
            (32, "minecraft:ender_pearl"),
            (117, "minecraft:wind_charge"),
            (62, "minecraft:fireball"),
            (94, "minecraft:small_fireball"),
        ] {
            let spec = classify_entity_rig(id).expect("entity id must classify");
            assert_eq!(spec.name, name);
            assert_eq!(spec.entity_type_id, id);
            let mesh = build_entity_rig_mesh(spec, EntityPose::default());
            assert_valid_mesh(&mesh);
            let (lo, hi) = mesh_bounds(&mesh);
            assert!(lo.iter().chain(hi.iter()).all(|v| v.is_finite()));
            // Feet rest at y = 0 and the body stays within a generous
            // multiple of the collision box on every axis.
            assert!(lo[1] >= -0.05 && lo[1] <= 0.05, "id {id} feet y");
            let span = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
            assert!(span[0] <= spec.width * 2.0 + 0.05, "id {id} x span");
            assert!(span[2] <= spec.width * 2.0 + 0.05, "id {id} z span");
            assert!(span[1] <= spec.height * 1.75 + 0.05, "id {id} y span");
        }
    }

    #[test]
    fn projectile_mesh_is_layered_not_a_single_box() {
        // The old generic template emitted exactly one 6-quad box; the
        // thrown-projectile proxy emits three boxes (shell, rotated inner,
        // core) with per-family accent tints.
        for id in [97_u16, 28, 32, 117, 62, 94] {
            let spec = classify_entity_rig(id).unwrap();
            let mesh = build_entity_rig_mesh(spec, EntityPose::default());
            assert_eq!(mesh.quads.len(), 18, "id {id}");
            let tints: std::collections::BTreeSet<i32> =
                mesh.quads.iter().map(|q| q.tint_index).collect();
            assert!(tints.iter().all(|t| *t > 0), "id {id} tints {tints:?}");
        }
        // Snowball reads as ice white: its shell/core share one slot and
        // the rotated inner facet a second.
        let snowball = classify_entity_rig(97).unwrap();
        let mesh = build_entity_rig_mesh(snowball, EntityPose::default());
        assert!(mesh.quads.iter().any(|q| q.tint_index == 2));
        assert!(mesh.quads.iter().any(|q| q.tint_index == 3));
    }

    #[test]
    fn fox_mouth_item_adds_geometry_and_changes_fingerprint() {
        let spec = classify_entity_rig(42).unwrap();
        let pose = EntityPose::default();
        let empty = build_entity_rig_mesh_with_mouth_item(spec, pose, None);
        let base = build_entity_rig_mesh(spec, pose);
        assert_eq!(empty, base, "None mouth item must equal the base rig");

        let rgb = [200_u8, 40, 30];
        let held = build_entity_rig_mesh_with_mouth_item(spec, pose, Some(rgb));
        assert_eq!(held.quads.len(), empty.quads.len() + 6);
        assert_valid_mesh(&held);
        assert_ne!(
            entity_mesh_fingerprint(&held),
            entity_mesh_fingerprint(&empty)
        );
        // The held prism carries the packed RGB tint on every one of its
        // quads and sits forward of the snout (negative Z).
        let expected_tint = MOUTH_ITEM_TINT_FLAG
            | (i32::from(rgb[0]) << 16)
            | (i32::from(rgb[1]) << 8)
            | i32::from(rgb[2]);
        let item_quads: Vec<_> = held
            .quads
            .iter()
            .filter(|q| q.tint_index == expected_tint)
            .collect();
        assert_eq!(item_quads.len(), 6);
        for (i, face) in item_quads.iter().enumerate() {
            assert!(item_quads[..i].iter().all(|earlier| earlier.uv != face.uv));
        }
        let extended = EntityRigPose::from(EntityPose {
            head_yaw_deg: 25.0,
            ..pose
        });
        let turned = build_entity_rig_mesh_with_pose_and_mouth_item(spec, extended, Some(rgb));
        assert_ne!(turned, held);
        assert_eq!(
            turned
                .quads
                .iter()
                .filter(|q| q.tint_index == expected_tint)
                .count(),
            6
        );
        assert!(item_quads
            .iter()
            .flat_map(|q| q.positions.iter())
            .all(|p| p[2] < -spec.width * 0.4));
        // A different item color changes the fingerprint again.
        let other = build_entity_rig_mesh_with_mouth_item(spec, pose, Some([1, 2, 3]));
        assert_ne!(
            entity_mesh_fingerprint(&other),
            entity_mesh_fingerprint(&held)
        );
        // Mouth items on non-fox entities are ignored.
        let zombie = classify_entity_rig(124).unwrap();
        assert_eq!(
            build_entity_rig_mesh_with_mouth_item(zombie, pose, Some(rgb)),
            build_entity_rig_mesh(zombie, pose)
        );
    }

    #[test]
    fn fox_and_snow_golem_have_distinct_part_layouts() {
        let fox = classify_entity_rig(42).unwrap();
        let cow = classify_entity_rig(22).unwrap();
        assert_ne!(
            build_entity_rig_mesh(fox, EntityPose::default()),
            build_entity_rig_mesh(cow, EntityPose::default())
        );
        // The fox tail sways with age while the generic quadruped has no
        // Tail-bound boxes beyond the aquatic template.
        let pose_a = EntityPose {
            age_ticks: 0.0,
            ..EntityPose::default()
        };
        let pose_b = EntityPose {
            age_ticks: 9.0,
            ..EntityPose::default()
        };
        assert_ne!(
            build_entity_rig_mesh(fox, pose_a),
            build_entity_rig_mesh(fox, pose_b)
        );

        let golem = classify_entity_rig(96).unwrap();
        let zombie = classify_entity_rig(124).unwrap();
        assert_ne!(
            build_entity_rig_mesh(golem, EntityPose::default()),
            build_entity_rig_mesh(zombie, EntityPose::default())
        );
        // Pumpkin head carries the accent tint slot.
        assert!(build_entity_rig_mesh(golem, EntityPose::default())
            .quads
            .iter()
            .any(|q| q.tint_index == 7));
    }

    #[test]
    fn dedicated_rigs_surpass_legacy_proxy_density() {
        // The old coarse templates topped out at one box per major body part.
        // Dedicated vanilla rigs must remain richer than those proxies while
        // preserving the same finite/CCW mesh contract.
        let expected_minimums = [
            (128_u16, 36_usize),
            (22, 36),
            (19, 24),
            (100, 66),
            (93, 12),
            (20, 30),
            (10, 12),
            (4, 6),
        ];
        for (id, minimum_quads) in expected_minimums {
            let spec = classify_entity_rig(id).unwrap();
            let mesh = build_entity_rig_mesh(spec, EntityPose::default());
            assert!(
                mesh.quads.len() >= minimum_quads,
                "id {id}: {} < {minimum_quads}",
                mesh.quads.len()
            );
            assert_valid_mesh(&mesh);
        }
    }
}
#[cfg(test)]
mod colossal_rig_tests {
    use std::f32::consts::FRAC_PI_2;

    use super::*;

    fn verify_colossal_mesh(mesh: &ExtrudedMesh) {
        assert!(!mesh.quads.is_empty());
        for quad in &mesh.quads {
            assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
            assert!(quad.normal.iter().all(|v| v.is_finite()));
            assert!(quad
                .uv
                .iter()
                .flatten()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
            let [a, b, c, _] = quad.positions;
            let x = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let y = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let cross = [
                x[1] * y[2] - x[2] * y[1],
                x[2] * y[0] - x[0] * y[2],
                x[0] * y[1] - x[1] * y[0],
            ];
            let dot = cross
                .iter()
                .zip(quad.normal)
                .map(|(a, b)| a * b)
                .sum::<f32>();
            assert!(dot > 0.0, "colossal quad is not outward CCW");
        }
    }

    fn bbox_extent(mesh: &ExtrudedMesh) -> f32 {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        for quad in &mesh.quads {
            for p in &quad.positions {
                for axis in 0..3 {
                    lo[axis] = lo[axis].min(p[axis]);
                    hi[axis] = hi[axis].max(p[axis]);
                }
            }
        }
        (0..3)
            .map(|axis| hi[axis] - lo[axis])
            .fold(0.0_f32, f32::max)
    }

    #[test]
    fn colossal_rigs_produce_valid_meshes_at_stage_bounds() {
        for kind in [ColossalRigKind::WingedDragon, ColossalRigKind::SeaSerpent] {
            for scale in [0.4_f32, 2.2] {
                let mesh = build_colossal_rig_mesh(
                    kind,
                    EntityPose::default(),
                    scale,
                    kind.palette().0,
                    kind.palette().1,
                );
                verify_colossal_mesh(&mesh);
            }
            let hatchling = build_colossal_rig_mesh(
                kind,
                EntityPose::default(),
                0.4,
                kind.palette().0,
                kind.palette().1,
            );
            let adult = build_colossal_rig_mesh(
                kind,
                EntityPose::default(),
                2.2,
                kind.palette().0,
                kind.palette().1,
            );
            assert!(
                bbox_extent(&adult) > bbox_extent(&hatchling),
                "{kind:?} stage 5 must be strictly larger than stage 1"
            );
        }
    }

    #[test]
    fn colossal_limb_swing_drives_wing_flap_and_serpent_undulation() {
        for kind in [ColossalRigKind::WingedDragon, ColossalRigKind::SeaSerpent] {
            let base = EntityPose {
                limb_swing_amount: 1.0,
                ..EntityPose::default()
            };
            let a = build_colossal_rig_mesh(kind, base, 1.0, [200, 30, 30], [240, 200, 60]);
            let b = build_colossal_rig_mesh(
                kind,
                EntityPose {
                    limb_swing: FRAC_PI_2,
                    ..base
                },
                1.0,
                [200, 30, 30],
                [240, 200, 60],
            );
            assert_ne!(
                entity_mesh_fingerprint(&a),
                entity_mesh_fingerprint(&b),
                "{kind:?} limb_swing must change the mesh"
            );
            verify_colossal_mesh(&b);
        }
    }

    #[test]
    fn colossal_head_pose_and_palette_are_deterministic() {
        let kind = ColossalRigKind::WingedDragon;
        let pose = EntityPose {
            head_yaw_deg: 35.0,
            head_pitch_deg: -15.0,
            limb_swing_amount: 0.8,
            limb_swing: 1.1,
            ..EntityPose::default()
        };
        let a = build_colossal_rig_mesh(kind, pose, 1.5, [10, 20, 30], [200, 210, 220]);
        let b = build_colossal_rig_mesh(kind, pose, 1.5, [10, 20, 30], [200, 210, 220]);
        assert_eq!(entity_mesh_fingerprint(&a), entity_mesh_fingerprint(&b));
        let untinted = build_colossal_rig_mesh(kind, pose, 1.5, [0, 0, 0], [0, 0, 0]);
        assert_ne!(
            entity_mesh_fingerprint(&a),
            entity_mesh_fingerprint(&untinted)
        );
        verify_colossal_mesh(&a);
        verify_colossal_mesh(&untinted);
    }

    #[test]
    fn colossal_stage_scale_sanitizes_non_finite_and_nonpositive() {
        for kind in [ColossalRigKind::WingedDragon, ColossalRigKind::SeaSerpent] {
            for scale in [f32::NAN, f32::INFINITY, 0.0, -2.5, f32::NEG_INFINITY] {
                let mesh = build_colossal_rig_mesh(
                    kind,
                    EntityPose::default(),
                    scale,
                    [1, 2, 3],
                    [4, 5, 6],
                );
                verify_colossal_mesh(&mesh);
            }
            // Clamped floor: NaN and zero both land on the 0.35 minimum.
            let floor_a = build_colossal_rig_mesh(
                kind,
                EntityPose::default(),
                f32::NAN,
                kind.palette().0,
                kind.palette().1,
            );
            let floor_b = build_colossal_rig_mesh(
                kind,
                EntityPose::default(),
                0.35,
                kind.palette().0,
                kind.palette().1,
            );
            assert_eq!(
                entity_mesh_fingerprint(&floor_a),
                entity_mesh_fingerprint(&floor_b)
            );
        }
    }
}

#[cfg(test)]
mod modded_classifier_tests {
    use super::*;

    fn assert_drawable(mesh: &ExtrudedMesh) {
        assert!(!mesh.quads.is_empty(), "modded rig produced no geometry");
        for quad in &mesh.quads {
            assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
            assert!(quad.normal.iter().all(|v| v.is_finite()));
            assert!(quad
                .uv
                .iter()
                .flatten()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        }
    }

    #[test]
    fn vanilla_names_resolve_to_byte_identical_canonical_specs() {
        for id in 0..ENTITIES.len() as u16 {
            let canonical = classify_entity_rig(id).expect("protocol id must classify");
            for alias in [
                canonical.name,
                canonical
                    .name
                    .strip_prefix("minecraft:")
                    .unwrap_or(canonical.name),
            ] {
                let named = classify_entity_rig_named(alias, None, None);
                assert_eq!(named, canonical, "id {id} alias {alias}");
            }
        }
        // The numeric path is untouched: out-of-range ids still yield None.
        assert!(classify_entity_rig(ENTITIES.len() as u16).is_none());
        assert!(classify_entity_rig(u16::MAX).is_none());
    }

    #[test]
    fn modded_creature_names_classify_and_build_valid_meshes() {
        let cases: &[(&str, EntityRigArchetype)] = &[
            ("iceandfire:fire_dragon", EntityRigArchetype::Avian),
            ("iceandfire:ice_dragon", EntityRigArchetype::Avian),
            ("iceandfire:sea_serpent", EntityRigArchetype::Aquatic),
            ("iceandfire:hippogriff", EntityRigArchetype::Avian),
            ("twilightforest:lich", EntityRigArchetype::Biped),
            ("twilightforest:minoshroom_boss", EntityRigArchetype::Biped),
            ("twilightforest:naga", EntityRigArchetype::GenericProxy),
            ("ars_nouveau:starbuncle", EntityRigArchetype::GenericProxy),
            ("mymod:cave_spider_queen", EntityRigArchetype::Arthropod),
            ("mymod:gelatinous_slime", EntityRigArchetype::SlimeCube),
            ("mymod:great_white_shark", EntityRigArchetype::Aquatic),
            ("mymod:tropical_reef_fish", EntityRigArchetype::Aquatic),
            ("mymod:sky_phoenix", EntityRigArchetype::Avian),
            ("mymod:void_wolf", EntityRigArchetype::Quadruped),
            ("mymod:arcane_bolt", EntityRigArchetype::ProjectileOrItem),
            ("mymod:sky_boat", EntityRigArchetype::VehicleOrHanging),
            (
                "mymod:totally_unknown_florb",
                EntityRigArchetype::ProjectileOrItem,
            ),
            ("custommod:flooble", EntityRigArchetype::GenericProxy),
        ];
        for &(name, expected) in cases {
            let spec = classify_entity_rig_named(name, None, None);
            assert_eq!(spec.archetype, expected, "{name}");
            assert_eq!(spec.entity_type_id, MODDED_ENTITY_TYPE_ID, "{name}");
            assert!(!spec.is_player, "{name}");
            let mesh = build_entity_rig_mesh(spec, EntityPose::default());
            assert_drawable(&mesh);
        }
    }

    #[test]
    fn size_hints_override_family_defaults_when_positive_and_finite() {
        let hinted = classify_entity_rig_named("mymod:stone_golem", Some(3.0), Some(4.5));
        assert_eq!(hinted.archetype, EntityRigArchetype::Biped);
        assert_eq!(hinted.width, 3.0);
        assert_eq!(hinted.height, 4.5);
        for bad in [Some(0.0), Some(-2.0), Some(f32::NAN), None] {
            let spec = classify_entity_rig_named("mymod:stone_golem", bad, bad);
            assert_eq!(spec.width, 0.7, "hint {bad:?}");
            assert_eq!(spec.height, 1.9, "hint {bad:?}");
        }
        // Dragon-family defaults stay large without hints.
        let dragon = classify_entity_rig_named("iceandfire:lightning_dragon", None, None);
        assert_eq!(dragon.width, 2.4);
        assert_eq!(dragon.height, 2.2);
    }

    #[test]
    fn generic_proxy_tint_is_deterministic_name_derived_and_bright() {
        let spec_a = classify_entity_rig_named("custommod:flooble", None, None);
        let spec_b = classify_entity_rig_named("custommod:flooble", None, None);
        let spec_c = classify_entity_rig_named("custommod:zzqxx", None, None);
        assert_eq!(spec_a.name, "custommod:flooble");
        let mesh_a = build_entity_rig_mesh(spec_a, EntityPose::default());
        let mesh_b = build_entity_rig_mesh(spec_b, EntityPose::default());
        let mesh_c = build_entity_rig_mesh(spec_c, EntityPose::default());
        assert_eq!(
            entity_mesh_fingerprint(&mesh_a),
            entity_mesh_fingerprint(&mesh_b)
        );
        assert_ne!(
            entity_mesh_fingerprint(&mesh_a),
            entity_mesh_fingerprint(&mesh_c)
        );
        // Every quad carries the packed sRGB material flag — never the
        // untinted default — and both palette slots appear.
        assert!(mesh_a.quads.iter().all(|q| q.tint_index >= 0x0100_0000));
        let tints: std::collections::BTreeSet<i32> =
            mesh_a.quads.iter().map(|q| q.tint_index).collect();
        assert_eq!(tints.len(), 2, "primary + accent tints expected");
    }

    #[test]
    fn or_fallback_prefers_vanilla_then_name_then_visible_proxy() {
        // Exact vanilla id wins even when a registry name is supplied.
        assert_eq!(
            classify_entity_rig_or_fallback(124, Some("iceandfire:fire_dragon")),
            classify_entity_rig(124).unwrap()
        );
        // Out-of-range id + name resolves through the token classifier.
        let named = classify_entity_rig_or_fallback(500, Some("twilightforest:lich"));
        assert_eq!(named.archetype, EntityRigArchetype::Biped);
        // No name — or an unusable id — still renders instead of vanishing.
        for spec in [
            classify_entity_rig_or_fallback(500, None),
            classify_entity_rig_or_fallback(u32::MAX, None),
            classify_entity_rig_or_fallback(9_999_999, Some("")),
            classify_entity_rig_or_fallback(u32::MAX, Some("custommod:flooble")),
        ] {
            let mesh = build_entity_rig_mesh(spec, EntityPose::default());
            assert_drawable(&mesh);
        }
        assert_eq!(
            classify_entity_rig_or_fallback(u32::MAX, Some("custommod:flooble")).archetype,
            EntityRigArchetype::GenericProxy
        );
    }
}
