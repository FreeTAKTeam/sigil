mod audio;
mod bss;
mod capture;
mod config;
mod correlation;
mod identity;
mod output;
mod processing;
mod publisher;
mod radio;
mod replay;
mod telemetry;
mod transcription;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};

use capture::Clip;
use output::{Output, Source, frequency_label, log};
use transcription::{Backend, Recognizer};

const DEFAULT_FREQUENCY_HZ: u32 = 435_995_000;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct CommonArgs {
    /// Offline speech recognition backend.
    #[arg(long, global = true, value_enum, default_value_t = Backend::Whisper)]
    backend: Backend,
    /// Local model file or Parakeet model directory.
    #[arg(long, global = true)]
    model: Option<PathBuf>,
    /// Receive frequency in Hz. In WAV modes this labels the output.
    #[arg(long, global = true, default_value_t = DEFAULT_FREQUENCY_HZ, value_parser = clap::value_parser!(u32).range(1..))]
    frequency_hz: u32,
    /// Directory for transcript JSON and optional audio.
    #[arg(long, global = true, default_value = "transcripts")]
    output_dir: PathBuf,
    /// Save each clip as 16 kHz mono WAV.
    #[arg(long, global = true)]
    save_wav: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Receive analog NFM continuously from a USB RTL-SDR.
    Listen {
        #[command(flatten)]
        settings: radio::Settings,
        /// Gateway settings for parallel processing and embedded RCH publishing.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Decode BSS on the same channel as voice.
        #[arg(long, group = "bss_mode")]
        bss: bool,
        /// Receive BSS packets only, on the selected frequency.
        #[arg(long, group = "bss_mode")]
        bss_only: bool,
        /// Save failed HDLC candidates with their recovered bits.
        #[arg(long, requires = "bss_mode")]
        bss_diagnostics: bool,
        /// Write unfiltered 32 kHz FM discriminator audio to a new WAV.
        #[arg(long, requires = "bss_mode")]
        bss_capture: Option<PathBuf>,
        /// Maximum trailing-ID correlation window in milliseconds.
        #[arg(long, value_parser = clap::value_parser!(u64).range(200..=30_000))]
        correlation_timeout_ms: Option<u64>,
    },
    /// Transcribe a local WAV file.
    Wav {
        /// PCM or float WAV (8–192 kHz, up to five minutes).
        input: PathBuf,
    },
    /// Decode BSS from a demodulated audio WAV.
    BssWav {
        input: PathBuf,
        /// Save failed HDLC candidates with their recovered bits.
        #[arg(long)]
        bss_diagnostics: bool,
    },
    /// Replay one 32 kHz discriminator WAV through the voice and BSS workers.
    GatewayWav {
        input: PathBuf,
        /// Squelch-open interval in the recording, in `START_MS:END_MS` form. Repeat as needed.
        #[arg(long, required = true)]
        rf_open: Vec<replay::CarrierInterval>,
        #[arg(long, default_value_t = 5_000, value_parser = clap::value_parser!(u64).range(200..=30_000))]
        correlation_timeout_ms: u64,
    },
}

fn main() -> Result<()> {
    let Cli { common, command } = Cli::parse();
    let model = common
        .model
        .as_deref()
        .unwrap_or_else(|| common.backend.default_model());
    let output = Output::new(common.output_dir, common.frequency_hz, common.save_wav)?;
    match command {
        Command::Listen {
            settings,
            config,
            bss,
            bss_only,
            bss_diagnostics,
            bss_capture,
            correlation_timeout_ms,
        } => {
            settings.validate()?;
            let configured = config.is_some();
            let config = config
                .as_deref()
                .map(config::Config::load)
                .transpose()?
                .unwrap_or_default();
            let recognizer = (!bss_only).then_some((common.backend, model));
            let data = (bss
                || bss_only
                || config.rch.enabled
                || (configured && config.processing.parallel_voice_bss))
                .then(|| {
                    bss::Decoder::new(
                        radio::IF_RATE,
                        Source::Live,
                        bss_diagnostics,
                        bss_capture.as_deref(),
                    )
                })
                .transpose()?;
            let publication = config
                .rch
                .enabled
                .then(|| publisher::Publisher::open(&config))
                .transpose()?;
            listen(
                &output,
                common.frequency_hz,
                &settings,
                recognizer,
                data,
                correlation_timeout_ms.unwrap_or(config.processing.correlation_timeout_ms),
                publication,
            )
        }
        Command::Wav { input } => {
            let audio = audio::read_wav(&input)?;
            let mut recognizer = Recognizer::load(common.backend, model)?;
            log(format_args!("Transcribing WAV {}", input.display()));
            let now = Utc::now();
            output
                .transcribe(
                    &mut recognizer,
                    &Clip {
                        started_at: now,
                        ended_at: now,
                        first_frame: 0,
                        signal_first_frame: 0,
                        signal_last_frame: 0,
                        audio,
                    },
                    Source::Wav,
                )
                .map(drop)
        }
        Command::BssWav {
            input,
            bss_diagnostics,
        } => bss_wav(&input, bss_diagnostics, &output),
        Command::GatewayWav {
            input,
            rf_open,
            correlation_timeout_ms,
        } => replay::run(
            &input,
            &rf_open,
            correlation_timeout_ms,
            &output,
            Recognizer::load(common.backend, model)?,
        ),
    }
}

fn bss_wav(path: &Path, diagnostics: bool, output: &Output) -> Result<()> {
    let (audio, sample_rate) = audio::read_wav_native(path)?;
    let (audio, sample_rate) = if yodel::SampleRate::new(sample_rate).is_err() {
        (audio::resample(audio, sample_rate)?, audio::SAMPLE_RATE)
    } else {
        (audio, sample_rate)
    };
    let mut decoder = bss::Decoder::new(sample_rate, Source::Wav, diagnostics, None)?;
    log(format_args!("Decoding BSS from WAV {}", path.display()));
    for chunk in audio.chunks(capture::AUDIO_FRAME_LEN) {
        decoder.feed(chunk, Utc::now(), output)?;
    }
    decoder.finish()
}

fn listen(
    output: &Output,
    frequency_hz: u32,
    settings: &radio::Settings,
    recognition: Option<(Backend, &Path)>,
    data: Option<bss::Decoder>,
    timeout_ms: u64,
    publication: Option<publisher::Publisher<'_>>,
) -> Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stopped);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .context("install Ctrl+C handler")?;

    let (receiver, gain) = radio::open(frequency_hz, settings)?;
    let demodulator = radio::Nfm::new(settings.squelch_dbfs)?;
    let frequency = frequency_label(frequency_hz);
    log(format_args!(
        "Tuned RTL-SDR device {} to {frequency} ({frequency_hz} Hz), NFM, gain {gain}",
        settings.device_index,
    ));
    if settings.squelch_dbfs.is_none() {
        log("Calibrating squelch on the idle channel");
    }
    if data.is_some() {
        log(format_args!("BSS decoder enabled on {frequency}"));
    }

    let transcribe = recognition.map(|(backend, model)| {
        let mut recognizer = None;
        move |clip: &Clip| {
            let recognizer = match &mut recognizer {
                Some(loaded) => loaded,
                empty => empty.insert(Recognizer::load(backend, model)?),
            };
            output.transcribe(recognizer, clip, Source::Live)
        }
    });
    let publisher_stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        let stopped_publishing = &publisher_stopped;
        let worker = publication.map(|mut publisher| {
            scope.spawn(move || publisher.run(output.directory(), stopped_publishing))
        });
        let result = processing::run(
            output,
            Source::Live,
            timeout_ms,
            transcribe,
            data,
            |ingress| {
                radio::receive(
                    &receiver,
                    demodulator,
                    settings.squelch_dbfs.is_none(),
                    settings.monitor_power,
                    &stopped,
                    |event| {
                        match event {
                            radio::Event::Ready => log(format_args!("Listening on {frequency}")),
                            radio::Event::Frame {
                                open,
                                audio,
                                discriminator_audio,
                                at,
                            } => {
                                ingress.push(open, audio, discriminator_audio, at);
                            }
                        }
                        Ok(())
                    },
                )
            },
        );
        publisher_stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = worker {
            worker.thread().unpark();
        }
        result
    })?;
    log("Receiver stopped");
    Ok(())
}
