//! [`TauriVtome`]: the engine inside a Tauri application.
//!
//! Tauri owns the process's event loop, on the main thread, and macOS will only
//! make windows there — so vtome cannot run a loop of its own. Instead it asks
//! Tauri for bare, borderless windows (no webview) on the main thread, makes a
//! GPU surface on each while it is there, and then does all of its work —
//! decoding, compositing, presenting — on a thread of its own that
//! [`close`](TauriVtome::close) stops. Clips, controls, and outputs are exactly
//! [`Vtome`](crate::Vtome)'s.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tauri::{AppHandle, PhysicalPosition, PhysicalSize, Runtime};

use crate::engine::{
    self, Audio, Clip, ClipId, Command, Controls, Core, Hold, OutputRect, Planned, Screen, Shared,
};
use crate::error::{Error, Result};
use crate::geometry::Rect;
use crate::placement::{monitor_id, Monitor};
use crate::render::Gpu;

/// Video and images on your monitors, as borderless always-on-top overlays,
/// in windows a Tauri application opens for vtome.
///
/// ```ignore
/// use std::collections::HashMap;
/// use vtome::{Clip, TauriVtome};
///
/// tauri::Builder::default().setup(|app| {
///     let mut vtome = TauriVtome::new(app.handle().clone(), ["monitor_e16def3726bc150b"], 30.0);
///
///     let screen = "monitor_342b0e446031e910".to_string();
///     let started = vtome.start(
///         HashMap::from([(screen.clone(), (1920, 1080, 0, 0))]),
///         HashMap::from([(screen.clone(), "#000000FF".to_string())]),
///         vtome::Audio::Off, // or Audio::atome(&output)
///     )?;
///     println!("{} monitors attached", started.monitors.len());
///
///     let id = vtome.add(Clip::video("intro.mp4", &screen))?;
///     app.manage(vtome); // keep it — dropping it closes the outputs
///     Ok(())
/// });
/// ```
///
/// [`start`](TauriVtome::start) returns as soon as the windows are open; the
/// engine keeps running on its own thread until [`close`](TauriVtome::close) or
/// drop. It can be called from any thread, including Tauri's main thread (the
/// `setup` hook, a synchronous command).
pub struct TauriVtome<R: Runtime> {
    app: AppHandle<R>,
    excluded: HashSet<String>,
    frame_rate: f64,
    click_through: bool,
    controls: Controls,
    commands: Mutex<Option<Receiver<Command>>>,
    shared: Arc<Shared>,
    engine: Mutex<Option<JoinHandle<Option<Error>>>>,
}

/// What [`TauriVtome::start`] opened, and what it could not.
#[derive(Clone, Debug)]
pub struct Started {
    /// Every monitor attached, with the ids `start`, `add`, and the excluded
    /// list use.
    pub monitors: Vec<Monitor>,
    /// Monitors with an output running.
    pub running: Vec<String>,
    /// Monitors left out, and why: excluded, not attached, or no area.
    pub skipped: Vec<(String, String)>,
}

impl<R: Runtime> TauriVtome<R> {
    /// An engine in `app` that will never draw on `excluded_monitors` — ids
    /// from [`monitors`](TauriVtome::monitors) — and runs at `frame_rate`
    /// frames a second (held between 1 and 240).
    pub fn new<I, S>(app: AppHandle<R>, excluded_monitors: I, frame_rate: f64) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let frame_rate = engine::sane_frame_rate(frame_rate);
        let (controls, commands, shared) = engine::channel(frame_rate);

        TauriVtome {
            app,
            excluded: excluded_monitors.into_iter().map(Into::into).collect(),
            frame_rate,
            click_through: true,
            controls,
            commands: Mutex::new(Some(commands)),
            shared,
            engine: Mutex::new(None),
        }
    }

    /// Whether the mouse passes through the overlays to whatever is under
    /// them — the application's own windows included. On by default.
    pub fn click_through(mut self, click_through: bool) -> Self {
        self.click_through = click_through;
        self
    }

    /// The monitors attached right now, with the ids `start`, `add`, and the
    /// excluded list use. The same ids [`Vtome`](crate::Vtome) gives, so a
    /// settings file works with either.
    ///
    /// # Errors
    ///
    /// [`Error::Render`] if Tauri cannot list them.
    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        let tauri_error = |error: tauri::Error| Error::Render {
            reason: format!("Tauri would not list the monitors: {error}"),
        };

        let primary = self.app.primary_monitor().map_err(tauri_error)?;

        let monitors = self
            .app
            .available_monitors()
            .map_err(tauri_error)?
            .into_iter()
            .map(|monitor| {
                let name = monitor.name().cloned().unwrap_or_default();
                let position = monitor.position();
                let size = monitor.size();
                let scale_factor = monitor.scale_factor();

                let is_primary = primary.as_ref().is_some_and(|primary| {
                    primary.position() == position && primary.name() == monitor.name()
                });

                Monitor {
                    persistent_id: monitor_id(&name, position.x, position.y, scale_factor),
                    name,
                    bounds: Rect::new(
                        f64::from(position.x),
                        f64::from(position.y),
                        f64::from(size.width),
                        f64::from(size.height),
                    ),
                    scale_factor,
                    // Tauri does not say, which is why the id does not use it.
                    refresh_millihertz: None,
                    is_primary,
                }
            })
            .collect();

        Ok(monitors)
    }

    /// A handle for adding and stopping clips from any thread.
    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    /// [`Controls::add`].
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

    /// [`Controls::stop`].
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
    /// `#RRGGBBAA` — and starts the engine on its own thread. Returns once the
    /// windows are open.
    ///
    /// `outputs` maps a monitor id to `(width, height, x, y)` in physical
    /// pixels, `x` and `y` relative to that monitor's top-left corner. A
    /// monitor with no background is transparent. `audio` is where time comes
    /// from: [`Audio::Off`] for vtome's own clock, or atome's output clock so
    /// every clip follows the sound.
    ///
    /// Returns every attached monitor, and which got an output and which did
    /// not.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchMonitor`] if none of `outputs` can be opened; a
    /// background that is neither a colour nor an image; anything Tauri or the
    /// GPU refuse; and a second call.
    pub fn start(
        &mut self,
        outputs: HashMap<String, OutputRect>,
        backgrounds: HashMap<String, String>,
        audio: Audio,
    ) -> Result<Started> {
        let commands = self
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or_else(|| Error::unsupported("this TauriVtome has already started"))?;

        let attached = self.monitors()?;
        let plans = engine::plan(&outputs, &backgrounds, &self.excluded, &attached, &self.shared)?;

        let (gpu, screens) = self.open(&plans)?;
        let mut core = Core::new(
            gpu,
            &plans,
            screens,
            commands,
            Arc::clone(&self.shared),
            &audio,
        )?;

        let period = Duration::from_secs_f64(1.0 / self.frame_rate);
        let controls = self.controls.clone();

        let engine = std::thread::Builder::new()
            .name("vtome".to_string())
            .spawn(move || {
                let failure = run(&mut core, period).err();

                // Surfaces before windows: a surface outliving its window is
                // drawing into something that is gone.
                for window in core.into_screens().into_iter().map(Screen::into_window) {
                    let _ = window.destroy();
                }

                controls.close();
                failure
            })
            .map_err(|error| Error::Render {
                reason: format!("the engine thread would not start: {error}"),
            })?;

        *self.engine.lock().unwrap_or_else(PoisonError::into_inner) = Some(engine);

        Ok(Started {
            monitors: attached,
            running: self.controls.running_monitors(),
            skipped: self.controls.skipped_monitors(),
        })
    }

    /// Closes every output and waits for the engine thread to finish.
    ///
    /// # Errors
    ///
    /// Whatever stopped the engine early, if something did.
    pub fn close(&self) -> Result<()> {
        self.controls.close();

        let engine = self.engine.lock().unwrap_or_else(PoisonError::into_inner).take();

        match engine.map(JoinHandle::join) {
            Some(Ok(Some(error))) => Err(error),
            Some(Err(_)) => Err(Error::Render {
                reason: "the engine thread panicked".to_string(),
            }),
            _ => Ok(()),
        }
    }

    /// Makes the windows and their surfaces on the main thread, which is the
    /// only thread macOS lets make either — waiting for Tauri to run it there,
    /// or doing it directly when this already is the main thread.
    fn open(&self, plans: &[Planned]) -> Result<(Gpu, Vec<Screen<tauri::Window<R>>>)> {
        let app = self.app.clone();
        let click_through = self.click_through;
        let plans: Vec<Planned> = plans.to_vec();

        let job = move || open_on_main_thread(&app, &plans, click_through);

        if is_main_thread() {
            return job();
        }

        let (reply, answer) = std::sync::mpsc::channel();

        self.app
            .run_on_main_thread(move || {
                let _ = reply.send(job());
            })
            .map_err(|error| Error::Render {
                reason: format!("Tauri would not run on its main thread: {error}"),
            })?;

        answer.recv_timeout(Duration::from_secs(10)).map_err(|_| Error::Render {
            reason: "Tauri's main thread did not open the windows within ten seconds — is its \
                     event loop running?"
                .to_string(),
        })?
    }
}

impl<R: Runtime> Drop for TauriVtome<R> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// The engine thread's loop: one refresh per period, on a fixed grid.
fn run<W>(core: &mut Core<W>, period: Duration) -> Result<()> {
    let mut next = Instant::now();

    while core.refresh()? {
        next += period;

        let now = Instant::now();
        match next.checked_duration_since(now) {
            Some(wait) => std::thread::sleep(wait),
            // Behind: skip ahead rather than catch up in a burst.
            None => next = now,
        }
    }

    Ok(())
}

/// Whether this is the process's main thread, which is where Tauri runs its
/// event loop.
fn is_main_thread() -> bool {
    #[cfg(target_vendor = "apple")]
    {
        extern "C" {
            fn pthread_main_np() -> std::ffi::c_int;
        }

        // SAFETY: no arguments, no state; answers for the calling thread.
        unsafe { pthread_main_np() == 1 }
    }

    #[cfg(not(target_vendor = "apple"))]
    {
        std::thread::current().name() == Some("main")
    }
}

/// Opens every planned window with its surface. Main thread only.
fn open_on_main_thread<R: Runtime>(
    app: &AppHandle<R>,
    plans: &[Planned],
    click_through: bool,
) -> Result<(Gpu, Vec<Screen<tauri::Window<R>>>)> {
    let windows = plans
        .iter()
        .map(|plan| window(app, plan, click_through))
        .collect::<Result<Vec<_>>>()?;

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
        Box::new(windows[0].clone()),
    ));

    let surfaces = windows
        .iter()
        .map(|window| {
            instance
                .create_surface(window.clone())
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

    Ok((gpu, screens))
}

/// One output's window: bare (no webview), borderless, see-through, above
/// everything, and never taking focus from the application's own windows.
fn window<R: Runtime>(
    app: &AppHandle<R>,
    plan: &Planned,
    click_through: bool,
) -> Result<tauri::Window<R>> {
    let tauri_error = |error: tauri::Error| Error::Render {
        reason: format!("Tauri would not open the window for {}: {error}", plan.monitor),
    };

    // Labels are unique per application, and a monitor has one output.
    let label = format!("vtome-{}", plan.monitor);

    let builder = tauri::WindowBuilder::new(app, label)
        .title(format!("vtome — {}", plan.monitor))
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .focused(false)
        .skip_taskbar(true)
        .shadow(false)
        // Placed while hidden, so it never flashes up somewhere else first.
        .visible(false);

    // Everywhere but macOS, Tauri makes a transparent window itself. On macOS
    // it only will behind `macos-private-api`, which is about the *webview*,
    // and which would put the whole application on private API; a bare
    // window needs two public AppKit calls instead — see `see_through`.
    #[cfg(not(target_os = "macos"))]
    let builder = builder.transparent(true);

    let window = builder.build().map_err(tauri_error)?;

    window
        .set_position(PhysicalPosition::new(plan.x, plan.y))
        .map_err(tauri_error)?;
    window
        .set_size(PhysicalSize::new(plan.width, plan.height))
        .map_err(tauri_error)?;

    #[cfg(target_os = "macos")]
    see_through(&window)?;

    if click_through {
        window.set_ignore_cursor_events(true).map_err(tauri_error)?;
    }

    window.show().map_err(tauri_error)?;

    Ok(window)
}

/// `window.opaque = NO; window.backgroundColor = NSColor.clearColor` — what a
/// transparent NSWindow is, in public AppKit. Main thread only.
#[cfg(target_os = "macos")]
fn see_through<R: Runtime>(window: &tauri::Window<R>) -> Result<()> {
    use std::ffi::c_void;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[link(name = "objc")]
    extern "C" {
        fn objc_msgSend();
        fn objc_getClass(name: *const std::ffi::c_char) -> Id;
        fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
    }

    let handle = window.window_handle().map_err(|error| Error::Render {
        reason: format!("no native handle for an output window: {error}"),
    })?;

    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return Err(Error::Render {
            reason: "a macOS window without an AppKit handle".to_string(),
        });
    };

    // `objc_msgSend` has to be called through a pointer of the exact type of
    // each method: on arm64 a mismatched signature is not an error, it is a
    // corrupted call.
    let send_id: unsafe extern "C" fn(Id, Sel) -> Id =
        // SAFETY: the signature of `-[NSView window]` and `+[NSColor clearColor]`.
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    let send_bool: unsafe extern "C" fn(Id, Sel, bool) =
        // SAFETY: the signature of `-[NSWindow setOpaque:]`, whose BOOL is a
        // one-byte `bool` on arm64 and a `signed char` on x86-64 — both passed
        // as the low byte of a register.
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    let send_object: unsafe extern "C" fn(Id, Sel, Id) =
        // SAFETY: the signature of `-[NSWindow setBackgroundColor:]`.
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };

    // SAFETY: `ns_view` is a live NSView from a window Tauri just made, and this
    // runs on the main thread, where AppKit requires it. The selectors and the
    // class are AppKit's own.
    unsafe {
        let view = appkit.ns_view.as_ptr();
        let ns_window = send_id(view, sel_registerName(c"window".as_ptr()));

        if ns_window.is_null() {
            return Err(Error::Render {
                reason: "an output view that is in no window".to_string(),
            });
        }

        let clear = send_id(
            objc_getClass(c"NSColor".as_ptr()),
            sel_registerName(c"clearColor".as_ptr()),
        );

        send_bool(ns_window, sel_registerName(c"setOpaque:".as_ptr()), false);
        send_object(ns_window, sel_registerName(c"setBackgroundColor:".as_ptr()), clear);
    }

    Ok(())
}
