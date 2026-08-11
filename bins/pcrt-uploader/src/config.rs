//! Validated uploader command-line and environment configuration.

use std::{collections::BTreeMap, env, fs, path::PathBuf, time::Duration};

const DEFAULT_QUEUE_PATH: &str = "sessions/outbox/results.sqlite";
const DEFAULT_POLL_INTERVAL_SEC: &str = "1";
const DEFAULT_INITIAL_BACKOFF_SEC: &str = "5";
const DEFAULT_MAX_BACKOFF_SEC: &str = "900";
const DEFAULT_API_TIMEOUT_SEC: &str = "10";

pub(super) struct UploaderProcessConfig {
    pub(super) queue_path: PathBuf,
    pub(super) api_base_url: String,
    pub(super) api_x_auth: String,
    pub(super) api_timeout: Duration,
    pub(super) poll_interval: Duration,
    pub(super) initial_backoff: Duration,
    pub(super) max_backoff: Duration,
    pub(super) exit_after: Option<Duration>,
}

pub(super) fn parse_args(
    arguments: impl IntoIterator<Item = String>,
) -> Result<UploaderProcessConfig, String> {
    let mut arguments = arguments.into_iter();
    let mut config_path = "config.env".to_owned();
    let mut uploader_path = "uploader.env".to_owned();
    let mut overrides = BTreeMap::new();
    let mut exit_after = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config-env-file" => config_path = argument_value(&argument, &mut arguments)?,
            "--env-file" => uploader_path = argument_value(&argument, &mut arguments)?,
            "--result-queue-db" => set_override(
                &mut overrides,
                "RESULT_QUEUE_DB",
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
    values.extend(read_env_file(&uploader_path)?);
    for key in environment_keys() {
        if let Ok(value) = env::var(key)
            && !value.is_empty()
        {
            values.insert((*key).to_owned(), value);
        }
    }
    values.extend(overrides);

    Ok(UploaderProcessConfig {
        queue_path: PathBuf::from(required_value(&values, "RESULT_QUEUE_DB")?),
        api_base_url: required_value(&values, "API_BASE_URL")?,
        api_x_auth: required_value(&values, "API_X_AUTH")?,
        api_timeout: positive_duration_seconds(&values, "API_TIMEOUT_SEC")?,
        poll_interval: positive_duration_seconds(&values, "UPLOADER_POLL_INTERVAL_SEC")?,
        initial_backoff: positive_duration_seconds(&values, "UPLOADER_INITIAL_BACKOFF_SEC")?,
        max_backoff: positive_duration_seconds(&values, "UPLOADER_MAX_BACKOFF_SEC")?,
        exit_after,
    })
}

fn defaults() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("RESULT_QUEUE_DB".to_owned(), DEFAULT_QUEUE_PATH.to_owned()),
        (
            "UPLOADER_POLL_INTERVAL_SEC".to_owned(),
            DEFAULT_POLL_INTERVAL_SEC.to_owned(),
        ),
        (
            "UPLOADER_INITIAL_BACKOFF_SEC".to_owned(),
            DEFAULT_INITIAL_BACKOFF_SEC.to_owned(),
        ),
        (
            "UPLOADER_MAX_BACKOFF_SEC".to_owned(),
            DEFAULT_MAX_BACKOFF_SEC.to_owned(),
        ),
        (
            "API_TIMEOUT_SEC".to_owned(),
            DEFAULT_API_TIMEOUT_SEC.to_owned(),
        ),
    ])
}

const fn environment_keys() -> &'static [&'static str] {
    &[
        "RESULT_QUEUE_DB",
        "API_BASE_URL",
        "API_X_AUTH",
        "API_TIMEOUT_SEC",
        "UPLOADER_POLL_INTERVAL_SEC",
        "UPLOADER_INITIAL_BACKOFF_SEC",
        "UPLOADER_MAX_BACKOFF_SEC",
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

const fn usage() -> &'static str {
    "usage: pcrt-uploader [--config-env-file PATH] [--env-file PATH] [--result-queue-db PATH] [--exit-after-ms MS]"
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::parse_args;

    #[test]
    fn config_uses_uploader_file_and_cli_precedence() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.env");
        let uploader = directory.path().join("uploader.env");
        fs::write(
            &config,
            "RESULT_QUEUE_DB=global.sqlite\nAPI_BASE_URL=http://global.example\nAPI_X_AUTH=global-secret\n",
        )
        .unwrap();
        fs::write(
            &uploader,
            "RESULT_QUEUE_DB=uploader.sqlite\nAPI_BASE_URL=http://uploader.example\nAPI_X_AUTH=uploader-secret\nUPLOADER_POLL_INTERVAL_SEC=0.25\n",
        )
        .unwrap();

        let parsed = parse_args([
            "--config-env-file".to_owned(),
            config.to_string_lossy().into_owned(),
            "--env-file".to_owned(),
            uploader.to_string_lossy().into_owned(),
            "--result-queue-db".to_owned(),
            "cli.sqlite".to_owned(),
        ])
        .unwrap();

        assert_eq!(parsed.queue_path, std::path::PathBuf::from("cli.sqlite"));
        assert_eq!(parsed.api_base_url, "http://uploader.example");
        assert_eq!(parsed.poll_interval, std::time::Duration::from_millis(250));
    }

    #[test]
    fn config_requires_api_credentials() {
        let directory = tempdir().unwrap();
        let uploader = directory.path().join("uploader.env");
        fs::write(&uploader, "API_BASE_URL=http://api.example\n").unwrap();

        assert!(
            parse_args([
                "--env-file".to_owned(),
                uploader.to_string_lossy().into_owned(),
            ])
            .is_err()
        );
    }
}
