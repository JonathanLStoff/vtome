//! Length-prefixed and start-code bitstreams, and the conversion between them.
//!
//! H.264 and HEVC exist in two shapes. MP4 stores each NAL unit behind a length
//! field; everything else — Annex B, and every platform decoder's preferred
//! input — separates them with `00 00 01` start codes. The two are trivially
//! interconvertible and completely incompatible, and handing a decoder the
//! wrong one produces no picture and no error, which is the worst combination
//! available.
//!
//! It is also where the parameter sets live. An MP4's `avcC` box holds the SPS
//! and PPS that a decoder needs *before* the first frame; a decoder started
//! without them produces nothing until the next in-band set, which in a
//! closed-GOP file may be never.

use crate::error::{Error, Result};
use crate::identify::Encoding;

/// A four-byte start code. Three would do, but four keeps NAL units aligned and
/// is what every muxer writes.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

fn malformed(reason: impl Into<String>) -> Error {
    Error::Decode {
        encoding: Encoding::H264,
        reason: reason.into(),
    }
}

/// The parameter sets and NAL length size out of an `avcC` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcConfig {
    /// How many bytes each NAL unit's length field takes: 1, 2, or 4.
    pub nal_length_size: usize,
    /// Sequence parameter sets — the picture's dimensions, profile, and level.
    pub sequence_parameter_sets: Vec<Vec<u8>>,
    /// Picture parameter sets.
    pub picture_parameter_sets: Vec<Vec<u8>>,
}

impl AvcConfig {
    /// Parses an `avcC` decoder configuration record.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] if the record is truncated or claims a NAL length size
    /// the format does not allow.
    pub fn parse(data: &[u8]) -> Result<Self> {
        // version, profile, compatibility, level, length size, SPS count.
        if data.len() < 7 {
            return Err(malformed(format!(
                "an avcC record is at least 7 bytes; this one is {}",
                data.len()
            )));
        }

        // The low two bits of byte 4, plus one. The upper six are reserved ones.
        let nal_length_size = (data[4] & 0b11) as usize + 1;

        // 3 is legal to write down and no encoder produces it; refusing it here
        // is cheaper than a decoder misreading every length.
        if !matches!(nal_length_size, 1 | 2 | 4) {
            return Err(malformed(format!(
                "avcC claims a {nal_length_size}-byte NAL length, which is not 1, 2, or 4"
            )));
        }

        let mut cursor = 5;

        // Five bits of count, three of reserved ones.
        let sps_count = (data[cursor] & 0b0001_1111) as usize;
        cursor += 1;

        let sequence_parameter_sets = read_sets(data, &mut cursor, sps_count, "SPS")?;

        if cursor >= data.len() {
            return Err(malformed("avcC ends before its PPS count"));
        }

        let pps_count = data[cursor] as usize;
        cursor += 1;

        let picture_parameter_sets = read_sets(data, &mut cursor, pps_count, "PPS")?;

        Ok(AvcConfig {
            nal_length_size,
            sequence_parameter_sets,
            picture_parameter_sets,
        })
    }

    /// The parameter sets as an Annex B stream, ready to be pushed into a
    /// decoder ahead of the first frame.
    pub fn to_annex_b(&self) -> Vec<u8> {
        let mut output = Vec::new();

        for set in self
            .sequence_parameter_sets
            .iter()
            .chain(&self.picture_parameter_sets)
        {
            output.extend_from_slice(&START_CODE);
            output.extend_from_slice(set);
        }

        output
    }
}

/// Reads a run of `u16`-length-prefixed parameter sets.
fn read_sets(data: &[u8], cursor: &mut usize, count: usize, what: &str) -> Result<Vec<Vec<u8>>> {
    let mut sets = Vec::with_capacity(count);

    for index in 0..count {
        if *cursor + 2 > data.len() {
            return Err(malformed(format!(
                "avcC ends inside {what} {index}'s length"
            )));
        }

        let length = u16::from_be_bytes([data[*cursor], data[*cursor + 1]]) as usize;
        *cursor += 2;

        let end = cursor
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| {
                malformed(format!(
                    "{what} {index} claims {length} bytes, past the end of the record"
                ))
            })?;

        sets.push(data[*cursor..end].to_vec());
        *cursor = end;
    }

    Ok(sets)
}

/// Turns length-prefixed NAL units into an Annex B stream.
///
/// # Errors
///
/// [`Error::Decode`] if a length field runs past the end of the data, which is
/// the shape a truncated or hostile sample takes.
pub fn length_prefixed_to_annex_b(data: &[u8], nal_length_size: usize) -> Result<Vec<u8>> {
    if !matches!(nal_length_size, 1 | 2 | 4) {
        return Err(malformed(format!(
            "a NAL length field is 1, 2, or 4 bytes, not {nal_length_size}"
        )));
    }

    let mut output = Vec::with_capacity(data.len() + 8);
    let mut cursor = 0;

    while cursor < data.len() {
        if cursor + nal_length_size > data.len() {
            return Err(malformed(format!(
                "a NAL length field is cut short {} bytes from the end",
                data.len() - cursor
            )));
        }

        let mut length = 0_usize;
        for byte in &data[cursor..cursor + nal_length_size] {
            length = (length << 8) | *byte as usize;
        }
        cursor += nal_length_size;

        let end = cursor
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| {
                malformed(format!(
                    "a NAL unit claims {length} bytes with only {} left",
                    data.len() - cursor
                ))
            })?;

        // A zero-length NAL unit is legal padding in some muxers; writing a
        // start code with nothing after it would confuse the decoder more than
        // skipping it does.
        if length > 0 {
            output.extend_from_slice(&START_CODE);
            output.extend_from_slice(&data[cursor..end]);
        }

        cursor = end;
    }

    Ok(output)
}

/// Turns an Annex B stream into length-prefixed NAL units.
///
/// # Errors
///
/// [`Error::Decode`] if a NAL unit is longer than `nal_length_size` can
/// express — a 2-byte length cannot describe a 70 KB keyframe.
pub fn annex_b_to_length_prefixed(data: &[u8], nal_length_size: usize) -> Result<Vec<u8>> {
    if !matches!(nal_length_size, 1 | 2 | 4) {
        return Err(malformed(format!(
            "a NAL length field is 1, 2, or 4 bytes, not {nal_length_size}"
        )));
    }

    let mut output = Vec::with_capacity(data.len());

    for unit in annex_b_units(data) {
        let length = unit.len();
        let ceiling = match nal_length_size {
            1 => u8::MAX as usize,
            2 => u16::MAX as usize,
            _ => u32::MAX as usize,
        };

        if length > ceiling {
            return Err(malformed(format!(
                "a {length}-byte NAL unit does not fit in a {nal_length_size}-byte length field"
            )));
        }

        output.extend_from_slice(&length.to_be_bytes()[8 - nal_length_size..]);
        output.extend_from_slice(unit);
    }

    Ok(output)
}

/// The NAL units of an Annex B stream, without their start codes.
///
/// Handles both the three- and four-byte start codes, which appear in the same
/// file: muxers use four before parameter sets and three elsewhere.
pub fn annex_b_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;

    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }

    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|next| {
            // The start code of the next unit belongs to neither, and it may be
            // three bytes or four — the fourth is a leading zero that is part of
            // the code rather than of this unit's payload.
            let code_start = next - 3;
            if code_start > 0 && data[code_start - 1] == 0 {
                code_start - 1
            } else {
                code_start
            }
        })
        .chain(std::iter::once(data.len()))
        .collect();

    starts
        .into_iter()
        .zip(ends)
        .filter(|(start, end)| end > start)
        .map(move |(start, end)| &data[start..end])
}

impl AvcConfig {
    /// One SPS and one PPS with four-byte lengths — what an encoder hands
    /// back and what MP4 stores.
    pub fn new(sps: Vec<u8>, pps: Vec<u8>) -> Self {
        AvcConfig {
            nal_length_size: 4,
            sequence_parameter_sets: vec![sps],
            picture_parameter_sets: vec![pps],
        }
    }

    /// Back into an `avcC` record: what [`TrackInfo::extra_data`] holds for
    /// H.264, whichever container the parameter sets came out of.
    ///
    /// [`TrackInfo::extra_data`]: crate::media::TrackInfo::extra_data
    pub fn to_record(&self) -> Vec<u8> {
        let Some(sps) = self.sequence_parameter_sets.first().filter(|sps| sps.len() >= 4) else {
            return Vec::new();
        };

        let mut record = vec![
            1,      // configuration version
            sps[1], // profile
            sps[2], // compatibility
            sps[3], // level
            // Reserved ones, then the length size less one.
            0xFC | (self.nal_length_size.clamp(1, 4) as u8 - 1),
            0xE0 | (self.sequence_parameter_sets.len().min(31) as u8),
        ];

        for set in self.sequence_parameter_sets.iter().take(31) {
            record.extend_from_slice(&(set.len() as u16).to_be_bytes());
            record.extend_from_slice(set);
        }

        record.push(self.picture_parameter_sets.len().min(255) as u8);

        for set in self.picture_parameter_sets.iter().take(255) {
            record.extend_from_slice(&(set.len() as u16).to_be_bytes());
            record.extend_from_slice(set);
        }

        record
    }
}

/// An encoder's Annex B access unit, taken apart for MP4: the parameter sets
/// it carried in-band, and everything else as four-byte length-prefixed NAL
/// units. What Media Foundation and MediaCodec hand back, made into what
/// VideoToolbox already does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessUnit {
    /// The SPS, if this unit carried one.
    pub sps: Option<Vec<u8>>,
    /// The PPS, if this unit carried one.
    pub pps: Option<Vec<u8>>,
    /// The picture's NAL units, length-prefixed; access unit delimiters and
    /// parameter sets are left out, since MP4 keeps the latter in `avcC`.
    pub data: Vec<u8>,
    /// Whether it holds an IDR slice.
    pub is_keyframe: bool,
}

impl AccessUnit {
    /// Takes an Annex B access unit apart.
    pub fn split(annex_b: &[u8]) -> Self {
        let mut unit = AccessUnit::default();

        for nal in annex_b_units(annex_b) {
            match nal[0] & 0x1F {
                7 => unit.sps = Some(nal.to_vec()),
                8 => unit.pps = Some(nal.to_vec()),
                9 => {}
                kind => {
                    unit.is_keyframe |= kind == 5;
                    unit.data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    unit.data.extend_from_slice(nal);
                }
            }
        }

        unit
    }
}

/// What an H.264 sequence parameter set says about the pictures under it.
///
/// Read for two reasons. The colour description: an MP4 says nothing about
/// colour unless it carries a `colr` box, which most do not, and the SPS's
/// video usability information is where the encoder wrote it — reading it is
/// the difference between the right matrix and a guess by resolution, and the
/// guess calls a 640×360 proxy of a BT.709 film BT.601. And the reorder depth,
/// which says how many pictures a decoder must hold before display order is
/// settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceParameterSet {
    /// 66 baseline, 77 main, 100 high, and so on.
    pub profile_idc: u8,
    /// Ten times the level: 41 is level 4.1.
    pub level_idc: u8,
    /// 0 monochrome, 1 4:2:0, 2 4:2:2, 3 4:4:4.
    pub chroma_format_idc: u32,
    /// Luma bit depth.
    pub bit_depth: u32,
    /// Picture width once the cropping window is applied.
    pub width: u32,
    /// Picture height once the cropping window is applied.
    pub height: u32,
    /// Whether full-range samples were signalled, where the encoder said.
    pub full_range: Option<bool>,
    /// H.273 `colour_primaries`, where the encoder said.
    pub primaries: Option<u8>,
    /// H.273 `transfer_characteristics`, where the encoder said.
    pub transfer: Option<u8>,
    /// H.273 `matrix_coefficients`, where the encoder said.
    pub matrix: Option<u8>,
    /// How many pictures may come out of order — 0 for a stream without
    /// B-frames — where the encoder said.
    pub max_num_reorder_frames: Option<u32>,
}

impl SequenceParameterSet {
    /// Parses an SPS NAL unit, header byte included.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] for something that is not an SPS, or one that ends
    /// before the picture size. A video usability section cut short is not an
    /// error: what was read before the cut is kept.
    pub fn parse(nal: &[u8]) -> Result<Self> {
        let Some((&header, payload)) = nal.split_first() else {
            return Err(malformed("an empty NAL unit is not a sequence parameter set"));
        };

        if header & 0x1F != 7 {
            return Err(malformed(format!(
                "NAL unit type {} is not a sequence parameter set (7)",
                header & 0x1F
            )));
        }

        let rbsp = unescape(payload);
        let mut bits = Bits::new(&rbsp);
        let short = |_| malformed("the sequence parameter set ends before the picture size");

        let profile_idc = bits.u(8).map_err(short)? as u8;
        bits.u(8).map_err(short)?; // constraint flags and reserved bits
        let level_idc = bits.u(8).map_err(short)? as u8;
        bits.ue().map_err(short)?; // seq_parameter_set_id

        let mut chroma_format_idc = 1;
        let mut separate_colour_plane = false;
        let mut bit_depth = 8;

        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            chroma_format_idc = bits.ue().map_err(short)?;
            if chroma_format_idc == 3 {
                separate_colour_plane = bits.flag().map_err(short)?;
            }
            bit_depth = 8 + bits.ue().map_err(short)?;
            bits.ue().map_err(short)?; // bit_depth_chroma_minus8
            bits.flag().map_err(short)?; // qpprime_y_zero_transform_bypass

            if bits.flag().map_err(short)? {
                let lists = if chroma_format_idc == 3 { 12 } else { 8 };
                for list in 0..lists {
                    if bits.flag().map_err(short)? {
                        bits.skip_scaling_list(if list < 6 { 16 } else { 64 })
                            .map_err(short)?;
                    }
                }
            }
        }

        bits.ue().map_err(short)?; // log2_max_frame_num_minus4

        match bits.ue().map_err(short)? {
            0 => {
                bits.ue().map_err(short)?; // log2_max_pic_order_cnt_lsb_minus4
            }
            1 => {
                bits.flag().map_err(short)?; // delta_pic_order_always_zero
                bits.se().map_err(short)?; // offset_for_non_ref_pic
                bits.se().map_err(short)?; // offset_for_top_to_bottom_field
                let cycle = bits.ue().map_err(short)?;
                for _ in 0..cycle.min(255) {
                    bits.se().map_err(short)?;
                }
            }
            _ => {}
        }

        bits.ue().map_err(short)?; // max_num_ref_frames
        bits.flag().map_err(short)?; // gaps_in_frame_num_value_allowed

        let width_in_mbs = bits.ue().map_err(short)? + 1;
        let height_in_map_units = bits.ue().map_err(short)? + 1;
        let frame_mbs_only = bits.flag().map_err(short)?;
        if !frame_mbs_only {
            bits.flag().map_err(short)?; // mb_adaptive_frame_field
        }
        bits.flag().map_err(short)?; // direct_8x8_inference

        let mut crop = [0_u32; 4];
        if bits.flag().map_err(short)? {
            for edge in &mut crop {
                *edge = bits.ue().map_err(short)?;
            }
        }

        // The cropping window is in chroma samples, and in field pairs for
        // interlaced streams.
        let chroma_array_type = if separate_colour_plane { 0 } else { chroma_format_idc };
        let field = 2 - u32::from(frame_mbs_only);
        let (crop_x, crop_y) = match chroma_array_type {
            0 => (1, field),
            1 => (2, 2 * field),
            2 => (2, field),
            _ => (1, field),
        };

        let width = (width_in_mbs * 16).saturating_sub(crop_x * (crop[0] + crop[1]));
        let height = (field * height_in_map_units * 16).saturating_sub(crop_y * (crop[2] + crop[3]));

        let mut sps = SequenceParameterSet {
            profile_idc,
            level_idc,
            chroma_format_idc,
            bit_depth,
            width,
            height,
            full_range: None,
            primaries: None,
            transfer: None,
            matrix: None,
            max_num_reorder_frames: None,
        };

        // Video usability information. Cut short is not malformed — plenty of
        // encoders write a truncated VUI — so whatever was read stays.
        if bits.flag().unwrap_or(false) {
            let _ = sps.read_vui(&mut bits);
        }

        Ok(sps)
    }

    /// The part of the VUI this crate uses, in the order the syntax has it.
    fn read_vui(&mut self, bits: &mut Bits<'_>) -> std::result::Result<(), Short> {
        if bits.flag()? {
            // aspect_ratio_idc, and the explicit ratio when it is 255.
            if bits.u(8)? == 255 {
                bits.u(16)?;
                bits.u(16)?;
            }
        }

        if bits.flag()? {
            bits.flag()?; // overscan_appropriate
        }

        if bits.flag()? {
            bits.u(3)?; // video_format
            self.full_range = Some(bits.flag()?);

            if bits.flag()? {
                self.primaries = Some(bits.u(8)? as u8);
                self.transfer = Some(bits.u(8)? as u8);
                self.matrix = Some(bits.u(8)? as u8);
            }
        }

        if bits.flag()? {
            bits.ue()?; // chroma_sample_loc_type_top_field
            bits.ue()?; // chroma_sample_loc_type_bottom_field
        }

        if bits.flag()? {
            bits.u(32)?; // num_units_in_tick
            bits.u(32)?; // time_scale
            bits.flag()?; // fixed_frame_rate
        }

        let nal_hrd = bits.flag()?;
        if nal_hrd {
            bits.skip_hrd()?;
        }
        let vcl_hrd = bits.flag()?;
        if vcl_hrd {
            bits.skip_hrd()?;
        }
        if nal_hrd || vcl_hrd {
            bits.flag()?; // low_delay_hrd
        }

        bits.flag()?; // pic_struct_present

        if bits.flag()? {
            bits.flag()?; // motion_vectors_over_pic_boundaries
            bits.ue()?; // max_bytes_per_pic_denom
            bits.ue()?; // max_bits_per_mb_denom
            bits.ue()?; // log2_max_mv_length_horizontal
            bits.ue()?; // log2_max_mv_length_vertical
            self.max_num_reorder_frames = Some(bits.ue()?);
        }

        Ok(())
    }
}

/// Removes emulation-prevention bytes: the `03` an encoder puts after every
/// `00 00` so the payload never contains a start code.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(data.len());
    let mut zeros = 0;

    for &byte in data {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }

        zeros = if byte == 0 { zeros + 1 } else { 0 };
        output.push(byte);
    }

    output
}

/// Ran out of bits.
#[derive(Debug)]
struct Short;

/// A reader of the fixed-width and Exp-Golomb fields H.264 headers are made of.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, position: 0 }
    }

    fn bit(&mut self) -> std::result::Result<u32, Short> {
        let byte = self.data.get(self.position / 8).ok_or(Short)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Ok(u32::from(bit))
    }

    fn flag(&mut self) -> std::result::Result<bool, Short> {
        Ok(self.bit()? == 1)
    }

    /// `u(n)`, up to 32 bits.
    fn u(&mut self, count: u32) -> std::result::Result<u32, Short> {
        let mut value = 0_u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Ok(value as u32)
    }

    /// `ue(v)`: unsigned Exp-Golomb.
    fn ue(&mut self) -> std::result::Result<u32, Short> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            // 32 leading zeros is not a value any header field holds.
            if zeros > 31 {
                return Err(Short);
            }
        }

        let rest = self.u(zeros)?;
        Ok(((1_u64 << zeros) - 1 + u64::from(rest)) as u32)
    }

    /// `se(v)`: signed Exp-Golomb.
    fn se(&mut self) -> std::result::Result<i32, Short> {
        let code = i64::from(self.ue()?);
        Ok(if code % 2 == 1 { (code + 1) / 2 } else { -code / 2 } as i32)
    }

    fn skip_scaling_list(&mut self, size: usize) -> std::result::Result<(), Short> {
        let (mut last, mut next) = (8_i32, 8_i32);

        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
        }

        Ok(())
    }

    fn skip_hrd(&mut self) -> std::result::Result<(), Short> {
        let count = self.ue()? + 1;
        self.u(4)?; // bit_rate_scale
        self.u(4)?; // cpb_size_scale
        for _ in 0..count.min(32) {
            self.ue()?; // bit_rate_value_minus1
            self.ue()?; // cpb_size_value_minus1
            self.flag()?; // cbr
        }
        self.u(5)?; // initial_cpb_removal_delay_length_minus1
        self.u(5)?; // cpb_removal_delay_length_minus1
        self.u(5)?; // dpb_output_delay_length_minus1
        self.u(5)?; // time_offset_length
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avcc(nal_length_size: u8) -> Vec<u8> {
        let mut record = vec![
            1,                            // version
            0x64,                         // profile: high
            0x00,                         // compatibility
            0x28,                         // level 4.0
            0xFC | (nal_length_size - 1), // reserved ones + length size
            0xE0 | 1,                     // reserved ones + one SPS
        ];

        record.extend_from_slice(&4_u16.to_be_bytes());
        record.extend_from_slice(&[0x67, 0x64, 0x00, 0x28]);
        record.push(1); // one PPS
        record.extend_from_slice(&3_u16.to_be_bytes());
        record.extend_from_slice(&[0x68, 0xEE, 0x38]);

        record
    }

    /// `1`, `010`, `011`, `00100`: 0, 1, 2, 3 — and the signed mapping
    /// alternates 1, -1, 2.
    #[test]
    fn exp_golomb_reads_both_ways() {
        // 1 010 011 00100 010 011 00100 → ue 0 1 2 3, then se 1 -1 2.
        let data = [0b1010_0110, 0b0100_0100, 0b1100_1000];
        let mut bits = Bits::new(&data);

        assert_eq!(bits.ue().unwrap(), 0);
        assert_eq!(bits.ue().unwrap(), 1);
        assert_eq!(bits.ue().unwrap(), 2);
        assert_eq!(bits.ue().unwrap(), 3);
        assert_eq!(bits.se().unwrap(), 1);
        assert_eq!(bits.se().unwrap(), -1);
        assert_eq!(bits.se().unwrap(), 2);
        assert!(bits.u(8).is_err(), "out of bits");
    }

    /// An encoder's Annex B output, made into MP4's shape: the parameter sets
    /// lifted out, the delimiter dropped, the slice length-prefixed.
    #[test]
    fn an_annex_b_access_unit_is_taken_apart() {
        let stream = [
            0, 0, 0, 1, 0x09, 0xF0, // access unit delimiter
            0, 0, 0, 1, 0x67, 0x64, 0x00, 0x29, // SPS
            0, 0, 1, 0x68, 0xEE, // PPS, three-byte start code
            0, 0, 0, 1, 0x65, 0x88, 0x80, // IDR slice
        ];

        let unit = AccessUnit::split(&stream);

        assert_eq!(unit.sps.as_deref(), Some(&[0x67, 0x64, 0x00, 0x29][..]));
        assert_eq!(unit.pps.as_deref(), Some(&[0x68, 0xEE][..]));
        assert_eq!(unit.data, [0, 0, 0, 3, 0x65, 0x88, 0x80]);
        assert!(unit.is_keyframe);

        assert!(!AccessUnit::split(&[0, 0, 1, 0x41, 0x9A]).is_keyframe, "a P slice");
    }

    /// The `03` after two zeros is the encoder's, not the payload's.
    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 7]), [0, 0, 1, 0, 0, 0, 7]);
        assert_eq!(unescape(&[0, 3, 0, 3]), [0, 3, 0, 3], "one zero is not two");
    }

    #[test]
    fn only_a_sequence_parameter_set_is_parsed_as_one() {
        assert!(SequenceParameterSet::parse(&[0x68, 0xEE]).is_err(), "a PPS");
        assert!(SequenceParameterSet::parse(&[]).is_err());
        assert!(SequenceParameterSet::parse(&[0x67, 0x64]).is_err(), "cut short");
    }

    #[test]
    fn an_avcc_record_gives_up_its_parameter_sets() {
        let config = AvcConfig::parse(&avcc(4)).unwrap();

        assert_eq!(config.nal_length_size, 4);
        assert_eq!(config.sequence_parameter_sets.len(), 1);
        assert_eq!(config.picture_parameter_sets.len(), 1);
        assert_eq!(config.sequence_parameter_sets[0][0], 0x67);

        let annex_b = config.to_annex_b();
        assert_eq!(&annex_b[..4], &START_CODE);
        assert_eq!(annex_b.len(), 4 + 4 + 4 + 3);
    }

    #[test]
    fn a_two_byte_length_size_is_read_from_the_low_bits() {
        let config = AvcConfig::parse(&avcc(2)).unwrap();

        assert_eq!(config.nal_length_size, 2);
    }

    #[test]
    fn a_truncated_avcc_is_refused_rather_than_read_past() {
        assert!(AvcConfig::parse(&[1, 0x64, 0, 0x28]).is_err());

        let mut cut = avcc(4);
        cut.truncate(9);
        assert!(AvcConfig::parse(&cut).is_err());
    }

    /// A record claiming an SPS longer than the record: the hostile input this
    /// parser exists to survive.
    #[test]
    fn a_parameter_set_claiming_more_than_it_has_is_refused() {
        let mut record = vec![1, 0x64, 0x00, 0x28, 0xFF, 0xE1];
        record.extend_from_slice(&9999_u16.to_be_bytes());
        record.extend_from_slice(&[0x67, 0x64]);

        let Err(Error::Decode { reason, .. }) = AvcConfig::parse(&record) else {
            panic!("that SPS runs off the end");
        };

        assert!(reason.contains("past the end"), "{reason}");
    }

    #[test]
    fn the_two_bitstream_shapes_round_trip() {
        let mut length_prefixed = Vec::new();
        for unit in [vec![0x67, 0x42, 0x00], vec![0x68, 0xCE], vec![0x65; 40]] {
            length_prefixed.extend_from_slice(&(unit.len() as u32).to_be_bytes());
            length_prefixed.extend_from_slice(&unit);
        }

        let annex_b = length_prefixed_to_annex_b(&length_prefixed, 4).unwrap();
        assert_eq!(&annex_b[..4], &START_CODE);

        let back = annex_b_to_length_prefixed(&annex_b, 4).unwrap();
        assert_eq!(back, length_prefixed);
    }

    #[test]
    fn three_and_four_byte_start_codes_are_both_understood() {
        let mut stream = vec![0, 0, 0, 1, 0x67, 0x42];
        stream.extend_from_slice(&[0, 0, 1, 0x68, 0xCE, 0x01]);
        stream.extend_from_slice(&[0, 0, 0, 1, 0x65, 0xAA]);

        let units: Vec<&[u8]> = annex_b_units(&stream).collect();

        assert_eq!(units.len(), 3);
        assert_eq!(units[0], &[0x67, 0x42]);
        assert_eq!(units[1], &[0x68, 0xCE, 0x01]);
        assert_eq!(units[2], &[0x65, 0xAA]);
    }

    #[test]
    fn a_length_running_past_the_data_is_refused() {
        let hostile = [0x00, 0x00, 0xFF, 0xFF, 0x67, 0x42];

        let Err(Error::Decode { reason, .. }) = length_prefixed_to_annex_b(&hostile, 4) else {
            panic!("that NAL claims 65535 bytes and has two");
        };

        assert!(reason.contains("claims"), "{reason}");
    }

    #[test]
    fn a_nal_too_large_for_its_length_field_is_refused() {
        let mut stream = vec![0, 0, 0, 1];
        stream.extend(std::iter::repeat_n(0x65, 300));

        assert!(annex_b_to_length_prefixed(&stream, 1).is_err());
        assert!(annex_b_to_length_prefixed(&stream, 2).is_ok());
    }

    #[test]
    fn zero_length_units_are_skipped_rather_than_written_as_bare_start_codes() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0_u32.to_be_bytes());
        stream.extend_from_slice(&2_u32.to_be_bytes());
        stream.extend_from_slice(&[0x67, 0x42]);

        let annex_b = length_prefixed_to_annex_b(&stream, 4).unwrap();

        assert_eq!(annex_b, vec![0, 0, 0, 1, 0x67, 0x42]);
    }

    #[test]
    fn an_impossible_length_size_is_refused_both_ways() {
        assert!(length_prefixed_to_annex_b(&[0; 8], 3).is_err());
        assert!(annex_b_to_length_prefixed(&[0, 0, 1, 5], 3).is_err());
    }
}
