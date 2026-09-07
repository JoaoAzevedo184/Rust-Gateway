//! Circuit breaker por upstream (spec §10.1, D8).
//!
//! O estado é **local à réplica**: é uma observação sobre a conexão *desta*
//! réplica com *aquele* upstream. Compartilhá-lo faria o problema de rede de uma
//! réplica abrir o circuito para todas, e colocaria o Redis no caminho crítico.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::clock::Clock;
use crate::config::BreakerPolicy;
use crate::observability::metrics::Metrics;

/// Divisões da janela deslizante. Mais divisões dão uma janela mais suave ao
/// custo de memória; dez é o suficiente para que o descarte do passado não
/// aconteça em saltos perceptíveis.
const BUCKETS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitState {
    pub fn as_str(self) -> &'static str {
        match self {
            CircuitState::Closed => "closed",
            CircuitState::Open => "open",
            CircuitState::HalfOpen => "half_open",
        }
    }

    /// Codificação numérica de `gateway_circuit_state` (spec §11.2): `0=closed
    /// 1=half_open 2=open`. Uma métrica não carrega string; o texto vive no log
    /// de transição, o número vive aqui.
    pub fn as_code(self) -> i64 {
        match self {
            CircuitState::Closed => 0,
            CircuitState::HalfOpen => 1,
            CircuitState::Open => 2,
        }
    }
}

/// Autorização para tentar. Carrega a informação de que a tentativa é uma sonda,
/// porque o resultado de uma sonda decide o estado do circuito, e o de uma
/// tentativa comum apenas alimenta a janela.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permit {
    Normal,
    Probe,
}

#[derive(Debug, Default, Clone, Copy)]
struct Bucket {
    epoch: u64,
    success: u32,
    failure: u32,
}

/// Janela deslizante por tempo, em divisões de largura fixa.
#[derive(Debug)]
struct SlidingWindow {
    buckets: [Bucket; BUCKETS],
    bucket_ms: u64,
}

impl SlidingWindow {
    fn new(window: Duration) -> Self {
        let bucket_ms = (window.as_millis() as u64 / BUCKETS as u64).max(1);
        Self {
            buckets: [Bucket::default(); BUCKETS],
            bucket_ms,
        }
    }

    fn epoch(&self, now_ms: u64) -> u64 {
        now_ms / self.bucket_ms
    }

    fn record(&mut self, now_ms: u64, success: bool) {
        let epoch = self.epoch(now_ms);
        let slot = (epoch % BUCKETS as u64) as usize;

        // A divisão reaproveitada pertence a uma volta anterior da janela: zerar
        // é o que faz o passado sair sem varredura periódica.
        if self.buckets[slot].epoch != epoch {
            self.buckets[slot] = Bucket {
                epoch,
                success: 0,
                failure: 0,
            };
        }

        if success {
            self.buckets[slot].success += 1;
        } else {
            self.buckets[slot].failure += 1;
        }
    }

    /// `(total, falhas)` dentro da janela.
    fn totals(&self, now_ms: u64) -> (u32, u32) {
        let current = self.epoch(now_ms);
        let oldest = current.saturating_sub(BUCKETS as u64 - 1);

        self.buckets
            .iter()
            .filter(|bucket| bucket.epoch >= oldest && bucket.epoch <= current)
            .fold((0, 0), |(total, failures), bucket| {
                (
                    total + bucket.success + bucket.failure,
                    failures + bucket.failure,
                )
            })
    }

    fn reset(&mut self) {
        self.buckets = [Bucket::default(); BUCKETS];
    }
}

#[derive(Debug)]
struct Inner {
    state: CircuitState,
    /// Instante em que um circuito aberto passa a aceitar sondas.
    open_until_ms: u64,
    half_open_inflight: u32,
    window: SlidingWindow,
}

#[derive(Debug)]
pub struct CircuitBreaker {
    id: Arc<str>,
    policy: BreakerPolicy,
    clock: Arc<dyn Clock>,
    metrics: Arc<Metrics>,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    pub fn new(
        id: Arc<str>,
        policy: BreakerPolicy,
        clock: Arc<dyn Clock>,
        metrics: Arc<Metrics>,
    ) -> Self {
        let window = SlidingWindow::new(policy.window);
        Self {
            id,
            policy,
            clock,
            metrics,
            inner: Mutex::new(Inner {
                state: CircuitState::Closed,
                open_until_ms: 0,
                half_open_inflight: 0,
                window,
            }),
        }
    }

    pub fn state(&self) -> CircuitState {
        self.inner
            .lock()
            .map(|inner| inner.state)
            .unwrap_or(CircuitState::Closed)
    }

    /// Pede autorização para tentar. `Err(espera)` significa circuito aberto.
    pub fn admit(&self) -> Result<Permit, Duration> {
        let now_ms = self.clock.now_ms();
        let Ok(mut inner) = self.inner.lock() else {
            // Mutex envenenado por um pânico: o breaker deixa de proteger, mas
            // não deixa de servir. Falhar fechado aqui derrubaria o upstream inteiro.
            return Ok(Permit::Normal);
        };

        match inner.state {
            CircuitState::Closed => Ok(Permit::Normal),

            CircuitState::Open if now_ms >= inner.open_until_ms => {
                self.transition(&mut inner, CircuitState::HalfOpen);
                inner.half_open_inflight = 1;
                Ok(Permit::Probe)
            }

            CircuitState::Open => Err(Duration::from_millis(
                inner.open_until_ms.saturating_sub(now_ms),
            )),

            CircuitState::HalfOpen if inner.half_open_inflight < self.policy.half_open_probes => {
                inner.half_open_inflight += 1;
                Ok(Permit::Probe)
            }

            // Já há sondas em voo. Deixar mais passar transformaria a recuperação
            // gradual em uma segunda avalanche sobre um upstream convalescente.
            CircuitState::HalfOpen => Err(self.policy.open_for),
        }
    }

    pub fn record(&self, permit: Permit, success: bool) {
        let now_ms = self.clock.now_ms();
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };

        match permit {
            Permit::Normal => {
                inner.window.record(now_ms, success);

                if inner.state == CircuitState::Closed && self.should_open(&inner, now_ms) {
                    self.open(&mut inner, now_ms);
                }
            }

            Permit::Probe => {
                inner.half_open_inflight = inner.half_open_inflight.saturating_sub(1);

                if success {
                    inner.window.reset();
                    self.transition(&mut inner, CircuitState::Closed);
                } else {
                    self.open(&mut inner, now_ms);
                }
            }
        }
    }

    /// `min_requests` antes de qualquer avaliação: sem isso, a primeira
    /// requisição do dia falhando abre o circuito com 100% de taxa de erro.
    fn should_open(&self, inner: &Inner, now_ms: u64) -> bool {
        let (total, failures) = inner.window.totals(now_ms);

        total >= self.policy.min_requests
            && f64::from(failures) / f64::from(total) > self.policy.failure_ratio
    }

    fn open(&self, inner: &mut Inner, now_ms: u64) {
        inner.open_until_ms = now_ms + self.policy.open_for.as_millis() as u64;
        inner.half_open_inflight = 0;
        self.transition(inner, CircuitState::Open);
    }

    fn transition(&self, inner: &mut Inner, to: CircuitState) {
        if inner.state == to {
            return;
        }

        tracing::warn!(
            upstream = %self.id,
            de = inner.state.as_str(),
            para = to.as_str(),
            "circuito mudou de estado"
        );
        inner.state = to;
        self.metrics
            .record_circuit_transition(&self.id, to.as_str());
    }
}

/// Estado vivo, fora do snapshot de configuração (D3).
///
/// Se os breakers vivessem no snapshot, um reload de configuração zeraria o
/// circuit breaker no meio de um incidente — exatamente quando ele importa.
#[derive(Debug)]
pub struct BreakerRegistry {
    breakers: RwLock<HashMap<Arc<str>, Arc<CircuitBreaker>>>,
    clock: Arc<dyn Clock>,
    metrics: Arc<Metrics>,
}

impl BreakerRegistry {
    pub fn new(clock: Arc<dyn Clock>, metrics: Arc<Metrics>) -> Self {
        Self {
            breakers: RwLock::new(HashMap::new()),
            clock,
            metrics,
        }
    }

    /// Breaker do upstream, criado na primeira vez que o upstream recebe tráfego.
    pub fn get(&self, id: &Arc<str>, policy: &BreakerPolicy) -> Arc<CircuitBreaker> {
        if let Ok(breakers) = self.breakers.read()
            && let Some(breaker) = breakers.get(id)
        {
            return breaker.clone();
        }

        let Ok(mut breakers) = self.breakers.write() else {
            return Arc::new(CircuitBreaker::new(
                id.clone(),
                policy.clone(),
                self.clock.clone(),
                self.metrics.clone(),
            ));
        };

        breakers
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(CircuitBreaker::new(
                    id.clone(),
                    policy.clone(),
                    self.clock.clone(),
                    self.metrics.clone(),
                ))
            })
            .clone()
    }

    /// Estado corrente de cada upstream que já recebeu tráfego.
    pub fn states(&self) -> Vec<(Arc<str>, CircuitState)> {
        match self.breakers.read() {
            Ok(breakers) => breakers
                .iter()
                .map(|(id, b)| (id.clone(), b.state()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl crate::observability::metrics::CircuitStateSource for BreakerRegistry {
    fn circuit_states(&self) -> Vec<(Arc<str>, i64)> {
        self.states()
            .into_iter()
            .map(|(id, state)| (id, state.as_code()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;

    fn policy() -> BreakerPolicy {
        BreakerPolicy {
            failure_ratio: 0.5,
            min_requests: 4,
            window: Duration::from_secs(30),
            open_for: Duration::from_secs(15),
            half_open_probes: 1,
        }
    }

    fn breaker(clock: Arc<TestClock>) -> CircuitBreaker {
        CircuitBreaker::new(
            "svc".into(),
            policy(),
            clock,
            Arc::new(Metrics::new().unwrap()),
        )
    }

    fn registra(breaker: &CircuitBreaker, quantidade: usize, sucesso: bool) {
        for _ in 0..quantidade {
            let permit = breaker.admit().expect("circuito fechado");
            breaker.record(permit, sucesso);
        }
    }

    #[test]
    fn nao_abre_antes_de_min_requests() {
        let breaker = breaker(Arc::new(TestClock::default()));

        registra(&breaker, 3, false);

        assert_eq!(
            breaker.state(),
            CircuitState::Closed,
            "3 falhas ainda não somam min_requests"
        );
        assert!(breaker.admit().is_ok());
    }

    #[test]
    fn abre_quando_a_taxa_de_falha_passa_do_limite() {
        let breaker = breaker(Arc::new(TestClock::default()));

        registra(&breaker, 4, false);

        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(
            breaker.admit().is_err(),
            "circuito aberto recusa imediatamente"
        );
    }

    #[test]
    fn taxa_abaixo_do_limite_mantem_o_circuito_fechado() {
        let breaker = breaker(Arc::new(TestClock::default()));

        registra(&breaker, 3, true);
        registra(&breaker, 3, false);

        // 3 de 6 é exatamente 0.5, e o limite é "maior que".
        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn quatro_xx_nao_contam_como_falha() {
        let breaker = breaker(Arc::new(TestClock::default()));

        // O layer traduz 4xx em sucesso antes de chegar aqui; o teste afirma que
        // é isso que mantém o circuito fechado.
        registra(&breaker, 20, true);

        assert_eq!(breaker.state(), CircuitState::Closed);
    }

    #[test]
    fn aberto_vira_meio_aberto_depois_de_open_for() {
        let clock = Arc::new(TestClock::new(1_000_000));
        let breaker = breaker(clock.clone());

        registra(&breaker, 4, false);
        assert!(breaker.admit().is_err());

        clock.advance_ms(14_999);
        assert!(breaker.admit().is_err(), "ainda dentro de open_for");

        clock.advance_ms(2);
        let permit = breaker.admit().expect("passou open_for");
        assert_eq!(permit, Permit::Probe);
        assert_eq!(breaker.state(), CircuitState::HalfOpen);
    }

    #[test]
    fn meio_aberto_admite_no_maximo_half_open_probes() {
        let clock = Arc::new(TestClock::new(1_000_000));
        let breaker = breaker(clock.clone());

        registra(&breaker, 4, false);
        clock.advance_ms(15_001);

        assert!(breaker.admit().is_ok(), "a primeira sonda passa");
        assert!(
            breaker.admit().is_err(),
            "a segunda espera o resultado da primeira"
        );
    }

    #[test]
    fn sonda_bem_sucedida_fecha_o_circuito() {
        let clock = Arc::new(TestClock::new(1_000_000));
        let breaker = breaker(clock.clone());

        registra(&breaker, 4, false);
        clock.advance_ms(15_001);

        let permit = breaker.admit().unwrap();
        breaker.record(permit, true);

        assert_eq!(breaker.state(), CircuitState::Closed);
        assert!(breaker.admit().is_ok());
    }

    #[test]
    fn sonda_que_falha_reabre_o_circuito() {
        let clock = Arc::new(TestClock::new(1_000_000));
        let breaker = breaker(clock.clone());

        registra(&breaker, 4, false);
        clock.advance_ms(15_001);

        let permit = breaker.admit().unwrap();
        breaker.record(permit, false);

        assert_eq!(breaker.state(), CircuitState::Open);
        assert!(breaker.admit().is_err());

        clock.advance_ms(15_001);
        assert!(
            breaker.admit().is_ok(),
            "e conta open_for de novo a partir da reabertura"
        );
    }

    #[test]
    fn a_janela_esquece_falhas_antigas() {
        let clock = Arc::new(TestClock::new(1_000_000));
        let breaker = breaker(clock.clone());

        registra(&breaker, 3, false);
        assert_eq!(breaker.state(), CircuitState::Closed);

        // Passada a janela inteira, as falhas antigas saem da conta.
        clock.advance_ms(31_000);
        registra(&breaker, 3, false);

        assert_eq!(
            breaker.state(),
            CircuitState::Closed,
            "6 falhas somadas ao longo de duas janelas não podem abrir o circuito"
        );
    }

    #[test]
    fn o_registry_devolve_o_mesmo_breaker_para_o_mesmo_upstream() {
        let registry = BreakerRegistry::new(
            Arc::new(TestClock::default()),
            Arc::new(Metrics::new().unwrap()),
        );
        let id: Arc<str> = Arc::from("user-service");

        let um = registry.get(&id, &policy());
        let outro = registry.get(&id, &policy());

        assert!(
            Arc::ptr_eq(&um, &outro),
            "rotas distintas precisam compartilhar o breaker"
        );
    }
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::error::GatewayError;
use crate::observability::metrics::{self, Outcome};
use crate::resilience::AttemptOutcome;
use crate::routing::layer::route_of;
use crate::{BoxFuture, Request, Response};

#[derive(Debug, Clone)]
pub struct CircuitBreakerLayer {
    registry: Arc<BreakerRegistry>,
}

impl CircuitBreakerLayer {
    pub fn new(registry: Arc<BreakerRegistry>) -> Self {
        Self { registry }
    }
}

impl<S> Layer<S> for CircuitBreakerLayer {
    type Service = CircuitBreakerService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CircuitBreakerService {
            inner,
            registry: self.registry.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CircuitBreakerService<S> {
    inner: S,
    registry: Arc<BreakerRegistry>,
}

impl<S> Service<Request> for CircuitBreakerService<S>
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

        let Some(route) = route_of(req.extensions()) else {
            return Box::pin(async move { inner.call(req).await });
        };

        // A política do breaker é a do **upstream**, não a da rota: o estado é
        // por upstream, e duas rotas para o mesmo serviço compartilham o circuito.
        let upstream = route.upstream.clone();
        let breaker = self
            .registry
            .get(&upstream.id, &upstream.resilience.circuit_breaker);

        let permit = match breaker.admit() {
            Ok(permit) => permit,
            Err(retry_after) => {
                tracing::debug!(upstream = %upstream.id, "circuito aberto, requisição cortada");
                metrics::mark(req.extensions(), Outcome::CircuitOpen);

                let response = GatewayError::unavailable(
                    "circuit_open",
                    "Upstream is not accepting requests right now",
                )
                .with_retry_after(retry_after)
                .into_response_for(req.extensions());

                return Box::pin(async move { Ok(response) });
            }
        };

        Box::pin(async move {
            let response = inner.call(req).await?;

            let outcome = AttemptOutcome::of(&response).unwrap_or(AttemptOutcome::Success);
            breaker.record(permit, !outcome.is_failure());

            Ok(response)
        })
    }
}
