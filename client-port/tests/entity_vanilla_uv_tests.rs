// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::entity_rig::{
    build_entity_rig_mesh, build_entity_rig_mesh_with_pose, classify_entity_rig, EntityPose,
    EntityRigPose,
};
use megu_client_mesh::vanilla_plus_3d::{vanilla_box_face_uvs, BoxUv, ExtrudedMesh};

fn mesh(id: u16) -> ExtrudedMesh {
    build_entity_rig_mesh(classify_entity_rig(id).unwrap(), EntityPose::default())
}

fn layered_mesh(id: u16) -> ExtrudedMesh {
    build_entity_rig_mesh(
        classify_entity_rig(id).unwrap(),
        EntityPose {
            enable_skin_3d_layers: true,
            ..EntityPose::default()
        },
    )
}

fn armored_mesh(id: u16) -> ExtrudedMesh {
    let pose = EntityRigPose {
        armor_visible: true,
        ..EntityRigPose::from(EntityPose::default())
    };
    build_entity_rig_mesh_with_pose(classify_entity_rig(id).unwrap(), pose)
}

fn matching_boxes(
    mesh: &ExtrudedMesh,
    model_size: [f32; 3],
    tex_off: [u16; 2],
    tex_size: [u16; 2],
    mirror: bool,
) -> usize {
    let uv = BoxUv::new(tex_off[0], tex_off[1], tex_size[0], tex_size[1]).with_mirror(mirror);
    let expected = vanilla_box_face_uvs(model_size, uv);
    mesh.quads
        .as_chunks::<6>()
        .0
        .iter()
        .filter(|chunk| chunk.iter().zip(expected).all(|(quad, uv)| quad.uv == uv))
        .count()
}

fn assert_box(
    mesh: &ExtrudedMesh,
    model_size: [f32; 3],
    tex_off: [u16; 2],
    tex_size: [u16; 2],
    mirror: bool,
    expected_count: usize,
    label: &str,
) {
    let count = matching_boxes(mesh, model_size, tex_off, tex_size, mirror);
    assert_eq!(
        count, expected_count,
        "{label}: model_size={model_size:?} tex_off={tex_off:?} tex_size={tex_size:?} mirror={mirror}"
    );
}

fn assert_humanoid_classic(
    mesh: &ExtrudedMesh,
    tex_height: u16,
    limb_size: [f32; 3],
    family: &str,
) {
    let sheet = [64, tex_height];
    assert_box(mesh, [8.0, 8.0, 8.0], [0, 0], sheet, false, 1, "head");
    assert_box(mesh, [8.0, 12.0, 4.0], [16, 16], sheet, false, 1, "body");
    assert_box(mesh, limb_size, [40, 16], sheet, false, 1, "right arm");
    assert_box(
        mesh,
        limb_size,
        [40, 16],
        sheet,
        true,
        1,
        "mirrored left arm",
    );
    assert_box(mesh, limb_size, [0, 16], sheet, false, 1, "right leg");
    assert_box(
        mesh,
        limb_size,
        [0, 16],
        sheet,
        true,
        1,
        "mirrored left leg",
    );
    assert_eq!(
        matching_boxes(mesh, [8.0, 8.0, 8.0], [0, 0], sheet, false),
        1,
        "{family} head count"
    );
}

#[test]
fn player_exact_skin_and_outer_layer_uvs() {
    let mesh = layered_mesh(128);
    let sheet = [64, 64];
    assert_box(
        &mesh,
        [8.0, 8.0, 8.0],
        [0, 0],
        sheet,
        false,
        1,
        "player head",
    );
    assert_box(
        &mesh,
        [8.0, 8.0, 8.0],
        [32, 0],
        sheet,
        false,
        1,
        "player hat layer",
    );
    assert_box(
        &mesh,
        [8.0, 12.0, 4.0],
        [16, 16],
        sheet,
        false,
        1,
        "player body",
    );
    assert_box(
        &mesh,
        [8.0, 12.0, 4.0],
        [16, 32],
        sheet,
        false,
        1,
        "player jacket layer",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [40, 16],
        sheet,
        false,
        1,
        "player right arm",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [40, 32],
        sheet,
        false,
        1,
        "player right sleeve",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [32, 48],
        sheet,
        false,
        1,
        "player left arm",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [48, 48],
        sheet,
        false,
        1,
        "player left sleeve",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 16],
        sheet,
        false,
        1,
        "player right leg",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 32],
        sheet,
        false,
        1,
        "player right pants",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [16, 48],
        sheet,
        false,
        1,
        "player left leg",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 48],
        sheet,
        false,
        1,
        "player left pants",
    );
}

#[test]
fn zombie_and_skeleton_exact_humanoid_uvs() {
    assert_humanoid_classic(&mesh(124), 64, [4.0, 12.0, 4.0], "zombie");
    assert_humanoid_classic(&mesh(91), 32, [2.0, 12.0, 2.0], "skeleton");
}

#[test]
fn creeper_exact_uvs() {
    let mesh = mesh(23);
    let sheet = [64, 32];
    assert_box(
        &mesh,
        [8.0, 8.0, 8.0],
        [0, 0],
        sheet,
        false,
        1,
        "creeper head",
    );
    assert_box(
        &mesh,
        [8.0, 12.0, 4.0],
        [16, 16],
        sheet,
        false,
        1,
        "creeper body",
    );
    assert_box(
        &mesh,
        [4.0, 6.0, 4.0],
        [0, 16],
        sheet,
        false,
        4,
        "creeper legs",
    );
}

#[test]
fn villager_exact_head_body_robe_and_leg_uvs() {
    let mesh = mesh(113);
    let sheet = [64, 64];
    assert_box(
        &mesh,
        [8.0, 10.0, 8.0],
        [0, 0],
        sheet,
        false,
        1,
        "villager head",
    );
    assert_box(
        &mesh,
        [2.0, 4.0, 2.0],
        [24, 0],
        sheet,
        false,
        1,
        "villager nose",
    );
    assert_box(
        &mesh,
        [8.0, 12.0, 6.0],
        [16, 20],
        sheet,
        false,
        1,
        "villager body",
    );
    assert_box(
        &mesh,
        [8.0, 20.0, 6.0],
        [0, 38],
        sheet,
        false,
        1,
        "villager robe overlay",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 22],
        sheet,
        false,
        1,
        "villager right leg",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 22],
        sheet,
        true,
        1,
        "villager mirrored left leg",
    );
}

#[test]
fn pig_exact_uvs() {
    let mesh = mesh(77);
    let sheet = [64, 32];
    assert_box(&mesh, [8.0, 8.0, 8.0], [0, 0], sheet, false, 1, "pig head");
    assert_box(
        &mesh,
        [4.0, 3.0, 1.0],
        [16, 16],
        sheet,
        false,
        1,
        "pig snout",
    );
    assert_box(
        &mesh,
        [10.0, 16.0, 8.0],
        [28, 8],
        sheet,
        false,
        1,
        "pig body",
    );
    assert_box(&mesh, [4.0, 6.0, 4.0], [0, 16], sheet, false, 4, "pig legs");
}

#[test]
fn cow_exact_uvs() {
    let mesh = mesh(22);
    let sheet = [64, 32];
    assert_box(&mesh, [8.0, 8.0, 6.0], [0, 0], sheet, false, 1, "cow head");
    assert_box(
        &mesh,
        [1.0, 3.0, 1.0],
        [22, 0],
        sheet,
        false,
        2,
        "cow horns",
    );
    assert_box(
        &mesh,
        [12.0, 18.0, 10.0],
        [18, 4],
        sheet,
        false,
        1,
        "cow body",
    );
    assert_box(
        &mesh,
        [4.0, 6.0, 1.0],
        [52, 0],
        sheet,
        false,
        1,
        "cow udder",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 16],
        sheet,
        false,
        4,
        "cow legs",
    );
}

#[test]
fn sheep_exact_body_wool_head_and_leg_uvs() {
    let mesh = mesh(87);
    let sheet = [64, 32];
    assert_box(
        &mesh,
        [6.0, 6.0, 8.0],
        [0, 0],
        sheet,
        false,
        1,
        "sheep head",
    );
    assert_box(
        &mesh,
        [8.0, 16.0, 6.0],
        [28, 8],
        sheet,
        false,
        2,
        "sheep body plus wool shell",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 4.0],
        [0, 16],
        sheet,
        false,
        4,
        "sheep legs",
    );
}

#[test]
fn spider_exact_uvs_and_mirrored_left_legs() {
    let mesh = mesh(100);
    let sheet = [64, 32];
    assert_box(
        &mesh,
        [8.0, 8.0, 8.0],
        [32, 4],
        sheet,
        false,
        1,
        "spider head",
    );
    assert_box(
        &mesh,
        [6.0, 6.0, 6.0],
        [0, 0],
        sheet,
        false,
        1,
        "spider front body",
    );
    assert_box(
        &mesh,
        [10.0, 8.0, 12.0],
        [0, 12],
        sheet,
        false,
        1,
        "spider abdomen",
    );
    assert_box(
        &mesh,
        [16.0, 2.0, 2.0],
        [18, 0],
        sheet,
        false,
        8,
        "spider right legs",
    );
    assert_box(
        &mesh,
        [16.0, 2.0, 2.0],
        [18, 0],
        sheet,
        true,
        8,
        "spider mirrored left legs",
    );
}

#[test]
fn enderman_exact_uvs() {
    let mesh = mesh(33);
    let sheet = [64, 32];
    assert_box(
        &mesh,
        [8.0, 8.0, 8.0],
        [0, 0],
        sheet,
        false,
        1,
        "enderman head",
    );
    assert_box(
        &mesh,
        [8.0, 12.0, 4.0],
        [32, 16],
        sheet,
        false,
        1,
        "enderman body",
    );
    assert_box(
        &mesh,
        [2.0, 30.0, 2.0],
        [56, 0],
        sheet,
        false,
        2,
        "enderman right limbs",
    );
    assert_box(
        &mesh,
        [2.0, 30.0, 2.0],
        [56, 0],
        sheet,
        true,
        2,
        "enderman mirrored left limbs",
    );
}

#[test]
fn wolf_exact_uvs() {
    let mesh = mesh(122);
    let sheet = [64, 32];
    assert_box(&mesh, [6.0, 6.0, 4.0], [0, 0], sheet, false, 1, "wolf head");
    assert_box(
        &mesh,
        [3.0, 3.0, 4.0],
        [0, 10],
        sheet,
        false,
        1,
        "wolf muzzle",
    );
    assert_box(
        &mesh,
        [2.0, 2.0, 1.0],
        [16, 14],
        sheet,
        false,
        2,
        "wolf ears",
    );
    assert_box(
        &mesh,
        [6.0, 9.0, 6.0],
        [18, 14],
        sheet,
        false,
        1,
        "wolf body",
    );
    assert_box(
        &mesh,
        [2.0, 8.0, 2.0],
        [0, 18],
        sheet,
        false,
        4,
        "wolf legs",
    );
    assert_box(
        &mesh,
        [2.0, 8.0, 2.0],
        [9, 18],
        sheet,
        false,
        2,
        "wolf bushy tail",
    );
}

#[test]
fn horse_exact_uvs_and_mirrored_left_legs() {
    let mesh = mesh(53);
    let sheet = [64, 64];
    assert_box(
        &mesh,
        [10.0, 10.0, 22.0],
        [0, 32],
        sheet,
        false,
        1,
        "horse body",
    );
    assert_box(
        &mesh,
        [4.0, 12.0, 7.0],
        [0, 35],
        sheet,
        false,
        1,
        "horse neck",
    );
    assert_box(
        &mesh,
        [6.0, 5.0, 7.0],
        [0, 13],
        sheet,
        false,
        1,
        "horse head",
    );
    assert_box(
        &mesh,
        [3.0, 14.0, 4.0],
        [42, 36],
        sheet,
        false,
        1,
        "horse tail",
    );
    assert_box(
        &mesh,
        [4.0, 11.0, 4.0],
        [48, 21],
        sheet,
        false,
        2,
        "horse right legs",
    );
    assert_box(
        &mesh,
        [4.0, 11.0, 4.0],
        [48, 21],
        sheet,
        true,
        2,
        "horse mirrored left legs",
    );

    let armored = armored_mesh(53);
    assert_box(
        &armored,
        [10.0, 9.0, 9.0],
        [26, 0],
        sheet,
        false,
        1,
        "horse armor shell",
    );
}
