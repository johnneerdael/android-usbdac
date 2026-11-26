/// Exact terminal/source cadence ratio. Reduction bounds both sides to u32:
/// larger coprime sides cannot connect two positive u32 frequencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RateRatio {
    numerator: u32,
    denominator: u32,
}

impl RateRatio {
    pub(crate) const IDENTITY: Self = Self { numerator: 1, denominator: 1 };

    pub(crate) fn from_factors(factors: &[(u16, u16)]) -> Result<Self, String> {
        if factors.iter().any(|&(n, d)| n == 0 || d == 0) {
            return Err("clock multiplier ratio contains zero".into());
        }
        let mut numerators: Vec<u32> = factors.iter().map(|&(n, _)| u32::from(n)).collect();
        let mut denominators: Vec<u32> = factors.iter().map(|&(_, d)| u32::from(d)).collect();
        // Cancel across the entire chain before multiplying. In particular,
        // several large forward multipliers can be cancelled by later inverses.
        for n in &mut numerators {
            for d in &mut denominators {
                let common = gcd(*n, *d);
                *n /= common;
                *d /= common;
            }
        }
        let product = |values: &[u32]| {
            values.iter().try_fold(1u32, |value, factor| value.checked_mul(*factor))
                .ok_or_else(|| "clock multiplier ratio exceeds positive u32 rate bounds".to_string())
        };
        Ok(Self { numerator: product(&numerators)?, denominator: product(&denominators)? })
    }

    pub(crate) fn source_rate(self, wire: u32) -> Option<u32> {
        exact_scale(wire, self.denominator, self.numerator)
    }

    pub(crate) fn wire_rate(self, source: u32) -> Option<u32> {
        exact_scale(source, self.numerator, self.denominator)
    }

    /// Project a source MIN/MAX/RES grid to exactly representable terminal Hz.
    /// RES=0 requires a MIN=MAX singleton, as in existing discrete rates.
    pub(crate) fn project_range(self, min: u32, max: u32, res: u32) -> Option<(u32, u32, u32)> {
        if min == 0 || max < min {
            return None;
        }
        if res == 0 && min != max {
            return None;
        }
        if res == 0 || min == max {
            let rate = self.wire_rate(min)?;
            return Some((rate, rate, 0));
        }
        let n = u64::from(self.numerator);
        let d = u64::from(self.denominator);
        // Cap in source space first; projecting MAX itself may overflow even
        // though a nonempty prefix of the source grid is usable.
        let upper = u64::from(max).min(u64::from(u32::MAX) * d / n);
        let min = u64::from(min);
        let res = u64::from(res);
        if upper < min {
            return None;
        }
        let common = u64::from(gcd(res as u32, self.denominator));
        if min % common != 0 {
            return None;
        }
        // min + k*res == 0 (mod d). Removing the gcd leaves an
        // invertible coefficient and one solution per d/gcd source steps.
        let period = d / common;
        let first_k = if period == 1 {
            0
        } else {
            let rhs = (period - (min / common) % period) % period;
            rhs.checked_mul(modular_inverse((res / common) % period, period))? % period
        };
        let max_k = (upper - min) / res;
        if first_k > max_k {
            return None;
        }
        let last_k = first_k.checked_add(((max_k - first_k) / period).checked_mul(period)?)?;
        let first_source = min.checked_add(first_k.checked_mul(res)?)?;
        let last_source = min.checked_add(last_k.checked_mul(res)?)?;
        let first = u32::try_from(first_source.checked_mul(n)? / d).ok()?;
        let last = u32::try_from(last_source.checked_mul(n)? / d).ok()?;
        let step = if first_k == last_k {
            0
        } else {
            u32::try_from((res / common).checked_mul(n)?).ok()?
        };
        Some((first, last, step))
    }
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn exact_scale(rate: u32, numerator: u32, denominator: u32) -> Option<u32> {
    if rate == 0 || rate % denominator != 0 {
        return None;
    }
    (rate / denominator).checked_mul(numerator)
}

/// Inputs are coprime, positive, and at most u32::MAX. Wider signed
/// intermediates keep the extended Euclidean steps bounded without wrapping.
fn modular_inverse(value: u64, modulus: u64) -> u64 {
    let (mut remainder, mut next_remainder) = (i128::from(modulus), i128::from(value));
    let (mut coefficient, mut next_coefficient) = (0i128, 1i128);
    while next_remainder != 0 {
        let quotient = remainder / next_remainder;
        (remainder, next_remainder) = (next_remainder, remainder - quotient * next_remainder);
        (coefficient, next_coefficient) = (next_coefficient, coefficient - quotient * next_coefficient);
    }
    coefficient.rem_euclid(i128::from(modulus)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(n: u16, d: u16) -> RateRatio {
        RateRatio::from_factors(&[(n, d)]).unwrap()
    }

    #[test]
    fn factors_cancel_before_bounded_product() {
        let mut factors = vec![(65535, 1); 16];
        factors.extend(vec![(1, 65535); 16]);
        assert_eq!(RateRatio::from_factors(&factors).unwrap(), RateRatio::IDENTITY);
        assert_eq!(RateRatio::from_factors(&[]).unwrap(), RateRatio::IDENTITY);
        assert!(RateRatio::from_factors(&[(65535, 1); 3]).is_err());
        assert!(RateRatio::from_factors(&[(1, 65535); 3]).is_err());
        assert!(RateRatio::from_factors(&[(0, 1)]).is_err());
        assert!(RateRatio::from_factors(&[(1, 0)]).is_err());
        assert_eq!(ratio(6, 4), ratio(3, 2));
    }

    #[test]
    fn rates_are_exact_nonzero_and_invertible() {
        let r = ratio(3, 2);
        assert_eq!(r.wire_rate(48000), Some(72000));
        assert_eq!(r.source_rate(72000), Some(48000));
        assert_eq!(r.wire_rate(48001), None);
        assert_eq!(r.source_rate(72001), None);
        assert_eq!(r.wire_rate(0), None);
        assert_eq!(r.source_rate(0), None);
        assert_eq!(r.wire_rate(u32::MAX - 1), None);
        assert_eq!(ratio(1, 2).source_rate(u32::MAX), None);
        assert_eq!(RateRatio::IDENTITY.wire_rate(u32::MAX), Some(u32::MAX));
        for source in 1..1000 {
            if let Some(wire) = r.wire_rate(source) {
                assert_eq!(r.source_rate(wire), Some(source));
            }
        }
    }

    #[test]
    fn projection_handles_discrete_and_identity_shapes() {
        assert_eq!(ratio(3, 2).project_range(48000, 48000, 0), Some((72000, 72000, 0)));
        assert_eq!(ratio(3, 2).project_range(48000, 96000, 0), None);
        assert_eq!(ratio(3, 2).project_range(48001, 96000, 0), None);
        assert_eq!(RateRatio::IDENTITY.project_range(1, 100, 7), Some((1, 99, 7)));
        assert_eq!(RateRatio::IDENTITY.project_range(1, 99, 7), Some((1, 99, 7)));
        assert_eq!(RateRatio::IDENTITY.project_range(8, 8, 7), Some((8, 8, 0)));
        assert_eq!(ratio(2, 3).project_range(0, 10, 1), None);
        assert_eq!(ratio(2, 3).project_range(10, 1, 1), None);
    }

    #[test]
    fn projection_solves_irregular_source_grids() {
        // Source 1,6,11,16,21,26: only 6 and 21 divide by three.
        assert_eq!(ratio(2, 3).project_range(1, 26, 5), Some((4, 14, 10)));
        assert_eq!(ratio(3, 4).project_range(2, 22, 6), Some((6, 15, 9)));
        assert_eq!(ratio(3, 4).project_range(1, 100, 6), None);
        assert_eq!(ratio(2, 3).project_range(1, 10, 5), Some((4, 4, 0)));
        assert_eq!(ratio(2, 3).project_range(1, 5, 5), None);
        assert_eq!(ratio(5, 2).project_range(2, 16, 4), Some((5, 35, 10)));
    }

    #[test]
    fn projection_caps_overflow_without_scanning_the_grid() {
        assert_eq!(ratio(2, 1).project_range(1, u32::MAX, 1), Some((2, u32::MAX - 1, 2)));
        assert_eq!(ratio(2, 1).project_range(u32::MAX / 2 + 1, u32::MAX, 1), None);
        assert_eq!(ratio(65535, 1).project_range(1, u32::MAX, 1), Some((65535, u32::MAX, 65535)));
        let r = RateRatio::from_factors(&[(65535, 1), (65535, 1)]).unwrap();
        assert_eq!(r.project_range(1, u32::MAX, 1), Some((4294836225, 4294836225, 0)));
        assert_eq!(ratio(1, 65535).project_range(1, u32::MAX, 1), Some((1, 65537, 1)));
    }

    #[test]
    fn projected_members_match_exact_inverse_on_small_grids() {
        for (n, d) in [(2, 3), (3, 4), (4, 3), (7, 5)] {
            let r = ratio(n, d);
            for min in 1..12 {
                for res in 1..12 {
                    let sources: Vec<_> = (min..=100).step_by(res as usize)
                        .filter_map(|source| r.wire_rate(source)).collect();
                    let projected = r.project_range(min, 100, res);
                    if sources.is_empty() {
                        assert_eq!(projected, None);
                    } else {
                        let (first, last, step) = projected.unwrap();
                        let actual: Vec<_> = if step == 0 { vec![first] } else {
                            (first..=last).step_by(step as usize).collect()
                        };
                        assert_eq!(actual, sources);
                        for wire in actual {
                            let source = r.source_rate(wire).unwrap();
                            assert_eq!((source - min) % res, 0);
                        }
                    }
                }
            }
        }
    }
}
