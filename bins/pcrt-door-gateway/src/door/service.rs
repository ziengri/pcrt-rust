use std::time::{Duration, Instant};

use pcrt_door_zmq::DoorsState;

use super::{
    DecodeEvent, DoorProtocol, DoorStateMachine, GatewayEffect, GatewayHealth, StreamDecoder,
};

/// Deterministic gateway lifecycle between byte source and snapshot publisher.
pub(crate) struct DoorService {
    protocol: DoorProtocol,
    decoder: StreamDecoder,
    machine: DoorStateMachine,
    health: GatewayHealth,
    reconnect_delay: Duration,
    serial_liveness_timeout: Duration,
    last_heartbeat: Instant,
    next_connect_at: Instant,
}

impl DoorService {
    pub(crate) fn new(
        protocol: DoorProtocol,
        stale_timeout: Duration,
        serial_liveness_timeout: Duration,
        reconnect_delay: Duration,
        started_at: Instant,
    ) -> Self {
        Self {
            protocol,
            decoder: StreamDecoder::new(protocol),
            machine: DoorStateMachine::new(protocol, stale_timeout),
            health: GatewayHealth::default(),
            reconnect_delay,
            serial_liveness_timeout,
            last_heartbeat: started_at,
            next_connect_at: started_at,
        }
    }

    pub(crate) const fn snapshot(&self) -> &DoorsState {
        self.machine.snapshot()
    }

    pub(crate) const fn health(&self) -> &GatewayHealth {
        &self.health
    }

    pub(crate) fn record_publish_failure(&mut self) {
        self.health.publish_failures = self.health.publish_failures.saturating_add(1);
    }

    pub(crate) fn reconnect_due(&self, now: Instant) -> bool {
        now >= self.next_connect_at
    }

    pub(crate) fn begin_connect_attempt(&mut self) {
        self.health.reconnect_attempts = self.health.reconnect_attempts.saturating_add(1);
    }

    pub(crate) fn source_connected(&mut self, now: Instant) {
        self.health.connected = true;
        self.health.connected_at = Some(now);
        self.health.last_valid_packet = None;
    }

    pub(crate) fn source_connect_failed(&mut self, now: Instant) {
        self.health.connected = false;
        self.next_connect_at = now + self.reconnect_delay;
    }

    pub(crate) fn source_disconnected(&mut self, now: Instant) {
        self.decoder = StreamDecoder::new(self.protocol);
        self.health.connected = false;
        self.health.connected_at = None;
        self.health.last_valid_packet = None;
        self.next_connect_at = now + self.reconnect_delay;
    }

    pub(crate) fn on_bytes(&mut self, bytes: &[u8], now: Instant) -> Vec<GatewayEffect> {
        let mut outputs = Vec::new();
        for event in self.decoder.push(bytes) {
            match event {
                DecodeEvent::Packet(packet) => {
                    self.health.valid_packets = self.health.valid_packets.saturating_add(1);
                    self.health.last_valid_packet = Some(now);
                    if let Ok(snapshot) = self.machine.accept(packet, now) {
                        outputs.push(GatewayEffect::Publish(snapshot.clone()));
                    }
                }
                DecodeEvent::Rejected(error) => {
                    self.health.rejected_packets = self.health.rejected_packets.saturating_add(1);
                    outputs.push(GatewayEffect::PacketRejected(error));
                }
                DecodeEvent::Truncated => {
                    self.health.truncated_packets = self.health.truncated_packets.saturating_add(1);
                    outputs.push(GatewayEffect::PacketTruncated);
                }
                DecodeEvent::Overflow => {
                    self.health.overflow_events = self.health.overflow_events.saturating_add(1);
                    outputs.push(GatewayEffect::DecoderOverflow);
                }
            }
        }
        outputs
    }

    pub(crate) fn tick(
        &mut self,
        now: Instant,
        heartbeat_interval: Duration,
    ) -> Vec<GatewayEffect> {
        let mut outputs = Vec::new();
        if self.health.connected
            && self
                .health
                .serial_liveness_expired(now, self.serial_liveness_timeout)
        {
            self.source_disconnected(now);
            outputs.push(GatewayEffect::DisconnectForLiveness);
        }
        if let Some(snapshot) = self.machine.mark_stale_if_due(now) {
            outputs.push(GatewayEffect::Publish(snapshot.clone()));
            self.last_heartbeat = now;
        }
        if now.duration_since(self.last_heartbeat) >= heartbeat_interval {
            outputs.push(GatewayEffect::Heartbeat(self.machine.snapshot().clone()));
            self.last_heartbeat = now;
        }
        outputs
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::door::{DoorProtocol, HEADER};

    use super::{DoorService, GatewayEffect};

    #[test]
    fn disconnected_source_drops_partial_serial_packet() {
        let protocol = DoorProtocol::new(3).unwrap();
        let started = Instant::now();
        let mut engine = DoorService::new(
            protocol,
            Duration::from_secs(10),
            Duration::from_secs(20),
            Duration::from_secs(1),
            started,
        );
        engine.source_connected(started);
        assert!(engine.on_bytes(b"!DOORS:1=", started).is_empty());

        engine.source_disconnected(started + Duration::from_secs(1));
        let events = engine.on_bytes(b"1,0;2=0,0;3=0,0;", started + Duration::from_secs(2));

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, GatewayEffect::Publish(_)))
        );
    }

    #[test]
    fn liveness_disconnect_resets_state_and_schedules_retry() {
        let protocol = DoorProtocol::new(3).unwrap();
        let started = Instant::now();
        let mut engine = DoorService::new(
            protocol,
            Duration::from_secs(10),
            Duration::from_secs(5),
            Duration::from_secs(2),
            started,
        );
        engine.source_connected(started);

        let outputs = engine.tick(started + Duration::from_secs(5), Duration::from_secs(60));

        assert!(matches!(
            outputs.as_slice(),
            [GatewayEffect::DisconnectForLiveness]
        ));
        assert!(!engine.health().connected);
        assert!(!engine.reconnect_due(started + Duration::from_secs(6)));
        assert!(engine.reconnect_due(started + Duration::from_secs(7)));
    }

    #[test]
    fn liveness_expires_without_a_valid_packet_or_after_silence() {
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut health = super::GatewayHealth {
            connected_at: Some(now.checked_sub(timeout).unwrap()),
            ..super::GatewayHealth::default()
        };
        assert!(health.serial_liveness_expired(now, timeout));

        health.last_valid_packet = Some(now.checked_sub(Duration::from_secs(1)).unwrap());
        assert!(!health.serial_liveness_expired(now, timeout));
        health.last_valid_packet = Some(now.checked_sub(timeout).unwrap());
        assert!(health.serial_liveness_expired(now, timeout));
    }

    #[test]
    fn fresh_packet_publishes_snapshot_and_prevents_liveness_disconnect() {
        let protocol = DoorProtocol::new(3).unwrap();
        let started = Instant::now();
        let mut engine = DoorService::new(
            protocol,
            Duration::from_secs(10),
            Duration::from_secs(5),
            Duration::from_secs(2),
            started,
        );
        engine.source_connected(started);
        let bytes = [HEADER, b"1=\x01,0;2=\0,0;3=\0,0;"].concat();

        assert!(matches!(
            engine.on_bytes(&bytes, started).as_slice(),
            [GatewayEffect::Publish(_)]
        ));
        assert!(
            engine
                .tick(started + Duration::from_secs(4), Duration::from_secs(60))
                .is_empty()
        );
    }
}
