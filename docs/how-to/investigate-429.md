# Investigar um 429 inesperado

## Confirme qual limite recusou

A resposta descreve o bucket **mais restritivo** da rota:

```bash
curl -i localhost:8080/payments
```

```
HTTP/1.1 429 Too Many Requests
retry-after: 2
x-ratelimit-limit: 30
x-ratelimit-remaining: 0
```

`x-ratelimit-limit` é o `capacity` do limite que está recusando. Compare com a configuração da rota: se a rota tem dois limites, esse número diz qual dos dois é o culpado.

## Encontre o bucket no Redis

As chaves têm o formato `rl:v1:<route_id>:<kind>:<valor>`:

```bash
docker compose exec redis redis-cli --scan --pattern 'rl:v1:payments:*'
```

```
rl:v1:payments:ip:172.20.0.1
```

```bash
docker compose exec redis redis-cli HGETALL rl:v1:payments:ip:172.20.0.1
```

```
tokens
0.132
last_refill_ms
1788744939565
```

`tokens` abaixo de 1 é a recusa. O TTL da chave é o tempo de encher o bucket do zero:

```bash
docker compose exec redis redis-cli TTL rl:v1:payments:ip:172.20.0.1
```

## Causas comuns

**O bucket é por IP e há um proxy na frente.** Todos os clientes viram um IP só. Sintoma: uma única chave `:ip:` com o endereço do balanceador. Solução: [configure `trusted_proxies`](multiple-replicas.md#configure-trusted_proxies).

**`refill_per_sec` fracionário é mais lento do que parece.** `0.1` é um token a cada dez segundos, não dez por segundo. Com `capacity: 5`, a rajada inicial de cinco requisições passa e a sexta espera dez segundos.

**Uma rota mais específica tem limite próprio.** `/users/signup` não herda o limite de `/users`; cada `route_id` tem buckets independentes. Confira qual rota realmente casou:

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics' \
  | grep gateway_requests_total | grep 429
```

```
gateway_requests_total{method="GET",outcome="rejected_ratelimit",route="payments",status="429"} 4
```

O label `route` diz qual rota recusou.

**Múltiplas réplicas com `store: memory`.** Cada réplica conta separadamente; o cliente vê um limite errático conforme cai em uma ou outra. Troque para `store: redis`.

## Zerar um bucket

Durante uma investigação, apagar a chave devolve o bucket cheio:

```bash
docker compose exec redis redis-cli DEL rl:v1:payments:ip:172.20.0.1
```

Um bucket ausente é indistinguível de um bucket cheio — é o mesmo caminho de código que atende um cliente novo.

## Se o 429 sumiu sozinho

Verifique se o store não caiu. O rate limit falha aberto: com o Redis fora, tudo passa.

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics' \
  | grep ratelimit_degraded
```

```
gateway_ratelimit_degraded_total 6
```

Qualquer valor acima de zero subindo significa que as decisões de limite não estão sendo tomadas.
