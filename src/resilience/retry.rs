//! Retry (spec §10.2, D7).
//!
//! Seguro por padrão, sem depender de configuração correta: retry de
//! `POST /payments` após um timeout de leitura pode cobrar duas vezes, e nenhuma
//! opção de configuração deveria tornar isso possível.

use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use bytes::Bytes;
use http::Method;
use http_body::Body as _;
use http_body_util::BodyExt;
use tower::{Layer, Service};

use crate::config::RetryPolicy;
use crate::error::GatewayError;
use crate::resilience::AttemptOutcome;
use crate::routing::layer::route_of;
use crate::{BoxFuture, Request, Response};

/// Métodos em que repetir a requisição não muda o resultado observável.
///
/// POST e PATCH ficam de fora, e não há opção para incluí-los: a diferença entre
/// "o pagamento não foi processado" e "o pagamento foi processado e a resposta se
/// perdeu" não é visível daqui.
pub fn is_idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE
    )
}

/// Número de tentativas que a política autoriza, contando a primeira.
///
/// `max_attempts: 0` e `max_attempts: 1` significam a mesma coisa — uma tentativa,
/// nenhuma repetição —, porque a spec usa `0` para desabilitar retry.
pub fn attempts_allowed(policy: &RetryPolicy) -> u32 {
    policy.max_attempts.max(1)
}

/// Espera antes da próxima tentativa, com jitter completo.
///
/// Backoff fixo faz todas as requisições que falharam juntas retentarem juntas, e
/// o thundering herd chega no upstream que ainda está se recuperando. O sorteio
/// em `[0, teto]` é o que espalha a segunda onda.
pub fn backoff_for(policy: &RetryPolicy, attempt: u32) -> Duration {
    let ceiling = policy.backoff.saturating_mul(1 << (attempt - 1).min(6));

    if !policy.jitter {
        return ceiling;
    }

    let millis = ceiling.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }

    Duration::from_millis(pseudo_random() % (millis + 1))
}

/// Gerador xorshift, semeado com o relógio. Jitter não precisa de qualidade
/// criptográfica, e uma dependência a mais para isso não se paga.
fn pseudo_random() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);

    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545F4914F6CDD1D)
            | 1;
    }

    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);

    x
}

#[derive(Debug, Clone, Copy)]
pub struct RetryLayer {
    max_buffer_bytes: u64,
}

impl RetryLayer {
    pub fn new(max_buffer_bytes: u64) -> Self {
        Self { max_buffer_bytes }
    }
}

impl<S> Layer<S> for RetryLayer {
    type Service = Retry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Retry {
            inner,
            max_buffer_bytes: self.max_buffer_bytes,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Retry<S> {
    inner: S,
    max_buffer_bytes: u64,
}

impl<S> Service<Request> for Retry<S>
where
    S: Service<Request, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = BoxFuture<Result<Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        let policy = match route_of(req.extensions()) {
            Some(route) => route.resilience.retry.clone(),
            None => return Box::pin(async move { inner.call(req).await }),
        };

        let attempts = attempts_allowed(&policy);
        if attempts <= 1 || !is_idempotent(req.method()) {
            return Box::pin(async move { inner.call(req).await });
        }

        let max_buffer_bytes = self.max_buffer_bytes;

        Box::pin(async move {
            // Repetir exige poder reenviar o corpo, e um corpo já consumido não
            // volta. Corpos grandes seguem normalmente, mas em tentativa única.
            let (parts, body) = match buffer_body(req, max_buffer_bytes).await {
                Buffered::Ready(parts, body) => (*parts, body),
                Buffered::TooLarge(req) => return inner.call(*req).await,
                Buffered::Unreadable => {
                    return Ok(GatewayError::bad_gateway("Could not read the request body")
                        .into_response(""));
                }
            };

            let mut last = None;

            for attempt in 1..=attempts {
                if attempt > 1 {
                    let wait = backoff_for(&policy, attempt - 1);
                    tracing::debug!(attempt, ?wait, "repetindo a requisição");
                    tokio::time::sleep(wait).await;
                }

                let mut req = http::Request::from_parts(parts.clone(), Body::from(body.clone()));
                *req.uri_mut() = parts.uri.clone();

                let response = inner.call(req).await?;

                // Só falha comprovadamente pré-resposta é repetida. Timeout e 5xx
                // significam que o upstream pode ter processado a requisição.
                if AttemptOutcome::of(&response) != Some(AttemptOutcome::PreResponseFailure) {
                    return Ok(response);
                }

                last = Some(response);
            }

            tracing::warn!(attempts, "todas as tentativas falharam antes da resposta");
            Ok(last.expect("o laço roda pelo menos uma vez"))
        })
    }
}

enum Buffered {
    Ready(Box<http::request::Parts>, Bytes),
    /// O corpo não cabe no limite, ou não declara tamanho. A requisição segue
    /// intacta, em tentativa única.
    TooLarge(Box<Request>),
    Unreadable,
}

async fn buffer_body(req: Request, max_bytes: u64) -> Buffered {
    // Só bufferiza o que declara tamanho conhecido dentro do limite. Sem essa
    // checagem, descobrir o excesso durante a leitura já teria consumido o corpo,
    // e a requisição não teria como seguir.
    match req.body().size_hint().upper() {
        Some(size) if size <= max_bytes => {}
        _ => return Buffered::TooLarge(Box::new(req)),
    }

    let (parts, body) = req.into_parts();
    match body.collect().await {
        Ok(collected) => Buffered::Ready(Box::new(parts), collected.to_bytes()),
        Err(_) => Buffered::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_attempts: u32, jitter: bool) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            backoff: Duration::from_millis(50),
            jitter,
        }
    }

    #[test]
    fn post_e_patch_nunca_sao_retriaveis() {
        assert!(!is_idempotent(&Method::POST));
        assert!(!is_idempotent(&Method::PATCH));
    }

    #[test]
    fn metodos_idempotentes_sao_retriaveis() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::OPTIONS,
            Method::PUT,
            Method::DELETE,
        ] {
            assert!(is_idempotent(&method), "{method} deveria ser retriável");
        }
    }

    #[test]
    fn max_attempts_zero_e_um_significam_tentativa_unica() {
        assert_eq!(attempts_allowed(&policy(0, true)), 1);
        assert_eq!(attempts_allowed(&policy(1, true)), 1);
        assert_eq!(attempts_allowed(&policy(2, true)), 2);
    }

    #[test]
    fn sem_jitter_o_backoff_cresce_exponencialmente() {
        let policy = policy(4, false);

        assert_eq!(backoff_for(&policy, 1), Duration::from_millis(50));
        assert_eq!(backoff_for(&policy, 2), Duration::from_millis(100));
        assert_eq!(backoff_for(&policy, 3), Duration::from_millis(200));
    }

    #[test]
    fn com_jitter_a_espera_fica_no_intervalo_e_varia() {
        let policy = policy(4, true);
        let teto = Duration::from_millis(100);

        let amostras: Vec<Duration> = (0..64).map(|_| backoff_for(&policy, 2)).collect();

        assert!(
            amostras.iter().all(|d| *d <= teto),
            "jitter estourou o teto"
        );
        assert!(
            amostras.windows(2).any(|par| par[0] != par[1]),
            "64 amostras idênticas não são jitter"
        );
    }
}
