use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use tempfile::{Builder, NamedTempFile};

use crate::audio::{SAMPLE_RATE, write_wav};
use crate::capture::Clip;
use kodiak_core::{AudioInput, Engine, KodiakResult};

pub struct Output {
    directory: PathBuf,
    frequency_hz: u32,
    save_wav: bool,
}

#[derive(Clone, Copy, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Live,
    Wav,
}

#[derive(Serialize)]
struct Transcript<'a> {
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    transcribed_at: DateTime<Utc>,
    frequency_hz: u32,
    source: Source,
    audio_duration_ms: u64,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_file: Option<&'a str>,
    /// Kodiak's chat, intent or rejection for this transcript.
    kodiak: &'a KodiakResult,
}

impl Output {
    pub fn new(directory: PathBuf, frequency_hz: u32, save_wav: bool) -> Result<Self> {
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create output directory {}", directory.display()))?;
        Ok(Self {
            directory,
            frequency_hz,
            save_wav,
        })
    }

    pub fn detected(&self, at: DateTime<Utc>) {
        let frequency = frequency_label(self.frequency_hz);
        log_at(at, format_args!("Transmission detected on {frequency}"));
    }

    pub fn frequency_hz(&self) -> u32 {
        self.frequency_hz
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn save_event(
        &self,
        at: DateTime<Utc>,
        prefix: &str,
        event: &impl Serialize,
    ) -> Result<PathBuf> {
        let prefix = format!(
            "{prefix}_{}_{}_",
            at.format("%Y%m%dT%H%M%S%.3fZ"),
            self.frequency_hz
        );
        let mut json = Builder::new()
            .prefix(&prefix)
            .suffix(".tmp")
            .tempfile_in(&self.directory)
            .with_context(|| format!("create event in {}", self.directory.display()))?;
        let path = json.path().with_extension("json");
        serde_json::to_writer_pretty(json.as_file_mut(), event).context("write event JSON")?;
        writeln!(json)?;
        persist(json, &path)?;
        Ok(path)
    }

    pub fn completed(&self, clip: &Clip) {
        let frequency = frequency_label(self.frequency_hz);
        log_at(
            clip.ended_at,
            format_args!("Transmission completed on {frequency}"),
        );
    }

    pub fn transcribe(&self, engine: &Engine, clip: &Clip, source: Source) -> Result<KodiakResult> {
        let prefix = format!(
            "{}_{}_",
            clip.started_at.format("%Y%m%dT%H%M%S%.3fZ"),
            self.frequency_hz
        );
        let mut json = Builder::new()
            .prefix(&prefix)
            .tempfile_in(&self.directory)
            .with_context(|| format!("create transcript in {}", self.directory.display()))?;
        let base_name = json.path().file_name().context("get transcript filename")?;
        let mut json_name = base_name.to_owned();
        json_name.push(".json");
        let json_path = self.directory.join(json_name);
        let wav_path = if self.save_wav {
            let mut wav_name = base_name.to_owned();
            wav_name.push(".wav");
            let wav_path = self.directory.join(wav_name);
            let mut wav = NamedTempFile::new_in(&self.directory).context("create capture WAV")?;
            write_wav(wav.as_file_mut(), &clip.audio).context("write capture WAV")?;
            persist(wav, &wav_path)?;
            Some(wav_path)
        } else {
            None
        };

        let result = engine.process_audio(&AudioInput::mono_16k(clip.audio.clone()))?;
        let text = result.transcript();
        let now = Utc::now();
        let frequency = frequency_label(self.frequency_hz);
        if text.is_empty() {
            log_at(now, format_args!("{frequency} No speech recognized"));
        } else {
            log_at(now, format_args!("{frequency} Transcript: {text}"));
        }
        match &result {
            KodiakResult::Chat { .. } => {}
            KodiakResult::Intent {
                intent,
                classifier,
                confidence,
                ..
            } => {
                log(format_args!(
                    "Kodiak {} intent {} ({classifier}{})",
                    intent.category(),
                    intent.name(),
                    confidence
                        .map(|p| format!(", confidence {p:.2}"))
                        .unwrap_or_default()
                ));
            }
            KodiakResult::Rejected { rejection, .. } => log(format_args!(
                "Kodiak rejected the command ({:?}): {}",
                rejection.reason, rejection.detail
            )),
        }
        if let KodiakResult::Intent { corrections, .. } | KodiakResult::Rejected { corrections, .. } =
            &result
            && !corrections.is_empty()
        {
            let heard: Vec<String> = corrections
                .iter()
                .map(|c| format!("{:?} as {:?}", c.heard, c.read_as))
                .collect();
            log(format_args!("Kodiak read {}", heard.join(", ")));
        }
        let transcript = Transcript {
            started_at: clip.started_at,
            ended_at: clip.ended_at,
            transcribed_at: now,
            frequency_hz: self.frequency_hz,
            source,
            audio_duration_ms: u64::try_from(clip.audio.len())? * 1_000 / u64::from(SAMPLE_RATE),
            text,
            audio_file: wav_path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(std::ffi::OsStr::to_str),
            kodiak: &result,
        };
        serde_json::to_writer_pretty(json.as_file_mut(), &transcript)
            .context("write transcript JSON")?;
        writeln!(json)?;
        persist(json, &json_path)?;
        log(format_args!("Transcript saved to {}", json_path.display()));
        Ok(result)
    }
}

fn persist(file: NamedTempFile, destination: &Path) -> Result<()> {
    file.as_file()
        .sync_all()
        .with_context(|| format!("flush {}", destination.display()))?;
    file.persist_noclobber(destination)
        .with_context(|| format!("save {}", destination.display()))?;
    Ok(())
}

pub fn frequency_label(hz: u32) -> String {
    format!("{:.4} MHz", f64::from(hz) / 1_000_000.0)
}

pub fn log(message: impl Display) {
    log_at(Utc::now(), message);
}

fn log_at(time: DateTime<Utc>, message: impl Display) {
    println!(
        "[{}] {message}",
        time.with_timezone(&Local).format("%H:%M:%S")
    );
}
