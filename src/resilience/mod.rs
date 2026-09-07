//! Resiliência: timeouts, retry e circuit breaker (spec §10).
//!
//! Os três se compõem em uma ordem específica, e a ordem é a decisão:
//!
//! ```text
//! rate_limit                 fora: uma tentativa extra não consome um segundo token
//!   └─ retry
//!       └─ circuit_breaker   dentro do retry: registra cada tentativa, e corta a
//!           └─ upstream_timeout   segunda imediatamente quando o circuito abre
//!               └─ proxy
//! ```

pub mod breaker;
pub mod retry;
pub mod timeout;

/// Como uma tentativa terminou, do ponto de vista das políticas de resiliência.
///
/// É anexado às extensions da resposta pela camada que produziu o resultado, e
/// lido pelas camadas de fora. Sem esse marcador, o retry não teria como separar
/// "não consegui falar com o upstream" de "o upstream respondeu 502", e o breaker
/// não teria como separar erro de servidor de recusa de política.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// O upstream respondeu com status abaixo de 500.
    Success,
    /// Nenhuma resposta chegou: recusa de conexão, falha de DNS, reset antes dos
    /// headers. É a **única** condição comprovadamente pré-resposta, e portanto a
    /// única em que retry é seguro.
    PreResponseFailure,
    /// A tentativa estourou o `upstream_timeout`. Não é retriável: o upstream pode
    /// ter recebido e processado a requisição.
    Timeout,
    /// O upstream respondeu 5xx. Conta para o breaker, não é retriável.
    ServerError,
}

impl AttemptOutcome {
    /// 4xx não conta como falha. Uma onda de 401 ou 404 é comportamento do
    /// cliente; deixá-la abrir o circuito significa que um cliente mal
    /// configurado derruba o serviço para todos.
    pub fn is_failure(self) -> bool {
        !matches!(self, AttemptOutcome::Success)
    }

    /// Anexa o marcador à resposta, para as camadas de fora lerem.
    pub fn mark(self, response: &mut crate::Response) {
        response.extensions_mut().insert(self);
    }

    pub fn of(response: &crate::Response) -> Option<Self> {
        response.extensions().get::<Self>().copied()
    }
}
