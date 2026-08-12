#![forbid(unsafe_code)]
//! Private-key operations used only by the offline license issuer.

use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use pcrt_license::{FORMAT_VERSION, LicenseEnvelope, LicenseError, LicensePayload};
use rand_core::OsRng;

/// Generates one Ed25519 signing key from the operating system random source.
pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

/// Parses a Base64-encoded 32-byte private Ed25519 key.
#[allow(clippy::missing_errors_doc)]
pub fn signing_key_from_base64(value: &str) -> Result<SigningKey, LicenseError> {
    let decoded = STANDARD
        .decode(value)
        .map_err(|_| LicenseError::InvalidFormat("private key encoding".to_owned()))?;
    let bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| LicenseError::InvalidFormat("private key length".to_owned()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Serializes and signs the exact payload bytes stored in the license envelope.
#[allow(clippy::missing_errors_doc)]
pub fn sign_payload(
    payload: &LicensePayload,
    signing_key: &SigningKey,
) -> Result<LicenseEnvelope, LicenseError> {
    let payload_bytes = serde_json::to_vec(payload)
        .map_err(|_| LicenseError::InvalidFormat("payload serialization".to_owned()))?;
    Ok(LicenseEnvelope {
        format_version: FORMAT_VERSION,
        payload_base64: STANDARD.encode(&payload_bytes),
        signature_base64: STANDARD.encode(signing_key.sign(&payload_bytes).to_bytes()),
    })
}

/// Exports a key as Base64 text suitable for a local secret file.
#[must_use]
pub fn signing_key_base64(signing_key: &SigningKey) -> String {
    STANDARD.encode(signing_key.to_bytes())
}

/// Exports a verifying key as Base64 text to compile into PCRT.
#[must_use]
pub fn verifying_key_base64(signing_key: &SigningKey) -> String {
    STANDARD.encode(signing_key.verifying_key().to_bytes())
}

#[cfg(test)]
mod tests {
    use pcrt_license::{
        FORMAT_VERSION, HardwareIdentifier, HardwareIdentifierType, LicensePayload, PRODUCT,
        hardware_hash, verify_envelope,
    };

    use super::{generate_signing_key, sign_payload};

    #[test]
    fn signed_payload_verifies_with_generated_public_key() {
        let signing_key = generate_signing_key();
        let payload = LicensePayload {
            format_version: FORMAT_VERSION,
            product: PRODUCT.to_owned(),
            license_id: "0f6e5ed8-09a7-47a2-9cb6-6b598ed923da".to_owned(),
            customer_id: "carrier-17".to_owned(),
            bus_id: "BUS-042".to_owned(),
            issued_at: "2026-08-12T00:00:00Z".to_owned(),
            valid_until: "2027-12-31T23:59:59Z".to_owned(),
            hardware_identifiers: vec![
                HardwareIdentifier {
                    kind: HardwareIdentifierType::SmbiosUuid,
                    sha256: hardware_hash(
                        HardwareIdentifierType::SmbiosUuid,
                        "03000200-0400-0500-0006-000700080009",
                    ),
                },
                HardwareIdentifier {
                    kind: HardwareIdentifierType::SystemDiskSerial,
                    sha256: hardware_hash(HardwareIdentifierType::SystemDiskSerial, "DISK-1"),
                },
            ],
        };
        let envelope = sign_payload(&payload, &signing_key).unwrap();
        let (_, verified) = verify_envelope(
            &serde_json::to_vec(&envelope).unwrap(),
            &signing_key.verifying_key(),
        )
        .unwrap();

        assert_eq!(verified, payload);
    }
}
