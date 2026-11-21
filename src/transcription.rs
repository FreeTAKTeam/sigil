use std::path::Path;

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
#[cfg(feature = "parakeet")]
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Backend {
    Whisper,
    #[cfg(feature = "parakeet")]
    Parakeet,
}

impl Backend {
    pub fn default_model(self) -> &'static Path {
        match self {
            Self::Whisper => Path::new("models/ggml-base.en.bin"),
            #[cfg(feature = "parakeet")]
            Self::Parakeet => Path::new("models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8"),
        }
    }
}

pub enum Recognizer {
    Whisper(Whisper),
    #[cfg(feature = "parakeet")]
    Parakeet(OfflineRecognizer),
}

impl Recognizer {
    pub fn load(backend: Backend, model: &Path) -> Result<Self> {
        match backend {
            Backend::Whisper => Ok(Self::Whisper(Whisper::load(model)?)),
            #[cfg(feature = "parakeet")]
            Backend::Parakeet => Ok(Self::Parakeet(load_parakeet(model)?)),
        }
    }

    pub fn transcribe(&mut self, audio: &[f32]) -> Result<String> {
        match self {
            Self::Whisper(recognizer) => recognizer.transcribe(audio),
            #[cfg(feature = "parakeet")]
            Self::Parakeet(recognizer) => {
                let stream = recognizer.create_stream();
                stream.accept_waveform(i32::try_from(crate::audio::SAMPLE_RATE)?, audio);
                recognizer.decode(&stream);
                let result = stream.get_result().context("read Parakeet transcript")?;
                Ok(result.text.split_whitespace().collect::<Vec<_>>().join(" "))
            }
        }
    }
}

#[cfg(feature = "parakeet")]
fn load_parakeet(directory: &Path) -> Result<OfflineRecognizer> {
    let model_file = |name: &str| -> Result<String> {
        let path = directory.join(name);
        ensure!(
            path.is_file(),
            "missing Parakeet model file {}",
            path.display()
        );
        path.into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Parakeet model path must be valid UTF-8"))
    };
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer = OfflineTransducerModelConfig {
        encoder: Some(model_file("encoder.int8.onnx")?),
        decoder: Some(model_file("decoder.int8.onnx")?),
        joiner: Some(model_file("joiner.int8.onnx")?),
    };
    config.model_config.tokens = Some(model_file("tokens.txt")?);
    config.model_config.model_type = Some("nemo_transducer".into());
    config.model_config.num_threads = recognition_threads()?;
    OfflineRecognizer::create(&config)
        .with_context(|| format!("load Parakeet TDT v2 from {}", directory.display()))
}

fn recognition_threads() -> Result<i32> {
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    Ok(i32::try_from(threads)?)
}

pub struct Whisper {
    state: WhisperState,
    threads: i32,
}

impl Whisper {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.is_file(),
            "missing local Whisper model {}",
            path.display()
        );
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(false);
        let context = WhisperContext::new_with_params(path, params)
            .with_context(|| format!("load whisper.cpp model {}", path.display()))?;
        Ok(Self {
            state: context
                .create_state()
                .context("initialize Whisper inference state")?,
            threads: recognition_threads()?,
        })
    }

    pub fn transcribe(&mut self, audio: &[f32]) -> Result<String> {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_n_threads(self.threads);
        params.set_no_context(true);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        self.state
            .full(params, audio)
            .context("run local Whisper inference")?;

        let mut segments = Vec::new();
        for segment in self.state.as_iter() {
            segments.push(
                segment
                    .to_str_lossy()
                    .context("read Whisper text segment")?,
            );
        }
        Ok(segments
            .iter()
            .flat_map(|text| text.split_whitespace())
            .collect::<Vec<_>>()
            .join(" "))
    }
}
