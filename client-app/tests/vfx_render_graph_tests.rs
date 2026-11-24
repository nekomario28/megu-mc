// SPDX-License-Identifier: LGPL-3.0-or-later
//! VFX1 wire-to-simulation-to-frame-plan integration and resource ceilings.

use std::time::Duration;

use megu_client::render::{ClientVfxHost, FrameBudgetGovernor, ThrottleStage};
use megu_client::render_graph::{
    plan_frame, plan_frame_with_vfx, vfx_budget_for_pipeline, FramePlanInput, GpuPipelineKind,
    PassSubmission, RenderPassId,
};
use megu_client_mesh::vfx_engine::{
    BeamEffect, ColorRamp, DynamicColoredLight, EntityAura, ParticleDraw, ParticleEmitterSpec,
    ParticleOrientation, RibbonDraw, RibbonVertex, ScalarCurve, Vfx1Packet, VfxBudget,
    VfxCodecError, VfxCommand, VfxFrameOutput, VfxHardwareTier, VFX1_CHANNEL,
};

const CAMERA_FORWARD: [f32; 3] = [0.0, 0.0, 1.0];

fn frame_input() -> FramePlanInput {
    FramePlanInput {
        width: 1280,
        height: 720,
        visible_sections: 12,
        meshlet_count: 192,
        block_entity_instances: 8,
        block_entity_mesh_kinds: 2,
        translucent_sections: 3,
    }
}

fn emitter(rate: f32) -> ParticleEmitterSpec {
    ParticleEmitterSpec {
        pos: [1.0, 2.0, 3.0],
        initial_velocity: [0.0, 0.1, 0.0],
        position_spread: [0.0; 3],
        velocity_spread: [0.0; 3],
        spawn_rate_per_tick: rate,
        particle_lifetime_ticks: 300,
        emitter_ttl_ticks: 120,
        gravity_per_tick: 0.0,
        drag: 1.0,
        spin_per_tick: 0.0,
        orientation: ParticleOrientation::CameraBillboard,
        color: ColorRamp::default(),
        size: ScalarCurve::default(),
        priority: 10,
    }
}

fn light() -> DynamicColoredLight {
    DynamicColoredLight {
        pos: [4.0, 5.0, 6.0],
        rgb: [1.0, 0.25, 0.5],
        radius: 12.0,
        intensity: 2.0,
        ttl_ticks: 100,
        priority: 10,
    }
}

fn packet() -> Vfx1Packet {
    Vfx1Packet {
        commands: vec![
            VfxCommand::SpawnEmitter {
                id: 1,
                spec: emitter(12.0),
            },
            VfxCommand::SpawnBeam {
                id: 2,
                beam: BeamEffect {
                    start: [0.0; 3],
                    end: [0.0, 2.0, 0.0],
                    width: 0.5,
                    pulse_frequency: 0.0,
                    core_rgba: [1.0; 4],
                    glow_rgba: [0.5; 4],
                    ttl_ticks: 100,
                    priority: 10,
                },
            },
            VfxCommand::AttachAura(EntityAura {
                entity_id: 33,
                pos: [1.0; 3],
                inner_radius: 1.0,
                outer_radius: 2.0,
                rgba: [0.5; 4],
                ttl_ticks: 100,
                priority: 10,
            }),
            VfxCommand::SpawnLight {
                id: 4,
                light: light(),
            },
        ],
    }
}

#[test]
fn all_gpu_pipelines_receive_the_expected_vfx_budget() {
    let cases = [
        (GpuPipelineKind::ClassicLowSpec, VfxHardwareTier::LowSpec),
        (GpuPipelineKind::Classic, VfxHardwareTier::Balanced),
        (GpuPipelineKind::MeshShader, VfxHardwareTier::Balanced),
        (GpuPipelineKind::ComputeIndirect, VfxHardwareTier::High),
    ];
    assert_eq!(cases.len(), GpuPipelineKind::ALL.len());
    for (pipeline, tier) in cases {
        let expected = VfxBudget::for_tier(tier);
        assert_eq!(vfx_budget_for_pipeline(pipeline), expected);
        let host = ClientVfxHost::new(pipeline);
        assert_eq!(host.budget(), expected);
        assert_eq!(host.budget_tier(), tier);
    }
}

#[test]
fn vfx1_wire_commands_step_into_translucent_pass_and_colored_lights() {
    assert_eq!(VFX1_CHANNEL, "megu:vfx_v1");
    let pipeline = GpuPipelineKind::Classic;
    let mut host = ClientVfxHost::new(pipeline);
    let encoded = packet().encode().unwrap();
    assert_eq!(host.ingest_vfx1_payload(&encoded), Ok(4));

    let frame = host.step(CAMERA_FORWARD);
    assert_eq!(frame.particles.len(), 12);
    assert_eq!(frame.beams.len(), 1);
    assert_eq!(frame.beam_quads.len(), 1);
    assert_eq!(frame.auras.len(), 1);
    let mut advanced_light = light();
    advanced_light.ttl_ticks -= 1;
    assert_eq!(frame.lights, vec![advanced_light]);

    let input = frame_input();
    let baseline = plan_frame(pipeline, &input);
    let plan = plan_frame_with_vfx(pipeline, &input, &frame);
    let pass = plan.vfx_pass.as_ref().expect("translucent VFX pass");
    assert_eq!(pass.label, "vfx_particles_beams_ribbons");
    assert_eq!(pass.after_pass, RenderPassId::TranslucentTerrain);
    assert_eq!(pass.submission, PassSubmission::DirectDraws);
    assert_eq!(pass.summary.active_particles, 12);
    assert_eq!(pass.summary.beam_count, 1);
    assert_eq!(pass.summary.aura_count, 1);
    assert_eq!(pass.summary.dynamic_light_count, 1);
    assert_eq!(pass.summary.budget_preset, VfxHardwareTier::Balanced);
    assert_eq!(plan.passes.len(), baseline.passes.len());
    assert_eq!(
        plan.total_estimated_draw_calls(),
        baseline.total_estimated_draw_calls() + 15
    );
    assert!(plan.vram_estimate_bytes() > baseline.vram_estimate_bytes());
    plan.validate_binding_layouts().unwrap();
    assert!(baseline.vfx_pass.is_none());

    // Malformed payloads are rejected before modifying the live world.
    let before = host.frame_output(CAMERA_FORWARD);
    assert_eq!(
        host.ingest_vfx1_payload(b"BAD1\0"),
        Err(VfxCodecError::InvalidMagic)
    );
    assert_eq!(host.frame_output(CAMERA_FORWARD), before);
}

#[test]
fn externally_supplied_vfx_frames_are_clamped_before_pass_planning() {
    let mut frame = VfxFrameOutput {
        particles: vec![
            ParticleDraw {
                pos: [0.0; 3],
                velocity: [0.0; 3],
                rotation: 0.0,
                orientation: ParticleOrientation::CameraBillboard,
                size: 1.0,
                rgba: [1.0; 4],
            };
            5_000
        ],
        beams: vec![],
        beam_quads: vec![],
        ribbons: vec![],
        auras: vec![],
        lights: vec![light(); 500],
    };
    let vertex = RibbonVertex {
        pos: [0.0; 3],
        rgba: [1.0; 4],
        uv: [0.0; 2],
    };
    frame.ribbons = (0..20)
        .map(|id| RibbonDraw {
            id,
            vertices: vec![vertex; 100],
        })
        .collect();

    let plan = plan_frame_with_vfx(GpuPipelineKind::ClassicLowSpec, &frame_input(), &frame);
    let pass = plan.vfx_pass.unwrap();
    assert_eq!(pass.summary.active_particles, 256);
    assert_eq!(pass.summary.ribbon_quad_count, 96);
    assert_eq!(pass.summary.dynamic_light_count, 8);
    assert_eq!(pass.summary.budget_preset, VfxHardwareTier::LowSpec);
    assert_eq!(pass.estimated_draw_calls, 256 + 96);
}

#[test]
fn low_spec_repeated_spell_emission_evicts_without_unbounded_growth() {
    let mut host = ClientVfxHost::new(GpuPipelineKind::ClassicLowSpec);
    let packet = Vfx1Packet {
        commands: vec![VfxCommand::SpawnEmitter {
            id: 1,
            spec: emitter(4096.0),
        }],
    }
    .encode()
    .unwrap();
    host.ingest_vfx1_payload(&packet).unwrap();
    let mut maximum = 0;
    // 80 ticks request >5,000 spawned particles even after the per-tick
    // allowance. The retained storage must remain within LowSpec bounds.
    for _ in 0..80 {
        let output = host.step(CAMERA_FORWARD);
        maximum = maximum.max(output.particles.len());
        assert!(output.particles.len() <= host.budget().max_active_particles);
    }
    assert_eq!(
        maximum,
        VfxBudget::for_tier(VfxHardwareTier::LowSpec).max_active_particles
    );
    let output = host.frame_output(CAMERA_FORWARD);
    let plan = host.plan_frame_with_vfx(&frame_input(), &output);
    assert_eq!(plan.vfx_pass.unwrap().summary.active_particles, 256);
}

#[test]
fn governor_quality_degradation_rebudgets_host_and_graph() {
    let mut host = ClientVfxHost::new(GpuPipelineKind::ComputeIndirect);
    let mut governor = FrameBudgetGovernor::new(Some(60));
    assert_eq!(host.budget_tier(), VfxHardwareTier::High);
    host.ingest_vfx1_payload(&packet().encode().unwrap())
        .unwrap();

    for _ in 0..3 {
        host.observe_frame(&mut governor, Duration::from_millis(80));
    }
    assert_eq!(governor.stage(), ThrottleStage::TrimRebuilds);
    assert_eq!(host.budget_tier(), VfxHardwareTier::High);
    for _ in 0..3 {
        host.observe_frame(&mut governor, Duration::from_millis(80));
    }
    assert_eq!(governor.stage(), ThrottleStage::TrimLod);
    assert_eq!(host.budget_tier(), VfxHardwareTier::Balanced);
    // VfxWorld does not expose budget mutation; switching its tier clears
    // transient effects. Newly ingested effects use the downgraded budget.
    assert!(host.frame_output(CAMERA_FORWARD).lights.is_empty());
    host.ingest_vfx1_payload(&packet().encode().unwrap())
        .unwrap();
    let frame = host.step(CAMERA_FORWARD);
    let plan = host.plan_frame_with_vfx(&frame_input(), &frame);
    assert_eq!(
        plan.vfx_pass.unwrap().summary.budget_preset,
        VfxHardwareTier::Balanced
    );

    for _ in 0..3 {
        host.observe_frame(&mut governor, Duration::from_millis(80));
    }
    assert_eq!(governor.stage(), ThrottleStage::MinBudget);
    assert_eq!(host.budget(), VfxBudget::for_tier(VfxHardwareTier::LowSpec));
    let plan = host.plan_frame_with_vfx(&frame_input(), &host.frame_output(CAMERA_FORWARD));
    assert_eq!(
        plan.vfx_pass.unwrap().summary.budget_preset,
        VfxHardwareTier::LowSpec
    );
}
