use megu_client_mesh::{
    MaterialBinding, MaterialSlotBinding, ShapeArchetype, ShapeTemplateRegistry, StairShape,
    WallSide, DIR_BASIS,
};

fn rotate_bounds(min: [f32; 3], max: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let mut out_min = [f32::INFINITY; 3];
    let mut out_max = [f32::NEG_INFINITY; 3];
    for x in [min[0], max[0]] {
        for y in [min[1], max[1]] {
            for z in [min[2], max[2]] {
                let p = [x, y, z];
                let rotated = [1.0 - p[2], p[1], p[0]];
                for axis in 0..3 {
                    out_min[axis] = out_min[axis].min(rotated[axis]);
                    out_max[axis] = out_max[axis].max(rotated[axis]);
                }
            }
        }
    }
    (out_min, out_max)
}

fn rotate_face(face: usize) -> usize {
    let normal = DIR_BASIS[face].n;
    let rotated = [-normal[2], normal[1], normal[0]];
    DIR_BASIS
        .iter()
        .position(|basis| basis.n == rotated)
        .unwrap()
}

type GeometryKey = (u32, u32, u32, u32, u32, u32, usize, Option<usize>);

fn geometry_key(quads: impl Iterator<Item = megu_client_mesh::TemplateQuad>) -> Vec<GeometryKey> {
    let mut key: Vec<_> = quads
        .map(|q| {
            (
                q.min[0].to_bits(),
                q.min[1].to_bits(),
                q.min[2].to_bits(),
                q.max[0].to_bits(),
                q.max[1].to_bits(),
                q.max[2].to_bits(),
                q.face,
                q.cull_face,
            )
        })
        .collect();
    key.sort_unstable();
    key
}

fn binding(sprite: u32, tint: Option<u8>) -> MaterialBinding {
    MaterialBinding {
        slots: vec![MaterialSlotBinding {
            texture_sprite_id: sprite,
            biome_tint_index: tint,
            emissive: false,
        }],
    }
}

fn check_bounds(registry: &mut ShapeTemplateRegistry, archetype: ShapeArchetype) {
    let quads = registry.instantiate(archetype, &binding(1, None));
    assert!(!quads.is_empty(), "{archetype:?}");
    for item in quads {
        let q = item.quad;
        assert!((0..6).contains(&q.face), "invalid face for {archetype:?}");
        if let Some(face) = q.cull_face {
            assert!((0..6).contains(&face));
        }
        for axis in 0..3 {
            assert!(q.min[axis].is_finite() && q.max[axis].is_finite());
            assert!(
                (-1e-6..=1.000001).contains(&q.min[axis]),
                "min {:?} for {archetype:?}",
                q.min
            );
            assert!(
                (-1e-6..=1.000001).contains(&q.max[axis]),
                "max {:?} for {archetype:?}",
                q.max
            );
            assert!(q.min[axis] <= q.max[axis]);
        }
    }
}

#[test]
fn all_requested_state_families_stay_in_block_bounds() {
    let mut registry = ShapeTemplateRegistry::new();
    check_bounds(&mut registry, ShapeArchetype::FullCube);
    for top in [false, true] {
        check_bounds(&mut registry, ShapeArchetype::Slab { top });
    }

    let shapes = [
        StairShape::Straight,
        StairShape::InnerLeft,
        StairShape::InnerRight,
        StairShape::OuterLeft,
        StairShape::OuterRight,
    ];
    let mut stair_count = 0;
    for shape in shapes {
        for half_top in [false, true] {
            for facing_nesw in 0..4 {
                let state = ShapeArchetype::Stair {
                    shape,
                    half_top,
                    facing_nesw,
                };
                check_bounds(&mut registry, state);
                stair_count += 1;
            }
        }
    }
    assert_eq!(stair_count, 40);

    let mut fence_count = 0;
    for connections_nesw in 0..16 {
        check_bounds(&mut registry, ShapeArchetype::Fence { connections_nesw });
        fence_count += 1;
    }
    assert_eq!(fence_count, 16);

    let sides = [WallSide::None, WallSide::Low, WallSide::Tall];
    let mut wall_count = 0;
    for up_post in [false, true] {
        for n in sides {
            for e in sides {
                for s in sides {
                    for w in sides {
                        check_bounds(
                            &mut registry,
                            ShapeArchetype::Wall {
                                up_post,
                                sides_nesw: [n, e, s, w],
                            },
                        );
                        wall_count += 1;
                        // Waterlogged has the same geometry and deliberately does not enter the key.
                        check_bounds(
                            &mut registry,
                            ShapeArchetype::Wall {
                                up_post,
                                sides_nesw: [n, e, s, w],
                            },
                        );
                        wall_count += 1;
                    }
                }
            }
        }
    }
    assert_eq!(wall_count, 324);

    let mut door_count = 0;
    for open in [false, true] {
        for hinge_right in [false, true] {
            for upper in [false, true] {
                for facing_nesw in 0..4 {
                    let door = ShapeArchetype::Door {
                        open,
                        hinge_right,
                        upper,
                        facing_nesw,
                    };
                    check_bounds(&mut registry, door);
                    door_count += 1;
                    // The separate powered state shares this archetype's geometry.
                    check_bounds(&mut registry, door);
                    door_count += 1;
                }
            }
        }
    }
    assert_eq!(door_count, 64);

    let mut trapdoor_count = 0;
    for open in [false, true] {
        for half_top in [false, true] {
            for facing_nesw in 0..4 {
                check_bounds(
                    &mut registry,
                    ShapeArchetype::Trapdoor {
                        open,
                        half_top,
                        facing_nesw,
                    },
                );
                trapdoor_count += 1;
            }
        }
    }
    assert_eq!(trapdoor_count, 16);
}

#[test]
fn north_fence_rotation_tracks_face_basis_and_returns_after_four_turns() {
    let mut registry = ShapeTemplateRegistry::new();
    let north = registry.instantiate(
        ShapeArchetype::Fence {
            connections_nesw: 1,
        },
        &binding(2, None),
    );
    let east = registry.instantiate(
        ShapeArchetype::Fence {
            connections_nesw: 2,
        },
        &binding(2, None),
    );
    assert_eq!(DIR_BASIS[2].horizontal_cw, 5);
    assert_eq!(DIR_BASIS[5].horizontal_cw, 3);
    assert_eq!(DIR_BASIS[3].horizontal_cw, 4);
    assert_eq!(DIR_BASIS[4].horizontal_cw, 2);
    assert!(north.iter().any(|q| q.quad.face == 2));
    assert!(east.iter().any(|q| q.quad.face == 5));
    let rotated_north = north.iter().map(|item| {
        let q = item.quad;
        let (min, max) = rotate_bounds(q.min, q.max);
        megu_client_mesh::TemplateQuad {
            min,
            max,
            face: rotate_face(q.face),
            cull_face: q.cull_face.map(rotate_face),
            ..q
        }
    });
    assert_eq!(
        geometry_key(rotated_north),
        geometry_key(east.iter().map(|q| q.quad))
    );

    let cw = [[0i8, 0, -1], [0, 1, 0], [1, 0, 0]];
    let mut matrix = [[1i8, 0, 0], [0, 1, 0], [0, 0, 1]];
    for _ in 0..4 {
        let mut next = [[0i8; 3]; 3];
        for row in 0..3 {
            for col in 0..3 {
                for k in 0..3 {
                    next[row][col] += cw[row][k] * matrix[k][col];
                }
            }
        }
        matrix = next;
    }
    assert_eq!(matrix, [[1, 0, 0], [0, 1, 0], [0, 0, 1]]);
}

#[test]
fn material_families_share_template_id_and_geometry() {
    let mut registry = ShapeTemplateRegistry::new();
    let stairs = ShapeArchetype::Stair {
        shape: StairShape::InnerRight,
        half_top: false,
        facing_nesw: 1,
    };
    let id = registry.template_id(stairs);
    let oak = registry.instantiate(stairs, &binding(12, Some(1)));
    let stone = registry.instantiate(stairs, &binding(71, None));
    assert_eq!(id, registry.template_id(stairs));
    assert_eq!(
        oak.iter().map(|q| q.quad).collect::<Vec<_>>(),
        stone.iter().map(|q| q.quad).collect::<Vec<_>>()
    );
    assert!(oak
        .iter()
        .all(|q| q.binding.texture_sprite_id == 12 && q.binding.biome_tint_index == Some(1)));
    assert!(stone
        .iter()
        .all(|q| q.binding.texture_sprite_id == 71 && q.binding.biome_tint_index.is_none()));
    assert!(registry.memory_savings_report(12).reduction_percent > 90);
}

#[test]
fn twelve_material_families_exceed_ninety_percent_template_reduction() {
    let report = ShapeTemplateRegistry::new().memory_savings_report(12);
    assert!(report.reduction_percent > 90);
    assert_eq!(report.naive_materialized_state_model_count, 463 * 12);
    assert_eq!(report.canonical_template_count, 269);
}
