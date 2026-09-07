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

A Fase 1 expõe o subconjunto mínimo. A superfície completa da spec entra na Fase 3.

### `gateway_requests_total`

Contador. Labels: `route`, `method`, `status`, `outcome`.

```
gateway_requests_total{method="GET",outcome="ok",route="users",status="200"} 4
gateway_requests_total{method="GET",outcome="rejected_ratelimit",route="payments",status="429"} 4
gateway_requests_total{method="GET",outcome="no_route",route="unmatched",status="404"} 2
```

Valores de `outcome` emitidos na Fase 1:

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

Histograma. Label: `route`. Mede a duração total, borda a borda, incluindo requisições rejeitadas.

Na Fase 3 entra `gateway_upstream_duration_seconds`, e a diferença entre os dois passa a responder "a lentidão é do gateway ou do upstream?".

### `gateway_ratelimit_degraded_total`

Contador sem labels. Incrementa uma vez por requisição que passou **sem** decisão de rate limit, por indisponibilidade do store.

É a métrica de alerta do risco assumido em D6: enquanto ela sobe, o gateway está sem proteção de taxa.

### Regra de cardinalidade

O label de rota é sempre o `route_id` vindo da configuração, e portanto limitado. Nunca o path bruto, nunca `sub`, nunca IP.

Requisições sem rota usam o valor constante `unmatched`. Um label de path cru transformaria um scanner de diretórios em um incidente de memória no Prometheus.

## Logs

Saída JSON estruturada, uma linha por evento. `RUST_LOG` controla o filtro; sem ele, `info`.

```json
{"timestamp":"2026-09-07T01:57:02.569Z","level":"INFO","message":"gateway ouvindo","publico":"0.0.0.0:8080","admin":"0.0.0.0:9090","target":"rust_gateway::server"}
```

Cada requisição abre um span com os campos `request_id`, `method`, `path` e, quando resolvidos, `route_id`, `sub` e `status`.

O warn de degradação de rate limit é throttled em 10 segundos: log por requisição durante uma queda de Redis é o segundo incidente.

Cada transição do circuit breaker gera uma linha de `WARN`:

```json
{"level":"WARN","message":"circuito mudou de estado","upstream":"payment-service","de":"closed","para":"open"}
```

As famílias `gateway_circuit_state`, `gateway_circuit_transitions_total` e `gateway_retries_total` entram na Fase 3. Até lá, o circuito é observável pelo label `outcome="circuit_open"` de `gateway_requests_total` e por essas transições no log.
