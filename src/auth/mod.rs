//! Autenticação por JWT (spec §8).
//!
//! O gateway **valida** tokens; não os emite, e não conhece o banco de usuários.
//! A fronteira é: Auth Service autentica e emite, gateway valida e aplica política
//! de rota, backend aplica autorização de domínio.

pub mod claims;
pub mod jwks;
pub mod layer;

use std::sync::Arc;

/// Identidade extraída de um token válido, disponível aos layers seguintes.
#[derive(Debug, Clone)]
pub struct Identity {
    pub sub: Arc<str>,
    pub scopes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthFailure {
    #[error("token ausente")]
    Missing,

    #[error("token malformado: {0}")]
    Malformed(&'static str),

    #[error("kid desconhecido")]
    UnknownKid,

    #[error("assinatura ou claims inválidas: {0}")]
    Invalid(String),

    #[error("token sem claim sub")]
    MissingSubject,

    /// Distinta das demais: é indisponibilidade do gateway, não erro do cliente,
    /// e por isso vira 503 em vez de 401.
    #[error("JWKS indisponível além da janela stale")]
    JwksUnavailable,
}

impl AuthFailure {
    /// Motivo curto e de cardinalidade limitada, próprio para label de métrica e log.
    pub fn reason(&self) -> &'static str {
        match self {
            AuthFailure::Missing => "missing",
            AuthFailure::Malformed(_) => "malformed",
            AuthFailure::UnknownKid => "unknown_kid",
            AuthFailure::Invalid(_) => "invalid",
            AuthFailure::MissingSubject => "missing_subject",
            AuthFailure::JwksUnavailable => "jwks_unavailable",
        }
    }
}
