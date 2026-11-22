//! Versioned, serializable result contract returned by every Kodiak operation.
//!
//! The JSON form is the stable contract. Rust callers use the typed
//! [`KodiakResult`]; serialization goes through a flat wire struct so the JSON
//! matches the documented shape exactly:
//!
//! ```json
//! {"schema":"kodiak.result.v1","type":"chat","content":"We need more food at the camp."}
//! {"schema":"kodiak.result.v1","type":"intent","category":"mecp","intent":"mecp.report",
//!  "data":{"severity":1,"code":"S02","content":"MECP/1/S02"},
//!  "transcript":"Kodiak. Urgent. Need Food.","classifier":"deterministic/1"}
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema identifier written into every serialized result.
pub const RESULT_SCHEMA: &str = "kodiak.result.v1";

/// The outcome of processing one transcript or one audio message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "Wire", try_from = "Wire")]
pub enum KodiakResult {
    /// No activation word: the transcript is an ordinary chat message.
    Chat {
        /// The transcript text, unchanged apart from whitespace normalization.
        content: String,
    },
    /// The activation word was present and a valid intent was extracted.
    Intent {
        /// The validated intent.
        intent: Intent,
        /// The full transcript the intent was extracted from.
        transcript: String,
        /// Identifier and version of the classifier that produced the intent.
        classifier: String,
        /// Unknown words the classifier read as vocabulary words. Empty when
        /// every word matched exactly.
        corrections: Vec<Correction>,
        /// Calibrated probability of this outcome, only from classifiers
        /// that produce one. Rule-based classifiers leave it empty.
        confidence: Option<f32>,
    },
    /// The activation word was present but no single valid intent could be
    /// extracted. Callers must not act on a rejection.
    Rejected {
        /// Why the command was rejected, with an optional intent family.
        rejection: Rejection,
        /// The full transcript.
        transcript: String,
        /// Identifier and version of the classifier that rejected it.
        classifier: String,
        /// Unknown words the classifier read as vocabulary words. Empty when
        /// every word matched exactly.
        corrections: Vec<Correction>,
        /// Calibrated probability of this outcome, only from classifiers
        /// that produce one. Rule-based classifiers leave it empty.
        confidence: Option<f32>,
    },
}

/// A transcribed word that the classifier read as a different vocabulary
/// word, for example `meet` read as `need`. Recorded so operators can audit
/// every interpretation that was not an exact match.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Correction {
    /// The word as transcribed.
    pub heard: String,
    /// The vocabulary word it was read as.
    pub read_as: String,
}

impl KodiakResult {
    /// The transcript this result was derived from.
    #[must_use]
    pub fn transcript(&self) -> &str {
        match self {
            Self::Chat { content } => content,
            Self::Intent { transcript, .. } | Self::Rejected { transcript, .. } => transcript,
        }
    }
}

/// Intent families recognized by Kodiak.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Emergency reports in the MECP code system.
    Mecp,
    /// Contact reports that the caller turns into map markers.
    Marker,
    /// Commands for the Reticulum Community Hub.
    Rch,
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mecp => "mecp",
            Self::Marker => "marker",
            Self::Rch => "rch",
        })
    }
}

/// A validated, typed intent.
#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    /// `mecp.report`
    MecpReport(MecpReport),
    /// `contact.report`
    ContactReport(ContactReport),
    /// An RCH command such as `mission.users`.
    RchCommand(RchCommand),
}

/// Intent name of [`Intent::MecpReport`].
pub const MECP_REPORT: &str = "mecp.report";
/// Intent name of [`Intent::ContactReport`].
pub const CONTACT_REPORT: &str = "contact.report";

impl Intent {
    /// The family this intent belongs to.
    #[must_use]
    pub fn category(&self) -> Category {
        match self {
            Self::MecpReport(_) => Category::Mecp,
            Self::ContactReport(_) => Category::Marker,
            Self::RchCommand(_) => Category::Rch,
        }
    }

    /// The dotted intent name, for example `mecp.report` or `mission.users`.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::MecpReport(_) => MECP_REPORT,
            Self::ContactReport(_) => CONTACT_REPORT,
            Self::RchCommand(command) => &command.command,
        }
    }
}

/// An MECP report in canonical form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MecpReport {
    /// Severity level from the MECP vocabulary (0 is most severe).
    pub severity: u8,
    /// Canonical MECP code, for example `S02`.
    pub code: String,
    /// Canonical MECP message text, for example `MECP/1/S02`.
    pub content: String,
}

/// Contact affiliation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affiliation {
    /// Friendly forces.
    Friendly,
    /// Hostile forces.
    Hostile,
    /// Neutral parties.
    Neutral,
    /// Affiliation not known.
    Unknown,
    /// Suspected hostile.
    Suspect,
}

/// Direction to a contact as spoken. Kodiak never converts a relative
/// direction to a compass bearing: it has no heading.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "direction_type", rename_all = "snake_case")]
pub enum Direction {
    /// Clock position relative to the observer's heading; 12 o'clock is 0°.
    RelativeClock {
        /// Clockwise angle from the observer's heading, in whole degrees.
        relative_bearing_deg: u16,
    },
    /// Explicit compass bearing as spoken.
    CompassBearing {
        /// Bearing in whole degrees, 0–359.
        bearing_deg: u16,
    },
}

/// A contact report. It carries no coordinates: the caller combines it with
/// the observer's own position and heading.
// `deny_unknown_fields` is incompatible with the flattened direction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContactReport {
    /// Contact affiliation.
    pub affiliation: Affiliation,
    /// Direction to the contact.
    #[serde(flatten)]
    pub direction: Direction,
    /// Distance to the contact in metres.
    pub distance_m: u32,
    /// Optional contact classification, for example `vehicle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<String>,
    /// Optional number of contacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity: Option<u32>,
    /// Optional observed activity, for example `moving`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

/// An RCH command, named by its Kodiak intent identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RchCommand {
    /// Dotted intent name, for example `mission.users`.
    pub command: String,
    /// Validated command arguments.
    pub arguments: BTreeMap<String, String>,
}

/// Machine-readable reasons for rejecting an activated command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The activation word was present but no intent was recognized.
    NoIntent,
    /// More than one intent or more than one value for a parameter matched.
    Ambiguous,
    /// A required parameter is missing.
    MissingParameter,
    /// A parameter was recognized but its value is outside the vocabulary.
    InvalidParameter,
    /// The command was recognized but the target system does not provide it.
    UnsupportedCommand,
}

/// A structured rejection of an activated command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejection {
    /// Machine-readable reason.
    pub reason: RejectReason,
    /// Human-readable explanation.
    pub detail: String,
    /// The intent family, when one was identified.
    pub category: Option<Category>,
}

impl Rejection {
    /// Creates a rejection.
    #[must_use]
    pub fn new(
        reason: RejectReason,
        category: Option<Category>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            reason,
            detail: detail.into(),
            category,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Chat,
    Intent,
    Rejected,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema: String,
    #[serde(rename = "type")]
    kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<Category>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<RejectReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    classifier: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    corrections: Vec<Correction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confidence: Option<f32>,
}

impl Wire {
    fn empty(kind: Kind) -> Self {
        Self {
            schema: RESULT_SCHEMA.into(),
            kind,
            content: None,
            category: None,
            intent: None,
            data: None,
            reason: None,
            detail: None,
            transcript: None,
            classifier: None,
            corrections: Vec::new(),
            confidence: None,
        }
    }
}

impl From<KodiakResult> for Wire {
    fn from(result: KodiakResult) -> Self {
        match result {
            KodiakResult::Chat { content } => Self {
                content: Some(content),
                ..Self::empty(Kind::Chat)
            },
            KodiakResult::Intent {
                intent,
                transcript,
                classifier,
                corrections,
                confidence,
            } => {
                let category = intent.category();
                let name = intent.name().to_owned();
                // Serializing these plain data structs cannot fail.
                let data = match intent {
                    Intent::MecpReport(report) => serde_json::to_value(report),
                    Intent::ContactReport(report) => serde_json::to_value(report),
                    Intent::RchCommand(command) => serde_json::to_value(command.arguments),
                }
                .unwrap_or(Value::Null);
                Self {
                    category: Some(category),
                    intent: Some(name),
                    data: Some(data),
                    transcript: Some(transcript),
                    classifier: Some(classifier),
                    corrections,
                    confidence,
                    ..Self::empty(Kind::Intent)
                }
            }
            KodiakResult::Rejected {
                rejection,
                transcript,
                classifier,
                corrections,
                confidence,
            } => Self {
                category: rejection.category,
                reason: Some(rejection.reason),
                detail: Some(rejection.detail),
                transcript: Some(transcript),
                classifier: Some(classifier),
                corrections,
                confidence,
                ..Self::empty(Kind::Rejected)
            },
        }
    }
}

/// Error for a JSON document that is not a valid `kodiak.result.v1`.
#[derive(Debug)]
pub struct ContractError(String);

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid Kodiak result: {}", self.0)
    }
}

impl std::error::Error for ContractError {}

fn require<T>(value: Option<T>, field: &str) -> Result<T, ContractError> {
    value.ok_or_else(|| ContractError(format!("missing {field}")))
}

impl TryFrom<Wire> for KodiakResult {
    type Error = ContractError;

    fn try_from(wire: Wire) -> Result<Self, ContractError> {
        if wire.schema != RESULT_SCHEMA {
            return Err(ContractError(format!(
                "unsupported schema {:?}",
                wire.schema
            )));
        }
        let invalid = |error: serde_json::Error| ContractError(error.to_string());
        Ok(match wire.kind {
            Kind::Chat => Self::Chat {
                content: require(wire.content, "content")?,
            },
            Kind::Intent => {
                let category = require(wire.category, "category")?;
                let name = require(wire.intent, "intent")?;
                let data = require(wire.data, "data")?;
                let intent = match name.as_str() {
                    MECP_REPORT => {
                        Intent::MecpReport(serde_json::from_value(data).map_err(invalid)?)
                    }
                    CONTACT_REPORT => {
                        Intent::ContactReport(serde_json::from_value(data).map_err(invalid)?)
                    }
                    _ => Intent::RchCommand(RchCommand {
                        command: name,
                        arguments: serde_json::from_value(data).map_err(invalid)?,
                    }),
                };
                if intent.category() != category {
                    return Err(ContractError(format!(
                        "intent {} does not belong to category {category}",
                        intent.name()
                    )));
                }
                Self::Intent {
                    intent,
                    transcript: require(wire.transcript, "transcript")?,
                    classifier: require(wire.classifier, "classifier")?,
                    corrections: wire.corrections,
                    confidence: wire.confidence,
                }
            }
            Kind::Rejected => Self::Rejected {
                rejection: Rejection {
                    reason: require(wire.reason, "reason")?,
                    detail: require(wire.detail, "detail")?,
                    category: wire.category,
                },
                transcript: require(wire.transcript, "transcript")?,
                classifier: require(wire.classifier, "classifier")?,
                corrections: wire.corrections,
                confidence: wire.confidence,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn documented_json_shapes_round_trip() {
        let chat = KodiakResult::Chat {
            content: "We need more food at the camp.".into(),
        };
        assert_eq!(
            serde_json::to_value(&chat).unwrap(),
            json!({"schema": RESULT_SCHEMA, "type": "chat", "content": "We need more food at the camp."})
        );
        let contact = KodiakResult::Intent {
            intent: Intent::ContactReport(ContactReport {
                affiliation: Affiliation::Hostile,
                direction: Direction::RelativeClock {
                    relative_bearing_deg: 105,
                },
                distance_m: 200,
                classification: None,
                quantity: None,
                activity: None,
            }),
            transcript: "Kodiak hostile three thirty two hundred meters.".into(),
            classifier: "test/1".into(),
            corrections: Vec::new(),
            confidence: Some(0.5),
        };
        let value = serde_json::to_value(&contact).unwrap();
        assert_eq!(value["category"], "marker");
        assert_eq!(value["intent"], "contact.report");
        assert_eq!(
            value["data"],
            json!({"affiliation": "hostile", "direction_type": "relative_clock", "relative_bearing_deg": 105, "distance_m": 200})
        );
        let users = KodiakResult::Intent {
            intent: Intent::RchCommand(RchCommand {
                command: "mission.users".into(),
                arguments: BTreeMap::new(),
            }),
            transcript: "Kodiak lisst users.".into(),
            classifier: "test/1".into(),
            corrections: vec![Correction {
                heard: "lisst".into(),
                read_as: "list".into(),
            }],
            confidence: None,
        };
        assert_eq!(serde_json::to_value(&users).unwrap()["data"], json!({}));
        assert_eq!(
            serde_json::to_value(&users).unwrap()["corrections"],
            json!([{"heard": "lisst", "read_as": "list"}])
        );
        assert!(
            serde_json::to_value(&contact)
                .unwrap()
                .get("corrections")
                .is_none()
        );
        assert_eq!(serde_json::to_value(&contact).unwrap()["confidence"], 0.5);
        assert!(
            serde_json::to_value(&users)
                .unwrap()
                .get("confidence")
                .is_none()
        );
        for result in [chat, contact, users] {
            let text = serde_json::to_string(&result).unwrap();
            assert_eq!(serde_json::from_str::<KodiakResult>(&text).unwrap(), result);
        }
    }

    #[test]
    fn mismatched_category_and_unknown_schema_are_rejected() {
        let wrong = json!({"schema": RESULT_SCHEMA, "type": "intent", "category": "rch", "intent": "mecp.report",
            "data": {"severity": 1, "code": "S02", "content": "MECP/1/S02"}, "transcript": "x", "classifier": "c"});
        assert!(serde_json::from_value::<KodiakResult>(wrong).is_err());
        let future = json!({"schema": "kodiak.result.v9", "type": "chat", "content": "x"});
        assert!(serde_json::from_value::<KodiakResult>(future).is_err());
    }
}
