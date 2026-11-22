//! MECP report extraction.

use std::fmt::Write as _;

use kodiak_core::{Category, Correction, Intent, MecpReport, RejectReason, Rejection};

use crate::fuzzy::as_filler;
use crate::text::{Token, number};
use crate::vocabulary::{CodeArgument, Mecp, phrase_words};

pub struct Parser {
    vocabulary: Mecp,
    phrases: Vec<(Vec<String>, usize)>,
}

/// A severity word, also with a trailing "s": recognizers wrote a spoken
/// "mayday" as "Maydays" on live NFM audio.
fn same_word(spoken: &str, word: &str) -> bool {
    spoken == word || spoken.strip_suffix('s') == Some(word)
}

fn reject(reason: RejectReason, detail: impl Into<String>) -> Rejection {
    Rejection::new(reason, Some(Category::Mecp), detail)
}

impl Parser {
    pub fn new(vocabulary: Mecp) -> Self {
        let phrases = vocabulary
            .codes
            .iter()
            .enumerate()
            .flat_map(|(index, code)| {
                code.phrases
                    .iter()
                    .map(move |phrase| (phrase_words(phrase), index))
            })
            .collect();
        Self {
            vocabulary,
            phrases,
        }
    }

    /// Joins multi-word severity phrases ("may day") into the level's first
    /// word ("mayday"), so recognizer spellings behave like the single word.
    pub fn join_severities(&self, tokens: &[Token]) -> Vec<Token> {
        let phrases: Vec<(Vec<String>, &str)> = self
            .vocabulary
            .severities
            .iter()
            .flat_map(|severity| {
                severity
                    .words
                    .iter()
                    .map(|word| (phrase_words(word), severity.words[0].as_str()))
            })
            .filter(|(words, _)| words.len() > 1)
            .collect();
        let mut joined = Vec::with_capacity(tokens.len());
        let mut index = 0;
        'tokens: while index < tokens.len() {
            for (words, canonical) in &phrases {
                let end = index + words.len();
                if end <= tokens.len()
                    && tokens[index..end]
                        .iter()
                        .zip(words)
                        .all(|(token, word)| same_word(&token.word, word))
                {
                    let original: Vec<&str> = tokens[index..end]
                        .iter()
                        .map(|t| t.original.as_str())
                        .collect();
                    joined.push(Token {
                        word: (*canonical).to_owned(),
                        original: original.join(" "),
                    });
                    index = end;
                    continue 'tokens;
                }
            }
            joined.push(tokens[index].clone());
            index += 1;
        }
        joined
    }

    /// Second chance for known words that no phrase used, e.g. "meet" (from
    /// "meet at") heard in place of the filler "need". Only fillers qualify.
    fn drop_misheard_fillers(&self, rest: &mut Vec<&str>, corrections: &mut Vec<Correction>) {
        rest.retain(|word| match as_filler(word, &self.vocabulary.fillers) {
            Some(filler) => {
                corrections.push(Correction {
                    heard: (*word).to_owned(),
                    read_as: filler.to_owned(),
                });
                false
            }
            None => true,
        });
    }

    /// The severity level named by a word.
    pub fn severity(&self, word: &str) -> Option<u8> {
        self.vocabulary
            .severities
            .iter()
            .find(|severity| severity.words.iter().any(|w| same_word(word, w)))
            .map(|severity| severity.level)
    }

    /// Whether any MECP event phrase occurs in the words.
    pub fn mentions_event(&self, words: &[&str]) -> bool {
        !self.matches(words).is_empty()
    }

    /// Longest non-overlapping phrase matches as (start, end, code index).
    fn matches(&self, words: &[&str]) -> Vec<(usize, usize, usize)> {
        let mut found: Vec<(usize, usize, usize)> = Vec::new();
        for start in 0..words.len() {
            for (phrase, code) in &self.phrases {
                let end = start + phrase.len();
                if end <= words.len() && words[start..end].iter().zip(phrase).all(|(a, b)| a == b) {
                    found.push((start, end, *code));
                }
            }
        }
        found.sort_by_key(|&(start, end, _)| (std::cmp::Reverse(end - start), start));
        let mut chosen: Vec<(usize, usize, usize)> = Vec::new();
        for candidate in found {
            if chosen
                .iter()
                .all(|c| candidate.1 <= c.0 || candidate.0 >= c.1)
            {
                chosen.push(candidate);
            }
        }
        chosen.sort_unstable();
        chosen
    }

    pub fn parse(
        &self,
        tokens: &[Token],
        corrections: &mut Vec<Correction>,
    ) -> Result<Intent, Rejection> {
        let mut severities = Vec::new();
        let mut words = Vec::new();
        for token in tokens {
            match self.severity(&token.word) {
                Some(level) => severities.push(level),
                None => words.push(token.word.as_str()),
            }
        }
        severities.dedup();
        let severity = match severities.as_slice() {
            [] => {
                return Err(reject(
                    RejectReason::MissingParameter,
                    "MECP severity is missing (mayday, urgent, safety or routine)",
                ));
            }
            [one] => *one,
            _ => {
                return Err(reject(
                    RejectReason::Ambiguous,
                    "more than one MECP severity",
                ));
            }
        };
        let matches = self.matches(&words);
        let mut codes: Vec<usize> = matches.iter().map(|m| m.2).collect();
        codes.sort_unstable();
        codes.dedup();
        let code = match codes.as_slice() {
            [] => {
                return Err(reject(
                    RejectReason::MissingParameter,
                    "no MECP event recognized",
                ));
            }
            [one] => &self.vocabulary.codes[*one],
            _ => {
                let names: Vec<_> = codes
                    .iter()
                    .map(|i| self.vocabulary.codes[*i].code.as_str())
                    .collect();
                return Err(reject(
                    RejectReason::Ambiguous,
                    format!("more than one MECP event: {}", names.join(", ")),
                ));
            }
        };
        let mut rest: Vec<&str> = Vec::new();
        let mut cursor = 0;
        for (start, end, _) in &matches {
            rest.extend(&words[cursor..*start]);
            cursor = *end;
        }
        rest.extend(&words[cursor..]);
        rest.retain(|word| !self.vocabulary.fillers.contains(*word));
        self.drop_misheard_fillers(&mut rest, corrections);

        let mut content = format!("MECP/{severity}/{}", code.code);
        if let Some(argument) = code.argument
            && let Some((value, used)) = number(&rest)
        {
            let units: &[&str] = match argument {
                CodeArgument::EtaMinutes => &["minutes", "minute", "min", "mins"],
                CodeArgument::Pax => &["people", "persons", "pax", "person"],
            };
            let used = used + usize::from(rest.get(used).is_some_and(|w| units.contains(w)));
            if value.fract() != 0.0 || !(1.0..=9999.0).contains(&value) {
                return Err(reject(
                    RejectReason::InvalidParameter,
                    "MECP count must be a whole number 1-9999",
                ));
            }
            match argument {
                CodeArgument::EtaMinutes => {
                    let _ = write!(content, " {value}");
                }
                CodeArgument::Pax => {
                    let _ = write!(content, " {value}pax");
                }
            }
            rest.drain(..used);
        }
        if !rest.is_empty() {
            return Err(reject(
                RejectReason::InvalidParameter,
                format!("unrecognized words in MECP report: {}", rest.join(" ")),
            ));
        }
        Ok(Intent::MecpReport(MecpReport {
            severity,
            code: code.code.clone(),
            content,
        }))
    }
}
