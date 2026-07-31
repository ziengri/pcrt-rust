//! OpenCV-backed video source for camera indexes, local files and RTSP URLs.

use std::path::Path;

use opencv::{
    core::Mat,
    prelude::{MatTraitConst, VideoCaptureTrait, VideoCaptureTraitConst},
    videoio,
};

/// Errors returned while opening, reading or resetting a video source.
#[derive(Debug)]
pub enum VideoSourceError {
    /// `OpenCV` rejected an operation.
    OpenCv(opencv::Error),
    /// `OpenCV` accepted the source but did not open it.
    OpenFailed(String),
}

impl std::fmt::Display for VideoSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenCv(error) => write!(formatter, "OpenCV: {error}"),
            Self::OpenFailed(source) => write!(formatter, "failed to open video source: {source}"),
        }
    }
}

impl std::error::Error for VideoSourceError {}

impl From<opencv::Error> for VideoSourceError {
    fn from(error: opencv::Error) -> Self {
        Self::OpenCv(error)
    }
}

/// One BGR frame read from the configured `OpenCV` source.
#[derive(Debug)]
pub struct VideoFrame(Mat);

impl VideoFrame {
    /// Wraps a BGR `OpenCV` matrix returned by an internal source/test adapter.
    #[must_use]
    pub(crate) const fn from_mat(mat: Mat) -> Self {
        Self(mat)
    }

    /// Returns the underlying `OpenCV` BGR matrix for resize and encoder adapters.
    #[must_use]
    pub const fn mat(&self) -> &Mat {
        &self.0
    }

    /// Transfers ownership of the `OpenCV` matrix to a frame processing adapter.
    #[must_use]
    pub(crate) fn into_mat(self) -> Mat {
        self.0
    }
}

/// Unified `OpenCV` source matching Python `cv2.VideoCapture` semantics.
pub struct OpenCvVideoSource {
    source: String,
    is_file: bool,
    exhausted: bool,
    capture: videoio::VideoCapture,
}

impl OpenCvVideoSource {
    /// Opens an `OpenCV` camera index, local file or RTSP URL.
    ///
    /// Numeric source strings select a camera index. A path that exists locally is
    /// replayable and reports exhaustion; all other values are handed to `OpenCV`,
    /// including RTSP URLs.
    ///
    /// # Errors
    ///
    /// Returns an error when `OpenCV` cannot initialize the configured source.
    pub fn open(source: impl Into<String>) -> Result<Self, VideoSourceError> {
        let source = source.into();
        let is_file = Path::new(&source).is_file();
        let capture = open_capture(&source)?;
        Ok(Self {
            source,
            is_file,
            exhausted: false,
            capture,
        })
    }

    /// Returns whether a local file source reached EOF.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.exhausted
    }

    /// Returns whether the source is a local regular file.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        self.is_file
    }

    /// Returns the next BGR frame or `None` when no frame is available.
    ///
    /// A failed RTSP/camera read returns `None` without marking the source exhausted;
    /// a local file read marks it exhausted so the recorder can reopen from the start.
    ///
    /// # Errors
    ///
    /// Returns an error for an `OpenCV` read failure.
    pub fn read(&mut self) -> Result<Option<VideoFrame>, VideoSourceError> {
        let mut frame = Mat::default();
        if !self.capture.read(&mut frame)? || frame.empty() {
            if self.is_file {
                self.exhausted = true;
            }
            return Ok(None);
        }
        Ok(Some(VideoFrame::from_mat(frame)))
    }

    /// Reopens a local file from its first frame.
    ///
    /// # Errors
    ///
    /// Returns an error when the source is not a file or cannot be reopened.
    pub fn reset(&mut self) -> Result<(), VideoSourceError> {
        if !self.is_file {
            return Err(VideoSourceError::OpenFailed(format!(
                "source is not a local file: {}",
                self.source
            )));
        }
        self.capture.release()?;
        self.capture = open_capture(&self.source)?;
        self.exhausted = false;
        Ok(())
    }
}

impl Drop for OpenCvVideoSource {
    fn drop(&mut self) {
        let _ = self.capture.release();
    }
}

fn open_capture(source: &str) -> Result<videoio::VideoCapture, VideoSourceError> {
    let capture = match source.parse::<i32>() {
        Ok(index) => videoio::VideoCapture::new(index, videoio::CAP_ANY)?,
        Err(_) => videoio::VideoCapture::from_file(source, videoio::CAP_ANY)?,
    };
    if !capture.is_opened()? {
        return Err(VideoSourceError::OpenFailed(source.to_owned()));
    }
    Ok(capture)
}
