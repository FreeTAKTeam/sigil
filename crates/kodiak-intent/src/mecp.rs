//! MECP report extraction.
//!
//! One pass over the words after the severity:
//!
//! 1. Keyword values ("callsign bravo six", "time fourteen thirty",
//!    "language french", "reference alpha one", "details …") are taken first,
//!    so their words are never read as events. A keyword inside an event
//!    phrase ("estimated time of arrival") is not a keyword.
//! 2. Event phrases are matched in the remaining words, longest first. Two
//!    phrases that cross each other make the report ambiguous.
//! 3. The words left over may only be fillers, a person count, coordinates,
//!    an ETA (with R03), or a time followed by "zulu" or "local". A negation
//!    cancels the report; any other word rejects it.
//!
//! The codes keep the order in which they were spoken. Codes listed together
//! under `[[conflicts]]` make the report contradictory. The message is
//! encoded with [`MecpReport::encode`], which enforces the MECP wire rules
//! and the 200-byte limit.

use std::collections::BTreeSet;

use kodiak_core::{
    Category, Coordinates, Correction, Intent, MecpParameters, MecpReport, RejectReason, Rejection,
};

use crate::fuzzy::as_filler;
use crate::text::{Token, number, single_digit};
use crate::vocabulary::{CodeArgument, Keyword, Mecp, phrase_words};

pub struct Parser {
    vocabulary: Mecp,
    phrases: Vec<(Vec<String>, usize)>,
    keywords: Vec<(Vec<String>, Keyword)>,
    grammar: BTreeSet<String>,
}

/// A severity word, also with a trailing "s": recognizers wrote a spoken
/// "mayday" as "Maydays" on live NFM audio.
fn same_word(spoken: &str, word: &str) -> bool {
    spoken == word || spoken.strip_suffix('s') == Some(word)
}

fn reject(reason: RejectReason, detail: impl Into<String>) -> Rejection {
    Rejection::new(reason, Some(Category::Mecp), detail)
}

/// Stores a value, or fails when a different one was already given.
fn set<T: PartialEq>(slot: &mut Option<T>, value: T, what: &str) -> Result<(), Rejection> {
    match slot {
        Some(existing) if *existing != value => Err(reject(
            RejectReason::Contradictory,
            format!("two different values for the {what}"),
        )),
        _ => {
            *slot = Some(value);
            Ok(())
        }
    }
}

/// A whole number in a range.
fn whole(value: f64, low: f64, high: f64) -> Option<u16> {
    // Range-checked, so the conversion is exact.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (value.fract() == 0.0 && (low..=high).contains(&value)).then_some(value as u16)
}

/// An ordinal word read as its number in a callsign ("bravo sixth" is how
/// "bravo six" was transcribed on live audio).
fn ordinal(word: &str) -> Option<u16> {
    let value = match word {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        _ => return None,
    };
    Some(value)
}

/// A span of matched words: start, end and code index.
type Span = (usize, usize, usize);

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
        let mut keywords: Vec<(Vec<String>, Keyword)> = vocabulary
            .parameters
            .keywords()
            .into_iter()
            .map(|(phrase, kind)| (phrase_words(phrase), kind))
            .collect();
        // Longest keyword first: "time of event" before "time".
        keywords.sort_by_key(|(words, _)| std::cmp::Reverse(words.len()));
        let mut parser = Self {
            vocabulary,
            phrases,
            keywords,
            grammar: BTreeSet::new(),
        };
        parser.grammar = parser.grammar_words().collect();
        parser
    }

    /// `version.revision` of the vocabulary.
    pub fn release(&self) -> String {
        self.vocabulary.release()
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

    /// Whether a word negates or cancels a report.
    pub fn is_negation(&self, word: &str) -> bool {
        self.vocabulary.negations.contains(word)
    }

    /// Second chance for known words that no phrase used, e.g. "meet" (from
    /// "meet at") heard in place of the filler "need". Only fillers qualify.
    fn drop_misheard_fillers(&self, rest: &mut Vec<&str>, corrections: &mut Vec<Correction>) {
        rest.retain(|word| {
            if self.grammar.contains(*word) {
                return true;
            }
            match as_filler(word, &self.vocabulary.fillers) {
                Some(filler) => {
                    corrections.push(Correction {
                        heard: (*word).to_owned(),
                        read_as: filler.to_owned(),
                    });
                    false
                }
                None => true,
            }
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

    /// The first negation that is not part of an event phrase ("not
    /// breathing" and "cancel beacon" are events).
    pub fn uncovered_negation<'a>(&self, words: &[&'a str]) -> Option<&'a str> {
        let spans = self.candidates(words);
        words.iter().enumerate().find_map(|(at, word)| {
            (self.is_negation(word) && !spans.iter().any(|s| s.0 <= at && at < s.1))
                .then_some(*word)
        })
    }

    /// Whether any MECP event phrase occurs in the words.
    pub fn mentions_event(&self, words: &[&str]) -> bool {
        !self.candidates(words).is_empty()
    }

    /// Every phrase occurrence as (start, end, code index).
    fn candidates(&self, words: &[&str]) -> Vec<Span> {
        let mut found = Vec::new();
        for start in 0..words.len() {
            for (phrase, code) in &self.phrases {
                let end = start + phrase.len();
                if end <= words.len() && words[start..end].iter().zip(phrase).all(|(a, b)| a == b) {
                    found.push((start, end, *code));
                }
            }
        }
        found
    }

    /// Longest non-overlapping phrase matches, in order. Fails with the two
    /// codes when a phrase of another code crosses a chosen match.
    fn matches(&self, words: &[&str]) -> Result<Vec<Span>, (usize, usize)> {
        let mut found = self.candidates(words);
        found.sort_by_key(|&(start, end, _)| (std::cmp::Reverse(end - start), start));
        let mut chosen: Vec<Span> = Vec::new();
        for candidate in &found {
            if chosen
                .iter()
                .all(|c| candidate.1 <= c.0 || candidate.0 >= c.1)
            {
                chosen.push(*candidate);
            }
        }
        for candidate in &found {
            for c in &chosen {
                let overlaps = candidate.0 < c.1 && c.0 < candidate.1;
                let inside = c.0 <= candidate.0 && candidate.1 <= c.1;
                if overlaps && !inside && candidate.2 != c.2 {
                    return Err((c.2, candidate.2));
                }
            }
        }
        chosen.sort_unstable();
        Ok(chosen)
    }

    /// The keyword starting at `words[0]`, with its length.
    fn keyword(&self, words: &[&str]) -> Option<(usize, Keyword)> {
        self.keywords.iter().find_map(|(phrase, kind)| {
            (words.len() >= phrase.len() && words.iter().zip(phrase).all(|(a, b)| a == b))
                .then_some((phrase.len(), *kind))
        })
    }

    /// A word that ends a keyword value.
    fn ends_value(&self, words: &[&str], at: usize, phrase_starts: &[usize]) -> bool {
        let word = words[at];
        self.vocabulary.fillers.contains(word)
            || self.is_negation(word)
            || self.keyword(&words[at..]).is_some()
            || phrase_starts.contains(&at)
    }

    /// The words of a callsign or reference value, with its length.
    fn value_span(&self, words: &[&str], phrase_starts: &[usize], from: usize) -> usize {
        let mut used = 0;
        while from + used < words.len()
            && used < self.vocabulary.parameters.max_value_words
            && !self.ends_value(words, from + used, phrase_starts)
        {
            used += 1;
        }
        used
    }

    /// A callsign or reference: up to `max_value_words` words, numbers
    /// written as digits, words joined with `-` ("bravo six" → `BRAVO-6`).
    /// Words must be numbers or listed `identifier_words`; an ordinal
    /// ("bravo sixth") is read as its number.
    fn identifier(
        &self,
        words: &[&str],
        phrase_starts: &[usize],
        from: usize,
    ) -> Result<(String, usize), Rejection> {
        let used = self.value_span(words, phrase_starts, from);
        let allowed = &self.vocabulary.parameters.identifier_words;
        let mut value = String::new();
        let mut last_digit = false;
        for &word in &words[from..from + used] {
            let digits = if matches!(word, "o" | "oh") {
                None
            } else {
                ordinal(word)
                    .or_else(|| number(&[word]).and_then(|(v, _)| whole(v, 0.0, 9999.0)))
                    .map(|v| v.to_string())
            };
            if digits.is_none() && !allowed.is_empty() && !allowed.contains(word) {
                return Err(reject(
                    RejectReason::InvalidParameter,
                    format!(
                        "{word:?} in a callsign or reference is neither a phonetic-alphabet word nor a number"
                    ),
                ));
            }
            if !(value.is_empty() || last_digit && digits.is_some()) {
                value.push('-');
            }
            match &digits {
                Some(digits) => value.push_str(digits),
                None => value.push_str(&word.to_uppercase()),
            }
            last_digit = digits.is_some();
        }
        Ok((value, used))
    }

    /// Which words keep their transcribed spelling: everything after a text
    /// keyword, and callsign and reference values. The fuzzy corrector must
    /// not change them ("north gate" is not "north late", "sixth" is not
    /// "sixty").
    pub fn verbatim(&self, words: &[&str]) -> Vec<bool> {
        let spans = self.candidates(words);
        let starts: Vec<usize> = spans.iter().map(|s| s.0).collect();
        let mut keep = vec![false; words.len()];
        let mut index = 0;
        while index < words.len() {
            let inside = spans.iter().any(|s| s.0 <= index && index < s.1);
            match self.keyword(&words[index..]).filter(|_| !inside) {
                Some((length, Keyword::Text)) => {
                    for flag in &mut keep[index + length..] {
                        *flag = true;
                    }
                    break;
                }
                Some((length, Keyword::Callsign | Keyword::Reference)) => {
                    let from = index + length;
                    let used = self.value_span(words, &starts, from);
                    for flag in &mut keep[from..from + used] {
                        *flag = true;
                    }
                    index = from + used;
                }
                Some((length, _)) => index += length,
                None => index += 1,
            }
        }
        keep
    }

    /// A time of day after a time keyword: `1430`, `14:30`, "fourteen
    /// thirty", "fourteen hundred", "zero nine thirty", "one four three zero".
    fn time(words: &[&str]) -> Option<(String, usize)> {
        let hhmm = |hour: u32, minute: u32| {
            (hour < 24 && minute < 60).then(|| format!("{hour:02}{minute:02}"))
        };
        let first = *words.first()?;
        if let Some((hour, minute)) = first.split_once(':') {
            return Some((hhmm(hour.parse().ok()?, minute.parse().ok()?)?, 1));
        }
        if first.len() == 4 && first.bytes().all(|b| b.is_ascii_digit()) {
            let value: u32 = first.parse().ok()?;
            return Some((hhmm(value / 100, value % 100)?, 1));
        }
        let digits: Vec<u32> = words
            .iter()
            .map_while(|w| single_digit(w))
            .take(4)
            .collect();
        if digits.len() == 4 {
            let value = digits.iter().fold(0, |total, d| total * 10 + d);
            return Some((hhmm(value / 100, value % 100)?, 4));
        }
        // "zero nine thirty": the hour as two digits.
        let (hour, used) = if digits.len() >= 2 && digits[0] <= 2 {
            (digits[0] * 10 + digits[1], 2)
        } else {
            let (value, used) = number(words)?;
            let value = u32::from(whole(value, 0.0, 2359.0)?);
            if value >= 100 {
                return Some((hhmm(value / 100, value % 100)?, used));
            }
            (value, used)
        };
        let rest = &words[used..];
        if rest.first() == Some(&"hundred") {
            return Some((hhmm(hour, 0)?, used + 1));
        }
        if let [o, digit, ..] = rest
            && matches!(*o, "o" | "oh")
            && let Some(minute) = single_digit(digit)
        {
            return Some((hhmm(hour, minute)?, used + 2));
        }
        let (minute, length) = number(rest)?;
        Some((
            hhmm(hour, u32::from(whole(minute, 0.0, 59.0)?))?,
            used + length,
        ))
    }

    /// A signed decimal: "-73.5", "minus seventy three point five".
    fn signed(&self, words: &[&str]) -> Option<(f64, usize)> {
        let first = *words.first()?;
        if let Some(digits) = first.strip_prefix('-') {
            return digits
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())
                .map(|v| (-v, 1));
        }
        if self
            .vocabulary
            .parameters
            .minus_words
            .iter()
            .any(|w| w == first)
        {
            let (value, used) = number(&words[1..])?;
            return Some((-value, used + 1));
        }
        number(words)
    }

    /// A latitude and longitude, both with a fractional part.
    fn coordinates(&self, words: &[&str]) -> Option<(f64, f64, usize)> {
        let (latitude, mut used) = self.signed(words)?;
        if words.get(used).is_some_and(|w| {
            self.vocabulary
                .parameters
                .coordinate_separators
                .iter()
                .any(|s| s == w)
        }) {
            used += 1;
        }
        let (longitude, length) = self.signed(&words[used..])?;
        (latitude.fract() != 0.0 && longitude.fract() != 0.0).then_some((
            latitude,
            longitude,
            used + length,
        ))
    }

    /// Takes the keyword values, marking their words as used.
    fn keyword_values(
        &self,
        tokens: &[&Token],
        words: &[&str],
        used: &mut [bool],
        parameters: &mut MecpParameters,
    ) -> Result<(), Rejection> {
        let spans = self.candidates(words);
        let inside = |at: usize| spans.iter().any(|s| s.0 <= at && at < s.1);
        let starts: Vec<usize> = spans.iter().map(|s| s.0).collect();
        let mut index = 0;
        while index < words.len() {
            let Some((length, kind)) = self.keyword(&words[index..]).filter(|_| !inside(index))
            else {
                index += 1;
                continue;
            };
            let from = index + length;
            let keyword = words[index..from].join(" ");
            let missing = || {
                reject(
                    RejectReason::MissingParameter,
                    format!("\"{keyword}\" has no value"),
                )
            };
            let taken = match kind {
                Keyword::Text => {
                    let text: Vec<&str> =
                        tokens[from..].iter().map(|t| t.original.as_str()).collect();
                    if text.is_empty() {
                        return Err(missing());
                    }
                    set(&mut parameters.text, text.join(" "), "descriptive text")?;
                    words.len() - from
                }
                Keyword::Language => {
                    let name = words.get(from).ok_or_else(missing)?;
                    let code = self.vocabulary.languages.get(*name).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            format!("unknown language {name:?}"),
                        )
                    })?;
                    set(&mut parameters.language, code.clone(), "language")?;
                    1
                }
                Keyword::Timestamp => {
                    if from >= words.len() {
                        return Err(missing());
                    }
                    let (time, mut taken) = Self::time(&words[from..]).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            format!("\"{keyword}\" is not followed by a valid time of day"),
                        )
                    })?;
                    if words.get(from + taken).is_some_and(|w| {
                        self.vocabulary
                            .parameters
                            .timestamp_trailing
                            .iter()
                            .any(|t| t == w)
                    }) {
                        taken += 1;
                    }
                    set(&mut parameters.timestamp, time, "time")?;
                    taken
                }
                Keyword::Callsign | Keyword::Reference => {
                    let (value, taken) = self.identifier(words, &starts, from)?;
                    if taken == 0 {
                        return Err(missing());
                    }
                    if kind == Keyword::Callsign {
                        set(&mut parameters.callsign, value, "callsign")?;
                    } else if !parameters.references.contains(&value) {
                        parameters.references.push(value);
                    }
                    taken
                }
            };
            for flag in &mut used[index..from + taken] {
                *flag = true;
            }
            index = from + taken;
        }
        Ok(())
    }

    /// Event matches in the words not used by keyword values.
    fn events(&self, words: &[&str], used: &[bool]) -> Result<Vec<Span>, Rejection> {
        let mut spans = Vec::new();
        let mut start = 0;
        while start < words.len() {
            if used[start] {
                start += 1;
                continue;
            }
            let end = (start..words.len())
                .find(|&i| used[i])
                .unwrap_or(words.len());
            let found = self.matches(&words[start..end]).map_err(|(a, b)| {
                reject(
                    RejectReason::Ambiguous,
                    format!(
                        "the words could be {} or {}",
                        self.vocabulary.codes[a].code, self.vocabulary.codes[b].code
                    ),
                )
            })?;
            spans.extend(found.into_iter().map(|(s, e, c)| (s + start, e + start, c)));
            start = end;
        }
        Ok(spans)
    }

    fn leftovers(
        &self,
        rest: &[&str],
        codes: &[&str],
        parameters: &mut MecpParameters,
    ) -> Result<(), Rejection> {
        let p = &self.vocabulary.parameters;
        let takes = |argument: CodeArgument| {
            self.vocabulary
                .codes
                .iter()
                .any(|code| code.argument == Some(argument) && codes.contains(&code.code.as_str()))
        };
        let (eta_code, pax_code) = (takes(CodeArgument::EtaMinutes), takes(CodeArgument::Pax));
        let mut unknown = Vec::new();
        let mut index = 0;
        while index < rest.len() {
            let words = &rest[index..];
            if let Some((latitude, longitude, used)) = self.coordinates(words) {
                if !((-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude)) {
                    return Err(reject(
                        RejectReason::InvalidParameter,
                        "coordinates must be latitude -90..90 and longitude -180..180",
                    ));
                }
                set(
                    &mut parameters.coordinates,
                    Coordinates {
                        latitude,
                        longitude,
                    },
                    "coordinates",
                )?;
                index += used;
                continue;
            }
            // "1430 zulu": a time without a keyword.
            if let [time, suffix, ..] = words
                && time.len() == 4
                && p.timestamp_suffixes.iter().any(|s| s == suffix)
                && let Some((time, _)) = Self::time(&[time])
            {
                set(&mut parameters.timestamp, time, "time")?;
                index += 2;
                continue;
            }
            if let Some((value, length)) = number(words) {
                let unit = words.get(length).copied();
                if unit.is_some_and(|u| p.pax_units.iter().any(|w| w == u)) {
                    let pax = whole(value, 1.0, 9999.0).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            "person count must be a whole number 1-9999",
                        )
                    })?;
                    set(&mut parameters.pax, pax, "person count")?;
                    index += length + 1;
                    continue;
                }
                let eta_unit = unit.and_then(|u| p.eta_units.get(u)).copied();
                if eta_code && (eta_unit.is_some() || !pax_code) {
                    let minutes = value * f64::from(eta_unit.unwrap_or(1));
                    let eta = whole(minutes, 0.0, 9999.0).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            "ETA must be a whole number of minutes, 0-9999",
                        )
                    })?;
                    set(&mut parameters.eta_minutes, eta, "ETA")?;
                    index += length + usize::from(eta_unit.is_some());
                    continue;
                }
                if eta_code && pax_code {
                    return Err(reject(
                        RejectReason::Ambiguous,
                        "a bare number could be the ETA (R03) or the person count (H07)",
                    ));
                }
                if pax_code {
                    let pax = whole(value, 1.0, 9999.0).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            "person count must be a whole number 1-9999",
                        )
                    })?;
                    set(&mut parameters.pax, pax, "person count")?;
                    index += length;
                    continue;
                }
            }
            unknown.push(words[0]);
            index += 1;
        }
        if unknown.is_empty() {
            Ok(())
        } else {
            Err(reject(
                RejectReason::InvalidParameter,
                format!("unrecognized words in MECP report: {}", unknown.join(" ")),
            ))
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one pass: severity, keyword values, events, checks, leftovers"
    )]
    pub fn parse(
        &self,
        tokens: &[Token],
        corrections: &mut Vec<Correction>,
    ) -> Result<Intent, Rejection> {
        let mut levels = Vec::new();
        let mut kept: Vec<&Token> = Vec::new();
        for token in tokens {
            match self.severity(&token.word) {
                Some(level) => {
                    if !levels.contains(&level) {
                        levels.push(level);
                    }
                }
                None => kept.push(token),
            }
        }
        let severity = match levels.as_slice() {
            [] => {
                return Err(reject(
                    RejectReason::MissingParameter,
                    "MECP severity is missing (mayday, urgent, safety or routine)",
                ));
            }
            [one] => *one,
            _ => {
                return Err(reject(
                    RejectReason::Contradictory,
                    "more than one MECP severity",
                ));
            }
        };
        let words: Vec<&str> = kept.iter().map(|t| t.word.as_str()).collect();
        let mut parameters = MecpParameters::default();
        let mut used = vec![false; words.len()];
        self.keyword_values(&kept, &words, &mut used, &mut parameters)?;
        let spans = self.events(&words, &used)?;
        for span in &spans {
            for flag in &mut used[span.0..span.1] {
                *flag = true;
            }
        }
        let mut rest: Vec<&str> = words
            .iter()
            .zip(&used)
            .filter(|(word, used)| !**used && !self.vocabulary.fillers.contains(**word))
            .map(|(word, _)| *word)
            .collect();
        self.drop_misheard_fillers(&mut rest, corrections);
        if let Some(word) = rest.iter().find(|word| self.is_negation(word)) {
            return Err(reject(
                RejectReason::Cancelled,
                format!("the report is negated or cancelled by {word:?}"),
            ));
        }
        let mut codes: Vec<&str> = Vec::new();
        for (_, _, index) in &spans {
            let code = self.vocabulary.codes[*index].code.as_str();
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
        if codes.is_empty() {
            return Err(reject(
                RejectReason::MissingParameter,
                "no MECP event recognized",
            ));
        }
        // Favor rejection over speculative correction: a word the recognizer
        // did not write may support a single event, never add one.
        if codes.len() > 1
            && let Some(token) = spans
                .iter()
                .flat_map(|span| &kept[span.0..span.1])
                .find(|token| token.word != token.original.to_lowercase())
        {
            return Err(reject(
                RejectReason::Ambiguous,
                format!(
                    "several events, one of them only after reading {:?} as {:?}",
                    token.original, token.word
                ),
            ));
        }
        for conflict in &self.vocabulary.conflicts {
            let named: Vec<&str> = codes
                .iter()
                .copied()
                .filter(|code| conflict.codes.iter().any(|c| c == code))
                .collect();
            if named.len() > 1 {
                return Err(reject(
                    RejectReason::Contradictory,
                    format!("{}: {}", named.join(" and "), conflict.reason),
                ));
            }
        }
        self.leftovers(&rest, &codes, &mut parameters)?;
        // Decoders read a bare number after R03 as its ETA.
        if codes.contains(&"R03")
            && parameters.eta_minutes.is_none()
            && parameters.text.as_deref().is_some_and(|text| {
                text.split_whitespace()
                    .any(|w| w.bytes().all(|b| b.is_ascii_digit()))
            })
        {
            return Err(reject(
                RejectReason::Ambiguous,
                "a number in the descriptive text would be read as the R03 ETA",
            ));
        }
        let codes = codes.into_iter().map(str::to_owned).collect();
        MecpReport::encode(severity, codes, parameters)
            .map(Intent::MecpReport)
            .map_err(|error| reject(RejectReason::InvalidParameter, error.0))
    }

    /// Every word the grammar knows outside event phrases.
    pub fn grammar_words(&self) -> impl Iterator<Item = String> + '_ {
        let p = &self.vocabulary.parameters;
        let lists = [
            &p.pax_units,
            &p.callsign_words,
            &p.reference_words,
            &p.timestamp_words,
            &p.timestamp_suffixes,
            &p.timestamp_trailing,
            &p.language_words,
            &p.text_words,
            &p.minus_words,
            &p.coordinate_separators,
        ];
        let phrases: Vec<String> = lists
            .into_iter()
            .flatten()
            .chain(p.eta_units.keys())
            .chain(self.vocabulary.languages.keys())
            .chain(&self.vocabulary.negations)
            .cloned()
            .collect();
        phrases.into_iter().flat_map(|phrase| phrase_words(&phrase))
    }
}
