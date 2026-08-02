use std::{io, path::PathBuf};

/// Errors from `ZeroMQ` transport and IPC endpoint ownership.
#[derive(Debug)]
pub enum DoorZmqError {
    Zmq(zmq::Error),
    Json(serde_json::Error),
    PublisherClosed,
    IpcPathNotAbsolute(PathBuf),
    IpcPathHasNoParent(PathBuf),
    IpcParentMissing(PathBuf),
    IpcAlreadyOwned(PathBuf),
    IpcRefusesSymlink(PathBuf),
    IpcRefusesNonSocket(PathBuf),
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl DoorZmqError {
    pub(crate) fn io(action: &'static str, path: PathBuf, source: io::Error) -> Self {
        Self::Io {
            action,
            path,
            source,
        }
    }
}

impl core::fmt::Display for DoorZmqError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Zmq(error) => write!(formatter, "ZeroMQ: {error}"),
            Self::Json(error) => write!(formatter, "door JSON: {error}"),
            Self::PublisherClosed => formatter.write_str("door publisher is closed"),
            Self::IpcPathNotAbsolute(path) => {
                write!(
                    formatter,
                    "IPC endpoint path must be absolute: {}",
                    path.display()
                )
            }
            Self::IpcPathHasNoParent(path) => {
                write!(
                    formatter,
                    "IPC endpoint has no parent directory: {}",
                    path.display()
                )
            }
            Self::IpcParentMissing(path) => {
                write!(
                    formatter,
                    "IPC parent directory does not exist: {}",
                    path.display()
                )
            }
            Self::IpcAlreadyOwned(path) => {
                write!(
                    formatter,
                    "IPC endpoint is already owned: {}",
                    path.display()
                )
            }
            Self::IpcRefusesSymlink(path) => {
                write!(
                    formatter,
                    "refusing symlink IPC endpoint: {}",
                    path.display()
                )
            }
            Self::IpcRefusesNonSocket(path) => {
                write!(
                    formatter,
                    "refusing non-socket IPC endpoint: {}",
                    path.display()
                )
            }
            Self::Io {
                action,
                path,
                source,
            } => write!(formatter, "cannot {action} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for DoorZmqError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Zmq(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::PublisherClosed
            | Self::IpcPathNotAbsolute(_)
            | Self::IpcPathHasNoParent(_)
            | Self::IpcParentMissing(_)
            | Self::IpcAlreadyOwned(_)
            | Self::IpcRefusesSymlink(_)
            | Self::IpcRefusesNonSocket(_) => None,
        }
    }
}
