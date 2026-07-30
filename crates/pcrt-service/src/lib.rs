#![forbid(unsafe_code)]
//! Общие lifecycle-примитивы для длительно работающих сервисов.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Потокобезопасный запрос на штатную остановку сервиса.
///
/// Signal adapter живёт в бинарнике, а рабочие циклы получают только этот тип.
/// Это позволяет тестировать shutdown без отправки настоящего сигнала процессу.
#[derive(Clone, Debug, Default)]
pub struct ShutdownToken(Arc<AtomicBool>);

impl ShutdownToken {
    /// Запрашивает прекращение приёма новой работы.
    pub fn request_shutdown(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Возвращает `true`, если сервис должен начать штатную остановку.
    #[must_use]
    pub fn is_shutdown_requested(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::ShutdownToken;

    #[test]
    fn cloned_tokens_share_shutdown_state() {
        let token = ShutdownToken::default();
        let worker_token = token.clone();
        assert!(!worker_token.is_shutdown_requested());

        token.request_shutdown();

        assert!(worker_token.is_shutdown_requested());
    }
}
