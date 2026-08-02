use core::fmt;

/// Error returned while configuring, decoding or encoding door data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolError {
    /// The controller protocol supports exactly three or four doors.
    UnsupportedDoorCount(u8),
    /// A full packet has an unexpected fixed size.
    InvalidPacketLength { actual: usize, expected: usize },
    /// A full packet does not begin with the controller header.
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
}

impl fmt::Display for ProtocolError {
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
        }
    }
}

impl std::error::Error for ProtocolError {}
