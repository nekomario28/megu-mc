// SPDX-License-Identifier: LGPL-3.0-or-later

//! Deterministic articulated entity meshes for Minecraft Java 1.21.1 (protocol 767).
//! Geometry uses entity-local block units, with feet at y = 0 and the origin
//! centered horizontally. The 130 registered dimensions are collision sizes,
//! not promises of exact Vanilla renderer model geometry.

use std::f32::consts::{PI, TAU};

use crate::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad, MAX_UPGRADE_QUADS};

/// Eight broad families of reusable procedural rigs.
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
            Self::ProjectileOrItem | Self::VehicleOrHanging => RigProportions {
                head: 0.0,
                torso: 1.0,
                limbs: 0.0,
            },
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
    MOUTH_ITEM_TINT_FLAG
        | (i32::from(rgb[0]) << 16)
        | (i32::from(rgb[1]) << 8)
        | i32::from(rgb[2])
}

#[derive(Clone, Copy)]
struct RigBox {
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    uv: [f32; 4],
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
            uv: value.uv,
            inflate: value.inflate,
            tint: RIG_TINT_DEFAULT,
        }
    }
}

fn articulation(part: EntityRigPart, pose: EntityPose) -> [f32; 3] {
    use EntityRigPart::*;
    let stride = pose.limb_swing.cos() * pose.limb_swing_amount * 0.8;
    let attack = (pose.swing_progress * PI).sin() * 1.2;
    let flight = if pose.elytra_flying {
        -1.15
    } else if pose.swimming {
        -0.75
    } else {
        0.0
    };
    let crouch = if pose.sneaking { 0.32 } else { 0.0 };
    let flap = (pose.age_ticks * 0.7 + pose.limb_swing).sin() * 0.5;
    let tail = (pose.age_ticks * 0.35 + pose.limb_swing).sin() * 0.35;
    match part {
        Head => [
            pose.head_pitch_deg.to_radians(),
            pose.head_yaw_deg.to_radians(),
            0.0,
        ],
        Torso => [
            crouch
                + if pose.swimming || pose.elytra_flying {
                    0.25
                } else {
                    0.0
                },
            0.0,
            0.0,
        ],
        LeftArm => [-stride + flight, 0.0, 0.0],
        RightArm => [stride - attack + flight, 0.0, 0.0],
        LeftLeg => [stride * 0.85 + crouch * 0.4, 0.0, 0.0],
        RightLeg => [-stride * 0.85 + crouch * 0.4, 0.0, 0.0],
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
        let joint_y = height * 0.70;
        [
            point[0] * 0.75,
            joint_y * 0.5 + (point[1] - joint_y) * 0.75,
            point[2] * 0.75,
        ]
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

fn emit_box(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityPose, shape: RigBox) {
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
    let [u0, v0, u1, v1] = shape.uv;
    let uv = [[u0, v1], [u1, v1], [u1, v0], [u0, v0]];
    let animation = articulation(shape.part, pose);
    for indices in FACES {
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
            *dest = baby_scale(shape.part, p, spec.height, pose.baby);
        }
        if let Some(normal) = outward_normal(&positions) {
            if positions.iter().flatten().all(|v| v.is_finite()) {
                mesh.quads.push(ExtrudedQuad {
                    positions,
                    normal,
                    uv,
                    tint_index: if shape.tint != RIG_TINT_DEFAULT {
                        shape.tint
                    } else if pose.hurt_flash {
                        1
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
    pose: EntityPose,
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
            uv: [0.0, 0.0, 1.0, 1.0],
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
    pose: EntityPose,
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
            uv: [0.0, 0.0, 1.0, 1.0],
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
    pose: EntityPose,
    mouth_item_rgb: Option<[u8; 3]>,
) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let head_pivot = [0.0, h * 0.74, -w * 0.26];

    add(
        mesh,
        spec,
        pose,
        Torso,
        [0.0, h * 0.58, w * 0.06],
        [w * 0.56, h * 0.50, w * 0.82],
        [0.0, h * 0.72, 0.0],
        [0.0; 3],
        0.0,
    );
    add(
        mesh,
        spec,
        pose,
        Head,
        [0.0, h * 0.80, -w * 0.36],
        [w * 0.48, h * 0.32, w * 0.40],
        head_pivot,
        [0.0; 3],
        0.0,
    );
    // Snout: a narrower muzzle box protruding forward from the head.
    add(
        mesh,
        spec,
        pose,
        Head,
        [0.0, h * 0.74, -w * 0.60],
        [w * 0.26, h * 0.18, w * 0.34],
        head_pivot,
        [0.0; 3],
        0.0,
    );
    // Two pointed ears: a tilted base slab plus a smaller, more tilted tip.
    for side in [-1.0_f32, 1.0] {
        add(
            mesh,
            spec,
            pose,
            Head,
            [side * w * 0.16, h * 0.95, -w * 0.36],
            [w * 0.10, h * 0.16, w * 0.08],
            head_pivot,
            [0.0, 0.0, -side * 0.45],
            0.0,
        );
        add(
            mesh,
            spec,
            pose,
            Head,
            [side * w * 0.23, h * 1.00, -w * 0.36],
            [w * 0.07, h * 0.12, w * 0.06],
            head_pivot,
            [0.0, 0.0, -side * 0.95],
            0.0,
        );
    }
    for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
        for z in [-0.28_f32, 0.30] {
            let pivot = [side * w * 0.23, h * 0.40, z * w];
            add(
                mesh,
                spec,
                pose,
                part,
                [pivot[0], h * 0.21, pivot[2]],
                [w * 0.15, h * 0.42, w * 0.17],
                pivot,
                [0.0; 3],
                0.0,
            );
        }
    }
    // Bushy tail: a thick angled base segment and a smaller raised tip,
    // both bound to `Tail` so it sways with the pose.
    let tail_pivot = [0.0, h * 0.60, w * 0.42];
    add(
        mesh,
        spec,
        pose,
        Tail,
        [0.0, h * 0.64, w * 0.58],
        [w * 0.20, h * 0.18, w * 0.36],
        tail_pivot,
        [-0.55, 0.0, 0.0],
        0.0,
    );
    add(
        mesh,
        spec,
        pose,
        Tail,
        [0.0, h * 0.76, w * 0.72],
        [w * 0.14, h * 0.14, w * 0.24],
        tail_pivot,
        [-0.95, 0.0, 0.0],
        0.0,
    );
    if let Some(rgb) = mouth_item_rgb {
        add_tinted(
            mesh,
            spec,
            pose,
            Head,
            [0.0, h * 0.70, -w * 0.80],
            [w * 0.14, h * 0.09, w * 0.26],
            head_pivot,
            [0.5, 0.0, 0.0],
            0.0,
            mouth_item_tint(rgb),
        );
    }
}

/// Snow golem (id 96): stacked lower/upper snow cubes, a pumpkin head,
/// and two angled stick arms.
fn add_snow_golem(mesh: &mut ExtrudedMesh, spec: EntityRigSpec, pose: EntityPose) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;

    add(
        mesh,
        spec,
        pose,
        Torso,
        [0.0, h * 0.27, 0.0],
        [w * 0.92, h * 0.54, w * 0.92],
        [0.0, h * 0.55, 0.0],
        [0.0; 3],
        0.0,
    );
    add(
        mesh,
        spec,
        pose,
        Torso,
        [0.0, h * 0.60, 0.0],
        [w * 0.78, h * 0.34, w * 0.78],
        [0.0, h * 0.55, 0.0],
        [0.0; 3],
        0.0,
    );
    // Pumpkin head: same box geometry, distinct accent tint slot.
    add_tinted(
        mesh,
        spec,
        pose,
        Head,
        [0.0, h * 0.83, 0.0],
        [w * 0.56, h * 0.30, w * 0.56],
        [0.0, h * 0.68, 0.0],
        [0.0; 3],
        0.0,
        7,
    );
    // Long thin arms angled outward from the shoulders.
    for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
        let pivot = [side * w * 0.40, h * 0.70, 0.0];
        add(
            mesh,
            spec,
            pose,
            part,
            [side * w * 0.50, h * 0.44, 0.0],
            [w * 0.11, h * 0.52, w * 0.11],
            pivot,
            [0.0, 0.0, side * 0.35],
            0.0,
        );
    }
}

/// Thrown spherical projectile proxy: an outer shell cube sized to the
/// collision box, a smaller inner cube yawed 45 degrees for a faceted
/// silhouette, and a compact core. `tint` selects a small procedural
/// accent palette slot per projectile family.
fn add_thrown_projectile(
    mesh: &mut ExtrudedMesh,
    spec: EntityRigSpec,
    pose: EntityPose,
    entity_type_id: u16,
) {
    use EntityRigPart::*;
    let w = spec.width;
    let h = spec.height;
    let tint = match entity_type_id {
        97 => 2,  // snowball: crisp ice white
        28 => 3,  // egg
        32 => 4,  // ender_pearl
        117 | 13 => 5, // wind_charge / breeze_wind_charge
        _ => 6,   // fireball / small_fireball: flame accent
    };
    let center = [0.0, h * 0.5, 0.0];
    add_tinted(
        mesh, spec, pose, Core, center, [w * 0.94, h * 0.94, w * 0.94], [0.0; 3],
        [0.0; 3], 0.0, tint,
    );
    add_tinted(
        mesh, spec, pose, Shell, center, [w * 0.70, h * 0.70, w * 0.70], [0.0; 3],
        [0.0, PI * 0.25, 0.0], 0.0, tint + 1,
    );
    add_tinted(
        mesh, spec, pose, Core, center, [w * 0.38, h * 0.38, w * 0.38], [0.0; 3],
        [0.0; 3], 0.0, tint,
    );
}

/// Build the shared procedural template for all 130 vanilla protocol IDs.
/// Zero-size collision entities receive small drawable placeholders; skin
/// overlay alpha/blending is a responsibility of the consuming renderer.
pub fn build_entity_rig_mesh(spec: EntityRigSpec, pose: EntityPose) -> ExtrudedMesh {
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
    build_entity_rig_mesh_inner(spec, pose, mouth_item_rgb)
}

fn build_entity_rig_mesh_inner(
    spec: EntityRigSpec,
    pose: EntityPose,
    mouth_item_rgb: Option<[u8; 3]>,
) -> ExtrudedMesh {
    use EntityRigArchetype::*;
    use EntityRigPart::*;
    let pose = pose.sanitized();
    let mut mesh = ExtrudedMesh::new();
    let w = finite_or_zero(spec.width).clamp(0.125, 32.0);
    let h = finite_or_zero(spec.height).clamp(0.125, 32.0);
    let spec = EntityRigSpec {
        height: h,
        width: w,
        ..spec
    };
    match spec.archetype {
        Biped if spec.entity_type_id == 96 => {
            add_snow_golem(&mut mesh, spec, pose);
        }
        Biped => {
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
            add(
                &mut mesh, spec, pose, Head, head.0, head.1, head.2, [0.0; 3], 0.0,
            );
            add(
                &mut mesh, spec, pose, Torso, torso.0, torso.1, torso.2, [0.0; 3], 0.0,
            );
            for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
                let pivot = [side * w * 0.43, h * 0.735, 0.0];
                let size = [w * 0.22, h * 0.37, w * 0.30];
                let center = [pivot[0], h * 0.55, 0.0];
                add(
                    &mut mesh, spec, pose, part, center, size, pivot, [0.0; 3], 0.0,
                );
            }
            for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
                let pivot = [side * w * 0.18, h * 0.35, 0.0];
                let size = [w * 0.27, h * spec.proportions.limbs, w * 0.34];
                let center = [pivot[0], h * 0.175, 0.0];
                add(
                    &mut mesh, spec, pose, part, center, size, pivot, [0.0; 3], 0.0,
                );
            }
            if pose.enable_skin_3d_layers {
                add(
                    &mut mesh,
                    spec,
                    pose,
                    Head,
                    head.0,
                    head.1,
                    head.2,
                    [0.0; 3],
                    w * 0.035,
                );
                add(
                    &mut mesh,
                    spec,
                    pose,
                    Torso,
                    torso.0,
                    torso.1,
                    torso.2,
                    [0.0; 3],
                    w * 0.015,
                );
                for (part, side) in [(LeftArm, -1.0_f32), (RightArm, 1.0)] {
                    let pivot = [side * w * 0.43, h * 0.735, 0.0];
                    add(
                        &mut mesh,
                        spec,
                        pose,
                        part,
                        [pivot[0], h * 0.55, 0.0],
                        [w * 0.22, h * 0.37, w * 0.30],
                        pivot,
                        [0.0; 3],
                        w * 0.02,
                    );
                }
                for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
                    let pivot = [side * w * 0.18, h * 0.35, 0.0];
                    add(
                        &mut mesh,
                        spec,
                        pose,
                        part,
                        [pivot[0], h * 0.175, 0.0],
                        [w * 0.27, h * spec.proportions.limbs, w * 0.34],
                        pivot,
                        [0.0; 3],
                        w * 0.02,
                    );
                }
            }
        }
        Quadruped if spec.entity_type_id == 42 => {
            add_fox(&mut mesh, spec, pose, mouth_item_rgb);
        }
        Quadruped => {
            add(
                &mut mesh,
                spec,
                pose,
                Torso,
                [0.0, h * 0.65, w * 0.08],
                [w * 0.75, h * spec.proportions.torso, w * 0.80],
                [0.0, h * 0.75, 0.0],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Head,
                [0.0, h * 0.77, -w * 0.34],
                [w * 0.56, h * spec.proportions.head, w * 0.37],
                [0.0, h * 0.76, -w * 0.24],
                [0.0; 3],
                0.0,
            );
            for (part, x) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
                for z in [-0.25_f32, 0.30] {
                    let pivot = [x * w * 0.27, h * 0.42, z * w];
                    add(
                        &mut mesh,
                        spec,
                        pose,
                        part,
                        [pivot[0], h * 0.21, pivot[2]],
                        [w * 0.20, h * spec.proportions.limbs, w * 0.21],
                        pivot,
                        [0.0; 3],
                        0.0,
                    );
                }
            }
        }
        Avian => {
            add(
                &mut mesh,
                spec,
                pose,
                Torso,
                [0.0, h * 0.43, 0.0],
                [w * 0.50, h * spec.proportions.torso, w * 0.62],
                [0.0, h * 0.72, 0.0],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Head,
                [0.0, h * 0.82, -w * 0.10],
                [w * 0.42, h * spec.proportions.head, w * 0.45],
                [0.0, h * 0.72, 0.0],
                [0.0; 3],
                0.0,
            );
            for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
                let pivot = [side * w * 0.20, h * 0.60, 0.0];
                add(
                    &mut mesh,
                    spec,
                    pose,
                    part,
                    [side * w * 0.34, h * 0.55, 0.0],
                    [w * 0.30, h * spec.proportions.limbs, w * 0.57],
                    pivot,
                    [0.0; 3],
                    0.0,
                );
            }
        }
        Arthropod => {
            add(
                &mut mesh,
                spec,
                pose,
                Head,
                [0.0, h * 0.53, -w * 0.30],
                [w * 0.38, h * spec.proportions.head, w * 0.27],
                [0.0, h * 0.52, -w * 0.2],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Torso,
                [0.0, h * 0.50, 0.0],
                [w * 0.44, h * spec.proportions.torso, w * 0.39],
                [0.0, h * 0.60, 0.0],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Core,
                [0.0, h * 0.58, w * 0.27],
                [w * 0.56, h * 0.53, w * 0.44],
                [0.0, h * 0.60, 0.0],
                [0.0; 3],
                0.0,
            );
            for (part, side) in [(LeftLeg, -1.0_f32), (RightLeg, 1.0)] {
                for index in 0..4 {
                    let z = (index as f32 - 1.5) * w * 0.16;
                    let pivot = [side * w * 0.24, h * 0.54, z];
                    add(
                        &mut mesh,
                        spec,
                        pose,
                        part,
                        [side * w * 0.34, h * 0.36, z],
                        [w * 0.40, h * spec.proportions.limbs, w * 0.10],
                        pivot,
                        [0.0, (index as f32 - 1.5) * 0.20, side * 0.40],
                        0.0,
                    );
                }
            }
        }
        SlimeCube => {
            add(
                &mut mesh,
                spec,
                pose,
                Core,
                [0.0, h * 0.5, 0.0],
                [w * 0.60, h * 0.60, w * 0.60],
                [0.0; 3],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Shell,
                [0.0, h * 0.5, 0.0],
                [w, h, w],
                [0.0; 3],
                [0.0; 3],
                0.0,
            );
        }
        Aquatic => {
            add(
                &mut mesh,
                spec,
                pose,
                Torso,
                [0.0, h * 0.53, 0.0],
                [w * 0.65, h * spec.proportions.torso, w * 0.67],
                [0.0, h * 0.53, 0.0],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Head,
                [0.0, h * 0.55, -w * 0.30],
                [w * 0.50, h * spec.proportions.head, w * 0.33],
                [0.0, h * 0.54, -w * 0.18],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Tail,
                [0.0, h * 0.56, w * 0.37],
                [w * 0.23, h * 0.48, w * 0.27],
                [0.0, h * 0.54, w * 0.24],
                [0.0; 3],
                0.0,
            );
            for (part, side) in [(LeftWing, -1.0_f32), (RightWing, 1.0)] {
                let pivot = [side * w * 0.29, h * 0.52, 0.0];
                add(
                    &mut mesh,
                    spec,
                    pose,
                    part,
                    [side * w * 0.40, h * 0.44, 0.0],
                    [w * 0.15, h * spec.proportions.limbs, w * 0.35],
                    pivot,
                    [0.0; 3],
                    0.0,
                );
            }
        }
        ProjectileOrItem => match spec.entity_type_id {
            // Thrown spherical projectiles get a layered faceted proxy
            // instead of the bare collision box. Arrow (4) and the other
            // item-like ids keep the generic template.
            13 | 28 | 32 | 62 | 94 | 97 | 117 => {
                add_thrown_projectile(&mut mesh, spec, pose, spec.entity_type_id);
            }
            _ => {
                add(
                    &mut mesh,
                    spec,
                    pose,
                    Core,
                    [0.0, h * 0.50, 0.0],
                    [w, h, w],
                    [0.0; 3],
                    [0.0; 3],
                    0.0,
                );
            }
        },
        VehicleOrHanging => {
            add(
                &mut mesh,
                spec,
                pose,
                Core,
                [0.0, h * 0.36, 0.0],
                [w, h * 0.45, w],
                [0.0; 3],
                [0.0; 3],
                0.0,
            );
            add(
                &mut mesh,
                spec,
                pose,
                Shell,
                [0.0, h * 0.66, 0.0],
                [w * 0.80, h * 0.22, w * 0.80],
                [0.0; 3],
                [0.0; 3],
                0.0,
            );
        }
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
    let pose = pose.sanitized();
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
        build_entity_rig_mesh(spec, pose)
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
    emit_colossal_box(
        mesh,
        s,
        [0.0, 1.05, 0.4],
        [0.0, 1.05, 0.4],
        [1.2, 1.0, 3.4],
        [0.0; 3],
        primary,
    );
    emit_colossal_box(
        mesh,
        s,
        [0.0, 0.6, 0.4],
        [0.0, 0.6, 0.4],
        [0.85, 0.14, 3.2],
        [0.0; 3],
        accent,
    );

    // Three-segment neck rising toward the horned head.
    for (pivot, center, size, pitch) in [
        ([0.0, 1.5, -1.2], [0.0, 1.72, -1.5], [0.6, 0.7, 0.8], -0.55),
        ([0.0, 1.9, -1.75], [0.0, 2.1, -2.0], [0.55, 0.62, 0.7], -0.6),
        ([0.0, 2.35, -2.2], [0.0, 2.5, -2.45], [0.5, 0.55, 0.6], -0.65),
    ] {
        emit_colossal_box(mesh, s, pivot, center, size, [pitch, 0.0, 0.0], primary);
    }

    // Head, snout, and jaw share the head yaw/pitch articulation.
    let head_pivot = [0.0, 2.62, -2.6];
    let head_rot = [head_pitch - 0.1, head_yaw, 0.0];
    emit_colossal_box(
        mesh,
        s,
        head_pivot,
        [0.0, 2.78, -2.75],
        [0.58, 0.52, 0.85],
        head_rot,
        primary,
    );
    emit_colossal_box(
        mesh,
        s,
        head_pivot,
        [0.0, 2.7, -3.25],
        [0.4, 0.34, 0.55],
        head_rot,
        primary,
    );
    emit_colossal_box(
        mesh,
        s,
        [0.0, 2.58, -2.7],
        [0.0, 2.52, -3.0],
        [0.44, 0.18, 0.7],
        [head_pitch + 0.4, head_yaw, 0.0],
        accent,
    );
    for side in [-1.0_f32, 1.0] {
        emit_colossal_box(
            mesh,
            s,
            [side * 0.2, 3.0, -2.5],
            [side * 0.2, 3.0, -2.5],
            [0.12, 0.45, 0.14],
            [0.7, 0.0, -side * 0.25],
            accent,
        );
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
            emit_colossal_box(
                mesh,
                s,
                hip,
                center,
                size,
                [swing, 0.0, 0.0],
                primary,
            );
        }
    }

    // Four-segment tapering tail with per-segment sinusoidal sway.
    for i in 0..4 {
        let f = i as f32;
        let radius = 0.4 * (1.0 - f * 0.16);
        let y = 1.15 + f * 0.18 - f * f * 0.02;
        let z = 2.15 + f * 0.85;
        let pivot = if i == 0 {
            [0.0, 1.15, 2.15]
        } else {
            [0.0, 1.15 + (f - 1.0) * 0.18 - (f - 1.0) * (f - 1.0) * 0.02, z - 0.85]
        };
        let sway = (pose.limb_swing - f * 0.55).sin() * 0.14 * (f + 1.0) * 0.45;
        emit_colossal_box(
            mesh,
            s,
            pivot,
            [0.0, y, z + 0.45],
            [radius * 2.0, radius * 2.0, 0.95],
            [0.06 * f, sway, 0.0],
            primary,
        );
        emit_colossal_box(
            mesh,
            s,
            [0.0, y, z + 0.35],
            [0.0, y + radius + 0.1, z + 0.3],
            [0.12, 0.28, 0.22],
            [-0.2, 0.0, 0.0],
            accent,
        );
    }

    // Dorsal spikes along the torso.
    for z in [-1.0, -0.2, 0.6, 1.4] {
        emit_colossal_box(
            mesh,
            s,
            [0.0, 1.58, z],
            [0.0, 1.58, z],
            [0.16, 0.4, 0.2],
            [-0.15, 0.0, 0.0],
            accent,
        );
    }

    // Two multi-segment wings: inner arm + swept membrane + wingtip.
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
        emit_colossal_box(
            mesh,
            s,
            shoulder,
            [side * 1.45, 1.52, -0.55],
            [1.7, 0.2, 0.5],
            inner_rot,
            primary,
        );
        emit_colossal_box(
            mesh,
            s,
            [side * 1.45, 1.64, -0.6],
            [side * 1.45, 1.64, -0.6],
            [1.6, 0.06, 0.12],
            inner_rot,
            accent,
        );
        let elbow = [side * 2.15, 1.5, -0.85];
        let mid_rot = [0.0, -side * 0.42, side * (flap * 0.9 + fold * 0.7)];
        emit_colossal_box(
            mesh,
            s,
            elbow,
            [side * 3.3, 1.42, -0.35],
            [2.4, 0.09, 1.5],
            mid_rot,
            accent,
        );
        let tip_pivot = [side * 4.3, 1.4, -0.15];
        let tip_rot = [0.0, -side * 0.6, side * (flap * 1.15 + fold * 0.9)];
        emit_colossal_box(
            mesh,
            s,
            tip_pivot,
            [side * 4.9, 1.38, 0.45],
            [1.5, 0.08, 1.0],
            tip_rot,
            accent,
        );
        emit_colossal_box(
            mesh,
            s,
            [side * 5.4, 1.36, 0.75],
            [side * 5.4, 1.36, 0.75],
            [0.5, 0.07, 0.35],
            tip_rot,
            accent,
        );
    }
}

/// Sea serpent / hydra template. Unit space: head toward -Z, body floats
/// around y = 0.8 so the silhouette stays near the origin, `stage_scale`
/// applied inside [`emit_colossal_box`].
fn build_sea_serpent(
    mesh: &mut ExtrudedMesh,
    s: f32,
    pose: EntityPose,
    primary: i32,
    accent: i32,
) {
    let head_pitch = pose.head_pitch_deg.to_radians();
    let head_yaw = pose.head_yaw_deg.to_radians();
    let head_rot = [head_pitch - 0.05, head_yaw, 0.0];

    // Fanged aquatic head with crest and jaw.
    let head_pivot = [0.0, 0.85, -2.7];
    emit_colossal_box(
        mesh,
        s,
        head_pivot,
        [0.0, 0.85, -2.95],
        [0.75, 0.65, 1.0],
        head_rot,
        primary,
    );
    emit_colossal_box(
        mesh,
        s,
        head_pivot,
        [0.0, 0.78, -3.55],
        [0.5, 0.4, 0.6],
        head_rot,
        primary,
    );
    emit_colossal_box(
        mesh,
        s,
        [0.0, 0.62, -2.85],
        [0.0, 0.55, -3.3],
        [0.55, 0.16, 0.7],
        [head_pitch + 0.3, head_yaw, 0.0],
        accent,
    );
    for side in [-1.0_f32, 1.0] {
        emit_colossal_box(
            mesh,
            s,
            [side * 0.14, 0.52, -3.45],
            [side * 0.14, 0.52, -3.45],
            [0.07, 0.22, 0.07],
            head_rot,
            accent,
        );
        emit_colossal_box(
            mesh,
            s,
            [side * 0.42, 0.72, -2.55],
            [side * 0.62, 0.68, -2.5],
            [0.5, 0.06, 0.35],
            [head_pitch, head_yaw, side * 0.5],
            accent,
        );
    }
    emit_colossal_box(
        mesh,
        s,
        [0.0, 1.22, -2.9],
        [0.0, 1.22, -2.9],
        [0.12, 0.4, 0.7],
        [0.0, head_yaw, 0.0],
        accent,
    );

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
        emit_colossal_box(
            mesh,
            s,
            center,
            center,
            [radius * 2.0, radius * 1.8, 0.85],
            [0.0, sway, 0.0],
            primary,
        );
        // Dorsal fin crest along the mid-body segments.
        if (1..=6).contains(&i) {
            emit_colossal_box(
                mesh,
                s,
                center,
                [lat, 0.8 + vert + radius * 0.9 + 0.12, z],
                [0.1, 0.3 + f * 0.04, 0.4],
                [0.0, sway, 0.0],
                accent,
            );
        }
    }

    // Forked tail fin: two accent blades angled into a V at the tip.
    let tail_z = -1.9 + 7.0 * 0.8 + 0.5;
    let tail_y = 0.8 + (pose.limb_swing - 7.0 * 0.65).cos() * (0.06 + 7.0 * 0.05);
    for (dy, pitch) in [(0.45, -0.5), (-0.4, 0.5)] {
        emit_colossal_box(
            mesh,
            s,
            [0.0, tail_y, tail_z - 0.3],
            [0.0, tail_y + dy, tail_z + 0.5],
            [0.1, 1.0, 0.6],
            [pitch, 0.0, 0.0],
            accent,
        );
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
            let span = [
                hi[0] - lo[0],
                hi[1] - lo[1],
                hi[2] - lo[2],
            ];
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
    fn existing_archetype_rigs_are_unchanged() {
        // Biped (128), generic quadruped (22), avian (19), slime (93),
        // aquatic (20), vehicle (10): fixed quad counts pin the templates
        // against accidental drift.
        let expected = [
            (128_u16, 36_usize),
            (22, 36),
            (19, 24),
            (100, 66),
            (93, 12),
            (20, 30),
            (10, 12),
            (4, 6), // arrow keeps the generic single-box proxy
        ];
        for (id, quads) in expected {
            let spec = classify_entity_rig(id).unwrap();
            let mesh = build_entity_rig_mesh(spec, EntityPose::default());
            assert_eq!(mesh.quads.len(), quads, "id {id}");
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
        assert_ne!(entity_mesh_fingerprint(&a), entity_mesh_fingerprint(&untinted));
        verify_colossal_mesh(&a);
        verify_colossal_mesh(&untinted);
    }

    #[test]
    fn colossal_stage_scale_sanitizes_non_finite_and_nonpositive() {
        for kind in [ColossalRigKind::WingedDragon, ColossalRigKind::SeaSerpent] {
            for scale in [f32::NAN, f32::INFINITY, 0.0, -2.5, f32::NEG_INFINITY] {
                let mesh =
                    build_colossal_rig_mesh(kind, EntityPose::default(), scale, [1, 2, 3], [4, 5, 6]);
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
