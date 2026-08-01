#![forbid(unsafe_code)]
//! Типы предметной области PCRT без зависимостей от инфраструктуры.

pub mod camera;
pub mod counting;
pub mod door;
pub mod session;

pub use camera::CameraId;
pub use counting::{PassengerCounts, ProcessingResult};
pub use session::{SESSION_MANIFEST_VERSION, SessionId, SessionState};
