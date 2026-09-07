//! Propagação de contexto de trace W3C e exportação OTLP (spec §11.4).
//!
//! A propagação roda sempre: o gateway aceita o `traceparent` que chega e gera um
//! span cujo id vira o parent-id do `traceparent` que segue para o upstream — é o
//! que faz um trace atravessar Rust → Java → Python em vez de virar três traces
//! desconexos. A exportação para um coletor OTLP é só o destino desses spans, e é
//! opcional: sem `tracing.otlp_endpoint` configurado, os spans continuam sendo
//! criados e propagados, só não saem do processo.

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
use tracing_opentelemetry::OpenTelemetrySpanExt;

const SERVICE_NAME: &str = "rust-gateway";

/// Instala o propagador W3C e o provider global, e devolve o provider para que o
/// chamador possa encerrá-lo de forma graciosa no desligamento.
///
/// Chamada uma única vez, no bootstrap, antes de montar o subscriber de tracing.
pub fn install(otlp_endpoint: Option<&str>) -> SdkTracerProvider {
    // O propagador é global e independente do provider: mesmo sem exportação,
    // aceitar e repassar o `traceparent` funciona.
    global::set_text_map_propagator(TraceContextPropagator::new());

    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    let mut builder = SdkTracerProvider::builder().with_resource(resource);

    if let Some(endpoint) = otlp_endpoint {
        match opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
        {
            Ok(exporter) => {
                builder = builder.with_batch_exporter(exporter);
            }
            Err(err) => {
                // Endpoint já foi validado como URL no startup (spec §5.3); um
                // erro aqui é do exportador em si, não da configuração. Os spans
                // continuam sendo criados — só não saem do processo.
                eprintln!(
                    "aviso: não foi possível montar o exportador OTLP para {endpoint:?}: {err}"
                );
            }
        }
    }

    let provider = builder.build();
    global::set_tracer_provider(provider.clone());
    provider
}

/// Tracer usado pelo layer `tracing-opentelemetry`.
///
/// Extraído do provider concreto (`SdkTracerProvider`), não do registro global
/// dinâmico: `opentelemetry::global::tracer_provider()` devolveria um
/// `BoxedTracer` de tipo apagado, e `tracing-opentelemetry` quer o tipo concreto.
pub fn tracer(provider: &SdkTracerProvider) -> SdkTracer {
    provider.tracer(SERVICE_NAME)
}

/// Aceita o `traceparent` recebido como pai remoto do span corrente.
///
/// Sem um `traceparent` válido, o SDK simplesmente origina um trace novo — o
/// comportamento correto para o primeiro salto de uma cadeia sem tracing correndo
/// ainda.
pub fn accept_incoming(span: &tracing::Span, headers: &http::HeaderMap) {
    let parent_cx =
        global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)));
    let _ = span.set_parent(parent_cx);
}

/// Escreve o `traceparent` (e `tracestate`, se houver) do span corrente nos
/// headers de saída, para que o upstream continue o mesmo trace.
///
/// Chamar isto finaliza o span — atribui seu trace/span id via o gerador do SDK —
/// se ainda não tiver sido finalizado; é seguro chamar antes de instrumentar o
/// futuro, no mesmo lugar em que `accept_incoming` rodou.
pub fn inject_outgoing(span: &tracing::Span, headers: &mut http::HeaderMap) {
    let cx = span.context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(headers));
    });
}

/// Id do trace do span corrente, formatado em hexadecimal — o mesmo valor que
/// aparece em um coletor OTLP, útil para pular do log direto para lá.
pub fn trace_id(span: &tracing::Span) -> Option<String> {
    use opentelemetry::trace::TraceContextExt;

    let cx = span.context();
    let span_context = cx.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `accept_incoming` + `inject_outgoing` sem um layer OTel registrado no
    /// subscriber corrente não devem entrar em pânico — é o caminho que os
    /// testes de integração do resto da suíte exercitam, sem instalar um
    /// subscriber de tracing próprio.
    #[test]
    fn funciona_mesmo_sem_layer_otel_registrado() {
        let span = tracing::info_span!("teste");
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );

        accept_incoming(&span, &headers);

        let mut outgoing = http::HeaderMap::new();
        inject_outgoing(&span, &mut outgoing);
        // Sem layer OTel, o contexto é vazio e nada é injetado — mas a chamada
        // não pode falhar.
        let _ = outgoing.get("traceparent");
    }
}
