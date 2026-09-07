//! Store Redis (spec §9.2).
//!
//! Uma round trip por requisição, com toda a decisão dentro de um script Lua.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;

use crate::ratelimit::bucket::ttl_ms;
use crate::ratelimit::store::{BucketRequest, Decision, RateLimitStore, StoreError};

/// Avalia todos os buckets antes de deduzir qualquer um, e devolve
/// `{allowed, limit, remaining, retry_after_ms}`.
///
/// Pontos que não são detalhe de implementação:
///
/// - **O relógio é o do Redis** (`TIME`), não o do gateway. Réplicas com clock
///   dessincronizado produziriam refills inconsistentes sobre o mesmo bucket.
/// - **Tudo-ou-nada:** o primeiro laço só calcula; o segundo é quem escreve.
/// - **Recusa não escreve.** Além de manter o estado intacto, evita renovar o TTL
///   de uma chave que só recebe requisições rejeitadas.
const SCRIPT: &str = r#"
local now_pair = redis.call('TIME')
local now_ms = tonumber(now_pair[1]) * 1000 + math.floor(tonumber(now_pair[2]) / 1000)

local n = #KEYS
local tokens = {}
local allowed = 1

for i = 1, n do
  local base = (i - 1) * 4
  local cost = tonumber(ARGV[base + 1])
  local capacity = tonumber(ARGV[base + 2])
  local rate = tonumber(ARGV[base + 3])

  local stored = redis.call('HMGET', KEYS[i], 'tokens', 'last_refill_ms')
  local current = tonumber(stored[1])
  local last = tonumber(stored[2])

  if current == nil or last == nil then
    current = capacity
    last = now_ms
  end

  local elapsed = now_ms - last
  if elapsed < 0 then elapsed = 0 end

  local filled = current + (elapsed / 1000.0) * rate
  if filled > capacity then filled = capacity end

  tokens[i] = filled
  if filled < cost then allowed = 0 end
end

local limit = -1
local remaining = -1
local retry_after_ms = 0

for i = 1, n do
  local base = (i - 1) * 4
  local cost = tonumber(ARGV[base + 1])
  local capacity = tonumber(ARGV[base + 2])
  local rate = tonumber(ARGV[base + 3])
  local ttl = tonumber(ARGV[base + 4])

  if allowed == 1 then
    tokens[i] = tokens[i] - cost
    redis.call('HSET', KEYS[i], 'tokens', tokens[i], 'last_refill_ms', now_ms)
    redis.call('PEXPIRE', KEYS[i], ttl)
  elseif tokens[i] < cost and rate > 0 then
    local wait = math.ceil(((cost - tokens[i]) / rate) * 1000)
    if wait > retry_after_ms then retry_after_ms = wait end
  end

  local left = math.floor(tokens[i])
  if left < 0 then left = 0 end
  if remaining == -1 or left < remaining then
    remaining = left
    limit = capacity
  end
end

return {allowed, limit, remaining, retry_after_ms}
"#;

pub struct RedisStore {
    connection: ConnectionManager,
    script: Arc<redis::Script>,
    timeout: Duration,
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl RedisStore {
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self, StoreError> {
        let client =
            redis::Client::open(url).map_err(|err| StoreError::Unavailable(err.to_string()))?;

        let connection = ConnectionManager::new(client)
            .await
            .map_err(|err| StoreError::Unavailable(err.to_string()))?;

        Ok(Self {
            connection,
            script: Arc::new(redis::Script::new(SCRIPT)),
            timeout,
        })
    }

    pub fn script_source() -> &'static str {
        SCRIPT
    }
}

#[async_trait]
impl RateLimitStore for RedisStore {
    async fn try_acquire(&self, buckets: &[BucketRequest]) -> Result<Decision, StoreError> {
        if buckets.is_empty() {
            return Ok(Decision {
                allowed: true,
                limit: 0,
                remaining: 0,
                retry_after: None,
            });
        }

        let mut invocation = self.script.prepare_invoke();
        for bucket in buckets {
            invocation.key(bucket.key.as_str());
        }
        for bucket in buckets {
            invocation
                .arg(bucket.cost)
                .arg(bucket.capacity)
                .arg(bucket.refill_per_sec)
                .arg(ttl_ms(bucket.capacity, bucket.refill_per_sec));
        }

        let mut connection = self.connection.clone();

        // Timeout agressivo: sem ele, um Redis lento vira latência em toda
        // requisição, e a proteção passa a custar mais que o abuso que evita.
        let result: Vec<i64> =
            tokio::time::timeout(self.timeout, invocation.invoke_async(&mut connection))
                .await
                .map_err(|_| StoreError::Timeout)?
                .map_err(|err| StoreError::Unavailable(err.to_string()))?;

        if result.len() < 4 {
            return Err(StoreError::Unavailable(format!(
                "resposta inesperada do script: {result:?}"
            )));
        }

        let allowed = result[0] == 1;
        Ok(Decision {
            allowed,
            limit: result[1].max(0) as u32,
            remaining: result[2].max(0) as u32,
            retry_after: (!allowed).then(|| Duration::from_millis(result[3].max(0) as u64)),
        })
    }

    fn kind(&self) -> &'static str {
        "redis"
    }
}
