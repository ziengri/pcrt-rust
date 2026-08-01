#![forbid(unsafe_code)]
//! Crash-safe orchestration between verified sessions, inference and result queue.

use core::fmt;

use pcrt_model::{CameraId, PassengerCounts, ProcessingResult, SessionId};
use pcrt_result_queue::{QueueError, ResultQueue, Timestamp};
use pcrt_service::ShutdownToken;
use pcrt_storage::{ClaimedSession, SessionStorage, StorageError};

/// AI backend independent from processor lifecycle and result delivery.
pub trait InferenceBackend {
    /// Analyzes all videos represented by one verified claim.
    ///
    /// # Errors
    ///
    /// Returns cancellation or a terminal model/video error.
    fn analyze(
        &mut self,
        session: &ClaimedSession,
        shutdown: &ShutdownToken,
    ) -> Result<PassengerCounts, InferenceError>;
}

/// Converts a domain result into the immutable payload stored for the uploader.
pub trait ResultEncoder {
    /// Produces a stable idempotency key and payload.
    ///
    /// # Errors
    ///
    /// Returns a terminal mapping or serialization error.
    fn encode(&self, result: &ProcessingResult) -> Result<PreparedResult, ResultEncodingError>;
}

/// Immutable result queue input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedResult {
    pub idempotency_key: String,
    pub payload_json: String,
}

/// Outcome of one processor iteration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessingStep {
    Paused,
    Idle,
    ShutdownRequested,
    Completed(SessionId),
    Reconciled(SessionId),
    Failed(SessionId),
}

/// Startup recovery counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessingRecoveryReport {
    pub completed_prepared_results: u32,
    pub released_claims: u32,
    pub failed_sessions: u32,
}

/// Crash-safe processor core. Door IPC, service loop and concrete AI are adapters.
pub struct Processor<B, E> {
    storage: SessionStorage,
    queue: ResultQueue,
    backend: B,
    encoder: E,
    shutdown: ShutdownToken,
}

impl<B: InferenceBackend, E: ResultEncoder> Processor<B, E> {
    #[must_use]
    pub const fn new(
        storage: SessionStorage,
        queue: ResultQueue,
        backend: B,
        encoder: E,
        shutdown: ShutdownToken,
    ) -> Self {
        Self {
            storage,
            queue,
            backend,
            encoder,
            shutdown,
        }
    }

    /// Completes all durable prepared results before releasing abandoned claims.
    ///
    /// # Errors
    ///
    /// Returns an error when queue or filesystem reconciliation cannot complete.
    pub fn recover(
        &self,
        recovered_at_ms: i64,
    ) -> Result<ProcessingRecoveryReport, ProcessingError> {
        let mut report = ProcessingRecoveryReport::default();
        for session_id in self.queue.prepared_session_ids()? {
            self.storage
                .delete_session_with_prepared_result(&session_id)?;
            self.queue.publish(&session_id)?;
            report.completed_prepared_results = report.completed_prepared_results.saturating_add(1);
        }
        let storage_report = self.storage.recover_processing(recovered_at_ms)?;
        report.released_claims = storage_report.released_claims;
        report.failed_sessions = storage_report.failed_sessions;
        Ok(report)
    }

    /// Processes at most one oldest ready session.
    ///
    /// `processing_allowed` must represent a fresh, non-stale, all-doors-closed
    /// decision made immediately before this call.
    ///
    /// # Errors
    ///
    /// Returns infrastructure or durable consistency errors. Terminal inference
    /// and encoding failures are persisted in `failed` and returned as an outcome.
    pub fn process_one(
        &mut self,
        processing_allowed: bool,
        now_ms: i64,
    ) -> Result<ProcessingStep, ProcessingError> {
        if self.shutdown.is_shutdown_requested() {
            return Ok(ProcessingStep::ShutdownRequested);
        }
        if !processing_allowed {
            return Ok(ProcessingStep::Paused);
        }
        let Some(claim) = self.storage.claim_next_ready(now_ms)? else {
            return Ok(ProcessingStep::Idle);
        };

        if self.shutdown.is_shutdown_requested() {
            self.storage
                .release_claim(claim, now_ms, "shutdown requested before inference")?;
            return Ok(ProcessingStep::ShutdownRequested);
        }

        if self.queue.contains_session(claim.session_id())? {
            let session_id = claim.session_id().clone();
            self.storage.delete_claimed(&claim)?;
            self.queue.publish(&session_id)?;
            return Ok(ProcessingStep::Reconciled(session_id));
        }

        let session_id = claim.session_id().clone();
        let camera_id = match single_camera_id(&claim) {
            Ok(camera_id) => camera_id,
            Err(error) => {
                self.storage
                    .mark_claim_failed(&claim, now_ms, &error.to_string())?;
                return Ok(ProcessingStep::Failed(session_id));
            }
        };
        let counts = match self.backend.analyze(&claim, &self.shutdown) {
            Ok(counts) => counts,
            Err(InferenceError::Cancelled) => {
                self.storage
                    .release_claim(claim, now_ms, "inference cancelled during shutdown")?;
                return Ok(ProcessingStep::ShutdownRequested);
            }
            Err(error @ InferenceError::Terminal(_)) => {
                self.storage
                    .mark_claim_failed(&claim, now_ms, &error.to_string())?;
                return Ok(ProcessingStep::Failed(session_id));
            }
        };
        let result = ProcessingResult {
            session_id: session_id.clone(),
            camera_id,
            captured_at_ms: claim.manifest().started_at_ms,
            counts,
        };
        let prepared = match self.encoder.encode(&result) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.storage
                    .mark_claim_failed(&claim, now_ms, &error.to_string())?;
                return Ok(ProcessingStep::Failed(session_id));
            }
        };

        match self.queue.insert(
            &session_id,
            &prepared.idempotency_key,
            &prepared.payload_json,
            Timestamp::from_unix_millis(now_ms),
        ) {
            Ok(_) => {}
            Err(error @ QueueError::ConflictingSessionResult { .. }) => {
                self.storage.mark_claim_failed(
                    &claim,
                    now_ms,
                    "queue contains a conflicting result for this session",
                )?;
                return Err(ProcessingError::Queue(error));
            }
            Err(error) => return Err(ProcessingError::Queue(error)),
        }
        self.storage.delete_claimed(&claim)?;
        self.queue.publish(&session_id)?;
        Ok(ProcessingStep::Completed(session_id))
    }
}

fn single_camera_id(session: &ClaimedSession) -> Result<CameraId, InferenceError> {
    let [video] = session.manifest().videos.as_slice() else {
        return Err(InferenceError::terminal(
            "processor currently requires exactly one video per session",
        ));
    };
    CameraId::new(video.camera_id.clone())
        .map_err(|error| InferenceError::terminal(error.to_string()))
}

/// AI cancellation or terminal model/video failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceError {
    Cancelled,
    Terminal(String),
}

impl InferenceError {
    #[must_use]
    pub fn terminal(message: impl Into<String>) -> Self {
        Self::Terminal(message.into())
    }
}

impl fmt::Display for InferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("inference cancelled"),
            Self::Terminal(message) => write!(formatter, "inference failed: {message}"),
        }
    }
}

impl std::error::Error for InferenceError {}

/// Terminal conversion error before queue commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultEncodingError(String);

impl ResultEncodingError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ResultEncodingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "result encoding failed: {}", self.0)
    }
}

impl std::error::Error for ResultEncodingError {}

/// Infrastructure or durable consistency failure.
#[derive(Debug)]
pub enum ProcessingError {
    Storage(StorageError),
    Queue(QueueError),
}

impl fmt::Display for ProcessingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "session storage: {error}"),
            Self::Queue(error) => write!(formatter, "result queue: {error}"),
        }
    }
}

impl std::error::Error for ProcessingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Queue(error) => Some(error),
        }
    }
}

impl From<StorageError> for ProcessingError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<QueueError> for ProcessingError {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs, rc::Rc};

    use pcrt_model::{PassengerCounts, ProcessingResult, SessionId};
    use pcrt_result_queue::{ResultQueue, Timestamp};
    use pcrt_service::ShutdownToken;
    use pcrt_storage::{CaptureMetadata, CapturedVideo, ClaimedSession, SessionStorage};
    use tempfile::tempdir;

    use super::{
        InferenceBackend, InferenceError, PreparedResult, ProcessingStep, Processor, ResultEncoder,
        ResultEncodingError,
    };

    struct FakeBackend {
        calls: Rc<Cell<u32>>,
        result: Result<PassengerCounts, InferenceError>,
    }

    impl InferenceBackend for FakeBackend {
        fn analyze(
            &mut self,
            _session: &ClaimedSession,
            _shutdown: &ShutdownToken,
        ) -> Result<PassengerCounts, InferenceError> {
            self.calls.set(self.calls.get() + 1);
            self.result.clone()
        }
    }

    struct FakeEncoder;

    impl ResultEncoder for FakeEncoder {
        fn encode(&self, result: &ProcessingResult) -> Result<PreparedResult, ResultEncodingError> {
            Ok(PreparedResult {
                idempotency_key: format!("pcrt-result:{}", result.session_id.as_str()),
                payload_json: format!(
                    "{{\"camera_id\":\"{}\",\"captured_at_ms\":{},\"entered\":{},\"exited\":{}}}",
                    result.camera_id.as_str(),
                    result.captured_at_ms,
                    result.counts.entered,
                    result.counts.exited
                ),
            })
        }
    }

    #[test]
    fn paused_processor_does_not_claim_ready_session() {
        let fixture = Fixture::new();
        let id = fixture.ready_session("1", 100);
        let calls = Rc::new(Cell::new(0));
        let mut processor = fixture.processor(calls.clone(), Ok(counts()));

        assert_eq!(
            processor.process_one(false, 200).unwrap(),
            ProcessingStep::Paused
        );
        assert_eq!(calls.get(), 0);
        assert!(fixture.root.path().join("ready").join(id.as_str()).exists());
    }

    #[test]
    fn successful_processing_deletes_video_and_publishes_result() {
        let fixture = Fixture::new();
        let id = fixture.ready_session("1", 100);
        let calls = Rc::new(Cell::new(0));
        let mut processor = fixture.processor(calls.clone(), Ok(counts()));

        assert_eq!(
            processor.process_one(true, 200).unwrap(),
            ProcessingStep::Completed(id.clone())
        );

        assert_eq!(calls.get(), 1);
        assert!(!fixture.root.path().join("ready").join(id.as_str()).exists());
        assert!(
            !fixture
                .root
                .path()
                .join("claimed")
                .join(id.as_str())
                .exists()
        );
        let entry = ResultQueue::open(fixture.queue_path())
            .unwrap()
            .next_due(Timestamp::from_unix_millis(200))
            .unwrap()
            .unwrap();
        assert_eq!(entry.session_id, id);
        assert!(entry.payload_json.contains("\"entered\":3"));
    }

    #[test]
    fn terminal_inference_failure_preserves_session_in_failed() {
        let fixture = Fixture::new();
        let id = fixture.ready_session("1", 100);
        let calls = Rc::new(Cell::new(0));
        let mut processor = fixture.processor(
            calls.clone(),
            Err(InferenceError::terminal("unsupported model output")),
        );

        assert_eq!(
            processor.process_one(true, 200).unwrap(),
            ProcessingStep::Failed(id.clone())
        );
        assert_eq!(calls.get(), 1);
        assert!(
            fixture
                .root
                .path()
                .join("failed")
                .join(id.as_str())
                .exists()
        );
        assert!(
            !ResultQueue::open(fixture.queue_path())
                .unwrap()
                .contains_session(&id)
                .unwrap()
        );
    }

    #[test]
    fn startup_recovery_completes_prepared_result_without_inference() {
        let fixture = Fixture::new();
        let id = fixture.ready_session("1", 100);
        let storage = SessionStorage::open(fixture.root.path()).unwrap();
        let _claim = storage.claim_next_ready(150).unwrap().unwrap();
        let queue = ResultQueue::open(fixture.queue_path()).unwrap();
        queue
            .insert(
                &id,
                "pcrt-result:test",
                "{\"entered\":3,\"exited\":1}",
                Timestamp::from_unix_millis(150),
            )
            .unwrap();
        drop(queue);
        let calls = Rc::new(Cell::new(0));
        let processor = fixture.processor(calls.clone(), Ok(counts()));

        let report = processor.recover(200).unwrap();

        assert_eq!(report.completed_prepared_results, 1);
        assert_eq!(calls.get(), 0);
        assert!(
            !fixture
                .root
                .path()
                .join("claimed")
                .join(id.as_str())
                .exists()
        );
        assert!(
            ResultQueue::open(fixture.queue_path())
                .unwrap()
                .next_due(Timestamp::from_unix_millis(200))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn startup_recovery_publishes_prepared_result_after_video_was_deleted() {
        let fixture = Fixture::new();
        let id = fixture.ready_session("1", 100);
        let storage = SessionStorage::open(fixture.root.path()).unwrap();
        let claim = storage.claim_next_ready(150).unwrap().unwrap();
        let queue = ResultQueue::open(fixture.queue_path()).unwrap();
        queue
            .insert(
                &id,
                "pcrt-result:test",
                "{\"entered\":3,\"exited\":1}",
                Timestamp::from_unix_millis(150),
            )
            .unwrap();
        storage.delete_claimed(&claim).unwrap();
        drop(queue);
        let calls = Rc::new(Cell::new(0));
        let processor = fixture.processor(calls.clone(), Ok(counts()));

        assert_eq!(
            processor.recover(200).unwrap().completed_prepared_results,
            1
        );
        assert_eq!(
            processor.recover(201).unwrap().completed_prepared_results,
            0
        );
        assert_eq!(calls.get(), 0);
        assert!(
            ResultQueue::open(fixture.queue_path())
                .unwrap()
                .next_due(Timestamp::from_unix_millis(201))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn existing_queue_result_is_never_inferred_again() {
        let fixture = Fixture::new();
        let id = fixture.ready_session("1", 100);
        ResultQueue::open(fixture.queue_path())
            .unwrap()
            .insert(
                &id,
                "pcrt-result:test",
                "{\"entered\":3,\"exited\":1}",
                Timestamp::from_unix_millis(150),
            )
            .unwrap();
        let calls = Rc::new(Cell::new(0));
        let mut processor = fixture.processor(calls.clone(), Ok(counts()));

        assert_eq!(
            processor.process_one(true, 200).unwrap(),
            ProcessingStep::Reconciled(id.clone())
        );
        assert_eq!(calls.get(), 0);
        assert!(
            !fixture
                .root
                .path()
                .join("claimed")
                .join(id.as_str())
                .exists()
        );
    }

    fn counts() -> PassengerCounts {
        PassengerCounts {
            entered: 3,
            exited: 1,
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempdir().unwrap(),
            }
        }

        fn queue_path(&self) -> std::path::PathBuf {
            self.root.path().join("results.sqlite")
        }

        fn ready_session(&self, camera_id: &str, started_at_ms: i64) -> SessionId {
            let storage = SessionStorage::open(self.root.path()).unwrap();
            let id = SessionStorage::session_id_for_capture(camera_id, started_at_ms).unwrap();
            let capture = storage
                .begin_capture(
                    &id,
                    CaptureMetadata::new(format!("source-{camera_id}"), started_at_ms),
                )
                .unwrap();
            let video_name = format!("cam{camera_id}.mkv");
            fs::write(capture.video_path(&video_name).unwrap(), b"video").unwrap();
            storage
                .finalize_capture(
                    capture,
                    started_at_ms + 10,
                    &[
                        CapturedVideo::new(camera_id, video_name, "libx264", "mkv", 10, 256, 256)
                            .unwrap(),
                    ],
                )
                .unwrap();
            id
        }

        fn processor(
            &self,
            calls: Rc<Cell<u32>>,
            result: Result<PassengerCounts, InferenceError>,
        ) -> Processor<FakeBackend, FakeEncoder> {
            Processor::new(
                SessionStorage::open(self.root.path()).unwrap(),
                ResultQueue::open(self.queue_path()).unwrap(),
                FakeBackend { calls, result },
                FakeEncoder,
                ShutdownToken::default(),
            )
        }
    }
}
