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
use crate::placement::{Monitor, Placement};
use crate::render::{Gpu, Renderer};

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
fn attached(event_loop: &ActiveEventLoop) -> Vec<Monitor> {
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

    Monitor {
        name: handle.name().unwrap_or_default(),
        bounds: Rect::new(
            f64::from(position.x),
            f64::from(position.y),
            f64::from(size.width),
            f64::from(size.height),
        ),
        scale_factor: handle.scale_factor(),
        refresh_millihertz: handle.refresh_rate_millihertz(),
        is_primary,
    }
}

/// Shows one picture, in one place, until it is closed.
///
/// Deliberately small: this is the "put that there" path, not a media player.
/// A player belongs on top of [`Renderer`](crate::render::Renderer) with a
/// decoder feeding it, and does not need to own the event loop the way this
/// does.
pub struct Viewer {
    frame: Frame,
    placement: Placement,
    title: String,
    decorations: bool,
}

impl Viewer {
    /// A viewer for one frame at one placement.
    pub fn new(frame: Frame, placement: Placement) -> Self {
        Viewer {
            frame,
            placement,
            title: "vtome".to_string(),
            decorations: false,
        }
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

    /// Opens the window and runs until it is closed or Escape is pressed.
    ///
    /// Blocks. On macOS it has to be called from the main thread, which is the
    /// platform's rule rather than this crate's.
    ///
    /// # Errors
    ///
    /// [`Error::Render`] for anything the event loop, the GPU, or the window
    /// refuses; [`Error::NoSuchMonitor`] or [`Error::Placement`] from resolving
    /// the placement against the monitors that are actually there.
    pub fn show(self) -> Result<()> {
        let event_loop = EventLoop::new().map_err(|error| Error::Render {
            reason: format!("the event loop would not start: {error}"),
        })?;

        // Wait for events rather than spinning: one still picture redrawn at
        // the refresh rate would burn a core to show something that is not
        // changing.
        event_loop.set_control_flow(ControlFlow::Wait);

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
            None => Ok(()),
        }
    }
}

/// Everything that exists only once the window does.
struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    gpu: Gpu,
    renderer: Renderer,
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

        let resolved = self.viewer.placement.resolve(
            self.viewer.frame.width(),
            self.viewer.frame.height(),
            &monitors,
        )?;

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
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .unwrap_or(capabilities.formats[0]);

        let size = window.inner_size();

        surface.configure(
            &gpu.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                color_space: wgpu::SurfaceColorSpace::Srgb,
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: wgpu::PresentMode::AutoVsync,
                // Whether the compositor honours our alpha is the platform's
                // decision; taking whatever it offers first is the only
                // portable answer.
                alpha_mode: capabilities.alpha_modes[0],
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );

        let mut renderer = Renderer::new(&gpu, format)?;
        renderer.upload(&gpu, &self.viewer.frame)?;

        Ok(State {
            window,
            surface,
            gpu,
            renderer,
            quad: resolved.quad_in_window(),
            opacity: resolved.opacity,
        })
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

            WindowEvent::Resized(size) => {
                let capabilities = state.surface.get_capabilities(&state.gpu.adapter);

                state.surface.configure(
                    &state.gpu.device,
                    &wgpu::SurfaceConfiguration {
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        format: state.renderer.format(),
                        color_space: wgpu::SurfaceColorSpace::Srgb,
                        width: size.width.max(1),
                        height: size.height.max(1),
                        present_mode: wgpu::PresentMode::AutoVsync,
                        alpha_mode: capabilities.alpha_modes[0],
                        view_formats: vec![],
                        desired_maximum_frame_latency: 2,
                    },
                );

                state.window.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                if let Err(error) = Application::redraw(state) {
                    self.failure = Some(error);
                    event_loop.exit();
                }
            }

            _ => {}
        }
    }
}
