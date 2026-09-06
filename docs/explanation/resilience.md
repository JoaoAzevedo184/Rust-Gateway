# Resiliência

Este documento explica como o gateway se comporta quando um upstream está lento, instável
ou fora — e por que as três peças (timeout, retry, circuit breaker) estão dispostas nessa
ordem.

## As três peças resolvem problemas diferentes

**Timeout** limita quanto tempo uma requisição pode custar. Sem ele, um upstream que aceita
conexões mas nunca responde acumula requisições no gateway até esgotar recursos — a falha
se propaga para trás em vez de ser contida.

**Retry** recupera falhas transitórias. Um reset de conexão durante um deploy do upstream é
recuperável; falhar a requisição do usuário por causa dele é desperdício.

**Circuit breaker** para de tentar quando tentar não adianta. Se um upstream está fora,
cada requisição paga o timeout inteiro antes de falhar. Com centenas de requisições
concorrentes, o gateway fica cheio de tarefas esperando um serviço que não vai responder, e
rotas saudáveis sofrem por contágio.

Timeout protege a requisição individual. O breaker protege o gateway. Retry serve o usuário.

## A ordem, e por que ela é essa

```
rate_limit
  └─ retry
      └─ circuit_breaker
          └─ upstream_timeout
              └─ proxy
```

**Rate limit fica fora do retry.** Uma requisição do usuário deve consumir um token, não
um por tentativa. Se o limiter ficasse dentro, uma falha transitória cobraria do usuário
duas vezes pelo mesmo pedido, e um upstream instável drenaria as cotas de todo mundo.

**O circuit breaker fica dentro do retry.** É o arranjo menos óbvio dos dois, e o mais
importante.

Se o breaker envolvesse o retry, ele veria um resultado por requisição — todas as tentativas
já colapsadas em um sucesso ou uma falha. A janela do breaker mediria coisa diferente do
que ele precisa medir, e a decisão de abrir chegaria tarde.

Por dentro, o breaker registra cada tentativa individualmente, que é a granularidade certa
da observação. E ganha uma segunda propriedade: se o circuito abrir entre a primeira e a
segunda tentativa, a segunda é cortada imediatamente, em vez de gastar mais um timeout
contra um upstream que já foi declarado fora.

## Retry seguro por padrão

O gateway retria apenas métodos idempotentes — GET, HEAD, OPTIONS, PUT, DELETE — e apenas
quando a falha é comprovadamente **anterior à resposta**: recusa de conexão, falha de DNS,
reset antes dos headers.

A exclusão que mais importa é o **timeout de leitura**. Ele parece falha, mas não diz nada
sobre o que aconteceu do outro lado. O upstream pode ter processado a requisição
inteiramente e sido lento apenas para responder. Retriar nesse estado é reenviar uma
operação já executada.

Isso deixa de ser abstrato com o Payment Service. `POST /payments` que expira na leitura
pode ter cobrado o cliente; um retry automático cobraria de novo. Por isso POST nunca é
retriado, independentemente de configuração — a segurança não deve depender de alguém
lembrar de desabilitar.

**Backoff tem jitter.** Sem ele, todas as requisições que falharam no mesmo instante
retentam no mesmo instante seguinte. O upstream que estava se recuperando recebe uma onda
sincronizada e cai de novo, e o retry vira o mecanismo que impede a recuperação.

**Retry exige bufferizar o corpo.** Não se pode reenviar um corpo já consumido. O gateway
transmite corpos em streaming por padrão — bufferizar significaria que um upload grande vira
memória proporcional por requisição concorrente. Rotas com retry habilitado bufferizam até
o limite configurado; corpos maiores seguem normalmente, mas sem elegibilidade a retry. Na
prática quase nada é bufferizado, porque os métodos retriáveis comuns não têm corpo.

## O que conta como falha

Contam: erro de conexão, timeout e respostas 5xx.

**Não contam: respostas 4xx.** Um 401, um 404 ou um 422 significam que o upstream recebeu a
requisição, entendeu e respondeu corretamente — que é o oposto de estar doente.

Se 4xx contasse, um cliente mal configurado disparando 404 em volume abriria o circuito e
tiraria o upstream do ar para todo mundo. Um scanner de vulnerabilidade viraria uma negação
de serviço trivial. O breaker mede saúde do upstream, não comportamento do cliente.

## Por que existe um mínimo de requisições

O breaker não avalia nada antes de acumular um número mínimo de resultados na janela.

Sem isso, a primeira requisição do dia falhando produz uma taxa de erro de 100%, e o
circuito abre com uma amostra de tamanho um. O mínimo é o que separa "este upstream está com
problema" de "aconteceu uma falha".

## O breaker não é compartilhado entre réplicas

Estado de rate limit é compartilhado via Redis; estado de circuit breaker é local a cada
réplica. A assimetria é deliberada.

Rate limit é uma **cota do usuário**. Ela precisa somar entre réplicas, senão o limite se
multiplica pelo número de instâncias.

Circuit breaker é uma **observação sobre a conexão desta réplica com este upstream**. Se
uma réplica está com problema de rede localizado, essa é informação sobre ela, não sobre o
upstream. Compartilhar o estado faria o problema de uma réplica abrir o circuito para todas
— transformando uma falha parcial em total, que é exatamente o que o breaker deveria
prevenir.

Há um custo: com N réplicas, o upstream em recuperação recebe até N sondas de meia-abertura
em vez de uma. Com poucas réplicas e uma sonda por vez, isso é desprezível perto do risco
que se evita. E manteria o Redis no caminho crítico de toda requisição — o breaker precisa
decidir antes de cada chamada, e uma consulta de rede para decidir se vale fazer uma
chamada de rede é um mau negócio.

## Os três timeouts

| Nível | Escopo |
|---|---|
| `connect_timeout` | estabelecer a conexão TCP |
| `upstream_timeout` | uma tentativa, do envio à resposta completa |
| `request_timeout` | a requisição inteira, incluindo todos os retries |

Confundi-los é como se produz um gateway que trava. Um timeout só de conexão não protege
contra um upstream que aceita e nunca responde. Um timeout só por tentativa deixa o total
crescer com o número de retries. O teto global existe para garantir que, aconteça o que
acontecer, o cliente recebe uma resposta em tempo previsível.

A validação de configuração rejeita um teto global menor que o timeout por tentativa: nessa
configuração o teto cortaria antes da tentativa individual terminar, o que nunca é a
intenção.
