//! Logs estruturados e span por requisição (spec §11.4).

use std::task::{Context, Poll};

use opentelemetry_sdk::trace::SdkTracerProvider;
use tower::{Layer, Service};
use tracing::field::Empty;
use tracing::{Instrument, Span};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::observability::correlation::RequestId;
use crate::observability::otel;
use crate::{BoxFuture, Request, Response};

/// Monta o subscriber e devolve o `SdkTracerProvider`, para que o chamador possa
/// encerrá-lo de forma graciosa no desligamento — sem isso, spans em lote ainda
/// no buffer no momento do `SIGTERM` nunca chegam ao coletor.
///
/// Saída JSON. `RUST_LOG` continua controlando o filtro; sem ele, `info`.
/// `otlp_endpoint` liga a exportação; ausente, os spans continuam sendo criados
/// e propagados, só não saem do processo.
pub fn init(otlp_endpoint: Option<&str>) -> SdkTracerProvider {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rust_gateway=info"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false);

    let provider = otel::install(otlp_endpoint);
    let otel_layer = tracing_opentelemetry::layer().with_tracer(otel::tracer(&provider));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    provider
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

    fn call(&mut self, mut req: Request) -> Self::Future {
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
            trace_id = Empty,
        );

        // Aceita o `traceparent` recebido como pai do span desta requisição, e
        // escreve o `traceparent` de saída — com o span do gateway como
        // parent-id — antes de a requisição seguir para o próximo layer. Os dois
        // lados da mesma operação: sem o primeiro, o gateway sempre origina um
        // trace novo; sem o segundo, o upstream nunca sabe que está continuando um.
        otel::accept_incoming(&span, req.headers());
        otel::inject_outgoing(&span, req.headers_mut());
        if let Some(trace_id) = otel::trace_id(&span) {
            span.record("trace_id", trace_id);
        }

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
