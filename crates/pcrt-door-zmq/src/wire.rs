use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};
use serde::{Deserialize, Serialize};

use crate::{DoorZmqError, DoorsState};

#[derive(Deserialize)]
struct AggregatePayload {
    seq: u64,
    #[allow(dead_code)]
    ts: f64,
    doors: BTreeMap<String, TelemetryPayload>,
    any_open: bool,
    all_closed: bool,
    stale: bool,
}

#[derive(Deserialize)]
struct TelemetryPayload {
    state: u8,
    voltage: u16,
}

#[derive(Serialize)]
struct AggregateWire {
    seq: u64,
    ts: f64,
    doors: BTreeMap<String, TelemetryWire>,
    any_open: bool,
    all_closed: bool,
    stale: bool,
}

#[derive(Serialize)]
struct SelectedWire {
    seq: u64,
    ts: f64,
    door_id: u8,
    state: u8,
    voltage: u16,
    stale: bool,
}

#[derive(Serialize)]
struct TelemetryWire {
    state: u8,
    voltage: u16,
}

pub(crate) fn timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

pub(crate) fn encode_aggregate(state: &DoorsState, timestamp: f64) -> Result<String, DoorZmqError> {
    let doors = state
        .doors()
        .iter()
        .map(|(door_id, telemetry)| {
            (
                door_id.get().to_string(),
                TelemetryWire {
                    state: telemetry.state.protocol_byte(),
                    voltage: telemetry.voltage_raw,
                },
            )
        })
        .collect();
    let payload = AggregateWire {
        seq: state.sequence(),
        ts: timestamp,
        doors,
        any_open: state.any_open(),
        all_closed: state.all_closed(),
        stale: state.stale(),
    };
    serde_json::to_string(&payload)
        .map(|payload| format!("doors.state {payload}"))
        .map_err(DoorZmqError::Json)
}

pub(crate) fn encode_selected(
    door_id: DoorId,
    telemetry: DoorTelemetry,
    state: &DoorsState,
    timestamp: f64,
) -> Result<String, DoorZmqError> {
    let payload = SelectedWire {
        seq: state.sequence(),
        ts: timestamp,
        door_id: door_id.get(),
        state: telemetry.state.protocol_byte(),
        voltage: telemetry.voltage_raw,
        stale: state.stale(),
    };
    serde_json::to_string(&payload)
        .map(|payload| format!("door.{}.state {payload}", door_id.get()))
        .map_err(DoorZmqError::Json)
}

pub(crate) fn decode_aggregate(payload: &str) -> Option<DoorsState> {
    let payload = serde_json::from_str::<AggregatePayload>(payload).ok()?;
    if !payload.ts.is_finite() {
        return None;
    }
    let mut doors = BTreeMap::new();
    for (raw_id, telemetry) in payload.doors {
        let door_id = raw_id.parse::<u8>().ok().and_then(DoorId::new)?;
        let state = DoorState::from_protocol_byte(telemetry.state)?;
        if doors
            .insert(
                door_id,
                DoorTelemetry {
                    state,
                    voltage_raw: telemetry.voltage,
                },
            )
            .is_some()
        {
            return None;
        }
    }
    if !(3..=4).contains(&doors.len())
        || !doors
            .keys()
            .copied()
            .zip(1_u8..)
            .all(|(door_id, expected)| door_id.get() == expected)
    {
        return None;
    }
    let state = DoorsState::new(payload.seq, doors, payload.stale);
    (state.any_open() == payload.any_open && state.all_closed() == payload.all_closed)
        .then_some(state)
}
