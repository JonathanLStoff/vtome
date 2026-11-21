//! Per-clip colour and transform, applied in the one pass that draws the clip.
//!
//! A [`Look`] is plain numbers. vtome folds it into the uniform block of the
//! clip's fragment shader: the colour controls become one 4×4 matrix with an
//! offset, the gamma one exponent, the transform one 2×3 affine on the texture
//! coordinates, and the chroma key a branch on the alpha. Nothing here costs a
//! pass of its own, so a clip with a full look draws as one picture does.
//!
//! The order is fixed, and it is the order the controls are listed in:
//! exposure, white balance, brightness, contrast, saturation, hue, then gamma,
//! then the clamp to 0..1. Colour before the transform, and the key tests the
//! picture as it came in, before any colour control.

use std::time::Duration;

/// Colour and transform for one clip. [`Look::default`] changes nothing.
///
/// Colour is applied to gamma-encoded values, as an editor's sliders expect, so
/// a brightness of 0.1 lifts every channel by 0.1 of full scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// Added to every channel, −1 to 1. 0 is none.
    pub brightness: f32,
    /// Around mid grey, 0 to 4. 1 is none.
    pub contrast: f32,
    /// 0 is grey, 1 is none, 2 is twice as saturated.
    pub saturation: f32,
    /// Rotation of the colour wheel, in degrees. 0 is none.
    pub hue: f32,
    /// In stops: each stop doubles the light. 0 is none.
    pub exposure: f32,
    /// Warm (1) to cool (−1). An approximation, not a Kelvin scale. 0 is none.
    pub temperature: f32,
    /// Green (−1) to magenta (1). 0 is none.
    pub tint: f32,
    /// Gamma: the picture is raised to 1/`gamma`. 1 is none.
    pub gamma: f32,
    /// The part of the picture shown, as `[left, top, right, bottom]` fractions
    /// of its size. `[0, 0, 1, 1]` is all of it.
    pub crop: [f32; 4],
    /// How far in the picture is shown: 1 is all of it, 2 is the middle half.
    pub zoom: f32,
    /// Where the shown part sits, as fractions of the picture, from its centre.
    /// Only has an effect when `zoom` is above 1. `[0, 0]` is the centre.
    pub pan: [f32; 2],
    /// Clockwise quarter turns, 0 to 3. Odd turns swap the picture's width and
    /// height, so [`Look::visible_size`] reports the turned shape.
    pub quarter_turns: u8,
    /// Mirrors left to right, before the turn.
    pub flip_h: bool,
    /// Mirrors top to bottom, before the turn.
    pub flip_v: bool,
    /// Pixels close to this colour go transparent. `None` keeps every pixel.
    pub key: Option<ChromaKey>,
    /// Darkens the corners: 0 is none, 1 is black at the very edge.
    pub vignette: f32,
    /// Shown behind the picture, inside its box, where the picture is not:
    /// red, green, blue and alpha, straight. Alpha 0 is none. This is what a
    /// fade to a colour fades into.
    pub background: [f32; 4],
    /// A transition in progress. See [`Transition`].
    pub transition: Transition,
}

/// A transition the picture plays as it comes in or goes out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transition {
    /// What moves.
    pub kind: TransitionKind,
    /// How far along it is: 0 is the start, 1 is the picture as it is. Animate it
    /// with [`Controls::set_look`](crate::Controls::set_look).
    pub progress: f32,
}

/// The shape of a [`Transition`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionKind {
    /// None: the picture is shown as it is.
    None,
    /// A reveal from the left edge, the picture hard against it.
    Wipe,
    /// The picture slides in from the right edge.
    Slide,
    /// The picture grows from the centre of its box.
    Zoom,
}

impl Default for Transition {
    fn default() -> Self {
        Transition { kind: TransitionKind::None, progress: 1.0 }
    }
}

/// A colour to make transparent, with how close is close enough.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChromaKey {
    /// The colour to remove, 0 to 1 per channel, as it is in the picture.
    pub color: [f32; 3],
    /// How far from the key colour, in chroma distance, a pixel is still fully
    /// removed. The chroma distance is BT.709 Cb and Cr, so about 0 to 1.
    pub tolerance: f32,
    /// How far past `tolerance` the transparency fades out. 0 is a hard edge.
    pub softness: f32,
}

impl Default for Look {
    fn default() -> Self {
        Look {
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            hue: 0.0,
            exposure: 0.0,
            temperature: 0.0,
            tint: 0.0,
            gamma: 1.0,
            crop: [0.0, 0.0, 1.0, 1.0],
            zoom: 1.0,
            pan: [0.0, 0.0],
            quarter_turns: 0,
            flip_h: false,
            flip_v: false,
            key: None,
            vignette: 0.0,
            background: [0.0; 4],
            transition: Transition::default(),
        }
    }
}

/// The numbers the fragment shader reads for one layer. Laid out by hand in
/// `render.rs` to match `present.wgsl`.
pub(crate) struct Shaping {
    /// Rows of the 2×3 affine from the quad's unit square to texture coordinates,
    /// the translation in the third column.
    pub transform: [[f32; 4]; 2],
    /// Rows of the 4×4 colour matrix, with the offset in the fourth column.
    pub grade: [[f32; 4]; 4],
    /// Chroma of the key colour (Cb, Cr), then tolerance and softness.
    pub key: [f32; 4],
    /// The exponent gamma raises the picture to: 1 / `gamma`.
    pub gamma: f32,
    pub keyed: bool,
    /// Transition kind (0 none, 1 wipe, 2 slide, 3 zoom), progress, vignette strength, unused.
    pub effects: [f32; 4],
    /// The background, straight RGBA.
    pub background: [f32; 4],
}

type Matrix3 = [[f64; 3]; 3];
type Matrix4 = [[f64; 4]; 4];

fn mul3(a: Matrix3, b: Matrix3) -> Matrix3 {
    let mut out = [[0.0; 3]; 3];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, cell) in out_row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    out
}

fn mul4(a: Matrix4, b: Matrix4) -> Matrix4 {
    let mut out = [[0.0; 4]; 4];
    for (row, out_row) in out.iter_mut().enumerate() {
        for (column, cell) in out_row.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[row][k] * b[k][column]).sum();
        }
    }
    out
}

fn identity4() -> Matrix4 {
    let mut out = [[0.0; 4]; 4];
    for (i, row) in out.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    out
}

/// A number from the editor, or the value that changes nothing if it is not one.
fn finite(value: f32, otherwise: f32) -> f64 {
    if value.is_finite() { f64::from(value) } else { f64::from(otherwise) }
}

/// Keeps `value` within `low`..`high`, and `otherwise` for a non-number.
fn within(value: f32, low: f32, high: f32, otherwise: f32) -> f64 {
    finite(value, otherwise).clamp(f64::from(low), f64::from(high))
}

impl Look {
    /// The shape of the picture as it is seen: its size after the crop and zoom,
    /// with width and height swapped for an odd number of quarter turns.
    ///
    /// The compositor fits this, not the raw picture, so a cropped or turned
    /// picture keeps its proportions in its box.
    pub(crate) fn visible_size(&self, width: f64, height: f64) -> (f64, f64) {
        let [left, top, right, bottom] = self.crop_window();
        let zoom = within(self.zoom, 1.0, 16.0, 1.0);
        let shown_w = width * (right - left) / zoom;
        let shown_h = height * (bottom - top) / zoom;
        if self.quarter_turns % 2 == 1 {
            (shown_h, shown_w)
        } else {
            (shown_w, shown_h)
        }
    }

    /// The crop, checked: fractions in order, inside the picture, never empty.
    fn crop_window(&self) -> [f64; 4] {
        let [left, top, right, bottom] = self.crop;
        let left = within(left, 0.0, 1.0, 0.0);
        let top = within(top, 0.0, 1.0, 0.0);
        let right = within(right, 0.0, 1.0, 1.0);
        let bottom = within(bottom, 0.0, 1.0, 1.0);
        if right - left < 1e-3 || bottom - top < 1e-3 {
            return [0.0, 0.0, 1.0, 1.0];
        }
        [left, top, right, bottom]
    }

    /// The 2×3 affine, as rows of `[a, b, tx]` and `[c, d, ty]` in f64.
    ///
    /// Read right to left: zoom and pan the quad's unit square, flip, turn back
    /// to the picture's own orientation, then crop into the texture.
    fn transform_matrix(&self) -> Matrix3 {
        let zoom = within(self.zoom, 1.0, 16.0, 1.0);
        // The window of a zoomed picture stays inside it.
        let room = 0.5 - 0.5 / zoom;
        let pan_x = within(self.pan[0], -room as f32, room as f32, 0.0);
        let pan_y = within(self.pan[1], -room as f32, room as f32, 0.0);

        let zoom_pan = [
            [1.0 / zoom, 0.0, 0.5 + pan_x - 0.5 / zoom],
            [0.0, 1.0 / zoom, 0.5 + pan_y - 0.5 / zoom],
            [0.0, 0.0, 1.0],
        ];

        let flip_h = if self.flip_h { [[-1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] } else { identity3() };
        let flip_v = if self.flip_v { [[1.0, 0.0, 0.0], [0.0, -1.0, 1.0], [0.0, 0.0, 1.0]] } else { identity3() };

        // One step anticlockwise maps (x, y) to (y, 1 - x) in the y-down unit square.
        let anticlockwise = [[0.0, 1.0, 0.0], [-1.0, 0.0, 1.0], [0.0, 0.0, 1.0]];
        let mut turn = identity3();
        for _ in 0..(self.quarter_turns % 4) {
            turn = mul3(anticlockwise, turn);
        }

        let [left, top, right, bottom] = self.crop_window();
        let crop = [[right - left, 0.0, left], [0.0, bottom - top, top], [0.0, 0.0, 1.0]];

        mul3(crop, mul3(turn, mul3(mul3(flip_h, flip_v), zoom_pan)))
    }

    /// The colour controls folded into one 4×4, in the fixed order.
    fn grade_matrix(&self) -> Matrix4 {
        let exposure = 2f64.powf(within(self.exposure, -8.0, 8.0, 0.0));
        let temperature = within(self.temperature, -1.0, 1.0, 0.0);
        let tint = within(self.tint, -1.0, 1.0, 0.0);
        let brightness = within(self.brightness, -1.0, 1.0, 0.0);
        let contrast = within(self.contrast, 0.0, 4.0, 1.0);
        let saturation = within(self.saturation, 0.0, 4.0, 1.0);
        let hue = finite(self.hue, 0.0).to_radians();

        let mut grade = identity4();

        // Exposure, then white balance: per-channel gains, the warm/cool and the
        // green/magenta push, each small enough to stay in gamut for most values.
        let red = exposure * (1.0 + 0.25 * temperature) * (1.0 + 0.125 * tint);
        let green = exposure * (1.0 - 0.25 * tint);
        let blue = exposure * (1.0 - 0.25 * temperature) * (1.0 + 0.125 * tint);
        grade = mul4(diagonal(red, green, blue), grade);

        // Brightness: an offset on every channel.
        let mut offset = identity4();
        offset[0][3] = brightness;
        offset[1][3] = brightness;
        offset[2][3] = brightness;
        grade = mul4(offset, grade);

        // Contrast around mid grey: x → c·(x − ½) + ½.
        let mut around_grey = diagonal(contrast, contrast, contrast);
        for channel in 0..3 {
            around_grey[channel][3] = 0.5 - 0.5 * contrast;
        }
        grade = mul4(around_grey, grade);

        // Saturation: mix each channel with Rec. 709 luma.
        const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];
        let mut saturate = [[0.0; 4]; 4];
        for row in 0..3 {
            for column in 0..3 {
                let own = if row == column { 1.0 } else { 0.0 };
                saturate[row][column] = saturation * own + (1.0 - saturation) * LUMA[column];
            }
        }
        saturate[3][3] = 1.0;
        grade = mul4(saturate, grade);

        // Hue: a rotation about the grey axis (1, 1, 1), which keeps luma.
        let (sin, cos) = hue.sin_cos();
        let root3 = 3f64.sqrt();
        let third = (1.0 - cos) / 3.0;
        let s = sin / root3;
        let mut rotate = identity4();
        rotate[0][0] = cos + third;
        rotate[0][1] = third - s;
        rotate[0][2] = third + s;
        rotate[1][0] = third + s;
        rotate[1][1] = cos + third;
        rotate[1][2] = third - s;
        rotate[2][0] = third - s;
        rotate[2][1] = third + s;
        rotate[2][2] = cos + third;
        grade = mul4(rotate, grade);

        grade
    }

    /// What the shader reads for this look.
    pub(crate) fn shaping(&self) -> Shaping {
        let transform = self.transform_matrix();
        let grade = self.grade_matrix();
        let gamma = within(self.gamma, 0.01, 10.0, 1.0);

        let mut key = [0.0f32; 4];
        let keyed = match self.key {
            Some(key_colour) => {
                let [red, green, blue] = key_colour.color.map(|channel| within(channel, 0.0, 1.0, 0.0));
                let luma = 0.2126 * red + 0.7152 * green + 0.0722 * blue;
                let cb = (blue - luma) / 1.8556;
                let cr = (red - luma) / 1.5748;
                let tolerance = within(key_colour.tolerance, 0.0, 2.0, 0.0);
                let softness = within(key_colour.softness, 0.0, 2.0, 0.0);
                key = [cb as f32, cr as f32, tolerance as f32, softness as f32];
                true
            }
            None => false,
        };

        Shaping {
            transform: [
                [transform[0][0] as f32, transform[0][1] as f32, transform[0][2] as f32, 0.0],
                [transform[1][0] as f32, transform[1][1] as f32, transform[1][2] as f32, 0.0],
            ],
            grade: grade.map(|row| row.map(|cell| cell as f32)),
            key,
            gamma: (1.0 / gamma) as f32,
            keyed,
            effects: [
                match self.transition.kind {
                    TransitionKind::None => 0.0,
                    TransitionKind::Wipe => 1.0,
                    TransitionKind::Slide => 2.0,
                    TransitionKind::Zoom => 3.0,
                },
                within(self.transition.progress, 0.0, 1.0, 1.0) as f32,
                within(self.vignette, 0.0, 1.0, 0.0) as f32,
                0.0,
            ],
            background: self.background.map(|channel| if channel.is_finite() { channel.clamp(0.0, 1.0) } else { 0.0 }),
        }
    }

    /// This look moved part of the way to `to`: `t` from 0 (this) to 1 (`to`).
    ///
    /// Numbers move in a straight line. The flips and quarter turns switch to
    /// `to` at once, since a half-turn of a picture has no sensible middle, and a
    /// key is only blended when both looks have one.
    pub(crate) fn toward(&self, to: &Look, t: f32) -> Look {
        let mix = |from: f32, into: f32| from + (into - from) * t;
        let mix4 = |from: [f32; 4], into: [f32; 4]| [mix(from[0], into[0]), mix(from[1], into[1]), mix(from[2], into[2]), mix(from[3], into[3])];
        let key = match (self.key, to.key) {
            (Some(from), Some(into)) => Some(ChromaKey {
                color: [mix(from.color[0], into.color[0]), mix(from.color[1], into.color[1]), mix(from.color[2], into.color[2])],
                tolerance: mix(from.tolerance, into.tolerance),
                softness: mix(from.softness, into.softness),
            }),
            _ => to.key,
        };
        Look {
            brightness: mix(self.brightness, to.brightness),
            contrast: mix(self.contrast, to.contrast),
            saturation: mix(self.saturation, to.saturation),
            hue: mix(self.hue, to.hue),
            exposure: mix(self.exposure, to.exposure),
            temperature: mix(self.temperature, to.temperature),
            tint: mix(self.tint, to.tint),
            gamma: mix(self.gamma, to.gamma),
            crop: mix4(self.crop, to.crop),
            zoom: mix(self.zoom, to.zoom),
            pan: [mix(self.pan[0], to.pan[0]), mix(self.pan[1], to.pan[1])],
            quarter_turns: to.quarter_turns,
            flip_h: to.flip_h,
            flip_v: to.flip_v,
            key,
            vignette: mix(self.vignette, to.vignette),
            background: mix4(self.background, to.background),
            // A transition's kind switches at once; its progress runs over the ramp.
            transition: Transition { kind: to.transition.kind, progress: mix(self.transition.progress, to.transition.progress) },
        }
    }
}

fn identity3() -> Matrix3 {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]
}

fn diagonal(red: f64, green: f64, blue: f64) -> Matrix4 {
    let mut out = [[0.0; 4]; 4];
    out[0][0] = red;
    out[1][1] = green;
    out[2][2] = blue;
    out[3][3] = 1.0;
    out
}

/// How long a [`Controls::set_look`](crate::Controls::set_look) takes to arrive
/// at its look. Zero is from the next frame.
pub(crate) fn ramp_done(since: std::time::Instant, over: Duration, now: std::time::Instant) -> (f32, bool) {
    if over.is_zero() {
        return (1.0, true);
    }
    let elapsed = now.duration_since(since);
    if elapsed >= over {
        (1.0, true)
    } else {
        (elapsed.as_secs_f32() / over.as_secs_f32(), false)
    }
}
