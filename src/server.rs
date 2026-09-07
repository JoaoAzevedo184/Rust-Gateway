//! Montagem da pilha e bootstrap dos dois listeners (spec §4.1, §11.1).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::serve::IncomingStream;
use tokio::net::TcpListener;
use tower::{Service, ServiceBuilder};

use crate::auth::jwks::{JwksCache, JwksSettings};
use crate::auth::layer::{AuthLayer, AuthSettings, Authenticator};
use crate::clock::system_clock;
use crate::config::provider::{ConfigProvider, FileProvider};
use crate::config::{Config, ConfigError, StoreKind};
use crate::observability::correlation::CorrelationIdLayer;
use crate::observability::health::{AdminState, ReadinessCheck};
use crate::observability::metrics::{Metrics, MetricsLayer};
use crate::observability::tracing::SpanLayer;
use crate::peer::PeerAddr;
use crate::proxy::limit::BodyLimitLayer;
use crate::proxy::scrub::IdentityScrubLayer;
use crate::proxy::{ProxyClient, ProxyService};
use crate::ratelimit::layer::RateLimitLayer;
use crate::ratelimit::limiter::RateLimiter;
use crate::ratelimit::memory::MemoryStore;
use crate::ratelimit::redis::RedisStore;
use crate::ratelimit::store::RateLimitStore;
use crate::resilience::breaker::{BreakerRegistry, CircuitBreakerLayer};
use crate::resilience::retry::RetryLayer;
use crate::resilience::timeout::{RequestTimeoutLayer, UpstreamTimeoutLayer};
use crate::state::{AppState, ServerSettings};
use crate::{BoxFuture, Request, Response};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A pilha de processamento, de fora para dentro.
///
/// Os cinco primeiros são **globais**: rodam em toda requisição, inclusive nas que
/// serão rejeitadas com 404 ou 401. É por isso que o scrub de identidade e a
/// contagem de métricas não podem morar em stacks montadas por rota — elas nunca
/// veriam a requisição que não casou com rota alguma (D1, D9).
///
/// Depois de `route_resolve` vêm os layers **por rota**, que leem o
/// `RouteRuntime` das extensions e viram no-op quando a rota não pede.
///
/// A ordem entre os três layers de resiliência é a decisão, não um detalhe:
///
/// - `rate_limit` fica **fora** de `retry`: uma tentativa extra não pode consumir
///   um segundo token do usuário, que não pediu duas requisições.
/// - `circuit_breaker` fica **dentro** de `retry`: assim registra o resultado de
///   cada tentativa em vez de um agregado por requisição, e corta a segunda
///   tentativa imediatamente quando o circuito abre.
/// - `upstream_timeout` é o mais interno: seu escopo é uma tentativa.
pub fn gateway_service(
    state: &AppState,
) -> impl Service<Request, Response = Response, Error = Infallible, Future: Send> + Clone + Send + 'static
{
    ServiceBuilder::new()
        .layer(CorrelationIdLayer)
        .layer(IdentityScrubLayer)
        .layer(SpanLayer)
        .layer(MetricsLayer::new(state.metrics.clone()))
        .layer(RequestTimeoutLayer::new(state.server.request_timeout))
        .layer(BodyLimitLayer::new(state.server.max_body_bytes))
        .layer(crate::routing::layer::RouteResolveLayer::new(
            state.table.clone(),
        ))
        .layer(AuthLayer::new(
            state.authenticator.clone(),
            state.metrics.clone(),
        ))
        .layer(RateLimitLayer::new(
            state.limiter.clone(),
            state.server.trusted_proxies.clone(),
            state.metrics.clone(),
        ))
        .layer(RetryLayer::new(
            state.server.max_body_bytes,
            state.metrics.clone(),
        ))
        .layer(CircuitBreakerLayer::new(state.breakers.clone()))
        .layer(UpstreamTimeoutLayer)
        .service(ProxyService::new(
            state.proxy.clone(),
            state.metrics.clone(),
        ))
}

/// Monta o `AppState` a partir de uma configuração já validada.
pub async fn build_state(
    config: &Config,
    provider: &FileProvider,
) -> Result<Arc<AppState>, BoxError> {
    let clock = system_clock();
    let metrics = Arc::new(Metrics::new()?);
    let server = ServerSettings::from_config(config);

    let store: Arc<dyn RateLimitStore> = match config.rate_limit.store {
        StoreKind::Memory => Arc::new(MemoryStore::new(clock.clone())),
        StoreKind::Redis => {
            let url = config
                .rate_limit
                .redis_url
                .as_deref()
                .expect("validação de startup garante a URL");
            Arc::new(RedisStore::connect(url, config.rate_limit.redis_timeout).await?)
        }
    };
    tracing::info!(store = store.kind(), "rate limit store pronto");

    let limiter = Arc::new(RateLimiter::new(store, metrics.clone(), clock.clone()));

    let authenticator = match &config.auth {
        None => None,
        Some(auth) => {
            let jwks = Arc::new(JwksCache::new(
                auth.jwks_url.clone(),
                JwksSettings {
                    refresh_interval: auth.refresh_interval,
                    stale_max_age: auth.stale_max_age,
                    unknown_kid_cooldown: auth.unknown_kid_cooldown,
                },
                clock.clone(),
            ));

            // Um Auth Service fora no boot não impede o processo de subir: o
            // gateway sobe reportando `/ready` negativo até a JWKS chegar.
            if let Err(err) = jwks.refresh().await {
                tracing::warn!(error = %err, "JWKS indisponível no boot; o gateway sobe não-pronto");
            }
            jwks.clone().spawn_refresher();

            if let Err(err) = metrics.register_jwks_age(jwks.clone()) {
                tracing::warn!(error = %err, "não foi possível registrar gateway_jwks_cache_age_seconds");
            }

            Some(Arc::new(Authenticator::new(
                jwks,
                AuthSettings {
                    issuer: auth.issuer.clone(),
                    audience: auth.audience.clone(),
                    leeway: auth.leeway,
                    scope_claim: auth.scope_claim.clone(),
                },
            )))
        }
    };

    // Um cliente por `connect_timeout` distinto entre os upstreams configurados —
    // o mesmo cálculo que `RouterTable::build` usa para resolver
    // `UpstreamRuntime.resilience`, replicado aqui porque o proxy precisa do
    // conjunto de valores antes de qualquer requisição, não de um upstream por vez.
    let connect_timeouts: std::collections::BTreeSet<std::time::Duration> = config
        .upstreams
        .values()
        .map(|upstream| {
            crate::config::ResiliencePolicy::default()
                .with(Some(&config.resilience.default))
                .with(upstream.resilience.as_ref())
                .connect_timeout
        })
        .collect();

    let proxy = Arc::new(ProxyClient::new(
        server.trusted_proxies.clone(),
        connect_timeouts,
    ));

    let breakers = Arc::new(BreakerRegistry::new(clock.clone(), metrics.clone()));
    if let Err(err) = metrics.register_circuit_states(breakers.clone()) {
        tracing::warn!(error = %err, "não foi possível registrar gateway_circuit_state");
    }

    Ok(Arc::new(AppState {
        table: provider.subscribe(),
        server,
        authenticator,
        limiter,
        breakers,
        proxy,
        metrics,
        clock,
    }))
}

/// Carrega a configuração, monta o estado e serve até o sinal de desligamento.
pub async fn run(config_path: impl AsRef<Path>) -> Result<(), BoxError> {
    let load_result = FileProvider::load(config_path.as_ref());

    // A inicialização do tracing precisa acontecer antes de qualquer log, e
    // portanto antes do `?` que propagaria uma config inválida — senão o erro
    // mais informativo que o gateway produz (a lista de problemas de validação)
    // sairia sem log algum. `otlp_endpoint` só existe quando a config carregou.
    let otlp_endpoint = load_result
        .as_ref()
        .ok()
        .and_then(|provider| provider.config().tracing.as_ref())
        .map(|tracing| tracing.otlp_endpoint.clone());

    let tracer_provider = crate::observability::tracing::init(otlp_endpoint.as_deref());

    let result = run_gateway(load_result).await;

    // Encerra o exportador OTLP com o processo, para que spans ainda em lote no
    // momento do sinal de desligamento cheguem ao coletor em vez de se perderem.
    if let Err(err) = tracer_provider.shutdown() {
        tracing::warn!(error = %err, "falha ao encerrar o exportador de tracing");
    }

    result
}

async fn run_gateway(load_result: Result<FileProvider, ConfigError>) -> Result<(), BoxError> {
    let provider = load_result?;
    let config = provider.config().clone();

    let state = build_state(&config, &provider).await?;

    tracing::info!(
        rotas = state.table().routes().len(),
        upstreams = config.upstreams.len(),
        "configuração carregada"
    );

    let admin = crate::observability::health::router(AdminState {
        metrics: state.metrics.clone(),
        readiness: state.clone() as Arc<dyn ReadinessCheck>,
    });

    let public_listener = TcpListener::bind(config.server.bind).await?;
    let admin_listener = TcpListener::bind(config.server.admin_bind).await?;

    tracing::info!(
        publico = %config.server.bind,
        admin = %config.server.admin_bind,
        "gateway ouvindo"
    );

    let public = axum::serve(public_listener, MakeGateway::new(gateway_service(&state)))
        .with_graceful_shutdown(shutdown_signal());
    let admin = axum::serve(admin_listener, admin).with_graceful_shutdown(shutdown_signal());

    let (public, admin) = tokio::join!(public, admin);
    public?;
    admin?;

    tracing::info!("desligamento concluído");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("sinal recebido, drenando conexões");
}

/// Make-service que carimba o endereço do peer em cada requisição.
///
/// O endereço do socket é a única identidade de rede que o gateway observa
/// diretamente; tudo o mais sobre o cliente é afirmação de header. `PeerAddr` é a
/// base do rate limit por IP e do `X-Forwarded-For` de saída.
#[derive(Debug, Clone)]
pub struct MakeGateway<S> {
    inner: S,
}

impl<S> MakeGateway<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<'a, S> Service<IncomingStream<'a, TcpListener>> for MakeGateway<S>
where
    S: Clone,
{
    type Response = WithPeer<S>;
    type Error = Infallible;
    type Future = std::future::Ready<Result<WithPeer<S>, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, stream: IncomingStream<'a, TcpListener>) -> Self::Future {
        std::future::ready(Ok(WithPeer {
            inner: self.inner.clone(),
            peer: *stream.remote_addr(),
        }))
    }
}

#[derive(Debug, Clone)]
pub struct WithPeer<S> {
    inner: S,
    peer: SocketAddr,
}

impl<S> Service<Request> for WithPeer<S>
where
    S: Service<Request, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        req.extensions_mut().insert(PeerAddr(self.peer));

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}
