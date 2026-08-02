//! Validated recorder command-line and environment configuration.

use std::{collections::BTreeMap, env, fs, path::PathBuf, time::Duration};

use pcrt_model::door::DoorId;

const DEFAULT_SESSIONS_DIR: &str = "sessions";
const DEFAULT_ENDPOINT: &str = "ipc:///run/doors.sock";

#[derive(Debug)]
pub(crate) struct RecorderConfig {
    pub(crate) source: String,
    pub(crate) camera_id: String,
    pub(crate) door_id: DoorId,
    pub(crate) door_state_ttl: Duration,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) endpoint: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frames_per_second: u32,
    pub(crate) max_session_seconds: u64,
    pub(crate) idle_sleep: Duration,
    pub(crate) exit_after: Option<Duration>,
}

pub(crate) fn parse_args(
    arguments: impl IntoIterator<Item = String>,
) -> Result<RecorderConfig, String> {
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
    Ok(RecorderConfig {
        source: required_value(&values, "SOURCE")?,
        camera_id: required_value(&values, "CAMERA_ID")?,
        door_id: DoorId::new(positive_u8(&values, "DOOR_CHANNEL")?)
            .ok_or_else(|| "DOOR_CHANNEL must be from 1 to 4".to_owned())?,
        door_state_ttl: positive_duration_seconds(&values, "DOOR_STATE_TTL_SEC")?,
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
        ("DOOR_STATE_TTL_SEC".to_owned(), "2".to_owned()),
        ("WIDTH".to_owned(), "256".to_owned()),
        ("HEIGHT".to_owned(), "256".to_owned()),
        ("FPS".to_owned(), "25".to_owned()),
        ("MAX_SESSION_SECONDS".to_owned(), "300".to_owned()),
        ("IDLE_SLEEP".to_owned(), "0.05".to_owned()),
    ])
}

const fn config_environment_keys() -> &'static [&'static str] {
    &[
        "SOURCE",
        "CAMERA_ID",
        "DOOR_CHANNEL",
        "DOOR_STATE_TTL_SEC",
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
    (value > 0)
        .then_some(value)
        .ok_or_else(|| format!("{key} must be positive"))
}

fn positive_u32(values: &BTreeMap<String, String>, key: &str) -> Result<u32, String> {
    let value = required_value(values, key)?
        .parse::<u32>()
        .map_err(|_| format!("{key} must be a positive integer"))?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| format!("{key} must be positive"))
}

fn positive_u64(values: &BTreeMap<String, String>, key: &str) -> Result<u64, String> {
    let value = required_value(values, key)?
        .parse::<u64>()
        .map_err(|_| format!("{key} must be a positive integer"))?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| format!("{key} must be positive"))
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

const fn usage() -> &'static str {
    "usage: pcrt-recorder [--config-env-file PATH] [--env-file PATH] [--source VALUE] [--camera-id VALUE] [--door-channel N] [--sessions-dir PATH] [--ipc-endpoint ENDPOINT] [--exit-after-ms MS]"
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::parse_args;

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
        assert_eq!(config.door_id.get(), 2);
        assert_eq!((config.width, config.height), (64, 256));
        assert_eq!(config.frames_per_second, 10);
    }

    #[test]
    fn door_channel_is_limited_to_supported_ids() {
        assert!(
            parse_args([
                "--source".to_owned(),
                "file.mp4".to_owned(),
                "--camera-id".to_owned(),
                "cam1".to_owned(),
                "--door-channel".to_owned(),
                "5".to_owned()
            ])
            .is_err()
        );
    }
}
