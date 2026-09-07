//! Listener administrativo: `/metrics`, `/health` e `/ready` (spec §11.1, §11.3).
//!
//! Separado do listener público porque as políticas de rota não se aplicam aqui —
//! um `/metrics` que exige JWT é inútil para o Prometheus — e porque no Compose a
//! porta simplesmente não é publicada.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use http::StatusCode;

use crate::observability::metrics::Metrics;

/// Readiness é uma pergunta sobre dependências; liveness não é. Manter as duas
/// atrás de tipos diferentes evita que uma vire a outra por descuido.
pub trait ReadinessCheck: Send + Sync + 'static {
    /// `Ok(())` quando o gateway pode receber tráfego, `Err(motivo)` quando não.
    fn readiness(&self) -> Result<(), String>;
}

#[derive(Clone)]
pub struct AdminState {
    pub metrics: Arc<Metrics>,
    pub readiness: Arc<dyn ReadinessCheck>,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .with_state(state)
}

/// Liveness: o processo está servindo. Não checa dependência alguma.
///
/// Se checasse o Auth Service, uma queda dele faria o orquestrador matar e
/// reiniciar gateways saudáveis, transformando degradação parcial em queda total.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// Readiness: configuração carregada e cache de JWKS utilizável.
///
/// Redis não entra: como o rate limit falha aberto, um Redis fora não torna o
/// gateway incapaz de servir.
async fn ready(State(state): State<AdminState>) -> impl IntoResponse {
    match state.readiness.readiness() {
        Ok(()) => (StatusCode::OK, "ready\n".to_string()),
        Err(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {reason}\n"),
        ),
    }
}

async fn metrics(State(state): State<AdminState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}
