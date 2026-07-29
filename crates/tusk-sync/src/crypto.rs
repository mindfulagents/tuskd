//! Client-side encryption (HOT_CACHE_SYNC_PROPOSAL §4; DECISIONS D22).
//!
//! Envelope model: a per-repo master key (RMK) wraps a small key table of
//! per-object DEKs; object content is XChaCha20-Poly1305 under its DEK with
//! `AAD = repo_id ‖ 0x00 ‖ rel_path`. Rotation re-encrypts only the key
//! table — blob ciphertext and blob names are untouched because names are
//! recorded in the table at first write.
//!
//! All uses of the RMK go through HKDF-SHA256 subkeys with distinct `info`
//! strings (key-table wrap vs blob naming), so the raw RMK is never fed to
//! two different algorithms.

use crate::error::SyncError;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Compact Repo Secret Key prefix: `otsk_<64 hex RMK><8 hex checksum>`.
pub const OTSK_PREFIX: &str = "otsk_";

const XNONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

const INFO_KEY_TABLE: &[u8] = b"opentusk-sync v1: key-table wrap";
const INFO_BLOB_NAME: &[u8] = b"opentusk-sync v1: blob-name hmac";

// ---------------------------------------------------------------------------
// AEAD helpers (shared with wrap.rs)
// ---------------------------------------------------------------------------

/// XChaCha20-Poly1305 seal: returns `nonce(24) ‖ ciphertext+tag`. The random
/// extended nonce is what makes "generate fresh per message" safe without
/// counters or state.
pub(crate) fn aead_seal(
    key: &[u8; 32],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, SyncError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; XNONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| SyncError::Crypto("aead encrypt failed".into()))?;
    let mut out = Vec::with_capacity(XNONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub(crate) fn aead_open(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Result<Vec<u8>, SyncError> {
    if blob.len() < XNONCE_LEN + TAG_LEN {
        return Err(SyncError::Crypto("ciphertext too short".into()));
    }
    let (nonce, ct) = blob.split_at(XNONCE_LEN);
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| {
            SyncError::Crypto("aead decrypt failed (tampered, wrong key, or wrong aad)".into())
        })
}

/// HKDF-SHA256 → 32-byte subkey from concatenated info parts.
pub(crate) fn derive_key(ikm: &[u8], info: &[&[u8]]) -> Result<[u8; 32], SyncError> {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut okm = [0u8; 32];
    hk.expand_multi_info(info, &mut okm)
        .map_err(|_| SyncError::Crypto("hkdf expand failed".into()))?;
    Ok(okm)
}

/// `repo_id ‖ 0x00 ‖ rel_path` — the object AAD from the proposal (§4), with
/// an explicit separator so `("ab","c")` and `("a","bc")` can't collide.
fn object_aad(repo_id: &str, rel_path: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(repo_id.len() + 1 + rel_path.len());
    aad.extend_from_slice(repo_id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(rel_path.as_bytes());
    aad
}

// ---------------------------------------------------------------------------
// Repo Master Key + Repo Secret Key encodings
// ---------------------------------------------------------------------------

/// RMK: 32 random bytes, minted on the first connecting device; never leaves
/// a device unencrypted. Zeroized on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct RepoMasterKey {
    bytes: [u8; 32],
}

impl PartialEq for RepoMasterKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes.ct_eq(&other.bytes).into()
    }
}
impl Eq for RepoMasterKey {}

impl std::fmt::Debug for RepoMasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepoMasterKey(..)") // never print key material
    }
}

impl RepoMasterKey {
    pub fn generate() -> RepoMasterKey {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        RepoMasterKey { bytes }
    }

    pub fn from_bytes(bytes: [u8; 32]) -> RepoMasterKey {
        RepoMasterKey { bytes }
    }

    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Repo Secret Key, 24-word form (BIP39 English; 32 bytes entropy ⇒ 24
    /// words with an 8-bit checksum, so single-word typos are detected).
    pub fn to_mnemonic(&self) -> Result<String, SyncError> {
        let mnemonic = bip39::Mnemonic::from_entropy(&self.bytes)
            .map_err(|e| SyncError::SecretKey(format!("encode mnemonic: {e}")))?;
        Ok(mnemonic.to_string())
    }

    /// Parse the 24-word phrase back to the RMK. Case- and
    /// whitespace-insensitive; wordlist and checksum catch typos.
    pub fn from_mnemonic(phrase: &str) -> Result<RepoMasterKey, SyncError> {
        let normalized = phrase
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        let mnemonic = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &normalized)
            .map_err(|e| SyncError::SecretKey(format!("bad phrase: {e}")))?;
        let entropy = mnemonic.to_entropy();
        let bytes: [u8; 32] = entropy.as_slice().try_into().map_err(|_| {
            SyncError::SecretKey(format!(
                "phrase has {} words, need 24",
                mnemonic.word_count()
            ))
        })?;
        Ok(RepoMasterKey { bytes })
    }

    /// Compact Repo Secret Key form: `otsk_` + 64 hex chars of RMK + 8 hex
    /// chars of sha256 checksum (typo detection for the paste-able form).
    pub fn to_otsk(&self) -> String {
        let sum = Sha256::digest(self.bytes);
        format!(
            "{OTSK_PREFIX}{}{}",
            hex::encode(self.bytes),
            hex::encode(&sum[..4])
        )
    }

    pub fn from_otsk(s: &str) -> Result<RepoMasterKey, SyncError> {
        let body = s
            .trim()
            .strip_prefix(OTSK_PREFIX)
            .ok_or_else(|| SyncError::SecretKey(format!("missing {OTSK_PREFIX} prefix")))?;
        let raw = hex::decode(body).map_err(|e| SyncError::SecretKey(format!("bad hex: {e}")))?;
        if raw.len() != 36 {
            return Err(SyncError::SecretKey(format!(
                "wrong length: {} bytes decoded, need 36",
                raw.len()
            )));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&raw[..32]);
        let sum = Sha256::digest(bytes);
        if raw[32..] != sum[..4] {
            return Err(SyncError::SecretKey("checksum mismatch (typo?)".into()));
        }
        Ok(RepoMasterKey { bytes })
    }

    /// Storage name for a vault-relative path: HMAC-SHA256 over `rel_path`
    /// under an HKDF subkey of the RMK, hex-encoded. Deterministic (same path
    /// ⇒ same name, enabling overwrite-in-place) and structure-blind: the
    /// server learns nothing about scopes/agents/filenames.
    pub fn blob_name(&self, rel_path: &str) -> Result<String, SyncError> {
        let key = derive_key(&self.bytes, &[INFO_BLOB_NAME])?;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key)
            .map_err(|_| SyncError::Crypto("hmac key".into()))?;
        mac.update(rel_path.as_bytes());
        Ok(hex::encode(mac.finalize().into_bytes()))
    }
}

// ---------------------------------------------------------------------------
// Per-object DEKs
// ---------------------------------------------------------------------------

/// Per-object data-encryption key (32 random bytes). Zeroized on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Dek {
    bytes: [u8; 32],
}

impl Dek {
    pub fn generate() -> Dek {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Dek { bytes }
    }

    fn to_hex(&self) -> String {
        hex::encode(self.bytes)
    }

    fn from_hex(s: &str) -> Result<Dek, SyncError> {
        let raw = hex::decode(s).map_err(|_| SyncError::Crypto("bad dek hex".into()))?;
        let bytes: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| SyncError::Crypto("bad dek length".into()))?;
        Ok(Dek { bytes })
    }

    /// Encrypt object content: XChaCha20-Poly1305, random 24-byte nonce
    /// prepended, `AAD = repo_id ‖ 0x00 ‖ rel_path` — a blob decrypts only
    /// for the repo and path it was written for.
    pub fn encrypt(
        &self,
        repo_id: &str,
        rel_path: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        aead_seal(&self.bytes, &object_aad(repo_id, rel_path), plaintext)
    }

    pub fn decrypt(
        &self,
        repo_id: &str,
        rel_path: &str,
        blob: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        aead_open(&self.bytes, &object_aad(repo_id, rel_path), blob)
    }
}

// ---------------------------------------------------------------------------
// Encrypted key table (rel_path → blob name + DEK), wrapped by the RMK
// ---------------------------------------------------------------------------

/// One key-table row. The blob name is recorded at first write so RMK
/// rotation (which changes the naming subkey) never renames or re-encrypts
/// existing blobs — only the sealed table changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    /// Storage name (`RepoMasterKey::blob_name` at mint time), hex.
    pub blob: String,
    /// DEK, hex. Private: reach it through [`ObjectEntry::dek`].
    dek: String,
}

impl ObjectEntry {
    pub fn dek(&self) -> Result<Dek, SyncError> {
        Dek::from_hex(&self.dek)
    }
}

/// The plaintext key table: `rel_path → ObjectEntry`. Lives only in client
/// memory; at rest and in transit it exists solely as `seal` output (the
/// `manifest` blob), wrapped under an HKDF subkey of the RMK.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyTable {
    objects: BTreeMap<String, ObjectEntry>,
}

impl KeyTable {
    pub fn new() -> KeyTable {
        KeyTable::default()
    }

    /// Entry for `rel_path`, minting a fresh DEK + blob name on first sight.
    /// Existing entries are returned unchanged (stable name and DEK across
    /// content updates — overwrites reuse the same blob slot).
    pub fn entry(&mut self, rmk: &RepoMasterKey, rel_path: &str) -> Result<ObjectEntry, SyncError> {
        if let Some(existing) = self.objects.get(rel_path) {
            return Ok(existing.clone());
        }
        let entry = ObjectEntry {
            blob: rmk.blob_name(rel_path)?,
            dek: Dek::generate().to_hex(),
        };
        self.objects.insert(rel_path.to_string(), entry.clone());
        Ok(entry)
    }

    pub fn get(&self, rel_path: &str) -> Option<&ObjectEntry> {
        self.objects.get(rel_path)
    }

    pub fn remove(&mut self, rel_path: &str) -> Option<ObjectEntry> {
        self.objects.remove(rel_path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &ObjectEntry)> {
        self.objects.iter()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Serialize and encrypt under `rmk` (`AAD = repo_id ‖ 0x00 ‖
    /// "key-table"`). Rotation = call this with the new RMK; nothing else in
    /// storage changes.
    pub fn seal(&self, rmk: &RepoMasterKey, repo_id: &str) -> Result<Vec<u8>, SyncError> {
        let key = derive_key(rmk.bytes(), &[INFO_KEY_TABLE])?;
        let plaintext = serde_json::to_vec(self)?;
        aead_seal(&key, &object_aad(repo_id, "key-table"), &plaintext)
    }

    /// Decrypt and deserialize a sealed table.
    pub fn open(sealed: &[u8], rmk: &RepoMasterKey, repo_id: &str) -> Result<KeyTable, SyncError> {
        let key = derive_key(rmk.bytes(), &[INFO_KEY_TABLE])?;
        let plaintext = aead_open(&key, &object_aad(repo_id, "key-table"), sealed)?;
        Ok(serde_json::from_slice(&plaintext)?)
    }
}
