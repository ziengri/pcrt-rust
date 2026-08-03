//! ffmpeg raw BGR24 encoder for one capture video.

use std::{
    ffi::OsString,
    io::{self, Write},
    os::fd::AsFd,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    poll::{PollFd, PollFlags, PollTimeout, poll},
};
use pcrt_recording::{
    lifecycle::{VIDEO_CODEC, X264_CRF, X264_PRESET},
    recorder::{EncoderFactory, FrameEncoder},
};
use pcrt_service::ShutdownToken;

/// How long `finish` or `abort` waits before killing an unresponsive ffmpeg child.
const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time to deliver one frame before treating the encoder as unresponsive.
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// Maximum interval between shutdown checks while the ffmpeg input pipe is full.
const WRITE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Validated raw-frame encoding parameters for one output video.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FfmpegConfig {
    program: PathBuf,
    output: PathBuf,
    width: u32,
    height: u32,
    frames_per_second: u32,
    write_timeout: Duration,
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
        Self::with_program_and_timeouts(
            "ffmpeg",
            output,
            width,
            height,
            frames_per_second,
            DEFAULT_WRITE_TIMEOUT,
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
    fn with_program_and_timeouts(
        program: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
        width: u32,
        height: u32,
        frames_per_second: u32,
        write_timeout: Duration,
        stop_timeout: Duration,
    ) -> Result<Self, FfmpegError> {
        if width == 0 || height == 0 {
            return Err(FfmpegError::ZeroDimensions);
        }
        if frames_per_second == 0 {
            return Err(FfmpegError::ZeroFrameRate);
        }
        if write_timeout.is_zero() {
            return Err(FfmpegError::ZeroWriteTimeout);
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
            write_timeout,
            stop_timeout,
        };
        let _ = config.frame_bytes()?;
        Ok(config)
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
enum FfmpegError {
    /// A video dimension is zero.
    ZeroDimensions,
    /// Frame rate is zero.
    ZeroFrameRate,
    /// Bounded shutdown timeout is zero.
    ZeroStopTimeout,
    /// Bounded frame-write timeout is zero.
    ZeroWriteTimeout,
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
    /// ffmpeg did not accept a complete frame before the configured deadline.
    InputTimedOut,
    /// Service shutdown was requested while waiting for ffmpeg input.
    ShutdownRequested,
}

impl std::fmt::Display for FfmpegError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimensions => formatter.write_str("ffmpeg dimensions must be positive"),
            Self::ZeroFrameRate => formatter.write_str("ffmpeg frame rate must be positive"),
            Self::ZeroStopTimeout => formatter.write_str("ffmpeg stop timeout must be positive"),
            Self::ZeroWriteTimeout => formatter.write_str("ffmpeg write timeout must be positive"),
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
            Self::InputTimedOut => {
                formatter.write_str("ffmpeg did not accept a frame before its timeout")
            }
            Self::ShutdownRequested => {
                formatter.write_str("recorder shutdown interrupted ffmpeg frame write")
            }
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
struct FfmpegEncoder {
    config: FfmpegConfig,
    child: Child,
    stdin: Option<ChildStdin>,
    shutdown: ShutdownToken,
    input_unresponsive: bool,
    frame_count: u64,
}

/// Production factory for the fixed `libx264` ffmpeg encoder.
#[derive(Clone, Debug)]
pub(crate) struct FfmpegEncoderFactory {
    shutdown: ShutdownToken,
}

impl FfmpegEncoderFactory {
    pub(crate) const fn new(shutdown: ShutdownToken) -> Self {
        Self { shutdown }
    }
}

impl EncoderFactory for FfmpegEncoderFactory {
    fn start(
        &self,
        output: &std::path::Path,
        width: u32,
        height: u32,
        frames_per_second: u32,
    ) -> Result<Box<dyn FrameEncoder>, String> {
        let config = FfmpegConfig::new(output, width, height, frames_per_second)
            .map_err(|error| error.to_string())?;
        FfmpegEncoder::start(config, self.shutdown.clone())
            .map(|encoder| Box::new(encoder) as Box<dyn FrameEncoder>)
            .map_err(|error| error.to_string())
    }
}

impl FrameEncoder for FfmpegEncoder {
    fn write_frame(&mut self, frame: &[u8]) -> Result<(), String> {
        self.write_frame(frame).map_err(|error| error.to_string())
    }

    fn finish(self: Box<Self>) -> Result<u64, String> {
        (*self).finish().map_err(|error| error.to_string())
    }

    fn abort(self: Box<Self>) -> Result<(), String> {
        (*self).abort().map_err(|error| error.to_string())
    }
}

impl FfmpegEncoder {
    /// Starts ffmpeg and opens its raw BGR24 input stream.
    ///
    /// # Errors
    ///
    /// Returns an error when the executable cannot start or does not expose stdin.
    fn start(config: FfmpegConfig, shutdown: ShutdownToken) -> Result<Self, FfmpegError> {
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
        if let Err(error) = set_nonblocking(&stdin) {
            drop(stdin);
            let _ = terminate_and_reap(&mut child);
            return Err(error);
        }
        Ok(Self {
            config,
            child,
            stdin: Some(stdin),
            shutdown,
            input_unresponsive: false,
            frame_count: 0,
        })
    }

    /// Writes exactly one resized BGR24 frame to ffmpeg standard input.
    ///
    /// # Errors
    ///
    /// Returns an error for a mismatched frame shape, unavailable encoder input,
    /// shutdown request, or an unresponsive encoder.
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
        let result =
            write_frame_with_deadline(stdin, frame, &self.shutdown, self.config.write_timeout);
        if matches!(
            result,
            Err(FfmpegError::InputTimedOut | FfmpegError::ShutdownRequested)
        ) {
            self.input_unresponsive = true;
        }
        result?;
        self.frame_count = self.frame_count.saturating_add(1);
        Ok(())
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
        if self.input_unresponsive {
            return terminate_and_reap(&mut self.child);
        }
        match wait_for_exit(&mut self.child, self.config.stop_timeout) {
            Ok(()) | Err(FfmpegError::ProcessFailed(_)) => Ok(()),
            Err(_) => terminate_and_reap(&mut self.child),
        }
    }
}

fn set_nonblocking(stdin: &ChildStdin) -> Result<(), FfmpegError> {
    let flags = fcntl(stdin, FcntlArg::F_GETFL).map_err(nix_to_io)?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(stdin, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(nix_to_io)?;
    Ok(())
}

fn write_frame_with_deadline(
    stdin: &mut ChildStdin,
    frame: &[u8],
    shutdown: &ShutdownToken,
    timeout: Duration,
) -> Result<(), FfmpegError> {
    let deadline = Instant::now() + timeout;
    let mut written = 0;
    while written < frame.len() {
        if shutdown.is_shutdown_requested() {
            return Err(FfmpegError::ShutdownRequested);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(FfmpegError::InputTimedOut);
        }
        let remaining = deadline.saturating_duration_since(now);
        let poll_timeout = PollTimeout::try_from(remaining.min(WRITE_POLL_INTERVAL))
            .map_err(|error| FfmpegError::Io(io::Error::other(error)))?;
        let events = {
            let mut poll_fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLOUT)];
            match poll(&mut poll_fds, poll_timeout) {
                Ok(_) => poll_fds[0].revents().unwrap_or_else(PollFlags::empty),
                Err(Errno::EINTR) => continue,
                Err(error) => return Err(nix_to_io(error)),
            }
        };
        if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            return Err(FfmpegError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ffmpeg input pipe is closed",
            )));
        }
        if !events.contains(PollFlags::POLLOUT) {
            continue;
        }
        match stdin.write(&frame[written..]) {
            Ok(0) => {
                return Err(FfmpegError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "ffmpeg input pipe accepted no frame data",
                )));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FfmpegError::Io(error)),
        }
    }
    Ok(())
}

fn nix_to_io(error: Errno) -> FfmpegError {
    FfmpegError::Io(error.into())
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
    use pcrt_recording::lifecycle::VIDEO_FORMAT;
    use pcrt_service::ShutdownToken;

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
            FfmpegConfig::with_program_and_timeouts(
                PathBuf::from("ffmpeg"),
                "out.mkv",
                1,
                1,
                25,
                Duration::from_secs(1),
                Duration::ZERO,
            ),
            Err(FfmpegError::ZeroStopTimeout)
        ));
        assert!(matches!(
            FfmpegConfig::with_program_and_timeouts(
                PathBuf::from("ffmpeg"),
                "out.mkv",
                1,
                1,
                25,
                Duration::ZERO,
                Duration::from_secs(1),
            ),
            Err(FfmpegError::ZeroWriteTimeout)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn finish_timeout_kills_and_reaps_child() {
        let directory = tempdir().unwrap();
        let program = directory.path().join("never-exits.sh");
        fs::write(&program, "#!/bin/sh\nexec sleep 1000\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let config = FfmpegConfig::with_program_and_timeouts(
            &program,
            directory.path().join("out.mkv"),
            1,
            1,
            25,
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .unwrap();
        let encoder = FfmpegEncoder::start(config, ShutdownToken::default()).unwrap();
        let pid = encoder.child.id().to_string();
        let started_at = Instant::now();

        let error = encoder.finish().unwrap_err();

        assert!(matches!(error, FfmpegError::StopTimedOut));
        assert!(started_at.elapsed() < Duration::from_secs(1));
        assert_child_exited(&pid);
    }

    #[cfg(unix)]
    #[test]
    fn frame_write_times_out_when_child_stops_reading_input() {
        let directory = tempdir().unwrap();
        let program = non_reading_child(directory.path());
        let config = FfmpegConfig::with_program_and_timeouts(
            &program,
            directory.path().join("out.mkv"),
            1024,
            1024,
            25,
            Duration::from_millis(50),
            Duration::from_secs(1),
        )
        .unwrap();
        let mut encoder = FfmpegEncoder::start(config, ShutdownToken::default()).unwrap();
        let pid = encoder.child.id().to_string();
        let frame = vec![0; 1024 * 1024 * 3];
        let started_at = Instant::now();

        let error = encoder.write_frame(&frame).unwrap_err();

        assert!(matches!(error, FfmpegError::InputTimedOut));
        assert!(started_at.elapsed() < Duration::from_secs(1));
        encoder.abort().unwrap();
        assert_child_exited(&pid);
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_interrupts_waiting_for_ffmpeg_input() {
        let directory = tempdir().unwrap();
        let program = non_reading_child(directory.path());
        let config = FfmpegConfig::with_program_and_timeouts(
            &program,
            directory.path().join("out.mkv"),
            1024,
            1024,
            25,
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let shutdown = ShutdownToken::default();
        let signaler = shutdown.clone();
        let request = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            signaler.request_shutdown();
        });
        let mut encoder = FfmpegEncoder::start(config, shutdown).unwrap();
        let pid = encoder.child.id().to_string();
        let frame = vec![0; 1024 * 1024 * 3];
        let started_at = Instant::now();

        let error = encoder.write_frame(&frame).unwrap_err();

        request.join().unwrap();
        assert!(matches!(error, FfmpegError::ShutdownRequested));
        assert!(started_at.elapsed() < Duration::from_secs(1));
        encoder.abort().unwrap();
        assert_child_exited(&pid);
    }

    #[cfg(unix)]
    fn non_reading_child(directory: &std::path::Path) -> PathBuf {
        let program = directory.join("never-reads.sh");
        fs::write(&program, "#!/bin/sh\nexec sleep 1000\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    #[cfg(unix)]
    fn assert_child_exited(pid: &str) {
        assert!(
            !Command::new("kill")
                .args(["-0", pid])
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}
