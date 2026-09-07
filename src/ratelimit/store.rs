//! Abstração de armazenamento (spec §9.1).

use std::time::Duration;

use async_trait::async_trait;

use crate::config::KeyKind;

#[derive(Debug, Clone)]
pub struct BucketRequest {
    /// Chave completa, no formato `rl:v1:<route_id>:<kind>:<valor>`.
    pub key: String,
    pub kind: KeyKind,
    pub capacity: u32,
    pub refill_per_sec: f64,
    pub cost: u32,
}

/// Resultado agregado dos buckets da rota.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    /// Capacidade do bucket mais restritivo — o que aparece nos headers.
    pub limit: u32,
    pub remaining: u32,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store indisponível: {0}")]
    Unavailable(String),

    #[error("timeout do store")]
    Timeout,
}

/// A operação é `try_acquire`, não `get`/`set`: **a matemática do token bucket
/// roda dentro do store**.
///
/// Se o gateway lesse o contador, calculasse e escrevesse de volta, duas réplicas
/// atendendo o mesmo usuário simultaneamente leriam o mesmo valor e ambas
/// deixariam passar — o limite viraria decorativo exatamente sob a carga em que
/// importa.
#[async_trait]
pub trait RateLimitStore: Send + Sync + std::fmt::Debug {
    /// Avalia **todos** os buckets, tudo-ou-nada: nenhum token é deduzido se
    /// algum bucket recusa. Sem isso, uma rota com dois limites drenaria o
    /// primeiro bucket enquanto rejeita no segundo.
    async fn try_acquire(&self, buckets: &[BucketRequest]) -> Result<Decision, StoreError>;

    /// Nome curto para log e diagnóstico.
    fn kind(&self) -> &'static str;
}

/// Monta a chave do bucket.
///
/// O `route_id` mantém limites por rota independentes; o `v1` permite mudar o
/// formato do bucket sem migração — a versão antiga simplesmente expira.
pub fn bucket_key(route_id: &str, kind: KeyKind, value: &str) -> String {
    format!("rl:v1:{route_id}:{}:{value}", kind.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chave_carrega_versao_rota_e_tipo() {
        assert_eq!(
            bucket_key("users", KeyKind::Sub, "u-42"),
            "rl:v1:users:sub:u-42"
        );
        assert_eq!(
            bucket_key("users", KeyKind::Ip, "10.0.0.1"),
            "rl:v1:users:ip:10.0.0.1"
        );
    }
}
