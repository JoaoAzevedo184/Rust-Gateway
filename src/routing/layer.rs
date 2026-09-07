//! Resolução de rota: injeta o `RouteRuntime` nas extensions, ou responde 404.

use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::watch;
use tower::{Layer, Service};

use crate::error::GatewayError;
use crate::observability::metrics::{self, Outcome};
use crate::observability::tracing::record_route;
use crate::routing::runtime::RouteRuntime;
use crate::routing::table::RouterTable;
use crate::{BoxFuture, Request, Response};

#[derive(Debug, Clone)]
pub struct RouteResolveLayer {
    table: watch::Receiver<Arc<RouterTable>>,
}

impl RouteResolveLayer {
    pub fn new(table: watch::Receiver<Arc<RouterTable>>) -> Self {
        Self { table }
    }
}

impl<S> Layer<S> for RouteResolveLayer {
    type Service = RouteResolve<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RouteResolve {
            inner,
            table: self.table.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteResolve<S> {
    inner: S,
    table: watch::Receiver<Arc<RouterTable>>,
}

impl<S> Service<Request> for RouteResolve<S>
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

    fn call(&mut self, mut req: Request) -> Self::Future {
        // Uma leitura do snapshot por requisição. O caminho da requisição nunca
        // segura o lock do canal enquanto trabalha.
        let table = self.table.borrow().clone();

        let Some(route) = table.resolve(req.uri().path()).cloned() else {
            metrics::mark(req.extensions(), Outcome::NoRoute);
            let response = GatewayError::not_found().into_response_for(req.extensions());
            return Box::pin(async move { Ok(response) });
        };

        record_route(&route.id);
        if let Some(slot) = req.extensions().get::<metrics::MetricsSlot>() {
            slot.set_route(route.id.clone());
        }
        req.extensions_mut().insert(route);

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move { inner.call(req).await })
    }
}

/// Rota resolvida para esta requisição, quando houver.
pub fn route_of(extensions: &http::Extensions) -> Option<&Arc<RouteRuntime>> {
    extensions.get::<Arc<RouteRuntime>>()
}
