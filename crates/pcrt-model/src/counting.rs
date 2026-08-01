//! Результаты подсчёта пассажиров без привязки к transport/API DTO.

use crate::{CameraId, SessionId};

/// Итог пересечений линии для одной видеосессии.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PassengerCounts {
    pub entered: u64,
    pub exited: u64,
}

/// Устойчивый предметный результат обработки одной сессии.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessingResult {
    pub session_id: SessionId,
    pub camera_id: CameraId,
    pub captured_at_ms: i64,
    pub counts: PassengerCounts,
}
