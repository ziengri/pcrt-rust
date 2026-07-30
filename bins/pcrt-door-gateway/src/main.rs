#![forbid(unsafe_code)]
//! RS-232 door gateway with a feature-gated Unix byte source for local tests.

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::PathBuf,
    process,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use pcrt_door::{
    DecodeEvent, DoorProtocol, DoorSnapshot, DoorStateMachine, StreamDecoder, encode_snapshot,
};
use pcrt_service::ShutdownToken;

const DEFAULT_ENDPOINT: &str = "ipc:///run/doors.sock";
const READ_BUFFER_SIZE: usize = 4096;

#[derive(Debug)]
struct GatewayConfig {
    serial_port: Option<String>,
    serial_port_find: Option<String>,
    #[cfg(feature = "test-transport")]
    test_source_path: Option<String>,
    endpoint: String,
    door_count: u8,
    serial_baudrate: u32,
    serial_data_bits: serialport::DataBits,
    serial_parity: serialport::Parity,
    serial_stop_bits: serialport::StopBits,
    serial_read_timeout: Duration,
    stale_timeout: Duration,
    heartbeat_interval: Duration,
    reconnect_delay: Duration,
    exit_after: Option<Duration>,
}

enum ByteSource {
    Serial(Box<dyn serialport::SerialPort>),
    #[cfg(feature = "test-transport")]
    TestUnix(UnixStream),
}

impl ByteSource {
    const fn is_test_transport(&self) -> bool {
        match self {
            Self::Serial(_) => false,
            #[cfg(feature = "test-transport")]
            Self::TestUnix(_) => true,
        }
    }
}

impl Read for ByteSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Serial(port) => port.read(buffer),
            #[cfg(feature = "test-transport")]
            Self::TestUnix(stream) => stream.read(buffer),
        }
    }
}

struct Publisher {
    _context: zmq::Context,
    socket: Option<zmq::Socket>,
    _lock: Option<File>,
    owned_ipc_path: Option<PathBuf>,
}

impl Publisher {
    fn bind(endpoint: &str) -> Result<Self, String> {
        let (lock, owned_ipc_path) = prepare_ipc_endpoint(endpoint)?;
        let context = zmq::Context::new();
        let socket = context.socket(zmq::PUB).map_err(zmq_error)?;
        socket.set_sndhwm(10).map_err(zmq_error)?;
        socket.bind(endpoint).map_err(zmq_error)?;
        log_event("publisher_bound", &[("endpoint", endpoint)]);
        Ok(Self {
            _context: context,
            socket: Some(socket),
            _lock: lock,
            owned_ipc_path,
        })
    }

    fn publish(&self, snapshot: &DoorSnapshot, health: &mut GatewayHealth) {
        let Some(socket) = &self.socket else {
            health.publish_failures = health.publish_failures.saturating_add(1);
            return;
        };
        match encode_snapshot(snapshot, epoch_seconds()) {
            Ok(messages) => {
                for message in messages {
                    if let Err(error) = socket.send(message.as_frame(), zmq::DONTWAIT) {
                        health.publish_failures = health.publish_failures.saturating_add(1);
                        log_event("publish_failed", &[("error", &error.to_string())]);
                    }
                }
            }
            Err(error) => {
                health.publish_failures = health.publish_failures.saturating_add(1);
                log_event("encode_failed", &[("error", &error.to_string())]);
            }
        }
    }

    fn close(mut self) {
        self.socket.take();
        if let Some(path) = self.owned_ipc_path.take() {
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    if let Err(error) = fs::remove_file(&path) {
                        log_event("ipc_cleanup_failed", &[("error", &error.to_string())]);
                    }
                }
                Ok(_) => log_event(
                    "ipc_cleanup_skipped",
                    &[("path", path.to_string_lossy().as_ref())],
                ),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => log_event("ipc_cleanup_failed", &[("error", &error.to_string())]),
            }
        }
    }
}

#[derive(Default)]
struct GatewayHealth {
    connected: bool,
    valid_packets: u64,
    rejected_packets: u64,
    truncated_packets: u64,
    overflow_events: u64,
    reconnect_attempts: u64,
    publish_failures: u64,
    last_valid_packet: Option<Instant>,
}

impl GatewayHealth {
    fn log(&self, snapshot: &DoorSnapshot) {
        let packet_age_ms = self.last_valid_packet.map_or_else(
            || "unknown".to_owned(),
            |instant| {
                Instant::now()
                    .duration_since(instant)
                    .as_millis()
                    .to_string()
            },
        );
        log_event(
            "health",
            &[
                (
                    "serial_connected",
                    if self.connected { "true" } else { "false" },
                ),
                ("stale", if snapshot.is_stale() { "true" } else { "false" }),
                ("seq", &snapshot.sequence().to_string()),
                ("last_valid_age_ms", &packet_age_ms),
                ("valid_packets", &self.valid_packets.to_string()),
                ("rejected_packets", &self.rejected_packets.to_string()),
                ("truncated_packets", &self.truncated_packets.to_string()),
                ("overflow_events", &self.overflow_events.to_string()),
                ("reconnect_attempts", &self.reconnect_attempts.to_string()),
                ("publish_failures", &self.publish_failures.to_string()),
            ],
        );
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("event=gateway_fatal error={error}");
        process::exit(1);
    }
}

#[allow(clippy::too_many_lines)] // The synchronous gateway loop keeps lifecycle ownership explicit.
fn run() -> Result<(), String> {
    let config = parse_args(env::args().skip(1))?;
    let protocol = DoorProtocol::new(config.door_count).map_err(|error| error.to_string())?;
    let shutdown = ShutdownToken::default();
    install_shutdown_handler(shutdown.clone())?;
    let publisher = Publisher::bind(&config.endpoint)?;
    let started_at = Instant::now();
    let mut decoder = StreamDecoder::new(protocol);
    let mut machine = DoorStateMachine::new(protocol, config.stale_timeout);
    let mut health = GatewayHealth::default();
    let mut last_heartbeat = started_at;
    let mut next_connect_at = started_at;
    let mut source = None;
    let mut buffer = [0_u8; READ_BUFFER_SIZE];

    publisher.publish(machine.snapshot(), &mut health);
    while !shutdown.is_shutdown_requested() {
        let now = Instant::now();
        if config
            .exit_after
            .is_some_and(|duration| now.duration_since(started_at) >= duration)
        {
            log_event("gateway_exit_after", &[]);
            break;
        }
        if source.is_none() && now >= next_connect_at {
            health.reconnect_attempts = health.reconnect_attempts.saturating_add(1);
            match open_source(&config) {
                Ok(opened) => {
                    health.connected = true;
                    log_event("source_connected", &[]);
                    source = Some(opened);
                }
                Err(error) => {
                    health.connected = false;
                    log_event("source_connect_failed", &[("error", &error)]);
                    next_connect_at = now + config.reconnect_delay;
                }
            }
        }
        if let Some(reader) = source.as_mut() {
            match reader.read(&mut buffer) {
                Ok(0) if reader.is_test_transport() => {
                    log_event("test_source_complete", &[]);
                    break;
                }
                Ok(0) => disconnect_source(
                    &mut source,
                    &mut health,
                    &mut next_connect_at,
                    now,
                    &config,
                    "eof",
                ),
                Ok(count) => {
                    for event in decoder.push(&buffer[..count]) {
                        match event {
                            DecodeEvent::Packet(packet) => {
                                health.valid_packets = health.valid_packets.saturating_add(1);
                                health.last_valid_packet = Some(Instant::now());
                                if let Ok(snapshot) = machine.accept(packet, Instant::now()) {
                                    publisher.publish(snapshot, &mut health);
                                }
                            }
                            DecodeEvent::Rejected(error) => {
                                health.rejected_packets = health.rejected_packets.saturating_add(1);
                                log_event("packet_rejected", &[("error", &error.to_string())]);
                            }
                            DecodeEvent::Truncated => {
                                health.truncated_packets =
                                    health.truncated_packets.saturating_add(1);
                                log_event("packet_truncated", &[]);
                            }
                            DecodeEvent::Overflow => {
                                health.overflow_events = health.overflow_events.saturating_add(1);
                                log_event("decoder_overflow", &[]);
                            }
                        }
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => disconnect_source(
                    &mut source,
                    &mut health,
                    &mut next_connect_at,
                    now,
                    &config,
                    &error.to_string(),
                ),
            }
        }
        let now = Instant::now();
        if let Some(snapshot) = machine.mark_stale_if_due(now) {
            publisher.publish(snapshot, &mut health);
            last_heartbeat = now;
        }
        if now.duration_since(last_heartbeat) >= config.heartbeat_interval {
            publisher.publish(machine.snapshot(), &mut health);
            health.log(machine.snapshot());
            last_heartbeat = now;
        }
    }
    log_event("gateway_stopping", &[]);
    drop(source);
    publisher.close();
    Ok(())
}

fn disconnect_source(
    source: &mut Option<ByteSource>,
    health: &mut GatewayHealth,
    next_connect_at: &mut Instant,
    now: Instant,
    config: &GatewayConfig,
    reason: &str,
) {
    source.take();
    health.connected = false;
    *next_connect_at = now + config.reconnect_delay;
    log_event("source_disconnected", &[("reason", reason)]);
}

fn open_source(config: &GatewayConfig) -> Result<ByteSource, String> {
    #[cfg(feature = "test-transport")]
    if let Some(path) = &config.test_source_path {
        let stream = UnixStream::connect(path)
            .map_err(|error| format!("connect test byte source {path}: {error}"))?;
        stream
            .set_read_timeout(Some(config.serial_read_timeout))
            .map_err(|error| format!("set test byte source timeout: {error}"))?;
        return Ok(ByteSource::TestUnix(stream));
    }
    if let Some(path) = &config.serial_port {
        if let Ok(source) = open_serial(path, config) {
            return Ok(source);
        }
    }
    let Some(pattern) = &config.serial_port_find else {
        return Err(
            "serial port is unavailable and no serial discovery pattern is configured".to_owned(),
        );
    };
    let paths = glob::glob(pattern)
        .map_err(|error| format!("invalid serial discovery pattern: {error}"))?;
    let mut failures = Vec::new();
    for path in paths.flatten() {
        let path = path.to_string_lossy().into_owned();
        match open_serial(&path, config) {
            Ok(source) => return Ok(source),
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "no serial port discovered for {pattern}: {}",
        failures.join("; ")
    ))
}

fn open_serial(path: &str, config: &GatewayConfig) -> Result<ByteSource, String> {
    serialport::new(path, config.serial_baudrate)
        .data_bits(config.serial_data_bits)
        .parity(config.serial_parity)
        .stop_bits(config.serial_stop_bits)
        .timeout(config.serial_read_timeout)
        .open()
        .map(ByteSource::Serial)
        .map_err(|error| format!("open serial port {path}: {error}"))
}

fn prepare_ipc_endpoint(endpoint: &str) -> Result<(Option<File>, Option<PathBuf>), String> {
    let Some(raw_path) = endpoint.strip_prefix("ipc://") else {
        return Ok((None, None));
    };
    let path = PathBuf::from(raw_path);
    if !path.is_absolute() {
        return Err("IPC endpoint path must be absolute".to_owned());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "IPC endpoint has no parent directory".to_owned())?;
    if !parent.is_dir() {
        return Err(format!(
            "IPC parent directory does not exist: {}",
            parent.display()
        ));
    }
    let lock_path = path.with_extension("sock.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("open IPC lock {}: {error}", lock_path.display()))?;
    lock.try_lock_exclusive()
        .map_err(|_| format!("IPC endpoint is already owned: {}", path.display()))?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!("refusing symlink IPC endpoint: {}", path.display()));
        }
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return Err(format!(
                "refusing non-socket IPC endpoint: {}",
                path.display()
            ));
        }
        Ok(_) => match UnixStream::connect(&path) {
            Ok(_) => {
                return Err(format!(
                    "IPC endpoint has an active owner: {}",
                    path.display()
                ));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(&path).map_err(|remove_error| {
                    format!("remove stale IPC socket {}: {remove_error}", path.display())
                })?;
                log_event(
                    "stale_ipc_socket_removed",
                    &[("path", path.to_string_lossy().as_ref())],
                );
            }
            Err(error) => return Err(format!("inspect IPC socket {}: {error}", path.display())),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect IPC endpoint {}: {error}", path.display())),
    }
    Ok((Some(lock), Some(path)))
}

fn install_shutdown_handler(token: ShutdownToken) -> Result<(), String> {
    ctrlc::set_handler(move || token.request_shutdown())
        .map_err(|error| format!("install shutdown handler: {error}"))
}

fn epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

#[allow(clippy::too_many_lines)] // Keeping flag-to-config mapping together makes precedence auditable.
fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<GatewayConfig, String> {
    let mut arguments = arguments.into_iter().peekable();
    let mut config_path = "config.env".to_owned();
    let mut gateway_path = "door_gateway.env".to_owned();
    let mut device_path = "/etc/pcrt/device.env".to_owned();
    let mut overrides = BTreeMap::new();
    #[cfg(feature = "test-transport")]
    let mut test_source_path = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config-env-file" => config_path = argument_value(&argument, &mut arguments)?,
            "--env-file" => gateway_path = argument_value(&argument, &mut arguments)?,
            "--device-env-file" => device_path = argument_value(&argument, &mut arguments)?,
            "--serial-port" => set_override(
                &mut overrides,
                "SERIAL_PORT",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--serial-port-find" => set_override(
                &mut overrides,
                "SERIAL_PORT_FIND",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--serial-baudrate" => set_override(
                &mut overrides,
                "SERIAL_BAUDRATE",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--serial-bytesize" => set_override(
                &mut overrides,
                "SERIAL_BYTESIZE",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--serial-parity" => set_override(
                &mut overrides,
                "SERIAL_PARITY",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--serial-stopbits" => set_override(
                &mut overrides,
                "SERIAL_STOPBITS",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--ipc-endpoint" => set_override(
                &mut overrides,
                "ZMQ_IPC_ENDPOINT",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--door-count" => set_override(
                &mut overrides,
                "DOOR_COUNT",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--stale-timeout-ms" => set_override(
                &mut overrides,
                "STALE_TIMEOUT_CLI_MS",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--heartbeat-ms" => set_override(
                &mut overrides,
                "HEARTBEAT_CLI_MS",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--reconnect-ms" => set_override(
                &mut overrides,
                "RECONNECT_CLI_MS",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--exit-after-ms" => set_override(
                &mut overrides,
                "EXIT_AFTER_MS",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--test-byte-source-unix" => {
                #[cfg(feature = "test-transport")]
                {
                    if test_source_path
                        .replace(argument_value(&argument, &mut arguments)?)
                        .is_some()
                    {
                        return Err("--test-byte-source-unix may be passed once".to_owned());
                    }
                }
                #[cfg(not(feature = "test-transport"))]
                return Err(
                    "--test-byte-source-unix requires Cargo feature test-transport".to_owned(),
                );
            }
            "--help" => return Err(usage().to_owned()),
            _ => return Err(format!("unknown argument {argument}\n{}", usage())),
        }
    }
    let mut values = defaults();
    values.extend(read_env_file(&config_path)?);
    values.extend(read_env_file(&gateway_path)?);
    let device_values = read_env_file(&device_path)?;
    if values.get("DOOR_COUNT").is_none_or(String::is_empty) {
        if let Some(count) = device_values.get("NUMBER_CAMS") {
            values.insert("DOOR_COUNT".to_owned(), count.clone());
        }
    }
    for key in config_environment_keys() {
        if let Ok(value) = env::var(key) {
            if !value.is_empty() {
                values.insert((*key).to_owned(), value);
            }
        }
    }
    values.extend(overrides);
    values
        .entry("DOOR_COUNT".to_owned())
        .or_insert_with(|| "3".to_owned());
    let serial_port = non_empty(&values, "SERIAL_PORT");
    let serial_port_find = non_empty(&values, "SERIAL_PORT_FIND");
    if let Some(path) = &serial_port {
        require_absolute_path(path, "SERIAL_PORT")?;
    }
    if let Some(pattern) = &serial_port_find {
        require_absolute_path(pattern, "SERIAL_PORT_FIND")?;
    }
    #[cfg(not(feature = "test-transport"))]
    let test_source_present = false;
    #[cfg(feature = "test-transport")]
    let test_source_present = test_source_path.is_some();
    if !test_source_present && serial_port.is_none() && serial_port_find.is_none() {
        return Err(format!(
            "--serial-port or --serial-port-find is required\n{}",
            usage()
        ));
    }
    if test_source_present && (serial_port.is_some() || serial_port_find.is_some()) {
        return Err("test source cannot be combined with serial configuration".to_owned());
    }
    let stale_timeout =
        duration_seconds_or_ms(&values, "STALE_TIMEOUT_SEC", "STALE_TIMEOUT_CLI_MS")?;
    let heartbeat_interval =
        duration_seconds_or_ms(&values, "HEARTBEAT_PUBLISH_SEC", "HEARTBEAT_CLI_MS")?;
    if heartbeat_interval >= stale_timeout {
        return Err("HEARTBEAT_PUBLISH_SEC must be less than STALE_TIMEOUT_SEC".to_owned());
    }
    let serial_read_timeout = duration_seconds(&values, "SERIAL_TIMEOUT")?;
    if serial_read_timeout > heartbeat_interval {
        return Err("SERIAL_TIMEOUT must not exceed HEARTBEAT_PUBLISH_SEC".to_owned());
    }
    Ok(GatewayConfig {
        serial_port,
        serial_port_find,
        #[cfg(feature = "test-transport")]
        test_source_path,
        endpoint: required_value(&values, "ZMQ_IPC_ENDPOINT")?,
        door_count: required_value(&values, "DOOR_COUNT")?
            .parse()
            .map_err(|_| "DOOR_COUNT must be 3 or 4".to_owned())?,
        serial_baudrate: positive_u32(&values, "SERIAL_BAUDRATE")?,
        serial_data_bits: parse_data_bits(&required_value(&values, "SERIAL_BYTESIZE")?)?,
        serial_parity: parse_parity(&required_value(&values, "SERIAL_PARITY")?)?,
        serial_stop_bits: parse_stop_bits(&required_value(&values, "SERIAL_STOPBITS")?)?,
        serial_read_timeout,
        stale_timeout,
        heartbeat_interval,
        reconnect_delay: duration_seconds_or_ms(&values, "RECONNECT_SEC", "RECONNECT_CLI_MS")?,
        exit_after: optional_duration_ms(&values, "EXIT_AFTER_MS")?,
    })
}

const fn config_environment_keys() -> &'static [&'static str] {
    &[
        "SERIAL_PORT",
        "SERIAL_PORT_FIND",
        "SERIAL_BAUDRATE",
        "SERIAL_BYTESIZE",
        "SERIAL_PARITY",
        "SERIAL_STOPBITS",
        "SERIAL_TIMEOUT",
        "ZMQ_IPC_ENDPOINT",
        "DOOR_COUNT",
        "STALE_TIMEOUT_SEC",
        "HEARTBEAT_PUBLISH_SEC",
        "RECONNECT_SEC",
        "EXIT_AFTER_MS",
    ]
}

fn require_absolute_path(value: &str, key: &str) -> Result<(), String> {
    if PathBuf::from(value).is_absolute() {
        Ok(())
    } else {
        Err(format!("{key} must be an absolute path"))
    }
}

fn defaults() -> BTreeMap<String, String> {
    [
        ("SERIAL_BAUDRATE", "19200"),
        ("SERIAL_BYTESIZE", "8"),
        ("SERIAL_PARITY", "N"),
        ("SERIAL_STOPBITS", "1"),
        ("SERIAL_TIMEOUT", "0.2"),
        ("ZMQ_IPC_ENDPOINT", DEFAULT_ENDPOINT),
        ("STALE_TIMEOUT_SEC", "2"),
        ("HEARTBEAT_PUBLISH_SEC", "0.5"),
        ("RECONNECT_SEC", "1"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect()
}

fn read_env_file(path: &str) -> Result<BTreeMap<String, String>, String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("read config {path}: {error}")),
    };
    let mut values = BTreeMap::new();
    for (line_number, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("invalid config {path}:{}", line_number + 1));
        };
        values.insert(
            key.trim().to_owned(),
            value.trim().trim_matches('"').to_owned(),
        );
    }
    Ok(values)
}

fn set_override(
    values: &mut BTreeMap<String, String>,
    key: &str,
    value: String,
) -> Result<(), String> {
    if values.insert(key.to_owned(), value).is_some() {
        return Err(format!("{key} may be passed once"));
    }
    Ok(())
}

fn non_empty(values: &BTreeMap<String, String>, key: &str) -> Option<String> {
    values
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn required_value(values: &BTreeMap<String, String>, key: &str) -> Result<String, String> {
    non_empty(values, key).ok_or_else(|| format!("{key} is required"))
}

fn positive_u32(values: &BTreeMap<String, String>, key: &str) -> Result<u32, String> {
    required_value(values, key)?
        .parse()
        .ok()
        .filter(|value: &u32| *value > 0)
        .ok_or_else(|| format!("{key} must be a positive integer"))
}

fn duration_seconds(values: &BTreeMap<String, String>, key: &str) -> Result<Duration, String> {
    required_value(values, key)?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .and_then(|value| Duration::try_from_secs_f64(value).ok())
        .ok_or_else(|| format!("{key} must be a positive number of seconds"))
}

fn duration_seconds_or_ms(
    values: &BTreeMap<String, String>,
    seconds_key: &str,
    milliseconds_key: &str,
) -> Result<Duration, String> {
    if let Some(value) = non_empty(values, milliseconds_key) {
        return value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
            .ok_or_else(|| format!("{milliseconds_key} must be a positive integer"));
    }
    duration_seconds(values, seconds_key)
}

fn optional_duration_ms(
    values: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<Duration>, String> {
    match non_empty(values, key) {
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
            .map(Some)
            .ok_or_else(|| format!("{key} must be a positive integer")),
        None => Ok(None),
    }
}

fn parse_data_bits(value: &str) -> Result<serialport::DataBits, String> {
    match value {
        "5" => Ok(serialport::DataBits::Five),
        "6" => Ok(serialport::DataBits::Six),
        "7" => Ok(serialport::DataBits::Seven),
        "8" => Ok(serialport::DataBits::Eight),
        _ => Err("SERIAL_BYTESIZE must be 5, 6, 7 or 8".to_owned()),
    }
}

fn parse_parity(value: &str) -> Result<serialport::Parity, String> {
    match value.to_ascii_uppercase().as_str() {
        "N" => Ok(serialport::Parity::None),
        "O" => Ok(serialport::Parity::Odd),
        "E" => Ok(serialport::Parity::Even),
        _ => Err("SERIAL_PARITY must be N, O or E".to_owned()),
    }
}

fn parse_stop_bits(value: &str) -> Result<serialport::StopBits, String> {
    match value {
        "1" | "1.0" => Ok(serialport::StopBits::One),
        "2" | "2.0" => Ok(serialport::StopBits::Two),
        _ => Err("SERIAL_STOPBITS must be 1 or 2".to_owned()),
    }
}

fn argument_value(
    name: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{name} requires a value"))
}

const fn usage() -> &'static str {
    "usage: pcrt-door-gateway (--serial-port PATH | --serial-port-find GLOB) [--config-env-file FILE] [--env-file FILE] [--device-env-file FILE] [--ipc-endpoint ENDPOINT] [--door-count 3|4]"
}

fn zmq_error(error: zmq::Error) -> String {
    format!("ZeroMQ: {error}")
}

fn log_event(event: &str, fields: &[(&str, &str)]) {
    eprint!("event={event}");
    for (key, value) in fields {
        eprint!(" {key}={}", value.replace([' ', '\n', '\r'], "_"));
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        os::unix::{fs::symlink, net::UnixListener},
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{parse_args, parse_parity, parse_stop_bits, prepare_ipc_endpoint};

    #[test]
    fn production_config_requires_source_and_rejects_invalid_timing() {
        assert!(parse_args([]).is_err());
        assert!(
            parse_args([
                "--serial-port".to_owned(),
                "/dev/ttyS0".to_owned(),
                "--heartbeat-ms".to_owned(),
                "2000".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn serial_parameters_are_validated() {
        assert!(parse_parity("Q").is_err());
        assert!(parse_stop_bits("1.5").is_err());
    }

    #[test]
    fn serial_paths_must_be_absolute() {
        assert!(parse_args(["--serial-port".to_owned(), "ttyS0".to_owned(),]).is_err());
        assert!(parse_args(["--serial-port-find".to_owned(), "ttyS*".to_owned(),]).is_err());
    }

    #[test]
    fn gateway_config_overrides_base_and_device_count_is_only_fallback() {
        let paths = ConfigTestPaths::new();
        fs::write(&paths.base, "SERIAL_PORT=/dev/base\nDOOR_COUNT=4\n").unwrap();
        fs::write(&paths.gateway, "SERIAL_PORT=/dev/gateway\n").unwrap();
        fs::write(&paths.device, "NUMBER_CAMS=3\n").unwrap();

        let config = parse_args([
            "--config-env-file".to_owned(),
            paths.base.to_string_lossy().into_owned(),
            "--env-file".to_owned(),
            paths.gateway.to_string_lossy().into_owned(),
            "--device-env-file".to_owned(),
            paths.device.to_string_lossy().into_owned(),
        ])
        .unwrap();

        assert_eq!(config.serial_port.as_deref(), Some("/dev/gateway"));
        assert_eq!(config.door_count, 4);
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
                "pcrt-door-ipc-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
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

    struct ConfigTestPaths {
        base: PathBuf,
        gateway: PathBuf,
        device: PathBuf,
    }

    impl ConfigTestPaths {
        fn new() -> Self {
            let suffix = format!(
                "pcrt-door-config-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let root = std::env::temp_dir();
            Self {
                base: root.join(format!("{suffix}-base.env")),
                gateway: root.join(format!("{suffix}-gateway.env")),
                device: root.join(format!("{suffix}-device.env")),
            }
        }
    }

    impl Drop for ConfigTestPaths {
        fn drop(&mut self) {
            for path in [&self.base, &self.gateway, &self.device] {
                let _ = fs::remove_file(path);
            }
        }
    }
}
