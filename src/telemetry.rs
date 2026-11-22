use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use lxmf_core::constants::FIELD_TELEMETRY;
use r3akt_map_core::telemetry_encode::{self, Location};
use rmpv::Value;

use crate::bss::Position;

/// LXMF fields `{2: <telemetry>}` as `MessagePack`.
pub fn fields(pos: Position, received_at: DateTime<Utc>) -> Result<Vec<u8>> {
    let map = Value::Map(vec![field(pos, received_at)?]);
    let mut fields = Vec::new();
    rmpv::encode::write_value(&mut fields, &map)?;
    Ok(fields)
}

/// The telemetry field `(0x02, <packed Telemeter>)`, to combine with other
/// fields in one LXMF message.
///
/// Encoded exactly as REM encodes a position (`reticulum_mobile`
/// `telemeter::encode_position`): `r3akt-map-core`'s Telemeter encoder with
/// the time and location sensors, and 0 for readings the radio did not send.
/// BSS reports speed in km/h, the unit of the Telemeter wire. BSS carries no
/// fix time, so both times are when the gateway received the fix.
pub fn field(pos: Position, received_at: DateTime<Utc>) -> Result<(Value, Value)> {
    let seconds = u64::try_from(received_at.timestamp())?;
    let packed = telemetry_encode::encode(
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
    )
    .map_err(|e| anyhow!(e))?;
    Ok((Value::from(FIELD_TELEMETRY), Value::Binary(packed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use r3akt_map_core::telemetry::decode_field;
    use serde_json::json;

    #[test]
    fn rch_reads_the_rem_telemeter_encoding() -> Result<()> {
        let position = Position {
            latitude: 40.0,
            longitude: -75.0,
            altitude_m: None,
            speed_kmh: Some(2),
            heading_deg: Some(354),
        };
        let at = DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        let fields = fields(position, at)?;
        let map = rmpv::decode::read_value(&mut fields.as_slice())?;
        let bytes = map.as_map().unwrap()[0].1.as_slice().unwrap();
        let data = decode_field(&json!(STANDARD.encode(bytes))).map_err(|e| anyhow!(e))?;
        let location = &data.telemetry["location"];
        assert_eq!(location["latitude"], 40.0);
        assert_eq!(location["longitude"], -75.0);
        assert_eq!(location["speed"], 2.0);
        assert_eq!(location["bearing"], 354.0);
        assert_eq!(location["last_update_timestamp"], 1_700_000_001.0);
        assert_eq!(data.timestamp_s, Some(1_700_000_001));
        Ok(())
    }
}
