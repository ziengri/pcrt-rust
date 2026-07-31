#![forbid(unsafe_code)]
//! Durable filesystem lifecycle для видеосессий.
//!
//! Сессия всегда является одним каталогом. Этот crate не зависит от `SQLite` и не
//! знает о доставке результата: processor координирует его с result queue.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use pcrt_model::{SESSION_MANIFEST_VERSION, SessionId, SessionState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CAPTURING_DIRECTORY: &str = "capturing";
const READY_DIRECTORY: &str = "ready";
const CLAIMED_DIRECTORY: &str = "claimed";
const FAILED_DIRECTORY: &str = "failed";
const MANIFEST_FILE: &str = "manifest.json";
const FAILURE_FILE: &str = "failure.txt";
const CLAIM_LOCK_FILE: &str = ".claim";

/// Метаданные, известные в момент начала capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureMetadata {
    pub source_id: String,
    pub started_at_ms: i64,
}

impl CaptureMetadata {
    /// Создаёт metadata capture.
    #[must_use]
    pub fn new(source_id: impl Into<String>, started_at_ms: i64) -> Self {
        Self {
            source_id: source_id.into(),
            started_at_ms,
        }
    }
}

/// Параметры видео, подтверждённые recorder после успешного завершения ffmpeg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedVideo {
    pub camera_id: String,
    pub path: String,
    pub codec: String,
    pub format: String,
    pub frame_count: u64,
    pub width: u32,
    pub height: u32,
}

impl CapturedVideo {
    /// Создаёт metadata одного завершённого видео.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если video metadata не пригодны для durable manifest.
    pub fn new(
        camera_id: impl Into<String>,
        path: impl Into<String>,
        codec: impl Into<String>,
        format: impl Into<String>,
        frame_count: u64,
        width: u32,
        height: u32,
    ) -> Result<Self, StorageError> {
        let video = Self {
            camera_id: camera_id.into(),
            path: path.into(),
            codec: codec.into(),
            format: format.into(),
            frame_count,
            width,
            height,
        };
        validate_captured_video(&video)?;
        Ok(video)
    }
}

/// Параметры и контрольная сумма неизменяемого видео в готовой сессии.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionVideo {
    pub camera_id: String,
    pub path: String,
    pub codec: String,
    pub format: String,
    pub frame_count: u64,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    pub sha256: String,
}

/// Зафиксированный переход состояния в manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTransition {
    pub state: SessionState,
    pub at_ms: i64,
    pub reason: Option<String>,
}

/// Прочитанное и проверенное представление `manifest.json`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionManifest {
    pub schema_version: u16,
    pub session_id: SessionId,
    pub source_id: String,
    pub state: SessionState,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub videos: Vec<SessionVideo>,
    pub transitions: Vec<SessionTransition>,
    pub failure: Option<String>,
}

/// Незавершённый каталог capture.
#[derive(Debug)]
pub struct CaptureSession {
    session_id: SessionId,
    directory: PathBuf,
}

impl CaptureSession {
    /// Идентификатор capture-сессии.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Каталог, в который recorder может писать видео.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Возвращает безопасный путь для одного video filename.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для абсолютного пути, вложенных каталогов, `.` и `..`.
    pub fn video_path(&self, filename: &str) -> Result<PathBuf, StorageError> {
        validate_video_path(filename)?;
        Ok(self.directory.join(filename))
    }
}

/// Готовая к обработке сессия.
#[derive(Clone, Debug)]
pub struct ReadySession {
    session_id: SessionId,
    directory: PathBuf,
    manifest: SessionManifest,
}

impl ReadySession {
    /// Идентификатор сессии.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Каталог immutable видео.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Проверенный manifest сессии.
    #[must_use]
    pub fn manifest(&self) -> &SessionManifest {
        &self.manifest
    }
}

/// Сессия, эксклюзивно забранная processor-ом.
#[derive(Debug)]
pub struct ClaimedSession {
    session_id: SessionId,
    directory: PathBuf,
    manifest: SessionManifest,
}

impl ClaimedSession {
    /// Идентификатор сессии.
    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Каталог видео, принадлежащий processor-у.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Проверенный manifest сессии.
    #[must_use]
    pub fn manifest(&self) -> &SessionManifest {
        &self.manifest
    }
}

/// Результат восстановления session storage.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    pub promoted_captures: u32,
    pub released_claims: u32,
    pub failed_sessions: u32,
}

/// Файловое хранилище сессий одного устройства.
#[derive(Clone, Debug)]
pub struct SessionStorage {
    root: PathBuf,
}

impl SessionStorage {
    /// Открывает state root и создаёт lifecycle-каталоги.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку доступа к файловой системе или если root не каталог.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .map_err(|error| StorageError::io("create storage root", error))?;
        ensure_directory(&root, "storage root")?;
        for directory in [
            CAPTURING_DIRECTORY,
            READY_DIRECTORY,
            CLAIMED_DIRECTORY,
            FAILED_DIRECTORY,
        ] {
            let path = root.join(directory);
            fs::create_dir_all(&path)
                .map_err(|error| StorageError::io("create storage state directory", error))?;
            ensure_directory(&path, "storage state directory")?;
        }
        sync_directory(&root)?;
        Ok(Self { root })
    }

    /// Создаёт читаемый идентификатор capture: `cam-{camera_id}-{unix_ms}`.
    ///
    /// `camera_id` входит в ID как один безопасный path component. При совпадении
    /// camera ID и timestamp `begin_capture` отклоняет collision, не перезаписывая
    /// существующую сессию.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для пустого/небезопасного camera ID или отрицательного
    /// Unix timestamp.
    pub fn session_id_for_capture(
        camera_id: &str,
        unix_ms: i64,
    ) -> Result<SessionId, StorageError> {
        if unix_ms < 0 {
            return Err(StorageError::NegativeCaptureTimestamp);
        }
        validate_camera_id(camera_id)?;
        SessionId::new(format!("cam-{camera_id}-{unix_ms}"))
            .map_err(|_| StorageError::InvalidCameraId)
    }

    /// Создаёт новый exclusive `capturing/<session_id>.tmp` каталог.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку при collision, некорректном source ID или записи
    /// начального manifest.
    pub fn begin_capture(
        &self,
        session_id: &SessionId,
        metadata: CaptureMetadata,
    ) -> Result<CaptureSession, StorageError> {
        validate_source_id(&metadata.source_id)?;
        let directory = self.path_for(StorageState::Capturing, session_id);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(StorageError::SessionAlreadyExists(session_id.clone()));
            }
            Err(error) => return Err(StorageError::io("create capture directory", error)),
        }

        let manifest = RawManifest::capturing(session_id, metadata);
        if let Err(error) = write_manifest(&directory, &manifest) {
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }
        sync_directory(&self.state_directory(StorageState::Capturing))?;
        Ok(CaptureSession {
            session_id: session_id.clone(),
            directory,
        })
    }

    /// Завершает capture и atomically публикует один каталог в `ready`.
    ///
    /// Recorder должен завершить ffmpeg до вызова этого метода. Storage fsync-ит
    /// каждое объявленное видео, вычисляет его SHA-256, записывает final manifest и только
    /// затем делает rename каталога.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для пустого списка video metadata, временных/вложенных
    /// файлов, symlink, collision или ошибки I/O.
    pub fn finalize_capture(
        &self,
        capture: CaptureSession,
        finished_at_ms: i64,
        videos: &[CapturedVideo],
    ) -> Result<ReadySession, StorageError> {
        self.ensure_handle_path(
            StorageState::Capturing,
            &capture.session_id,
            &capture.directory,
        )?;
        let mut manifest = read_manifest(&capture.directory, &capture.session_id)?;
        if manifest.state != ManifestState::Capturing {
            return Err(StorageError::UnexpectedManifestState {
                session_id: capture.session_id,
                expected: SessionState::Capturing,
                actual: manifest.state.as_public(),
            });
        }
        if finished_at_ms < manifest.started_at_ms {
            return Err(StorageError::InvalidTimestampOrder);
        }

        manifest.videos = collect_videos(&capture.directory, videos)?;
        manifest.finished_at_ms = Some(finished_at_ms);
        manifest.transition(ManifestState::Ready, finished_at_ms, None);

        let ready_path = self.path_for(StorageState::Ready, &capture.session_id);
        ensure_absent(&ready_path, &capture.session_id)?;
        write_manifest(&capture.directory, &manifest)?;
        move_session_directory(
            &capture.directory,
            &ready_path,
            &self.state_directory(StorageState::Capturing),
            &self.state_directory(StorageState::Ready),
            &capture.session_id,
        )?;

        Ok(ReadySession {
            session_id: capture.session_id.clone(),
            directory: ready_path,
            manifest: manifest.to_public()?,
        })
    }

    /// Permanently removes an intentionally discarded in-progress capture.
    ///
    /// The opaque [`CaptureSession`] handle is validated before removal, so this
    /// operation cannot delete a session from another storage state.
    ///
    /// # Errors
    ///
    /// Returns an error when the capture handle is stale or filesystem cleanup fails.
    pub fn discard_capture(&self, capture: &CaptureSession) -> Result<(), StorageError> {
        self.ensure_handle_path(
            StorageState::Capturing,
            &capture.session_id,
            &capture.directory,
        )?;
        fs::remove_dir_all(&capture.directory)
            .map_err(|error| StorageError::io("remove discarded capture", error))?;
        sync_directory(&self.state_directory(StorageState::Capturing))
    }

    /// Atomically забирает самую старую проверенную ready-сессию.
    ///
    /// Повреждённые ready-сессии переносятся в `failed`; корректные сортируются
    /// по `started_at_ms`, затем `session_id`.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку файловой системы или если claim collision не позволяет
    /// безопасно выбрать следующую сессию.
    pub fn claim_next_ready(
        &self,
        claimed_at_ms: i64,
    ) -> Result<Option<ClaimedSession>, StorageError> {
        let mut candidates = Vec::new();
        for session_id in self.session_ids_in(StorageState::Ready)? {
            let directory = self.path_for(StorageState::Ready, &session_id);
            match Self::verify_completed(&directory, &session_id) {
                Ok(manifest) if manifest.state == ManifestState::Ready => {
                    candidates.push((manifest.started_at_ms, session_id, manifest));
                }
                Ok(manifest) => {
                    self.fail_session(
                        StorageState::Ready,
                        &session_id,
                        claimed_at_ms,
                        &format!(
                            "ready directory has {} manifest state",
                            manifest.state.as_str()
                        ),
                    )?;
                }
                Err(error) => {
                    self.fail_session(
                        StorageState::Ready,
                        &session_id,
                        claimed_at_ms,
                        &format!("ready session validation failed: {error}"),
                    )?;
                }
            }
        }
        candidates.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.as_str().cmp(right.1.as_str()))
        });

        for (_, session_id, mut manifest) in candidates {
            let ready_path = self.path_for(StorageState::Ready, &session_id);
            if !try_create_claim_lock(&ready_path)? {
                continue;
            }
            let claimed_path = self.path_for(StorageState::Claimed, &session_id);
            if let Err(error) = move_session_directory(
                &ready_path,
                &claimed_path,
                &self.state_directory(StorageState::Ready),
                &self.state_directory(StorageState::Claimed),
                &session_id,
            ) {
                let _ = remove_claim_lock(&ready_path);
                return Err(error);
            }

            manifest.transition(ManifestState::Claimed, claimed_at_ms, None);
            write_manifest(&claimed_path, &manifest)?;
            remove_claim_lock(&claimed_path)?;
            return Ok(Some(ClaimedSession {
                session_id: session_id.clone(),
                directory: claimed_path,
                manifest: manifest.to_public()?,
            }));
        }
        Ok(None)
    }

    /// Возвращает claim в `ready` до начала необратимой обработки.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если handle не принадлежит этому storage или manifest
    /// не находится в состоянии `claimed`.
    pub fn release_claim(
        &self,
        claim: ClaimedSession,
        released_at_ms: i64,
        reason: &str,
    ) -> Result<ReadySession, StorageError> {
        self.ensure_handle_path(StorageState::Claimed, &claim.session_id, &claim.directory)?;
        let mut manifest = read_manifest(&claim.directory, &claim.session_id)?;
        if manifest.state != ManifestState::Claimed {
            return Err(StorageError::UnexpectedManifestState {
                session_id: claim.session_id,
                expected: SessionState::Claimed,
                actual: manifest.state.as_public(),
            });
        }
        manifest.transition(
            ManifestState::Ready,
            released_at_ms,
            non_empty_reason(reason),
        );
        write_manifest(&claim.directory, &manifest)?;

        let ready_path = self.path_for(StorageState::Ready, &claim.session_id);
        move_session_directory(
            &claim.directory,
            &ready_path,
            &self.state_directory(StorageState::Claimed),
            &self.state_directory(StorageState::Ready),
            &claim.session_id,
        )?;
        Ok(ReadySession {
            session_id: claim.session_id,
            directory: ready_path,
            manifest: manifest.to_public()?,
        })
    }

    /// Переводит claimed-сессию в `failed`, сохраняя артефакты и причину.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку доступа к файловой системе или collision в `failed`.
    pub fn mark_claim_failed(
        &self,
        claim: &ClaimedSession,
        failed_at_ms: i64,
        reason: &str,
    ) -> Result<(), StorageError> {
        self.ensure_handle_path(StorageState::Claimed, &claim.session_id, &claim.directory)?;
        self.fail_session(
            StorageState::Claimed,
            &claim.session_id,
            failed_at_ms,
            reason,
        )
    }

    /// Удаляет claimed-каталог после durable `result_queue.insert(prepared)`.
    ///
    /// Storage намеренно не знает, была ли запись результата создана. Processor
    /// обязан вызвать этот метод только после её commit.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку доступа к каталогу claimed-сессии.
    pub fn delete_claimed(&self, claim: &ClaimedSession) -> Result<(), StorageError> {
        self.ensure_handle_path(StorageState::Claimed, &claim.session_id, &claim.directory)?;
        fs::remove_dir_all(&claim.directory)
            .map_err(|error| StorageError::io("delete claimed session directory", error))?;
        sync_directory(&self.state_directory(StorageState::Claimed))
    }

    /// Восстанавливает storage после остановки recorder или processor.
    ///
    /// Незавершённый `capturing` переносится в `failed` без удаления файлов.
    /// Capture с уже готовым manifest, оставшийся перед rename, публикуется в
    /// `ready`. Все stale `claimed` возвращаются в `ready`, если videos
    /// проходят проверку; повреждённые сессии переносятся в `failed`.
    ///
    /// Этот метод вызывается до запуска recorder и processor, когда нет активных
    /// владельцев session-directory.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку I/O или collision, который не позволяет безопасно
    /// завершить recovery.
    pub fn recover(&self, recovered_at_ms: i64) -> Result<RecoveryReport, StorageError> {
        let mut report = RecoveryReport::default();

        for session_id in self.session_ids_in(StorageState::Capturing)? {
            let directory = self.path_for(StorageState::Capturing, &session_id);
            remove_recovery_markers(&directory)?;
            match Self::verify_completed(&directory, &session_id) {
                Ok(manifest) if manifest.state == ManifestState::Ready => {
                    let ready_path = self.path_for(StorageState::Ready, &session_id);
                    move_session_directory(
                        &directory,
                        &ready_path,
                        &self.state_directory(StorageState::Capturing),
                        &self.state_directory(StorageState::Ready),
                        &session_id,
                    )?;
                    report.promoted_captures = report.promoted_captures.saturating_add(1);
                }
                _ => {
                    self.fail_session(
                        StorageState::Capturing,
                        &session_id,
                        recovered_at_ms,
                        "capture interrupted before durable finalization",
                    )?;
                    report.failed_sessions = report.failed_sessions.saturating_add(1);
                }
            }
        }

        for session_id in self.session_ids_in(StorageState::Ready)? {
            let directory = self.path_for(StorageState::Ready, &session_id);
            remove_recovery_markers(&directory)?;
            if !matches!(
                Self::verify_completed(&directory, &session_id),
                Ok(manifest) if manifest.state == ManifestState::Ready
            ) {
                self.fail_session(
                    StorageState::Ready,
                    &session_id,
                    recovered_at_ms,
                    "ready session failed recovery validation",
                )?;
                report.failed_sessions = report.failed_sessions.saturating_add(1);
            }
        }

        for session_id in self.session_ids_in(StorageState::Claimed)? {
            let directory = self.path_for(StorageState::Claimed, &session_id);
            remove_recovery_markers(&directory)?;
            let mut manifest = match Self::verify_completed(&directory, &session_id) {
                Ok(manifest)
                    if matches!(
                        manifest.state,
                        ManifestState::Ready | ManifestState::Claimed
                    ) =>
                {
                    manifest
                }
                _ => {
                    self.fail_session(
                        StorageState::Claimed,
                        &session_id,
                        recovered_at_ms,
                        "claimed session failed recovery validation",
                    )?;
                    report.failed_sessions = report.failed_sessions.saturating_add(1);
                    continue;
                }
            };
            manifest.transition(
                ManifestState::Ready,
                recovered_at_ms,
                Some("recovery released abandoned claim".to_owned()),
            );
            write_manifest(&directory, &manifest)?;
            let ready_path = self.path_for(StorageState::Ready, &session_id);
            move_session_directory(
                &directory,
                &ready_path,
                &self.state_directory(StorageState::Claimed),
                &self.state_directory(StorageState::Ready),
                &session_id,
            )?;
            report.released_claims = report.released_claims.saturating_add(1);
        }

        Ok(report)
    }

    /// Возвращает текущее Unix-время в миллисекундах для transitions storage.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если системные часы находятся до Unix epoch.
    pub fn unix_millis_now() -> Result<i64, StorageError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| StorageError::ClockBeforeEpoch(error.to_string()))?;
        i64::try_from(duration.as_millis()).map_err(|_| StorageError::ClockOutOfRange)
    }

    fn verify_completed(
        directory: &Path,
        session_id: &SessionId,
    ) -> Result<RawManifest, StorageError> {
        let manifest = read_manifest(directory, session_id)?;
        if manifest.finished_at_ms.is_none() || manifest.videos.is_empty() {
            return Err(StorageError::IncompleteManifest(session_id.clone()));
        }
        verify_videos(directory, &manifest.videos)?;
        Ok(manifest)
    }

    fn fail_session(
        &self,
        from: StorageState,
        session_id: &SessionId,
        failed_at_ms: i64,
        reason: &str,
    ) -> Result<(), StorageError> {
        let source = self.path_for(from, session_id);
        let failed = self.path_for(StorageState::Failed, session_id);
        ensure_absent(&failed, session_id)?;

        match read_manifest(&source, session_id) {
            Ok(mut manifest) => {
                manifest.failure = Some(reason.to_owned());
                manifest.transition(
                    ManifestState::Failed,
                    failed_at_ms,
                    non_empty_reason(reason),
                );
                write_manifest(&source, &manifest)?;
            }
            Err(_) => write_failure_file(&source, reason)?,
        }
        move_session_directory(
            &source,
            &failed,
            &self.state_directory(from),
            &self.state_directory(StorageState::Failed),
            session_id,
        )
    }

    fn session_ids_in(&self, state: StorageState) -> Result<Vec<SessionId>, StorageError> {
        let mut session_ids = Vec::new();
        let directory = self.state_directory(state);
        for entry in fs::read_dir(&directory)
            .map_err(|error| StorageError::io("read storage state directory", error))?
        {
            let entry =
                entry.map_err(|error| StorageError::io("read storage directory entry", error))?;
            let file_type = entry
                .file_type()
                .map_err(|error| StorageError::io("read storage entry type", error))?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let id = match state {
                StorageState::Capturing => name.strip_suffix(".tmp"),
                StorageState::Ready | StorageState::Claimed | StorageState::Failed => {
                    Some(name.as_str())
                }
            };
            if let Some(id) = id.and_then(|value| SessionId::new(value.to_owned()).ok()) {
                session_ids.push(id);
            }
        }
        session_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(session_ids)
    }

    fn state_directory(&self, state: StorageState) -> PathBuf {
        self.root.join(state.directory_name())
    }

    fn path_for(&self, state: StorageState, session_id: &SessionId) -> PathBuf {
        let name = match state {
            StorageState::Capturing => format!("{}.tmp", session_id.as_str()),
            StorageState::Ready | StorageState::Claimed | StorageState::Failed => {
                session_id.as_str().to_owned()
            }
        };
        self.state_directory(state).join(name)
    }

    fn ensure_handle_path(
        &self,
        state: StorageState,
        session_id: &SessionId,
        path: &Path,
    ) -> Result<(), StorageError> {
        if path != self.path_for(state, session_id) {
            return Err(StorageError::ForeignSessionHandle(session_id.clone()));
        }
        ensure_directory(path, "session directory")
    }
}

/// Ошибки durable session storage.
#[derive(Debug)]
pub enum StorageError {
    Io {
        context: &'static str,
        source: io::Error,
    },
    SessionAlreadyExists(SessionId),
    SessionCollision(SessionId),
    ForeignSessionHandle(SessionId),
    InvalidSourceId,
    InvalidCameraId,
    NegativeCaptureTimestamp,
    InvalidVideoPath(String),
    InvalidVideoMetadata(String),
    EmptyVideoList,
    InvalidTimestampOrder,
    InvalidManifest {
        session_id: SessionId,
        message: String,
    },
    IncompleteManifest(SessionId),
    UnexpectedManifestState {
        session_id: SessionId,
        expected: SessionState,
        actual: SessionState,
    },
    VideoIntegrity {
        path: PathBuf,
        message: String,
    },
    ClockBeforeEpoch(String),
    ClockOutOfRange,
}

impl StorageError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }
}

impl core::fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io { context, source } => write!(formatter, "cannot {context}: {source}"),
            Self::SessionAlreadyExists(session_id) => {
                write!(
                    formatter,
                    "capture session already exists: {}",
                    session_id.as_str()
                )
            }
            Self::SessionCollision(session_id) => {
                write!(
                    formatter,
                    "session state target already exists: {}",
                    session_id.as_str()
                )
            }
            Self::ForeignSessionHandle(session_id) => {
                write!(
                    formatter,
                    "session handle does not belong to this storage: {}",
                    session_id.as_str()
                )
            }
            Self::InvalidSourceId => {
                formatter.write_str("capture source ID must not be empty or contain a null byte")
            }
            Self::InvalidCameraId => formatter
                .write_str("camera ID must be a non-empty safe session identifier component"),
            Self::NegativeCaptureTimestamp => {
                formatter.write_str("capture timestamp must not precede Unix epoch")
            }
            Self::InvalidVideoPath(name) => {
                write!(formatter, "invalid session video path: {name:?}")
            }
            Self::InvalidVideoMetadata(message) => {
                write!(formatter, "invalid session video metadata: {message}")
            }
            Self::EmptyVideoList => formatter.write_str("capture must contain at least one video"),
            Self::InvalidTimestampOrder => {
                formatter.write_str("capture finish time must not precede its start time")
            }
            Self::InvalidManifest {
                session_id,
                message,
            } => {
                write!(
                    formatter,
                    "invalid session manifest for {}: {message}",
                    session_id.as_str()
                )
            }
            Self::IncompleteManifest(session_id) => {
                write!(
                    formatter,
                    "session manifest is incomplete: {}",
                    session_id.as_str()
                )
            }
            Self::UnexpectedManifestState {
                session_id,
                expected,
                actual,
            } => write!(
                formatter,
                "unexpected manifest state for {}: expected {}, found {}",
                session_id.as_str(),
                display_session_state(*expected),
                display_session_state(*actual)
            ),
            Self::VideoIntegrity { path, message } => {
                write!(
                    formatter,
                    "video integrity check failed for {}: {message}",
                    path.display()
                )
            }
            Self::ClockBeforeEpoch(message) => {
                write!(formatter, "system clock is before Unix epoch: {message}")
            }
            Self::ClockOutOfRange => {
                formatter.write_str("system clock is outside supported millisecond range")
            }
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::SessionAlreadyExists(_)
            | Self::SessionCollision(_)
            | Self::ForeignSessionHandle(_)
            | Self::InvalidSourceId
            | Self::InvalidCameraId
            | Self::NegativeCaptureTimestamp
            | Self::InvalidVideoPath(_)
            | Self::InvalidVideoMetadata(_)
            | Self::EmptyVideoList
            | Self::InvalidTimestampOrder
            | Self::InvalidManifest { .. }
            | Self::IncompleteManifest(_)
            | Self::UnexpectedManifestState { .. }
            | Self::VideoIntegrity { .. }
            | Self::ClockBeforeEpoch(_)
            | Self::ClockOutOfRange => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum StorageState {
    Capturing,
    Ready,
    Claimed,
    Failed,
}

impl StorageState {
    const fn directory_name(self) -> &'static str {
        match self {
            Self::Capturing => CAPTURING_DIRECTORY,
            Self::Ready => READY_DIRECTORY,
            Self::Claimed => CLAIMED_DIRECTORY,
            Self::Failed => FAILED_DIRECTORY,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Capturing,
    Ready,
    Claimed,
    Failed,
}

impl ManifestState {
    const fn as_public(self) -> SessionState {
        match self {
            Self::Capturing => SessionState::Capturing,
            Self::Ready => SessionState::Ready,
            Self::Claimed => SessionState::Claimed,
            Self::Failed => SessionState::Failed,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Capturing => "capturing",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema_version: u16,
    session_id: String,
    source_id: String,
    state: ManifestState,
    started_at_ms: i64,
    finished_at_ms: Option<i64>,
    videos: Vec<RawVideo>,
    transitions: Vec<RawTransition>,
    failure: Option<String>,
}

impl RawManifest {
    fn capturing(session_id: &SessionId, metadata: CaptureMetadata) -> Self {
        Self {
            schema_version: SESSION_MANIFEST_VERSION,
            session_id: session_id.as_str().to_owned(),
            source_id: metadata.source_id,
            state: ManifestState::Capturing,
            started_at_ms: metadata.started_at_ms,
            finished_at_ms: None,
            videos: Vec::new(),
            transitions: vec![RawTransition {
                state: ManifestState::Capturing,
                at_ms: metadata.started_at_ms,
                reason: None,
            }],
            failure: None,
        }
    }

    fn transition(&mut self, state: ManifestState, at_ms: i64, reason: Option<String>) {
        self.state = state;
        self.transitions.push(RawTransition {
            state,
            at_ms,
            reason,
        });
    }

    fn to_public(&self) -> Result<SessionManifest, StorageError> {
        let session_id =
            SessionId::new(self.session_id.clone()).map_err(|_| StorageError::InvalidManifest {
                session_id: fallback_session_id(&self.session_id),
                message: "session_id is not a safe path component".to_owned(),
            })?;
        Ok(SessionManifest {
            schema_version: self.schema_version,
            session_id,
            source_id: self.source_id.clone(),
            state: self.state.as_public(),
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
            videos: self
                .videos
                .iter()
                .map(|video| SessionVideo {
                    camera_id: video.camera_id.clone(),
                    path: video.path.clone(),
                    codec: video.codec.clone(),
                    format: video.format.clone(),
                    frame_count: video.frame_count,
                    width: video.width,
                    height: video.height,
                    size_bytes: video.size_bytes,
                    sha256: video.sha256.clone(),
                })
                .collect(),
            transitions: self
                .transitions
                .iter()
                .map(|transition| SessionTransition {
                    state: transition.state.as_public(),
                    at_ms: transition.at_ms,
                    reason: transition.reason.clone(),
                })
                .collect(),
            failure: self.failure.clone(),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawVideo {
    camera_id: String,
    path: String,
    codec: String,
    format: String,
    frame_count: u64,
    width: u32,
    height: u32,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawTransition {
    state: ManifestState,
    at_ms: i64,
    reason: Option<String>,
}

fn read_manifest(
    directory: &Path,
    expected_session_id: &SessionId,
) -> Result<RawManifest, StorageError> {
    let path = directory.join(MANIFEST_FILE);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| StorageError::io("read session manifest metadata", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: "manifest must be a regular non-symlink file".to_owned(),
        });
    }
    let file =
        File::open(&path).map_err(|error| StorageError::io("open session manifest", error))?;
    let manifest: RawManifest = serde_json::from_reader(BufReader::new(file)).map_err(|error| {
        StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: error.to_string(),
        }
    })?;
    validate_raw_manifest(&manifest, expected_session_id)?;
    Ok(manifest)
}

fn validate_raw_manifest(
    manifest: &RawManifest,
    expected_session_id: &SessionId,
) -> Result<(), StorageError> {
    if manifest.schema_version != SESSION_MANIFEST_VERSION {
        return Err(StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: format!("unsupported schema version {}", manifest.schema_version),
        });
    }
    if manifest.session_id != expected_session_id.as_str() {
        return Err(StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: "manifest session_id does not match directory name".to_owned(),
        });
    }
    validate_source_id(&manifest.source_id)?;
    if let Some(finished_at_ms) = manifest.finished_at_ms {
        if finished_at_ms < manifest.started_at_ms {
            return Err(StorageError::InvalidTimestampOrder);
        }
    }
    let Some(first_transition) = manifest.transitions.first() else {
        return Err(StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: "manifest transition history must not be empty".to_owned(),
        });
    };
    if first_transition.state != ManifestState::Capturing {
        return Err(StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: "manifest transition history must start in capturing".to_owned(),
        });
    }
    if manifest
        .transitions
        .last()
        .map(|transition| transition.state)
        != Some(manifest.state)
    {
        return Err(StorageError::InvalidManifest {
            session_id: expected_session_id.clone(),
            message: "manifest state must match its final transition".to_owned(),
        });
    }
    for transitions in manifest.transitions.windows(2) {
        let previous = &transitions[0];
        let next = &transitions[1];
        if next.at_ms < previous.at_ms {
            return Err(StorageError::InvalidManifest {
                session_id: expected_session_id.clone(),
                message: "manifest transitions must be time ordered".to_owned(),
            });
        }
        if !previous
            .state
            .as_public()
            .can_transition_to(next.state.as_public())
        {
            return Err(StorageError::InvalidManifest {
                session_id: expected_session_id.clone(),
                message: "manifest contains an invalid state transition".to_owned(),
            });
        }
    }
    let mut paths = BTreeSet::new();
    let mut camera_ids = BTreeSet::new();
    for video in &manifest.videos {
        validate_raw_video(video)?;
        if video.sha256.len() != 64 || !video.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StorageError::InvalidManifest {
                session_id: expected_session_id.clone(),
                message: format!("invalid SHA-256 for video {:?}", video.path),
            });
        }
        if !paths.insert(&video.path) {
            return Err(StorageError::InvalidManifest {
                session_id: expected_session_id.clone(),
                message: format!("duplicate video path {:?}", video.path),
            });
        }
        if !camera_ids.insert(&video.camera_id) {
            return Err(StorageError::InvalidManifest {
                session_id: expected_session_id.clone(),
                message: format!("duplicate video camera ID {:?}", video.camera_id),
            });
        }
    }
    Ok(())
}

fn write_manifest(directory: &Path, manifest: &RawManifest) -> Result<(), StorageError> {
    let bytes =
        serde_json::to_vec_pretty(manifest).map_err(|error| StorageError::InvalidManifest {
            session_id: fallback_session_id(&manifest.session_id),
            message: error.to_string(),
        })?;
    atomic_write(directory, MANIFEST_FILE, &bytes)
}

fn write_failure_file(directory: &Path, reason: &str) -> Result<(), StorageError> {
    atomic_write(directory, FAILURE_FILE, reason.as_bytes())
}

fn atomic_write(directory: &Path, filename: &str, bytes: &[u8]) -> Result<(), StorageError> {
    let target = directory.join(filename);
    let temporary = directory.join(format!(".{filename}.tmp"));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| StorageError::io("open temporary session file", error))?;
        file.write_all(bytes)
            .map_err(|error| StorageError::io("write temporary session file", error))?;
        file.sync_all()
            .map_err(|error| StorageError::io("fsync temporary session file", error))?;
    }
    fs::rename(&temporary, &target)
        .map_err(|error| StorageError::io("replace session file atomically", error))?;
    sync_directory(directory)
}

fn collect_videos(
    directory: &Path,
    captured_videos: &[CapturedVideo],
) -> Result<Vec<RawVideo>, StorageError> {
    if captured_videos.is_empty() {
        return Err(StorageError::EmptyVideoList);
    }
    let mut declared_paths = BTreeSet::new();
    let mut camera_ids = BTreeSet::new();
    let mut videos = Vec::with_capacity(captured_videos.len());
    for video in captured_videos {
        validate_captured_video(video)?;
        if !declared_paths.insert(video.path.as_str()) {
            return Err(StorageError::InvalidVideoMetadata(format!(
                "duplicate video path {:?}",
                video.path
            )));
        }
        if !camera_ids.insert(&video.camera_id) {
            return Err(StorageError::InvalidVideoMetadata(format!(
                "duplicate video camera ID {:?}",
                video.camera_id
            )));
        }
        let path = directory.join(&video.path);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| StorageError::io("read capture video metadata", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StorageError::VideoIntegrity {
                path,
                message: "video must be a regular non-symlink file".to_owned(),
            });
        }
        if metadata.len() == 0 {
            return Err(StorageError::VideoIntegrity {
                path,
                message: "video must not be empty".to_owned(),
            });
        }
        let (size_bytes, sha256) = sync_and_hash_file(&path)?;
        videos.push(RawVideo {
            camera_id: video.camera_id.clone(),
            path: video.path.clone(),
            codec: video.codec.clone(),
            format: video.format.clone(),
            frame_count: video.frame_count,
            width: video.width,
            height: video.height,
            size_bytes,
            sha256,
        });
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| StorageError::io("read capture directory", error))?
    {
        let entry = entry.map_err(|error| StorageError::io("read capture entry", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(StorageError::InvalidVideoPath(
                entry.path().display().to_string(),
            ));
        };
        if name == MANIFEST_FILE {
            continue;
        }
        if name.starts_with('.') || is_temporary_video_name(name) {
            return Err(StorageError::InvalidVideoPath(name.to_owned()));
        }
        if !declared_paths.contains(name) {
            return Err(StorageError::VideoIntegrity {
                path: entry.path(),
                message: "capture directory contains an undeclared video".to_owned(),
            });
        }
    }
    videos.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(videos)
}

fn verify_videos(directory: &Path, videos: &[RawVideo]) -> Result<(), StorageError> {
    let expected_paths = videos
        .iter()
        .map(|video| video.path.as_str())
        .collect::<BTreeSet<_>>();
    for entry in fs::read_dir(directory)
        .map_err(|error| StorageError::io("read session directory", error))?
    {
        let entry =
            entry.map_err(|error| StorageError::io("read session directory entry", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(StorageError::VideoIntegrity {
                path: entry.path(),
                message: "session directory contains a non-Unicode entry".to_owned(),
            });
        };
        if name != MANIFEST_FILE && name != CLAIM_LOCK_FILE && !expected_paths.contains(name) {
            return Err(StorageError::VideoIntegrity {
                path: entry.path(),
                message: "session directory contains an untracked video".to_owned(),
            });
        }
    }
    for video in videos {
        let path = directory.join(&video.path);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| StorageError::io("read session video metadata", error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StorageError::VideoIntegrity {
                path,
                message: "video must be a regular non-symlink file".to_owned(),
            });
        }
        let (size_bytes, sha256) = hash_file(&path)?;
        if size_bytes != video.size_bytes {
            return Err(StorageError::VideoIntegrity {
                path,
                message: format!("expected {} bytes, found {size_bytes}", video.size_bytes),
            });
        }
        if sha256 != video.sha256 {
            return Err(StorageError::VideoIntegrity {
                path,
                message: "SHA-256 does not match manifest".to_owned(),
            });
        }
    }
    Ok(())
}

fn sync_and_hash_file(path: &Path) -> Result<(u64, String), StorageError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| StorageError::io("open session video for fsync", error))?;
    file.sync_all()
        .map_err(|error| StorageError::io("fsync session video", error))?;
    drop(file);
    hash_file(path)
}

fn hash_file(path: &Path) -> Result<(u64, String), StorageError> {
    let file = File::open(path).map_err(|error| StorageError::io("open session video", error))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut size_bytes = 0_u64;
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| StorageError::io("read session video", error))?;
        if count == 0 {
            break;
        }
        size_bytes = size_bytes.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        hasher.update(&buffer[..count]);
    }
    Ok((size_bytes, format!("{:x}", hasher.finalize())))
}

fn move_session_directory(
    source: &Path,
    target: &Path,
    source_parent: &Path,
    target_parent: &Path,
    session_id: &SessionId,
) -> Result<(), StorageError> {
    ensure_absent(target, session_id)?;
    fs::rename(source, target)
        .map_err(|error| StorageError::io("move session directory atomically", error))?;
    sync_directory(source_parent)?;
    sync_directory(target_parent)
}

fn ensure_absent(path: &Path, session_id: &SessionId) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(StorageError::SessionCollision(session_id.clone())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageError::io("inspect session state target", error)),
    }
}

fn try_create_claim_lock(directory: &Path) -> Result<bool, StorageError> {
    let lock_path = directory.join(CLAIM_LOCK_FILE);
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(file) => {
            file.sync_all()
                .map_err(|error| StorageError::io("fsync session claim lock", error))?;
            sync_directory(directory)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StorageError::io(
            "create exclusive session claim lock",
            error,
        )),
    }
}

fn remove_claim_lock(directory: &Path) -> Result<(), StorageError> {
    let lock_path = directory.join(CLAIM_LOCK_FILE);
    match fs::remove_file(&lock_path) {
        Ok(()) => sync_directory(directory),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StorageError::io("remove session claim lock", error)),
    }
}

fn remove_recovery_markers(directory: &Path) -> Result<(), StorageError> {
    for filename in [
        format!(".{MANIFEST_FILE}.tmp"),
        format!(".{FAILURE_FILE}.tmp"),
        CLAIM_LOCK_FILE.to_owned(),
    ] {
        let path = directory.join(filename);
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(directory)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(StorageError::io(
                    "remove abandoned session recovery marker",
                    error,
                ));
            }
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path, context: &'static str) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| StorageError::io(context, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StorageError::Io {
            context,
            source: io::Error::new(io::ErrorKind::InvalidInput, "path is not a directory"),
        });
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), StorageError> {
    File::open(path)
        .map_err(|error| StorageError::io("open directory for fsync", error))?
        .sync_all()
        .map_err(|error| StorageError::io("fsync directory", error))
}

fn validate_source_id(value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() || value.contains('\0') || value.len() > 128 {
        return Err(StorageError::InvalidSourceId);
    }
    Ok(())
}

fn validate_camera_id(value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty()
        || value.len() > 96
        || value.contains(['/', '\\', '\0'])
        || value == "."
        || value == ".."
    {
        return Err(StorageError::InvalidCameraId);
    }
    Ok(())
}

fn validate_captured_video(video: &CapturedVideo) -> Result<(), StorageError> {
    validate_camera_id(&video.camera_id)?;
    validate_video_path(&video.path)?;
    validate_video_value("codec", &video.codec)?;
    validate_video_value("format", &video.format)?;
    if video.frame_count == 0 {
        return Err(StorageError::InvalidVideoMetadata(
            "frame_count must be greater than zero".to_owned(),
        ));
    }
    if video.width == 0 || video.height == 0 {
        return Err(StorageError::InvalidVideoMetadata(
            "width and height must be greater than zero".to_owned(),
        ));
    }
    let extension = Path::new(&video.path)
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            StorageError::InvalidVideoMetadata("video path must have a format extension".to_owned())
        })?;
    if !extension.eq_ignore_ascii_case(&video.format) {
        return Err(StorageError::InvalidVideoMetadata(format!(
            "video format {:?} does not match path extension {:?}",
            video.format, extension
        )));
    }
    Ok(())
}

fn validate_raw_video(video: &RawVideo) -> Result<(), StorageError> {
    validate_captured_video(&CapturedVideo {
        camera_id: video.camera_id.clone(),
        path: video.path.clone(),
        codec: video.codec.clone(),
        format: video.format.clone(),
        frame_count: video.frame_count,
        width: video.width,
        height: video.height,
    })
}

fn validate_video_path(value: &str) -> Result<(), StorageError> {
    let path = Path::new(value);
    let is_single_file_name = path.file_name().and_then(|name| name.to_str()) == Some(value);
    if value.is_empty()
        || value == "."
        || value == ".."
        || value == MANIFEST_FILE
        || value == FAILURE_FILE
        || value == CLAIM_LOCK_FILE
        || value.starts_with('.')
        || is_temporary_video_name(value)
        || value.contains('\\')
        || value.contains('\0')
        || !is_single_file_name
    {
        return Err(StorageError::InvalidVideoPath(value.to_owned()));
    }
    Ok(())
}

fn validate_video_value(field: &str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(StorageError::InvalidVideoMetadata(format!(
            "{field} must contain only ASCII letters, digits, dot, underscore, or hyphen"
        )));
    }
    Ok(())
}

fn fallback_session_id(value: &str) -> SessionId {
    SessionId::new(value.to_owned())
        .unwrap_or_else(|_| SessionId::new("invalid-session").expect("constant is valid"))
}

fn non_empty_reason(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

fn is_temporary_video_name(value: &str) -> bool {
    Path::new(value)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
}

const fn display_session_state(state: SessionState) -> &'static str {
    match state {
        SessionState::Capturing => "capturing",
        SessionState::Ready => "ready",
        SessionState::Claimed => "claimed",
        SessionState::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Barrier,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use pcrt_model::{SessionId, SessionState};

    use super::{
        CaptureMetadata, CapturedVideo, RecoveryReport, SessionStorage, StorageError, StorageState,
        read_manifest,
    };

    static NEXT_STORAGE_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn finalized_capture_is_one_verified_ready_directory() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        let ready = completed_session(&storage, "session-1", 100);

        assert_eq!(ready.manifest().state, SessionState::Ready);
        assert_eq!(ready.manifest().videos.len(), 1);
        assert_eq!(ready.manifest().videos[0].camera_id, "1");
        assert_eq!(ready.manifest().videos[0].path, "camera-1.mkv");
        assert_eq!(ready.manifest().videos[0].codec, "ffv1");
        assert_eq!(ready.manifest().videos[0].format, "mkv");
        assert_eq!(ready.manifest().videos[0].frame_count, 123);
        assert_eq!(ready.manifest().videos[0].width, 256);
        assert_eq!(ready.manifest().videos[0].height, 256);
        assert_eq!(ready.manifest().videos[0].size_bytes, 5);
        assert_eq!(
            ready.manifest().videos[0].sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(!root.join("capturing/session-1.tmp").exists());
        assert!(root.join("ready/session-1/manifest.json").is_file());
        remove_root(&root);
    }

    #[test]
    fn claim_is_oldest_first_and_exclusive_between_processors() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        completed_session(&storage, "session-new", 200);
        completed_session(&storage, "session-old", 100);

        let first = storage.claim_next_ready(300).unwrap().unwrap();
        assert_eq!(first.session_id().as_str(), "session-old");
        storage.mark_claim_failed(&first, 301, "test").unwrap();

        let start = Arc::new(Barrier::new(3));
        let first_storage = storage.clone();
        let first_start = Arc::clone(&start);
        let first_worker = thread::spawn(move || {
            first_start.wait();
            first_storage.claim_next_ready(400).unwrap().is_some()
        });
        let second_storage = storage.clone();
        let second_start = Arc::clone(&start);
        let second_worker = thread::spawn(move || {
            second_start.wait();
            second_storage.claim_next_ready(400).unwrap().is_some()
        });
        start.wait();

        let claim_count =
            u8::from(first_worker.join().unwrap()) + u8::from(second_worker.join().unwrap());
        assert_eq!(claim_count, 1);
        remove_root(&root);
    }

    #[test]
    fn recovery_releases_claim_without_reprocessing_videos() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        completed_session(&storage, "session-1", 100);
        let claim = storage.claim_next_ready(200).unwrap().unwrap();
        assert_eq!(claim.manifest().state, SessionState::Claimed);

        assert_eq!(
            storage.recover(300).unwrap(),
            RecoveryReport {
                promoted_captures: 0,
                released_claims: 1,
                failed_sessions: 0,
            }
        );
        let recovered_claim = storage.claim_next_ready(400).unwrap().unwrap();
        assert_eq!(recovered_claim.session_id().as_str(), "session-1");
        assert_eq!(recovered_claim.manifest().state, SessionState::Claimed);
        remove_root(&root);
    }

    #[test]
    fn corrupted_ready_video_is_preserved_in_failed() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        completed_session(&storage, "session-1", 100);
        fs::write(root.join("ready/session-1/camera-1.mkv"), b"tampered").unwrap();

        assert_eq!(
            storage.recover(200).unwrap(),
            RecoveryReport {
                promoted_captures: 0,
                released_claims: 0,
                failed_sessions: 1,
            }
        );
        assert!(!root.join("ready/session-1").exists());
        assert_eq!(
            fs::read(root.join("failed/session-1/camera-1.mkv")).unwrap(),
            b"tampered"
        );
        let manifest = read_manifest(
            &root.join("failed/session-1"),
            &SessionId::new("session-1").unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.state.as_public(), SessionState::Failed);
        assert!(manifest.failure.unwrap().contains("recovery validation"));
        remove_root(&root);
    }

    #[test]
    fn interrupted_capture_is_failed_without_deleting_its_video() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        let id = SessionId::new("session-1").unwrap();
        let capture = storage
            .begin_capture(&id, CaptureMetadata::new("camera-1", 100))
            .unwrap();
        fs::write(capture.video_path("camera-1.mkv").unwrap(), b"partial").unwrap();

        assert_eq!(
            storage.recover(200).unwrap(),
            RecoveryReport {
                promoted_captures: 0,
                released_claims: 0,
                failed_sessions: 1,
            }
        );
        assert_eq!(
            fs::read(root.join("failed/session-1/camera-1.mkv")).unwrap(),
            b"partial"
        );
        remove_root(&root);
    }

    #[test]
    fn discarded_capture_is_removed_immediately() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        let id = SessionId::new("session-1").unwrap();
        let capture = storage
            .begin_capture(&id, CaptureMetadata::new("camera-1", 100))
            .unwrap();
        fs::write(capture.video_path("camera-1.mkv").unwrap(), b"discarded").unwrap();

        storage.discard_capture(&capture).unwrap();

        assert!(!root.join("capturing/session-1.tmp").exists());
        assert_eq!(storage.recover(200).unwrap().failed_sessions, 0);
        remove_root(&root);
    }

    #[test]
    fn manifest_cannot_escape_session_directory() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        completed_session(&storage, "session-1", 100);
        fs::write(
            root.join("ready/session-1/manifest.json"),
            r#"{
  "schema_version": 1,
  "session_id": "session-1",
  "source_id": "camera-1",
  "state": "ready",
  "started_at_ms": 100,
  "finished_at_ms": 200,
  "videos": [{"camera_id": "1", "path": "../outside", "codec": "ffv1", "format": "mkv", "frame_count": 123, "width": 256, "height": 256, "size_bytes": 1, "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}],
  "transitions": [{"state": "ready", "at_ms": 200, "reason": null}],
  "failure": null
}"#,
        )
        .unwrap();

        assert_eq!(storage.recover(300).unwrap().failed_sessions, 1);
        assert!(root.join("failed/session-1/camera-1.mkv").exists());
        remove_root(&root);
    }

    #[test]
    fn video_paths_reject_nested_and_absolute_names() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        let id = SessionId::new("session-1").unwrap();
        let capture = storage
            .begin_capture(&id, CaptureMetadata::new("camera-1", 100))
            .unwrap();

        for path in [
            "../outside",
            "nested/video.mkv",
            "/tmp/video.mkv",
            "manifest.json",
        ] {
            assert!(matches!(
                capture.video_path(path),
                Err(StorageError::InvalidVideoPath(_))
            ));
        }
        remove_root(&root);
    }

    #[test]
    fn undeclared_file_prevents_ready_publication() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        let id = SessionId::new("session-1").unwrap();
        let capture = storage
            .begin_capture(&id, CaptureMetadata::new("camera-1", 100))
            .unwrap();
        fs::write(capture.video_path("camera-1.mkv").unwrap(), b"video").unwrap();
        fs::write(capture.video_path("camera-2.mkv").unwrap(), b"video").unwrap();

        assert!(matches!(
            storage.finalize_capture(capture, 110, &[video("1", "camera-1.mkv")]),
            Err(StorageError::VideoIntegrity { .. })
        ));
        assert!(root.join("capturing/session-1.tmp").is_dir());
        assert!(!root.join("ready/session-1").exists());
        remove_root(&root);
    }

    #[test]
    fn capture_id_is_readable_camera_and_timestamp() {
        assert_eq!(
            SessionStorage::session_id_for_capture("4", 1_785_340_800_000)
                .unwrap()
                .as_str(),
            "cam-4-1785340800000"
        );
    }

    #[test]
    fn capture_id_rejects_unsafe_camera_or_time() {
        for camera_id in ["", ".", "..", "front/left", "front\\left"] {
            assert!(matches!(
                SessionStorage::session_id_for_capture(camera_id, 1),
                Err(StorageError::InvalidCameraId)
            ));
        }
        assert!(matches!(
            SessionStorage::session_id_for_capture("1", -1),
            Err(StorageError::NegativeCaptureTimestamp)
        ));
    }

    #[test]
    fn equal_camera_and_timestamp_cannot_overwrite_capture() {
        let root = test_root();
        let storage = SessionStorage::open(&root).unwrap();
        let id = SessionStorage::session_id_for_capture("1", 100).unwrap();
        storage
            .begin_capture(&id, CaptureMetadata::new("camera-1", 100))
            .unwrap();

        assert!(matches!(
            storage.begin_capture(&id, CaptureMetadata::new("camera-1", 100)),
            Err(StorageError::SessionAlreadyExists(_))
        ));
        remove_root(&root);
    }

    fn completed_session(
        storage: &SessionStorage,
        session_id: &str,
        started_at_ms: i64,
    ) -> super::ReadySession {
        let id = SessionId::new(session_id).unwrap();
        let capture = storage
            .begin_capture(&id, CaptureMetadata::new("camera-1", started_at_ms))
            .unwrap();
        fs::write(capture.video_path("camera-1.mkv").unwrap(), b"hello").unwrap();
        storage
            .finalize_capture(capture, started_at_ms + 10, &[video("1", "camera-1.mkv")])
            .unwrap()
    }

    fn video(camera_id: &str, path: &str) -> CapturedVideo {
        CapturedVideo::new(camera_id, path, "ffv1", "mkv", 123, 256, 256).unwrap()
    }

    fn test_root() -> PathBuf {
        let id = NEXT_STORAGE_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pcrt-storage-test-{}-{id}", std::process::id()))
    }

    fn remove_root(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn storage_state_paths_are_not_exposed_as_public_api() {
        assert_eq!(StorageState::Ready.directory_name(), "ready");
    }
}
