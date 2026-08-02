use std::time::{Duration, Instant};

use pcrt_door_zmq::{DoorsState, ReceivedDoorsState};
use pcrt_model::door::{DoorId, DoorState};

/// Fail-closed policy for one configured door and aggregate bus state.
pub(crate) struct DoorGate {
    door_id: DoorId,
    receipt_ttl: Duration,
}

impl DoorGate {
    pub(crate) const fn new(door_id: DoorId, receipt_ttl: Duration) -> Self {
        Self {
            door_id,
            receipt_ttl,
        }
    }

    /// Returns true only for a fresh, non-stale open state of the selected door.
    pub(crate) fn is_open(&self, received: Option<&ReceivedDoorsState>, now: Instant) -> bool {
        received
            .is_some_and(|received| self.is_open_at(received.state(), received.received_at(), now))
    }

    fn is_open_at(&self, state: &DoorsState, received_at: Instant, now: Instant) -> bool {
        now.checked_duration_since(received_at)
            .is_some_and(|age| age <= self.receipt_ttl)
            && self.state_is_open(state)
    }

    fn state_is_open(&self, state: &DoorsState) -> bool {
        !state.stale()
            && state
                .door(self.door_id)
                .is_some_and(|telemetry| telemetry.state == DoorState::Open)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pcrt_door_zmq::DoorsState;
    use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};

    use super::DoorGate;

    #[test]
    fn accepts_only_a_fresh_open_selected_door() {
        let gate = DoorGate::new(DoorId::new(2).unwrap(), std::time::Duration::from_secs(2));
        let mut doors = BTreeMap::new();
        for raw_id in 1..=3 {
            doors.insert(
                DoorId::new(raw_id).unwrap(),
                DoorTelemetry {
                    state: if raw_id == 2 {
                        DoorState::Open
                    } else {
                        DoorState::Closed
                    },
                    voltage_raw: 0,
                },
            );
        }

        let now = std::time::Instant::now();
        let fresh = DoorsState::new(1, doors.clone(), false);
        assert!(gate.is_open_at(&fresh, now, now + std::time::Duration::from_secs(2)));
        assert!(!gate.is_open_at(
            &fresh,
            now,
            now + std::time::Duration::from_secs(2) + std::time::Duration::from_nanos(1)
        ));
        assert!(!gate.state_is_open(&DoorsState::new(1, doors, true)));
    }

    #[test]
    fn rejects_closed_or_missing_selected_door() {
        let gate = DoorGate::new(DoorId::new(2).unwrap(), std::time::Duration::from_secs(2));
        let mut doors = BTreeMap::new();
        doors.insert(
            DoorId::new(1).unwrap(),
            DoorTelemetry {
                state: DoorState::Open,
                voltage_raw: 0,
            },
        );

        assert!(!gate.state_is_open(&DoorsState::new(1, doors, false)));
    }
}
