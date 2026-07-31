//! Door-gated recording service driven by an `OpenCV` video source.

use opencv::{
    core::{self, Mat, Size},
    imgproc,
    prelude::{MatTraitConst, MatTraitConstManual},
};

use crate::{
    recorder::{EncoderFactory, Recorder, RecorderError},
    video::{OpenCvVideoSource, VideoFrame, VideoSourceError},
};

/// Result of one camera read and recorder iteration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingServiceStep {
    /// A frame was resized/validated and supplied to the recorder lifecycle.
    FrameHandled,
    /// A frame was read to keep the source current but discarded while the door was closed or stale.
    FrameDiscardedDoorClosed,
    /// A local file reached EOF, active recording was finalized and the source restarted.
    FileRestarted,
    /// RTSP/camera returned no frame; it remains eligible for future reads.
    NoFrame,
}

/// Error from `OpenCV` frame processing or durable recording.
#[derive(Debug)]
pub enum RecordingServiceError {
    /// Opening, reading or resetting the source failed.
    Source(VideoSourceError),
    /// Frame resize/type/contiguity processing failed in `OpenCV`.
    OpenCv(opencv::Error),
    /// Incoming frame was not 8-bit three-channel BGR.
    InvalidFrameType { actual: i32 },
    /// The durable recording lifecycle rejected an action.
    Recorder(RecorderError),
}

impl std::fmt::Display for RecordingServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Source(error) => write!(formatter, "video source: {error}"),
            Self::OpenCv(error) => write!(formatter, "OpenCV frame processing: {error}"),
            Self::InvalidFrameType { actual } => {
                write!(
                    formatter,
                    "expected CV_8UC3 BGR frame, received OpenCV type {actual}"
                )
            }
            Self::Recorder(error) => write!(formatter, "recorder: {error}"),
        }
    }
}

impl std::error::Error for RecordingServiceError {}

impl From<VideoSourceError> for RecordingServiceError {
    fn from(error: VideoSourceError) -> Self {
        Self::Source(error)
    }
}

impl From<opencv::Error> for RecordingServiceError {
    fn from(error: opencv::Error) -> Self {
        Self::OpenCv(error)
    }
}

impl From<RecorderError> for RecordingServiceError {
    fn from(error: RecorderError) -> Self {
        Self::Recorder(error)
    }
}

/// Python `SessionRecorderService` equivalent, with `OpenCV` as the source adapter.
pub struct RecordingService<F> {
    source: OpenCvVideoSource,
    recorder: Recorder<F>,
    width: u32,
    height: u32,
}

impl<F: EncoderFactory> RecordingService<F> {
    /// Combines an opened `OpenCV` source with durable recorder orchestration.
    ///
    /// # Errors
    ///
    /// Returns an error when target dimensions cannot fit `OpenCV`'s signed size type.
    pub fn new(
        source: OpenCvVideoSource,
        recorder: Recorder<F>,
        width: u32,
        height: u32,
    ) -> Result<Self, RecordingServiceError> {
        let _ = target_size(width, height)?;
        Ok(Self {
            source,
            recorder,
            width,
            height,
        })
    }

    /// Runs one read/record iteration using the latest normalized door state.
    ///
    /// The caller maps missing or stale door telemetry to `false`. A file EOF
    /// finalizes an active capture and resets the source, matching Python recorder
    /// behavior. An RTSP/camera no-frame result is retried by a later iteration.
    ///
    /// # Errors
    ///
    /// Returns an error for source, `OpenCV` processing, encoder or storage failures.
    pub fn step(
        &mut self,
        door_open: bool,
        now_ms: i64,
    ) -> Result<RecordingServiceStep, RecordingServiceError> {
        self.recorder.on_door_state(door_open, now_ms)?;
        let Some(frame) = self.source.read()? else {
            if self.source.exhausted() {
                self.recorder.shutdown(now_ms)?;
                self.source.reset()?;
                return Ok(RecordingServiceStep::FileRestarted);
            }
            return Ok(RecordingServiceStep::NoFrame);
        };
        if !door_open {
            return Ok(RecordingServiceStep::FrameDiscardedDoorClosed);
        }
        let bytes = resize_bgr24(frame, self.width, self.height)?;
        self.recorder.on_frame(door_open, &bytes, now_ms)?;
        Ok(RecordingServiceStep::FrameHandled)
    }

    /// Finalizes the active capture for controlled process shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if encoder completion or durable publication fails.
    pub fn shutdown(&mut self, now_ms: i64) -> Result<(), RecordingServiceError> {
        self.recorder.shutdown(now_ms)?;
        Ok(())
    }
}

fn resize_bgr24(
    frame: VideoFrame,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, RecordingServiceError> {
    let source = frame.into_mat();
    if source.typ() != core::CV_8UC3 {
        return Err(RecordingServiceError::InvalidFrameType {
            actual: source.typ(),
        });
    }
    let mut resized = Mat::default();
    let target = target_size(width, height)?;
    let output = if source.cols() == target.width && source.rows() == target.height {
        &source
    } else {
        imgproc::resize(
            &source,
            &mut resized,
            target,
            0.0,
            0.0,
            imgproc::INTER_LINEAR,
        )?;
        &resized
    };
    Ok(output.data_bytes()?.to_vec())
}

fn target_size(width: u32, height: u32) -> Result<Size, RecordingServiceError> {
    let width =
        i32::try_from(width).map_err(|_| RecordingServiceError::InvalidFrameType { actual: -1 })?;
    let height = i32::try_from(height)
        .map_err(|_| RecordingServiceError::InvalidFrameType { actual: -1 })?;
    Ok(Size::new(width, height))
}

#[cfg(test)]
mod tests {
    use opencv::core::{self, Mat, Scalar};

    use crate::video::VideoFrame;

    use super::{RecordingServiceError, resize_bgr24};

    #[test]
    fn resizes_bgr_frame_to_raw_bgr24_bytes() {
        let frame = Mat::new_rows_cols_with_default(1, 2, core::CV_8UC3, Scalar::all(7.0)).unwrap();

        let bytes = resize_bgr24(VideoFrame::from_mat(frame), 3, 2).unwrap();

        assert_eq!(bytes.len(), 18);
        assert!(bytes.iter().all(|byte| *byte == 7));
    }

    #[test]
    fn rejects_non_bgr_frame() {
        let frame = Mat::new_rows_cols_with_default(1, 1, core::CV_8UC1, Scalar::all(0.0)).unwrap();

        assert!(matches!(
            resize_bgr24(VideoFrame::from_mat(frame), 1, 1),
            Err(RecordingServiceError::InvalidFrameType { .. })
        ));
    }

    #[test]
    #[ignore = "requires PCRT_RECORDING_SMOKE_SOURCE, OpenCV and ffmpeg"]
    fn records_a_local_file_through_opencv_and_ffmpeg() {
        use std::env;

        use pcrt_storage::SessionStorage;
        use tempfile::tempdir;

        use crate::{
            lifecycle::RecordingLimits,
            recorder::{FfmpegEncoderFactory, Recorder, RecorderConfig},
            video::OpenCvVideoSource,
        };

        use super::{RecordingService, RecordingServiceStep};

        let source = env::var("PCRT_RECORDING_SMOKE_SOURCE")
            .expect("PCRT_RECORDING_SMOKE_SOURCE must name a local video file");
        let directory = tempdir().unwrap();
        let storage = SessionStorage::open(directory.path()).unwrap();
        let recorder = Recorder::new(
            storage,
            FfmpegEncoderFactory,
            RecorderConfig {
                camera_id: "cam1".to_owned(),
                source_id: source.clone(),
                width: 256,
                height: 256,
                frames_per_second: 25,
            },
            RecordingLimits::new(25, 100).unwrap(),
        );
        let mut service =
            RecordingService::new(OpenCvVideoSource::open(source).unwrap(), recorder, 256, 256)
                .unwrap();

        let mut frames = 0;
        for now_ms in 100_i64..110 {
            if service.step(true, now_ms).unwrap() == RecordingServiceStep::FrameHandled {
                frames += 1;
            }
        }
        assert!(frames > 0, "source did not provide any frames");
        service.step(false, 110).unwrap();

        let storage = SessionStorage::open(directory.path()).unwrap();
        let ready = storage.claim_next_ready(111).unwrap().unwrap();
        let video = &ready.manifest().videos[0];
        assert_eq!(video.path, "cam1.mkv");
        assert_eq!(video.frame_count, frames);
        assert_eq!((video.width, video.height), (256, 256));
    }
}
