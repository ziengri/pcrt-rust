#![forbid(unsafe_code)]
//! Delivery loop для результатов из `pcrt-result-queue`.
//!
//! Uploader управляет retry policy, но не знает деталей HTTP и `SQLite` схемы.

use std::{
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pcrt_api_client::{DeliveryFailure, DeliveryOutcome, TimelineDelivery};
use pcrt_result_queue::{QueueError, ResultQueue, Timestamp};
use pcrt_service::ShutdownToken;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Настройки повторной доставки.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploaderConfig {
    pub poll_interval: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for UploaderConfig {
    fn default() -> Self {
        Self {
            poll_interval: DEFAULT_POLL_INTERVAL,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

impl UploaderConfig {
    /// Проверяет согласованность retry policy.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для нулевых задержек или initial backoff больше max
    /// backoff.
    pub fn validate(&self) -> Result<(), UploaderConfigError> {
        if self.poll_interval.is_zero() {
            return Err(UploaderConfigError::ZeroPollInterval);
        }
        if self.initial_backoff.is_zero() {
            return Err(UploaderConfigError::ZeroInitialBackoff);
        }
        if self.max_backoff.is_zero() {
            return Err(UploaderConfigError::ZeroMaxBackoff);
        }
        if self.initial_backoff > self.max_backoff {
            return Err(UploaderConfigError::InitialBackoffExceedsMax);
        }
        Ok(())
    }

    fn retry_limit(&self, failed_attempts: u32) -> Duration {
        let mut delay = self.initial_backoff;
        for _ in 1..failed_attempts {
            delay = delay.checked_mul(2).unwrap_or(self.max_backoff);
            if delay >= self.max_backoff {
                return self.max_backoff;
            }
        }
        delay.min(self.max_backoff)
    }
}

/// Ошибка недопустимой retry policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UploaderConfigError {
    ZeroPollInterval,
    ZeroInitialBackoff,
    ZeroMaxBackoff,
    InitialBackoffExceedsMax,
}

impl core::fmt::Display for UploaderConfigError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroPollInterval => {
                formatter.write_str("uploader poll interval must be greater than zero")
            }
            Self::ZeroInitialBackoff => {
                formatter.write_str("uploader initial backoff must be greater than zero")
            }
            Self::ZeroMaxBackoff => {
                formatter.write_str("uploader maximum backoff must be greater than zero")
            }
            Self::InitialBackoffExceedsMax => {
                formatter.write_str("uploader initial backoff must not exceed maximum backoff")
            }
        }
    }
}

impl std::error::Error for UploaderConfigError {}

/// Источник full jitter для retry delay.
pub trait RetryJitter {
    /// Выбирает задержку от нуля до `upper_bound` включительно.
    fn delay_up_to(&mut self, upper_bound: Duration) -> Duration;
}

/// Лёгкий pseudo-random источник full jitter без внешней зависимости.
pub struct FullJitter {
    state: u64,
}

impl Default for FullJitter {
    fn default() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seed = u64::try_from(nanos).unwrap_or(u64::MAX) ^ u64::from(std::process::id());
        Self { state: seed.max(1) }
    }
}

impl RetryJitter for FullJitter {
    fn delay_up_to(&mut self, upper_bound: Duration) -> Duration {
        let upper_bound_ns = u64::try_from(upper_bound.as_nanos().min(u128::from(u64::MAX - 1)))
            .unwrap_or(u64::MAX - 1);
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        Duration::from_nanos(self.state % (upper_bound_ns + 1))
    }
}

/// Изменение очереди после одной итерации uploader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadStep {
    /// Нет pending-результата с наступившим временем повтора.
    Idle,
    /// API подтвердил результат, строка удалена из очереди.
    Delivered { session_id: String },
    /// Временная ошибка сохранена с новым временем попытки.
    Rescheduled {
        session_id: String,
        attempts: u32,
        retry_at: Timestamp,
    },
    /// Автоматическая доставка остановлена.
    DeadLettered { session_id: String, attempts: u32 },
}

/// Ошибка доступа к queue или недопустимой конфигурации.
#[derive(Debug)]
pub enum UploaderError {
    Config(UploaderConfigError),
    Queue(QueueError),
}

impl core::fmt::Display for UploaderError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "invalid uploader configuration: {error}"),
            Self::Queue(error) => write!(formatter, "uploader queue operation failed: {error}"),
        }
    }
}

impl std::error::Error for UploaderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Queue(error) => Some(error),
        }
    }
}

impl From<QueueError> for UploaderError {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

/// Синхронный uploader для одного owner SQLite-очереди.
pub struct Uploader<D, J = FullJitter> {
    queue: ResultQueue,
    delivery: D,
    config: UploaderConfig,
    jitter: J,
}

impl<D> Uploader<D, FullJitter>
where
    D: TimelineDelivery,
{
    /// Создаёт uploader с full jitter, инициализированным системным временем.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку недопустимой retry policy.
    pub fn new(
        queue: ResultQueue,
        delivery: D,
        config: UploaderConfig,
    ) -> Result<Self, UploaderError> {
        Self::with_jitter(queue, delivery, config, FullJitter::default())
    }
}

impl<D, J> Uploader<D, J>
where
    D: TimelineDelivery,
    J: RetryJitter,
{
    /// Создаёт uploader с заданным источником jitter.
    ///
    /// Этот конструктор нужен преимущественно для воспроизводимых тестов.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку недопустимой retry policy.
    pub fn with_jitter(
        queue: ResultQueue,
        delivery: D,
        config: UploaderConfig,
        jitter: J,
    ) -> Result<Self, UploaderError> {
        config.validate().map_err(UploaderError::Config)?;
        Ok(Self {
            queue,
            delivery,
            config,
            jitter,
        })
    }

    /// Обрабатывает одну готовую строку queue.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку SQLite-операции. После ошибки supervisor должен
    /// перезапустить сервис: безопасная повторная доставка определяется
    /// idempotency key результата.
    pub fn process_next(&mut self, now: Timestamp) -> Result<UploadStep, UploaderError> {
        let Some(entry) = self.queue.next_due(now)? else {
            return Ok(UploadStep::Idle);
        };
        let session_id = entry.session_id.as_str().to_owned();
        let failed_attempts = entry.attempts.saturating_add(1);

        match self
            .delivery
            .send_timeline(&entry.payload_json, &entry.idempotency_key)
        {
            DeliveryOutcome::Delivered => {
                self.queue.delete(&entry.session_id)?;
                Ok(UploadStep::Delivered { session_id })
            }
            DeliveryOutcome::Retryable(failure) => {
                let retry_at = retry_at(
                    now,
                    self.jitter
                        .delay_up_to(self.config.retry_limit(failed_attempts)),
                );
                self.queue.reschedule(
                    &entry.session_id,
                    now,
                    retry_at,
                    &failure_message(&failure),
                )?;
                Ok(UploadStep::Rescheduled {
                    session_id,
                    attempts: failed_attempts,
                    retry_at,
                })
            }
            DeliveryOutcome::Permanent(failure) => {
                self.queue
                    .dead_letter(&entry.session_id, now, &failure_message(&failure))?;
                Ok(UploadStep::DeadLettered {
                    session_id,
                    attempts: failed_attempts,
                })
            }
        }
    }

    /// Запускает цикл до получения [`ShutdownToken`].
    ///
    /// После каждой idle-итерации ждёт `poll_interval`. При SQLite-ошибке
    /// возвращает её supervisor, не удаляя строку очереди.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку SQLite-операции.
    pub fn run_until_shutdown(&mut self, shutdown: &ShutdownToken) -> Result<(), UploaderError> {
        while !shutdown.is_shutdown_requested() {
            if matches!(self.process_next(Timestamp::now())?, UploadStep::Idle) {
                thread::sleep(self.config.poll_interval);
            }
        }
        Ok(())
    }

    /// Даёт доступ к queue для diagnostics и тестов.
    #[must_use]
    pub fn queue(&self) -> &ResultQueue {
        &self.queue
    }
}

fn retry_at(now: Timestamp, delay: Duration) -> Timestamp {
    let delay_ms = i64::try_from(delay.as_millis()).unwrap_or(i64::MAX);
    Timestamp::from_unix_millis(now.as_unix_millis().saturating_add(delay_ms))
}

fn failure_message(failure: &DeliveryFailure) -> String {
    match failure.status {
        Some(status) => format!("HTTP {status}: {}", failure.message),
        None => failure.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use pcrt_api_client::{DeliveryFailure, DeliveryOutcome, TimelineDelivery};
    use pcrt_model::SessionId;
    use pcrt_result_queue::{InsertOutcome, ResultQueue};

    use super::{
        RetryJitter, Timestamp, UploadStep, Uploader, UploaderConfig, UploaderConfigError,
    };

    static NEXT_DATABASE_ID: AtomicU64 = AtomicU64::new(0);

    const NOW: Timestamp = Timestamp::from_unix_millis(1_000);

    #[test]
    fn empty_queue_does_not_call_delivery() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let delivery = FakeDelivery::default();
        let mut uploader = uploader(queue, delivery, FixedJitter(Duration::ZERO));

        assert_eq!(uploader.process_next(NOW).unwrap(), UploadStep::Idle);
        assert_eq!(uploader.delivery.calls.get(), 0);
        drop(uploader);
        remove_test_database(&path);
    }

    #[test]
    fn delivered_result_is_deleted() {
        let path = test_database_path();
        let queue = queued_result(&path);
        let delivery = FakeDelivery::with(DeliveryOutcome::Delivered);
        let mut uploader = uploader(queue, delivery, FixedJitter(Duration::ZERO));

        assert_eq!(
            uploader.process_next(NOW).unwrap(),
            UploadStep::Delivered {
                session_id: "session-1".to_owned(),
            }
        );
        assert!(!uploader.queue().contains_session(&session()).unwrap());
        drop(uploader);
        remove_test_database(&path);
    }

    #[test]
    fn retryable_failure_is_rescheduled_with_jitter() {
        let path = test_database_path();
        let queue = queued_result(&path);
        let delivery = FakeDelivery::with(retryable_failure());
        let mut uploader = uploader(queue, delivery, FixedJitter(Duration::from_secs(3)));

        assert_eq!(
            uploader.process_next(NOW).unwrap(),
            UploadStep::Rescheduled {
                session_id: "session-1".to_owned(),
                attempts: 1,
                retry_at: Timestamp::from_unix_millis(4_000),
            }
        );
        assert_eq!(
            uploader
                .queue()
                .next_due(Timestamp::from_unix_millis(3_999))
                .unwrap(),
            None
        );
        assert_eq!(
            uploader
                .queue()
                .next_due(Timestamp::from_unix_millis(4_000))
                .unwrap()
                .unwrap()
                .attempts,
            1
        );
        drop(uploader);
        remove_test_database(&path);
    }

    #[test]
    fn retryable_failure_after_many_attempts_is_rescheduled() {
        let path = test_database_path();
        let queue = queued_result(&path);
        for attempt in 1..10 {
            queue
                .reschedule(
                    &session(),
                    Timestamp::from_unix_millis(1_000 + i64::from(attempt)),
                    NOW,
                    "offline",
                )
                .unwrap();
        }
        let delivery = FakeDelivery::with(retryable_failure());
        let mut uploader = uploader(queue, delivery, FixedJitter(Duration::ZERO));

        assert_eq!(
            uploader.process_next(NOW).unwrap(),
            UploadStep::Rescheduled {
                session_id: "session-1".to_owned(),
                attempts: 10,
                retry_at: NOW,
            }
        );
        assert_eq!(
            uploader.queue().next_due(NOW).unwrap().unwrap().attempts,
            10
        );
        drop(uploader);
        remove_test_database(&path);
    }

    #[test]
    fn permanent_failure_is_dead_lettered_without_retry() {
        let path = test_database_path();
        let queue = queued_result(&path);
        let delivery = FakeDelivery::with(DeliveryOutcome::Permanent(DeliveryFailure {
            status: Some(422),
            message: "invalid timeline payload".to_owned(),
        }));
        let mut uploader = uploader(queue, delivery, FixedJitter(Duration::ZERO));

        assert_eq!(
            uploader.process_next(NOW).unwrap(),
            UploadStep::DeadLettered {
                session_id: "session-1".to_owned(),
                attempts: 1,
            }
        );
        assert_eq!(uploader.queue().next_due(NOW).unwrap(), None);
        drop(uploader);
        remove_test_database(&path);
    }

    #[test]
    fn prepared_result_is_not_delivered() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        queue
            .insert(&session(), "result:session-1", r"{}", NOW)
            .unwrap();
        let delivery = FakeDelivery::with(DeliveryOutcome::Delivered);
        let mut uploader = uploader(queue, delivery, FixedJitter(Duration::ZERO));

        assert_eq!(uploader.process_next(NOW).unwrap(), UploadStep::Idle);
        assert_eq!(uploader.delivery.calls.get(), 0);
        drop(uploader);
        remove_test_database(&path);
    }

    #[test]
    fn backoff_is_capped_at_configured_maximum() {
        let config = UploaderConfig {
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(15),
            ..test_config()
        };

        assert_eq!(config.retry_limit(1), Duration::from_secs(5));
        assert_eq!(config.retry_limit(2), Duration::from_secs(10));
        assert_eq!(config.retry_limit(3), Duration::from_secs(15));
        assert_eq!(config.retry_limit(40), Duration::from_secs(15));
    }

    #[test]
    fn invalid_retry_policy_is_rejected() {
        let config = UploaderConfig {
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(1),
            ..test_config()
        };

        assert_eq!(
            config.validate(),
            Err(UploaderConfigError::InitialBackoffExceedsMax)
        );
    }

    fn uploader(
        queue: ResultQueue,
        delivery: FakeDelivery,
        jitter: FixedJitter,
    ) -> Uploader<FakeDelivery, FixedJitter> {
        Uploader::with_jitter(queue, delivery, test_config(), jitter).unwrap()
    }

    fn test_config() -> UploaderConfig {
        UploaderConfig {
            poll_interval: Duration::from_millis(1),
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(60),
        }
    }

    fn retryable_failure() -> DeliveryOutcome {
        DeliveryOutcome::Retryable(DeliveryFailure {
            status: None,
            message: "connection refused".to_owned(),
        })
    }

    fn queued_result(path: &Path) -> ResultQueue {
        let queue = ResultQueue::open(path).unwrap();
        assert_eq!(
            queue
                .insert(&session(), "result:session-1", r"{}", NOW)
                .unwrap(),
            InsertOutcome::Inserted
        );
        queue.publish(&session()).unwrap();
        queue
    }

    fn session() -> SessionId {
        SessionId::new("session-1").unwrap()
    }

    fn test_database_path() -> PathBuf {
        let id = NEXT_DATABASE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "pcrt-uploader-test-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn remove_test_database(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}-shm", path.display()));
        let _ = fs::remove_file(format!("{}-wal", path.display()));
    }

    #[derive(Default)]
    struct FakeDelivery {
        outcomes: RefCell<VecDeque<DeliveryOutcome>>,
        calls: Cell<u32>,
    }

    impl FakeDelivery {
        fn with(outcome: DeliveryOutcome) -> Self {
            Self {
                outcomes: RefCell::new(VecDeque::from([outcome])),
                calls: Cell::new(0),
            }
        }
    }

    impl TimelineDelivery for FakeDelivery {
        fn send_timeline(&self, _payload_json: &str, _idempotency_key: &str) -> DeliveryOutcome {
            self.calls.set(self.calls.get().saturating_add(1));
            self.outcomes
                .borrow_mut()
                .pop_front()
                .expect("unexpected delivery call")
        }
    }

    struct FixedJitter(Duration);

    impl RetryJitter for FixedJitter {
        fn delay_up_to(&mut self, upper_bound: Duration) -> Duration {
            assert!(self.0 <= upper_bound);
            self.0
        }
    }
}
