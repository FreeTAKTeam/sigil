//! Turns a Kodiak result for an attributed transmission into RCH LXMF output.
//!
//! Sigil owns everything Kodiak deliberately does not know: the radio's
//! position and heading, the RCH wire format, and the publishing policy.
//!
//! - Chat is sent as an ordinary LXMF chat message.
//! - MECP reports become an RCH mission log entry
//!   (`mission.registry.log_entry.upsert`) with the radio's position appended.
//! - Contact reports become an RCH map marker (`mission.marker.create`) at the
//!   computed contact position.
//! - RCH commands are sent unchanged as RCH field commands from the radio's
//!   own identity, so RCH's dispatcher and authorization decide the outcome.
//! - Rejections are never sent.
//!
//! Position rule: use the GPS fix from this transmission's BSS packet,
//! otherwise the radio's last valid fix. With no fix ever received, MECP and
//! contact reports are ignored locally. Clock directions need a verified
//! compass heading; the BSS heading counts only when the operator configures
//! it as a compass heading.

use std::fmt::Write as _;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use kodiak_core::{Affiliation, ContactReport, Direction, Intent, KodiakResult};
use lxmf_core::constants::FIELD_COMMANDS;
use rns_transport::hash::AddressHash;
use serde_json::{Value, json};

use crate::bss::Position;
use crate::output::frequency_label;

/// Mean Earth radius in metres (IUGG).
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// What the publisher should do with one transcript event.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Send an ordinary chat message.
    Chat(String),
    /// Send an RCH field command, with a human-readable body.
    Command { body: String, fields: Vec<u8> },
    /// Do not send; record why.
    Ignore(String),
    /// Nothing to send under the configured policy.
    Nothing,
}

/// Facts about the transmission that only Sigil knows.
pub struct Context<'a> {
    pub radio_name: &'a str,
    pub event_id: &'a str,
    pub frequency_hz: u32,
    pub timestamp: DateTime<Utc>,
    /// Fix carried by this transmission's BSS packet.
    pub current: Option<Position>,
    /// The radio's last valid fix from the identity registry.
    pub last_known: Option<Position>,
    /// Whether the BSS heading is a verified compass heading.
    pub heading_is_compass: bool,
    pub send_chat: bool,
    pub send_intents: bool,
}

impl Context<'_> {
    fn position(&self) -> Option<Position> {
        self.current.or(self.last_known)
    }

    fn chat(&self, text: &str) -> Plan {
        if !self.send_chat {
            return Plan::Nothing;
        }
        Plan::Chat(format!(
            "Radio: {}\nFrequency: {}\nTime: {}\n\n{text}",
            self.radio_name,
            frequency_label(self.frequency_hz),
            self.timestamp.to_rfc3339()
        ))
    }
}

/// Decides the output for a transcript and its Kodiak result.
///
/// # Errors
///
/// Fails only if the command fields cannot be encoded.
pub fn plan(text: &str, result: Option<&KodiakResult>, context: &Context<'_>) -> Result<Plan> {
    let intent = match result {
        None | Some(KodiakResult::Chat { .. }) => return Ok(context.chat(text)),
        Some(KodiakResult::Rejected { rejection, .. }) => {
            return Ok(Plan::Ignore(format!(
                "Kodiak rejected the command ({:?}): {}",
                rejection.reason, rejection.detail
            )));
        }
        Some(KodiakResult::Intent { .. }) if !context.send_intents => return Ok(Plan::Nothing),
        Some(KodiakResult::Intent { intent, .. }) => intent,
    };
    match intent {
        Intent::MecpReport(report) => {
            let Some(position) = context.position() else {
                return Ok(no_position());
            };
            let log_entry = format!(
                "{} {:.6},{:.6}",
                report.content, position.latitude, position.longitude
            );
            let fields = command_fields(
                "mission.registry.log_entry.upsert",
                &json!({ "content": log_entry }),
                context.event_id,
            )?;
            Ok(Plan::Command {
                body: log_entry,
                fields,
            })
        }
        Intent::ContactReport(report) => contact(report, text, context),
        Intent::RchCommand(command) => {
            let fields = command_fields(
                &command.command,
                &json!(command.arguments),
                context.event_id,
            )?;
            Ok(Plan::Command {
                body: String::new(),
                fields,
            })
        }
    }
}

fn no_position() -> Plan {
    Plan::Ignore("no valid GPS position has been received from this radio".into())
}

fn contact(report: &ContactReport, text: &str, context: &Context<'_>) -> Result<Plan> {
    let Some(observer) = context.position() else {
        return Ok(no_position());
    };
    let bearing = match report.direction {
        Direction::CompassBearing { bearing_deg } => bearing_deg,
        Direction::RelativeClock {
            relative_bearing_deg,
        } => {
            if !context.heading_is_compass {
                return Ok(Plan::Ignore(
                    "a clock direction needs a verified compass heading; the BSS heading is not configured as one".into(),
                ));
            }
            let Some(heading) = observer.heading_deg.filter(|heading| *heading <= 360) else {
                return Ok(Plan::Ignore("the radio reported no compass heading".into()));
            };
            (heading + relative_bearing_deg) % 360
        }
    };
    let (latitude, longitude) = destination(
        observer.latitude,
        observer.longitude,
        bearing,
        report.distance_m,
    );
    let (symbol, label) = match report.affiliation {
        Affiliation::Friendly => ("friendly", "Friendly"),
        Affiliation::Hostile => ("hostile", "Hostile"),
        Affiliation::Neutral => ("neutral", "Neutral"),
        Affiliation::Unknown => ("unknown", "Unknown"),
        // The RCH symbol catalog has no suspect symbol.
        Affiliation::Suspect => ("unknown", "Suspect"),
    };
    let mut name = format!(
        "{label} {}",
        report.classification.as_deref().unwrap_or("contact")
    );
    if let Some(quantity) = report.quantity {
        let _ = write!(name, " x{quantity}");
    }
    let mut notes = format!(
        "Voice report from {} on {}: \"{text}\". {} m at bearing {bearing}° from the reporter.",
        context.radio_name,
        frequency_label(context.frequency_hz),
        report.distance_m
    );
    if let Some(activity) = &report.activity {
        let _ = write!(notes, " Activity: {activity}.");
    }
    let args = json!({
        // Deterministic, so a replayed event cannot create a second marker.
        "object_destination_hash": AddressHash::new_from_slice(context.event_id.as_bytes()).to_hex_string(),
        "marker_type": symbol,
        "symbol": symbol,
        "name": name,
        "category": "contact",
        "lat": latitude,
        "lon": longitude,
        "notes": notes,
    });
    let fields = command_fields("mission.marker.create", &args, context.event_id)?;
    Ok(Plan::Command {
        body: format!("Map marker created: {name}"),
        fields,
    })
}

/// Great-circle destination from a start point, bearing and distance.
pub fn destination(latitude: f64, longitude: f64, bearing_deg: u16, distance_m: u32) -> (f64, f64) {
    let angular = f64::from(distance_m) / EARTH_RADIUS_M;
    let bearing = f64::from(bearing_deg).to_radians();
    let start_lat = latitude.to_radians();
    let start_lon = longitude.to_radians();
    let lat =
        (start_lat.sin() * angular.cos() + start_lat.cos() * angular.sin() * bearing.cos()).asin();
    let lon = start_lon
        + (bearing.sin() * angular.sin() * start_lat.cos())
            .atan2(angular.cos() - start_lat.sin() * lat.sin());
    let lon = (lon.to_degrees() + 540.0).rem_euclid(360.0) - 180.0;
    (lat.to_degrees(), lon)
}

/// LXMF fields `{9: [{"command_id", "command_type", "args"}]}` as `MessagePack`.
pub fn command_fields(command_type: &str, args: &Value, command_id: &str) -> Result<Vec<u8>> {
    let entry = json!([{ "command_id": command_id, "command_type": command_type, "args": args }]);
    let encoded = rmp_serde::to_vec_named(&entry).context("encode RCH command")?;
    let commands =
        rmpv::decode::read_value(&mut encoded.as_slice()).context("re-read RCH command")?;
    let map = rmpv::Value::Map(vec![(rmpv::Value::from(FIELD_COMMANDS), commands)]);
    let mut fields = Vec::new();
    rmpv::encode::write_value(&mut fields, &map)?;
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kodiak_core::{MecpReport, RchCommand, RejectReason, Rejection};

    use super::*;

    const ID: &str = "voice-20261121T100000Z-42";

    fn context(
        current: Option<Position>,
        last_known: Option<Position>,
        compass: bool,
    ) -> Context<'static> {
        Context {
            radio_name: "R4",
            event_id: ID,
            frequency_hz: 435_995_000,
            timestamp: DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            current,
            last_known,
            heading_is_compass: compass,
            send_chat: true,
            send_intents: true,
        }
    }

    fn fix(latitude: f64, longitude: f64, heading_deg: Option<u16>) -> Position {
        Position {
            latitude,
            longitude,
            altitude_m: None,
            speed_kmh: None,
            heading_deg,
        }
    }

    fn intent(intent: Intent) -> KodiakResult {
        KodiakResult::Intent {
            intent,
            transcript: "Kodiak test".into(),
            classifier: "test/1".into(),
            corrections: Vec::new(),
            confidence: None,
        }
    }

    fn command(plan: &Plan) -> serde_json::Value {
        let Plan::Command { fields, .. } = plan else {
            panic!("expected a command, got {plan:?}");
        };
        let map = rmpv::decode::read_value(&mut fields.as_slice()).unwrap();
        let (key, value) = &map.as_map().unwrap()[0];
        assert_eq!(key.as_u64(), Some(9));
        let bytes = rmp_serde::to_vec(value).unwrap();
        let entries: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
        entries[0].clone()
    }

    fn contact_report(direction: Direction) -> KodiakResult {
        intent(Intent::ContactReport(ContactReport {
            affiliation: Affiliation::Hostile,
            direction,
            distance_m: 1_000,
            classification: Some("vehicle".into()),
            quantity: Some(2),
            activity: None,
        }))
    }

    #[test]
    fn destination_matches_known_offsets() {
        let (lat, lon) = destination(0.0, 0.0, 90, 111_195);
        assert!(
            (lat).abs() < 1e-9 && (lon - 1.0).abs() < 1e-3,
            "{lat} {lon}"
        );
        let (lat, lon) = destination(45.0, 7.0, 0, 1_000);
        assert!((lat - 45.008_993).abs() < 1e-5 && (lon - 7.0).abs() < 1e-9);
        let (_, lon) = destination(0.0, 179.999, 90, 1_000);
        assert!(lon < -179.0, "wrapped across the antimeridian: {lon}");
    }

    #[test]
    fn chat_and_rejections() {
        let ctx = context(None, None, false);
        assert!(
            matches!(plan("hello", None, &ctx).unwrap(), Plan::Chat(body) if body.ends_with("\n\nhello"))
        );
        let rejected = KodiakResult::Rejected {
            rejection: Rejection::new(RejectReason::NoIntent, None, "no intent"),
            transcript: "Kodiak launch".into(),
            classifier: "test/1".into(),
            corrections: Vec::new(),
            confidence: None,
        };
        assert!(matches!(
            plan("Kodiak launch", Some(&rejected), &ctx).unwrap(),
            Plan::Ignore(_)
        ));
    }

    #[test]
    fn mecp_uses_current_then_last_fix_and_never_invents_one() {
        let report = intent(Intent::MecpReport(MecpReport {
            severity: 1,
            code: "S02".into(),
            content: "MECP/1/S02".into(),
        }));
        let current = Some(fix(40.0, -75.0, None));
        let last = Some(fix(41.0, -74.0, None));
        let entry = command(&plan("", Some(&report), &context(current, last, false)).unwrap());
        assert_eq!(entry["command_type"], "mission.registry.log_entry.upsert");
        assert_eq!(entry["command_id"], ID);
        assert_eq!(entry["args"]["content"], "MECP/1/S02 40.000000,-75.000000");
        let entry = command(&plan("", Some(&report), &context(None, last, false)).unwrap());
        assert_eq!(entry["args"]["content"], "MECP/1/S02 41.000000,-74.000000");
        assert!(matches!(
            plan("", Some(&report), &context(None, None, false)).unwrap(),
            Plan::Ignore(_)
        ));
    }

    #[test]
    fn contact_markers_need_a_fix_and_clock_needs_a_verified_heading() {
        let clock = contact_report(Direction::RelativeClock {
            relative_bearing_deg: 90,
        });
        let observer = Some(fix(0.0, 0.0, Some(0)));
        assert!(matches!(
            plan("", Some(&clock), &context(observer, None, false)).unwrap(),
            Plan::Ignore(_)
        ));
        assert!(matches!(
            plan("", Some(&clock), &context(None, None, true)).unwrap(),
            Plan::Ignore(_)
        ));
        let no_heading = Some(fix(0.0, 0.0, None));
        assert!(matches!(
            plan("", Some(&clock), &context(no_heading, None, true)).unwrap(),
            Plan::Ignore(_)
        ));
        // Heading 0 + 3 o'clock = due east.
        let entry = command(&plan("t", Some(&clock), &context(observer, None, true)).unwrap());
        assert_eq!(entry["command_type"], "mission.marker.create");
        assert_eq!(entry["args"]["symbol"], "hostile");
        assert_eq!(entry["args"]["name"], "Hostile vehicle x2");
        assert!(entry["args"]["lon"].as_f64().unwrap() > 0.0089);
        assert_eq!(
            entry["args"]["object_destination_hash"]
                .as_str()
                .unwrap()
                .len(),
            32
        );
        // A compass bearing needs only the fix.
        let bearing = contact_report(Direction::CompassBearing { bearing_deg: 180 });
        let entry = command(
            &plan(
                "t",
                Some(&bearing),
                &context(None, Some(fix(10.0, 10.0, None)), false),
            )
            .unwrap(),
        );
        assert!(entry["args"]["lat"].as_f64().unwrap() < 10.0);
    }

    #[test]
    fn rch_commands_pass_through_unchanged() {
        let users = intent(Intent::RchCommand(RchCommand {
            command: "mission.users".into(),
            arguments: BTreeMap::new(),
        }));
        let entry = command(&plan("", Some(&users), &context(None, None, false)).unwrap());
        assert_eq!(
            entry,
            json!({"command_id": ID, "command_type": "mission.users", "args": {}})
        );
        let mut off = context(None, None, false);
        off.send_intents = false;
        assert_eq!(plan("", Some(&users), &off).unwrap(), Plan::Nothing);
    }
}
