//! Colour metadata, and the matrix that turns YUV into RGB.
//!
//! This is not garnish. A frame decoded as BT.709 and drawn as BT.601 is
//! visibly wrong — skin goes orange, saturated reds shift — and a limited-range
//! frame drawn as full range is the washed-out look that gets blamed on the
//! camera. The decoder knows these; carrying them to the shader is the whole
//! job of this module.
//!
//! Nothing here converts pixels. It computes a 3×4 matrix and hands it to the
//! GPU, because per-pixel colour conversion on the CPU is the single largest
//! waste available to a video player.

/// The chromaticities the encoder mastered in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Primaries {
    /// Rec. 709 — HD, and what sRGB shares.
    #[default]
    Bt709,
    /// Rec. 601 525-line — NTSC-era SD.
    Bt601_525,
    /// Rec. 601 625-line — PAL-era SD.
    Bt601_625,
    /// Rec. 2020 — UHD and HDR.
    Bt2020,
}

/// The transfer function: how code values relate to light.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Transfer {
    /// Rec. 709's curve, which is what nearly all SDR video carries.
    #[default]
    Bt709,
    /// sRGB's curve. Close to 709 but not identical; still images use it.
    Srgb,
    /// SMPTE ST 2084 (PQ) — HDR10.
    Pq,
    /// Hybrid log-gamma — broadcast HDR.
    Hlg,
}

impl Transfer {
    /// Whether this is a high-dynamic-range curve.
    ///
    /// Both HDR curves are tone-mapped to SDR on the way to the screen for now;
    /// see `planning/TODO.md` §11.
    pub fn is_hdr(self) -> bool {
        matches!(self, Transfer::Pq | Transfer::Hlg)
    }
}

/// Which luma coefficients the encoder used to make Y from RGB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Matrix {
    /// Rec. 709. HD's default.
    #[default]
    Bt709,
    /// Rec. 601. SD's default.
    Bt601,
    /// Rec. 2020 non-constant luminance. UHD's default.
    Bt2020Ncl,
    /// No conversion: the planes are already G, B, R.
    Identity,
}

impl Matrix {
    /// The red and blue luma weights. Green is whatever is left, which is why
    /// it is not stored.
    fn luma_weights(self) -> (f32, f32) {
        match self {
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt2020Ncl => (0.2627, 0.0593),
            // Not used — Identity short-circuits before the weights matter.
            Matrix::Identity => (0.0, 0.0),
        }
    }
}

/// Whether the samples use the whole numeric range or the broadcast subset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Range {
    /// 16..=235 luma, 16..=240 chroma at 8 bits. The default for video, and the
    /// one people forget.
    #[default]
    Limited,
    /// 0..=255 at 8 bits. The default for stills and screen capture.
    Full,
}

/// Everything needed to turn a frame's samples into light, in one place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ColorSpace {
    /// The chromaticities.
    pub primaries: Primaries,
    /// The transfer curve.
    pub transfer: Transfer,
    /// The luma coefficients.
    pub matrix: Matrix,
    /// Full or limited.
    pub range: Range,
}

impl ColorSpace {
    /// What a frame of this size is, when the file does not say.
    ///
    /// Files with no colour metadata are common, and guessing by resolution is
    /// what every player does: SD was made on 601, HD on 709, UHD increasingly
    /// on 2020. It is a convention rather than a rule, so anything the
    /// container *does* state overrides this.
    pub fn guess_for(width: u32, height: u32) -> Self {
        let (primaries, matrix) = if height <= 576 {
            // The 525/625 split is the old NTSC/PAL line count.
            let primaries = if height == 576 || height == 288 {
                Primaries::Bt601_625
            } else {
                Primaries::Bt601_525
            };

            (primaries, Matrix::Bt601)
        } else if width >= 3840 || height >= 2160 {
            (Primaries::Bt2020, Matrix::Bt2020Ncl)
        } else {
            (Primaries::Bt709, Matrix::Bt709)
        };

        ColorSpace {
            primaries,
            transfer: Transfer::Bt709,
            matrix,
            range: Range::Limited,
        }
    }

    /// What a still image is: sRGB, full range, no chroma subsampling to undo.
    pub fn srgb() -> Self {
        ColorSpace {
            primaries: Primaries::Bt709,
            transfer: Transfer::Srgb,
            matrix: Matrix::Identity,
            range: Range::Full,
        }
    }

    /// The YUV→RGB matrix for these settings, as three rows of `[y, u, v,
    /// offset]`.
    ///
    /// Feed it samples normalised by the maximum code value — `code / 255` at
    /// 8 bits, `code / 1023` at 10 — and it returns display RGB in
    /// `0.0..=1.0`. The offset column absorbs both the limited-range pedestal
    /// and chroma's neutral point, so a shader is one `matrix * yuv + offset`
    /// with no branches and no per-pixel arithmetic of its own.
    ///
    /// `bit_depth` is needed even though the samples arrive normalised, because
    /// the neutral chroma code is `2^(n-1)` against a maximum of `2^n - 1`:
    /// 128/255 at 8 bits, 512/1023 at 10. Calling it exactly 0.5 is the
    /// approximation most shaders make, and it tints neutral greys by a
    /// fraction of a code value in the wrong direction. It costs nothing to be
    /// right.
    pub fn yuv_to_rgb(self, bit_depth: u32) -> [[f32; 4]; 3] {
        let depth = bit_depth.clamp(8, 16);
        let max = ((1_u32 << depth) - 1) as f32;
        // Limited range's 16 and 235 are 8-bit numbers; at greater depths the
        // same fractions land on 64/940, 256/3760, and so on.
        let step = (1_u32 << (depth - 8)) as f32;
        let neutral = (1_u32 << (depth - 1)) as f32 / max;

        let (y_scale, y_black, c_scale) = match self.range {
            Range::Full => (1.0, 0.0, 1.0),
            Range::Limited => (
                max / (219.0 * step),
                16.0 * step / max,
                max / (224.0 * step),
            ),
        };

        if self.matrix == Matrix::Identity {
            // The planes are already colour, in G, B, R order. Only the range
            // needs undoing, and it applies to all three alike.
            let offset = -y_scale * y_black;

            return [
                [y_scale, 0.0, 0.0, offset],
                [0.0, y_scale, 0.0, offset],
                [0.0, 0.0, y_scale, offset],
            ];
        }

        let (kr, kb) = self.matrix.luma_weights();
        let kg = 1.0 - kr - kb;

        let r_v = 2.0 * (1.0 - kr) * c_scale;
        let b_u = 2.0 * (1.0 - kb) * c_scale;
        let g_u = -2.0 * (1.0 - kb) * kb / kg * c_scale;
        let g_v = -2.0 * (1.0 - kr) * kr / kg * c_scale;

        let luma_offset = -y_scale * y_black;

        [
            [y_scale, 0.0, r_v, luma_offset - r_v * neutral],
            [y_scale, g_u, g_v, luma_offset - (g_u + g_v) * neutral],
            [y_scale, b_u, 0.0, luma_offset - b_u * neutral],
        ]
    }
}

/// Applies [`ColorSpace::yuv_to_rgb`] on the CPU.
///
/// For tests, for a single pixel, and for the software fallback where there is
/// no GPU at all. A player never calls this per pixel — that is what the shader
/// is for, and doing it here instead is how a 4K player ends up CPU-bound.
pub fn convert_pixel(space: ColorSpace, bit_depth: u32, yuv: [f32; 3]) -> [f32; 3] {
    let matrix = space.yuv_to_rgb(bit_depth);

    std::array::from_fn(|row| {
        let [y, u, v, offset] = matrix[row];
        (y * yuv[0] + u * yuv[1] + v * yuv[2] + offset).clamp(0.0, 1.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Within a thousandth, which on 8-bit input is a quarter of a code value.
    fn close(actual: [f32; 3], expected: [f32; 3]) -> bool {
        actual
            .iter()
            .zip(expected)
            .all(|(a, e)| (a - e).abs() < 1e-3)
    }

    #[test]
    fn limited_range_black_and_white_land_on_zero_and_one() {
        let space = ColorSpace::default();

        let black = convert_pixel(space, 8, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let white = convert_pixel(space, 8, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);

        assert!(close(black, [0.0, 0.0, 0.0]), "{black:?}");
        assert!(close(white, [1.0, 1.0, 1.0]), "{white:?}");
    }

    /// The same picture at 10 bits has black at 64 and white at 940 against a
    /// maximum of 1023 — different numbers, same result, which is what the
    /// `bit_depth` argument is for.
    #[test]
    fn ten_bit_limited_range_lands_in_the_same_place() {
        let space = ColorSpace::default();

        let black = convert_pixel(space, 10, [64.0 / 1023.0, 512.0 / 1023.0, 512.0 / 1023.0]);
        let white = convert_pixel(space, 10, [940.0 / 1023.0, 512.0 / 1023.0, 512.0 / 1023.0]);

        assert!(close(black, [0.0, 0.0, 0.0]), "{black:?}");
        assert!(close(white, [1.0, 1.0, 1.0]), "{white:?}");
    }

    #[test]
    fn full_range_black_and_white_land_on_zero_and_one() {
        let space = ColorSpace {
            range: Range::Full,
            ..ColorSpace::default()
        };

        let black = convert_pixel(space, 8, [0.0, 128.0 / 255.0, 128.0 / 255.0]);
        let white = convert_pixel(space, 8, [1.0, 128.0 / 255.0, 128.0 / 255.0]);

        assert!(close(black, [0.0, 0.0, 0.0]), "{black:?}");
        assert!(close(white, [1.0, 1.0, 1.0]), "{white:?}");
    }

    /// The failure this module exists to prevent: the same samples through the
    /// wrong matrix are a different colour, not a rounding difference.
    #[test]
    fn the_matrix_choice_actually_changes_the_colour() {
        let samples = [0.5, 0.8, 0.3];

        let bt709 = convert_pixel(ColorSpace::default(), 8, samples);
        let bt601 = convert_pixel(
            ColorSpace {
                matrix: Matrix::Bt601,
                ..ColorSpace::default()
            },
            8,
            samples,
        );

        assert!(
            !close(bt709, bt601),
            "709 and 601 gave the same answer: {bt709:?}"
        );
    }

    /// Grey has no chroma, so every matrix must agree on it — the sanity check
    /// that catches a transposed row or a neutral point rounded to 0.5.
    #[test]
    fn every_matrix_agrees_about_grey() {
        let grey = [128.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0];

        let mut results = Vec::new();
        for matrix in [Matrix::Bt709, Matrix::Bt601, Matrix::Bt2020Ncl] {
            let space = ColorSpace {
                matrix,
                ..ColorSpace::default()
            };

            let rgb = convert_pixel(space, 8, grey);
            assert!(
                (rgb[0] - rgb[1]).abs() < 1e-6 && (rgb[1] - rgb[2]).abs() < 1e-6,
                "{matrix:?} tinted a neutral: {rgb:?}"
            );

            results.push(rgb[0]);
        }

        assert!(results
            .windows(2)
            .all(|pair| (pair[0] - pair[1]).abs() < 1e-6));
    }

    #[test]
    fn identity_passes_colour_planes_through() {
        let space = ColorSpace::srgb();
        let rgb = convert_pixel(space, 8, [0.25, 0.5, 0.75]);

        assert!(close(rgb, [0.25, 0.5, 0.75]), "{rgb:?}");
    }

    #[test]
    fn the_guess_follows_the_resolution() {
        assert_eq!(ColorSpace::guess_for(720, 480).matrix, Matrix::Bt601);
        assert_eq!(
            ColorSpace::guess_for(720, 576).primaries,
            Primaries::Bt601_625
        );
        assert_eq!(ColorSpace::guess_for(1920, 1080).matrix, Matrix::Bt709);
        assert_eq!(ColorSpace::guess_for(3840, 2160).matrix, Matrix::Bt2020Ncl);
    }

    #[test]
    fn hdr_curves_are_flagged_as_such() {
        assert!(Transfer::Pq.is_hdr() && Transfer::Hlg.is_hdr());
        assert!(!Transfer::Bt709.is_hdr() && !Transfer::Srgb.is_hdr());
    }
}
