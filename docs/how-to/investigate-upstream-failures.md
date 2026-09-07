# Investigar um 502, 503 ou 504

Os três erros dizem coisas diferentes. Comece separando-os.

| Status | `error` | O que aconteceu |
|---|---|---|
| 502 | `bad_gateway` | O gateway não conseguiu falar com o upstream. |
| 503 | `jwks_unavailable` | O cache de JWKS passou de `stale_max_age`. Não tem relação com o upstream. |
| 503 | `circuit_open` | O gateway parou de tentar: o upstream falhou demais nos últimos segundos. |
| 504 | `upstream_timeout` | O upstream aceitou a conexão e não respondeu a tempo. |

O `request_id` do corpo casa com a linha de log:

```bash
docker compose logs gateway | grep 01a07981-b858-7a82-81fe-f3503e46b5b1
```

## 502 — não consegui falar com o upstream

O log traz o upstream e o erro do cliente HTTP:

```json
{"level":"WARN","message":"falha ao encaminhar para o upstream","upstream":"user-service","error":"..."}
```

Verifique, nesta ordem:

**O nome resolve?** O `url` do upstream usa o nome de serviço da rede do Compose, não `localhost`.

```bash
docker compose run --rm --entrypoint sh redis -c 'nslookup user-service'
```

**O serviço está de pé e ouvindo na porta declarada?**

```bash
docker compose ps
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://user-service:80/'
```

Um erro comum é declarar a porta interna errada: o `url` do upstream aponta para a porta **dentro** da rede, não para a porta publicada no host.

**O upstream demora a aceitar a conexão?** `connect_timeout` (2s por padrão) corta a tentativa e produz 502, não 504. Se o serviço é lento para subir, use `depends_on` com healthcheck em vez de aumentar o timeout.

## 503 `jwks_unavailable` — não é sobre o upstream

O gateway não conseguiu buscar a JWKS por mais tempo que `stale_max_age`. Rotas anônimas continuam funcionando; só as autenticadas respondem 503.

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/ready'
```

```
not ready: cache de JWKS indisponível ou fora da janela stale
```

O log mostra as tentativas de refresh:

```bash
docker compose logs gateway | grep -i jwks
```

```json
{"level":"WARN","message":"refresh periódico de JWKS falhou, mantendo o snapshot anterior","error":"..."}
```

Enquanto essa linha aparece mas o 503 não, o cache stale está sustentando o tráfego — é para isso que ele existe. Verifique se `jwks_url` está correto e se o Auth Service responde:

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://auth-service/.well-known/jwks.json'
```

## 503 `circuit_open` — o gateway parou de tentar

Nenhuma requisição chegou ao upstream. O circuito abriu porque a taxa de falha das tentativas recentes passou de `failure_ratio`, e o `Retry-After` diz quanto falta para ele aceitar uma sonda.

```bash
docker compose logs gateway | grep 'circuito mudou'
```

```json
{"level":"WARN","message":"circuito mudou de estado","upstream":"payment-service","de":"closed","para":"open"}
```

O campo `upstream` diz qual serviço está sendo protegido — e, portanto, onde investigar. O circuito é por upstream, então **todas** as rotas que apontam para ele são cortadas, inclusive as que não falharam.

Encontre a causa nas falhas que antecederam a abertura:

```bash
docker compose logs gateway | grep 'falha ao encaminhar' | tail -5
```

O circuito se fecha sozinho quando o upstream volta: após `open_for` ele deixa passar uma sonda, e uma sonda bem-sucedida fecha o circuito. Não há nada a reiniciar.

## 504 — o upstream não respondeu a tempo

```bash
docker compose logs gateway | grep upstream_timeout
```

```json
{"level":"WARN","message":"tentativa estourou o upstream_timeout","budget":"5s"}
```

Duas causas distintas produzem 504:

- **`upstream_timeout`**, o prazo de uma tentativa. O upstream aceitou a conexão e não mandou os headers a tempo, ou estagnou no meio do corpo.
- **`request_timeout`**, o teto global. Some as tentativas e as esperas de backoff: com `max_attempts: 3` e um upstream lento, três tentativas de 5s passam de 15s.

Um 504 **não** é repetido, mesmo em método idempotente: o upstream pode ter recebido e processado a requisição.

Se o upstream é legitimamente lento, [ajuste os timeouts](tune-resilience.md#um-upstream-lento) em vez de aumentar as tentativas.

## Confirme pelas métricas

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics' \
  | grep gateway_requests_total
```

```
gateway_requests_total{method="GET",outcome="upstream_error",route="users",status="502"} 3
gateway_requests_total{method="GET",outcome="circuit_open",route="users",status="503"} 4
gateway_requests_total{method="GET",outcome="timeout",route="users",status="504"} 1
```

O label `outcome` separa a falha de conexão (`upstream_error`) da recusa de política (`rejected_auth`, `rejected_ratelimit`), mesmo quando o status não deixa claro.

## 5xx que vieram do upstream

Um 500 gerado pelo backend é repassado **sem alteração de corpo**. Se a resposta não tem o corpo JSON do gateway — sem os campos `error` e `request_id` —, ela veio do backend, e a investigação continua lá.
