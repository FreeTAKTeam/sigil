use std::ops::Range;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use chrono::{TimeDelta, Utc};

use crate::audio;
use crate::bss::Decoder;
use crate::capture::{Clip, FRAME_MS};
use crate::output::{Output, Source};
use crate::processing;
use crate::radio::{IF_FRAME_LEN, IF_RATE, VoiceAudio};
use kodiak_core::Engine;

#[derive(Clone)]
pub struct CarrierInterval(Range<u64>);

impl FromStr for CarrierInterval {
    type Err = anyhow::Error;
    fn from_str(text: &str) -> Result<Self> {
        let (start, end) = text
            .split_once(':')
            .context("RF interval must be START_MS:END_MS")?;
        let start = start.parse()?;
        let end = end.parse()?;
        ensure!(start < end, "RF interval must have positive duration");
        Ok(Self(start..end))
    }
}

pub fn run(
    path: &Path,
    carriers: &[CarrierInterval],
    timing: crate::correlation::Timing,
    output: &Output,
    engine: &Engine,
) -> Result<()> {
    let (audio, rate) = audio::read_wav_native(path)?;
    ensure!(
        rate == IF_RATE,
        "gateway replay requires a {IF_RATE} Hz discriminator WAV from --bss-capture"
    );
    let duration_ms = (audio.len() as u64 * 1_000).div_ceil(u64::from(IF_RATE));
    ensure!(
        carriers.iter().all(|carrier| carrier.0.end <= duration_ms),
        "RF interval exceeds WAV duration"
    );
    let mut voice = VoiceAudio::new()?;
    let start = Utc::now();
    let transcribe = |clip: &Clip| output.transcribe(engine, clip, Source::Wav);
    processing::run(
        output,
        Source::Wav,
        timing,
        Some(transcribe),
        Some(Decoder::new(rate, Source::Wav, false, None)?),
        |ingress| {
            for (index, raw) in audio.chunks(IF_FRAME_LEN).enumerate() {
                let t = (index * FRAME_MS) as u64;
                let mut frame = [0.0; IF_FRAME_LEN];
                frame[..raw.len()].copy_from_slice(raw);
                let open = carriers
                    .iter()
                    .any(|carrier| carrier.0.start < t + FRAME_MS as u64 && carrier.0.end > t);
                let now = start + TimeDelta::milliseconds(i64::try_from(t)?);
                ingress.push(open, voice.process(&frame)?, &frame, now);
            }
            Ok(())
        },
    )
}
