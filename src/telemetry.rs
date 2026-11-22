use anyhow::{Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use lxmf_core::constants::FIELD_TELEMETRY;
use r3akt_map_core::telemetry::decode_field;
use r3akt_map_core::telemetry_encode::encode_with_information;
use rmpv::Value;
use serde_json::json;

use crate::bss::Position;

/// LXMF fields `{2: <telemetry>}` as `MessagePack`.
pub fn fields(name: &str, pos: Position, received_at: DateTime<Utc>) -> Result<Vec<u8>> {
    let map = Value::Map(vec![field(name, pos, received_at)?]);
    let mut fields = Vec::new();
    rmpv::encode::write_value(&mut fields, &map)?;
    Ok(fields)
}

/// The telemetry field `(0x02, <packed telemetry>)`, to combine with other
/// fields in one LXMF message.
pub fn field(name: &str, pos: Position, received_at: DateTime<Utc>) -> Result<(Value, Value)> {
    let seconds = u64::try_from(received_at.timestamp())?;
    // Use Telemeter's standard time and Information sensors. Current RCH also
    // accepts a named location projection, which can omit unmeasured readings.
    let standard =
        encode_with_information(seconds, None, None, Some(name)).map_err(|e| anyhow!(e))?;
    let mut projection = decode_field(&json!(STANDARD.encode(standard)))
        .map_err(|e| anyhow!(e))?
        .telemetry;
    let mut location = json!({"latitude": pos.latitude, "longitude": pos.longitude, "last_update_timestamp": seconds});
    if let Some(altitude) = pos.altitude_m {
        location["altitude"] = json!(altitude);
    }
    if let Some(speed) = pos.speed_kmh {
        location["speed"] = json!(speed);
    }
    if let Some(bearing) = pos.heading_deg.filter(|heading| *heading <= 360) {
        location["bearing"] = json!(bearing);
    }
    projection["location"] = location;
    let bytes = rmp_serde::to_vec_named(&projection)?;
    decode_field(&json!(STANDARD.encode(&bytes))).map_err(|e| anyhow!(e))?;
    Ok((Value::from(FIELD_TELEMETRY), Value::Binary(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rch_reads_sparse_location_without_a_fictional_accuracy() -> Result<()> {
        let position = Position {
            latitude: 40.0,
            longitude: -75.0,
            altitude_m: None,
            speed_kmh: Some(2),
            heading_deg: Some(354),
        };
        let at = DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        let fields = fields("R4", position, at)?;
        let map = rmpv::decode::read_value(&mut fields.as_slice())?;
        let bytes = map.as_map().unwrap()[0].1.as_slice().unwrap();
        let data = decode_field(&json!(STANDARD.encode(bytes))).map_err(|e| anyhow!(e))?;
        assert_eq!(data.telemetry["information"]["contents"], "R4");
        assert_eq!(data.telemetry["location"]["bearing"], 354);
        assert!(data.telemetry["location"].get("accuracy").is_none());
        assert!(data.telemetry["location"].get("altitude").is_none());
        Ok(())
    }
}
