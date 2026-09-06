# Rust Gateway — Design

**Data:** 2026-09-06
**Status:** Aprovado
**Escopo desta spec:** o gateway completo. O plano de implementação que a segue cobre apenas a Fase 1.

---

## 1. Contexto e objetivos

O Rust Gateway é o único ponto de entrada exposto de um conjunto de serviços internos
(User Service, Order Service, Payment Service), escritos em Java e Python, que se comunicam
por HTTP/REST e vivem na rede interna do Docker Compose.

O gateway concentra o que não deve ser reimplementado em cada serviço: roteamento, validação
de JWT, rate limiting, resiliência, correlação de requisições e métricas.

**Objetivos:**

- Robustez suficiente para uso real em um homelab, não apenas demonstração.
- Preparar migração para Kubernetes e, depois, cloud/VPS, sem reescrever o gateway.
- Servir de veículo de aprendizado de Rust assíncrono, Axum/Tower, observabilidade e
  padrões de resiliência.

**Ambiente inicial:** Docker Compose, uma réplica do gateway. O design não assume instância
única em nenhum ponto.

## 2. Escopo

**Dentro:**

- Proxy reverso HTTP/1.1 com roteamento por prefixo configurado em arquivo.
- Validação de JWT por assinatura assimétrica com JWKS em cache.
- Rate limiting token bucket, distribuído, configurável por rota.
- Circuit breaker, retry e timeouts.
- Correlation ID, logs estruturados, métricas Prometheus, tracing OpenTelemetry.
- Health e readiness.

**Fora:**

- Emissão de tokens, gestão de usuários ou acesso a banco de usuários. Isso é do Auth Service.
- Autorização específica de domínio. Isso é dos serviços de backend.
- gRPC, WebSocket e service discovery dinâmico. A spec registra os pontos de extensão que
  os tornam aditivos; nada além disso.
- TLS terminado no gateway. Na topologia inicial, TLS é responsabilidade do reverse proxy
  de borda, quando existir.

**Fronteira de responsabilidade:**

```
Auth Service     autentica e emite JWT
Rust Gateway     valida token e aplica política de rota
Backend          aplica autorização específica de domínio
```

## 3. Decisões arquiteturais

Cada decisão abaixo foi tomada explicitamente; o racional está registrado porque o custo de
reverter uma delas sem entender o motivo é alto.

| # | Decisão | Racional |
|---|---|---|
| D1 | Políticas globais em layers fixos; políticas de rota dirigidas por configuração | Layers globais precisam envolver inclusive requisições rejeitadas (404, 401). Stacks pré-compiladas por rota não veem a requisição que nunca chegou à rota. |
| D2 | Snapshot de config imutável e substituível (`Arc<RouterTable>`) | Torna hot reload e discovery dinâmico aditivos: um provider novo empurra um snapshot, o caminho da requisição não muda. |
| D3 | Estado vivo fora do snapshot | Buckets de rate limit e estado de circuit breaker vivem em registries no `AppState`. Se vivessem no snapshot, um reload de config zeraria o circuit breaker no meio de um incidente. |
| D4 | Casamento por prefixo mais longo, sem matcher de padrões | Cobre o caso real e resolve exceções anônimas de graça: `/users/signup` vence `/users`. |
| D5 | Upstreams nomeados, não URLs inline | Estado de circuit breaker é por upstream. Rotas distintas para o mesmo serviço precisam compartilhar o breaker, senão o circuito abre N vezes e não protege ninguém. |
| D6 | JWT falha fechado; rate limit falha aberto | Token não validado é falha de segurança. Rate limit é proteção: derrubar tráfego legítimo porque o Redis caiu é pior que o abuso evitado. |
| D7 | Retry só em métodos idempotentes e só em falha pré-resposta | Retry de `POST /payments` após timeout de leitura pode cobrar duas vezes. Seguro por padrão, sem depender de configuração correta. |
| D8 | Estado de circuit breaker local à réplica; estado de rate limit compartilhado | Rate limit é cota do usuário, precisa somar entre réplicas. Breaker é observação sobre a conexão daquela réplica com aquele upstream; compartilhá-lo faz o problema de rede de uma réplica abrir o circuito para todas, e coloca o Redis no caminho crítico. |
| D9 | Scrub de headers de identidade é global e incondicional | Rota anônima não roda auth. Se o scrub morasse no layer de auth, uma rota anônima repassaria `X-User-Id` forjado pelo cliente. |
| D10 | Crate único com módulos, não workspace | Workspace se paga ao forçar aciclicidade entre times ou publicar partes; aqui só pioraria o tempo de compilação. Promover módulo a crate depois é mecânico. |

## 4. Arquitetura

### 4.1 Pilha de processamento

```
Request
  │
  ├─ correlation_id        globais: sempre ativos, envolvem inclusive requisições rejeitadas
  ├─ identity_scrub
  ├─ tracing_span
  ├─ metrics
  ├─ body_limit
  │
  ├─ route_resolve         injeta Arc<RouteRuntime> nas extensions, ou 404
  │
  ├─ auth                  por rota: leem RouteRuntime, no-op quando a rota não pede
  ├─ rate_limit
  ├─ retry
  │   └─ circuit_breaker
  │       └─ upstream_timeout
  │
  └─ proxy → hyper client → upstream
```

**Ordem interna, e por quê:**

- `rate_limit` fora de `retry`: uma tentativa extra não pode consumir um segundo token.
- `circuit_breaker` dentro de `retry`: registra o resultado de cada tentativa em vez de um
  agregado por requisição, e corta a segunda tentativa imediatamente quando o circuito abre.
- `upstream_timeout` é o mais interno: aplica-se a uma tentativa.

### 4.2 Módulos

```
src/
  main.rs              bootstrap: carrega config, monta estado, sobe os dois listeners
  state.rs             AppState: watch<Arc<RouterTable>> + registries compartilhados
  config/
    mod.rs             tipos serde + validação de startup
    provider.rs        trait ConfigProvider, FileProvider
  routing/
    table.rs           RouterTable: matching por prefixo mais longo
    runtime.rs         RouteRuntime: política resolvida + handles de estado
  proxy/
    mod.rs             cliente hyper, forwarding, hop-by-hop, X-Forwarded-*
  auth/
    layer.rs           layer de auth
    jwks.rs            cache de JWKS, refresh, lookup por kid
    claims.rs          validação de exp/nbf/iss/aud/scopes
  ratelimit/
    layer.rs  bucket.rs  store.rs  memory.rs  redis.rs
  resilience/
    breaker.rs  retry.rs  timeout.rs
  observability/
    correlation.rs  metrics.rs  tracing.rs  health.rs
  clock.rs             trait Clock: real e de teste
```

`state.rs` carrega o peso da arquitetura: mantém o `watch::Receiver<Arc<RouterTable>>` do
snapshot e os registries de estado vivo, chaveados por `route_id` e por `upstream_id`.

## 5. Configuração

### 5.1 Schema

```yaml
server:
  bind: 0.0.0.0:8080
  admin_bind: 0.0.0.0:9090
  request_timeout: 30s          # teto absoluto por requisição, inclui retries
  max_body_bytes: 2MiB
  trusted_proxies: []           # vazio = ignora X-Forwarded-For, usa peer addr

auth:
  jwks_url: http://auth-service:9000/.well-known/jwks.json
  issuer: https://auth.homelab.local
  audience: rust-gateway
  refresh_interval: 5m          # refresh proativo em background
  stale_max_age: 30m            # janela de aceitação de cache stale
  leeway: 30s                   # tolerância de clock em exp/nbf
  unknown_kid_cooldown: 30s     # intervalo mínimo entre refreshes forçados
  scope_claim: scope

rate_limit:
  store: redis                  # memory | redis
  redis_url: redis://redis:6379
  redis_timeout: 50ms

resilience:
  default:
    connect_timeout: 2s
    upstream_timeout: 5s
    retry: { max_attempts: 2, backoff: 50ms, jitter: true }
    circuit_breaker:
      failure_ratio: 0.5
      min_requests: 20
      window: 30s
      open_for: 15s
      half_open_probes: 1

upstreams:
  user-service:    { url: http://user-service:8080 }
  order-service:   { url: http://order-service:8081 }
  payment-service:
    url: http://payment-service:8082
    resilience:
      upstream_timeout: 15s
      retry: { max_attempts: 0 }        # pagamento nunca retria

routes:
  - id: users
    match: { prefix: /users }
    upstream: user-service
    strip_prefix: false
    auth: { required: true, scopes: [user.read] }
    rate_limit:
      - { key: sub, capacity: 600,  refill_per_sec: 10 }
      - { key: ip,  capacity: 2000, refill_per_sec: 50 }   # teto bruto adicional

  - id: users-signup
    match: { prefix: /users/signup }
    upstream: user-service
    auth: { required: false }
    rate_limit:
      - { key: ip, capacity: 5, refill_per_sec: 0.1 }

  - id: orders
    match: { prefix: /orders }
    upstream: order-service
    strip_prefix: true            # /orders/42 -> upstream /42
    auth: { required: true, scopes: [order.read] }

  - id: payments
    match: { prefix: /payments }
    upstream: payment-service
    auth: { required: true, scopes: [payment.write] }
    rate_limit:
      - { key: sub, capacity: 30, refill_per_sec: 0.5 }
```

**Herança de política:** `resilience.default` → `upstreams.<id>.resilience` →
`routes[].resilience`. O nível mais específico vence, campo a campo.

**`rate_limit` é uma lista.** Todos os limites da rota precisam passar. É assim que
"limitada por `sub`, com teto bruto por IP" cabe sem mecanismo especial.

### 5.2 `trusted_proxies`

Rate limit por IP que confia em `X-Forwarded-For` sem validar a origem é trivialmente
burlável: o cliente forja o header e ganha um bucket novo por requisição.

Por padrão o gateway **ignora** `X-Forwarded-For` e usa o endereço do peer. Só lê o header
quando o peer está em `trusted_proxies`, e nesse caso toma o último endereço não confiável
da cadeia. No Compose inicial a lista fica vazia, porque só o gateway é exposto.

### 5.3 Validação de startup

Configuração inválida impede o processo de subir. Falhas duras:

- Referência a upstream inexistente.
- `id` de rota duplicado.
- `match.prefix` duplicado.
- `store: redis` sem `redis_url`.
- `jwks_url`, `issuer` ou URL de upstream malformadas.
- Rota sem bloco `auth`. Deve ser explícito, mesmo para declarar `required: false`.
- `key: sub` em rota com `auth.required: false` — ambiguidade, não fallback silencioso.
- `request_timeout` menor que `upstream_timeout` — o teto global cortaria antes da
  tentativa individual, o que é sempre engano.

### 5.4 Carga e substituição

`ConfigProvider` produz `RouterTable` validadas. Na Fase 1 existe apenas `FileProvider`,
que lê o arquivo no boot. O provider publica em um `tokio::sync::watch`; o caminho da
requisição só lê o `Arc` corrente.

Hot reload e discovery dinâmico entram depois como providers adicionais, sem alterar o
caminho da requisição.

## 6. Roteamento

Casamento por **prefixo mais longo**, com fronteira de segmento obrigatória: `/users` casa
`/users` e `/users/42`, e **não** casa `/usersecret`. A ausência dessa checagem é a diferença
entre uma rota anônima e um vazamento.

Sem casamento, 404.

`strip_prefix: true` remove o prefixo da rota antes de encaminhar; a query string é sempre
preservada intacta. O padrão é `false`.

`auth` é obrigatório em toda rota, inclusive para declarar `required: false`. Omitir o bloco
é erro de validação de startup: uma rota que ficou pública por esquecimento é a falha mais
cara que este schema pode permitir.

`RouteRuntime` é o resultado da resolução: política de auth, lista de limites, política de
resiliência já herdada e resolvida, handle do breaker do upstream, e o `route_id` usado como
label de métrica.

## 7. Proxy e headers

**Cliente:** `hyper-util::client::legacy::Client` com `HttpConnector` e pool de conexões por
upstream. Escolhido em vez de `reqwest` por controle explícito sobre pooling e timeouts.

**Corpos são streaming por padrão.** O gateway não bufferiza requisição nem resposta.
Bufferizar significaria que um upload de 100 MB vira 100 MB de RAM por requisição concorrente.

**Exceção para retry:** rota com retry habilitado bufferiza o corpo da requisição até
`max_body_bytes`. Corpo maior que isso segue normalmente, mas perde elegibilidade a retry.
Como retry só vale para métodos idempotentes, e o caso dominante (GET/HEAD/DELETE) não tem
corpo, quase nada é bufferizado na prática.

**Na ida:**

- Remove hop-by-hop: `Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`,
  `TE`, `Trailers`, `Transfer-Encoding`, `Upgrade`, e todo header **listado dentro** do
  `Connection`.
- Reescreve `Host` para a autoridade do upstream.
- Acrescenta `X-Forwarded-For` (append do peer), `X-Forwarded-Proto`, `X-Forwarded-Host`.
- Preserva o `Authorization` original.

**Na volta:** mesma remoção de hop-by-hop antes de devolver ao cliente.

### 7.1 Scrub de identidade

Global e incondicional, antes da resolução de rota. Remove de toda requisição de entrada:
`X-User-Id`, `X-User-Scopes`, e qualquer header com prefixo `X-Auth-`.

A regra resultante é auditável em uma frase: **nenhum header de identidade sobrevive à borda;
os únicos que chegam ao upstream foram escritos pelo gateway.**

### 7.2 Correlation ID

Aceita `X-Request-Id` do cliente se for bem-formado — no máximo 64 caracteres, apenas
alfanuméricos e hífen. Caso contrário, gera um UUIDv7.

A validação não é preciosismo: o id entra em log, e log que aceita string arbitrária aceita
injeção de linha. UUIDv7 em vez de v4 porque é ordenável por tempo, o que mantém os logs
agrupáveis.

O id é propagado para o span de tracing, para o upstream, para o header de resposta e para o
corpo de erro.

## 8. Autenticação

O layer roda após a resolução de rota e lê `auth` do `RouteRuntime`. Com `required: false`,
ainda valida um token presente para popular identidade, mas não rejeita quando ausente.

### 8.1 Cache de JWKS

`ArcSwap<JwksSnapshot>` com task de background fazendo refresh a cada `refresh_interval`.
O caminho da requisição só lê; não faz I/O de rede para validar um token, exceto no caso
de `kid` desconhecido.

**`kid` desconhecido** dispara refresh imediato com *single-flight* (requisições concorrentes
esperam o mesmo refresh, não N chamadas ao Auth Service) e cooldown de `unknown_kid_cooldown`.
Sem o refresh forçado, toda rotação de chave causaria até `refresh_interval` de 401 em massa;
sem o cooldown, `kid` aleatório em loop viraria vetor de DoS contra o Auth Service. Se após o
refresh o `kid` continuar desconhecido, 401.

**Cache stale:** com o refresh falhando, o snapshot anterior continua válido até
`stale_max_age`. Passando disso, 503. É o que faz um restart do Auth Service não derrubar o
tráfego autenticado.

### 8.2 Validação

Nesta ordem:

1. **Algoritmo** — allowlist derivada da própria JWKS (RS256/ES256). O `alg` do header do
   token não decide nada; `none` é rejeitado. Esta é a vulnerabilidade clássica de JWT.
2. **Assinatura**, pela chave correspondente ao `kid`.
3. **`exp` e `nbf`**, com `leeway` de tolerância de clock.
4. **`iss` e `aud`**, comparação exata contra a configuração.
5. **Scopes** exigidos pela rota.

### 8.3 Scopes

Lê a claim configurada em `scope_claim` no formato OAuth2 (string separada por espaço) e,
quando presente, também `roles` como array.

A rota exige **todos** os scopes listados — conjunção. Menos surpreendente do que disjunção
quando alguém adiciona um scope à lista achando que está restringindo.

### 8.4 Headers injetados

`X-User-Id` (do `sub`), `X-User-Scopes` (separado por espaço), `X-Auth-Method: jwt`, mais o
`Authorization` original intacto. Como o scrub global já rodou, esses headers são
inequivocamente do gateway.

## 9. Rate limiting

### 9.1 Abstração

```rust
#[async_trait]
trait RateLimitStore: Send + Sync {
    async fn try_acquire(&self, buckets: &[BucketRequest]) -> Result<Decision, StoreError>;
}
```

A operação é `try_acquire`, não `get`/`set`: **a matemática do token bucket roda dentro do
store**. Se o gateway lesse o contador, calculasse e escrevesse de volta, duas réplicas
atendendo o mesmo usuário simultaneamente leriam o mesmo valor e ambas deixariam passar — o
limite viraria decorativo exatamente sob a carga em que importa.

### 9.2 Implementação Redis

Script Lua, uma round trip por requisição. Cada bucket é um hash com `tokens` e
`last_refill_ms`; o script recalcula o refill preguiçosamente pelo tempo decorrido, deduz e
retorna a decisão.

- **O relógio vem do `TIME` do Redis**, não do gateway. Réplicas com clock dessincronizado
  produziriam refills inconsistentes sobre o mesmo bucket.
- **TTL igual ao tempo de encher o bucket do zero.** Sem isso, todo IP que já passou pelo
  gateway fica residente no Redis para sempre.
- **Todos os buckets da rota são avaliados no mesmo script, tudo-ou-nada.** O script verifica
  todos antes de deduzir qualquer um. Sem isso, uma rota com dois limites drenaria o primeiro
  bucket enquanto rejeita no segundo.

**Formato de chave:** `rl:v1:<route_id>:<kind>:<valor>`. O `route_id` mantém limites por rota
independentes; o `v1` permite mudar o formato do bucket sem migração.

### 9.3 Implementação in-memory

Mesma semântica, mutex por shard. Usada em desenvolvimento e testes. **Não recomendada para
múltiplas réplicas**, porque cada réplica conta separadamente e o limite efetivo vira N vezes
o configurado.

### 9.4 Degradação

Conforme D6, falha aberto. Três peças:

- Timeout agressivo (`redis_timeout`, padrão 50ms). Sem ele, um Redis lento vira latência em
  toda requisição.
- Breaker sobre o próprio store: enquanto o Redis está fora, o gateway para de tentar a cada
  requisição em vez de pagar o timeout sempre.
- Cada requisição degradada incrementa `gateway_ratelimit_degraded_total`; o log de warn é
  throttled, porque log por requisição durante uma queda de Redis é o segundo incidente.

### 9.5 Headers de resposta

`X-RateLimit-Limit` e `X-RateLimit-Remaining` em toda resposta de rota limitada.
`Retry-After` no 429, calculado a partir da taxa de refill.

## 10. Resiliência

### 10.1 Circuit breaker

Por upstream, estado local à réplica (D8). Registry no `AppState`, chaveado por `upstream_id`.

**Contam como falha:** erro de conexão, timeout e 5xx do upstream.
**Não contam:** respostas 4xx. Uma onda de 401 ou 404 é comportamento do cliente; deixá-la
abrir o circuito significa que um cliente mal configurado derruba o serviço para todos.

Janela deslizante por tempo (`window`), com `min_requests` antes de qualquer avaliação — sem
isso, a primeira requisição do dia falhando abre o circuito com 100% de taxa de erro.

**Estados:** `Closed` → `Open` quando a taxa de falha excede `failure_ratio` com pelo menos
`min_requests` na janela. `Open` → `HalfOpen` após `open_for`. Em `HalfOpen`, no máximo
`half_open_probes` sondas concorrentes: sucesso fecha, falha reabre.

Circuito aberto responde 503 com `Retry-After`.

### 10.2 Retry

Conforme D7:

- Métodos elegíveis: GET, HEAD, OPTIONS, PUT, DELETE. Nunca POST.
- Condições elegíveis: falha comprovadamente pré-resposta — recusa de conexão, falha de DNS,
  reset antes dos headers de resposta. Nunca após o upstream começar a responder, nunca em
  timeout de leitura.
- Backoff com **jitter**. Backoff fixo faz todas as requisições que falharam juntas retentarem
  juntas, e o thundering herd chega no upstream que ainda está se recuperando.
- `max_attempts: 0` desabilita retry para a rota ou upstream.

### 10.3 Timeouts

| Nível | Escopo | Padrão |
|---|---|---|
| `connect_timeout` | estabelecer a conexão TCP | 2s |
| `upstream_timeout` | uma tentativa, do request à resposta completa | 5s |
| `request_timeout` | teto absoluto da requisição, incluindo todos os retries | 30s |

Confundir os três é como se produz um gateway que trava. A validação de startup rejeita
`request_timeout` menor que `upstream_timeout`.

## 11. Observabilidade e health

### 11.1 Listener administrativo

`/metrics`, `/health` e `/ready` ficam em `admin_bind` (porta 9090), separados do listener
público. Não colidem com prefixo de rota, não passam pela pilha de políticas — um `/metrics`
que exige JWT é inútil para o Prometheus — e no Compose a porta simplesmente não é publicada.

### 11.2 Métricas

```
gateway_requests_total{route, method, status, outcome}
gateway_request_duration_seconds{route}        # total, borda a borda
gateway_upstream_duration_seconds{upstream}    # só o tempo do upstream
gateway_upstream_inflight{upstream}
gateway_ratelimit_decisions_total{route, key_kind, decision}
gateway_ratelimit_degraded_total
gateway_circuit_state{upstream}                # 0=closed 1=half_open 2=open
gateway_circuit_transitions_total{upstream, to}
gateway_retries_total{route, result}
gateway_auth_failures_total{reason}
gateway_jwks_cache_age_seconds
```

`outcome` assume: `ok`, `rejected_auth`, `rejected_ratelimit`, `upstream_error`,
`circuit_open`, `timeout`.

Os dois histogramas separados respondem "a lentidão é do gateway ou do upstream?" — a
diferença entre eles é o overhead real do gateway.

**Regra de cardinalidade:** o label é sempre `route_id`, valor vindo da configuração e
portanto limitado. Nunca o path bruto, nunca `sub`, nunca IP. Um label de path cru transforma
um scanner de diretórios em um incidente de memória no Prometheus.

### 11.3 Health e readiness

São coisas diferentes, e confundi-las causa loop de restart.

- **`/health` (liveness):** o processo está servindo. Não checa dependência alguma. Se
  checasse o Auth Service, uma queda dele faria o orquestrador matar e reiniciar gateways
  saudáveis, transformando degradação parcial em queda total.
- **`/ready`:** configuração carregada e cache de JWKS utilizável (fresco ou dentro da janela
  stale). **Redis não entra**: como o rate limit falha aberto, um Redis fora não torna o
  gateway incapaz de servir.

**Saúde de upstream é passiva**, observada pelo circuit breaker sobre o tráfego real. Sem
probes ativos: probe custa scheduler e mente com frequência, respondendo 200 em `/health`
enquanto as requisições reais falham.

### 11.4 Tracing e logs

`tracing` + `tracing-subscriber` com saída JSON. Um span por requisição carregando correlation
id, `route_id` e `sub`.

`tracing-opentelemetry` exportando OTLP, com **propagação de `traceparent` W3C**: o gateway
aceita o contexto de trace que chega e continua o mesmo trace no upstream. É o que faz um
trace atravessar Rust → Java → Python em vez de virar três traces desconexos.

`X-Request-Id` e `traceparent` convivem: um para humano lendo log, outro para o sistema de
tracing.

## 12. Erros

Toda resposta de erro gerada pelo gateway usa o mesmo corpo JSON, com o correlation id dentro:

```json
{
  "error": "rate_limited",
  "message": "Request rate exceeded for this route",
  "request_id": "01924f8e-..."
}
```

| Situação | Status |
|---|---|
| Prefixo não casa | 404 |
| Token ausente, inválido ou expirado | 401 |
| Token válido, scope insuficiente | 403 |
| Rate limit estourado | 429 + `Retry-After` |
| Falha ao conectar no upstream | 502 |
| Circuito aberto | 503 + `Retry-After` |
| JWKS indisponível além do `stale_max_age` | 503 |
| Timeout do upstream | 504 |

Respostas produzidas pelo upstream são repassadas sem alteração de corpo.

## 13. Estratégia de testes

**Relógio injetável** (`trait Clock`) em tudo que depende de tempo: token bucket, circuit
breaker, cache de JWKS. Sem isso os testes viram `sleep`, e uma suíte que dorme é uma suíte
que se deixa de rodar.

**Unitários:** matemática do bucket (refill, burst, esgotamento), matcher de prefixo, scrub de
headers, validação de claims, herança de política de resiliência.

Um caso do matcher merece teste dedicado: prefixo casa em fronteira de segmento. `/users`
casa `/users/42` e não casa `/usersecret`.

**Por layer:** cada layer de política é testado injetando um `RouteRuntime` nas extensions e
um inner service stub, sem levantar router nem servidor. É o benefício direto de D1.

**Conformance suite compartilhada para `RateLimitStore`:** um único conjunto de testes rodado
contra a implementação in-memory e contra o Redis (testcontainers). Garante que trocar de
store não muda comportamento.

**Integração** com upstream `wiremock` e Auth Service falso: par de chaves RSA gerado no
teste, servindo JWKS e assinando tokens. Cobre token válido, expirado, `aud` errada, scope
faltando, `X-User-Id` forjado sendo removido, 429 no limite, circuito abrindo após N 5xx, e
POST não sendo retriado.

**Carga:** `oha` ou `k6` para RPS, P50, P95, P99, CPU e RAM.

## 14. Fases de entrega

### Fase 1 — núcleo

Configuração, validação, `FileProvider` e snapshot; `RouterTable`; proxy com higiene de
headers, scrub de identidade e correlation ID; autenticação completa com JWKS; rate limiting
com as duas implementações de store; listener administrativo com health e readiness; logs
JSON estruturados e o subconjunto mínimo de métricas (`gateway_requests_total`,
`gateway_request_duration_seconds`, `gateway_ratelimit_degraded_total`); Docker Compose com
gateway, Redis e upstreams stub.

Ao fim da Fase 1 o gateway é utilizável em produção doméstica.

### Fase 2 — resiliência

Os três timeouts, retry e circuit breaker. Precede a observabilidade completa porque robustez
é a prioridade declarada.

### Fase 3 — observabilidade completa

Superfície inteira de métricas, OpenTelemetry com propagação de `traceparent`, benchmarks de
carga.

### Além do escopo desta spec

gRPC, service discovery dinâmico e hot reload de configuração. Os pontos de extensão que os
tornam aditivos estão registrados na seção 15.

## 15. Pontos de extensão

| Extensão futura | Ponto de extensão | O que muda |
|---|---|---|
| Hot reload de config | `ConfigProvider` + snapshot em `watch` | Um provider que observa o arquivo e publica novo snapshot. Caminho da requisição inalterado. |
| Service discovery dinâmico | `ConfigProvider` | Um provider que consulta Consul/DNS/API do K8s e publica snapshots. Mesma interface. |
| Backend alternativo de rate limit | `RateLimitStore` | Nova implementação, validada pela conformance suite existente. |
| gRPC | Camada de proxy | Requer suporte a HTTP/2 end-to-end no cliente e no listener. Roteamento, auth e rate limit são reutilizáveis. |
| Múltiplas réplicas | Já suportado | Trocar `store: memory` por `store: redis`. Nenhuma outra mudança. |

## 16. Riscos conhecidos

- **Rate limit falha aberto (D6).** Durante uma queda de Redis, o gateway fica sem proteção
  de taxa. Mitigação: métrica e alerta sobre `gateway_ratelimit_degraded_total`.
- **Breaker local à réplica (D8).** Com N réplicas, o upstream pode receber até N vezes o
  tráfego de sondagem em `HalfOpen`. Aceitável com `half_open_probes: 1` e poucas réplicas.
- **Aceitação de cache stale de JWKS (8.1).** Uma chave revogada continua aceita até
  `stale_max_age` se o Auth Service estiver inacessível. Trade-off deliberado contra
  indisponibilidade total; ajustável reduzindo `stale_max_age`.
- **Retry bufferiza corpo até `max_body_bytes`.** Requisições idempotentes com corpo grande
  perdem elegibilidade a retry silenciosamente. Observável via `gateway_retries_total`.
