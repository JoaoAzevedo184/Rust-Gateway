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

**Fases 1 e 2 implementadas.** O gateway roteia, autentica, limita taxa, encaminha
e se protege de upstreams doentes, com Docker Compose subindo o ambiente completo.

| Fase | Conteúdo | Estado |
|---|---|---|
| 1 — núcleo | Configuração, roteamento, proxy, JWT/JWKS, rate limiting, health/ready | Implementada |
| 2 — resiliência | Três timeouts, retry, circuit breaker | Implementada |
| 3 — observabilidade | Superfície completa de métricas, OpenTelemetry, benchmarks | Especificada |

A Fase 3 acrescenta as famílias de métrica que faltam — `gateway_circuit_state`,
`gateway_retries_total`, `gateway_upstream_duration_seconds` — e a propagação de
`traceparent` W3C. Até lá, o circuit breaker é observável pelo label
`outcome="circuit_open"` e pelas transições no log.

## O que o gateway faz

- **Roteamento** por prefixo mais longo, configurado em YAML, com `strip_prefix` opcional.
- **Autenticação**: valida JWT por assinatura assimétrica, com JWKS em cache e suporte a
  rotação de chave via `kid`. Não emite tokens nem acessa banco de usuários — isso é do
  Auth Service.
- **Rate limiting** token bucket, por `sub` em rotas autenticadas e por IP em rotas
  anônimas, com estado compartilhado no Redis para múltiplas réplicas.
- **Resiliência**: três níveis de timeout, retry restrito a métodos idempotentes e a
  falhas pré-resposta, circuit breaker por upstream com janela deslizante.
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

- **[Tutorial](docs/tutorial/)** — do clone ao primeiro request autenticado, em quinze
  minutos. Comece por aqui.
- **[How-to](docs/how-to/)** — adicionar uma rota, proteger com escopos, subir réplicas,
  ajustar resiliência, investigar um 429 ou um 502.
- **[Reference](docs/reference/)** — todo campo de configuração, código de erro, header e
  métrica.
- **[Explanation](docs/explanation/)** — por que cada decisão foi tomada: a ordem dos
  layers, a fronteira de autenticação, por que o rate limit falha aberto, por que o
  breaker fica dentro do retry.

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
cargo test                                    # 118 testes
REDIS_URL=redis://127.0.0.1:6379 cargo test   # inclui a conformance do store Redis
```

A arquitetura não assume instância única: subir múltiplas réplicas exige apenas
`store: redis` na configuração.

## Métricas de referência

Ao fim da Fase 3: requisições por segundo, P50, P95, P99, CPU e RAM sob carga.
