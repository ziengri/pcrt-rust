//! JSON snapshot encoding independent from `ZeroMQ` sockets.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::{DoorError, DoorSnapshot};

/// One `ZeroMQ` PUB message before transport framing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WireMessage {
    /// Topic subscribers use for filtering.
    pub topic: String,
    /// Compact UTF-8 JSON payload without the topic prefix.
    pub payload_json: String,
}

impl WireMessage {
    /// Returns the one-frame form used by current Python subscribers.
    #[must_use]
    pub fn as_frame(&self) -> Vec<u8> {
        format!("{} {}", self.topic, self.payload_json).into_bytes()
    }
}

#[derive(Serialize)]
struct WireTelemetry {
    state: u8,
    voltage: u16,
}

#[derive(Serialize)]
struct AggregatePayload {
    seq: u64,
    ts: f64,
    doors: BTreeMap<String, WireTelemetry>,
    any_open: bool,
    all_closed: bool,
    stale: bool,
}

#[derive(Serialize)]
struct PerDoorPayload {
    seq: u64,
    ts: f64,
    door_id: u8,
    state: u8,
    voltage: u16,
    stale: bool,
}

/// Encodes aggregate then ascending per-door JSON messages for one snapshot.
///
/// `emitted_at_epoch_seconds` is intentionally supplied by the gateway so the FSM
/// remains purely monotonic. It preserves the Python `ts` number format.
///
/// # Errors
///
/// Returns [`DoorError::InvalidTimestamp`] when the supplied wall-clock value is not
/// representable as JSON number.
///
/// # Panics
///
/// Panics only if serializing one of the fixed, in-memory payload structs fails.
pub fn encode_snapshot(
    snapshot: &DoorSnapshot,
    emitted_at_epoch_seconds: f64,
) -> Result<Vec<WireMessage>, DoorError> {
    if !emitted_at_epoch_seconds.is_finite() {
        return Err(DoorError::InvalidTimestamp);
    }
    let doors = snapshot
        .doors()
        .iter()
        .map(|(door_id, telemetry)| {
            (
                door_id.get().to_string(),
                WireTelemetry {
                    state: telemetry.state.protocol_byte(),
                    voltage: telemetry.voltage_raw,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let aggregate = AggregatePayload {
        seq: snapshot.sequence(),
        ts: emitted_at_epoch_seconds,
        doors,
        any_open: snapshot.any_open(),
        all_closed: snapshot.all_closed(),
        stale: snapshot.is_stale(),
    };
    let mut messages = Vec::with_capacity(snapshot.doors().len().saturating_add(1));
    messages.push(WireMessage {
        topic: "doors.state".to_owned(),
        payload_json: serde_json::to_string(&aggregate)
            .expect("serializable aggregate payload cannot fail"),
    });
    for (door_id, telemetry) in snapshot.doors() {
        let payload = PerDoorPayload {
            seq: snapshot.sequence(),
            ts: emitted_at_epoch_seconds,
            door_id: door_id.get(),
            state: telemetry.state.protocol_byte(),
            voltage: telemetry.voltage_raw,
            stale: snapshot.is_stale(),
        };
        messages.push(WireMessage {
            topic: format!("door.{}.state", door_id.get()),
            payload_json: serde_json::to_string(&payload)
                .expect("serializable per-door payload cannot fail"),
        });
    }
    Ok(messages)
}
