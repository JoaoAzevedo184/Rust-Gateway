[LICENSE__BADGE]: https://img.shields.io/github/license/JoaoAzevedo184/Rust-Gateway?style=for-the-badge
[RUST__BADGE]: https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white
[TOKIO__BADGE]: https://img.shields.io/badge/Tokio-4E4E4E?style=for-the-badge
[AXUM__BADGE]: https://img.shields.io/badge/Axum-1C1C1C?style=for-the-badge
[TOWER__BADGE]: https://img.shields.io/badge/Tower-2B2B2B?style=for-the-badge
[REDIS__BADGE]: https://img.shields.io/badge/redis-%23DD0031.svg?style=for-the-badge&logo=redis&logoColor=white
[PROMETHEUS__BADGE]: https://img.shields.io/badge/Prometheus-E6522C?style=for-the-badge&logo=prometheus&logoColor=white
[OPENTELEMETRY__BADGE]: https://img.shields.io/badge/OpenTelemetry-425CC7?style=for-the-badge&logo=opentelemetry&logoColor=white
[DOCKER__BADGE]: https://img.shields.io/badge/docker-%230db7ed.svg?style=for-the-badge&logo=docker&logoColor=white
[PRS__BADGE]: https://img.shields.io/badge/PRs-welcome-green?style=for-the-badge

<h1 align="center" style="font-weight: bold;">Rust Gateway 🦀🚪</h1>

<p align="center">
  <img src="https://img.shields.io/github/license/JoaoAzevedo184/Rust-Gateway?style=for-the-badge" alt="license" />
  <img src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="rust" />
  <img src="https://img.shields.io/badge/Tokio-4E4E4E?style=for-the-badge" alt="tokio" />
  <img src="https://img.shields.io/badge/Axum-1C1C1C?style=for-the-badge" alt="axum" />
  <img src="https://img.shields.io/badge/Tower-2B2B2B?style=for-the-badge" alt="tower" />
  <img src="https://img.shields.io/badge/redis-%23DD0031.svg?style=for-the-badge&logo=redis&logoColor=white" alt="redis" />
  <img src="https://img.shields.io/badge/Prometheus-E6522C?style=for-the-badge&logo=prometheus&logoColor=white" alt="prometheus" />
  <img src="https://img.shields.io/badge/OpenTelemetry-425CC7?style=for-the-badge&logo=opentelemetry&logoColor=white" alt="opentelemetry" />
  <img src="https://img.shields.io/badge/docker-%230db7ed.svg?style=for-the-badge&logo=docker&logoColor=white" alt="docker" />
  <img src="https://img.shields.io/badge/PRs-welcome-green?style=for-the-badge" alt="prs" />
</p>

<details open="open">
<summary>Sumário</summary>

- [🚀 Começando](#started)
  - [Pré-requisitos](#prerequisites)
  - [Clonando](#cloning)
  - [Configuração](#environment-variables)
  - [Rodando](#starting)
- [🧠 O que o gateway faz](#what-it-does)
- [🚧 O que ele deliberadamente não faz](#what-it-doesnt-do)
- [🗺️ Roteamento e políticas](#config-schema)
- [📍 Endpoints administrativos](#routes)
  - [GET /health](#get-health-detail)
  - [GET /ready](#get-ready-detail)
  - [GET /metrics](#get-metrics-detail)
- [🔭 Rastreamento distribuído](#tracing)
- [📊 Métricas de referência](#benchmarks)
- [📚 Documentação](#docs)
- [🤝 Colaboradores](#colab)
- [📫 Contribua](#contribute)

</details>

<p align="center">
  <b>API gateway em Rust: ponto único de entrada para um conjunto de serviços internos,
  concentrando roteamento, validação de JWT, rate limiting, resiliência e observabilidade.</b>
</p>

```
Client
  ↓
Rust Gateway
  ├── User Service
  ├── Order Service
  └── Payment Service
```

Os serviços de backend (Java e Python) vivem apenas na rede interna. Só o gateway é exposto.

## Estado

**Fases 1, 2 e 3 implementadas.** O gateway roteia, autentica, limita taxa, encaminha,
se protege de upstreams doentes e expõe a superfície completa de observabilidade da
spec, com Docker Compose subindo o ambiente completo.

| Fase | Conteúdo | Estado |
|---|---|---|
| 1 — núcleo | Configuração, roteamento, proxy, JWT/JWKS, rate limiting, health/ready | Implementada |
| 2 — resiliência | Três timeouts, retry, circuit breaker | Implementada |
| 3 — observabilidade | Superfície completa de métricas, OpenTelemetry, benchmarks de carga | Implementada |

<h2 id="started">🚀 Começando</h2>

<h3 id="prerequisites">Pré-requisitos</h3>

Caminho recomendado:

- [Docker](https://docs.docker.com/get-docker/) e [Docker Compose](https://docs.docker.com/compose/)

Alternativa sem Docker:

- [Rust](https://www.rust-lang.org/tools/install) 1.85 ou mais novo (`edition = "2024"`), instalado via `rustup`
- Um Redis acessível, se for usar `rate_limit.store: redis`

<h3 id="cloning">Clonando</h3>

```bash
git clone https://github.com/JoaoAzevedo184/Rust-Gateway.git
cd Rust-Gateway
```

<h3 id="environment-variables">Configuração</h3>

O gateway não tem variáveis de ambiente de negócio — a configuração inteira (rotas, upstreams, auth, rate limit, resiliência, tracing) vive em um único arquivo YAML, validado no startup. Veja a seção [Roteamento e políticas](#config-schema) para o schema, e [`config/gateway.example.yaml`](config/gateway.example.yaml) para a referência completa com defaults.

As únicas duas variáveis de ambiente que o processo lê:

| Variável | Efeito |
|---|---|
| `GATEWAY_CONFIG` | Caminho do arquivo de configuração, se nenhum argumento de linha de comando for passado. |
| `RUST_LOG` | Filtro do `tracing`. Sem ela, `info,rust_gateway=info`. |

<h3 id="starting">Rodando</h3>

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

Sem Docker, com o toolchain Rust instalado:

```bash
cargo run -- config/gateway.yaml              # o caminho da config é o argumento
cargo test                                    # 132 testes
REDIS_URL=redis://127.0.0.1:6379 cargo test   # inclui a conformance do store Redis
```

A arquitetura não assume instância única: subir múltiplas réplicas exige apenas
`store: redis` na configuração.

<h2 id="what-it-does">🧠 O que o gateway faz</h2>

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

<h2 id="what-it-doesnt-do">🚧 O que ele deliberadamente não faz</h2>

Emitir tokens, gerir usuários, aplicar autorização de domínio, terminar TLS. A fronteira é:

```
Auth Service     autentica e emite JWT
Rust Gateway     valida o token e aplica política de rota
Backend          aplica autorização específica de domínio
```

<h2 id="config-schema">🗺️ Roteamento e políticas</h2>

Rotas e políticas em um único arquivo YAML — não há endpoints de negócio fixos no gateway em si; cada rota é definida pela configuração:

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

<h2 id="routes">📍 Endpoints administrativos</h2>

Diferente das rotas de negócio (dinâmicas, definidas pela configuração), estes três endpoints são fixos e vivem em `server.admin_bind` (porta `9090` por padrão) — um listener separado do público, que não passa pela pilha de políticas e não é publicado no Compose de propósito.

| Rota | Descrição |
|---|---|
| <kbd>GET /health</kbd> | liveness — o processo está servindo, veja [detalhes](#get-health-detail) |
| <kbd>GET /ready</kbd> | readiness — config carregada e JWKS utilizável, veja [detalhes](#get-ready-detail) |
| <kbd>GET /metrics</kbd> | métricas Prometheus, veja [detalhes](#get-metrics-detail) |

<h3 id="get-health-detail">GET /health</h3>

Não checa dependência alguma — só responde se o processo está de pé.

**RESPOSTA**
```
200 OK
ok
```

<h3 id="get-ready-detail">GET /ready</h3>

**RESPOSTA (pronto)**
```
200 OK
ready
```

**RESPOSTA (não pronto)**
```
503 Service Unavailable
not ready: cache de JWKS indisponível ou fora da janela stale
```

<h3 id="get-metrics-detail">GET /metrics</h3>

**RESPOSTA**
```
# HELP gateway_requests_total Requisições atendidas pelo gateway
# TYPE gateway_requests_total counter
gateway_requests_total{method="GET",outcome="ok",route="users",status="200"} 4
gateway_request_duration_seconds_count{route="users"} 4
gateway_upstream_duration_seconds_count{upstream="user-service"} 4
gateway_circuit_state{upstream="user-service"} 0
```

Consulte de dentro da rede, sem expor a porta administrativa:

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics'
```

A superfície completa de métricas — `outcome`s, labels, e o porquê da regra de cardinalidade — está em [`docs/reference/observability.md`](docs/reference/observability.md).

<h2 id="tracing">🔭 Rastreamento distribuído</h2>

```bash
docker compose -f docker-compose.yml -f docker-compose.tracing.yml up --build
```

Sobe um Jaeger com o receptor OTLP ligado, e troca a configuração do gateway por uma
com `tracing.otlp_endpoint` apontando para ele. A UI fica em `localhost:16686`. A
propagação de `traceparent` W3C funciona com ou sem este overlay — ele só liga o
destino dos spans; veja [tracing distribuído](docs/reference/observability.md#tracing-distribuído).

<h2 id="benchmarks">📊 Métricas de referência</h2>

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

<h2 id="docs">📚 Documentação</h2>

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

### Stack

Rust · Tokio · Hyper · Axum · Tower · Redis · Prometheus · OpenTelemetry · Docker

<h2 id="colab">🤝 Colaboradores</h2>

<table>
  <tr>
    <td align="center">
      <a href="https://github.com/JoaoAzevedo184">
        <img src="https://github.com/JoaoAzevedo184.png" width="100px;" alt="Foto de perfil de JoaoAzevedo184"/><br>
        <sub>
          <b>JoaoAzevedo184</b>
        </sub>
      </a>
    </td>
  </tr>
</table>

<h2 id="contribute">📫 Contribua</h2>

1. Fork e clone o repositório.
2. Crie uma branch a partir de `main`: `git checkout -b feat/nome-da-mudanca`.
3. Antes de commitar, rode a verificação local completa:
   ```bash
   cargo fmt
   cargo clippy --all-targets
   cargo test
   ```
4. Siga o padrão de commit já usado no histórico: [Conventional Commits](https://gist.github.com/joshbuchea/6f47e86d2510bce28f8e7f42ae84c716) (`feat:`, `fix:`, `refactor:`, `docs:`, ...), em português, descrevendo o quê e o porquê.
5. Abra um [Pull Request](https://www.atlassian.com/br/git/tutorials/making-a-pull-request) explicando o problema resolvido ou a funcionalidade adicionada. Se a mudança tocar `docs/`, mantenha o quadrante certo do [Diátaxis](https://diataxis.fr/) — não é tudo que é "explicação".
