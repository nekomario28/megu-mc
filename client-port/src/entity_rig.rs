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

#[derive(Clone, Copy)]
struct RigBox {
    part: EntityRigPart,
    center: [f32; 3],
    size: [f32; 3],
    pivot: [f32; 3],
    rotation: [f32; 3],
    uv: [f32; 4],
    inflate: f32,
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
                    tint_index: if pose.hurt_flash { 1 } else { -1 },
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
        },
    );
}

/// Build the shared procedural template for all 130 vanilla protocol IDs.
/// Zero-size collision entities receive small drawable placeholders; skin
/// overlay alpha/blending is a responsibility of the consuming renderer.
pub fn build_entity_rig_mesh(spec: EntityRigSpec, pose: EntityPose) -> ExtrudedMesh {
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
        ProjectileOrItem => {
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

/// Stable FNV-1a hash of ordered quad topology, floating-point bits, UVs,
/// normals, and tint. Uses no platform hasher or timing information.
pub fn entity_mesh_fingerprint(mesh: &ExtrudedMesh) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    };
    feed(&(mesh.quads.len() as u64).to_le_bytes());
    for quad in &mesh.quads {
        for value in quad
            .positions
            .iter()
            .flatten()
            .chain(quad.normal.iter())
            .chain(quad.uv.iter().flatten())
        {
            feed(&value.to_bits().to_le_bytes());
        }
        feed(&quad.tint_index.to_le_bytes());
    }
    hash
}
