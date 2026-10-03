//! The core both versions of the engine share: outputs per monitor, clips
//! added and stopped from any thread, and one refresh at the configured rate.
//!
//! [`Vtome`](crate::Vtome) opens its own windows and runs its own event loop;
//! [`TauriVtome`](crate::TauriVtome) opens Tauri windows inside an application
//! that already owns the event loop. Everything they have in common — what a
//! clip is, how one is added and stopped, what a refresh does — is here, so
//! the two cannot drift apart.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::clock::{Monotonic, SharedClock};
use crate::color::parse_hex_color;
use crate::compositor::Compositor;
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::geometry::{Fit, Rect};
use crate::output::Output;
use crate::output_layer::OutputLayer;
use crate::placement::Monitor;
use crate::render::{Gpu, Renderer};
use crate::video_source::{FrameCache, VideoSource};

/// What `start` takes for each monitor: `(width, height, x, y)` in physical
/// pixels, `x` and `y` being the output's top-left corner *relative to that
/// monitor's own top-left*. `(1920, 1080, 0, 0)` is the whole of a 1080p
/// monitor, wherever it sits in the desktop.
pub type OutputRect = (i64, i64, i64, i64);

/// Where the engine's time comes from: whether video follows sound.
///
/// Every clip runs against one timeline (see [`crate::clock`]). With audio,
/// that timeline is the audio engine's output clock — what the speakers are
/// playing, latency included — so pictures stay on the sound and never the
/// other way round. Without, it is a monotonic clock of vtome's own.
///
/// ```no_run
/// # #[cfg(feature = "atome")]
/// # fn demo(output: &atome::output::OutputClass<f32>) {
/// use vtome::Audio;
///
/// let audio = Audio::atome(output);
/// # let _ = audio;
/// # }
/// ```
///
/// vtome never opens an audio device itself; the application owns atome and
/// hands over a handle to one of its outputs.
#[derive(Clone, Default)]
pub enum Audio {
    /// No audio: a monotonic clock of vtome's own, and no soundtracks.
    #[default]
    Off,
    /// Follow this master, playing no sound — any [`MasterClock`](crate::MasterClock).
    Follow(SharedClock),
    /// Follow an atome output's clock *and* play every video clip's
    /// soundtrack through it, in step with the pictures. Made with
    /// [`Audio::atome`].
    #[cfg(feature = "atome")]
    Atome(atome::output::Scheduler<f32>),
}

impl Audio {
    /// Video on atome's clock, with every video clip's sound played through
    /// `output`: its own audio track, or the file given with
    /// [`Clip::audio`] when the film has none.
    #[cfg(feature = "atome")]
    pub fn atome(output: &atome::output::OutputClass<f32>) -> Self {
        Audio::Atome(output.scheduler())
    }

    /// Whether video follows an audio clock.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Audio::Off)
    }

    /// The timeline every clip will run against.
    pub(crate) fn timeline(&self) -> SharedClock {
        match self {
            Audio::Off => Monotonic::shared(),
            Audio::Follow(clock) => Arc::clone(clock),
            #[cfg(feature = "atome")]
            Audio::Atome(scheduler) => Arc::new(scheduler.clock()),
        }
    }
}

impl std::fmt::Debug for Audio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Audio::Off => formatter.write_str("Audio::Off"),
            Audio::Follow(clock) => formatter
                .debug_struct("Audio::Follow")
                .field("position", &clock.position())
                .field("running", &clock.is_running())
                .finish(),
            #[cfg(feature = "atome")]
            Audio::Atome(scheduler) => formatter
                .debug_struct("Audio::Atome")
                .field("channels", &scheduler.channels())
                .field("sample_rate", &scheduler.sample_rate())
                .finish(),
        }
    }
}

/// A clip's handle: what [`Controls::stop`] takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClipId(pub u64);

impl std::fmt::Display for ClipId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "clip {}", self.0)
    }
}

impl ClipId {
    /// The compositor source id this clip lives under.
    fn key(self) -> String {
        format!("clip-{}", self.0)
    }
}

/// How long an image stays on screen.
///
/// ```no_run
/// # use std::time::Duration;
/// # fn demo(controls: vtome::Controls, monitor: &str) -> vtome::Result<()> {
/// use vtome::Hold;
///
/// controls.add_image("logo.png", monitor, Hold::Frames(90))?;
/// controls.add_image("slate.png", monitor, Hold::Time(Duration::from_secs(5)))?;
/// let bug = controls.add_image("bug.png", monitor, Hold::Forever)?;
/// controls.stop(bug);
/// # Ok(()) }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hold {
    /// Until [`stop`](Controls::stop)ped.
    #[default]
    Forever,
    /// Exactly this many of the engine's frames, then gone.
    Frames(u64),
    /// This long, counted as the engine's frames — `time × frame rate`,
    /// rounded up — so it ends on a frame boundary, in step with everything
    /// else on the output.
    Time(Duration),
}

/// Something to put on a monitor: a video, an image, or a frame already in
/// hand, with where and how.
///
/// ```no_run
/// # use std::time::Duration;
/// use vtome::geometry::Rect;
/// use vtome::{Clip, Hold};
///
/// let intro = Clip::video("intro.mp4", "monitor_342b0e446031e910");
/// let lower_third = Clip::image("name.png", "monitor_342b0e446031e910")
///     .area(Rect::new(80.0, 860.0, 900.0, 160.0))
///     .hold(Hold::Time(Duration::from_secs(8)));
/// ```
#[derive(Clone, Debug)]
pub struct Clip {
    media: Media,
    monitor: String,
    area: Option<Rect>,
    fit: Fit,
    opacity: f32,
    z_index: i32,
    hold: Hold,
    looping: bool,
    start_at: Option<Duration>,
    audio: Option<PathBuf>,
}

#[derive(Clone, Debug)]
enum Media {
    Video(PathBuf),
    Image(PathBuf),
    Frame(Frame),
}

impl Clip {
    fn with(media: Media, monitor: impl Into<String>) -> Self {
        Clip {
            media,
            monitor: monitor.into(),
            area: None,
            fit: Fit::Contain,
            opacity: 1.0,
            z_index: 0,
            hold: Hold::Forever,
            looping: false,
            start_at: None,
            audio: None,
        }
    }

    /// A video file, played once unless [`looping`](Clip::looping).
    pub fn video(path: impl Into<PathBuf>, monitor: impl Into<String>) -> Self {
        Clip::with(Media::Video(path.into()), monitor)
    }

    /// An image file, shown until stopped unless given a
    /// [`hold`](Clip::hold).
    pub fn image(path: impl Into<PathBuf>, monitor: impl Into<String>) -> Self {
        Clip::with(Media::Image(path.into()), monitor)
    }

    /// A frame already decoded, shown like an image.
    pub fn frame(frame: Frame, monitor: impl Into<String>) -> Self {
        Clip::with(Media::Frame(frame), monitor)
    }

    /// Where on the monitor's output, in its pixels. The whole output by
    /// default.
    pub fn area(mut self, area: Rect) -> Self {
        self.area = Some(area);
        self
    }

    /// How the picture sits inside the area. [`Fit::Contain`] by default.
    pub fn fit(mut self, fit: Fit) -> Self {
        self.fit = fit;
        self
    }

    /// 0.0 to 1.0.
    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// Stacking order: 0 is on top, as for
    /// [`OutputLayer`]. Clips with equal
    /// z-index stack in the order they were added, the newest on top.
    pub fn z_index(mut self, z_index: i32) -> Self {
        self.z_index = z_index;
        self
    }

    /// How long an image stays up: a number of frames, a length of time, or
    /// until stopped (the default). A video ignores this and runs its own
    /// length.
    pub fn hold(mut self, hold: Hold) -> Self {
        self.hold = hold;
        self
    }

    /// Shorthand for [`hold`](Clip::hold)`(Hold::Time(duration))`.
    pub fn duration(self, duration: Duration) -> Self {
        self.hold(Hold::Time(duration))
    }

    /// Whether a video starts again when it ends. Off by default: a video
    /// plays once and then its clip is finished.
    pub fn looping(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }

    /// Puts a video's first frame on screen exactly when the engine's
    /// timeline reaches `position`, and nothing before then. By default a
    /// video starts as soon as its first picture is up.
    ///
    /// With [`Audio::atome`] the timeline is atome's output clock, whose
    /// position is the mixer's index over channels over sample rate — so a
    /// sound scheduled at interleaved index `i` and a clip started at
    /// `i / channels / rate` seconds begin on the same frame. Read where the
    /// timeline is now with [`Controls::position`]. Stills ignore this.
    pub fn start_at(mut self, position: Duration) -> Self {
        self.start_at = Some(position);
        self
    }

    /// The sound for a video that has none of its own — the FLAC `import`
    /// split out, say. A film with its own audio track plays that instead,
    /// and this is not used. Heard only with [`Audio::atome`]; stills ignore
    /// it.
    pub fn audio(mut self, path: impl Into<PathBuf>) -> Self {
        self.audio = Some(path.into());
        self
    }
}

/// A clip that finished on its own: it ended, ran out its frames, or failed.
#[derive(Clone, Debug)]
pub struct ClipEnded {
    /// Which clip.
    pub id: ClipId,
    /// Why, if it failed rather than ended.
    pub error: Option<String>,
}

/// What a clip becomes once its file is open: ready for the engine thread.
pub(crate) struct Prepared {
    content: Content,
    monitor: String,
    area: Option<Rect>,
    fit: Fit,
    opacity: f32,
    z_index: i32,
    looping: bool,
    start_at: Option<Duration>,
    /// Where a video's sound comes from: the film itself, or the file it was
    /// given. `None` for a still, or a silent film.
    #[cfg_attr(not(feature = "atome"), allow(dead_code))]
    soundtrack: Option<PathBuf>,
    /// How long the film runs, for a looping soundtrack.
    #[cfg_attr(not(feature = "atome"), allow(dead_code))]
    duration: Duration,
}

enum Content {
    Video(VideoSource),
    Still { frame: Frame, frames: Option<u64> },
}

pub(crate) enum Command {
    Add(ClipId, Box<Prepared>),
    Stop(ClipId),
    Snapshot(String, Sender<Result<Frame>>),
    SnapshotClip(ClipId, Sender<Result<Frame>>),
    Close,
}

/// State the engine thread and every [`Controls`] share.
#[derive(Default)]
pub(crate) struct Shared {
    next_id: AtomicU64,
    /// The monitors outputs are running on, once started.
    running: Mutex<Option<HashSet<String>>>,
    /// Monitors `start` was given but could not use, and why.
    skipped: Mutex<Vec<(String, String)>>,
    active: Mutex<HashSet<ClipId>>,
    /// Clips that ended, failed, or were stopped. A clip id is never reused,
    /// so this only grows — by eight bytes a clip.
    finished: Mutex<HashSet<ClipId>>,
    ended: Mutex<Vec<ClipEnded>>,
    closed: AtomicBool,
    /// Refreshes the engine has run.
    frames: AtomicU64,
    /// The timeline every clip runs against, once started.
    timeline: Mutex<Option<SharedClock>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Adds and stops clips from any thread, while the engine runs on its own.
///
/// Cheap to clone; every clone drives the same engine. Opening a clip's file —
/// the decoder, the image — happens in the calling thread, so a file that
/// will not open is an error right here rather than a silent gap on screen.
#[derive(Clone)]
pub struct Controls {
    commands: Sender<Command>,
    shared: Arc<Shared>,
    frame_rate: f64,
}

impl Controls {
    pub(crate) fn new(commands: Sender<Command>, shared: Arc<Shared>, frame_rate: f64) -> Self {
        Controls {
            commands,
            shared,
            frame_rate,
        }
    }

    /// Opens a clip and puts it on screen from the next frame. Returns the id
    /// to [`stop`](Controls::stop) it with.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchMonitor`] if the engine is running and has no output on
    /// that monitor; whatever opening the video or image refuses; and
    /// [`Error::Unsupported`] once the engine has closed.
    pub fn add(&self, clip: Clip) -> Result<ClipId> {
        self.add_with(clip, None)
    }

    /// Puts any file vtome reads on a monitor's whole output, deciding from
    /// its *content* what it is: a still is shown until stopped, a video plays
    /// once. [`add`](Controls::add) with a [`Clip`] says which, and how.
    ///
    /// # Errors
    ///
    /// As [`add`](Controls::add), and whatever identifying the file refuses.
    #[cfg(feature = "demux")]
    pub fn add_generic(
        &self,
        path: impl Into<PathBuf>,
        monitor: impl Into<String>,
    ) -> Result<ClipId> {
        let path = path.into();
        let container = crate::identify::identify_path(&path)?;

        let clip = if container.is_image() {
            Clip::image(path, monitor)
        } else {
            Clip::video(path, monitor)
        };

        self.add(clip)
    }

    /// [`add`](Controls::add) for a video with ranges of it already decoded:
    /// those frames are served from memory and the decoder runs only outside
    /// them — an instant start, a loop that never waits on a keyframe.
    ///
    /// `ranges` are frame numbers, start included and end not, counted from
    /// the first frame at the file's frame rate; `frames` are those ranges'
    /// frames end to end, as [`cache_frames`](crate::cache_frames) makes them.
    ///
    /// # Errors
    ///
    /// As [`add`](Controls::add); [`Error::Unsupported`] for a clip that is
    /// not a video, a file with no frame rate, or ranges and frames that do
    /// not add up.
    pub fn add_cached(
        &self,
        clip: Clip,
        ranges: Vec<(u64, u64)>,
        frames: Vec<Frame>,
    ) -> Result<ClipId> {
        if !matches!(clip.media, Media::Video(_)) {
            return Err(Error::unsupported(
                "only a video has frames to cache; this clip is a still",
            ));
        }

        let cache = FrameCache::new(ranges, frames)?;
        self.add_with(clip, Some(cache))
    }

    fn add_with(&self, clip: Clip, cache: Option<FrameCache>) -> Result<ClipId> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(Error::unsupported("the engine has closed"));
        }

        if let Some(running) = lock(&self.shared.running).as_ref() {
            if !running.contains(&clip.monitor) {
                return Err(Error::NoSuchMonitor {
                    selector: clip.monitor.clone(),
                    available: running.len(),
                });
            }
        }

        let mut soundtrack = None;
        let mut duration = Duration::ZERO;

        let content = match clip.media {
            Media::Video(path) => {
                let mut source = VideoSource::from_file(&path)?;

                // The film's own sound if it has any; otherwise the file
                // given for it, if one was.
                soundtrack = if source.info().has_audio() {
                    Some(path)
                } else {
                    clip.audio
                };
                duration = source.info().duration;

                if let Some(cache) = cache {
                    source.set_cache(cache)?;
                }

                Content::Video(source)
            }
            Media::Image(path) => Content::Still {
                frames: self.frames_for(clip.hold)?,
                frame: crate::still::load_image(path)?,
            },
            Media::Frame(frame) => Content::Still {
                frames: self.frames_for(clip.hold)?,
                frame,
            },
        };

        let id = ClipId(self.shared.next_id.fetch_add(1, Ordering::Relaxed));
        lock(&self.shared.active).insert(id);

        let prepared = Prepared {
            content,
            monitor: clip.monitor,
            area: clip.area,
            fit: clip.fit,
            opacity: clip.opacity,
            z_index: clip.z_index,
            looping: clip.looping,
            start_at: clip.start_at,
            soundtrack,
            duration,
        };

        self.commands
            .send(Command::Add(id, Box::new(prepared)))
            .map_err(|_| Error::unsupported("the engine has closed"))?;

        Ok(id)
    }

    /// Puts an image file on a monitor's whole output for as long as `hold`
    /// says: a number of frames, a length of time, or until
    /// [`stop`](Controls::stop)ped. [`add`](Controls::add) with
    /// [`Clip::image`] does the same with an area, a fit, an opacity, or a
    /// z-index as well.
    ///
    /// # Errors
    ///
    /// As [`add`](Controls::add), and [`Error::Unsupported`] for a hold of no
    /// frames or no time — an image nobody would ever see.
    pub fn add_image(
        &self,
        path: impl Into<PathBuf>,
        monitor: impl Into<String>,
        hold: Hold,
    ) -> Result<ClipId> {
        self.add(Clip::image(path, monitor).hold(hold))
    }

    /// [`add_image`](Controls::add_image) for a frame already in hand — one
    /// rendered, decoded, or generated rather than read from a file.
    ///
    /// # Errors
    ///
    /// As [`add_image`](Controls::add_image).
    pub fn add_frame(&self, frame: Frame, monitor: impl Into<String>, hold: Hold) -> Result<ClipId> {
        self.add(Clip::frame(frame, monitor).hold(hold))
    }

    /// Takes a clip off screen. Whether it was still running — `false` for a
    /// clip that already ended, or was never added.
    pub fn stop(&self, id: ClipId) -> bool {
        if !lock(&self.shared.active).remove(&id) {
            return false;
        }

        lock(&self.shared.finished).insert(id);
        self.commands.send(Command::Stop(id)).is_ok()
    }

    /// Whether a clip is on screen, or about to be: added, and not yet ended,
    /// failed, or stopped.
    pub fn is_running(&self, id: ClipId) -> bool {
        lock(&self.shared.active).contains(&id)
    }

    /// Whether a clip is over: it ended, failed, or was stopped. `false` for
    /// one still running, and for an id this engine never handed out — the
    /// two questions are not each other's opposite.
    pub fn is_finished(&self, id: ClipId) -> bool {
        lock(&self.shared.finished).contains(&id)
    }

    /// Where the engine's timeline is now — atome's output clock with
    /// [`Audio::atome`] — for [`Clip::start_at`]. Zero until started.
    pub fn position(&self) -> Duration {
        lock(&self.shared.timeline)
            .as_ref()
            .map_or(Duration::ZERO, |timeline| timeline.position())
    }

    /// Clips that finished on their own since the last call, oldest first.
    pub fn take_ended(&self) -> Vec<ClipEnded> {
        std::mem::take(&mut *lock(&self.shared.ended))
    }

    /// The monitors outputs are running on. Empty until started.
    pub fn running_monitors(&self) -> Vec<String> {
        let mut monitors: Vec<String> = lock(&self.shared.running)
            .iter()
            .flatten()
            .cloned()
            .collect();
        monitors.sort();
        monitors
    }

    /// Monitors `start` was handed but did not use, and why: excluded, not
    /// attached, or an output rectangle with no area.
    pub fn skipped_monitors(&self) -> Vec<(String, String)> {
        lock(&self.shared.skipped).clone()
    }

    /// The picture a clip is showing right now: a video's current frame, as
    /// decoded (usually NV12, with its colour space), or a still's.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a clip that is not running, or a video whose
    /// first frame is not up yet; [`Error::Render`] if the engine is not
    /// running or does not answer within a second.
    pub fn snapshot(&self, id: ClipId) -> Result<Frame> {
        self.ask(|reply| Command::SnapshotClip(id, reply))
    }

    /// What `monitor`'s whole output shows right now, as an RGBA frame —
    /// premultiplied, as it was composited. For a preview in a control
    /// application, a thumbnail, or a test.
    ///
    /// Waits for the engine to draw it, which is at most a frame or two.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchMonitor`] if no output runs there; [`Error::Render`] if
    /// the engine is not running or does not answer within a second.
    pub fn snapshot_output(&self, monitor: &str) -> Result<Frame> {
        let monitor = monitor.to_string();
        self.ask(|reply| Command::Snapshot(monitor, reply))
    }

    /// Sends a command that answers, and waits a second for the answer.
    fn ask(&self, command: impl FnOnce(Sender<Result<Frame>>) -> Command) -> Result<Frame> {
        let (reply, answer) = std::sync::mpsc::channel();

        self.commands
            .send(command(reply))
            .map_err(|_| Error::unsupported("the engine has closed"))?;

        answer
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| Error::Render {
                reason: "the engine did not answer — is it running?".to_string(),
            })?
    }

    /// Closes every output and ends the engine.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Close);
    }

    /// How many frames the engine has drawn since it started. Two readings a
    /// second apart are its real frame rate — which is what a hold of so
    /// many frames is counted in.
    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// Whether the engine has closed.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    /// How many refreshes a hold lasts, or `None` for until stopped.
    fn frames_for(&self, hold: Hold) -> Result<Option<u64>> {
        match hold {
            Hold::Forever => Ok(None),
            Hold::Frames(0) => Err(Error::unsupported(
                "an image held for 0 frames would never be on screen",
            )),
            Hold::Frames(frames) => Ok(Some(frames)),
            Hold::Time(duration) if duration.is_zero() => Err(Error::unsupported(
                "an image held for no time would never be on screen",
            )),
            Hold::Time(duration) => Ok(Some(self.frames_in(duration))),
        }
    }

    /// `duration × frame rate`, rounded up, and never less than one frame.
    fn frames_in(&self, duration: Duration) -> u64 {
        ((duration.as_secs_f64() * self.frame_rate).ceil() as u64).max(1)
    }
}

/// One monitor's output, before its window exists: where it goes on the
/// desktop and what it shows behind every clip.
#[derive(Clone)]
pub(crate) struct Planned {
    pub(crate) monitor: String,
    /// In desktop physical pixels.
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    background: Background,
}

#[derive(Clone)]
enum Background {
    Color([f32; 4]),
    Image(String),
}

/// Works out which outputs to open: the requested monitors that are attached
/// and not excluded, each with its background. Anything left out is recorded
/// in `shared`, with the reason.
///
/// # Errors
///
/// A background that is neither a hex colour nor an image that loads; and
/// [`Error::NoSuchMonitor`] if nothing at all is left to open.
pub(crate) fn plan(
    outputs: &HashMap<String, OutputRect>,
    backgrounds: &HashMap<String, String>,
    excluded: &HashSet<String>,
    attached: &[Monitor],
    shared: &Shared,
) -> Result<Vec<Planned>> {
    let mut planned = Vec::new();
    let mut skipped = Vec::new();

    // Sorted, so windows open in the same order every run.
    let mut requested: Vec<(&String, &OutputRect)> = outputs.iter().collect();
    requested.sort();

    for (id, &(width, height, x, y)) in requested {
        if excluded.contains(id) {
            skipped.push((id.clone(), "excluded".to_string()));
            continue;
        }

        let Some(monitor) = attached.iter().find(|monitor| &monitor.persistent_id == id) else {
            skipped.push((id.clone(), "not attached".to_string()));
            continue;
        };

        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            skipped.push((id.clone(), format!("{width}×{height} is not a size")));
            continue;
        };

        if width == 0 || height == 0 {
            skipped.push((id.clone(), format!("{width}×{height} has no area")));
            continue;
        }

        let to_desktop = |origin: f64, offset: i64| i32::try_from(origin as i64 + offset).ok();

        let (Some(x), Some(y)) = (
            to_desktop(monitor.bounds.x, x),
            to_desktop(monitor.bounds.y, y),
        ) else {
            skipped.push((id.clone(), "the position is off the desktop".to_string()));
            continue;
        };

        let background = match backgrounds.get(id).map(|text| text.trim()) {
            None | Some("") => Background::Color([0.0; 4]),
            Some(color) if color.starts_with('#') => Background::Color(parse_hex_color(color)?),
            // Loaded once here only to say now, rather than on the first
            // frame, that the file is missing or is not an image.
            Some(path) => {
                crate::still::load_image(path)?;
                Background::Image(path.to_string())
            }
        };

        planned.push(Planned {
            monitor: id.clone(),
            x,
            y,
            width,
            height,
            background,
        });
    }

    *lock(&shared.skipped) = skipped;

    if planned.is_empty() {
        return Err(Error::NoSuchMonitor {
            selector: format!(
                "any of the {} monitor{} given, once excluded and unattached ones are left out",
                outputs.len(),
                if outputs.len() == 1 { "" } else { "s" }
            ),
            available: attached.len(),
        });
    }

    *lock(&shared.running) = Some(planned.iter().map(|plan| plan.monitor.clone()).collect());

    Ok(planned)
}

/// One output's window and the surface drawn into it.
pub(crate) struct Screen<W> {
    pub(crate) monitor: String,
    output: usize,
    pub(crate) window: W,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

impl<W> Screen<W> {
    /// Wraps a window whose surface has been made, configuring it — on the
    /// thread that owns the window, which on macOS is the main thread.
    pub(crate) fn new(
        plan: &Planned,
        window: W,
        surface: wgpu::Surface<'static>,
        gpu: &Gpu,
    ) -> Self {
        let capabilities = surface.get_capabilities(&gpu.adapter);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: crate::render::surface_format(&capabilities),
            color_space: wgpu::SurfaceColorSpace::Srgb,
            width: plan.width,
            height: plan.height,
            // Paced by the engine's own clock, at its frame rate; vsync keeps
            // each present tear-free without the engine waiting on it.
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: crate::render::see_through(&capabilities),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        surface.configure(&gpu.device, &config);

        Screen {
            monitor: plan.monitor.clone(),
            output: usize::MAX,
            window,
            surface,
            config,
        }
    }

    /// The window, once its surface has been dropped — the order a teardown
    /// needs, since a surface must not outlive what it draws into.
    #[cfg_attr(not(feature = "tauri"), allow(dead_code))]
    pub(crate) fn into_window(self) -> W {
        drop(self.surface);
        self.window
    }
}

/// The engine: outputs, clips, and the GPU they are drawn with.
pub(crate) struct Core<W> {
    compositor: Compositor,
    gpu: Gpu,
    renderers: HashMap<wgpu::TextureFormat, Renderer>,
    screens: Vec<Screen<W>>,
    commands: Receiver<Command>,
    shared: Arc<Shared>,
    #[cfg_attr(not(feature = "atome"), allow(dead_code))]
    audio: Audio,
    /// Soundtracks playing, by clip; dropping one stops it.
    #[cfg(feature = "atome")]
    soundtracks: HashMap<ClipId, crate::audio::Soundtrack>,
}

/// How far ahead a clip with sound is started, so its first samples can be
/// scheduled before they are due and the first picture decoded in time.
#[cfg(feature = "atome")]
const PREROLL: Duration = Duration::from_millis(250);

impl<W> Core<W> {
    /// An engine over screens already open, one output each.
    pub(crate) fn new(
        gpu: Gpu,
        plans: &[Planned],
        mut screens: Vec<Screen<W>>,
        commands: Receiver<Command>,
        shared: Arc<Shared>,
        audio: &Audio,
    ) -> Result<Self> {
        let timeline = audio.timeline();
        *lock(&shared.timeline) = Some(Arc::clone(&timeline));

        let mut compositor = Compositor::with_clock(timeline);
        let mut renderers = HashMap::new();

        for screen in &mut screens {
            let plan = plans
                .iter()
                .find(|plan| plan.monitor == screen.monitor)
                .ok_or_else(|| Error::unsupported("a screen with no plan behind it"))?;

            let output = Output::new(plan.width, plan.height);
            let output = match &plan.background {
                Background::Color(color) => output.with_background_color(*color),
                Background::Image(path) => output
                    .with_transparent_background()
                    .with_background_image(path.clone()),
            };

            screen.output = compositor.add_output(output);

            if let std::collections::hash_map::Entry::Vacant(slot) =
                renderers.entry(screen.config.format)
            {
                slot.insert(Renderer::new(&gpu, screen.config.format)?);
            }
        }

        Ok(Core {
            compositor,
            gpu,
            renderers,
            screens,
            commands,
            shared,
            audio: audio.clone(),
            #[cfg(feature = "atome")]
            soundtracks: HashMap::new(),
        })
    }

    /// One frame: take commands, move every clip on, draw every output, and
    /// present. `false` once the engine has been told to close.
    pub(crate) fn refresh(&mut self) -> Result<bool> {
        // Commands first, so a clip added before this frame is in it.
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Add(id, prepared) => self.place(id, *prepared),
                Command::Stop(id) => {
                    self.compositor.remove_source(&id.key());
                    #[cfg(feature = "atome")]
                    self.soundtracks.remove(&id);
                }
                Command::Snapshot(monitor, reply) => {
                    let _ = reply.send(self.snapshot(&monitor));
                }
                Command::SnapshotClip(id, reply) => {
                    let _ = reply.send(self.snapshot_clip(id));
                }
                Command::Close => return Ok(false),
            }
        }

        for finished in self.compositor.tick() {
            if let Some(id) = parse_key(&finished.id) {
                self.finish(id, finished.error.map(|error| error.to_string()));
            }
        }

        for screen in &mut self.screens {
            use wgpu::CurrentSurfaceTexture;

            let texture = match screen.surface.get_current_texture() {
                CurrentSurfaceTexture::Success(texture)
                | CurrentSurfaceTexture::Suboptimal(texture) => texture,

                // A monitor change or a sleeping display: configure again and
                // catch this screen up on the next frame.
                CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                    screen.surface.configure(&self.gpu.device, &screen.config);
                    continue;
                }

                CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => continue,

                other => {
                    return Err(Error::Render {
                        reason: format!("no surface texture for {}: {other:?}", screen.monitor),
                    })
                }
            };

            let view = texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let renderer = &self.renderers[&screen.config.format];

            self.compositor.draw_output(
                screen.output,
                &self.gpu,
                renderer,
                &view,
                screen.config.width,
                screen.config.height,
            )?;

            self.gpu.queue.present(texture);
        }

        self.compositor.presented();
        self.shared.frames.fetch_add(1, Ordering::Relaxed);

        Ok(true)
    }

    /// Puts a prepared clip on its monitor's output.
    fn place(&mut self, id: ClipId, prepared: Prepared) {
        let Some(screen) = self
            .screens
            .iter()
            .find(|screen| screen.monitor == prepared.monitor)
        else {
            // Added before `start`, for a monitor that did not open.
            self.finish(
                id,
                Some(format!("no output is running on {}", prepared.monitor)),
            );
            return;
        };

        let output = screen.output;
        let key = id.key();

        let start_at = match prepared.content {
            Content::Video(_) => self.soundtrack(id, &prepared).or(prepared.start_at),
            Content::Still { .. } => None,
        };

        match prepared.content {
            Content::Video(source) => {
                self.compositor
                    .insert_video_at(&key, source, prepared.looping, start_at);
            }
            Content::Still { frame, frames } => self.compositor.insert_still(&key, frame, frames),
        }

        if let Some(target) = self.compositor.get_output_mut(output) {
            let area = prepared.area.unwrap_or_else(|| {
                Rect::from_size(f64::from(target.width), f64::from(target.height))
            });

            target.add_layer(
                OutputLayer::new(key, area)
                    .with_fit(prepared.fit)
                    .with_z_index(prepared.z_index)
                    .with_opacity(prepared.opacity),
            );
        }
    }

    /// One output drawn offscreen, with the renderer its window uses — a
    /// picture belongs to the renderer that uploaded it.
    fn snapshot(&mut self, monitor: &str) -> Result<Frame> {
        let screen = self
            .screens
            .iter()
            .find(|screen| screen.monitor == monitor)
            .ok_or_else(|| Error::NoSuchMonitor {
                selector: monitor.to_string(),
                available: self.screens.len(),
            })?;

        let (width, height) = (screen.config.width, screen.config.height);
        let renderer = &self.renderers[&screen.config.format];

        let pixels = self
            .compositor
            .render_output_to_rgba(screen.output, &self.gpu, renderer, width, height)?;

        Frame::packed(
            width,
            height,
            crate::frame::PixelFormat::Rgba8,
            crate::color::ColorSpace::srgb(),
            Duration::ZERO,
            pixels,
        )
    }

    /// One clip's current picture.
    fn snapshot_clip(&self, id: ClipId) -> Result<Frame> {
        self.compositor
            .showing(&id.key())
            .cloned()
            .ok_or_else(|| {
                Error::unsupported(format!(
                    "{id} has no picture up: it is not running, or its first frame is not due yet"
                ))
            })
    }

    /// Starts a video clip's sound through atome, if it has any and the
    /// engine plays audio, and returns where on the timeline the pictures must
    /// start to stay with it: the clip's own start, or a moment from now.
    #[cfg(feature = "atome")]
    fn soundtrack(&mut self, id: ClipId, prepared: &Prepared) -> Option<Duration> {
        let Audio::Atome(scheduler) = &self.audio else {
            return None;
        };
        let path = prepared.soundtrack.as_ref()?;

        let at = prepared
            .start_at
            .unwrap_or_else(|| self.compositor.clock().position() + PREROLL);
        let period = (prepared.looping && !prepared.duration.is_zero()).then_some(prepared.duration);

        match crate::audio::Soundtrack::start(path, scheduler.clone(), at, period) {
            Ok(soundtrack) => {
                self.soundtracks.insert(id, soundtrack);
            }
            // The pictures still play; the reason is reported, not swallowed.
            Err(error) => lock(&self.shared.ended).push(ClipEnded {
                id,
                error: Some(format!("playing without sound: {error}")),
            }),
        }

        Some(at)
    }

    #[cfg(not(feature = "atome"))]
    fn soundtrack(&mut self, _id: ClipId, _prepared: &Prepared) -> Option<Duration> {
        None
    }

    fn finish(&mut self, id: ClipId, error: Option<String>) {
        #[cfg(feature = "atome")]
        self.soundtracks.remove(&id);
        lock(&self.shared.active).remove(&id);
        lock(&self.shared.finished).insert(id);
        lock(&self.shared.ended).push(ClipEnded { id, error });
    }

    /// The screens, for closing their windows once the engine stops.
    #[cfg_attr(not(feature = "tauri"), allow(dead_code))]
    pub(crate) fn into_screens(self) -> Vec<Screen<W>> {
        self.screens
    }
}

/// The clip a compositor source id belongs to.
fn parse_key(key: &str) -> Option<ClipId> {
    key.strip_prefix("clip-")?.parse().ok().map(ClipId)
}

/// The channel and shared state for a new engine.
pub(crate) fn channel(frame_rate: f64) -> (Controls, Receiver<Command>, Arc<Shared>) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let shared = Arc::new(Shared::default());

    (
        Controls::new(sender, Arc::clone(&shared), frame_rate),
        receiver,
        shared,
    )
}

/// A frame rate made sane: at least one frame a second, at most 240.
pub(crate) fn sane_frame_rate(frame_rate: f64) -> f64 {
    if frame_rate.is_finite() {
        frame_rate.clamp(1.0, 240.0)
    } else {
        30.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: f64) -> Monitor {
        Monitor::new(name, Rect::new(x, 0.0, 1920.0, 1080.0))
    }

    #[test]
    fn a_plan_keeps_what_is_attached_and_not_excluded_and_says_why_for_the_rest() {
        let left = monitor("LEFT", 0.0);
        let right = monitor("RIGHT", 1920.0);
        let attached = vec![left.clone(), right.clone()];

        let outputs = HashMap::from([
            (left.persistent_id.clone(), (1920, 1080, 0, 0)),
            (right.persistent_id.clone(), (640, 360, 100, 50)),
            ("monitor_gone".to_string(), (1920, 1080, 0, 0)),
        ]);
        let backgrounds = HashMap::from([(right.persistent_id.clone(), "#FF000080".to_string())]);
        let excluded = HashSet::from([left.persistent_id.clone()]);
        let shared = Shared::default();

        let planned = plan(&outputs, &backgrounds, &excluded, &attached, &shared).unwrap();

        assert_eq!(planned.len(), 1);
        let only = &planned[0];
        assert_eq!(only.monitor, right.persistent_id);
        // Relative to the monitor, moved onto the desktop.
        assert_eq!((only.x, only.y, only.width, only.height), (2020, 50, 640, 360));
        assert!(matches!(only.background, Background::Color([1.0, 0.0, 0.0, _])));

        let skipped = lock(&shared.skipped).clone();
        assert!(skipped.contains(&(left.persistent_id.clone(), "excluded".to_string())));
        assert!(skipped.contains(&("monitor_gone".to_string(), "not attached".to_string())));
        assert_eq!(
            lock(&shared.running).clone().unwrap(),
            HashSet::from([right.persistent_id])
        );
    }

    #[test]
    fn nothing_left_to_open_is_an_error_rather_than_an_engine_with_no_screens() {
        let only = monitor("ONLY", 0.0);
        let outputs = HashMap::from([(only.persistent_id.clone(), (1920, 1080, 0, 0))]);
        let excluded = HashSet::from([only.persistent_id.clone()]);

        assert!(matches!(
            plan(&outputs, &HashMap::new(), &excluded, &[only], &Shared::default()),
            Err(Error::NoSuchMonitor { .. })
        ));
    }

    #[test]
    fn a_background_that_is_neither_colour_nor_image_is_refused_at_start() {
        let only = monitor("ONLY", 0.0);
        let outputs = HashMap::from([(only.persistent_id.clone(), (1920, 1080, 0, 0))]);
        let backgrounds =
            HashMap::from([(only.persistent_id.clone(), "#notacolour".to_string())]);

        assert!(plan(
            &outputs,
            &backgrounds,
            &HashSet::new(),
            std::slice::from_ref(&only),
            &Shared::default()
        )
        .is_err());

        let backgrounds =
            HashMap::from([(only.persistent_id.clone(), "/no/such/image.png".to_string())]);

        assert!(plan(&outputs, &backgrounds, &HashSet::new(), &[only], &Shared::default())
            .is_err());
    }

    #[test]
    fn an_image_duration_becomes_frames_at_the_engine_rate() {
        let (controls, _receiver, _shared) = channel(30.0);

        assert_eq!(controls.frames_in(Duration::from_secs(2)), 60);
        assert_eq!(controls.frames_in(Duration::from_millis(10)), 1, "never zero");
        assert_eq!(controls.frames_in(Duration::from_millis(1001)), 31, "rounded up");
    }

    fn two_by_two() -> Frame {
        Frame::packed(
            2,
            2,
            crate::PixelFormat::Rgba8,
            crate::ColorSpace::srgb(),
            Duration::ZERO,
            vec![255; 16],
        )
        .unwrap()
    }

    /// Frames, time, or forever — each arrives at the engine as the number of
    /// refreshes the compositor counts down.
    #[test]
    fn every_kind_of_hold_arrives_at_the_engine_as_frames() {
        let (controls, receiver, _shared) = channel(30.0);

        for (hold, expected) in [
            (Hold::Frames(90), Some(90)),
            (Hold::Time(Duration::from_secs(2)), Some(60)),
            (Hold::Time(Duration::from_millis(1001)), Some(31)),
            (Hold::Forever, None),
        ] {
            let id = controls.add_frame(two_by_two(), "any", hold).unwrap();
            assert!(controls.is_running(id));

            let Ok(Command::Add(sent, prepared)) = receiver.try_recv() else {
                panic!("{hold:?} never reached the engine");
            };
            let Content::Still { frames, .. } = prepared.content else {
                panic!("a frame should be a still");
            };

            assert_eq!(sent, id);
            assert_eq!(frames, expected, "{hold:?}");
        }
    }

    #[test]
    fn a_hold_of_nothing_is_refused_before_anything_is_queued() {
        let (controls, receiver, _shared) = channel(30.0);

        assert!(controls.add_frame(two_by_two(), "any", Hold::Frames(0)).is_err());
        assert!(controls
            .add_frame(two_by_two(), "any", Hold::Time(Duration::ZERO))
            .is_err());
        assert!(receiver.try_recv().is_err(), "a refused clip reached the engine");
    }

    #[test]
    fn an_image_that_will_not_open_is_an_error_at_add_rather_than_a_gap_on_screen() {
        let (controls, receiver, _shared) = channel(30.0);

        assert!(controls
            .add_image("/no/such/image.png", "any", Hold::Frames(10))
            .is_err());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn stopping_is_answered_honestly() {
        let (controls, _receiver, _shared) = channel(30.0);
        let frame = Frame::packed(
            2,
            2,
            crate::PixelFormat::Rgba8,
            crate::ColorSpace::srgb(),
            Duration::ZERO,
            vec![255; 16],
        )
        .unwrap();

        let id = controls.add(Clip::frame(frame, "any")).unwrap();

        assert!(controls.is_running(id));
        assert!(!controls.is_finished(id));
        assert!(controls.stop(id));
        assert!(!controls.stop(id), "already stopped");
        assert!(!controls.stop(ClipId(999)), "never existed");

        assert!(!controls.is_running(id) && controls.is_finished(id));
        // Never handed out: neither running nor finished.
        assert!(!controls.is_running(ClipId(999)) && !controls.is_finished(ClipId(999)));
    }

    /// Only a video has frames to cache, and a cache that does not add up is
    /// refused at the call — before a file is opened or a clip id spent.
    #[test]
    fn a_cache_is_checked_before_anything_is_queued() {
        let (controls, receiver, _shared) = channel(30.0);

        let still = controls.add_cached(Clip::frame(two_by_two(), "any"), vec![(0, 1)], vec![two_by_two()]);
        assert!(matches!(still, Err(Error::Unsupported { .. })));

        let short = controls.add_cached(Clip::video("/no/such/film.mp4", "any"), vec![(0, 3)], vec![two_by_two()]);
        assert!(matches!(short, Err(Error::Unsupported { .. })), "three frames promised, one given");

        assert!(receiver.try_recv().is_err());
    }

    /// `add_generic` decides by content: a PNG is a still, however it is named.
    #[test]
    fn a_generic_file_is_shown_as_what_it_is() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("not-a-video.mp4");
        // A PNG's bytes under an MP4's name.
        let png = directory.path().join("x.png");
        image::save_buffer(&png, &[255; 16], 2, 2, image::ExtendedColorType::Rgba8).unwrap();
        std::fs::copy(&png, &path).unwrap();

        let (controls, receiver, _shared) = channel(30.0);
        let id = controls.add_generic(&path, "any").unwrap();

        let Ok(Command::Add(sent, prepared)) = receiver.try_recv() else {
            panic!("the file never reached the engine");
        };
        assert_eq!(sent, id);
        assert!(matches!(prepared.content, Content::Still { frames: None, .. }), "until stopped");
    }

    /// The clock clips follow, before and after a start.
    #[test]
    fn the_engine_position_is_zero_until_a_timeline_exists() {
        let (controls, _receiver, shared) = channel(30.0);
        assert_eq!(controls.position(), Duration::ZERO);

        *lock(&shared.timeline) = Some(Audio::Off.timeline());
        std::thread::sleep(Duration::from_millis(5));
        assert!(controls.position() >= Duration::from_millis(5));
        assert!(!Audio::Off.is_enabled());
    }

    #[test]
    fn a_closed_engine_refuses_new_clips() {
        let (controls, _receiver, _shared) = channel(30.0);
        controls.close();

        assert!(controls.is_closed());
        assert!(controls.add(Clip::image("x.png", "any")).is_err());
    }

    #[test]
    fn frame_rates_are_kept_sane() {
        assert_eq!(sane_frame_rate(0.0), 1.0);
        assert_eq!(sane_frame_rate(1000.0), 240.0);
        assert_eq!(sane_frame_rate(f64::NAN), 30.0);
        assert_eq!(sane_frame_rate(29.97), 29.97);
    }
}
