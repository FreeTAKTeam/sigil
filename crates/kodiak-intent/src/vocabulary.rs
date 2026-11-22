//! Versioned vocabularies, embedded from the package's `vocabularies/` folder.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use crate::text::tokenize;

/// The MECP vocabulary source, version 1.
pub const MECP_V1: &str = include_str!("../vocabularies/mecp/v1.toml");
/// The contact vocabulary source, version 1.
pub const CONTACT_V1: &str = include_str!("../vocabularies/contact/v1.toml");
/// The RCH command vocabulary source, version 1.
pub const RCH_V1: &str = include_str!("../vocabularies/rch/v1.toml");

/// A vocabulary that failed to load or validate.
#[derive(Debug)]
pub struct VocabularyError(pub String);

impl std::fmt::Display for VocabularyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid Kodiak vocabulary: {}", self.0)
    }
}

impl std::error::Error for VocabularyError {}

fn error(message: impl Into<String>) -> VocabularyError {
    VocabularyError(message.into())
}

/// Lower-case word sequence of a phrase.
#[must_use]
pub fn phrase_words(phrase: &str) -> Vec<String> {
    tokenize(phrase)
        .into_iter()
        .map(|token| token.word)
        .collect()
}

fn check_header(
    schema: &str,
    version: u32,
    revision: u32,
    expected: &str,
) -> Result<(), VocabularyError> {
    if schema != expected || version != 1 || revision == 0 {
        return Err(error(format!(
            "expected {expected} v1, found {schema} v{version} revision {revision}"
        )));
    }
    Ok(())
}

fn first_revision() -> u32 {
    1
}

/// Fails when a list names the same entry twice.
fn unique<'a>(
    what: &str,
    items: impl IntoIterator<Item = &'a String>,
) -> Result<(), VocabularyError> {
    let mut seen = BTreeSet::new();
    for item in items {
        if !seen.insert(phrase_words(item)) {
            return Err(error(format!("{what} {item:?} is repeated")));
        }
    }
    Ok(())
}

/// Fails when an entry is not a lower-case word sequence.
fn lower_case<'a>(
    what: &str,
    items: impl IntoIterator<Item = &'a String>,
) -> Result<(), VocabularyError> {
    for item in items {
        let words = phrase_words(item);
        if words.is_empty() || words.join(" ") != *item {
            return Err(error(format!(
                "{what} {item:?} must be lower-case words separated by single spaces"
            )));
        }
    }
    Ok(())
}

/// MECP vocabulary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mecp {
    schema: String,
    /// Vocabulary schema version.
    pub version: u32,
    /// Content revision within the schema version.
    #[serde(default = "first_revision")]
    pub revision: u32,
    /// Words ignored around a report.
    pub fillers: BTreeSet<String>,
    /// Words that cancel a report when no event phrase uses them.
    #[serde(default)]
    pub negations: BTreeSet<String>,
    /// Structured value grammar.
    #[serde(default)]
    pub parameters: Parameters,
    /// Spoken language name to ISO 639 code.
    #[serde(default)]
    pub languages: BTreeMap<String, String>,
    /// Severity levels.
    pub severities: Vec<Severity>,
    /// Event codes.
    pub codes: Vec<Code>,
    /// Groups of codes that exclude each other.
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
}

/// Spoken grammar of the MECP structured values.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    /// Units after a number that make it a person count.
    #[serde(default)]
    pub pax_units: Vec<String>,
    /// Units of an ETA, in minutes.
    #[serde(default)]
    pub eta_units: BTreeMap<String, u16>,
    /// Keywords before a callsign.
    #[serde(default)]
    pub callsign_words: Vec<String>,
    /// Keywords before a reference.
    #[serde(default)]
    pub reference_words: Vec<String>,
    /// Keywords before a time.
    #[serde(default)]
    pub timestamp_words: Vec<String>,
    /// Words after a four-digit time that mark it as a time on their own.
    #[serde(default)]
    pub timestamp_suffixes: Vec<String>,
    /// Words that may follow a keyworded time.
    #[serde(default)]
    pub timestamp_trailing: Vec<String>,
    /// Keywords before a language name.
    #[serde(default)]
    pub language_words: Vec<String>,
    /// Keywords before free descriptive text.
    #[serde(default)]
    pub text_words: Vec<String>,
    /// Words that make the next coordinate negative.
    #[serde(default)]
    pub minus_words: Vec<String>,
    /// Words allowed between latitude and longitude.
    #[serde(default)]
    pub coordinate_separators: Vec<String>,
    /// Most words a keyword value may have.
    #[serde(default = "default_value_words")]
    pub max_value_words: usize,
    /// Words a callsign or reference may contain besides numbers (the
    /// phonetic alphabet). Empty allows any word.
    #[serde(default)]
    pub identifier_words: BTreeSet<String>,
}

fn default_value_words() -> usize {
    3
}

impl Parameters {
    /// Every keyword phrase, with what it introduces.
    #[must_use]
    pub fn keywords(&self) -> Vec<(&String, Keyword)> {
        let mut keywords = Vec::new();
        for (list, kind) in [
            (&self.callsign_words, Keyword::Callsign),
            (&self.reference_words, Keyword::Reference),
            (&self.timestamp_words, Keyword::Timestamp),
            (&self.language_words, Keyword::Language),
            (&self.text_words, Keyword::Text),
        ] {
            keywords.extend(list.iter().map(|phrase| (phrase, kind)));
        }
        keywords
    }
}

/// What a parameter keyword introduces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    /// `~callsign`
    Callsign,
    /// `#reference`
    Reference,
    /// `@hhmm`
    Timestamp,
    /// `@language`
    Language,
    /// Free text.
    Text,
}

/// Codes that cannot be reported together.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    /// At least two event codes.
    pub codes: Vec<String>,
    /// Why they exclude each other.
    pub reason: String,
}

/// One MECP severity level.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Severity {
    /// Numeric level used in `MECP/<level>/...`.
    pub level: u8,
    /// Canonical label.
    pub label: String,
    /// Spoken words for this level.
    pub words: Vec<String>,
}

/// Optional spoken argument of an MECP code.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodeArgument {
    /// Minutes, encoded as a bare number (R03).
    EtaMinutes,
    /// Number of people, encoded as `<n>pax` (H07).
    Pax,
}

/// One MECP event code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Code {
    /// Canonical code, e.g. `S02`.
    pub code: String,
    /// Canonical label.
    pub label: String,
    /// Spoken phrases.
    pub phrases: Vec<String>,
    /// Optional argument.
    #[serde(default)]
    pub argument: Option<CodeArgument>,
}

impl Mecp {
    /// Loads and validates an MECP vocabulary.
    ///
    /// # Errors
    ///
    /// Fails on malformed TOML, an unexpected schema, invalid codes or levels,
    /// or a phrase claimed by two codes.
    pub fn parse(source: &str) -> Result<Self, VocabularyError> {
        let vocabulary: Self = toml::from_str(source).map_err(|e| error(e.to_string()))?;
        check_header(
            &vocabulary.schema,
            vocabulary.version,
            vocabulary.revision,
            "kodiak.vocabulary.mecp",
        )?;
        vocabulary.validate_grammar()?;
        let mut words = BTreeSet::new();
        for severity in &vocabulary.severities {
            if severity.level > 3 {
                return Err(error(format!(
                    "MECP severity {} is outside 0-3",
                    severity.level
                )));
            }
            if severity
                .words
                .first()
                .is_none_or(|word| phrase_words(word).len() != 1)
            {
                return Err(error(format!(
                    "severity {} must start with a single word",
                    severity.level
                )));
            }
            for word in &severity.words {
                if !words.insert(word.clone()) {
                    return Err(error(format!("severity word {word:?} is repeated")));
                }
            }
        }
        let mut phrases = BTreeMap::new();
        for code in &vocabulary.codes {
            let bytes = code.code.as_bytes();
            if bytes.len() != 3
                || !bytes[0].is_ascii_uppercase()
                || !bytes[1..].iter().all(u8::is_ascii_digit)
            {
                return Err(error(format!(
                    "MECP code {:?} is not a letter and two digits",
                    code.code
                )));
            }
            for phrase in &code.phrases {
                let key = phrase_words(phrase);
                if key.iter().any(|word| words.contains(word)) {
                    return Err(error(format!("phrase {phrase:?} contains a severity word")));
                }
                if let [word] = key.as_slice()
                    && vocabulary.fillers.contains(word)
                {
                    return Err(error(format!("phrase {phrase:?} is also a filler")));
                }
                if let Some(other) = phrases.insert(key, code.code.clone()) {
                    return Err(error(format!(
                        "phrase {phrase:?} belongs to {other} and {}",
                        code.code
                    )));
                }
            }
        }
        for (keyword, _) in vocabulary.parameters.keywords() {
            if let Some(code) = phrases.get(&phrase_words(keyword)) {
                return Err(error(format!(
                    "parameter keyword {keyword:?} is also a phrase of {code}"
                )));
            }
        }
        for name in vocabulary.languages.keys() {
            if let Some(code) = phrases.get(&phrase_words(name)) {
                return Err(error(format!(
                    "language {name:?} is also a phrase of {code}"
                )));
            }
        }
        let known: BTreeSet<&str> = vocabulary.codes.iter().map(|c| c.code.as_str()).collect();
        for conflict in &vocabulary.conflicts {
            if conflict.codes.len() < 2 {
                return Err(error("a conflict needs at least two codes"));
            }
            for code in &conflict.codes {
                if !known.contains(code.as_str()) {
                    return Err(error(format!("conflict names unknown code {code:?}")));
                }
            }
            unique("conflict code", &conflict.codes)?;
        }
        Ok(vocabulary)
    }

    /// Checks the word lists that do not involve event phrases.
    fn validate_grammar(&self) -> Result<(), VocabularyError> {
        let p = &self.parameters;
        let lists: [(&str, &Vec<String>); 10] = [
            ("pax unit", &p.pax_units),
            ("callsign keyword", &p.callsign_words),
            ("reference keyword", &p.reference_words),
            ("time keyword", &p.timestamp_words),
            ("time suffix", &p.timestamp_suffixes),
            ("time trailing word", &p.timestamp_trailing),
            ("language keyword", &p.language_words),
            ("text keyword", &p.text_words),
            ("minus word", &p.minus_words),
            ("coordinate separator", &p.coordinate_separators),
        ];
        for (what, list) in lists {
            lower_case(what, list)?;
            unique(what, list)?;
        }
        lower_case("identifier word", &p.identifier_words)?;
        lower_case("filler", &self.fillers)?;
        lower_case("negation", &self.negations)?;
        lower_case("language", self.languages.keys())?;
        lower_case("ETA unit", p.eta_units.keys())?;
        unique(
            "parameter keyword",
            p.keywords().into_iter().map(|(phrase, _)| phrase),
        )?;
        // A word may have only one role outside event phrases.
        let single = |list: &Vec<String>| -> Vec<String> {
            list.iter()
                .filter(|phrase| phrase_words(phrase).len() == 1)
                .cloned()
                .collect()
        };
        let groups: Vec<(&str, Vec<String>)> = vec![
            ("filler", self.fillers.iter().cloned().collect()),
            ("negation", self.negations.iter().cloned().collect()),
            ("pax unit", p.pax_units.clone()),
            ("ETA unit", p.eta_units.keys().cloned().collect()),
            ("keyword", single(&p.callsign_words)),
            ("keyword", single(&p.reference_words)),
            ("keyword", single(&p.timestamp_words)),
            ("keyword", single(&p.language_words)),
            ("keyword", single(&p.text_words)),
            ("minus word", p.minus_words.clone()),
            ("coordinate separator", p.coordinate_separators.clone()),
            ("language", self.languages.keys().cloned().collect()),
        ];
        let mut roles: BTreeMap<String, &str> = BTreeMap::new();
        for (role, words) in groups {
            for word in words {
                if let Some(other) = roles.insert(word.clone(), role)
                    && other != role
                {
                    return Err(error(format!("{word:?} is both a {other} and a {role}")));
                }
            }
        }
        if p.eta_units.values().any(|minutes| *minutes == 0) {
            return Err(error("an ETA unit must be at least one minute"));
        }
        if self.languages.values().any(|code| {
            !(2..=3).contains(&code.len()) || !code.bytes().all(|b| b.is_ascii_lowercase())
        }) {
            return Err(error(
                "language codes must be two or three lower-case letters",
            ));
        }
        if p.max_value_words == 0 {
            return Err(error("max_value_words must be at least 1"));
        }
        Ok(())
    }

    /// `version.revision`, as recorded in results.
    #[must_use]
    pub fn release(&self) -> String {
        format!("{}.{}", self.version, self.revision)
    }
}

/// Contact report vocabulary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    schema: String,
    /// Vocabulary schema version.
    pub version: u32,
    /// Content revision within the schema version.
    #[serde(default = "first_revision")]
    pub revision: u32,
    /// Words ignored in a report.
    pub fillers: BTreeSet<String>,
    /// Words that cancel a report.
    #[serde(default)]
    pub negations: BTreeSet<String>,
    /// Words after a number that make it a compass bearing in degrees.
    #[serde(default)]
    pub degree_words: BTreeSet<String>,
    /// Fraction word to multiplier, for "half a kilometer".
    #[serde(default)]
    pub fractions: BTreeMap<String, f64>,
    /// Largest accepted distance in metres.
    pub max_distance_m: u32,
    /// Affiliation name to spoken words.
    pub affiliations: BTreeMap<String, Vec<String>>,
    /// Spoken word to canonical classification.
    pub classifications: BTreeMap<String, String>,
    /// Spoken word to canonical activity.
    pub activities: BTreeMap<String, String>,
    /// Unit word to metres.
    pub units: BTreeMap<String, f64>,
    /// Words that introduce a compass bearing.
    pub bearing_words: BTreeSet<String>,
    /// Compass point to bearing in degrees.
    pub compass_points: BTreeMap<String, u16>,
}

impl Contact {
    /// Loads and validates a contact vocabulary.
    ///
    /// # Errors
    ///
    /// Fails on malformed TOML, an unexpected schema, an unknown affiliation,
    /// or a word with two meanings.
    pub fn parse(source: &str) -> Result<Self, VocabularyError> {
        let vocabulary: Self = toml::from_str(source).map_err(|e| error(e.to_string()))?;
        check_header(
            &vocabulary.schema,
            vocabulary.version,
            vocabulary.revision,
            "kodiak.vocabulary.contact",
        )?;
        let mut seen = BTreeSet::new();
        for (name, words) in &vocabulary.affiliations {
            if !matches!(
                name.as_str(),
                "friendly" | "hostile" | "neutral" | "unknown" | "suspect"
            ) {
                return Err(error(format!("unknown affiliation {name:?}")));
            }
            seen.extend(words.iter().cloned());
        }
        let all = vocabulary
            .classifications
            .keys()
            .chain(vocabulary.activities.keys())
            .chain(vocabulary.units.keys())
            .chain(&vocabulary.bearing_words)
            .chain(vocabulary.compass_points.keys())
            .chain(&vocabulary.fillers)
            .chain(&vocabulary.negations)
            .chain(&vocabulary.degree_words)
            .chain(vocabulary.fractions.keys());
        for word in all {
            if !seen.insert(word.clone()) {
                return Err(error(format!("contact word {word:?} has two meanings")));
            }
        }
        if vocabulary
            .compass_points
            .values()
            .any(|bearing| *bearing >= 360)
        {
            return Err(error("compass point outside 0-359"));
        }
        if vocabulary
            .fractions
            .values()
            .chain(vocabulary.units.values())
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return Err(error("units and fractions must be positive"));
        }
        if vocabulary.max_distance_m == 0 {
            return Err(error("max_distance_m must be positive"));
        }
        Ok(vocabulary)
    }

    /// `version.revision`, as recorded in results.
    #[must_use]
    pub fn release(&self) -> String {
        format!("{}.{}", self.version, self.revision)
    }
}

/// RCH command vocabulary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rch {
    schema: String,
    /// Vocabulary schema version.
    pub version: u32,
    /// Content revision within the schema version.
    #[serde(default = "first_revision")]
    pub revision: u32,
    /// Words removed before matching.
    pub fillers: BTreeSet<String>,
    /// Supported commands.
    pub commands: Vec<Command>,
    /// Recognized but unsupported requests.
    #[serde(default)]
    pub unsupported: Vec<Unsupported>,
}

/// One RCH command.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// Canonical RCH command name, used as the Kodiak intent name.
    pub intent: String,
    /// Spoken phrases.
    pub phrases: Vec<String>,
    /// Name of a free-text argument made of the words after the phrase.
    #[serde(default)]
    pub argument: Option<String>,
    /// Words dropped from the start of the argument, e.g. "for".
    #[serde(default)]
    pub argument_lead: Vec<String>,
}

/// A recognized request that RCH does not support.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unsupported {
    /// Human-readable name.
    pub name: String,
    /// Spoken phrases.
    pub phrases: Vec<String>,
    /// Whether words may follow the phrase.
    #[serde(default)]
    pub prefix: bool,
    /// Why it is unsupported.
    pub reason: String,
}

impl Rch {
    /// Loads and validates an RCH command vocabulary.
    ///
    /// # Errors
    ///
    /// Fails on malformed TOML, an unexpected schema, an intent name that the
    /// result contract reserves, or a phrase claimed twice.
    pub fn parse(source: &str) -> Result<Self, VocabularyError> {
        let vocabulary: Self = toml::from_str(source).map_err(|e| error(e.to_string()))?;
        check_header(
            &vocabulary.schema,
            vocabulary.version,
            vocabulary.revision,
            "kodiak.vocabulary.rch",
        )?;
        let mut phrases = BTreeSet::new();
        for command in &vocabulary.commands {
            if matches!(
                command.intent.as_str(),
                kodiak_core::MECP_REPORT | kodiak_core::CONTACT_REPORT
            ) || command.intent.is_empty()
                || !command
                    .intent
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_')
            {
                return Err(error(format!(
                    "invalid RCH intent name {:?}",
                    command.intent
                )));
            }
        }
        let all = vocabulary
            .commands
            .iter()
            .flat_map(|command| &command.phrases)
            .chain(
                vocabulary
                    .unsupported
                    .iter()
                    .flat_map(|entry| &entry.phrases),
            );
        for phrase in all {
            if !phrases.insert(phrase_words(phrase)) {
                return Err(error(format!("RCH phrase {phrase:?} is repeated")));
            }
        }
        Ok(vocabulary)
    }

    /// `version.revision`, as recorded in results.
    #[must_use]
    pub fn release(&self) -> String {
        format!("{}.{}", self.version, self.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_vocabularies_are_valid() {
        let mecp = Mecp::parse(MECP_V1).unwrap();
        // 12 categories, 108 codes: the RCH table (B04 is REM-only).
        assert_eq!(mecp.codes.len(), 108);
        assert_eq!(mecp.severities.len(), 4);
        assert_eq!(mecp.release(), "1.7");
        assert_eq!(Contact::parse(CONTACT_V1).unwrap().release(), "1.3");
        assert_eq!(Rch::parse(RCH_V1).unwrap().release(), "1.1");
    }

    fn mecp_with(from: &str, to: &str) -> Result<Mecp, VocabularyError> {
        assert!(MECP_V1.contains(from), "{from}");
        Mecp::parse(&MECP_V1.replacen(from, to, 1))
    }

    #[test]
    fn inconsistent_vocabularies_are_refused() {
        // Unknown field: schema validation.
        assert!(mecp_with("revision = 7", "revision = 6\nsurprise = 1").is_err());
        assert!(mecp_with("revision = 7", "revision = 0").is_err());
        // A phrase claimed by two codes.
        assert!(mecp_with(r#""injury", "#, r#""injury", "fuel", "#).is_err());
        // The same alias twice in one list.
        assert!(mecp_with("pax_units = [", r#"pax_units = ["pax", "#).is_err());
        // A word with two roles.
        assert!(mecp_with("minus_words = [", r#"minus_words = ["no", "#).is_err());
        // A keyword that is also an event phrase.
        assert!(mecp_with("text_words = [", r#"text_words = ["coffee", "#).is_err());
        // A conflict with an unknown code.
        assert!(mecp_with(r#"["S01", "H01"]"#, r#"["S01", "Z99"]"#).is_err());
        // An alias that is not lower case.
        assert!(mecp_with("english = ", "English = ").is_err());
        // Contact: a word with two meanings, and a zero fraction.
        assert!(Contact::parse(&CONTACT_V1.replace("half = 0.5", "north = 0.5")).is_err());
        assert!(Contact::parse(&CONTACT_V1.replace("half = 0.5", "half = 0.0")).is_err());
    }
}
