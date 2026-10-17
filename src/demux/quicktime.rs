//! QuickTime sound entries, rewritten into the ISO layout the `mp4` crate reads.
//!
//! A `.mov` from QuickTime, Final Cut and their kin describes AAC with a
//! version 1 or 2 *sound description*: 16 or 36 bytes more than the ISO
//! `mp4a` entry, and the `esds` that matters wrapped one level down in a
//! `wave` box. The `mp4` crate only knows the ISO layout, so it reads those
//! extra bytes as the next box's header, finds a nonsense size, and refuses
//! the whole file — "mp4a box contains a box with a larger size than it".
//!
//! Rather than fork the crate, the entry is rewritten before the crate sees
//! it: the ISO fields, then the `esds`, then a `free` box over whatever is
//! left. The entry keeps its exact length, so not one box size in the file
//! changes and every sample offset stays true. The rewrite lives in memory,
//! laid over the file by [`Patched`]; the file itself is never touched.

use std::io::{self, Read, Seek, SeekFrom};

/// The boxes between `moov` and the sample descriptions, in order.
const TO_STSD: [&[u8; 4]; 5] = [b"trak", b"mdia", b"minf", b"stbl", b"stsd"];

/// No `moov` larger than this is read in to be checked. Hours of video index
/// into tens of megabytes; past this, the file is left to the crate as it is.
const MOOV_LIMIT: u64 = 256 << 20;

/// Where the ISO fields of a sound entry end, from the start of its header.
const SOUND_FIELDS: usize = 36;

/// A reader with the rewritten `moov` laid over the bytes it came from.
pub(super) struct Patched<R> {
    inner: R,
    /// Where `inner` is, so a read knows which bytes it got.
    position: u64,
    /// The rewritten `moov` and where it sits in the file — `None` when the
    /// file needed nothing, and reads pass straight through.
    overlay: Option<(u64, Vec<u8>)>,
}

impl<R: Read + Seek> Patched<R> {
    /// Checks `source` for QuickTime sound entries and readies the rewrite,
    /// leaving it at the start of the file.
    ///
    /// Anything that goes wrong finding or reading `moov` just means no
    /// rewrite: a damaged file is the crate's to report, with its own error.
    pub(super) fn new(mut source: R, size: u64) -> io::Result<Self> {
        let overlay = rewritten_moov(&mut source, size);
        source.seek(SeekFrom::Start(0))?;

        Ok(Patched {
            inner: source,
            position: 0,
            overlay,
        })
    }
}

impl<R: Read> Read for Patched<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        let (from, to) = (self.position, self.position + read as u64);
        self.position = to;

        if let Some((at, bytes)) = &self.overlay {
            let (low, high) = (from.max(*at), to.min(at + bytes.len() as u64));
            if low < high {
                buf[(low - from) as usize..(high - from) as usize]
                    .copy_from_slice(&bytes[(low - at) as usize..(high - at) as usize]);
            }
        }

        Ok(read)
    }
}

impl<R: Seek> Seek for Patched<R> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        self.position = self.inner.seek(to)?;
        Ok(self.position)
    }
}

/// The file's `moov` with every QuickTime sound entry rewritten, and where it
/// goes — or `None` if there is no `moov` to read or nothing in it to change.
fn rewritten_moov<R: Read + Seek>(source: &mut R, size: u64) -> Option<(u64, Vec<u8>)> {
    let mut at = 0;

    // Top-level boxes are walked on the file, not read: `mdat` is most of it.
    while at + 8 <= size {
        source.seek(SeekFrom::Start(at)).ok()?;
        let mut head = [0; 16];
        source.read_exact(&mut head[..8]).ok()?;
        if head[..4] == 1_u32.to_be_bytes() {
            source.read_exact(&mut head[8..]).ok()?;
        }

        let atom = Atom::parse(&head, 0, usize::try_from(size - at).unwrap_or(usize::MAX))?;
        if &atom.kind == b"moov" {
            if atom.size as u64 > MOOV_LIMIT {
                return None;
            }

            let mut moov = vec![0; atom.size];
            source.seek(SeekFrom::Start(at)).ok()?;
            source.read_exact(&mut moov).ok()?;

            return rewrite(&mut moov).then_some((at, moov));
        }

        at += atom.size as u64;
    }

    None
}

/// Rewrites every QuickTime sound entry in `moov`. Returns whether any were.
fn rewrite(moov: &mut [u8]) -> bool {
    let Some(atom) = Atom::parse(moov, 0, moov.len()) else {
        return false;
    };

    let mut stsds = Vec::new();
    find_stsds(moov, atom.header, moov.len(), &mut stsds);

    let mut changed = false;
    for (start, end) in stsds {
        // Version and flags, then the entry count, then the entries.
        let entries: Vec<_> = children(moov, start + 8, end).collect();
        for (at, entry) in entries {
            if &entry.kind == b"mp4a" && entry.header == 8 {
                changed |= rewrite_sound(&mut moov[at..at + entry.size]);
            }
        }
    }

    changed
}

/// Collects the payload range of every `stsd` under `moov`, following
/// [`TO_STSD`] down.
fn find_stsds(bytes: &[u8], start: usize, end: usize, found: &mut Vec<(usize, usize)>) {
    fn descend(bytes: &[u8], start: usize, end: usize, depth: usize, found: &mut Vec<(usize, usize)>) {
        for (at, atom) in children(bytes, start, end) {
            if &atom.kind != TO_STSD[depth] {
                continue;
            }

            let (inner, outer) = (at + atom.header, at + atom.size);
            if depth + 1 == TO_STSD.len() {
                found.push((inner, outer));
            } else {
                descend(bytes, inner, outer, depth + 1, found);
            }
        }
    }

    descend(bytes, start, end, 0, found);
}

/// Rewrites one `mp4a` entry, header included, into the ISO layout in place.
/// Returns whether it needed it.
///
/// The ISO entry is the QuickTime version 0 one: the crate reads its fields,
/// then expects `esds` as the very next box. So a version 1 or 2 entry loses
/// its extension, an `esds` inside `wave` is lifted out, and an `esds` behind
/// some other box is brought to the front.
fn rewrite_sound(entry: &mut [u8]) -> bool {
    if entry.len() < SOUND_FIELDS {
        return false;
    }

    let version = u16::from_be_bytes([entry[16], entry[17]]);
    let extension = match version {
        0 => 0,
        1 => 16,
        2 => 36,
        _ => return false,
    };

    let first = SOUND_FIELDS + extension;
    if first > entry.len() {
        return false;
    }

    let esds = children(entry, first, entry.len()).find_map(|(at, atom)| match &atom.kind {
        b"esds" => Some(at..at + atom.size),
        b"wave" => children(entry, at + atom.header, at + atom.size)
            .find(|(_, inner)| &inner.kind == b"esds")
            .map(|(inner_at, inner)| inner_at..inner_at + inner.size),
        _ => None,
    });

    if version == 0 && esds.as_ref().is_none_or(|esds| esds.start == first) {
        return false;
    }

    let mut iso = entry[..SOUND_FIELDS].to_vec();
    // Version, revision, vendor; then compression id and packet size, which
    // only mean something in QuickTime.
    iso[16..24].fill(0);
    iso[28..32].fill(0);

    if version == 2 {
        // Version 2 leaves placeholders in the fields shared with version 0
        // and states the real values in its extension.
        let extension = &entry[SOUND_FIELDS..first];
        let rate = f64::from_bits(u64::from_be_bytes(extension[4..12].try_into().unwrap()));
        let channels = u32::from_be_bytes(extension[12..16].try_into().unwrap());
        let bits = u32::from_be_bytes(extension[20..24].try_into().unwrap());

        iso[24..26].copy_from_slice(&u16::try_from(channels).unwrap_or(u16::MAX).to_be_bytes());
        iso[26..28].copy_from_slice(&(if bits == 0 { 16 } else { bits as u16 }).to_be_bytes());
        // 16.16 fixed point, which cannot hold 65536 Hz or more.
        let fixed = if (0.0..65536.0).contains(&rate) { (rate as u32) << 16 } else { 0 };
        iso[32..36].copy_from_slice(&fixed.to_be_bytes());
    }

    if let Some(esds) = esds {
        iso.extend_from_slice(&entry[esds]);
    }

    // Whatever is left becomes one `free` box, which the crate skips. There is
    // always room for its header: what was dropped is either the extension
    // (16 bytes at least) or a whole box (8 at least).
    let rest = entry.len() - iso.len();
    if rest != 0 {
        if rest < 8 {
            return false;
        }
        iso.extend_from_slice(&(rest as u32).to_be_bytes());
        iso.extend_from_slice(b"free");
        iso.resize(entry.len(), 0);
    }

    entry.copy_from_slice(&iso);
    true
}

/// One box header.
struct Atom {
    kind: [u8; 4],
    /// Header length: 8, or 16 with a 64-bit size.
    header: usize,
    /// The whole box, header included.
    size: usize,
}

impl Atom {
    /// The box starting at `at`, if a whole one fits before `end`.
    fn parse(bytes: &[u8], at: usize, end: usize) -> Option<Atom> {
        let head = bytes.get(at..at + 8)?;
        let kind = head[4..8].try_into().ok()?;

        let (header, size) = match u32::from_be_bytes(head[..4].try_into().ok()?) {
            // A 64-bit size follows the type.
            1 => (16, usize::try_from(u64::from_be_bytes(bytes.get(at + 8..at + 16)?.try_into().ok()?)).ok()?),
            // Runs to the end of whatever holds it.
            0 => (8, end.checked_sub(at)?),
            size => (8, size as usize),
        };

        (size >= header && at.checked_add(size)? <= end).then_some(Atom { kind, header, size })
    }
}

/// The boxes laid end to end from `start` to `end`, each with where it starts.
/// Stops at the first one that does not fit.
fn children(bytes: &[u8], start: usize, end: usize) -> impl Iterator<Item = (usize, Atom)> + '_ {
    let mut at = start;
    std::iter::from_fn(move || {
        let atom = Atom::parse(bytes, at, end)?;
        let here = at;
        at += atom.size;
        Some((here, atom))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// The ISO fields of an `mp4a` entry: stereo, 16-bit, 48 kHz.
    fn sound_fields(version: u16) -> Vec<u8> {
        let mut fields = vec![0; 28];
        fields[7] = 1; // data reference index
        fields[8..10].copy_from_slice(&version.to_be_bytes());
        fields[16..18].copy_from_slice(&2_u16.to_be_bytes());
        fields[18..20].copy_from_slice(&16_u16.to_be_bytes());
        fields[24..28].copy_from_slice(&(48_000_u32 << 16).to_be_bytes());
        fields
    }

    fn moov_with(entry: Vec<u8>) -> Vec<u8> {
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(entry);
        let mut boxed = atom(b"stsd", &stsd);
        for kind in [b"stbl", b"minf", b"mdia", b"trak", b"moov"] {
            boxed = atom(kind, &boxed);
        }
        boxed
    }

    /// The `mp4a` entry inside a moov built by [`moov_with`].
    fn entry_of(moov: &[u8]) -> &[u8] {
        let at = 8 * 6 + 8;
        let size = u32::from_be_bytes(moov[at..at + 4].try_into().unwrap()) as usize;
        &moov[at..at + size]
    }

    /// What QuickTime writes: a version 1 entry with `esds` inside `wave`.
    #[test]
    fn a_version_1_entry_is_lifted_out_of_wave() {
        let esds = atom(b"esds", &[0, 0, 0, 0, 3, 1, 2, 3]);
        let mut wave = atom(b"frma", b"mp4a");
        wave.extend(atom(b"mp4a", &[0, 0, 0, 0]));
        wave.extend(&esds);
        wave.extend([0, 0, 0, 8, 0, 0, 0, 0]);

        let mut body = sound_fields(1);
        body.extend([0xAA; 16]);
        body.extend(atom(b"wave", &wave));
        body.extend(atom(b"chan", &[0; 16]));

        let mut moov = moov_with(atom(b"mp4a", &body));
        let before = moov.len();
        assert!(rewrite(&mut moov));
        assert_eq!(moov.len(), before, "nothing changes size");

        let entry = entry_of(&moov);
        assert_eq!(&entry[16..18], &[0, 0], "version 0");
        assert_eq!(&entry[24..36], &body[16..28], "channels, sample size, rate kept");
        assert_eq!(&entry[36..36 + esds.len()], &esds[..], "esds comes first");

        let free = Atom::parse(entry, 36 + esds.len(), entry.len()).unwrap();
        assert_eq!(&free.kind, b"free");
        assert_eq!(36 + esds.len() + free.size, entry.len(), "free fills the rest");
    }

    /// Version 2 states its real channel count and rate in the extension.
    #[test]
    fn a_version_2_entry_takes_its_values_from_the_extension() {
        let mut body = sound_fields(2);
        body[16..18].copy_from_slice(&3_u16.to_be_bytes());
        let mut extension = 72_u32.to_be_bytes().to_vec();
        extension.extend(44_100.0_f64.to_bits().to_be_bytes());
        extension.extend(6_u32.to_be_bytes());
        extension.extend(0x7F00_0000_u32.to_be_bytes());
        extension.extend(24_u32.to_be_bytes());
        extension.extend([0; 12]);
        body.extend(extension);
        body.extend(atom(b"esds", &[0; 8]));

        let mut moov = moov_with(atom(b"mp4a", &body));
        assert!(rewrite(&mut moov));

        let entry = entry_of(&moov);
        assert_eq!(u16::from_be_bytes([entry[24], entry[25]]), 6);
        assert_eq!(u16::from_be_bytes([entry[26], entry[27]]), 24);
        assert_eq!(u32::from_be_bytes(entry[32..36].try_into().unwrap()), 44_100 << 16);
        assert_eq!(&entry[40..44], b"esds");
    }

    /// An entry the crate can already read is left byte for byte.
    #[test]
    fn an_iso_entry_is_left_alone() {
        let mut body = sound_fields(0);
        body.extend(atom(b"esds", &[0; 8]));
        let mut moov = moov_with(atom(b"mp4a", &body));
        let before = moov.clone();

        assert!(!rewrite(&mut moov));
        assert_eq!(moov, before);
    }

    /// Reads see the rewrite wherever they start and however they are cut.
    #[test]
    fn the_overlay_shows_through_any_read() {
        let file: Vec<u8> = (0..64).collect();
        let mut patched = Patched {
            inner: io::Cursor::new(file.clone()),
            position: 0,
            overlay: Some((10, vec![0xFF; 20])),
        };

        let mut expected = file;
        expected[10..30].fill(0xFF);

        for start in [0, 5, 10, 29, 30] {
            patched.seek(SeekFrom::Start(start)).unwrap();
            let mut got = Vec::new();
            let mut chunk = [0; 7];
            loop {
                let read = patched.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                got.extend_from_slice(&chunk[..read]);
            }
            assert_eq!(got, expected[start as usize..]);
        }
    }
}
