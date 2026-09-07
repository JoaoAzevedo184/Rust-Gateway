//! Relógio injetável.
//!
//! Tudo que depende de tempo — token bucket, cache de JWKS, futuramente o circuit
//! breaker — recebe um `Arc<dyn Clock>`. Sem isso os testes viram `sleep`, e uma
//! suíte que dorme é uma suíte que se deixa de rodar.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: std::fmt::Debug + Send + Sync + 'static {
    /// Milissegundos desde a época Unix.
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

/// Relógio manual, para testes. Não é `#[cfg(test)]` porque os testes de
/// integração consomem a crate como biblioteca externa.
#[derive(Debug)]
pub struct TestClock(AtomicU64);

impl TestClock {
    pub fn new(start_ms: u64) -> Self {
        Self(AtomicU64::new(start_ms))
    }

    pub fn advance_ms(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn set_ms(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new(1_700_000_000_000)
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
