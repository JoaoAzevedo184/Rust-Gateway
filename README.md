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

**Fases 1, 2 e 3 implementadas.** O gateway roteia, autentica, limita taxa, encaminha,
se protege de upstreams doentes e expõe a superfície completa de observabilidade da
spec, com Docker Compose subindo o ambiente completo.

| Fase | Conteúdo | Estado |
|---|---|---|
| 1 — núcleo | Configuração, roteamento, proxy, JWT/JWKS, rate limiting, health/ready | Implementada |
| 2 — resiliência | Três timeouts, retry, circuit breaker | Implementada |
| 3 — observabilidade | Superfície completa de métricas, OpenTelemetry, benchmarks de carga | Implementada |

## O que o gateway faz

- **Roteamento** por prefixo mais longo, configurado em YAML, com `strip_prefix` opcional.
- **Autenticação**: valida JWT por assinatura assimétrica, com JWKS em cache e suporte a
  rotação de chave via `kid`. Não emite tokens nem acessa banco de usuários — isso é do
  Auth Service.
- **Rate limiting** token bucket, por `sub` em rotas autenticadas e por IP em rotas
  anônimas, com estado compartilhado no Redis para múltiplas réplicas.
- **Resiliência**: três níveis de timeout, retry restrito a métodos idempotentes e a
  falhas pré-resposta, circuit breaker por upstream com janela deslizante.
- **Observabilidade**: correlation ID, logs JSON estruturados, a superfície completa de
  métricas Prometheus da spec e propagação de `traceparent` W3C, com exportação OTLP
  opcional.

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
cargo test                                    # 132 testes
REDIS_URL=redis://127.0.0.1:6379 cargo test   # inclui a conformance do store Redis
```

A arquitetura não assume instância única: subir múltiplas réplicas exige apenas
`store: redis` na configuração.

### Tracing distribuído

```bash
docker compose -f docker-compose.yml -f docker-compose.tracing.yml up --build
```

Sobe um Jaeger com o receptor OTLP ligado, e troca a configuração do gateway por uma
com `tracing.otlp_endpoint` apontando para ele. A UI fica em `localhost:16686`. A
propagação de `traceparent` W3C funciona com ou sem este overlay — ele só liga o
destino dos spans; veja [tracing distribuído](docs/reference/observability.md#tracing-distribuído).

## Métricas de referência

Medido com [`scripts/load-test.sh`](scripts/load-test.sh) (`oha`) contra o Compose
padrão, em uma rota anônima e sem rate limit efetivo, para isolar o overhead do
gateway em si. Máquina de desenvolvimento com 12 vCPUs, compartilhada com outras
cargas — não é um rack dedicado, então trate como indicativo, não como cota.

| Concorrência | RPS | P50 | P95 | P99 | CPU do gateway | RAM |
|---|---|---|---|---|---|---|
| 50 | ~28 500 | 1,6 ms | 2,9 ms | 3,8 ms | ~4,8 núcleos | ~16 MiB |
| 200 | ~30 300 | 6,2 ms | 10,6 ms | 13,8 ms | ~5,5 núcleos | ~32 MiB |
| 500 | ~29 000 | 16,7 ms | 25,0 ms | 30,1 ms | ~5,7 núcleos | ~69 MiB |

Sem erros em nenhum nível de carga testado — a rota rejeitada por `aborted due to
deadline` no relatório do `oha` é só a cauda de requisições em voo quando o teste
termina, não uma falha do gateway. O throughput satura por volta de 29–30 mil req/s
nesta máquina; RAM cresce com o número de conexões simultâneas em voo, não com o
tempo de execução.

