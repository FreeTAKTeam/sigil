//! # kodiak-asr
//!
//! Offline speech recognizers implementing [`kodiak_core::Transcriber`].
//! Every recognizer receives 16 kHz mono `f32` audio and runs on the CPU.
//!
//! | Feature | Backend | Model |
//! |---|---|---|
//! | `whisper` (default) | whisper.cpp via `whisper-rs` | a ggml file such as `ggml-base.en.bin` |
//! | `parakeet` | Sherpa-ONNX offline transducer | a directory with `encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt` |
//!
//! Model paths are always supplied by the caller; this crate has no default
//! locations.

use std::path::Path;

use kodiak_core::{KodiakError, Transcriber};

/// Available recognizer backends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    /// whisper.cpp, English.
    #[cfg(feature = "whisper")]
    Whisper,
    /// NVIDIA Parakeet TDT 0.6B v2 INT8, English.
    #[cfg(feature = "parakeet")]
    Parakeet,
}

/// Loads a recognizer.
///
/// # Errors
///
/// Returns [`KodiakError::AsrUnavailable`] when the model is missing or fails to load.
pub fn load(backend: Backend, model: &Path) -> Result<Box<dyn Transcriber>, KodiakError> {
    match backend {
        #[cfg(feature = "whisper")]
        Backend::Whisper => Ok(Box::new(whisper::Whisper::load(model)?)),
        #[cfg(feature = "parakeet")]
        Backend::Parakeet => Ok(Box::new(parakeet::Parakeet::load(model)?)),
    }
}

/// Threads used per recognizer: available parallelism, capped at four.
fn recognition_threads() -> i32 {
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    i32::try_from(threads).unwrap_or(1)
}

fn unavailable(message: impl Into<String>) -> KodiakError {
    KodiakError::AsrUnavailable(message.into())
}

fn join_words<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    segments
        .into_iter()
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(feature = "whisper")]
mod whisper {
    use std::path::Path;

    use kodiak_core::{KodiakError, Transcriber};
    use whisper_rs::{
        FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    };

    pub struct Whisper {
        state: WhisperState,
        threads: i32,
    }

    impl Whisper {
        pub fn load(path: &Path) -> Result<Self, KodiakError> {
            if !path.is_file() {
                return Err(super::unavailable(format!(
                    "missing local Whisper model {}",
                    path.display()
                )));
            }
            whisper_rs::install_logging_hooks();
            let mut params = WhisperContextParameters::default();
            params.use_gpu(false);
            let context = WhisperContext::new_with_params(path, params).map_err(|error| {
                super::unavailable(format!(
                    "load whisper.cpp model {}: {error}",
                    path.display()
                ))
            })?;
            let state = context.create_state().map_err(|error| {
                super::unavailable(format!("initialize Whisper state: {error}"))
            })?;
            Ok(Self {
                state,
                threads: super::recognition_threads(),
            })
        }
    }

    impl Transcriber for Whisper {
        fn transcribe(&mut self, audio: &[f32]) -> Result<String, KodiakError> {
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
                .map_err(|error| KodiakError::Asr(format!("Whisper inference: {error}")))?;
            let mut segments = Vec::new();
            for segment in self.state.as_iter() {
                segments.push(
                    segment
                        .to_str_lossy()
                        .map_err(|error| KodiakError::Asr(format!("Whisper segment: {error}")))?
                        .into_owned(),
                );
            }
            Ok(super::join_words(segments.iter().map(String::as_str)))
        }
    }
}

#[cfg(feature = "parakeet")]
mod parakeet {
    use std::path::Path;

    use kodiak_core::{AUDIO_SAMPLE_RATE_HZ, KodiakError, Transcriber};
    use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig};

    pub struct Parakeet(OfflineRecognizer);

    impl Parakeet {
        pub fn load(directory: &Path) -> Result<Self, KodiakError> {
            let model_file = |name: &str| -> Result<String, KodiakError> {
                let path = directory.join(name);
                if !path.is_file() {
                    return Err(super::unavailable(format!(
                        "missing Parakeet model file {}",
                        path.display()
                    )));
                }
                path.into_os_string()
                    .into_string()
                    .map_err(|_| super::unavailable("Parakeet model path must be valid UTF-8"))
            };
            let mut config = OfflineRecognizerConfig::default();
            config.model_config.transducer = OfflineTransducerModelConfig {
                encoder: Some(model_file("encoder.int8.onnx")?),
                decoder: Some(model_file("decoder.int8.onnx")?),
                joiner: Some(model_file("joiner.int8.onnx")?),
            };
            config.model_config.tokens = Some(model_file("tokens.txt")?);
            config.model_config.model_type = Some("nemo_transducer".into());
            config.model_config.num_threads = super::recognition_threads();
            OfflineRecognizer::create(&config).map(Self).ok_or_else(|| {
                super::unavailable(format!(
                    "load Parakeet TDT v2 from {}: Sherpa-ONNX reports no cause; \
                     see its log output",
                    directory.display()
                ))
            })
        }
    }

    impl Transcriber for Parakeet {
        fn transcribe(&mut self, audio: &[f32]) -> Result<String, KodiakError> {
            let stream = self.0.create_stream();
            let rate = i32::try_from(AUDIO_SAMPLE_RATE_HZ).unwrap_or(16_000);
            stream.accept_waveform(rate, audio);
            self.0.decode(&stream);
            let result = stream
                .get_result()
                .ok_or_else(|| KodiakError::Asr("read Parakeet transcript".into()))?;
            Ok(super::join_words([result.text.as_str()]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_models_are_reported_not_panicked() {
        #[cfg(feature = "whisper")]
        assert!(matches!(
            load(Backend::Whisper, Path::new("no-such-model.bin")),
            Err(KodiakError::AsrUnavailable(_))
        ));
        #[cfg(feature = "parakeet")]
        assert!(matches!(
            load(Backend::Parakeet, Path::new("no-such-directory")),
            Err(KodiakError::AsrUnavailable(_))
        ));
        assert_eq!(join_words([" a  b ", "c"]), "a b c");
    }
}
