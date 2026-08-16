//! Rectangles, quadrilaterals, and the projective map between them.
//!
//! This is where the trapezoid lives. Putting a picture into four arbitrary
//! corners is not a matter of moving the vertices: a quadrilateral that is not
//! a parallelogram needs a **projective** map, and interpolating texture
//! coordinates linearly across the two triangles of the quad gives the wrong
//! answer everywhere except the corners.
//!
//! The symptom is a visible crease along the diagonal — the two triangles
//! disagree about what should be in the middle — and it is exactly what shows
//! up when a projector is aimed off-axis at a wall. The fix is the homography
//! computed here, applied per pixel with a divide by `w`.
//!
//! ```
//! use vtome::geometry::{Point, Quad};
//!
//! // A keystoned projection: the top edge is narrower than the bottom.
//! let quad = Quad::new([
//!     Point::new(100.0, 0.0),
//!     Point::new(900.0, 0.0),
//!     Point::new(1000.0, 600.0),
//!     Point::new(0.0, 600.0),
//! ]);
//!
//! let map = quad.homography()?;
//!
//! // The centre of the picture lands where the diagonals cross, which is not
//! // the average of the corners — that difference is the crease.
//! let centre = map.transform(Point::new(0.5, 0.5));
//! assert!((centre.y - 300.0).abs() > 1.0);
//! # Ok::<(), vtome::Error>(())
//! ```

use crate::error::{Error, Result};

/// A point in whatever space the caller is working in — monitor pixels, or the
/// unit square of a texture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// Horizontal.
    pub x: f64,
    /// Vertical, increasing downwards, as every screen coordinate system does.
    pub y: f64,
}

impl Point {
    /// A point.
    pub const fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }

    /// Distance to another point.
    pub fn distance(self, other: Point) -> f64 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}

/// An axis-aligned rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width. Never negative.
    pub width: f64,
    /// Height. Never negative.
    pub height: f64,
}

impl Rect {
    /// A rectangle.
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// A rectangle at the origin.
    pub const fn from_size(width: f64, height: f64) -> Self {
        Rect::new(0.0, 0.0, width, height)
    }

    /// Right edge.
    pub fn right(self) -> f64 {
        self.x + self.width
    }

    /// Bottom edge.
    pub fn bottom(self) -> f64 {
        self.y + self.height
    }

    /// The middle.
    pub fn center(self) -> Point {
        Point::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// Whether a point is inside, edges included.
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x
            && point.x <= self.right()
            && point.y >= self.y
            && point.y <= self.bottom()
    }

    /// The overlap of two rectangles, or `None` if they do not touch.
    pub fn intersection(self, other: Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());

        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    /// Whether this has any area at all.
    pub fn is_empty(self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// The same rectangle in integer pixels, rounded outwards so nothing is
    /// clipped by half a pixel.
    pub fn to_physical(self) -> (i32, i32, u32, u32) {
        let x = self.x.floor();
        let y = self.y.floor();
        let width = (self.right().ceil() - x).max(0.0);
        let height = (self.bottom().ceil() - y).max(0.0);

        (x as i32, y as i32, width as u32, height as u32)
    }
}

/// How a picture is placed inside the area it was given, when the two do not
/// share an aspect ratio.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Fit {
    /// Fill the area exactly, distorting the picture.
    Stretch,
    /// The whole picture, as large as fits. Letterboxes or pillarboxes.
    #[default]
    Contain,
    /// Fill the area, cropping whichever dimension is too long.
    Cover,
    /// One picture pixel per screen pixel, centred, cropped if too large.
    Exact,
}

impl Fit {
    /// Where a `source_width × source_height` picture goes inside `area`.
    ///
    /// The result may extend beyond `area` for [`Fit::Cover`] and [`Fit::Exact`];
    /// that overflow is what the renderer clips against, and computing it
    /// honestly here is what lets the caller know it is happening.
    pub fn apply(self, source_width: f64, source_height: f64, area: Rect) -> Rect {
        if source_width <= 0.0 || source_height <= 0.0 || area.is_empty() {
            return Rect::new(area.x, area.y, 0.0, 0.0);
        }

        let (width, height) = match self {
            Fit::Stretch => (area.width, area.height),
            Fit::Contain | Fit::Cover => {
                let by_width = area.width / source_width;
                let by_height = area.height / source_height;

                // Contain takes the smaller scale so both dimensions fit;
                // cover takes the larger so neither leaves a gap.
                let scale = if self == Fit::Contain {
                    by_width.min(by_height)
                } else {
                    by_width.max(by_height)
                };

                (source_width * scale, source_height * scale)
            }
            Fit::Exact => (source_width, source_height),
        };

        Rect::new(
            area.x + (area.width - width) / 2.0,
            area.y + (area.height - height) / 2.0,
            width,
            height,
        )
    }
}

/// A 3×3 matrix, row-major, acting on homogeneous points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat3(pub [[f64; 3]; 3]);

impl Mat3 {
    /// The identity.
    pub const IDENTITY: Mat3 = Mat3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    /// Applies the matrix to a point and divides through by `w`.
    ///
    /// The divide is the whole difference between a projective map and an
    /// affine one, and it is why this cannot be folded into vertex attributes.
    pub fn transform(self, point: Point) -> Point {
        let Mat3(m) = self;

        let x = m[0][0] * point.x + m[0][1] * point.y + m[0][2];
        let y = m[1][0] * point.x + m[1][1] * point.y + m[1][2];
        let w = m[2][0] * point.x + m[2][1] * point.y + m[2][2];

        if w.abs() < f64::EPSILON {
            // The point is on the horizon line of the map — infinitely far away
            // in the destination. Nothing sensible to return, and no caller can
            // do anything with a NaN, so this stays put rather than exploding.
            return point;
        }

        Point::new(x / w, y / w)
    }

    /// The determinant.
    pub fn determinant(self) -> f64 {
        let Mat3(m) = self;

        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    }

    /// The inverse, or `None` if the matrix is singular.
    ///
    /// The renderer wants this one rather than the forward map: a fragment
    /// shader starts from a screen pixel and asks which texel it came from.
    pub fn inverse(self) -> Option<Mat3> {
        let determinant = self.determinant();

        if determinant.abs() < 1e-12 {
            return None;
        }

        let Mat3(m) = self;
        let inverse_determinant = 1.0 / determinant;

        let mut result = [[0.0_f64; 3]; 3];

        result[0][0] = (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inverse_determinant;
        result[0][1] = (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inverse_determinant;
        result[0][2] = (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inverse_determinant;
        result[1][0] = (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inverse_determinant;
        result[1][1] = (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inverse_determinant;
        result[1][2] = (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inverse_determinant;
        result[2][0] = (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inverse_determinant;
        result[2][1] = (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inverse_determinant;
        result[2][2] = (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inverse_determinant;

        Some(Mat3(result))
    }

    /// Matrix product: `self` after `other`.
    pub fn then(self, other: Mat3) -> Mat3 {
        let Mat3(a) = other;
        let Mat3(b) = self;

        let mut result = [[0.0_f64; 3]; 3];

        for (row, output) in result.iter_mut().enumerate() {
            for (column, cell) in output.iter_mut().enumerate() {
                *cell = (0..3).map(|k| a[row][k] * b[k][column]).sum();
            }
        }

        Mat3(result)
    }

    /// The matrix as three columns of four floats.
    ///
    /// WGSL lays a `mat3x3<f32>` out as three 16-byte columns, so a uniform
    /// wants the padding that shape implies rather than nine tight floats.
    pub fn to_columns_padded(self) -> [[f32; 4]; 3] {
        let Mat3(m) = self;

        std::array::from_fn(|column| {
            [
                m[0][column] as f32,
                m[1][column] as f32,
                m[2][column] as f32,
                0.0,
            ]
        })
    }
}

/// Four corners, in order: top-left, top-right, bottom-right, bottom-left.
///
/// The order is the contract. It matches the unit square's `(0,0)`, `(1,0)`,
/// `(1,1)`, `(0,1)`, which is how a texture is indexed, so "top-left" means the
/// corner the picture's top-left goes to — whatever that does to the geometry
/// on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quad {
    /// The corners, clockwise from the picture's top-left.
    pub corners: [Point; 4],
}

impl Quad {
    /// A quad from four corners, in the documented order.
    pub const fn new(corners: [Point; 4]) -> Self {
        Quad { corners }
    }

    /// The rectangle as a quad — the case with no keystone at all.
    pub fn from_rect(rect: Rect) -> Self {
        Quad::new([
            Point::new(rect.x, rect.y),
            Point::new(rect.right(), rect.y),
            Point::new(rect.right(), rect.bottom()),
            Point::new(rect.x, rect.bottom()),
        ])
    }

    /// A symmetric keystone: the top edge narrowed by `inset` at each end.
    ///
    /// The shape a projector makes when it is aimed upwards, and the one most
    /// people want when they reach for "trapezoid".
    pub fn keystone(rect: Rect, inset: f64) -> Self {
        Quad::new([
            Point::new(rect.x + inset, rect.y),
            Point::new(rect.right() - inset, rect.y),
            Point::new(rect.right(), rect.bottom()),
            Point::new(rect.x, rect.bottom()),
        ])
    }

    /// Twice the signed area, by the shoelace formula. Positive is clockwise in
    /// a y-down coordinate system.
    pub fn signed_area(self) -> f64 {
        let corners = self.corners;

        (0..4)
            .map(|index| {
                let current = corners[index];
                let next = corners[(index + 1) % 4];
                current.x * next.y - next.x * current.y
            })
            .sum::<f64>()
            / 2.0
    }

    /// Whether the quad is convex and non-degenerate.
    ///
    /// A concave or self-intersecting quad has no single projective map onto
    /// it, so this is not a style preference — it is the precondition for
    /// [`homography`](Quad::homography) meaning anything.
    pub fn is_convex(self) -> bool {
        let corners = self.corners;
        let mut sign = 0.0_f64;

        for index in 0..4 {
            let a = corners[index];
            let b = corners[(index + 1) % 4];
            let c = corners[(index + 2) % 4];

            let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);

            if cross.abs() < 1e-9 {
                // Three corners in a line: the quad has collapsed to a triangle
                // and the map would be singular.
                return false;
            }

            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                return false;
            }
        }

        true
    }

    /// The axis-aligned box around the quad.
    pub fn bounds(self) -> Rect {
        let xs = self.corners.map(|corner| corner.x);
        let ys = self.corners.map(|corner| corner.y);

        let left = xs.iter().copied().fold(f64::INFINITY, f64::min);
        let right = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let top = ys.iter().copied().fold(f64::INFINITY, f64::min);
        let bottom = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        Rect::new(left, top, right - left, bottom - top)
    }

    /// Whether a point is inside the quad.
    pub fn contains(self, point: Point) -> bool {
        let corners = self.corners;
        let mut sign = 0.0_f64;

        for index in 0..4 {
            let a = corners[index];
            let b = corners[(index + 1) % 4];

            let cross = (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x);

            if cross.abs() < 1e-12 {
                continue;
            }

            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                return false;
            }
        }

        true
    }

    /// The projective map from the unit square onto this quad.
    ///
    /// `(0,0)` lands on the first corner, `(1,0)` on the second, and so on, so
    /// feeding it texture coordinates maps the picture into the quad. The
    /// bottom row is the perspective part: it is zero exactly when the quad is
    /// a parallelogram, which is the only case where linear interpolation would
    /// have been right.
    ///
    /// This is Heckbert's unit-square-to-quadrilateral construction; it is
    /// closed-form, so there is no solve and no iteration.
    ///
    /// # Errors
    ///
    /// [`Error::Placement`] if the quad is concave, self-intersecting, or has
    /// three corners in a line.
    pub fn homography(self) -> Result<Mat3> {
        if !self.is_convex() {
            return Err(Error::placement(
                "a corner-pinned quad must be convex: this one folds over itself \
                 or has three corners in a line, and there is no projective map onto it",
            ));
        }

        let [p0, p1, p2, p3] = self.corners;

        // How far the fourth corner is from where a parallelogram would put it.
        // Zero in both axes means there is no perspective to account for.
        let sum_x = p0.x - p1.x + p2.x - p3.x;
        let sum_y = p0.y - p1.y + p2.y - p3.y;

        if sum_x.abs() < 1e-12 && sum_y.abs() < 1e-12 {
            return Ok(Mat3([
                [p1.x - p0.x, p3.x - p0.x, p0.x],
                [p1.y - p0.y, p3.y - p0.y, p0.y],
                [0.0, 0.0, 1.0],
            ]));
        }

        let dx1 = p1.x - p2.x;
        let dx2 = p3.x - p2.x;
        let dy1 = p1.y - p2.y;
        let dy2 = p3.y - p2.y;

        let denominator = dx1 * dy2 - dx2 * dy1;

        if denominator.abs() < 1e-12 {
            return Err(Error::placement(
                "the quad is degenerate: two of its edges are parallel through the same point",
            ));
        }

        let g = (sum_x * dy2 - dx2 * sum_y) / denominator;
        let h = (dx1 * sum_y - sum_x * dy1) / denominator;

        Ok(Mat3([
            [p1.x - p0.x + g * p1.x, p3.x - p0.x + h * p3.x, p0.x],
            [p1.y - p0.y + g * p1.y, p3.y - p0.y + h * p3.y, p0.y],
            [g, h, 1.0],
        ]))
    }

    /// The map the other way: from this quad back to the unit square.
    ///
    /// This is what a fragment shader uses. It starts at a screen pixel and has
    /// to find the texel, which is the inverse direction from the one that is
    /// natural to write down.
    ///
    /// # Errors
    ///
    /// As [`homography`](Quad::homography).
    pub fn inverse_homography(self) -> Result<Mat3> {
        self.homography()?.inverse().ok_or_else(|| {
            Error::placement("the quad's projective map has no inverse; it is degenerate")
        })
    }

    /// Whether this quad needs the perspective divide at all.
    ///
    /// A parallelogram does not: its map is affine, linear interpolation is
    /// exact, and a renderer may take a cheaper path.
    pub fn is_affine(self) -> bool {
        let [p0, p1, p2, p3] = self.corners;

        (p0.x - p1.x + p2.x - p3.x).abs() < 1e-9 && (p0.y - p1.y + p2.y - p3.y).abs() < 1e-9
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Point, b: Point) -> bool {
        a.distance(b) < 1e-6
    }

    fn trapezoid() -> Quad {
        Quad::new([
            Point::new(100.0, 0.0),
            Point::new(900.0, 0.0),
            Point::new(1000.0, 600.0),
            Point::new(0.0, 600.0),
        ])
    }

    /// The property everything else rests on.
    #[test]
    fn the_homography_puts_the_corners_exactly_on_the_corners() {
        for quad in [
            trapezoid(),
            Quad::from_rect(Rect::new(10.0, 20.0, 300.0, 200.0)),
            Quad::keystone(Rect::from_size(1920.0, 1080.0), 240.0),
        ] {
            let map = quad.homography().unwrap();
            let unit = [
                Point::new(0.0, 0.0),
                Point::new(1.0, 0.0),
                Point::new(1.0, 1.0),
                Point::new(0.0, 1.0),
            ];

            for (source, expected) in unit.into_iter().zip(quad.corners) {
                let actual = map.transform(source);
                assert!(
                    close(actual, expected),
                    "{source:?} landed at {actual:?}, not {expected:?}"
                );
            }
        }
    }

    #[test]
    fn the_inverse_takes_the_corners_back() {
        let quad = trapezoid();
        let inverse = quad.inverse_homography().unwrap();

        let unit = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
        ];

        for (expected, corner) in unit.into_iter().zip(quad.corners) {
            let actual = inverse.transform(corner);
            assert!(
                close(actual, expected),
                "{corner:?} came back as {actual:?}"
            );
        }
    }

    #[test]
    fn forward_then_inverse_is_the_identity_anywhere_inside() {
        let quad = trapezoid();
        let forward = quad.homography().unwrap();
        let inverse = quad.inverse_homography().unwrap();

        for (u, v) in [(0.1, 0.9), (0.5, 0.5), (0.33, 0.66), (0.99, 0.01)] {
            let source = Point::new(u, v);
            let round_trip = inverse.transform(forward.transform(source));

            assert!(close(round_trip, source), "{source:?} → {round_trip:?}");
        }
    }

    /// The crease, as a number. The centre of the picture belongs where the
    /// diagonals cross; the average of the corners is what linear interpolation
    /// would give, and on a keystone the two are far apart.
    #[test]
    fn the_centre_lands_on_the_diagonals_not_on_the_average_of_the_corners() {
        let quad = trapezoid();
        let map = quad.homography().unwrap();

        let centre = map.transform(Point::new(0.5, 0.5));

        // Where the diagonals actually cross, found independently.
        let [p0, p1, p2, p3] = quad.corners;
        let crossing = intersect(p0, p2, p1, p3);

        assert!(
            close(centre, crossing),
            "the projective centre {centre:?} is not the diagonal crossing {crossing:?}"
        );

        let average = Point::new(
            quad.corners.iter().map(|corner| corner.x).sum::<f64>() / 4.0,
            quad.corners.iter().map(|corner| corner.y).sum::<f64>() / 4.0,
        );

        assert!(
            centre.distance(average) > 10.0,
            "on this keystone the two differ by {:.1} px — that difference is the seam \
             linear interpolation leaves along the diagonal",
            centre.distance(average)
        );
    }

    /// Line intersection, for the test above only: the assertion has to be
    /// derived some other way than by the code under test.
    fn intersect(a1: Point, a2: Point, b1: Point, b2: Point) -> Point {
        let d1 = (a2.x - a1.x, a2.y - a1.y);
        let d2 = (b2.x - b1.x, b2.y - b1.y);

        let denominator = d1.0 * d2.1 - d1.1 * d2.0;
        let t = ((b1.x - a1.x) * d2.1 - (b1.y - a1.y) * d2.0) / denominator;

        Point::new(a1.x + t * d1.0, a1.y + t * d1.1)
    }

    #[test]
    fn a_parallelogram_has_no_perspective_row() {
        let quad = Quad::new([
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(150.0, 100.0),
            Point::new(50.0, 100.0),
        ]);

        assert!(quad.is_affine());

        let Mat3(m) = quad.homography().unwrap();
        assert_eq!((m[2][0], m[2][1], m[2][2]), (0.0, 0.0, 1.0));
    }

    #[test]
    fn a_keystone_does_have_one() {
        let quad = trapezoid();

        assert!(!quad.is_affine());

        let Mat3(m) = quad.homography().unwrap();
        assert!(m[2][0].abs() > 1e-9 || m[2][1].abs() > 1e-9);
    }

    #[test]
    fn a_folded_quad_is_refused_rather_than_drawn() {
        // The corners in the wrong order, which crosses two edges.
        let bowtie = Quad::new([
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(0.0, 100.0),
            Point::new(100.0, 100.0),
        ]);

        assert!(!bowtie.is_convex());

        let Err(Error::Placement { reason }) = bowtie.homography() else {
            panic!("a self-intersecting quad has no projective map");
        };

        assert!(reason.contains("convex"), "{reason}");
    }

    #[test]
    fn a_collapsed_quad_is_refused() {
        let flat = Quad::new([
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(200.0, 0.0),
            Point::new(50.0, 50.0),
        ]);

        assert!(flat.homography().is_err());
    }

    #[test]
    fn convexity_does_not_depend_on_winding() {
        let clockwise = Quad::from_rect(Rect::from_size(10.0, 10.0));
        let anticlockwise = Quad::new([
            clockwise.corners[0],
            clockwise.corners[3],
            clockwise.corners[2],
            clockwise.corners[1],
        ]);

        assert!(clockwise.is_convex() && anticlockwise.is_convex());
        assert!(clockwise.signed_area() * anticlockwise.signed_area() < 0.0);
    }

    #[test]
    fn containment_follows_the_edges_not_the_bounding_box() {
        let quad = trapezoid();

        assert!(quad.contains(Point::new(500.0, 300.0)));
        assert!(quad.contains(Point::new(101.0, 1.0)));
        // Inside the bounding box, outside the trapezoid's top-left corner.
        assert!(!quad.contains(Point::new(5.0, 5.0)));
        assert!(quad.bounds().contains(Point::new(5.0, 5.0)));
    }

    #[test]
    fn contain_letterboxes_and_cover_crops() {
        let area = Rect::from_size(1000.0, 1000.0);

        let contained = Fit::Contain.apply(1920.0, 1080.0, area);
        assert!((contained.width - 1000.0).abs() < 1e-9);
        assert!((contained.height - 562.5).abs() < 1e-9);
        assert!(
            (contained.y - 218.75).abs() < 1e-9,
            "not centred vertically"
        );

        let covered = Fit::Cover.apply(1920.0, 1080.0, area);
        assert!((covered.height - 1000.0).abs() < 1e-9);
        assert!(covered.width > 1000.0, "cover should overflow the width");
        assert!((covered.center().x - area.center().x).abs() < 1e-9);
    }

    #[test]
    fn stretch_fills_and_exact_does_not_scale() {
        let area = Rect::new(100.0, 100.0, 400.0, 400.0);

        assert_eq!(Fit::Stretch.apply(1920.0, 1080.0, area), area);

        let exact = Fit::Exact.apply(200.0, 100.0, area);
        assert_eq!((exact.width, exact.height), (200.0, 100.0));
        assert_eq!(exact.center(), area.center());
    }

    #[test]
    fn a_fit_into_nothing_is_nothing_rather_than_a_nan() {
        let result = Fit::Contain.apply(0.0, 0.0, Rect::from_size(100.0, 100.0));

        assert!(result.is_empty());
        assert!(result.width.is_finite() && result.height.is_finite());
    }

    #[test]
    fn matrices_invert_and_compose() {
        let quad = trapezoid();
        let map = quad.homography().unwrap();
        let identity = map.then(map.inverse().unwrap());

        let point = Point::new(0.3, 0.7);
        assert!(close(identity.transform(point), point));
    }

    #[test]
    fn a_singular_matrix_has_no_inverse() {
        let singular = Mat3([[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [1.0, 1.0, 1.0]]);

        assert!(singular.inverse().is_none());
    }

    /// WGSL pads each column of a `mat3x3<f32>` to sixteen bytes; a uniform
    /// written without that padding is read back sheared.
    #[test]
    fn the_uniform_form_is_column_major_and_padded() {
        let padded = Mat3::IDENTITY.to_columns_padded();

        assert_eq!(padded[0], [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(padded[1], [0.0, 1.0, 0.0, 0.0]);
        assert_eq!(padded[2], [0.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn rectangles_intersect_and_round_outwards() {
        let a = Rect::new(0.0, 0.0, 100.0, 100.0);
        let b = Rect::new(50.0, 50.0, 100.0, 100.0);

        assert_eq!(a.intersection(b), Some(Rect::new(50.0, 50.0, 50.0, 50.0)));
        assert_eq!(a.intersection(Rect::new(500.0, 0.0, 10.0, 10.0)), None);

        let (x, y, width, height) = Rect::new(10.4, 10.6, 100.1, 100.9).to_physical();
        assert_eq!((x, y), (10, 10));
        assert_eq!((width, height), (101, 102));
    }
}
