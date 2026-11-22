//! Sound-aware correction of transcribed words that no vocabulary contains.
//!
//! Recognizers misspell words on noisy narrowband FM audio ("meet" for
//! "need"). Before the strict parsers run, each unknown word is compared with
//! every vocabulary word by a weighted edit distance in which letters that
//! sound alike are cheap to swap. A word is replaced only when one vocabulary
//! word is clearly the closest and within a length-dependent limit. Every
//! replacement is reported as a [`Correction`].
//!
//! Words that are already known, numbers, and words shorter than four letters
//! are never changed. Negations ("not", "no", "don", "never", …) are known
//! words, so they can never be corrected into something else and still cause
//! a rejection wherever the grammar does not expect them.

use std::collections::BTreeSet;

use kodiak_core::Correction;

use crate::text::Token;

/// Letter groups that narrowband radio speech and recognizers confuse.
const CONFUSABLE: &[&[char]] = &[
    &['m', 'n'],
    &['t', 'd'],
    &['p', 'b'],
    &['k', 'g', 'c', 'q'],
    &['s', 'z', 'c'],
    &['f', 'v'],
    &['a', 'e', 'i', 'o', 'u', 'y'],
];

/// Words that must never be corrected: they change or negate a command.
pub const PROTECTED: &[&str] = &[
    "not",
    "no",
    "never",
    "don",
    "doesn",
    "didn",
    "isn",
    "aren",
    "wasn",
    "won",
    "cannot",
    "cancel",
    "without",
    "nothing",
    "none",
    "stop",
    "ignore",
    "disregard",
];

/// Shortest word that may be corrected.
const MIN_LEN: usize = 4;

/// Costs are in half units: a confusable substitution costs 1, any other
/// edit costs 2.
fn substitution(a: char, b: char) -> u32 {
    if a == b {
        0
    } else if CONFUSABLE
        .iter()
        .any(|group| group.contains(&a) && group.contains(&b))
    {
        1
    } else {
        2
    }
}

/// Weighted Levenshtein distance in half units.
pub fn distance(heard: &str, word: &str) -> u32 {
    let heard: Vec<char> = heard.chars().collect();
    let word: Vec<char> = word.chars().collect();
    let mut previous: Vec<u32> = (0..=word.len())
        .map(|i| 2 * u32::try_from(i).unwrap_or(u32::MAX / 4))
        .collect();
    for &a in &heard {
        let mut current = vec![previous[0] + 2];
        for (j, &b) in word.iter().enumerate() {
            let value = (previous[j] + substitution(a, b))
                .min(previous[j + 1] + 2)
                .min(current[j] + 2);
            current.push(value);
        }
        previous = current;
    }
    previous[word.len()]
}

/// Largest accepted distance, in half units: one edit for words of up to six
/// letters, one and a half for longer words.
fn limit(length: usize) -> u32 {
    if length <= 6 { 2 } else { 3 }
}

/// A word left over by a strict parser, read as one of the given filler
/// words if it clearly sounds like one ("meet" as "need"). Fillers carry
/// no meaning, so this can drop a misheard word but never add an action.
pub fn as_filler<'a>(heard: &str, fillers: &'a BTreeSet<String>) -> Option<&'a str> {
    let length = heard.chars().count();
    if length < MIN_LEN
        || !heard.chars().all(char::is_alphabetic)
        || PROTECTED.contains(&heard)
        || fillers.contains(heard)
    {
        return None;
    }
    let mut costs: Vec<(u32, &str)> = fillers
        .iter()
        .filter(|filler| filler.chars().count() >= 3)
        .map(|filler| (distance(heard, filler), filler.as_str()))
        .collect();
    costs.sort_unstable();
    match costs.as_slice() {
        [(cost, word), rest @ ..]
            if *cost <= limit(length) && rest.first().is_none_or(|(next, _)| next > cost) =>
        {
            Some(word)
        }
        _ => None,
    }
}

pub struct Corrector {
    known: BTreeSet<String>,
    candidates: Vec<String>,
}

impl Corrector {
    /// Corrects unknown words to `known` words, never to one in `excluded`.
    pub fn new(mut known: BTreeSet<String>, excluded: &BTreeSet<String>) -> Self {
        known.extend(PROTECTED.iter().map(|word| (*word).to_owned()));
        let candidates = known
            .iter()
            .filter(|word| word.chars().count() >= 3 && word.chars().all(char::is_alphabetic))
            .filter(|word| !PROTECTED.contains(&word.as_str()) && !excluded.contains(*word))
            .cloned()
            .collect();
        Self { known, candidates }
    }

    /// The single closest vocabulary word, if it is close enough and closer
    /// than every other candidate.
    fn closest(&self, heard: &str) -> Option<&str> {
        let length = heard.chars().count();
        if length < MIN_LEN || !heard.chars().all(char::is_alphabetic) || self.known.contains(heard)
        {
            return None;
        }
        let mut best: Option<(u32, &str)> = None;
        let mut second = u32::MAX;
        for candidate in &self.candidates {
            let cost = distance(heard, candidate);
            match best {
                Some((current, _)) if cost < current => {
                    second = current;
                    best = Some((cost, candidate));
                }
                Some((current, _)) if cost == current => second = cost,
                Some(_) => second = second.min(cost),
                None => best = Some((cost, candidate)),
            }
        }
        let (cost, word) = best?;
        (cost <= limit(length) && cost < second).then_some(word)
    }

    /// Replaces unknown words with their unambiguous vocabulary match.
    /// Tokens marked in `verbatim` (free text, callsign and reference
    /// values) keep their transcribed spelling.
    pub fn correct(&self, tokens: Vec<Token>, verbatim: &[bool]) -> (Vec<Token>, Vec<Correction>) {
        let mut corrections = Vec::new();
        let tokens = tokens
            .into_iter()
            .enumerate()
            .map(|(index, mut token)| {
                if verbatim.get(index).copied().unwrap_or(false) {
                    return token;
                }
                if let Some(word) = self.closest(&token.word) {
                    corrections.push(Correction {
                        heard: token.word.clone(),
                        read_as: word.to_owned(),
                    });
                    word.clone_into(&mut token.word);
                }
                token
            })
            .collect();
        (tokens, corrections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::tokenize;

    fn corrector() -> Corrector {
        Corrector::new(
            [
                "need", "water", "mayday", "heat", "meds", "hostile", "dead", "food",
            ]
            .map(String::from)
            .into(),
            &BTreeSet::new(),
        )
    }

    fn read(text: &str) -> (Vec<String>, Vec<Correction>) {
        let (tokens, corrections) = corrector().correct(tokenize(text), &[]);
        (tokens.into_iter().map(|t| t.word).collect(), corrections)
    }

    #[test]
    fn confusable_sounds_cost_less() {
        assert_eq!(distance("meet", "need"), 2);
        assert_eq!(distance("meet", "heat"), 3);
        assert_eq!(distance("maydays", "mayday"), 2);
        assert_eq!(distance("hostyle", "hostile"), 1);
    }

    #[test]
    fn leftover_words_may_only_become_fillers() {
        let fillers: BTreeSet<String> = ["need", "here", "the"].map(String::from).into();
        assert_eq!(as_filler("meet", &fillers), Some("need"));
        assert_eq!(as_filler("never", &fillers), None);
        assert_eq!(as_filler("anymore", &fillers), None);
        assert_eq!(as_filler("the", &fillers), None);
    }

    #[test]
    fn only_clear_close_matches_are_corrected() {
        let (words, corrections) = read("Mayday meet water");
        assert_eq!(words, ["mayday", "need", "water"]);
        assert_eq!(
            corrections,
            [Correction {
                heard: "meet".into(),
                read_as: "need".into()
            }]
        );
        // Too short, a number, already known, or too far: unchanged.
        assert_eq!(
            read("fod 200 need banana").0,
            ["fod", "200", "need", "banana"]
        );
        // Protected negations are never rewritten.
        assert_eq!(read("never nothing").0, ["never", "nothing"]);
        // The closest sound wins: "deed" reads as "dead", not "need".
        assert_eq!(read("deed").0, ["dead"]);
    }
}
