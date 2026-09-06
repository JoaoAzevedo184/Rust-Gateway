# Rate limiting

Este documento explica por que o rate limiter tem a forma que tem: o algoritmo escolhido,
por que a decisão roda dentro do armazenamento, e por que ele deixa a requisição passar
quando o Redis cai.

## Por que token bucket

A implementação mais simples de rate limit é a janela fixa: conte requisições por minuto,
zere o contador ao virar o minuto. Ela tem um defeito conhecido nas bordas — um limite de
60 por minuto permite 120 requisições em dois segundos, se 60 chegarem no fim de um minuto
e 60 no começo do seguinte. O limite nominal e o comportamento real divergem por um fator
de dois exatamente sob abuso.

Token bucket descreve o comportamento desejado diretamente. O bucket tem uma **capacidade**
(quantas requisições em rajada são toleradas) e uma **taxa de refill** (o ritmo sustentado).
Cada requisição consome um token; tokens repõem continuamente até a capacidade.

Os dois parâmetros correspondem a duas perguntas que já se fazia de qualquer forma: quanto
de pico tolerar, e qual o ritmo sustentável. Eles são independentes — dá para permitir uma
rajada generosa com um ritmo baixo, ou o contrário.

## A decisão roda dentro do store

A interface do armazenamento é uma única operação:

```rust
async fn try_acquire(&self, buckets: &[BucketRequest]) -> Result<Decision, StoreError>;
```

Não é `get` seguido de `set`. A matemática do bucket — calcular o refill decorrido, deduzir,
decidir — acontece **dentro** da implementação do store.

Isso não é preferência estética. Se o gateway lesse o contador, calculasse localmente e
escrevesse de volta, duas réplicas atendendo o mesmo usuário no mesmo instante leriam o
mesmo valor, ambas concluiriam que há token disponível, e ambas deixariam passar. O limite
falharia precisamente sob a concorrência que ele existe para controlar — e passaria em todos
os testes de réplica única.

Com a operação inteira dentro do store, o Redis a executa atomicamente em um script Lua, e
a implementação in-memory usa um mutex por shard. Ambas oferecem a mesma garantia, e a mesma
suíte de testes de conformidade roda contra as duas.

## Três detalhes do script Lua

**O relógio é o do Redis, não o do gateway.** O script obtém o tempo via `TIME`. Réplicas
com clock levemente dessincronizado, escrevendo no mesmo bucket com timestamps próprios,
produziriam refills inconsistentes — uma réplica adiantada "criaria" tokens que a outra não
vê. Um relógio único elimina a classe inteira de problema.

**Cada bucket expira.** O TTL é o tempo necessário para encher o bucket do zero: passado
isso, o bucket está cheio e é indistinguível de um bucket novo. Sem TTL, todo IP que já
tocou o gateway ficaria residente no Redis para sempre, e a memória cresceria com o total
histórico de clientes em vez de com os ativos.

**Todos os buckets da rota são avaliados juntos, tudo-ou-nada.** Uma rota pode ter mais de
um limite — tipicamente um por `sub` e um teto bruto por IP — e todos precisam passar. Se o
script deduzisse na ordem, um cenário em que o primeiro passa e o segundo rejeita já teria
consumido um token do primeiro. O usuário seria cobrado por uma requisição que não
aconteceu, e sob rejeição sustentada o primeiro bucket drenaria sem servir ninguém. O script
verifica todos antes de deduzir qualquer um.

## Por que a chave inclui a rota

As chaves têm a forma `rl:v1:<route_id>:<kind>:<valor>`.

O identificador da rota no meio é o que mantém limites independentes. Sem ele, um usuário
que gastasse sua cota em `/orders` chegaria a `/payments` já limitado, ainda que
`/payments` tenha sua própria política. Cada rota conta seus próprios tokens.

O prefixo de versão permite mudar o formato interno do bucket sem migração: chaves antigas
simplesmente expiram pelo TTL.

## Por que falha aberto

Quando o Redis está inacessível, o gateway **deixa a requisição passar**, incrementa
`gateway_ratelimit_degraded_total` e registra um aviso.

A alternativa seria responder 503 em toda rota limitada. Isso transformaria o Redis em ponto
único de falha para o sistema inteiro: uma dependência auxiliar, cuja função é proteger
contra abuso, derrubaria todo o tráfego legítimo. O remédio seria pior que a doença.

A escolha só é defensável porque é **observável**. A métrica de degradação existe para
sustentar um alerta; sem ele, você tem um gateway silenciosamente desprotegido e nenhuma
forma de saber. Se há uma coisa a monitorar deste documento, é essa métrica.

Duas peças acompanham a decisão. O timeout da chamada ao Redis é agressivo — na casa de
dezenas de milissegundos — porque um Redis lento, sem isso, viraria latência em toda
requisição do sistema. E um circuit breaker sobre o próprio store faz o gateway parar de
tentar durante a queda, em vez de pagar o timeout a cada requisição.

## Chaveamento: `sub` para autenticado, IP para anônimo

Rotas autenticadas limitam pelo `sub` do token. É a identidade correta: um usuário atrás de
NAT corporativo compartilha IP com centenas de colegas, e limitar por IP puniria todos por
causa de um.

Rotas anônimas não têm alternativa senão o IP, com as imprecisões que isso carrega.

Rotas autenticadas podem ainda declarar um teto bruto por IP, como proteção adicional — é
o que pega uma única origem operando muitas contas.

O IP usado é o endereço do peer, não o `X-Forwarded-For`, a menos que o peer esteja
explicitamente na lista de proxies confiáveis. Confiar nesse header sem validar a origem
tornaria o limite por IP decorativo: o cliente forjaria o header e ganharia um bucket novo
a cada requisição.

## A implementação in-memory não é para produção

Existem dois stores. O in-memory serve desenvolvimento e testes: sem dependência externa,
determinístico, rápido.

Ele **não** é adequado para múltiplas réplicas. Cada réplica conta separadamente, então o
limite efetivo é o configurado multiplicado pelo número de réplicas. Com uma réplica os dois
stores são equivalentes; a partir de duas, apenas o Redis entrega o limite que a
configuração declara.
