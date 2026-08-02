//! Storage-backed execution of the door-gated recording lifecycle.

use std::path::Path;

use pcrt_storage::{CaptureMetadata, CaptureSession, CapturedVideo, SessionStorage, StorageError};

use crate::lifecycle::{
    RecordingAction, RecordingLifecycle, RecordingLimits, VIDEO_CODEC, VIDEO_FORMAT,
};

/// Starts an encoder for one capture output path.
pub trait EncoderFactory {
    /// Starts an encoder that accepts resized BGR24 frames.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error if the encoder process cannot start.
    fn start(
        &self,
        output: &Path,
        width: u32,
        height: u32,
        frames_per_second: u32,
    ) -> Result<Box<dyn FrameEncoder>, String>;
}

/// Writes and closes one capture encoder.
pub trait FrameEncoder {
    /// Writes one exact BGR24 frame.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error when the encoder rejects the frame.
    fn write_frame(&mut self, frame: &[u8]) -> Result<(), String>;

    /// Closes input and completes the encoded output.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error if encoding does not complete successfully.
    fn finish(self: Box<Self>) -> Result<u64, String>;

    /// Stops an incomplete encoder without publishing its output.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error if the encoder cannot be stopped.
    fn abort(self: Box<Self>) -> Result<(), String>;
}

/// Static configuration for a one-camera recorder process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecorderConfig {
    /// Stable camera identifier recorded in the session manifest.
    pub camera_id: String,
    /// Source label, normally the configured RTSP/file source.
    pub source_id: String,
    /// Resized encoded frame width.
    pub width: u32,
    /// Resized encoded frame height.
    pub height: u32,
    /// Target encoded frame rate.
    pub frames_per_second: u32,
}

/// Errors produced by recorder orchestration.
#[derive(Debug)]
pub enum RecorderError {
    /// Durable capture lifecycle failed.
    Storage(StorageError),
    /// ffmpeg or a test encoder rejected an operation.
    Encoder(String),
    /// The lifecycle requested a side effect incompatible with the active capture.
    Invariant(&'static str),
    /// Encoder accepted a different number of frames than the lifecycle counted.
    FrameCountMismatch { encoder: u64, lifecycle: u64 },
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "recording storage: {error}"),
            Self::Encoder(error) => write!(formatter, "recording encoder: {error}"),
            Self::Invariant(message) => write!(formatter, "recording invariant: {message}"),
            Self::FrameCountMismatch { encoder, lifecycle } => {
                write!(
                    formatter,
                    "encoder frame count {encoder} differs from lifecycle {lifecycle}"
                )
            }
        }
    }
}

impl std::error::Error for RecorderError {}

impl From<StorageError> for RecorderError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

struct ActiveCapture {
    capture: CaptureSession,
    encoder: Box<dyn FrameEncoder>,
}

/// Executes one camera lifecycle against durable storage and an encoder factory.
pub struct Recorder<F> {
    storage: SessionStorage,
    encoder_factory: F,
    config: RecorderConfig,
    lifecycle: RecordingLifecycle,
    active: Option<ActiveCapture>,
}

impl<F: EncoderFactory> Recorder<F> {
    /// Creates an idle one-camera recorder.
    #[must_use]
    pub fn new(
        storage: SessionStorage,
        encoder_factory: F,
        config: RecorderConfig,
        limits: RecordingLimits,
    ) -> Self {
        Self {
            storage,
            encoder_factory,
            config,
            lifecycle: RecordingLifecycle::new(limits),
            active: None,
        }
    }

    /// Applies the latest door state. A closed or stale state finalizes active output.
    ///
    /// # Errors
    ///
    /// Returns an error if finalization cannot complete and publish durably.
    pub fn on_door_state(&mut self, door_open: bool, now_ms: i64) -> Result<(), RecorderError> {
        if let Some(action) = self.lifecycle.plan_door_state(door_open) {
            self.execute_and_complete(action, None, now_ms)?;
        } else {
            self.lifecycle.acknowledge_closed_door(door_open);
        }
        Ok(())
    }

    /// Applies one pre-resized BGR24 frame after the latest door state.
    ///
    /// # Errors
    ///
    /// Returns an error if capture allocation, frame writing or over-limit discard fails.
    pub fn on_frame(
        &mut self,
        door_open: bool,
        frame: &[u8],
        now_ms: i64,
    ) -> Result<(), RecorderError> {
        for action in self.lifecycle.plan_frame(door_open) {
            self.execute_and_complete(action, Some(frame), now_ms)?;
        }
        Ok(())
    }

    /// Finalizes an active capture during controlled process shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the encoder or durable publication fails.
    pub fn shutdown(&mut self, now_ms: i64) -> Result<(), RecorderError> {
        if let Some(action) = self.lifecycle.plan_shutdown() {
            self.execute_and_complete(action, None, now_ms)?;
        }
        Ok(())
    }

    fn execute_and_complete(
        &mut self,
        action: RecordingAction,
        frame: Option<&[u8]>,
        now_ms: i64,
    ) -> Result<(), RecorderError> {
        let result = match action {
            RecordingAction::StartCapture => self.start_capture(now_ms),
            RecordingAction::WriteFrame => self.write_frame(frame),
            RecordingAction::FinalizeCapture { frame_count } => {
                self.finalize_capture(frame_count, now_ms)
            }
            RecordingAction::DiscardCapture { .. } => self.discard_capture(),
        };
        if result.is_ok() {
            self.lifecycle.complete(action);
        } else {
            self.lifecycle.abandon();
            self.abort_active();
        }
        result
    }

    fn start_capture(&mut self, now_ms: i64) -> Result<(), RecorderError> {
        if self.active.is_some() {
            return Err(RecorderError::Invariant(
                "attempted to start while a capture is active",
            ));
        }
        let session_id = SessionStorage::session_id_for_capture(&self.config.camera_id, now_ms)?;
        let capture = self.storage.begin_capture(
            &session_id,
            CaptureMetadata::new(&self.config.source_id, now_ms),
        )?;
        let output = capture.video_path(&format!("{}.{}", self.config.camera_id, VIDEO_FORMAT))?;
        let encoder = match self.encoder_factory.start(
            &output,
            self.config.width,
            self.config.height,
            self.config.frames_per_second,
        ) {
            Ok(encoder) => encoder,
            Err(error) => {
                self.storage.discard_capture(&capture)?;
                return Err(RecorderError::Encoder(error));
            }
        };
        self.active = Some(ActiveCapture { capture, encoder });
        Ok(())
    }

    fn write_frame(&mut self, frame: Option<&[u8]>) -> Result<(), RecorderError> {
        let frame = frame.ok_or(RecorderError::Invariant("frame action has no frame"))?;
        let active = self.active.as_mut().ok_or(RecorderError::Invariant(
            "frame action has no active capture",
        ))?;
        active
            .encoder
            .write_frame(frame)
            .map_err(RecorderError::Encoder)
    }

    fn finalize_capture(&mut self, frame_count: u64, now_ms: i64) -> Result<(), RecorderError> {
        let active = self.active.take().ok_or(RecorderError::Invariant(
            "finalize action has no active capture",
        ))?;
        let encoder_frame_count = active.encoder.finish().map_err(RecorderError::Encoder)?;
        if encoder_frame_count != frame_count {
            return Err(RecorderError::FrameCountMismatch {
                encoder: encoder_frame_count,
                lifecycle: frame_count,
            });
        }
        let video = CapturedVideo::new(
            &self.config.camera_id,
            format!("{}.{}", self.config.camera_id, VIDEO_FORMAT),
            VIDEO_CODEC,
            VIDEO_FORMAT,
            frame_count,
            self.config.width,
            self.config.height,
        )?;
        let _ = self
            .storage
            .finalize_capture(active.capture, now_ms, &[video])?;
        Ok(())
    }

    fn discard_capture(&mut self) -> Result<(), RecorderError> {
        let active = self.active.take().ok_or(RecorderError::Invariant(
            "discard action has no active capture",
        ))?;
        active.encoder.abort().map_err(RecorderError::Encoder)?;
        self.storage.discard_capture(&active.capture)?;
        Ok(())
    }

    fn abort_active(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active.encoder.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        path::{Path, PathBuf},
        rc::Rc,
    };

    use pcrt_storage::SessionStorage;
    use tempfile::tempdir;

    use crate::lifecycle::RecordingLimits;

    use super::{EncoderFactory, FrameEncoder, Recorder, RecorderConfig, RecorderError};

    #[derive(Clone, Default)]
    struct FakeFactory {
        state: Rc<RefCell<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        started_outputs: Vec<PathBuf>,
        frames: Vec<Vec<u8>>,
        start_error: bool,
        write_error: bool,
        finish_error: bool,
        aborted: usize,
    }

    impl EncoderFactory for FakeFactory {
        fn start(
            &self,
            output: &Path,
            _width: u32,
            _height: u32,
            _frames_per_second: u32,
        ) -> Result<Box<dyn FrameEncoder>, String> {
            let mut state = self.state.borrow_mut();
            state.started_outputs.push(output.to_owned());
            if state.start_error {
                return Err("encoder start failed".to_owned());
            }
            Ok(Box::new(FakeEncoder {
                state: Rc::clone(&self.state),
                frame_count: 0,
            }))
        }
    }

    struct FakeEncoder {
        state: Rc<RefCell<FakeState>>,
        frame_count: u64,
    }

    impl FrameEncoder for FakeEncoder {
        fn write_frame(&mut self, frame: &[u8]) -> Result<(), String> {
            let mut state = self.state.borrow_mut();
            if state.write_error {
                return Err("encoder write failed".to_owned());
            }
            state.frames.push(frame.to_vec());
            self.frame_count += 1;
            Ok(())
        }

        fn finish(self: Box<Self>) -> Result<u64, String> {
            if self.state.borrow().finish_error {
                return Err("encoder failed".to_owned());
            }
            for output in &self.state.borrow().started_outputs {
                fs::write(output, b"encoded video").map_err(|error| error.to_string())?;
            }
            Ok(self.frame_count)
        }

        fn abort(self: Box<Self>) -> Result<(), String> {
            self.state.borrow_mut().aborted += 1;
            Ok(())
        }
    }

    fn recorder(
        factory: FakeFactory,
        max_frames: u64,
    ) -> (tempfile::TempDir, Recorder<FakeFactory>) {
        let directory = tempdir().unwrap();
        let storage = SessionStorage::open(directory.path()).unwrap();
        let config = RecorderConfig {
            camera_id: "cam1".to_owned(),
            source_id: "rtsp://camera/stream".to_owned(),
            width: 2,
            height: 1,
            frames_per_second: 25,
        };
        let recorder = Recorder::new(
            storage,
            factory,
            config,
            RecordingLimits::new(25, max_frames).unwrap(),
        );
        (directory, recorder)
    }

    #[test]
    fn close_publishes_one_atomic_ready_session() {
        let factory = FakeFactory::default();
        let state = Rc::clone(&factory.state);
        let (directory, mut recorder) = recorder(factory, 10);
        let frame = [1_u8; 6];

        recorder.on_frame(true, &frame, 100).unwrap();
        recorder.on_frame(true, &frame, 101).unwrap();
        recorder.on_door_state(false, 102).unwrap();

        assert_eq!(state.borrow().frames, vec![frame.to_vec(), frame.to_vec()]);
        let storage = SessionStorage::open(directory.path()).unwrap();
        let claimed = storage.claim_next_ready(103).unwrap().unwrap();
        assert_eq!(claimed.manifest().videos.len(), 1);
        let video = &claimed.manifest().videos[0];
        assert_eq!(video.camera_id, "cam1");
        assert_eq!(video.path, "cam1.mkv");
        assert_eq!(video.codec, "libx264");
        assert_eq!(video.frame_count, 2);
    }

    #[test]
    fn over_limit_aborts_and_never_publishes_a_ready_session() {
        let factory = FakeFactory::default();
        let state = Rc::clone(&factory.state);
        let (directory, mut recorder) = recorder(factory, 1);
        let frame = [2_u8; 6];

        recorder.on_frame(true, &frame, 100).unwrap();
        recorder.on_frame(true, &frame, 101).unwrap();
        recorder.on_frame(true, &frame, 102).unwrap();

        assert_eq!(state.borrow().aborted, 1);
        let storage = SessionStorage::open(directory.path()).unwrap();
        assert!(storage.claim_next_ready(103).unwrap().is_none());
        let report = storage.recover_recording("cam1", 104).unwrap();
        assert_eq!(report.failed_sessions, 0);
        assert!(
            !directory
                .path()
                .join("capturing")
                .read_dir()
                .unwrap()
                .any(|entry| entry.is_ok())
        );
    }

    #[test]
    fn encoder_failure_leaves_capture_for_recovery() {
        let factory = FakeFactory::default();
        factory.state.borrow_mut().finish_error = true;
        let (directory, mut recorder) = recorder(factory, 10);
        let frame = [3_u8; 6];

        recorder.on_frame(true, &frame, 100).unwrap();
        assert!(matches!(
            recorder.on_door_state(false, 101),
            Err(RecorderError::Encoder(_))
        ));

        let storage = SessionStorage::open(directory.path()).unwrap();
        assert!(storage.claim_next_ready(102).unwrap().is_none());
        assert_eq!(
            storage
                .recover_recording("cam1", 103)
                .unwrap()
                .failed_sessions,
            1
        );
    }

    #[test]
    fn failed_encoder_start_waits_for_a_closed_door_before_retrying() {
        let factory = FakeFactory::default();
        let state = Rc::clone(&factory.state);
        state.borrow_mut().start_error = true;
        let (_directory, mut recorder) = recorder(factory, 10);

        assert!(matches!(
            recorder.on_frame(true, &[5_u8; 6], 100),
            Err(RecorderError::Encoder(_))
        ));
        state.borrow_mut().start_error = false;
        recorder.on_frame(true, &[5_u8; 6], 101).unwrap();
        assert_eq!(state.borrow().started_outputs.len(), 1);

        recorder.on_door_state(false, 102).unwrap();
        recorder.on_frame(true, &[5_u8; 6], 103).unwrap();
        assert_eq!(state.borrow().started_outputs.len(), 2);
    }

    #[test]
    fn failed_frame_write_aborts_and_does_not_issue_follow_up_writes() {
        let factory = FakeFactory::default();
        let state = Rc::clone(&factory.state);
        state.borrow_mut().write_error = true;
        let (_directory, mut recorder) = recorder(factory, 10);

        assert!(matches!(
            recorder.on_frame(true, &[6_u8; 6], 100),
            Err(RecorderError::Encoder(_))
        ));
        assert_eq!(state.borrow().aborted, 1);
        state.borrow_mut().write_error = false;
        recorder.on_frame(true, &[6_u8; 6], 101).unwrap();
        assert!(state.borrow().frames.is_empty());
    }

    #[test]
    fn shutdown_finalizes_active_capture() {
        let factory = FakeFactory::default();
        let (directory, mut recorder) = recorder(factory, 10);

        recorder.on_frame(true, &[4_u8; 6], 100).unwrap();
        recorder.shutdown(101).unwrap();

        let storage = SessionStorage::open(directory.path()).unwrap();
        assert!(storage.claim_next_ready(102).unwrap().is_some());
    }
}
