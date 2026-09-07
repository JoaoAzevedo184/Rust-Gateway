# Subir múltiplas réplicas

A arquitetura não assume instância única. Passar de uma para N réplicas exige **uma** mudança de configuração.

## Troque o store de rate limit para Redis

```yaml
rate_limit:
  store: redis
  redis_url: redis://redis:6379
  redis_timeout: 50ms
```

É a única mudança obrigatória. Com `store: memory`, cada réplica conta separadamente e o limite efetivo vira N vezes o configurado — `capacity: 600` com três réplicas deixa passar 1800.

## Suba as réplicas

```bash
docker compose up -d --scale gateway=3
```

Com `--scale`, remova o mapeamento de portas do serviço `gateway` no Compose e ponha um balanceador na frente; três containers não podem publicar a mesma porta do host.

## Configure `trusted_proxies`

Com um balanceador na frente, o peer que o gateway observa é o balanceador, não o cliente. Sem `trusted_proxies`, todo o tráfego vira um único IP e o rate limit por IP passa a limitar o balanceador.

```yaml
server:
  trusted_proxies:
    - 10.0.0.0/8
```

Liste apenas as redes dos proxies que você controla. `X-Forwarded-For` vindo de um peer fora da lista continua sendo descartado — é o que impede um cliente de forjar o header e ganhar um bucket novo por requisição.

## Verifique que as réplicas compartilham o estado

```bash
docker compose exec redis redis-cli --scan --pattern 'rl:v1:*'
```

```
rl:v1:users:sub:dev-user-1
rl:v1:payments:ip:172.20.0.1
```

Requisições do mesmo usuário atendidas por réplicas diferentes devem consumir o **mesmo** bucket. Para confirmar, dispare até o 429 e observe que a contagem não reinicia ao trocar de réplica.

## O que continua local a cada réplica

O circuit breaker, quando entrar na Fase 2. É deliberado: o breaker é uma observação sobre a conexão *daquela* réplica com *aquele* upstream. Compartilhá-lo faria o problema de rede de uma réplica abrir o circuito para todas, e colocaria o Redis no caminho crítico.

A consequência aceita é que, em `HalfOpen`, o upstream pode receber até N sondas concorrentes.

## Redis fora do ar

O rate limit **falha aberto**: as requisições passam sem decisão. Não há erro para o cliente; o sintoma é a métrica.

```
gateway_ratelimit_degraded_total 6
```

Alerte sobre essa métrica. Enquanto ela sobe, o gateway está sem proteção de taxa.
