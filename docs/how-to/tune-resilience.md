# Ajustar timeouts, retry e circuit breaker

As três políticas se compõem, e mexer numa sem olhar as outras costuma produzir o oposto do pretendido. Comece pelo nível certo.

## Escolha o nível

A política é herdada em três níveis, resolvida campo a campo:

```yaml
resilience:
  default:                        # todos os upstreams
    upstream_timeout: 5s

upstreams:
  payment-service:
    url: http://payment-service:8082
    resilience:                   # este upstream
      upstream_timeout: 15s

routes:
  - id: payments-relatorio
    resilience:                   # esta rota
      upstream_timeout: 25s
```

O mais específico vence, **campo a campo**: a rota acima ainda herda o `retry` do upstream e o `circuit_breaker` do default.

`circuit_breaker` é a exceção: só pode ser definido em `default` ou no upstream. O estado do circuito é por upstream, e configurá-lo por rota seria pedir dois comportamentos para um único circuito — o gateway recusa a configuração no startup.

## Um upstream lento

```yaml
upstreams:
  relatorios:
    url: http://relatorios:8080
    resilience:
      upstream_timeout: 30s
```

Se `upstream_timeout` passar de `server.request_timeout`, o processo não sobe: o teto global cortaria antes da tentativa individual, o que é sempre engano. Aumente os dois:

```yaml
server:
  request_timeout: 60s
```

`upstream_timeout` cobre a espera pelos headers e, depois deles, a inatividade do corpo. Um download longo mas progredindo nunca é cortado; um que estagnou, sim. Não aumente esse valor por causa do tamanho da resposta.

## Um upstream que nunca pode receber a requisição duas vezes

```yaml
upstreams:
  payment-service:
    url: http://payment-service:8082
    resilience:
      retry: { max_attempts: 0 }
```

Mesmo sem isso, o gateway nunca repete POST ou PATCH. O `max_attempts: 0` cobre o resto: um `PUT` ou `DELETE` que, naquele serviço, não seja realmente idempotente.

## Um upstream instável, para abrir o circuito mais cedo

```yaml
upstreams:
  flaky:
    url: http://flaky:8080
    resilience:
      circuit_breaker:
        failure_ratio: 0.3
        min_requests: 10
        open_for: 30s
```

`min_requests` é o freio: sem ele, a primeira requisição do dia falhando abriria o circuito com 100% de taxa de erro. Baixe-o só na medida em que o upstream receba tráfego suficiente para que a amostra signifique alguma coisa.

## Verifique o que mudou

O circuito registra cada transição:

```bash
docker compose logs gateway | grep 'circuito mudou'
```

```json
{"level":"WARN","message":"circuito mudou de estado","upstream":"flaky","de":"closed","para":"open"}
```

Um ciclo saudável de recuperação aparece como três linhas: `closed -> open`, `open -> half_open`, `half_open -> closed`. Terminar de novo em `open` significa que o upstream ainda não voltou.

Os desfechos aparecem nas métricas:

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics' \
  | grep gateway_requests_total
```

```
gateway_requests_total{method="GET",outcome="upstream_error",route="pedidos",status="502"} 1
gateway_requests_total{method="GET",outcome="circuit_open",route="pedidos",status="503"} 4
gateway_requests_total{method="GET",outcome="timeout",route="relatorios",status="504"} 2
```

## Interações que costumam surpreender

**O breaker conta tentativas, não requisições.** Com `max_attempts: 2`, uma requisição contra um upstream fora contribui com **duas** falhas para a janela. `min_requests: 20` é atingido em dez requisições, não vinte. É deliberado: o breaker observa a conexão, e cada tentativa é uma observação.

**Retry não consome um segundo token de rate limit.** O layer de rate limit fica fora do retry: o usuário pediu uma requisição, e não deve pagar pelas tentativas do gateway.

**Circuito aberto corta a segunda tentativa na hora.** Se o circuito abrir entre a primeira e a segunda tentativa, a segunda é cortada imediatamente em vez de gastar mais um timeout contra um upstream já declarado fora.

**Aumentar `max_attempts` multiplica a carga sobre um upstream em dificuldade.** É por isso que o backoff tem jitter, e por isso o breaker fica dentro do retry. Prefira ajustar o breaker a aumentar as tentativas.
