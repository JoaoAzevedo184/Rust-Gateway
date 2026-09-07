//! Schema de configuração, defaults e validação de startup (spec §5).
//!
//! Configuração inválida impede o processo de subir. A validação acumula todos os
//! problemas antes de falhar: descobrir um erro por vez, com um restart entre
//! cada, é o que torna a edição de config um exercício de paciência.

pub mod provider;

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

use http::Uri;
use ipnet::IpNet;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("não foi possível ler {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("YAML inválido em {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_norway::Error,
    },

    #[error("configuração inválida:\n{}", .0.iter().map(|p| format!("  - {p}")).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    /// Ausente quando nenhuma rota exige autenticação. A validação garante a
    /// coerência entre esta seção e as rotas.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    #[serde(default)]
    pub resilience: ResilienceSection,
    #[serde(default)]
    pub upstreams: BTreeMap<String, UpstreamConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "defaults::bind")]
    pub bind: SocketAddr,
    #[serde(default = "defaults::admin_bind")]
    pub admin_bind: SocketAddr,
    /// Teto absoluto por requisição, incluindo retries. Aplicado na Fase 2.
    #[serde(default = "defaults::request_timeout", with = "humantime_serde")]
    pub request_timeout: Duration,
    #[serde(
        default = "defaults::max_body_bytes",
        deserialize_with = "de_byte_size"
    )]
    pub max_body_bytes: u64,
    /// Vazio = ignora `X-Forwarded-For` e usa o endereço do peer.
    #[serde(default)]
    pub trusted_proxies: Vec<IpNet>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: defaults::bind(),
            admin_bind: defaults::admin_bind(),
            request_timeout: defaults::request_timeout(),
            max_body_bytes: defaults::max_body_bytes(),
            trusted_proxies: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub jwks_url: String,
    pub issuer: String,
    pub audience: String,
    #[serde(default = "defaults::refresh_interval", with = "humantime_serde")]
    pub refresh_interval: Duration,
    #[serde(default = "defaults::stale_max_age", with = "humantime_serde")]
    pub stale_max_age: Duration,
    #[serde(default = "defaults::leeway", with = "humantime_serde")]
    pub leeway: Duration,
    #[serde(default = "defaults::unknown_kid_cooldown", with = "humantime_serde")]
    pub unknown_kid_cooldown: Duration,
    #[serde(default = "defaults::scope_claim")]
    pub scope_claim: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub store: StoreKind,
    #[serde(default)]
    pub redis_url: Option<String>,
    #[serde(default = "defaults::redis_timeout", with = "humantime_serde")]
    pub redis_timeout: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            store: StoreKind::Memory,
            redis_url: None,
            redis_timeout: defaults::redis_timeout(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    #[default]
    Memory,
    Redis,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResilienceSection {
    #[serde(default)]
    pub default: ResiliencePatch,
}

/// Política de resiliência parcial. A herança é
/// `resilience.default` → `upstreams.<id>.resilience` → `routes[].resilience`,
/// com o nível mais específico vencendo campo a campo.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResiliencePatch {
    #[serde(default, with = "humantime_serde")]
    pub connect_timeout: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub upstream_timeout: Option<Duration>,
    #[serde(default)]
    pub retry: Option<RetryPatch>,
    #[serde(default)]
    pub circuit_breaker: Option<BreakerPatch>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPatch {
    pub max_attempts: Option<u32>,
    #[serde(default, with = "humantime_serde")]
    pub backoff: Option<Duration>,
    pub jitter: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BreakerPatch {
    pub failure_ratio: Option<f64>,
    pub min_requests: Option<u32>,
    #[serde(default, with = "humantime_serde")]
    pub window: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub open_for: Option<Duration>,
    pub half_open_probes: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub url: String,
    #[serde(default)]
    pub resilience: Option<ResiliencePatch>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub id: String,
    #[serde(rename = "match")]
    pub match_: MatchConfig,
    pub upstream: String,
    #[serde(default)]
    pub strip_prefix: bool,
    /// Obrigatório, inclusive para declarar `required: false`. Uma rota que ficou
    /// pública por esquecimento é a falha mais cara que este schema pode permitir.
    #[serde(default)]
    pub auth: Option<RouteAuthConfig>,
    #[serde(default)]
    pub rate_limit: Vec<LimitConfig>,
    #[serde(default)]
    pub resilience: Option<ResiliencePatch>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    pub prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteAuthConfig {
    pub required: bool,
    #[serde(default)]
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitConfig {
    pub key: KeyKind,
    pub capacity: u32,
    pub refill_per_sec: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    Sub,
    Ip,
}

impl KeyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            KeyKind::Sub => "sub",
            KeyKind::Ip => "ip",
        }
    }
}

// ---------------------------------------------------------------------------
// Política de resiliência resolvida
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ResiliencePolicy {
    pub connect_timeout: Duration,
    pub upstream_timeout: Duration,
    pub retry: RetryPolicy,
    pub circuit_breaker: BreakerPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: Duration,
    pub jitter: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BreakerPolicy {
    pub failure_ratio: f64,
    pub min_requests: u32,
    pub window: Duration,
    pub open_for: Duration,
    pub half_open_probes: u32,
}

impl Default for ResiliencePolicy {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(2),
            upstream_timeout: Duration::from_secs(5),
            retry: RetryPolicy {
                max_attempts: 2,
                backoff: Duration::from_millis(50),
                jitter: true,
            },
            circuit_breaker: BreakerPolicy {
                failure_ratio: 0.5,
                min_requests: 20,
                window: Duration::from_secs(30),
                open_for: Duration::from_secs(15),
                half_open_probes: 1,
            },
        }
    }
}

impl ResiliencePolicy {
    /// Aplica um patch campo a campo. Um `retry` parcial no nível da rota não
    /// zera o `backoff` herdado do upstream.
    pub fn apply(&mut self, patch: &ResiliencePatch) {
        if let Some(v) = patch.connect_timeout {
            self.connect_timeout = v;
        }
        if let Some(v) = patch.upstream_timeout {
            self.upstream_timeout = v;
        }
        if let Some(retry) = &patch.retry {
            if let Some(v) = retry.max_attempts {
                self.retry.max_attempts = v;
            }
            if let Some(v) = retry.backoff {
                self.retry.backoff = v;
            }
            if let Some(v) = retry.jitter {
                self.retry.jitter = v;
            }
        }
        if let Some(breaker) = &patch.circuit_breaker {
            if let Some(v) = breaker.failure_ratio {
                self.circuit_breaker.failure_ratio = v;
            }
            if let Some(v) = breaker.min_requests {
                self.circuit_breaker.min_requests = v;
            }
            if let Some(v) = breaker.window {
                self.circuit_breaker.window = v;
            }
            if let Some(v) = breaker.open_for {
                self.circuit_breaker.open_for = v;
            }
            if let Some(v) = breaker.half_open_probes {
                self.circuit_breaker.half_open_probes = v;
            }
        }
    }

    pub fn with(mut self, patch: Option<&ResiliencePatch>) -> Self {
        if let Some(patch) = patch {
            self.apply(patch);
        }
        self
    }
}

impl Config {
    pub fn parse(yaml: &str, origin: &str) -> Result<Self, ConfigError> {
        let config: Config = serde_norway::from_str(yaml).map_err(|source| ConfigError::Parse {
            path: origin.to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Política de resiliência efetiva de uma rota, já com a herança resolvida.
    pub fn resolved_resilience(&self, route: &RouteConfig) -> ResiliencePolicy {
        let upstream = self.upstreams.get(&route.upstream);
        ResiliencePolicy::default()
            .with(Some(&self.resilience.default))
            .with(upstream.and_then(|u| u.resilience.as_ref()))
            .with(route.resilience.as_ref())
    }

    /// Validação de startup (spec §5.3). Acumula todas as falhas.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();

        if let Some(auth) = &self.auth {
            if let Err(err) = validate_absolute_url(&auth.jwks_url) {
                problems.push(format!("auth.jwks_url {err}"));
            }
            if auth.issuer.trim().is_empty() {
                problems.push("auth.issuer não pode ser vazio".into());
            }
            if auth.audience.trim().is_empty() {
                problems.push("auth.audience não pode ser vazio".into());
            }
            if auth.stale_max_age < auth.refresh_interval {
                problems.push(format!(
                    "auth.stale_max_age ({:?}) é menor que auth.refresh_interval ({:?}): o cache expiraria antes do primeiro refresh",
                    auth.stale_max_age, auth.refresh_interval
                ));
            }
        }

        if self.rate_limit.store == StoreKind::Redis && self.rate_limit.redis_url.is_none() {
            problems.push("rate_limit.store: redis exige rate_limit.redis_url".into());
        }

        for (id, upstream) in &self.upstreams {
            if let Err(err) = validate_absolute_url(&upstream.url) {
                problems.push(format!("upstreams.{id}.url {err}"));
            }
        }

        if self.routes.is_empty() {
            problems.push("nenhuma rota configurada".into());
        }

        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut seen_prefixes: HashSet<String> = HashSet::new();

        for route in &self.routes {
            let id = &route.id;

            if !seen_ids.insert(id.as_str()) {
                problems.push(format!("routes: id duplicado {id:?}"));
            }

            if !self.upstreams.contains_key(&route.upstream) {
                problems.push(format!(
                    "routes.{id}: upstream {:?} não existe em upstreams",
                    route.upstream
                ));
            }

            let prefix = &route.match_.prefix;
            if !prefix.starts_with('/') {
                problems.push(format!(
                    "routes.{id}: match.prefix {prefix:?} precisa começar com /"
                ));
            }
            let normalized = normalize_prefix(prefix);
            if !seen_prefixes.insert(normalized.clone()) {
                problems.push(format!("routes.{id}: match.prefix {prefix:?} duplicado"));
            }

            let Some(auth) = &route.auth else {
                problems.push(format!(
                    "routes.{id}: bloco auth ausente. Deve ser explícito, mesmo para declarar required: false"
                ));
                continue;
            };

            if auth.required && self.auth.is_none() {
                problems.push(format!(
                    "routes.{id}: auth.required: true, mas não há seção auth global com jwks_url"
                ));
            }

            if !auth.required && !auth.scopes.is_empty() {
                problems.push(format!(
                    "routes.{id}: auth.required: false com scopes {:?}. Escopo só é verificável em rota autenticada",
                    auth.scopes
                ));
            }

            for limit in &route.rate_limit {
                if limit.key == KeyKind::Sub && !auth.required {
                    problems.push(format!(
                        "routes.{id}: rate_limit key: sub em rota com auth.required: false — sem token não há sub"
                    ));
                }
                if limit.capacity == 0 {
                    problems.push(format!(
                        "routes.{id}: rate_limit capacity precisa ser maior que zero"
                    ));
                }
                if limit.refill_per_sec <= 0.0 || !limit.refill_per_sec.is_finite() {
                    problems.push(format!(
                        "routes.{id}: rate_limit refill_per_sec precisa ser um número positivo"
                    ));
                }
            }

            let resilience = self.resolved_resilience(route);
            if self.server.request_timeout < resilience.upstream_timeout {
                problems.push(format!(
                    "routes.{id}: server.request_timeout ({:?}) é menor que upstream_timeout ({:?}) — o teto global cortaria antes da tentativa individual",
                    self.server.request_timeout, resilience.upstream_timeout
                ));
            }
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(problems))
        }
    }
}

/// Remove a barra final, exceto na raiz, que vira string vazia. `/users/` e
/// `/users` são o mesmo prefixo, e comparar as duas formas cruas deixaria
/// passar uma duplicata.
pub fn normalize_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_end_matches('/');
    trimmed.to_string()
}

fn validate_absolute_url(raw: &str) -> Result<Uri, String> {
    let uri: Uri = raw
        .parse()
        .map_err(|_| format!("{raw:?} não é uma URL válida"))?;
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        Some(other) => return Err(format!("{raw:?} usa esquema não suportado {other:?}")),
        None => return Err(format!("{raw:?} precisa de esquema http ou https")),
    }
    if uri.authority().is_none() {
        return Err(format!("{raw:?} não tem host"));
    }
    Ok(uri)
}

fn de_byte_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(u64),
        Str(String),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Int(n) => Ok(n),
        Raw::Str(s) => parse_byte_size(&s).map_err(D::Error::custom),
    }
}

/// Aceita `1024`, `2MiB`, `10 MB`. Sufixos binários e decimais são distintos:
/// `MiB` é 1024², `MB` é 1000².
pub fn parse_byte_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(split);

    let number: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("tamanho inválido {raw:?}"))?;

    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        other => {
            return Err(format!(
                "sufixo de tamanho desconhecido {other:?} em {raw:?}"
            ));
        }
    };

    Ok((number * multiplier as f64) as u64)
}

mod defaults {
    use super::*;

    pub fn bind() -> SocketAddr {
        "0.0.0.0:8080".parse().expect("literal válido")
    }
    pub fn admin_bind() -> SocketAddr {
        "0.0.0.0:9090".parse().expect("literal válido")
    }
    pub fn request_timeout() -> Duration {
        Duration::from_secs(30)
    }
    pub fn max_body_bytes() -> u64 {
        2 * 1024 * 1024
    }
    pub fn refresh_interval() -> Duration {
        Duration::from_secs(5 * 60)
    }
    pub fn stale_max_age() -> Duration {
        Duration::from_secs(30 * 60)
    }
    pub fn leeway() -> Duration {
        Duration::from_secs(30)
    }
    pub fn unknown_kid_cooldown() -> Duration {
        Duration::from_secs(30)
    }
    pub fn scope_claim() -> String {
        "scope".to_string()
    }
    pub fn redis_timeout() -> Duration {
        Duration::from_millis(50)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMO: &str = r#"
upstreams:
  user-service: { url: http://user-service:8080 }
routes:
  - id: users
    match: { prefix: /users }
    upstream: user-service
    auth: { required: false }
"#;

    fn com_routes(routes: &str) -> String {
        format!(
            "upstreams:\n  user-service: {{ url: http://user-service:8080 }}\nroutes:\n{routes}"
        )
    }

    fn erros(yaml: &str) -> Vec<String> {
        match Config::parse(yaml, "teste") {
            Err(ConfigError::Invalid(problems)) => problems,
            Err(other) => panic!("esperava falha de validação, veio {other}"),
            Ok(_) => panic!("esperava falha de validação, a config passou"),
        }
    }

    #[test]
    fn config_minima_aplica_todos_os_defaults() {
        let config = Config::parse(MINIMO, "teste").unwrap();

        assert_eq!(config.server.bind.port(), 8080);
        assert_eq!(config.server.admin_bind.port(), 9090);
        assert_eq!(config.server.request_timeout, Duration::from_secs(30));
        assert_eq!(config.server.max_body_bytes, 2 * 1024 * 1024);
        assert_eq!(config.rate_limit.store, StoreKind::Memory);
        assert_eq!(config.rate_limit.redis_timeout, Duration::from_millis(50));
    }

    #[test]
    fn rota_sem_bloco_auth_e_erro_de_startup() {
        let problems = erros(&com_routes(
            "  - id: users\n    match: { prefix: /users }\n    upstream: user-service\n",
        ));
        assert!(
            problems.iter().any(|p| p.contains("bloco auth ausente")),
            "{problems:?}"
        );
    }

    #[test]
    fn upstream_inexistente_e_erro_de_startup() {
        let problems = erros(&com_routes(
            "  - id: users\n    match: { prefix: /users }\n    upstream: fantasma\n    auth: { required: false }\n",
        ));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("não existe em upstreams")),
            "{problems:?}"
        );
    }

    #[test]
    fn id_e_prefixo_duplicados_sao_erros_distintos() {
        let problems = erros(&com_routes(
            "  - id: users\n    match: { prefix: /users }\n    upstream: user-service\n    auth: { required: false }\n\
             \n  - id: users\n    match: { prefix: /users/ }\n    upstream: user-service\n    auth: { required: false }\n",
        ));
        assert!(
            problems.iter().any(|p| p.contains("id duplicado")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("duplicado") && p.contains("prefix")),
            "{problems:?}"
        );
    }

    #[test]
    fn key_sub_em_rota_anonima_e_erro_de_startup() {
        let problems = erros(&com_routes(
            "  - id: users\n    match: { prefix: /users }\n    upstream: user-service\n    auth: { required: false }\n\
             \n    rate_limit:\n      - { key: sub, capacity: 10, refill_per_sec: 1 }\n",
        ));
        assert!(
            problems.iter().any(|p| p.contains("key: sub")),
            "{problems:?}"
        );
    }

    #[test]
    fn scopes_em_rota_anonima_e_erro_de_startup() {
        let problems = erros(&com_routes(
            "  - id: users\n    match: { prefix: /users }\n    upstream: user-service\n    auth: { required: false, scopes: [user.read] }\n",
        ));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("Escopo só é verificável")),
            "{problems:?}"
        );
    }

    #[test]
    fn request_timeout_menor_que_upstream_timeout_e_erro_de_startup() {
        let yaml = r#"
server:
  request_timeout: 2s
resilience:
  default:
    upstream_timeout: 5s
upstreams:
  user-service: { url: http://user-service:8080 }
routes:
  - id: users
    match: { prefix: /users }
    upstream: user-service
    auth: { required: false }
"#;
        let problems = erros(yaml);
        assert!(
            problems.iter().any(|p| p.contains("request_timeout")),
            "{problems:?}"
        );
    }

    #[test]
    fn store_redis_sem_url_e_erro_de_startup() {
        let yaml = format!("rate_limit:\n  store: redis\n{MINIMO}");
        let problems = erros(&yaml);
        assert!(
            problems.iter().any(|p| p.contains("redis_url")),
            "{problems:?}"
        );
    }

    #[test]
    fn url_de_upstream_malformada_e_erro_de_startup() {
        let problems = erros(
            "upstreams:\n  user-service: { url: \"user-service:8080\" }\nroutes:\n  - id: users\n    match: { prefix: /users }\n    upstream: user-service\n    auth: { required: false }\n",
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("upstreams.user-service.url")),
            "{problems:?}"
        );
    }

    #[test]
    fn campo_desconhecido_nao_passa_silenciosamente() {
        let yaml = com_routes(
            "  - id: users\n    match: { prefix: /users }\n    upstream: user-service\n    auth: { required: false }\n    strip_prefixo: true\n",
        );
        assert!(matches!(
            Config::parse(&yaml, "teste"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn heranca_de_resiliencia_resolve_campo_a_campo() {
        let yaml = r#"
resilience:
  default:
    upstream_timeout: 5s
    retry: { max_attempts: 2, backoff: 50ms }
upstreams:
  payment-service:
    url: http://payment-service:8082
    resilience:
      upstream_timeout: 15s
      retry: { max_attempts: 0 }
routes:
  - id: payments
    match: { prefix: /payments }
    upstream: payment-service
    auth: { required: false }
  - id: payments-slow
    match: { prefix: /payments/slow }
    upstream: payment-service
    auth: { required: false }
    resilience:
      upstream_timeout: 25s
"#;
        let config = Config::parse(yaml, "teste").unwrap();

        let payments = config.resolved_resilience(&config.routes[0]);
        assert_eq!(
            payments.upstream_timeout,
            Duration::from_secs(15),
            "upstream vence o default"
        );
        assert_eq!(payments.retry.max_attempts, 0);
        assert_eq!(
            payments.retry.backoff,
            Duration::from_millis(50),
            "backoff continua herdado"
        );
        assert!(payments.retry.jitter, "campo não tocado mantém o default");

        let slow = config.resolved_resilience(&config.routes[1]);
        assert_eq!(
            slow.upstream_timeout,
            Duration::from_secs(25),
            "rota vence o upstream"
        );
        assert_eq!(
            slow.retry.max_attempts, 0,
            "o resto continua vindo do upstream"
        );
    }

    #[test]
    fn tamanhos_binarios_e_decimais_sao_distintos() {
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_byte_size("2 MB").unwrap(), 2_000_000);
        assert_eq!(parse_byte_size("1GiB").unwrap(), 1 << 30);
        assert!(parse_byte_size("2 quilos").is_err());
    }

    #[test]
    fn validacao_acumula_todos_os_problemas() {
        let problems = erros(
            "upstreams:\n  a: { url: http://a:1 }\nroutes:\n  - id: x\n    match: { prefix: /x }\n    upstream: fantasma\n",
        );
        assert!(
            problems.len() >= 2,
            "esperava mais de um problema, veio {problems:?}"
        );
    }
}
