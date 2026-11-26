//! Reading and writing samples, dither, and saturation.
//!
//! Integer samples are handled as `i32` values with their significant bits at the top and zeros
//! below ("left-justified"): a 16-bit sample `s` is `s << 16`, a packed 24-bit one `s << 8`. In that
//! form widening needs no arithmetic, narrowing is a rounding at a bit position, and the same code
//! serves every width.

use super::format::Narrowing;

/// A xorshift64* pseudo-random generator: tiny, fast, allocation-free and reproducible from a seed.
pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Triangular noise in the open interval (-1, 1): the difference of two uniform numbers in [0, 1).
    #[inline]
    pub fn tpdf_unit(&mut self) -> f64 {
        let r = self.next_u64();
        let a = (r >> 32) as f64 / 4_294_967_296.0;
        let b = (r & 0xffff_ffff) as f64 / 4_294_967_296.0;
        a - b
    }

    /// Triangular noise in (-`lsb`, `lsb`) as an integer, `lsb` at most 2^24.
    #[inline]
    pub fn tpdf_int(&mut self, lsb: u32) -> i64 {
        let r = self.next_u64();
        let a = ((r >> 32) * u64::from(lsb)) >> 32;
        let b = ((r & 0xffff_ffff) * u64::from(lsb)) >> 32;
        a as i64 - b as i64
    }
}

/// Reads one integer sample of `slot` bytes, little-endian, as a left-justified `i32`. The padding
/// bits below the `bits` significant ones are ignored.
#[inline]
pub(crate) fn read_int(bytes: &[u8], slot: usize, bits: u8) -> i32 {
    let mut word = 0u32;
    for (i, byte) in bytes[..slot].iter().enumerate() {
        word |= u32::from(*byte) << (8 * i);
    }
    let value = (word << (32 - 8 * slot)) as i32;
    value & keep_mask(bits)
}

/// Writes a left-justified sample into `slot` bytes, little-endian (the low padding bits are zero if `value` has them zero).
#[inline]
pub(crate) fn write_int(out: &mut [u8], slot: usize, value: i32) {
    let word = (value as u32) >> (32 - 8 * slot);
    for (i, byte) in out[..slot].iter_mut().enumerate() {
        *byte = (word >> (8 * i)) as u8;
    }
}

/// The mask that keeps the top `bits` bits of a left-justified sample.
#[inline]
pub(crate) fn keep_mask(bits: u8) -> i32 {
    !(((1u64 << (32 - u32::from(bits))) - 1) as u32 as i32)
}

/// A left-justified sample as a float in [-1, 1): exact, since an `f64` holds 32 bits.
#[inline]
pub(crate) fn int_to_f64(value: i32) -> f64 {
    f64::from(value) / 2_147_483_648.0
}

/// Reads a float sample; a NaN or an infinity becomes silence.
#[inline]
pub(crate) fn read_float(bytes: &[u8]) -> f64 {
    let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if value.is_finite() {
        f64::from(value)
    } else {
        0.0
    }
}

/// Reduces a left-justified sample to its top `bits` bits, by truncation or with TPDF dither (rounded
/// to nearest, saturating at the largest and smallest value). `clipped` counts saturations.
#[inline]
pub(crate) fn narrow(
    value: i32,
    bits: u8,
    mode: Narrowing,
    rng: &mut Rng,
    clipped: &mut u64,
) -> i32 {
    if bits >= 32 {
        return value;
    }
    let shift = 32 - u32::from(bits);
    match mode {
        Narrowing::Truncate => value & keep_mask(bits),
        Narrowing::TpdfDither => {
            let lsb = 1u32 << shift;
            let noisy = i64::from(value) + rng.tpdf_int(lsb) + i64::from(lsb >> 1);
            let level = noisy >> shift; // floor
            let max = (1i64 << (bits - 1)) - 1;
            let min = -(1i64 << (bits - 1));
            let kept = level.clamp(min, max);
            if kept != level {
                *clipped += 1;
            }
            (kept << shift) as i32
        }
    }
}

/// Quantises a float sample (nominal range -1 to 1) to `bits` bits, left-justified, by truncation (floor) or with
/// TPDF dither, saturating at full scale. `clipped` counts saturations.
#[inline]
pub(crate) fn float_to_int(
    x: f64,
    bits: u8,
    mode: Narrowing,
    rng: &mut Rng,
    clipped: &mut u64,
) -> i32 {
    let scale = (1u64 << (bits - 1)) as f64;
    let scaled = x * scale;
    let level = match mode {
        Narrowing::Truncate => scaled.floor(),
        Narrowing::TpdfDither => (scaled + rng.tpdf_unit() + 0.5).floor(),
    };
    let max = scale - 1.0;
    let min = -scale;
    if level > max || level < min {
        *clipped += 1;
    }
    let kept = level.clamp(min, max) as i64;
    (kept << (32 - u32::from(bits))) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_round_trip_through_every_slot() {
        let mut bytes = [0u8; 4];
        for (bits, slot) in [(16u8, 2usize), (24, 3), (24, 4), (32, 4), (16, 3), (8, 1)] {
            for value in [
                0i64,
                1,
                -1,
                12345,
                -12345,
                (1 << (bits - 1)) - 1,
                -(1 << (bits - 1)),
            ] {
                let left = ((value << (32 - bits)) as i32) & keep_mask(bits);
                write_int(&mut bytes, slot, left);
                assert_eq!(
                    read_int(&bytes, slot, bits),
                    left,
                    "{bits} bits in {slot} bytes, value {value}"
                );
            }
        }
    }

    #[test]
    fn the_wire_layouts_match_the_documented_ones() {
        let mut bytes = [0u8; 4];
        // 24-bit packed: little-endian.
        write_int(&mut bytes, 3, 0x1234_5600u32 as i32);
        assert_eq!(&bytes[..3], &[0x56, 0x34, 0x12]);
        // 24-bit in 4 bytes: the sample shifted left by 8, low byte zero.
        write_int(&mut bytes, 4, 0x1234_5600u32 as i32);
        assert_eq!(bytes, [0x00, 0x56, 0x34, 0x12]);
        // A negative 16-bit sample.
        write_int(&mut bytes, 2, (-2i32) << 16);
        assert_eq!(&bytes[..2], &[0xfe, 0xff]);
    }

    #[test]
    fn padding_bits_below_the_significant_ones_are_ignored_on_read() {
        // 24 bits in a 4-byte slot, with garbage in the low byte.
        assert_eq!(read_int(&[0xff, 0x56, 0x34, 0x12], 4, 24), 0x1234_5600);
    }

    #[test]
    fn truncation_rounds_toward_negative_infinity() {
        let mut rng = Rng::new(1);
        let mut clipped = 0;
        assert_eq!(
            narrow(
                0x0001_ffffu32 as i32,
                16,
                Narrowing::Truncate,
                &mut rng,
                &mut clipped
            ),
            0x0001_0000
        );
        assert_eq!(
            narrow(
                0x0000_ffffu32 as i32,
                16,
                Narrowing::Truncate,
                &mut rng,
                &mut clipped
            ),
            0x0000_0000
        );
        assert_eq!(
            narrow(-1, 16, Narrowing::Truncate, &mut rng, &mut clipped),
            -0x1_0000
        );
        assert_eq!(
            narrow(0x1234_5678, 16, Narrowing::Truncate, &mut rng, &mut clipped),
            0x1234_0000
        );
        assert_eq!(clipped, 0);
    }

    #[test]
    fn dither_noise_is_zero_mean_and_half_a_step_rms_within_one_and_a_half_steps() {
        let mut rng = Rng::new(7);
        let mut clipped = 0;
        let lsb = 65536.0;
        let n: u64 = 400_000;
        let (mut sum, mut square, mut worst) = (0.0, 0.0, 0.0f64);
        for i in 0..n {
            // A signal that is not on the grid, and varies.
            let value = (((i * 7919) % 2_000_000) as i64 - 1_000_000) as i32 * 977;
            let narrowed = narrow(value, 16, Narrowing::TpdfDither, &mut rng, &mut clipped);
            let error = (f64::from(narrowed) - f64::from(value)) / lsb;
            sum += error;
            square += error * error;
            worst = worst.max(error.abs());
        }
        let mean = sum / n as f64;
        let rms = (square / n as f64).sqrt();
        // Rounding noise (1/12) plus TPDF dither (1/6) is 1/4 of a step squared: half a step RMS.
        assert!(mean.abs() < 0.005, "mean {mean}");
        assert!((rms - 0.5).abs() < 0.01, "rms {rms}");
        assert!(worst <= 1.5 + 1e-9, "worst error {worst} steps");
        assert_eq!(clipped, 0);
    }

    #[test]
    fn truncation_error_has_a_mean_of_minus_half_a_step() {
        let mut rng = Rng::new(7);
        let mut clipped = 0;
        let mut sum = 0.0;
        let n: u64 = 100_000;
        for i in 0..n {
            let value = (((i * 104_729) % 2_000_001) as i64 * 1000 - 1_000_000_000) as i32;
            let narrowed = narrow(value, 16, Narrowing::Truncate, &mut rng, &mut clipped);
            sum += (f64::from(narrowed) - f64::from(value)) / 65536.0;
        }
        assert!((sum / n as f64 + 0.5).abs() < 0.01);
    }

    #[test]
    fn dither_is_reproducible_from_the_seed_and_differs_between_seeds() {
        let run = |seed| {
            let mut rng = Rng::new(seed);
            let mut clipped = 0;
            (0..64)
                .map(|i| {
                    narrow(
                        i * 31_337,
                        16,
                        Narrowing::TpdfDither,
                        &mut rng,
                        &mut clipped,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(run(5), run(5));
        assert_ne!(run(5), run(6));
    }

    #[test]
    fn narrowing_saturates_instead_of_wrapping_and_counts_it() {
        let mut rng = Rng::new(1);
        let mut clipped = 0;
        for _ in 0..1000 {
            let narrowed = narrow(i32::MAX, 16, Narrowing::TpdfDither, &mut rng, &mut clipped);
            assert!(narrowed > 0x7ffe_0000, "wrapped to {narrowed:#x}");
            let narrowed = narrow(i32::MIN, 16, Narrowing::TpdfDither, &mut rng, &mut clipped);
            assert!(narrowed <= -0x7fff_0000, "wrapped to {narrowed:#x}");
        }
        assert!(clipped > 0);
    }

    #[test]
    fn floats_quantise_exactly_on_the_grid_and_saturate_beyond() {
        let mut rng = Rng::new(1);
        let mut clipped = 0;
        for k in [-32768i32, -1, 0, 1, 12345, 32767] {
            let x = f64::from(k) / 32768.0;
            let value = float_to_int(x, 16, Narrowing::Truncate, &mut rng, &mut clipped);
            assert_eq!(value >> 16, k);
        }
        assert_eq!(clipped, 0);
        assert_eq!(
            float_to_int(1.0, 16, Narrowing::Truncate, &mut rng, &mut clipped),
            32767 << 16
        );
        assert_eq!(
            float_to_int(-3.0, 16, Narrowing::Truncate, &mut rng, &mut clipped),
            -32768 << 16
        );
        assert_eq!(clipped, 2);
        assert_eq!(
            float_to_int(f64::NAN, 16, Narrowing::Truncate, &mut rng, &mut clipped),
            0
        );
    }

    #[test]
    fn a_non_finite_float_input_is_silence() {
        assert_eq!(read_float(&f32::NAN.to_le_bytes()), 0.0);
        assert_eq!(read_float(&f32::INFINITY.to_le_bytes()), 0.0);
        assert_eq!(read_float(&0.25f32.to_le_bytes()), 0.25);
    }

    #[test]
    fn integers_become_floats_exactly() {
        assert_eq!(int_to_f64(i32::MIN), -1.0);
        assert_eq!(int_to_f64(0x4000_0000), 0.5);
        assert_eq!(int_to_f64(1 << 16), 1.0 / 32768.0);
    }
}
