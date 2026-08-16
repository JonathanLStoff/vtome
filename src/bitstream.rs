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
