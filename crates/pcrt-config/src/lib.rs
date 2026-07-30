#![forbid(unsafe_code)]
//! Общие правила происхождения и безопасного отображения конфигурации.
//!
//! Конкретные schema принадлежат сервисам. Этот crate фиксирует единый порядок
//! источников, чтобы сервисы не получали разные значения из одинаковых файлов.

/// Источник значения конфигурации в порядке повышения приоритета.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ConfigSource {
    Defaults,
    ConfigFile,
    DeviceFile,
    Environment,
    CommandLine,
}

/// Упорядоченный список источников для `pcrtctl check-config` и документации.
pub const PRECEDENCE: [ConfigSource; 5] = [
    ConfigSource::Defaults,
    ConfigSource::ConfigFile,
    ConfigSource::DeviceFile,
    ConfigSource::Environment,
    ConfigSource::CommandLine,
];

/// Ссылка на секрет, не раскрывающая его содержимое.
///
/// Production конфигурация должна хранить такие ссылки, а не сами ключи API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretRef(String);

impl SecretRef {
    /// Создаёт ссылку на systemd credential или защищённый файл.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для пустой ссылки или строки с нулевым байтом.
    pub fn new(reference: impl Into<String>) -> Result<Self, InvalidSecretRef> {
        let reference = reference.into();
        if reference.trim().is_empty() || reference.contains('\0') {
            return Err(InvalidSecretRef);
        }
        Ok(Self(reference))
    }

    /// Безопасное представление для статуса и логов.
    #[must_use]
    pub fn redacted(&self) -> &'static str {
        "[configured]"
    }
}

/// Ошибка пустой или некорректной ссылки на secret.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSecretRef;

impl core::fmt::Display for InvalidSecretRef {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("secret reference must not be empty")
    }
}

impl std::error::Error for InvalidSecretRef {}

#[cfg(test)]
mod tests {
    use super::{ConfigSource, PRECEDENCE, SecretRef};

    #[test]
    fn precedence_is_explicit_and_stable() {
        assert_eq!(PRECEDENCE.first(), Some(&ConfigSource::Defaults));
        assert_eq!(PRECEDENCE.last(), Some(&ConfigSource::CommandLine));
    }

    #[test]
    fn secret_references_are_never_rendered() {
        let secret = SecretRef::new("timeline-api-key").unwrap();
        assert_eq!(secret.redacted(), "[configured]");
        assert!(SecretRef::new(" ").is_err());
    }
}
