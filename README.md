# Rust Gateway

API gateway em Rust: ponto único de entrada para um conjunto de serviços internos,
concentrando roteamento, validação de JWT, rate limiting, resiliência e observabilidade.

```
Client
  ↓
Rust Gateway
  ├── User Service
  ├── Order Service
  └── Payment Service
```

Os serviços de backend (Java e Python) vivem apenas na rede interna. Só o gateway é
exposto.

## Estado

**Fase 1 em design concluído, implementação não iniciada.** O código atual é um
esqueleto com `/health`.

| Fase | Conteúdo | Estado |
|---|---|---|
| 1 — núcleo | Configuração, roteamento, proxy, JWT/JWKS, rate limiting, health/ready | Especificado |
| 2 — resiliência | Timeouts, retry, circuit breaker | Especificado |
| 3 — observabilidade | Métricas completas, OpenTelemetry, benchmarks | Especificado |

## O que o gateway faz

- **Roteamento** por prefixo mais longo, configurado em YAML, com `strip_prefix` opcional.
- **Autenticação**: valida JWT por assinatura assimétrica, com JWKS em cache e suporte a
  rotação de chave via `kid`. Não emite tokens nem acessa banco de usuários — isso é do
  Auth Service.
- **Rate limiting** token bucket, por `sub` em rotas autenticadas e por IP em rotas
  anônimas, com estado compartilhado no Redis para múltiplas réplicas.
- **Resiliência**: três níveis de timeout, retry restrito a métodos idempotentes e falhas
  pré-resposta, circuit breaker por upstream.
- **Observabilidade**: correlation ID, logs JSON estruturados, métricas Prometheus e
  propagação de contexto de trace W3C.

## O que ele deliberadamente não faz

Emitir tokens, gerir usuários, aplicar autorização de domínio, terminar TLS. A fronteira é:

```
Auth Service     autentica e emite JWT
Rust Gateway     valida o token e aplica política de rota
Backend          aplica autorização específica de domínio
```

## Configuração

Rotas e políticas em um único arquivo YAML:

```yaml
upstreams:
  user-service:  { url: http://user-service:8080 }
  order-service: { url: http://order-service:8081 }

routes:
  - id: users
    match: { prefix: /users }
    upstream: user-service
    auth: { required: true, scopes: [user.read] }
    rate_limit:
      - { key: sub, capacity: 600, refill_per_sec: 10 }

  - id: users-signup
    match: { prefix: /users/signup }     # prefixo mais longo vence
    upstream: user-service
    auth: { required: false }
    rate_limit:
      - { key: ip, capacity: 5, refill_per_sec: 0.1 }
```

O schema completo, com defaults e regras de validação, está na
[spec de design](docs/superpowers/specs/2026-09-06-rust-gateway-design.md#5-configuração).

## Stack

Rust · Tokio · Hyper · Axum · Tower · Redis · Prometheus · OpenTelemetry · Docker

## Documentação

A documentação segue o [Diátaxis](https://diataxis.fr/) e vive em [`docs/`](docs/).

O quadrante **Explanation** está escrito e explica o racional de cada decisão:

- [Arquitetura](docs/explanation/architecture.md) — a pilha de layers, o que é global e o
  que é por rota, onde vive o estado.
- [Modelo de autenticação](docs/explanation/authentication-model.md) — a fronteira entre
  Auth Service, gateway e backend.
- [Rate limiting](docs/explanation/rate-limiting.md) — por que token bucket, e por que
  falha aberto.
- [Resiliência](docs/explanation/resilience.md) — a ordem entre retry e circuit breaker.
- [Observabilidade](docs/explanation/observability.md) — liveness contra readiness, e a
  regra de cardinalidade.

Reference, How-to e Tutorial são escritos junto com a Fase 1, verificados contra o binário.

## Ambiente

Docker Compose no início, com gateway, Redis e upstreams. A arquitetura não assume
instância única: subir múltiplas réplicas exige apenas `store: redis` na configuração.

## Métricas de referência

Ao fim da Fase 3: requisições por segundo, P50, P95, P99, CPU e RAM sob carga.
