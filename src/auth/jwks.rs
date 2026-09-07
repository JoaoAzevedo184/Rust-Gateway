//! Cache de JWKS (spec §8.1).
//!
//! O caminho da requisição só lê o snapshot; não faz I/O de rede para validar um
//! token, exceto no caso de `kid` desconhecido.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey};

use crate::clock::Clock;

#[derive(Debug, Clone)]
pub struct JwksSettings {
    pub refresh_interval: Duration,
    pub stale_max_age: Duration,
    pub unknown_kid_cooldown: Duration,
}

#[derive(Debug, Clone)]
pub struct VerifyKey {
    pub key: DecodingKey,
    /// Algoritmo derivado da própria chave, nunca do header do token.
    pub alg: Algorithm,
}

#[derive(Debug, Default)]
pub struct JwksSnapshot {
    keys: HashMap<String, VerifyKey>,
    /// Instante do último fetch **bem-sucedido**. Zero significa "nunca".
    fetched_at_ms: u64,
}

impl JwksSnapshot {
    pub fn get(&self, kid: &str) -> Option<&VerifyKey> {
        self.keys.get(kid)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn fetched_at_ms(&self) -> u64 {
        self.fetched_at_ms
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    #[error("falha ao buscar JWKS: {0}")]
    Fetch(String),

    #[error("JWKS malformada: {0}")]
    Malformed(String),
}

pub struct JwksCache {
    snapshot: ArcSwap<JwksSnapshot>,
    url: String,
    settings: JwksSettings,
    clock: Arc<dyn Clock>,
    http: reqwest::Client,
    /// Serializa refreshes forçados: requisições concorrentes com o mesmo `kid`
    /// novo esperam o mesmo refresh, não disparam N chamadas ao Auth Service.
    single_flight: tokio::sync::Mutex<()>,
    last_forced_ms: AtomicU64,
}

impl std::fmt::Debug for JwksCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwksCache")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl JwksCache {
    pub fn new(url: String, settings: JwksSettings, clock: Arc<dyn Clock>) -> Self {
        ensure_crypto_provider();

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("cliente HTTP construível: o provider TLS acabou de ser instalado");

        Self {
            snapshot: ArcSwap::from_pointee(JwksSnapshot::default()),
            url,
            settings,
            clock,
            http,
            single_flight: tokio::sync::Mutex::new(()),
            last_forced_ms: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> Arc<JwksSnapshot> {
        self.snapshot.load_full()
    }

    /// Idade do cache. `None` enquanto nenhum fetch tiver sucedido.
    pub fn age(&self) -> Option<Duration> {
        let fetched_at = self.snapshot().fetched_at_ms;
        if fetched_at == 0 {
            return None;
        }
        Some(Duration::from_millis(
            self.clock.now_ms().saturating_sub(fetched_at),
        ))
    }

    /// Um snapshot é utilizável enquanto estiver dentro de `stale_max_age`.
    ///
    /// Com o refresh falhando, o snapshot anterior continua válido: é o que faz um
    /// restart do Auth Service não derrubar o tráfego autenticado. Passando da
    /// janela, o gateway prefere 503 a aceitar chaves de idade desconhecida.
    pub fn is_usable(&self) -> bool {
        match self.age() {
            Some(age) => age <= self.settings.stale_max_age,
            None => false,
        }
    }

    /// Busca a chave, com um refresh forçado quando o `kid` é desconhecido.
    pub async fn key_for(&self, kid: &str) -> Option<VerifyKey> {
        if let Some(key) = self.snapshot().get(kid).cloned() {
            return Some(key);
        }

        // Sem o refresh forçado, toda rotação de chave causaria até
        // `refresh_interval` de 401 em massa.
        self.force_refresh().await;
        self.snapshot().get(kid).cloned()
    }

    /// Refresh forçado, com single-flight e cooldown.
    ///
    /// Sem o cooldown, um `kid` aleatório em loop viraria vetor de DoS contra o
    /// Auth Service: cada requisição inválida custaria uma chamada de rede.
    async fn force_refresh(&self) {
        let now = self.clock.now_ms();
        let last = self.last_forced_ms.load(Ordering::SeqCst);
        let cooldown = self.settings.unknown_kid_cooldown.as_millis() as u64;

        if last != 0 && now.saturating_sub(last) < cooldown {
            return;
        }

        let before = self.snapshot().fetched_at_ms;
        let _guard = self.single_flight.lock().await;

        // Quem esperou o lock pode já ter sido atendido por quem o segurava.
        if self.snapshot().fetched_at_ms > before {
            return;
        }

        self.last_forced_ms
            .store(self.clock.now_ms(), Ordering::SeqCst);

        if let Err(err) = self.refresh().await {
            tracing::warn!(error = %err, "refresh forçado de JWKS falhou");
        }
    }

    /// Busca a JWKS e substitui o snapshot. Em caso de falha, o snapshot anterior
    /// permanece: um erro de rede não pode zerar as chaves conhecidas.
    pub async fn refresh(&self) -> Result<(), JwksError> {
        let response = self
            .http
            .get(&self.url)
            .send()
            .await
            .map_err(|err| JwksError::Fetch(err.to_string()))?;

        if !response.status().is_success() {
            return Err(JwksError::Fetch(format!("HTTP {}", response.status())));
        }

        let body = response
            .bytes()
            .await
            .map_err(|err| JwksError::Fetch(err.to_string()))?;

        let keys = parse_jwks(&body)?;
        let fetched_at_ms = self.clock.now_ms();

        tracing::info!(keys = keys.len(), "JWKS atualizada");
        self.snapshot.store(Arc::new(JwksSnapshot {
            keys,
            fetched_at_ms,
        }));

        Ok(())
    }

    /// Refresh proativo em background. O caminho da requisição nunca espera por ele.
    pub fn spawn_refresher(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.settings.refresh_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                ticker.tick().await;
                if let Err(err) = self.refresh().await {
                    tracing::warn!(error = %err, "refresh periódico de JWKS falhou, mantendo o snapshot anterior");
                }
            }
        })
    }
}

/// Instala o provider criptográfico do rustls uma única vez.
///
/// O build não escolhe um por padrão, e sem provider o cliente HTTPS entra em
/// pânico na construção. Fica na biblioteca, e não no `main`, para que os testes
/// e qualquer outro consumidor da crate obtenham o mesmo comportamento.
fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn parse_jwks(body: &[u8]) -> Result<HashMap<String, VerifyKey>, JwksError> {
    let set: JwkSet =
        serde_json::from_slice(body).map_err(|err| JwksError::Malformed(err.to_string()))?;

    let mut keys = HashMap::new();

    for jwk in &set.keys {
        // Uma chave sem `kid` não é endereçável por um token; ignorá-la é melhor
        // que inventar um identificador.
        let Some(kid) = jwk.common.key_id.clone() else {
            continue;
        };
        let Some(alg) = algorithm_of(jwk) else {
            continue;
        };

        match DecodingKey::from_jwk(jwk) {
            Ok(key) => {
                keys.insert(kid, VerifyKey { key, alg });
            }
            Err(err) => {
                tracing::warn!(kid = %kid, error = %err, "chave da JWKS ignorada");
            }
        }
    }

    if keys.is_empty() {
        return Err(JwksError::Malformed(
            "nenhuma chave utilizável no conjunto".into(),
        ));
    }

    Ok(keys)
}

/// Algoritmo da chave: o declarado em `alg`, ou o padrão do tipo de chave.
/// Chaves simétricas são recusadas — uma JWKS pública não deveria carregá-las, e
/// aceitá-las abriria a porta para verificação com segredo compartilhado.
fn algorithm_of(jwk: &Jwk) -> Option<Algorithm> {
    if let Some(declared) = jwk.common.key_algorithm {
        return Algorithm::try_from(declared).ok().filter(is_asymmetric);
    }

    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(_) => Some(Algorithm::ES256),
        AlgorithmParameters::OctetKeyPair(_) => Some(Algorithm::EdDSA),
        AlgorithmParameters::OctetKey(_) | AlgorithmParameters::Other(_) => None,
        _ => None,
    }
}

fn is_asymmetric(alg: &Algorithm) -> bool {
    !matches!(alg, Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    fn settings() -> JwksSettings {
        JwksSettings {
            refresh_interval: Duration::from_secs(300),
            stale_max_age: Duration::from_secs(1800),
            unknown_kid_cooldown: Duration::from_secs(30),
        }
    }

    #[test]
    fn cache_sem_fetch_nunca_e_utilizavel() {
        let cache = JwksCache::new(
            "http://auth/jwks".into(),
            settings(),
            Arc::new(TestClock::default()),
        );

        assert!(!cache.is_usable());
        assert!(cache.age().is_none());
    }

    #[test]
    fn snapshot_dentro_da_janela_stale_continua_utilizavel() {
        let clock = Arc::new(TestClock::new(1_000_000));
        let cache = JwksCache::new("http://auth/jwks".into(), settings(), clock.clone());

        cache.snapshot.store(Arc::new(JwksSnapshot {
            keys: HashMap::new(),
            fetched_at_ms: 1_000_000,
        }));

        clock.advance_ms(1_799_000);
        assert!(cache.is_usable(), "ainda dentro de stale_max_age");

        clock.advance_ms(2_000);
        assert!(!cache.is_usable(), "passou de stale_max_age");
    }

    #[test]
    fn jwks_sem_chave_utilizavel_e_recusada() {
        let body = br#"{"keys":[]}"#;
        assert!(matches!(parse_jwks(body), Err(JwksError::Malformed(_))));
    }

    #[test]
    fn chave_simetrica_na_jwks_e_ignorada() {
        let body = br#"{"keys":[{"kty":"oct","kid":"k1","alg":"HS256","k":"c2VjcmV0"}]}"#;
        assert!(matches!(parse_jwks(body), Err(JwksError::Malformed(_))));
    }
}
