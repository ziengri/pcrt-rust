#![forbid(unsafe_code)]
//! Offline PCRT license validation and Linux hardware identification.

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_LICENSE_PATH: &str = "/etc/pcrt/license.lic";
pub const DEMO_VALID_UNTIL: &str = "2027-12-31T23:59:59Z";
pub const PRODUCT: &str = "pcrt";
pub const FORMAT_VERSION: u32 = 1;
const HWID_DOMAIN: &[u8] = b"PCRT-HWID-v1";
const EMBEDDED_PUBLIC_KEY: [u8; 32] = [
    0x8d, 0xf2, 0x04, 0x70, 0x57, 0xe3, 0xf1, 0x0b, 0x5c, 0xb8, 0x01, 0x02, 0xb5, 0x5b, 0x9c, 0x47,
    0x5a, 0x8f, 0xe2, 0x88, 0x50, 0xfc, 0xe4, 0xb1, 0x81, 0x83, 0x26, 0x28, 0xc7, 0x63, 0xbf, 0x21,
];

/// Data supplied by a bus computer to request a license.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LicenseRequest {
    pub format_version: u32,
    pub product: String,
    pub request_id: String,
    pub bus_id: String,
    pub hardware_identifiers: Vec<HardwareIdentifier>,
}

/// Signed contents of a PCRT license.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LicensePayload {
    pub format_version: u32,
    pub product: String,
    pub license_id: String,
    pub customer_id: String,
    pub bus_id: String,
    pub issued_at: String,
    pub valid_until: String,
    pub hardware_identifiers: Vec<HardwareIdentifier>,
}

/// An individual domain-separated hardware hash.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HardwareIdentifier {
    #[serde(rename = "type")]
    pub kind: HardwareIdentifierType,
    pub sha256: String,
}

/// Hardware identifier types supported by format version one.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareIdentifierType {
    SmbiosUuid,
    SystemDiskSerial,
    SystemDiskWwid,
}

impl HardwareIdentifierType {
    const fn label(self) -> &'static str {
        match self {
            Self::SmbiosUuid => "smbios_uuid",
            Self::SystemDiskSerial => "system_disk_serial",
            Self::SystemDiskWwid => "system_disk_wwid",
        }
    }
}

/// The unsigned outer JSON license envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LicenseEnvelope {
    pub format_version: u32,
    pub payload_base64: String,
    pub signature_base64: String,
}

/// Parsed and verified license data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedLicense {
    pub payload: LicensePayload,
}

/// Sanitized reason a license cannot permit a PCRT operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LicenseError {
    Io(String),
    InvalidFormat(String),
    InvalidSignature,
    PublicKeyUnavailable,
    PublicKeyInvalid,
    ProductMismatch,
    BusMismatch,
    Expired,
    HardwareMismatch,
    HardwareUnavailable(String),
}

impl fmt::Display for LicenseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("license file is unavailable"),
            Self::InvalidFormat(_) => formatter.write_str("license format is invalid"),
            Self::InvalidSignature => formatter.write_str("license signature is invalid"),
            Self::PublicKeyUnavailable => {
                formatter.write_str("license public key is not configured")
            }
            Self::PublicKeyInvalid => formatter.write_str("license public key is invalid"),
            Self::ProductMismatch => formatter.write_str("license is for a different product"),
            Self::BusMismatch => formatter.write_str("license is for a different bus"),
            Self::Expired => formatter.write_str("license has expired"),
            Self::HardwareMismatch => formatter.write_str("license hardware does not match"),
            Self::HardwareUnavailable(reason) => {
                write!(
                    formatter,
                    "required license hardware is unavailable: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for LicenseError {}

/// Returns the verifying key compiled into this PCRT build.
#[allow(clippy::missing_errors_doc)]
pub fn embedded_public_key() -> Result<VerifyingKey, LicenseError> {
    VerifyingKey::from_bytes(&EMBEDDED_PUBLIC_KEY).map_err(|_| LicenseError::PublicKeyInvalid)
}

/// Parses one Base64-encoded Ed25519 public key.
#[allow(clippy::missing_errors_doc)]
pub fn verifying_key_from_base64(value: &str) -> Result<VerifyingKey, LicenseError> {
    let decoded = STANDARD
        .decode(value)
        .map_err(|_| LicenseError::PublicKeyInvalid)?;
    let bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| LicenseError::PublicKeyInvalid)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| LicenseError::PublicKeyInvalid)
}

/// Reads, verifies and validates the installed license for one bus computer.
#[allow(clippy::missing_errors_doc)]
pub fn validate_installed(bus_id: &str) -> Result<VerifiedLicense, LicenseError> {
    validate_file(
        Path::new(DEFAULT_LICENSE_PATH),
        bus_id,
        &embedded_public_key()?,
    )
}

/// Reads, verifies and validates a license file using the supplied public key.
#[allow(clippy::missing_errors_doc)]
pub fn validate_file(
    path: &Path,
    bus_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<VerifiedLicense, LicenseError> {
    let contents = fs::read(path).map_err(|error| io_error(&error))?;
    validate_bytes(&contents, bus_id, verifying_key)
}

/// Verifies and validates one `license.lic` JSON document.
#[allow(clippy::missing_errors_doc)]
pub fn validate_bytes(
    contents: &[u8],
    bus_id: &str,
    verifying_key: &VerifyingKey,
) -> Result<VerifiedLicense, LicenseError> {
    let (payload_bytes, payload) = verify_envelope(contents, verifying_key)?;
    let _ = payload_bytes;
    validate_payload(&payload, bus_id)?;
    Ok(VerifiedLicense { payload })
}

/// Verifies a license document and returns its payload bytes and parsed payload.
#[allow(clippy::missing_errors_doc)]
pub fn verify_envelope(
    contents: &[u8],
    verifying_key: &VerifyingKey,
) -> Result<(Vec<u8>, LicensePayload), LicenseError> {
    let envelope: LicenseEnvelope = serde_json::from_slice(contents)
        .map_err(|_| LicenseError::InvalidFormat("outer JSON".to_owned()))?;
    if envelope.format_version != FORMAT_VERSION {
        return Err(LicenseError::InvalidFormat(
            "outer format version".to_owned(),
        ));
    }
    let payload_bytes = STANDARD
        .decode(envelope.payload_base64)
        .map_err(|_| LicenseError::InvalidFormat("payload encoding".to_owned()))?;
    let signature_bytes = STANDARD
        .decode(envelope.signature_base64)
        .map_err(|_| LicenseError::InvalidFormat("signature encoding".to_owned()))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| LicenseError::InvalidFormat("signature length".to_owned()))?;
    verifying_key
        .verify(&payload_bytes, &signature)
        .map_err(|_| LicenseError::InvalidSignature)?;
    let payload = serde_json::from_slice(&payload_bytes)
        .map_err(|_| LicenseError::InvalidFormat("payload JSON".to_owned()))?;
    Ok((payload_bytes, payload))
}

/// Creates a hardware-bound license request from current Linux hardware.
#[allow(clippy::missing_errors_doc)]
pub fn create_request(bus_id: &str, request_id: String) -> Result<LicenseRequest, LicenseError> {
    validate_nonempty("bus id", bus_id)?;
    validate_uuid_format("request id", &request_id)?;
    Ok(LicenseRequest {
        format_version: FORMAT_VERSION,
        product: PRODUCT.to_owned(),
        request_id,
        bus_id: bus_id.to_owned(),
        hardware_identifiers: collect_hardware_identifiers()?,
    })
}

/// Reads and hashes required identifiers from the current Linux computer.
#[allow(clippy::missing_errors_doc)]
pub fn collect_hardware_identifiers() -> Result<Vec<HardwareIdentifier>, LicenseError> {
    let (disk_kind, disk_value) = read_system_disk_identifier()?;
    Ok(vec![
        HardwareIdentifier {
            kind: HardwareIdentifierType::SmbiosUuid,
            sha256: hardware_hash(HardwareIdentifierType::SmbiosUuid, &read_smbios_uuid()?),
        },
        HardwareIdentifier {
            kind: disk_kind,
            sha256: hardware_hash(disk_kind, &disk_value),
        },
    ])
}

/// Validates license payload fields and binding against current hardware.
#[allow(clippy::missing_errors_doc)]
pub fn validate_payload(payload: &LicensePayload, bus_id: &str) -> Result<(), LicenseError> {
    validate_payload_shape(payload)?;
    if payload.bus_id != bus_id {
        return Err(LicenseError::BusMismatch);
    }
    let now = unix_now()?;
    if now > parse_rfc3339(&payload.valid_until)? {
        return Err(LicenseError::Expired);
    }
    let current = collect_hardware_identifiers()?;
    if !hardware_identifiers_match(&current, &payload.hardware_identifiers) {
        return Err(LicenseError::HardwareMismatch);
    }
    Ok(())
}

/// Validates license payload fields independent of the current computer.
#[allow(clippy::missing_errors_doc)]
pub fn validate_payload_shape(payload: &LicensePayload) -> Result<(), LicenseError> {
    if payload.format_version != FORMAT_VERSION {
        return Err(LicenseError::InvalidFormat(
            "payload format version".to_owned(),
        ));
    }
    if payload.product != PRODUCT {
        return Err(LicenseError::ProductMismatch);
    }
    validate_uuid_format("license id", &payload.license_id)?;
    validate_nonempty("customer id", &payload.customer_id)?;
    validate_nonempty("bus id", &payload.bus_id)?;
    let issued_at = parse_rfc3339(&payload.issued_at)?;
    let valid_until = parse_rfc3339(&payload.valid_until)?;
    if issued_at > valid_until || payload.valid_until != DEMO_VALID_UNTIL {
        return Err(LicenseError::InvalidFormat("license dates".to_owned()));
    }
    validate_required_identifier_set(&payload.hardware_identifiers)?;
    Ok(())
}

/// Validates request fields before issuing a license.
#[allow(clippy::missing_errors_doc)]
pub fn validate_request(request: &LicenseRequest) -> Result<(), LicenseError> {
    if request.format_version != FORMAT_VERSION || request.product != PRODUCT {
        return Err(LicenseError::InvalidFormat(
            "request product or format".to_owned(),
        ));
    }
    validate_uuid_format("request id", &request.request_id)?;
    validate_nonempty("bus id", &request.bus_id)?;
    validate_required_identifier_set(&request.hardware_identifiers)
}

/// Reads and normalizes the SMBIOS UUID exposed by Linux DMI.
#[allow(clippy::missing_errors_doc)]
pub fn read_smbios_uuid() -> Result<String, LicenseError> {
    let value = read_identifier_file(Path::new("/sys/class/dmi/id/product_uuid"))?;
    let normalized = value.trim_ascii().to_ascii_uppercase();
    validate_hardware_value("SMBIOS UUID", &normalized)?;
    validate_uuid("SMBIOS UUID", &normalized)?;
    Ok(normalized)
}

/// Reads and normalizes the serial of the physical disk hosting `/`.
#[allow(clippy::missing_errors_doc)]
pub fn read_system_disk_serial() -> Result<String, LicenseError> {
    let (kind, value) = read_system_disk_identifier()?;
    if kind == HardwareIdentifierType::SystemDiskSerial {
        Ok(value)
    } else {
        Err(LicenseError::HardwareUnavailable(
            "system disk serial is unavailable; WWID fallback is in use".to_owned(),
        ))
    }
}

/// Reads the preferred stable identifier of the physical disk hosting `/`.
#[allow(clippy::missing_errors_doc)]
pub fn read_system_disk_identifier() -> Result<(HardwareIdentifierType, String), LicenseError> {
    read_system_disk_identifier_from(Path::new("/proc/self/mountinfo"), Path::new("/sys"))
}

fn read_system_disk_identifier_from(
    mountinfo: &Path,
    sys_root: &Path,
) -> Result<(HardwareIdentifierType, String), LicenseError> {
    let source = fs::read_to_string(mountinfo).map_err(|error| io_error(&error))?;
    let major_minor = root_major_minor(&source)?;
    let device_link = sys_root.join("dev/block").join(&major_minor);
    let resolved = fs::canonicalize(&device_link).map_err(|error| io_error(&error))?;
    let disk = physical_disk_name(&resolved)?;
    let mut current = fs::canonicalize(sys_root.join("class/block").join(&disk))
        .map_err(|error| io_error(&error))?;
    loop {
        for serial_path in [current.join("device/serial"), current.join("serial")] {
            if serial_path.is_file()
                && let Ok(value) = read_identifier_file(&serial_path)
                    .and_then(|value| normalize_disk_identifier(&value))
            {
                return Ok((HardwareIdentifierType::SystemDiskSerial, value));
            }
        }
        for wwid_path in [
            current.join("device/wwid"),
            current.join("device/cid"),
            current.join("wwid"),
            current.join("cid"),
        ] {
            if wwid_path.is_file()
                && let Ok(value) = read_identifier_file(&wwid_path)
                    .and_then(|value| normalize_disk_identifier(&value))
            {
                return Ok((HardwareIdentifierType::SystemDiskWwid, value));
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current || !parent.starts_with(sys_root) {
            break;
        }
        current = parent.to_owned();
    }
    Err(LicenseError::HardwareUnavailable(format!(
        "system disk {disk} has no serial or WWID"
    )))
}

fn root_major_minor(mountinfo: &str) -> Result<String, LicenseError> {
    let line = mountinfo
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some("/"))
        .ok_or_else(|| LicenseError::HardwareUnavailable("root mount is unavailable".to_owned()))?;
    let major_minor = line
        .split_whitespace()
        .nth(2)
        .filter(|value| value.contains(':'))
        .ok_or_else(|| {
            LicenseError::HardwareUnavailable("root device is unavailable".to_owned())
        })?;
    Ok(major_minor.to_owned())
}

fn physical_disk_name(resolved: &Path) -> Result<String, LicenseError> {
    let current_name = resolved
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            LicenseError::HardwareUnavailable("root is not a physical disk".to_owned())
        })?;
    let block = if resolved.join("partition").is_file() {
        resolved
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                LicenseError::HardwareUnavailable("root disk is unavailable".to_owned())
            })?
    } else {
        current_name
    };
    if block.starts_with("dm-") || block.starts_with("md") || block.starts_with("loop") {
        return Err(LicenseError::HardwareUnavailable(
            "virtual root device is unsupported".to_owned(),
        ));
    }
    Ok(block.to_owned())
}

fn normalize_disk_identifier(value: &str) -> Result<String, LicenseError> {
    let normalized = value.trim_ascii().to_ascii_uppercase();
    validate_hardware_value("system disk identifier", &normalized)?;
    Ok(normalized)
}

fn read_identifier_file(path: &Path) -> Result<String, LicenseError> {
    fs::read_to_string(path).map_err(|error| io_error(&error))
}

fn validate_required_identifier_set(
    identifiers: &[HardwareIdentifier],
) -> Result<(), LicenseError> {
    if identifiers.len() != 2 {
        return Err(LicenseError::InvalidFormat(
            "hardware identifier count".to_owned(),
        ));
    }
    let mut expected = BTreeMap::new();
    for identifier in identifiers {
        if identifier.sha256.len() != 64
            || !identifier
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(LicenseError::InvalidFormat("hardware hash".to_owned()));
        }
        if expected
            .insert(identifier.kind, identifier.sha256.to_ascii_uppercase())
            .is_some()
        {
            return Err(LicenseError::InvalidFormat(
                "duplicate hardware type".to_owned(),
            ));
        }
    }
    if expected.len() != 2
        || !expected.contains_key(&HardwareIdentifierType::SmbiosUuid)
        || (expected.contains_key(&HardwareIdentifierType::SystemDiskSerial)
            == expected.contains_key(&HardwareIdentifierType::SystemDiskWwid))
    {
        return Err(LicenseError::InvalidFormat(
            "required hardware types".to_owned(),
        ));
    }
    Ok(())
}

fn hardware_identifiers_match(
    current: &[HardwareIdentifier],
    licensed: &[HardwareIdentifier],
) -> bool {
    let to_map = |identifiers: &[HardwareIdentifier]| {
        identifiers
            .iter()
            .map(|identifier| (identifier.kind, identifier.sha256.to_ascii_uppercase()))
            .collect::<BTreeMap<_, _>>()
    };
    to_map(current) == to_map(licensed)
}

fn validate_uuid(name: &str, value: &str) -> Result<(), LicenseError> {
    validate_uuid_format(name, value).and_then(|()| {
        if is_placeholder(value) {
            Err(LicenseError::HardwareUnavailable(name.to_owned()))
        } else {
            Ok(())
        }
    })
}

fn validate_uuid_format(name: &str, value: &str) -> Result<(), LicenseError> {
    let valid = value.len() == 36
        && [8, 13, 18, 23]
            .iter()
            .all(|index| value.as_bytes()[*index] == b'-')
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit());
    if valid {
        Ok(())
    } else {
        Err(LicenseError::InvalidFormat(name.to_owned()))
    }
}

fn validate_nonempty(name: &str, value: &str) -> Result<(), LicenseError> {
    if value.trim().is_empty() {
        Err(LicenseError::InvalidFormat(name.to_owned()))
    } else {
        Ok(())
    }
}

fn validate_hardware_value(name: &str, value: &str) -> Result<(), LicenseError> {
    if value.is_empty() || is_placeholder(value) {
        Err(LicenseError::HardwareUnavailable(name.to_owned()))
    } else {
        Ok(())
    }
}

fn is_placeholder(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|character| *character != '-' && !character.is_ascii_whitespace())
        .collect();
    compact.chars().all(|character| character == '0')
        || matches!(
            value,
            "UNKNOWN"
                | "NONE"
                | "NOT SPECIFIED"
                | "DEFAULT"
                | "N/A"
                | "TO BE FILLED BY O.E.M."
                | "TO BE FILLED BY OEM"
        )
}

/// Calculates a domain-separated SHA-256 hardware hash.
#[must_use]
pub fn hardware_hash(kind: HardwareIdentifierType, normalized_value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(HWID_DOMAIN);
    hasher.update([0]);
    hasher.update(kind.label());
    hasher.update([0]);
    hasher.update(normalized_value);
    format!("{:x}", hasher.finalize())
}

fn parse_rfc3339(value: &str) -> Result<i64, LicenseError> {
    if value.len() != 20
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
        || value.as_bytes().get(10) != Some(&b'T')
        || value.as_bytes().get(13) != Some(&b':')
        || value.as_bytes().get(16) != Some(&b':')
        || value.as_bytes().get(19) != Some(&b'Z')
    {
        return Err(LicenseError::InvalidFormat("timestamp".to_owned()));
    }
    let (date, time) = value
        .split_once('T')
        .ok_or_else(|| LicenseError::InvalidFormat("timestamp".to_owned()))?;
    let time = time
        .strip_suffix('Z')
        .ok_or_else(|| LicenseError::InvalidFormat("timestamp timezone".to_owned()))?;
    let mut date = date.split('-');
    let year = parse_time_component(date.next(), "year")?;
    let month = parse_time_component(date.next(), "month")?;
    let day = parse_time_component(date.next(), "day")?;
    if date.next().is_some() {
        return Err(LicenseError::InvalidFormat("timestamp date".to_owned()));
    }
    let mut time = time.split(':');
    let hour = parse_time_component(time.next(), "hour")?;
    let minute = parse_time_component(time.next(), "minute")?;
    let second = parse_time_component(time.next(), "second")?;
    if time.next().is_some()
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(LicenseError::InvalidFormat("timestamp value".to_owned()));
    }
    let max_day = days_in_month(year, month);
    if day == 0 || day > max_day {
        return Err(LicenseError::InvalidFormat("timestamp day".to_owned()));
    }
    let days = days_since_unix_epoch(year, month, day);
    days.checked_mul(86_400)
        .and_then(|seconds| seconds.checked_add(i64::from(hour) * 3_600))
        .and_then(|seconds| seconds.checked_add(i64::from(minute) * 60))
        .and_then(|seconds| seconds.checked_add(i64::from(second)))
        .ok_or_else(|| LicenseError::InvalidFormat("timestamp range".to_owned()))
}

fn parse_time_component(value: Option<&str>, name: &str) -> Result<i32, LicenseError> {
    value
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| LicenseError::InvalidFormat(format!("timestamp {name}")))?
        .parse()
        .map_err(|_| LicenseError::InvalidFormat(format!("timestamp {name}")))
}

const fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

const fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn days_since_unix_epoch(year: i32, month: i32, day: i32) -> i64 {
    let adjusted_year = year - i32::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era * 146_097 + day_of_era - 719_468)
}

fn unix_now() -> Result<i64, LicenseError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| LicenseError::InvalidFormat("system clock".to_owned()))
        .and_then(|duration| {
            i64::try_from(duration.as_secs())
                .map_err(|_| LicenseError::InvalidFormat("system clock".to_owned()))
        })
}

fn io_error(error: &io::Error) -> LicenseError {
    LicenseError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;
    #[cfg(unix)]
    use tempfile::tempdir;

    use super::*;

    const UUID: &str = "03000200-0400-0500-0006-000700080009";

    #[test]
    fn hardware_hash_is_stable_and_domain_separated() {
        assert_ne!(
            hardware_hash(HardwareIdentifierType::SmbiosUuid, UUID),
            hardware_hash(HardwareIdentifierType::SystemDiskSerial, UUID)
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_disk_identifier_uses_parent_physical_disk() {
        let directory = tempdir().unwrap();
        let sys = directory.path().join("sys");
        let devices = sys.join("devices/pci/block/sda/sda1");
        fs::create_dir_all(&devices).unwrap();
        fs::write(devices.join("partition"), "1\n").unwrap();
        fs::create_dir_all(sys.join("devices/pci/block/sda/device")).unwrap();
        fs::write(
            sys.join("devices/pci/block/sda/device/serial"),
            " disk-1 \n",
        )
        .unwrap();
        fs::create_dir_all(sys.join("dev/block")).unwrap();
        fs::create_dir_all(sys.join("class/block")).unwrap();
        std::os::unix::fs::symlink(&devices, sys.join("dev/block/8:1")).unwrap();
        std::os::unix::fs::symlink(
            sys.join("devices/pci/block/sda"),
            sys.join("class/block/sda"),
        )
        .unwrap();
        let mountinfo = directory.path().join("mountinfo");
        fs::write(&mountinfo, "1 2 8:1 / / rw - ext4 /dev/sda1 rw\n").unwrap();
        assert_eq!(
            read_system_disk_identifier_from(&mountinfo, &sys).unwrap(),
            (
                HardwareIdentifierType::SystemDiskSerial,
                "DISK-1".to_owned()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_disk_identifier_falls_back_to_wwid() {
        let directory = tempdir().unwrap();
        let sys = directory.path().join("sys");
        let devices = sys.join("devices/pci/block/sda/sda1");
        fs::create_dir_all(&devices).unwrap();
        fs::write(devices.join("partition"), "1\n").unwrap();
        fs::create_dir_all(sys.join("devices/pci/block/sda/device")).unwrap();
        fs::write(
            sys.join("devices/pci/block/sda/device/wwid"),
            " naa.1234 \n",
        )
        .unwrap();
        fs::create_dir_all(sys.join("dev/block")).unwrap();
        fs::create_dir_all(sys.join("class/block")).unwrap();
        std::os::unix::fs::symlink(&devices, sys.join("dev/block/8:1")).unwrap();
        std::os::unix::fs::symlink(
            sys.join("devices/pci/block/sda"),
            sys.join("class/block/sda"),
        )
        .unwrap();
        let mountinfo = directory.path().join("mountinfo");
        fs::write(&mountinfo, "1 2 8:1 / / rw - ext4 /dev/sda1 rw\n").unwrap();

        assert_eq!(
            read_system_disk_identifier_from(&mountinfo, &sys).unwrap(),
            (
                HardwareIdentifierType::SystemDiskWwid,
                "NAA.1234".to_owned()
            )
        );
    }

    #[test]
    fn envelope_signature_is_verified_before_payload_parse() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let payload = payload();
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let envelope = LicenseEnvelope {
            format_version: FORMAT_VERSION,
            payload_base64: STANDARD.encode(&payload_bytes),
            signature_base64: STANDARD.encode(signing_key.sign(&payload_bytes).to_bytes()),
        };
        let contents = serde_json::to_vec(&envelope).unwrap();
        let (_, parsed) = verify_envelope(&contents, &signing_key.verifying_key()).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn modified_payload_is_rejected() {
        let signing_key = SigningKey::generate(&mut OsRng);
        let payload_bytes = serde_json::to_vec(&payload()).unwrap();
        let envelope = LicenseEnvelope {
            format_version: FORMAT_VERSION,
            payload_base64: STANDARD.encode(b"{}"),
            signature_base64: STANDARD.encode(signing_key.sign(&payload_bytes).to_bytes()),
        };
        assert_eq!(
            verify_envelope(
                &serde_json::to_vec(&envelope).unwrap(),
                &signing_key.verifying_key()
            )
            .unwrap_err(),
            LicenseError::InvalidSignature
        );
    }

    #[test]
    fn invalid_placeholder_is_rejected() {
        assert!(validate_hardware_value("serial", "TO BE FILLED BY O.E.M.").is_err());
        assert!(validate_uuid("uuid", "00000000-0000-0000-0000-000000000000").is_err());
    }

    #[test]
    fn embedded_public_key_is_valid() {
        assert!(embedded_public_key().is_ok());
    }

    fn payload() -> LicensePayload {
        LicensePayload {
            format_version: FORMAT_VERSION,
            product: PRODUCT.to_owned(),
            license_id: "0f6e5ed8-09a7-47a2-9cb6-6b598ed923da".to_owned(),
            customer_id: "carrier-17".to_owned(),
            bus_id: "BUS-042".to_owned(),
            issued_at: "2026-01-01T00:00:00Z".to_owned(),
            valid_until: "2027-12-31T23:59:59Z".to_owned(),
            hardware_identifiers: vec![
                HardwareIdentifier {
                    kind: HardwareIdentifierType::SmbiosUuid,
                    sha256: hardware_hash(HardwareIdentifierType::SmbiosUuid, UUID),
                },
                HardwareIdentifier {
                    kind: HardwareIdentifierType::SystemDiskWwid,
                    sha256: hardware_hash(HardwareIdentifierType::SystemDiskWwid, "DISK-1"),
                },
            ],
        }
    }
}
