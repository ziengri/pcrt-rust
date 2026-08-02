use std::{collections::BTreeMap, time::Instant};

use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};

/// Complete shared state published on the door bus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorsState {
    sequence: u64,
    doors: BTreeMap<DoorId, DoorTelemetry>,
    stale: bool,
}

impl DoorsState {
    /// Creates a complete state produced by the gateway state machine.
    #[must_use]
    pub fn new(sequence: u64, doors: BTreeMap<DoorId, DoorTelemetry>, stale: bool) -> Self {
        Self {
            sequence,
            doors,
            stale,
        }
    }

    /// Sequence changed only by accepted packets and stale transitions.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Complete telemetry for every configured door.
    #[must_use]
    pub const fn doors(&self) -> &BTreeMap<DoorId, DoorTelemetry> {
        &self.doors
    }

    /// Whether telemetry is expired or has not yet been received.
    #[must_use]
    pub const fn stale(&self) -> bool {
        self.stale
    }

    /// Returns telemetry for one door.
    #[must_use]
    pub fn door(&self, door_id: DoorId) -> Option<&DoorTelemetry> {
        self.doors.get(&door_id)
    }

    /// Whether any configured door is open.
    #[must_use]
    pub fn any_open(&self) -> bool {
        self.doors
            .values()
            .any(|telemetry| telemetry.state == DoorState::Open)
    }

    /// Whether all configured doors are closed.
    #[must_use]
    pub fn all_closed(&self) -> bool {
        !self.any_open()
    }
}

/// Latest aggregate state together with the local subscriber receipt time.
#[derive(Clone, Debug)]
pub struct ReceivedDoorsState {
    pub(crate) state: DoorsState,
    pub(crate) received_at: Instant,
}

impl ReceivedDoorsState {
    /// Returns the validated aggregate state.
    #[must_use]
    pub const fn state(&self) -> &DoorsState {
        &self.state
    }

    /// Returns the local monotonic receipt time for consumer TTL policy.
    #[must_use]
    pub const fn received_at(&self) -> Instant {
        self.received_at
    }
}
