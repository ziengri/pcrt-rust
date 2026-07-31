//! ffmpeg raw BGR24 encoder for one capture video.

use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::lifecycle::{VIDEO_CODEC, X264_CRF, X264_PRESET};

/// How long `finish` or `abort` waits before killing an unresponsive ffmpeg child.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Validated raw-frame encoding parameters for one output video.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FfmpegConfig {
    program: PathBuf,
    output: PathBuf,
    width: u32,
    height: u32,
    frames_per_second: u32,
    stop_timeout: Duration,
}

impl FfmpegConfig {
    /// Creates configuration for a Matroska H.264 output video.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, frame rate, timeout or raw frame size are
    /// invalid.
    pub fn new(
        output: impl Into<PathBuf>,
        width: u32,
        height: u32,
        frames_per_second: u32,
    ) -> Result<Self, FfmpegError> {
        Self::with_program_and_timeout(
            "ffmpeg",
            output,
            width,
            height,
            frames_per_second,
            DEFAULT_STOP_TIMEOUT,
        )
    }

    /// Creates configuration with an explicit binary path and stop timeout.
    ///
    /// This is primarily useful for service configuration and deterministic adapter
    /// tests that use a controlled child process.
    ///
    /// # Errors
    ///
    /// Returns an error when dimensions, frame rate, timeout or raw frame size are
    /// invalid.
    pub fn with_program_and_timeout(
        program: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
        width: u32,
        height: u32,
        frames_per_second: u32,
        stop_timeout: Duration,
    ) -> Result<Self, FfmpegError> {
        if width == 0 || height == 0 {
            return Err(FfmpegError::ZeroDimensions);
        }
        if frames_per_second == 0 {
            return Err(FfmpegError::ZeroFrameRate);
        }
        if stop_timeout.is_zero() {
            return Err(FfmpegError::ZeroStopTimeout);
        }
        let config = Self {
            program: program.into(),
            output: output.into(),
            width,
            height,
            frames_per_second,
            stop_timeout,
        };
        let _ = config.frame_bytes()?;
        Ok(config)
    }

    /// Returns output image width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Returns output image height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Returns expected raw BGR24 byte count for one resized frame.
    ///
    /// # Errors
    ///
    /// Returns an error if dimensions overflow an addressable frame size.
    pub fn frame_bytes(&self) -> Result<usize, FfmpegError> {
        let pixels = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(FfmpegError::FrameSizeOverflow)?;
        pixels.checked_mul(3).ok_or(FfmpegError::FrameSizeOverflow)
    }

    fn arguments(&self) -> Vec<OsString> {
        vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pixel_format".into(),
            "bgr24".into(),
            "-video_size".into(),
            format!("{}x{}", self.width, self.height).into(),
            "-framerate".into(),
            self.frames_per_second.to_string().into(),
            "-i".into(),
            "pipe:0".into(),
            "-an".into(),
            "-c:v".into(),
            VIDEO_CODEC.into(),
            "-preset".into(),
            X264_PRESET.into(),
            "-crf".into(),
            X264_CRF.to_string().into(),
            "-f".into(),
            "matroska".into(),
            self.output.as_os_str().to_owned(),
        ]
    }
}

/// Error produced while configuring, running or closing ffmpeg.
#[derive(Debug)]
pub enum FfmpegError {
    /// A video dimension is zero.
    ZeroDimensions,
    /// Frame rate is zero.
    ZeroFrameRate,
    /// Bounded shutdown timeout is zero.
    ZeroStopTimeout,
    /// Width, height and BGR channels overflow an addressable buffer.
    FrameSizeOverflow,
    /// ffmpeg could not be started or standard input could not be written.
    Io(io::Error),
    /// A caller supplied a frame with a different byte count than the configured BGR24 shape.
    InvalidFrameSize { actual: usize, expected: usize },
    /// ffmpeg returned an unsuccessful status.
    ProcessFailed(String),
    /// ffmpeg did not exit within the configured timeout and was killed.
    StopTimedOut,
}

impl std::fmt::Display for FfmpegError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimensions => formatter.write_str("ffmpeg dimensions must be positive"),
            Self::ZeroFrameRate => formatter.write_str("ffmpeg frame rate must be positive"),
            Self::ZeroStopTimeout => formatter.write_str("ffmpeg stop timeout must be positive"),
            Self::FrameSizeOverflow => formatter.write_str("BGR24 frame size overflows usize"),
            Self::Io(error) => write!(formatter, "ffmpeg I/O: {error}"),
            Self::InvalidFrameSize { actual, expected } => {
                write!(
                    formatter,
                    "invalid BGR24 frame size {actual}; expected {expected}"
                )
            }
            Self::ProcessFailed(status) => {
                write!(formatter, "ffmpeg exited unsuccessfully: {status}")
            }
            Self::StopTimedOut => formatter.write_str("ffmpeg did not stop before its timeout"),
        }
    }
}

impl std::error::Error for FfmpegError {}

impl From<io::Error> for FfmpegError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Running ffmpeg encoder for one capture session.
#[derive(Debug)]
pub struct FfmpegEncoder {
    config: FfmpegConfig,
    child: Child,
    stdin: Option<ChildStdin>,
    frame_count: u64,
}

impl FfmpegEncoder {
    /// Starts ffmpeg and opens its raw BGR24 input stream.
    ///
    /// # Errors
    ///
    /// Returns an error when the executable cannot start or does not expose stdin.
    pub fn start(config: FfmpegConfig) -> Result<Self, FfmpegError> {
        let mut command = Command::new(&config.program);
        command
            .args(config.arguments())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| FfmpegError::Io(io::Error::other("ffmpeg child stdin was not piped")))?;
        Ok(Self {
            config,
            child,
            stdin: Some(stdin),
            frame_count: 0,
        })
    }

    /// Writes exactly one resized BGR24 frame to ffmpeg standard input.
    ///
    /// # Errors
    ///
    /// Returns an error for a mismatched frame shape or broken encoder input.
    pub fn write_frame(&mut self, frame: &[u8]) -> Result<(), FfmpegError> {
        let expected = self.config.frame_bytes()?;
        if frame.len() != expected {
            return Err(FfmpegError::InvalidFrameSize {
                actual: frame.len(),
                expected,
            });
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            FfmpegError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ffmpeg input is already closed",
            ))
        })?;
        stdin.write_all(frame)?;
        self.frame_count = self.frame_count.saturating_add(1);
        Ok(())
    }

    /// Returns the successfully accepted input frame count.
    #[must_use]
    pub const fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// Closes input and waits for a successful ffmpeg exit.
    ///
    /// # Errors
    ///
    /// Returns an error for a nonzero status, I/O failure or stop timeout.
    pub fn finish(mut self) -> Result<u64, FfmpegError> {
        self.stdin.take();
        match wait_for_exit(&mut self.child, self.config.stop_timeout) {
            Ok(()) => Ok(self.frame_count),
            Err(FfmpegError::ProcessFailed(status)) => Err(FfmpegError::ProcessFailed(status)),
            Err(error) => {
                terminate_and_reap(&mut self.child)?;
                Err(error)
            }
        }
    }

    /// Closes input and ensures an incomplete capture process is stopped.
    ///
    /// This never publishes output; the caller must leave the capture directory for
    /// `pcrt-storage` recovery to classify as failed.
    ///
    /// # Errors
    ///
    /// Returns an error if stopping the child fails or exceeds the configured timeout.
    pub fn abort(mut self) -> Result<(), FfmpegError> {
        self.stdin.take();
        match wait_for_exit(&mut self.child, self.config.stop_timeout) {
            Ok(()) | Err(FfmpegError::ProcessFailed(_)) => Ok(()),
            Err(_) => terminate_and_reap(&mut self.child),
        }
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = terminate_and_reap(&mut self.child);
    }
}

fn terminate_and_reap(child: &mut Child) -> Result<(), FfmpegError> {
    match child.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(FfmpegError::Io(error)),
    }
    child.wait().map(|_| ()).map_err(FfmpegError::Io)
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<(), FfmpegError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            return Err(FfmpegError::ProcessFailed(status.to_string()));
        }
        if Instant::now() >= deadline {
            return Err(FfmpegError::StopTimedOut);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    #[cfg(unix)]
    use std::{os::unix::fs::PermissionsExt, process::Command, thread, time::Instant};

    use tempfile::tempdir;

    use super::{FfmpegConfig, FfmpegEncoder, FfmpegError, VIDEO_CODEC, X264_CRF, X264_PRESET};
    use crate::lifecycle::VIDEO_FORMAT;

    #[test]
    fn builds_fixed_h264_matroska_command() {
        let config = FfmpegConfig::new("capture/cam1.mkv", 256, 256, 25).unwrap();
        let arguments = config
            .arguments()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pixel_format",
                "bgr24",
                "-video_size",
                "256x256",
                "-framerate",
                "25",
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                VIDEO_CODEC,
                "-preset",
                X264_PRESET,
                "-crf",
                "18",
                "-f",
                "matroska",
                "capture/cam1.mkv",
            ]
        );
        assert_eq!(VIDEO_FORMAT, "mkv");
        assert_eq!(X264_CRF, 18);
    }

    #[test]
    fn validates_raw_bgr24_frame_size() {
        let config = FfmpegConfig::new("out.mkv", 4, 3, 25).unwrap();

        assert!(matches!(config.frame_bytes(), Ok(36)));
    }

    #[test]
    fn rejects_invalid_configuration() {
        assert!(matches!(
            FfmpegConfig::new("out.mkv", 0, 1, 25),
            Err(FfmpegError::ZeroDimensions)
        ));
        assert!(matches!(
            FfmpegConfig::new("out.mkv", 1, 1, 0),
            Err(FfmpegError::ZeroFrameRate)
        ));
        assert!(matches!(
            FfmpegConfig::with_program_and_timeout(
                PathBuf::from("ffmpeg"),
                "out.mkv",
                1,
                1,
                25,
                Duration::ZERO,
            ),
            Err(FfmpegError::ZeroStopTimeout)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn finish_timeout_kills_and_reaps_child() {
        let directory = tempdir().unwrap();
        let program = directory.path().join("never-exits.sh");
        let pid_file = directory.path().join("child.pid");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > \"{}\"\nwhile :; do :; done\n",
                pid_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let config = FfmpegConfig::with_program_and_timeout(
            &program,
            directory.path().join("out.mkv"),
            1,
            1,
            25,
            Duration::from_millis(20),
        )
        .unwrap();
        let encoder = FfmpegEncoder::start(config).unwrap();
        let pid = wait_for_file(&pid_file);
        let started_at = Instant::now();

        let error = encoder.finish().unwrap_err();

        assert!(matches!(error, FfmpegError::StopTimedOut));
        assert!(started_at.elapsed() < Duration::from_secs(1));
        assert!(
            !Command::new("kill")
                .args(["-0", &pid])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[cfg(unix)]
    fn wait_for_file(path: &std::path::Path) -> String {
        for _ in 0..100 {
            if let Ok(pid) = fs::read_to_string(path) {
                return pid;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {}", path.display());
    }
}
