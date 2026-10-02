//! AV1 through `rav1e`, the encoder bundled with every transcode build.
//!
//! Pure Rust, royalty-free, and slow: rav1e is the reason `import` writes a
//! proxy first and queues the real encode (see `planning/TODO.md` §14). It is
//! what vtome writes where the operating system has no H.264 encoder to use —
//! Linux, today — and whenever AV1 is asked for.
//!
//! Shaped like the H.264 files where §15's spec applies: 8-bit 4:2:0, a
//! keyframe exactly every two seconds with no scene-cut keyframes between, and
//! low-latency mode, so no frame is held back to be shown out of order —
//! AV1's equivalent of "no B-frames".

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use rav1e::prelude::{
    ChromaSamplePosition, ChromaSampling, ColorDescription, ColorPrimaries, Config,
    Context, EncoderConfig as Rav1eConfig, EncoderStatus, FrameType, MatrixCoefficients,
    PixelRange, Rational, SpeedSettings, TransferCharacteristics,
};

use crate::color::{ColorSpace, Matrix, Primaries, Range, Transfer};
use crate::decode::Hardware;
use crate::encode::{Backend, Encoder, EncoderConfig};
use crate::error::{Error, Result};
use crate::frame::Frame;
use crate::identify::Encoding;
use crate::media::Packet;

/// AV1 through rav1e.
pub struct Rav1eEncoder {
    context: Context<u8>,
    /// The timestamp of each picture handed in and not yet out, by its number.
    pending: VecDeque<(u64, Duration)>,
    frames_in: u64,
    width: u32,
    height: u32,
    record: Vec<u8>,
    flushed: bool,
}

impl Rav1eEncoder {
    /// An AV1 encoder for `config`.
    ///
    /// # Errors
    ///
    /// [`Error::NoEncoder`] for anything but AV1, or with hardware required —
    /// rav1e is software; [`Error::Encode`] for a configuration rav1e refuses.
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        if config.encoding != Encoding::Av1 {
            return Err(Error::NoEncoder {
                encoding: config.encoding,
                remedy: "rav1e writes AV1; H.264 is only ever the operating system's".to_string(),
            });
        }

        if config.hardware == Hardware::Require {
            return Err(Error::NoEncoder {
                encoding: Encoding::Av1,
                remedy: "hardware encoding was required, and rav1e encodes in software"
                    .to_string(),
            });
        }

        let keyframes = u64::from(config.keyframe_frames());

        let mut settings = Rav1eConfig::with_speed_preset(if config.realtime { 10 } else { 6 });
        settings.width = config.width as usize;
        settings.height = config.height as usize;
        // One tick a frame: the frame rate turned over.
        settings.time_base = Rational::new(
            u64::from(config.frame_rate.denominator),
            u64::from(config.frame_rate.numerator.max(1)),
        );
        settings.bit_depth = 8;
        settings.chroma_sampling = ChromaSampling::Cs420;
        settings.chroma_sample_position = ChromaSamplePosition::Unknown;
        settings.pixel_range = match config.color.range {
            Range::Limited => PixelRange::Limited,
            Range::Full => PixelRange::Full,
        };
        settings.color_description = Some(color_description(config.color));
        // Exactly every two seconds: with the minimum at the maximum, no
        // scene cut can put a keyframe anywhere else.
        settings.min_key_frame_interval = keyframes;
        settings.max_key_frame_interval = keyframes;
        settings.low_latency = true;
        settings.quantizer = quantizer(config.quality);
        settings.speed_settings = SpeedSettings::from_preset(if config.realtime { 10 } else { 6 });

        let threads = if config.threads == 0 {
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
        } else {
            config.threads
        };

        let context = Config::new()
            .with_encoder_config(settings)
            .with_threads(threads)
            .new_context::<u8>()
            .map_err(|error| encode_error(format!("rav1e refused the configuration: {error}")))?;

        let record = context.container_sequence_header();

        Ok(Rav1eEncoder {
            context,
            pending: VecDeque::new(),
            frames_in: 0,
            width: config.width,
            height: config.height,
            record,
            flushed: false,
        })
    }

    /// Every packet rav1e has finished.
    fn drain(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();

        loop {
            match self.context.receive_packet() {
                Ok(packet) => {
                    // Shown in the order they went in: low latency holds
                    // nothing back to reorder.
                    let pts = loop {
                        match self.pending.pop_front() {
                            Some((number, pts)) if number == packet.input_frameno => break pts,
                            Some(_) => continue,
                            None => break Duration::ZERO,
                        }
                    };

                    packets.push(Packet {
                        track_id: 1,
                        data: packet.data,
                        pts,
                        dts: pts,
                        is_keyframe: packet.frame_type == FrameType::KEY,
                    });
                }
                Err(EncoderStatus::Encoded) => {}
                Err(EncoderStatus::NeedMoreData | EncoderStatus::LimitReached) => break,
                Err(other) => return Err(encode_error(format!("rav1e: {other}"))),
            }
        }

        Ok(packets)
    }
}

impl Encoder for Rav1eEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::Av1
    }

    fn backend(&self) -> Backend {
        Backend::Rav1e
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn config_record(&self) -> Option<Vec<u8>> {
        Some(self.record.clone())
    }

    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        if self.flushed {
            return Err(encode_error("a picture handed to an encoder already finished"));
        }

        if (frame.width(), frame.height()) != (self.width, self.height) {
            return Err(encode_error(format!(
                "a {}×{} picture handed to an encoder opened for {}×{}",
                frame.width(),
                frame.height(),
                self.width,
                self.height
            )));
        }

        let planar = crate::scale::to_i420(frame)?;
        let mut picture = self.context.new_frame();

        for (index, plane) in picture.planes.iter_mut().enumerate() {
            let data = planar.plane_data(index).ok_or_else(|| Error::BadFrame {
                reason: format!("plane {index} is missing"),
            })?;
            plane.copy_from_raw_u8(data, planar.planes()[index].stride, 1);
            // rav1e pads only a frame it holds the sole reference to, and the
            // retry below keeps a second one — so pad here, every time.
            plane.pad(self.width as usize, self.height as usize);
        }

        self.pending.push_back((self.frames_in, frame.pts()));
        self.frames_in += 1;

        let picture = Arc::new(picture);
        let mut packets = Vec::new();

        loop {
            match self.context.send_frame(Arc::clone(&picture)) {
                Ok(()) => break,
                // Full: take what is finished, then hand the picture over again.
                Err(EncoderStatus::EnoughData) => packets.extend(self.drain()?),
                Err(other) => return Err(encode_error(format!("rav1e: {other}"))),
            }
        }

        packets.extend(self.drain()?);
        Ok(packets)
    }

    fn finish(&mut self) -> Result<Vec<Packet>> {
        if !self.flushed {
            self.context.flush();
            self.flushed = true;
        }

        self.drain()
    }
}

/// 0.0–1.0 quality onto rav1e's 0–255 quantizer, best first: 0.68 lands
/// near rav1e's own default of 100, a proxy's 0.3 well above it.
fn quantizer(quality: f32) -> usize {
    (255.0 * (1.0 - quality.clamp(0.0, 1.0))).round().clamp(0.0, 255.0) as usize
}

/// The H.273 colour description, in rav1e's names.
fn color_description(color: ColorSpace) -> ColorDescription {
    ColorDescription {
        color_primaries: match color.primaries {
            Primaries::Bt709 => ColorPrimaries::BT709,
            Primaries::Bt601_625 => ColorPrimaries::BT470BG,
            Primaries::Bt601_525 => ColorPrimaries::BT601,
            Primaries::Bt2020 => ColorPrimaries::BT2020,
        },
        transfer_characteristics: match color.transfer {
            Transfer::Bt709 => TransferCharacteristics::BT709,
            Transfer::Srgb => TransferCharacteristics::SRGB,
            Transfer::Pq => TransferCharacteristics::SMPTE2084,
            Transfer::Hlg => TransferCharacteristics::HLG,
        },
        matrix_coefficients: match color.matrix {
            Matrix::Identity => MatrixCoefficients::Identity,
            Matrix::Bt709 => MatrixCoefficients::BT709,
            Matrix::Bt601 => MatrixCoefficients::BT601,
            Matrix::Bt2020Ncl => MatrixCoefficients::BT2020NCL,
        },
    }
}

fn encode_error(reason: impl Into<String>) -> Error {
    Error::Encode {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::PixelFormat;
    use crate::media::Rational as Ratio;

    fn frame(index: u64) -> Frame {
        let (width, height) = (64_u32, 48_u32);
        let mut data: Vec<u8> = (0..width * height).map(|i| ((i + index as u32) % 256) as u8).collect();
        data.extend(std::iter::repeat_n(128, (width * height / 2) as usize));

        Frame::packed(
            width,
            height,
            PixelFormat::Nv12,
            ColorSpace::guess_for(1920, 1080),
            Duration::from_millis(index * 100),
            data,
        )
        .unwrap()
    }

    /// Ten frames a second and a keyframe every two seconds: frames 0 and 20,
    /// and no reordering — every packet comes out with its own picture's time.
    #[test]
    fn keyframes_land_exactly_every_two_seconds_in_order() {
        let mut config = EncoderConfig::new(
            Encoding::Av1,
            64,
            48,
            Ratio::new(10, 1),
            ColorSpace::guess_for(1920, 1080),
        );
        config.realtime = true;
        config.threads = 2;

        let mut encoder = Rav1eEncoder::new(&config).unwrap();
        let mut packets = Vec::new();

        for index in 0..30 {
            packets.extend(encoder.encode(&frame(index)).unwrap());
        }
        packets.extend(encoder.finish().unwrap());

        assert_eq!(packets.len(), 30);

        let keyframes: Vec<usize> = packets
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet.is_keyframe)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(keyframes, [0, 20]);

        for (index, packet) in packets.iter().enumerate() {
            assert_eq!(packet.pts, Duration::from_millis(index as u64 * 100));
        }

        // av1C: marker and version, then the sequence header OBU.
        let record = encoder.config_record().unwrap();
        assert_eq!(record[0], 0x81, "av1C version 1");
    }

    #[test]
    fn hardware_cannot_be_required_of_a_software_encoder() {
        let mut config = EncoderConfig::new(
            Encoding::Av1,
            64,
            48,
            Ratio::new(10, 1),
            ColorSpace::default(),
        );
        config.hardware = Hardware::Require;

        assert!(matches!(Rav1eEncoder::new(&config), Err(Error::NoEncoder { .. })));
    }

    #[test]
    fn quality_maps_onto_the_quantizer_best_first() {
        assert_eq!(quantizer(1.0), 0);
        assert_eq!(quantizer(0.0), 255);
        assert!(quantizer(0.68) < quantizer(0.3));
    }
}
