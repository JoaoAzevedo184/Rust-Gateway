# Referência de headers

## Na entrada

### Removidos incondicionalmente

Antes de qualquer política, inclusive antes da resolução de rota:

- `X-User-Id`
- `X-User-Scopes`
- qualquer header com prefixo `X-Auth-`

A regra é auditável em uma frase: **nenhum header de identidade sobrevive à borda; os únicos que chegam ao upstream foram escritos pelo gateway.**

O scrub é global porque rota anônima não roda autenticação. Se ele morasse no layer de auth, uma rota com `required: false` repassaria um `X-User-Id` forjado direto ao backend.

### Hop-by-hop

Removidos nos dois sentidos, na ida e na volta:

`Connection`, `Keep-Alive`, `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailers`, `Transfer-Encoding`, `Upgrade`

Além desses, **todo header nomeado dentro do `Connection`**. Ignorar essa segunda parte deixaria passar exatamente os headers que o peer anterior declarou como sendo só daquela conexão.

### `X-Request-Id`

Aceito do cliente se for bem-formado: no máximo 64 caracteres, apenas alfanuméricos e hífen. Caso contrário é descartado e substituído por um UUIDv7 gerado pelo gateway.

A validação não é preciosismo: o id entra em log, e log que aceita string arbitrária aceita injeção de linha.

### `X-Forwarded-For`

O tratamento depende de `server.trusted_proxies`:

| Peer | Comportamento |
|---|---|
| Fora de `trusted_proxies` | O header recebido é **descartado**. O gateway usa e repassa apenas o endereço do socket. |
| Dentro de `trusted_proxies` | A cadeia recebida é preservada e o endereço do peer é acrescentado ao final. |

Para rate limit por IP, o endereço do cliente é o **último endereço não confiável** da cadeia — o primeiro salto que o gateway não controla, e portanto o mais próximo do cliente real que ainda é verificável.

Com `trusted_proxies: []`, que é o default, o header do cliente nunca vale nada.

## Na saída, para o upstream

| Header | Valor |
|---|---|
| `Host` | Autoridade do upstream, reescrita |
| `X-Forwarded-Host` | `Host` original da requisição |
| `X-Forwarded-For` | Conforme a regra acima |
| `X-Forwarded-Proto` | `http` — o gateway não termina TLS nesta topologia |
| `X-Request-Id` | O correlation id, aceito ou gerado |
| `Authorization` | O original, intacto |

Com um token válido, o gateway acrescenta:

| Header | Valor |
|---|---|
| `X-User-Id` | A claim `sub` |
| `X-User-Scopes` | Escopos concedidos, separados por espaço |
| `X-Auth-Method` | `jwt` |

Como o scrub global já rodou, esses três são inequivocamente do gateway.

## Na resposta ao cliente

| Header | Quando |
|---|---|
| `X-Request-Id` | Sempre |
| `X-RateLimit-Limit` | Em rota com `rate_limit` configurado, permitida ou não |
| `X-RateLimit-Remaining` | Idem |
| `Retry-After` | Em 429, e em 503 de circuito aberto na Fase 2 |

`X-RateLimit-Limit` e `X-RateLimit-Remaining` descrevem o bucket **mais restritivo** da rota — o que está mais perto de recusar. `Retry-After` é em segundos, arredondado para cima, e nunca é zero.

Uma rota limitada que passou pela degradação do store não recebe headers de limite: não há número honesto para anunciar.
