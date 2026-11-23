//! The typed questions Kodiak asks laya, generated from the vocabularies.
//!
//! The probes behind `laya/2` showed that laya is reliable at literal word
//! identity ("which of these words is `word`?", "which of these events does
//! `report` describe?") and unreliable at abstract judgements ("what kind of
//! message is this?", "is this word a mishearing of Kodiak?"). So every
//! option names the surface words it stands for, synonyms are options of
//! their own whose probabilities are summed afterwards, and each question
//! reads only the part of the command it is about.
//!
//! No question offers more than [`MAX_OPTIONS`] options: laya's calibration
//! for eleven or more options is not trustworthy (the shipped English
//! temperature for that bucket sharpens instead of softening), so large
//! label sets are asked as several smaller questions in one batch.

use kodiak_intent::vocabulary::{Mecp, Rch};

/// The most options one choice question may offer.
pub const MAX_OPTIONS: usize = 10;

/// The option meaning that a question chunk does not contain the answer.
pub const NONE_OF_THESE: &str = "none_of_these";

/// Severity option meaning the word is not a severity.
pub const NOT_A_SEVERITY: &str = "other";

/// State key holding the first word of the command.
pub const WORD: &str = "word";
/// State key holding the MECP report after the severity word.
pub const REPORT: &str = "report";
/// State key holding the whole command after the activation word.
pub const COMMAND: &str = "command";

/// A typed question.
#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    /// A yes/no question; the reply holds the probability of `"true"`.
    Noul {
        /// The question.
        instructions: String,
    },
    /// Pick one label.
    Choice {
        /// The question.
        instructions: String,
        /// `(label, description)` pairs in order.
        options: Vec<(String, String)>,
    },
}

impl Ask {
    fn choice(instructions: &str, options: Vec<(String, String)>) -> Self {
        debug_assert!(options.len() <= MAX_OPTIONS);
        Self::Choice {
            instructions: instructions.into(),
            options,
        }
    }
}

/// One spoken severity word.
#[derive(Clone, Debug, PartialEq)]
pub struct SeverityWord {
    /// The word, which is also the option label.
    pub word: String,
    /// Its MECP level.
    pub level: u8,
    /// How many words of the command it stands for ("may" of "may day"
    /// stands for two).
    pub words: usize,
}

/// Builds every question from the vocabularies.
#[derive(Debug)]
pub struct QuestionSet {
    severity_words: Vec<SeverityWord>,
    /// Every spoken severity phrase with its level, lower case.
    severity_phrases: Vec<(u8, String)>,
    labels: Vec<(u8, String)>,
    codes: Vec<(String, String)>,
    commands: Vec<(String, String)>,
}

/// MECP category letters in the order codes are chunked. Needs (S) and
/// supplies on hand (H) are interleaved so "need water" and "have water"
/// are options of the same question.
const LETTER_ORDER: &str = "MTWSHPCRDLXB";

impl QuestionSet {
    /// Questions for the given vocabularies.
    #[must_use]
    pub fn new(mecp: &Mecp, rch: &Rch) -> Self {
        let mut severity_words: Vec<SeverityWord> = Vec::new();
        let mut add = |word: &str, level: u8, words: usize| {
            if !severity_words.iter().any(|s| s.word == word) {
                severity_words.push(SeverityWord {
                    word: word.to_owned(),
                    level,
                    words,
                });
            }
        };
        for severity in &mecp.severities {
            for phrase in &severity.words {
                let mut parts = phrase.split_whitespace();
                if let Some(first) = parts.next() {
                    add(first, severity.level, 1 + parts.count());
                }
            }
        }
        let labels = mecp
            .severities
            .iter()
            .map(|s| (s.level, s.label.to_lowercase()))
            .collect();
        let severity_phrases = mecp
            .severities
            .iter()
            .flat_map(|s| s.words.iter().map(|w| (s.level, w.to_lowercase())))
            .collect();

        let rank = |code: &str| {
            let letter = code.chars().next().unwrap_or('?');
            let number: u32 = code[1..].parse().unwrap_or(0);
            let group = LETTER_ORDER.find(letter).unwrap_or(LETTER_ORDER.len());
            // S and H share a group, interleaved by number.
            match letter {
                'S' => (LETTER_ORDER.find('S').unwrap_or(0), number, 0),
                'H' => (LETTER_ORDER.find('S').unwrap_or(0), number, 1),
                _ => (group, number, 0),
            }
        };
        let mut ordered: Vec<_> = mecp.codes.iter().collect();
        ordered.sort_by_key(|code| rank(&code.code));
        let codes = ordered
            .iter()
            .map(|code| {
                let phrases: Vec<&str> = code.phrases.iter().take(3).map(String::as_str).collect();
                (
                    code.code.clone(),
                    format!("{}: {}", code.label.to_lowercase(), phrases.join(", ")),
                )
            })
            .collect();

        let mut commands: Vec<(String, Vec<String>)> = Vec::new();
        for command in &rch.commands {
            let phrases = command.phrases.iter().take(3).cloned();
            match commands
                .iter_mut()
                .find(|(name, _)| *name == command.intent)
            {
                Some((_, all)) => all.extend(phrases),
                None => commands.push((command.intent.clone(), phrases.collect())),
            }
        }
        let commands = commands
            .into_iter()
            .map(|(intent, phrases)| (intent, phrases.join(", ")))
            .collect();
        Self {
            severity_words,
            severity_phrases,
            labels,
            codes,
            commands,
        }
    }

    /// Step 3b: is the first word of the command a severity word? Asked of
    /// `word` alone, with one option per spoken form, in chunks of at most
    /// [`MAX_OPTIONS`].
    #[must_use]
    pub fn severity(&self) -> Vec<(String, Ask)> {
        // Like `chunked`, but the extra option says the word is not a
        // severity at all.
        let options: Vec<(String, String)> = self
            .severity_words
            .iter()
            .map(|s| (s.word.clone(), s.word.clone()))
            .collect();
        let per = MAX_OPTIONS - 1;
        let count = options.len().div_ceil(per).max(1);
        let size = options.len().div_ceil(count).max(1);
        options
            .chunks(size)
            .enumerate()
            .map(|(index, chunk)| {
                let mut chunk = chunk.to_vec();
                chunk.push((NOT_A_SEVERITY.into(), "another word".into()));
                let key = if count == 1 {
                    "severity".to_owned()
                } else {
                    format!("severity{index}")
                };
                (key, Ask::choice(&format!("Which word is `{WORD}`?"), chunk))
            })
            .collect()
    }

    /// The spoken severity word behind an option label.
    #[must_use]
    pub fn severity_word(&self, label: &str) -> Option<&SeverityWord> {
        self.severity_words.iter().find(|s| s.word == label)
    }

    /// Whether `words` spell a severity phrase of `level` ("may they" for
    /// mayday), ignoring case.
    #[must_use]
    pub fn is_severity_phrase(&self, level: u8, words: &[&str]) -> bool {
        let spoken = words.join(" ").to_lowercase();
        self.severity_phrases
            .iter()
            .any(|(l, phrase)| *l == level && *phrase == spoken)
    }

    /// The vocabulary label of an MECP level, such as `routine`.
    #[must_use]
    pub fn severity_label(&self, level: u8) -> Option<&str> {
        self.labels
            .iter()
            .find(|(l, _)| *l == level)
            .map(|(_, label)| label.as_str())
    }

    /// Step 3b: the MECP event in `report`, every code in chunks.
    #[must_use]
    pub fn codes(&self) -> Vec<(String, Ask)> {
        chunked(
            "code",
            &format!("Which event does `{REPORT}` describe?"),
            &self.codes,
        )
    }

    /// Step 3c: the hub command in `command`, in chunks.
    #[must_use]
    pub fn commands(&self) -> Vec<(String, Ask)> {
        chunked(
            "command",
            &format!("Which hub command does `{COMMAND}` ask for?"),
            &self.commands,
        )
    }
}

/// Splits options into questions of at most [`MAX_OPTIONS`], each with a
/// [`NONE_OF_THESE`] option.
fn chunked(prefix: &str, instructions: &str, options: &[(String, String)]) -> Vec<(String, Ask)> {
    let per = MAX_OPTIONS - 1;
    let count = options.len().div_ceil(per).max(1);
    let size = options.len().div_ceil(count).max(1);
    options
        .chunks(size)
        .enumerate()
        .map(|(index, chunk)| {
            let mut chunk = chunk.to_vec();
            chunk.push((NONE_OF_THESE.into(), "none of these".into()));
            (format!("{prefix}{index}"), Ask::choice(instructions, chunk))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use kodiak_intent::vocabulary::{MECP_V1, RCH_V1};

    use super::*;

    fn set() -> QuestionSet {
        QuestionSet::new(&Mecp::parse(MECP_V1).unwrap(), &Rch::parse(RCH_V1).unwrap())
    }

    fn labels(asks: &[(String, Ask)]) -> Vec<String> {
        asks.iter()
            .flat_map(|(_, ask)| match ask {
                Ask::Choice { options, .. } => {
                    assert!(options.len() <= MAX_OPTIONS, "{} options", options.len());
                    options.iter().map(|(label, _)| label.clone()).collect()
                }
                Ask::Noul { .. } => Vec::new(),
            })
            .filter(|label| label != NONE_OF_THESE)
            .collect()
    }

    #[test]
    fn every_question_is_small_and_every_code_and_command_is_reachable() {
        let set = set();
        labels(&set.severity());
        let mecp = Mecp::parse(MECP_V1).unwrap();
        let codes: BTreeSet<String> = labels(&set.codes()).into_iter().collect();
        let all: BTreeSet<String> = mecp.codes.iter().map(|code| code.code.clone()).collect();
        assert_eq!(codes, all);
        let rch = Rch::parse(RCH_V1).unwrap();
        let commands: BTreeSet<String> = labels(&set.commands()).into_iter().collect();
        let all: BTreeSet<String> = rch.commands.iter().map(|c| c.intent.clone()).collect();
        assert_eq!(commands, all);
    }

    #[test]
    fn need_and_have_share_a_question() {
        let set = set();
        let with = |code: &str| {
            set.codes()
                .iter()
                .position(|(_, ask)| labels(&[(String::new(), ask.clone())]).contains(&code.into()))
        };
        assert_eq!(with("S01"), with("H01"));
        assert_eq!(with("S02"), with("H02"));
    }

    #[test]
    fn severity_words_map_to_mecp_levels() {
        let set = set();
        let level = |w: &str| set.severity_word(w).map(|s| (s.level, s.words));
        assert_eq!(level("mayday"), Some((0, 1)));
        assert_eq!(level("may"), Some((0, 2)));
        assert_eq!(level("urgent"), Some((1, 1)));
        assert_eq!(level("normal"), Some((3, 1)));
        assert_eq!(level(NOT_A_SEVERITY), None);
        assert_eq!(set.severity_label(3), Some("routine"));
    }
}
