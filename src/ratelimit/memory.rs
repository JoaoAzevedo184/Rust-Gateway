//! Store in-memory (spec §9.3).
//!
//! Mesma semântica do Redis, com mutex por shard. **Não recomendado para
//! múltiplas réplicas**: cada réplica conta separadamente, e o limite efetivo
//! vira N vezes o configurado.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::clock::Clock;
use crate::ratelimit::bucket::{BucketState, refill, time_to_tokens_ms, ttl_ms};
use crate::ratelimit::store::{BucketRequest, Decision, RateLimitStore, StoreError};

const SHARDS: usize = 16;
/// Entradas por shard antes de uma varredura de expirados.
const PRUNE_THRESHOLD: usize = 1024;

#[derive(Debug, Clone, Copy)]
struct Entry {
    state: BucketState,
    expires_at_ms: u64,
}

#[derive(Debug)]
pub struct MemoryStore {
    shards: Vec<Mutex<HashMap<String, Entry>>>,
    clock: Arc<dyn Clock>,
}

impl MemoryStore {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            clock,
        }
    }

    fn shard_of(&self, key: &str) -> usize {
        use std::hash::{BuildHasher, RandomState};
        // Um hasher fixo por store mantém a chave sempre no mesmo shard.
        static SEED: std::sync::OnceLock<RandomState> = std::sync::OnceLock::new();
        (SEED.get_or_init(RandomState::new).hash_one(key) as usize) % SHARDS
    }

    fn prune(map: &mut HashMap<String, Entry>, now_ms: u64) {
        if map.len() > PRUNE_THRESHOLD {
            map.retain(|_, entry| entry.expires_at_ms > now_ms);
        }
    }
}

#[async_trait]
impl RateLimitStore for MemoryStore {
    async fn try_acquire(&self, buckets: &[BucketRequest]) -> Result<Decision, StoreError> {
        if buckets.is_empty() {
            return Ok(Decision {
                allowed: true,
                limit: 0,
                remaining: 0,
                retry_after: None,
            });
        }

        let now_ms = self.clock.now_ms();

        // Os buckets de uma rota podem cair em shards diferentes, e a decisão é
        // tudo-ou-nada. Travar em ordem crescente de índice é o que impede que
        // duas requisições com conjuntos de shards cruzados se travem mutuamente.
        let mut indices: Vec<usize> = buckets.iter().map(|b| self.shard_of(&b.key)).collect();
        let mut ordered: Vec<usize> = indices.clone();
        ordered.sort_unstable();
        ordered.dedup();

        let mut guards: HashMap<usize, std::sync::MutexGuard<'_, HashMap<String, Entry>>> =
            HashMap::new();
        for index in ordered {
            let guard = self.shards[index].lock().map_err(|_| {
                StoreError::Unavailable("shard envenenado por um pânico anterior".into())
            })?;
            guards.insert(index, guard);
        }

        // Passo 1: recalcular todos, sem escrever nada.
        let mut refreshed = Vec::with_capacity(buckets.len());
        for (bucket, shard) in buckets.iter().zip(indices.iter()) {
            let guard = guards.get(shard).expect("shard travado acima");
            let current = guard
                .get(&bucket.key)
                .map(|entry| entry.state)
                .unwrap_or_else(|| BucketState::full(bucket.capacity, now_ms));

            refreshed.push(refill(
                current,
                bucket.capacity,
                bucket.refill_per_sec,
                now_ms,
            ));
        }

        let allowed = buckets
            .iter()
            .zip(refreshed.iter())
            .all(|(bucket, state)| state.tokens >= bucket.cost as f64);

        // Passo 2: só então deduzir.
        if allowed {
            for ((bucket, state), shard) in
                buckets.iter().zip(refreshed.iter_mut()).zip(indices.iter())
            {
                state.tokens -= bucket.cost as f64;

                let guard = guards.get_mut(shard).expect("shard travado acima");
                guard.insert(
                    bucket.key.clone(),
                    Entry {
                        state: *state,
                        expires_at_ms: now_ms
                            .saturating_add(ttl_ms(bucket.capacity, bucket.refill_per_sec)),
                    },
                );
                Self::prune(guard, now_ms);
            }
        }

        indices.clear();
        Ok(summarize(buckets, &refreshed, allowed))
    }

    fn kind(&self) -> &'static str {
        "memory"
    }
}

/// Agrega a decisão: os headers descrevem o bucket **mais restritivo**, e o
/// `Retry-After` é o maior tempo de espera entre os que recusaram — esperar menos
/// que isso garantiria uma segunda recusa.
pub fn summarize(buckets: &[BucketRequest], states: &[BucketState], allowed: bool) -> Decision {
    let mut limit = u32::MAX;
    let mut remaining = u32::MAX;
    let mut retry_after_ms = 0u64;

    for (bucket, state) in buckets.iter().zip(states.iter()) {
        let left = state.tokens.max(0.0).floor() as u32;
        if left < remaining {
            remaining = left;
            limit = bucket.capacity;
        }

        if !allowed && state.tokens < bucket.cost as f64 {
            retry_after_ms = retry_after_ms.max(time_to_tokens_ms(
                state.tokens,
                bucket.cost as f64,
                bucket.refill_per_sec,
            ));
        }
    }

    Decision {
        allowed,
        limit: if limit == u32::MAX { 0 } else { limit },
        remaining: if remaining == u32::MAX { 0 } else { remaining },
        retry_after: (!allowed).then(|| Duration::from_millis(retry_after_ms)),
    }
}
