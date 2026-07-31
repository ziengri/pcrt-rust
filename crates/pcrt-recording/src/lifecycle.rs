/// Stable encoder profile for the Rust recorder migration.
pub const VIDEO_CODEC: &str = "libx264";
/// Matroska keeps the existing recorder filename/container convention.
pub const VIDEO_FORMAT: &str = "mkv";
/// ffmpeg `libx264` preset selected for the production recorder.
pub const X264_PRESET: &str = "fast";
/// ffmpeg `libx264` CRF selected for the production recorder.
pub const X264_CRF: u8 = 18;

/// Validated limits for one camera recorder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingLimits {
    frames_per_second: u32,
    max_frames: u64,
}

impl RecordingLimits {
    /// Validates an integral frame rate and positive frame cap.
    ///
    /// # Errors
    ///
    /// Returns an error when either limit is zero.
    pub const fn new(frames_per_second: u32, max_frames: u64) -> Result<Self, RecordingError> {
        if frames_per_second == 0 {
            return Err(RecordingError::ZeroFrameRate);
        }
        if max_frames == 0 {
            return Err(RecordingError::ZeroFrameLimit);
        }
        Ok(Self {
            frames_per_second,
            max_frames,
        })
    }

    /// Returns the maximum count accepted before an over-limit discard.
    #[must_use]
    pub const fn max_frames(self) -> u64 {
        self.max_frames
    }
}

/// Lifecycle errors detected before an adapter is invoked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingError {
    /// FPS cannot be zero.
    ZeroFrameRate,
    /// A recording must allow at least one frame.
    ZeroFrameLimit,
}

impl std::fmt::Display for RecordingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroFrameRate => formatter.write_str("recording frame rate must be positive"),
            Self::ZeroFrameLimit => formatter.write_str("recording frame limit must be positive"),
        }
    }
}

impl std::error::Error for RecordingError {}

/// Current lifecycle state for one configured camera.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingState {
    /// No open capture exists.
    Idle,
    /// A capture is accepting video frames.
    Capturing { frame_count: u64 },
    /// An over-limit capture was discarded; wait for closed/stale door state.
    DiscardedUntilDoorClosed,
}

/// Side effect that an outer recorder adapter must perform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingAction {
    /// Allocate storage and start the encoder before writing the current frame.
    StartCapture,
    /// Write the frame received by the outer adapter.
    WriteFrame,
    /// Close encoder and publish a completed capture through durable storage.
    FinalizeCapture { frame_count: u64 },
    /// Close encoder without publishing an over-limit capture.
    DiscardCapture { frame_count: u64 },
}

/// Deterministic Python-compatible door-gated capture lifecycle.
#[derive(Debug)]
pub struct RecordingLifecycle {
    limits: RecordingLimits,
    state: RecordingState,
}

impl RecordingLifecycle {
    /// Creates an idle recorder lifecycle.
    #[must_use]
    pub const fn new(limits: RecordingLimits) -> Self {
        Self {
            limits,
            state: RecordingState::Idle,
        }
    }

    /// Returns the current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> RecordingState {
        self.state
    }

    /// Handles the latest door state before reading the next frame.
    ///
    /// A close or stale state finalizes an active capture and re-enables recording
    /// after an earlier over-limit discard.
    pub fn on_door_state(&mut self, door_open: bool) -> Option<RecordingAction> {
        if door_open {
            return None;
        }
        match self.state {
            RecordingState::Capturing { frame_count } => {
                self.state = RecordingState::Idle;
                Some(RecordingAction::FinalizeCapture { frame_count })
            }
            RecordingState::DiscardedUntilDoorClosed => {
                self.state = RecordingState::Idle;
                None
            }
            RecordingState::Idle => None,
        }
    }

    /// Handles one valid frame after the latest door state was applied.
    ///
    /// Python compatibility intentionally checks the cap after the frame write, so
    /// `max_frames + 1` is written then discarded and recording is suppressed until
    /// the door becomes closed or stale.
    pub fn on_frame(&mut self, door_open: bool) -> Vec<RecordingAction> {
        if !door_open || matches!(self.state, RecordingState::DiscardedUntilDoorClosed) {
            return Vec::new();
        }
        if matches!(self.state, RecordingState::Idle) {
            self.state = RecordingState::Capturing { frame_count: 0 };
            let mut actions = vec![RecordingAction::StartCapture];
            actions.extend(self.record_frame());
            return actions;
        }
        self.record_frame()
    }

    /// Requests a controlled shutdown. An active capture is finalized; a previously
    /// discarded capture has already been closed and needs no action.
    pub fn shutdown(&mut self) -> Option<RecordingAction> {
        let action = match self.state {
            RecordingState::Capturing { frame_count } => {
                Some(RecordingAction::FinalizeCapture { frame_count })
            }
            RecordingState::Idle | RecordingState::DiscardedUntilDoorClosed => None,
        };
        self.state = RecordingState::Idle;
        action
    }

    fn record_frame(&mut self) -> Vec<RecordingAction> {
        let RecordingState::Capturing { frame_count } = self.state else {
            unreachable!("frames are recorded only while capturing");
        };
        let frame_count = frame_count.saturating_add(1);
        if frame_count > self.limits.max_frames() {
            self.state = RecordingState::DiscardedUntilDoorClosed;
            vec![
                RecordingAction::WriteFrame,
                RecordingAction::DiscardCapture { frame_count },
            ]
        } else {
            self.state = RecordingState::Capturing { frame_count };
            vec![RecordingAction::WriteFrame]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RecordingAction, RecordingError, RecordingLifecycle, RecordingLimits, RecordingState,
        VIDEO_CODEC, VIDEO_FORMAT, X264_CRF, X264_PRESET,
    };

    fn lifecycle() -> RecordingLifecycle {
        RecordingLifecycle::new(RecordingLimits::new(25, 2).unwrap())
    }

    #[test]
    fn h264_profile_is_fixed_for_the_migration() {
        assert_eq!(VIDEO_CODEC, "libx264");
        assert_eq!(VIDEO_FORMAT, "mkv");
        assert_eq!(X264_PRESET, "fast");
        assert_eq!(X264_CRF, 18);
    }

    #[test]
    fn open_frames_then_close_finalizes_one_capture() {
        let mut lifecycle = lifecycle();

        assert_eq!(
            lifecycle.on_frame(true),
            vec![RecordingAction::StartCapture, RecordingAction::WriteFrame]
        );
        assert_eq!(lifecycle.on_frame(true), vec![RecordingAction::WriteFrame]);
        assert_eq!(
            lifecycle.on_door_state(false),
            Some(RecordingAction::FinalizeCapture { frame_count: 2 })
        );
        assert_eq!(lifecycle.state(), RecordingState::Idle);
    }

    #[test]
    fn closed_or_stale_state_never_starts_a_capture() {
        let mut lifecycle = lifecycle();

        assert!(lifecycle.on_frame(false).is_empty());
        assert_eq!(lifecycle.state(), RecordingState::Idle);
    }

    #[test]
    fn repeated_open_state_does_not_start_another_capture() {
        let mut lifecycle = lifecycle();

        assert_eq!(
            lifecycle.on_frame(true),
            vec![RecordingAction::StartCapture, RecordingAction::WriteFrame]
        );
        assert_eq!(lifecycle.on_door_state(true), None);
        assert_eq!(lifecycle.on_frame(true), vec![RecordingAction::WriteFrame]);
    }

    #[test]
    fn python_frame_limit_discards_after_one_extra_frame() {
        let mut lifecycle = lifecycle();

        assert_eq!(
            lifecycle.on_frame(true),
            vec![RecordingAction::StartCapture, RecordingAction::WriteFrame]
        );
        assert_eq!(lifecycle.on_frame(true), vec![RecordingAction::WriteFrame]);
        assert_eq!(
            lifecycle.on_frame(true),
            vec![
                RecordingAction::WriteFrame,
                RecordingAction::DiscardCapture { frame_count: 3 }
            ]
        );
        assert_eq!(lifecycle.state(), RecordingState::DiscardedUntilDoorClosed);
        assert!(lifecycle.on_frame(true).is_empty());
        assert_eq!(lifecycle.on_door_state(false), None);
        assert_eq!(lifecycle.state(), RecordingState::Idle);
    }

    #[test]
    fn shutdown_finalizes_active_capture() {
        let mut lifecycle = lifecycle();
        let _ = lifecycle.on_frame(true);

        assert_eq!(
            lifecycle.shutdown(),
            Some(RecordingAction::FinalizeCapture { frame_count: 1 })
        );
        assert_eq!(lifecycle.state(), RecordingState::Idle);
    }

    #[test]
    fn limits_must_be_positive() {
        assert_eq!(
            RecordingLimits::new(0, 1),
            Err(RecordingError::ZeroFrameRate)
        );
        assert_eq!(
            RecordingLimits::new(25, 0),
            Err(RecordingError::ZeroFrameLimit)
        );
    }
}
