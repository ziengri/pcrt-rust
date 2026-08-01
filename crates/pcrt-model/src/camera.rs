//! Идентификаторы камер.

/// Стабильный идентификатор камеры внутри PCRT.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CameraId(String);

impl CameraId {
    /// Создаёт непустой безопасный идентификатор камеры.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для пустого значения, path separators или управляющих байтов.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidCameraId> {
        let value = value.into();
        if value.is_empty()
            || matches!(value.as_str(), "." | "..")
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b'/' | b'\\'))
        {
            return Err(InvalidCameraId);
        }
        Ok(Self(value))
    }

    /// Возвращает строковое представление идентификатора.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Ошибка небезопасного идентификатора камеры.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCameraId;

impl core::fmt::Display for InvalidCameraId {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("camera ID must be a non-empty safe path component")
    }
}

impl std::error::Error for InvalidCameraId {}

#[cfg(test)]
mod tests {
    use super::{CameraId, InvalidCameraId};

    #[test]
    fn camera_id_is_a_safe_path_component() {
        assert_eq!(CameraId::new("cam-1").unwrap().as_str(), "cam-1");
        for value in ["", ".", "..", "front/left", "front\\left", "cam\0one"] {
            assert_eq!(CameraId::new(value), Err(InvalidCameraId));
        }
    }
}
