use crate::error::CoreError;
use std::fmt;
use std::path::PathBuf;

/// Memory scope: `agent:<id>` | `project:<id>` | `user` | `org`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Agent(String),
    Project(String),
    User,
    Org,
}

/// Ids in scopes: 1–64 chars of `[A-Za-z0-9._-]`, starting alphanumeric.
fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Scope {
    pub fn parse(s: &str) -> Result<Scope, CoreError> {
        let scope = match s {
            "user" => Scope::User,
            "org" => Scope::Org,
            _ => {
                if let Some(id) = s.strip_prefix("agent:") {
                    if !valid_id(id) {
                        return Err(CoreError::InvalidScope(s.to_string()));
                    }
                    Scope::Agent(id.to_string())
                } else if let Some(id) = s.strip_prefix("project:") {
                    if !valid_id(id) {
                        return Err(CoreError::InvalidScope(s.to_string()));
                    }
                    Scope::Project(id.to_string())
                } else {
                    return Err(CoreError::InvalidScope(s.to_string()));
                }
            }
        };
        Ok(scope)
    }

    /// Path under `vault/memory/`.
    pub fn rel_path(&self) -> PathBuf {
        match self {
            Scope::Agent(id) => PathBuf::from("agent").join(id),
            Scope::Project(id) => PathBuf::from("project").join(id),
            Scope::User => PathBuf::from("user"),
            Scope::Org => PathBuf::from("org"),
        }
    }

    /// Dashed form used under `vault/skills/` (e.g. `project-opentusk`).
    pub fn dashed(&self) -> String {
        self.to_string().replace(':', "-")
    }

    /// Own scope of an agent id.
    pub fn own(agent_id: &str) -> Scope {
        Scope::Agent(agent_id.to_string())
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Agent(id) => write!(f, "agent:{id}"),
            Scope::Project(id) => write!(f, "project:{id}"),
            Scope::User => write!(f, "user"),
            Scope::Org => write!(f, "org"),
        }
    }
}

impl serde::Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Scope::parse(&raw).map_err(serde::de::Error::custom)
    }
}
