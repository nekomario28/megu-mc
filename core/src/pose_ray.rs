// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Canonical player pose dimensions, eye positions, view rays, and interaction
//! reach ownership for Minecraft Java 1.21.1.
//!
//! Server validation, client block/entity selection, and camera code must all
//! start from the same pose-aware eye position. Camera interpolation remains a
//! presentation concern; this module deliberately contains only the resolved
//! pose at one instant.

/// Player pose kinds relevant to collision shape and eye-height selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlayerPoseKind {
    Standing,
    Crouching,
    Swimming,
    FallFlying,
    Sleeping,
    SpinAttack,
    Dying,
}

impl PlayerPoseKind {
    pub const ALL: [Self; 7] = [
        Self::Standing,
        Self::Crouching,
        Self::Swimming,
        Self::FallFlying,
        Self::Sleeping,
        Self::SpinAttack,
        Self::Dying,
    ];

    /// Vanilla 1.21.1 player dimensions for this pose.
    pub const fn dimensions(self) -> PlayerPoseDimensions {
        match self {
            Self::Standing => PlayerPoseDimensions::new(0.6, 1.8, 1.62),
            Self::Crouching => PlayerPoseDimensions::new(0.6, 1.5, 1.27),
            Self::Swimming | Self::FallFlying | Self::SpinAttack => {
                PlayerPoseDimensions::new(0.6, 0.6, 0.4)
            }
            Self::Sleeping => PlayerPoseDimensions::new(0.2, 0.2, 0.2),
            // Vanilla deliberately keeps the default standing eye height for
            // the dying pose even though the collision box collapses.
            Self::Dying => PlayerPoseDimensions::new(0.2, 0.2, 1.62),
        }
    }
}

/// Axis-aligned player dimensions and eye offset for one pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerPoseDimensions {
    pub width: f32,
    pub height: f32,
    pub eye_height: f32,
}

impl PlayerPoseDimensions {
    pub const fn new(width: f32, height: f32, eye_height: f32) -> Self {
        Self {
            width,
            height,
            eye_height,
        }
    }
}

/// A pose-aware interaction/selection ray in world coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewRay {
    pub origin: [f64; 3],
    pub dir: [f64; 3],
}

impl ViewRay {
    /// Evaluate a point along the ray at distance `t` from the eye.
    pub fn at(self, t: f64) -> [f64; 3] {
        [
            self.origin[0] + self.dir[0] * t,
            self.origin[1] + self.dir[1] * t,
            self.origin[2] + self.dir[2] * t,
        ]
    }
}

/// Construct the canonical eye ray for a player whose feet are at `feet`.
///
/// Yaw and pitch are Minecraft Java degrees. The direction follows the Java
/// convention exactly: `x = -sin(yaw) * cos(pitch)`,
/// `y = -sin(pitch)`, and `z = cos(yaw) * cos(pitch)`. Thus yaw `0` looks
/// south (`+Z`), yaw `90` looks west (`-X`), pitch `-90` looks up, and pitch
/// `90` looks down.
pub fn view_ray(feet: [f64; 3], pose: PlayerPoseKind, yaw_deg: f32, pitch_deg: f32) -> ViewRay {
    let yaw = f64::from(yaw_deg).to_radians();
    let pitch = f64::from(pitch_deg).to_radians();
    let cos_pitch = pitch.cos();
    ViewRay {
        origin: [
            feet[0],
            feet[1] + f64::from(pose.dimensions().eye_height),
            feet[2],
        ],
        dir: [-yaw.sin() * cos_pitch, -pitch.sin(), yaw.cos() * cos_pitch],
    }
}

/// Interaction distances for one authority boundary.
///
/// These are center-line selection distances, not AABB acceptance distances:
/// vanilla block interactions additionally test the block shape/AABB and the
/// server may deliberately use a separate tolerance at its own boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InteractionReach {
    pub block_survival: f64,
    pub block_creative: f64,
    pub entity_survival: f64,
    pub entity_creative: f64,
}

impl InteractionReach {
    /// Exact vanilla client selection reach for 1.21.1.
    pub const VANILLA_CLIENT_SELECTION: Self = Self {
        block_survival: 4.5,
        block_creative: 5.0,
        entity_survival: 3.0,
        entity_creative: 5.0,
    };

    /// Block reach for a game mode.
    pub const fn block(self, creative: bool) -> f64 {
        if creative {
            self.block_creative
        } else {
            self.block_survival
        }
    }

    /// Entity reach for a game mode.
    pub const fn entity(self, creative: bool) -> f64 {
        if creative {
            self.entity_creative
        } else {
            self.entity_survival
        }
    }
}

/// Explicit ownership boundary between what a client may select and what a
/// server accepts.
///
/// The default server acceptance bound is the same vanilla attribute value as
/// client selection. Servers that need network or interpolation tolerance can
/// widen only `server_acceptance` with [`Self::with_server_slack`] without
/// changing the client-facing selection distances.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReachPolicy {
    pub client_selection: InteractionReach,
    pub server_acceptance: InteractionReach,
}

impl ReachPolicy {
    pub const VANILLA: Self = Self {
        client_selection: InteractionReach::VANILLA_CLIENT_SELECTION,
        server_acceptance: InteractionReach::VANILLA_CLIENT_SELECTION,
    };

    /// Construct a policy with explicit server acceptance bounds.
    pub const fn with_server_acceptance(server_acceptance: InteractionReach) -> Self {
        Self {
            client_selection: InteractionReach::VANILLA_CLIENT_SELECTION,
            server_acceptance,
        }
    }

    /// Construct a policy whose server acceptance bounds are the vanilla
    /// client selection bounds plus independent block/entity slack.
    pub const fn with_server_slack(block_slack: f64, entity_slack: f64) -> Self {
        Self {
            client_selection: InteractionReach::VANILLA_CLIENT_SELECTION,
            server_acceptance: InteractionReach {
                block_survival: 4.5 + block_slack,
                block_creative: 5.0 + block_slack,
                entity_survival: 3.0 + entity_slack,
                entity_creative: 5.0 + entity_slack,
            },
        }
    }

    /// Client block-selection reach for a game mode.
    pub const fn client_block_reach(self, creative: bool) -> f64 {
        self.client_selection.block(creative)
    }

    /// Client entity-selection reach for a game mode.
    pub const fn client_entity_reach(self, creative: bool) -> f64 {
        self.client_selection.entity(creative)
    }

    /// Server block-acceptance bound for a game mode.
    pub const fn server_block_acceptance(self, creative: bool) -> f64 {
        self.server_acceptance.block(creative)
    }

    /// Server entity-acceptance bound for a game mode.
    pub const fn server_entity_acceptance(self, creative: bool) -> f64 {
        self.server_acceptance.entity(creative)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_vec_close(actual: [f64; 3], expected: [f64; 3]) {
        for axis in 0..3 {
            assert!(
                (actual[axis] - expected[axis]).abs() < 1e-12,
                "axis {axis}: actual {actual:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn vanilla_pose_dimensions_are_exact() {
        assert_eq!(
            PlayerPoseKind::Standing.dimensions(),
            PlayerPoseDimensions::new(0.6, 1.8, 1.62)
        );
        assert_eq!(
            PlayerPoseKind::Crouching.dimensions(),
            PlayerPoseDimensions::new(0.6, 1.5, 1.27)
        );
        for pose in [
            PlayerPoseKind::Swimming,
            PlayerPoseKind::FallFlying,
            PlayerPoseKind::SpinAttack,
        ] {
            assert_eq!(pose.dimensions(), PlayerPoseDimensions::new(0.6, 0.6, 0.4));
        }
        assert_eq!(
            PlayerPoseKind::Sleeping.dimensions(),
            PlayerPoseDimensions::new(0.2, 0.2, 0.2)
        );
        assert_eq!(
            PlayerPoseKind::Dying.dimensions(),
            PlayerPoseDimensions::new(0.2, 0.2, 1.62)
        );
    }

    #[test]
    fn view_ray_origin_uses_the_pose_eye_height() {
        let feet = [10.5, 64.0, -3.5];
        for (pose, eye) in [
            (PlayerPoseKind::Standing, 1.62f32),
            (PlayerPoseKind::Crouching, 1.27f32),
            (PlayerPoseKind::Swimming, 0.4f32),
        ] {
            let ray = view_ray(feet, pose, 0.0, 0.0);
            assert_eq!(ray.origin, [10.5, 64.0 + f64::from(eye), -3.5]);
        }
    }

    #[test]
    fn view_ray_cardinal_and_pitch_directions_are_unit_vectors() {
        let feet = [0.0, 64.0, 0.0];
        for (yaw, pitch, expected) in [
            (0.0, 0.0, [0.0, 0.0, 1.0]),
            (90.0, 0.0, [-1.0, 0.0, 0.0]),
            (180.0, 0.0, [0.0, 0.0, -1.0]),
            (270.0, 0.0, [1.0, 0.0, 0.0]),
            (0.0, -90.0, [0.0, 1.0, 0.0]),
            (0.0, 90.0, [0.0, -1.0, 0.0]),
        ] {
            let ray = view_ray(feet, PlayerPoseKind::Standing, yaw, pitch);
            assert_vec_close(ray.dir, expected);
            let len_sq = ray.dir.iter().map(|v| v * v).sum::<f64>();
            assert!((len_sq - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn diagonal_view_ray_matches_java_yaw_pitch_formula() {
        let yaw = 37.0f32;
        let pitch = -23.0f32;
        let yaw_rad = f64::from(yaw).to_radians();
        let pitch_rad = f64::from(pitch).to_radians();
        let cos_pitch = pitch_rad.cos();
        let ray = view_ray([1.0, 2.0, 3.0], PlayerPoseKind::Crouching, yaw, pitch);
        assert_vec_close(
            ray.dir,
            [
                -yaw_rad.sin() * cos_pitch,
                -pitch_rad.sin(),
                yaw_rad.cos() * cos_pitch,
            ],
        );
        assert_eq!(ray.origin[1], 2.0 + f64::from(1.27f32));
    }

    #[test]
    fn reach_policy_keeps_client_selection_exact_and_separate() {
        let policy = ReachPolicy::VANILLA;
        assert_eq!(policy.client_selection.block_survival, 4.5);
        assert_eq!(policy.client_selection.block_creative, 5.0);
        assert_eq!(policy.client_selection.entity_survival, 3.0);
        assert_eq!(policy.client_selection.entity_creative, 5.0);
        assert_eq!(policy.client_block_reach(false), 4.5);
        assert_eq!(policy.client_block_reach(true), 5.0);
        assert_eq!(policy.client_entity_reach(false), 3.0);
        assert_eq!(policy.client_entity_reach(true), 5.0);
        assert_eq!(policy.server_acceptance, policy.client_selection);

        let lenient = ReachPolicy::with_server_slack(0.5, 0.2);
        assert_eq!(lenient.client_block_reach(false), 4.5);
        assert_eq!(lenient.client_entity_reach(false), 3.0);
        assert_eq!(lenient.server_block_acceptance(false), 5.0);
        assert_eq!(lenient.server_block_acceptance(true), 5.5);
        assert_eq!(lenient.server_entity_acceptance(false), 3.2);
        assert_eq!(lenient.server_entity_acceptance(true), 5.2);
    }
}
