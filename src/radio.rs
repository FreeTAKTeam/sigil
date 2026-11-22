use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Args;
use librtlsdr_rs::{RtlSdrDevice, RtlSdrReader};
use sdr_dsp::correction::DcBlocker;
use sdr_dsp::demod::FmDemod;
use sdr_dsp::filter::{ComplexFirFilter, DecimatingFirFilter, DeemphasisFilter};
use sdr_dsp::multirate::PowerDecimator;
use sdr_dsp::noise::PowerSquelch;
use sdr_dsp::taps;
use sdr_types::Complex;

use crate::audio::SAMPLE_RATE;
use crate::capture::{AUDIO_FRAME_LEN, FRAME_MS};
use crate::output::log;

const USB_RATE: u32 = 1_024_000;
const IF_DOWNSAMPLE: u32 = 32;
pub const IF_RATE: u32 = USB_RATE / IF_DOWNSAMPLE;
const IQ_FRAME_LEN: usize = USB_RATE as usize * FRAME_MS / 1_000;
pub const IF_FRAME_LEN: usize = IQ_FRAME_LEN / IF_DOWNSAMPLE as usize;
const USB_FRAME_BYTES: usize = 2 * IQ_FRAME_LEN;
const AUDIO_DOWNSAMPLE: usize = (IF_RATE / SAMPLE_RATE) as usize;
const CHANNEL_BANDWIDTH_HZ: f64 = 12_500.0;
const SQUELCH_MARGIN_DB: f32 = 8.0;
const RECEIVER_SETTLE_FRAMES: usize = 5;
/// USB transfer size: the driver's default, 128 ms of I/Q at 1.024 MS/s.
const USB_TRANSFER_BYTES: usize = 16 * 32 * 512;
/// Transfers buffered between the USB reader and the DSP: about 4 s.
const USB_QUEUE: usize = 32;

#[derive(Args)]
pub struct Settings {
    /// RTL-SDR device index.
    #[arg(long, default_value_t = 0)]
    pub device_index: u32,
    /// Requested tuner gain in dB, rounded to the nearest supported step.
    #[arg(long, default_value_t = 29.7, allow_negative_numbers = true)]
    gain_db: f32,
    /// Channel squelch threshold in dBFS. Omit to calibrate at startup.
    #[arg(long, allow_negative_numbers = true)]
    pub squelch_dbfs: Option<f32>,
    /// Tuner frequency correction in parts per million.
    #[arg(long, default_value_t = 0, allow_negative_numbers = true, value_parser = clap::value_parser!(i32).range(-200..=200))]
    ppm: i32,
    /// Print peak channel level and squelch state once per second.
    #[arg(long)]
    pub monitor_power: bool,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (-10.0..=60.0).contains(&self.gain_db),
            "--gain-db must be between -10 and 60 dB"
        );
        if let Some(dbfs) = self.squelch_dbfs {
            ensure!(
                (-120.0..=0.0).contains(&dbfs),
                "--squelch-dbfs must be between -120 and 0 dBFS"
            );
        }
        Ok(())
    }
}

pub fn open(frequency_hz: u32, settings: &Settings) -> Result<(RtlSdrDevice, String)> {
    let mut device = RtlSdrDevice::open(settings.device_index)
        .with_context(|| format!("open RTL-SDR device {}", settings.device_index))?;
    device
        .set_sample_rate(USB_RATE)
        .context("set RTL-SDR I/Q sample rate")?;
    if settings.ppm != 0 {
        device
            .set_freq_correction(settings.ppm)
            .context("apply RTL-SDR frequency correction")?;
    }
    device
        .set_center_freq(frequency_hz)
        .with_context(|| format!("tune RTL-SDR to {frequency_hz} Hz"))?;
    device
        .set_tuner_gain_mode(true)
        .context("enable manual tuner gain")?;
    device
        .set_agc_mode(false)
        .context("disable RTL2832 automatic gain")?;
    // The validated user range fits in the driver's tenths-of-a-dB integer.
    #[expect(clippy::cast_possible_truncation)]
    let requested_gain = (settings.gain_db * 10.0).round() as i32;
    let gain = device
        .closest_gain(requested_gain)
        .context("read supported tuner gains")?;
    device
        .set_tuner_gain(gain)
        .context("set RTL-SDR tuner gain")?;
    device.reset_buffer().context("reset RTL-SDR USB buffer")?;
    Ok((device, format!("{:.1} dB", f64::from(gain) / 10.0)))
}

pub struct Nfm {
    decimate: PowerDecimator,
    dc_block: DcBlocker,
    channel_filter: ComplexFirFilter,
    squelch: PowerSquelch,
    discriminator: FmDemod,
    iq: Vec<Complex>,
    if_samples: [Complex; IF_FRAME_LEN],
    corrected: [Complex; IF_FRAME_LEN],
    channel: [Complex; IF_FRAME_LEN],
    squelch_buffer: [Complex; IF_FRAME_LEN],
    discriminator_audio: [f32; IF_FRAME_LEN],
    voice: VoiceAudio,
}

pub struct VoiceAudio {
    deemphasis: DeemphasisFilter,
    audio_filter: DecimatingFirFilter,
    deemphasized: [f32; IF_FRAME_LEN],
    audio: [f32; AUDIO_FRAME_LEN],
}

impl VoiceAudio {
    pub fn new() -> Result<Self> {
        let if_rate = f64::from(IF_RATE);
        Ok(Self {
            deemphasis: DeemphasisFilter::new(750e-6, if_rate)?,
            audio_filter: DecimatingFirFilter::new(
                taps::band_pass(250.0, 3_500.0, 400.0, if_rate, true)?,
                AUDIO_DOWNSAMPLE,
            )?,
            deemphasized: [0.0; IF_FRAME_LEN],
            audio: [0.0; AUDIO_FRAME_LEN],
        })
    }

    pub fn process(
        &mut self,
        discriminator: &[f32; IF_FRAME_LEN],
    ) -> Result<&[f32; AUDIO_FRAME_LEN]> {
        self.deemphasis
            .process(discriminator, &mut self.deemphasized)?;
        let count = self
            .audio_filter
            .process_f32(&self.deemphasized, &mut self.audio)?;
        ensure!(
            count == AUDIO_FRAME_LEN,
            "unexpected NFM audio frame length {count}"
        );
        Ok(&self.audio)
    }
}

impl Nfm {
    pub fn new(threshold: Option<f32>) -> Result<Self> {
        let if_rate = f64::from(IF_RATE);
        Ok(Self {
            decimate: PowerDecimator::new(IF_DOWNSAMPLE)?,
            dc_block: DcBlocker::new(0.002)?,
            channel_filter: ComplexFirFilter::new(taps::low_pass(
                CHANNEL_BANDWIDTH_HZ / 2.0,
                1_500.0,
                if_rate,
                true,
            )?)?,
            squelch: PowerSquelch::new(threshold.unwrap_or_default()),
            // Match SDR-RS's NFM discriminator scaling and retain voice-band audio.
            discriminator: FmDemod::from_hz(CHANNEL_BANDWIDTH_HZ / 2.0, if_rate)?,
            iq: vec![Complex::default(); IQ_FRAME_LEN],
            if_samples: [Complex::default(); IF_FRAME_LEN],
            corrected: [Complex::default(); IF_FRAME_LEN],
            channel: [Complex::default(); IF_FRAME_LEN],
            squelch_buffer: [Complex::default(); IF_FRAME_LEN],
            discriminator_audio: [0.0; IF_FRAME_LEN],
            voice: VoiceAudio::new()?,
        })
    }

    fn process(&mut self, bytes: &[u8]) -> Result<()> {
        for (sample, pair) in self.iq.iter_mut().zip(bytes.chunks_exact(2)) {
            *sample = Complex::new(
                (f32::from(pair[0]) - 127.5) / 127.5,
                (f32::from(pair[1]) - 127.5) / 127.5,
            );
        }
        let count = self.decimate.process(&self.iq, &mut self.if_samples)?;
        ensure!(
            count == IF_FRAME_LEN,
            "unexpected NFM IF frame length {count}"
        );
        self.dc_block
            .process(&self.if_samples, &mut self.corrected)?;
        self.channel_filter
            .process(&self.corrected, &mut self.channel)?;
        self.squelch
            .process(&self.channel, &mut self.squelch_buffer)?;
        self.discriminator
            .process(&self.channel, &mut self.discriminator_audio)?;
        self.voice.process(&self.discriminator_audio)?;
        Ok(())
    }
}

pub enum Event<'a> {
    Ready,
    Frame {
        open: bool,
        audio: &'a [f32; AUDIO_FRAME_LEN],
        discriminator_audio: &'a [f32],
        at: DateTime<Utc>,
    },
}

/// Receives I/Q until `stopped`, calling `on_event` for every 20 ms block.
///
/// A dedicated thread only reads USB transfers and hands them over, so the
/// dongle is drained continuously. Reading and processing on one thread left
/// a gap after every block in which the dongle's small buffer could overflow;
/// the lost samples shortened AFSK bits and broke BSS frames on a busy
/// computer (21 November 2026).
pub fn receive(
    device: &RtlSdrDevice,
    nfm: Nfm,
    calibrate_squelch: bool,
    monitor_power: bool,
    stopped: &AtomicBool,
    on_event: impl FnMut(Event<'_>) -> Result<()>,
) -> Result<()> {
    let reader = device.reader();
    let (filled_tx, filled_rx) = sync_channel::<(Vec<u8>, usize)>(USB_QUEUE);
    let (spare_tx, spare_rx) = sync_channel::<Vec<u8>>(USB_QUEUE + 2);
    thread::scope(|scope| {
        let usb = scope.spawn(move || read_usb(&reader, &filled_tx, &spare_rx, stopped));
        let result = process(
            nfm,
            calibrate_squelch,
            monitor_power,
            stopped,
            &filled_rx,
            &spare_tx,
            on_event,
        );
        // Unblock and stop the reader whether processing ended or failed.
        stopped.store(true, Ordering::Relaxed);
        drop(filled_rx);
        let usb = usb
            .join()
            .map_err(|_| anyhow::anyhow!("USB reader thread panicked"))?;
        result.and(usb)
    })
}

fn read_usb(
    reader: &RtlSdrReader,
    filled: &SyncSender<(Vec<u8>, usize)>,
    spare: &Receiver<Vec<u8>>,
    stopped: &AtomicBool,
) -> Result<()> {
    while !stopped.load(Ordering::Relaxed) {
        let mut buffer = spare
            .try_recv()
            .unwrap_or_else(|_| vec![0; USB_TRANSFER_BYTES]);
        let count = reader
            .read_sync(&mut buffer)
            .context("read I/Q from RTL-SDR")?;
        ensure!(count > 0, "RTL-SDR ended its I/Q stream");
        match filled.try_send((buffer, count)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // The DSP is over USB_QUEUE transfers behind: keep the
                // dongle drained and report the gap instead of stalling.
                let lost_ms = count as u64 * 1_000 / (2 * u64::from(USB_RATE));
                log(format_args!(
                    "Signal processing fell behind; dropped {lost_ms} ms of I/Q"
                ));
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    Ok(())
}

fn process(
    mut nfm: Nfm,
    calibrate_squelch: bool,
    monitor_power: bool,
    stopped: &AtomicBool,
    filled: &Receiver<(Vec<u8>, usize)>,
    spare: &SyncSender<Vec<u8>>,
    mut on_event: impl FnMut(Event<'_>) -> Result<()>,
) -> Result<()> {
    let mut frame = vec![0_u8; USB_FRAME_BYTES];
    let mut filled_bytes = 0;
    let mut blocks = 0_usize;
    let blocks_per_second = 1_000 / FRAME_MS;
    let warmup_blocks = if calibrate_squelch {
        blocks_per_second
    } else {
        RECEIVER_SETTLE_FRAMES
    };
    let mut idle_levels = Vec::with_capacity(warmup_blocks);
    let mut peak = f32::NEG_INFINITY;
    let mut open_in_interval = false;

    while !stopped.load(Ordering::Relaxed) {
        let (buffer, count) = match filled.recv_timeout(Duration::from_millis(250)) {
            Ok(transfer) => transfer,
            Err(RecvTimeoutError::Timeout) => continue,
            // The reader stopped: it was told to, or its error is joined above.
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let mut input = &buffer[..count];
        // Transfers need not align with blocks. Preserve I/Q byte pairing.
        while !input.is_empty() {
            let take = (USB_FRAME_BYTES - filled_bytes).min(input.len());
            frame[filled_bytes..filled_bytes + take].copy_from_slice(&input[..take]);
            input = &input[take..];
            filled_bytes += take;
            if filled_bytes < USB_FRAME_BYTES {
                continue;
            }
            filled_bytes = 0;
            nfm.process(&frame)?;
            blocks += 1;
            if blocks <= warmup_blocks {
                if calibrate_squelch && blocks > RECEIVER_SETTLE_FRAMES {
                    idle_levels.push(nfm.squelch.diagnostic_snapshot().last_measured_db);
                }
                if blocks == warmup_blocks {
                    if calibrate_squelch {
                        idle_levels.sort_unstable_by(f32::total_cmp);
                        let noise_floor = idle_levels[idle_levels.len() / 2];
                        let threshold = (noise_floor + SQUELCH_MARGIN_DB).clamp(-120.0, 0.0);
                        nfm.squelch.set_level(threshold);
                        log(format_args!(
                            "Idle channel {noise_floor:.1} dBFS, squelch set to {threshold:.1} dBFS"
                        ));
                    }
                    on_event(Event::Ready)?;
                }
                continue;
            }
            let open = nfm.squelch.is_open();
            on_event(Event::Frame {
                open,
                audio: &nfm.voice.audio,
                discriminator_audio: &nfm.discriminator_audio,
                at: Utc::now(),
            })?;

            if monitor_power {
                let snapshot = nfm.squelch.diagnostic_snapshot();
                peak = peak.max(snapshot.last_measured_db);
                open_in_interval |= open;
                if blocks.is_multiple_of(blocks_per_second) {
                    let state = if open_in_interval { "opened" } else { "closed" };
                    log(format_args!(
                        "Channel peak {peak:.1} dBFS, threshold {:.1} dBFS, squelch {state}",
                        snapshot.level_db
                    ));
                    peak = f32::NEG_INFINITY;
                    open_in_interval = false;
                }
            }
        }
        let _ = spare.try_send(buffer);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use super::*;

    #[test]
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn narrowband_chain_recovers_voice_band_tone_and_rejects_off_channel_carrier() -> Result<()> {
        let mut nfm = Nfm::new(Some(-32.0))?;
        let mut frame = vec![0; USB_FRAME_BYTES];
        let mut phase = 0.0;
        for carrier in [800.0, 25_000.0] {
            for _ in 0..5 {
                for (index, pair) in frame.chunks_exact_mut(2).enumerate() {
                    let time = f64::from(u32::try_from(index)?) / f64::from(USB_RATE);
                    phase += (carrier + 1_000.0 * (TAU * 1_000.0 * time).sin()) * TAU
                        / f64::from(USB_RATE);
                    let (q, i) = phase.sin_cos();
                    pair[0] = (127.5 + 55.0 * i).round() as u8;
                    pair[1] = (127.5 + 55.0 * q).round() as u8;
                }
                nfm.process(&frame)?;
            }
            if carrier < 1_000.0 {
                assert!(nfm.squelch.is_open());
                // A 1 kHz tone at 16 kHz has a 16-sample period.
                let (cosine, sine) = nfm.voice.audio.iter().zip(0_u32..).fold(
                    (0.0, 0.0),
                    |(cosine, sine), (&sample, index)| {
                        let (sin, cos) = (TAU * f64::from(index) / 16.0).sin_cos();
                        (
                            cosine + f64::from(sample) * cos,
                            sine + f64::from(sample) * sin,
                        )
                    },
                );
                assert!(cosine.hypot(sine) > 1.0, "voice-band tone is missing");
            } else {
                assert!(!nfm.squelch.is_open(), "off-channel signal opened squelch");
            }
        }
        Ok(())
    }
}
