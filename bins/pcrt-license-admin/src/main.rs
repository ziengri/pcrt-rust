#![forbid(unsafe_code)]

use std::{env, fs, io::Write, path::Path, process};

use pcrt_license::{
    FORMAT_VERSION, LicensePayload, LicenseRequest, verify_envelope, verifying_key_from_base64,
};
use pcrt_license_signing::{
    generate_signing_key, sign_payload, signing_key_base64, signing_key_from_base64,
    verifying_key_base64,
};

fn main() {
    if let Err(error) = run(env::args().skip(1)) {
        eprintln!("pcrt-license-admin: {error}");
        process::exit(1);
    }
}

fn run(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    match arguments.next().as_deref() {
        Some("generate-keys") => generate_keys(arguments),
        Some("issue") => issue(arguments),
        Some("inspect") => inspect(arguments),
        _ => Err(usage().to_owned()),
    }
}

fn generate_keys(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let private_path = next_path("generate-keys", &mut arguments)?;
    let public_path = next_path("generate-keys", &mut arguments)?;
    no_extra_arguments(&mut arguments)?;
    reject_existing(&private_path)?;
    reject_existing(&public_path)?;
    let signing_key = generate_signing_key();
    write_secret_key(
        &private_path,
        &format!("{}\n", signing_key_base64(&signing_key)),
    )?;
    if let Err(error) = write_new(
        &public_path,
        &format!("{}\n", verifying_key_base64(&signing_key)),
    ) {
        let _ = fs::remove_file(&private_path);
        return Err(error);
    }
    println!("public_key_base64={}", verifying_key_base64(&signing_key));
    Ok(())
}

fn issue(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let request_path = next_path("issue", &mut arguments)?;
    let private_key_path = next_path("issue", &mut arguments)?;
    let output_path = next_path("issue", &mut arguments)?;
    let customer_id = next_value("issue", &mut arguments)?;
    let license_id = next_value("issue", &mut arguments)?;
    let issued_at = next_value("issue", &mut arguments)?;
    let valid_until = next_value("issue", &mut arguments)?;
    no_extra_arguments(&mut arguments)?;
    let request: LicenseRequest = read_json(&request_path)?;
    pcrt_license::validate_request(&request).map_err(|error| error.to_string())?;
    let private_key = read_text(&private_key_path)?;
    let signing_key =
        signing_key_from_base64(private_key.trim()).map_err(|error| error.to_string())?;
    let payload = LicensePayload {
        format_version: FORMAT_VERSION,
        product: pcrt_license::PRODUCT.to_owned(),
        license_id,
        customer_id,
        bus_id: request.bus_id,
        issued_at,
        valid_until,
        hardware_identifiers: request.hardware_identifiers,
    };
    pcrt_license::validate_payload_shape(&payload).map_err(|error| error.to_string())?;
    let envelope = sign_payload(&payload, &signing_key).map_err(|error| error.to_string())?;
    write_new(
        &output_path,
        &serde_json::to_string_pretty(&envelope).map_err(|error| error.to_string())?,
    )
}

fn inspect(mut arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let license_path = next_path("inspect", &mut arguments)?;
    let public_key_path = next_path("inspect", &mut arguments)?;
    no_extra_arguments(&mut arguments)?;
    let public_key = verifying_key_from_base64(read_text(&public_key_path)?.trim())
        .map_err(|error| error.to_string())?;
    let (_, payload) = verify_envelope(
        &fs::read(&license_path).map_err(|error| error.to_string())?,
        &public_key,
    )
    .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn next_path(
    command: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<std::path::PathBuf, String> {
    Ok(next_value(command, arguments)?.into())
}

fn next_value(
    command: &str,
    arguments: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    arguments
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| usage_for(command))
}

fn no_extra_arguments(arguments: &mut impl Iterator<Item = String>) -> Result<(), String> {
    if arguments.next().is_some() {
        Err(usage().to_owned())
    } else {
        Ok(())
    }
}

fn reject_existing(path: &Path) -> Result<(), String> {
    if path.exists() {
        Err(format!("{} already exists", path.display()))
    } else {
        Ok(())
    }
}

fn write_new(path: &Path, contents: &str) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn write_secret_key(path: &Path, contents: &str) -> Result<(), String> {
    write_new(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("set permissions {}: {error}", path.display()))?;
    }
    Ok(())
}

fn read_text(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(
        &fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", path.display()))
}

fn usage_for(_command: &str) -> String {
    usage().to_owned()
}

const fn usage() -> &'static str {
    "usage: pcrt-license-admin generate-keys PRIVATE_KEY PUBLIC_KEY | issue REQUEST PRIVATE_KEY LICENSE CUSTOMER_ID LICENSE_ID ISSUED_AT VALID_UNTIL | inspect LICENSE PUBLIC_KEY"
}
