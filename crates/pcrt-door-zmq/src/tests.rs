use std::{
    fs,
    io::Write,
    os::unix::{fs::symlink, net::UnixListener},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{AggregateDoorSubscriber, DoorPublisher, ipc_endpoint};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn publisher_never_lingers_on_shutdown() {
    let publisher =
        DoorPublisher::bind(&format!("inproc://pcrt-door-test-{}", std::process::id())).unwrap();

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
    assert!(ipc_endpoint::prepare(&paths.endpoint()).is_err());
    fs::remove_file(&paths.socket).unwrap();
    symlink(&paths.target, &paths.socket).unwrap();
    assert!(ipc_endpoint::prepare(&paths.endpoint()).is_err());
}

#[test]
fn ipc_removes_only_stale_socket_after_lock() {
    let paths = IpcTestPaths::new();
    let listener = UnixListener::bind(&paths.socket).unwrap();
    drop(listener);
    assert!(paths.socket.exists());

    let (lock, owned) = ipc_endpoint::prepare(&paths.endpoint()).unwrap();

    assert!(lock.is_some());
    assert_eq!(owned.as_deref(), Some(paths.socket.as_path()));
    assert!(!paths.socket.exists());
}

#[test]
fn ipc_lock_rejects_second_gateway_before_socket_is_touched() {
    let paths = IpcTestPaths::new();
    let first = ipc_endpoint::prepare(&paths.endpoint()).unwrap();

    assert!(ipc_endpoint::prepare(&paths.endpoint()).is_err());

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
