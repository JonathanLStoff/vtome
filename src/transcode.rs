//! One file in, one file out: demux, decode, scale, encode, mux.
//!
//! The pipeline under [`import`](crate::import()), with no queue and no proxy —
//! it writes exactly one file at the path it is given, at the settings it is
//! given, and stops at the next frame when told to. Nothing is buffered beyond
//! a frame or two and the encoder's own lookahead, so a two-hour film costs
//! the same memory as a two-second one.
//!
//! ```no_run
//! use std::sync::atomic::AtomicBool;
//! use vtome::transcode::{transcode, Settings};
//!
//! let summary = transcode(
//!     "camera.mov",
//!     "camera.mp4",
//!     &Settings::default(),
//!     |progress| println!("{:.0}%", progress.fraction * 100.0),
//!     &AtomicBool::new(false),
//! )?;
//! println!("{} frames of {} at {}×{}", summary.frames, summary.encoding, summary.width, summary.height);
//! # Ok::<(), vtome::Error>(())
//! ```
//!
//! The output can be something other than a path — [`transcode_into`] writes
//! to any [`Storage`](crate::mux::Storage): a file already open, or an entry of
//! a `pfac` bundle, so a transcode lands in the project it belongs to without
//! being written somewhere else first.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::decode::{DecoderConfig, Hardware};
use crate::encode::{self, Backend, Encoder, EncoderConfig, Level};
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::{Container, Encoding};
use crate::media::{Packet, Rational};
use crate::mux::{self, Muxer, Storage, VideoTrack};

/// How to write a file.
///
/// [`Settings::default`] is the final file — §15's spec at full quality —
/// and [`Settings::proxy`] the quick, small stand-in `import` writes first.
#[derive(Clone, Debug)]
pub struct Settings {
    /// What to write: `None` for H.264, the default everywhere (see
    /// [`encode::default_encoding`]); `Some(Encoding::Av1)` for rav1e.
    pub encoding: Option<Encoding>,
    /// 0.0 to 1.0; see [`EncoderConfig::quality`].
    pub quality: f32,
    /// Never bigger than this; the shape is kept. `None` for the source's own
    /// size — though H.264 at Level 4.1 may still shrink it to fit.
    pub max_size: Option<(u32, u32)>,
    /// The H.264 level to declare: 4.1 unless asked otherwise.
    pub level: Level,
    /// A keyframe exactly this often: two seconds.
    pub keyframe_interval: Duration,
    /// Favour speed over size.
    pub realtime: bool,
    /// Whether decoding and encoding may, must, or must not use hardware.
    pub hardware: Hardware,
    /// Threads for a software encoder; 0 for its own choice.
    pub threads: usize,
}

impl Default for Settings {
    /// The final file: §15's spec — quality 0.68, Level 4.1, a keyframe every
    /// two seconds — at the source's size where 4.1 allows it.
    fn default() -> Self {
        Settings {
            encoding: None,
            quality: 0.68,
            max_size: None,
            level: Level::L4_1,
            keyframe_interval: Duration::from_secs(2),
            realtime: false,
            hardware: Hardware::Prefer,
            threads: 0,
        }
    }
}

impl Settings {
    /// The stand-in: no bigger than 640 pixels either way, low quality, the
    /// encoder's fastest settings. Small and quick, and still the same spec —
    /// it plays everywhere the final file will.
    pub fn proxy() -> Self {
        Settings {
            quality: 0.3,
            max_size: Some((640, 640)),
            realtime: true,
            ..Settings::default()
        }
    }
}

/// How far a transcode has got.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Progress {
    /// Pictures encoded so far.
    pub frames: u64,
    /// Pictures there will be, as far as the container's duration and frame
    /// rate say. `None` where it states neither.
    pub total: Option<u64>,
    /// 0.0 to 1.0. Held just under 1.0 until the file is finished, since an
    /// estimate of the total can be a frame or two short.
    pub fraction: f32,
}

/// What a finished transcode wrote.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    /// The encoding written.
    pub encoding: Encoding,
    /// The container it went into.
    pub container: Container,
    /// The encoder that wrote it.
    pub encoder: Backend,
    /// Picture width written.
    pub width: u32,
    /// Picture height written.
    pub height: u32,
    /// Pictures written.
    pub frames: u64,
    /// How long it plays.
    pub duration: Duration,
    /// The file's size.
    pub bytes: u64,
    /// Whether the source was decoded in hardware.
    pub decoder_hardware: bool,
    /// Whether it was encoded in hardware.
    pub encoder_hardware: bool,
}

/// Writes `input` to `output` as `settings` say, calling `progress` after
/// every picture, and stopping at the next picture once `cancel` is set.
///
/// The container follows the encoding — MP4 for H.264, WebM for AV1 —
/// whatever `output` is called. A transcode that fails or is cancelled
/// removes what it had written of `output`.
///
/// # Errors
///
/// Whatever opening, decoding, or encoding refuses — [`Error::NoDecoder`] and
/// [`Error::NoEncoder`] name what was missing; [`Error::Cancelled`] if
/// `cancel` was set; [`Error::Decode`] for a file with no pictures in it.
pub fn transcode(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    settings: &Settings,
    mut progress: impl FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Summary> {
    let (input, output) = (input.as_ref(), output.as_ref());

    let result = Pipeline::open(input, Output::Path(output), settings)
        .and_then(|pipeline| pipeline.run(&mut progress, cancel));

    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }

    result
}

/// [`transcode`], into `output` rather than a path.
///
/// `output` must be empty and positioned at its start. It is written as a file
/// is — headers patched afterwards, the MP4 index moved to the front — which
/// is why it has to read and seek as well as write, and why a bundle entry is
/// a scratch file until it is finished. When this returns `Ok` it holds a
/// complete file, flushed and positioned at its end.
///
/// Pass `&mut output` to keep hold of it. **A failed or cancelled transcode
/// leaves whatever it had written in `output`**: there is no path to remove, and
/// what to do with a half-written destination — drop an unfinished bundle
/// entry, truncate a file — is its owner's call.
///
/// ```no_run
/// use std::io::Cursor;
/// use std::sync::atomic::AtomicBool;
/// use vtome::transcode::{transcode_into, Settings};
///
/// let mut mp4 = Cursor::new(Vec::new());
/// let summary = transcode_into(
///     "camera.mov",
///     &mut mp4,
///     &Settings::default(),
///     |_| {},
///     &AtomicBool::new(false),
/// )?;
/// assert_eq!(summary.bytes, mp4.get_ref().len() as u64);
/// # Ok::<(), vtome::Error>(())
/// ```
///
/// # Errors
///
/// As [`transcode`], and [`Error::Encode`] for an `output` that is not at its
/// start.
pub fn transcode_into(
    input: impl AsRef<Path>,
    mut output: impl Storage,
    settings: &Settings,
    mut progress: impl FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<Summary> {
    Pipeline::open(input.as_ref(), Output::Storage(Some(&mut output)), settings)
        .and_then(|pipeline| pipeline.run(&mut progress, cancel))
}

/// What a transcode would write for `input` under `settings`, worked out
/// without decoding anything: the encoding, and the size after `max_size`
/// and the H.264 level have had their say.
///
/// # Errors
///
/// As opening the file, and [`Error::NoEncoder`] if nothing here writes
/// what the settings ask for.
pub fn plan(input: impl AsRef<Path>, settings: &Settings) -> Result<(Encoding, u32, u32)> {
    let demuxer = crate::open_media(input.as_ref())?;
    let track = demuxer
        .info()
        .video()
        .ok_or_else(|| no_video(input.as_ref()))?;

    let encoding = choose_encoding(settings)?;
    let (width, height) = output_size(
        track.width,
        track.height,
        frame_rate_of(track.frame_rate),
        encoding,
        settings,
    );

    Ok((encoding, width, height))
}

/// The encoding `settings` ask for, or the default where they do not.
pub(crate) fn choose_encoding(settings: &Settings) -> Result<Encoding> {
    match settings.encoding {
        Some(encoding) => {
            if !encode::is_available(encoding, settings.hardware) {
                // Let `open` say why, in its own words.
                let config = EncoderConfig {
                    hardware: settings.hardware,
                    ..EncoderConfig::new(
                        encoding,
                        16,
                        16,
                        Rational::new(30, 1),
                        Default::default(),
                    )
                };
                encode::open(&config)?;
            }
            Ok(encoding)
        }
        None => encode::default_encoding(settings.hardware).ok_or_else(|| Error::NoEncoder {
            encoding: Encoding::H264,
            remedy: "H.264 is the default and only ever the OS's encoder — VideoToolbox, \
                     Media Foundation, or MediaCodec — and none is usable here (or none in \
                     the way `hardware` asks). Ask for AV1 explicitly to write it with rav1e"
                .to_string(),
        }),
    }
}

/// The size a `width` × `height` source is written at.
fn output_size(
    width: u32,
    height: u32,
    frame_rate: Rational,
    encoding: Encoding,
    settings: &Settings,
) -> (u32, u32) {
    let (mut width, mut height) = match settings.max_size {
        Some((max_width, max_height)) => crate::scale::fit_within(width, height, max_width, max_height),
        None => (width, height),
    };

    if encoding == Encoding::H264 {
        (width, height) = encode::fit_level(width, height, frame_rate, settings.level);
    }

    // 4:2:0 needs pairs, and encoders refuse odd sizes.
    width = (width & !1).max(2);
    height = (height & !1).max(2);

    (width, height)
}

fn frame_rate_of(stated: Option<Rational>) -> Rational {
    stated
        .filter(|rate| rate.numerator > 0)
        .unwrap_or(Rational::new(30, 1))
}

fn no_video(path: &Path) -> Error {
    Error::unsupported(format!("{}: there is no video track in it", path.display()))
}

/// A transcode under way.
struct Pipeline<'a> {
    output: Output<'a>,
    demuxer: Box<dyn crate::demux::Demuxer>,
    decoder: Box<dyn crate::decode::Decoder>,
    track_id: u32,
    encoding: Encoding,
    frame_rate: Rational,
    width: u32,
    height: u32,
    total: Option<u64>,
    settings: Settings,
    /// Opened at the first picture, which knows its own colour.
    encoder: Option<Box<dyn Encoder>>,
    /// That colour, as the encoder was told it.
    color: crate::color::ColorSpace,
    /// Opened once the encoder can say what its stream is.
    muxer: Option<Box<dyn Muxer + 'a>>,
    /// Packets waiting for the muxer to open.
    waiting: Vec<Packet>,
    /// The first picture's timestamp: the output starts at zero.
    origin: Option<Duration>,
    frames: u64,
}

/// Where the pipeline's file goes.
enum Output<'a> {
    Path(&'a Path),
    /// Taken when the muxer is opened, which happens once.
    Storage(Option<&'a mut dyn Storage>),
}

impl<'a> Pipeline<'a> {
    fn open(input: &Path, output: Output<'a>, settings: &Settings) -> Result<Self> {
        let demuxer = crate::open_media(input)?;
        let track = demuxer.info().video().ok_or_else(|| no_video(input))?.clone();

        let decoder =
            crate::decode::open_with(&DecoderConfig::from_track(&track)?, settings.hardware)?;

        let encoding = choose_encoding(settings)?;
        let frame_rate = frame_rate_of(track.frame_rate);
        let (width, height) = output_size(track.width, track.height, frame_rate, encoding, settings);

        let total = (!track.duration.is_zero() && track.frame_rate.is_some())
            .then(|| (track.duration.as_secs_f64() * frame_rate.as_f64()).round() as u64)
            .filter(|total| *total > 0);

        Ok(Pipeline {
            output,
            demuxer,
            decoder,
            track_id: track.id,
            encoding,
            frame_rate,
            width,
            height,
            total,
            settings: settings.clone(),
            encoder: None,
            color: crate::color::ColorSpace::default(),
            muxer: None,
            waiting: Vec::new(),
            origin: None,
            frames: 0,
        })
    }

    fn run(mut self, progress: &mut impl FnMut(Progress), cancel: &AtomicBool) -> Result<Summary> {
        while let Some(packet) = self.demuxer.next_packet()? {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }

            if packet.track_id != self.track_id {
                continue;
            }

            if let Some(frame) = self.decoder.decode(&packet)? {
                self.picture(frame, progress)?;
            }
        }

        for frame in self.decoder.flush()? {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            self.picture(frame, progress)?;
        }

        let Some(encoder) = self.encoder.as_mut() else {
            return Err(Error::Decode {
                encoding: self.decoder.encoding(),
                reason: "the file decoded to no pictures at all".to_string(),
            });
        };

        let rest = encoder.finish()?;
        self.deliver(rest)?;
        let encoder = self.encoder.take().expect("checked above");

        let muxer = self
            .muxer
            .take()
            .ok_or_else(|| encode_error("the encoder never said what its stream was"))?;
        let written = muxer.finish()?;

        progress(Progress {
            frames: self.frames,
            total: self.total,
            fraction: 1.0,
        });

        Ok(Summary {
            encoding: self.encoding,
            container: mux::container_for(self.encoding)?,
            encoder: encoder.backend(),
            width: self.width,
            height: self.height,
            frames: written.samples,
            duration: written.duration,
            bytes: written.bytes,
            decoder_hardware: self.decoder.is_hardware(),
            encoder_hardware: encoder.is_hardware(),
        })
    }

    /// One decoded picture: sized, restamped from zero, encoded.
    fn picture(&mut self, frame: Frame, progress: &mut impl FnMut(Progress)) -> Result<()> {
        let origin = *self.origin.get_or_insert(frame.pts());

        let frame = if (frame.width(), frame.height()) == (self.width, self.height) {
            frame
        } else {
            crate::scale::resize(&frame, self.width, self.height)?
        };
        let frame = frame.with_pts(frame.pts().saturating_sub(origin));

        if self.encoder.is_none() {
            let config = EncoderConfig {
                quality: self.settings.quality,
                keyframe_interval: self.settings.keyframe_interval,
                realtime: self.settings.realtime,
                hardware: self.settings.hardware,
                level: self.settings.level,
                threads: self.settings.threads,
                // The picture's own colour, as decoded — the range in
                // particular, which a decoder may have changed.
                ..EncoderConfig::new(
                    self.encoding,
                    self.width,
                    self.height,
                    self.frame_rate,
                    frame.color(),
                )
            };

            self.color = frame.color();
            self.encoder = Some(encode::open(&config)?);
        }

        let packets = self
            .encoder
            .as_mut()
            .expect("opened above")
            .encode(&frame)?;
        self.deliver(packets)?;

        self.frames += 1;

        progress(Progress {
            frames: self.frames,
            total: self.total,
            fraction: self
                .total
                .map_or(0.0, |total| (self.frames as f32 / total as f32).min(0.99)),
        });

        Ok(())
    }

    /// Packets to the muxer, opening it once the encoder can say what its
    /// stream is — VideoToolbox only knows its parameter sets after its first
    /// picture, so packets wait until then.
    fn deliver(&mut self, packets: Vec<Packet>) -> Result<()> {
        self.waiting.extend(packets);

        if self.muxer.is_none() {
            let Some(record) = self
                .encoder
                .as_ref()
                .and_then(|encoder| encoder.config_record())
            else {
                return Ok(());
            };

            let track = VideoTrack {
                encoding: self.encoding,
                width: self.width,
                height: self.height,
                frame_rate: self.frame_rate,
                color: self.color,
                config_record: record,
            };

            self.muxer = Some(match &mut self.output {
                Output::Path(path) => mux::create(path, track)?,
                Output::Storage(storage) => {
                    let storage = storage.take().expect("the muxer is opened once");
                    mux::create_in(storage, track)?
                }
            });
        }

        let muxer = self.muxer.as_mut().expect("opened above");
        for packet in self.waiting.drain(..) {
            muxer.write(packet)?;
        }

        Ok(())
    }
}

fn encode_error(reason: impl Into<String>) -> Error {
    Error::Encode {
        reason: reason.into(),
    }
}
