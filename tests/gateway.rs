//! Testes de integração ponta a ponta (spec §13).
//!
//! A requisição atravessa a pilha inteira de layers e sai por uma conexão TCP real
//! até um upstream stub, que devolve o que recebeu. É assim que se afirma o que
//! **chega ao backend**, e não apenas o que o gateway respondeu.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::routing::any;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::StatusCode;
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower::util::BoxCloneService;

use rust_gateway::config::provider::FileProvider;
use rust_gateway::peer::PeerAddr;
use rust_gateway::server::{build_state, gateway_service};
use rust_gateway::state::AppState;

const KID: &str = "test-key-1";
const ISSUER: &str = "https://auth.test";
const AUDIENCE: &str = "rust-gateway";

type Gateway = BoxCloneService<rust_gateway::Request, rust_gateway::Response, Infallible>;

// ---------------------------------------------------------------------------
// Servidores de apoio
// ---------------------------------------------------------------------------

/// Upstream stub: responde 200 com o que recebeu, para que o teste possa afirmar
/// exatamente quais headers e qual caminho atravessaram o gateway.
async fn spawn_upstream() -> SocketAddr {
    async fn echo(req: http::Request<Body>) -> axum::Json<Value> {
        let headers: serde_json::Map<String, Value> = req
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    Value::String(value.to_str().unwrap_or_default().to_string()),
                )
            })
            .collect();

        axum::Json(json!({
            "path": req.uri().to_string(),
            "method": req.method().as_str(),
            "headers": headers,
        }))
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().fallback(any(echo));
        let _ = axum::serve(listener, app).await;
    });

    addr
}

/// Auth Service falso: serve a JWKS correspondente à chave privada das fixtures.
async fn spawn_jwks() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let app = Router::new().route(
            "/jwks.json",
            any(|| async { include_str!("fixtures/jwks.json") }),
        );
        let _ = axum::serve(listener, app).await;
    });

    addr
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn sign(claims: Value) -> String {
    let key = EncodingKey::from_rsa_pem(include_bytes!("fixtures/test_key.pem"))
        .expect("chave de teste em PEM");

    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());

    jsonwebtoken::encode(&header, &claims, &key).expect("token assinável")
}

fn token_valido() -> String {
    sign(json!({
        "sub": "user-42",
        "iss": ISSUER,
        "aud": AUDIENCE,
        "exp": now() + 3600,
        "nbf": now() - 10,
        "scope": "user.read user.write",
    }))
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    gateway: Gateway,
    state: Arc<AppState>,
}

impl Harness {
    async fn new() -> Self {
        let upstream = spawn_upstream().await;
        let jwks = spawn_jwks().await;

        let yaml = format!(
            r#"
server:
  bind: 127.0.0.1:0
  admin_bind: 127.0.0.1:0
  max_body_bytes: 1MiB
auth:
  jwks_url: http://{jwks}/jwks.json
  issuer: {ISSUER}
  audience: {AUDIENCE}
  refresh_interval: 5m
upstreams:
  echo: {{ url: http://{upstream} }}
routes:
  - id: publico
    match: {{ prefix: /publico }}
    upstream: echo
    auth: {{ required: false }}
  - id: users
    match: {{ prefix: /users }}
    upstream: echo
    auth: {{ required: true, scopes: [user.read] }}
  - id: orders
    match: {{ prefix: /orders }}
    upstream: echo
    strip_prefix: true
    auth: {{ required: true }}
  - id: limitado
    match: {{ prefix: /limitado }}
    upstream: echo
    auth: {{ required: false }}
    rate_limit:
      - {{ key: ip, capacity: 2, refill_per_sec: 0.01 }}
"#
        );

        let provider = FileProvider::from_yaml(&yaml, "teste").expect("config válida");
        let config = provider.config().clone();
        let state = build_state(&config, &provider)
            .await
            .expect("estado montável");

        Self {
            gateway: BoxCloneService::new(gateway_service(&state)),
            state,
        }
    }

    async fn send(&self, req: http::Request<Body>) -> http::Response<Body> {
        let mut req = req;
        // Carimbo que o make-service do listener público faria.
        req.extensions_mut()
            .insert(PeerAddr("203.0.113.10:54321".parse().unwrap()));

        self.gateway
            .clone()
            .oneshot(req)
            .await
            .expect("a pilha é infalível")
    }

    async fn get(&self, uri: &str) -> http::Response<Body> {
        self.send(http::Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    async fn get_com_token(&self, uri: &str, token: &str) -> http::Response<Body> {
        self.send(
            http::Request::get(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }
}

async fn json_do_corpo(response: http::Response<Body>) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("corpo JSON")
}

/// Headers que o upstream stub declarou ter recebido.
async fn headers_no_upstream(response: http::Response<Body>) -> Value {
    json_do_corpo(response).await["headers"].clone()
}

// ---------------------------------------------------------------------------
// Roteamento e proxy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rota_anonima_chega_ao_upstream_com_o_caminho_intacto() {
    let harness = Harness::new().await;
    let response = harness.get("/publico/recurso?a=1&b=2").await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_do_corpo(response).await["path"],
        "/publico/recurso?a=1&b=2"
    );
}

#[tokio::test]
async fn strip_prefix_remove_o_prefixo_e_preserva_a_query() {
    let harness = Harness::new().await;
    let response = harness
        .get_com_token("/orders/42?fields=id", &token_valido())
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_do_corpo(response).await["path"], "/42?fields=id");
}

#[tokio::test]
async fn prefixo_sem_rota_responde_404_com_corpo_de_erro_do_gateway() {
    let harness = Harness::new().await;
    let response = harness.get("/inexistente").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let body = json_do_corpo(response).await;
    assert_eq!(body["error"], "not_found");
    assert!(body["request_id"].as_str().is_some_and(|id| !id.is_empty()));
}

#[tokio::test]
async fn prefixo_que_nao_respeita_fronteira_de_segmento_nao_casa() {
    let harness = Harness::new().await;
    // `/publicosecreto` não pode herdar a política de `/publico`.
    assert_eq!(
        harness.get("/publicosecreto").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn o_host_e_reescrito_para_a_autoridade_do_upstream() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::get("/publico")
                .header("host", "gateway.externo")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    let headers = headers_no_upstream(response).await;
    assert_ne!(headers["host"], "gateway.externo");
    assert_eq!(headers["x-forwarded-host"], "gateway.externo");
}

#[tokio::test]
async fn headers_hop_by_hop_nao_atravessam_o_gateway() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::get("/publico")
                .header("connection", "keep-alive, x-hop-secreto")
                .header("x-hop-secreto", "nao deveria passar")
                .header("keep-alive", "timeout=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    let headers = headers_no_upstream(response).await;
    assert!(
        headers.get("x-hop-secreto").is_none(),
        "header listado no Connection vazou"
    );
    assert!(headers.get("keep-alive").is_none());
}

#[tokio::test]
async fn x_forwarded_for_forjado_e_substituido_pelo_peer_observado() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::get("/publico")
                .header("x-forwarded-for", "1.1.1.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    // `trusted_proxies` vazio: o header do cliente não vale nada.
    assert_eq!(
        headers_no_upstream(response).await["x-forwarded-for"],
        "203.0.113.10"
    );
}

// ---------------------------------------------------------------------------
// Correlation ID
// ---------------------------------------------------------------------------

#[tokio::test]
async fn o_correlation_id_do_cliente_e_propagado_nos_dois_sentidos() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::get("/publico")
                .header("x-request-id", "abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    assert_eq!(response.headers()["x-request-id"], "abc-123");
    assert_eq!(
        headers_no_upstream(response).await["x-request-id"],
        "abc-123"
    );
}

#[tokio::test]
async fn correlation_id_malformado_e_substituido() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::get("/publico")
                .header("x-request-id", "injecao\tde log")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(id, "injecao\tde log");
    assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
}

// ---------------------------------------------------------------------------
// Autenticação
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_valido_passa_e_o_gateway_injeta_a_identidade() {
    let harness = Harness::new().await;
    let response = harness.get_com_token("/users/me", &token_valido()).await;

    assert_eq!(response.status(), StatusCode::OK);

    let headers = headers_no_upstream(response).await;
    assert_eq!(headers["x-user-id"], "user-42");
    assert_eq!(headers["x-user-scopes"], "user.read user.write");
    assert_eq!(headers["x-auth-method"], "jwt");
    assert!(
        headers["authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer "),
        "o Authorization original segue intacto para o backend"
    );
}

#[tokio::test]
async fn header_de_identidade_forjado_e_removido_antes_de_qualquer_politica() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::get("/publico")
                .header("x-user-id", "admin")
                .header("x-user-scopes", "tudo")
                .header("x-auth-method", "confia-em-mim")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

    // Rota anônima: o layer de auth nem roda. O scrub global é quem protege.
    let headers = headers_no_upstream(response).await;
    assert!(
        headers.get("x-user-id").is_none(),
        "identidade forjada chegou ao backend"
    );
    assert!(headers.get("x-user-scopes").is_none());
    assert!(headers.get("x-auth-method").is_none());
}

#[tokio::test]
async fn rota_autenticada_sem_token_responde_401() {
    let harness = Harness::new().await;
    let response = harness.get("/users/me").await;

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(json_do_corpo(response).await["error"], "unauthorized");
}

#[tokio::test]
async fn token_expirado_responde_401() {
    let harness = Harness::new().await;
    let expirado = sign(json!({
        "sub": "user-42", "iss": ISSUER, "aud": AUDIENCE,
        "exp": now() - 3600, "scope": "user.read",
    }));

    assert_eq!(
        harness.get_com_token("/users/me", &expirado).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn audience_errada_responde_401() {
    let harness = Harness::new().await;
    let outra_aud = sign(json!({
        "sub": "user-42", "iss": ISSUER, "aud": "outro-servico",
        "exp": now() + 3600, "scope": "user.read",
    }));

    assert_eq!(
        harness
            .get_com_token("/users/me", &outra_aud)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn issuer_errado_responde_401() {
    let harness = Harness::new().await;
    let outro_iss = sign(json!({
        "sub": "user-42", "iss": "https://impostor.test", "aud": AUDIENCE,
        "exp": now() + 3600, "scope": "user.read",
    }));

    assert_eq!(
        harness
            .get_com_token("/users/me", &outro_iss)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn kid_desconhecido_responde_401() {
    let harness = Harness::new().await;

    let key = EncodingKey::from_rsa_pem(include_bytes!("fixtures/test_key.pem")).unwrap();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("chave-que-nao-existe".into());
    let token = jsonwebtoken::encode(
        &header,
        &json!({ "sub": "u", "iss": ISSUER, "aud": AUDIENCE, "exp": now() + 3600 }),
        &key,
    )
    .unwrap();

    assert_eq!(
        harness.get_com_token("/users/me", &token).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn token_sem_assinatura_com_alg_none_responde_401() {
    let harness = Harness::new().await;

    // A vulnerabilidade clássica de JWT: `alg: none`. O algoritmo vem da chave da
    // JWKS, não do header do token, então isto não tem como passar.
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT","kid":"test-key-1"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        json!({ "sub": "admin", "iss": ISSUER, "aud": AUDIENCE, "exp": now() + 3600 }).to_string(),
    );
    let token = format!("{header}.{claims}.");

    assert_eq!(
        harness.get_com_token("/users/me", &token).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn escopo_insuficiente_responde_403_e_nao_401() {
    let harness = Harness::new().await;
    let sem_escopo = sign(json!({
        "sub": "user-42", "iss": ISSUER, "aud": AUDIENCE,
        "exp": now() + 3600, "scope": "outra.coisa",
    }));

    let response = harness.get_com_token("/users/me", &sem_escopo).await;

    // A distinção importa: 401 diz "identifique-se", 403 diz "você não pode".
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(json_do_corpo(response).await["error"], "forbidden");
}

#[tokio::test]
async fn token_invalido_em_rota_anonima_ainda_e_recusado() {
    let harness = Harness::new().await;
    let expirado = sign(json!({
        "sub": "u", "iss": ISSUER, "aud": AUDIENCE, "exp": now() - 3600,
    }));

    // A rota dispensa credencial, não perdoa credencial ruim.
    assert_eq!(
        harness.get_com_token("/publico", &expirado).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

// ---------------------------------------------------------------------------
// Rate limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_limite_responde_429_com_retry_after_e_headers() {
    let harness = Harness::new().await;

    for _ in 0..2 {
        let response = harness.get("/limitado").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-ratelimit-limit"], "2");
    }

    let response = harness.get("/limitado").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["x-ratelimit-remaining"], "0");
    assert!(response.headers().contains_key("retry-after"));
    assert_eq!(json_do_corpo(response).await["error"], "rate_limited");
}

#[tokio::test]
async fn rota_sem_rate_limit_nao_ganha_headers_de_limite() {
    let harness = Harness::new().await;
    let response = harness.get("/publico").await;

    assert!(response.headers().get("x-ratelimit-limit").is_none());
}

// ---------------------------------------------------------------------------
// Observabilidade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn as_metricas_registram_rota_status_e_desfecho() {
    let harness = Harness::new().await;

    harness.get("/publico").await;
    harness.get("/users/me").await;
    harness.get("/inexistente").await;

    let rendered = harness.state.metrics.render();

    assert!(
        rendered.contains(r#"route="publico""#) && rendered.contains(r#"outcome="ok""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"outcome="rejected_auth""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"route="unmatched""#) && rendered.contains(r#"outcome="no_route""#),
        "o 404 precisa de um label de rota constante: {rendered}"
    );
    assert!(rendered.contains("gateway_request_duration_seconds"));
}

#[tokio::test]
async fn health_nao_depende_de_ninguem_e_ready_depende_da_jwks() {
    let harness = Harness::new().await;

    let admin = rust_gateway::observability::health::router(
        rust_gateway::observability::health::AdminState {
            metrics: harness.state.metrics.clone(),
            readiness: harness.state.clone()
                as Arc<dyn rust_gateway::observability::health::ReadinessCheck>,
        },
    );

    let health = admin
        .clone()
        .oneshot(http::Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let ready = admin
        .clone()
        .oneshot(http::Request::get("/ready").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        ready.status(),
        StatusCode::OK,
        "a JWKS foi carregada no boot"
    );

    let metrics = admin
        .oneshot(http::Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Limite de corpo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corpo_acima_do_limite_declarado_responde_413() {
    let harness = Harness::new().await;
    let response = harness
        .send(
            http::Request::post("/publico")
                .header("content-length", (2 * 1024 * 1024).to_string())
                .body(Body::from("x"))
                .unwrap(),
        )
        .await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
