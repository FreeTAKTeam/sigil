//! Contact report extraction: Description, Distance, Direction (DDD).
//!
//! The words may come in any order. A report needs an affiliation, one
//! distance and one direction; classification, quantity and activity are
//! optional. Two different values for one of them make the report
//! contradictory, and any negation word cancels it. Clock positions are
//! relative to the operator's heading; compass points and bearings are
//! absolute. Kodiak never turns one into the other.

use kodiak_core::{
    Affiliation, Category, ContactReport, Direction, Intent, RejectReason, Rejection,
};

use crate::text::{Token, number, single_digit, starts_number};
use crate::vocabulary::Contact;

pub struct Parser {
    vocabulary: Contact,
}

fn reject(reason: RejectReason, detail: impl Into<String>) -> Rejection {
    Rejection::new(reason, Some(Category::Marker), detail)
}

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

/// A clock position: hour 1-12 and minutes 0 or 30, plus words used.
fn clock(words: &[&str]) -> Option<(u16, u16, usize)> {
    let first = *words.first()?;
    if let Some((hour, minutes)) = first.split_once(':') {
        let hour: u16 = hour.parse().ok()?;
        let minutes: u16 = minutes.parse().ok()?;
        return Some((hour, minutes, 1));
    }
    // "330" or "1030" as written by recognizers: whole or half hours only.
    if (3..=4).contains(&first.len()) && first.bytes().all(|b| b.is_ascii_digit()) {
        let value: u16 = first.parse().ok()?;
        let (hour, minutes) = (value / 100, value % 100);
        return ((1..=12).contains(&hour) && matches!(minutes, 0 | 30))
            .then_some((hour, minutes, 1));
    }
    if first == "half" && words.get(1) == Some(&"past") {
        let (hour, _, used) = clock(&words[2..])?;
        return Some((hour, 30, 2 + used));
    }
    let (value, used) = number(&words[..1])?;
    if used != 1 || value.fract() != 0.0 || !(1.0..=12.0).contains(&value) {
        return None;
    }
    // Validated to 1-12 above.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let hour = value as u16;
    match words.get(1..) {
        Some(["o", "clock", ..]) => Some((hour, 0, 3)),
        Some(["oclock", ..]) => Some((hour, 0, 2)),
        Some(["thirty" | "30", "o", "clock", ..]) => Some((hour, 30, 4)),
        Some(["thirty" | "30", ..]) => Some((hour, 30, 2)),
        _ => Some((hour, 0, 1)),
    }
}

fn whole(value: f64, low: f64, high: f64) -> Option<u32> {
    // Range-checked, so the conversion is exact.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (value.fract() == 0.0 && (low..=high).contains(&value)).then_some(value as u32)
}

impl Parser {
    pub fn new(vocabulary: Contact) -> Self {
        Self { vocabulary }
    }

    /// `version.revision` of the vocabulary.
    pub fn release(&self) -> String {
        self.vocabulary.release()
    }

    /// Whether a word cancels a report.
    pub fn is_negation(&self, word: &str) -> bool {
        self.vocabulary.negations.contains(word)
    }

    /// Every word of the vocabulary added in revision 2.
    pub fn grammar_words(&self) -> impl Iterator<Item = &String> {
        self.vocabulary
            .negations
            .iter()
            .chain(&self.vocabulary.degree_words)
            .chain(self.vocabulary.fractions.keys())
    }

    /// "half a kilometer", "quarter of a klick", "half km": the distance in
    /// metres and the words used.
    fn fraction(&self, words: &[&str]) -> Option<(f64, usize)> {
        let fraction = self.vocabulary.fractions.get(*words.first()?)?;
        let mut used = 1;
        if words.get(used) == Some(&"of") {
            used += 1;
        }
        if matches!(words.get(used), Some(&("a" | "an"))) {
            used += 1;
        }
        let unit = self.unit(words.get(used))?;
        Some((fraction * unit, used + 1))
    }

    pub fn affiliation(&self, word: &str) -> Option<Affiliation> {
        let name = self
            .vocabulary
            .affiliations
            .iter()
            .find(|(_, words)| words.iter().any(|w| w == word))?
            .0;
        Some(match name.as_str() {
            "friendly" => Affiliation::Friendly,
            "hostile" => Affiliation::Hostile,
            "neutral" => Affiliation::Neutral,
            "unknown" => Affiliation::Unknown,
            _ => Affiliation::Suspect,
        })
    }

    fn unit(&self, word: Option<&&str>) -> Option<f64> {
        word.and_then(|word| self.vocabulary.units.get(*word))
            .copied()
    }

    /// A compass point spoken as one or two words.
    fn compass_point(&self, words: &[&str]) -> Option<(u16, usize)> {
        if let [first, second, ..] = words
            && let Some(bearing) = self
                .vocabulary
                .compass_points
                .get(&format!("{first}{second}"))
        {
            return Some((*bearing, 2));
        }
        self.vocabulary
            .compass_points
            .get(*words.first()?)
            .map(|bearing| (*bearing, 1))
    }

    /// A bearing spoken digit by digit ("two seven zero"), as hundreds and
    /// a two-digit number ("two seventy", "one twenty five") or as a number.
    fn bearing(words: &[&str]) -> Option<(f64, usize)> {
        let digits: Vec<u32> = words
            .iter()
            .map_while(|w| single_digit(w))
            .take(3)
            .collect();
        if digits.len() >= 2 {
            let value = digits.iter().fold(0, |total, digit| total * 10 + digit);
            return Some((f64::from(value), digits.len()));
        }
        if let [hundreds, ..] = words
            && !hundreds.starts_with(|c: char| c.is_ascii_digit())
            && let Some(hundreds @ 1..=3) = single_digit(hundreds)
            && let Some((rest, used)) = number(&words[1..])
            && rest.fract() == 0.0
            && (10.0..100.0).contains(&rest)
        {
            return Some((f64::from(hundreds) * 100.0 + rest, used + 1));
        }
        number(words)
    }

    /// A distance from a number and its unit word. "One meters" is refused
    /// as ambiguous: a plural unit after one means the recognizer most
    /// likely dropped a word ("one hundred meters" heard as "one meters").
    fn spoken_distance(&self, value: f64, unit_word: &str) -> Result<u32, Rejection> {
        let unit = self.unit(Some(&unit_word)).unwrap_or(1.0);
        let plural = unit_word
            .strip_suffix('s')
            .is_some_and(|singular| self.vocabulary.units.contains_key(singular));
        if (value - 1.0).abs() < f64::EPSILON && plural {
            return Err(reject(
                RejectReason::Ambiguous,
                format!("\"one {unit_word}\": a number word may be missing"),
            ));
        }
        self.distance(value, unit)
    }

    fn distance(&self, value: f64, unit: f64) -> Result<u32, Rejection> {
        let metres = (value * unit).round();
        whole(metres, 1.0, f64::from(self.vocabulary.max_distance_m)).ok_or_else(|| {
            reject(
                RejectReason::InvalidParameter,
                format!("distance must be 1-{} m", self.vocabulary.max_distance_m),
            )
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one left-to-right pass over the report grammar"
    )]
    pub fn parse(&self, tokens: &[Token]) -> Result<Intent, Rejection> {
        let words: Vec<&str> = tokens.iter().map(|token| token.word.as_str()).collect();
        let vocabulary = &self.vocabulary;
        if let Some(word) = words.iter().find(|word| self.is_negation(word)) {
            return Err(reject(
                RejectReason::Cancelled,
                format!("the contact report is negated or cancelled by {word:?}"),
            ));
        }
        let mut affiliation = None;
        let mut clock_position: Option<(u16, u16)> = None;
        let mut bearing: Option<u16> = None;
        let mut distance: Option<u32> = None;
        let mut classification: Option<String> = None;
        let mut quantity: Option<u32> = None;
        let mut activity: Option<String> = None;
        let mut unknown = Vec::new();
        let mut index = 0;
        while index < words.len() {
            let rest = &words[index..];
            let word = rest[0];
            if let Some(found) = self.affiliation(word) {
                set(&mut affiliation, found, "affiliation")?;
                index += 1;
            } else if let Some(found) = vocabulary.activities.get(word) {
                let mut value = found.clone();
                index += 1;
                if found == "moving"
                    && let Some((_, used)) = self.compass_point(&words[index..])
                {
                    value = format!("moving {}", words[index..index + used].concat());
                    index += used;
                }
                set(&mut activity, value, "activity")?;
            } else if let Some(found) = vocabulary.classifications.get(word) {
                set(&mut classification, found.clone(), "classification")?;
                index += 1;
            } else if vocabulary.bearing_words.contains(word) {
                index += 1;
                if words.get(index) == Some(&"of") {
                    index += 1;
                }
                let (value, used) = Self::bearing(&words[index..]).ok_or_else(|| {
                    reject(RejectReason::MissingParameter, "bearing has no value")
                })?;
                let degrees = whole(value, 0.0, 359.0).ok_or_else(|| {
                    reject(
                        RejectReason::InvalidParameter,
                        "bearing must be 0-359 degrees",
                    )
                })?;
                set(&mut bearing, u16::try_from(degrees).unwrap_or(0), "bearing")?;
                index += used;
                if matches!(words.get(index), Some(&("degrees" | "degree"))) {
                    index += 1;
                }
            } else if let Some((degrees, used)) = self.compass_point(rest) {
                set(&mut bearing, degrees, "bearing")?;
                index += used;
            } else if let Some((metres, used)) = self.fraction(rest) {
                set(&mut distance, self.distance(metres, 1.0)?, "distance")?;
                index += used;
            } else if matches!(word, "a" | "an")
                && let Some(unit) = self.unit(rest.get(1))
            {
                // "a kilometer"
                set(&mut distance, self.distance(1.0, unit)?, "distance")?;
                index += 2;
            } else if word.contains(':') || word == "half" {
                let (hour, minutes, used) = clock(rest).ok_or_else(|| {
                    reject(
                        RejectReason::InvalidParameter,
                        format!("invalid clock position {word:?}"),
                    )
                })?;
                set(&mut clock_position, (hour, minutes), "clock position")?;
                index += used;
            } else if starts_number(word) || word == "a" && number(rest).is_some() {
                // Quantity: a number directly before a classification.
                if let Some((value, used)) = number(rest)
                    && let Some(found) = rest
                        .get(used)
                        .and_then(|w| vocabulary.classifications.get(*w))
                {
                    let count = whole(value, 1.0, 100_000.0).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            "quantity must be a whole number",
                        )
                    })?;
                    set(&mut quantity, count, "quantity")?;
                    set(&mut classification, found.clone(), "classification")?;
                    index += used + 1;
                    continue;
                }
                // Quantity with affiliation in between: "two hostile vehicles".
                if let Some((value, used)) = number(rest)
                    && let Some(found) = rest.get(used).and_then(|w| self.affiliation(w))
                    && let Some(kind) = rest
                        .get(used + 1)
                        .and_then(|w| vocabulary.classifications.get(*w))
                {
                    let count = whole(value, 1.0, 100_000.0).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            "quantity must be a whole number",
                        )
                    })?;
                    set(&mut quantity, count, "quantity")?;
                    set(&mut affiliation, found, "affiliation")?;
                    set(&mut classification, kind.clone(), "classification")?;
                    index += used + 2;
                    continue;
                }
                // Clock then distance: "three thirty two hundred meters".
                if clock_position.is_none()
                    && let Some((hour, minutes, used)) = clock(rest)
                    && let Some((value, length)) = number(&rest[used..])
                    && let Some(unit_word) = rest
                        .get(used + length)
                        .filter(|w| self.unit(Some(w)).is_some())
                {
                    set(&mut clock_position, (hour, minutes), "clock position")?;
                    set(
                        &mut distance,
                        self.spoken_distance(value, unit_word)?,
                        "distance",
                    )?;
                    index += used + length + 1;
                    continue;
                }
                // Distance: a number followed by a unit.
                if let Some((value, length)) = number(rest)
                    && let Some(unit_word) =
                        rest.get(length).filter(|w| self.unit(Some(w)).is_some())
                {
                    set(
                        &mut distance,
                        self.spoken_distance(value, unit_word)?,
                        "distance",
                    )?;
                    index += length + 1;
                    continue;
                }
                // Compass bearing: "270 degrees", "two seven zero degrees".
                if let Some((value, length)) = Self::bearing(rest)
                    && rest
                        .get(length)
                        .is_some_and(|w| vocabulary.degree_words.contains(*w))
                {
                    let degrees = whole(value, 0.0, 359.0).ok_or_else(|| {
                        reject(
                            RejectReason::InvalidParameter,
                            "bearing must be 0-359 degrees",
                        )
                    })?;
                    set(&mut bearing, u16::try_from(degrees).unwrap_or(0), "bearing")?;
                    index += length + 1;
                    continue;
                }
                // Otherwise a clock position.
                if let Some((hour, minutes, used)) = clock(rest) {
                    set(&mut clock_position, (hour, minutes), "clock position")?;
                    index += used;
                    continue;
                }
                unknown.push(word);
                index += 1;
            } else if vocabulary.fillers.contains(word) {
                index += 1;
            } else {
                unknown.push(word);
                index += 1;
            }
        }
        if !unknown.is_empty() {
            return Err(reject(
                RejectReason::InvalidParameter,
                format!(
                    "unrecognized words in contact report: {}",
                    unknown.join(" ")
                ),
            ));
        }
        let affiliation = affiliation
            .ok_or_else(|| reject(RejectReason::MissingParameter, "affiliation is missing"))?;
        let direction = match (clock_position, bearing) {
            (Some(_), Some(_)) => {
                return Err(reject(
                    RejectReason::Contradictory,
                    "both a clock position and a compass bearing",
                ));
            }
            (None, None) => {
                return Err(reject(
                    RejectReason::MissingParameter,
                    "direction is missing",
                ));
            }
            (Some((hour, minutes)), None) => {
                if !(1..=12).contains(&hour) || !matches!(minutes, 0 | 30) {
                    return Err(reject(
                        RejectReason::InvalidParameter,
                        "clock position must be a whole or half hour from 1 to 12",
                    ));
                }
                Direction::RelativeClock {
                    relative_bearing_deg: (hour % 12) * 30 + minutes / 2,
                }
            }
            (None, Some(bearing_deg)) => Direction::CompassBearing { bearing_deg },
        };
        let distance_m = distance
            .ok_or_else(|| reject(RejectReason::MissingParameter, "distance is missing"))?;
        Ok(Intent::ContactReport(ContactReport {
            affiliation,
            direction,
            distance_m,
            classification,
            quantity,
            activity,
        }))
    }
}
