use std::time::{Duration, Instant};

/// Counters and connection timestamps owned by the gateway lifecycle.
#[derive(Default)]
pub(crate) struct GatewayHealth {
    pub(crate) connected: bool,
    pub(crate) valid_packets: u64,
    pub(crate) rejected_packets: u64,
    pub(crate) truncated_packets: u64,
    pub(crate) overflow_events: u64,
    pub(crate) reconnect_attempts: u64,
    pub(crate) publish_failures: u64,
    pub(crate) connected_at: Option<Instant>,
    pub(crate) last_valid_packet: Option<Instant>,
}

impl GatewayHealth {
    pub(crate) fn serial_liveness_expired(&self, now: Instant, timeout: Duration) -> bool {
        let Some(connected_at) = self.connected_at else {
            return false;
        };
        now.duration_since(connected_at) >= timeout
            && self
                .last_valid_packet
                .is_none_or(|last_valid_packet| now.duration_since(last_valid_packet) >= timeout)
    }
}
