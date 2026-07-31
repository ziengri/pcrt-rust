#![forbid(unsafe_code)]
//! One-camera door-gated recorder service.

use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    process, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use pcrt_recording::{
    lifecycle::RecordingLimits,
    recorder::{FfmpegEncoderFactory, Recorder, RecorderConfig},
    service::{RecordingService, RecordingServiceStep},
    video::OpenCvVideoSource,
};
use pcrt_service::ShutdownToken;
use pcrt_storage::SessionStorage;
use serde::Deserialize;

const DEFAULT_SESSIONS_DIR: &str = "sessions";
const DEFAULT_ENDPOINT: &str = "ipc:///run/doors.sock";

#[derive(Debug)]
struct RecorderServiceConfig {
    source: String,
    camera_id: String,
    door_channel: u8,
    door_open_value: u8,
    sessions_dir: PathBuf,
    endpoint: String,
    width: u32,
    height: u32,
    frames_per_second: u32,
    max_session_seconds: u64,
    idle_sleep: Duration,
    exit_after: Option<Duration>,
}

#[derive(Clone, Debug, Deserialize)]
struct DoorMessage {
    state: u8,
    stale: bool,
}

struct DoorSubscriber {
    _context: zmq::Context,
    socket: zmq::Socket,
    topic: String,
    latest: Option<DoorMessage>,
}

impl DoorSubscriber {
    fn connect(endpoint: &str, door_channel: u8) -> Result<Self, String> {
        Self::connect_with_context(zmq::Context::new(), endpoint, door_channel)
    }

    fn connect_with_context(
        context: zmq::Context,
        endpoint: &str,
        door_channel: u8,
    ) -> Result<Self, String> {
        let socket = context.socket(zmq::SUB).map_err(zmq_error)?;
        let topic = format!("door.{door_channel}.state");
        socket.set_subscribe(topic.as_bytes()).map_err(zmq_error)?;
        socket.set_rcvhwm(10).map_err(zmq_error)?;
        socket.connect(endpoint).map_err(zmq_error)?;
        Ok(Self {
            _context: context,
            socket,
            topic,
            latest: None,
        })
    }

    fn door_open(&mut self, open_value: u8) -> bool {
        self.drain();
        self.latest
            .as_ref()
            .is_some_and(|message| !message.stale && message.state == open_value)
    }

    fn drain(&mut self) {
        loop {
            let Ok(frame) = self.socket.recv_string(zmq::DONTWAIT) else {
                return;
            };
            let Ok(frame) = frame else {
                continue;
            };
            let Some((topic, payload)) = frame.split_once(' ') else {
                continue;
            };
            if topic != self.topic {
                continue;
            }
            if let Ok(message) = serde_json::from_str::<DoorMessage>(payload) {
                self.latest = Some(message);
            }
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("event=recorder_fatal error={error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = parse_args(env::args().skip(1))?;
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let storage = SessionStorage::open(&config.sessions_dir).map_err(|error| error.to_string())?;
    let recovery = storage
        .recover(now_ms())
        .map_err(|error| error.to_string())?;
    log_event(
        "storage_recovered",
        &[
            ("failed_sessions", &recovery.failed_sessions.to_string()),
            ("released_claims", &recovery.released_claims.to_string()),
        ],
    );
    let recorder = Recorder::new(
        storage,
        FfmpegEncoderFactory,
        RecorderConfig {
            camera_id: config.camera_id.clone(),
            source_id: config.source.clone(),
            width: config.width,
            height: config.height,
            frames_per_second: config.frames_per_second,
        },
        RecordingLimits::new(
            config.frames_per_second,
            u64::from(config.frames_per_second)
                .checked_mul(config.max_session_seconds)
                .ok_or_else(|| "MAX_SESSION_SECONDS * FPS overflows frame limit".to_owned())?,
        )
        .map_err(|error| error.to_string())?,
    );
    let source = OpenCvVideoSource::open(&config.source).map_err(|error| error.to_string())?;
    let mut service = RecordingService::new(source, recorder, config.width, config.height)
        .map_err(|error| error.to_string())?;
    let mut doors = DoorSubscriber::connect(&config.endpoint, config.door_channel)?;
    let started_at = Instant::now();
    log_event(
        "recorder_started",
        &[
            ("camera_id", &config.camera_id),
            ("door_channel", &config.door_channel.to_string()),
            ("endpoint", &config.endpoint),
        ],
    );
    while !shutdown.is_shutdown_requested() {
        if config
            .exit_after
            .is_some_and(|duration| started_at.elapsed() >= duration)
        {
            log_event("recorder_exit_after", &[]);
            break;
        }
        let door_open = doors.door_open(config.door_open_value);
        match service
            .step(door_open, now_ms())
            .map_err(|error| error.to_string())?
        {
            RecordingServiceStep::FileRestarted => log_event("source_file_restarted", &[]),
            RecordingServiceStep::NoFrame => thread::sleep(config.idle_sleep),
            RecordingServiceStep::FrameHandled | RecordingServiceStep::FrameDiscardedDoorClosed => {
            }
        }
    }
    service
        .shutdown(now_ms())
        .map_err(|error| error.to_string())?;
    log_event("recorder_stopped", &[]);
    Ok(())
}

fn install_shutdown_handler(token: ShutdownToken) -> Result<(), String> {
    ctrlc::set_handler(move || token.request_shutdown())
        .map_err(|error| format!("install shutdown handler: {error}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn zmq_error(error: zmq::Error) -> String {
    format!("ZeroMQ: {error}")
}

fn log_event(event: &str, values: &[(&str, &str)]) {
    let mut line = format!("event={event}");
    for (key, value) in values {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    eprintln!("{line}");
}

fn parse_args(
    arguments: impl IntoIterator<Item = String>,
) -> Result<RecorderServiceConfig, String> {
    let mut arguments = arguments.into_iter();
    let mut config_path = "config.env".to_owned();
    let mut recorder_path = "recorder-cam.env".to_owned();
    let mut overrides = BTreeMap::new();
    let mut exit_after = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config-env-file" => config_path = argument_value(&argument, &mut arguments)?,
            "--env-file" => recorder_path = argument_value(&argument, &mut arguments)?,
            "--source" => set_override(
                &mut overrides,
                "SOURCE",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--camera-id" => set_override(
                &mut overrides,
                "CAMERA_ID",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--door-channel" => set_override(
                &mut overrides,
                "DOOR_CHANNEL",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--sessions-dir" => set_override(
                &mut overrides,
                "SESSIONS_DIR",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--ipc-endpoint" => set_override(
                &mut overrides,
                "ZMQ_IPC_ENDPOINT",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--exit-after-ms" => {
                if exit_after.is_some() {
                    return Err("--exit-after-ms may be passed once".to_owned());
                }
                exit_after = Some(Duration::from_millis(
                    argument_value(&argument, &mut arguments)?
                        .parse::<u64>()
                        .map_err(|_| "--exit-after-ms must be an integer".to_owned())?,
                ));
            }
            "--help" => return Err(usage().to_owned()),
            _ => return Err(format!("unknown argument {argument}\n{}", usage())),
        }
    }
    let mut values = defaults();
    values.extend(read_env_file(&config_path)?);
    values.extend(read_env_file(&recorder_path)?);
    for key in config_environment_keys() {
        if let Ok(value) = env::var(key) {
            if !value.is_empty() {
                values.insert((*key).to_owned(), value);
            }
        }
    }
    values.extend(overrides);
    Ok(RecorderServiceConfig {
        source: required_value(&values, "SOURCE")?,
        camera_id: required_value(&values, "CAMERA_ID")?,
        door_channel: positive_u8(&values, "DOOR_CHANNEL")?,
        door_open_value: parse_u8(&values, "DOOR_OPEN_VALUE")?,
        sessions_dir: PathBuf::from(required_value(&values, "SESSIONS_DIR")?),
        endpoint: required_value(&values, "ZMQ_IPC_ENDPOINT")?,
        width: positive_u32(&values, "WIDTH")?,
        height: positive_u32(&values, "HEIGHT")?,
        frames_per_second: positive_u32(&values, "FPS")?,
        max_session_seconds: positive_u64(&values, "MAX_SESSION_SECONDS")?,
        idle_sleep: positive_duration_seconds(&values, "IDLE_SLEEP")?,
        exit_after,
    })
}

fn defaults() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("SESSIONS_DIR".to_owned(), DEFAULT_SESSIONS_DIR.to_owned()),
        ("ZMQ_IPC_ENDPOINT".to_owned(), DEFAULT_ENDPOINT.to_owned()),
        ("DOOR_OPEN_VALUE".to_owned(), "1".to_owned()),
        ("WIDTH".to_owned(), "256".to_owned()),
        ("HEIGHT".to_owned(), "256".to_owned()),
        ("FPS".to_owned(), "25".to_owned()),
        ("MAX_SESSION_SECONDS".to_owned(), "300".to_owned()),
        ("IDLE_SLEEP".to_owned(), "0.05".to_owned()),
    ])
}

fn config_environment_keys() -> &'static [&'static str] {
    &[
        "SOURCE",
        "CAMERA_ID",
        "DOOR_CHANNEL",
        "DOOR_OPEN_VALUE",
        "SESSIONS_DIR",
        "ZMQ_IPC_ENDPOINT",
        "WIDTH",
        "HEIGHT",
        "FPS",
        "MAX_SESSION_SECONDS",
        "IDLE_SLEEP",
    ]
}

fn read_env_file(path: &str) -> Result<BTreeMap<String, String>, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("read {path}: {error}")),
    };
    let mut values = BTreeMap::new();
    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{path}:{} must be KEY=VALUE", line_number + 1));
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
        {
            return Err(format!("{path}:{} has invalid key", line_number + 1));
        }
        values.insert(key.to_owned(), value.trim().to_owned());
    }
    Ok(values)
}

fn argument_value(
    argument: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    arguments
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{argument} requires a value"))
}

fn set_override(
    values: &mut BTreeMap<String, String>,
    key: &str,
    value: String,
) -> Result<(), String> {
    if values.insert(key.to_owned(), value).is_some() {
        return Err(format!("{key} may be overridden once"));
    }
    Ok(())
}

fn required_value(values: &BTreeMap<String, String>, key: &str) -> Result<String, String> {
    values
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| format!("{key} is required"))
}

fn parse_u8(values: &BTreeMap<String, String>, key: &str) -> Result<u8, String> {
    required_value(values, key)?
        .parse()
        .map_err(|_| format!("{key} must be an integer from 0 to 255"))
}

fn positive_u8(values: &BTreeMap<String, String>, key: &str) -> Result<u8, String> {
    let value = parse_u8(values, key)?;
    if value == 0 {
        return Err(format!("{key} must be positive"));
    }
    Ok(value)
}

fn positive_u32(values: &BTreeMap<String, String>, key: &str) -> Result<u32, String> {
    let value = required_value(values, key)?
        .parse::<u32>()
        .map_err(|_| format!("{key} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("{key} must be positive"));
    }
    Ok(value)
}

fn positive_u64(values: &BTreeMap<String, String>, key: &str) -> Result<u64, String> {
    let value = required_value(values, key)?
        .parse::<u64>()
        .map_err(|_| format!("{key} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("{key} must be positive"));
    }
    Ok(value)
}

fn positive_duration_seconds(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<Duration, String> {
    let seconds = required_value(values, key)?
        .parse::<f64>()
        .map_err(|_| format!("{key} must be a positive number of seconds"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!("{key} must be positive"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn usage() -> &'static str {
    "usage: pcrt-recorder [--config-env-file PATH] [--env-file PATH] [--source VALUE] [--camera-id VALUE] [--door-channel N] [--sessions-dir PATH] [--ipc-endpoint ENDPOINT] [--exit-after-ms MS]"
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{DoorMessage, parse_args};

    #[test]
    fn config_uses_file_environment_and_cli_precedence() {
        let directory = tempdir().unwrap();
        let global = directory.path().join("config.env");
        let camera = directory.path().join("recorder.env");
        fs::write(
            &global,
            "SOURCE=file.mp4\nCAMERA_ID=global\nDOOR_CHANNEL=1\nFPS=10\n",
        )
        .unwrap();
        fs::write(&camera, "CAMERA_ID=cam2\nDOOR_CHANNEL=2\nWIDTH=64\n").unwrap();
        let config = parse_args([
            "--config-env-file".to_owned(),
            global.to_string_lossy().into_owned(),
            "--env-file".to_owned(),
            camera.to_string_lossy().into_owned(),
            "--camera-id".to_owned(),
            "cam3".to_owned(),
        ])
        .unwrap();

        assert_eq!(config.source, "file.mp4");
        assert_eq!(config.camera_id, "cam3");
        assert_eq!(config.door_channel, 2);
        assert_eq!((config.width, config.height), (64, 256));
        assert_eq!(config.frames_per_second, 10);
    }

    #[test]
    fn invalid_door_message_is_not_deserializable() {
        assert!(serde_json::from_str::<DoorMessage>(r#"{\"stale\":false}"#).is_err());
    }

    #[test]
    fn subscriber_uses_latest_fresh_selected_door_message() {
        let endpoint = format!("inproc://pcrt-recorder-test-{}", std::process::id());
        let context = zmq::Context::new();
        let publisher = context.socket(zmq::PUB).unwrap();
        publisher.bind(&endpoint).unwrap();
        let mut subscriber =
            super::DoorSubscriber::connect_with_context(context.clone(), &endpoint, 2).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        publisher
            .send(r#"door.1.state {"state":1,"stale":false}"#, 0)
            .unwrap();
        publisher
            .send(r#"door.2.state {"state":1,"stale":false}"#, 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(subscriber.door_open(1));

        publisher
            .send(r#"door.2.state {"state":1,"stale":true}"#, 0)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!subscriber.door_open(1));
    }
}
