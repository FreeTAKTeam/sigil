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

fn check_header(schema: &str, version: u32, expected: &str) -> Result<(), VocabularyError> {
    if schema != expected || version != 1 {
        return Err(error(format!(
            "expected {expected} v1, found {schema} v{version}"
        )));
    }
    Ok(())
}

/// MECP vocabulary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mecp {
    schema: String,
    /// Vocabulary version.
    pub version: u32,
    /// Words ignored around a report.
    pub fillers: BTreeSet<String>,
    /// Severity levels.
    pub severities: Vec<Severity>,
    /// Event codes.
    pub codes: Vec<Code>,
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
            "kodiak.vocabulary.mecp",
        )?;
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
                if let Some(other) = phrases.insert(key, code.code.clone()) {
                    return Err(error(format!(
                        "phrase {phrase:?} belongs to {other} and {}",
                        code.code
                    )));
                }
            }
        }
        Ok(vocabulary)
    }
}

/// Contact report vocabulary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contact {
    schema: String,
    /// Vocabulary version.
    pub version: u32,
    /// Words ignored in a report.
    pub fillers: BTreeSet<String>,
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
            .chain(&vocabulary.fillers);
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
        Ok(vocabulary)
    }
}

/// RCH command vocabulary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rch {
    schema: String,
    /// Vocabulary version.
    pub version: u32,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_vocabularies_are_valid() {
        let mecp = Mecp::parse(MECP_V1).unwrap();
        // 12 categories, 108 codes: the RCH table (B04 is REM-only).
        assert_eq!(mecp.codes.len(), 108);
        Contact::parse(CONTACT_V1).unwrap();
        Rch::parse(RCH_V1).unwrap();
    }
}
