//! GStreamer: the H.264 encoder and decoder a Linux system has.
//!
//! The Linux counterpart of VideoToolbox, Media Foundation and MediaCodec.
//! Linux has no one media API, but every desktop distribution ships GStreamer,
//! and GStreamer reaches whatever codecs the machine has: VA-API or NVIDIA in
//! hardware where the drivers are there, the distribution's software codecs
//! where they are not. The codec is the system's, installed by whoever owns
//! the machine, so vtome bundles none — exactly as on the other platforms.
//!
//! GStreamer is plain C, and it is opened at run time with `dlopen` rather
//! than linked: a vtome built on Linux needs no GStreamer to build or start,
//! and a machine without it gets [`Error::NoDecoder`] / [`Error::NoEncoder`]
//! naming the packages to install, rather than failing to load. The handful of
//! calls it needs are declared here by hand, so it pulls in no crate.
//!
//! # Which codec
//!
//! Hardware first — `vah264dec`/`vah264enc` (VA-API), the older `vaapi`
//! elements, NVIDIA's, V4L2 on ARM boards — then software: `avdec_h264` or
//! `openh264dec` to read, `x264enc` or `openh264enc` to write. [`Hardware`]
//! narrows that list the same way it does everywhere else. Whatever is picked,
//! the stream is fed and read as MP4 keeps it — `avcC` and length-prefixed NAL
//! units in, NV12 or I420 out; NV12 in, §15's H.264 out.
//!
//! What a Debian or Ubuntu machine needs for both directions:
//! `gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad
//! gstreamer1.0-plugins-ugly gstreamer1.0-libav`.

#![allow(non_camel_case_types)]

use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::bitstream::{AccessUnit, AvcConfig};
use crate::color::{ColorSpace, Matrix, Range};
use crate::decode::{Decoder, DecoderConfig, Hardware};
use crate::encode::{Backend, Encoder, EncoderConfig, Level};
use crate::error::{Error, Result};
use crate::frame::{Frame, PixelFormat, Plane};
use crate::identify::Encoding;
use crate::media::Packet;

/// What to install, for an error to say.
const INSTALL: &str = "install GStreamer with its H.264 plugins — on Debian or Ubuntu \
    gstreamer1.0-plugins-base, gstreamer1.0-plugins-bad, gstreamer1.0-plugins-ugly and \
    gstreamer1.0-libav; on Fedora (with RPM Fusion) gstreamer1-plugins-base, \
    gstreamer1-plugins-bad-free, gstreamer1-plugins-ugly and gstreamer1-plugin-libav; on \
    Alpine gst-plugins-base, gst-plugins-bad, gst-plugins-ugly and gst-libav";

/// Decoders, best first, and whether each is hardware.
const DECODERS: [(&str, bool); 7] = [
    ("vah264dec", true),
    ("vaapih264dec", true),
    ("nvh264dec", true),
    ("v4l2slh264dec", true),
    ("v4l2h264dec", true),
    ("avdec_h264", false),
    ("openh264dec", false),
];

/// Encoders, best first, and whether each is hardware.
const ENCODERS: [(&str, bool); 5] = [
    ("vah264enc", true),
    ("vaapih264enc", true),
    ("nvh264enc", true),
    ("x264enc", false),
    ("openh264enc", false),
];

/// How long a flush waits for the last pictures before giving up on them.
const DRAIN: Duration = Duration::from_secs(30);

mod sys {
    use std::ffi::{c_char, c_void};

    pub type GstElement = c_void;
    pub type GstBus = c_void;
    pub type GstMessage = c_void;
    pub type GstSample = c_void;
    pub type GstCaps = c_void;
    pub type GstStructure = c_void;
    pub type gboolean = i32;

    #[repr(C)]
    pub struct GError {
        pub domain: u32,
        pub code: i32,
        pub message: *mut c_char,
    }

    /// `GstMiniObject`, which every buffer starts with.
    #[repr(C)]
    pub struct GstMiniObject {
        pub type_: usize,
        pub refcount: i32,
        pub lockstate: i32,
        pub flags: u32,
        pub copy: *mut c_void,
        pub dispose: *mut c_void,
        pub free: *mut c_void,
        pub priv_uint: u32,
        pub priv_pointer: *mut c_void,
    }

    /// The public head of `GstBuffer`: the timestamps are fields, set and read
    /// through macros in C, so they are reached the same way here.
    #[repr(C)]
    pub struct GstBuffer {
        pub mini_object: GstMiniObject,
        pub pool: *mut c_void,
        pub pts: u64,
        pub dts: u64,
        pub duration: u64,
        pub offset: u64,
        pub offset_end: u64,
    }

    #[repr(C)]
    pub struct GstMapInfo {
        pub memory: *mut c_void,
        pub flags: u32,
        pub data: *mut u8,
        pub size: usize,
        pub maxsize: usize,
        pub user_data: [*mut c_void; 4],
        pub reserved: [*mut c_void; 4],
    }

    pub const CLOCK_TIME_NONE: u64 = u64::MAX;
    pub const STATE_NULL: i32 = 1;
    pub const STATE_PLAYING: i32 = 4;
    pub const STATE_CHANGE_FAILURE: i32 = 0;
    pub const MESSAGE_ERROR: u32 = 1 << 1;
    pub const MAP_READ: u32 = 1;
    pub const RTLD_NOW: i32 = 2;
    pub const RTLD_GLOBAL: i32 = 0x100;

    #[link(name = "dl")]
    extern "C" {
        pub fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
}

use sys::{gboolean, GError, GstBuffer, GstBus, GstCaps, GstElement, GstMapInfo, GstMessage, GstSample, GstStructure};

/// The calls vtome makes, looked up once.
struct Api {
    parse_launch: unsafe extern "C" fn(*const c_char, *mut *mut GError) -> *mut GstElement,
    bin_get_by_name: unsafe extern "C" fn(*mut GstElement, *const c_char) -> *mut GstElement,
    element_set_state: unsafe extern "C" fn(*mut GstElement, i32) -> i32,
    element_get_bus: unsafe extern "C" fn(*mut GstElement) -> *mut GstBus,
    element_factory_find: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    object_unref: unsafe extern "C" fn(*mut c_void),
    mini_object_unref: unsafe extern "C" fn(*mut c_void),
    bus_pop_filtered: unsafe extern "C" fn(*mut GstBus, u32) -> *mut GstMessage,
    message_parse_error: unsafe extern "C" fn(*mut GstMessage, *mut *mut GError, *mut *mut c_char),
    buffer_new_allocate: unsafe extern "C" fn(*mut c_void, usize, *mut c_void) -> *mut GstBuffer,
    buffer_fill: unsafe extern "C" fn(*mut GstBuffer, usize, *const c_void, usize) -> usize,
    buffer_map: unsafe extern "C" fn(*mut GstBuffer, *mut GstMapInfo, u32) -> gboolean,
    buffer_unmap: unsafe extern "C" fn(*mut GstBuffer, *mut GstMapInfo),
    sample_get_buffer: unsafe extern "C" fn(*mut GstSample) -> *mut GstBuffer,
    sample_get_caps: unsafe extern "C" fn(*mut GstSample) -> *mut GstCaps,
    caps_get_structure: unsafe extern "C" fn(*const GstCaps, u32) -> *mut GstStructure,
    structure_get_int: unsafe extern "C" fn(*const GstStructure, *const c_char, *mut i32) -> gboolean,
    structure_get_string: unsafe extern "C" fn(*const GstStructure, *const c_char) -> *const c_char,
    signal_emit_by_name: unsafe extern "C" fn(*mut c_void, *const c_char, ...),
    object_get: unsafe extern "C" fn(*mut c_void, *const c_char, ...),
    error_free: unsafe extern "C" fn(*mut GError),
    free: unsafe extern "C" fn(*mut c_void),
}

// SAFETY: plain function pointers into libraries that stay loaded for the life
// of the process; GStreamer's API is thread-safe.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// GStreamer, loaded and initialised — or why it could not be.
fn api() -> std::result::Result<&'static Api, String> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn load() -> std::result::Result<Api, String> {
    fn open(name: &CStr) -> std::result::Result<*mut c_void, String> {
        // SAFETY: a NUL-terminated name; the handle is never closed, so every
        // pointer looked up in it stays valid.
        let handle = unsafe { sys::dlopen(name.as_ptr(), sys::RTLD_NOW | sys::RTLD_GLOBAL) };
        if handle.is_null() {
            Err(format!("{} could not be loaded: {INSTALL}", name.to_string_lossy()))
        } else {
            Ok(handle)
        }
    }

    let gst = open(c"libgstreamer-1.0.so.0")?;
    let gobject = open(c"libgobject-2.0.so.0")?;
    let glib = open(c"libglib-2.0.so.0")?;

    macro_rules! symbol {
        ($library:expr, $name:literal) => {{
            // SAFETY: the symbol is looked up by its C name, and the field it
            // lands in has the exact signature GStreamer's headers give it.
            let found = unsafe { sys::dlsym($library, concat!($name, "\0").as_ptr().cast()) };
            if found.is_null() {
                return Err(format!("this GStreamer has no {}: {INSTALL}", $name));
            }
            unsafe { std::mem::transmute_copy(&found) }
        }};
    }

    let init_check: unsafe extern "C" fn(*mut i32, *mut c_void, *mut *mut GError) -> gboolean =
        symbol!(gst, "gst_init_check");

    let api = Api {
        parse_launch: symbol!(gst, "gst_parse_launch"),
        bin_get_by_name: symbol!(gst, "gst_bin_get_by_name"),
        element_set_state: symbol!(gst, "gst_element_set_state"),
        element_get_bus: symbol!(gst, "gst_element_get_bus"),
        element_factory_find: symbol!(gst, "gst_element_factory_find"),
        object_unref: symbol!(gst, "gst_object_unref"),
        mini_object_unref: symbol!(gst, "gst_mini_object_unref"),
        bus_pop_filtered: symbol!(gst, "gst_bus_pop_filtered"),
        message_parse_error: symbol!(gst, "gst_message_parse_error"),
        buffer_new_allocate: symbol!(gst, "gst_buffer_new_allocate"),
        buffer_fill: symbol!(gst, "gst_buffer_fill"),
        buffer_map: symbol!(gst, "gst_buffer_map"),
        buffer_unmap: symbol!(gst, "gst_buffer_unmap"),
        sample_get_buffer: symbol!(gst, "gst_sample_get_buffer"),
        sample_get_caps: symbol!(gst, "gst_sample_get_caps"),
        caps_get_structure: symbol!(gst, "gst_caps_get_structure"),
        structure_get_int: symbol!(gst, "gst_structure_get_int"),
        structure_get_string: symbol!(gst, "gst_structure_get_string"),
        signal_emit_by_name: symbol!(gobject, "g_signal_emit_by_name"),
        object_get: symbol!(gobject, "g_object_get"),
        error_free: symbol!(glib, "g_error_free"),
        free: symbol!(glib, "g_free"),
    };

    let mut error = std::ptr::null_mut();
    // SAFETY: no arguments to parse; GStreamer makes initialising more than
    // once harmless, and an application that already did keeps its own setup.
    if unsafe { init_check(std::ptr::null_mut(), std::ptr::null_mut(), &mut error) } == 0 {
        return Err(format!("GStreamer would not start: {}", take_error(&api, error)));
    }

    Ok(api)
}

/// A `GError`'s message, freeing it.
fn take_error(api: &Api, error: *mut GError) -> String {
    if error.is_null() {
        return "no reason given".to_string();
    }

    // SAFETY: a live GError, freed exactly once here.
    unsafe {
        let message = if (*error).message.is_null() {
            "no reason given".to_string()
        } else {
            CStr::from_ptr((*error).message).to_string_lossy().into_owned()
        };
        (api.error_free)(error);
        message
    }
}

/// Whether this GStreamer has an element called `name`.
fn has_element(api: &Api, name: &str) -> bool {
    let Ok(name) = CString::new(name) else {
        return false;
    };

    // SAFETY: a NUL-terminated name; the factory, if found, is a new
    // reference, given straight back.
    unsafe {
        let factory = (api.element_factory_find)(name.as_ptr());
        if factory.is_null() {
            return false;
        }
        (api.object_unref)(factory);
        true
    }
}

/// The codecs from `list` this machine has, under `hardware`, best first.
fn candidates(api: &Api, list: &[(&'static str, bool)], hardware: Hardware) -> Vec<(&'static str, bool)> {
    list.iter()
        .copied()
        .filter(|(_, is_hardware)| match hardware {
            Hardware::Prefer => true,
            Hardware::Require => *is_hardware,
            Hardware::Off => !*is_hardware,
        })
        .filter(|(name, _)| has_element(api, name))
        .collect()
}

/// What to say when `list` has nothing here under `hardware`.
fn nothing_here(what: &str, hardware: Hardware) -> String {
    match hardware {
        Hardware::Require => format!(
            "hardware {what} was required, and GStreamer has no H.264 {what} in hardware here \
             (VA-API, NVIDIA or V4L2)"
        ),
        _ => format!("GStreamer has no H.264 {what} here: {INSTALL}"),
    }
}

/// One running pipeline: an `appsrc` vtome feeds, an `appsink` it reads.
struct Pipeline {
    api: &'static Api,
    pipeline: *mut GstElement,
    source: *mut GstElement,
    sink: *mut GstElement,
    bus: *mut GstBus,
}

// SAFETY: GStreamer elements may be driven from any thread, and a pipeline is
// only ever used by the one that owns its decoder or encoder.
unsafe impl Send for Pipeline {}

impl Pipeline {
    /// Builds `description` — which names its `appsrc` `src` and its
    /// `appsink` `sink` — and sets it playing.
    fn launch(api: &'static Api, description: &str) -> std::result::Result<Self, String> {
        let text = CString::new(description).map_err(|_| "a NUL in a pipeline".to_string())?;
        let mut error = std::ptr::null_mut();

        // SAFETY: a NUL-terminated description; what comes back is owned
        // here and released in `drop`.
        let pipeline = unsafe { (api.parse_launch)(text.as_ptr(), &mut error) };
        if !error.is_null() {
            if !pipeline.is_null() {
                // SAFETY: a partial pipeline, owned and never used.
                unsafe { (api.object_unref)(pipeline) };
            }
            return Err(take_error(api, error));
        }
        if pipeline.is_null() {
            return Err("GStreamer built no pipeline".to_string());
        }

        // SAFETY: a live bin and NUL-terminated names; each lookup is a new
        // reference, released in `drop`.
        let (source, sink, bus) = unsafe {
            (
                (api.bin_get_by_name)(pipeline, c"src".as_ptr()),
                (api.bin_get_by_name)(pipeline, c"sink".as_ptr()),
                (api.element_get_bus)(pipeline),
            )
        };

        let built = Pipeline {
            api,
            pipeline,
            source,
            sink,
            bus,
        };

        if source.is_null() || sink.is_null() || bus.is_null() {
            return Err("the pipeline has no appsrc or appsink".to_string());
        }

        // SAFETY: a live pipeline.
        if unsafe { (api.element_set_state)(pipeline, sys::STATE_PLAYING) } == sys::STATE_CHANGE_FAILURE {
            return Err(built.error().unwrap_or_else(|| "the pipeline would not start".to_string()));
        }

        Ok(built)
    }

    /// Hands `data` to the pipeline, stamped.
    fn push(&self, data: &[u8], pts: Duration, dts: Option<Duration>, duration: Option<Duration>) -> std::result::Result<(), String> {
        let nanos = |time: Duration| u64::try_from(time.as_nanos()).unwrap_or(sys::CLOCK_TIME_NONE - 1);

        // SAFETY: a new buffer of `data.len()` bytes, filled from a slice of
        // that length; the push signal takes its own reference, so the one made
        // here is released after it.
        unsafe {
            let buffer = (self.api.buffer_new_allocate)(std::ptr::null_mut(), data.len(), std::ptr::null_mut());
            if buffer.is_null() {
                return Err(format!("GStreamer could not allocate {} bytes", data.len()));
            }
            (self.api.buffer_fill)(buffer, 0, data.as_ptr().cast(), data.len());
            (*buffer).pts = nanos(pts);
            (*buffer).dts = dts.map_or(sys::CLOCK_TIME_NONE, nanos);
            (*buffer).duration = duration.map_or(sys::CLOCK_TIME_NONE, nanos);

            let mut flow: i32 = 0;
            (self.api.signal_emit_by_name)(self.source, c"push-buffer".as_ptr(), buffer, &mut flow as *mut i32);
            (self.api.mini_object_unref)(buffer.cast());

            if flow < 0 {
                return Err(self.error().unwrap_or_else(|| format!("the pipeline refused data (flow {flow})")));
            }
        }

        Ok(())
    }

    /// Tells the pipeline nothing more is coming.
    fn end(&self) {
        let mut flow: i32 = 0;
        // SAFETY: an action signal on a live appsrc, with its out-parameter.
        unsafe { (self.api.signal_emit_by_name)(self.source, c"end-of-stream".as_ptr(), &mut flow as *mut i32) };
    }

    /// The next output, waiting up to `timeout` for it.
    fn pull(&self, timeout: Duration) -> Option<Sample> {
        let mut sample: *mut GstSample = std::ptr::null_mut();
        let nanos = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX - 1);

        // SAFETY: an action signal on a live appsink; the sample, if any, is a
        // new reference owned by the `Sample` that wraps it.
        unsafe {
            (self.api.signal_emit_by_name)(self.sink, c"try-pull-sample".as_ptr(), nanos, &mut sample as *mut *mut GstSample)
        };

        (!sample.is_null()).then(|| Sample { api: self.api, sample })
    }

    /// Whether everything has come out after [`end`](Self::end).
    fn is_finished(&self) -> bool {
        let mut eos: gboolean = 0;
        // SAFETY: a boolean property read into a gboolean, list NULL-ended.
        unsafe { (self.api.object_get)(self.sink, c"eos".as_ptr(), &mut eos as *mut gboolean, std::ptr::null::<c_char>()) };
        eos != 0
    }

    /// The first error the pipeline has posted, if any.
    fn error(&self) -> Option<String> {
        // SAFETY: a live bus; the message, if any, is ours to release, and the
        // error and debug string it hands out are freed here.
        unsafe {
            let message = (self.api.bus_pop_filtered)(self.bus, sys::MESSAGE_ERROR);
            if message.is_null() {
                return None;
            }

            let mut error = std::ptr::null_mut();
            let mut debug = std::ptr::null_mut();
            (self.api.message_parse_error)(message, &mut error, &mut debug);
            (self.api.mini_object_unref)(message);

            let mut reason = take_error(self.api, error);
            if !debug.is_null() {
                let _ = write!(reason, " ({})", CStr::from_ptr(debug).to_string_lossy());
                (self.api.free)(debug.cast());
            }
            Some(reason)
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: every pointer is either null or a reference this owns.
        unsafe {
            (self.api.element_set_state)(self.pipeline, sys::STATE_NULL);
            for object in [self.source, self.sink, self.bus, self.pipeline] {
                if !object.is_null() {
                    (self.api.object_unref)(object);
                }
            }
        }
    }
}

/// One output buffer and the caps it came with.
struct Sample {
    api: &'static Api,
    sample: *mut GstSample,
}

impl Sample {
    /// The bytes, copied out.
    fn bytes(&self) -> Option<Vec<u8>> {
        // SAFETY: the buffer belongs to the live sample; it is mapped for
        // reading, copied, and unmapped before the map info goes away.
        unsafe {
            let buffer = (self.api.sample_get_buffer)(self.sample);
            if buffer.is_null() {
                return None;
            }

            let mut map: GstMapInfo = std::mem::zeroed();
            if (self.api.buffer_map)(buffer, &mut map, sys::MAP_READ) == 0 {
                return None;
            }
            let bytes = std::slice::from_raw_parts(map.data, map.size).to_vec();
            (self.api.buffer_unmap)(buffer, &mut map);
            Some(bytes)
        }
    }

    /// When the buffer is due, if it says.
    fn pts(&self) -> Option<Duration> {
        // SAFETY: the buffer belongs to the live sample; only a field is read.
        let pts = unsafe {
            let buffer = (self.api.sample_get_buffer)(self.sample);
            if buffer.is_null() {
                return None;
            }
            (*buffer).pts
        };

        (pts != sys::CLOCK_TIME_NONE).then(|| Duration::from_nanos(pts))
    }

    /// An integer field of the caps, such as `width`.
    fn int(&self, field: &CStr) -> Option<i32> {
        let structure = self.structure()?;
        let mut value = 0;
        // SAFETY: a live structure and a NUL-terminated field name.
        (unsafe { (self.api.structure_get_int)(structure, field.as_ptr(), &mut value) } != 0).then_some(value)
    }

    /// A string field of the caps, such as `format`.
    fn string(&self, field: &CStr) -> Option<String> {
        let structure = self.structure()?;
        // SAFETY: a live structure; the string is the structure's, copied.
        unsafe {
            let value = (self.api.structure_get_string)(structure, field.as_ptr());
            (!value.is_null()).then(|| CStr::from_ptr(value).to_string_lossy().into_owned())
        }
    }

    fn structure(&self) -> Option<*mut GstStructure> {
        // SAFETY: the caps belong to the live sample.
        unsafe {
            let caps = (self.api.sample_get_caps)(self.sample);
            if caps.is_null() {
                return None;
            }
            let structure = (self.api.caps_get_structure)(caps, 0);
            (!structure.is_null()).then_some(structure)
        }
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        // SAFETY: the reference `try-pull-sample` handed over.
        unsafe { (self.api.mini_object_unref)(self.sample) };
    }
}

/// Rounds up to a multiple of `to`, which is how GStreamer pads rows.
fn round_up(value: usize, to: usize) -> usize {
    value.div_ceil(to) * to
}

/// Where GStreamer puts the planes of a `width` × `height` picture when no
/// one has asked for anything else: rows padded to four bytes, chroma after
/// an even number of luma rows. Also the total size.
fn default_layout(format: PixelFormat, width: usize, height: usize) -> (Vec<Plane>, usize) {
    let luma = round_up(width, 4);
    let rows = round_up(height, 2);

    match format {
        PixelFormat::I420 => {
            let chroma = round_up(round_up(width, 2) / 2, 4);
            let u = luma * rows;
            let v = u + chroma * (rows / 2);
            (
                vec![
                    Plane { offset: 0, stride: luma },
                    Plane { offset: u, stride: chroma },
                    Plane { offset: v, stride: chroma },
                ],
                v + chroma * (rows / 2),
            )
        }
        _ => {
            let uv = luma * rows;
            (
                vec![Plane { offset: 0, stride: luma }, Plane { offset: uv, stride: luma }],
                uv + luma * (rows / 2),
            )
        }
    }
}

/// The H.264 decoder this machine's GStreamer has, into NV12 or I420.
pub struct GStreamerDecoder {
    api: &'static Api,
    /// The decoders that could still be tried, the one in use first.
    elements: Vec<(&'static str, bool)>,
    /// The input caps: `avcC` as `codec_data`.
    caps: String,
    pipeline: Option<Pipeline>,
    color: ColorSpace,
    ready: VecDeque<Frame>,
    /// Packets since the last reset, kept until the first picture comes out,
    /// so a hardware decoder that turns the stream down can hand it to the
    /// next one from the start.
    replay: Option<Vec<Packet>>,
    /// Whether the pipeline has been told the stream is over.
    ended: bool,
}

impl GStreamerDecoder {
    /// A decoder for the track `config` describes.
    ///
    /// # Errors
    ///
    /// [`Error::NoDecoder`] for anything but H.264, or a machine whose
    /// GStreamer has no H.264 decoder under `hardware`; [`Error::Decode`] for
    /// a track without parameter sets.
    pub fn new(config: &DecoderConfig, hardware: Hardware) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoDecoder {
                encoding: config.encoding,
                remedy: "vtome drives GStreamer for H.264".to_string(),
            });
        }

        let no_decoder = |remedy: String| Error::NoDecoder {
            encoding: Encoding::H264,
            remedy,
        };

        let api = api().map_err(no_decoder)?;
        AvcConfig::parse(&config.extra_data)?;

        let elements = candidates(api, &DECODERS, hardware);
        if elements.is_empty() {
            return Err(no_decoder(nothing_here("decoder", hardware)));
        }

        let mut hex = String::with_capacity(config.extra_data.len() * 2);
        for byte in &config.extra_data {
            let _ = write!(hex, "{byte:02x}");
        }
        let caps = format!(
            "video/x-h264,stream-format=(string)avc,alignment=(string)au,codec_data=(buffer){hex},\
             width=(int){},height=(int){}",
            config.width, config.height
        );

        let mut decoder = GStreamerDecoder {
            api,
            elements,
            caps,
            pipeline: None,
            color: config.color,
            ready: VecDeque::new(),
            replay: Some(Vec::new()),
            ended: false,
        };

        // A decoder that will not even build is passed over now; one that
        // turns the stream down is passed over at its first packet.
        let mut refusal = String::new();
        while !decoder.elements.is_empty() {
            match decoder.launch() {
                Ok(()) => return Ok(decoder),
                Err(reason) => {
                    refusal = format!("{}: {reason}", decoder.elements[0].0);
                    decoder.elements.remove(0);
                }
            }
        }

        Err(no_decoder(refusal))
    }

    fn launch(&mut self) -> std::result::Result<(), String> {
        let (element, _) = self.elements[0];
        let parse = if has_element(self.api, "h264parse") { "h264parse ! " } else { "" };

        self.pipeline = None;
        self.pipeline = Some(Pipeline::launch(
            self.api,
            &format!(
                "appsrc name=src format=time is-live=false caps=\"{}\" ! {parse}{element} ! \
                 videoconvert ! video/x-raw,format=(string){{NV12,I420}} ! \
                 appsink name=sink sync=false",
                self.caps
            ),
        )?);
        self.ended = false;
        Ok(())
    }

    fn pipeline(&self) -> &Pipeline {
        self.pipeline.as_ref().expect("a decoder always has a pipeline once opened")
    }

    /// Takes what has come out, waiting up to `timeout` for the first of it.
    fn take_output(&mut self, timeout: Duration) -> Result<()> {
        let mut wait = timeout;

        while let Some(sample) = self.pipeline().pull(wait) {
            wait = Duration::ZERO;

            let format = match sample.string(c"format").as_deref() {
                Some("I420") => PixelFormat::I420,
                _ => PixelFormat::Nv12,
            };
            let (Some(width), Some(height), Some(bytes)) = (sample.int(c"width"), sample.int(c"height"), sample.bytes()) else {
                continue;
            };
            let (width, height) = (width.max(0) as u32, height.max(0) as u32);

            let (planes, size) = default_layout(format, width as usize, height as usize);
            if bytes.len() < size {
                return Err(Error::Decode {
                    encoding: Encoding::H264,
                    reason: format!(
                        "{} handed back {} bytes for a {width}×{height} picture, which takes {size}",
                        self.elements[0].0,
                        bytes.len()
                    ),
                });
            }

            let frame = Frame::with_planes(width, height, format, self.color, sample.pts().unwrap_or_default(), planes, bytes)?;
            self.ready.push_back(frame);
            self.replay = None;
        }

        self.check()
    }

    /// Fails over to the next decoder if this one refused the stream before
    /// showing anything; otherwise reports the refusal.
    fn check(&mut self) -> Result<()> {
        let Some(reason) = self.pipeline().error() else {
            return Ok(());
        };

        let failed = self.elements[0].0;
        if let Some(packets) = self.replay.take() {
            if self.elements.len() > 1 {
                self.elements.remove(0);
                if self.launch().is_ok() {
                    for packet in &packets {
                        self.pipeline().push(&packet.data, packet.pts, Some(packet.dts), None).map_err(decode_error)?;
                    }
                    self.replay = Some(packets);
                    return Ok(());
                }
            }
        }

        Err(Error::Decode {
            encoding: Encoding::H264,
            reason: format!("{failed}: {reason}"),
        })
    }
}

fn decode_error(reason: String) -> Error {
    Error::Decode {
        encoding: Encoding::H264,
        reason,
    }
}

impl Decoder for GStreamerDecoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn decode(&mut self, packet: &Packet) -> Result<Option<Frame>> {
        if self.ended {
            self.reset()?;
        }

        if !packet.is_empty() {
            self.pipeline().push(&packet.data, packet.pts, Some(packet.dts), None).map_err(decode_error)?;
            if let Some(replay) = &mut self.replay {
                replay.push(packet.clone());
            }
        }

        self.take_output(Duration::ZERO)?;
        Ok(self.ready.pop_front())
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        if !self.ended {
            self.pipeline().end();
            self.ended = true;
        }

        let deadline = Instant::now() + DRAIN;
        while !self.pipeline().is_finished() && Instant::now() < deadline {
            self.take_output(Duration::from_millis(50))?;
        }
        self.take_output(Duration::ZERO)?;

        Ok(self.ready.drain(..).collect())
    }

    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.replay = Some(Vec::new());
        self.launch().map_err(decode_error)
    }

    fn is_hardware(&self) -> bool {
        self.elements[0].1
    }
}

/// The H.264 encoder this machine's GStreamer has, to §15's spec.
pub struct GStreamerEncoder {
    pipeline: Pipeline,
    element: &'static str,
    hardware: bool,
    width: u32,
    height: u32,
    frame_duration: Duration,
    record: Option<Vec<u8>>,
    packets: Vec<Packet>,
    ended: bool,
}

impl GStreamerEncoder {
    /// An H.264 encoder for `config`.
    ///
    /// # Errors
    ///
    /// [`Error::NoEncoder`] for anything but H.264, or a machine whose
    /// GStreamer has no H.264 encoder under the configured [`Hardware`].
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        if config.encoding != Encoding::H264 {
            return Err(Error::NoEncoder {
                encoding: config.encoding,
                remedy: "GStreamer writes H.264 for vtome; AV1 is rav1e's".to_string(),
            });
        }

        let no_encoder = |remedy: String| Error::NoEncoder {
            encoding: Encoding::H264,
            remedy,
        };

        let api = api().map_err(no_encoder)?;
        if !has_element(api, "h264parse") {
            return Err(no_encoder(format!("GStreamer has no h264parse: {INSTALL}")));
        }

        let elements = candidates(api, &ENCODERS, config.hardware);
        if elements.is_empty() {
            return Err(no_encoder(nothing_here("encoder", config.hardware)));
        }

        let frame_rate = config.frame_rate;
        let mut source = format!(
            "video/x-raw,format=(string)NV12,width=(int){},height=(int){},framerate=(fraction){}/{}",
            config.width,
            config.height,
            frame_rate.numerator.max(1),
            frame_rate.denominator.max(1),
        );
        if let Some(colorimetry) = colorimetry(config.color) {
            let _ = write!(source, ",colorimetry=(string){colorimetry}");
        }

        let mut refusal = String::new();
        for (element, hardware) in elements {
            let description = format!(
                "appsrc name=src format=time is-live=false caps=\"{source}\" ! videoconvert ! \
                 {} ! h264parse config-interval=-1 ! {} ! appsink name=sink sync=false",
                settings(element, config),
                output_caps(element),
            );

            match Pipeline::launch(api, &description) {
                Ok(pipeline) => {
                    let fps = frame_rate.as_f64();
                    return Ok(GStreamerEncoder {
                        pipeline,
                        element,
                        hardware,
                        width: config.width,
                        height: config.height,
                        frame_duration: if fps > 0.0 { Duration::from_secs_f64(1.0 / fps) } else { Duration::ZERO },
                        record: None,
                        packets: Vec::new(),
                        ended: false,
                    });
                }
                Err(reason) => refusal = format!("{element}: {reason}"),
            }
        }

        Err(no_encoder(refusal))
    }

    /// Takes what has come out, waiting up to `timeout` for the first of it.
    fn take_output(&mut self, timeout: Duration) -> Result<()> {
        let mut wait = timeout;

        while let Some(sample) = self.pipeline.pull(wait) {
            wait = Duration::ZERO;

            let Some(bytes) = sample.bytes() else {
                continue;
            };
            let unit = AccessUnit::split(&bytes);

            if self.record.is_none() {
                if let (Some(sps), Some(pps)) = (unit.sps.clone(), unit.pps.clone()) {
                    self.record = Some(AvcConfig::new(sps, pps).to_record());
                }
            }
            if unit.data.is_empty() {
                continue;
            }

            let pts = sample.pts().unwrap_or_default();
            self.packets.push(Packet {
                track_id: 1,
                data: unit.data,
                pts,
                dts: pts,
                is_keyframe: unit.is_keyframe,
            });
        }

        match self.pipeline.error() {
            Some(reason) => Err(Error::Encode {
                reason: format!("{}: {reason}", self.element),
            }),
            None => Ok(()),
        }
    }

    /// The picture as NV12 in GStreamer's default layout.
    fn layout(&self, frame: &Frame) -> Result<Vec<u8>> {
        let nv12 = crate::scale::to_nv12(frame)?;
        let (planes, size) = default_layout(PixelFormat::Nv12, self.width as usize, self.height as usize);
        let mut data = vec![0_u8; size];

        for (index, rows) in [(0, self.height), (1, self.height.div_ceil(2))] {
            let Plane { offset, stride } = planes[index];
            for row in 0..rows {
                let source = nv12.row(index, row).unwrap_or_default();
                let start = offset + row as usize * stride;
                data[start..start + source.len()].copy_from_slice(source);
            }
        }

        Ok(data)
    }
}

/// The element and the properties that hold it to §15: a keyframe exactly
/// every interval, no B-frames, the quality's bitrate.
fn settings(element: &str, config: &EncoderConfig) -> String {
    let keyframes = config.keyframe_frames();
    let kilobits = (config.bitrate() / 1000).max(1);

    match element {
        "vah264enc" => format!("vah264enc bitrate={kilobits} key-int-max={keyframes} b-frames=0 rate-control=vbr"),
        "vaapih264enc" => {
            format!("vaapih264enc bitrate={kilobits} keyframe-period={keyframes} max-bframes=0 rate-control=vbr")
        }
        "nvh264enc" => format!("nvh264enc bitrate={kilobits} gop-size={keyframes} bframes=0 rc-mode=vbr"),
        "x264enc" => {
            // Scene cuts off and the minimum interval pinned, so keyframes
            // fall on the interval and nowhere else.
            let mut options = format!("scenecut=0:min-keyint={keyframes}");
            if config.level == Level::L4_1 {
                options.push_str(":level=4.1");
            }
            let (preset, tune) = if config.realtime { ("veryfast", " tune=zerolatency") } else { ("medium", "") };
            // x264 takes a quality, so it is given one rather than the
            // bitrate that stands in for it elsewhere — which starves a small
            // picture: a 64×48 proxy would get 7 kb/s.
            format!(
                "x264enc pass=qual quantizer={} key-int-max={keyframes} bframes=0 b-adapt=false \
                 speed-preset={preset}{tune} threads={} option-string=\"{options}\"",
                constant_quality(config.quality),
                config.threads
            )
        }
        _ => format!(
            "openh264enc bitrate={} gop-size={keyframes} rate-control=bitrate",
            config.bitrate().max(1)
        ),
    }
}

/// 0.0–1.0 quality onto x264's constant-quality scale, 0–50 best first, as
/// rav1e's quantizer is mapped: 0.68 lands at 16, a 0.3 proxy at 35.
fn constant_quality(quality: f32) -> u32 {
    (50.0 * (1.0 - quality.clamp(0.0, 1.0))).round() as u32
}

/// What the encoder is asked to write: Annex B access units, in High profile
/// where it can.
fn output_caps(element: &str) -> &'static str {
    match element {
        // openh264 negotiates the profiles it has; asking for High would make
        // it refuse rather than fall back.
        "openh264enc" => "video/x-h264,stream-format=(string)byte-stream,alignment=(string)au",
        _ => "video/x-h264,stream-format=(string)byte-stream,alignment=(string)au,profile=(string)high",
    }
}

/// The colorimetry name GStreamer knows the colour space by, where it has one.
fn colorimetry(color: ColorSpace) -> Option<&'static str> {
    match (color.matrix, color.range) {
        (Matrix::Bt709, Range::Limited) => Some("bt709"),
        (Matrix::Bt601, Range::Limited) => Some("bt601"),
        (Matrix::Bt2020Ncl, Range::Limited) => Some("bt2020"),
        _ => None,
    }
}

impl Encoder for GStreamerEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::H264
    }

    fn backend(&self) -> Backend {
        Backend::GStreamer
    }

    fn is_hardware(&self) -> bool {
        self.hardware
    }

    fn config_record(&self) -> Option<Vec<u8>> {
        self.record.clone()
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err(Error::Encode {
                reason: format!(
                    "a {}×{} picture handed to an encoder opened for {}×{}",
                    frame.width(),
                    frame.height(),
                    self.width,
                    self.height
                ),
            });
        }
        if self.ended {
            return Err(Error::Encode {
                reason: "a picture handed to an encoder that has already finished".to_string(),
            });
        }

        let data = self.layout(frame)?;
        self.pipeline
            .push(&data, frame.pts(), None, Some(self.frame_duration))
            .map_err(|reason| Error::Encode { reason })?;

        self.take_output(Duration::ZERO)?;
        Ok(std::mem::take(&mut self.packets))
    }

    fn finish(&mut self) -> Result<Vec<Packet>> {
        if !self.ended {
            self.pipeline.end();
            self.ended = true;
        }

        let deadline = Instant::now() + DRAIN;
        while !self.pipeline.is_finished() && Instant::now() < deadline {
            self.take_output(Duration::from_millis(50))?;
        }
        self.take_output(Duration::ZERO)?;

        Ok(std::mem::take(&mut self.packets))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout GStreamer gives a picture nobody asked to pad: rows to four
    /// bytes, chroma after an even number of rows.
    #[test]
    fn the_default_layout_is_gstreamers() {
        let (planes, size) = default_layout(PixelFormat::Nv12, 1920, 1080);
        assert_eq!((planes[1].offset, planes[1].stride, size), (1920 * 1080, 1920, 1920 * 1080 * 3 / 2));

        // 318 wide pads to 320; 239 rows round to 240 before the chroma.
        let (planes, size) = default_layout(PixelFormat::I420, 318, 239);
        assert_eq!(planes[0].stride, 320);
        assert_eq!(planes[1], Plane { offset: 320 * 240, stride: 160 });
        assert_eq!(planes[2], Plane { offset: 320 * 240 + 160 * 120, stride: 160 });
        assert_eq!(size, 320 * 240 + 2 * 160 * 120);
    }

    #[test]
    fn quality_maps_onto_constant_quality_best_first() {
        assert_eq!(constant_quality(1.0), 0);
        assert_eq!(constant_quality(0.68), 16);
        assert_eq!(constant_quality(0.3), 35);
        assert_eq!(constant_quality(0.0), 50);
    }

    /// Whatever this machine has, asking cannot crash, and a refusal says what
    /// to install.
    #[test]
    fn a_machine_without_gstreamer_is_told_what_to_install() {
        if let Err(reason) = api() {
            assert!(reason.contains("gstreamer1.0"), "{reason}");
        }
    }

    /// Hardware is a filter on the same list, never a different list.
    #[test]
    fn hardware_narrows_the_candidates() {
        let Ok(api) = api() else {
            return;
        };

        for (name, hardware) in candidates(api, &DECODERS, Hardware::Require) {
            assert!(hardware, "{name}");
        }
        for (name, hardware) in candidates(api, &ENCODERS, Hardware::Off) {
            assert!(!hardware, "{name}");
        }
    }
}
