use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use lxmf_core::constants::FIELD_TELEMETRY;
use r3akt_map_core::telemetry_encode::{self, Location};
use rmpv::Value;

use crate::bss::Position;

/// The radio's presence: LXMF fields `{2: <packed Telemeter>}` as
/// `MessagePack`.
///
/// The time and location sensors are encoded as REM encodes a position
/// (`reticulum_mobile` `telemeter::encode_position`), with `r3akt-map-core`'s
/// Telemeter encoder and 0 for readings the radio did not send. BSS reports
/// speed in km/h, the unit of the Telemeter wire. BSS carries no fix time,
/// so both times are when the gateway received the fix.
///
/// The radio's name goes in Sideband's Information sensor, through the same
/// encoder. RCH stores no name with telemetry received over LXMF, and its
/// web map labels such a position from this sensor, or else with the
/// sender's address (RCH `ui/src/stores/telemetry.ts`).
pub fn fields(pos: Position, received_at: DateTime<Utc>, name: &str) -> Result<Vec<u8>> {
    let seconds = u64::try_from(received_at.timestamp())?;
    let name = name.trim();
    let packed = telemetry_encode::encode_with_information(
        seconds,
        Some(Location {
            latitude: pos.latitude,
            longitude: pos.longitude,
            altitude: pos.altitude_m.map_or(0.0, f64::from),
            speed: pos.speed_kmh.map_or(0.0, f64::from),
            bearing: pos
                .heading_deg
                .filter(|heading| *heading <= 360)
                .map_or(0.0, f64::from),
            accuracy: 0.0,
            updated_at_s: seconds,
        }),
        None,
        (!name.is_empty()).then_some(name),
    )
    .map_err(|e| anyhow!(e))?;
    let map = Value::Map(vec![(Value::from(FIELD_TELEMETRY), Value::Binary(packed))]);
    let mut fields = Vec::new();
    rmpv::encode::write_value(&mut fields, &map)?;
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use r3akt_map_core::telemetry::decode_field;
    use serde_json::json;

    #[test]
    fn rch_reads_the_position_and_the_radio_name() -> Result<()> {
        let position = Position {
            latitude: 40.0,
            longitude: -75.0,
            altitude_m: None,
            speed_kmh: Some(2),
            heading_deg: Some(354),
        };
        let at = DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        let decode = |fields: Vec<u8>| -> Result<_> {
            let map = rmpv::decode::read_value(&mut fields.as_slice())?;
            let bytes = map.as_map().unwrap()[0].1.as_slice().unwrap().to_vec();
            decode_field(&json!(STANDARD.encode(bytes))).map_err(|e| anyhow!(e))
        };
        let data = decode(fields(position, at, " Corvo ")?)?;
        // What RCH's web map shows as the label of this position.
        assert_eq!(data.telemetry["information"]["contents"], "Corvo");
        let location = &data.telemetry["location"];
        assert_eq!(location["latitude"], 40.0);
        assert_eq!(location["longitude"], -75.0);
        assert_eq!(location["speed"], 2.0);
        assert_eq!(location["bearing"], 354.0);
        assert_eq!(location["last_update_timestamp"], 1_700_000_001.0);
        assert_eq!(data.timestamp_s, Some(1_700_000_001));
        // Without a name the Information sensor is left out.
        let unnamed = decode(fields(position, at, "")?)?;
        assert!(unnamed.telemetry.get("information").is_none());
        assert_eq!(unnamed.telemetry["location"], data.telemetry["location"]);
        Ok(())
    }
}
