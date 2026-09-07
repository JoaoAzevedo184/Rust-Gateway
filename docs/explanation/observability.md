# Observabilidade

Este documento explica o que o gateway expõe sobre si mesmo e por quê: a separação entre liveness e readiness, a decisão de observar a saúde dos upstreams passivamente, e a regra que impede as métricas de virarem um problema próprio.

## Um gateway é o melhor lugar para medir

Toda requisição do sistema passa por aqui. Isso faz do gateway o único ponto onde se pode responder "qual a latência do sistema?" sem instrumentar cada serviço — e o único que vê tanto a requisição do cliente quanto a chamada ao upstream.

Daí a decisão de manter dois histogramas de duração em vez de um: o tempo total, de borda a borda, e o tempo gasto no upstream. A diferença entre eles é o overhead real do gateway. Com um número só não dá para distinguir "o gateway está lento" de "o gateway está esperando" — e são diagnósticos com remédios opostos.

## Liveness e readiness respondem perguntas diferentes

`/health` responde: **este processo está vivo?**
`/ready` responde: **este processo deve receber tráfego agora?**

A distinção existe porque o orquestrador reage a cada uma de forma distinta. Falha de liveness mata e reinicia o container. Falha de readiness apenas o tira do balanceamento.

Por isso **`/health` não verifica dependência nenhuma**. É tentador fazê-lo checar o Auth Service — parece mais informativo. O resultado seria desastroso: o Auth Service cai, todos os gateways reprovam liveness, o orquestrador mata todos eles, e uma degradação parcial (rotas anônimas ainda funcionavam) vira uma queda total, com reinícios em loop enquanto a dependência não volta.

`/ready` verifica o que é necessário para servir: configuração carregada e cache de JWKS utilizável. **Redis não entra na lista**, ainda que o rate limit dependa dele — porque o rate limit falha aberto. Um Redis fora deixa o gateway desprotegido, não incapaz. Tirá-lo do balanceamento não melhoraria nada e derrubaria a capacidade do sistema no momento em que ela é mais necessária.

A pergunta que decide se algo entra em `/ready` é: *se isto está quebrado, outra réplica atenderia melhor?* Se a resposta é não, não pertence ao readiness.

## Saúde de upstream é observada, não sondada

O gateway não faz probes ativos contra os upstreams. Ele infere saúde pelo circuit breaker, a partir do tráfego real.

Probe ativo tem duas fraquezas. A primeira é que ele mede a coisa errada: um endpoint `/health` de um serviço Java responde 200 enquanto o pool de conexões com o banco está esgotado e toda requisição real falha. A segunda é que ele custa um scheduler, uma janela de intervalo, e a lógica de tratar suas próprias falhas.

O tráfego real é uma amostra melhor porque é a coisa que interessa. Se as requisições falham, o upstream está com problema — independentemente do que o `/health` dele diga. A única desvantagem é não detectar recuperação sem tráfego, e o estado de meia-abertura do breaker cobre exatamente isso.

## A regra de cardinalidade

Toda métrica é rotulada por identificador de rota — um valor vindo da configuração, e portanto de conjunto conhecido e limitado. **Nunca pelo path bruto, nunca pelo `sub`, nunca pelo IP.**

Não é uma preferência de estilo. Cada combinação distinta de labels vira uma série temporal no Prometheus, com custo de memória permanente. Rotular por path cru significa que um scanner de diretórios varrendo URLs aleatórias cria uma série por URL tentada. O ataque não derruba o gateway; derruba o Prometheus, e leva junto a visibilidade sobre todo o resto, exatamente durante um incidente.

Rotular por identidade de usuário tem o mesmo problema com um agravante: coloca dado de usuário em um sistema que normalmente não é tratado como tal.

O `sub` aparece nos spans de tracing, onde a retenção é curta e a amostragem reduz o volume. Ele não aparece em labels de métrica.

## Correlation ID e traceparent coexistem

O gateway propaga dois identificadores, e eles não são redundantes.

`X-Request-Id` serve o humano lendo log. É um valor só, fácil de copiar de uma resposta de erro e colar num filtro. Ele aparece no corpo de erro justamente para que quem reporta um problema traga consigo a chave de busca.

`traceparent` (W3C) serve o sistema de tracing. Carrega contexto de trace e span, e é o que faz uma requisição atravessando Rust, Java e Python aparecer como um trace único em vez de três desconexos. O gateway aceita o contexto que chega e o continua, em vez de iniciar um novo.

O correlation ID vindo do cliente é validado antes de ser aceito — comprimento máximo, alfabeto restrito. Ele vai para arquivos de log, e um campo de log que aceita string arbitrária aceita injeção de linha. Um valor malformado é descartado e substituído por um gerado.

A geração usa UUIDv7 em vez de v4 porque v7 é ordenável por tempo. Ao ordenar logs por correlation ID, requisições próximas ficam próximas — uma propriedade pequena que se paga toda vez que se investiga um incidente.

## O listener administrativo é separado

`/metrics`, `/health` e `/ready` vivem em uma porta distinta da do tráfego público.

Três razões. Não colidem com prefixo de rota nenhum — sem isso, `/metrics` seria um caminho que nenhum serviço poderia usar. Não passam pela pilha de políticas, porque um `/metrics` que exige JWT é inútil para o Prometheus e um `/health` sujeito a rate limit é ativamente perigoso. E, no Docker Compose, a porta administrativa simplesmente não é publicada: a superfície interna fica inacessível de fora por configuração de rede, não por confiança em uma regra de roteamento.
