//! Exclusive ownership of one session root during recovery and processing.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use fs2::FileExt;

const LOCK_FILE: &str = ".pcrt-processor.lock";

pub(crate) struct ProcessorLock {
    _file: File,
}

impl ProcessorLock {
    pub(crate) fn acquire(sessions_dir: &Path) -> Result<Self, ProcessorLockError> {
        fs::create_dir_all(sessions_dir).map_err(|source| ProcessorLockError::Io {
            action: "create sessions directory for processor lock",
            path: sessions_dir.to_path_buf(),
            source,
        })?;
        let path = sessions_dir.join(LOCK_FILE);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ProcessorLockError::RefusesSymlink(path));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ProcessorLockError::Io {
                    action: "inspect processor lock",
                    path,
                    source,
                });
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| ProcessorLockError::Io {
                action: "open processor lock",
                path: path.clone(),
                source,
            })?;
        file.try_lock_exclusive()
            .map_err(|_| ProcessorLockError::AlreadyOwned(path))?;
        Ok(Self { _file: file })
    }
}

#[derive(Debug)]
pub(crate) enum ProcessorLockError {
    AlreadyOwned(PathBuf),
    RefusesSymlink(PathBuf),
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl std::fmt::Display for ProcessorLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyOwned(path) => {
                write!(
                    formatter,
                    "processor is already running for {}",
                    path.display()
                )
            }
            Self::RefusesSymlink(path) => {
                write!(
                    formatter,
                    "refusing symlink processor lock: {}",
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

impl std::error::Error for ProcessorLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyOwned(_) | Self::RefusesSymlink(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::ProcessorLock;

    #[test]
    fn rejects_second_owner_and_symlink_lock() {
        let directory = tempdir().unwrap();
        let first = ProcessorLock::acquire(directory.path()).unwrap();
        assert!(ProcessorLock::acquire(directory.path()).is_err());
        drop(first);

        let lock_path = directory.path().join(".pcrt-processor.lock");
        fs::remove_file(&lock_path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(directory.path().join("target"), &lock_path).unwrap();
        #[cfg(unix)]
        assert!(ProcessorLock::acquire(directory.path()).is_err());
    }
}
