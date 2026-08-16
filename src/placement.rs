//! Which monitor, and where on it.
//!
//! A placement is written down once and resolved against the monitors that are
//! actually attached at the moment it is applied. That order matters: a show
//! file written on Tuesday names "the projector", and on Wednesday the
//! projector is on a different port with a different index. Resolving late, with
//! a stated fallback, is the difference between a black screen and a warning.
//!
//! Everything here is in **physical pixels**. Logical pixels across a desktop
//! with a 4K monitor next to a 1080p one are a bug generator: the same logical
//! coordinate is two different places depending on which monitor you ask.

use crate::error::{Error, Result};
use crate::geometry::{Fit, Point, Quad, Rect};

/// A monitor, as the platform describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// What the platform calls it. Not unique, and sometimes empty.
    pub name: String,
    /// Where it sits in the virtual desktop, in physical pixels.
    pub bounds: Rect,
    /// Physical pixels per logical pixel. 2.0 on a Retina display.
    pub scale_factor: f64,
    /// Refresh rate in millihertz — 59_940 for 59.94 Hz — or `None` where the
    /// platform will not say. Millihertz because 59.94 and 23.976 are real
    /// rates and rounding them to integers loses the drift that matters over an
    /// hour.
    pub refresh_millihertz: Option<u32>,
    /// Whether the platform considers this the primary display.
    pub is_primary: bool,
}

impl Monitor {
    /// A monitor, for tests and for platforms that report nothing useful.
    pub fn new(name: impl Into<String>, bounds: Rect) -> Self {
        Monitor {
            name: name.into(),
            bounds,
            scale_factor: 1.0,
            refresh_millihertz: None,
            is_primary: false,
        }
    }

    /// Refresh rate in hertz, if known.
    pub fn refresh_hz(&self) -> Option<f64> {
        self.refresh_millihertz
            .map(|millihertz| f64::from(millihertz) / 1000.0)
    }

    /// How long one frame lasts at this refresh rate.
    pub fn frame_interval(&self) -> Option<std::time::Duration> {
        self.refresh_hz()
            .filter(|hz| *hz > 0.0)
            .map(|hz| std::time::Duration::from_secs_f64(1.0 / hz))
    }
}

/// How to find the monitor a placement means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorSelector {
    /// Whichever the platform calls primary.
    Primary,
    /// By position in the list. Stable within a session and not across one.
    Index(usize),
    /// By name, case-insensitively, matching on a substring so `"DELL"` finds
    /// `"DELL U2720Q"`.
    Name(String),
    /// Whichever monitor contains a point in desktop coordinates. Survives
    /// re-ordering, which an index does not.
    ContainingPoint(i32, i32),
}

impl std::fmt::Display for MonitorSelector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MonitorSelector::Primary => formatter.write_str("the primary monitor"),
            MonitorSelector::Index(index) => write!(formatter, "monitor {index}"),
            MonitorSelector::Name(name) => write!(formatter, "the monitor named {name:?}"),
            MonitorSelector::ContainingPoint(x, y) => {
                write!(formatter, "the monitor containing ({x}, {y})")
            }
        }
    }
}

impl MonitorSelector {
    /// The monitor this names, if it is attached.
    pub fn find<'a>(&self, monitors: &'a [Monitor]) -> Option<&'a Monitor> {
        match self {
            MonitorSelector::Primary => monitors
                .iter()
                .find(|monitor| monitor.is_primary)
                .or_else(|| monitors.first()),

            MonitorSelector::Index(index) => monitors.get(*index),

            MonitorSelector::Name(wanted) => {
                let wanted = wanted.to_lowercase();
                monitors
                    .iter()
                    .find(|monitor| monitor.name.to_lowercase().contains(&wanted))
            }

            MonitorSelector::ContainingPoint(x, y) => {
                let point = Point::new(f64::from(*x), f64::from(*y));
                monitors
                    .iter()
                    .find(|monitor| monitor.bounds.contains(point))
            }
        }
    }
}

/// What to fill on the chosen monitor.
#[derive(Clone, Debug, PartialEq)]
pub enum Area {
    /// The whole monitor.
    FullScreen,
    /// A rectangle, in that monitor's own coordinates — `(0, 0)` is its
    /// top-left corner, not the desktop's.
    Rect(Rect),
    /// Four corners, in that monitor's coordinates. The trapezoid case; see
    /// [`Quad`](crate::geometry::Quad).
    Corners(Quad),
    /// Four corners as fractions of the monitor, `0.0` to `1.0`.
    ///
    /// The same shape without having to know how big the monitor is first —
    /// which matters more than it sounds, because finding that out means
    /// starting an event loop, and there is only one of those per process. A
    /// keystone written this way is also portable: the same configuration file
    /// works on a 1080p projector and a 4K one.
    NormalizedCorners(Quad),
}

/// What to show, where, and how.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    /// Which monitor.
    pub monitor: MonitorSelector,
    /// What part of it.
    pub area: Area,
    /// How the picture sits inside that area. Ignored for
    /// [`Area::Corners`], which pins the picture's own corners and so has no
    /// spare room to fit anything into.
    pub fit: Fit,
    /// 0.0 to 1.0. The "translucent" the name promises.
    pub opacity: f32,
    /// Whether to keep this above other windows. Only meaningful when vtome
    /// owns the window.
    pub always_on_top: bool,
    /// Whether to fall back to the primary monitor when the selector finds
    /// nothing, rather than failing.
    ///
    /// On by default: a show that loses a projector should carry on somewhere
    /// visible, and say so, rather than stop.
    pub fall_back_to_primary: bool,
}

impl Placement {
    /// A full-screen placement on the monitor a selector names.
    pub fn new(monitor: MonitorSelector) -> Self {
        Placement {
            monitor,
            area: Area::FullScreen,
            fit: Fit::Contain,
            opacity: 1.0,
            always_on_top: false,
            fall_back_to_primary: true,
        }
    }

    /// A rectangle of that monitor, in its own coordinates.
    pub fn area(mut self, rect: Rect) -> Self {
        self.area = Area::Rect(rect);
        self
    }

    /// Four corners of that monitor, in its own coordinates.
    pub fn corners(mut self, quad: Quad) -> Self {
        self.area = Area::Corners(quad);
        self
    }

    /// Four corners as fractions of the monitor, `0.0` to `1.0`.
    ///
    /// Prefer this when the monitor's size is not known yet — see
    /// [`Area::NormalizedCorners`].
    pub fn corners_normalized(mut self, quad: Quad) -> Self {
        self.area = Area::NormalizedCorners(quad);
        self
    }

    /// A symmetric keystone over the whole monitor, narrowing the top edge by
    /// `inset` of the width at each end.
    ///
    /// The projector-aimed-upwards case, in one call and without needing to
    /// know the monitor's size. `0.1` is a mild correction; `0.4` is nearly a
    /// triangle.
    pub fn keystone(self, inset: f64) -> Self {
        let inset = inset.clamp(0.0, 0.49);

        self.corners_normalized(Quad::new([
            Point::new(inset, 0.0),
            Point::new(1.0 - inset, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
        ]))
    }

    /// How the picture sits inside the area.
    pub fn fit(mut self, fit: Fit) -> Self {
        self.fit = fit;
        self
    }

    /// How opaque, from 0.0 to 1.0.
    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }

    /// Whether to stay above other windows.
    pub fn always_on_top(mut self, on_top: bool) -> Self {
        self.always_on_top = on_top;
        self
    }

    /// Whether a missing monitor is an error rather than a fallback.
    pub fn require_monitor(mut self, required: bool) -> Self {
        self.fall_back_to_primary = !required;
        self
    }

    /// Works out where a `source_width × source_height` picture actually goes.
    ///
    /// Everything in the result is in desktop coordinates, so a caller can hand
    /// it straight to a window system without knowing which monitor it came
    /// from.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchMonitor`] if nothing matches and fallback is off,
    /// [`Error::Placement`] for an opacity outside `0..=1`, an empty rectangle,
    /// or a quad that is not convex.
    pub fn resolve(
        &self,
        source_width: u32,
        source_height: u32,
        monitors: &[Monitor],
    ) -> Result<ResolvedPlacement> {
        if !(0.0..=1.0).contains(&self.opacity) {
            return Err(Error::placement(format!(
                "opacity is {} — it has to be between 0 and 1",
                self.opacity
            )));
        }

        if monitors.is_empty() {
            return Err(Error::NoSuchMonitor {
                selector: self.monitor.to_string(),
                available: 0,
            });
        }

        let (monitor, fell_back) = match self.monitor.find(monitors) {
            Some(monitor) => (monitor, false),
            None if self.fall_back_to_primary => {
                let primary = MonitorSelector::Primary
                    .find(monitors)
                    .expect("the list is not empty");

                (primary, true)
            }
            None => {
                return Err(Error::NoSuchMonitor {
                    selector: self.monitor.to_string(),
                    available: monitors.len(),
                })
            }
        };

        let origin = Point::new(monitor.bounds.x, monitor.bounds.y);

        let quad = match &self.area {
            Area::FullScreen => {
                let area = Rect::from_size(monitor.bounds.width, monitor.bounds.height);
                let placed =
                    self.fit
                        .apply(f64::from(source_width), f64::from(source_height), area);

                Quad::from_rect(offset(placed, origin))
            }

            Area::Rect(rect) => {
                if rect.is_empty() {
                    return Err(Error::placement(format!(
                        "the area is {}×{} — there is nothing to draw into",
                        rect.width, rect.height
                    )));
                }

                let placed =
                    self.fit
                        .apply(f64::from(source_width), f64::from(source_height), *rect);

                Quad::from_rect(offset(placed, origin))
            }

            // Fractions of the monitor become pixels of it, and then take the
            // same path as any other quad.
            Area::NormalizedCorners(quad) => {
                let scaled = Quad::new(quad.corners.map(|corner| {
                    Point::new(
                        origin.x + corner.x * monitor.bounds.width,
                        origin.y + corner.y * monitor.bounds.height,
                    )
                }));

                scaled.homography()?;

                scaled
            }

            // Corner pinning maps the picture's own corners onto the given
            // ones, so there is no leftover space and nothing to fit.
            Area::Corners(quad) => {
                let moved = Quad::new(
                    quad.corners
                        .map(|corner| Point::new(corner.x + origin.x, corner.y + origin.y)),
                );

                // Checked here rather than at draw time: a bad quad should be
                // an error while it is still a configuration mistake.
                moved.homography()?;

                moved
            }
        };

        Ok(ResolvedPlacement {
            monitor: monitor.clone(),
            quad,
            opacity: self.opacity,
            always_on_top: self.always_on_top,
            fell_back,
        })
    }
}

/// Moves a rectangle from monitor coordinates into desktop coordinates.
fn offset(rect: Rect, origin: Point) -> Rect {
    Rect::new(
        rect.x + origin.x,
        rect.y + origin.y,
        rect.width,
        rect.height,
    )
}

/// A [`Placement`] against the monitors that were actually there.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPlacement {
    /// The monitor it landed on.
    pub monitor: Monitor,
    /// Where the picture goes, in desktop coordinates. A rectangle is a quad
    /// with square corners, so there is only one shape here to draw.
    pub quad: Quad,
    /// How opaque.
    pub opacity: f32,
    /// Whether to stay on top.
    pub always_on_top: bool,
    /// Whether the requested monitor was missing and the primary was used.
    ///
    /// Worth logging: it is the difference between "as configured" and "as
    /// improvised", and silence there is how a show ends up on the wrong wall.
    pub fell_back: bool,
}

impl ResolvedPlacement {
    /// The window rectangle that contains the quad, in desktop coordinates.
    ///
    /// A corner-pinned quad still needs a rectangular window to live in; this
    /// is the smallest one that holds it.
    pub fn window_rect(&self) -> Rect {
        self.quad.bounds()
    }

    /// The quad relative to its own window, which is what the renderer wants —
    /// a shader works in surface coordinates, not desktop ones.
    pub fn quad_in_window(&self) -> Quad {
        let bounds = self.window_rect();

        Quad::new(
            self.quad
                .corners
                .map(|corner| Point::new(corner.x - bounds.x, corner.y - bounds.y)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desktop() -> Vec<Monitor> {
        vec![
            Monitor {
                is_primary: true,
                ..Monitor::new(
                    "Built-in Retina Display",
                    Rect::new(0.0, 0.0, 3024.0, 1964.0),
                )
            },
            Monitor {
                scale_factor: 1.0,
                refresh_millihertz: Some(59_940),
                ..Monitor::new("DELL U2720Q", Rect::new(3024.0, 0.0, 3840.0, 2160.0))
            },
            Monitor::new("EPSON Projector", Rect::new(-1920.0, 0.0, 1920.0, 1080.0)),
        ]
    }

    #[test]
    fn selectors_find_what_they_name() {
        let monitors = desktop();

        assert_eq!(
            MonitorSelector::Primary.find(&monitors).unwrap().name,
            "Built-in Retina Display"
        );
        assert_eq!(
            MonitorSelector::Index(1).find(&monitors).unwrap().name,
            "DELL U2720Q"
        );
        assert_eq!(
            MonitorSelector::Name("dell".to_string())
                .find(&monitors)
                .unwrap()
                .name,
            "DELL U2720Q",
            "name matching should not care about case"
        );
        assert_eq!(
            MonitorSelector::ContainingPoint(-1000, 500)
                .find(&monitors)
                .unwrap()
                .name,
            "EPSON Projector",
            "a monitor to the left of the primary has negative coordinates"
        );
    }

    /// The Wednesday case: the projector is unplugged and the show has to end
    /// up somewhere a person can see it.
    #[test]
    fn an_unplugged_monitor_falls_back_and_admits_it() {
        let monitors = vec![desktop()[0].clone()];

        let placement = Placement::new(MonitorSelector::Name("EPSON".to_string()));
        let resolved = placement.resolve(1920, 1080, &monitors).unwrap();

        assert!(resolved.fell_back);
        assert_eq!(resolved.monitor.name, "Built-in Retina Display");
    }

    #[test]
    fn a_required_monitor_is_an_error_when_it_is_missing() {
        let monitors = vec![desktop()[0].clone()];

        let placement = Placement::new(MonitorSelector::Index(7)).require_monitor(true);

        let Err(Error::NoSuchMonitor {
            selector,
            available,
        }) = placement.resolve(1920, 1080, &monitors)
        else {
            panic!("monitor 7 is not attached");
        };

        assert_eq!(available, 1);
        assert!(selector.contains('7'), "{selector}");
    }

    #[test]
    fn no_monitors_at_all_is_an_error_however_forgiving_the_placement() {
        let placement = Placement::new(MonitorSelector::Primary);

        assert!(matches!(
            placement.resolve(1920, 1080, &[]),
            Err(Error::NoSuchMonitor { available: 0, .. })
        ));
    }

    /// A rectangle is written in the monitor's own coordinates and comes back
    /// in the desktop's, which is the translation every window system wants.
    #[test]
    fn an_area_is_relative_to_its_monitor_and_resolves_to_the_desktop() {
        let monitors = desktop();

        let placement = Placement::new(MonitorSelector::Index(1))
            .area(Rect::new(100.0, 100.0, 1920.0, 1080.0))
            .fit(Fit::Stretch);

        let resolved = placement.resolve(1920, 1080, &monitors).unwrap();
        let rect = resolved.window_rect();

        assert_eq!((rect.x, rect.y), (3024.0 + 100.0, 100.0));
        assert_eq!((rect.width, rect.height), (1920.0, 1080.0));
        assert!(!resolved.fell_back);
    }

    #[test]
    fn full_screen_with_contain_letterboxes_within_the_monitor() {
        let monitors = desktop();

        // A 16:9 picture on a 1.54:1 display leaves bars top and bottom.
        let placement = Placement::new(MonitorSelector::Primary);
        let resolved = placement.resolve(1920, 1080, &monitors).unwrap();
        let rect = resolved.window_rect();

        assert_eq!(rect.width, 3024.0);
        assert!((rect.height - 1701.0).abs() < 1.0, "{rect:?}");
        assert!(
            rect.y > 0.0,
            "the picture should be centred, not at the top"
        );
    }

    #[test]
    fn corner_pinning_moves_the_corners_and_keeps_their_shape() {
        let monitors = desktop();

        let quad = Quad::keystone(Rect::from_size(1920.0, 1080.0), 200.0);
        let placement = Placement::new(MonitorSelector::Index(1)).corners(quad);

        let resolved = placement.resolve(1920, 1080, &monitors).unwrap();

        // Moved onto the second monitor...
        assert_eq!(resolved.quad.corners[0].x, 3024.0 + 200.0);
        // ...and back to the window's own space for the shader.
        assert_eq!(resolved.quad_in_window().corners[3].x, 0.0);
        assert!(resolved.quad.homography().is_ok());
    }

    /// Written once as fractions, correct on every monitor — and needing no
    /// event loop to find out how big the monitor is.
    #[test]
    fn normalized_corners_scale_to_whichever_monitor_they_land_on() {
        let monitors = desktop();

        let placement = Placement::new(MonitorSelector::Index(1)).keystone(0.1);
        let resolved = placement.resolve(1920, 1080, &monitors).unwrap();

        // The 4K monitor starts at x = 3024 and is 3840 wide, so a tenth in
        // from its left edge is 3024 + 384.
        assert_eq!(resolved.quad.corners[0], Point::new(3024.0 + 384.0, 0.0));
        assert_eq!(resolved.quad.corners[3], Point::new(3024.0, 2160.0));
        assert!(
            !resolved.quad.is_affine(),
            "a keystone is not a parallelogram"
        );

        // The same placement on the projector gives the same shape at its size.
        let elsewhere = Placement::new(MonitorSelector::Name("EPSON".to_string()))
            .keystone(0.1)
            .resolve(1920, 1080, &monitors)
            .unwrap();

        assert_eq!(elsewhere.quad.corners[0], Point::new(-1920.0 + 192.0, 0.0));
    }

    #[test]
    fn a_folded_quad_is_refused_while_it_is_still_configuration() {
        let monitors = desktop();

        let bowtie = Quad::new([
            Point::new(0.0, 0.0),
            Point::new(100.0, 0.0),
            Point::new(0.0, 100.0),
            Point::new(100.0, 100.0),
        ]);

        let placement = Placement::new(MonitorSelector::Primary).corners(bowtie);

        assert!(matches!(
            placement.resolve(1920, 1080, &monitors),
            Err(Error::Placement { .. })
        ));
    }

    #[test]
    fn an_impossible_opacity_or_an_empty_area_is_refused() {
        let monitors = desktop();

        let too_opaque = Placement::new(MonitorSelector::Primary).opacity(1.5);
        assert!(matches!(
            too_opaque.resolve(64, 64, &monitors),
            Err(Error::Placement { .. })
        ));

        let nothing = Placement::new(MonitorSelector::Primary).area(Rect::from_size(0.0, 100.0));
        assert!(matches!(
            nothing.resolve(64, 64, &monitors),
            Err(Error::Placement { .. })
        ));
    }

    #[test]
    fn refresh_rates_keep_their_fractions() {
        let monitors = desktop();
        let dell = &monitors[1];

        assert_eq!(dell.refresh_hz(), Some(59.94));

        let interval = dell.frame_interval().unwrap();
        assert!((interval.as_secs_f64() - 1.0 / 59.94).abs() < 1e-9);
        assert!(monitors[0].frame_interval().is_none());
    }
}
