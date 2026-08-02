use pcrt_door_zmq::DoorsState;

use super::ProtocolError;

/// One effect produced by byte decoding or a periodic gateway tick.
pub(crate) enum GatewayEffect {
    Publish(DoorsState),
    PacketRejected(ProtocolError),
    PacketTruncated,
    DecoderOverflow,
    DisconnectForLiveness,
    Heartbeat(DoorsState),
}
