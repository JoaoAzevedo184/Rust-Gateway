//! Degradação do rate limit (spec §9.4, D6).
//!
//! Conforme D6, o rate limit **falha aberto**: derrubar tráfego legítimo porque o
//! Redis caiu é pior que o abuso evitado. Três peças sustentam isso sem que a
//! queda do store vire um segundo incidente.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::clock::Clock;
use crate::observability::metrics::Metrics;
use crate::ratelimit::store::{BucketRequest, Decision, RateLimitStore};

/// Falhas consecutivas antes de o gateway parar de tentar.
const OPEN_AFTER_FAILURES: u32 = 5;
/// Quanto tempo o store fica de lado antes de uma nova tentativa.
const OPEN_FOR: Duration = Duration::from_secs(5);
/// Intervalo mínimo entre warns. Log por requisição durante uma queda de Redis é
/// o segundo incidente.
const WARN_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct RateLimiter {
    store: Arc<dyn RateLimitStore>,
    metrics: Arc<Metrics>,
    clock: Arc<dyn Clock>,
    consecutive_failures: AtomicU32,
    open_until_ms: AtomicU64,
    last_warn_ms: AtomicU64,
}

impl RateLimiter {
    pub fn new(
        store: Arc<dyn RateLimitStore>,
        metrics: Arc<Metrics>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            metrics,
            clock,
            consecutive_failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
            last_warn_ms: AtomicU64::new(0),
        }
    }

    pub fn store_kind(&self) -> &'static str {
        self.store.kind()
    }

    /// `None` significa degradado: a requisição passa sem decisão.
    pub async fn check(&self, buckets: &[BucketRequest]) -> Option<Decision> {
        if buckets.is_empty() {
            return None;
        }

        let now_ms = self.clock.now_ms();

        // Breaker sobre o próprio store: enquanto o Redis está fora, o gateway
        // para de tentar a cada requisição em vez de pagar o timeout sempre.
        if now_ms < self.open_until_ms.load(Ordering::Relaxed) {
            self.degrade("store em cooldown após falhas consecutivas", now_ms);
            return None;
        }

        match self.store.try_acquire(buckets).await {
            Ok(decision) => {
                self.consecutive_failures.store(0, Ordering::Relaxed);
                Some(decision)
            }
            Err(err) => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= OPEN_AFTER_FAILURES {
                    self.open_until_ms
                        .store(now_ms + OPEN_FOR.as_millis() as u64, Ordering::Relaxed);
                    self.consecutive_failures.store(0, Ordering::Relaxed);
                }

                self.degrade(&err.to_string(), now_ms);
                None
            }
        }
    }

    fn degrade(&self, reason: &str, now_ms: u64) {
        self.metrics.ratelimit_degraded();

        let last = self.last_warn_ms.load(Ordering::Relaxed);
        if last == 0 || now_ms.saturating_sub(last) >= WARN_EVERY.as_millis() as u64 {
            self.last_warn_ms.store(now_ms, Ordering::Relaxed);
            tracing::warn!(
                store = self.store.kind(),
                reason,
                "rate limit degradado: requisições passam sem decisão"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use crate::config::KeyKind;
    use crate::ratelimit::store::StoreError;
    use async_trait::async_trait;

    #[derive(Debug)]
    struct SempreFalha(AtomicU32);

    #[async_trait]
    impl RateLimitStore for SempreFalha {
        async fn try_acquire(&self, _buckets: &[BucketRequest]) -> Result<Decision, StoreError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(StoreError::Timeout)
        }
        fn kind(&self) -> &'static str {
            "falho"
        }
    }

    fn bucket() -> BucketRequest {
        BucketRequest {
            key: "rl:v1:r:ip:1.2.3.4".into(),
            kind: KeyKind::Ip,
            capacity: 10,
            refill_per_sec: 1.0,
            cost: 1,
        }
    }

    #[tokio::test]
    async fn store_fora_falha_aberto_e_conta_a_degradacao() {
        let metrics = Arc::new(Metrics::new().unwrap());
        let limiter = RateLimiter::new(
            Arc::new(SempreFalha(AtomicU32::new(0))),
            metrics.clone(),
            Arc::new(TestClock::default()),
        );

        assert!(limiter.check(&[bucket()]).await.is_none(), "falha aberto");
        assert!(
            metrics
                .render()
                .contains("gateway_ratelimit_degraded_total 1")
        );
    }

    #[tokio::test]
    async fn apos_falhas_consecutivas_o_gateway_para_de_tentar() {
        let store = Arc::new(SempreFalha(AtomicU32::new(0)));
        let clock = Arc::new(TestClock::new(1_000_000));
        let limiter = RateLimiter::new(
            store.clone(),
            Arc::new(Metrics::new().unwrap()),
            clock.clone(),
        );

        for _ in 0..OPEN_AFTER_FAILURES {
            limiter.check(&[bucket()]).await;
        }
        let tentativas = store.0.load(Ordering::SeqCst);

        // Com o breaker aberto, nenhuma dessas chega ao store.
        for _ in 0..10 {
            limiter.check(&[bucket()]).await;
        }
        assert_eq!(
            store.0.load(Ordering::SeqCst),
            tentativas,
            "o store foi consultado durante o cooldown"
        );

        clock.advance_ms(OPEN_FOR.as_millis() as u64 + 1);
        limiter.check(&[bucket()]).await;
        assert_eq!(
            store.0.load(Ordering::SeqCst),
            tentativas + 1,
            "passado o cooldown, volta a tentar"
        );
    }
}
