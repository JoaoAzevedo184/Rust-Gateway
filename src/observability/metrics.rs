//! Métricas Prometheus (spec §11.2).
//!
//! Fase 1 expõe o subconjunto mínimo: `gateway_requests_total`,
//! `gateway_request_duration_seconds` e `gateway_ratelimit_degraded_total`.
//!
//! **Regra de cardinalidade:** o label de rota é sempre o `route_id` vindo da
//! configuração, e portanto limitado. Nunca o path bruto — um label de path cru
//! transforma um scanner de diretórios em um incidente de memória no Prometheus.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts, Registry, TextEncoder,
};
use tower::{Layer, Service};

use crate::{BoxFuture, Request, Response};

/// Label usado quando nenhuma rota casou. Constante, para não abrir cardinalidade.
pub const UNMATCHED: &str = "unmatched";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    NoRoute,
    RejectedAuth,
    RejectedRateLimit,
    UpstreamError,
    CircuitOpen,
    Timeout,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::NoRoute => "no_route",
            Outcome::RejectedAuth => "rejected_auth",
            Outcome::RejectedRateLimit => "rejected_ratelimit",
            Outcome::UpstreamError => "upstream_error",
            Outcome::CircuitOpen => "circuit_open",
            Outcome::Timeout => "timeout",
        }
    }
}

#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    requests_total: IntCounterVec,
    request_duration: HistogramVec,
    ratelimit_degraded: IntCounter,
}

impl Metrics {
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new(
                "gateway_requests_total",
                "Requisições atendidas pelo gateway",
            ),
            &["route", "method", "status", "outcome"],
        )?;

        let request_duration = HistogramVec::new(
            HistogramOpts::new(
                "gateway_request_duration_seconds",
                "Duração total da requisição, borda a borda",
            ),
            &["route"],
        )?;

        let ratelimit_degraded = IntCounter::new(
            "gateway_ratelimit_degraded_total",
            "Requisições liberadas sem decisão de rate limit por indisponibilidade do store",
        )?;

        registry.register(Box::new(requests_total.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        registry.register(Box::new(ratelimit_degraded.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            request_duration,
            ratelimit_degraded,
        })
    }

    pub fn ratelimit_degraded(&self) {
        self.ratelimit_degraded.inc();
    }

    pub fn render(&self) -> String {
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        if encoder
            .encode(&self.registry.gather(), &mut buffer)
            .is_err()
        {
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }
}

/// Rótulos que os layers internos preenchem durante o processamento.
///
/// O layer de métricas é global e roda **fora** da resolução de rota, então não
/// conhece o `route_id` no momento em que cria o cronômetro. Os layers internos
/// escrevem aqui pelo `Arc` que encontram nas extensions da requisição.
#[derive(Debug, Default)]
struct Labels {
    route: Option<Arc<str>>,
    outcome: Option<Outcome>,
}

#[derive(Debug, Clone, Default)]
pub struct MetricsSlot(Arc<Mutex<Labels>>);

impl MetricsSlot {
    pub fn set_route(&self, route: Arc<str>) {
        if let Ok(mut labels) = self.0.lock() {
            labels.route = Some(route);
        }
    }

    pub fn set_outcome(&self, outcome: Outcome) {
        if let Ok(mut labels) = self.0.lock() {
            labels.outcome = Some(outcome);
        }
    }

    fn take(&self) -> (String, Option<Outcome>) {
        match self.0.lock() {
            Ok(labels) => (
                labels.route.as_deref().unwrap_or(UNMATCHED).to_string(),
                labels.outcome,
            ),
            Err(_) => (UNMATCHED.to_string(), None),
        }
    }
}

/// Marca o resultado no slot da requisição, se houver um.
pub fn mark(extensions: &http::Extensions, outcome: Outcome) {
    if let Some(slot) = extensions.get::<MetricsSlot>() {
        slot.set_outcome(outcome);
    }
}

#[derive(Debug, Clone)]
pub struct MetricsLayer {
    metrics: Arc<Metrics>,
}

impl MetricsLayer {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self { metrics }
    }
}

impl<S> Layer<S> for MetricsLayer {
    type Service = MetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MetricsService {
            inner,
            metrics: self.metrics.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MetricsService<S> {
    inner: S,
    metrics: Arc<Metrics>,
}

impl<S> Service<Request> for MetricsService<S>
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
        let slot = MetricsSlot::default();
        req.extensions_mut().insert(slot.clone());

        let method = req.method().as_str().to_string();
        let started = Instant::now();
        let metrics = self.metrics.clone();

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            let response = inner.call(req).await?;
            let elapsed = started.elapsed().as_secs_f64();

            let (route, outcome) = slot.take();
            let status = response.status();
            let outcome = outcome.unwrap_or(if status.is_server_error() {
                Outcome::UpstreamError
            } else {
                Outcome::Ok
            });

            metrics
                .requests_total
                .with_label_values(&[&route, &method, status.as_str(), outcome.as_str()])
                .inc();
            metrics
                .request_duration
                .with_label_values(&[&route])
                .observe(elapsed);

            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_expoe_o_subconjunto_minimo_da_fase_1() {
        let metrics = Metrics::new().unwrap();
        metrics.ratelimit_degraded();
        metrics
            .requests_total
            .with_label_values(&["users", "GET", "200", "ok"])
            .inc();
        metrics
            .request_duration
            .with_label_values(&["users"])
            .observe(0.01);

        let rendered = metrics.render();
        assert!(rendered.contains("gateway_requests_total"));
        assert!(rendered.contains("gateway_request_duration_seconds"));
        assert!(rendered.contains("gateway_ratelimit_degraded_total 1"));
        assert!(rendered.contains(r#"route="users""#));
    }

    #[test]
    fn slot_sem_rota_cai_no_label_constante() {
        let slot = MetricsSlot::default();
        assert_eq!(slot.take().0, UNMATCHED);

        slot.set_route(Arc::from("users"));
        slot.set_outcome(Outcome::RejectedAuth);
        assert_eq!(
            slot.take(),
            ("users".to_string(), Some(Outcome::RejectedAuth))
        );
    }
}
