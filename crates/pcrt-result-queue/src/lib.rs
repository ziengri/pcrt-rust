#![forbid(unsafe_code)]
//! Надёжная SQLite-очередь готовых результатов пассажиропотока.
//!
//! Очередь рассчитана на один uploader. Она не выполняет HTTP-запросы и не
//! определяет retry policy: uploader читает строку, затем удаляет её, переносит
//! на новое время или в dead letter.

use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use pcrt_model::SessionId;
use rusqlite::{Connection, OptionalExtension, params};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const LATEST_SCHEMA_VERSION: i64 = 1;
const MAX_ERROR_CHARS: usize = 2_000;

/// Миллисекунды Unix epoch.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Создаёт время из миллисекунд Unix epoch.
    #[must_use]
    pub const fn from_unix_millis(value: i64) -> Self {
        Self(value)
    }

    /// Возвращает текущее системное время в миллисекундах Unix epoch.
    #[must_use]
    pub fn now() -> Self {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let millis = duration.as_millis();
        let value = i64::try_from(millis).unwrap_or(i64::MAX);
        Self(value)
    }

    /// Возвращает представление для `SQLite`.
    #[must_use]
    pub const fn as_unix_millis(self) -> i64 {
        self.0
    }
}

/// Итог операции `insert`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertOutcome {
    /// Создана новая prepared-строка.
    Inserted,
    /// Идентичный результат для этой сессии уже существует.
    Existing,
}

/// Итог публикации prepared-результата для uploader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishOutcome {
    /// Prepared-строка стала доступной uploader.
    Published,
    /// Строка уже была опубликована ранее.
    AlreadyPublished,
}

/// Готовая к отправке строка очереди.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueEntry {
    pub session_id: SessionId,
    pub idempotency_key: String,
    pub payload_json: String,
    pub attempts: u32,
    pub created_at: Timestamp,
    pub next_attempt_at: Timestamp,
}

/// SQLite-очередь готовых результатов.
pub struct ResultQueue {
    connection: Connection,
}

impl ResultQueue {
    /// Открывает SQLite-базу, создаёт родительский каталог и применяет миграции.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку при недоступном пути, SQLite-ошибке или неизвестной
    /// версии схемы.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, QueueError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(QueueError::Io)?;
        }

        let mut connection = Connection::open(path).map_err(QueueError::Sqlite)?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(QueueError::Sqlite)?;
        connection
            .execute_batch(
                "
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = FULL;
                PRAGMA foreign_keys = ON;
                ",
            )
            .map_err(QueueError::Sqlite)?;
        migrate(&mut connection)?;

        Ok(Self { connection })
    }

    /// Добавляет готовый результат как prepared-строку.
    ///
    /// Processor вызывает `publish` только после удаления видео. Пока запись
    /// prepared, uploader не может её прочитать и удалить.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для пустого ключа/сообщения, несовпадающего повтора или
    /// SQLite-ошибки.
    pub fn insert(
        &self,
        session_id: &SessionId,
        idempotency_key: &str,
        payload_json: &str,
        now: Timestamp,
    ) -> Result<InsertOutcome, QueueError> {
        if idempotency_key.trim().is_empty() {
            return Err(QueueError::EmptyIdempotencyKey);
        }
        if payload_json.trim().is_empty() {
            return Err(QueueError::EmptyPayload);
        }

        let inserted = self
            .connection
            .execute(
                "
                INSERT INTO result_queue (
                    session_id,
                    idempotency_key,
                    payload_json,
                    state,
                    created_at_ms,
                    next_attempt_at_ms
                ) VALUES (?1, ?2, ?3, 'prepared', ?4, ?4)
                ON CONFLICT(session_id) DO NOTHING
                ",
                params![
                    session_id.as_str(),
                    idempotency_key,
                    payload_json,
                    now.as_unix_millis(),
                ],
            )
            .map_err(QueueError::Sqlite)?;

        if inserted == 1 {
            return Ok(InsertOutcome::Inserted);
        }

        let existing = self
            .connection
            .query_row(
                "
                SELECT idempotency_key, payload_json
                FROM result_queue
                WHERE session_id = ?1
                ",
                [session_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(QueueError::Sqlite)?;

        if existing == (idempotency_key.to_owned(), payload_json.to_owned()) {
            Ok(InsertOutcome::Existing)
        } else {
            Err(QueueError::ConflictingSessionResult {
                session_id: session_id.as_str().to_owned(),
            })
        }
    }

    /// Делает prepared-результат доступным uploader.
    ///
    /// Вызов после уже выполненной публикации безопасен. Вызов для dead-letter
    /// строки или отсутствующего `session_id` является ошибкой.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если строка отсутствует или находится в dead letter,
    /// либо SQLite-ошибку.
    pub fn publish(&self, session_id: &SessionId) -> Result<PublishOutcome, QueueError> {
        let updated = self
            .connection
            .execute(
                "
                UPDATE result_queue
                SET state = 'pending'
                WHERE session_id = ?1 AND state = 'prepared'
                ",
                [session_id.as_str()],
            )
            .map_err(QueueError::Sqlite)?;

        if updated == 1 {
            return Ok(PublishOutcome::Published);
        }

        let state = self
            .connection
            .query_row(
                "SELECT state FROM result_queue WHERE session_id = ?1",
                [session_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(QueueError::Sqlite)?;

        match state.as_deref() {
            Some("pending") => Ok(PublishOutcome::AlreadyPublished),
            Some("dead_letter") => Err(QueueError::MessageIsDeadLetter {
                session_id: session_id.as_str().to_owned(),
            }),
            Some(other) => Err(QueueError::InvalidStoredState {
                value: other.to_owned(),
            }),
            None => Err(QueueError::MissingMessage {
                session_id: session_id.as_str().to_owned(),
            }),
        }
    }

    /// Проверяет наличие строки для указанной сессии независимо от её состояния.
    ///
    /// # Errors
    ///
    /// Возвращает SQLite-ошибку.
    pub fn contains_session(&self, session_id: &SessionId) -> Result<bool, QueueError> {
        let found = self
            .connection
            .query_row(
                "SELECT 1 FROM result_queue WHERE session_id = ?1",
                [session_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(QueueError::Sqlite)?;
        Ok(found.is_some())
    }

    /// Возвращает идентификаторы prepared-строк для startup recovery.
    ///
    /// Storage проверяет наличие видеокаталога для каждого идентификатора,
    /// удаляет его при необходимости, затем вызывает `publish`.
    ///
    /// # Errors
    ///
    /// Возвращает SQLite-ошибку или ошибку целостности повреждённой строки.
    pub fn prepared_session_ids(&self) -> Result<Vec<SessionId>, QueueError> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT session_id
                FROM result_queue
                WHERE state = 'prepared'
                ORDER BY created_at_ms ASC, session_id ASC
                ",
            )
            .map_err(QueueError::Sqlite)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(QueueError::Sqlite)?;

        rows.map(|row| {
            let value = row.map_err(QueueError::Sqlite)?;
            SessionId::new(value.clone()).map_err(|_| QueueError::InvalidStoredSessionId { value })
        })
        .collect()
    }

    /// Возвращает самую старую готовую к отправке строку.
    ///
    /// Очередь рассчитана на единственный uploader. Падение uploader до `delete`
    /// приводит к безопасной повторной отправке с тем же idempotency key.
    ///
    /// # Errors
    ///
    /// Возвращает SQLite-ошибку или ошибку целостности повреждённой строки.
    pub fn next_due(&self, now: Timestamp) -> Result<Option<QueueEntry>, QueueError> {
        let entry = self
            .connection
            .query_row(
                "
                SELECT
                    session_id,
                    idempotency_key,
                    payload_json,
                    attempts,
                    created_at_ms,
                    next_attempt_at_ms
                FROM result_queue
                WHERE state = 'pending' AND next_attempt_at_ms <= ?1
                ORDER BY next_attempt_at_ms ASC, created_at_ms ASC, session_id ASC
                LIMIT 1
                ",
                [now.as_unix_millis()],
                |row| {
                    Ok(StoredEntry {
                        session_id: row.get(0)?,
                        idempotency_key: row.get(1)?,
                        payload_json: row.get(2)?,
                        attempts: row.get(3)?,
                        created_at_ms: row.get(4)?,
                        next_attempt_at_ms: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(QueueError::Sqlite)?;

        entry.map(QueueEntry::try_from).transpose()
    }

    /// Удаляет подтверждённый API результат из очереди.
    ///
    /// Возвращает `true`, если строка существовала.
    ///
    /// # Errors
    ///
    /// Возвращает SQLite-ошибку.
    pub fn delete(&self, session_id: &SessionId) -> Result<bool, QueueError> {
        let deleted = self
            .connection
            .execute(
                "DELETE FROM result_queue WHERE session_id = ?1",
                [session_id.as_str()],
            )
            .map_err(QueueError::Sqlite)?;
        Ok(deleted == 1)
    }

    /// Назначает повторную попытку отправки и сохраняет диагностическую ошибку.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если pending-строка не найдена, либо SQLite-ошибку.
    pub fn reschedule(
        &self,
        session_id: &SessionId,
        attempted_at: Timestamp,
        retry_at: Timestamp,
        error: &str,
    ) -> Result<(), QueueError> {
        let updated = self
            .connection
            .execute(
                "
                UPDATE result_queue
                SET
                    attempts = attempts + 1,
                    last_attempt_at_ms = ?2,
                    next_attempt_at_ms = ?3,
                    last_error = ?4
                WHERE session_id = ?1 AND state = 'pending'
                ",
                params![
                    session_id.as_str(),
                    attempted_at.as_unix_millis(),
                    retry_at.as_unix_millis(),
                    truncate_error(error),
                ],
            )
            .map_err(QueueError::Sqlite)?;
        ensure_pending(updated, session_id)
    }

    /// Прекращает автоматические попытки и сохраняет причину в dead letter.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если pending-строка не найдена, либо SQLite-ошибку.
    pub fn dead_letter(
        &self,
        session_id: &SessionId,
        attempted_at: Timestamp,
        error: &str,
    ) -> Result<(), QueueError> {
        let updated = self
            .connection
            .execute(
                "
                UPDATE result_queue
                SET
                    state = 'dead_letter',
                    attempts = attempts + 1,
                    last_attempt_at_ms = ?2,
                    last_error = ?3,
                    dead_letter_at_ms = ?2
                WHERE session_id = ?1 AND state = 'pending'
                ",
                params![
                    session_id.as_str(),
                    attempted_at.as_unix_millis(),
                    truncate_error(error),
                ],
            )
            .map_err(QueueError::Sqlite)?;
        ensure_pending(updated, session_id)
    }
}

#[derive(Debug)]
struct StoredEntry {
    session_id: String,
    idempotency_key: String,
    payload_json: String,
    attempts: u32,
    created_at_ms: i64,
    next_attempt_at_ms: i64,
}

impl TryFrom<StoredEntry> for QueueEntry {
    type Error = QueueError;

    fn try_from(entry: StoredEntry) -> Result<Self, Self::Error> {
        let session_id = SessionId::new(entry.session_id.clone()).map_err(|_| {
            QueueError::InvalidStoredSessionId {
                value: entry.session_id,
            }
        })?;
        Ok(Self {
            session_id,
            idempotency_key: entry.idempotency_key,
            payload_json: entry.payload_json,
            attempts: entry.attempts,
            created_at: Timestamp::from_unix_millis(entry.created_at_ms),
            next_attempt_at: Timestamp::from_unix_millis(entry.next_attempt_at_ms),
        })
    }
}

/// Ошибка доступа или нарушения инвариантов очереди.
#[derive(Debug)]
pub enum QueueError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    EmptyIdempotencyKey,
    EmptyPayload,
    ConflictingSessionResult { session_id: String },
    MissingMessage { session_id: String },
    MessageIsDeadLetter { session_id: String },
    MissingPendingMessage { session_id: String },
    InvalidStoredSessionId { value: String },
    InvalidStoredState { value: String },
    UnsupportedSchemaVersion { found: i64 },
}

impl core::fmt::Display for QueueError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "result queue filesystem error: {error}"),
            Self::Sqlite(error) => write!(formatter, "result queue SQLite error: {error}"),
            Self::EmptyIdempotencyKey => formatter.write_str("idempotency key must not be empty"),
            Self::EmptyPayload => formatter.write_str("result payload must not be empty"),
            Self::ConflictingSessionResult { session_id } => {
                write!(
                    formatter,
                    "conflicting result already exists for session {session_id}"
                )
            }
            Self::MissingMessage { session_id } => {
                write!(
                    formatter,
                    "no result queue message for session {session_id}"
                )
            }
            Self::MessageIsDeadLetter { session_id } => {
                write!(
                    formatter,
                    "result queue message is dead letter for session {session_id}"
                )
            }
            Self::MissingPendingMessage { session_id } => {
                write!(
                    formatter,
                    "no pending result queue message for session {session_id}"
                )
            }
            Self::InvalidStoredSessionId { value } => {
                write!(
                    formatter,
                    "stored result queue session id is invalid: {value:?}"
                )
            }
            Self::InvalidStoredState { value } => {
                write!(formatter, "stored result queue state is invalid: {value:?}")
            }
            Self::UnsupportedSchemaVersion { found } => {
                write!(formatter, "unsupported result queue schema version {found}")
            }
        }
    }
}

impl std::error::Error for QueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::EmptyIdempotencyKey
            | Self::EmptyPayload
            | Self::ConflictingSessionResult { .. }
            | Self::MissingMessage { .. }
            | Self::MessageIsDeadLetter { .. }
            | Self::MissingPendingMessage { .. }
            | Self::InvalidStoredSessionId { .. }
            | Self::InvalidStoredState { .. }
            | Self::UnsupportedSchemaVersion { .. } => None,
        }
    }
}

fn migrate(connection: &mut Connection) -> Result<(), QueueError> {
    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY
            );
            ",
        )
        .map_err(QueueError::Sqlite)?;

    let version = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(QueueError::Sqlite)?
        .unwrap_or_default();
    if version > LATEST_SCHEMA_VERSION {
        return Err(QueueError::UnsupportedSchemaVersion { found: version });
    }

    if version == 0 {
        let transaction = connection.transaction().map_err(QueueError::Sqlite)?;
        transaction
            .execute_batch(
                "
                CREATE TABLE result_queue (
                    session_id TEXT PRIMARY KEY NOT NULL,
                    idempotency_key TEXT NOT NULL UNIQUE
                        CHECK(length(trim(idempotency_key)) > 0),
                    payload_json TEXT NOT NULL CHECK(length(trim(payload_json)) > 0),
                    state TEXT NOT NULL
                        CHECK(state IN ('prepared', 'pending', 'dead_letter')),
                    created_at_ms INTEGER NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
                    next_attempt_at_ms INTEGER NOT NULL,
                    last_attempt_at_ms INTEGER,
                    last_error TEXT,
                    dead_letter_at_ms INTEGER
                );

                CREATE INDEX result_queue_due_idx
                ON result_queue(next_attempt_at_ms, created_at_ms, session_id)
                WHERE state = 'pending';
                ",
            )
            .map_err(QueueError::Sqlite)?;
        transaction
            .execute(
                "INSERT INTO schema_migrations(version) VALUES (?1)",
                [1_i64],
            )
            .map_err(QueueError::Sqlite)?;
        transaction.commit().map_err(QueueError::Sqlite)?;
    }

    Ok(())
}

fn ensure_pending(updated: usize, session_id: &SessionId) -> Result<(), QueueError> {
    if updated == 1 {
        Ok(())
    } else {
        Err(QueueError::MissingPendingMessage {
            session_id: session_id.as_str().to_owned(),
        })
    }
}

fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{InsertOutcome, PublishOutcome, QueueError, ResultQueue, Timestamp};
    use pcrt_model::SessionId;

    static NEXT_DATABASE_ID: AtomicU64 = AtomicU64::new(0);

    const NOW: Timestamp = Timestamp::from_unix_millis(1_000);

    #[test]
    fn prepared_result_is_hidden_until_published() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let session = session("session-1");

        assert_eq!(
            queue
                .insert(&session, "result:session-1", r#"{"in":4}"#, NOW)
                .unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(queue.next_due(NOW).unwrap(), None);
        assert_eq!(queue.prepared_session_ids().unwrap(), vec![session.clone()]);
        assert_eq!(queue.publish(&session).unwrap(), PublishOutcome::Published);
        assert_eq!(queue.prepared_session_ids().unwrap(), Vec::new());

        let entry = queue.next_due(NOW).unwrap().unwrap();
        assert_eq!(entry.session_id, session);
        assert_eq!(entry.idempotency_key, "result:session-1");
        assert_eq!(entry.payload_json, r#"{"in":4}"#);
        assert_eq!(entry.attempts, 0);
        drop(queue);
        remove_test_database(&path);
    }

    #[test]
    fn identical_insert_is_idempotent_but_conflict_is_rejected() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let session = session("session-1");

        assert_eq!(
            queue
                .insert(&session, "result:session-1", r"{}", NOW)
                .unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            queue
                .insert(&session, "result:session-1", r"{}", NOW)
                .unwrap(),
            InsertOutcome::Existing
        );
        assert!(matches!(
            queue.insert(&session, "result:session-1", r#"{"in":5}"#, NOW),
            Err(QueueError::ConflictingSessionResult { .. })
        ));
        drop(queue);
        remove_test_database(&path);
    }

    #[test]
    fn publishing_is_idempotent() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let session = session("session-1");
        queue
            .insert(&session, "result:session-1", r"{}", NOW)
            .unwrap();

        assert_eq!(queue.publish(&session).unwrap(), PublishOutcome::Published);
        assert_eq!(
            queue.publish(&session).unwrap(),
            PublishOutcome::AlreadyPublished
        );
        drop(queue);
        remove_test_database(&path);
    }

    #[test]
    fn rescheduled_entry_waits_and_counts_attempts() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let session = session("session-1");
        insert_and_publish(&queue, &session);

        queue
            .reschedule(
                &session,
                Timestamp::from_unix_millis(1_100),
                Timestamp::from_unix_millis(2_000),
                "API unavailable",
            )
            .unwrap();

        assert_eq!(
            queue.next_due(Timestamp::from_unix_millis(1_999)).unwrap(),
            None
        );
        let entry = queue
            .next_due(Timestamp::from_unix_millis(2_000))
            .unwrap()
            .unwrap();
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_at, Timestamp::from_unix_millis(2_000));
        drop(queue);
        remove_test_database(&path);
    }

    #[test]
    fn dead_letter_is_not_deliverable_or_reschedulable() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let session = session("session-1");
        insert_and_publish(&queue, &session);

        queue
            .dead_letter(
                &session,
                Timestamp::from_unix_millis(1_100),
                "invalid payload",
            )
            .unwrap();

        assert_eq!(
            queue.next_due(Timestamp::from_unix_millis(2_000)).unwrap(),
            None
        );
        assert!(matches!(
            queue.reschedule(
                &session,
                Timestamp::from_unix_millis(2_000),
                Timestamp::from_unix_millis(3_000),
                "retry",
            ),
            Err(QueueError::MissingPendingMessage { .. })
        ));
        drop(queue);
        remove_test_database(&path);
    }

    #[test]
    fn delete_removes_delivered_result() {
        let path = test_database_path();
        let queue = ResultQueue::open(&path).unwrap();
        let session = session("session-1");
        insert_and_publish(&queue, &session);

        assert!(queue.delete(&session).unwrap());
        assert!(!queue.delete(&session).unwrap());
        assert!(!queue.contains_session(&session).unwrap());
        drop(queue);
        remove_test_database(&path);
    }

    #[test]
    fn results_survive_reopen() {
        let path = test_database_path();
        let session = session("session-1");
        {
            let queue = ResultQueue::open(&path).unwrap();
            insert_and_publish(&queue, &session);
        }

        let reopened = ResultQueue::open(&path).unwrap();
        assert!(reopened.contains_session(&session).unwrap());
        assert!(reopened.next_due(NOW).unwrap().is_some());
        drop(reopened);
        remove_test_database(&path);
    }

    fn insert_and_publish(queue: &ResultQueue, session: &SessionId) {
        queue
            .insert(session, "result:session-1", r"{}", NOW)
            .unwrap();
        queue.publish(session).unwrap();
    }

    fn session(value: &str) -> SessionId {
        SessionId::new(value).unwrap()
    }

    fn test_database_path() -> PathBuf {
        let id = NEXT_DATABASE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "pcrt-result-queue-test-{}-{id}.sqlite",
            std::process::id()
        ))
    }

    fn remove_test_database(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}-shm", path.display()));
        let _ = fs::remove_file(format!("{}-wal", path.display()));
    }
}
