//! Compositor — sources, and the outputs that show them in layers.
//!
//! A *source* is something with a picture: a film (a [`VideoSource`] with its
//! own clock) or a still (one frame, shown until removed or for a set number of
//! refreshes). An [`Output`] is one surface's worth of layers, each pointing at
//! a source by id, over a background colour or image.
//!
//! [`Compositor::tick`] moves every source on by one refresh;
//! [`Compositor::draw_output`] draws one output — background, then every
//! visible layer from the bottom up — in a single GPU pass. A source's picture
//! is uploaded again only when it has changed, so a still costs nothing after
//! its first frame.

use std::collections::HashMap;
use std::path::Path;

use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::output::Output;
use crate::playback::{Playback, Tick};
use crate::video_source::VideoSource;

#[cfg(feature = "render")]
use crate::render::{Gpu, Layer, Picture, Renderer};

/// Sources and the outputs that show them.
///
/// # Workflow
///
/// 1. Add sources: [`insert_video`](Compositor::insert_video),
///    [`insert_still`](Compositor::insert_still)
/// 2. Add outputs whose layers name those sources
/// 3. Each refresh: [`tick`](Compositor::tick), then
///    [`draw_output`](Compositor::draw_output) for every output, then
///    [`presented`](Compositor::presented) once they are on screen
pub struct Compositor {
    sources: HashMap<String, Source>,
    outputs: Vec<Output>,
    /// What each output has on the GPU, index for index with `outputs`.
    #[cfg(feature = "render")]
    gpu: Vec<OutputGpu>,
}

/// Something a layer can show.
struct Source {
    kind: Kind,
    /// The picture it shows now. `None` until a film's first frame is due.
    showing: Option<Frame>,
    /// Goes up whenever `showing` changes, so each output uploads only then.
    generation: u64,
}

enum Kind {
    Video(Box<Playback>),
    /// Shown until removed, or for this many more refreshes.
    Still { frames_left: Option<u64> },
}

/// A source that stopped on its own during a [`tick`](Compositor::tick) —
/// because it ended, ran its frames out, or failed — and was removed along with
/// every layer that showed it.
#[derive(Debug)]
pub struct Finished {
    /// The source's id.
    pub id: String,
    /// Why, if it was a failure rather than an ending. One broken file is
    /// reported here rather than stopping every other source with it.
    pub error: Option<Error>,
}

impl Compositor {
    /// A compositor with no sources and no outputs.
    pub fn new() -> Self {
        Compositor {
            sources: HashMap::new(),
            outputs: Vec::new(),
            #[cfg(feature = "render")]
            gpu: Vec::new(),
        }
    }

    /// Opens a film and adds it, playing once, under its
    /// [persistent id](VideoSource::persistent_id), which is returned.
    ///
    /// # Errors
    ///
    /// Whatever [`VideoSource::from_file`] refuses.
    pub fn add_source_from_file<P: AsRef<Path>>(&mut self, path: P) -> Result<String> {
        let source = VideoSource::from_file(path, None)?;

        Ok(self.add_source(source))
    }

    /// Adds a film, playing once, under its
    /// [persistent id](VideoSource::persistent_id), which is returned. Two
    /// sources from the same file share that id; use
    /// [`insert_video`](Compositor::insert_video) to show one file twice.
    pub fn add_source(&mut self, source: VideoSource) -> String {
        let id = source.persistent_id().to_string();
        self.insert_video(id.clone(), source, false);
        id
    }

    /// Adds a film under an id of the caller's choosing, replacing whatever
    /// had that id. It ends when the file does, unless `looping`.
    pub fn insert_video(&mut self, id: impl Into<String>, source: VideoSource, looping: bool) {
        self.sources.insert(
            id.into(),
            Source {
                kind: Kind::Video(Box::new(Playback::new(source, looping))),
                showing: None,
                generation: 0,
            },
        );
    }

    /// Adds a still under an id of the caller's choosing, replacing whatever
    /// had that id. It stays until removed, or for `frames` refreshes.
    pub fn insert_still(&mut self, id: impl Into<String>, frame: Frame, frames: Option<u64>) {
        self.sources.insert(
            id.into(),
            Source {
                kind: Kind::Still {
                    frames_left: frames,
                },
                showing: Some(frame),
                generation: 1,
            },
        );
    }

    /// Removes a source and every layer, on every output, that showed it.
    /// Whether there was one.
    pub fn remove_source(&mut self, id: &str) -> bool {
        let existed = self.sources.remove(id).is_some();

        for output in &mut self.outputs {
            output.remove_layer(id);
        }

        existed
    }

    /// Whether a source with this id exists.
    pub fn has_source(&self, id: &str) -> bool {
        self.sources.contains_key(id)
    }

    /// The film behind a source, if it is one.
    pub fn get_source(&self, id: &str) -> Option<&VideoSource> {
        match &self.sources.get(id)?.kind {
            Kind::Video(playback) => Some(playback.source()),
            Kind::Still { .. } => None,
        }
    }

    /// The film behind a source, mutably, if it is one.
    pub fn get_source_mut(&mut self, id: &str) -> Option<&mut VideoSource> {
        match &mut self.sources.get_mut(id)?.kind {
            Kind::Video(playback) => Some(playback.source_mut()),
            Kind::Still { .. } => None,
        }
    }

    /// Adds an output, returning its index.
    pub fn add_output(&mut self, output: Output) -> usize {
        self.outputs.push(output);

        #[cfg(feature = "render")]
        self.gpu.push(OutputGpu::default());

        self.outputs.len() - 1
    }

    /// Get a reference to an output.
    pub fn get_output(&self, index: usize) -> Option<&Output> {
        self.outputs.get(index)
    }

    /// Get a mutable reference to an output.
    pub fn get_output_mut(&mut self, index: usize) -> Option<&mut Output> {
        self.outputs.get_mut(index)
    }

    /// Removes the output at `index`, and what it had on the GPU. Later
    /// outputs move down one.
    pub fn remove_output(&mut self, index: usize) -> Option<Output> {
        if index >= self.outputs.len() {
            return None;
        }

        #[cfg(feature = "render")]
        self.gpu.remove(index);

        Some(self.outputs.remove(index))
    }

    /// Number of sources.
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Number of outputs.
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// [`tick`](Compositor::tick), for callers that do not need to know what
    /// finished. `delta` is unused: every film keeps its own clock.
    ///
    /// # Errors
    ///
    /// None today; the signature leaves room.
    pub fn advance(&mut self, _delta: std::time::Duration) -> Result<()> {
        self.tick();
        Ok(())
    }

    /// Moves every source on by one refresh: each film to whichever frame its
    /// clock says is due, each timed still one frame nearer its end.
    ///
    /// Sources that end or fail are removed, with their layers, and returned —
    /// a film that will not decode is one entry here, not a stopped show.
    pub fn tick(&mut self) -> Vec<Finished> {
        let mut finished = Vec::new();

        for (id, source) in &mut self.sources {
            match &mut source.kind {
                Kind::Video(playback) => match playback.tick() {
                    Ok(Tick::Show(frame)) => {
                        source.showing = Some(frame);
                        source.generation += 1;
                    }
                    Ok(Tick::Hold) => {}
                    Ok(Tick::Ended) => finished.push(Finished {
                        id: id.clone(),
                        error: None,
                    }),
                    Err(error) => finished.push(Finished {
                        id: id.clone(),
                        error: Some(error),
                    }),
                },

                Kind::Still {
                    frames_left: Some(left),
                } => {
                    if *left == 0 {
                        finished.push(Finished {
                            id: id.clone(),
                            error: None,
                        });
                    } else {
                        *left -= 1;
                    }
                }

                Kind::Still { frames_left: None } => {}
            }
        }

        for done in &finished {
            self.remove_source(&done.id);
        }

        finished
    }

    /// Tells every film its latest frame is on screen, which is what starts
    /// its clock. Call once per refresh, after presenting.
    pub fn presented(&mut self) {
        for source in self.sources.values_mut() {
            if let Kind::Video(playback) = &mut source.kind {
                playback.presented();
            }
        }
    }

    /// Checks that an output exists. Drawing it is
    /// [`draw_output`](Compositor::draw_output), which needs a GPU and a
    /// target; this needs neither.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] if there is no output at `index`.
    pub fn render_output(&mut self, index: usize) -> Result<()> {
        self.get_output(index)
            .map(|_| ())
            .ok_or_else(|| Error::unsupported(format!("there is no output {index}")))
    }

    /// Draws output `index` into `view`: the background, then every visible
    /// layer whose source has a picture, bottom to top, in one pass.
    ///
    /// `width` and `height` are the target's pixels. The output's own
    /// coordinates are scaled to them, so an output described at 1920×1080 fills
    /// a surface that is really 3840×2160.
    ///
    /// Pictures are kept per output and belong to `renderer`: draw a given
    /// output with the same renderer every time.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a missing output or a background image in a
    /// build without the `image` feature; whatever loading the background image
    /// or uploading and drawing refuses.
    #[cfg(feature = "render")]
    pub fn draw_output(
        &mut self,
        index: usize,
        gpu: &Gpu,
        renderer: &Renderer,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Result<()> {
        self.compose(index, gpu, renderer, width, height, |clear, layers| {
            renderer.draw_layers(gpu, view, width, height, clear, layers)
        })
    }

    /// [`draw_output`](Compositor::draw_output) into an offscreen texture, read
    /// back as premultiplied RGBA8. For tests, thumbnails, and previews.
    ///
    /// # Errors
    ///
    /// As [`draw_output`](Compositor::draw_output), plus a renderer that does
    /// not draw RGBA8.
    #[cfg(feature = "render")]
    pub fn render_output_to_rgba(
        &mut self,
        index: usize,
        gpu: &Gpu,
        renderer: &Renderer,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        self.compose(index, gpu, renderer, width, height, |clear, layers| {
            renderer.render_layers_to_rgba(gpu, width, height, clear, layers)
        })
    }

    /// Brings output `index`'s pictures up to date and hands the finished
    /// layer list to `finish`.
    #[cfg(feature = "render")]
    fn compose<T>(
        &mut self,
        index: usize,
        gpu: &Gpu,
        renderer: &Renderer,
        width: u32,
        height: u32,
        finish: impl FnOnce(wgpu::Color, &[Layer<'_>]) -> Result<T>,
    ) -> Result<T> {
        use crate::geometry::{Quad, Rect};

        let output = self
            .outputs
            .get(index)
            .ok_or_else(|| Error::unsupported(format!("there is no output {index}")))?;

        if output.width == 0 || output.height == 0 {
            return Err(Error::placement(format!(
                "output {index} is {}×{}, which has nothing to draw into",
                output.width, output.height
            )));
        }

        let state = &mut self.gpu[index];

        // The background image, loaded once and kept until it changes.
        match &output.background_image {
            Some(path) if state.background.as_ref().is_none_or(|kept| &kept.path != path) => {
                let frame = load_background(path)?;
                let mut picture = Picture::new();
                renderer.upload_picture(gpu, &mut picture, &frame)?;

                state.background = Some(Background {
                    path: path.clone(),
                    picture,
                    size: (frame.width(), frame.height()),
                });
            }
            Some(_) => {}
            None => state.background = None,
        }

        // Bottom to top: the highest z-index first, and among equals the one
        // added first, so a later one lands on top.
        let mut stack: Vec<(usize, &crate::output_layer::OutputLayer)> = output
            .layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| layer.visible)
            .collect();
        stack.sort_by(|(a_index, a), (b_index, b)| {
            b.z_index.cmp(&a.z_index).then(a_index.cmp(b_index))
        });

        let scale_x = f64::from(width) / f64::from(output.width);
        let scale_y = f64::from(height) / f64::from(output.height);

        // Upload what changed. A picture per appearance of a source, since one
        // picture cannot be drawn in two places in one pass.
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut placed = Vec::with_capacity(stack.len());

        for (_, layer) in &stack {
            let id = layer.video_source_id.as_str();
            let Some(source) = self.sources.get(id) else {
                continue;
            };
            let Some(frame) = &source.showing else {
                continue;
            };

            let appearance = seen.entry(id).or_insert(0);
            let key = (id.to_string(), *appearance);
            *appearance += 1;

            let entry = state
                .pictures
                .entry(key.clone())
                .or_insert_with(|| (Picture::new(), 0));

            if entry.1 != source.generation {
                renderer.upload_picture(gpu, &mut entry.0, frame)?;
                entry.1 = source.generation;
            }

            let bounds = Rect::new(
                layer.bounds.x * scale_x,
                layer.bounds.y * scale_y,
                layer.bounds.width * scale_x,
                layer.bounds.height * scale_y,
            );

            let rect = layer
                .fit
                .apply(f64::from(frame.width()), f64::from(frame.height()), bounds);

            placed.push((key, Quad::from_rect(rect), layer.opacity));
        }

        // Pictures for sources that went away, or appear fewer times now, go
        // with them.
        let live: std::collections::HashSet<&(String, usize)> =
            placed.iter().map(|(key, _, _)| key).collect();
        state.pictures.retain(|key, _| live.contains(key));

        let mut layers = Vec::with_capacity(placed.len() + 1);

        if let Some(background) = &state.background {
            let (image_width, image_height) = background.size;
            let area = Rect::from_size(f64::from(width), f64::from(height));
            let rect = output.background_fit.apply(
                f64::from(image_width),
                f64::from(image_height),
                area,
            );

            layers.push(Layer {
                picture: &background.picture,
                quad: Quad::from_rect(rect),
                opacity: 1.0,
            });
        }

        for (key, quad, opacity) in &placed {
            layers.push(Layer {
                picture: &state.pictures[key].0,
                quad: *quad,
                opacity: *opacity,
            });
        }

        // Premultiplied, as `draw_layers` wants its clear colour.
        let [red, green, blue, alpha] = output.background_color.map(f64::from);
        let clear = wgpu::Color {
            r: red * alpha,
            g: green * alpha,
            b: blue * alpha,
            a: alpha,
        };

        finish(clear, &layers)
    }

    /// Get all output dimensions as (width, height) tuples.
    pub fn output_dimensions(&self) -> Vec<(u32, u32)> {
        self.outputs.iter().map(|o| (o.width, o.height)).collect()
    }

    /// Get total layer count across all outputs.
    pub fn total_layer_count(&self) -> usize {
        self.outputs.iter().map(|o| o.layer_count()).sum()
    }

    /// Get total visible layer count across all outputs.
    pub fn total_visible_layer_count(&self) -> usize {
        self.outputs.iter().map(|o| o.visible_layer_count()).sum()
    }

    /// Check if all outputs are valid (all referenced sources exist).
    pub fn validate_all_outputs(&self) -> bool {
        self.outputs.iter().all(|output| {
            output
                .visible_layers()
                .all(|layer| self.sources.contains_key(&layer.video_source_id))
        })
    }

    /// Get playback statistics for diagnostics.
    pub fn stats(&self) -> CompositorStats {
        CompositorStats {
            source_count: self.source_count(),
            output_count: self.output_count(),
            total_layers: self.total_layer_count(),
            total_visible_layers: self.total_visible_layer_count(),
            all_outputs_valid: self.validate_all_outputs(),
        }
    }
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}

/// What one output has on the GPU.
#[cfg(feature = "render")]
#[derive(Default)]
struct OutputGpu {
    /// Keyed by source id and which appearance of it on this output, with the
    /// generation last uploaded.
    pictures: HashMap<(String, usize), (Picture, u64)>,
    background: Option<Background>,
}

#[cfg(feature = "render")]
struct Background {
    path: String,
    picture: Picture,
    size: (u32, u32),
}

#[cfg(all(feature = "render", feature = "image"))]
fn load_background(path: &str) -> Result<Frame> {
    crate::still::load_image(path)
}

#[cfg(all(feature = "render", not(feature = "image")))]
fn load_background(path: &str) -> Result<Frame> {
    Err(Error::unsupported(format!(
        "{path}: a background image needs the `image` feature"
    )))
}

/// Compositor statistics for diagnostics and monitoring.
#[derive(Debug, Clone)]
pub struct CompositorStats {
    /// Number of sources.
    pub source_count: usize,
    /// Number of output targets.
    pub output_count: usize,
    /// Total layers across all outputs.
    pub total_layers: usize,
    /// Total visible layers across all outputs.
    pub total_visible_layers: usize,
    /// Whether all outputs reference valid sources.
    pub all_outputs_valid: bool,
}

impl std::fmt::Display for CompositorStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Compositor: {} sources, {} outputs, {} layers ({} visible), valid={}",
            self.source_count,
            self.output_count,
            self.total_layers,
            self.total_visible_layers,
            self.all_outputs_valid
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorSpace;
    use crate::frame::PixelFormat;
    use crate::geometry::Rect;
    use crate::output::Output;
    use crate::output_layer::OutputLayer;
    use std::time::Duration;

    fn solid(rgba: [u8; 4]) -> Frame {
        Frame::packed(
            4,
            4,
            PixelFormat::Rgba8,
            ColorSpace::srgb(),
            Duration::ZERO,
            rgba.repeat(16),
        )
        .unwrap()
    }

    #[test]
    fn compositor_output_management() {
        let mut comp = Compositor::new();
        assert_eq!(comp.output_count(), 0);

        let index = comp.add_output(Output::new(1920, 1080));
        assert_eq!((index, comp.output_count()), (0, 1));

        assert!(comp.remove_output(0).is_some());
        assert_eq!(comp.output_count(), 0);
    }

    /// "Shown for this long" is counted in refreshes: `n` frames means `n`
    /// draws, then it is gone, along with its layer.
    #[test]
    fn a_timed_still_is_shown_for_exactly_its_frames_then_removed() {
        let mut comp = Compositor::new();
        comp.insert_still("card", solid([255, 0, 0, 255]), Some(3));

        let mut output = Output::new(64, 64);
        output.add_layer(OutputLayer::new("card", Rect::from_size(64.0, 64.0)));
        comp.add_output(output);

        for _ in 0..3 {
            assert!(comp.tick().is_empty());
            assert!(comp.has_source("card"));
        }

        let finished = comp.tick();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].id, "card");
        assert!(finished[0].error.is_none());
        assert!(!comp.has_source("card"));
        assert_eq!(comp.get_output(0).unwrap().layer_count(), 0);
    }

    #[test]
    fn an_untimed_still_stays_until_removed() {
        let mut comp = Compositor::new();
        comp.insert_still("logo", solid([0, 0, 255, 255]), None);

        for _ in 0..100 {
            assert!(comp.tick().is_empty());
        }

        assert!(comp.remove_source("logo"));
        assert!(!comp.remove_source("logo"), "it was already gone");
    }

    #[test]
    fn removing_a_source_takes_its_layers_off_every_output() {
        let mut comp = Compositor::new();
        comp.insert_still("shared", solid([0, 255, 0, 255]), None);

        for _ in 0..2 {
            let mut output = Output::new(64, 64);
            output.add_layer(OutputLayer::new("shared", Rect::from_size(32.0, 32.0)));
            output.add_layer(OutputLayer::new("other", Rect::from_size(32.0, 32.0)));
            comp.add_output(output);
        }

        comp.remove_source("shared");

        for index in 0..2 {
            let output = comp.get_output(index).unwrap();
            assert!(!output.has_layer("shared"));
            assert!(output.has_layer("other"));
        }
    }

    #[cfg(feature = "render")]
    mod gpu {
        use super::*;
        use crate::render::{Gpu, Renderer};

        fn gpu() -> Option<(Gpu, Renderer)> {
            let gpu = Gpu::new().map_err(|error| eprintln!("skipping: {error}")).ok()?;
            let renderer = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm).ok()?;
            Some((gpu, renderer))
        }

        fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
            let start = ((y * width + x) * 4) as usize;
            pixels[start..start + 4].try_into().unwrap()
        }

        /// The background colour where nothing is, each layer where only it
        /// is, and z-index 0 on top where two overlap.
        #[test]
        fn an_output_draws_its_background_and_its_layers_in_order() {
            let Some((gpu, renderer)) = gpu() else { return };

            let mut comp = Compositor::new();
            comp.insert_still("red", solid([255, 0, 0, 255]), None);
            comp.insert_still("blue", solid([0, 0, 255, 255]), None);

            let mut output = Output::new(64, 64).with_background_rgb(0.0, 1.0, 0.0);
            output.add_layers([
                // 0 is the top.
                OutputLayer::new("blue", Rect::new(24.0, 24.0, 40.0, 40.0)).with_z_index(0),
                OutputLayer::new("red", Rect::new(0.0, 0.0, 40.0, 40.0)).with_z_index(1),
            ]);
            comp.add_output(output);

            let pixels = comp.render_output_to_rgba(0, &gpu, &renderer, 64, 64).unwrap();

            assert_eq!(pixel(&pixels, 64, 8, 8), [255, 0, 0, 255], "red alone");
            assert_eq!(pixel(&pixels, 64, 32, 32), [0, 0, 255, 255], "blue, z 0, on top");
            assert_eq!(pixel(&pixels, 64, 56, 56), [0, 0, 255, 255], "blue alone");
            assert_eq!(pixel(&pixels, 64, 56, 8), [0, 255, 0, 255], "background");
        }

        /// An output described at one size drawn into a target of another:
        /// the layers scale with it rather than staying in the corner.
        #[test]
        fn layers_scale_with_the_target() {
            let Some((gpu, renderer)) = gpu() else { return };

            let mut comp = Compositor::new();
            comp.insert_still("red", solid([255, 0, 0, 255]), None);

            let mut output = Output::new(32, 32).with_transparent_background();
            output.add_layer(OutputLayer::new("red", Rect::new(16.0, 16.0, 16.0, 16.0)));
            comp.add_output(output);

            // Twice the size: the layer is the bottom-right quarter either way.
            let pixels = comp.render_output_to_rgba(0, &gpu, &renderer, 64, 64).unwrap();

            assert_eq!(pixel(&pixels, 64, 48, 48), [255, 0, 0, 255]);
            assert_eq!(pixel(&pixels, 64, 20, 20)[3], 0, "transparent outside it");
        }

        /// One source on two layers of one output needs two pictures; the
        /// renderer refuses one picture in two places.
        #[test]
        fn one_source_can_appear_twice_on_one_output() {
            let Some((gpu, renderer)) = gpu() else { return };

            let mut comp = Compositor::new();
            comp.insert_still("red", solid([255, 0, 0, 255]), None);

            let mut output = Output::new(64, 64).with_transparent_background();
            output.add_layers([
                OutputLayer::new("red", Rect::new(0.0, 0.0, 16.0, 16.0)),
                OutputLayer::new("red", Rect::new(48.0, 48.0, 16.0, 16.0)),
            ]);
            comp.add_output(output);

            let pixels = comp.render_output_to_rgba(0, &gpu, &renderer, 64, 64).unwrap();

            assert_eq!(pixel(&pixels, 64, 8, 8), [255, 0, 0, 255]);
            assert_eq!(pixel(&pixels, 64, 56, 56), [255, 0, 0, 255]);
            assert_eq!(pixel(&pixels, 64, 32, 32)[3], 0);
        }

        /// A background image fills the output the way a wallpaper does.
        #[cfg(feature = "image")]
        #[test]
        fn a_background_image_covers_the_output() {
            let Some((gpu, renderer)) = gpu() else { return };

            let directory = std::env::temp_dir().join(format!("vtome-bg-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("magenta.png");
            image::save_buffer(&path, &[255, 0, 255, 255].repeat(16), 4, 4, image::ExtendedColorType::Rgba8)
                .unwrap();

            let mut comp = Compositor::new();
            comp.add_output(
                Output::new(64, 32)
                    .with_background_rgb(0.0, 0.0, 0.0)
                    .with_background_image(path.to_string_lossy()),
            );

            let pixels = comp.render_output_to_rgba(0, &gpu, &renderer, 64, 32).unwrap();

            // Cover: a square image on a 2:1 output reaches both sides.
            assert_eq!(pixel(&pixels, 64, 1, 16), [255, 0, 255, 255]);
            assert_eq!(pixel(&pixels, 64, 62, 16), [255, 0, 255, 255]);

            std::fs::remove_dir_all(directory).ok();
        }
    }
}
