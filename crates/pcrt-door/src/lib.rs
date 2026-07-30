#![forbid(unsafe_code)]
//! RS-232 framing, door-state lifecycle and ZeroMQ-compatible snapshot encoding.
//!
//! This crate deliberately has no serial device or `ZeroMQ` socket dependency. A
//! transport adapter supplies byte chunks to [`StreamDecoder`], and a gateway
//! publishes [`WireMessage`] values emitted by [`encode_snapshot`].

use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, Instant},
};

use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};
use serde::Serialize;

/// Prefix of every controller packet.
pub const HEADER: &[u8] = b"!DOORS:";
const DOOR_RECORD_SIZE: usize = 7;
const MIN_DOOR_COUNT: u8 = 3;
const MAX_DOOR_COUNT: u8 = 4;
const MAX_STREAM_BUFFER: usize = (2 * packet_size_for(MAX_DOOR_COUNT)) + HEADER.len();

const fn packet_size_for(door_count: u8) -> usize {
    HEADER.len() + (DOOR_RECORD_SIZE * door_count as usize)
}

/// Validated number of door records configured for one controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DoorCount(u8);

impl DoorCount {
    /// Creates a count supported by the current controller protocol: 3 or 4.
    ///
    /// # Errors
    ///
    /// Returns [`DoorError::UnsupportedDoorCount`] for every other value.
    pub const fn new(value: u8) -> Result<Self, DoorError> {
        if value >= MIN_DOOR_COUNT && value <= MAX_DOOR_COUNT {
            Ok(Self(value))
        } else {
            Err(DoorError::UnsupportedDoorCount(value))
        }
    }

    /// Returns the number of configured doors.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Returns the fixed packet size for this count.
    #[must_use]
    pub const fn packet_size(self) -> usize {
        packet_size_for(self.0)
    }
}

/// Error returned while configuring, decoding or encoding door data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DoorError {
    /// The controller protocol supports exactly three or four doors.
    UnsupportedDoorCount(u8),
    /// A full packet has an unexpected fixed size.
    InvalidPacketLength { actual: usize, expected: usize },
    /// A full packet does not begin with [`HEADER`].
    InvalidPrefix,
    /// One record uses a non-ASCII or unsupported door ID byte.
    InvalidDoorId(u8),
    /// One record names a door outside the configured range.
    UnexpectedDoorId(u8),
    /// One packet names the same door twice.
    DuplicateDoorId(u8),
    /// One configured door was absent from a packet.
    MissingDoorId(u8),
    /// A fixed record separator differs from the protocol value.
    InvalidSeparator {
        /// Door named by the record when available.
        door_id: u8,
        /// Expected ASCII separator.
        expected: u8,
        /// Received byte.
        actual: u8,
    },
    /// A state byte differs from zero or one.
    InvalidState {
        /// Door named by the record when available.
        door_id: u8,
        /// Received state byte.
        actual: u8,
    },
    /// A packet was parsed for a different controller configuration.
    PacketDoorCountMismatch { packet: u8, state_machine: u8 },
    /// JSON timestamps must be finite values.
    InvalidTimestamp,
}

impl fmt::Display for DoorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDoorCount(count) => {
                write!(formatter, "unsupported door count {count}; expected 3 or 4")
            }
            Self::InvalidPacketLength { actual, expected } => {
                write!(
                    formatter,
                    "invalid door packet length {actual}; expected {expected}"
                )
            }
            Self::InvalidPrefix => formatter.write_str("invalid door packet prefix"),
            Self::InvalidDoorId(value) => write!(formatter, "invalid door ID byte {value:#04x}"),
            Self::UnexpectedDoorId(value) => write!(formatter, "unexpected door ID {value}"),
            Self::DuplicateDoorId(value) => write!(formatter, "duplicate door ID {value}"),
            Self::MissingDoorId(value) => write!(formatter, "missing door ID {value}"),
            Self::InvalidSeparator {
                door_id,
                expected,
                actual,
            } => write!(
                formatter,
                "invalid separator for door {door_id}: expected {expected:#04x}, found {actual:#04x}"
            ),
            Self::InvalidState { door_id, actual } => {
                write!(formatter, "invalid state for door {door_id}: {actual:#04x}")
            }
            Self::PacketDoorCountMismatch {
                packet,
                state_machine,
            } => write!(
                formatter,
                "packet has {packet} doors but state machine expects {state_machine}"
            ),
            Self::InvalidTimestamp => formatter.write_str("door snapshot timestamp must be finite"),
        }
    }
}

impl std::error::Error for DoorError {}

/// Validated complete controller packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorPacket {
    door_count: DoorCount,
    doors: BTreeMap<DoorId, DoorTelemetry>,
}

impl DoorPacket {
    /// Returns the controller count this packet was validated against.
    #[must_use]
    pub const fn door_count(&self) -> DoorCount {
        self.door_count
    }

    /// Returns complete telemetry for every configured door.
    #[must_use]
    pub const fn doors(&self) -> &BTreeMap<DoorId, DoorTelemetry> {
        &self.doors
    }
}

/// Fixed-size parser for one configured controller protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DoorProtocol {
    door_count: DoorCount,
}

impl DoorProtocol {
    /// Creates a parser for exactly three or four doors.
    ///
    /// # Errors
    ///
    /// Returns [`DoorError::UnsupportedDoorCount`] for values other than three or four.
    pub const fn new(door_count: u8) -> Result<Self, DoorError> {
        match DoorCount::new(door_count) {
            Ok(door_count) => Ok(Self { door_count }),
            Err(error) => Err(error),
        }
    }

    /// Returns the configured count.
    #[must_use]
    pub const fn door_count(self) -> DoorCount {
        self.door_count
    }

    /// Parses one complete, fixed-size controller packet.
    ///
    /// The parser reads record bytes by fixed offsets. In particular, a raw voltage
    /// byte equal to `b';'` is valid data and is never treated as a delimiter.
    ///
    /// # Errors
    ///
    /// Returns an error when packet shape, configured IDs or telemetry values violate
    /// the controller contract.
    pub fn parse_packet(self, bytes: &[u8]) -> Result<DoorPacket, DoorError> {
        let expected_size = self.door_count.packet_size();
        if bytes.len() != expected_size {
            return Err(DoorError::InvalidPacketLength {
                actual: bytes.len(),
                expected: expected_size,
            });
        }
        if !bytes.starts_with(HEADER) {
            return Err(DoorError::InvalidPrefix);
        }

        let mut doors = BTreeMap::new();
        for record_index in 0..usize::from(self.door_count.get()) {
            let offset = HEADER.len() + (record_index * DOOR_RECORD_SIZE);
            let record = &bytes[offset..offset + DOOR_RECORD_SIZE];
            let door_id_raw = record[0];
            let Some(door_id) = DoorId::new(door_id_raw.saturating_sub(b'0')) else {
                return Err(DoorError::InvalidDoorId(door_id_raw));
            };
            if door_id_raw < b'1' || door_id.get() > self.door_count.get() {
                return Err(DoorError::UnexpectedDoorId(
                    door_id_raw.saturating_sub(b'0'),
                ));
            }
            if record[1] != b'=' {
                return Err(DoorError::InvalidSeparator {
                    door_id: door_id.get(),
                    expected: b'=',
                    actual: record[1],
                });
            }
            let Some(state) = DoorState::from_protocol_byte(record[2]) else {
                return Err(DoorError::InvalidState {
                    door_id: door_id.get(),
                    actual: record[2],
                });
            };
            if record[3] != b',' {
                return Err(DoorError::InvalidSeparator {
                    door_id: door_id.get(),
                    expected: b',',
                    actual: record[3],
                });
            }
            if record[6] != b';' {
                return Err(DoorError::InvalidSeparator {
                    door_id: door_id.get(),
                    expected: b';',
                    actual: record[6],
                });
            }
            let telemetry = DoorTelemetry {
                state,
                voltage_raw: u16::from_be_bytes([record[4], record[5]]),
            };
            if doors.insert(door_id, telemetry).is_some() {
                return Err(DoorError::DuplicateDoorId(door_id.get()));
            }
        }

        for expected_id in 1..=self.door_count.get() {
            let Some(expected_id) = DoorId::new(expected_id) else {
                return Err(DoorError::InvalidDoorId(expected_id));
            };
            if !doors.contains_key(&expected_id) {
                return Err(DoorError::MissingDoorId(expected_id.get()));
            }
        }

        Ok(DoorPacket {
            door_count: self.door_count,
            doors,
        })
    }
}

/// Stream-level event emitted for each decoded packet or recovery decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeEvent {
    /// One complete valid packet was decoded.
    Packet(DoorPacket),
    /// One complete fixed-size candidate was semantically invalid.
    Rejected(DoorError),
    /// A new header interrupted an incomplete older packet.
    Truncated,
    /// Input exceeded the bounded decoder buffer and older bytes were discarded.
    Overflow,
}

/// Bounded byte-stream decoder for packets parsed by [`DoorProtocol`].
#[derive(Clone, Debug)]
pub struct StreamDecoder {
    protocol: DoorProtocol,
    buffer: Vec<u8>,
}

impl StreamDecoder {
    /// Creates a decoder for one fixed controller protocol.
    #[must_use]
    pub fn new(protocol: DoorProtocol) -> Self {
        Self {
            protocol,
            buffer: Vec::with_capacity(MAX_STREAM_BUFFER),
        }
    }

    /// Accepts raw bytes from any transport and returns all newly available events.
    ///
    /// Chunks may split packets at arbitrary positions, contain garbage, concatenate
    /// packets or end mid-frame. The internal buffer is bounded independently of the
    /// caller-provided input slice.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<DecodeEvent> {
        let mut events = Vec::new();
        self.append_bounded(bytes, &mut events);

        loop {
            let Some(header_offset) = find_header(&self.buffer) else {
                self.retain_header_tail();
                break;
            };
            if header_offset > 0 {
                self.buffer.drain(..header_offset);
            }

            let packet_size = self.protocol.door_count().packet_size();
            if self.buffer.len() < packet_size {
                break;
            }
            if let Some(next_header_offset) = find_header(&self.buffer[1..packet_size]) {
                let next_header_offset = next_header_offset + 1;
                self.buffer.drain(..next_header_offset);
                events.push(DecodeEvent::Truncated);
                continue;
            }

            let candidate = self.buffer[..packet_size].to_vec();
            match self.protocol.parse_packet(&candidate) {
                Ok(packet) => {
                    self.buffer.drain(..packet_size);
                    events.push(DecodeEvent::Packet(packet));
                }
                Err(error) => {
                    events.push(DecodeEvent::Rejected(error));
                    if let Some(next_header_offset) = find_header(&self.buffer[1..packet_size]) {
                        self.buffer.drain(..=next_header_offset);
                    } else {
                        self.buffer.drain(..packet_size);
                    }
                }
            }
        }
        events
    }

    fn append_bounded(&mut self, bytes: &[u8], events: &mut Vec<DecodeEvent>) {
        if bytes.is_empty() {
            return;
        }
        if bytes.len() >= MAX_STREAM_BUFFER {
            self.buffer.clear();
            self.buffer
                .extend_from_slice(&bytes[bytes.len() - MAX_STREAM_BUFFER..]);
            events.push(DecodeEvent::Overflow);
            return;
        }
        let combined_len = self.buffer.len().saturating_add(bytes.len());
        if combined_len > MAX_STREAM_BUFFER {
            let overflow = combined_len - MAX_STREAM_BUFFER;
            self.buffer.drain(..overflow);
            events.push(DecodeEvent::Overflow);
        }
        self.buffer.extend_from_slice(bytes);
    }

    fn retain_header_tail(&mut self) {
        let retained = self.buffer.len().min(HEADER.len().saturating_sub(1));
        let discarded = self.buffer.len().saturating_sub(retained);
        if discarded > 0 {
            self.buffer.drain(..discarded);
        }
    }
}

fn find_header(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(HEADER.len())
        .position(|window| window == HEADER)
}

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
        if packet.door_count != self.door_count {
            return Err(DoorError::PacketDoorCountMismatch {
                packet: packet.door_count.get(),
                state_machine: self.door_count.get(),
            });
        }
        self.snapshot.sequence = self.snapshot.sequence.saturating_add(1);
        self.snapshot.doors = packet.doors;
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
        .doors
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
        seq: snapshot.sequence,
        ts: emitted_at_epoch_seconds,
        doors,
        any_open: snapshot.any_open(),
        all_closed: snapshot.all_closed(),
        stale: snapshot.stale,
    };
    let mut messages = Vec::with_capacity(snapshot.doors.len().saturating_add(1));
    messages.push(WireMessage {
        topic: "doors.state".to_owned(),
        payload_json: serde_json::to_string(&aggregate)
            .expect("serializable aggregate payload cannot fail"),
    });
    for (door_id, telemetry) in &snapshot.doors {
        let payload = PerDoorPayload {
            seq: snapshot.sequence,
            ts: emitted_at_epoch_seconds,
            door_id: door_id.get(),
            state: telemetry.state.protocol_byte(),
            voltage: telemetry.voltage_raw,
            stale: snapshot.stale,
        };
        messages.push(WireMessage {
            topic: format!("door.{}.state", door_id.get()),
            payload_json: serde_json::to_string(&payload)
                .expect("serializable per-door payload cannot fail"),
        });
    }
    Ok(messages)
}

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
    fn parses_static_three_door_fixture_including_semicolon_voltage_byte() {
        let protocol = DoorProtocol::new(3).unwrap();
        let packet = protocol.parse_packet(&hex_fixture(VALID_3)).unwrap();

        assert_eq!(packet.doors().len(), 3);
        assert_eq!(
            packet.doors().get(&DoorId::new(1).unwrap()).unwrap().state,
            DoorState::Open
        );
        assert_eq!(
            packet
                .doors()
                .get(&DoorId::new(2).unwrap())
                .unwrap()
                .voltage_raw,
            59
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
            0xabcd
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

        let events = decoder.push(&stream);

        assert!(matches!(
            events.as_slice(),
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
        let events = decoder.push(&second_chunk);

        assert!(matches!(events.as_slice(), [DecodeEvent::Packet(_)]));
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
