//! Agent identities, grants, and the ACL choke-point (spec §4).

use crate::clock::{Clock, SystemClock};
use crate::error::CoreError;
use crate::scope::Scope;
use chrono::SecondsFormat;
use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, EncodePrivateKey, EncodePublicKey};
use ed25519_dalek::SigningKey;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Read,
    Write,
    Promote,
}

impl Verb {
    pub fn parse(s: &str) -> Result<Verb, CoreError> {
        match s {
            "read" => Ok(Verb::Read),
            "write" => Ok(Verb::Write),
            "promote" => Ok(Verb::Promote),
            other => Err(CoreError::Other(format!("unknown verb: {other:?}"))),
        }
    }
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verb::Read => "read",
            Verb::Write => "write",
            Verb::Promote => "promote",
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Grants {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub promote: Vec<String>,
}

impl Grants {
    pub fn for_verb(&self, verb: Verb) -> &[String] {
        match verb {
            Verb::Read => &self.read,
            Verb::Write => &self.write,
            Verb::Promote => &self.promote,
        }
    }

    fn for_verb_mut(&mut self, verb: Verb) -> &mut Vec<String> {
        match verb {
            Verb::Read => &mut self.read,
            Verb::Write => &mut self.write,
            Verb::Promote => &mut self.promote,
        }
    }
}

/// Persisted entry in `agents.json` (spec §4): no secrets beyond the
/// sha256 of the bearer token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub id: String,
    pub public_key_pem: String,
    pub token_sha256: String,
    pub grants: Grants,
    pub created_at: String,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct KeyringFile {
    agents: Vec<AgentEntry>,
}

/// Returned by `create` exactly once; never persisted.
#[derive(Debug, Clone)]
pub struct CreatedAgent {
    pub id: String,
    pub token: String,
    pub private_key_pem: String,
    pub public_key_pem: String,
}

/// Constant-time equality on two sha256 digests (`subtle`).
pub fn ct_eq_digest(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.ct_eq(b).into()
}

/// A grant pattern is a valid scope or a `kind:*` wildcard.
fn validate_pattern(pattern: &str) -> Result<(), CoreError> {
    if pattern == "agent:*" || pattern == "project:*" {
        return Ok(());
    }
    Scope::parse(pattern).map(|_| ())
}

/// Does `pattern` entitle `scope`? Exact match, or `kind:*` prefix wildcard.
pub fn pattern_matches(pattern: &str, scope: &Scope) -> bool {
    let scope_str = scope.to_string();
    if pattern == scope_str {
        return true;
    }
    match pattern.strip_suffix(":*") {
        Some(kind) => scope_str
            .strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix(':'))
            .is_some_and(|id| !id.is_empty()),
        None => false,
    }
}

pub struct Keyring {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    state: Mutex<KeyringFile>,
}

impl Keyring {
    pub fn open(path: &Path) -> Result<Keyring, CoreError> {
        Keyring::open_with_clock(path, Arc::new(SystemClock))
    }

    pub fn open_with_clock(path: &Path, clock: Arc<dyn Clock>) -> Result<Keyring, CoreError> {
        let state = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| CoreError::io(path.display().to_string(), e))?;
            serde_json::from_str(&raw)?
        } else {
            KeyringFile::default()
        };
        Ok(Keyring {
            path: path.to_path_buf(),
            clock,
            state: Mutex::new(state),
        })
    }

    fn with_state<T>(
        &self,
        f: impl FnOnce(&mut KeyringFile) -> Result<T, CoreError>,
    ) -> Result<T, CoreError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CoreError::Other("keyring poisoned".into()))?;
        f(&mut state)
    }

    fn save(&self, state: &KeyringFile) -> Result<(), CoreError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| CoreError::io(dir.display().to_string(), e))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(state)?)
            .map_err(|e| CoreError::io(tmp.display().to_string(), e))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| CoreError::io(self.path.display().to_string(), e))?;
        Ok(())
    }

    pub fn create(
        &self,
        id: &str,
        read: &[&str],
        write: &[&str],
        promote: &[&str],
    ) -> Result<CreatedAgent, CoreError> {
        // Agent ids share the scope-id grammar (they appear in `agent:<id>`).
        Scope::parse(&format!("agent:{id}"))
            .map_err(|_| CoreError::Other(format!("invalid agent id: {id:?}")))?;
        let grants = Grants {
            read: read.iter().map(|s| s.to_string()).collect(),
            write: write.iter().map(|s| s.to_string()).collect(),
            promote: promote.iter().map(|s| s.to_string()).collect(),
        };
        for pattern in grants
            .read
            .iter()
            .chain(&grants.write)
            .chain(&grants.promote)
        {
            validate_pattern(pattern)?;
        }

        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        let token = format!("tusk_{}", hex::encode(secret));
        let token_sha256 = hex::encode(Sha256::digest(token.as_bytes()));

        let signing = SigningKey::generate(&mut rand::rngs::OsRng);
        let private_key_pem = signing
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| CoreError::Other(format!("pkcs8 encode: {e}")))?
            .to_string();
        let public_key_pem = signing
            .verifying_key()
            .to_public_key_pem(LineEnding::LF)
            .map_err(|e| CoreError::Other(format!("spki encode: {e}")))?;

        let created_at = self
            .clock
            .now()
            .to_rfc3339_opts(SecondsFormat::Millis, true);

        self.with_state(|state| {
            if state.agents.iter().any(|a| a.id == id) {
                return Err(CoreError::Other(format!("agent already exists: {id}")));
            }
            state.agents.push(AgentEntry {
                id: id.to_string(),
                public_key_pem: public_key_pem.clone(),
                token_sha256,
                grants,
                created_at,
                revoked: false,
            });
            self.save(state)
        })?;

        Ok(CreatedAgent {
            id: id.to_string(),
            token,
            private_key_pem,
            public_key_pem,
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<AgentEntry>, CoreError> {
        self.with_state(|state| Ok(state.agents.iter().find(|a| a.id == id).cloned()))
    }

    pub fn list(&self) -> Result<Vec<AgentEntry>, CoreError> {
        self.with_state(|state| Ok(state.agents.clone()))
    }

    pub fn grant(&self, id: &str, verb: Verb, scope_pattern: &str) -> Result<(), CoreError> {
        validate_pattern(scope_pattern)?;
        self.with_state(|state| {
            let agent = state
                .agents
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or_else(|| CoreError::NotFound(format!("agent {id}")))?;
            let list = agent.grants.for_verb_mut(verb);
            if !list.contains(&scope_pattern.to_string()) {
                list.push(scope_pattern.to_string());
            }
            self.save(state)
        })
    }

    /// Mint a replacement bearer token for an existing, non-revoked agent.
    /// The old token stops authenticating immediately; the new one is
    /// returned once and only its sha256 is stored.
    pub fn rotate_token(&self, id: &str) -> Result<String, CoreError> {
        let mut secret = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        let token = format!("tusk_{}", hex::encode(secret));
        let token_sha256 = hex::encode(Sha256::digest(token.as_bytes()));
        self.with_state(|state| {
            let agent = state
                .agents
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or_else(|| CoreError::NotFound(format!("agent {id}")))?;
            if agent.revoked {
                return Err(CoreError::Denied(format!("agent {id} is revoked")));
            }
            agent.token_sha256 = token_sha256;
            self.save(state)
        })?;
        Ok(token)
    }

    pub fn revoke(&self, id: &str) -> Result<(), CoreError> {
        self.with_state(|state| {
            let agent = state
                .agents
                .iter_mut()
                .find(|a| a.id == id)
                .ok_or_else(|| CoreError::NotFound(format!("agent {id}")))?;
            agent.revoked = true;
            self.save(state)
        })
    }

    /// Authenticate a bearer token: sha256 then constant-time compare against
    /// every non-revoked agent (spec §4).
    pub fn auth_by_token(&self, token: &str) -> Result<AgentEntry, CoreError> {
        let digest = Sha256::digest(token.as_bytes());
        self.with_state(|state| {
            let mut found: Option<AgentEntry> = None;
            for agent in &state.agents {
                let stored = hex::decode(&agent.token_sha256).unwrap_or_default();
                if ct_eq_digest(&stored, &digest) && !agent.revoked {
                    found = Some(agent.clone());
                }
            }
            found.ok_or_else(|| CoreError::Denied("unknown or revoked token".into()))
        })
    }

    /// THE choke-point (spec §4): every tool call passes through here.
    /// Unknown or revoked agents get `false` for everything; agents always
    /// implicitly hold read+write on `agent:<own-id>`.
    pub fn can(&self, agent_id: &str, verb: Verb, scope: &Scope) -> Result<bool, CoreError> {
        self.with_state(|state| {
            let Some(agent) = state.agents.iter().find(|a| a.id == agent_id) else {
                return Ok(false);
            };
            if agent.revoked {
                return Ok(false);
            }
            if matches!(verb, Verb::Read | Verb::Write) && *scope == Scope::own(&agent.id) {
                return Ok(true);
            }
            Ok(agent
                .grants
                .for_verb(verb)
                .iter()
                .any(|p| pattern_matches(p, scope)))
        })
    }
}
