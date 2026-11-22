//! Turns a Kodiak result for an attributed transmission into RCH LXMF output.
//!
//! Sigil owns everything Kodiak deliberately does not know: the radio's
//! position and heading, the RCH wire format, and the publishing policy.
//!
//! - Chat is sent as an ordinary LXMF chat message.
//! - MECP reports become an RCH mission log entry
//!   (`mission.registry.log_entry.upsert`) whose content is the MECP message
//!   exactly as Kodiak encoded it. When the radio has a fix, the same LXMF
//!   message also carries the radio's position as telemetry (field `0x02`),
//!   so RCH and REM can place the event on the map. Without a fix the report
//!   is still sent, without telemetry.
//! - Contact reports become an RCH map marker (`mission.marker.create`) at the
//!   computed contact position.
//! - RCH commands are sent unchanged as RCH field commands from the radio's
//!   own identity, so RCH's dispatcher and authorization decide the outcome.
//! - Rejections are never sent.
//!
//! Position rule: use the GPS fix from this transmission's BSS packet,
//! otherwise the radio's last valid fix. With no fix ever received, contact
//! reports are ignored locally and MECP reports go out without telemetry. A
//! contact marker also ignores a last fix older than the configured age,
//! since the operator may have moved.
//!
//! Heading rule: clock directions need a verified compass heading from this
//! transmission's own BSS packet. The BSS heading counts only when the
//! operator configures it as a compass heading, and a stored heading is never
//! used because the operator may have turned since.

use std::fmt::Write as _;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use kodiak_core::{Affiliation, ContactReport, Direction, Intent, KodiakResult};
use lxmf_core::constants::FIELD_COMMANDS;
use rns_transport::hash::AddressHash;
use serde_json::{Value, json};

use crate::bss::Position;
use crate::output::frequency_label;
use crate::telemetry;

/// Mean Earth radius in metres (IUGG).
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

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
    /// The radio's last valid fix from the identity registry, with the
    /// time it was received.
    pub last_known: Option<(Position, DateTime<Utc>)>,
    /// Whether the BSS heading is a verified compass heading.
    pub heading_is_compass: bool,
    /// Oldest last-known fix a contact marker may use, in seconds.
    pub marker_max_fix_age_s: u64,
    pub send_chat: bool,
    pub send_intents: bool,
}

impl Context<'_> {
    /// The radio's fix and when it was received: this transmission's fix,
    /// otherwise the last valid one.
    fn fix(&self) -> Option<(Position, DateTime<Utc>)> {
        self.current
            .map(|position| (position, self.timestamp))
            .or(self.last_known)
    }

    /// The observer's position for a contact marker: the current fix, or a
    /// last fix no older than the configured age.
    fn marker_position(&self) -> std::result::Result<Position, String> {
        if let Some(current) = self.current {
            return Ok(current);
        }
        let Some((position, received_at)) = self.last_known else {
            return Err("no valid GPS position has been received from this radio".into());
        };
        let age = (self.timestamp - received_at).num_seconds();
        let limit = i64::try_from(self.marker_max_fix_age_s).unwrap_or(i64::MAX);
        if age > limit {
            return Err(format!(
                "this transmission carried no position and the radio's last fix is {age} s old (limit {limit} s)"
            ));
        }
        Ok(position)
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
            let mut fields = vec![command_field(
                "mission.registry.log_entry.upsert",
                &json!({ "content": report.content }),
                context.event_id,
            )?];
            if let Some((position, received_at)) = context.fix() {
                fields.push(telemetry::field(position, received_at)?);
            }
            Ok(Plan::Command {
                body: report.content.clone(),
                fields: encode_fields(fields)?,
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

fn contact(report: &ContactReport, text: &str, context: &Context<'_>) -> Result<Plan> {
    let observer = match context.marker_position() {
        Ok(observer) => observer,
        Err(reason) => return Ok(Plan::Ignore(reason)),
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
            let Some(current) = context.current else {
                return Ok(Plan::Ignore(
                    "a clock direction needs the heading from this transmission; a stored heading may be stale".into(),
                ));
            };
            let Some(heading) = current.heading_deg.filter(|heading| *heading < 360) else {
                return Ok(Plan::Ignore(
                    "this transmission carried no valid compass heading".into(),
                ));
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
    encode_fields(vec![command_field(command_type, args, command_id)?])
}

/// The command field `(9, [{"command_id", "command_type", "args"}])`.
fn command_field(
    command_type: &str,
    args: &Value,
    command_id: &str,
) -> Result<(rmpv::Value, rmpv::Value)> {
    let entry = json!([{ "command_id": command_id, "command_type": command_type, "args": args }]);
    let encoded = rmp_serde::to_vec_named(&entry).context("encode RCH command")?;
    let commands =
        rmpv::decode::read_value(&mut encoded.as_slice()).context("re-read RCH command")?;
    Ok((rmpv::Value::from(FIELD_COMMANDS), commands))
}

/// LXMF fields as one `MessagePack` map.
fn encode_fields(fields: Vec<(rmpv::Value, rmpv::Value)>) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    rmpv::encode::write_value(&mut encoded, &rmpv::Value::Map(fields))?;
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kodiak_core::{
        Coordinates, MecpParameters, MecpReport, RchCommand, RejectReason, Rejection,
    };

    use super::*;

    const ID: &str = "voice-20261121T100000Z-42";

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn context(
        current: Option<Position>,
        last_known: Option<Position>,
        compass: bool,
    ) -> Context<'static> {
        Context {
            radio_name: "R4",
            event_id: ID,
            frequency_hz: 435_995_000,
            timestamp: now(),
            current,
            // Received a minute before the transmission.
            last_known: last_known.map(|fix| (fix, now() - chrono::Duration::seconds(60))),
            heading_is_compass: compass,
            marker_max_fix_age_s: 300,
            send_chat: true,
            send_intents: true,
        }
    }

    fn ignored(plan: &Plan) -> &str {
        match plan {
            Plan::Ignore(reason) => reason,
            other => panic!("expected Ignore, got {other:?}"),
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
            vocabularies: std::collections::BTreeMap::new(),
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
            vocabularies: std::collections::BTreeMap::new(),
        };
        assert!(matches!(
            plan("Kodiak launch", Some(&rejected), &ctx).unwrap(),
            Plan::Ignore(_)
        ));
    }

    /// The fields of a planned command, by field number.
    fn fields(plan: &Plan) -> BTreeMap<u64, rmpv::Value> {
        let Plan::Command { fields, .. } = plan else {
            panic!("expected a command, got {plan:?}");
        };
        let map = rmpv::decode::read_value(&mut fields.as_slice()).unwrap();
        map.as_map()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.as_u64().unwrap(), value.clone()))
            .collect()
    }

    /// The decoded telemetry in field `0x02`.
    fn telemetry_of(plan: &Plan) -> serde_json::Value {
        use base64::Engine as _;
        let fields = fields(plan);
        let packed = fields[&2].as_slice().expect("telemetry must be binary");
        r3akt_map_core::telemetry::decode_field(&json!(
            base64::engine::general_purpose::STANDARD.encode(packed)
        ))
        .unwrap()
        .telemetry
    }

    #[test]
    fn mecp_is_sent_unchanged_with_the_fix_as_telemetry_in_the_same_message() {
        let report = intent(Intent::MecpReport(MecpReport::single(1, "S02").unwrap()));
        let current = Some(fix(40.0, -75.0, None));
        let last = Some(fix(41.0, -74.0, None));

        let sent = plan("", Some(&report), &context(current, last, false)).unwrap();
        assert!(matches!(&sent, Plan::Command { body, .. } if body == "MECP/1/S02"));
        assert_eq!(fields(&sent).keys().copied().collect::<Vec<_>>(), [2, 9]);
        let entry = command(&sent);
        assert_eq!(entry["command_type"], "mission.registry.log_entry.upsert");
        assert_eq!(entry["command_id"], ID);
        assert_eq!(entry["args"]["content"], "MECP/1/S02");
        let telemetry = telemetry_of(&sent);
        assert_eq!(telemetry["location"]["latitude"], 40.0);
        assert_eq!(telemetry["location"]["longitude"], -75.0);
        assert_eq!(
            telemetry["location"]["last_update_timestamp"].as_f64(),
            Some(1_800_000_000.0)
        );

        // The last fix, stamped with when it was received.
        let sent = plan("", Some(&report), &context(None, last, false)).unwrap();
        assert_eq!(command(&sent)["args"]["content"], "MECP/1/S02");
        let telemetry = telemetry_of(&sent);
        assert_eq!(telemetry["location"]["latitude"], 41.0);
        assert_eq!(
            telemetry["location"]["last_update_timestamp"].as_f64(),
            Some(1_800_000_000.0 - 60.0)
        );

        // No fix at all: still sent, with field 9 only.
        let sent = plan("", Some(&report), &context(None, None, false)).unwrap();
        assert_eq!(fields(&sent).keys().copied().collect::<Vec<_>>(), [9]);
        assert_eq!(command(&sent)["args"]["content"], "MECP/1/S02");
    }

    /// How LXMF would deliver `plan` over a link, by its own rule
    /// (`lxmf_runtime::delivery`: wire length minus `LXMF_OVERHEAD`).
    fn link_representation(plan: &Plan) -> lxmf_core::message::MessageMethod {
        use lxmf_core::constants::{DESTINATION_LENGTH, LXMF_OVERHEAD, SIGNATURE_LENGTH};
        use lxmf_core::message::{Payload, TransportMethod, decide_delivery};
        let Plan::Command { body, fields } = plan else {
            panic!("expected a command, got {plan:?}");
        };
        let fields = rmpv::decode::read_value(&mut fields.as_slice()).unwrap();
        let payload = Payload::new(
            1_800_000_000.0,
            Some(body.clone().into_bytes()),
            Some(Vec::new()),
            Some(fields),
            None,
        );
        // `WireMessage::pack`: destination, source, signature, payload.
        let wire = 2 * DESTINATION_LENGTH + SIGNATURE_LENGTH + payload.to_msgpack().unwrap().len();
        let size = wire - LXMF_OVERHEAD;
        decide_delivery(TransportMethod::Direct, false, size)
            .unwrap()
            .representation
    }

    #[test]
    fn mecp_reports_with_telemetry_fit_one_link_packet() {
        use lxmf_core::message::MessageMethod;
        let full = Position {
            latitude: -43.966_633,
            longitude: -166.125_633,
            altitude_m: Some(-1_200),
            speed_kmh: Some(120),
            heading_deg: Some(359),
        };
        let mut facts = context(Some(full), None, false);
        // A live event ID, as long as the ones Sigil writes.
        facts.event_id = "voice-20261122T205620.420176900Z-72580";
        for report in [
            MecpReport::single(0, "L06").unwrap(),
            MecpReport::encode(
                0,
                vec!["M01".into(), "M07".into(), "S01".into()],
                MecpParameters {
                    pax: Some(12),
                    callsign: Some("BRAVO-6".into()),
                    ..MecpParameters::default()
                },
            )
            .unwrap(),
        ] {
            let sent = plan("", Some(&intent(Intent::MecpReport(report))), &facts).unwrap();
            assert_eq!(link_representation(&sent), MessageMethod::Packet);
        }
    }

    #[test]
    fn multi_code_mecp_keeps_spoken_values_unchanged() {
        let fix = Some(fix(40.0, -75.0, None));
        let spoken = |parameters: MecpParameters| {
            intent(Intent::MecpReport(
                MecpReport::encode(0, vec!["M01".into(), "S01".into()], parameters).unwrap(),
            ))
        };
        let report = spoken(MecpParameters {
            pax: Some(3),
            callsign: Some("BRAVO-6".into()),
            ..MecpParameters::default()
        });
        let sent = plan("", Some(&report), &context(fix, None, false)).unwrap();
        assert_eq!(
            command(&sent)["args"]["content"],
            "MECP/0/M01 S01 3pax ~BRAVO-6"
        );
        assert_eq!(telemetry_of(&sent)["location"]["latitude"], 40.0);
        // Spoken coordinates stay in the text; the fix goes in telemetry.
        let report = spoken(MecpParameters {
            coordinates: Some(Coordinates {
                latitude: 45.5,
                longitude: -73.25,
            }),
            ..MecpParameters::default()
        });
        let sent = plan("", Some(&report), &context(fix, None, false)).unwrap();
        assert_eq!(
            command(&sent)["args"]["content"],
            "MECP/0/M01 S01 45.5,-73.25"
        );
        assert_eq!(telemetry_of(&sent)["location"]["longitude"], -75.0);
        // The text is never lengthened, so a long report is still sent.
        let report = spoken(MecpParameters {
            text: Some("x".repeat(170)),
            ..MecpParameters::default()
        });
        assert!(matches!(
            plan("", Some(&report), &context(fix, None, false)).unwrap(),
            Plan::Command { .. }
        ));
    }

    #[test]
    fn markers_refuse_untrusted_position_or_heading() {
        let clock = contact_report(Direction::RelativeClock {
            relative_bearing_deg: 90,
        });
        let bearing = contact_report(Direction::CompassBearing { bearing_deg: 90 });
        // No fix in this transmission: a stored heading is never used.
        let stored = Some(fix(0.0, 0.0, Some(0)));
        assert!(
            ignored(&plan("", Some(&clock), &context(None, stored, true)).unwrap())
                .contains("stale")
        );
        // A recent stored fix is enough for an absolute bearing.
        assert!(matches!(
            plan("", Some(&bearing), &context(None, stored, false)).unwrap(),
            Plan::Command { .. }
        ));
        // A stale stored fix is not.
        let mut old = context(None, stored, false);
        old.last_known = stored.map(|f| (f, now() - chrono::Duration::seconds(301)));
        assert!(ignored(&plan("", Some(&bearing), &old).unwrap()).contains("301 s old"));
        // An invalid heading is refused.
        let bad = Some(fix(0.0, 0.0, Some(360)));
        assert!(matches!(
            plan("", Some(&clock), &context(bad, None, true)).unwrap(),
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
