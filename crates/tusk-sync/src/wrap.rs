//! Device wraps: encrypt the RMK to a device's key so a new device can join
//! by approval, no phrase typed (HOT_CACHE_SYNC_PROPOSAL §4, onboarding path
//! 2). The device identity is the slice-1 ed25519 keypair
//! (`.tusk/sync/device.pem`); we convert it to X25519 via the standard
//! birational map (ed25519-dalek `to_scalar_bytes` / `to_montgomery`, the
//! same construction as libsodium's `crypto_sign_ed25519_*_to_curve25519`).
//!
//! Wrap = ephemeral-static ECDH: the approver mints an ephemeral X25519 key,
//! derives `HKDF(shared, info ‖ eph_pub ‖ device_pub)` and seals the RMK
//! with XChaCha20-Poly1305, `AAD = repo_id`. Only the holder of the device
//! *private* key can recompute the shared secret and unwrap.

use crate::crypto::{aead_open, aead_seal, derive_key, RepoMasterKey};
use crate::error::SyncError;
use ed25519_dalek::pkcs8::{DecodePrivateKey, DecodePublicKey};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

const INFO_DEVICE_WRAP: &[u8] = b"opentusk-sync v1: device-wrap";

/// A wrapped RMK, publishable through the (untrusted) server: without the
/// target device's private key it is just AEAD ciphertext.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceWrap {
    /// Format version (1 = X25519 ephemeral-static + XChaCha20-Poly1305).
    pub v: u8,
    /// Approver's ephemeral X25519 public key, hex (32 bytes).
    pub ephemeral_pub: String,
    /// `nonce(24) ‖ ciphertext+tag` over the 32-byte RMK, hex.
    pub sealed: String,
}

fn crypto_err(what: &str, e: impl std::fmt::Display) -> SyncError {
    SyncError::Crypto(format!("{what}: {e}"))
}

/// Target device's X25519 public key from its ed25519 public key (SPKI PEM,
/// as stored in the repo's device registry).
fn device_x25519_public(device_public_pem: &str) -> Result<[u8; 32], SyncError> {
    let vk = VerifyingKey::from_public_key_pem(device_public_pem)
        .map_err(|e| crypto_err("device public key", e))?;
    Ok(vk.to_montgomery().to_bytes())
}

/// Wrap `rmk` to the device identified by `device_public_pem`. Run by an
/// already-authorized device (the approver); the result travels to the new
/// device via the server, which cannot open it.
pub fn wrap_rmk_for_device(
    rmk: &RepoMasterKey,
    repo_id: &str,
    device_public_pem: &str,
) -> Result<DeviceWrap, SyncError> {
    let device_pub_bytes = device_x25519_public(device_public_pem)?;
    let device_pub = PublicKey::from(device_pub_bytes);

    let ephemeral = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
    let ephemeral_pub = PublicKey::from(&ephemeral);
    let shared = ephemeral.diffie_hellman(&device_pub);
    if !shared.was_contributory() {
        return Err(SyncError::Crypto(
            "non-contributory dh (low-order device key)".into(),
        ));
    }

    let key = derive_key(
        shared.as_bytes(),
        &[
            INFO_DEVICE_WRAP,
            ephemeral_pub.as_bytes(),
            &device_pub_bytes,
        ],
    )?;
    let sealed = aead_seal(&key, repo_id.as_bytes(), rmk.bytes())?;
    Ok(DeviceWrap {
        v: 1,
        ephemeral_pub: hex::encode(ephemeral_pub.as_bytes()),
        sealed: hex::encode(sealed),
    })
}

/// Unwrap on the new device using its ed25519 private key (PKCS#8 PEM,
/// `.tusk/sync/device.pem`).
pub fn unwrap_rmk_for_device(
    wrap: &DeviceWrap,
    repo_id: &str,
    device_private_pem: &str,
) -> Result<RepoMasterKey, SyncError> {
    if wrap.v != 1 {
        return Err(SyncError::Crypto(format!(
            "unknown wrap version {}",
            wrap.v
        )));
    }
    let signing = SigningKey::from_pkcs8_pem(device_private_pem)
        .map_err(|e| crypto_err("device private key", e))?;
    let device_secret = StaticSecret::from(signing.to_scalar_bytes());
    let device_pub_bytes = signing.verifying_key().to_montgomery().to_bytes();

    let eph_raw =
        hex::decode(&wrap.ephemeral_pub).map_err(|e| crypto_err("ephemeral_pub hex", e))?;
    let eph_bytes: [u8; 32] = eph_raw
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::Crypto("ephemeral_pub length".into()))?;
    let shared = device_secret.diffie_hellman(&PublicKey::from(eph_bytes));
    if !shared.was_contributory() {
        return Err(SyncError::Crypto(
            "non-contributory dh (low-order ephemeral)".into(),
        ));
    }

    let key = derive_key(
        shared.as_bytes(),
        &[INFO_DEVICE_WRAP, &eph_bytes, &device_pub_bytes],
    )?;
    let sealed = hex::decode(&wrap.sealed).map_err(|e| crypto_err("sealed hex", e))?;
    let plaintext = aead_open(&key, repo_id.as_bytes(), &sealed)?;
    let bytes: [u8; 32] = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::Crypto("unwrapped key length".into()))?;
    Ok(RepoMasterKey::from_bytes(bytes))
}
