#![forbid(unsafe_code)]
//! `ZeroMQ` transport adapters for the PCRT door wire contract.
//!
//! This crate owns the shared door-bus state, PUB/SUB sockets and IPC socket paths.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use pcrt_model::door::{DoorId, DoorState, DoorTelemetry};
use serde::{Deserialize, Serialize};

/// Complete shared state published on the door bus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorsState {
    sequence: u64,
    doors: BTreeMap<DoorId, DoorTelemetry>,
    stale: bool,
}

impl DoorsState {
    /// Creates a complete state produced by the gateway state machine.
    #[must_use]
    pub fn new(sequence: u64, doors: BTreeMap<DoorId, DoorTelemetry>, stale: bool) -> Self {
        Self {
            sequence,
            doors,
            stale,
        }
    }

    /// Sequence changed only by accepted packets and stale transitions.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Complete telemetry for every configured door.
    #[must_use]
    pub const fn doors(&self) -> &BTreeMap<DoorId, DoorTelemetry> {
        &self.doors
    }

    /// Whether telemetry is expired or has not yet been received.
    #[must_use]
    pub const fn stale(&self) -> bool {
        self.stale
    }

    /// Returns telemetry for one door.
    #[must_use]
    pub fn door(&self, door_id: DoorId) -> Option<&DoorTelemetry> {
        self.doors.get(&door_id)
    }

    /// Whether any configured door is open.
    #[must_use]
    pub fn any_open(&self) -> bool {
        self.doors
            .values()
            .any(|telemetry| telemetry.state == DoorState::Open)
    }

    /// Whether all configured doors are closed.
    #[must_use]
    pub fn all_closed(&self) -> bool {
        !self.any_open()
    }
}

/// Latest aggregate state together with the local subscriber receipt time.
#[derive(Clone, Debug)]
pub struct ReceivedDoorsState {
    state: DoorsState,
    received_at: Instant,
}

impl ReceivedDoorsState {
    /// Returns the validated aggregate state.
    #[must_use]
    pub const fn state(&self) -> &DoorsState {
        &self.state
    }

    /// Returns the local monotonic receipt time for consumer TTL policy.
    #[must_use]
    pub const fn received_at(&self) -> Instant {
        self.received_at
    }
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

    /// Encodes and publishes aggregate and per-door compatibility messages.
    ///
    /// # Errors
    ///
    /// Returns a `ZeroMQ` or JSON encoding failure. The caller chooses whether to
    /// retry later; a failure never changes the supplied state.
    pub fn publish(&self, state: &DoorsState) -> Result<(), DoorZmqError> {
        let Some(socket) = &self.socket else {
            return Err(DoorZmqError::PublisherClosed);
        };
        let timestamp = epoch_seconds();
        let aggregate = encode_aggregate(state, timestamp)?;
        socket
            .send(&aggregate, zmq::DONTWAIT)
            .map_err(DoorZmqError::Zmq)?;
        for (door_id, telemetry) in state.doors() {
            let frame = encode_selected(*door_id, *telemetry, state, timestamp)?;
            socket
                .send(&frame, zmq::DONTWAIT)
                .map_err(DoorZmqError::Zmq)?;
        }
        Ok(())
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

/// `ZeroMQ` SUB socket retaining only the latest valid aggregate state.
pub struct AggregateDoorSubscriber {
    _context: zmq::Context,
    socket: zmq::Socket,
    latest: Option<ReceivedDoorsState>,
}

impl AggregateDoorSubscriber {
    /// Connects to the aggregate `doors.state` topic.
    ///
    /// # Errors
    ///
    /// Returns a `ZeroMQ` socket setup or connect failure.
    pub fn connect(endpoint: &str) -> Result<Self, DoorZmqError> {
        Self::connect_with_context(zmq::Context::new(), endpoint)
    }

    fn connect_with_context(context: zmq::Context, endpoint: &str) -> Result<Self, DoorZmqError> {
        let socket = context.socket(zmq::SUB).map_err(DoorZmqError::Zmq)?;
        socket
            .set_subscribe(b"doors.state")
            .map_err(DoorZmqError::Zmq)?;
        socket.set_rcvhwm(10).map_err(DoorZmqError::Zmq)?;
        socket.connect(endpoint).map_err(DoorZmqError::Zmq)?;
        Ok(Self {
            _context: context,
            socket,
            latest: None,
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
            if topic != "doors.state" {
                continue;
            }
            if let Some(state) = decode_aggregate(payload) {
                self.latest = Some(ReceivedDoorsState {
                    state,
                    received_at: Instant::now(),
                });
            }
        }
    }

    /// Returns the latest valid update without applying any freshness policy.
    #[must_use]
    pub const fn latest(&self) -> Option<&ReceivedDoorsState> {
        self.latest.as_ref()
    }
}

#[derive(Deserialize)]
struct AggregatePayload {
    seq: u64,
    #[allow(dead_code)]
    ts: f64,
    doors: BTreeMap<String, TelemetryPayload>,
    any_open: bool,
    all_closed: bool,
    stale: bool,
}

#[derive(Deserialize)]
struct TelemetryPayload {
    state: u8,
    voltage: u16,
}

#[derive(Serialize)]
struct AggregateWire {
    seq: u64,
    ts: f64,
    doors: BTreeMap<String, TelemetryWire>,
    any_open: bool,
    all_closed: bool,
    stale: bool,
}

#[derive(Serialize)]
struct SelectedWire {
    seq: u64,
    ts: f64,
    door_id: u8,
    state: u8,
    voltage: u16,
    stale: bool,
}

#[derive(Serialize)]
struct TelemetryWire {
    state: u8,
    voltage: u16,
}

fn encode_aggregate(state: &DoorsState, timestamp: f64) -> Result<String, DoorZmqError> {
    let doors = state
        .doors()
        .iter()
        .map(|(door_id, telemetry)| {
            (
                door_id.get().to_string(),
                TelemetryWire {
                    state: telemetry.state.protocol_byte(),
                    voltage: telemetry.voltage_raw,
                },
            )
        })
        .collect();
    let payload = AggregateWire {
        seq: state.sequence(),
        ts: timestamp,
        doors,
        any_open: state.any_open(),
        all_closed: state.all_closed(),
        stale: state.stale(),
    };
    serde_json::to_string(&payload)
        .map(|payload| format!("doors.state {payload}"))
        .map_err(DoorZmqError::Json)
}

fn encode_selected(
    door_id: DoorId,
    telemetry: DoorTelemetry,
    state: &DoorsState,
    timestamp: f64,
) -> Result<String, DoorZmqError> {
    let payload = SelectedWire {
        seq: state.sequence(),
        ts: timestamp,
        door_id: door_id.get(),
        state: telemetry.state.protocol_byte(),
        voltage: telemetry.voltage_raw,
        stale: state.stale(),
    };
    serde_json::to_string(&payload)
        .map(|payload| format!("door.{}.state {payload}", door_id.get()))
        .map_err(DoorZmqError::Json)
}

fn decode_aggregate(payload: &str) -> Option<DoorsState> {
    let payload = serde_json::from_str::<AggregatePayload>(payload).ok()?;
    if !payload.ts.is_finite() {
        return None;
    }
    let mut doors = BTreeMap::new();
    for (raw_id, telemetry) in payload.doors {
        let door_id = raw_id.parse::<u8>().ok().and_then(DoorId::new)?;
        let state = DoorState::from_protocol_byte(telemetry.state)?;
        if doors
            .insert(
                door_id,
                DoorTelemetry {
                    state,
                    voltage_raw: telemetry.voltage,
                },
            )
            .is_some()
        {
            return None;
        }
    }
    if !(3..=4).contains(&doors.len())
        || !doors
            .keys()
            .copied()
            .zip(1_u8..)
            .all(|(door_id, expected)| door_id.get() == expected)
    {
        return None;
    }
    let state = DoorsState::new(payload.seq, doors, payload.stale);
    (state.any_open() == payload.any_open && state.all_closed() == payload.all_closed)
        .then_some(state)
}

fn epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

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

    use super::{AggregateDoorSubscriber, DoorPublisher, prepare_ipc_endpoint};

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
    fn subscriber_retains_latest_valid_aggregate_state() {
        let endpoint = format!("inproc://pcrt-door-sub-test-{}", std::process::id());
        let context = zmq::Context::new();
        let publisher = context.socket(zmq::PUB).unwrap();
        publisher.bind(&endpoint).unwrap();
        let mut subscriber =
            AggregateDoorSubscriber::connect_with_context(context.clone(), &endpoint).unwrap();

        thread::sleep(Duration::from_millis(50));
        publisher
            .send(r#"doors.state {"seq":1,"ts":1.0,"doors":{"1":{"state":1,"voltage":42},"2":{"state":0,"voltage":7},"3":{"state":0,"voltage":8}},"any_open":true,"all_closed":false,"stale":false}"#, 0)
            .unwrap();
        publisher.send("doors.state {bad json}", 0).unwrap();
        publisher
            .send(r#"doors.state {"seq":2,"ts":1.0,"doors":{"1":{"state":0,"voltage":6},"2":{"state":0,"voltage":7},"3":{"state":0,"voltage":8}},"any_open":false,"all_closed":true,"stale":false}"#, 0)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        subscriber.drain().unwrap();
        let latest = subscriber.latest().unwrap();
        assert_eq!(latest.state().sequence(), 2);
        assert!(latest.state().all_closed());

        publisher
            .send(r#"doors.state {"seq":3,"ts":1.0,"doors":{"1":{"state":0,"voltage":6},"2":{"state":0,"voltage":7},"3":{"state":0,"voltage":8}},"any_open":true,"all_closed":true,"stale":true}"#, 0)
            .unwrap();
        thread::sleep(Duration::from_millis(50));
        subscriber.drain().unwrap();
        assert_eq!(subscriber.latest().unwrap().state().sequence(), 2);
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
