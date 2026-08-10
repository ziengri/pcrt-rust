//! Private processor binary adapters and policies.

mod gate;
mod lock;

pub(crate) use gate::DoorGate;
pub(crate) use lock::ProcessorLock;
