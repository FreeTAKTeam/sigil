use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use hound::{SampleFormat, WavSpec, WavWriter};
use serde::Serialize;
use yodel::ax25::fcs::crc16_x25;
use yodel::{AfskDemodulator, Bit, DemodulatorConfig, NrziDecoder, SampleRate};

use crate::capture::FRAME_MS;
#[cfg(test)]
use crate::capture::{AUDIO_FRAME_LEN, Clip};
use crate::output::{Output, Source};

const BSS_MARKER: u8 = 0x01;
const BIT_RATE: u64 = 1_200;
const FLAG: u8 = 0x7e;
const MAX_FRAME_BITS: usize = 4_096;
const DEDUP_SECONDS: u64 = 10;
const MASK_GUARD_FRAMES: u64 = 3;

#[derive(Debug, Default, PartialEq, Serialize)]
struct Packet {
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vr_n76_header_hex: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unknown_fields: Vec<RawField>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Location {
    raw_hex: String,
    encoding: &'static str,
    #[serde(flatten)]
    position: Option<Position>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, serde::Deserialize)]
pub struct Position {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_kmh: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<u16>,
}

pub struct Decoded {
    pub id: String,
    pub at: DateTime<Utc>,
    pub frames: Range<u64>,
    pub sender: Option<String>,
    pub position: Option<Position>,
    pub duplicate: bool,
}

impl Location {
    fn parse(value: &[u8]) -> Self {
        let position = Position::parse(value);
        Self {
            raw_hex: hex::encode_upper(value),
            encoding: if position.is_some() {
                "bss_fixed_point"
            } else {
                "unresolved"
            },
            position,
        }
    }
}

impl Position {
    fn parse(value: &[u8]) -> Option<Self> {
        let (latitude_bytes, rest) = value.split_first_chunk::<3>()?;
        let (longitude_bytes, motion) = rest.split_first_chunk::<3>()?;
        // SDRmm documents signed 24-bit coordinates in 1/500 arcminutes.
        let degrees = |&[high, middle, low]: &[u8; 3]| {
            let upper = i32::from(i8::from_be_bytes([high]));
            let units = (upper << 16) | (i32::from(middle) << 8) | i32::from(low);
            f64::from(units) / 30_000.0
        };
        let latitude = degrees(latitude_bytes);
        let longitude = degrees(longitude_bytes);
        if latitude.abs() > 90.0 || longitude.abs() > 180.0 {
            return None;
        }
        let mut position = Self {
            latitude,
            longitude,
            altitude_m: None,
            speed_kmh: None,
            heading_deg: None,
        };
        if let Some(&[alt_hi, alt_lo, speed_hi, speed_lo, heading_hi, heading_lo]) =
            motion.first_chunk::<6>()
        {
            let altitude = i16::from_be_bytes([alt_hi, alt_lo]);
            let speed = u16::from_be_bytes([speed_hi, speed_lo]);
            let heading = u16::from_be_bytes([heading_hi, heading_lo]);
            position.altitude_m = (altitude != i16::MIN).then_some(altitude);
            position.speed_kmh = (speed != u16::MAX).then_some(speed);
            position.heading_deg = (heading != u16::MAX).then_some(heading);
        }
        Some(position)
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct RawField {
    #[serde(rename = "type")]
    field_type: u8,
    raw_hex: String,
}

fn parse(payload: &[u8]) -> Result<Packet> {
    let (marker, mut fields) = payload.split_first().context("empty packet")?;
    ensure!(
        *marker == BSS_MARKER && !fields.is_empty(),
        "invalid BSS marker or empty body"
    );
    let mut packet = Packet::default();
    // A CRC-verified VR-N76 packet had four opaque bytes after 0x8b,
    // immediately before the sender field. Preserve the complete header.
    if let [0x8b, _, _, _, _, sender_len, 0x20, ..] = fields
        && (2..=fields.len() - 6).contains(&usize::from(*sender_len))
        && fields.len() <= 0x8b
    {
        packet.vr_n76_header_hex = Some(hex::encode_upper(&fields[..5]));
        fields = &fields[5..];
    }

    while let Some((&len, rest)) = fields.split_first() {
        if len == 0x85 {
            let (id, tail) = rest
                .split_first_chunk::<2>()
                .context("truncated BSS message ID")?;
            packet.message_id = Some(u16::from_be_bytes(*id));
            fields = tail;
            continue;
        }
        ensure!(len != 0, "zero BSS field length");
        let (field, tail) = rest
            .split_at_checked(usize::from(len))
            .context("truncated BSS field")?;
        let (&field_type, value) = field
            .split_first()
            .context("BSS field is missing its type")?;
        match field_type {
            0x20 => packet.sender = Some(text(value)),
            0x21 => packet.destination = Some(text(value)),
            0x24 => packet.text = Some(text(value)),
            0x25 => packet.location = Some(Location::parse(value)),
            0x27 => packet.location_request = Some(text(value)),
            0x28 => packet.call_request = Some(text(value)),
            _ => packet.unknown_fields.push(RawField {
                field_type,
                raw_hex: hex::encode_upper(value),
            }),
        }
        fields = tail;
    }
    Ok(packet)
}

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

#[derive(Serialize)]
struct Event<'a> {
    schema: &'static str,
    protocol: &'static str,
    timestamp: DateTime<Utc>,
    frequency_hz: u32,
    source: Source,
    audio_sample_offset: u64,
    sample_rate_hz: u32,
    payload_hex: String,
    #[serde(flatten)]
    packet: &'a Packet,
}

#[derive(Serialize)]
struct Diagnostic {
    schema: &'static str,
    timestamp: DateTime<Utc>,
    frequency_hz: u32,
    source: Source,
    audio_sample_offset: u64,
    sample_rate_hz: u32,
    error: String,
    hdlc_bits: String,
}

struct Frame {
    samples: Range<u64>,
    bits: Vec<u8>,
    error: Option<&'static str>,
    preamble_flags: usize,
}

// HDLC framing for arbitrary packet bodies including BSS payloads shorter than
// the AX.25 minimum. Yodel supplies the AFSK, NRZI, and CRC-16/X.25 layers.
#[derive(Default)]
struct Framer {
    shift: u8,
    first_flag: Option<u64>,
    bits: Vec<u8>,
    preamble_flags: usize,
}

impl Framer {
    fn push(&mut self, bit: u8, sample: u64, flag_samples: u64) -> Option<Frame> {
        self.shift = (self.shift << 1) | bit;
        if self.first_flag.is_some() && self.bits.len() <= MAX_FRAME_BITS + 8 {
            self.bits.push(bit);
        }
        if self.shift & 0x7f == 0x7f {
            let start = self.first_flag.take()?;
            let bits = std::mem::take(&mut self.bits);
            let preamble_flags = std::mem::take(&mut self.preamble_flags);
            return (bits.len() > 8).then_some(Frame {
                samples: start..sample,
                bits,
                error: Some("HDLC abort"),
                preamble_flags,
            });
        }
        if self.shift != FLAG {
            return None;
        }
        let flag_start = sample.saturating_sub(flag_samples);
        if self.bits.len() <= 8 {
            self.first_flag.get_or_insert(flag_start);
            self.bits.clear();
            self.preamble_flags = self.preamble_flags.saturating_add(1);
            return None;
        }
        let start = self.first_flag.replace(flag_start)?;
        let mut bits = std::mem::take(&mut self.bits);
        let preamble_flags = std::mem::replace(&mut self.preamble_flags, 1);
        let error = (bits.len() > MAX_FRAME_BITS + 8).then_some("HDLC frame exceeds capture limit");
        if error.is_none() {
            bits.truncate(bits.len() - 8);
        }
        Some(Frame {
            samples: start..sample,
            bits,
            error,
            preamble_flags,
        })
    }
}

fn frame_bytes(bits: &[u8]) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(bits.len() / 8);
    let mut ones = 0;
    let mut offset = 0;
    let mut value = 0;
    for &bit in bits {
        if ones == 5 {
            ensure!(bit == 0, "invalid HDLC bit stuffing");
            ones = 0;
            continue;
        }
        ones = if bit == 1 { ones + 1 } else { 0 };
        value |= bit << offset;
        offset += 1;
        if offset == 8 {
            bytes.push(value);
            value = 0;
            offset = 0;
        }
    }
    ensure!(
        offset == 0 && ones != 5,
        "HDLC frame is truncated or not byte-aligned"
    );
    ensure!(bytes.len() > 2, "HDLC frame is missing its payload or FCS");
    let fcs = bytes.split_off(bytes.len() - 2);
    ensure!(
        crc16_x25(&bytes).to_le_bytes().as_slice() == fcs,
        "HDLC CRC-16/X.25 mismatch"
    );
    Ok(bytes)
}

pub struct Decoder {
    demod: AfskDemodulator,
    nrzi: NrziDecoder,
    framer: Framer,
    sample: u64,
    sample_rate: u32,
    source: Source,
    diagnostics: bool,
    recent: VecDeque<(u64, Vec<u8>, String)>,
    voice_masks: VecDeque<Range<u64>>,
    recording: Option<WavWriter<BufWriter<File>>>,
    antenna: Option<(f64, f64)>,
}

/// Great-circle distance in metres between two positions in degrees.
pub fn ground_distance_m(from: (f64, f64), to: (f64, f64)) -> f64 {
    let (lat1, lat2) = (from.0.to_radians(), to.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (to.1 - from.1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * crate::intents::EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
}

impl Decoder {
    pub fn new(
        sample_rate: u32,
        source: Source,
        diagnostics: bool,
        record: Option<&Path>,
    ) -> Result<Self> {
        let demod = Self::demodulator(sample_rate)?;
        let recording = record
            .map(|path| -> Result<_> {
                let file = File::options()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .with_context(|| format!("create discriminator WAV {}", path.display()))?;
                Ok(WavWriter::new(
                    BufWriter::new(file),
                    WavSpec {
                        channels: 1,
                        sample_rate,
                        bits_per_sample: 32,
                        sample_format: SampleFormat::Float,
                    },
                )?)
            })
            .transpose()?;
        Ok(Self {
            demod,
            nrzi: NrziDecoder::new(Bit::One),
            framer: Framer::default(),
            sample: 0,
            sample_rate,
            source,
            diagnostics,
            recent: VecDeque::new(),
            voice_masks: VecDeque::new(),
            recording,
            antenna: None,
        })
    }

    /// Logs each packet's distance from the receiving antenna.
    #[must_use]
    pub fn with_antenna(mut self, antenna: Option<(f64, f64)>) -> Self {
        self.antenna = antenna;
        self
    }

    fn demodulator(sample_rate: u32) -> Result<AfskDemodulator> {
        let model_rate =
            SampleRate::new(sample_rate).map_err(|e| anyhow::anyhow!("AFSK sample rate: {e:?}"))?;
        let settings = DemodulatorConfig::bell_202(model_rate)
            .map_err(|e| anyhow::anyhow!("AFSK configuration: {e:?}"))?;
        AfskDemodulator::new(settings).map_err(|e| anyhow::anyhow!("AFSK demodulator: {e:?}"))
    }

    pub fn skip_to_frame(&mut self, index: u64) -> Result<()> {
        let sample = index * (u64::from(self.sample_rate) * FRAME_MS as u64 / 1_000);
        if let Some(recording) = &mut self.recording {
            for _ in self.sample..sample {
                recording.write_sample(0.0_f32)?;
            }
        }
        self.sample = sample;
        self.demod = Self::demodulator(self.sample_rate)?;
        self.nrzi = NrziDecoder::new(Bit::One);
        self.framer = Framer::default();
        Ok(())
    }

    pub fn take_voice_masks(&mut self) -> VecDeque<Range<u64>> {
        std::mem::take(&mut self.voice_masks)
    }

    pub fn feed(
        &mut self,
        audio: &[f32],
        at: DateTime<Utc>,
        output: &Output,
    ) -> Result<Vec<Decoded>> {
        let flag_samples = (8 * u64::from(self.sample_rate)).div_ceil(BIT_RATE);
        let mut decoded = Vec::new();
        for &value in audio {
            if let Some(recording) = &mut self.recording {
                recording.write_sample(value)?;
            }
            self.sample += 1;
            let Some(level) = self.demod.push_sample_f32(value) else {
                continue;
            };
            let bit = u8::from(self.nrzi.decode(level) == Bit::One);
            if let Some(frame) = self.framer.push(bit, self.sample, flag_samples)
                && let Some(packet) = self.handle_frame(&frame, at, output)?
            {
                decoded.push(packet);
            }
        }
        let current = self.frame_index(self.sample);
        self.voice_masks
            .retain(|region| current.saturating_sub(region.end) < 2_000);
        Ok(decoded)
    }

    fn handle_frame(
        &mut self,
        frame: &Frame,
        at: DateTime<Utc>,
        output: &Output,
    ) -> Result<Option<Decoded>> {
        let candidate = frame.bits.starts_with(&[1, 0, 0, 0, 0, 0, 0, 0]);
        if !candidate && (!self.diagnostics || frame.preamble_flags < 3) {
            return Ok(None);
        }
        let result = frame.error.map_or_else(
            || frame_bytes(&frame.bits),
            |message| Err(anyhow::anyhow!(message)),
        );
        // Repeated flags plus the BSS marker also identify a corrupted BSS burst.
        if candidate && (result.is_ok() || frame.preamble_flags >= 3) {
            self.voice_masks.push_back(
                self.frame_index(frame.samples.start)
                    .saturating_sub(MASK_GUARD_FRAMES)
                    ..self.frame_index(frame.samples.end) + 1 + MASK_GUARD_FRAMES,
            );
        }
        match result {
            Ok(payload) if payload.first() == Some(&BSS_MARKER) => match parse(&payload) {
                Ok(packet) => self
                    .emit(&packet, payload, &frame.samples, at, output)
                    .map(Some),
                Err(error) => self
                    .diagnostic(frame, at, output, error.to_string())
                    .map(|()| None),
            },
            Ok(_) => Ok(None),
            Err(error) => self
                .diagnostic(frame, at, output, error.to_string())
                .map(|()| None),
        }
    }

    fn emit(
        &mut self,
        packet: &Packet,
        payload: Vec<u8>,
        samples: &Range<u64>,
        at: DateTime<Utc>,
        output: &Output,
    ) -> Result<Decoded> {
        let window = u64::from(self.sample_rate) * DEDUP_SECONDS;
        self.recent
            .retain(|(sample, _, _)| self.sample - sample <= window);
        let mut decoded = Decoded {
            id: String::new(),
            at,
            frames: self.frame_index(samples.start)..self.frame_index(samples.end) + 1,
            sender: packet.sender.clone(),
            position: packet.location.as_ref().and_then(|loc| loc.position),
            duplicate: false,
        };
        if let Some((_, _, id)) = self
            .recent
            .iter()
            .find(|(_, earlier, _)| earlier == &payload)
        {
            decoded.id.clone_from(id);
            decoded.duplicate = true;
            return Ok(decoded);
        }
        let event = Event {
            schema: "sdr.bss.v1",
            protocol: "BSS",
            timestamp: at,
            frequency_hz: output.frequency_hz(),
            source: self.source,
            audio_sample_offset: samples.start,
            sample_rate_hz: self.sample_rate,
            payload_hex: hex::encode_upper(&payload),
            packet,
        };
        let path = output.save_event(at, "bss", &event)?;
        decoded.id = path
            .file_stem()
            .context("BSS event filename")?
            .to_string_lossy()
            .into_owned();
        // Logged only: the distance is not part of the event.
        let distance = self
            .distance_from_antenna(decoded.position)
            .map(|metres| format!(" {metres} m from the antenna,"))
            .unwrap_or_default();
        crate::output::log(format_args!(
            "BSS sender {:?}{distance} saved to {}",
            packet.sender.as_deref().unwrap_or("(absent)"),
            path.display()
        ));
        self.recent
            .push_back((self.sample, payload, decoded.id.clone()));
        Ok(decoded)
    }

    fn distance_from_antenna(&self, position: Option<Position>) -> Option<u32> {
        let (antenna, position) = (self.antenna?, position?);
        let metres = ground_distance_m(antenna, (position.latitude, position.longitude)).round();
        // Bounded by half the Earth's circumference, so the conversion is exact.
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(metres as u32)
    }

    fn diagnostic(
        &self,
        frame: &Frame,
        at: DateTime<Utc>,
        output: &Output,
        error: String,
    ) -> Result<()> {
        if self.diagnostics {
            let event = Diagnostic {
                schema: "sdr.bss.diagnostic.v1",
                timestamp: at,
                frequency_hz: output.frequency_hz(),
                source: self.source,
                audio_sample_offset: frame.samples.start,
                sample_rate_hz: self.sample_rate,
                error,
                hdlc_bits: frame.bits.iter().map(|&b| char::from(b'0' + b)).collect(),
            };
            output.save_event(at, "bss-diagnostic", &event)?;
        }
        Ok(())
    }

    fn frame_index(&self, sample: u64) -> u64 {
        sample.saturating_sub(1) / (u64::from(self.sample_rate) * FRAME_MS as u64 / 1_000)
    }

    #[cfg(test)]
    fn remove_data(&self, clip: &mut Clip) -> bool {
        let mut remaining = 0;
        for (offset, audio) in clip.audio.chunks_mut(AUDIO_FRAME_LEN).enumerate() {
            let index = clip.first_frame + offset as u64;
            if self.voice_masks.iter().any(|range| range.contains(&index)) {
                audio.fill(0.0);
            } else {
                remaining += 1;
            }
        }
        remaining >= 5
    }

    pub fn finish(self) -> Result<()> {
        if let Some(mut wav) = self.recording {
            wav.flush()?;
            wav.finalize()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use yodel::ax25::hdlc::frame_bits;
    use yodel::{Modulator, ModulatorConfig, NrziEncoder};

    use super::*;

    const EXAMPLE: &str = "0107204B4B37565A540121062468656C6C6F072514C72DC7CDF1";

    #[test]
    fn distance_from_the_antenna() {
        // Antenna and BLACKY's fix from the 22 November 2026 range test.
        let antenna = (43.966_633, -66.125_633);
        let blacky = (43.967_467, -66.126_5);
        let metres = ground_distance_m(antenna, blacky);
        assert!((110.0..125.0).contains(&metres), "{metres}");
        assert!(ground_distance_m(antenna, antenna) < 0.001);
        // One degree of latitude is about 111.2 km.
        let degree = ground_distance_m((0.0, 0.0), (1.0, 0.0));
        assert!((degree - 111_195.0).abs() < 10.0, "{degree}");
    }

    #[test]
    fn published_bss_payload_preserves_raw_location() -> Result<()> {
        let payload = hex::decode(EXAMPLE)?;
        let packet = parse(&payload)?;
        assert_eq!(packet.sender.as_deref(), Some("KK7VZT"));
        assert_eq!(packet.destination.as_deref(), Some(""));
        assert_eq!(packet.text.as_deref(), Some("hello"));
        let location = packet.location.context("location field")?;
        assert_eq!(location.raw_hex, "14C72DC7CDF1");
        assert_eq!(location.encoding, "bss_fixed_point");
        assert!(location.position.is_some());
        Ok(())
    }

    #[test]
    fn decodes_reference_position_and_keeps_unrecognized_location_bytes() -> Result<()> {
        let location = Location::parse(&hex::decode("124F80DDAAF000640000005A")?);
        let json = serde_json::to_value(location)?;
        assert_eq!(json["latitude"], 40.0);
        assert_eq!(json["longitude"], -75.0);
        assert_eq!(json["altitude_m"], 100);
        assert_eq!(json["speed_kmh"], 0);
        assert_eq!(json["heading_deg"], 90);

        let location = Location::parse(&hex::decode("124F80DDAAF08000FFFFFFFF")?);
        let position = location.position.context("position with unknown motion")?;
        assert_eq!(position.altitude_m, None);
        assert_eq!(position.speed_kmh, None);
        assert_eq!(position.heading_deg, None);

        for invalid in ["01", "7FFFFFFFFFFF"] {
            let location = Location::parse(&hex::decode(invalid)?);
            assert_eq!(location.encoding, "unresolved");
            assert_eq!(location.raw_hex, invalid);
            assert!(location.position.is_none());
        }
        Ok(())
    }

    #[test]
    fn counter_and_unknown_fields_are_bounded() -> Result<()> {
        let packet = parse(&[1, 0x85, 0x12, 0x34, 2, 0xa2, 0xff])?;
        assert_eq!(packet.message_id, Some(0x1234));
        assert_eq!(
            packet.unknown_fields,
            vec![RawField {
                field_type: 0xa2,
                raw_hex: "FF".into()
            }],
        );
        for bad in [
            &[1, 0x85, 1][..],
            &[1, 0],
            &[1, 2, 0x25],
            &[1],
            &[2, 1, 0x25],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        Ok(())
    }

    #[test]
    fn vr_n76_header_before_sender_preserves_opaque_bytes() -> Result<()> {
        let payload = hex::decode("018B000188DE0220410D25112233445566778899AABBCC")?;
        let packet = parse(&payload)?;
        assert_eq!(packet.vr_n76_header_hex.as_deref(), Some("8B000188DE"));
        assert_eq!(packet.sender.as_deref(), Some("A"));
        assert_eq!(packet.message_id, None);
        assert_eq!(
            packet.location.map(|location| location.raw_hex).as_deref(),
            Some("112233445566778899AABBCC")
        );
        assert!(parse(&hex::decode("018B000188DE042041")?).is_err());
        Ok(())
    }

    #[test]
    fn bell_202_wav_deduplicates_frames_and_preserves_crc_failures() -> Result<()> {
        let rate = crate::audio::SAMPLE_RATE;
        let directory = tempdir()?;
        let wav_path = directory.path().join("synthetic-bss.wav");
        let payload = hex::decode(EXAMPLE)?;
        let short = [1, 2, 0x20, b'A'];
        let mut corrupted: Vec<_> = frame_bits(&payload, 32, 3).collect();
        corrupted[32 * 8 + 16] = Bit::One;
        let mut writer = WavWriter::create(
            &wav_path,
            WavSpec {
                channels: 1,
                sample_rate: rate,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )?;
        let sample_rate = SampleRate::new(rate).expect("valid sample rate");
        let config = ModulatorConfig::bell_202(sample_rate).expect("valid modem config");
        let bits = frame_bits(&payload, 32, 3)
            .chain(frame_bits(&payload, 32, 3))
            .chain(corrupted)
            .chain(frame_bits(&short, 32, 3));
        for sample in
            Modulator::new(config).f32_samples(NrziEncoder::new(Bit::One).encode_iter(bits))
        {
            writer.write_sample(sample * 0.5)?;
        }
        writer.finalize()?;

        let output = Output::new(directory.path().to_path_buf(), 435_995_000, false)?;
        let mut decoder = Decoder::new(rate, Source::Wav, true, None)?;
        let now = Utc::now();
        for chunk in crate::audio::read_wav(&wav_path)?.chunks(320) {
            decoder.feed(chunk, now, &output)?;
        }
        let mut events = Vec::new();
        let mut failed = Vec::new();
        for entry in directory.path().read_dir()? {
            let path = entry?.path();
            if path.extension().is_some_and(|value| value == "json") {
                let event: serde_json::Value = serde_json::from_reader(File::open(path)?)?;
                if event["schema"] == "sdr.bss.v1" {
                    events.push(event);
                } else {
                    failed.push(event);
                }
            }
        }
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(events.iter().any(|event| event["sender"] == "KK7VZT"));
        assert!(events.iter().any(|event| event["sender"] == "A"));
        assert!(
            failed.iter().any(|event| {
                event["error"] == "HDLC CRC-16/X.25 mismatch"
                    && !event["hdlc_bits"].as_str().unwrap_or_default().is_empty()
            }),
            "{failed:?}"
        );
        let mask = decoder
            .voice_masks
            .front()
            .expect("data frame mask")
            .clone();
        let mut pure = Clip {
            started_at: now,
            ended_at: now,
            first_frame: mask.start,
            signal_first_frame: mask.start,
            signal_last_frame: mask.end - 1,
            audio: vec![0.5; usize::try_from(mask.end - mask.start)? * AUDIO_FRAME_LEN],
        };
        assert!(!decoder.remove_data(&mut pure));
        assert!(pure.audio.iter().all(|sample| *sample == 0.0));
        let mut mixed = Clip {
            started_at: now,
            ended_at: now,
            first_frame: 0,
            signal_first_frame: 0,
            signal_last_frame: mask.end + 7,
            audio: vec![0.5; usize::try_from(mask.end + 8)? * AUDIO_FRAME_LEN],
        };
        decoder.voice_masks.clear();
        decoder.voice_masks.push_back(5..mask.end);
        assert!(decoder.remove_data(&mut mixed));
        assert_eq!(
            &mixed.audio[..5 * AUDIO_FRAME_LEN],
            &[0.5; 5 * AUDIO_FRAME_LEN]
        );
        assert!(
            mixed.audio[5 * AUDIO_FRAME_LEN..usize::try_from(mask.end)? * AUDIO_FRAME_LEN]
                .iter()
                .all(|sample| *sample == 0.0)
        );
        Ok(())
    }
}
