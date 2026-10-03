//! [`Vtome`]: the engine with windows and an event loop of its own.
//!
//! For an application that does not already own an event loop — a command-line
//! tool, a service, a test. Inside a Tauri application, where Tauri owns the
//! loop, use [`TauriVtome`](crate::TauriVtome) instead: same clips, same
//! controls.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId, WindowLevel};

use crate::engine::{
    self, Audio, Clip, ClipId, Command, Controls, Core, Hold, OutputRect, Planned, Screen, Shared,
};
use crate::error::{Error, Result};
use crate::placement::Monitor;
use crate::render::Gpu;

/// Video and images on your monitors, as borderless always-on-top overlays,
/// in windows vtome opens itself.
///
/// ```no_run
/// use std::collections::HashMap;
/// use vtome::{Clip, Vtome};
///
/// let mut vtome = Vtome::new(["monitor_e16def3726bc150b"], 30.0);
///
/// // Clips are added from another thread while `start` runs this one.
/// let controls = vtome.controls();
/// std::thread::spawn(move || {
///     let clip = Clip::video("intro.mp4", "monitor_342b0e446031e910");
///     let id = controls.add(clip).expect("the file opens");
///     # let _ = id;
/// });
///
/// let outputs = HashMap::from([("monitor_342b0e446031e910".to_string(), (1920, 1080, 0, 0))]);
/// let backgrounds = HashMap::from([("monitor_342b0e446031e910".to_string(), "#000000FF".to_string())]);
///
/// // No audio: vtome keeps its own clock. `Audio::atome(&output)`
/// // would have every clip follow atome's output instead.
/// vtome.start(outputs, backgrounds, vtome::Audio::Off)?; // runs until `controls.close()`
/// # Ok::<(), vtome::Error>(())
/// ```
///
/// # One thread owns the windows
///
/// [`start`](Vtome::start) runs the event loop and does not return until
/// [`Controls::close`] is called. On macOS it has to be called on the main
/// thread, and an event loop can run once per process — so a `Vtome` starts
/// once. Everything else goes through [`Controls`], which any thread can hold.
pub struct Vtome {
    excluded: HashSet<String>,
    frame_rate: f64,
    click_through: bool,
    controls: Controls,
    commands: Option<Receiver<Command>>,
    shared: Arc<Shared>,
}

impl Vtome {
    /// An engine that will never draw on `excluded_monitors` — ids from
    /// [`Monitor::persistent_id`] — and runs at `frame_rate` frames a second
    /// (held between 1 and 240).
    pub fn new<I, S>(excluded_monitors: I, frame_rate: f64) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let frame_rate = engine::sane_frame_rate(frame_rate);
        let (controls, commands, shared) = engine::channel(frame_rate);

        Vtome {
            excluded: excluded_monitors.into_iter().map(Into::into).collect(),
            frame_rate,
            click_through: true,
            controls,
            commands: Some(commands),
            shared,
        }
    }

    /// Whether the mouse passes through the overlays to whatever is under
    /// them. On by default: an output covering a monitor would otherwise
    /// swallow every click on it.
    pub fn click_through(mut self, click_through: bool) -> Self {
        self.click_through = click_through;
        self
    }

    /// A handle for adding and stopping clips from any thread.
    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    /// [`Controls::add`], from the thread that holds the engine.
    ///
    /// # Errors
    ///
    /// As [`Controls::add`].
    pub fn add(&self, clip: Clip) -> Result<ClipId> {
        self.controls.add(clip)
    }

    /// [`Controls::add_image`]: an image file on a monitor's whole output for
    /// a number of frames, a length of time, or until stopped.
    ///
    /// # Errors
    ///
    /// As [`Controls::add_image`].
    pub fn add_image(
        &self,
        path: impl Into<std::path::PathBuf>,
        monitor: impl Into<String>,
        hold: Hold,
    ) -> Result<ClipId> {
        self.controls.add_image(path, monitor, hold)
    }

    /// [`Controls::add_frame`]: a frame already in hand, held the same way.
    ///
    /// # Errors
    ///
    /// As [`Controls::add_frame`].
    pub fn add_frame(
        &self,
        frame: crate::frame::Frame,
        monitor: impl Into<String>,
        hold: Hold,
    ) -> Result<ClipId> {
        self.controls.add_frame(frame, monitor, hold)
    }

    /// [`Controls::add_generic`]: any file, on a monitor's whole output.
    ///
    /// # Errors
    ///
    /// As [`Controls::add_generic`].
    pub fn add_generic(
        &self,
        path: impl Into<std::path::PathBuf>,
        monitor: impl Into<String>,
    ) -> Result<ClipId> {
        self.controls.add_generic(path, monitor)
    }

    /// [`Controls::add_cached`]: a video with ranges of it served from memory.
    ///
    /// # Errors
    ///
    /// As [`Controls::add_cached`].
    pub fn add_cached(
        &self,
        clip: Clip,
        ranges: Vec<(u64, u64)>,
        frames: Vec<crate::frame::Frame>,
    ) -> Result<ClipId> {
        self.controls.add_cached(clip, ranges, frames)
    }

    /// [`Controls::stop`], from the thread that holds the engine.
    pub fn stop(&self, id: ClipId) -> bool {
        self.controls.stop(id)
    }

    /// [`Controls::is_running`].
    pub fn is_running(&self, id: ClipId) -> bool {
        self.controls.is_running(id)
    }

    /// [`Controls::is_finished`].
    pub fn is_finished(&self, id: ClipId) -> bool {
        self.controls.is_finished(id)
    }

    /// [`Controls::snapshot`]: the picture a clip is showing now.
    ///
    /// # Errors
    ///
    /// As [`Controls::snapshot`].
    pub fn snapshot(&self, id: ClipId) -> Result<crate::frame::Frame> {
        self.controls.snapshot(id)
    }

    /// Opens an output on every monitor in `outputs` that is attached and not
    /// excluded, shows its background — a path to an image, or a colour as
    /// `#RRGGBBAA` — and runs until [`Controls::close`].
    ///
    /// `outputs` maps a monitor id to `(width, height, x, y)` in physical
    /// pixels, `x` and `y` relative to that monitor's top-left corner. A
    /// monitor with no background is transparent. Monitors left out, and why,
    /// are in [`Controls::skipped_monitors`].
    ///
    /// `audio` is where time comes from: [`Audio::Off`] for vtome's own clock,
    /// or atome's output clock so every clip follows the sound.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchMonitor`] if none of `outputs` can be opened; a
    /// background that is neither a colour nor an image; anything the event
    /// loop, the windows, or the GPU refuse; and a second call, since an event
    /// loop runs once.
    pub fn start(
        &mut self,
        outputs: HashMap<String, OutputRect>,
        backgrounds: HashMap<String, String>,
        audio: Audio,
    ) -> Result<()> {
        self.start_with(audio, move |_| (outputs, backgrounds))
    }

    /// [`start`](Vtome::start), with the outputs chosen once the attached
    /// monitors are known — the list of monitors a start hands back.
    ///
    /// An event loop is the only way to ask about monitors, and it runs once,
    /// so a program that needs monitor ids before it can say which outputs it
    /// wants gets them here, inside the loop, rather than from a second one.
    ///
    /// # Errors
    ///
    /// As [`start`](Vtome::start).
    pub fn start_with<F>(&mut self, audio: Audio, choose: F) -> Result<()>
    where
        F: FnOnce(&[Monitor]) -> (HashMap<String, OutputRect>, HashMap<String, String>) + 'static,
    {
        let commands = self.commands.take().ok_or_else(|| {
            Error::unsupported("this Vtome has already run; an event loop runs once per process")
        })?;

        let event_loop = EventLoop::new().map_err(|error| Error::Render {
            reason: format!("the event loop would not start: {error}"),
        })?;

        let mut host = Host {
            choose: Some(Box::new(choose)),
            audio,
            excluded: self.excluded.clone(),
            click_through: self.click_through,
            commands: Some(commands),
            shared: Arc::clone(&self.shared),
            period: Duration::from_secs_f64(1.0 / self.frame_rate),
            next: Instant::now(),
            core: None,
            failure: None,
        };

        let outcome = event_loop.run_app(&mut host);

        // Whatever ended the loop, nothing more will be drawn.
        self.controls.close();

        outcome.map_err(|error| Error::Render {
            reason: format!("the event loop stopped: {error}"),
        })?;

        match host.failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

type Choose = Box<dyn FnOnce(&[Monitor]) -> (HashMap<String, OutputRect>, HashMap<String, String>)>;

/// The event loop's side of a running [`Vtome`].
struct Host {
    choose: Option<Choose>,
    audio: Audio,
    excluded: HashSet<String>,
    click_through: bool,
    commands: Option<Receiver<Command>>,
    shared: Arc<Shared>,
    period: Duration,
    next: Instant,
    core: Option<Core<Arc<Window>>>,
    /// Kept rather than returned: `ApplicationHandler` has nowhere to put an
    /// error, and exiting quietly would read as a clean close.
    failure: Option<Error>,
}

impl Host {
    /// Plans the outputs, opens a window on each, and builds the engine.
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<Core<Arc<Window>>> {
        let attached = crate::window::attached(event_loop);

        let choose = self
            .choose
            .take()
            .ok_or_else(|| Error::unsupported("the engine was already opened"))?;
        let (outputs, backgrounds) = choose(&attached);

        let plans = engine::plan(&outputs, &backgrounds, &self.excluded, &attached, &self.shared)?;

        let windows = plans
            .iter()
            .map(|plan| self.window(event_loop, plan))
            .collect::<Result<Vec<_>>>()?;

        // One instance for every surface, made knowing the display — which is
        // why this is not `Gpu::new`.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(Arc::clone(&windows[0])),
        ));

        let surfaces = windows
            .iter()
            .map(|window| {
                instance
                    .create_surface(Arc::clone(window))
                    .map_err(|error| Error::Render {
                        reason: format!("no surface for an output window: {error}"),
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        let gpu = Gpu::from_instance(instance, surfaces.first())?;

        let screens = plans
            .iter()
            .zip(windows)
            .zip(surfaces)
            .map(|((plan, window), surface)| Screen::new(plan, window, surface, &gpu))
            .collect();

        let commands = self
            .commands
            .take()
            .ok_or_else(|| Error::unsupported("the engine was already opened"))?;

        Core::new(
            gpu,
            &plans,
            screens,
            commands,
            Arc::clone(&self.shared),
            &self.audio,
        )
    }

    /// One output's window: borderless, see-through, above everything, and
    /// never taking focus from the application in front.
    fn window(&self, event_loop: &ActiveEventLoop, plan: &Planned) -> Result<Arc<Window>> {
        let attributes = Window::default_attributes()
            .with_title(format!("vtome — {}", plan.monitor))
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_active(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(plan.x, plan.y))
            .with_inner_size(PhysicalSize::new(plan.width, plan.height));

        // A drop shadow around an overlay is a grey smudge on the picture
        // behind it.
        #[cfg(target_os = "macos")]
        let attributes = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attributes.with_has_shadow(false)
        };

        let window = event_loop
            .create_window(attributes)
            .map_err(|error| Error::Render {
                reason: format!("the window for {} would not open: {error}", plan.monitor),
            })?;

        if self.click_through {
            window
                .set_cursor_hittest(false)
                .map_err(|error| Error::Render {
                    reason: format!("this platform will not let clicks through a window: {error}"),
                })?;
        }

        Ok(Arc::new(window))
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: Error) {
        self.failure = Some(error);
        event_loop.exit();
    }
}

impl ApplicationHandler for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.core.is_some() || self.failure.is_some() {
            return;
        }

        match self.open(event_loop) {
            Ok(core) => {
                self.core = Some(core);
                self.next = Instant::now();
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.next));
            }
            Err(error) => self.fail(event_loop, error),
        }
    }

    // The outputs take no input: they are click-through and never focused.
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(core) = self.core.as_mut() else {
            return;
        };

        let now = Instant::now();

        if now >= self.next {
            match core.refresh() {
                Ok(true) => {}
                Ok(false) => {
                    event_loop.exit();
                    return;
                }
                Err(error) => {
                    self.fail(event_loop, error);
                    return;
                }
            }

            // On a fixed grid rather than "a period from now", so the rate
            // does not drift by however long each frame took. A stall skips
            // ahead instead of trying to catch up in a burst.
            self.next += self.period;
            if self.next < now {
                self.next = now + self.period;
            }
        }

        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next));
    }
}
