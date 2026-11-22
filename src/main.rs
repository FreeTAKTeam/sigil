mod audio;
mod bss;
mod capture;
mod config;
mod correlation;
mod identity;
mod intents;
mod output;
mod processing;
mod publisher;
mod radio;
mod radios;
mod replay;
mod telemetry;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use kodiak_core::{Engine, WakeWord};
use kodiak_intent::DeterministicClassifier;

use capture::Clip;
use output::{Output, Source, frequency_label, log};

const DEFAULT_FREQUENCY_HZ: u32 = 435_995_000;

/// Offline speech recognition backends provided by `kodiak-asr`.
/// NVIDIA Parakeet TDT v2 (English, 0.6B, INT8) is the default when built in.
#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Whisper,
    #[cfg(feature = "parakeet")]
    Parakeet,
}

impl Backend {
    #[cfg(feature = "parakeet")]
    const DEFAULT: Self = Self::Parakeet;
    #[cfg(not(feature = "parakeet"))]
    const DEFAULT: Self = Self::Whisper;

    fn default_model(self) -> &'static Path {
        match self {
            Self::Whisper => Path::new("models/ggml-base.en.bin"),
            #[cfg(feature = "parakeet")]
            Self::Parakeet => Path::new("models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8"),
        }
    }

    fn asr(self) -> kodiak_asr::Backend {
        match self {
            Self::Whisper => kodiak_asr::Backend::Whisper,
            #[cfg(feature = "parakeet")]
            Self::Parakeet => kodiak_asr::Backend::Parakeet,
        }
    }
}

/// One Kodiak engine per process. Its speech model loads on the first clip and
/// is shared by every radio the gateway hears.
fn kodiak(
    backend: Backend,
    model: &Path,
    wake: WakeWord,
    settings: &config::Kodiak,
) -> Result<Engine> {
    let model = model.to_owned();
    let engine = match settings.classifier {
        config::Classifier::Deterministic => Engine::new(DeterministicClassifier::new()),
        #[cfg(feature = "laya")]
        config::Classifier::Laya => {
            let started = std::time::Instant::now();
            let classifier = kodiak_laya::LayaClassifier::from_dir(
                &settings.laya_checkpoint,
                kodiak_laya::Policy {
                    threshold: settings.laya_threshold,
                    mode: kodiak_laya::Mode::Hybrid,
                },
            )
            .map_err(anyhow::Error::msg)
            .context("load the laya checkpoint")?;
            log(format_args!(
                "Kodiak classifier {} loaded in {:.1} s",
                kodiak_core::IntentClassifier::id(&classifier),
                started.elapsed().as_secs_f32()
            ));
            Engine::new(classifier)
        }
        #[cfg(not(feature = "laya"))]
        config::Classifier::Laya => {
            anyhow::bail!("classifier = \"laya\" needs a build with `--features laya`")
        }
    };
    Ok(engine
        .with_wake_word(wake)
        .with_transcriber_loader(move || kodiak_asr::load(backend.asr(), &model)))
}

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
    #[arg(long, global = true, value_enum, default_value_t = Backend::DEFAULT)]
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
        /// Longest gap in milliseconds between the end of speech and a BSS
        /// packet that still identifies the speaker.
        #[arg(long, value_parser = clap::value_parser!(u64).range(0..=10_000))]
        trailing_id_gap_ms: Option<u64>,
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
    /// View and manage the radio identity database.
    Radios {
        /// Gateway settings that locate the identity registry.
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: radios::Action,
    },
    /// Replay one 32 kHz discriminator WAV through the voice and BSS workers.
    GatewayWav {
        input: PathBuf,
        /// Squelch-open interval in the recording, in `START_MS:END_MS` form. Repeat as needed.
        #[arg(long, required = true)]
        rf_open: Vec<replay::CarrierInterval>,
        #[arg(long, default_value_t = 5_000, value_parser = clap::value_parser!(u64).range(200..=30_000))]
        correlation_timeout_ms: u64,
        #[arg(long, default_value_t = correlation::Timing::DEFAULT_TRAILING_GAP_MS, value_parser = clap::value_parser!(u64).range(0..=10_000))]
        trailing_id_gap_ms: u64,
    },
}

#[expect(clippy::too_many_lines, reason = "one match over every subcommand")]
fn main() -> Result<()> {
    let Cli { common, command } = Cli::parse();
    let command = match command {
        Command::Radios { config, action } => return radios::run(&config, action),
        other => other,
    };
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
            trailing_id_gap_ms,
        } => {
            settings.validate()?;
            let configured = config.is_some();
            let config = config
                .as_deref()
                .map(config::Config::load)
                .transpose()?
                .unwrap_or_default();
            let wake = WakeWord::with_forms(&config.kodiak.wake_words);
            let engine = (!bss_only)
                .then(|| kodiak(common.backend, model, wake, &config.kodiak))
                .transpose()?;
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
                engine.as_ref(),
                data,
                correlation::Timing {
                    window_ms: correlation_timeout_ms
                        .unwrap_or(config.processing.correlation_timeout_ms),
                    trailing_gap_ms: trailing_id_gap_ms
                        .unwrap_or(config.processing.trailing_id_gap_ms),
                },
                publication,
            )
        }
        Command::Wav { input } => {
            let audio = audio::read_wav(&input)?;
            let engine = kodiak(
                common.backend,
                model,
                WakeWord::default(),
                &config::Kodiak::default(),
            )?;
            log(format_args!("Transcribing WAV {}", input.display()));
            let now = Utc::now();
            output
                .transcribe(
                    &engine,
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
            trailing_id_gap_ms,
        } => replay::run(
            &input,
            &rf_open,
            correlation::Timing {
                window_ms: correlation_timeout_ms,
                trailing_gap_ms: trailing_id_gap_ms,
            },
            &output,
            &kodiak(
                common.backend,
                model,
                WakeWord::default(),
                &config::Kodiak::default(),
            )?,
        ),
        Command::Radios { .. } => unreachable!("handled before the output directory is created"),
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
    engine: Option<&Engine>,
    data: Option<bss::Decoder>,
    timing: correlation::Timing,
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

    let transcribe =
        engine.map(|engine| move |clip: &Clip| output.transcribe(engine, clip, Source::Live));
    let publisher_stopped = AtomicBool::new(false);
    thread::scope(|scope| {
        let stopped_publishing = &publisher_stopped;
        let worker = publication.map(|mut publisher| {
            scope.spawn(move || publisher.run(output.directory(), stopped_publishing))
        });
        let result = processing::run(output, Source::Live, timing, transcribe, data, |ingress| {
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
        });
        publisher_stopped.store(true, Ordering::Relaxed);
        if let Some(worker) = worker {
            worker.thread().unpark();
        }
        result
    })?;
    log("Receiver stopped");
    Ok(())
}
