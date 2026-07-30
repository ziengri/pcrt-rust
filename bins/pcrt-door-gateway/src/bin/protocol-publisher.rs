#![forbid(unsafe_code)]
//! Test-only raw byte publisher for `pcrt-door-gateway`.

use std::{
    env, fs,
    io::Write,
    os::unix::{fs::FileTypeExt, net::UnixListener},
    process, thread,
    time::Duration,
};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioEvent {
    after_ms: u64,
    #[serde(default)]
    bytes_hex: Option<String>,
    #[serde(default)]
    disconnect: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("pcrt-door-protocol-publisher: {error}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (listen_path, scenario_path) = parse_args(env::args().skip(1))?;
    let events: Vec<ScenarioEvent> = serde_json::from_str(
        &fs::read_to_string(&scenario_path)
            .map_err(|error| format!("read scenario {scenario_path}: {error}"))?,
    )
    .map_err(|error| format!("parse scenario {scenario_path}: {error}"))?;
    if events.is_empty() {
        return Err("scenario must contain at least one event".to_owned());
    }
    match fs::symlink_metadata(&listen_path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&listen_path)
            .map_err(|error| format!("remove stale test source {listen_path}: {error}"))?,
        Ok(_) => return Err(format!("refusing to replace non-socket {listen_path}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect test source {listen_path}: {error}")),
    }
    let listener = UnixListener::bind(&listen_path)
        .map_err(|error| format!("bind test source {listen_path}: {error}"))?;
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("accept gateway source connection: {error}"))?;
    for event in events {
        thread::sleep(Duration::from_millis(event.after_ms));
        if let Some(bytes_hex) = event.bytes_hex {
            let bytes = decode_hex(&bytes_hex)?;
            stream
                .write_all(&bytes)
                .map_err(|error| format!("write raw test bytes: {error}"))?;
        }
        if event.disconnect {
            return Ok(());
        }
    }
    Ok(())
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<(String, String), String> {
    let mut listen_path = None;
    let mut scenario_path = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("{argument} requires a value"))?;
        match argument.as_str() {
            "--listen" => listen_path = Some(value),
            "--scenario" => scenario_path = Some(value),
            _ => return Err(format!("unknown argument {argument}\n{}", usage())),
        }
    }
    match (listen_path, scenario_path) {
        (Some(listen_path), Some(scenario_path)) => Ok((listen_path, scenario_path)),
        _ => Err(usage().to_owned()),
    }
}

const fn usage() -> &'static str {
    "usage: pcrt-door-protocol-publisher --listen PATH --scenario SCENARIO.json"
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    let compact = value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    if compact.len() % 2 != 0 {
        return Err("bytes_hex must contain an even number of hexadecimal digits".to_owned());
    }
    (0..compact.len())
        .step_by(2)
        .map(|offset| {
            u8::from_str_radix(&compact[offset..offset + 2], 16)
                .map_err(|_| format!("invalid hexadecimal byte at offset {offset}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::decode_hex;

    #[test]
    fn decodes_raw_binary_scenario_bytes() {
        assert_eq!(decode_hex("00 3b ff").unwrap(), [0, 59, 255]);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
