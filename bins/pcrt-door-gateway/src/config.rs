//! Validated gateway command-line and environment configuration.

use std::{collections::BTreeMap, env, fs, io, path::PathBuf, time::Duration};

const DEFAULT_ENDPOINT: &str = "ipc:///run/doors.sock";

#[derive(Debug)]
pub(crate) struct GatewayConfig {
    pub(crate) serial_port: Option<String>,
    pub(crate) serial_port_find: Option<String>,
    #[cfg(feature = "test-transport")]
    pub(crate) test_source_path: Option<String>,
    pub(crate) endpoint: String,
    pub(crate) door_count: u8,
    pub(crate) serial_baudrate: u32,
    pub(crate) serial_data_bits: serialport::DataBits,
    pub(crate) serial_parity: serialport::Parity,
    pub(crate) serial_stop_bits: serialport::StopBits,
    pub(crate) serial_read_timeout: Duration,
    pub(crate) stale_timeout: Duration,
    pub(crate) serial_liveness_timeout: Duration,
    pub(crate) heartbeat_interval: Duration,
    pub(crate) reconnect_delay: Duration,
    pub(crate) exit_after: Option<Duration>,
}

#[allow(clippy::too_many_lines)] // Keeping flag-to-config mapping together makes precedence auditable.
pub(crate) fn parse_args(
    arguments: impl IntoIterator<Item = String>,
) -> Result<GatewayConfig, String> {
    let mut arguments = arguments.into_iter().peekable();
    let mut config_path = "config.env".to_owned();
    let mut gateway_path = "door_gateway.env".to_owned();
    let mut device_path = "/etc/pcrt/device.env".to_owned();
    let mut overrides = BTreeMap::new();
    let mut exit_after = None;
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
            "--exit-after-ms" => {
                if exit_after.is_some() {
                    return Err("--exit-after-ms may be passed once".to_owned());
                }
                exit_after = Some(parse_duration_ms(
                    &argument_value(&argument, &mut arguments)?,
                    "--exit-after-ms",
                )?);
            }
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
    if values.contains_key("EXIT_AFTER_MS") || env::var_os("EXIT_AFTER_MS").is_some() {
        return Err("EXIT_AFTER_MS is only supported through --exit-after-ms".to_owned());
    }
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
    let serial_liveness_timeout = duration_seconds(&values, "SERIAL_LIVENESS_TIMEOUT_SEC")?;
    if serial_liveness_timeout <= stale_timeout {
        return Err("SERIAL_LIVENESS_TIMEOUT_SEC must exceed STALE_TIMEOUT_SEC".to_owned());
    }
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
        serial_liveness_timeout,
        heartbeat_interval,
        reconnect_delay: duration_seconds_or_ms(&values, "RECONNECT_SEC", "RECONNECT_CLI_MS")?,
        exit_after,
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
        "SERIAL_LIVENESS_TIMEOUT_SEC",
        "HEARTBEAT_PUBLISH_SEC",
        "RECONNECT_SEC",
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
        ("SERIAL_LIVENESS_TIMEOUT_SEC", "15"),
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

fn parse_duration_ms(value: &str, key: &str) -> Result<Duration, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("{key} must be a positive integer"))
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
    "usage: pcrt-door-gateway (--serial-port PATH | --serial-port-find GLOB) [--config-env-file FILE] [--env-file FILE] [--device-env-file FILE] [--ipc-endpoint ENDPOINT] [--door-count 3|4] [--exit-after-ms MS]"
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::{parse_args, parse_parity, parse_stop_bits};

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
    fn serial_liveness_must_follow_stale_timeout() {
        let paths = ConfigTestPaths::new();
        fs::write(
            &paths.gateway,
            "SERIAL_PORT=/dev/ttyS0\nSTALE_TIMEOUT_SEC=10\nSERIAL_LIVENESS_TIMEOUT_SEC=10\n",
        )
        .unwrap();

        assert!(
            parse_args([
                "--env-file".to_owned(),
                paths.gateway.to_string_lossy().into_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn exit_after_is_rejected_in_env_file_but_allowed_on_cli() {
        let paths = ConfigTestPaths::new();
        fs::write(
            &paths.gateway,
            "SERIAL_PORT=/dev/ttyS0\nEXIT_AFTER_MS=100\n",
        )
        .unwrap();
        assert!(
            parse_args([
                "--env-file".to_owned(),
                paths.gateway.to_string_lossy().into_owned(),
            ])
            .is_err()
        );

        let serial_port = absolute_serial_port();
        let config = parse_args([
            "--serial-port".to_owned(),
            serial_port,
            "--exit-after-ms".to_owned(),
            "100".to_owned(),
        ])
        .unwrap();
        assert_eq!(config.exit_after, Some(Duration::from_millis(100)));
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
        let base_port = std::env::temp_dir().join("pcrt-door-base");
        let gateway_port = absolute_serial_port();
        fs::write(
            &paths.base,
            format!("SERIAL_PORT={}\nDOOR_COUNT=4\n", base_port.display()),
        )
        .unwrap();
        fs::write(&paths.gateway, format!("SERIAL_PORT={gateway_port}\n")).unwrap();
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

        assert_eq!(config.serial_port.as_deref(), Some(gateway_port.as_str()));
        assert_eq!(config.door_count, 4);
    }

    fn absolute_serial_port() -> String {
        std::env::temp_dir()
            .join("pcrt-door-gateway")
            .to_string_lossy()
            .into_owned()
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
