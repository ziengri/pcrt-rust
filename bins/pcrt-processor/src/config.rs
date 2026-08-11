//! Validated processor command-line and environment configuration.

use std::{collections::BTreeMap, env, fs, path::PathBuf, time::Duration};

const DEFAULT_SESSIONS_DIR: &str = "sessions";
const DEFAULT_ENDPOINT: &str = "ipc:///run/doors.sock";
const DEFAULT_DEVICE_ENV: &str = "/etc/pcrt/device.env";
const MODEL_INPUT_SIZE: usize = 256;
const MAX_INFERENCE_STREAMS: usize = 16;
const MAX_SKIP_FRAMES: usize = 1_000;

#[derive(Debug)]
pub(crate) struct ProcessorConfig {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) queue_path: PathBuf,
    pub(crate) endpoint: String,
    pub(crate) door_state_ttl: Duration,
    pub(crate) idle_sleep: Duration,
    pub(crate) exit_after: Option<Duration>,
    pub(crate) bus_id: String,
    pub(crate) inference: InferenceConfig,
}

#[derive(Clone, Debug)]
pub(crate) struct InferenceConfig {
    pub(crate) model_path: PathBuf,
    pub(crate) streams: usize,
    pub(crate) confidence: f32,
    pub(crate) skip_frames: usize,
    pub(crate) target_size: usize,
    pub(crate) line_y_ratio: f32,
    pub(crate) track_threshold: f32,
    pub(crate) track_buffer: usize,
    pub(crate) track_match_threshold: f32,
    pub(crate) track_init_threshold: f32,
}

pub(crate) fn parse_args(
    arguments: impl IntoIterator<Item = String>,
) -> Result<ProcessorConfig, String> {
    let mut arguments = arguments.into_iter();
    let mut config_path = "config.env".to_owned();
    let mut processor_path = "processor.env".to_owned();
    let mut device_path = DEFAULT_DEVICE_ENV.to_owned();
    let mut overrides = BTreeMap::new();
    let mut exit_after = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config-env-file" => config_path = argument_value(&argument, &mut arguments)?,
            "--env-file" => processor_path = argument_value(&argument, &mut arguments)?,
            "--device-env-file" => device_path = argument_value(&argument, &mut arguments)?,
            "--sessions-dir" => set_override(
                &mut overrides,
                "SESSIONS_DIR",
                argument_value(&argument, &mut arguments)?,
            )?,
            "--result-queue-db" => set_override(
                &mut overrides,
                "RESULT_QUEUE_DB",
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
    values.extend(read_env_file(&processor_path)?);
    let device_values = read_env_file(&device_path)?;
    for key in config_environment_keys() {
        if let Ok(value) = env::var(key)
            && !value.is_empty()
        {
            values.insert((*key).to_owned(), value);
        }
    }
    values.extend(overrides);

    let sessions_dir = PathBuf::from(required_value(&values, "SESSIONS_DIR")?);
    let queue_path = values
        .get("RESULT_QUEUE_DB")
        .filter(|value| !value.trim().is_empty())
        .map_or_else(|| sessions_dir.join("outbox/results.sqlite"), PathBuf::from);
    Ok(ProcessorConfig {
        sessions_dir,
        queue_path,
        endpoint: required_value(&values, "ZMQ_IPC_ENDPOINT")?,
        door_state_ttl: positive_duration_seconds(&values, "DOOR_STATE_TTL_SEC")?,
        idle_sleep: positive_duration_seconds(&values, "IDLE_SLEEP")?,
        exit_after,
        bus_id: required_value(&device_values, "BUS_ID")?,
        inference: InferenceConfig {
            model_path: PathBuf::from(required_value(&values, "AI_MODEL_PATH")?),
            streams: bounded_usize(&values, "AI_STREAMS", 1, MAX_INFERENCE_STREAMS)?,
            confidence: unit_interval(&values, "AI_CONFIDENCE")?,
            skip_frames: bounded_usize(&values, "AI_SKIP_FRAMES", 0, MAX_SKIP_FRAMES)?,
            target_size: exact_usize(&values, "AI_TARGET_SIZE", MODEL_INPUT_SIZE)?,
            line_y_ratio: unit_interval(&values, "AI_LINE_Y_RATIO")?,
            track_threshold: unit_interval(&values, "AI_TRACK_THRESHOLD")?,
            track_buffer: bounded_usize(&values, "AI_TRACK_BUFFER", 1, 10_000)?,
            track_match_threshold: unit_interval(&values, "AI_TRACK_MATCH_THRESHOLD")?,
            track_init_threshold: unit_interval(&values, "AI_TRACK_INIT_THRESHOLD")?,
        },
    })
}

fn defaults() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("SESSIONS_DIR".to_owned(), DEFAULT_SESSIONS_DIR.to_owned()),
        ("ZMQ_IPC_ENDPOINT".to_owned(), DEFAULT_ENDPOINT.to_owned()),
        ("DOOR_STATE_TTL_SEC".to_owned(), "2".to_owned()),
        ("IDLE_SLEEP".to_owned(), "0.1".to_owned()),
        (
            "AI_MODEL_PATH".to_owned(),
            "models/yolo26n-head-v3_int8_openvino_model/yolo26n-head-v3.xml".to_owned(),
        ),
        ("AI_STREAMS".to_owned(), "4".to_owned()),
        ("AI_CONFIDENCE".to_owned(), "0.25".to_owned()),
        ("AI_SKIP_FRAMES".to_owned(), "2".to_owned()),
        ("AI_TARGET_SIZE".to_owned(), "256".to_owned()),
        ("AI_LINE_Y_RATIO".to_owned(), "0.40".to_owned()),
        ("AI_TRACK_THRESHOLD".to_owned(), "0.50".to_owned()),
        ("AI_TRACK_BUFFER".to_owned(), "30".to_owned()),
        ("AI_TRACK_MATCH_THRESHOLD".to_owned(), "0.80".to_owned()),
        ("AI_TRACK_INIT_THRESHOLD".to_owned(), "0.60".to_owned()),
    ])
}

const fn config_environment_keys() -> &'static [&'static str] {
    &[
        "SESSIONS_DIR",
        "RESULT_QUEUE_DB",
        "ZMQ_IPC_ENDPOINT",
        "DOOR_STATE_TTL_SEC",
        "IDLE_SLEEP",
        "AI_MODEL_PATH",
        "AI_STREAMS",
        "AI_CONFIDENCE",
        "AI_SKIP_FRAMES",
        "AI_TARGET_SIZE",
        "AI_LINE_Y_RATIO",
        "AI_TRACK_THRESHOLD",
        "AI_TRACK_BUFFER",
        "AI_TRACK_MATCH_THRESHOLD",
        "AI_TRACK_INIT_THRESHOLD",
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
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| format!("{key} is outside the supported duration range"))
}

fn bounded_usize(
    values: &BTreeMap<String, String>,
    key: &str,
    minimum: usize,
    maximum: usize,
) -> Result<usize, String> {
    let value = required_value(values, key)?
        .parse::<usize>()
        .map_err(|_| format!("{key} must be an integer"))?;
    if !(minimum..=maximum).contains(&value) {
        return Err(format!("{key} must be between {minimum} and {maximum}"));
    }
    Ok(value)
}

fn exact_usize(
    values: &BTreeMap<String, String>,
    key: &str,
    expected: usize,
) -> Result<usize, String> {
    let value = required_value(values, key)?
        .parse::<usize>()
        .map_err(|_| format!("{key} must be the integer {expected}"))?;
    if value != expected {
        return Err(format!("{key} must be {expected} for the production model"));
    }
    Ok(value)
}

fn unit_interval(values: &BTreeMap<String, String>, key: &str) -> Result<f32, String> {
    let value = required_value(values, key)?
        .parse::<f32>()
        .map_err(|_| format!("{key} must be a number between 0 and 1"))?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("{key} must be between 0 and 1"));
    }
    Ok(value)
}

const fn usage() -> &'static str {
    "usage: pcrt-processor [--config-env-file PATH] [--env-file PATH] [--device-env-file PATH] [--sessions-dir PATH] [--result-queue-db PATH] [--ipc-endpoint ENDPOINT] [--exit-after-ms MS]"
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
        let processor = directory.path().join("processor.env");
        let device = directory.path().join("device.env");
        fs::write(
            &global,
            "SESSIONS_DIR=global-sessions\nDOOR_STATE_TTL_SEC=3\n",
        )
        .unwrap();
        fs::write(
            &processor,
            "SESSIONS_DIR=processor-sessions\nIDLE_SLEEP=0.25\n",
        )
        .unwrap();
        fs::write(&device, "BUS_ID=BUS-001\n").unwrap();

        let config = parse_args([
            "--config-env-file".to_owned(),
            global.to_string_lossy().into_owned(),
            "--env-file".to_owned(),
            processor.to_string_lossy().into_owned(),
            "--device-env-file".to_owned(),
            device.to_string_lossy().into_owned(),
            "--sessions-dir".to_owned(),
            "cli-sessions".to_owned(),
        ])
        .unwrap();

        assert_eq!(
            config.sessions_dir,
            std::path::PathBuf::from("cli-sessions")
        );
        assert_eq!(
            config.queue_path,
            std::path::PathBuf::from("cli-sessions/outbox/results.sqlite")
        );
        assert_eq!(config.door_state_ttl.as_secs(), 3);
        assert_eq!(config.idle_sleep.as_millis(), 250);
    }

    #[test]
    fn config_rejects_unrepresentable_duration() {
        let directory = tempdir().unwrap();
        let device = directory.path().join("device.env");
        fs::write(&device, "BUS_ID=BUS-001\n").unwrap();
        assert!(
            parse_args([
                "--ipc-endpoint".to_owned(),
                "ipc:///tmp/doors.sock".to_owned(),
                "--device-env-file".to_owned(),
                device.to_string_lossy().into_owned(),
            ])
            .is_ok()
        );
        let processor = directory.path().join("processor.env");
        fs::write(&processor, "DOOR_STATE_TTL_SEC=1e300\n").unwrap();

        assert!(
            parse_args([
                "--env-file".to_owned(),
                processor.to_string_lossy().into_owned(),
                "--device-env-file".to_owned(),
                device.to_string_lossy().into_owned(),
            ])
            .is_err()
        );
    }
}
