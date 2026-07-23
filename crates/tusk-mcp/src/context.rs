use std::path::Path;
use std::sync::Arc;
use tusk_core::clock::Clock;
use tusk_core::error::CoreError;
use tusk_core::gate::{Gate, GraduationConfig, Policies};
use tusk_core::index::{Indexer, RankingConfig};
use tusk_core::keyring::Keyring;
use tusk_core::vault::VaultStore;

/// Everything a tool handler needs; one per vault, shared by all sessions.
pub struct TuskContext {
    pub vault: Arc<VaultStore>,
    pub indexer: Arc<Indexer>,
    pub keyring: Arc<Keyring>,
    pub gate: Arc<Gate>,
    pub graduation: GraduationConfig,
}

impl TuskContext {
    pub fn open(root: &Path, clock: Arc<dyn Clock>) -> Result<TuskContext, CoreError> {
        TuskContext::open_with(
            root,
            clock,
            Policies::default(),
            RankingConfig::default(),
            GraduationConfig::default(),
        )
    }

    pub fn open_with(
        root: &Path,
        clock: Arc<dyn Clock>,
        policies: Policies,
        ranking: RankingConfig,
        graduation: GraduationConfig,
    ) -> Result<TuskContext, CoreError> {
        let vault = Arc::new(VaultStore::init(root, Arc::clone(&clock))?);
        let indexer = Arc::new(Indexer::open(&vault.tusk_dir().join("index.db"), ranking)?);
        let keyring_dir = vault.tusk_dir().join("keyring");
        std::fs::create_dir_all(&keyring_dir)
            .map_err(|e| CoreError::io(keyring_dir.display().to_string(), e))?;
        let keyring = Arc::new(Keyring::open_with_clock(
            &keyring_dir.join("agents.json"),
            Arc::clone(&clock),
        )?);
        let gate = Arc::new(Gate::new(
            Arc::clone(&vault),
            Arc::clone(&indexer),
            policies,
        )?);
        Ok(TuskContext {
            vault,
            indexer,
            keyring,
            gate,
            graduation,
        })
    }
}
