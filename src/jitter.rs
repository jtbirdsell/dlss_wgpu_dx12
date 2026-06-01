//! Subpixel jitter helper shared by the Super Resolution and Ray Reconstruction upscalers.
//!
//! Both contexts suggest the same Halton-sequence camera jitter (DLSS expects a low-discrepancy
//! sub-pixel offset per frame); this is the single shared implementation.

/// The `index`-th value of the [Halton sequence] in the given `base` (a low-discrepancy sequence in
/// `[0, 1)`), via the radical inverse. Used to generate DLSS sub-pixel camera jitter (typically with
/// `base` 2 for X and 3 for Y, recentred to `[-0.5, 0.5)` by the caller).
///
/// [Halton sequence]: https://en.wikipedia.org/wiki/Halton_sequence
pub(crate) fn halton_sequence(mut index: u32, base: u32) -> f32 {
    let mut f = 1.0;
    let mut result = 0.0;
    while index > 0 {
        f /= base as f32;
        result += f * (index % base) as f32;
        index = (index as f32 / base as f32).floor() as u32;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::halton_sequence;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    #[test]
    fn halton_base_2_radical_inverse() {
        // base 2: 1->0.1b=0.5, 2->0.01b=0.25, 3->0.11b=0.75, 4->0.001b=0.125.
        approx(halton_sequence(1, 2), 0.5);
        approx(halton_sequence(2, 2), 0.25);
        approx(halton_sequence(3, 2), 0.75);
        approx(halton_sequence(4, 2), 0.125);
    }

    #[test]
    fn halton_base_3_radical_inverse() {
        // base 3: 1->1/3, 2->2/3, 3->1/9.
        approx(halton_sequence(1, 3), 1.0 / 3.0);
        approx(halton_sequence(2, 3), 2.0 / 3.0);
        approx(halton_sequence(3, 3), 1.0 / 9.0);
    }

    #[test]
    fn halton_index_zero_is_zero() {
        approx(halton_sequence(0, 2), 0.0);
        approx(halton_sequence(0, 3), 0.0);
    }
}
