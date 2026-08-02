use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::PathBuf,
};

use fs2::FileExt;

use crate::DoorZmqError;

pub(crate) fn prepare(endpoint: &str) -> Result<(Option<File>, Option<PathBuf>), DoorZmqError> {
    let Some(raw_path) = endpoint.strip_prefix("ipc://") else {
        return Ok((None, None));
    };
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err(DoorZmqError::IpcPathNotAbsolute(path));
    }
    let parent = path
        .parent()
        .ok_or_else(|| DoorZmqError::IpcPathHasNoParent(path.clone()))?;
    if !parent.is_dir() {
        return Err(DoorZmqError::IpcParentMissing(parent.to_path_buf()));
    }
    let lock_path = path.with_extension("sock.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| DoorZmqError::io("open IPC lock", lock_path, source))?;
    lock.try_lock_exclusive()
        .map_err(|_| DoorZmqError::IpcAlreadyOwned(path.clone()))?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(DoorZmqError::IpcRefusesSymlink(path));
        }
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return Err(DoorZmqError::IpcRefusesNonSocket(path));
        }
        Ok(_) => match UnixStream::connect(&path) {
            Ok(_) => return Err(DoorZmqError::IpcAlreadyOwned(path)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(&path).map_err(|source| {
                    DoorZmqError::io("remove stale IPC socket", path.clone(), source)
                })?;
            }
            Err(source) => return Err(DoorZmqError::io("inspect IPC socket", path, source)),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(DoorZmqError::io("inspect IPC endpoint", path, source)),
    }
    Ok((Some(lock), Some(path)))
}

pub(crate) fn remove_owned(path: PathBuf) -> Result<(), DoorZmqError> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)
            .map_err(|source| DoorZmqError::io("remove owned IPC socket", path, source)),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DoorZmqError::io("inspect owned IPC socket", path, source)),
    }
}
