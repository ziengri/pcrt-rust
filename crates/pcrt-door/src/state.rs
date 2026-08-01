//! Monotonic door telemetry lifecycle independent of transport and wall clock.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};

use crate::{DoorCount, DoorError, DoorPacket, DoorProtocol};

/// Current complete state, independent of transport and wall-clock timestamps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorSnapshot {
    sequence: u64,
    doors: BTreeMap<DoorId, DoorTelemetry>,
    stale: bool,
}

impl DoorSnapshot {
    /// Returns the sequence changed only by packet and stale transitions.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns telemetry for every configured door.
    #[must_use]
    pub const fn doors(&self) -> &BTreeMap<DoorId, DoorTelemetry> {
        &self.doors
    }

    /// Returns whether controller telemetry has expired or was never received.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.stale
    }

    /// Returns whether any configured door is open.
    #[must_use]
    pub fn any_open(&self) -> bool {
        self.doors
            .values()
            .any(|telemetry| telemetry.state == DoorState::Open)
    }

    /// Returns whether all configured doors are closed.
    #[must_use]
    pub fn all_closed(&self) -> bool {
        !self.any_open()
    }
}

/// Deterministic lifecycle for accepted packets and stale transitions.
#[derive(Clone, Debug)]
pub struct DoorStateMachine {
    door_count: DoorCount,
    stale_timeout: Duration,
    snapshot: DoorSnapshot,
    last_valid_packet_at: Option<Instant>,
}

impl DoorStateMachine {
    /// Creates an initially stale all-closed state machine.
    #[must_use]
    pub fn new(protocol: DoorProtocol, stale_timeout: Duration) -> Self {
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
            snapshot: DoorSnapshot {
                sequence: 0,
                doors,
                stale: true,
            },
            last_valid_packet_at: None,
        }
    }

    /// Returns the initial or most recently accepted snapshot.
    #[must_use]
    pub const fn snapshot(&self) -> &DoorSnapshot {
        &self.snapshot
    }

    /// Replaces complete telemetry with one valid packet and clears stale state.
    ///
    /// # Errors
    ///
    /// Returns an error when the packet belongs to another controller count.
    pub fn accept(&mut self, packet: DoorPacket, now: Instant) -> Result<&DoorSnapshot, DoorError> {
        let (packet_door_count, doors) = packet.into_parts();
        if packet_door_count != self.door_count {
            return Err(DoorError::PacketDoorCountMismatch {
                packet: packet_door_count.get(),
                state_machine: self.door_count.get(),
            });
        }
        self.snapshot.sequence = self.snapshot.sequence.saturating_add(1);
        self.snapshot.doors = doors;
        self.snapshot.stale = false;
        self.last_valid_packet_at = Some(now);
        Ok(&self.snapshot)
    }

    /// Marks the state stale once after the strict Python-compatible timeout.
    ///
    /// Returns the new snapshot only for that one stale transition. A never-seen
    /// controller is already stale and does not emit a duplicate transition.
    pub fn mark_stale_if_due(&mut self, now: Instant) -> Option<&DoorSnapshot> {
        let elapsed = self
            .last_valid_packet_at
            .and_then(|last_packet| now.checked_duration_since(last_packet));
        if self.snapshot.stale || elapsed.is_none_or(|elapsed| elapsed <= self.stale_timeout) {
            return None;
        }
        self.snapshot.sequence = self.snapshot.sequence.saturating_add(1);
        self.snapshot.stale = true;
        Some(&self.snapshot)
    }
}
