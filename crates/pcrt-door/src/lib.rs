#![forbid(unsafe_code)]
//! RS-232 door protocol, monotonic state lifecycle and transport-neutral wire codec.
//!
//! The public facade preserves stable consumers while implementation concerns stay
//! separated: [`controller`] handles raw bytes, [`state`] owns lifecycle and
//! [`wire`] encodes messages for a transport adapter.

pub mod controller;
mod error;
pub mod state;
pub mod wire;

pub use controller::{DecodeEvent, DoorCount, DoorPacket, DoorProtocol, HEADER, StreamDecoder};
pub use error::DoorError;
pub use state::{DoorSnapshot, DoorStateMachine};
pub use wire::{WireMessage, encode_snapshot};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pcrt_model::door::{DoorId, DoorState};
    use serde_json::Value;

    use super::{
        DecodeEvent, DoorError, DoorProtocol, DoorStateMachine, HEADER, StreamDecoder,
        encode_snapshot,
    };

    const VALID_3: &str = include_str!("../../../contracts/door/frames/v1/valid-3.hex");
    const VALID_4_UNORDERED: &str =
        include_str!("../../../contracts/door/frames/v1/valid-4-unordered.hex");
    const INVALID_STATE: &str = include_str!("../../../contracts/door/frames/v1/invalid-state.hex");
    const FRESH_OPEN: &str = include_str!("../../../contracts/door/snapshots/v1/fresh-open.json");

    #[test]
    fn parses_live_three_door_fixture() {
        let protocol = DoorProtocol::new(3).unwrap();
        let packet = protocol.parse_packet(&hex_fixture(VALID_3)).unwrap();

        assert_eq!(packet.doors().len(), 3);
        assert_eq!(
            packet.doors().get(&DoorId::new(2).unwrap()).unwrap().state,
            DoorState::Open
        );
        assert_eq!(
            packet
                .doors()
                .get(&DoorId::new(2).unwrap())
                .unwrap()
                .voltage_raw,
            13
        );
    }

    #[test]
    fn accepts_unordered_four_door_records() {
        let protocol = DoorProtocol::new(4).unwrap();
        let packet = protocol
            .parse_packet(&hex_fixture(VALID_4_UNORDERED))
            .unwrap();

        assert_eq!(packet.doors().len(), 4);
        assert_eq!(
            packet
                .doors()
                .get(&DoorId::new(1).unwrap())
                .unwrap()
                .voltage_raw,
            171
        );
    }

    #[test]
    fn rejects_invalid_state_from_static_fixture() {
        let protocol = DoorProtocol::new(3).unwrap();

        assert!(matches!(
            protocol.parse_packet(&hex_fixture(INVALID_STATE)),
            Err(DoorError::InvalidState {
                door_id: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn decoder_handles_every_split_boundary() {
        let bytes = hex_fixture(VALID_3);
        let protocol = DoorProtocol::new(3).unwrap();
        for split_at in 1..bytes.len() {
            let mut decoder = StreamDecoder::new(protocol);
            assert!(
                decoder.push(&bytes[..split_at]).is_empty(),
                "split {split_at}"
            );
            let events = decoder.push(&bytes[split_at..]);
            assert!(
                matches!(events.as_slice(), [DecodeEvent::Packet(_)]),
                "split {split_at}"
            );
        }
    }

    #[test]
    fn decoder_resynchronizes_after_garbage_and_truncated_frame() {
        let bytes = hex_fixture(VALID_3);
        let mut decoder = StreamDecoder::new(DoorProtocol::new(3).unwrap());
        let mut stream = b"garbage!DOORS:1=\x01,".to_vec();
        stream.extend_from_slice(&bytes);

        let events = decoder.push(&stream);

        assert!(matches!(events.first(), Some(DecodeEvent::Truncated)));
        assert!(matches!(events.last(), Some(DecodeEvent::Packet(_))));
    }

    #[test]
    fn decoder_rejects_invalid_candidate_then_accepts_next_packet() {
        let invalid = hex_fixture(INVALID_STATE);
        let valid = hex_fixture(VALID_3);
        let mut decoder = StreamDecoder::new(DoorProtocol::new(3).unwrap());
        let mut stream = invalid;
        stream.extend_from_slice(&valid);

        assert!(matches!(
            decoder.push(&stream).as_slice(),
            [DecodeEvent::Rejected(_), DecodeEvent::Packet(_)]
        ));
    }

    #[test]
    fn initial_state_is_stale_and_timeout_is_strict() {
        let protocol = DoorProtocol::new(3).unwrap();
        let mut machine = DoorStateMachine::new(protocol, Duration::from_secs(2));
        let started = std::time::Instant::now();
        assert!(machine.snapshot().is_stale());
        assert_eq!(machine.snapshot().sequence(), 0);

        let packet = protocol.parse_packet(&hex_fixture(VALID_3)).unwrap();
        machine.accept(packet, started).unwrap();
        assert!(!machine.snapshot().is_stale());
        assert_eq!(machine.snapshot().sequence(), 1);
        assert!(
            machine
                .mark_stale_if_due(started + Duration::from_secs(2))
                .is_none()
        );
        let stale = machine
            .mark_stale_if_due(started + Duration::from_secs(2) + Duration::from_nanos(1))
            .unwrap();
        assert!(stale.is_stale());
        assert_eq!(stale.sequence(), 2);
        assert!(
            machine
                .mark_stale_if_due(started + Duration::from_secs(3))
                .is_none()
        );
    }

    #[test]
    fn encodes_python_compatible_aggregate_snapshot_fixture() {
        let protocol = DoorProtocol::new(3).unwrap();
        let mut machine = DoorStateMachine::new(protocol, Duration::from_secs(2));
        let packet = protocol.parse_packet(&hex_fixture(VALID_3)).unwrap();
        machine.accept(packet, std::time::Instant::now()).unwrap();

        let messages = encode_snapshot(machine.snapshot(), 1_785_340_800.125).unwrap();
        let expected: Value = serde_json::from_str(FRESH_OPEN).unwrap();
        let actual_payload: Value = serde_json::from_str(&messages[0].payload_json).unwrap();

        assert_eq!(messages[0].topic, expected["topic"]);
        assert_eq!(actual_payload, expected["payload"]);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].topic, "door.1.state");
        assert_eq!(messages[0].as_frame().first(), Some(&b'd'));
    }

    #[test]
    fn rejects_non_finite_wire_timestamp() {
        let machine = DoorStateMachine::new(DoorProtocol::new(3).unwrap(), Duration::ZERO);
        assert_eq!(
            encode_snapshot(machine.snapshot(), f64::NAN),
            Err(DoorError::InvalidTimestamp)
        );
    }

    #[test]
    fn decoder_retains_header_across_garbage_chunk_boundary() {
        let bytes = hex_fixture(VALID_3);
        let mut decoder = StreamDecoder::new(DoorProtocol::new(3).unwrap());
        let mut first_chunk = b"x".to_vec();
        first_chunk.extend_from_slice(&HEADER[..HEADER.len() - 1]);
        assert!(decoder.push(&first_chunk).is_empty());

        let mut second_chunk = HEADER[HEADER.len() - 1..].to_vec();
        second_chunk.extend_from_slice(&bytes[HEADER.len()..]);
        assert!(matches!(
            decoder.push(&second_chunk).as_slice(),
            [DecodeEvent::Packet(_)]
        ));
    }

    fn hex_fixture(source: &str) -> Vec<u8> {
        let hex = source
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<String>();
        assert_eq!(hex.len() % 2, 0);
        (0..hex.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
            .collect()
    }
}
