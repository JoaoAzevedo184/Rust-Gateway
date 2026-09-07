//! Testes de integração das políticas de resiliência (spec §10).
//!
//! Cada cenário usa um upstream construído para falhar de um jeito específico:
//! fechar a conexão sem responder, ou aceitar e nunca responder. É a única forma
//! de afirmar que o gateway distingue "não consegui falar" de "falou e demorou" —
//! a distinção da qual depende a segurança do retry.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::routing::any;
use http::StatusCode;
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower::util::BoxCloneService;

use rust_gateway::config::provider::FileProvider;
use rust_gateway::peer::PeerAddr;
use rust_gateway::server::{build_state, gateway_service};
use rust_gateway::state::AppState;

type Gateway = BoxCloneService<rust_gateway::Request, rust_gateway::Response, Infallible>;

/// Upstream que aceita a conexão e a fecha sem responder.
///
/// Do ponto de vista do cliente HTTP é uma falha **pré-resposta**, indistinguível
/// de uma recusa de conexão — e o contador de accepts diz quantas tentativas o
/// gateway realmente fez.
async fn spawn_failing_upstream() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));

    let counter = attempts.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });

    (addr, attempts)
}

/// Upstream que aceita e demora a responder.
async fn spawn_slow_upstream(delay: Duration) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().fallback(any(move || async move {
            tokio::time::sleep(delay).await;
            "tarde demais"
        }));
        let _ = axum::serve(listener, app).await;
    });

    addr
}

/// Upstream que sempre responde 500.
async fn spawn_erroring_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().fallback(any(|| async {
            (StatusCode::INTERNAL_SERVER_ERROR, "quebrado")
        }));
        let _ = axum::serve(listener, app).await;
    });

    addr
}

async fn harness(yaml: &str) -> (Gateway, Arc<AppState>) {
    let provider = FileProvider::from_yaml(yaml, "teste").expect("config válida");
    let config = provider.config().clone();
    let state = build_state(&config, &provider)
        .await
        .expect("estado montável");

    (BoxCloneService::new(gateway_service(&state)), state)
}

async fn send(gateway: &Gateway, req: http::Request<Body>) -> http::Response<Body> {
    let mut req = req;
    req.extensions_mut()
        .insert(PeerAddr("203.0.113.10:54321".parse().unwrap()));
    gateway
        .clone()
        .oneshot(req)
        .await
        .expect("a pilha é infalível")
}

async fn get(gateway: &Gateway, uri: &str) -> http::Response<Body> {
    send(
        gateway,
        http::Request::get(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn json(response: http::Response<Body>) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("corpo JSON")
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

fn config_retry(upstream: SocketAddr, max_attempts: u32) -> String {
    format!(
        r#"
server: {{ bind: 127.0.0.1:0, admin_bind: 127.0.0.1:0, request_timeout: 30s }}
resilience:
  default:
    upstream_timeout: 2s
    retry: {{ max_attempts: {max_attempts}, backoff: 1ms, jitter: false }}
upstreams:
  quebrado: {{ url: "http://{upstream}" }}
routes:
  - id: r
    match: {{ prefix: / }}
    upstream: quebrado
    auth: {{ required: false }}
"#
    )
}

#[tokio::test]
async fn get_e_repetido_ate_max_attempts_em_falha_pre_resposta() {
    let (upstream, attempts) = spawn_failing_upstream().await;
    let (gateway, _state) = harness(&config_retry(upstream, 3)).await;

    let response = get(&gateway, "/qualquer").await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "o GET deveria ter sido tentado três vezes"
    );
}

#[tokio::test]
async fn post_nunca_e_repetido() {
    let (upstream, attempts) = spawn_failing_upstream().await;
    let (gateway, _state) = harness(&config_retry(upstream, 3)).await;

    let response = send(
        &gateway,
        http::Request::post("/qualquer")
            .body(Body::from("carga"))
            .unwrap(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "repetir um POST pode cobrar duas vezes: uma tentativa e só"
    );
}

#[tokio::test]
async fn max_attempts_zero_desabilita_o_retry() {
    let (upstream, attempts) = spawn_failing_upstream().await;
    let (gateway, _state) = harness(&config_retry(upstream, 0)).await;

    get(&gateway, "/qualquer").await;

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn erro_5xx_do_upstream_nao_e_repetido() {
    let upstream = spawn_erroring_upstream().await;
    let (gateway, _state) = harness(&config_retry(upstream, 3)).await;

    let response = get(&gateway, "/qualquer").await;

    // O upstream respondeu: pode ter processado a requisição, e repetir deixa de
    // ser seguro. O 500 é repassado como veio.
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tentativa_que_estoura_upstream_timeout_responde_504() {
    let upstream = spawn_slow_upstream(Duration::from_secs(10)).await;
    let yaml = format!(
        r#"
server: {{ bind: 127.0.0.1:0, admin_bind: 127.0.0.1:0, request_timeout: 30s }}
resilience:
  default:
    upstream_timeout: 100ms
    retry: {{ max_attempts: 0 }}
upstreams:
  lento: {{ url: "http://{upstream}" }}
routes:
  - id: r
    match: {{ prefix: / }}
    upstream: lento
    auth: {{ required: false }}
"#
    );
    let (gateway, _state) = harness(&yaml).await;

    let started = Instant::now();
    let response = get(&gateway, "/qualquer").await;

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cortou no prazo, não esperou o upstream"
    );
    assert_eq!(json(response).await["error"], "upstream_timeout");
}

#[tokio::test]
async fn timeout_nao_e_repetido() {
    let upstream = spawn_slow_upstream(Duration::from_secs(10)).await;
    let yaml = format!(
        r#"
server: {{ bind: 127.0.0.1:0, admin_bind: 127.0.0.1:0, request_timeout: 30s }}
resilience:
  default:
    upstream_timeout: 100ms
    retry: {{ max_attempts: 3, backoff: 1ms, jitter: false }}
upstreams:
  lento: {{ url: "http://{upstream}" }}
routes:
  - id: r
    match: {{ prefix: / }}
    upstream: lento
    auth: {{ required: false }}
"#
    );
    let (gateway, _state) = harness(&yaml).await;

    let started = Instant::now();
    let response = get(&gateway, "/qualquer").await;

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "um timeout repetido três vezes levaria 300ms; ele não é repetido"
    );
}

#[tokio::test]
async fn request_timeout_e_o_teto_absoluto_incluindo_os_retries() {
    let (upstream, _) = spawn_failing_upstream().await;
    let yaml = format!(
        r#"
server: {{ bind: 127.0.0.1:0, admin_bind: 127.0.0.1:0, request_timeout: 200ms }}
resilience:
  default:
    upstream_timeout: 200ms
    retry: {{ max_attempts: 5, backoff: 300ms, jitter: false }}
upstreams:
  quebrado: {{ url: "http://{upstream}" }}
routes:
  - id: r
    match: {{ prefix: / }}
    upstream: quebrado
    auth: {{ required: false }}
"#
    );
    let (gateway, _state) = harness(&yaml).await;

    let started = Instant::now();
    let response = get(&gateway, "/qualquer").await;

    // Cinco tentativas com 300ms de espera passariam de um segundo. O teto global
    // corta antes, e é ele quem responde.
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "levou {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

fn config_breaker(upstream: SocketAddr) -> String {
    format!(
        r#"
server: {{ bind: 127.0.0.1:0, admin_bind: 127.0.0.1:0, request_timeout: 30s }}
resilience:
  default:
    upstream_timeout: 2s
    retry: {{ max_attempts: 0 }}
    circuit_breaker:
      failure_ratio: 0.5
      min_requests: 3
      window: 30s
      open_for: 60s
      half_open_probes: 1
upstreams:
  quebrado: {{ url: "http://{upstream}" }}
routes:
  - id: primeira
    match: {{ prefix: /primeira }}
    upstream: quebrado
    auth: {{ required: false }}
  - id: segunda
    match: {{ prefix: /segunda }}
    upstream: quebrado
    auth: {{ required: false }}
"#
    )
}

#[tokio::test]
async fn o_circuito_abre_apos_falhas_e_passa_a_cortar_imediatamente() {
    let upstream = spawn_erroring_upstream().await;
    let (gateway, _state) = harness(&config_breaker(upstream)).await;

    for _ in 0..3 {
        assert_eq!(
            get(&gateway, "/primeira").await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    let response = get(&gateway, "/primeira").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(json(response).await["error"], "circuit_open");
}

#[tokio::test]
async fn o_circuito_e_compartilhado_entre_rotas_do_mesmo_upstream() {
    let upstream = spawn_erroring_upstream().await;
    let (gateway, _state) = harness(&config_breaker(upstream)).await;

    for _ in 0..3 {
        get(&gateway, "/primeira").await;
    }

    // O estado é por upstream, não por rota: uma rota que nunca falhou também é
    // cortada, porque o serviço atrás dela é o mesmo.
    assert_eq!(
        get(&gateway, "/segunda").await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn respostas_4xx_nao_abrem_o_circuito() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = Router::new().fallback(any(|| async { StatusCode::NOT_FOUND }));
        let _ = axum::serve(listener, app).await;
    });

    let (gateway, _state) = harness(&config_breaker(upstream)).await;

    for _ in 0..10 {
        assert_eq!(
            get(&gateway, "/primeira").await.status(),
            StatusCode::NOT_FOUND
        );
    }

    // Uma onda de 404 é comportamento do cliente. Deixá-la abrir o circuito faria
    // um cliente mal configurado derrubar o serviço para todos.
    assert_eq!(
        get(&gateway, "/primeira").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn circuito_aberto_nao_consome_tentativas_de_retry() {
    let (upstream, attempts) = spawn_failing_upstream().await;
    let yaml = config_breaker(upstream).replace(
        "retry: { max_attempts: 0 }",
        "retry: { max_attempts: 3, backoff: 1ms, jitter: false }",
    );
    let (gateway, _state) = harness(&yaml).await;

    // A primeira requisição gasta três tentativas e abre o circuito.
    get(&gateway, "/primeira").await;
    let apos_primeira = attempts.load(Ordering::SeqCst);
    assert_eq!(apos_primeira, 3);

    let response = get(&gateway, "/primeira").await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        apos_primeira,
        "com o circuito aberto, nenhuma tentativa chega ao upstream"
    );
}

// ---------------------------------------------------------------------------
// Métricas
// ---------------------------------------------------------------------------

#[tokio::test]
async fn os_desfechos_da_fase_2_aparecem_nas_metricas() {
    let upstream = spawn_erroring_upstream().await;
    let (gateway, state) = harness(&config_breaker(upstream)).await;

    for _ in 0..4 {
        get(&gateway, "/primeira").await;
    }

    let rendered = state.metrics.render();
    assert!(
        rendered.contains(r#"outcome="upstream_error""#),
        "{rendered}"
    );
    assert!(rendered.contains(r#"outcome="circuit_open""#), "{rendered}");
}

#[tokio::test]
async fn timeout_aparece_como_desfecho_proprio() {
    let upstream = spawn_slow_upstream(Duration::from_secs(10)).await;
    let yaml = format!(
        r#"
server: {{ bind: 127.0.0.1:0, admin_bind: 127.0.0.1:0, request_timeout: 30s }}
resilience:
  default:
    upstream_timeout: 50ms
    retry: {{ max_attempts: 0 }}
upstreams:
  lento: {{ url: "http://{upstream}" }}
routes:
  - id: r
    match: {{ prefix: / }}
    upstream: lento
    auth: {{ required: false }}
"#
    );
    let (gateway, state) = harness(&yaml).await;

    get(&gateway, "/qualquer").await;

    assert!(state.metrics.render().contains(r#"outcome="timeout""#));
}
