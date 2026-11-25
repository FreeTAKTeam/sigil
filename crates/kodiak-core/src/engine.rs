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
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::result::{Correction, Intent, KodiakResult, RejectReason, Rejection};
use crate::wake::WakeWord;

/// Sample rate every [`AudioInput`] must use.
pub const AUDIO_SAMPLE_RATE_HZ: u32 = 16_000;

/// Longest audio message accepted by default, in seconds.
pub const DEFAULT_MAX_AUDIO_SECONDS: u32 = 300;

/// Most words accepted after the activation word by default.
pub const DEFAULT_MAX_COMMAND_WORDS: usize = 128;

/// Shortest time between two attempts to load the speech model after a
/// failed load.
pub const ASR_RETRY_INTERVAL: Duration = Duration::from_secs(30);

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
    /// `wake` is the engine's activation word. The default never does, so
    /// transcripts without the word stay chat.
    fn addressed(&self, transcript: &str, wake: &WakeWord) -> Option<Addressed> {
        let _ = (transcript, wake);
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
    /// The classification of `command` when the classifier already produced
    /// it; the engine then uses it instead of classifying again. `command`
    /// must then have no leading activation words
    /// ([`WakeWord::strip_leading`]).
    pub classification: Option<Classification>,
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

type Loader = Box<dyn Fn() -> Result<Box<dyn Transcriber>, KodiakError> + Send>;

/// The speech recognizer and, when it can be (re)loaded, its loader.
struct Asr {
    loader: Option<Loader>,
    state: AsrState,
}

enum AsrState {
    /// Not loaded yet, or discarded after a transcriber panicked.
    Unloaded,
    Loaded(Box<dyn Transcriber>),
    /// Loading failed at `at`; retried after [`ASR_RETRY_INTERVAL`].
    Failed {
        message: String,
        at: Instant,
    },
}

impl Asr {
    /// Loads the model when none is loaded and a load is due.
    fn load_if_due(&mut self) {
        let Some(loader) = self.loader.as_ref() else {
            return;
        };
        let due = match &self.state {
            AsrState::Unloaded => true,
            AsrState::Failed { at, .. } => at.elapsed() >= ASR_RETRY_INTERVAL,
            AsrState::Loaded(_) => false,
        };
        if !due {
            return;
        }
        // Stays in place if the loader panics, so that is retried later too.
        self.state = AsrState::Failed {
            message: "loading the speech model panicked".into(),
            at: Instant::now(),
        };
        self.state = match loader() {
            Ok(transcriber) => AsrState::Loaded(transcriber),
            Err(error) => AsrState::Failed {
                message: error.to_string(),
                at: Instant::now(),
            },
        };
    }

    /// After a transcriber panicked: discards it, so that the model is
    /// reloaded, or reports it unavailable when there is no loader.
    fn discard_transcriber(&mut self) {
        if !matches!(self.state, AsrState::Loaded(_)) {
            return;
        }
        let next = if self.loader.is_some() {
            AsrState::Unloaded
        } else {
            AsrState::Failed {
                message: "the speech recognizer panicked and cannot be reloaded".into(),
                at: Instant::now(),
            }
        };
        drop(std::mem::replace(&mut self.state, next));
    }
}

/// Words as the classifiers tokenize them: hyphens and other punctuation
/// split words, so one long hyphenated "word" cannot bypass the word limit.
fn word_count(text: &str) -> usize {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .count()
}

/// The Kodiak engine.
pub struct Engine {
    wake: WakeWord,
    classifier: Box<dyn IntentClassifier>,
    asr: Mutex<Asr>,
    max_audio_seconds: u32,
    max_command_words: usize,
}

impl Engine {
    /// Creates a text-only engine. [`Engine::process_audio`] fails until a
    /// transcriber is attached.
    #[must_use]
    pub fn new(classifier: impl IntentClassifier + 'static) -> Self {
        Self {
            wake: WakeWord::default(),
            classifier: Box::new(classifier),
            asr: Mutex::new(Asr {
                loader: None,
                state: AsrState::Unloaded,
            }),
            max_audio_seconds: DEFAULT_MAX_AUDIO_SECONDS,
            max_command_words: DEFAULT_MAX_COMMAND_WORDS,
        }
    }

    /// Longest audio message accepted, in seconds
    /// ([`DEFAULT_MAX_AUDIO_SECONDS`] by default). Longer audio is
    /// [`KodiakError::InvalidAudio`].
    #[must_use]
    pub fn with_max_audio_seconds(mut self, seconds: u32) -> Self {
        self.max_audio_seconds = seconds;
        self
    }

    /// Most whitespace-separated words accepted after the activation word
    /// ([`DEFAULT_MAX_COMMAND_WORDS`] by default). A longer command is
    /// rejected as [`RejectReason::InvalidParameter`] without being
    /// classified, and a transcript without the activation word that is
    /// longer than this plus one word stays chat without being judged.
    #[must_use]
    pub fn with_max_command_words(mut self, words: usize) -> Self {
        self.max_command_words = words;
        self
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
        self.with_asr(Asr {
            loader: None,
            state: AsrState::Loaded(transcriber),
        })
    }

    /// Attaches a transcriber that is loaded on the first audio message, so
    /// text-only use never pays for model loading. The loader is kept: a
    /// failed load is retried on a later audio message, at most once per
    /// [`ASR_RETRY_INTERVAL`] (in between, the failure is reported), and the
    /// model is reloaded on the audio message after a transcriber panicked.
    #[must_use]
    pub fn with_transcriber_loader(
        self,
        loader: impl Fn() -> Result<Box<dyn Transcriber>, KodiakError> + Send + 'static,
    ) -> Self {
        self.with_asr(Asr {
            loader: Some(Box::new(loader)),
            state: AsrState::Unloaded,
        })
    }

    fn with_asr(self, asr: Asr) -> Self {
        Self {
            asr: Mutex::new(asr),
            ..self
        }
    }

    /// Whether a speech model is loaded and usable. False after a
    /// transcriber panicked, until the next audio message reloads it.
    #[must_use]
    pub fn transcriber_loaded(&self) -> bool {
        self.asr
            .lock()
            .is_ok_and(|asr| matches!(asr.state, AsrState::Loaded(_)))
    }

    /// The recognizer lock. When a transcriber panicked while holding it, its
    /// possibly corrupt state is discarded and the lock is usable again.
    fn recognizer(&self) -> MutexGuard<'_, Asr> {
        self.asr.lock().unwrap_or_else(|poisoned| {
            let mut asr = poisoned.into_inner();
            asr.discard_transcriber();
            self.asr.clear_poison();
            asr
        })
    }

    /// Steps 3–6: classify text that is already transcribed.
    #[must_use]
    pub fn process_text(&self, input: &TextInput) -> KodiakResult {
        let transcript = input.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let (command, mut heard, mut precomputed) = match self.wake.command(&transcript) {
            Some(command) => (command.to_owned(), None, None),
            None if word_count(&transcript) > self.max_command_words.saturating_add(1) => {
                return KodiakResult::Chat {
                    content: transcript,
                };
            }
            None => match self.classifier.addressed(&transcript, &self.wake) {
                Some(Addressed {
                    command,
                    heard,
                    classification,
                }) => (command, heard, classification),
                None => {
                    return KodiakResult::Chat {
                        content: transcript,
                    };
                }
            },
        };
        let mut command = self.wake.strip_leading(&command).to_owned();
        let classifier = self.classifier.id().to_owned();
        let vocabularies = self.classifier.vocabularies();
        // Count words as the classifiers tokenize them: hyphens and other
        // punctuation split words, so one long hyphenated "word" cannot
        // bypass the limit.
        let words = word_count(&command);
        if words > self.max_command_words {
            return KodiakResult::Rejected {
                rejection: Rejection::new(
                    RejectReason::InvalidParameter,
                    None,
                    format!(
                        "the command has {words} words; at most {} are accepted",
                        self.max_command_words
                    ),
                ),
                transcript,
                classifier,
                corrections: Vec::new(),
                confidence: None,
                vocabularies,
            };
        }
        // A repeated activation word can be misheard too ("Kodiak, Cardiac,
        // mayday …"). Read as words of the command it could add an event
        // nobody reported, so the classifier may take it as the activation
        // word when the rest is a recognized command.
        let mut repeated = None;
        if precomputed.is_none()
            && let Some(Addressed {
                command: rest,
                heard: Some(word),
                classification,
            }) = self.classifier.addressed(&command, &self.wake)
        {
            command = rest;
            repeated = Some(word);
            precomputed = classification;
        }
        let Classification {
            result,
            mut corrections,
            confidence,
        } = precomputed.unwrap_or_else(|| self.classifier.classify(&command));
        for heard in repeated.into_iter().chain(heard.take()).collect::<Vec<_>>() {
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
    /// Fails when the audio is not 16 kHz or longer than the maximum, when no
    /// recognizer is available, or when recognition fails. Recognition runs
    /// while holding the engine's recognizer lock, so concurrent callers are
    /// serialized. A panic in the transcriber propagates to the caller; the
    /// next call discards that transcriber and reloads the model when a
    /// loader is attached.
    pub fn process_audio(&self, input: &AudioInput) -> Result<KodiakResult, KodiakError> {
        if input.sample_rate_hz != AUDIO_SAMPLE_RATE_HZ {
            return Err(KodiakError::InvalidAudio(format!(
                "expected {AUDIO_SAMPLE_RATE_HZ} Hz, received {} Hz",
                input.sample_rate_hz
            )));
        }
        let samples = u64::try_from(input.samples.len()).unwrap_or(u64::MAX);
        let rate = u64::from(AUDIO_SAMPLE_RATE_HZ);
        if samples > u64::from(self.max_audio_seconds) * rate {
            let tenths = samples.saturating_mul(10) / rate;
            return Err(KodiakError::InvalidAudio(format!(
                "audio is {}.{} s long; the maximum is {} s",
                tenths / 10,
                tenths % 10,
                self.max_audio_seconds
            )));
        }
        if input.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(KodiakError::InvalidAudio("non-finite sample".into()));
        }
        let text = {
            let mut asr = self.recognizer();
            asr.load_if_due();
            match &mut asr.state {
                AsrState::Loaded(transcriber) => transcriber.transcribe(&input.samples)?,
                AsrState::Failed { message, .. } => {
                    return Err(KodiakError::AsrUnavailable(message.clone()));
                }
                AsrState::Unloaded => {
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

        fn addressed(&self, transcript: &str, _: &WakeWord) -> Option<Addressed> {
            let rest = transcript.strip_prefix("Kardiac")?;
            Some(Addressed {
                command: rest.to_owned(),
                heard: Some("Kardiac".into()),
                classification: None,
            })
        }
    }

    /// Counts classifications; "Kardiac" addresses it with a ready result.
    struct Counting(Arc<AtomicUsize>);

    impl IntentClassifier for Counting {
        fn id(&self) -> &'static str {
            "counting/1"
        }

        fn classify(&self, command: &str) -> Classification {
            self.0.fetch_add(1, Ordering::SeqCst);
            Echo.classify(command)
        }

        fn addressed(&self, transcript: &str, _: &WakeWord) -> Option<Addressed> {
            let command = transcript.strip_prefix("Kardiac")?.to_owned();
            let classification = Some(self.classify(&command));
            Some(Addressed {
                command,
                heard: Some("Kardiac".into()),
                classification,
            })
        }
    }

    /// Records every command it classifies.
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl IntentClassifier for Recorder {
        fn id(&self) -> &'static str {
            "recorder/1"
        }

        fn classify(&self, command: &str) -> Classification {
            self.0.lock().unwrap().push(command.to_owned());
            Echo.classify(command)
        }
    }

    #[test]
    fn repeated_activation_words_are_not_part_of_the_command() {
        let recorder = Arc::new(Mutex::new(Vec::new()));
        let engine = Engine::new(Recorder(Arc::clone(&recorder)))
            .with_wake_word(WakeWord::with_forms(["falcon", "falkon"]));
        let _ = engine.process_text(&TextInput::new("Falcon, falkon FALCON list users"));
        let _ = engine.process_text(&TextInput::new("falcon kodiak list users"));
        assert_eq!(
            *recorder.lock().unwrap(),
            vec![" list users".to_owned(), " kodiak list users".to_owned()]
        );
    }

    #[test]
    fn overlong_commands_are_rejected_unclassified() {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = Engine::new(Counting(Arc::clone(&calls))).with_max_command_words(3);
        assert!(matches!(
            engine.process_text(&TextInput::new("Kodiak Kodiak list the users")),
            KodiakResult::Intent { .. }
        ));
        let KodiakResult::Rejected { rejection, .. } =
            engine.process_text(&TextInput::new("Kodiak list all the users"))
        else {
            panic!("expected rejection");
        };
        assert_eq!(rejection.reason, RejectReason::InvalidParameter);
        assert_eq!(
            rejection.detail,
            "the command has 4 words; at most 3 are accepted"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Without the activation word: chat, never judged when too long.
        assert!(matches!(
            engine.process_text(&TextInput::new("Kardiac list all the users")),
            KodiakResult::Chat { .. }
        ));
        assert!(matches!(
            engine.process_text(&TextInput::new("we need lots more food here")),
            KodiakResult::Chat { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            engine.process_text(&TextInput::new("Kardiac list the users")),
            KodiakResult::Intent { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(DEFAULT_MAX_COMMAND_WORDS, 128);
    }

    #[test]
    fn overlong_audio_is_invalid() {
        let engine = Engine::new(Echo)
            .with_transcriber(Box::new(Fixed("Kodiak list users")))
            .with_max_audio_seconds(1);
        assert!(
            engine
                .process_audio(&AudioInput::mono_16k(vec![0.0; 16_000]))
                .is_ok()
        );
        let Err(KodiakError::InvalidAudio(message)) =
            engine.process_audio(&AudioInput::mono_16k(vec![0.0; 24_000]))
        else {
            panic!("expected invalid audio");
        };
        assert_eq!(message, "audio is 1.5 s long; the maximum is 1 s");
        assert_eq!(DEFAULT_MAX_AUDIO_SECONDS, 300);
    }

    /// Moves a failed load back by the retry interval.
    fn age_failure(engine: &Engine) {
        let mut asr = engine.asr.lock().unwrap();
        let AsrState::Failed { at, .. } = &mut asr.state else {
            panic!("expected a failed load");
        };
        *at = Instant::now().checked_sub(ASR_RETRY_INTERVAL).unwrap();
    }

    #[test]
    fn a_failed_load_is_retried_after_the_interval() {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&loads);
        let engine = Engine::new(Echo).with_transcriber_loader(move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(KodiakError::AsrUnavailable("model missing".into()))
            } else {
                Ok(Box::new(Fixed("Kodiak list users")) as Box<dyn Transcriber>)
            }
        });
        let audio = AudioInput::mono_16k(vec![0.0; 160]);
        for _ in 0..2 {
            let Err(KodiakError::AsrUnavailable(message)) = engine.process_audio(&audio) else {
                panic!("expected unavailable");
            };
            assert!(message.contains("model missing"), "{message}");
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "not retried within the interval"
        );
        age_failure(&engine);
        assert!(matches!(
            engine.process_audio(&audio),
            Ok(KodiakResult::Intent { .. })
        ));
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert!(engine.transcriber_loaded());
    }

    struct Panics;

    impl Transcriber for Panics {
        fn transcribe(&mut self, _: &[f32]) -> Result<String, KodiakError> {
            panic!("recognizer crashed");
        }
    }

    #[test]
    fn a_panicked_transcriber_is_reloaded_on_the_next_call() {
        let loads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&loads);
        let engine = Engine::new(Echo).with_transcriber_loader(move || {
            Ok(if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Box::new(Panics) as Box<dyn Transcriber>
            } else {
                Box::new(Fixed("Kodiak list users"))
            })
        });
        let audio = AudioInput::mono_16k(vec![0.0; 160]);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.process_audio(&audio)
        }));
        assert!(crashed.is_err(), "the panic reaches the caller");
        assert!(!engine.transcriber_loaded());
        assert!(matches!(
            engine.process_audio(&audio),
            Ok(KodiakResult::Intent { .. })
        ));
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert!(engine.transcriber_loaded());

        // Without a loader there is nothing to reload.
        let engine = Engine::new(Echo).with_transcriber(Box::new(Panics));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.process_audio(&audio)
        }));
        assert!(crashed.is_err());
        for _ in 0..2 {
            assert!(matches!(
                engine.process_audio(&audio),
                Err(KodiakError::AsrUnavailable(_))
            ));
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
