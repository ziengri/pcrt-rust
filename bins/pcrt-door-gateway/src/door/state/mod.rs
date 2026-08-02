//! Monotonic door telemetry lifecycle independent of transport and wall clock.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};

use pcrt_door_zmq::DoorsState;

use super::{ControllerPacket, DoorCount, DoorProtocol, ProtocolError};

/// Deterministic lifecycle for accepted packets and stale transitions.
#[derive(Clone, Debug)]
pub(crate) struct DoorStateMachine {
    door_count: DoorCount,
    stale_timeout: Duration,
    snapshot: DoorsState,
    last_valid_packet_at: Option<Instant>,
}

impl DoorStateMachine {
    /// Creates an initially stale all-closed state machine.
    #[must_use]
    pub(crate) fn new(protocol: DoorProtocol, stale_timeout: Duration) -> Self {
        let mut doors = BTreeMap::new();
        for raw_id in 1..=protocol.door_count().get() {
            if let Some(door_id) = DoorId::new(raw_id) {
                doors.insert(
                    door_id,
                    DoorTelemetry {
                        state: DoorState::Closed,
                        voltage_raw: 0,
                    },
                );
            }
        }
        Self {
            door_count: protocol.door_count(),
            stale_timeout,
            snapshot: DoorsState::new(0, doors, true),
            last_valid_packet_at: None,
        }
    }

    /// Returns the initial or most recently accepted snapshot.
    #[must_use]
    pub(crate) const fn snapshot(&self) -> &DoorsState {
        &self.snapshot
    }

    /// Replaces complete telemetry with one valid packet and clears stale state.
    ///
    /// # Errors
    ///
    /// Returns an error when the packet belongs to another controller count.
    pub(crate) fn accept(
        &mut self,
        packet: ControllerPacket,
        now: Instant,
    ) -> Result<&DoorsState, ProtocolError> {
        let (packet_door_count, doors) = packet.into_parts();
        if packet_door_count != self.door_count {
            return Err(ProtocolError::PacketDoorCountMismatch {
                packet: packet_door_count.get(),
                state_machine: self.door_count.get(),
            });
        }
        self.snapshot = DoorsState::new(self.snapshot.sequence().saturating_add(1), doors, false);
        self.last_valid_packet_at = Some(now);
        Ok(&self.snapshot)
    }

    /// Marks the state stale once after the strict Python-compatible timeout.
    ///
    /// Returns the new snapshot only for that one stale transition. A never-seen
    /// controller is already stale and does not emit a duplicate transition.
    pub(crate) fn mark_stale_if_due(&mut self, now: Instant) -> Option<&DoorsState> {
        let elapsed = self
            .last_valid_packet_at
            .and_then(|last_packet| now.checked_duration_since(last_packet));
        if self.snapshot.stale() || elapsed.is_none_or(|elapsed| elapsed <= self.stale_timeout) {
            return None;
        }
        self.snapshot = DoorsState::new(
            self.snapshot.sequence().saturating_add(1),
            self.snapshot.doors().clone(),
            true,
        );
        Some(&self.snapshot)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{DoorProtocol, DoorStateMachine};

    #[test]
    fn initial_state_is_stale_and_transitions_once_after_timeout() {
        let protocol = DoorProtocol::new(3).unwrap();
        let now = Instant::now();
        let mut machine = DoorStateMachine::new(protocol, Duration::from_secs(2));
        assert!(machine.snapshot().stale());

        let packet = protocol
            .parse_packet(b"!DOORS:1=\0,0;2=\x01,1;3=\0,2;")
            .unwrap();
        let accepted = machine.accept(packet, now).unwrap();
        assert_eq!(accepted.sequence(), 1);
        assert!(!accepted.stale());
        assert!(
            machine
                .mark_stale_if_due(now + Duration::from_secs(2))
                .is_none()
        );

        let stale = machine
            .mark_stale_if_due(now + Duration::from_secs(2) + Duration::from_nanos(1))
            .unwrap();
        assert_eq!(stale.sequence(), 2);
        assert!(stale.stale());
        assert!(
            machine
                .mark_stale_if_due(now + Duration::from_secs(3))
                .is_none()
        );
    }
}
