#![allow(clippy::disallowed_methods)] // Tests may unwrap; contract code may not.

extern crate std;

use super::*;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// allocate: table-driven boundary cases (SPEC.md §8.3)
// ---------------------------------------------------------------------------

#[test]
fn allocate_boundary_table() {
    struct Case {
        name: &'static str,
        total: i128,
        weights: &'static [i128],
        expected: Result<&'static [i128], MathError>,
    }
    let cases = [
        Case {
            name: "even split",
            total: 300,
            weights: &[1, 1, 1],
            expected: Ok(&[100, 100, 100]),
        },
        Case {
            name: "one unit left over, lowest index wins tie",
            total: 100,
            weights: &[1, 1, 1],
            expected: Ok(&[34, 33, 33]),
        },
        Case {
            name: "two units left over",
            total: 101,
            weights: &[1, 1, 1],
            expected: Ok(&[34, 34, 33]),
        },
        Case {
            name: "largest remainder beats index",
            // exact shares: 3.3, 3.3, 3.4 -> floors 3,3,3, one unit to idx 2
            total: 10,
            weights: &[33, 33, 34],
            expected: Ok(&[3, 3, 4]),
        },
        Case {
            name: "zero total",
            total: 0,
            weights: &[5, 7],
            expected: Ok(&[0, 0]),
        },
        Case {
            name: "zero total, no recipients",
            total: 0,
            weights: &[],
            expected: Ok(&[]),
        },
        Case {
            name: "zero-weight entries get nothing",
            total: 7,
            weights: &[0, 1, 0, 1, 0],
            expected: Ok(&[0, 4, 0, 3, 0]),
        },
        Case {
            name: "single recipient takes all",
            total: 12_345,
            weights: &[9],
            expected: Ok(&[12_345]),
        },
        Case {
            name: "more recipients than units",
            total: 3,
            weights: &[1, 1, 1, 1, 1],
            expected: Ok(&[1, 1, 1, 0, 0]),
        },
        Case {
            name: "all-zero weights",
            total: 10,
            weights: &[0, 0],
            expected: Err(MathError::NoRecipients),
        },
        Case {
            name: "no recipients",
            total: 10,
            weights: &[],
            expected: Err(MathError::NoRecipients),
        },
        Case {
            name: "negative total",
            total: -1,
            weights: &[1],
            expected: Err(MathError::Negative),
        },
        Case {
            name: "negative weight",
            total: 1,
            weights: &[1, -1],
            expected: Err(MathError::Negative),
        },
        Case {
            name: "weight sum overflows u128",
            total: 1,
            weights: &[i128::MAX, i128::MAX, i128::MAX],
            expected: Err(MathError::Overflow),
        },
    ];

    for case in cases {
        let got = allocate(case.total, case.weights);
        match case.expected {
            Ok(exp) => assert_eq!(got.as_deref(), Ok(exp), "{}", case.name),
            Err(e) => assert_eq!(got, Err(e), "{}", case.name),
        }
    }
}

#[test]
fn allocate_handles_products_wider_than_128_bits() {
    // total * w overflows u128 but the result fits: needs the 256-bit path.
    let total = i128::MAX;
    let out = allocate(total, &[i128::MAX, i128::MAX]).unwrap();
    assert_eq!(out.iter().sum::<i128>(), total);
    assert_eq!(out[0] - out[1], 1);
}

#[test]
fn allocate_is_deterministic() {
    let w = [3, 1, 4, 1, 5, 9, 2, 6];
    assert_eq!(allocate(1_000_003, &w), allocate(1_000_003, &w));
}

// ---------------------------------------------------------------------------
// split / mul_div / route_dust
// ---------------------------------------------------------------------------

#[test]
fn split_bps_table() {
    assert_eq!(
        split_bps(1_000, 2_500),
        Ok(Split {
            cut: 250,
            rest: 750
        })
    );
    assert_eq!(
        split_bps(999, 3_333),
        Ok(Split {
            cut: 332,
            rest: 667
        })
    );
    assert_eq!(split_bps(1, 9_999), Ok(Split { cut: 0, rest: 1 }));
    assert_eq!(
        split_bps(1_000, 0),
        Ok(Split {
            cut: 0,
            rest: 1_000
        })
    );
    assert_eq!(
        split_bps(1_000, 10_000),
        Ok(Split {
            cut: 1_000,
            rest: 0
        })
    );
    assert_eq!(split_bps(1_000, 10_001), Err(MathError::InvalidRatio));
    assert_eq!(split_bps(1_000, -1), Err(MathError::InvalidRatio));
    assert_eq!(split_bps(-5, 100), Err(MathError::Negative));
    // Would overflow with naive `amount * bps`.
    let s = split_bps(i128::MAX, 5_000).unwrap();
    assert_eq!(s.cut + s.rest, i128::MAX);
}

#[test]
fn split_ratio_table() {
    assert_eq!(split_ratio(10, 1, 3), Ok(Split { cut: 3, rest: 7 }));
    assert_eq!(split_ratio(10, 4, 3), Err(MathError::InvalidRatio));
    assert_eq!(split_ratio(10, 0, 0), Err(MathError::DivisionByZero));
}

#[test]
fn mul_div_floor_table() {
    assert_eq!(mul_div_floor(7, 3, 2), Ok(10));
    assert_eq!(mul_div_floor(0, 3, 2), Ok(0));
    assert_eq!(mul_div_floor(1, 1, 0), Err(MathError::DivisionByZero));
    assert_eq!(mul_div_floor(-1, 1, 1), Err(MathError::Negative));
    assert_eq!(
        mul_div_floor(i128::MAX, i128::MAX, i128::MAX),
        Ok(i128::MAX)
    );
    assert_eq!(mul_div_floor(i128::MAX, 2, 1), Err(MathError::Overflow));
    assert_eq!(
        mul_div_floor(i128::MAX, i128::MAX, 1),
        Err(MathError::Overflow)
    );
}

#[test]
fn route_dust_table() {
    assert_eq!(route_dust(10, 7), Ok((7, 3)));
    assert_eq!(route_dust(10, 10), Ok((10, 0)));
    assert_eq!(route_dust(10, 11), Err(MathError::Negative));
}

#[test]
fn wide_multiply_matches_u128_when_it_fits() {
    let samples = [
        0u128,
        1,
        2,
        u64::MAX as u128,
        (u64::MAX as u128) + 7,
        1 << 100,
    ];
    for a in samples {
        for b in samples {
            if let Some(p) = a.checked_mul(b) {
                assert_eq!(mul_wide(a, b), (0, p));
            }
        }
    }
    assert_eq!(mul_wide(u128::MAX, u128::MAX), (u128::MAX - 1, 1));
}

// ---------------------------------------------------------------------------
// Property tests: conservation and fairness (SPEC.md §8.4)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn allocate_conserves_and_is_fair(
        total in 0i128..1_000_000_000_000_000,
        weights in prop::collection::vec(0i128..1_000_000_000_000, 1..40),
    ) {
        let w_sum: i128 = weights.iter().sum();
        match allocate(total, &weights) {
            Ok(out) => {
                prop_assert_eq!(out.len(), weights.len());
                prop_assert_eq!(out.iter().sum::<i128>(), total);
                for (o, w) in out.iter().zip(&weights) {
                    prop_assert!(*o >= 0);
                    if *w == 0 {
                        prop_assert_eq!(*o, 0);
                    }
                    // Within one unit of the exact share: |o*W - total*w| < W.
                    let exact = total * *w;
                    prop_assert!((o * w_sum - exact).abs() < w_sum);
                }
            }
            Err(e) => {
                prop_assert_eq!(e, MathError::NoRecipients);
                prop_assert!(total > 0 && w_sum == 0);
            }
        }
    }

    #[test]
    fn allocate_conserves_at_full_range(
        total in 0i128..=i128::MAX,
        weights in prop::collection::vec(0i128..=i128::MAX / 64, 1..32),
    ) {
        if let Ok(out) = allocate(total, &weights) {
            prop_assert_eq!(out.iter().sum::<i128>(), total);
        }
    }

    #[test]
    fn split_bps_conserves(amount in 0i128..=i128::MAX, bps in 0i128..=10_000) {
        let s = split_bps(amount, bps).unwrap();
        prop_assert_eq!(s.cut + s.rest, amount);
        prop_assert!(s.cut >= 0 && s.rest >= 0);
    }

    #[test]
    fn mul_div_floor_matches_reference(
        a in 0i128..(1i128 << 62),
        b in 0i128..(1i128 << 62),
        c in 1i128..(1i128 << 62),
    ) {
        prop_assert_eq!(mul_div_floor(a, b, c), Ok(a * b / c));
    }

    #[test]
    fn wide_division_inverts_multiply(a in any::<u128>(), b in any::<u128>(), d in 1u128..) {
        let (hi, lo) = mul_wide(a, b);
        if hi < d {
            let (q, r) = div_wide(hi, lo, d);
            prop_assert!(r < d);
            // q * d + r must reconstruct (hi, lo).
            let (qh, ql) = mul_wide(q, d);
            let (sum_lo, carry) = ql.overflowing_add(r);
            prop_assert_eq!((qh + carry as u128, sum_lo), (hi, lo));
        }
    }
}
