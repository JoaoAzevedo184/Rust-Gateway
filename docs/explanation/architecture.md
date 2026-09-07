# Arquitetura

Este documento explica como uma requisição atravessa o gateway e por que a estrutura é essa. Para consultar campos de configuração, veja a referência; para adicionar uma rota, veja os guias how-to.

## O problema que o gateway resolve

Atrás do gateway há serviços em Java e Python. Cada um deles precisaria, sozinho, validar tokens, limitar taxa, propagar contexto de trace e se proteger de vizinhos lentos. Isso é código idêntico reescrito em duas linguagens, com duas chances de errar.

O gateway concentra o que é política de borda. Os serviços continuam responsáveis pelo que é específico de domínio — se *este* usuário pode ver *aquele* pedido é uma pergunta que só o Order Service sabe responder.

## A pilha de processamento

```
Request
  │
  ├─ correlation_id        globais
  ├─ identity_scrub
  ├─ tracing_span
  ├─ metrics
  ├─ body_limit
  │
  ├─ route_resolve         resolve o prefixo, ou 404
  │
  ├─ auth                  por rota
  ├─ rate_limit
  ├─ retry
  │   └─ circuit_breaker
  │       └─ upstream_timeout
  │
  └─ proxy → upstream
```

A divisão em dois grupos é a decisão estrutural do gateway, e não é arbitrária.

## Por que globais e por rota são grupos separados

Uma alternativa considerada foi compilar, para cada rota, uma pilha Tower contendo exatamente os layers que aquela rota precisa. Rota anônima não teria layer de autenticação; rota sem limite não teria limiter. É a forma mais idiomática de usar Tower, e mais eficiente no papel.

Ela quebra em um detalhe decisivo: **uma requisição rejeitada nunca chega à rota**.

Se o path não casa com prefixo nenhum, não existe rota para despachar — mas essa requisição ainda precisa aparecer nas métricas, carregar um correlation ID e produzir um erro rastreável. O mesmo vale para um 401: o token foi rejeitado, e é justamente aí que você quer observabilidade. Com stacks por rota, esses casos caem fora de toda a instrumentação, que é onde ela mais importa.

Então o gateway inverte: tudo que precisa envolver *qualquer* requisição, inclusive as que não chegam a lugar nenhum, é um layer global fixo. Só depois a rota é resolvida, e os layers de política leem a rota resolvida das extensions da requisição.

O custo é que todo layer de política roda sempre, mesmo quando a rota não pede nada dele. Na prática isso é um branch em `Option` — irrelevante ao lado de uma chamada de rede.

O benefício colateral é testabilidade. Cada layer de política é testável isoladamente: injeta-se um `RouteRuntime` nas extensions, um serviço interno de mentira, e testa-se o layer sem levantar roteador nem servidor.

## Por que o scrub de identidade é global

O gateway injeta `X-User-Id` e `X-User-Scopes` para os backends consumirem. Isso só é seguro se o backend puder confiar que esses headers vieram do gateway — o que exige remover qualquer header equivalente enviado pelo cliente.

A remoção poderia parecer trabalho do layer de autenticação. Não é: **rota anônima não roda autenticação**. Se o scrub morasse ali, `POST /users/signup` repassaria ao User Service um `X-User-Id` escolhido pelo cliente, e o backend confiaria nele. Isso é escalada de privilégio, entregue pela peça de infraestrutura que existe para impedi-la.

Por isso o scrub é global e incondicional, antes até da resolução de rota. A regra fica auditável em uma frase: nenhum header de identidade sobrevive à borda; os únicos que chegam ao upstream foram escritos pelo gateway.

## Onde vive o estado

Há dois tipos de estado, e misturá-los causa um bug sutil.

**Configuração** é um snapshot imutável, um `Arc<RouterTable>` publicado em um canal de watch. O caminho da requisição só lê o `Arc` corrente. Quando a configuração mudar — hoje nunca, no futuro por hot reload ou service discovery — um provider publica um snapshot novo e as requisições seguintes o enxergam. Nada no caminho da requisição precisa saber disso.

**Estado vivo** — buckets de rate limit, estado de circuit breaker — vive fora do snapshot, em registries no `AppState`, chaveados por identificador de rota e de upstream.

A separação existe porque estado vivo tem vida mais longa que a configuração que o criou. Se o estado do circuit breaker morasse dentro do snapshot, recarregar a configuração zeraria todos os circuitos. Você editaria um limite de rate para responder a um incidente e, no mesmo gesto, fecharia todos os circuitos abertos que estavam protegendo os upstreams em dificuldade. Registries separados tornam isso impossível por construção.

## Por que um crate só

O gateway é um crate com módulos, não um workspace. Workspaces se pagam quando você precisa forçar aciclicidade entre times ou publicar partes independentemente; aqui só piorariam o tempo de compilação. Se um módulo crescer a ponto de justificar isolamento, promovê-lo a crate é uma mudança mecânica.
