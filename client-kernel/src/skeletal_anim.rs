// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Pure-std skeletal animation for numeric Bedrock-style geometry and animation data.
//!
//! Pivots in `.geo.json` are absolute model coordinates. A bone's world matrix
//! maps coordinates relative to that bone's pivot into model space. Euler angles
//! are applied in X, then Y, then Z order (column vectors). Rotation keyframes
//! preserve authored degree values, including rotations beyond a full turn.

use std::collections::{HashMap, HashSet};

pub const MAX_BONES: usize = 256;
pub const MAX_JSON_BYTES: usize = 1_048_576;
pub const MAX_JSON_DEPTH: usize = 48;
pub const MAX_CLIPS: usize = 128;
pub const MAX_KEYFRAMES: usize = 4096;
pub const MAX_LAYERS: usize = 16;
pub const ANIMATION_CHANNEL: &str = "mc-spine:anim_v1";
pub const ANM1_BYTES: usize = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationError {
    InvalidRig,
    InvalidClip,
    InvalidTime,
    InvalidJson,
    InvalidWire,
    LimitExceeded,
    UnknownBone,
    UnknownClip,
}

impl std::fmt::Display for AnimationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for AnimationError {}

fn finite3(v: [f32; 3]) -> bool {
    v.iter().all(|n| n.is_finite())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoneDef {
    pub name: String,
    pub parent: Option<usize>,
    pub pivot: [f32; 3],
    pub bind_rotation_deg: [f32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoneTransform {
    pub position: [f32; 3],
    pub rotation_deg: [f32; 3],
    pub scale: [f32; 3],
}

impl Default for BoneTransform {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            rotation_deg: [0.0; 3],
            scale: [1.0; 3],
        }
    }
}

impl BoneTransform {
    fn is_finite(self) -> bool {
        finite3(self.position) && finite3(self.rotation_deg) && finite3(self.scale)
    }
}

#[derive(Clone, Debug)]
pub struct SkeletonRig {
    bones: Vec<BoneDef>,
    order: Vec<usize>,
}

impl SkeletonRig {
    pub fn new(bones: Vec<BoneDef>) -> Result<Self, AnimationError> {
        if bones.len() > MAX_BONES {
            return Err(AnimationError::LimitExceeded);
        }
        let mut names = HashSet::with_capacity(bones.len());
        for bone in &bones {
            if !valid_name(&bone.name)
                || !names.insert(&bone.name)
                || !finite3(bone.pivot)
                || !finite3(bone.bind_rotation_deg)
            {
                return Err(AnimationError::InvalidRig);
            }
            if bone.parent.is_some_and(|parent| parent >= bones.len()) {
                return Err(AnimationError::InvalidRig);
            }
        }

        // Three-color DFS builds a deterministic parent-first order, including
        // when source geometry lists children before their parents.
        fn visit(
            index: usize,
            bones: &[BoneDef],
            state: &mut [u8],
            order: &mut Vec<usize>,
        ) -> Result<(), AnimationError> {
            match state[index] {
                1 => return Err(AnimationError::InvalidRig),
                2 => return Ok(()),
                _ => {}
            }
            state[index] = 1;
            if let Some(parent) = bones[index].parent {
                visit(parent, bones, state, order)?;
            }
            state[index] = 2;
            order.push(index);
            Ok(())
        }
        let mut state = vec![0; bones.len()];
        let mut order = Vec::with_capacity(bones.len());
        for index in 0..bones.len() {
            visit(index, &bones, &mut state, &mut order)?;
        }
        Ok(Self { bones, order })
    }

    pub fn bones(&self) -> &[BoneDef] {
        &self.bones
    }

    pub fn evaluation_order(&self) -> &[usize] {
        &self.order
    }

    pub fn bone_index(&self, name: &str) -> Option<usize> {
        self.bones.iter().position(|bone| bone.name == name)
    }

    /// One column-major model-space matrix per bone in the original bone order.
    /// `evaluation_order()` supplies the parent-first traversal used internally.
    pub fn evaluate_world_matrices(
        &self,
        pose: &[BoneTransform],
    ) -> Result<Vec<[f32; 16]>, AnimationError> {
        if pose.len() != self.bones.len() || pose.iter().any(|v| !v.is_finite()) {
            return Err(AnimationError::InvalidRig);
        }
        let mut result = vec![[0.0; 16]; self.bones.len()];
        for &index in &self.order {
            let bone = &self.bones[index];
            let parent_pivot = bone.parent.map_or([0.0; 3], |p| self.bones[p].pivot);
            let offset = std::array::from_fn(|axis| {
                bone.pivot[axis] - parent_pivot[axis] + pose[index].position[axis]
            });
            let angles = std::array::from_fn(|axis| {
                bone.bind_rotation_deg[axis] + pose[index].rotation_deg[axis]
            });
            let local = multiply(
                multiply(translation_matrix(offset), rotation_matrix(angles)),
                scale_matrix(pose[index].scale),
            );
            result[index] = if let Some(parent) = bone.parent {
                multiply(result[parent], local)
            } else {
                local
            };
            if result[index].iter().any(|n| !n.is_finite()) {
                return Err(AnimationError::InvalidRig);
            }
        }
        Ok(result)
    }
}

fn identity() -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ]
}

fn multiply(a: [f32; 16], b: [f32; 16]) -> [f32; 16] {
    let mut result = [0.0; 16];
    for column in 0..4 {
        for row in 0..4 {
            result[column * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[column * 4 + k]).sum();
        }
    }
    result
}

fn translation_matrix(position: [f32; 3]) -> [f32; 16] {
    let mut matrix = identity();
    matrix[12..15].copy_from_slice(&position);
    matrix
}

fn scale_matrix(scale: [f32; 3]) -> [f32; 16] {
    let mut matrix = identity();
    matrix[0] = scale[0];
    matrix[5] = scale[1];
    matrix[10] = scale[2];
    matrix
}

fn rotation_matrix(degrees: [f32; 3]) -> [f32; 16] {
    let [x, y, z] = degrees.map(f32::to_radians);
    let (sx, cx) = x.sin_cos();
    let (sy, cy) = y.sin_cos();
    let (sz, cz) = z.sin_cos();
    [
        cz * cy,
        sz * cy,
        -sy,
        0.0,
        cz * sy * sx - sz * cx,
        sz * sy * sx + cz * cx,
        cy * sx,
        0.0,
        cz * sy * cx + sz * sx,
        sz * sy * cx - cz * sx,
        cy * cx,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpolationMode {
    Step,
    Linear,
    CatmullRom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopMode {
    Once,
    Loop,
    HoldOnLastFrame,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    pub time_sec: f32,
    /// The outgoing (post) value at this timestamp.
    pub value: [f32; 3],
    /// Optional incoming (pre) value, for a discontinuous Bedrock keyframe.
    pub pre: Option<[f32; 3]>,
    /// Interpolation used from this keyframe toward the next keyframe.
    pub interpolation: InterpolationMode,
}

impl Keyframe {
    pub fn new(time_sec: f32, value: [f32; 3], interpolation: InterpolationMode) -> Self {
        Self {
            time_sec,
            value,
            pre: None,
            interpolation,
        }
    }
}

fn validate_frames(frames: &[Keyframe]) -> Result<(), AnimationError> {
    if frames.len() > MAX_KEYFRAMES {
        return Err(AnimationError::LimitExceeded);
    }
    let mut last = None;
    for frame in frames {
        if !frame.time_sec.is_finite()
            || frame.time_sec < 0.0
            || !finite3(frame.value)
            || frame.pre.is_some_and(|v| !finite3(v))
            || last.is_some_and(|time| frame.time_sec <= time)
        {
            return Err(AnimationError::InvalidClip);
        }
        last = Some(frame.time_sec);
    }
    Ok(())
}

fn sample_frames(frames: &[Keyframe], time: f32, default: [f32; 3]) -> [f32; 3] {
    if frames.is_empty() {
        return default;
    }
    if time < frames[0].time_sec {
        return frames[0].pre.unwrap_or(frames[0].value);
    }
    let upper = frames.partition_point(|frame| frame.time_sec <= time);
    if upper == frames.len() {
        return frames[frames.len() - 1].value;
    }
    let from = &frames[upper - 1];
    let to = &frames[upper];
    let dt = to.time_sec - from.time_sec;
    let u = (time - from.time_sec) / dt;
    let target = to.pre.unwrap_or(to.value);
    match from.interpolation {
        InterpolationMode::Step => from.value,
        InterpolationMode::Linear => {
            std::array::from_fn(|i| from.value[i] + (target[i] - from.value[i]) * u)
        }
        InterpolationMode::CatmullRom => {
            let previous = if upper >= 2 {
                frames[upper - 2].value
            } else {
                from.value
            };
            let next = frames
                .get(upper + 1)
                .map_or(target, |frame| frame.pre.unwrap_or(frame.value));
            std::array::from_fn(|i| {
                let p0 = previous[i];
                let p1 = from.value[i];
                let p2 = target[i];
                let p3 = next[i];
                0.5 * ((2.0 * p1)
                    + (-p0 + p2) * u
                    + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * u * u
                    + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * u * u * u)
            })
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoneTrack {
    pub position: Vec<Keyframe>,
    pub rotation: Vec<Keyframe>,
    pub scale: Vec<Keyframe>,
}

impl BoneTrack {
    pub fn validate(&self) -> Result<(), AnimationError> {
        validate_frames(&self.position)?;
        validate_frames(&self.rotation)?;
        validate_frames(&self.scale)
    }

    pub fn sample(&self, time_sec: f32) -> Result<BoneTransform, AnimationError> {
        if !time_sec.is_finite() || time_sec < 0.0 {
            return Err(AnimationError::InvalidTime);
        }
        self.validate()?;
        let transform = BoneTransform {
            position: sample_frames(&self.position, time_sec, [0.0; 3]),
            rotation_deg: sample_frames(&self.rotation, time_sec, [0.0; 3]),
            scale: sample_frames(&self.scale, time_sec, [1.0; 3]),
        };
        if !transform.is_finite() {
            return Err(AnimationError::InvalidClip);
        }
        Ok(transform)
    }
}

#[derive(Clone, Debug)]
pub struct AnimationClip {
    pub name: String,
    pub length_sec: f32,
    pub loop_mode: LoopMode,
    pub tracks: HashMap<String, BoneTrack>,
}

impl AnimationClip {
    pub fn validate(&self) -> Result<(), AnimationError> {
        if !valid_name(&self.name)
            || !self.length_sec.is_finite()
            || !(0.0..=86_400.0).contains(&self.length_sec)
            || self.tracks.len() > MAX_BONES
        {
            return Err(AnimationError::InvalidClip);
        }
        for (name, track) in &self.tracks {
            if !valid_name(name) {
                return Err(AnimationError::InvalidClip);
            }
            track.validate()?;
            for frame in track
                .position
                .iter()
                .chain(&track.rotation)
                .chain(&track.scale)
            {
                if frame.time_sec > self.length_sec {
                    return Err(AnimationError::InvalidClip);
                }
            }
        }
        Ok(())
    }

    pub fn sample_pose(
        &self,
        rig: &SkeletonRig,
        time_sec: f32,
    ) -> Result<Vec<BoneTransform>, AnimationError> {
        if !time_sec.is_finite() || time_sec < 0.0 {
            return Err(AnimationError::InvalidTime);
        }
        self.validate()?;
        if self.loop_mode == LoopMode::Once && time_sec > self.length_sec {
            return Ok(vec![BoneTransform::default(); rig.bones.len()]);
        }
        let time = if self.loop_mode == LoopMode::Loop && self.length_sec > 0.0 {
            time_sec.rem_euclid(self.length_sec)
        } else {
            time_sec.min(self.length_sec)
        };
        let mut pose = vec![BoneTransform::default(); rig.bones.len()];
        for (name, track) in &self.tracks {
            let index = rig.bone_index(name).ok_or(AnimationError::UnknownBone)?;
            pose[index] = track.sample(time)?;
        }
        Ok(pose)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoneMask {
    pub weights: Vec<f32>,
}

impl BoneMask {
    pub fn all(rig: &SkeletonRig) -> Self {
        Self {
            weights: vec![1.0; rig.bones.len()],
        }
    }

    pub fn only(rig: &SkeletonRig, names: &[&str]) -> Result<Self, AnimationError> {
        let mut weights = vec![0.0; rig.bones.len()];
        for name in names {
            let index = rig.bone_index(name).ok_or(AnimationError::UnknownBone)?;
            weights[index] = 1.0;
        }
        Ok(Self { weights })
    }

    fn validate(&self, count: usize) -> Result<(), AnimationError> {
        if self.weights.len() != count
            || self
                .weights
                .iter()
                .any(|w| !w.is_finite() || !(0.0..=1.0).contains(w))
        {
            return Err(AnimationError::InvalidRig);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum FadeSource {
    Playing { clip: String, time: f64, speed: f32 },
    Snapshot(Vec<BoneTransform>),
}

#[derive(Clone, Debug)]
pub struct AnimationLayer {
    pub mask: BoneMask,
    pub weight: f32,
    current: Option<String>,
    time: f64,
    speed: f32,
    fade_from: Option<FadeSource>,
    fade_elapsed: f32,
    fade_duration: f32,
}

impl AnimationLayer {
    pub fn current_clip(&self) -> Option<&str> {
        self.current.as_deref()
    }
}

#[derive(Clone, Debug)]
pub struct AnimationController {
    rig: SkeletonRig,
    clips: HashMap<String, AnimationClip>,
    layers: Vec<AnimationLayer>,
}

fn lerp_transform(a: BoneTransform, b: BoneTransform, factor: f32) -> BoneTransform {
    let lerp = |left: [f32; 3], right: [f32; 3]| {
        std::array::from_fn(|i| left[i] + (right[i] - left[i]) * factor)
    };
    BoneTransform {
        position: lerp(a.position, b.position),
        rotation_deg: lerp(a.rotation_deg, b.rotation_deg),
        scale: lerp(a.scale, b.scale),
    }
}

impl AnimationController {
    pub fn new(rig: SkeletonRig, clips: Vec<AnimationClip>) -> Result<Self, AnimationError> {
        if clips.len() > MAX_CLIPS {
            return Err(AnimationError::LimitExceeded);
        }
        let mut map = HashMap::with_capacity(clips.len());
        for clip in clips {
            clip.validate()?;
            if clip
                .tracks
                .keys()
                .any(|name| rig.bone_index(name).is_none())
            {
                return Err(AnimationError::UnknownBone);
            }
            if map.insert(clip.name.clone(), clip).is_some() {
                return Err(AnimationError::InvalidClip);
            }
        }
        Ok(Self {
            rig,
            clips: map,
            layers: Vec::new(),
        })
    }

    pub fn add_layer(&mut self, mask: BoneMask, weight: f32) -> Result<usize, AnimationError> {
        if self.layers.len() >= MAX_LAYERS {
            return Err(AnimationError::LimitExceeded);
        }
        mask.validate(self.rig.bones.len())?;
        if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
            return Err(AnimationError::InvalidClip);
        }
        let index = self.layers.len();
        self.layers.push(AnimationLayer {
            mask,
            weight,
            current: None,
            time: 0.0,
            speed: 1.0,
            fade_from: None,
            fade_elapsed: 0.0,
            fade_duration: 0.0,
        });
        Ok(index)
    }

    pub fn layers(&self) -> &[AnimationLayer] {
        &self.layers
    }

    fn sample_clip(&self, name: &str, elapsed: f64) -> Result<Vec<BoneTransform>, AnimationError> {
        let clip = self.clips.get(name).ok_or(AnimationError::UnknownClip)?;
        let duration = f64::from(clip.length_sec);
        let sample = if clip.loop_mode == LoopMode::Loop && duration > 0.0 {
            elapsed.rem_euclid(duration) as f32
        } else if clip.loop_mode == LoopMode::Once && elapsed > duration {
            // A one-shot finishes by releasing its affected bones.
            return Ok(vec![BoneTransform::default(); self.rig.bones.len()]);
        } else {
            elapsed.min(duration) as f32
        };
        clip.sample_pose(&self.rig, sample)
    }

    fn layer_pose(&self, layer: &AnimationLayer) -> Result<Vec<BoneTransform>, AnimationError> {
        let neutral = || vec![BoneTransform::default(); self.rig.bones.len()];
        let Some(name) = &layer.current else {
            return Ok(neutral());
        };
        let current = self.sample_clip(name, layer.time)?;
        let Some(source) = &layer.fade_from else {
            return Ok(current);
        };
        let old = match source {
            FadeSource::Playing { clip, time, .. } => self.sample_clip(clip, *time)?,
            FadeSource::Snapshot(pose) => pose.clone(),
        };
        let factor = (layer.fade_elapsed / layer.fade_duration).clamp(0.0, 1.0);
        Ok(old
            .into_iter()
            .zip(current)
            .map(|(a, b)| lerp_transform(a, b, factor))
            .collect())
    }

    /// Crossfade to a clip. Replaying the active clip with `restart=false`
    /// retains phase and simply updates speed.
    pub fn play(
        &mut self,
        layer_index: usize,
        clip_name: &str,
        blend_time_sec: f32,
        speed: f32,
        restart: bool,
    ) -> Result<(), AnimationError> {
        if !self.clips.contains_key(clip_name) {
            return Err(AnimationError::UnknownClip);
        }
        if !blend_time_sec.is_finite()
            || blend_time_sec < 0.0
            || !speed.is_finite()
            || !(0.0..=16.0).contains(&speed)
        {
            return Err(AnimationError::InvalidClip);
        }
        let layer = self
            .layers
            .get(layer_index)
            .ok_or(AnimationError::InvalidRig)?;
        if layer.current.as_deref() == Some(clip_name) && !restart {
            self.layers[layer_index].speed = speed;
            return Ok(());
        }
        let source = if blend_time_sec == 0.0 {
            None
        } else if layer.fade_from.is_some() {
            Some(FadeSource::Snapshot(self.layer_pose(layer)?))
        } else if let Some(previous) = &layer.current {
            Some(FadeSource::Playing {
                clip: previous.clone(),
                time: layer.time,
                speed: layer.speed,
            })
        } else {
            Some(FadeSource::Snapshot(vec![
                BoneTransform::default();
                self.rig.bones.len()
            ]))
        };
        let layer = &mut self.layers[layer_index];
        layer.current = Some(clip_name.to_owned());
        layer.time = 0.0;
        layer.speed = speed;
        layer.fade_from = source;
        layer.fade_duration = blend_time_sec;
        layer.fade_elapsed = 0.0;
        Ok(())
    }

    pub fn advance(&mut self, delta_sec: f32) -> Result<(), AnimationError> {
        if !delta_sec.is_finite() || delta_sec < 0.0 {
            return Err(AnimationError::InvalidTime);
        }
        for layer in &mut self.layers {
            layer.time += f64::from(delta_sec) * f64::from(layer.speed);
            if let Some(FadeSource::Playing { time, speed, .. }) = &mut layer.fade_from {
                *time += f64::from(delta_sec) * f64::from(*speed);
            }
            if layer.fade_from.is_some() {
                layer.fade_elapsed = (layer.fade_elapsed + delta_sec).min(layer.fade_duration);
                if layer.fade_elapsed >= layer.fade_duration {
                    layer.fade_from = None;
                }
            }
        }
        Ok(())
    }

    /// Layers override the bind pose in insertion order, weighted per bone.
    pub fn evaluate_pose(&self) -> Result<Vec<BoneTransform>, AnimationError> {
        let mut output = vec![BoneTransform::default(); self.rig.bones.len()];
        for layer in &self.layers {
            layer.mask.validate(self.rig.bones.len())?;
            if !layer.weight.is_finite() || !(0.0..=1.0).contains(&layer.weight) {
                return Err(AnimationError::InvalidClip);
            }
            let source = self.layer_pose(layer)?;
            for ((result, pose), mask) in output.iter_mut().zip(source).zip(&layer.mask.weights) {
                *result = lerp_transform(*result, pose, layer.weight * mask);
            }
        }
        if output.iter().any(|pose| !pose.is_finite()) {
            return Err(AnimationError::InvalidClip);
        }
        Ok(output)
    }

    pub fn evaluate_world_matrices(&self) -> Result<Vec<[f32; 16]>, AnimationError> {
        self.rig.evaluate_world_matrices(&self.evaluate_pose()?)
    }
}

/// Fixed-width, little-endian `ANM1` payload, exactly 19 bytes. Clip IDs are
/// resolved by the caller's per-entity animation registry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationTrigger {
    pub entity_id: i32,
    pub layer: u8,
    pub clip_id: u16,
    pub speed: f32,
    pub fade_ticks: u16,
    pub loop_mode: LoopMode,
    pub restart: bool,
}

fn validate_trigger(trigger: &AnimationTrigger) -> Result<(), AnimationError> {
    if trigger.entity_id < 0
        || trigger.layer as usize >= MAX_LAYERS
        || trigger.clip_id > 4095
        || !trigger.speed.is_finite()
        || !(0.0..=16.0).contains(&trigger.speed)
        || trigger.fade_ticks > 1200
    {
        return Err(AnimationError::InvalidWire);
    }
    Ok(())
}

pub fn encode_animation_trigger(
    trigger: AnimationTrigger,
) -> Result<[u8; ANM1_BYTES], AnimationError> {
    validate_trigger(&trigger)?;
    let mut data = [0u8; ANM1_BYTES];
    data[..4].copy_from_slice(b"ANM1");
    data[4..8].copy_from_slice(&trigger.entity_id.to_le_bytes());
    data[8] = trigger.layer;
    data[9..11].copy_from_slice(&trigger.clip_id.to_le_bytes());
    data[11..15].copy_from_slice(&trigger.speed.to_bits().to_le_bytes());
    data[15..17].copy_from_slice(&trigger.fade_ticks.to_le_bytes());
    data[17] = match trigger.loop_mode {
        LoopMode::Once => 0,
        LoopMode::Loop => 1,
        LoopMode::HoldOnLastFrame => 2,
    };
    data[18] = u8::from(trigger.restart);
    Ok(data)
}

pub fn decode_animation_trigger(data: &[u8]) -> Result<AnimationTrigger, AnimationError> {
    if data.len() != ANM1_BYTES || &data[..4] != b"ANM1" {
        return Err(AnimationError::InvalidWire);
    }
    let loop_mode = match data[17] {
        0 => LoopMode::Once,
        1 => LoopMode::Loop,
        2 => LoopMode::HoldOnLastFrame,
        _ => return Err(AnimationError::InvalidWire),
    };
    let restart = match data[18] {
        0 => false,
        1 => true,
        _ => return Err(AnimationError::InvalidWire),
    };
    let trigger = AnimationTrigger {
        entity_id: i32::from_le_bytes(
            data[4..8]
                .try_into()
                .map_err(|_| AnimationError::InvalidWire)?,
        ),
        layer: data[8],
        clip_id: u16::from_le_bytes(
            data[9..11]
                .try_into()
                .map_err(|_| AnimationError::InvalidWire)?,
        ),
        speed: f32::from_bits(u32::from_le_bytes(
            data[11..15]
                .try_into()
                .map_err(|_| AnimationError::InvalidWire)?,
        )),
        fade_ticks: u16::from_le_bytes(
            data[15..17]
                .try_into()
                .map_err(|_| AnimationError::InvalidWire)?,
        ),
        loop_mode,
        restart,
    };
    validate_trigger(&trigger)?;
    Ok(trigger)
}

// This deliberately small JSON decoder accepts JSON syntax only. Bedrock
// Molang expressions, string-valued vector entries and animation controllers
// must be evaluated by a separate, explicitly bounded interpreter.
#[derive(Debug)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn field(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn object(&self) -> Result<&[(String, Self)], AnimationError> {
        match self {
            Self::Object(fields) => Ok(fields),
            _ => Err(AnimationError::InvalidJson),
        }
    }

    fn array(&self) -> Result<&[Self], AnimationError> {
        match self {
            Self::Array(items) => Ok(items),
            _ => Err(AnimationError::InvalidJson),
        }
    }

    fn string(&self) -> Result<&str, AnimationError> {
        match self {
            Self::String(value) => Ok(value),
            _ => Err(AnimationError::InvalidJson),
        }
    }

    fn float(&self) -> Result<f32, AnimationError> {
        match self {
            Self::Number(value) if value.is_finite() && value.abs() <= f32::MAX as f64 => {
                Ok(*value as f32)
            }
            _ => Err(AnimationError::InvalidJson),
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
}

impl<'a> JsonParser<'a> {
    fn parse(input: &'a str) -> Result<JsonValue, AnimationError> {
        if input.len() > MAX_JSON_BYTES {
            return Err(AnimationError::LimitExceeded);
        }
        let mut parser = Self {
            bytes: input.as_bytes(),
            offset: 0,
            nodes: 0,
        };
        let result = parser.value(0)?;
        parser.whitespace();
        if parser.offset != parser.bytes.len() {
            return Err(AnimationError::InvalidJson);
        }
        Ok(result)
    }

    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.offset)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.offset += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.offset) == Some(&byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), AnimationError> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(AnimationError::InvalidJson)
        }
    }

    fn keyword(&mut self, word: &[u8]) -> Result<(), AnimationError> {
        if self.bytes.get(self.offset..self.offset + word.len()) == Some(word) {
            self.offset += word.len();
            Ok(())
        } else {
            Err(AnimationError::InvalidJson)
        }
    }

    fn value(&mut self, depth: usize) -> Result<JsonValue, AnimationError> {
        if depth > MAX_JSON_DEPTH || self.nodes >= 100_000 {
            return Err(AnimationError::LimitExceeded);
        }
        self.nodes += 1;
        self.whitespace();
        match self.bytes.get(self.offset) {
            Some(b'{') => self.object_value(depth + 1),
            Some(b'[') => self.array_value(depth + 1),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => {
                self.keyword(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.keyword(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'n') => {
                self.keyword(b"null")?;
                Ok(JsonValue::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number().map(JsonValue::Number),
            _ => Err(AnimationError::InvalidJson),
        }
    }

    fn object_value(&mut self, depth: usize) -> Result<JsonValue, AnimationError> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        self.whitespace();
        if self.consume(b'}') {
            return Ok(JsonValue::Object(fields));
        }
        loop {
            self.whitespace();
            let key = self.parse_string()?;
            if !seen.insert(key.clone()) {
                return Err(AnimationError::InvalidJson);
            }
            self.whitespace();
            self.expect(b':')?;
            let value = self.value(depth)?;
            fields.push((key, value));
            self.whitespace();
            if self.consume(b'}') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Object(fields))
    }

    fn array_value(&mut self, depth: usize) -> Result<JsonValue, AnimationError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.whitespace();
        if self.consume(b']') {
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.value(depth)?);
            self.whitespace();
            if self.consume(b']') {
                break;
            }
            self.expect(b',')?;
        }
        Ok(JsonValue::Array(items))
    }

    fn unicode_unit(&mut self) -> Result<u16, AnimationError> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = self
                .bytes
                .get(self.offset)
                .and_then(|b| (*b as char).to_digit(16))
                .ok_or(AnimationError::InvalidJson)?;
            self.offset += 1;
            value = value * 16 + digit as u16;
        }
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, AnimationError> {
        self.expect(b'"')?;
        let mut result = String::new();
        let mut start = self.offset;
        loop {
            let byte = *self
                .bytes
                .get(self.offset)
                .ok_or(AnimationError::InvalidJson)?;
            if byte == b'"' || byte == b'\\' {
                let chunk = std::str::from_utf8(&self.bytes[start..self.offset])
                    .map_err(|_| AnimationError::InvalidJson)?;
                result.push_str(chunk);
                if byte == b'"' {
                    self.offset += 1;
                    break;
                }
                self.offset += 1;
                let escape = *self
                    .bytes
                    .get(self.offset)
                    .ok_or(AnimationError::InvalidJson)?;
                self.offset += 1;
                match escape {
                    b'"' => result.push('"'),
                    b'\\' => result.push('\\'),
                    b'/' => result.push('/'),
                    b'b' => result.push('\u{0008}'),
                    b'f' => result.push('\u{000C}'),
                    b'n' => result.push('\n'),
                    b'r' => result.push('\r'),
                    b't' => result.push('\t'),
                    b'u' => {
                        let high = self.unicode_unit()?;
                        let codepoint = if (0xD800..=0xDBFF).contains(&high) {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            let low = self.unicode_unit()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(AnimationError::InvalidJson);
                            }
                            0x10000 + ((high as u32 - 0xD800) << 10) + (low as u32 - 0xDC00)
                        } else if (0xDC00..=0xDFFF).contains(&high) {
                            return Err(AnimationError::InvalidJson);
                        } else {
                            high as u32
                        };
                        result.push(char::from_u32(codepoint).ok_or(AnimationError::InvalidJson)?);
                    }
                    _ => return Err(AnimationError::InvalidJson),
                }
                start = self.offset;
            } else if byte < 0x20 {
                return Err(AnimationError::InvalidJson);
            } else {
                self.offset += 1;
            }
            if result.len() + self.offset.saturating_sub(start) > 65_536 {
                return Err(AnimationError::LimitExceeded);
            }
        }
        if result.len() > 65_536 {
            return Err(AnimationError::LimitExceeded);
        }
        Ok(result)
    }

    fn number(&mut self) -> Result<f64, AnimationError> {
        let start = self.offset;
        self.consume(b'-');
        if !self.consume(b'0') {
            let first = *self
                .bytes
                .get(self.offset)
                .ok_or(AnimationError::InvalidJson)?;
            if !(b'1'..=b'9').contains(&first) {
                return Err(AnimationError::InvalidJson);
            }
            self.offset += 1;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
        }
        if self.consume(b'.') {
            let digits = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if self.offset == digits {
                return Err(AnimationError::InvalidJson);
            }
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            let digits = self.offset;
            while self.bytes.get(self.offset).is_some_and(u8::is_ascii_digit) {
                self.offset += 1;
            }
            if self.offset == digits {
                return Err(AnimationError::InvalidJson);
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.offset])
            .map_err(|_| AnimationError::InvalidJson)?;
        let number = text
            .parse::<f64>()
            .map_err(|_| AnimationError::InvalidJson)?;
        if !number.is_finite() {
            return Err(AnimationError::InvalidJson);
        }
        Ok(number)
    }
}

fn vector3(value: &JsonValue, uniform_scalar: bool) -> Result<[f32; 3], AnimationError> {
    if uniform_scalar {
        if let JsonValue::Number(_) = value {
            return Ok([value.float()?; 3]);
        }
    }
    let entries = value.array()?;
    if entries.len() != 3 {
        return Err(AnimationError::InvalidJson);
    }
    Ok([
        entries[0].float()?,
        entries[1].float()?,
        entries[2].float()?,
    ])
}

fn optional_vector3(
    value: Option<&JsonValue>,
    default: [f32; 3],
) -> Result<[f32; 3], AnimationError> {
    value.map_or(Ok(default), |v| vector3(v, false))
}

/// Returns all geometry definitions, preserving file order. Each entry's
/// string identifier comes from `description.identifier` when present.
pub fn parse_geometry_rigs_json(input: &str) -> Result<Vec<(String, SkeletonRig)>, AnimationError> {
    let root = JsonParser::parse(input)?;
    let geometries = root
        .field("minecraft:geometry")
        .ok_or(AnimationError::InvalidJson)?
        .array()?;
    if geometries.is_empty() || geometries.len() > 32 {
        return Err(AnimationError::LimitExceeded);
    }
    let mut output = Vec::with_capacity(geometries.len());
    let mut geometry_names = HashSet::new();
    for (geo_index, geometry) in geometries.iter().enumerate() {
        geometry.object()?;
        let identifier = match geometry.field("description") {
            Some(description) => description
                .field("identifier")
                .ok_or(AnimationError::InvalidJson)?
                .string()?
                .to_owned(),
            None => format!("geometry.{geo_index}"),
        };
        if !valid_name(&identifier) || !geometry_names.insert(identifier.clone()) {
            return Err(AnimationError::InvalidJson);
        }
        let list = geometry
            .field("bones")
            .ok_or(AnimationError::InvalidJson)?
            .array()?;
        if list.len() > MAX_BONES {
            return Err(AnimationError::LimitExceeded);
        }
        let mut pending = Vec::with_capacity(list.len());
        let mut index = HashMap::new();
        for (i, item) in list.iter().enumerate() {
            item.object()?;
            let name = item
                .field("name")
                .ok_or(AnimationError::InvalidJson)?
                .string()?
                .to_owned();
            if !valid_name(&name) || index.insert(name.clone(), i).is_some() {
                return Err(AnimationError::InvalidRig);
            }
            let parent = match item.field("parent") {
                Some(JsonValue::Null) | None => None,
                Some(value) => Some(value.string()?.to_owned()),
            };
            let pivot = optional_vector3(item.field("pivot"), [0.0; 3])?;
            let bind_rotation_deg = optional_vector3(item.field("rotation"), [0.0; 3])?;
            pending.push((name, parent, pivot, bind_rotation_deg));
        }
        let mut bones = Vec::with_capacity(pending.len());
        for (name, parent_name, pivot, bind_rotation_deg) in pending {
            let parent = match parent_name {
                Some(parent_name) => {
                    Some(*index.get(&parent_name).ok_or(AnimationError::InvalidRig)?)
                }
                None => None,
            };
            bones.push(BoneDef {
                name,
                parent,
                pivot,
                bind_rotation_deg,
            });
        }
        output.push((identifier, SkeletonRig::new(bones)?));
    }
    Ok(output)
}

/// Loads a geometry JSON document with exactly one geometry definition.
pub fn parse_geometry_json(input: &str) -> Result<SkeletonRig, AnimationError> {
    let mut geometries = parse_geometry_rigs_json(input)?;
    if geometries.len() != 1 {
        return Err(AnimationError::InvalidJson);
    }
    Ok(geometries.remove(0).1)
}

fn parse_interpolation(value: Option<&JsonValue>) -> Result<InterpolationMode, AnimationError> {
    let Some(value) = value else {
        return Ok(InterpolationMode::Linear);
    };
    match value.string()? {
        "step" => Ok(InterpolationMode::Step),
        "linear" => Ok(InterpolationMode::Linear),
        "catmullrom" => Ok(InterpolationMode::CatmullRom),
        _ => Err(AnimationError::InvalidJson),
    }
}

fn frame_value(
    time: f32,
    value: &JsonValue,
    uniform_scalar: bool,
) -> Result<Keyframe, AnimationError> {
    match value {
        JsonValue::Array(_) | JsonValue::Number(_) => Ok(Keyframe::new(
            time,
            vector3(value, uniform_scalar)?,
            InterpolationMode::Linear,
        )),
        JsonValue::Object(_) => {
            let interpolation = parse_interpolation(value.field("lerp_mode"))?;
            let pre = value
                .field("pre")
                .map(|v| vector3(v, uniform_scalar))
                .transpose()?;
            let post = value.field("post").or_else(|| value.field("vector"));
            let outgoing = match post {
                Some(value) => vector3(value, uniform_scalar)?,
                None => pre.ok_or(AnimationError::InvalidJson)?,
            };
            Ok(Keyframe {
                time_sec: time,
                value: outgoing,
                pre,
                interpolation,
            })
        }
        _ => Err(AnimationError::InvalidJson),
    }
}

fn parse_channel(value: &JsonValue, uniform_scalar: bool) -> Result<Vec<Keyframe>, AnimationError> {
    match value {
        JsonValue::Array(_) | JsonValue::Number(_) => {
            Ok(vec![frame_value(0.0, value, uniform_scalar)?])
        }
        JsonValue::Object(entries) => {
            if value.field("vector").is_some()
                || value.field("post").is_some()
                || value.field("pre").is_some()
            {
                return Ok(vec![frame_value(0.0, value, uniform_scalar)?]);
            }
            if entries.len() > MAX_KEYFRAMES {
                return Err(AnimationError::LimitExceeded);
            }
            let mut frames = Vec::with_capacity(entries.len());
            for (timestamp, value) in entries {
                let time = timestamp
                    .parse::<f32>()
                    .map_err(|_| AnimationError::InvalidJson)?;
                if !time.is_finite() || time < 0.0 {
                    return Err(AnimationError::InvalidJson);
                }
                frames.push(frame_value(time, value, uniform_scalar)?);
            }
            frames.sort_by(|a, b| a.time_sec.total_cmp(&b.time_sec));
            validate_frames(&frames)?;
            Ok(frames)
        }
        _ => Err(AnimationError::InvalidJson),
    }
}

fn parse_loop_mode(value: Option<&JsonValue>) -> Result<LoopMode, AnimationError> {
    match value {
        None | Some(JsonValue::Bool(false)) => Ok(LoopMode::Once),
        Some(JsonValue::Bool(true)) => Ok(LoopMode::Loop),
        Some(JsonValue::String(mode)) if mode == "hold_on_last_frame" => {
            Ok(LoopMode::HoldOnLastFrame)
        }
        Some(JsonValue::String(mode)) if mode == "loop" => Ok(LoopMode::Loop),
        Some(JsonValue::String(mode)) if mode == "once" => Ok(LoopMode::Once),
        _ => Err(AnimationError::InvalidJson),
    }
}

/// Reads numeric `animations` from Bedrock/GeckoLib `.animation.json`.
/// `pre` and `post` keyframe values are preserved. Unsupported Molang/string
/// expressions fail closed instead of silently generating an incorrect pose.
pub fn parse_animation_json(input: &str) -> Result<Vec<AnimationClip>, AnimationError> {
    let root = JsonParser::parse(input)?;
    let animations = root
        .field("animations")
        .ok_or(AnimationError::InvalidJson)?
        .object()?;
    if animations.len() > MAX_CLIPS {
        return Err(AnimationError::LimitExceeded);
    }
    let mut result = Vec::with_capacity(animations.len());
    for (name, value) in animations {
        value.object()?;
        let loop_mode = parse_loop_mode(value.field("loop"))?;
        let bones = match value.field("bones") {
            Some(bones) => bones.object()?,
            None => &[],
        };
        if bones.len() > MAX_BONES {
            return Err(AnimationError::LimitExceeded);
        }
        let mut tracks = HashMap::with_capacity(bones.len());
        let mut max_time = 0.0f32;
        for (bone_name, bone) in bones {
            bone.object()?;
            let mut track = BoneTrack::default();
            if let Some(value) = bone.field("position") {
                track.position = parse_channel(value, false)?;
            }
            if let Some(value) = bone.field("rotation") {
                track.rotation = parse_channel(value, false)?;
            }
            if let Some(value) = bone.field("scale") {
                track.scale = parse_channel(value, true)?;
            }
            for frame in track
                .position
                .iter()
                .chain(&track.rotation)
                .chain(&track.scale)
            {
                max_time = max_time.max(frame.time_sec);
            }
            if tracks.insert(bone_name.clone(), track).is_some() {
                return Err(AnimationError::InvalidClip);
            }
        }
        let length_sec = value
            .field("animation_length")
            .map_or(Ok(max_time), JsonValue::float)?;
        let clip = AnimationClip {
            name: name.clone(),
            length_sec,
            loop_mode,
            tracks,
        };
        clip.validate()?;
        result.push(clip);
    }
    Ok(result)
}
