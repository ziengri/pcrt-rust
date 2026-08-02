//! Private controller ingestion and gateway state lifecycle.

mod effect;
mod health;
mod protocol;
mod service;
mod source;
mod state;

pub(crate) use effect::GatewayEffect;
pub(crate) use health::GatewayHealth;
#[cfg(test)]
pub(crate) use protocol::HEADER;
pub(crate) use protocol::ProtocolError;
pub(crate) use protocol::{ControllerPacket, DecodeEvent, DoorCount, DoorProtocol, StreamDecoder};
pub(crate) use service::DoorService;
pub(crate) use source::{ByteSource, open_source};
pub(crate) use state::DoorStateMachine;
