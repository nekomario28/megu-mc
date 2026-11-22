// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use megu_core::math_opt_foundation::{
    chebyshev_eval, classify_subtile_lipschitz, exp_decay_exact, fast_exp_neg,
    fast_exp_neg_chebyshev, fast_exp_neg_unit, hermite_eval_and_deriv, morton_decode_3d,
    morton_decode_3d_u16, morton_encode_3d, morton_encode_3d_u16, morton_section_index,
    symplectic_verlet_step, IntegralGrid2D, IntegralRgbGrid2D, IntegralVolume3D, IntervalF64,
    LipschitzTileClassifier, TileSignDecision,
};

fn next_random(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

fn scalar_field(x: f64, y: f64, z: f64) -> f64 {
    (0.2 * x).sin() + (0.2 * z).cos() + 0.1 * y
}

#[test]
fn lipschitz_proofs_never_misclassify_any_of_64_voxels() {
    // ||grad f|| <= sqrt(0.2^2 + 0.1^2 + 0.2^2) = 0.3.
    let mut positive = 0;
    let mut negative = 0;
    let mut mixed = 0;
    for tile_y in (-24..24).step_by(4) {
        for tile_z in (-24..24).step_by(4) {
            for tile_x in (-24..24).step_by(4) {
                let center = scalar_field(
                    f64::from(tile_x) + 1.5,
                    f64::from(tile_y) + 1.5,
                    f64::from(tile_z) + 1.5,
                );
                let decision = classify_subtile_lipschitz(center, 0.3, [1.5; 3]);
                assert_eq!(
                    decision,
                    LipschitzTileClassifier::classify_subtile_lipschitz(center, 0.3, [1.5; 3])
                );
                match decision {
                    TileSignDecision::UniformPositive => positive += 1,
                    TileSignDecision::UniformNegative => negative += 1,
                    TileSignDecision::MixedRequiresEvaluation => mixed += 1,
                }
                for dy in 0..4 {
                    for dz in 0..4 {
                        for dx in 0..4 {
                            let value = scalar_field(
                                f64::from(tile_x + dx),
                                f64::from(tile_y + dy),
                                f64::from(tile_z + dz),
                            );
                            match decision {
                                TileSignDecision::UniformPositive => assert!(value > 0.0),
                                TileSignDecision::UniformNegative => assert!(value < 0.0),
                                TileSignDecision::MixedRequiresEvaluation => {}
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(positive > 0 && negative > 0 && mixed > 0);
}

#[test]
fn lipschitz_classifier_rejects_unprovable_or_invalid_bounds() {
    use TileSignDecision::{MixedRequiresEvaluation as Mixed, UniformNegative, UniformPositive};

    assert_eq!(
        classify_subtile_lipschitz(5.0, 1.0, [1.0; 3]),
        UniformPositive
    );
    assert_eq!(
        classify_subtile_lipschitz(-5.0, 1.0, [1.0; 3]),
        UniformNegative
    );
    assert_eq!(classify_subtile_lipschitz(0.0, 0.0, [0.0; 3]), Mixed);
    assert_eq!(
        classify_subtile_lipschitz(1.0, 0.0, [0.0; 3]),
        UniformPositive
    );
    assert_eq!(classify_subtile_lipschitz(1.0, -1.0, [1.0; 3]), Mixed);
    assert_eq!(classify_subtile_lipschitz(f64::NAN, 1.0, [1.0; 3]), Mixed);
    assert_eq!(
        classify_subtile_lipschitz(1.0, f64::INFINITY, [1.0; 3]),
        Mixed
    );
    assert_eq!(
        classify_subtile_lipschitz(1.0, 1.0, [f64::NAN, 1.0, 1.0]),
        Mixed
    );
    assert_eq!(classify_subtile_lipschitz(1.0, 1.0, [f64::MAX; 3]), Mixed);
    // A bound must not round tiny nonzero differences to a false zero.
    assert_eq!(
        classify_subtile_lipschitz(f64::from_bits(1), 1.0, [f64::from_bits(1), 0.0, 0.0]),
        Mixed
    );
}

#[test]
fn interval_arithmetic_contains_all_corner_operations() {
    let mut state = 1_u64;
    for _ in 0..1_000 {
        let mut endpoints = [0.0; 4];
        for endpoint in &mut endpoints {
            *endpoint = (next_random(&mut state) % 20_001) as f64 / 31.0 - 300.0;
        }
        let a = IntervalF64 {
            min: endpoints[0].min(endpoints[1]),
            max: endpoints[0].max(endpoints[1]),
        };
        let b = IntervalF64 {
            min: endpoints[2].min(endpoints[3]),
            max: endpoints[2].max(endpoints[3]),
        };
        let sum = a.add(b);
        let difference = a.sub(b);
        let product = a.mul(b);
        for x in [a.min, a.max] {
            for y in [b.min, b.max] {
                assert!((sum.min..=sum.max).contains(&(x + y)));
                assert!((difference.min..=difference.max).contains(&(x - y)));
                assert!((product.min..=product.max).contains(&(x * y)));
            }
        }
        for factor in [-2.0, 0.0, 3.25] {
            let scaled = a.scale(factor);
            assert!((scaled.min..=scaled.max).contains(&(a.min * factor)));
            assert!((scaled.min..=scaled.max).contains(&(a.max * factor)));
        }
    }
    assert!(IntervalF64 {
        min: -1.0,
        max: 2.0
    }
    .contains_zero());
    assert!(IntervalF64 { min: 0.1, max: 2.0 }.strictly_positive());
    assert!(IntervalF64 {
        min: -2.0,
        max: -0.1
    }
    .strictly_negative());
    let invalid = IntervalF64 {
        min: f64::NAN,
        max: 1.0,
    };
    assert!(invalid
        .mul(IntervalF64 { min: 1.0, max: 2.0 })
        .contains_zero());
    assert!(!invalid.strictly_positive());
    assert!(!invalid.strictly_negative());
    let whole = IntervalF64 {
        min: 0.0,
        max: f64::INFINITY,
    }
    .mul(IntervalF64 { min: 0.0, max: 1.0 });
    assert_eq!(whole.min, f64::NEG_INFINITY);
    assert_eq!(whole.max, f64::INFINITY);
}

#[test]
fn integral_2d_matches_bruteforce_including_clipped_and_15_by_15_windows() {
    let (width, height) = (31, 29);
    let mut state = 1_u64;
    let values: Vec<f64> = (0..width * height)
        .map(|_| (next_random(&mut state) % 20_001) as f64 / 17.0 - 500.0)
        .collect();
    let grid = IntegralGrid2D::new(width, height, &values).unwrap();

    for query in 0..2_000 {
        let (x0, z0, x1, z1) = if query % 5 == 0 {
            let x = next_random(&mut state) as usize % width;
            let z = next_random(&mut state) as usize % height;
            (x, z, x + 14, z + 14)
        } else {
            (
                next_random(&mut state) as usize % (width + 10),
                next_random(&mut state) as usize % (height + 10),
                next_random(&mut state) as usize % (width + 10),
                next_random(&mut state) as usize % (height + 10),
            )
        };
        let mut expected = 0.0;
        let mut count = 0;
        if x0 <= x1 && z0 <= z1 {
            for z in z0..=z1.min(height - 1) {
                for x in x0..=x1.min(width - 1) {
                    expected += values[z * width + x];
                    count += 1;
                }
            }
        }
        assert!((grid.box_sum(x0, z0, x1, z1) - expected).abs() < 1e-9);
        if count > 0 {
            assert!((grid.box_mean(x0, z0, x1, z1) - expected / count as f64).abs() < 1e-9);
        } else {
            assert_eq!(grid.box_mean(x0, z0, x1, z1), 0.0);
        }
    }
    assert!(IntegralGrid2D::new(2, 2, &[1.0]).is_none());
    assert!(IntegralGrid2D::new(1, 1, &[f64::NAN]).is_none());
    assert_eq!(
        IntegralGrid2D::new(0, 0, &[]).unwrap().box_sum(0, 0, 1, 1),
        0.0
    );
}

#[test]
fn integral_rgb_15_by_15_biome_blend_matches_bruteforce() {
    let (width, height) = (33, 31);
    let values: Vec<[f64; 3]> = (0..height)
        .flat_map(|z| {
            (0..width).map(move |x| {
                [
                    ((x * 31 + z * 17) % 256) as f64,
                    ((x * 19 + z * 23) % 256) as f64,
                    ((x * 11 + z * 29) % 256) as f64,
                ]
            })
        })
        .collect();
    let grid = IntegralRgbGrid2D::new(width, height, &values).unwrap();
    for z0 in 0..height {
        for x0 in 0..width {
            let (x1, z1) = (x0 + 14, z0 + 14);
            let (mut sums, mut count) = ([0.0; 3], 0);
            for z in z0..=z1.min(height - 1) {
                for x in x0..=x1.min(width - 1) {
                    for (channel, sum) in sums.iter_mut().enumerate() {
                        *sum += values[z * width + x][channel];
                    }
                    count += 1;
                }
            }
            let actual_sum = grid.box_sum(x0, z0, x1, z1);
            let actual_mean = grid.box_mean(x0, z0, x1, z1);
            for channel in 0..3 {
                assert!((actual_sum[channel] - sums[channel]).abs() < 1e-9);
                assert!((actual_mean[channel] - sums[channel] / count as f64).abs() < 1e-9);
            }
        }
    }
    assert_eq!(grid.box_mean(35, 35, 40, 40), [0.0; 3]);
    assert!(IntegralRgbGrid2D::new(1, 1, &[[f64::NAN, 2.0, 3.0]]).is_none());
}

#[test]
fn integral_volume_inclusion_exclusion_matches_bruteforce() {
    let (width, height, depth) = (9, 8, 7);
    let mut state = 123_u64;
    let values: Vec<i64> = (0..width * height * depth)
        .map(|_| (next_random(&mut state) % 2_001) as i64 - 1_000)
        .collect();
    let volume = IntegralVolume3D::new(width, height, depth, &values).unwrap();
    for _ in 0..2_000 {
        let (x0, y0, z0) = (
            next_random(&mut state) as usize % (width + 3),
            next_random(&mut state) as usize % (height + 3),
            next_random(&mut state) as usize % (depth + 3),
        );
        let (x1, y1, z1) = (
            next_random(&mut state) as usize % (width + 3),
            next_random(&mut state) as usize % (height + 3),
            next_random(&mut state) as usize % (depth + 3),
        );
        let mut expected = 0;
        if x0 <= x1 && y0 <= y1 && z0 <= z1 {
            for y in y0..=y1.min(height - 1) {
                for z in z0..=z1.min(depth - 1) {
                    for x in x0..=x1.min(width - 1) {
                        expected += values[(y * depth + z) * width + x];
                    }
                }
            }
        }
        assert_eq!(volume.box_sum_3d(x0, y0, z0, x1, y1, z1), expected);
    }
    assert_eq!(
        volume.box_sum_3d(0, 0, 0, width, height, depth),
        values.iter().sum()
    );
    assert!(IntegralVolume3D::new(1, 1, 2, &[i64::MAX, 1]).is_none());
    assert!(IntegralVolume3D::new(2, 2, 2, &[1]).is_none());
    let extremes = IntegralVolume3D::new(2, 1, 1, &[i64::MIN, i64::MAX]).unwrap();
    assert_eq!(extremes.box_sum_3d(0, 0, 0, 0, 0, 0), i64::MIN);
    assert_eq!(extremes.box_sum_3d(1, 0, 0, 1, 0, 0), i64::MAX);
    assert_eq!(extremes.box_sum_3d(0, 0, 0, 1, 0, 0), -1);
    assert_eq!(
        IntegralVolume3D::new(0, 0, 0, &[])
            .unwrap()
            .box_sum_3d(0, 0, 0, 0, 0, 0),
        0
    );
}

#[test]
fn chebyshev_minimax_and_analytic_hermite_derivative() {
    assert_eq!(chebyshev_eval(0.5, 0.0, 1.0, &[1.0, 2.0, 3.0]), -2.0);
    assert_eq!(chebyshev_eval(0.3, 0.0, 1.0, &[]), 0.0);
    let mut maximum_relative_error = 0.0_f64;
    for i in 0..=20_000 {
        let x = i as f64 / 1_000.0;
        let expected = (-x).exp();
        let actual = fast_exp_neg(x);
        assert!((actual - expected).abs() < 2e-6);
        maximum_relative_error = maximum_relative_error.max(((actual - expected) / expected).abs());
    }
    assert!(
        maximum_relative_error < 1e-6,
        "relative error {maximum_relative_error}"
    );
    for i in 0..=1_000 {
        let x = i as f64 / 1_000.0;
        assert!((fast_exp_neg_unit(x) - (-x).exp()).abs() < 2e-11);
        assert_eq!(fast_exp_neg_unit(x), fast_exp_neg_chebyshev(x));
    }
    assert_eq!(fast_exp_neg(f64::NAN), 0.0);
    assert_eq!(fast_exp_neg(f64::INFINITY), 0.0);
    assert_eq!(fast_exp_neg(1e9), 0.0);

    let mut state = 42_u64;
    for _ in 0..1_000 {
        let mut samples = [0.0; 5];
        for sample in &mut samples {
            *sample = (next_random(&mut state) % 20_001) as f64 / 2_000.0 - 5.0;
        }
        let [p0, m0, p1, m1, t] = samples;
        let epsilon = 1e-5;
        let (value, derivative) = hermite_eval_and_deriv(p0, m0, p1, m1, t);
        let before = hermite_eval_and_deriv(p0, m0, p1, m1, t - epsilon).0;
        let after = hermite_eval_and_deriv(p0, m0, p1, m1, t + epsilon).0;
        assert!(value.is_finite());
        assert!((derivative - (after - before) / (2.0 * epsilon)).abs() < 1e-7);
    }
    let p0 = hermite_eval_and_deriv(2.0, 3.0, 7.0, -4.0, 0.0);
    let p1 = hermite_eval_and_deriv(2.0, 3.0, 7.0, -4.0, 1.0);
    assert_eq!(p0, (2.0, 3.0));
    assert_eq!(p1, (7.0, -4.0));
}

#[test]
fn velocity_verlet_has_bounded_energy_and_decay_is_framerate_independent() {
    let (mut pos, mut vel) = ([1.0, 0.0, 0.0], [0.0, 0.0, 0.0]);
    let mut maximum_error = 0.0_f64;
    for _ in 0..10_000 {
        (pos, vel) = symplectic_verlet_step(pos, vel, |p| p.map(|coordinate| -coordinate), 0.01);
        let energy = 0.5 * (pos[0] * pos[0] + vel[0] * vel[0]);
        maximum_error = maximum_error.max((energy - 0.5).abs());
    }
    assert!(maximum_error < 1e-3, "maximum energy error {maximum_error}");
    assert_eq!(pos[1], 0.0);
    assert_eq!(pos[2], 0.0);

    let one_step = exp_decay_exact(100.0, -5.0, 1.75, 1.0);
    let mut many_steps = 100.0;
    for _ in 0..100 {
        many_steps = exp_decay_exact(many_steps, -5.0, 1.75, 0.01);
    }
    assert!((one_step - many_steps).abs() < 1e-12);
    assert_eq!(exp_decay_exact(3.0, 5.0, 1.0, 0.0), 3.0);
    assert_eq!(exp_decay_exact(3.0, 5.0, 0.0, 1.0), 3.0);
}

#[test]
fn morton_mapping_is_bijective_and_every_aligned_cube_is_contiguous() {
    let mut seen = [false; 4096];
    for y in 0..16 {
        for z in 0..16 {
            for x in 0..16 {
                let index = morton_section_index(x, y, z).unwrap();
                assert!(index < 4096);
                assert!(!seen[index]);
                seen[index] = true;
                assert_eq!(
                    morton_decode_3d(index as u32),
                    (x as u32, y as u32, z as u32)
                );
            }
        }
    }
    assert!(seen.iter().all(|visited| *visited));
    for tile_size in [2, 4] {
        for tile_y in (0..16).step_by(tile_size) {
            for tile_z in (0..16).step_by(tile_size) {
                for tile_x in (0..16).step_by(tile_size) {
                    let mut indices = Vec::new();
                    for y in tile_y..tile_y + tile_size {
                        for z in tile_z..tile_z + tile_size {
                            for x in tile_x..tile_x + tile_size {
                                indices.push(morton_section_index(x, y, z).unwrap());
                            }
                        }
                    }
                    indices.sort_unstable();
                    let size = tile_size * tile_size * tile_size;
                    assert_eq!(indices.len(), size);
                    assert_eq!(indices[0] % size, 0);
                    for (offset, index) in indices.iter().enumerate() {
                        assert_eq!(*index, indices[0] + offset);
                    }
                }
            }
        }
    }
    assert_eq!(morton_section_index(16, 0, 0), None);
    assert_eq!(morton_section_index(0, 16, 0), None);
    assert_eq!(morton_section_index(0, 0, 16), None);
    for x in [0, 1, 7, 127, 1023] {
        for y in [0, 2, 32, 511, 1023] {
            for z in [0, 3, 63, 255, 1023] {
                assert_eq!(morton_decode_3d(morton_encode_3d(x, y, z)), (x, y, z));
            }
        }
    }
    for x in [0, 1, 15, 1024, u16::MAX] {
        for y in [0, 17, 4095, u16::MAX] {
            for z in [0, 2, 16000, u16::MAX] {
                assert_eq!(
                    morton_decode_3d_u16(morton_encode_3d_u16(x, y, z)),
                    (x, y, z)
                );
            }
        }
    }
}
