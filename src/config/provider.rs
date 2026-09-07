//! Carga e publicação de configuração (spec §5.4).
//!
//! O provider produz `RouterTable` validadas e publica em um `tokio::sync::watch`.
//! O caminho da requisição só lê o `Arc` corrente. Hot reload e service discovery
//! entram depois como providers adicionais, sem alterar o caminho da requisição.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::watch;

use crate::config::{Config, ConfigError};
use crate::routing::table::RouterTable;

pub trait ConfigProvider {
    /// Snapshot corrente. Providers dinâmicos publicam novos snapshots no mesmo canal.
    fn subscribe(&self) -> watch::Receiver<Arc<RouterTable>>;
}

/// Lê o arquivo uma vez, no boot. É o único provider da Fase 1.
pub struct FileProvider {
    config: Config,
    tx: watch::Sender<Arc<RouterTable>>,
}

impl FileProvider {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let origin = path.display().to_string();
        let yaml = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: origin.clone(),
            source,
        })?;

        Self::from_yaml(&yaml, &origin)
    }

    pub fn from_yaml(yaml: &str, origin: &str) -> Result<Self, ConfigError> {
        let config = Config::parse(yaml, origin)?;
        let table = Arc::new(RouterTable::build(&config));
        let (tx, _rx) = watch::channel(table);
        Ok(Self { config, tx })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn table(&self) -> Arc<RouterTable> {
        self.tx.borrow().clone()
    }
}

impl ConfigProvider for FileProvider {
    fn subscribe(&self) -> watch::Receiver<Arc<RouterTable>> {
        self.tx.subscribe()
    }
}
