# Referência de erros

## Corpo de erro

Toda resposta de erro **gerada pelo gateway** usa o mesmo corpo JSON, com `Content-Type: application/json`:

```json
{
  "error": "rate_limited",
  "message": "Request rate exceeded for this route",
  "request_id": "01a07981-b858-7a82-81fe-f3503e46b5b1"
}
```

| Campo | Descrição |
|---|---|
| `error` | Código estável, legível por máquina. É por ele que se programa, não pela mensagem. |
| `message` | Texto em inglês, para humanos. Pode mudar entre versões. |
| `request_id` | O mesmo valor do header `X-Request-Id`, para casar a resposta com a linha de log. |

Respostas produzidas pelo **upstream** são repassadas sem alteração de corpo, inclusive as de erro. Um 500 do backend chega ao cliente como o backend o escreveu.

## Códigos

| Situação | Status | `error` | Headers adicionais |
|---|---|---|---|
| Prefixo não casa com rota alguma | 404 | `not_found` | |
| Token ausente em rota que exige | 401 | `unauthorized` | |
| Token malformado, expirado, com assinatura, `iss` ou `aud` inválidos | 401 | `unauthorized` | |
| `kid` desconhecido mesmo após refresh forçado | 401 | `unauthorized` | |
| Token sem claim `sub` | 401 | `unauthorized` | |
| Token válido, escopo insuficiente | 403 | `forbidden` | |
| Corpo maior que `max_body_bytes` | 413 | `payload_too_large` | |
| Rate limit estourado | 429 | `rate_limited` | `Retry-After`, `X-RateLimit-*` |
| Falha ao conectar no upstream | 502 | `bad_gateway` | |
| Circuito aberto para o upstream | 503 | `circuit_open` | `Retry-After` |
| JWKS indisponível além de `stale_max_age` | 503 | `jwks_unavailable` | |
| Tentativa estourou `upstream_timeout` | 504 | `upstream_timeout` | |
| Requisição estourou `request_timeout` | 504 | `upstream_timeout` | |

### 502 e 504 dizem coisas diferentes

502 significa que nenhuma resposta chegou: recusa de conexão, falha de DNS, ou a conexão caiu antes dos headers. É a única condição em que o gateway repete a requisição, e mesmo assim só em método idempotente.

504 significa que o upstream teve tempo e não respondeu. Não é repetido: ele pode ter recebido e processado a requisição, e a resposta é que se perdeu.

### O 503 de circuito aberto não chegou ao upstream

`circuit_open` é uma recusa do próprio gateway, decidida a partir das falhas recentes daquele upstream. O `Retry-After` diz quanto falta para o circuito aceitar uma sonda.

Nenhuma tentativa é feita, e nenhum token de rate limit adicional é consumido.

### Por que 401 e 403 são distintos

401 diz "identifique-se": não há credencial, ou a que veio não é válida. 403 diz "você não pode": a credencial é válida e a identidade é conhecida, mas não tem o escopo exigido.

Trocar um pelo outro faz o cliente tentar renovar um token que já estava bom.

### Token inválido em rota anônima

Uma rota com `auth.required: false` aceita requisições sem `Authorization`. Se um token **for** enviado, ele é validado, e um token inválido produz 401.

A rota dispensa credencial; não perdoa credencial ruim.

## Degradação e o que ela não produz

Quando o store de rate limit está indisponível, o gateway **não** responde erro: a requisição passa sem decisão de limite e sem headers `X-RateLimit-*`. É a decisão D6, e o sintoma observável é a métrica `gateway_ratelimit_degraded_total`, não um status.

Quando a JWKS está indisponível mas ainda dentro de `stale_max_age`, o snapshot anterior continua sendo usado e nada muda para o cliente. Passando da janela, rotas autenticadas respondem 503.
