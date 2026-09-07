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

**Fase 1 implementada.** O gateway roteia, autentica, limita taxa e encaminha, com
Docker Compose subindo o ambiente completo.

| Fase | Conteúdo | Estado |
|---|---|---|
| 1 — núcleo | Configuração, roteamento, proxy, JWT/JWKS, rate limiting, health/ready | Implementada |
| 2 — resiliência | Timeouts, retry, circuit breaker | Especificada |
| 3 — observabilidade | Métricas completas, OpenTelemetry, benchmarks | Especificada |

Da Fase 2, só `connect_timeout` está em vigor. `upstream_timeout`,
`request_timeout`, retry e circuit breaker são lidos e validados na configuração,
mas ainda não aplicados no caminho da requisição.

## O que o gateway faz

- **Roteamento** por prefixo mais longo, configurado em YAML, com `strip_prefix` opcional.
- **Autenticação**: valida JWT por assinatura assimétrica, com JWKS em cache e suporte a
  rotação de chave via `kid`. Não emite tokens nem acessa banco de usuários — isso é do
  Auth Service.
- **Rate limiting** token bucket, por `sub` em rotas autenticadas e por IP em rotas
  anônimas, com estado compartilhado no Redis para múltiplas réplicas.
- **Resiliência** (Fase 2): três níveis de timeout, retry restrito a métodos idempotentes
  e falhas pré-resposta, circuit breaker por upstream.
- **Observabilidade**: correlation ID, logs JSON estruturados e métricas Prometheus.
  Propagação de contexto de trace W3C na Fase 3.

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

[`config/gateway.example.yaml`](config/gateway.example.yaml) documenta todos os
campos com seus defaults; [`config/gateway.yaml`](config/gateway.yaml) é o que o
Compose sobe. Configuração inválida impede o processo de subir, e a validação
reporta todos os problemas de uma vez — descobrir um erro por vez, com um restart
entre cada, torna a edição de config um exercício de paciência.

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

## Como rodar

```bash
docker compose up --build
```

Sobe o gateway em `localhost:8080`, um Redis e três upstreams stub
(`traefik/whoami`, que ecoa os headers recebidos — conveniente para ver o que o
gateway injetou e o que removeu).

```bash
curl localhost:8080/users/perfil                   # roteia para user-service
curl localhost:8080/orders/42                      # strip_prefix: chega como /42
curl -H 'X-User-Id: forjado' localhost:8080/users  # o header não sobrevive à borda
curl -i localhost:8080/payments                    # X-RateLimit-* na resposta
```

A porta administrativa (9090) não é publicada de propósito: `/metrics` e `/ready`
não passam pela pilha de políticas e não deveriam estar expostos. De dentro da rede:

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics'
```

Sem Docker, com um toolchain Rust 1.85 ou mais novo:

```bash
cargo run -- config/gateway.yaml              # o caminho da config é o argumento
cargo test                                    # 88 testes
REDIS_URL=redis://127.0.0.1:6379 cargo test   # inclui a conformance do store Redis
```

A arquitetura não assume instância única: subir múltiplas réplicas exige apenas
`store: redis` na configuração.

## Métricas de referência

Ao fim da Fase 3: requisições por segundo, P50, P95, P99, CPU e RAM sob carga.
