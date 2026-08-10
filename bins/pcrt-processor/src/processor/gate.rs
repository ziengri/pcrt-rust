//! Fail-closed admission policy for starting a new processing claim.

use std::time::{Duration, Instant};

use pcrt_door_zmq::DoorUpdate;

/// Processor admission policy. It only guards a new claim; it never cancels
/// inference already running for a claimed session.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DoorGate {
    ttl: Duration,
}

impl DoorGate {
    pub(crate) const fn new(ttl: Duration) -> Self {
        Self { ttl }
    }

    pub(crate) fn processing_allowed(
        self,
        update: Option<DoorUpdate>,
        received_at: Option<Instant>,
        now: Instant,
    ) -> bool {
        let Some(update) = update else {
            return false;
        };
        let Some(received_at) = received_at else {
            return false;
        };
        self.processing_allowed_at(update, received_at, now)
    }

    fn processing_allowed_at(self, update: DoorUpdate, received_at: Instant, now: Instant) -> bool {
        let DoorUpdate::Aggregate { all_closed, stale } = update else {
            return false;
        };
        now.checked_duration_since(received_at)
            .is_some_and(|age| age <= self.ttl)
            && !stale
            && all_closed
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use pcrt_door_zmq::DoorUpdate;

    use super::DoorGate;

    #[test]
    fn accepts_only_fresh_non_stale_all_closed_aggregate_state() {
        let now = Instant::now();
        let gate = DoorGate::new(Duration::from_secs(2));

        assert!(gate.processing_allowed_at(update(true, false), now, now));
        assert!(!gate.processing_allowed_at(update(false, false), now, now));
        assert!(!gate.processing_allowed_at(update(true, true), now, now));
    }

    #[test]
    fn rejects_expired_state() {
        let now = Instant::now();
        let gate = DoorGate::new(Duration::from_secs(2));

        assert!(!gate.processing_allowed_at(
            update(true, false),
            now.checked_sub(Duration::from_secs(3)).unwrap(),
            now
        ));
    }

    fn update(all_closed: bool, stale: bool) -> DoorUpdate {
        DoorUpdate::Aggregate { all_closed, stale }
    }
}
