//! `tuskd.toml` + vault path resolution (spec §7, DECISIONS D7/D13).

use serde::Deserialize;
use std::path::{Path, PathBuf};
use tusk_core::error::CoreError;
use tusk_core::gate::{GraduationConfig, Policies, Policy};
use tusk_core::index::RankingConfig;

pub const DEFAULT_HTTP_PORT: u16 = 7477;

#[derive(Debug, Clone)]
pub struct Config {
    pub vault: PathBuf,
    pub http_port: u16,
    pub uds_path: PathBuf,
    pub policies: Policies,
    pub graduation: GraduationConfig,
    pub graduation_interval_hours: u64,
    pub ranking: RankingConfig,
    /// `[sync] enabled` (D21). Off by default — sync groundwork (identities,
    /// change journal) only activates when a user opts in; the full `[sync]`
    /// section arrives with the networked worker (M1).
    pub sync_enabled: bool,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    http_port: Option<u16>,
    uds_path: Option<PathBuf>,
    #[serde(default)]
    policies: toml::Table,
    #[serde(default)]
    graduation: FileGraduation,
    #[serde(default)]
    ranking: FileRanking,
    #[serde(default)]
    sync: FileSync,
}

#[derive(Debug, Default, Deserialize)]
struct FileGraduation {
    min_uses: Option<i64>,
    min_success_rate: Option<f64>,
    interval_hours: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct FileRanking {
    uses_weight: Option<f64>,
    trust_weight: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct FileSync {
    enabled: Option<bool>,
}

/// Written by `tuskd init`.
pub const DEFAULT_TOML: &str = r#"# tuskd.toml — tuskd configuration
http_port = 7477
# uds_path = ".tusk/tuskd.sock"

[policies]
# scope pattern -> "auto" | "review"; first match wins (exact before wildcard).
"org" = "review"
"project:*" = "auto"

[graduation]
min_uses = 5
min_success_rate = 0.8
interval_hours = 24

[ranking]
uses_weight = 0.3
trust_weight = 0.2

[sync]
# Sync groundwork (M0): local change journal + identities only; no
# networking. The full section lands with the sync worker.
enabled = false
"#;

/// D7: --vault > $OPENTUSK_VAULT > ./.tusk > ./vault/.tusk > ".".
pub fn resolve_vault(flag: Option<PathBuf>) -> PathBuf {
    if let Some(v) = flag {
        return v;
    }
    if let Ok(env) = std::env::var("OPENTUSK_VAULT") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    if Path::new(".tusk").is_dir() {
        return PathBuf::from(".");
    }
    if Path::new("vault/.tusk").is_dir() {
        return PathBuf::from("vault");
    }
    PathBuf::from(".")
}

/// D13: primary config is `.tusk/tuskd.toml`; pre-rename vaults with only
/// `.tusk/opentusk.toml` keep working via fallback.
pub fn config_path(vault: &Path) -> PathBuf {
    let primary = vault.join(".tusk").join("tuskd.toml");
    if primary.exists() {
        return primary;
    }
    let legacy = vault.join(".tusk").join("opentusk.toml");
    if legacy.exists() {
        legacy
    } else {
        primary
    }
}

pub fn load(vault: &Path) -> Result<Config, CoreError> {
    let path = config_path(vault);
    let file: FileConfig = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| CoreError::io(path.display().to_string(), e))?;
        toml::from_str(&raw).map_err(|e| CoreError::Config(format!("{}: {e}", path.display())))?
    } else {
        FileConfig::default()
    };

    let mut rules: Vec<(String, Policy)> = Vec::new();
    for (pattern, value) in &file.policies {
        let policy = match value.as_str() {
            Some("auto") => Policy::Auto,
            Some("review") => Policy::Review,
            other => {
                return Err(CoreError::Config(format!(
                    "policy for {pattern:?} must be \"auto\" or \"review\", got {other:?}"
                )))
            }
        };
        rules.push((pattern.clone(), policy));
    }
    // Exact patterns take precedence over wildcards regardless of file order.
    rules.sort_by_key(|(p, _)| p.ends_with(":*"));

    let uds_path = match &file.uds_path {
        Some(p) if p.is_absolute() => p.clone(),
        Some(p) => vault.join(p),
        None => crate::platform::default_uds_path(vault),
    };

    Ok(Config {
        vault: vault.to_path_buf(),
        http_port: file.http_port.unwrap_or(DEFAULT_HTTP_PORT),
        uds_path,
        policies: Policies { rules },
        graduation: GraduationConfig {
            min_uses: file.graduation.min_uses.unwrap_or(5),
            min_success_rate: file.graduation.min_success_rate.unwrap_or(0.8),
        },
        graduation_interval_hours: file.graduation.interval_hours.unwrap_or(24),
        ranking: RankingConfig {
            uses_weight: file.ranking.uses_weight.unwrap_or(0.3),
            trust_weight: file.ranking.trust_weight.unwrap_or(0.2),
        },
        sync_enabled: file.sync.enabled.unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_opentusk_toml_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let tusk = dir.path().join(".tusk");
        std::fs::create_dir_all(&tusk).unwrap();

        // No config at all: primary path is the default target.
        assert!(config_path(dir.path()).ends_with(".tusk/tuskd.toml"));

        // Legacy-only vault: opentusk.toml is honored.
        std::fs::write(tusk.join("opentusk.toml"), "http_port = 1234\n").unwrap();
        let cfg = load(dir.path()).unwrap();
        assert_eq!(cfg.http_port, 1234);

        // Both present: tuskd.toml wins.
        std::fs::write(tusk.join("tuskd.toml"), "http_port = 5678\n").unwrap();
        let cfg = load(dir.path()).unwrap();
        assert_eq!(cfg.http_port, 5678);
    }
}
