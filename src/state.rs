//! Estado compartilhado do processo (spec §4.2, D2, D3).
//!
//! Duas coisas diferentes convivem aqui, e a diferença é a decisão D3:
//!
//! - o **snapshot de configuração**, imutável e substituível como um todo;
//! - o **estado vivo** — hoje o rate limiter, na Fase 2 os breakers — que fica
//!   **fora** do snapshot. Se vivesse dentro, um reload de configuração zeraria o
//!   circuit breaker no meio de um incidente.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::auth::layer::Authenticator;
use crate::clock::Clock;
use crate::config::Config;
use crate::observability::health::ReadinessCheck;
use crate::observability::metrics::Metrics;
use crate::peer::TrustedProxies;
use crate::proxy::ProxyClient;
use crate::ratelimit::limiter::RateLimiter;
use crate::resilience::breaker::BreakerRegistry;
use crate::routing::table::RouterTable;

#[derive(Debug, Clone)]
pub struct ServerSettings {
    pub max_body_bytes: u64,
    pub request_timeout: Duration,
    pub trusted_proxies: TrustedProxies,
}

pub struct AppState {
    /// Snapshot corrente. Providers dinâmicos publicam neste canal; o caminho da
    /// requisição só lê.
    pub table: watch::Receiver<Arc<RouterTable>>,
    pub server: ServerSettings,
    /// Ausente quando não há seção `auth` — configuração sem rota autenticada.
    pub authenticator: Option<Arc<Authenticator>>,
    pub limiter: Arc<RateLimiter>,
    /// Estado dos circuitos, chaveado por upstream. Vive aqui, e não no snapshot,
    /// para que um reload de configuração não zere o breaker no meio de um incidente.
    pub breakers: Arc<BreakerRegistry>,
    pub proxy: Arc<ProxyClient>,
    pub metrics: Arc<Metrics>,
    pub clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn table(&self) -> Arc<RouterTable> {
        self.table.borrow().clone()
    }
}

impl ServerSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_body_bytes: config.server.max_body_bytes,
            request_timeout: config.server.request_timeout,
            trusted_proxies: TrustedProxies::new(config.server.trusted_proxies.clone()),
        }
    }
}

impl ReadinessCheck for AppState {
    /// Configuração carregada e cache de JWKS utilizável.
    ///
    /// Redis não entra: como o rate limit falha aberto, um Redis fora não torna o
    /// gateway incapaz de servir, e um `/ready` que dissesse o contrário tiraria
    /// a réplica do balanceador sem motivo.
    fn readiness(&self) -> Result<(), String> {
        if self.table().routes().is_empty() {
            return Err("nenhuma rota carregada".into());
        }

        if let Some(authenticator) = &self.authenticator
            && !authenticator.jwks().is_usable()
        {
            return Err("cache de JWKS indisponível ou fora da janela stale".into());
        }

        Ok(())
    }
}
