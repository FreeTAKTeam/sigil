//! # kodiak-laya
//!
//! A Kodiak [`IntentClassifier`] that asks a laya typed-decision model
//! (`laya-candle`) which command a transcript carries, following the
//! operator's flow:
//!
//! 1. **Is the first word Kodiak?** Any first word close to "kodiak" counts
//!    ("Kardiac", "Podax", "Codeac"). This step is the deterministic
//!    [`kodiak_intent::first_word_wake`]: probes showed laya cannot judge
//!    spelling or sound similarity at all. A misheard activation word is
//!    only accepted when the rest of the message is a recognized command.
//! 2. **Not Kodiak:** chat.
//! 3. **Kodiak:** read the rest of the message.
//!    - **MECP** when its first word is a severity: laya picks the spoken
//!      word (mayday, "may" of "may day", urgent, safety, routine…),
//!      then the event code from the text after it.
//!    - **Contact report** when it carries an affiliation, a direction and a
//!      distance; those are values, so the deterministic parser decides.
//!    - **Hub command** otherwise: laya picks the command.
//!
//! laya only picks labels from closed sets. It never produces values.
//! Distances, clock positions, nicknames, ETAs and search terms still come
//! from the deterministic parsers, and every intent is built from the same
//! vocabularies, so laya cannot emit a code or command Kodiak does not know.
//!
//! In [`Mode::Hybrid`] (the default) the deterministic rules run first and
//! laya is asked only when they fail to recognize a command. It may then
//! read an unknown first word as a severity (the rules still read the
//! event) or pick a plain hub command, and its answer is used only when it
//! reached the threshold with a calibrated probability.
//! [`Mode::LayaOnly`] lets laya pick every label, for benchmarking.

mod questions;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use kodiak_core::{
    Addressed, Category, Classification, Intent, IntentClassifier, MecpReport, RchCommand,
    RejectReason, Rejection,
};
use kodiak_intent::{DeterministicClassifier, first_word_wake};
use kodiak_intent::vocabulary::{MECP_V1, Mecp, RCH_V1, Rch};
use serde_json::{Value, json};

pub use questions::{
    Ask, COMMAND, MAX_OPTIONS, NONE_OF_THESE, NOT_A_SEVERITY, QuestionSet, REPORT, SeverityWord,
    WORD,
};

/// Version of the question sets and decision policy.
pub const LAYA_ID: &str = "laya/2";

/// laya's answer to one question: a probability per label (`"true"` for a
/// yes/no question), and whether that probability is calibrated.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// Probability of each label.
    pub probabilities: BTreeMap<String, f32>,
    /// False when the checkpoint's temperature for this question shape was
    /// clamped, so the probabilities are not calibrated.
    pub calibrated: bool,
}

impl Reply {
    /// The most probable label other than [`NONE_OF_THESE`].
    #[must_use]
    pub fn top(&self) -> Option<(&str, f32)> {
        self.probabilities
            .iter()
            .filter(|(label, _)| *label != NONE_OF_THESE)
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(label, p)| (label.as_str(), *p))
    }
}

/// Something that answers typed questions about a state. [`LayaOracle`]
/// runs the model; tests use a scripted oracle.
pub trait Oracle: Send + Sync {
    /// Answers every question in one batch, in order. `state` is a JSON
    /// object whose keys the instructions name in backticks.
    ///
    /// # Errors
    ///
    /// Fails when the model cannot run.
    fn ask(&self, state: &Value, questions: &[(String, Ask)]) -> Result<Vec<Reply>, String>;

    /// The checkpoint name, recorded in the classifier identifier.
    fn name(&self) -> &str;
}

impl<O: Oracle> Oracle for std::sync::Arc<O> {
    fn ask(&self, state: &Value, questions: &[(String, Ask)]) -> Result<Vec<Reply>, String> {
        self.as_ref().ask(state, questions)
    }

    fn name(&self) -> &str {
        self.as_ref().name()
    }
}

/// A laya checkpoint loaded from a local directory.
pub struct LayaOracle {
    agent: laya::Agent,
    name: String,
}

impl LayaOracle {
    /// Loads the five-file checkpoint in `dir`. Nothing is downloaded.
    ///
    /// # Errors
    ///
    /// Fails when a file is missing or the checkpoint does not load.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self, String> {
        let dir = dir.as_ref();
        let agent = laya::Agent::from_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = dir
            .file_name()
            .map_or_else(|| "laya".into(), |n| n.to_string_lossy().into_owned());
        Ok(Self { agent, name })
    }

    /// Calibration buckets whose shipped temperature was clamped.
    #[must_use]
    pub fn clamped(&self) -> &[String] {
        self.agent.clamped_temperatures()
    }

    fn calibrated(&self, ask: &Ask) -> bool {
        let bucket = match ask {
            Ask::Noul { .. } => "noul".to_owned(),
            Ask::Choice { options, .. } => format!(
                "choice:{}",
                match options.len() {
                    0..=2 => "2",
                    3..=5 => "3-5",
                    6..=10 => "6-10",
                    _ => "11+",
                }
            ),
        };
        !self
            .clamped()
            .iter()
            .any(|entry| entry.split('=').next() == Some(bucket.as_str()))
    }
}

impl Oracle for LayaOracle {
    fn ask(&self, state: &Value, questions: &[(String, Ask)]) -> Result<Vec<Reply>, String> {
        let mut set = laya::Questions::new();
        for (id, ask) in questions {
            let question: laya::Question = match ask {
                Ask::Noul { instructions } => laya::Question::noul(instructions.as_str()).into(),
                Ask::Choice {
                    instructions,
                    options,
                } => options
                    .iter()
                    .fold(
                        laya::Question::choice(instructions.as_str()),
                        |q, (label, description)| q.option(label.as_str(), description.as_str()),
                    )
                    .into(),
            };
            set.insert(id.as_str(), question);
        }
        let prediction = self
            .agent
            .predict_value(state, &set)
            .map_err(|e| e.to_string())?;
        questions
            .iter()
            .map(|(id, ask)| {
                let answer = prediction
                    .get(id)
                    .ok_or_else(|| format!("laya returned no answer for {id}"))?;
                let probabilities = match ask {
                    Ask::Noul { .. } => {
                        BTreeMap::from([("true".to_owned(), answer.as_noul().unwrap_or(0.0))])
                    }
                    Ask::Choice { options, .. } => options
                        .iter()
                        .map(|(label, _)| (label.clone(), answer.probability(label).unwrap_or(0.0)))
                        .collect(),
                };
                Ok(Reply {
                    probabilities,
                    calibrated: self.calibrated(ask),
                })
            })
            .collect()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// How laya's answers are combined with the deterministic baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// The rules run first; laya fills in only when they recognize nothing
    /// or miss the MECP severity or event.
    Hybrid,
    /// laya alone picks the severity, MECP code and hub command (values
    /// still come from the parsers). For benchmarking only.
    LayaOnly,
}

/// Decision policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    /// Minimum calibrated probability for every question on a path.
    pub threshold: f32,
    /// How answers are combined.
    pub mode: Mode,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            threshold: 0.85,
            mode: Mode::Hybrid,
        }
    }
}

/// A severity laya is sure of.
struct SeverityRead {
    level: u8,
    p: f32,
    /// Words of the command the severity word stands for.
    words: usize,
}

/// The laya classifier.
pub struct LayaClassifier<O: Oracle = LayaOracle> {
    oracle: O,
    policy: Policy,
    id: String,
    questions: QuestionSet,
    mecp: Mecp,
    rch: Rch,
    baseline: DeterministicClassifier,
    /// Every word the MECP and hub vocabularies know.
    known: BTreeSet<String>,
    /// The classification of the command last judged by `addressed`.
    cache: Mutex<Option<(String, Classification)>>,
}

impl LayaClassifier<LayaOracle> {
    /// Loads a checkpoint directory.
    ///
    /// # Errors
    ///
    /// Fails when the checkpoint does not load.
    pub fn from_dir(dir: impl AsRef<Path>, policy: Policy) -> Result<Self, String> {
        Ok(Self::new(LayaOracle::from_dir(dir)?, policy))
    }
}

impl<O: Oracle> LayaClassifier<O> {
    /// Wraps an oracle with the embedded version 1 vocabularies.
    ///
    /// # Panics
    ///
    /// Never in practice: the embedded vocabularies are validated by tests.
    #[must_use]
    pub fn new(oracle: O, policy: Policy) -> Self {
        let mecp = Mecp::parse(MECP_V1).expect("valid MECP vocabulary");
        let rch = Rch::parse(RCH_V1).expect("valid RCH vocabulary");
        let mode = match policy.mode {
            Mode::Hybrid => "hybrid",
            Mode::LayaOnly => "only",
        };
        let known = mecp
            .fillers
            .iter()
            .chain(mecp.severities.iter().flat_map(|s| &s.words))
            .chain(mecp.codes.iter().flat_map(|c| &c.phrases))
            .chain(rch.fillers.iter())
            .chain(rch.commands.iter().flat_map(|c| &c.phrases))
            .flat_map(|phrase| phrase.split_whitespace())
            .map(str::to_owned)
            .collect();
        Self {
            id: format!("{LAYA_ID}+{}/{mode}", oracle.name()),
            questions: QuestionSet::new(&mecp, &rch),
            oracle,
            policy,
            mecp,
            rch,
            baseline: DeterministicClassifier::new(),
            known,
            cache: Mutex::new(None),
        }
    }

    fn sure(&self, reply: &Reply, p: f32) -> bool {
        reply.calibrated && p >= self.policy.threshold
    }

    /// The best label across chunked questions, if laya is sure of it.
    fn best(&self, state: &Value, asks: &[(String, Ask)]) -> Result<Option<(String, f32)>, String> {
        let replies = self.oracle.ask(state, asks)?;
        Ok(replies
            .iter()
            .filter_map(|reply| reply.top().map(|(l, p)| (l.to_owned(), p, reply)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .filter(|(_, p, reply)| self.sure(reply, *p))
            .map(|(label, p, _)| (label, p)))
    }

    /// Step 3b: is the first word a severity? Probabilities of spoken forms
    /// of the same level are summed ("mayday" and "may").
    fn severity(&self, word: &str) -> Result<Option<SeverityRead>, String> {
        let replies = self
            .oracle
            .ask(&json!({ WORD: word }), &self.questions.severity())?;
        let Some(reply) = replies.first() else {
            return Err("laya returned no severity".into());
        };
        let mut levels: BTreeMap<u8, (f32, f32, usize)> = BTreeMap::new();
        for (label, p) in &reply.probabilities {
            if let Some(spoken) = self.questions.severity_word(label) {
                let entry = levels.entry(spoken.level).or_insert((0.0, 0.0, 1));
                entry.0 += p;
                if *p > entry.1 {
                    entry.1 = *p;
                    entry.2 = spoken.words;
                }
            }
        }
        Ok(levels
            .into_iter()
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
            .filter(|(_, (p, _, _))| self.sure(reply, *p))
            .map(|(level, (p, _, words))| SeverityRead { level, p, words }))
    }

    /// The flow of step 3 with laya picking the labels.
    fn read(&self, command: &str, baseline: &Classification) -> Result<Classification, String> {
        let words = words(command);
        let Some(first) = words.first() else {
            return Ok(rejected(RejectReason::NoIntent, None, "nothing after Kodiak"));
        };
        // In hybrid mode laya may read a word the vocabularies do not know
        // as a severity, but not reinterpret a known one: it read
        // the filler "need" as urgent at 0.96.
        let unknown = !self.known.contains(&first.to_lowercase());
        if (self.policy.mode != Mode::Hybrid || unknown)
            && let Some(severity) = self.severity(first)?
        {
            return self.read_mecp(&words, &severity);
        }
        if self.policy.mode == Mode::Hybrid && category(baseline).is_some() {
            // The rules placed it in a family (an MECP report led by a known
            // severity word, say): laya does not move it to another one.
            return Ok(baseline.clone());
        }
        if let Some(category) = category(baseline)
            && category == Category::Marker
        {
            // Affiliation, direction and distance are values: the parser
            // decides whether this is a contact report.
            return Ok(baseline.clone());
        }
        self.read_command(command, baseline)
    }

    /// Step 3b: an MECP report led by a severity word.
    fn read_mecp(
        &self,
        words: &[&str],
        severity: &SeverityRead,
    ) -> Result<Classification, String> {
        let level = severity.level;
        let report = words.get(severity.words..).unwrap_or_default().join(" ");
        let label = self.questions.severity_label(level).unwrap_or("routine");
        // The rules read the report with the canonical severity word, for
        // corrections and argument values.
        let rules = self.baseline.classify(&format!("{label} {report}"));
        let rules_report = match &rules.result {
            Ok(Intent::MecpReport(r)) if r.severity == level => Some(r.clone()),
            _ => None,
        };
        if self.policy.mode == Mode::Hybrid {
            // laya's event guesses were confidently wrong too often ("broke
            // a leg" as injury, "fod" as water): in hybrid mode the rules
            // read the event.
            return Ok(match rules_report {
                Some(found) => Classification {
                    result: Ok(Intent::MecpReport(found)),
                    corrections: rules.corrections,
                    confidence: Some(severity.p),
                },
                None => rules,
            });
        }
        if report.is_empty() {
            return Ok(rejected(
                RejectReason::MissingParameter,
                Some(Category::Mecp),
                "no MECP event after the severity",
            ));
        }
        let Some((code, p_code)) = self.best(&json!({ REPORT: report }), &self.questions.codes())?
        else {
            return Ok(rejected(
                RejectReason::MissingParameter,
                Some(Category::Mecp),
                "laya is not sure which MECP event this is",
            ));
        };
        let confidence = Some(severity.p * p_code);
        if let Some(found) = rules_report.filter(|r| r.code == code) {
            return Ok(Classification {
                result: Ok(Intent::MecpReport(found)),
                corrections: rules.corrections,
                confidence,
            });
        }
        let takes_argument = self
            .mecp
            .codes
            .iter()
            .any(|c| c.code == code && c.argument.is_some());
        if takes_argument {
            return Ok(Classification {
                confidence,
                ..rejected(
                    RejectReason::MissingParameter,
                    Some(Category::Mecp),
                    format!("laya read {code} but its value was not recognized"),
                )
            });
        }
        Ok(Classification {
            result: Ok(Intent::MecpReport(MecpReport {
                severity: level,
                content: format!("MECP/{level}/{code}"),
                code,
            })),
            corrections: Vec::new(),
            confidence,
        })
    }

    /// Step 3c: a hub command.
    fn read_command(
        &self,
        command: &str,
        baseline: &Classification,
    ) -> Result<Classification, String> {
        let Some((intent, p)) =
            self.best(&json!({ COMMAND: command.trim() }), &self.questions.commands())?
        else {
            return Ok(rejected(
                RejectReason::NoIntent,
                None,
                "laya is not sure this is a command",
            ));
        };
        if let Ok(found @ Intent::RchCommand(rch)) = &baseline.result
            && rch.command == intent
        {
            return Ok(Classification {
                result: Ok(found.clone()),
                corrections: baseline.corrections.clone(),
                confidence: Some(p),
            });
        }
        let plain = self
            .rch
            .commands
            .iter()
            .any(|c| c.intent == intent && c.argument.is_none());
        if !plain {
            return Ok(Classification {
                confidence: Some(p),
                ..rejected(
                    RejectReason::MissingParameter,
                    Some(Category::Rch),
                    format!("laya read {intent} but its value was not recognized"),
                )
            });
        }
        Ok(Classification {
            result: Ok(Intent::RchCommand(RchCommand {
                command: intent,
                arguments: BTreeMap::new(),
            })),
            corrections: Vec::new(),
            confidence: Some(p),
        })
    }

    fn decide(&self, command: &str) -> Classification {
        let baseline = self.baseline.classify(command);
        match self.policy.mode {
            Mode::Hybrid => {
                let recover = match &baseline.result {
                    Ok(_) => false,
                    Err(rejection) => recoverable(rejection),
                };
                if !recover {
                    return baseline;
                }
                match self.read(command, &baseline) {
                    Ok(read) if read.result.is_ok() && read.confidence.is_some() => read,
                    _ => baseline,
                }
            }
            Mode::LayaOnly => self.read(command, &baseline).unwrap_or_else(|error| {
                Classification {
                    corrections: baseline.corrections,
                    ..rejected(RejectReason::NoIntent, None, error)
                }
            }),
        }
    }
}

fn rejected(
    reason: RejectReason,
    category: Option<Category>,
    detail: impl Into<String>,
) -> Classification {
    Classification {
        result: Err(Rejection::new(reason, category, detail)),
        corrections: Vec::new(),
        confidence: None,
    }
}

fn category(classification: &Classification) -> Option<Category> {
    match &classification.result {
        Ok(intent) => Some(intent.category()),
        Err(rejection) => rejection.category,
    }
}

/// Whether a baseline rejection is a recognition failure laya may fill in:
/// nothing recognized, or an MECP report missing its severity or event.
/// Ambiguous, invalid and incomplete contact or hub commands stay rejected.
fn recoverable(rejection: &Rejection) -> bool {
    match rejection.reason {
        RejectReason::NoIntent => true,
        RejectReason::MissingParameter => {
            rejection.category.is_none() || rejection.category == Some(Category::Mecp)
        }
        _ => false,
    }
}

/// The words of a command: runs of letters, digits and apostrophes.
fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect()
}

impl<O: Oracle> IntentClassifier for LayaClassifier<O> {
    fn id(&self) -> &str {
        &self.id
    }

    fn classify(&self, command: &str) -> Classification {
        if let Ok(mut cache) = self.cache.lock()
            && let Some((cached, classification)) = cache.take()
            && cached == command
        {
            return classification;
        }
        self.decide(command)
    }

    /// Steps 1 and 2. The engine has already looked for the exact activation
    /// word anywhere in the transcript; this accepts a misheard one as the
    /// first word, but only when the rest is a recognized command, so
    /// ordinary conversation stays chat.
    fn addressed(&self, transcript: &str) -> Option<Addressed> {
        let (heard, command) = first_word_wake(transcript)?;
        let classification = self.decide(&command);
        if classification.result.is_err() {
            return None;
        }
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some((command.clone(), classification));
        }
        Some(Addressed {
            command,
            heard: Some(heard),
        })
    }
}

#[cfg(test)]
mod tests;
