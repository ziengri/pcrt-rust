#![forbid(unsafe_code)]
//! `ZeroMQ` transport adapters for the PCRT door wire contract.
//!
//! Packet decoding, door state lifecycle and JSON encoding remain in `pcrt-door`.
//! This crate owns only PUB/SUB sockets and secure ownership of IPC socket paths.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::PathBuf,
    time::Instant,
};

use fs2::FileExt;
use pcrt_door::WireMessage;
use serde::Deserialize;

/// A `ZeroMQ` subscription supported by the current door protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoorSubscription {
    Aggregate,
    SelectedDoor(u8),
}

impl DoorSubscription {
    fn topic(self) -> String {
        match self {
            Self::Aggregate => "doors.state".to_owned(),
            Self::SelectedDoor(channel) => format!("door.{channel}.state"),
        }
    }
}

/// Latest valid state received for a [`DoorSubscription`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoorUpdate {
    Aggregate { all_closed: bool, stale: bool },
    SelectedDoor { state: u8, stale: bool },
}

/// `ZeroMQ` PUB socket which exclusively owns an optional IPC endpoint.
pub struct DoorPublisher {
    _context: zmq::Context,
    socket: Option<zmq::Socket>,
    _lock: Option<File>,
    owned_ipc_path: Option<PathBuf>,
}

impl DoorPublisher {
    /// Binds a publisher with the gateway's established socket settings.
    ///
    /// IPC endpoints require an absolute path and exclusive ownership. A stale
    /// socket is removed only after acquiring the endpoint lock.
    ///
    /// # Errors
    ///
    /// Returns an error when `ZeroMQ` cannot bind or IPC ownership is unsafe.
    pub fn bind(endpoint: &str) -> Result<Self, DoorZmqError> {
        let (lock, owned_ipc_path) = prepare_ipc_endpoint(endpoint)?;
        let context = zmq::Context::new();
        let socket = context.socket(zmq::PUB).map_err(DoorZmqError::Zmq)?;
        socket.set_sndhwm(10).map_err(DoorZmqError::Zmq)?;
        socket.set_linger(0).map_err(DoorZmqError::Zmq)?;
        socket.bind(endpoint).map_err(DoorZmqError::Zmq)?;
        Ok(Self {
            _context: context,
            socket: Some(socket),
            _lock: lock,
            owned_ipc_path,
        })
    }

    /// Sends one already encoded protocol message without blocking the gateway.
    ///
    /// # Errors
    ///
    /// Returns a `ZeroMQ` send failure. The caller chooses whether to retry or
    /// continue publishing later messages from the same snapshot.
    pub fn publish(&self, message: &WireMessage) -> Result<(), DoorZmqError> {
        let Some(socket) = &self.socket else {
            return Err(DoorZmqError::PublisherClosed);
        };
        socket
            .send(message.as_frame(), zmq::DONTWAIT)
            .map_err(DoorZmqError::Zmq)
    }

    /// Closes the socket and removes an IPC socket owned by this publisher.
    ///
    /// # Errors
    ///
    /// Returns a filesystem error while removing the owned IPC socket.
    pub fn close(mut self) -> Result<(), DoorZmqError> {
        self.socket.take();
        self.cleanup_owned_ipc_path()
    }

    fn cleanup_owned_ipc_path(&mut self) -> Result<(), DoorZmqError> {
        let Some(path) = self.owned_ipc_path.take() else {
            return Ok(());
        };
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&path)
                .map_err(|source| DoorZmqError::io("remove owned IPC socket", path, source)),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(DoorZmqError::io("inspect owned IPC socket", path, source)),
        }
    }
}

impl Drop for DoorPublisher {
    fn drop(&mut self) {
        self.socket.take();
        let _ = self.cleanup_owned_ipc_path();
    }
}

/// `ZeroMQ` SUB socket retaining only the latest valid subscribed door state.
pub struct DoorSubscriber {
    _context: zmq::Context,
    socket: zmq::Socket,
    topic: String,
    subscription: DoorSubscription,
    latest: Option<DoorUpdate>,
    latest_received_at: Option<Instant>,
}

impl DoorSubscriber {
    /// Connects to one exact aggregate or selected-door topic.
    ///
    /// # Errors
    ///
    /// Returns a `ZeroMQ` socket setup or connect failure.
    pub fn connect(endpoint: &str, subscription: DoorSubscription) -> Result<Self, DoorZmqError> {
        Self::connect_with_context(zmq::Context::new(), endpoint, subscription)
    }

    fn connect_with_context(
        context: zmq::Context,
        endpoint: &str,
        subscription: DoorSubscription,
    ) -> Result<Self, DoorZmqError> {
        let socket = context.socket(zmq::SUB).map_err(DoorZmqError::Zmq)?;
        let topic = subscription.topic();
        socket
            .set_subscribe(topic.as_bytes())
            .map_err(DoorZmqError::Zmq)?;
        socket.set_rcvhwm(10).map_err(DoorZmqError::Zmq)?;
        socket.connect(endpoint).map_err(DoorZmqError::Zmq)?;
        Ok(Self {
            _context: context,
            socket,
            topic,
            subscription,
            latest: None,
            latest_received_at: None,
        })
    }

    /// Drains currently available frames and retains the latest valid update.
    ///
    /// Invalid UTF-8, wrong topic and malformed payloads are ignored without
    /// replacing the last valid update. `EAGAIN` means the socket is drained.
    ///
    /// # Errors
    ///
    /// Returns an unexpected `ZeroMQ` receive error.
    pub fn drain(&mut self) -> Result<(), DoorZmqError> {
        loop {
            let frame = match self.socket.recv_string(zmq::DONTWAIT) {
                Ok(Ok(frame)) => frame,
                Ok(Err(_)) => continue,
                Err(zmq::Error::EAGAIN) => return Ok(()),
                Err(error) => return Err(DoorZmqError::Zmq(error)),
            };
            let Some((topic, payload)) = frame.split_once(' ') else {
                continue;
            };
            if topic != self.topic {
                continue;
            }
            let update = match self.subscription {
                DoorSubscription::Aggregate => serde_json::from_str::<AggregatePayload>(payload)
                    .ok()
                    .map(|message| DoorUpdate::Aggregate {
                        all_closed: message.all_closed,
                        stale: message.stale,
                    }),
                DoorSubscription::SelectedDoor(_) => {
                    serde_json::from_str::<SelectedDoorPayload>(payload)
                        .ok()
                        .map(|message| DoorUpdate::SelectedDoor {
                            state: message.state,
                            stale: message.stale,
                        })
                }
            };
            if let Some(update) = update {
                self.latest = Some(update);
                self.latest_received_at = Some(Instant::now());
            }
        }
    }

    /// Returns the latest valid update without applying any freshness policy.
    #[must_use]
    pub const fn latest(&self) -> Option<DoorUpdate> {
        self.latest
    }

    /// Returns when the latest valid update was received on this process.
    ///
    /// Invalid frames and an empty `drain` never refresh this timestamp.
    #[must_use]
    pub const fn latest_received_at(&self) -> Option<Instant> {
        self.latest_received_at
    }
}

#[derive(Deserialize)]
struct AggregatePayload {
    all_closed: bool,
    stale: bool,
}

#[derive(Deserialize)]
struct SelectedDoorPayload {
    state: u8,
    stale: bool,
}

/// Errors from `ZeroMQ` transport and IPC endpoint ownership.
#[derive(Debug)]
pub enum DoorZmqError {
    Zmq(zmq::Error),
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
    fn io(action: &'static str, path: PathBuf, source: io::Error) -> Self {
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

fn prepare_ipc_endpoint(endpoint: &str) -> Result<(Option<File>, Option<PathBuf>), DoorZmqError> {
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
            Err(source) => {
                return Err(DoorZmqError::io("inspect IPC socket", path, source));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(DoorZmqError::io("inspect IPC endpoint", path, source)),
    }
    Ok((Some(lock), Some(path)))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        os::unix::{fs::symlink, net::UnixListener},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{
        DoorPublisher, DoorSubscriber, DoorSubscription, DoorUpdate, prepare_ipc_endpoint,
    };

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn publisher_never_lingers_on_shutdown() {
        let publisher =
            DoorPublisher::bind(&format!("inproc://pcrt-door-test-{}", std::process::id()))
                .unwrap();

        assert_eq!(publisher.socket.as_ref().unwrap().get_linger().unwrap(), 0);
        publisher.close().unwrap();
    }

    #[test]
    fn subscriber_retains_latest_valid_selected_door_update() {
        let endpoint = format!("inproc://pcrt-door-sub-test-{}", std::process::id());
        let context = zmq::Context::new();
        let publisher = context.socket(zmq::PUB).unwrap();
        publisher.bind(&endpoint).unwrap();
        let mut subscriber = DoorSubscriber::connect_with_context(
            context.clone(),
            &endpoint,
            DoorSubscription::SelectedDoor(2),
        )
        .unwrap();

        thread::sleep(Duration::from_millis(50));
        publisher
            .send(r#"door.1.state {"state":1,"stale":false}"#, 0)
            .unwrap();
        publisher.send("door.2.state {bad json}", 0).unwrap();
        publisher
            .send(r#"door.2.state {"state":1,"stale":false}"#, 0)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        subscriber.drain().unwrap();
        assert_eq!(
            subscriber.latest(),
            Some(DoorUpdate::SelectedDoor {
                state: 1,
                stale: false,
            })
        );
        let received_at = subscriber.latest_received_at().unwrap();

        publisher.send("door.2.state {bad json}", 0).unwrap();
        thread::sleep(Duration::from_millis(50));
        subscriber.drain().unwrap();
        assert_eq!(subscriber.latest_received_at(), Some(received_at));

        publisher
            .send(r#"door.2.state {"state":1,"stale":true}"#, 0)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        subscriber.drain().unwrap();
        assert_eq!(
            subscriber.latest(),
            Some(DoorUpdate::SelectedDoor {
                state: 1,
                stale: true,
            })
        );
    }

    #[test]
    fn aggregate_subscriber_reads_processing_gate_fields() {
        let endpoint = format!("inproc://pcrt-door-aggregate-test-{}", std::process::id());
        let context = zmq::Context::new();
        let publisher = context.socket(zmq::PUB).unwrap();
        publisher.bind(&endpoint).unwrap();
        let mut subscriber = DoorSubscriber::connect_with_context(
            context.clone(),
            &endpoint,
            DoorSubscription::Aggregate,
        )
        .unwrap();

        thread::sleep(Duration::from_millis(50));
        publisher
            .send(r#"doors.state {"all_closed":true,"stale":false}"#, 0)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        subscriber.drain().unwrap();

        assert_eq!(
            subscriber.latest(),
            Some(DoorUpdate::Aggregate {
                all_closed: true,
                stale: false,
            })
        );
        assert!(subscriber.latest_received_at().is_some());
    }

    #[test]
    fn ipc_refuses_regular_file_and_symlink() {
        let paths = IpcTestPaths::new();
        fs::File::create(&paths.socket)
            .unwrap()
            .write_all(b"not a socket")
            .unwrap();
        assert!(prepare_ipc_endpoint(&paths.endpoint()).is_err());
        fs::remove_file(&paths.socket).unwrap();
        symlink(&paths.target, &paths.socket).unwrap();
        assert!(prepare_ipc_endpoint(&paths.endpoint()).is_err());
    }

    #[test]
    fn ipc_removes_only_stale_socket_after_lock() {
        let paths = IpcTestPaths::new();
        let listener = UnixListener::bind(&paths.socket).unwrap();
        drop(listener);
        assert!(paths.socket.exists());

        let (lock, owned) = prepare_ipc_endpoint(&paths.endpoint()).unwrap();

        assert!(lock.is_some());
        assert_eq!(owned.as_deref(), Some(paths.socket.as_path()));
        assert!(!paths.socket.exists());
    }

    #[test]
    fn ipc_lock_rejects_second_gateway_before_socket_is_touched() {
        let paths = IpcTestPaths::new();
        let first = prepare_ipc_endpoint(&paths.endpoint()).unwrap();

        assert!(prepare_ipc_endpoint(&paths.endpoint()).is_err());

        drop(first);
    }

    struct IpcTestPaths {
        socket: PathBuf,
        target: PathBuf,
    }

    impl IpcTestPaths {
        fn new() -> Self {
            let suffix = format!(
                "pcrt-door-ipc-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed)
            );
            let root = std::env::temp_dir();
            Self {
                socket: root.join(format!("{suffix}.sock")),
                target: root.join(format!("{suffix}.target")),
            }
        }

        fn endpoint(&self) -> String {
            format!("ipc://{}", self.socket.display())
        }
    }

    impl Drop for IpcTestPaths {
        fn drop(&mut self) {
            for path in [
                &self.socket,
                &self.target,
                &self.socket.with_extension("sock.lock"),
            ] {
                if path.exists() || path.is_symlink() {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}
