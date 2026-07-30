#![forbid(unsafe_code)]
#![cfg(feature = "test-transport")]

use std::{
    fs,
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

const SCENARIO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contracts/door/streams/v1/valid-open.json"
);

#[test]
fn unix_byte_source_drives_parser_fsm_and_zeromq_pub() {
    let paths = TestPaths::new();
    let mut source = Command::new(env!("CARGO_BIN_EXE_pcrt-door-protocol-publisher"))
        .args([
            "--listen",
            paths.source.to_str().unwrap(),
            "--scenario",
            SCENARIO,
        ])
        .spawn()
        .unwrap();
    wait_for_path(&paths.source);

    // A ZeroMQ PUB drops messages before a subscriber handshake completes. Connect
    // the subscriber before binding the gateway endpoint so the valid fixture frame
    // cannot race the handshake on a busy CI runner.
    let context = zmq::Context::new();
    let subscriber = context.socket(zmq::SUB).unwrap();
    subscriber.set_rcvhwm(10).unwrap();
    subscriber.set_subscribe(b"doors.state").unwrap();
    subscriber.connect(&paths.zmq_endpoint()).unwrap();
    subscriber.set_rcvtimeo(1_000).unwrap();

    let mut gateway = Command::new(env!("CARGO_BIN_EXE_pcrt-door-gateway"))
        .args([
            "--test-byte-source-unix",
            paths.source.to_str().unwrap(),
            "--ipc-endpoint",
            &paths.zmq_endpoint(),
            "--heartbeat-ms",
            "250",
            "--stale-timeout-ms",
            "1000",
        ])
        .spawn()
        .unwrap();

    let (topic, payload) = receive_fresh_aggregate(&subscriber);
    assert_eq!(topic, "doors.state");
    assert_eq!(payload["seq"], 1);
    assert_eq!(payload["stale"], false);
    assert_eq!(payload["any_open"], true);
    assert_eq!(payload["all_closed"], false);
    assert_eq!(payload["doors"]["1"]["state"], 1);
    assert_eq!(payload["doors"]["2"]["voltage"], 59);
    assert_eq!(payload["doors"]["3"]["voltage"], 65_535);

    assert_success(source.wait().unwrap(), "protocol publisher");
    assert_success(gateway.wait().unwrap(), "gateway");
    paths.cleanup();
}

fn receive_fresh_aggregate(subscriber: &zmq::Socket) -> (String, Value) {
    for _ in 0..20 {
        let frame = subscriber.recv_string(0).unwrap().unwrap();
        let (topic, payload) = frame.split_once(' ').unwrap();
        let payload: Value = serde_json::from_str(payload).unwrap();
        if payload["seq"] == 1 {
            return (topic.to_owned(), payload);
        }
    }
    panic!("did not receive fresh aggregate snapshot");
}

fn wait_for_path(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {}", path.display());
}

fn assert_success(status: std::process::ExitStatus, process_name: &str) {
    assert!(
        status.success(),
        "{process_name} exited with {}",
        status.into_raw()
    );
}

struct TestPaths {
    source: PathBuf,
    zmq_socket: PathBuf,
}

impl TestPaths {
    fn new() -> Self {
        let unique = format!(
            "pcrt-door-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir();
        Self {
            source: root.join(format!("{unique}-source.sock")),
            zmq_socket: root.join(format!("{unique}-output.sock")),
        }
    }

    fn zmq_endpoint(&self) -> String {
        format!("ipc://{}", self.zmq_socket.display())
    }

    fn cleanup(&self) {
        for path in [&self.source, &self.zmq_socket] {
            if path.exists() {
                fs::remove_file(path).unwrap();
            }
        }
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        self.cleanup();
    }
}
