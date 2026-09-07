//! Utilidades de teste compartilhadas.
//!
//! Não está atrás de `#[cfg(test)]` de propósito: os testes de integração
//! consomem a crate como biblioteca externa e precisam dos mesmos construtores.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use http::uri::Scheme;
use tower::Service;

use crate::config::{KeyKind, ResiliencePolicy};
use crate::routing::runtime::{AuthPolicy, LimitSpec, RouteRuntime, UpstreamRuntime};
use crate::{BoxFuture, Request, Response};

/// Serviço interno de mentira: devolve 200 com os headers **da requisição**.
///
/// É o que permite testar um layer isoladamente e ainda assim afirmar o que teria
/// chegado ao upstream — headers injetados, headers removidos — sem levantar
/// router nem servidor.
#[derive(Debug, Clone, Default)]
pub struct EchoService;

impl Service<Request> for EchoService {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<Result<Response, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let path = req.uri().to_string();
        let headers = req.headers().clone();

        Box::pin(async move {
            let mut response = http::Response::new(Body::from(path));
            *response.headers_mut() = headers;
            Ok(response)
        })
    }
}

pub fn upstream(id: &str, authority: &str) -> Arc<UpstreamRuntime> {
    Arc::new(UpstreamRuntime {
        id: Arc::from(id),
        scheme: Scheme::HTTP,
        authority: authority.parse().expect("autoridade válida"),
        base_path: String::new(),
        resilience: ResiliencePolicy::default(),
    })
}

/// Construtor de `RouteRuntime` para testes de layer.
pub struct RouteBuilder {
    route: RouteRuntime,
}

impl RouteBuilder {
    pub fn new(id: &str, prefix: &str) -> Self {
        Self {
            route: RouteRuntime {
                id: Arc::from(id),
                prefix: prefix.trim_end_matches('/').to_string(),
                strip_prefix: false,
                upstream: upstream("svc", "svc:8080"),
                auth: AuthPolicy::default(),
                limits: Vec::new(),
                resilience: ResiliencePolicy::default(),
            },
        }
    }

    pub fn auth(mut self, required: bool, scopes: &[&str]) -> Self {
        self.route.auth = AuthPolicy {
            required,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        };
        self
    }

    pub fn limit(mut self, key: KeyKind, capacity: u32, refill_per_sec: f64) -> Self {
        self.route.limits.push(LimitSpec {
            key,
            capacity,
            refill_per_sec,
        });
        self
    }

    pub fn strip_prefix(mut self, strip: bool) -> Self {
        self.route.strip_prefix = strip;
        self
    }

    pub fn build(self) -> Arc<RouteRuntime> {
        Arc::new(self.route)
    }
}

/// Requisição com a rota já injetada nas extensions, como se `route_resolve`
/// tivesse rodado.
pub fn request_with_route(uri: &str, route: Arc<RouteRuntime>) -> Request {
    let mut req = http::Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("requisição válida");

    req.extensions_mut().insert(route);
    req
}
