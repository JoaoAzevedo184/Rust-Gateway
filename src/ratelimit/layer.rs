//! Layer de rate limit: monta os buckets da rota e aplica a decisão (spec §9.5).

use std::sync::Arc;
use std::task::{Context, Poll};

use http::HeaderValue;
use tower::{Layer, Service};

use crate::auth::Identity;
use crate::config::KeyKind;
use crate::error::GatewayError;
use crate::observability::metrics::{self, Metrics, Outcome};
use crate::peer::{PeerAddr, TrustedProxies};
use crate::ratelimit::limiter::RateLimiter;
use crate::ratelimit::store::{BucketRequest, Decision, bucket_key};
use crate::routing::layer::route_of;
use crate::routing::runtime::RouteRuntime;
use crate::{BoxFuture, Request, Response};

#[derive(Debug, Clone)]
pub struct RateLimitLayer {
    limiter: Arc<RateLimiter>,
    trusted: TrustedProxies,
    metrics: Arc<Metrics>,
}

impl RateLimitLayer {
    pub fn new(limiter: Arc<RateLimiter>, trusted: TrustedProxies, metrics: Arc<Metrics>) -> Self {
        Self {
            limiter,
            trusted,
            metrics,
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            limiter: self.limiter.clone(),
            trusted: self.trusted.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RateLimitService<S> {
    inner: S,
    limiter: Arc<RateLimiter>,
    trusted: TrustedProxies,
    metrics: Arc<Metrics>,
}

impl<S> Service<Request> for RateLimitService<S>
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

        let route = route_of(req.extensions()).cloned();
        let buckets = match &route {
            Some(route) if !route.limits.is_empty() => {
                build_buckets(route, req.extensions(), req.headers(), &self.trusted)
            }
            _ => Vec::new(),
        };

        if buckets.is_empty() {
            return Box::pin(async move { inner.call(req).await });
        }

        let limiter = self.limiter.clone();
        let metrics = self.metrics.clone();
        // A rota existe: `buckets` só é não-vazio quando `route_of` casou acima.
        let route_id = route.expect("rota presente quando há buckets").id.clone();

        Box::pin(async move {
            let Some(decision) = limiter.check(&buckets).await else {
                // Degradado: passa sem decisão e sem headers, porque não há
                // número honesto para anunciar.
                return inner.call(req).await;
            };

            // Todos os buckets da rota se avaliam juntos (tudo-ou-nada), então o
            // desfecho é o mesmo para cada `key_kind` presente — mas o label
            // continua separado por kind, para diferenciar "quantas decisões
            // envolveram sub" de "quantas envolveram ip".
            for kind in distinct_kinds(&buckets) {
                metrics.record_ratelimit_decision(&route_id, kind, decision.allowed);
            }

            if !decision.allowed {
                metrics::mark(req.extensions(), Outcome::RejectedRateLimit);
                let mut response = GatewayError::rate_limited(decision.retry_after)
                    .into_response_for(req.extensions());
                apply_headers(response.headers_mut(), &decision);
                return Ok(response);
            }

            let mut response = inner.call(req).await?;
            apply_headers(response.headers_mut(), &decision);
            Ok(response)
        })
    }
}

/// `key_kind`s distintos entre os buckets da rota, sem repetir. A lista é
/// sempre pequena (um ou dois limites por rota), então uma varredura linear é
/// mais simples e tão barata quanto um `HashSet` aqui.
fn distinct_kinds(buckets: &[BucketRequest]) -> Vec<&'static str> {
    let mut kinds: Vec<&'static str> = Vec::new();
    for bucket in buckets {
        let kind = bucket.kind.as_str();
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Monta um `BucketRequest` por limite configurado na rota.
///
/// Um limite cuja chave não pode ser resolvida é omitido em vez de virar um
/// bucket com valor genérico: um bucket `sub:desconhecido` compartilhado entre
/// todos os anônimos limitaria o conjunto errado de requisições.
fn build_buckets(
    route: &Arc<RouteRuntime>,
    extensions: &http::Extensions,
    headers: &http::HeaderMap,
    trusted: &TrustedProxies,
) -> Vec<BucketRequest> {
    let identity = extensions.get::<Identity>();
    let peer = extensions.get::<PeerAddr>().map(|peer| peer.0.ip());

    route
        .limits
        .iter()
        .filter_map(|limit| {
            let value = match limit.key {
                KeyKind::Sub => identity.map(|identity| identity.sub.to_string())?,
                KeyKind::Ip => trusted.client_ip(headers, peer?).to_string(),
            };

            Some(BucketRequest {
                key: bucket_key(&route.id, limit.key, &value),
                kind: limit.key,
                capacity: limit.capacity,
                refill_per_sec: limit.refill_per_sec,
                cost: 1,
            })
        })
        .collect()
}

/// `X-RateLimit-*` em toda resposta de rota limitada, permitida ou não.
fn apply_headers(headers: &mut http::HeaderMap, decision: &Decision) {
    if let Ok(value) = HeaderValue::from_str(&decision.limit.to_string()) {
        headers.insert("x-ratelimit-limit", value);
    }
    if let Ok(value) = HeaderValue::from_str(&decision.remaining.to_string()) {
        headers.insert("x-ratelimit-remaining", value);
    }
}
