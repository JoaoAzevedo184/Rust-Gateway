//! Logs estruturados e span por requisição (spec §11.4).

use std::task::{Context, Poll};

use tower::{Layer, Service};
use tracing::field::Empty;
use tracing::{Instrument, Span};
use tracing_subscriber::EnvFilter;

use crate::observability::correlation::RequestId;
use crate::{BoxFuture, Request, Response};

/// Saída JSON. `RUST_LOG` continua controlando o filtro; sem ele, `info`.
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rust_gateway=info"));

    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_env_filter(filter)
        .init();
}

/// Anota a rota resolvida no span corrente. Chamado pelo layer de resolução.
pub fn record_route(route_id: &str) {
    Span::current().record("route_id", route_id);
}

/// Anota a identidade no span corrente. Chamado pelo layer de auth.
pub fn record_sub(sub: &str) {
    Span::current().record("sub", sub);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SpanLayer;

impl<S> Layer<S> for SpanLayer {
    type Service = SpanService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        SpanService { inner }
    }
}

#[derive(Debug, Clone)]
pub struct SpanService<S> {
    inner: S,
}

impl<S> Service<Request> for SpanService<S>
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
        let request_id = req
            .extensions()
            .get::<RequestId>()
            .map(|id| id.as_str().to_string())
            .unwrap_or_default();

        // `route_id`, `sub` e `status` só são conhecidos mais adiante na pilha;
        // declará-los vazios aqui é o que permite gravá-los depois no mesmo span.
        let span = tracing::info_span!(
            "request",
            request_id = %request_id,
            method = %req.method(),
            path = %req.uri().path(),
            route_id = Empty,
            sub = Empty,
            status = Empty,
        );

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let recorder = span.clone();

        Box::pin(async move {
            let response = inner.call(req).instrument(span).await?;
            recorder.record("status", response.status().as_u16());
            Ok(response)
        })
    }
}
