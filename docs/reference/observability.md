# Referência de observabilidade

## Listener administrativo

`/health`, `/ready` e `/metrics` vivem em `server.admin_bind` (9090 por padrão), separados do listener público.

Não colidem com prefixo de rota, não passam pela pilha de políticas — um `/metrics` que exige JWT é inútil para o Prometheus — e no Compose a porta simplesmente não é publicada.

| Endpoint | Método | Resposta |
|---|---|---|
| `/health` | GET | `200` com corpo `ok` |
| `/ready` | GET | `200` com `ready`, ou `503` com `not ready: <motivo>` |
| `/metrics` | GET | `200` em `text/plain; version=0.0.4` |

### `/health` — liveness

O processo está servindo. **Não checa dependência alguma.**

Se checasse o Auth Service, uma queda dele faria o orquestrador matar e reiniciar gateways saudáveis, transformando degradação parcial em queda total.

### `/ready` — readiness

Responde `200` quando a configuração está carregada com pelo menos uma rota **e** o cache de JWKS está utilizável — fresco ou dentro de `stale_max_age`. Sem seção `auth` configurada, só a primeira condição vale.

Motivos possíveis de `503`:

| Motivo | Significado |
|---|---|
| `nenhuma rota carregada` | O snapshot de configuração está vazio. |
| `cache de JWKS indisponível ou fora da janela stale` | Nenhum fetch bem-sucedido, ou o último passou de `stale_max_age`. |

**Redis não entra.** Como o rate limit falha aberto, um Redis fora não torna o gateway incapaz de servir, e um `/ready` que dissesse o contrário tiraria a réplica do balanceador sem motivo.

## Métricas

Superfície completa. Todos os labels de rota e upstream usam `route_id`/`upstream_id` vindos da configuração — veja a regra de cardinalidade ao final.

### `gateway_requests_total`

Contador. Labels: `route`, `method`, `status`, `outcome`.

```
gateway_requests_total{method="GET",outcome="ok",route="users",status="200"} 4
gateway_requests_total{method="GET",outcome="rejected_ratelimit",route="payments",status="429"} 4
gateway_requests_total{method="GET",outcome="no_route",route="unmatched",status="404"} 2
```

Valores de `outcome`:

| Valor | Situação |
|---|---|
| `ok` | O upstream respondeu, com status abaixo de 500. |
| `no_route` | Nenhum prefixo casou. |
| `rejected_auth` | 401 ou 403 do layer de autenticação. |
| `rejected_ratelimit` | 429. |
| `upstream_error` | Falha de conexão, ou 5xx vindo do upstream. |
| `circuit_open` | 503 recusado pelo circuit breaker, sem tentativa ao upstream. |
| `timeout` | 504 por `upstream_timeout` ou `request_timeout`. |

### `gateway_request_duration_seconds`

Histograma. Label: `route`. Duração total, borda a borda, incluindo requisições rejeitadas.

### `gateway_upstream_duration_seconds`

Histograma. Label: `upstream`. Duração só da chamada ao upstream — sem o tempo gasto nos layers do gateway antes de chegar lá.

A diferença entre os dois histogramas responde "a lentidão é do gateway ou do upstream?": se `gateway_request_duration_seconds` cresce mas `gateway_upstream_duration_seconds` não, o tempo está sendo gasto no próprio gateway — rate limit degradado, ou um layer travando.

### `gateway_upstream_inflight`

Gauge. Label: `upstream`. Requisições em voo para o upstream neste instante — sobe antes da chamada, desce depois, inclusive no caminho de erro.

Útil para distinguir "o upstream está lento" (inflight alto, duration alto) de "o upstream está recusando rápido" (inflight baixo, duration baixo, `upstream_error` alto).

### `gateway_ratelimit_degraded_total`

Contador sem labels. Incrementa uma vez por requisição que passou **sem** decisão de rate limit, por indisponibilidade do store.

É a métrica de alerta do risco assumido em D6: enquanto ela sobe, o gateway está sem proteção de taxa.

### `gateway_ratelimit_decisions_total`

Contador. Labels: `route`, `key_kind` (`sub` ou `ip`), `decision` (`allowed` ou `rejected`).

```
gateway_ratelimit_decisions_total{decision="allowed",key_kind="ip",route="users"} 118
gateway_ratelimit_decisions_total{decision="rejected",key_kind="ip",route="users"} 3
```

Uma rota com limites por `sub` e por `ip` ao mesmo tempo gera uma linha para cada `key_kind`: a decisão é tudo-ou-nada (todos os buckets da rota se avaliam juntos), mas o label continua separado para diferenciar quantas decisões envolveram cada tipo de chave.

### `gateway_circuit_state`

Gauge. Label: `upstream`. Estado corrente do circuito: `0=closed 1=half_open 2=open`.

Computado a cada scrape a partir do estado vivo do breaker — não é incrementado, é lido. Um upstream que nunca recebeu tráfego não aparece: não há "estado padrão" a inventar para um circuito ocioso.

### `gateway_circuit_transitions_total`

Contador. Labels: `upstream`, `to` (o estado de destino: `closed`, `half_open` ou `open`).

Incrementa uma vez por transição real — `Closed → Closed` não conta, só mudanças de fato.

### `gateway_retries_total`

Contador. Labels: `route`, `result` (`success` ou `failure`).

Conta **tentativas além da primeira**. Uma requisição que precisou de duas repetições e só a terceira tentativa teve sucesso incrementa duas vezes: uma `failure` (a segunda tentativa) e uma `success` (a terceira). O contador mede tentativas gastas, não requisições.

### `gateway_auth_failures_total`

Contador. Label: `reason`.

| Valor | Situação |
|---|---|
| `missing` | Rota exige token, nenhum foi enviado. |
| `malformed` | Header do JWT ilegível, ou sem `kid`. |
| `unknown_kid` | `kid` não encontrado na JWKS, mesmo após refresh forçado. |
| `invalid` | Assinatura, `exp`, `nbf`, `iss` ou `aud` inválidos. |
| `missing_subject` | Token sem claim `sub`. |
| `jwks_unavailable` | Token presente, mas a JWKS está indisponível para validá-lo. |
| `insufficient_scope` | Token válido, mas sem os escopos exigidos pela rota (403, não 401). |

### `gateway_jwks_cache_age_seconds`

Gauge sem labels. Idade do snapshot corrente de JWKS, em segundos desde o último fetch bem-sucedido.

Só aparece quando há seção `auth` configurada **e** pelo menos um fetch já teve sucesso. Sem isso a métrica simplesmente não é emitida — uma idade de `0` seria lida como "cache fresco", o oposto de "nunca buscado".

### Regra de cardinalidade

O label de rota é sempre o `route_id` vindo da configuração, e o de upstream é sempre o `upstream_id`. Ambos limitados pela configuração. Nunca o path bruto, nunca `sub`, nunca IP.

Requisições sem rota usam o valor constante `unmatched`. Um label de path cru transformaria um scanner de diretórios em um incidente de memória no Prometheus.

## Logs

Saída JSON estruturada, uma linha por evento. `RUST_LOG` controla o filtro; sem ele, `info`.

```json
{"timestamp":"2026-09-07T01:57:02.569Z","level":"INFO","message":"gateway ouvindo","publico":"0.0.0.0:8080","admin":"0.0.0.0:9090","target":"rust_gateway::server"}
```

Cada requisição abre um span com os campos `request_id`, `method`, `path`, `trace_id` e, quando resolvidos, `route_id`, `sub` e `status`.

```json
{"level":"WARN","message":"tentativa estourou o upstream_timeout","span":{"request_id":"01a0...","route_id":"payments","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}}
```

`trace_id` é o mesmo valor que aparece em um coletor OTLP — é o que permite pular do log direto para o trace correspondente.

O warn de degradação de rate limit é throttled em 10 segundos: log por requisição durante uma queda de Redis é o segundo incidente.

Cada transição do circuit breaker gera uma linha de `WARN`:

```json
{"level":"WARN","message":"circuito mudou de estado","upstream":"payment-service","de":"closed","para":"open"}
```

## Tracing distribuído

`tracing-opentelemetry` com propagação de `traceparent` W3C. `X-Request-Id` e `traceparent` convivem: um para humano lendo log, outro para o sistema de tracing.

### Propagação — sempre ativa

O gateway aceita o `traceparent` que chega e gera um span cujo id vira o *parent-id* do `traceparent` que segue para o upstream. É o que faz um trace atravessar Rust → Java → Python em vez de virar três traces desconexos.

```
cliente          traceparent: 00-4bf9...4736-00f0...02b7-01
  ↓
gateway          span próprio, trace_id 4bf9...4736, parent 00f0...02b7
  ↓ (traceparent reescrito)
upstream         traceparent: 00-4bf9...4736-<span do gateway>-01
```

Sem um `traceparent` de entrada válido, o gateway origina um trace novo — o comportamento correto para o primeiro salto de uma cadeia sem tracing.

A propagação não depende de exportação: acontece mesmo sem `tracing.otlp_endpoint` configurado, porque o span do gateway existe de qualquer forma — só não é enviado a lugar nenhum.

### Exportação — opcional

```yaml
tracing:
  otlp_endpoint: http://otel-collector:4318/v1/traces
```

Ausente, nenhum span sai do processo. Presente, os spans são enviados em lote via OTLP/HTTP (protobuf) para o endpoint informado.

O exportador roda em background e é encerrado de forma graciosa no desligamento (`SIGTERM`/`SIGINT`), esvaziando o lote pendente antes do processo sair.
