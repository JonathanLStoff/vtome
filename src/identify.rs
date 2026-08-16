//! What a file is, read from its first bytes rather than its name.
//!
//! An extension is a claim, not evidence: `.mp4` on a Matroska file is common
//! enough that trusting it produces a demuxer error three layers down instead
//! of a clear answer here. Magic bytes are cheap and they do not lie.
//!
//! ```
//! use vtome::identify::{identify_bytes, Container};
//!
//! let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
//! assert_eq!(identify_bytes(&png), Some(Container::Png));
//! assert!(Container::Png.is_image());
//! ```

use std::fmt;
use std::path::Path;

use crate::error::{Error, Result};

/// How many bytes identification wants. A little more than any signature needs,
/// because Matroska's DocType — the difference between `.mkv` and `.webm` — sits
/// past the EBML header rather than at a fixed offset.
pub const PROBE_BYTES: usize = 4096;

/// The wrapper a picture or a video arrives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Container {
    /// ISO base media: `.mp4`, `.m4v`, `.mov`.
    Mp4,
    /// Matroska, the general case.
    Matroska,
    /// Matroska restricted to royalty-free codecs, which is what vtome writes.
    WebM,
    /// AVI. Old, common in archives, and not something this crate writes.
    Avi,
    /// Ogg, usually Theora if it holds video at all.
    Ogg,
    /// AV1 in an ISOBMFF still image — an image by structure, video by codec.
    Avif,
    /// HEIC/HEIF, the same idea with HEVC inside.
    Heif,
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF, which may be animated.
    Gif,
    /// WebP, which may be animated.
    WebP,
    /// BMP.
    Bmp,
    /// TIFF.
    Tiff,
}

impl Container {
    /// Whether this is a still-image (or animated-image) format, handled
    /// through the `image` feature rather than a video decoder.
    ///
    /// AVIF and HEIF are *not* in this set: they are pictures, but the picture
    /// inside is an AV1 or HEVC keyframe, so they go down the video path.
    pub fn is_image(self) -> bool {
        matches!(
            self,
            Container::Png
                | Container::Jpeg
                | Container::Gif
                | Container::WebP
                | Container::Bmp
                | Container::Tiff
        )
    }

    /// Whether this crate can demux it.
    ///
    /// AVI and Ogg identify but do not open: writing demuxers for containers
    /// nothing modern produces is not where the effort goes, and saying so here
    /// is better than a parse error later.
    pub fn is_demuxable(self) -> bool {
        matches!(
            self,
            Container::Mp4
                | Container::Matroska
                | Container::WebM
                | Container::Avif
                | Container::Heif
        )
    }

    /// The usual extension, for messages and for naming an output file.
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Matroska => "mkv",
            Container::WebM => "webm",
            Container::Avi => "avi",
            Container::Ogg => "ogv",
            Container::Avif => "avif",
            Container::Heif => "heic",
            Container::Png => "png",
            Container::Jpeg => "jpg",
            Container::Gif => "gif",
            Container::WebP => "webp",
            Container::Bmp => "bmp",
            Container::Tiff => "tiff",
        }
    }
}

impl fmt::Display for Container {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Container::Mp4 => "MP4",
            Container::Matroska => "Matroska",
            Container::WebM => "WebM",
            Container::Avi => "AVI",
            Container::Ogg => "Ogg",
            Container::Avif => "AVIF",
            Container::Heif => "HEIF",
            Container::Png => "PNG",
            Container::Jpeg => "JPEG",
            Container::Gif => "GIF",
            Container::WebP => "WebP",
            Container::Bmp => "BMP",
            Container::Tiff => "TIFF",
        };

        formatter.write_str(name)
    }
}

/// How the pictures inside are compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Encoding {
    /// H.264 / AVC. Patent-pooled: decodable through the OS, never written.
    H264,
    /// H.265 / HEVC. Patent-pooled twice over, for the same treatment.
    H265,
    /// AV1. Royalty-free, and what vtome writes by default.
    Av1,
    /// VP9. Royalty-free, and the fallback for hardware that predates AV1.
    Vp9,
    /// VP8. Royalty-free, superseded by VP9.
    Vp8,
    /// Theora. Royalty-free and obsolete; identified, not decoded.
    Theora,
    /// Apple ProRes. Intermediate format, not a delivery one.
    ProRes,
    /// MPEG-4 Part 2 (DivX/Xvid).
    Mpeg4Part2,
    /// Motion JPEG.
    MotionJpeg,
    /// A still image, decoded by the `image` feature.
    Still,
}

impl Encoding {
    /// Whether using this costs a licence fee.
    ///
    /// The whole reason this crate exists: everything it *writes* answers true
    /// here, and everything that answers false is read through a decoder the
    /// operating system already licensed.
    pub fn is_royalty_free(self) -> bool {
        matches!(
            self,
            Encoding::Av1 | Encoding::Vp9 | Encoding::Vp8 | Encoding::Theora | Encoding::Still
        )
    }

    /// Whether vtome can be asked to *produce* this.
    ///
    /// Deliberately narrower than [`is_royalty_free`](Encoding::is_royalty_free):
    /// Theora is free of charge and still not worth writing in 2026.
    pub fn is_encodable(self) -> bool {
        matches!(self, Encoding::Av1 | Encoding::Vp9)
    }

    /// The four-character code this encoding appears as in an MP4 sample entry.
    pub fn fourcc(self) -> &'static str {
        match self {
            Encoding::H264 => "avc1",
            Encoding::H265 => "hvc1",
            Encoding::Av1 => "av01",
            Encoding::Vp9 => "vp09",
            Encoding::Vp8 => "vp08",
            Encoding::Theora => "theo",
            Encoding::ProRes => "apcn",
            Encoding::Mpeg4Part2 => "mp4v",
            Encoding::MotionJpeg => "mjpg",
            Encoding::Still => "still",
        }
    }

    /// The encoding a codec string names, as MP4 sample entries and Matroska
    /// codec IDs spell it.
    ///
    /// Case-insensitive, and tolerant of the several spellings each format has
    /// collected — `hvc1` and `hev1` are both HEVC, `V_AV1` and `av01` are both
    /// AV1.
    pub fn from_codec_id(id: &str) -> Option<Self> {
        let id = id.to_ascii_lowercase();
        let id = id.trim();

        Some(match id {
            "avc1" | "avc3" | "h264" | "v_mpeg4/iso/avc" => Encoding::H264,
            "hvc1" | "hev1" | "h265" | "hevc" | "v_mpegh/iso/hevc" => Encoding::H265,
            "av01" | "av1" | "v_av1" => Encoding::Av1,
            "vp09" | "vp9" | "v_vp9" => Encoding::Vp9,
            "vp08" | "vp8" | "v_vp8" => Encoding::Vp8,
            "theo" | "v_theora" => Encoding::Theora,
            "mp4v" | "v_mpeg4/iso/asp" => Encoding::Mpeg4Part2,
            "mjpg" | "mjpeg" | "v_mjpeg" => Encoding::MotionJpeg,
            _ if id.starts_with("apc") || id == "ap4h" => Encoding::ProRes,
            _ => return None,
        })
    }
}

impl fmt::Display for Encoding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Encoding::H264 => "H.264",
            Encoding::H265 => "HEVC",
            Encoding::Av1 => "AV1",
            Encoding::Vp9 => "VP9",
            Encoding::Vp8 => "VP8",
            Encoding::Theora => "Theora",
            Encoding::ProRes => "ProRes",
            Encoding::Mpeg4Part2 => "MPEG-4 Part 2",
            Encoding::MotionJpeg => "Motion JPEG",
            Encoding::Still => "still image",
        };

        formatter.write_str(name)
    }
}

/// What container these bytes are, if any.
///
/// Wants [`PROBE_BYTES`] to be sure about Matroska; shorter input still
/// identifies everything whose signature fits in what was given.
pub fn identify_bytes(bytes: &[u8]) -> Option<Container> {
    if bytes.len() < 4 {
        return None;
    }

    // ISOBMFF: the size field comes first and the type second, so the
    // signature is at offset 4 rather than 0. The brand at offset 8 then says
    // whether this is a film or a still picture wearing the same structure.
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];

        return Some(match brand {
            b"avif" | b"avis" => Container::Avif,
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"mif1" | b"msf1" => Container::Heif,
            _ => Container::Mp4,
        });
    }

    // EBML. Matroska and WebM share it; the DocType string decides which, and
    // it is a few bytes in rather than at a fixed offset, so this looks for it
    // in the header rather than computing its way there.
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let header = &bytes[..bytes.len().min(PROBE_BYTES)];

        return Some(if contains(header, b"webm") {
            Container::WebM
        } else {
            Container::Matroska
        });
    }

    // RIFF carries several unrelated formats; the form type at offset 8 is
    // what distinguishes them.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") {
        return match &bytes[8..12] {
            b"AVI " => Some(Container::Avi),
            b"WEBP" => Some(Container::WebP),
            _ => None,
        };
    }

    if bytes.starts_with(b"OggS") {
        return Some(Container::Ogg);
    }

    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(Container::Png);
    }

    // Every JPEG variant starts SOI then a marker.
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Container::Jpeg);
    }

    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(Container::Gif);
    }

    if bytes.starts_with(b"BM") {
        return Some(Container::Bmp);
    }

    // Both byte orders.
    if bytes.starts_with(&[0x49, 0x49, 0x2A, 0x00]) || bytes.starts_with(&[0x4D, 0x4D, 0x00, 0x2A])
    {
        return Some(Container::Tiff);
    }

    None
}

/// What container a file is.
///
/// Reads [`PROBE_BYTES`] from the front and nothing else — identification never
/// costs a full read, however large the file.
///
/// # Errors
///
/// [`Error::Io`] if the file cannot be read, [`Error::UnknownContainer`] if
/// nothing recognises it.
pub fn identify_path(path: impl AsRef<Path>) -> Result<Container> {
    use std::io::Read;

    let path = path.as_ref();
    let mut file = std::fs::File::open(path).map_err(|error| Error::io(path, error))?;

    let mut probe = vec![0_u8; PROBE_BYTES];
    let read = read_up_to(&mut file, &mut probe).map_err(|error| Error::io(path, error))?;
    probe.truncate(read);

    // A short file is not an error on its own — a 1 KB GIF is a GIF — so this
    // reports on what it read rather than on how much there was.
    let _ = &mut file as &mut dyn Read;

    identify_bytes(&probe).ok_or_else(|| Error::UnknownContainer {
        path: path.to_path_buf(),
        found: describe_unknown(&probe),
    })
}

/// `Read::read` is allowed to return less than asked for without being at the
/// end, which would make a signature check fail on a slow pipe rather than on
/// the bytes.
fn read_up_to(mut reader: impl std::io::Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;

    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }

    Ok(filled)
}

/// The first few bytes in hex, so an unrecognised file says something useful
/// rather than only that it was unrecognised.
fn describe_unknown(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "empty file".to_string();
    }

    let head: Vec<String> = bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();

    format!("starts {}", head.join(" "))
}

/// `slice::contains` is for single elements; this is the subsequence version,
/// which the standard library does not have.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }

    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0x20];
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&[0; 16]);
        bytes
    }

    #[test]
    fn isobmff_is_split_by_its_brand() {
        assert_eq!(identify_bytes(&ftyp(b"isom")), Some(Container::Mp4));
        assert_eq!(identify_bytes(&ftyp(b"mp42")), Some(Container::Mp4));
        assert_eq!(identify_bytes(&ftyp(b"qt  ")), Some(Container::Mp4));
        assert_eq!(identify_bytes(&ftyp(b"avif")), Some(Container::Avif));
        assert_eq!(identify_bytes(&ftyp(b"heic")), Some(Container::Heif));
    }

    /// The DocType is what separates the two, and it is not at a fixed offset.
    #[test]
    fn matroska_and_webm_are_told_apart_by_doctype() {
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
        mkv.extend_from_slice(&[0x01; 20]);
        mkv.extend_from_slice(b"matroska");

        let mut webm = vec![0x1A, 0x45, 0xDF, 0xA3];
        webm.extend_from_slice(&[0x01; 20]);
        webm.extend_from_slice(b"webm");

        assert_eq!(identify_bytes(&mkv), Some(Container::Matroska));
        assert_eq!(identify_bytes(&webm), Some(Container::WebM));
    }

    /// RIFF is a family, not a format: the same first four bytes are an AVI, a
    /// WebP, or a WAV, and only the form type says which.
    #[test]
    fn riff_needs_its_form_type() {
        let mut avi = b"RIFF".to_vec();
        avi.extend_from_slice(&[0; 4]);
        avi.extend_from_slice(b"AVI ");

        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0; 4]);
        webp.extend_from_slice(b"WEBP");

        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0; 4]);
        wav.extend_from_slice(b"WAVE");

        assert_eq!(identify_bytes(&avi), Some(Container::Avi));
        assert_eq!(identify_bytes(&webp), Some(Container::WebP));
        assert_eq!(identify_bytes(&wav), None, "audio is not vtome's business");
    }

    #[test]
    fn the_still_formats_identify() {
        let cases: &[(&[u8], Container)] = &[
            (
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                Container::Png,
            ),
            (&[0xFF, 0xD8, 0xFF, 0xE0], Container::Jpeg),
            (b"GIF89a  ", Container::Gif),
            (b"BM   ", Container::Bmp),
            (&[0x49, 0x49, 0x2A, 0x00], Container::Tiff),
            (&[0x4D, 0x4D, 0x00, 0x2A], Container::Tiff),
        ];

        for (bytes, expected) in cases {
            assert_eq!(identify_bytes(bytes), Some(*expected), "{expected}");
            assert!(expected.is_image(), "{expected} should be an image");
        }
    }

    #[test]
    fn nothing_recognises_nothing() {
        assert_eq!(identify_bytes(b""), None);
        assert_eq!(identify_bytes(b"not a media file at all"), None);
    }

    #[test]
    fn a_file_is_identified_from_its_bytes_not_its_name() {
        let temp = tempfile::TempDir::new().unwrap();

        // A PNG called .mp4, which is the case the extension gets wrong.
        let path = temp.path().join("misleading.mp4");
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0; 64]);
        std::fs::write(&path, &bytes).unwrap();

        assert_eq!(identify_path(&path).unwrap(), Container::Png);
    }

    #[test]
    fn an_unidentifiable_file_says_what_it_starts_with() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("mystery.bin");
        std::fs::write(&path, b"\x01\x02\x03\x04nothing").unwrap();

        let Err(Error::UnknownContainer { found, .. }) = identify_path(&path) else {
            panic!("that is not a container");
        };

        assert!(found.contains("01 02 03 04"), "{found}");
    }

    #[test]
    fn codec_ids_map_from_both_worlds() {
        assert_eq!(Encoding::from_codec_id("avc1"), Some(Encoding::H264));
        assert_eq!(
            Encoding::from_codec_id("V_MPEG4/ISO/AVC"),
            Some(Encoding::H264)
        );
        assert_eq!(Encoding::from_codec_id("hev1"), Some(Encoding::H265));
        assert_eq!(Encoding::from_codec_id("V_AV1"), Some(Encoding::Av1));
        assert_eq!(Encoding::from_codec_id("vp09"), Some(Encoding::Vp9));
        assert_eq!(Encoding::from_codec_id("nonsense"), None);
    }

    /// The crate's reason for existing, as an assertion rather than a comment.
    #[test]
    fn only_royalty_free_encodings_are_encodable() {
        for encoding in [Encoding::H264, Encoding::H265, Encoding::ProRes] {
            assert!(!encoding.is_royalty_free(), "{encoding}");
            assert!(!encoding.is_encodable(), "{encoding} must never be written");
        }

        for encoding in [Encoding::Av1, Encoding::Vp9] {
            assert!(
                encoding.is_royalty_free() && encoding.is_encodable(),
                "{encoding}"
            );
        }
    }
}
