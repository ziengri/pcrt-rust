//! Инварианты жизненного цикла сессии пассажиропотока.

/// Версия первого формата session manifest.
pub const SESSION_MANIFEST_VERSION: u16 = 1;

/// Стабильный идентификатор сессии.
///
/// Значение безопасно использовать как имя одного каталога. Storage создаёт новые
/// идентификаторы в читаемом формате `cam-{camera_id}-{unix_ms}`; проверка не
/// должна зависеть от конкретного генератора.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(String);

impl SessionId {
    /// Создаёт идентификатор, не допускающий обхода файловой системы.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если значение пустое, слишком длинное или не является
    /// одним безопасным компонентом пути.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSessionId> {
        let value = value.into();
        if value.is_empty()
            || value == "."
            || value == ".."
            || value.contains(['/', '\\', '\0'])
            || value.len() > 128
        {
            return Err(InvalidSessionId);
        }
        Ok(Self(value))
    }

    /// Возвращает строковую форму идентификатора.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Ошибка валидации идентификатора сессии.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSessionId;

impl core::fmt::Display for InvalidSessionId {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("session id must be a single non-empty path component")
    }
}

impl std::error::Error for InvalidSessionId {}

/// Логическое состояние одной сессии.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Capturing,
    Ready,
    Claimed,
    Failed,
}

impl SessionState {
    /// Проверяет допустимость необратимого или recovery-перехода.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Capturing | Self::Claimed, Self::Ready | Self::Failed)
                | (Self::Ready, Self::Claimed | Self::Failed)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionId, SessionState};

    #[test]
    fn session_id_cannot_escape_its_directory() {
        for invalid in ["", ".", "..", "../../etc", "session/video.mkv", "a\\b"] {
            assert!(
                SessionId::new(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
        assert_eq!(
            SessionId::new("018ff5a0-0000").unwrap().as_str(),
            "018ff5a0-0000"
        );
    }

    #[test]
    fn lifecycle_supports_capture_and_processing_recovery() {
        assert!(SessionState::Capturing.can_transition_to(SessionState::Ready));
        assert!(SessionState::Claimed.can_transition_to(SessionState::Ready));
        assert!(!SessionState::Ready.can_transition_to(SessionState::Capturing));
        assert!(!SessionState::Failed.can_transition_to(SessionState::Ready));
    }
}
