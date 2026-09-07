# Referência de configuração

O gateway lê um único arquivo YAML. O caminho vem do primeiro argumento da linha de comando, da variável `GATEWAY_CONFIG`, ou, na ausência dos dois, de `config/gateway.yaml`.

```bash
rust-gateway /etc/rust-gateway/gateway.yaml
GATEWAY_CONFIG=/etc/rust-gateway/gateway.yaml rust-gateway
```

Configuração inválida impede o processo de subir. A validação reporta **todos** os problemas de uma vez, não o primeiro.

Um campo desconhecido é erro, não algo ignorado em silêncio: `strip_prefixo: true` seria uma rota que não faz o que se pediu, sem aviso.

## Tipos de valor

| Tipo | Formato | Exemplos |
|---|---|---|
| duração | número seguido de unidade | `30s`, `5m`, `50ms`, `1h`, `1m30s` |
| tamanho | número, com sufixo opcional | `1024`, `2MiB`, `10 MB`, `1GiB` |
| endereço | `host:porta` | `0.0.0.0:8080`, `127.0.0.1:8080` |
| CIDR | rede IPv4 ou IPv6 | `10.0.0.0/8`, `172.16.0.0/12` |

Sufixos de tamanho são distintos: `MiB` é 1024², `MB` é 1000². Sem sufixo, o valor é em bytes.

## `server`

| Campo | Tipo | Default | Descrição |
|---|---|---|---|
| `bind` | endereço | `0.0.0.0:8080` | Listener público, por onde entra o tráfego roteado. |
| `admin_bind` | endereço | `0.0.0.0:9090` | Listener administrativo: `/health`, `/ready` e `/metrics`. |
| `request_timeout` | duração | `30s` | Teto absoluto por requisição, incluindo todos os retries. |
| `max_body_bytes` | tamanho | `2MiB` | Limite de corpo de requisição. |
| `trusted_proxies` | lista de CIDR | `[]` | Redes cujo `X-Forwarded-For` é confiável. |

Com `trusted_proxies` vazio, o gateway **ignora** `X-Forwarded-For` e usa o endereço do peer. Veja [headers](headers.md#x-forwarded-for) para a regra completa.

## `auth`

Seção opcional. Obrigatória se qualquer rota declarar `auth.required: true`.

| Campo | Tipo | Default | Descrição |
|---|---|---|---|
| `jwks_url` | URL | — | Endereço da JWKS do Auth Service. Obrigatório. |
| `issuer` | string | — | Valor exigido na claim `iss`, comparação exata. Obrigatório. |
| `audience` | string | — | Valor exigido na claim `aud`, comparação exata. Obrigatório. |
| `refresh_interval` | duração | `5m` | Intervalo do refresh proativo em background. |
| `stale_max_age` | duração | `30m` | Por quanto tempo um snapshot de JWKS continua aceito sem refresh bem-sucedido. |
| `leeway` | duração | `30s` | Tolerância de clock aplicada a `exp` e `nbf`. |
| `unknown_kid_cooldown` | duração | `30s` | Intervalo mínimo entre refreshes forçados por `kid` desconhecido. |
| `scope_claim` | string | `scope` | Nome da claim que carrega os escopos. |

A claim de escopo é lida no formato OAuth2 — string separada por espaço — e também como array. Quando existe uma claim `roles` em array, seus valores se somam aos escopos.

## `rate_limit`

| Campo | Tipo | Default | Descrição |
|---|---|---|---|
| `store` | `memory` \| `redis` | `memory` | Onde vive o estado dos buckets. |
| `redis_url` | URL | — | Obrigatório quando `store: redis`. |
| `redis_timeout` | duração | `50ms` | Timeout de cada consulta ao Redis. |

`memory` conta por processo. Com múltiplas réplicas, o limite efetivo vira N vezes o configurado — veja [subir múltiplas réplicas](../how-to/multiple-replicas.md).

## `resilience`

Política herdada em três níveis, resolvida **campo a campo**: `resilience.default` → `upstreams.<id>.resilience` → `routes[].resilience`. Um `retry` parcial na rota não zera o `backoff` herdado do upstream.

| Campo | Tipo | Default | Descrição |
|---|---|---|---|
| `connect_timeout` | duração | `2s` | Prazo para estabelecer a conexão TCP. |
| `upstream_timeout` | duração | `5s` | Prazo de **uma** tentativa. |
| `retry.max_attempts` | inteiro | `2` | Total de tentativas, contando a primeira. |
| `retry.backoff` | duração | `50ms` | Espera base entre tentativas. |
| `retry.jitter` | booleano | `true` | Sorteia a espera em `[0, teto]`. |
| `circuit_breaker.failure_ratio` | número | `0.5` | Taxa de falha que abre o circuito. |
| `circuit_breaker.min_requests` | inteiro | `20` | Tentativas mínimas na janela antes de qualquer avaliação. |
| `circuit_breaker.window` | duração | `30s` | Largura da janela deslizante. |
| `circuit_breaker.open_for` | duração | `15s` | Tempo em `Open` antes de aceitar uma sonda. |
| `circuit_breaker.half_open_probes` | inteiro | `1` | Sondas concorrentes em `HalfOpen`. |

### Os três timeouts

| Nível | Escopo |
|---|---|
| `connect_timeout` | Estabelecer a conexão TCP. Estourar produz 502. |
| `upstream_timeout` | Uma tentativa: envio, mais a espera pelos headers da resposta. Depois dos headers, vira prazo de **inatividade**, reiniciado a cada quadro do corpo. Estourar produz 504. |
| `request_timeout` | A requisição inteira, incluindo todos os retries. Estourar produz 504. |

`upstream_timeout` não é um teto sobre a duração total do download. Corpos são transmitidos em streaming, e um teto absoluto mataria toda transferência longa — exatamente o caso que o streaming existe para atender. O que ele garante é que o upstream não pode estagnar no meio do corpo.

`connect_timeout` é aplicado por upstream. O gateway mantém um pool de conexões por valor distinto de `connect_timeout` entre os upstreams configurados.

### `retry`

`max_attempts` conta a **primeira** tentativa. `2` significa uma tentativa e uma repetição; `0` e `1` significam a mesma coisa — tentativa única, sem repetição.

Repetir exige duas condições, e nenhuma delas é configurável:

- **Método idempotente**: GET, HEAD, OPTIONS, PUT ou DELETE. POST e PATCH nunca são repetidos.
- **Falha comprovadamente pré-resposta**: nenhuma resposta chegou. Timeout e 5xx não são repetidos, porque o upstream pode ter processado a requisição.

Rotas com retry habilitado bufferizam o corpo da requisição até `server.max_body_bytes`. Um corpo maior que isso, ou sem tamanho declarado, segue normalmente em tentativa única.

Com `jitter: true`, a espera é sorteada em `[0, teto]`, onde o teto dobra a cada tentativa a partir de `backoff`. Backoff fixo faz todas as requisições que falharam juntas retentarem juntas.

### `circuit_breaker`

O estado é **por upstream** e local à réplica. Rotas distintas para o mesmo upstream compartilham o circuito.

Por isso, `circuit_breaker` só pode ser configurado em `resilience.default` ou em `upstreams.<id>.resilience`. Defini-lo em uma rota é erro de startup.

Contam como falha: erro de conexão, timeout e 5xx do upstream. **4xx não conta** — uma onda de 401 ou 404 é comportamento do cliente.

| Estado | Comportamento |
|---|---|
| `Closed` | Passa tudo, alimentando a janela. Abre quando a taxa de falha ultrapassa `failure_ratio` com pelo menos `min_requests` na janela. |
| `Open` | Corta imediatamente com 503 e `Retry-After`. Após `open_for`, aceita sondas. |
| `HalfOpen` | Aceita até `half_open_probes` sondas concorrentes. Uma sonda bem-sucedida fecha o circuito e zera a janela; uma que falha reabre. |

## `upstreams`

Mapa de nome para destino. Upstreams são **nomeados**, não URLs inline, porque o estado de circuit breaker é por upstream: rotas distintas para o mesmo serviço precisam compartilhá-lo.

| Campo | Tipo | Default | Descrição |
|---|---|---|---|
| `url` | URL | — | Esquema `http` ou `https`, com host. Um caminho na URL vira prefixo de todas as requisições ao upstream. |
| `resilience` | política parcial | — | Sobrepõe `resilience.default` para este upstream. |

```yaml
upstreams:
  user-service: { url: http://user-service:8080 }
  payment-service:
    url: http://payment-service:8082
    resilience:
      upstream_timeout: 15s
      retry: { max_attempts: 0 }
```

## `routes`

Lista. A ordem no arquivo não importa: o casamento é sempre por prefixo mais longo.

| Campo | Tipo | Default | Descrição |
|---|---|---|---|
| `id` | string | — | Único. Vira o label `route` das métricas e o prefixo das chaves de rate limit. |
| `match.prefix` | caminho | — | Prefixo, casado com fronteira de segmento obrigatória. |
| `upstream` | string | — | Nome de um upstream declarado. |
| `strip_prefix` | booleano | `false` | Remove o prefixo antes de encaminhar. |
| `auth` | bloco | — | Obrigatório, inclusive para declarar `required: false`. |
| `auth.required` | booleano | — | Obrigatório dentro do bloco. |
| `auth.scopes` | lista de string | `[]` | Escopos exigidos, em **conjunção**: todos precisam estar presentes. |
| `rate_limit` | lista | `[]` | Limites da rota. Todos precisam passar. |
| `resilience` | política parcial | — | Sobrepõe o nível do upstream para esta rota. |

### Casamento de prefixo

`/users` casa `/users` e `/users/42`, e **não** casa `/usersecret`. A fronteira de segmento é obrigatória.

Prefixo mais longo vence, independente da ordem no arquivo: com `/users` e `/users/signup` declarados, `/users/signup/confirmar` resolve para `users-signup`.

Sem casamento, 404.

### `rate_limit[]`

| Campo | Tipo | Descrição |
|---|---|---|
| `key` | `sub` \| `ip` | Sobre o que o bucket é contado. |
| `capacity` | inteiro > 0 | Tokens do bucket cheio. É o teto de rajada. |
| `refill_per_sec` | número > 0 | Tokens recuperados por segundo. Aceita fração: `0.1` é um token a cada dez segundos. |

A lista é uma conjunção: todos os limites precisam permitir. É assim que "limitada por `sub`, com teto bruto por IP" cabe sem mecanismo especial.

```yaml
rate_limit:
  - { key: sub, capacity: 600,  refill_per_sec: 10 }
  - { key: ip,  capacity: 2000, refill_per_sec: 50 }
```

`key: sub` só é válido em rota com `auth.required: true`.

## Erros de validação de startup

| Situação | Mensagem |
|---|---|
| Campo desconhecido em qualquer bloco | erro de parse, com o nome do campo |
| Rota sem bloco `auth` | `bloco auth ausente` |
| `auth.required: true` sem seção `auth` global | `não há seção auth global com jwks_url` |
| `auth.required: false` com `scopes` | `Escopo só é verificável em rota autenticada` |
| `rate_limit` com `key: sub` em rota anônima | `sem token não há sub` |
| `upstream` que não existe | `não existe em upstreams` |
| `id` de rota duplicado | `id duplicado` |
| `match.prefix` duplicado | `match.prefix duplicado` |
| `match.prefix` sem `/` inicial | `precisa começar com /` |
| `capacity` zero, ou `refill_per_sec` não positivo | `precisa ser maior que zero` |
| `store: redis` sem `redis_url` | `exige rate_limit.redis_url` |
| URL de upstream ou de JWKS malformada | `não é uma URL válida` |
| `stale_max_age` menor que `refresh_interval` | `o cache expiraria antes do primeiro refresh` |
| `request_timeout` menor que `upstream_timeout` | `o teto global cortaria antes da tentativa individual` |
| `circuit_breaker` definido em uma rota | `o estado do breaker é por upstream` |
| Nenhuma rota configurada | `nenhuma rota configurada` |

`/users` e `/users/` são o mesmo prefixo: declarar os dois é duplicata.
