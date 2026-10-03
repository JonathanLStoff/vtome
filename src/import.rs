//! `import`: a playable file at the output path now, the real one later — and
//! a queue, so importing a hundred files never means a hundred encoders.
//!
//! ```no_run
//! // H.264, always, unless AV1 is asked for; the audio split
//! // to FLAC through atome (the `split-audio` feature) when a path is given.
//! let job = vtome::import("camera/take-3.mov", "show/take-3.mp4", None::<&str>)?;
//!
//! // Polled, say, every 500 ms by a Tauri command:
//! for file in vtome::import_progress() {
//!     println!("{}: {:?} {:.0}%", file.output.display(), file.stage, file.fraction * 100.0);
//! }
//!
//! // `show/take-3.mp4` already plays — the proxy — and is replaced in one
//! // rename when the full-quality encode finishes.
//! vtome::import_queue().wait(job);
//! # Ok::<(), vtome::Error>(())
//! ```
//!
//! # What happens to one file
//!
//! 1. **At the call**: the input is opened and checked — a container vtome
//!    reads, a video track, something here to decode it and to encode what it
//!    becomes. A bad file is an error from `import`, not a failed job later.
//! 2. **Proxy** (proxy lane): a small, quick copy — no bigger than 640 pixels,
//!    low quality, the encoder's fastest settings, the same §15 spec — is
//!    written beside the output and renamed onto it. From here the
//!    application can use the output path.
//! 3. **Audio** (proxy lane), if asked: the soundtrack to FLAC through atome.
//! 4. **Final** (final lane): the full-quality file is encoded into a temp
//!    directory, then replaces the proxy in one rename — or copy-then-rename
//!    when the temp directory is on another volume — so the output path never
//!    holds half a file.
//!
//! # Why two lanes
//!
//! One lane would leave a newly imported file unusable behind an hour of
//! someone else's AV1 encode. Two — proxies and audio in one, final encodes in
//! the other — each with its own workers (one apiece by default), keep a new
//! file's proxy minutes away at worst while still never running more than two
//! encoders, however many files are waiting. A software encoder is also held
//! to half the machine's cores, so playback is never starved.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;

use crate::decode::{DecoderConfig, Hardware};
use crate::encode::Level;
use crate::error::{Error, Result};
use crate::identify::{Container, Encoding};
use crate::transcode::{self, Settings};

/// Job ids are unique in the process, not just in one queue: each job's temp
/// directory is named after its id, and two queues numbering from one would
/// share — and delete — each other's.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A file being imported: what [`ImportQueue::cancel`],
/// [`ImportQueue::wait`], and [`ImportQueue::job`] take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "import {}", self.0)
    }
}

/// How to import.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// What to write: `None` for H.264, the default; `Some(Encoding::Av1)`
    /// for the bundled rav1e, which vtome cannot yet play back on most
    /// machines.
    pub encoding: Option<Encoding>,
    /// Whether decoding and encoding may, must, or must not use hardware —
    /// for the proxy and the final file alike.
    pub hardware: Hardware,
    /// Write a proxy at the output path first. On by default; off, the output
    /// path stays empty until the final file lands.
    pub proxy: bool,
    /// The final file's quality, 0.0 to 1.0: §15's 0.68 by default.
    pub quality: f32,
    /// The H.264 level to declare: 4.1, scaling bigger pictures to fit.
    pub level: Level,
    /// Where the final file is encoded before it replaces the proxy. The
    /// system's temp directory by default.
    pub temp_dir: Option<PathBuf>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            encoding: None,
            hardware: Hardware::Prefer,
            proxy: true,
            quality: 0.68,
            level: Level::L4_1,
            temp_dir: None,
        }
    }
}

/// How big a queue is.
#[derive(Clone, Copy, Debug)]
pub struct QueueConfig {
    /// Workers writing proxies and splitting audio. One by default.
    pub proxy_workers: usize,
    /// Workers writing final files. One by default.
    pub final_workers: usize,
    /// Threads each software encoder may use; 0 for half the machine's cores.
    pub encoder_threads: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            proxy_workers: 1,
            final_workers: 1,
            encoder_threads: 0,
        }
    }
}

/// Where a file is in its import.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Waiting for a proxy-lane worker.
    Queued,
    /// The proxy is being written.
    Proxy,
    /// The soundtrack is being split out to FLAC.
    Audio,
    /// The proxy is at the output path; the final encode is waiting for a
    /// final-lane worker.
    WaitingForFinal,
    /// The final file is being encoded.
    Final,
    /// The final file is replacing the proxy.
    Replacing,
    /// Finished: the final file is at the output path.
    Done,
    /// Stopped by an error, which [`JobProgress::error`] holds. A proxy
    /// already written stays where it is.
    Failed,
    /// Stopped by [`ImportQueue::cancel`]. A proxy already written stays
    /// where it is.
    Cancelled,
}

impl Stage {
    /// Whether nothing more will happen to the file.
    pub fn is_over(self) -> bool {
        matches!(self, Stage::Done | Stage::Failed | Stage::Cancelled)
    }
}

/// One file's import, as of now.
#[derive(Clone, Debug, PartialEq)]
pub struct JobProgress {
    /// Which import.
    pub id: JobId,
    /// The file being imported.
    pub input: PathBuf,
    /// Where it is going.
    pub output: PathBuf,
    /// Where its audio is going, if anywhere.
    pub output_audio: Option<PathBuf>,
    /// What is happening to it.
    pub stage: Stage,
    /// The whole import, 0.0 to 1.0.
    pub fraction: f32,
    /// The current stage, 0.0 to 1.0.
    pub stage_fraction: f32,
    /// Pictures written in the current stage, and how many there will be
    /// where the file says.
    pub frames: (u64, Option<u64>),
    /// What is being written.
    pub encoding: Encoding,
    /// The container it goes into — which follows the encoding, whatever the
    /// output is called.
    pub container: Container,
    /// The size the final file is written at.
    pub size: (u32, u32),
    /// Whether the output path holds a playable file yet — the proxy, then
    /// the final one.
    pub output_ready: bool,
    /// Whether the output path holds the final file.
    pub final_ready: bool,
    /// Whether the last picture decoded came from hardware.
    pub decoder_hardware: Option<bool>,
    /// Whether the last picture encoded went through hardware.
    pub encoder_hardware: Option<bool>,
    /// Why it failed.
    pub error: Option<String>,
}

/// Imports from any thread, through one queue. Cheap to clone; every clone is
/// the same queue.
///
/// Most applications want the one behind [`import`] —
/// [`import_queue`] — rather than their own.
#[derive(Clone)]
pub struct ImportQueue {
    shared: Arc<Shared>,
    /// Shared by the handles and not the workers: when the last handle goes,
    /// this closes the queue.
    _owner: Arc<Owner>,
}

struct Shared {
    config: QueueConfig,
    jobs: Mutex<Jobs>,
    /// Signalled whenever a job's progress changes, for `wait`.
    changed: Condvar,
    /// Signalled when a lane gets work, or the queue closes, for the workers.
    work: Condvar,
    /// Workers, started on the first import.
    workers: Mutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
}

/// Closes the queue when the last [`ImportQueue`] handle is dropped: every
/// import still running stops at its next picture, and the workers end.
struct Owner(Arc<Shared>);

impl Drop for Owner {
    fn drop(&mut self) {
        let shared = &self.0;
        shared.closed.store(true, Ordering::Release);

        for job in lock(&shared.jobs).jobs.values() {
            job.cancel.store(true, Ordering::Relaxed);
        }

        shared.work.notify_all();
        shared.changed.notify_all();
    }
}

#[derive(Default)]
struct Jobs {
    jobs: HashMap<JobId, Job>,
    proxy_lane: VecDeque<JobId>,
    final_lane: VecDeque<JobId>,
}

struct Job {
    progress: JobProgress,
    options: ImportOptions,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Proxy,
    Final,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ImportQueue {
    /// A queue of its own. Its workers start with the first import and stop
    /// when the last clone is dropped, cancelling whatever is still running.
    pub fn new(config: QueueConfig) -> Self {
        let shared = Arc::new(Shared {
            config: QueueConfig {
                proxy_workers: config.proxy_workers.max(1),
                final_workers: config.final_workers.max(1),
                encoder_threads: config.encoder_threads,
            },
            jobs: Mutex::new(Jobs::default()),
            changed: Condvar::new(),
            work: Condvar::new(),
            workers: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        });

        ImportQueue {
            _owner: Arc::new(Owner(Arc::clone(&shared))),
            shared,
        }
    }

    /// Queues `input` to be written to `output`, with its audio split to
    /// `output_audio` if given, and returns at once.
    ///
    /// # Errors
    ///
    /// Before anything is queued: whatever opening `input` refuses; a file
    /// with no video, or none that decodes here ([`Error::NoDecoder`]);
    /// nothing here to write what was asked for ([`Error::NoEncoder`]); an
    /// output directory that does not exist; and audio asked of a file
    /// without any, or of a build without `split-audio`.
    pub fn import(
        &self,
        input: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
        output_audio: Option<PathBuf>,
        options: ImportOptions,
    ) -> Result<JobId> {
        let (input, output) = (input.into(), output.into());

        if self.shared.closed.load(Ordering::Acquire) {
            return Err(Error::unsupported("the import queue has closed"));
        }

        let (encoding, size) = check(&input, &output, output_audio.as_deref(), &options)?;
        let id = JobId(NEXT_ID.fetch_add(1, Ordering::Relaxed));

        let progress = JobProgress {
            id,
            input,
            output,
            output_audio: output_audio.clone(),
            stage: Stage::Queued,
            fraction: 0.0,
            stage_fraction: 0.0,
            frames: (0, None),
            encoding,
            container: crate::mux::container_for(encoding)?,
            size,
            output_ready: false,
            final_ready: false,
            decoder_hardware: None,
            encoder_hardware: None,
            error: None,
        };

        let first = if options.proxy || output_audio.is_some() {
            Lane::Proxy
        } else {
            Lane::Final
        };

        {
            let mut jobs = lock(&self.shared.jobs);

            jobs.jobs.insert(
                id,
                Job {
                    progress,
                    options,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );

            match first {
                Lane::Proxy => jobs.proxy_lane.push_back(id),
                Lane::Final => {
                    if let Some(job) = jobs.jobs.get_mut(&id) {
                        job.progress.stage = Stage::WaitingForFinal;
                    }
                    jobs.final_lane.push_back(id);
                }
            }
        }

        self.start_workers();
        self.shared.work.notify_all();
        self.shared.changed.notify_all();

        Ok(id)
    }

    /// Every file this queue has been given and not yet cleared, oldest
    /// first: what is waiting, what is being worked on, and what finished.
    pub fn progress(&self) -> Vec<JobProgress> {
        let jobs = lock(&self.shared.jobs);
        let mut all: Vec<JobProgress> = jobs.jobs.values().map(|job| job.progress.clone()).collect();
        all.sort_by_key(|progress| progress.id);
        all
    }

    /// One file's import.
    pub fn job(&self, id: JobId) -> Option<JobProgress> {
        lock(&self.shared.jobs)
            .jobs
            .get(&id)
            .map(|job| job.progress.clone())
    }

    /// Stops an import: dropped if it is waiting, stopped at the next picture
    /// if it is running. Temp files go; a proxy already at the output path
    /// stays. Whether there was an unfinished import to stop.
    pub fn cancel(&self, id: JobId) -> bool {
        let mut jobs = lock(&self.shared.jobs);

        let Some(job) = jobs.jobs.get_mut(&id) else {
            return false;
        };

        if job.progress.stage.is_over() {
            return false;
        }

        job.cancel.store(true, Ordering::Relaxed);

        // Waiting rather than running: nothing will pick it up, so it is
        // cancelled here.
        if matches!(job.progress.stage, Stage::Queued | Stage::WaitingForFinal) {
            job.progress.stage = Stage::Cancelled;
            jobs.proxy_lane.retain(|queued| *queued != id);
            jobs.final_lane.retain(|queued| *queued != id);
        }

        drop(jobs);
        self.shared.changed.notify_all();
        true
    }

    /// Blocks until an import is over — done, failed, or cancelled — and
    /// returns how it ended. `None` for an id this queue never handed out.
    pub fn wait(&self, id: JobId) -> Option<JobProgress> {
        let mut jobs = lock(&self.shared.jobs);

        loop {
            let progress = jobs.jobs.get(&id)?.progress.clone();

            if progress.stage.is_over() {
                return Some(progress);
            }

            jobs = self
                .shared
                .changed
                .wait(jobs)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Blocks until every import given so far is over.
    pub fn wait_all(&self) {
        let ids: Vec<JobId> = lock(&self.shared.jobs).jobs.keys().copied().collect();

        for id in ids {
            self.wait(id);
        }
    }

    /// Forgets the imports that are over, and returns them.
    pub fn clear_finished(&self) -> Vec<JobProgress> {
        let mut jobs = lock(&self.shared.jobs);
        let over: Vec<JobId> = jobs
            .jobs
            .iter()
            .filter(|(_, job)| job.progress.stage.is_over())
            .map(|(id, _)| *id)
            .collect();

        let mut cleared: Vec<JobProgress> = over
            .into_iter()
            .filter_map(|id| jobs.jobs.remove(&id))
            .map(|job| job.progress)
            .collect();
        cleared.sort_by_key(|progress| progress.id);
        cleared
    }

    fn start_workers(&self) {
        let mut workers = lock(&self.shared.workers);

        if !workers.is_empty() {
            return;
        }

        let lanes = std::iter::repeat_n(Lane::Proxy, self.shared.config.proxy_workers)
            .chain(std::iter::repeat_n(Lane::Final, self.shared.config.final_workers));

        for (index, lane) in lanes.enumerate() {
            let shared = Arc::clone(&self.shared);
            let name = match lane {
                Lane::Proxy => format!("vtome-import-proxy-{index}"),
                Lane::Final => format!("vtome-import-final-{index}"),
            };

            if let Ok(handle) = std::thread::Builder::new()
                .name(name)
                .spawn(move || work(&shared, lane))
            {
                workers.push(handle);
            }
        }
    }
}

/// The queue [`import`] uses: one proxy worker, one final worker. Started on
/// first use and kept for the life of the process.
pub fn import_queue() -> &'static ImportQueue {
    static QUEUE: OnceLock<ImportQueue> = OnceLock::new();
    QUEUE.get_or_init(|| ImportQueue::new(QueueConfig::default()))
}

/// Imports `input` to `output` through [`import_queue`], with the default
/// [`ImportOptions`]: a proxy at `output` first, then the final file in
/// H.264 — and the audio to
/// `output_audio` as FLAC if given. Returns at once.
///
/// # Errors
///
/// As [`ImportQueue::import`].
pub fn import(
    input: impl Into<PathBuf>,
    output: impl Into<PathBuf>,
    output_audio: Option<impl Into<PathBuf>>,
) -> Result<JobId> {
    import_queue().import(input, output, output_audio.map(Into::into), ImportOptions::default())
}

/// [`import`], with options — hardware acceleration among them.
///
/// # Errors
///
/// As [`ImportQueue::import`].
pub fn import_with(
    input: impl Into<PathBuf>,
    output: impl Into<PathBuf>,
    output_audio: Option<impl Into<PathBuf>>,
    options: ImportOptions,
) -> Result<JobId> {
    import_queue().import(input, output, output_audio.map(Into::into), options)
}

/// Every file [`import`] has been given: [`ImportQueue::progress`] on
/// [`import_queue`].
pub fn import_progress() -> Vec<JobProgress> {
    import_queue().progress()
}

/// Everything that can be known about an import before it is queued — and
/// every reason to refuse it now rather than fail it later.
fn check(
    input: &Path,
    output: &Path,
    output_audio: Option<&Path>,
    options: &ImportOptions,
) -> Result<(Encoding, (u32, u32))> {
    let demuxer = crate::open_media(input)?;
    let info = demuxer.info();
    let track = info.video().ok_or_else(|| {
        Error::unsupported(format!("{}: there is no video track in it", input.display()))
    })?;

    // Opening a decoder costs a session, and is the only honest answer to
    // "can this machine read it, in the way asked".
    crate::decode::open_with(&DecoderConfig::from_track(track)?, options.hardware)?;

    let settings = final_settings(options, 0);
    let (encoding, width, height) = transcode::plan(input, &settings)?;

    for path in std::iter::once(output).chain(output_audio) {
        let directory = path.parent().filter(|parent| !parent.as_os_str().is_empty());
        if directory.is_some_and(|directory| !directory.is_dir()) {
            return Err(Error::io(
                path,
                std::io::Error::new(std::io::ErrorKind::NotFound, "its directory does not exist"),
            ));
        }
    }

    if output_audio.is_some() {
        if !cfg!(feature = "split-audio") {
            return Err(Error::unsupported(
                "splitting the audio out needs the split-audio feature (atome)",
            ));
        }

        if !info.has_audio() {
            return Err(Error::unsupported(format!(
                "{}: there is no audio in it to split out",
                input.display()
            )));
        }
    }

    Ok((encoding, (width, height)))
}

fn final_settings(options: &ImportOptions, threads: usize) -> Settings {
    Settings {
        encoding: options.encoding,
        quality: options.quality,
        level: options.level,
        hardware: options.hardware,
        threads,
        ..Settings::default()
    }
}

fn proxy_settings(options: &ImportOptions, threads: usize) -> Settings {
    Settings {
        encoding: options.encoding,
        level: options.level,
        hardware: options.hardware,
        threads,
        ..Settings::proxy()
    }
}

/// A worker: takes the next file from its lane, does that lane's part, and
/// hands the file on — until the queue closes.
fn work(queue: &Shared, lane: Lane) {
    loop {
        let next = {
            let mut jobs = lock(&queue.jobs);

            loop {
                if queue.closed.load(Ordering::Acquire) {
                    return;
                }

                let taken = match lane {
                    Lane::Proxy => jobs.proxy_lane.pop_front(),
                    Lane::Final => jobs.final_lane.pop_front(),
                };

                if let Some(id) = taken {
                    break id;
                }

                jobs = queue
                    .work
                    .wait(jobs)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };

        match lane {
            Lane::Proxy => proxy_lane(queue, next),
            Lane::Final => final_lane(queue, next),
        }
    }
}

/// What a worker needs to know about a job, copied out from under the lock.
struct Ticket {
    input: PathBuf,
    output: PathBuf,
    output_audio: Option<PathBuf>,
    options: ImportOptions,
    cancel: Arc<AtomicBool>,
}

fn ticket(queue: &Shared, id: JobId) -> Option<Ticket> {
    let jobs = lock(&queue.jobs);
    let job = jobs.jobs.get(&id)?;

    Some(Ticket {
        input: job.progress.input.clone(),
        output: job.progress.output.clone(),
        output_audio: job.progress.output_audio.clone(),
        options: job.options.clone(),
        cancel: Arc::clone(&job.cancel),
    })
}

/// Changes one job's progress and tells everyone waiting.
fn update(queue: &Shared, id: JobId, change: impl FnOnce(&mut JobProgress)) {
    if let Some(job) = lock(&queue.jobs).jobs.get_mut(&id) {
        change(&mut job.progress);
    }
    queue.changed.notify_all();
}

/// How much of the whole import each stage is. The final encode dominates.
fn weights(progress: &JobProgress, proxy: bool) -> (f32, f32, f32) {
    let proxy = if proxy { 0.1 } else { 0.0 };
    let audio = if progress.output_audio.is_some() { 0.05 } else { 0.0 };
    (proxy, audio, 1.0 - proxy - audio)
}

fn threads(queue: &Shared) -> usize {
    match queue.config.encoder_threads {
        0 => std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .div_ceil(2),
        threads => threads,
    }
}

/// The proxy lane's part: the proxy, then the audio, then on to the final
/// lane.
fn proxy_lane(queue: &Shared, id: JobId) {
    let Some(ticket) = ticket(queue, id) else {
        return;
    };

    if ticket.cancel.load(Ordering::Relaxed) {
        update(queue, id, |progress| progress.stage = Stage::Cancelled);
        return;
    }

    if ticket.options.proxy {
        update(queue, id, |progress| progress.stage = Stage::Proxy);

        let settings = proxy_settings(&ticket.options, threads(queue));
        let partial = beside(&ticket.output, "proxy");

        let result = transcode::transcode(
            &ticket.input,
            &partial,
            &settings,
            |done| {
                update(queue, id, |progress| {
                    let (proxy, _, _) = weights(progress, true);
                    progress.stage_fraction = done.fraction;
                    progress.frames = (done.frames, done.total);
                    progress.fraction = proxy * done.fraction;
                });
            },
            &ticket.cancel,
        )
        .and_then(|summary| {
            std::fs::rename(&partial, &ticket.output)
                .map_err(|error| Error::io(&ticket.output, error))?;
            Ok(summary)
        });

        match result {
            Ok(summary) => update(queue, id, |progress| {
                progress.output_ready = true;
                progress.decoder_hardware = Some(summary.decoder_hardware);
                progress.encoder_hardware = Some(summary.encoder_hardware);
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&partial);
                return end(queue, id, error);
            }
        }
    }

    if let Some(audio) = &ticket.output_audio {
        update(queue, id, |progress| {
            progress.stage = Stage::Audio;
            progress.stage_fraction = 0.0;
        });

        if let Err(error) = split_audio(&ticket.input, audio, &ticket.cancel) {
            return end(queue, id, error);
        }

        update(queue, id, |progress| {
            let (proxy, audio, _) = weights(progress, ticket.options.proxy);
            progress.stage_fraction = 1.0;
            progress.fraction = proxy + audio;
        });
    }

    if ticket.cancel.load(Ordering::Relaxed) {
        return end(queue, id, Error::Cancelled);
    }

    {
        let mut jobs = lock(&queue.jobs);
        if let Some(job) = jobs.jobs.get_mut(&id) {
            job.progress.stage = Stage::WaitingForFinal;
            job.progress.stage_fraction = 0.0;
        }
        jobs.final_lane.push_back(id);
    }
    queue.work.notify_all();
    queue.changed.notify_all();
}

/// The final lane's part: the full-quality file in the temp directory, then
/// onto the output path.
fn final_lane(queue: &Shared, id: JobId) {
    let Some(ticket) = ticket(queue, id) else {
        return;
    };

    if ticket.cancel.load(Ordering::Relaxed) {
        update(queue, id, |progress| progress.stage = Stage::Cancelled);
        return;
    }

    update(queue, id, |progress| {
        progress.stage = Stage::Final;
        progress.stage_fraction = 0.0;
        progress.frames = (0, None);
    });

    let directory = ticket
        .options
        .temp_dir
        .clone()
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("vtome-import-{}-{}", std::process::id(), id.0));

    if let Err(error) = std::fs::create_dir_all(&directory) {
        return end(queue, id, Error::io(&directory, error));
    }

    let name = ticket
        .output
        .file_name()
        .map_or_else(|| "final".into(), |name| name.to_os_string());
    let encoded = directory.join(name);

    let settings = final_settings(&ticket.options, threads(queue));

    let result = transcode::transcode(
        &ticket.input,
        &encoded,
        &settings,
        |done| {
            update(queue, id, |progress| {
                let (proxy, audio, final_share) = weights(progress, ticket.options.proxy);
                progress.stage_fraction = done.fraction;
                progress.frames = (done.frames, done.total);
                progress.fraction = proxy + audio + final_share * done.fraction;
            });
        },
        &ticket.cancel,
    );

    let result = result.and_then(|summary| {
        update(queue, id, |progress| progress.stage = Stage::Replacing);
        replace(&encoded, &ticket.output)?;
        Ok(summary)
    });

    let _ = std::fs::remove_dir_all(&directory);

    match result {
        Ok(summary) => update(queue, id, |progress| {
            progress.stage = Stage::Done;
            progress.fraction = 1.0;
            progress.stage_fraction = 1.0;
            progress.output_ready = true;
            progress.final_ready = true;
            progress.decoder_hardware = Some(summary.decoder_hardware);
            progress.encoder_hardware = Some(summary.encoder_hardware);
        }),
        Err(error) => end(queue, id, error),
    }
}

/// Ends a job early: cancelled, or failed with the reason.
fn end(queue: &Shared, id: JobId, error: Error) {
    update(queue, id, |progress| match error {
        Error::Cancelled => progress.stage = Stage::Cancelled,
        other => {
            progress.stage = Stage::Failed;
            progress.error = Some(other.to_string());
        }
    });
}

/// A hidden file beside `output`, for writing before a rename onto it — the
/// same directory, so the same volume, so the rename is atomic.
fn beside(output: &Path, what: &str) -> PathBuf {
    let name = output
        .file_name()
        .map_or_else(|| "import".into(), |name| name.to_string_lossy().into_owned());

    output.with_file_name(format!(".{name}.vtome-{what}"))
}

/// Puts `from` at `to` in one step: a rename where the two share a volume,
/// otherwise a copy beside `to` and a rename from there — so `to` holds the
/// old file or the new one, never half of either.
fn replace(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }

    let staged = beside(to, "final");

    let copied = std::fs::copy(from, &staged).and_then(|_| std::fs::rename(&staged, to));

    if let Err(error) = copied {
        let _ = std::fs::remove_file(&staged);
        return Err(Error::io(to, error));
    }

    let _ = std::fs::remove_file(from);
    Ok(())
}

/// The soundtrack to FLAC, through atome — written beside the target and
/// renamed onto it.
#[cfg(feature = "split-audio")]
fn split_audio(input: &Path, output: &Path, cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }

    let partial = beside(output, "audio");

    let result = atome::export::to_flac(input, &partial)
        .map_err(|error| Error::Encode {
            reason: format!("splitting the audio out of {}: {error}", input.display()),
        })
        .and_then(|_| std::fs::rename(&partial, output).map_err(|error| Error::io(output, error)));

    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }

    result
}

#[cfg(not(feature = "split-audio"))]
fn split_audio(_input: &Path, _output: &Path, _cancel: &AtomicBool) -> Result<()> {
    Err(Error::unsupported(
        "splitting the audio out needs the split-audio feature (atome)",
    ))
}
