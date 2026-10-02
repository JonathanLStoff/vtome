//! Windows of vtome's own, on the monitor you asked for.
//!
//! This is the optional half. An application that already has a window — a
//! Tauri front end, a game engine — takes the `render` feature and hands
//! [`Renderer`](crate::render::Renderer) a surface of its own; nothing here is
//! compiled in that case, and neither is winit.
//!
//! ```no_run
//! use vtome::{MonitorSelector, Placement};
//! use vtome::geometry::{Quad, Rect};
//! use vtome::window::Viewer;
//!
//! let frame = vtome::load_image("poster.png")?;
//!
//! // The projector, keystoned because it is aimed upwards at the wall.
//! let placement = Placement::new(MonitorSelector::Name("EPSON".into()))
//!     .corners(Quad::keystone(Rect::from_size(1920.0, 1080.0), 180.0));
//!
//! Viewer::new(frame, placement).show()?;
//! # Ok::<(), vtome::Error>(())
//! ```
//!
//! A film is the same call with a [`crate::VideoSource`], and
//! shows up as an overlay — above other applications and, if asked, letting
//! the mouse through to them:
//!
//! ```no_run
//! # #[cfg(feature = "demux")] {
//! use vtome::{MonitorSelector, Placement, VideoSource};
//! use vtome::geometry::Rect;
//! use vtome::window::Viewer;
//!
//! let source = VideoSource::from_file("clip.mp4")?;
//! let placement = Placement::new(MonitorSelector::Primary)
//!     .area(Rect::new(40.0, 40.0, 640.0, 360.0))
//!     .always_on_top(true);
//!
//! Viewer::video(source, placement).click_through(true).show()?;
//! # }
//! # Ok::<(), vtome::Error>(())
//! ```
//!
//! # Mobile has no monitors
//!
//! iOS and Android give an application one surface and no desktop to place it
//! on. The placement still resolves — it reports a single monitor the size of
//! that surface — so the same code runs, but "the second screen" means nothing
//! there. See `planning/TODO.md` §11.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId, WindowLevel};

use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::geometry::Rect;
use crate::placement::{monitor_id, Monitor, Placement};
#[cfg(feature = "demux")]
use crate::playback::{Playback, Tick};
use crate::render::{Gpu, Renderer};
#[cfg(feature = "demux")]
use crate::video_source::VideoSource;

/// The monitors attached right now.
///
/// # Errors
///
/// [`Error::Render`] if the platform will not start an event loop, which on
/// macOS also means "this was not called from the main thread".
///
/// # Panics on a second call
///
/// An event loop can be built once per process on most platforms, and this
/// builds one. Call it before [`Viewer::show`] or not at all — a viewer reports
/// the monitor it landed on through
/// [`ResolvedPlacement`](crate::ResolvedPlacement) anyway.
pub fn monitors() -> Result<Vec<Monitor>> {
    // Only an *active* event loop can be asked about monitors, so this starts
    // one, takes the list on the first callback, and stops it again. No window
    // is ever created.
    struct Collector(Vec<Monitor>);

    impl ApplicationHandler for Collector {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            self.0 = attached(event_loop);
            event_loop.exit();
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    let event_loop = EventLoop::new().map_err(|error| Error::Render {
        reason: format!("no event loop, so no monitors to list: {error}"),
    })?;

    let mut collector = Collector(Vec::new());

    event_loop
        .run_app(&mut collector)
        .map_err(|error| Error::Render {
            reason: format!("the event loop stopped before it listed anything: {error}"),
        })?;

    Ok(collector.0)
}

/// The monitors an active event loop can see.
pub(crate) fn attached(event_loop: &ActiveEventLoop) -> Vec<Monitor> {
    let primary = event_loop.primary_monitor();

    event_loop
        .available_monitors()
        .map(|handle| {
            let is_primary = primary
                .as_ref()
                .is_some_and(|candidate| *candidate == handle);

            describe(&handle, is_primary)
        })
        .collect()
}

/// One winit monitor as vtome describes it.
fn describe(handle: &winit::monitor::MonitorHandle, is_primary: bool) -> Monitor {
    let position = handle.position();
    let size = handle.size();
    let name = handle.name().unwrap_or_default();
    let scale_factor = handle.scale_factor();
    let refresh_millihertz = handle.refresh_rate_millihertz();
    let persistent_id = monitor_id(&name, position.x, position.y, scale_factor);

    Monitor {
        name,
        bounds: Rect::new(
            f64::from(position.x),
            f64::from(position.y),
            f64::from(size.width),
            f64::from(size.height),
        ),
        scale_factor,
        refresh_millihertz,
        is_primary,
        persistent_id,
    }
}

/// Shows a picture or a film in one place, as an overlay, until it is closed.
///
/// The window is the picture's own shape — the bounds of the placement's quad
/// and no larger — with no title bar or border, and transparent wherever the
/// quad does not reach, so a corner-pinned picture shows the desktop around it.
/// With [`Placement::always_on_top`] it stays above other applications, and
/// [`click_through`](Viewer::click_through) lets the mouse fall through to
/// whatever is underneath: a graphic laid over the screen rather than an
/// application window.
///
/// This is the "put that there" path. Seeking, playlists, and several layers on
/// one output belong to an application built on
/// [`Renderer`], which does not need to own the event
/// loop the way this does.
pub struct Viewer {
    content: Content,
    placement: Placement,
    title: String,
    decorations: bool,
    click_through: bool,
    looping: bool,
}

impl Viewer {
    /// A viewer for one frame at one placement.
    pub fn new(frame: Frame, placement: Placement) -> Self {
        Viewer::showing(Content::Still(frame), placement)
    }

    /// A viewer that plays `source` at one placement, looping unless told
    /// [not to](Viewer::looping).
    ///
    /// Each picture goes up when its own timestamp says, against a clock that
    /// starts once the first one is on screen — not as fast as they decode and
    /// not one per refresh. Late pictures are dropped rather than shown late,
    /// so a stall costs a few frames instead of permanent lag.
    #[cfg(feature = "demux")]
    pub fn video(source: VideoSource, placement: Placement) -> Self {
        Viewer::showing(
            Content::Video(Box::new(Playback::new(
                source,
                true,
                crate::clock::Monotonic::shared(),
                None,
            ))),
            placement,
        )
    }

    fn showing(content: Content, placement: Placement) -> Self {
        Viewer {
            content,
            placement,
            title: "vtome".to_string(),
            decorations: false,
            click_through: false,
            looping: true,
        }
    }

    /// Whether the mouse passes through to whatever is underneath.
    ///
    /// Off by default. On, the overlay can no longer be clicked to give it
    /// focus, and Escape only reaches a focused window — so something else has
    /// to end it: the application that opened it, or a film that does not loop.
    pub fn click_through(mut self, click_through: bool) -> Self {
        self.click_through = click_through;
        self
    }

    /// Whether a film starts again when it ends — the default — or the viewer
    /// closes. A still ignores this.
    pub fn looping(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }

    /// The window title, where the platform shows one.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Whether to draw a title bar and a border.
    ///
    /// Off by default: a corner-pinned picture in a decorated window is a
    /// trapezoid inside a rectangle, which is rarely what anybody wanted.
    pub fn decorations(mut self, decorations: bool) -> Self {
        self.decorations = decorations;
        self
    }

    /// Opens the window and runs until it is closed, Escape is pressed, or a
    /// film that does not loop ends. Space pauses a film.
    ///
    /// Blocks. On macOS it has to be called from the main thread, which is the
    /// platform's rule rather than this crate's.
    ///
    /// # Errors
    ///
    /// [`Error::Render`] for anything the event loop, the GPU, or the window
    /// refuses; [`Error::NoSuchMonitor`] or [`Error::Placement`] from resolving
    /// the placement against the monitors that are actually there;
    /// [`Error::Decode`] if a film stops decoding partway.
    pub fn show(self) -> Result<Report> {
        let event_loop = EventLoop::new().map_err(|error| Error::Render {
            reason: format!("the event loop would not start: {error}"),
        })?;

        // A still waits for events rather than spinning: redrawing one picture
        // at the refresh rate would burn a core to show nothing new. A film
        // polls, and the surface's vsync is what paces it.
        event_loop.set_control_flow(match self.content {
            Content::Still(_) => ControlFlow::Wait,
            #[cfg(feature = "demux")]
            Content::Video(_) => ControlFlow::Poll,
        });

        let mut application = Application {
            viewer: self,
            state: None,
            failure: None,
        };

        event_loop
            .run_app(&mut application)
            .map_err(|error| Error::Render {
                reason: format!("the event loop stopped: {error}"),
            })?;

        match application.failure {
            Some(error) => Err(error),
            None => Ok(application.viewer.content.report()),
        }
    }
}

/// What happened while a [`Viewer`] was up.
///
/// The counters are [`crate::clock::Pacing`]'s: dropped pictures mean
/// decoding fell behind the clock, and repeats mean the display refreshes
/// faster than the film changes — normal, at 24 fps on a 60 Hz screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Pictures put on screen. One, for a still.
    pub presented: u64,
    /// Pictures that arrived too late to be worth showing.
    pub dropped: u64,
    /// Refreshes that showed the previous picture again.
    pub repeated: u64,
    /// How many times a film started again from the top.
    pub loops: u32,
    /// Whether the film was decoded in hardware. `None` for a still.
    pub hardware_decode: Option<bool>,
}

/// What a viewer shows.
enum Content {
    Still(Frame),
    #[cfg(feature = "demux")]
    Video(Box<Playback>),
}

impl Content {
    /// The picture, when there is only ever one.
    fn still(&self) -> Option<&Frame> {
        match self {
            Content::Still(frame) => Some(frame),
            #[cfg(feature = "demux")]
            Content::Video(_) => None,
        }
    }

    /// The picture's size, known before any window exists.
    fn size(&self) -> (u32, u32) {
        match self {
            Content::Still(frame) => (frame.width(), frame.height()),
            // As stored rather than as rotated: the renderer draws the planes
            // as they are, so the placement has to be sized to match.
            #[cfg(feature = "demux")]
            Content::Video(playback) => playback.size(),
        }
    }

    fn report(&self) -> Report {
        match self {
            Content::Still(_) => Report {
                presented: 1,
                ..Report::default()
            },
            #[cfg(feature = "demux")]
            Content::Video(playback) => {
                let (presented, dropped, repeated) = playback.counts();

                Report {
                    presented,
                    dropped,
                    repeated,
                    loops: playback.loops(),
                    hardware_decode: Some(playback.source().decoder().is_hardware()),
                }
            }
        }
    }
}

/// Everything that exists only once the window does.
struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Gpu,
    renderer: Renderer,
    /// Whether anything has been uploaded yet. A film has nothing to draw
    /// until its first picture decodes.
    has_picture: bool,
    /// The quad in the window's own coordinates, ready for the shader.
    quad: crate::geometry::Quad,
    opacity: f32,
}

struct Application {
    viewer: Viewer,
    state: Option<State>,
    /// Kept rather than returned: `ApplicationHandler` has nowhere to put an
    /// error, and exiting the loop silently would leave a caller thinking the
    /// window had been shown and closed.
    failure: Option<Error>,
}

impl Application {
    /// Builds the window, the surface, and the renderer. Called once.
    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<State> {
        let monitors = attached(event_loop);
        let (width, height) = self.viewer.content.size();

        let resolved = self.viewer.placement.resolve(width, height, &monitors)?;

        if resolved.fell_back {
            eprintln!(
                "vtome: {} is not attached — showing on {} instead",
                self.viewer.placement.monitor, resolved.monitor.name
            );
        }

        let rect = resolved.window_rect();
        let (x, y, width, height) = rect.to_physical();

        let attributes = Window::default_attributes()
            .with_title(&self.viewer.title)
            .with_decorations(self.viewer.decorations)
            // Transparent so that everything outside a corner-pinned quad shows
            // what is behind the window rather than black.
            .with_transparent(true)
            .with_position(winit::dpi::PhysicalPosition::new(x, y))
            .with_inner_size(winit::dpi::PhysicalSize::new(width.max(1), height.max(1)))
            .with_window_level(if resolved.always_on_top {
                WindowLevel::AlwaysOnTop
            } else {
                WindowLevel::Normal
            });

        let window =
            Arc::new(
                event_loop
                    .create_window(attributes)
                    .map_err(|error| Error::Render {
                        reason: format!("the window would not open: {error}"),
                    })?,
            );

        if self.viewer.click_through {
            window
                .set_cursor_hittest(false)
                .map_err(|error| Error::Render {
                    reason: format!("this platform will not let clicks through a window: {error}"),
                })?;
        }

        // The instance has to know about the display to make a surface from it,
        // which is why this is not `Gpu::new`.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(Arc::clone(&window)),
        ));

        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|error| Error::Render {
                reason: format!("no surface for that window: {error}"),
            })?;

        let gpu = Gpu::from_instance(instance, Some(&surface))?;

        let capabilities = surface.get_capabilities(&gpu.adapter);
        let format = crate::render::surface_format(&capabilities);

        let size = window.inner_size();

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Srgb,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: crate::render::see_through(&capabilities),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        surface.configure(&gpu.device, &config);

        let mut renderer = Renderer::new(&gpu, format)?;
        let mut has_picture = false;

        if let Some(frame) = self.viewer.content.still() {
            renderer.upload(&gpu, frame)?;
            has_picture = true;
        }

        Ok(State {
            window,
            surface,
            config,
            gpu,
            renderer,
            has_picture,
            quad: resolved.quad_in_window(),
            opacity: resolved.opacity,
        })
    }

    /// One refresh: a film decides what is due, then whatever is uploaded is
    /// drawn.
    fn refresh(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let Some(state) = self.state.as_mut() else {
            return Ok(());
        };

        #[cfg(feature = "demux")]
        if let Content::Video(playback) = &mut self.viewer.content {
            playback.looping = self.viewer.looping;

            match playback.tick()? {
                Tick::Show(frame) => {
                    state.renderer.upload(&state.gpu, &frame)?;
                    state.has_picture = true;
                }
                Tick::Hold => {}
                Tick::Ended => {
                    event_loop.exit();
                    return Ok(());
                }
            }
        }

        if !state.has_picture {
            return Ok(());
        }

        Application::redraw(state)?;

        #[cfg(feature = "demux")]
        if let Content::Video(playback) = &mut self.viewer.content {
            playback.presented();
        }

        // Only a film uses the loop to end itself.
        let _ = event_loop;

        Ok(())
    }

    /// Draws one frame into the surface.
    fn redraw(state: &mut State) -> Result<()> {
        use wgpu::CurrentSurfaceTexture;

        let texture = match state.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(texture)
            | CurrentSurfaceTexture::Suboptimal(texture) => texture,

            // A resize, a monitor change, or a minimised window. The next
            // redraw gets a fresh swapchain; none of these is fatal.
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Outdated
            | CurrentSurfaceTexture::Lost => return Ok(()),

            other => {
                return Err(Error::Render {
                    reason: format!("no surface texture to draw into: {other:?}"),
                })
            }
        };

        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let size = state.window.inner_size();

        state.renderer.draw(
            &state.gpu,
            &view,
            size.width.max(1),
            size.height.max(1),
            state.quad,
            state.opacity,
        )?;

        // Presenting is the queue's job in this version of wgpu, not the
        // texture's.
        state.gpu.queue.present(texture);

        Ok(())
    }
}

impl ApplicationHandler for Application {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        match self.start(event_loop) {
            Ok(state) => {
                state.window.request_redraw();
                self.state = Some(state);
            }
            Err(error) => {
                self.failure = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };

        match event {
            WindowEvent::CloseRequested
            | WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => event_loop.exit(),

            #[cfg(feature = "demux")]
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Space),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => {
                if let Content::Video(playback) = &mut self.viewer.content {
                    playback.toggle_pause();
                }
            }

            WindowEvent::Resized(size) => {
                state.config.width = size.width.max(1);
                state.config.height = size.height.max(1);
                state.surface.configure(&state.gpu.device, &state.config);

                state.window.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                if let Err(error) = self.refresh(event_loop) {
                    self.failure = Some(error);
                    event_loop.exit();
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // A film asks for the next refresh as soon as this one is done; the
        // surface's vsync is what keeps that to one per display frame.
        #[cfg(feature = "demux")]
        if let (Content::Video(_), Some(state)) = (&self.viewer.content, &self.state) {
            state.window.request_redraw();
        }
    }
}
