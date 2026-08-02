#![forbid(unsafe_code)]
//! `ZeroMQ` transport adapters for the PCRT door wire contract.

mod error;
mod ipc_endpoint;
mod model;
mod publisher;
mod subscriber;
mod wire;

pub use error::DoorZmqError;
pub use model::{DoorsState, ReceivedDoorsState};
pub use publisher::DoorPublisher;
pub use subscriber::AggregateDoorSubscriber;

#[cfg(test)]
mod tests;
