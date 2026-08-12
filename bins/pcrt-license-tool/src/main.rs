#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process,
};

use pcrt_license::{DEFAULT_LICENSE_PATH, create_request, embedded_public_key, validate_file};
use rand_core::{OsRng, RngCore};

fn main() {
    if let Err(error) = run(env::args().skip(1)) {
        eprintln!("pcrt-license-tool: {error}");
        process::exit(1);
    }
}

fn run(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    match arguments.next().as_deref() {
        Some("request") => request(arguments),
        Some("import") => import(arguments),
        Some("status") => status(arguments),
        _ => Err(usage().to_owned()),
    }
}

fn request(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let mut device_env = PathBuf::from("/etc/pcrt/device.env");
    let mut output = PathBuf::from("license-request.json");
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--device-env-file" => device_env = next_path(&mut arguments)?,
            "--output" => output = next_path(&mut arguments)?,
            _ => return Err(usage().to_owned()),
        }
    }
    let values = read_env_file(&device_env)?;
    let bus_id = values
        .get("BUS_ID")
        .filter(|value| !value.is_empty())
        .ok_or("BUS_ID is required".to_owned())?;
    let request = create_request(bus_id, generate_uuid_v4()?).map_err(|error| error.to_string())?;
    write_new(
        &output,
        &serde_json::to_string_pretty(&request).map_err(|error| error.to_string())?,
    )
}

fn import(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let source = next_path(&mut arguments)?;
    let mut device_env = PathBuf::from("/etc/pcrt/device.env");
    let mut destination = PathBuf::from(DEFAULT_LICENSE_PATH);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--device-env-file" => device_env = next_path(&mut arguments)?,
            "--license-path" => destination = next_path(&mut arguments)?,
            _ => return Err(usage().to_owned()),
        }
    }
    let bus_id = required_bus_id(&device_env)?;
    let key = embedded_public_key().map_err(|error| error.to_string())?;
    validate_file(&source, &bus_id, &key).map_err(|error| error.to_string())?;
    install_atomically(&source, &destination)
}

fn status(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let mut device_env = PathBuf::from("/etc/pcrt/device.env");
    let mut license_path = PathBuf::from(DEFAULT_LICENSE_PATH);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--device-env-file" => device_env = next_path(&mut arguments)?,
            "--license-path" => license_path = next_path(&mut arguments)?,
            _ => return Err(usage().to_owned()),
        }
    }
    let bus_id = required_bus_id(&device_env)?;
    let key = embedded_public_key().map_err(|error| error.to_string())?;
    let license = validate_file(&license_path, &bus_id, &key).map_err(|error| error.to_string())?;
    println!("license_id={}", license.payload.license_id);
    println!("bus_id={}", license.payload.bus_id);
    println!("valid_until={}", license.payload.valid_until);
    println!("status=valid");
    Ok(())
}

fn required_bus_id(path: &Path) -> Result<String, String> {
    read_env_file(path)?
        .remove("BUS_ID")
        .filter(|value| !value.is_empty())
        .ok_or("BUS_ID is required".to_owned())
}

fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>, String> {
    let contents =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut values = BTreeMap::new();
    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "{}:{} must be KEY=VALUE",
                path.display(),
                line_number + 1
            ));
        };
        values.insert(
            key.trim().to_owned(),
            value.trim().trim_matches('"').to_owned(),
        );
    }
    Ok(values)
}

fn install_atomically(source: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or("license path has no parent directory".to_owned())?;
    let temporary = parent.join(format!(".license.lic.{}.tmp", process::id()));
    let bytes = fs::read(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("write {}: {error}", temporary.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("set permissions {}: {error}", temporary.display()))?;
    }
    fs::rename(&temporary, destination)
        .map_err(|error| format!("install {}: {error}", destination.display()))
}

fn write_new(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
}

fn next_path(arguments: &mut impl Iterator<Item = String>) -> Result<PathBuf, String> {
    arguments
        .next()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| usage().to_owned())
}

fn generate_uuid_v4() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| error.to_string())?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

const fn usage() -> &'static str {
    "usage: pcrt-license-tool request [--device-env-file PATH] [--output PATH] | import LICENSE [--device-env-file PATH] [--license-path PATH] | status [--device-env-file PATH] [--license-path PATH]"
}
