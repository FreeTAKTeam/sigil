use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use rubato::{FftFixedInOut, Resampler};

pub const SAMPLE_RATE: u32 = 16_000;
const MAX_WAV_SECONDS: u32 = 5 * 60;

pub fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let (mut mono, sample_rate) = read_wav_native(path)?;
    for sample in &mut mono {
        *sample = sample.clamp(-1.0, 1.0);
    }
    resample(mono, sample_rate)
}

pub fn read_wav_native(path: &Path) -> Result<(Vec<f32>, u32)> {
    let input = File::open(path).with_context(|| format!("open WAV {}", path.display()))?;
    let mut reader = WavReader::new(BufReader::new(input))
        .with_context(|| format!("read WAV header {}", path.display()))?;
    let spec = reader.spec();
    ensure!(
        (8_000..=192_000).contains(&spec.sample_rate),
        "WAV sample rate must be 8–192 kHz"
    );
    ensure!(spec.channels != 0, "WAV has zero channels");
    ensure!(reader.duration() != 0, "WAV contains no audio");
    ensure!(
        reader.duration() <= MAX_WAV_SECONDS * spec.sample_rate,
        "WAV exceeds five minutes"
    );

    let channels = usize::from(spec.channels);
    let mut mono = Vec::with_capacity(reader.duration() as usize);
    match spec.sample_format {
        SampleFormat::Int => {
            ensure!(
                (1..=32).contains(&spec.bits_per_sample),
                "unsupported WAV bit depth"
            );
            let scale = 2_f64.powi(1 - i32::from(spec.bits_per_sample));
            downmix(
                reader.samples::<i32>(),
                channels,
                |sample| f64::from(sample) * scale,
                &mut mono,
            )?;
        }
        SampleFormat::Float => {
            downmix(reader.samples::<f32>(), channels, f64::from, &mut mono)?;
        }
    }
    Ok((mono, spec.sample_rate))
}

#[expect(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn downmix<T>(
    samples: impl Iterator<Item = hound::Result<T>>,
    channels: usize,
    as_float: impl Fn(T) -> f64,
    mono: &mut Vec<f32>,
) -> Result<()> {
    let mut sum = 0.0;
    for (index, sample) in samples.enumerate() {
        let sample = as_float(sample.context("read WAV sample")?);
        ensure!(sample.is_finite(), "WAV contains a non-finite audio sample");
        sum += sample;
        if (index + 1).is_multiple_of(channels) {
            mono.push((sum / channels as f64) as f32);
            sum = 0.0;
        }
    }
    Ok(())
}

pub fn resample(mono: Vec<f32>, input_rate: u32) -> Result<Vec<f32>> {
    if input_rate == SAMPLE_RATE {
        return Ok(mono);
    }
    let mut resampler =
        FftFixedInOut::<f32>::new(input_rate as usize, SAMPLE_RATE as usize, 1_024, 1)?;
    let delay = resampler.output_delay();
    let target_len = (mono.len() * SAMPLE_RATE as usize).div_ceil(input_rate as usize);
    let mut output = Vec::with_capacity(delay + target_len + resampler.output_frames_max());
    let mut frames = mono.chunks_exact(resampler.input_frames_next());
    for block in frames.by_ref() {
        let mut converted = resampler.process(&[block], None)?;
        output.append(&mut converted[0]);
    }
    if !frames.remainder().is_empty() {
        let mut converted = resampler.process_partial(Some(&[frames.remainder()]), None)?;
        output.append(&mut converted[0]);
    }
    while output.len() < delay + target_len {
        let mut tail = resampler.process_partial::<&[f32]>(None, None)?;
        output.append(&mut tail[0]);
    }
    output.truncate(delay + target_len);
    output.drain(..delay);
    Ok(output)
}

pub fn write_wav(file: &mut File, audio: &[f32]) -> Result<()> {
    let mut writer = WavWriter::new(
        file,
        WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        },
    )?;
    for &sample in audio {
        #[expect(clippy::cast_possible_truncation)]
        let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        writer.write_sample(pcm)?;
    }
    writer.finalize()?;
    Ok(())
}
