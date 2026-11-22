//! Canonical MECP wire encoding.
//!
//! Kodiak follows the MECP token grammar implemented by the two pinned
//! reference codecs:
//!
//! - REM `apps/mobile/src/utils/mecp.ts` (`encodeMecpMessage`, commit 3e1e172)
//! - RCH `crates/r3akt-profile-rch/src/lib.rs` (`decode_mecp_message`,
//!   commit c2bf78a)
//!
//! A message is `MECP/<severity>/<codes> <extras> <details>`: one or more
//! space-separated event codes in the order reported, then the structured
//! extras in this fixed order — `<n>pax`, `<lat>,<lon>`, `#<reference>`…,
//! `<eta minutes>`, `@<language>`, `@<hhmm>`, `~<callsign>` — then free
//! descriptive text. The whole message is at most [`MAX_MESSAGE_BYTES`] bytes
//! of UTF-8.

use std::fmt::Write as _;

use crate::result::{Coordinates, MecpParameters, MecpReport};

/// Largest encoded MECP message, in bytes of UTF-8.
pub const MAX_MESSAGE_BYTES: usize = 200;

/// Why a report cannot be encoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid MECP report: {}", self.0)
    }
}

impl std::error::Error for EncodeError {}

fn fail<T>(message: impl Into<String>) -> Result<T, EncodeError> {
    Err(EncodeError(message.into()))
}

/// An event code: an upper-case letter and two digits.
#[must_use]
pub fn is_event_code(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 3 && bytes[0].is_ascii_uppercase() && bytes[1..].iter().all(u8::is_ascii_digit)
}

/// A callsign or reference body: `[A-Za-z0-9][A-Za-z0-9_-]*` (references
/// also allow `.`).
fn is_identifier(value: &str, allow_dot: bool) -> bool {
    let mut characters = value.chars();
    characters.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && characters
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || (allow_dot && c == '.'))
}

fn coordinate(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        // At most six decimals, without trailing zeros, as REM writes them.
        let text = format!("{value:.6}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

impl MecpParameters {
    /// Whether no parameter was given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self) -> Result<(), EncodeError> {
        if let Some(pax) = self.pax
            && !(1..=9999).contains(&pax)
        {
            return fail("person count must be 1-9999");
        }
        if let Some(Coordinates {
            latitude,
            longitude,
        }) = self.coordinates
            && !(latitude.is_finite()
                && longitude.is_finite()
                && (-90.0..=90.0).contains(&latitude)
                && (-180.0..=180.0).contains(&longitude))
        {
            return fail("coordinates must be latitude -90..90 and longitude -180..180");
        }
        if self.references.iter().any(|r| !is_identifier(r, true)) {
            return fail("references must be letters, digits, '_', '-' or '.'");
        }
        if self.eta_minutes.is_some_and(|eta| eta > 9999) {
            return fail("ETA must be 0-9999 minutes");
        }
        if let Some(language) = &self.language
            && !((2..=3).contains(&language.len())
                && language.bytes().all(|b| b.is_ascii_lowercase()))
        {
            return fail("language must be a two or three letter code");
        }
        if let Some(timestamp) = &self.timestamp {
            let valid = timestamp.len() == 4
                && timestamp.bytes().all(|b| b.is_ascii_digit())
                && timestamp[..2] < *"24"
                && timestamp[2..] < *"60";
            if !valid {
                return fail("timestamp must be HHMM, 0000-2359");
            }
        }
        if let Some(callsign) = &self.callsign
            && !is_identifier(callsign, false)
        {
            return fail("callsign must be letters, digits, '_' or '-'");
        }
        if let Some(text) = &self.text {
            let first = text.split_whitespace().next().unwrap_or_default();
            if text.trim().is_empty() {
                return fail("descriptive text is empty");
            }
            // Decoders read leading code-shaped words as event codes.
            if self.is_empty_but_text() && is_event_code(&first.to_ascii_uppercase()) {
                return fail("descriptive text must not start with an event code");
            }
        }
        Ok(())
    }

    fn is_empty_but_text(&self) -> bool {
        Self {
            text: None,
            ..self.clone()
        }
        .is_empty()
    }
}

impl MecpReport {
    /// Builds a report and its canonical wire text.
    ///
    /// # Errors
    ///
    /// Fails on a severity outside 0-3, no codes, a malformed or repeated
    /// code, an invalid parameter, or a message over
    /// [`MAX_MESSAGE_BYTES`].
    pub fn encode(
        severity: u8,
        codes: Vec<String>,
        parameters: MecpParameters,
    ) -> Result<Self, EncodeError> {
        if severity > 3 {
            return fail("severity must be 0-3");
        }
        if codes.is_empty() {
            return fail("at least one event code is required");
        }
        for (index, code) in codes.iter().enumerate() {
            if !is_event_code(code) {
                return fail(format!("{code:?} is not an event code"));
            }
            if codes[..index].contains(code) {
                return fail(format!("{code} is repeated"));
            }
        }
        parameters.validate()?;
        let mut content = format!("MECP/{severity}/{}", codes.join(" "));
        let p = &parameters;
        if let Some(pax) = p.pax {
            let _ = write!(content, " {pax}pax");
        }
        if let Some(Coordinates {
            latitude,
            longitude,
        }) = p.coordinates
        {
            let _ = write!(
                content,
                " {},{}",
                coordinate(latitude),
                coordinate(longitude)
            );
        }
        for reference in &p.references {
            let _ = write!(content, " #{reference}");
        }
        if let Some(eta) = p.eta_minutes {
            let _ = write!(content, " {eta}");
        }
        if let Some(language) = &p.language {
            let _ = write!(content, " @{language}");
        }
        if let Some(timestamp) = &p.timestamp {
            let _ = write!(content, " @{timestamp}");
        }
        if let Some(callsign) = &p.callsign {
            let _ = write!(content, " ~{callsign}");
        }
        if let Some(text) = &p.text {
            let _ = write!(
                content,
                " {}",
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
        if content.len() > MAX_MESSAGE_BYTES {
            return fail(format!(
                "message is {} bytes; MECP allows {MAX_MESSAGE_BYTES}",
                content.len()
            ));
        }
        Ok(Self {
            severity,
            code: codes[0].clone(),
            codes,
            content,
            parameters,
        })
    }

    /// A report with one event code and no parameters.
    ///
    /// # Errors
    ///
    /// As [`MecpReport::encode`].
    pub fn single(severity: u8, code: impl Into<String>) -> Result<Self, EncodeError> {
        Self::encode(severity, vec![code.into()], MecpParameters::default())
    }

    /// Whether the operator spoke coordinates.
    #[must_use]
    pub fn has_spoken_coordinates(&self) -> bool {
        self.parameters.coordinates.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(list: &[&str]) -> Vec<String> {
        list.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn matches_the_reference_codecs() {
        // The RCH decoder's own fixture, re-encoded in canonical order.
        let report = MecpReport::encode(
            1,
            codes(&["R03", "T99"]),
            MecpParameters {
                pax: Some(4),
                coordinates: Some(Coordinates {
                    latitude: 45.5017,
                    longitude: -73.5673,
                }),
                references: vec!["A1".into()],
                eta_minutes: Some(15),
                language: Some("en".into()),
                timestamp: Some("0930".into()),
                callsign: Some("EAGLE-1".into()),
                text: Some("north gate".into()),
            },
        )
        .unwrap();
        assert_eq!(
            report.content,
            "MECP/1/R03 T99 4pax 45.5017,-73.5673 #A1 15 @en @0930 ~EAGLE-1 north gate"
        );
        assert_eq!(report.code, "R03");
        assert_eq!(
            MecpReport::encode(0, codes(&["M01", "M07", "S01"]), MecpParameters::default())
                .unwrap()
                .content,
            "MECP/0/M01 M07 S01"
        );
    }

    #[test]
    fn invalid_reports_are_refused() {
        let none = MecpParameters::default;
        assert!(MecpReport::encode(4, codes(&["S01"]), none()).is_err());
        assert!(MecpReport::encode(0, Vec::new(), none()).is_err());
        assert!(MecpReport::encode(0, codes(&["S01", "S01"]), none()).is_err());
        assert!(MecpReport::encode(0, codes(&["s01"]), none()).is_err());
        let timestamp = |t: &str| MecpParameters {
            timestamp: Some(t.into()),
            ..none()
        };
        assert!(MecpReport::encode(0, codes(&["S01"]), timestamp("2400")).is_err());
        assert!(MecpReport::encode(0, codes(&["S01"]), timestamp("2359")).is_ok());
        let long = MecpParameters {
            text: Some("word ".repeat(40)),
            ..none()
        };
        assert!(MecpReport::encode(0, codes(&["S01"]), long).is_err());
        let code_text = MecpParameters {
            text: Some("b12 behind the barn".into()),
            ..none()
        };
        assert!(MecpReport::encode(0, codes(&["S01"]), code_text).is_err());
    }
}
