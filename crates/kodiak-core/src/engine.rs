//! The Kodiak processing sequence.
//!
//! 1. Audio arrives ([`Engine::process_audio`]).
//! 2. It is transcribed offline by the configured [`Transcriber`].
//! 3. The transcript is checked for the activation word. Text-only callers
//!    enter here with [`Engine::process_text`].
//! 4. Without the activation word the transcript is returned as chat.
//! 5. With it, the [`IntentClassifier`] extracts one validated intent.
//! 6. A typed intent or a structured rejection is returned.
//!
//! The engine is `Send + Sync`. One engine, and therefore one loaded speech
//! model, is meant to be shared by every radio a gateway serves. The API is
//! synchronous because recognition and classification are CPU-bound; async
//! callers should run it on a blocking thread pool.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use crate::result::{Correction, Intent, KodiakResult, Rejection};
use crate::wake::WakeWord;

/// Sample rate every [`AudioInput`] must use.
pub const AUDIO_SAMPLE_RATE_HZ: u32 = 16_000;

/// Mono PCM audio for transcription.
#[derive(Clone, Debug)]
pub struct AudioInput {
    /// Samples in the range -1.0 to 1.0.
    pub samples: Vec<f32>,
    /// Sample rate in Hz. Must equal [`AUDIO_SAMPLE_RATE_HZ`].
    pub sample_rate_hz: u32,
}

impl AudioInput {
    /// 16 kHz mono audio.
    #[must_use]
    pub fn mono_16k(samples: Vec<f32>) -> Self {
        Self {
            samples,
            sample_rate_hz: AUDIO_SAMPLE_RATE_HZ,
        }
    }
}

/// Text that enters the sequence at the activation-word check, for example a
/// message typed in an ordinary LXMF client.
#[derive(Clone, Debug)]
pub struct TextInput {
    /// The message text.
    pub text: String,
}

impl TextInput {
    /// Wraps a message.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Errors that prevent Kodiak from producing a result at all.
#[derive(Debug)]
pub enum KodiakError {
    /// The audio does not meet the input contract.
    InvalidAudio(String),
    /// The engine has no speech recognizer, or loading it failed.
    AsrUnavailable(String),
    /// The speech recognizer failed on this audio.
    Asr(String),
}

impl fmt::Display for KodiakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAudio(message) => write!(f, "invalid audio: {message}"),
            Self::AsrUnavailable(message) => write!(f, "speech recognition unavailable: {message}"),
            Self::Asr(message) => write!(f, "speech recognition failed: {message}"),
        }
    }
}

impl std::error::Error for KodiakError {}

/// Offline speech-to-text. Implementations receive 16 kHz mono audio.
pub trait Transcriber: Send {
    /// Transcribes one message.
    ///
    /// # Errors
    ///
    /// Returns [`KodiakError::Asr`] when recognition fails.
    fn transcribe(&mut self, audio: &[f32]) -> Result<String, KodiakError>;
}

/// Extracts one intent from the words that follow the activation word.
///
/// Implementations must return a [`Rejection`] rather than guess, and must not
/// report confidence values that the underlying method does not produce.
pub trait IntentClassifier: Send + Sync {
    /// Identifier and version, recorded in every result, e.g. `deterministic/1`.
    fn id(&self) -> &str;

    /// Classifies the command text. The result is a [`Rejection`] for
    /// unknown, ambiguous or incomplete commands.
    fn classify(&self, command: &str) -> Classification;

    /// Called only when the activation word was not found. A classifier that
    /// can judge whether the transcript still addresses Kodiak (for example
    /// with the activation word misheard) returns the command to classify.
    /// The default never does, so transcripts without the word stay chat.
    fn addressed(&self, transcript: &str) -> Option<Addressed> {
        let _ = transcript;
        None
    }

    /// Name and version of every vocabulary the classifier uses, recorded in
    /// every intent and rejection. Empty by default.
    fn vocabularies(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

/// A transcript judged to address Kodiak although the activation word was
/// not recognized.
#[derive(Clone, Debug, PartialEq)]
pub struct Addressed {
    /// The text to classify, without the misheard activation word.
    pub command: String,
    /// The word taken to be the misheard activation word, if one was removed.
    pub heard: Option<String>,
}

/// What a classifier produced, with any word corrections it applied.
#[derive(Clone, Debug, PartialEq)]
pub struct Classification {
    /// One validated intent, or a rejection.
    pub result: Result<Intent, Rejection>,
    /// Words read as different vocabulary words, in order.
    pub corrections: Vec<Correction>,
    /// Calibrated probability of the result, only from classifiers that
    /// produce one.
    pub confidence: Option<f32>,
}

impl From<Result<Intent, Rejection>> for Classification {
    fn from(result: Result<Intent, Rejection>) -> Self {
        Self {
            result,
            corrections: Vec::new(),
            confidence: None,
        }
    }
}

type Loader = Box<dyn FnOnce() -> Result<Box<dyn Transcriber>, KodiakError> + Send>;

enum Asr {
    None,
    Pending(Loader),
    Loaded(Box<dyn Transcriber>),
    Failed(String),
}

/// The Kodiak engine.
pub struct Engine {
    wake: WakeWord,
    classifier: Box<dyn IntentClassifier>,
    asr: Mutex<Asr>,
}

impl Engine {
    /// Creates a text-only engine. [`Engine::process_audio`] fails until a
    /// transcriber is attached.
    #[must_use]
    pub fn new(classifier: impl IntentClassifier + 'static) -> Self {
        Self {
            wake: WakeWord::default(),
            classifier: Box::new(classifier),
            asr: Mutex::new(Asr::None),
        }
    }

    /// Replaces the activation word.
    #[must_use]
    pub fn with_wake_word(mut self, wake: WakeWord) -> Self {
        self.wake = wake;
        self
    }

    /// Attaches a loaded transcriber.
    #[must_use]
    pub fn with_transcriber(self, transcriber: Box<dyn Transcriber>) -> Self {
        self.with_asr(Asr::Loaded(transcriber))
    }

    /// Attaches a transcriber that is loaded on the first audio message, so
    /// text-only use never pays for model loading. A failed load is reported
    /// for every later audio message and is not retried.
    #[must_use]
    pub fn with_transcriber_loader(
        self,
        loader: impl FnOnce() -> Result<Box<dyn Transcriber>, KodiakError> + Send + 'static,
    ) -> Self {
        self.with_asr(Asr::Pending(Box::new(loader)))
    }

    fn with_asr(self, asr: Asr) -> Self {
        Self {
            asr: Mutex::new(asr),
            ..self
        }
    }

    /// Whether a speech model is loaded.
    #[must_use]
    pub fn transcriber_loaded(&self) -> bool {
        self.asr
            .lock()
            .is_ok_and(|asr| matches!(*asr, Asr::Loaded(_)))
    }

    /// Steps 3–6: classify text that is already transcribed.
    #[must_use]
    pub fn process_text(&self, input: &TextInput) -> KodiakResult {
        let transcript = input.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let (command, heard) = match self.wake.command(&transcript) {
            Some(command) => (command.to_owned(), None),
            None => match self.classifier.addressed(&transcript) {
                Some(Addressed { command, heard }) => (command, heard),
                None => {
                    return KodiakResult::Chat {
                        content: transcript,
                    };
                }
            },
        };
        let classifier = self.classifier.id().to_owned();
        let vocabularies = self.classifier.vocabularies();
        let Classification {
            result,
            mut corrections,
            confidence,
        } = self.classifier.classify(&command);
        if let Some(heard) = heard {
            corrections.insert(
                0,
                Correction {
                    heard,
                    read_as: self.wake.word().to_owned(),
                },
            );
        }
        match result {
            Ok(intent) => KodiakResult::Intent {
                intent,
                transcript,
                classifier,
                corrections,
                confidence,
                vocabularies,
            },
            Err(rejection) => KodiakResult::Rejected {
                rejection,
                transcript,
                classifier,
                corrections,
                confidence,
                vocabularies,
            },
        }
    }

    /// Steps 1–6: transcribe audio, then classify the transcript.
    ///
    /// # Errors
    ///
    /// Fails when the audio is not 16 kHz, when no recognizer is available, or
    /// when recognition fails. Recognition runs while holding the engine's
    /// recognizer lock, so concurrent callers are serialized.
    pub fn process_audio(&self, input: &AudioInput) -> Result<KodiakResult, KodiakError> {
        if input.sample_rate_hz != AUDIO_SAMPLE_RATE_HZ {
            return Err(KodiakError::InvalidAudio(format!(
                "expected {AUDIO_SAMPLE_RATE_HZ} Hz, received {} Hz",
                input.sample_rate_hz
            )));
        }
        if input.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(KodiakError::InvalidAudio("non-finite sample".into()));
        }
        let text = {
            let mut asr = self
                .asr
                .lock()
                .map_err(|_| KodiakError::AsrUnavailable("recognizer lock poisoned".into()))?;
            if matches!(*asr, Asr::Pending(_)) {
                let Asr::Pending(loader) = std::mem::replace(&mut *asr, Asr::None) else {
                    unreachable!("checked above");
                };
                *asr = match loader() {
                    Ok(transcriber) => Asr::Loaded(transcriber),
                    Err(error) => Asr::Failed(error.to_string()),
                };
            }
            match &mut *asr {
                Asr::Loaded(transcriber) => transcriber.transcribe(&input.samples)?,
                Asr::Failed(message) => return Err(KodiakError::AsrUnavailable(message.clone())),
                Asr::None | Asr::Pending(_) => {
                    return Err(KodiakError::AsrUnavailable(
                        "no recognizer configured".into(),
                    ));
                }
            }
        };
        Ok(self.process_text(&TextInput { text }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::result::{Category, RchCommand, RejectReason};

    struct Echo;

    impl IntentClassifier for Echo {
        fn id(&self) -> &'static str {
            "echo/1"
        }

        fn classify(&self, command: &str) -> Classification {
            Classification::from(if command.contains("users") {
                Ok(Intent::RchCommand(RchCommand {
                    command: "mission.users".into(),
                    arguments: BTreeMap::new(),
                }))
            } else {
                Err(Rejection::new(RejectReason::NoIntent, None, "nothing"))
            })
        }
    }

    /// Treats "kardiac" as the misheard activation word.
    struct Judge;

    impl IntentClassifier for Judge {
        fn id(&self) -> &'static str {
            "judge/1"
        }

        fn classify(&self, command: &str) -> Classification {
            Echo.classify(command)
        }

        fn addressed(&self, transcript: &str) -> Option<Addressed> {
            let rest = transcript.strip_prefix("Kardiac")?;
            Some(Addressed {
                command: rest.to_owned(),
                heard: Some("Kardiac".into()),
            })
        }
    }

    #[test]
    fn a_classifier_can_recover_a_misheard_activation_word() {
        let KodiakResult::Intent { corrections, .. } =
            Engine::new(Judge).process_text(&TextInput::new("Kardiac list users"))
        else {
            panic!("expected intent");
        };
        assert_eq!(
            corrections,
            vec![Correction {
                heard: "Kardiac".into(),
                read_as: "kodiak".into()
            }]
        );
        assert!(matches!(
            Engine::new(Judge).process_text(&TextInput::new("list users")),
            KodiakResult::Chat { .. }
        ));
        assert!(matches!(
            Engine::new(Echo).process_text(&TextInput::new("Kardiac list users")),
            KodiakResult::Chat { .. }
        ));
    }

    struct Fixed(&'static str);

    impl Transcriber for Fixed {
        fn transcribe(&mut self, _: &[f32]) -> Result<String, KodiakError> {
            Ok(self.0.into())
        }
    }

    #[test]
    fn sequence_branches_on_the_activation_word() {
        let engine = Engine::new(Echo);
        assert_eq!(
            engine.process_text(&TextInput::new("  We need  food ")),
            KodiakResult::Chat {
                content: "We need food".into()
            }
        );
        let KodiakResult::Intent { intent, .. } =
            engine.process_text(&TextInput::new("Kodiak list users"))
        else {
            panic!("expected intent");
        };
        assert_eq!(intent.category(), Category::Rch);
        assert!(matches!(
            engine.process_text(&TextInput::new("Kodiak")),
            KodiakResult::Rejected { .. }
        ));
    }

    #[test]
    fn text_never_loads_asr_and_one_shared_model_serves_every_caller() {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&loads);
        let engine = Arc::new(Engine::new(Echo).with_transcriber_loader(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Fixed("Kodiak list users")) as Box<dyn Transcriber>)
        }));
        let _ = engine.process_text(&TextInput::new("Kodiak list users"));
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        assert!(!engine.transcriber_loaded());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let engine = Arc::clone(&engine);
                std::thread::spawn(move || {
                    engine.process_audio(&AudioInput::mono_16k(vec![0.0; 160]))
                })
            })
            .collect();
        for thread in threads {
            assert!(matches!(
                thread.join().unwrap(),
                Ok(KodiakResult::Intent { .. })
            ));
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(engine.transcriber_loaded());
    }

    #[test]
    fn audio_contract_and_missing_recognizer_are_errors() {
        let engine = Engine::new(Echo);
        assert!(matches!(
            engine.process_audio(&AudioInput::mono_16k(vec![0.0])),
            Err(KodiakError::AsrUnavailable(_))
        ));
        let engine = engine.with_transcriber(Box::new(Fixed("hello")));
        let wrong_rate = AudioInput {
            samples: vec![0.0],
            sample_rate_hz: 8_000,
        };
        assert!(matches!(
            engine.process_audio(&wrong_rate),
            Err(KodiakError::InvalidAudio(_))
        ));
    }
}
