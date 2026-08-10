//! Fixed-size RS-232 controller packet parsing and bounded stream decoding.

use std::collections::BTreeMap;

use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};

use crate::DoorError;

/// Prefix of every controller packet.
pub const HEADER: &[u8] = b"!DOORS:";
// Live controller packets encode voltage in one byte: `<id>=<state>,<voltage>;`.
const DOOR_RECORD_SIZE: usize = 6;
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

    pub(crate) fn into_parts(self) -> (DoorCount, BTreeMap<DoorId, DoorTelemetry>) {
        (self.door_count, self.doors)
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
    /// The parser reads fixed six-byte records: `<id>=<state>,<voltage>;`.
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
            if record[5] != b';' {
                return Err(DoorError::InvalidSeparator {
                    door_id: door_id.get(),
                    expected: b';',
                    actual: record[5],
                });
            }
            let telemetry = DoorTelemetry {
                state,
                voltage_raw: u16::from(record[4]),
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
