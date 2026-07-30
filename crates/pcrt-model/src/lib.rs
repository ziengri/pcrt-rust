#![forbid(unsafe_code)]
//! Типы предметной области PCRT без зависимостей от инфраструктуры.

pub mod door;
pub mod session;

pub use session::{SESSION_MANIFEST_VERSION, SessionId, SessionState};
