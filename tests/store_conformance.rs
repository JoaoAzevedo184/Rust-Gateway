//! Conformance suite compartilhada para `RateLimitStore` (spec §13).
//!
//! Um único conjunto de asserções, rodado contra a implementação in-memory e —
//! quando há um Redis disponível — contra a implementação Redis. É o que garante
//! que trocar de store não muda comportamento: sem isso, "memory em dev, redis em
//! prod" vira duas semânticas diferentes descobertas em produção.
//!
//! Para incluir o Redis: `REDIS_URL=redis://127.0.0.1:6379 cargo test`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rust_gateway::clock::{Clock, TestClock};
use rust_gateway::config::KeyKind;
use rust_gateway::ratelimit::memory::MemoryStore;
use rust_gateway::ratelimit::redis::RedisStore;
use rust_gateway::ratelimit::store::{BucketRequest, RateLimitStore, bucket_key};

/// Avanço de tempo. O store in-memory move um relógio de teste; o Redis, que usa
/// o próprio `TIME`, precisa de espera real.
type Advance = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

fn bucket(prefix: &str, kind: KeyKind, value: &str, capacity: u32, rate: f64) -> BucketRequest {
    BucketRequest {
        key: bucket_key(prefix, kind, value),
        kind,
        capacity,
        refill_per_sec: rate,
        cost: 1,
    }
}

/// Sufixo único por execução: o Redis do teste pode ser reaproveitado entre rodadas.
fn scope(name: &str) -> String {
    format!("conf-{name}-{}", uuid_like())
}

fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

async fn esgota_a_capacidade_e_recusa_a_proxima(store: &dyn RateLimitStore) {
    let route = scope("esgota");
    let request = vec![bucket(&route, KeyKind::Ip, "1.2.3.4", 3, 1.0)];

    for esperado in [2, 1, 0] {
        let decision = store.try_acquire(&request).await.unwrap();
        assert!(decision.allowed, "capacidade ainda não esgotada");
        assert_eq!(decision.remaining, esperado);
        assert_eq!(decision.limit, 3);
    }

    let decision = store.try_acquire(&request).await.unwrap();
    assert!(
        !decision.allowed,
        "a quarta requisição deveria ser recusada"
    );
    assert_eq!(decision.remaining, 0);
    assert!(
        decision.retry_after.is_some_and(|d| d > Duration::ZERO),
        "recusa precisa dizer quando tentar de novo"
    );
}

async fn o_refill_devolve_tokens_com_o_tempo(store: &dyn RateLimitStore, advance: &Advance) {
    let route = scope("refill");
    // 2 tokens de capacidade, 10 por segundo: 100ms devolvem um token.
    let request = vec![bucket(&route, KeyKind::Ip, "1.2.3.4", 2, 10.0)];

    assert!(store.try_acquire(&request).await.unwrap().allowed);
    assert!(store.try_acquire(&request).await.unwrap().allowed);
    assert!(
        !store.try_acquire(&request).await.unwrap().allowed,
        "bucket esgotado"
    );

    advance(Duration::from_millis(250)).await;

    assert!(
        store.try_acquire(&request).await.unwrap().allowed,
        "depois do refill a requisição volta a passar"
    );
}

async fn a_decisao_e_tudo_ou_nada(store: &dyn RateLimitStore) {
    let route = scope("atomico");

    // Um bucket generoso e um apertado, como "por sub, com teto bruto por IP".
    let generoso = bucket(&route, KeyKind::Sub, "u-1", 100, 10.0);
    let apertado = bucket(&route, KeyKind::Ip, "1.2.3.4", 1, 0.01);

    let ambos = vec![generoso.clone(), apertado.clone()];

    assert!(store.try_acquire(&ambos).await.unwrap().allowed);
    assert!(
        !store.try_acquire(&ambos).await.unwrap().allowed,
        "o bucket apertado recusa"
    );

    // O bucket generoso não pode ter sido debitado pelas tentativas recusadas.
    let so_generoso = vec![generoso];
    let decision = store.try_acquire(&so_generoso).await.unwrap();
    assert!(decision.allowed);
    assert_eq!(
        decision.remaining, 98,
        "o bucket generoso perdeu exatamente um token, o da requisição que passou"
    );
}

async fn chaves_distintas_sao_independentes(store: &dyn RateLimitStore) {
    let route = scope("independentes");
    let um = vec![bucket(&route, KeyKind::Ip, "1.1.1.1", 1, 0.01)];
    let outro = vec![bucket(&route, KeyKind::Ip, "2.2.2.2", 1, 0.01)];

    assert!(store.try_acquire(&um).await.unwrap().allowed);
    assert!(!store.try_acquire(&um).await.unwrap().allowed);
    assert!(
        store.try_acquire(&outro).await.unwrap().allowed,
        "outra chave, outro bucket"
    );
}

async fn os_headers_descrevem_o_bucket_mais_restritivo(store: &dyn RateLimitStore) {
    let route = scope("restritivo");
    let request = vec![
        bucket(&route, KeyKind::Sub, "u-1", 100, 10.0),
        bucket(&route, KeyKind::Ip, "1.2.3.4", 5, 1.0),
    ];

    let decision = store.try_acquire(&request).await.unwrap();
    assert_eq!(
        decision.limit, 5,
        "o limite anunciado é o do bucket que restringe"
    );
    assert_eq!(decision.remaining, 4);
}

async fn lista_vazia_e_permitida(store: &dyn RateLimitStore) {
    let decision = store.try_acquire(&[]).await.unwrap();
    assert!(decision.allowed);
}

async fn run_suite(store: Arc<dyn RateLimitStore>, advance: Advance) {
    esgota_a_capacidade_e_recusa_a_proxima(store.as_ref()).await;
    o_refill_devolve_tokens_com_o_tempo(store.as_ref(), &advance).await;
    a_decisao_e_tudo_ou_nada(store.as_ref()).await;
    chaves_distintas_sao_independentes(store.as_ref()).await;
    os_headers_descrevem_o_bucket_mais_restritivo(store.as_ref()).await;
    lista_vazia_e_permitida(store.as_ref()).await;
}

#[tokio::test]
async fn memory_store_cumpre_a_conformance_suite() {
    let clock = Arc::new(TestClock::default());
    let store = Arc::new(MemoryStore::new(clock.clone() as Arc<dyn Clock>));

    let advance: Advance = {
        let clock = clock.clone();
        Arc::new(move |d: Duration| {
            let clock = clock.clone();
            Box::pin(async move { clock.advance_ms(d.as_millis() as u64) })
                as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    };

    run_suite(store, advance).await;
}

#[tokio::test]
async fn redis_store_cumpre_a_conformance_suite() {
    let Ok(url) = std::env::var("REDIS_URL") else {
        eprintln!("REDIS_URL ausente: conformance do Redis pulada");
        return;
    };

    let store = Arc::new(
        RedisStore::connect(&url, Duration::from_millis(500))
            .await
            .expect("Redis acessível na URL informada"),
    );

    // O relógio do Redis é o do próprio Redis, então aqui a espera é real.
    let advance: Advance = Arc::new(|d: Duration| {
        Box::pin(tokio::time::sleep(d)) as Pin<Box<dyn Future<Output = ()> + Send>>
    });

    run_suite(store, advance).await;
}
