//! Métricas Prometheus (spec §11.2).
//!
//! Superfície completa: os dois histogramas (`gateway_request_duration_seconds` e
//! `gateway_upstream_duration_seconds`) respondem "a lentidão é do gateway ou do
//! upstream?" — a diferença entre eles é o overhead real do gateway.
//!
//! **Regra de cardinalidade:** o label de rota é sempre o `route_id` vindo da
//! configuração, e portanto limitado. Nunca o path bruto — um label de path cru
//! transforma um scanner de diretórios em um incidente de memória no Prometheus.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{
    Encoder, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGaugeVec, Opts,
    Registry, TextEncoder,
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

/// Fonte dos estados de circuito para o coletor de `gateway_circuit_state`.
///
/// Implementada por `BreakerRegistry`. O desacoplamento evita que este módulo
/// dependa de `resilience` — mesmo padrão de `ReadinessCheck` em
/// `observability::health`, implementada por `AppState`.
pub trait CircuitStateSource: Send + Sync {
    /// `(upstream_id, código do estado)`, código conforme a convenção da spec:
    /// `0=closed 1=half_open 2=open`.
    fn circuit_states(&self) -> Vec<(Arc<str>, i64)>;
}

/// Fonte da idade do cache de JWKS para `gateway_jwks_cache_age_seconds`.
/// Implementada por `JwksCache`.
pub trait JwksAgeSource: Send + Sync {
    /// `None` quando nenhum fetch jamais teve sucesso — nesse caso a métrica não
    /// é emitida, porque "idade zero" seria lido como "cache fresco", o oposto
    /// da realidade.
    fn jwks_age_seconds(&self) -> Option<f64>;
}

#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    requests_total: IntCounterVec,
    request_duration: HistogramVec,
    upstream_duration: HistogramVec,
    upstream_inflight: IntGaugeVec,
    ratelimit_degraded: IntCounter,
    ratelimit_decisions: IntCounterVec,
    circuit_transitions: IntCounterVec,
    retries_total: IntCounterVec,
    auth_failures: IntCounterVec,
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

        let upstream_duration = HistogramVec::new(
            HistogramOpts::new(
                "gateway_upstream_duration_seconds",
                "Duração só da conversa com o upstream, sem o overhead do gateway",
            ),
            &["upstream"],
        )?;

        let upstream_inflight = IntGaugeVec::new(
            Opts::new(
                "gateway_upstream_inflight",
                "Requisições em voo para o upstream neste instante",
            ),
            &["upstream"],
        )?;

        let ratelimit_degraded = IntCounter::new(
            "gateway_ratelimit_degraded_total",
            "Requisições liberadas sem decisão de rate limit por indisponibilidade do store",
        )?;

        let ratelimit_decisions = IntCounterVec::new(
            Opts::new(
                "gateway_ratelimit_decisions_total",
                "Decisões de rate limit tomadas pelo store",
            ),
            &["route", "key_kind", "decision"],
        )?;

        let circuit_transitions = IntCounterVec::new(
            Opts::new(
                "gateway_circuit_transitions_total",
                "Transições de estado do circuit breaker, por upstream",
            ),
            &["upstream", "to"],
        )?;

        let retries_total = IntCounterVec::new(
            Opts::new(
                "gateway_retries_total",
                "Tentativas de repetição além da primeira, por rota",
            ),
            &["route", "result"],
        )?;

        let auth_failures = IntCounterVec::new(
            Opts::new(
                "gateway_auth_failures_total",
                "Falhas de autenticação, por motivo",
            ),
            &["reason"],
        )?;

        registry.register(Box::new(requests_total.clone()))?;
        registry.register(Box::new(request_duration.clone()))?;
        registry.register(Box::new(upstream_duration.clone()))?;
        registry.register(Box::new(upstream_inflight.clone()))?;
        registry.register(Box::new(ratelimit_degraded.clone()))?;
        registry.register(Box::new(ratelimit_decisions.clone()))?;
        registry.register(Box::new(circuit_transitions.clone()))?;
        registry.register(Box::new(retries_total.clone()))?;
        registry.register(Box::new(auth_failures.clone()))?;

        Ok(Self {
            registry,
            requests_total,
            request_duration,
            upstream_duration,
            upstream_inflight,
            ratelimit_degraded,
            ratelimit_decisions,
            circuit_transitions,
            retries_total,
            auth_failures,
        })
    }

    pub fn ratelimit_degraded(&self) {
        self.ratelimit_degraded.inc();
    }

    pub fn record_ratelimit_decision(&self, route: &str, key_kind: &str, allowed: bool) {
        let decision = if allowed { "allowed" } else { "rejected" };
        self.ratelimit_decisions
            .with_label_values(&[route, key_kind, decision])
            .inc();
    }

    /// Chamada pelo `CircuitBreaker` a cada transição real de estado (não a cada
    /// avaliação — `transition()` já filtra o caso "mudou para o mesmo estado").
    pub fn record_circuit_transition(&self, upstream: &str, to: &str) {
        self.circuit_transitions
            .with_label_values(&[upstream, to])
            .inc();
    }

    /// Uma tentativa além da primeira. `result` é `"success"` quando essa
    /// tentativa específica teve sucesso, `"failure"` quando também falhou —
    /// múltiplas falhas na mesma requisição incrementam múltiplas vezes, o que é
    /// o ponto: o contador mede tentativas gastas, não requisições.
    pub fn record_retry(&self, route: &str, result: &str) {
        self.retries_total.with_label_values(&[route, result]).inc();
    }

    pub fn record_auth_failure(&self, reason: &str) {
        self.auth_failures.with_label_values(&[reason]).inc();
    }

    pub fn observe_upstream_duration(&self, upstream: &str, secs: f64) {
        self.upstream_duration
            .with_label_values(&[upstream])
            .observe(secs);
    }

    /// Marca uma requisição como em voo para `upstream`. O guarda decrementa
    /// sozinho quando sai de escopo — inclusive no caminho de erro, sem exigir
    /// que cada `return` intermediário lembre de decrementar.
    pub fn track_upstream_inflight(&self, upstream: &str) -> InflightGuard {
        let gauge = self.upstream_inflight.with_label_values(&[upstream]);
        gauge.inc();
        InflightGuard(gauge)
    }

    /// Registra o coletor de `gateway_circuit_state`. Chamado uma vez no
    /// bootstrap, com o `BreakerRegistry` já construído.
    pub fn register_circuit_states(
        &self,
        source: Arc<dyn CircuitStateSource>,
    ) -> Result<(), prometheus::Error> {
        self.registry
            .register(Box::new(CircuitStateCollector::new(source)?))
    }

    /// Registra o coletor de `gateway_jwks_cache_age_seconds`. Só faz sentido
    /// chamar quando há autenticação configurada — sem `auth`, não há cache.
    pub fn register_jwks_age(
        &self,
        source: Arc<dyn JwksAgeSource>,
    ) -> Result<(), prometheus::Error> {
        self.registry
            .register(Box::new(JwksAgeCollector::new(source)?))
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

/// RAII para `gateway_upstream_inflight`. Decrementa no `Drop`, então cobre
/// tanto a resposta bem-sucedida quanto o erro de conexão sem duplicar lógica.
pub struct InflightGuard(prometheus::IntGauge);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.dec();
    }
}

/// Coletor de `gateway_circuit_state`, computado a cada scrape.
///
/// Um circuito recém-criado (upstream que ainda não recebeu tráfego) não tem
/// entrada no registro de breakers, e portanto não aparece até a primeira
/// requisição — não há "estado padrão" a inventar para um upstream ocioso.
struct CircuitStateCollector {
    gauge: GaugeVec,
    source: Arc<dyn CircuitStateSource>,
}

impl CircuitStateCollector {
    fn new(source: Arc<dyn CircuitStateSource>) -> Result<Self, prometheus::Error> {
        let gauge = GaugeVec::new(
            Opts::new(
                "gateway_circuit_state",
                "Estado do circuit breaker por upstream (0=closed 1=half_open 2=open)",
            ),
            &["upstream"],
        )?;
        Ok(Self { gauge, source })
    }
}

impl Collector for CircuitStateCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.gauge.desc()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        for (upstream, code) in self.source.circuit_states() {
            self.gauge.with_label_values(&[&upstream]).set(code as f64);
        }
        self.gauge.collect()
    }
}

/// Coletor de `gateway_jwks_cache_age_seconds`, computado a cada scrape.
struct JwksAgeCollector {
    gauge: prometheus::Gauge,
    source: Arc<dyn JwksAgeSource>,
}

impl JwksAgeCollector {
    fn new(source: Arc<dyn JwksAgeSource>) -> Result<Self, prometheus::Error> {
        let gauge = prometheus::Gauge::new(
            "gateway_jwks_cache_age_seconds",
            "Idade do snapshot corrente de JWKS, em segundos desde o último fetch bem-sucedido",
        )?;
        Ok(Self { gauge, source })
    }
}

impl Collector for JwksAgeCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.gauge.desc()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        match self.source.jwks_age_seconds() {
            Some(age) => {
                self.gauge.set(age);
                self.gauge.collect()
            }
            // Sem fetch bem-sucedido ainda: nenhuma amostra, em vez de uma
            // idade inventada.
            None => Vec::new(),
        }
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
    fn render_expoe_o_subconjunto_da_fase_1() {
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
    fn render_expoe_a_superficie_da_fase_3() {
        let metrics = Metrics::new().unwrap();

        metrics.observe_upstream_duration("user-service", 0.02);
        metrics.record_ratelimit_decision("users", "ip", true);
        metrics.record_ratelimit_decision("users", "ip", false);
        metrics.record_circuit_transition("user-service", "open");
        metrics.record_retry("orders", "success");
        metrics.record_auth_failure("expired");

        let rendered = metrics.render();
        assert!(rendered.contains("gateway_upstream_duration_seconds"));
        assert!(rendered.contains(
            r#"gateway_ratelimit_decisions_total{decision="allowed",key_kind="ip",route="users"} 1"#
        ));
        assert!(rendered.contains(r#"gateway_ratelimit_decisions_total{decision="rejected",key_kind="ip",route="users"} 1"#));
        assert!(
            rendered.contains(
                r#"gateway_circuit_transitions_total{to="open",upstream="user-service"} 1"#
            )
        );
        assert!(rendered.contains(r#"gateway_retries_total{result="success",route="orders"} 1"#));
        assert!(rendered.contains(r#"gateway_auth_failures_total{reason="expired"} 1"#));
    }

    #[test]
    fn inflight_guard_incrementa_na_criacao_e_decrementa_ao_sair_de_escopo() {
        let metrics = Metrics::new().unwrap();

        {
            let _guard = metrics.track_upstream_inflight("user-service");
            assert!(
                metrics
                    .render()
                    .contains(r#"gateway_upstream_inflight{upstream="user-service"} 1"#)
            );
        }

        assert!(
            metrics
                .render()
                .contains(r#"gateway_upstream_inflight{upstream="user-service"} 0"#)
        );
    }

    struct FakeCircuitStates(Vec<(Arc<str>, i64)>);
    impl CircuitStateSource for FakeCircuitStates {
        fn circuit_states(&self) -> Vec<(Arc<str>, i64)> {
            self.0.clone()
        }
    }

    #[test]
    fn circuit_state_e_computado_a_cada_scrape() {
        let metrics = Metrics::new().unwrap();
        let source = Arc::new(FakeCircuitStates(vec![("user-service".into(), 2)]));
        metrics.register_circuit_states(source).unwrap();

        assert!(
            metrics
                .render()
                .contains(r#"gateway_circuit_state{upstream="user-service"} 2"#)
        );
    }

    struct FakeJwksAge(Option<f64>);
    impl JwksAgeSource for FakeJwksAge {
        fn jwks_age_seconds(&self) -> Option<f64> {
            self.0
        }
    }

    #[test]
    fn jwks_age_ausente_nao_emite_a_metrica() {
        let metrics = Metrics::new().unwrap();
        metrics
            .register_jwks_age(Arc::new(FakeJwksAge(None)))
            .unwrap();

        assert!(!metrics.render().contains("gateway_jwks_cache_age_seconds"));
    }

    #[test]
    fn jwks_age_presente_emite_a_metrica() {
        let metrics = Metrics::new().unwrap();
        metrics
            .register_jwks_age(Arc::new(FakeJwksAge(Some(12.5))))
            .unwrap();

        assert!(
            metrics
                .render()
                .contains("gateway_jwks_cache_age_seconds 12.5")
        );
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
