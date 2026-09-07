# Modelo de autenticação

Este documento explica como responsabilidade de autenticação e autorização se divide entre o Auth Service, o gateway e os serviços de backend — e por que o gateway faz menos do que poderia.

## Três responsabilidades, três lugares

```
Auth Service     autentica e emite JWT
Rust Gateway     valida o token e aplica política de rota
Backend          aplica autorização específica de domínio
```

O Auth Service é dono dos usuários, das senhas, do login, dos refresh tokens e da emissão dos JWTs. O gateway **não emite tokens e não acessa banco de usuários**. Ele extrai o Bearer token, valida assinatura e claims, verifica os scopes que a rota exige e encaminha.

A separação importa porque as duas metades falham de formas diferentes e escalam de formas diferentes. Validar um JWT é uma operação local, sem I/O, que escala junto com as réplicas do gateway. Emitir um token exige banco, hash de senha e estado. Fundir os dois faria o caminho de leitura mais quente do sistema depender do componente com mais estado.

## O que o gateway pode e não pode decidir

O gateway sabe *quem* é o requisitante e *que permissões amplas* ele carrega. Ele não sabe nada sobre o domínio.

`GET /orders/42` com um token válido carregando o scope `order.read` passa pelo gateway. Se o pedido 42 pertence a esse usuário é uma pergunta que exige consultar o banco de pedidos, e essa é exatamente a fronteira: o gateway faz a checagem que não precisa de dados, o backend faz a que precisa.

A tentação de mover mais autorização para o gateway é forte, porque centraliza. Ela custa caro: o gateway passaria a precisar de acesso a dados de domínio, e uma mudança de regra de negócio viraria um deploy de infraestrutura.

## Por que assinatura assimétrica e JWKS

A alternativa simples seria um segredo HMAC compartilhado entre Auth Service e gateway. Ela funciona, e tem duas propriedades ruins.

A primeira é que quem valida o token também pode forjá-lo — a mesma chave faz as duas coisas. A segunda é operacional: rotacionar o segredo exige distribuir o valor novo para todos os consumidores simultaneamente, o que na prática significa uma janela em que alguma coisa está com a chave errada.

Com assinatura assimétrica, o Auth Service guarda a chave privada e publica as públicas em um endpoint JWKS. O gateway busca esse conjunto e o mantém em cache. Cada token traz um `kid` no header indicando qual chave o assinou, então o Auth Service pode publicar a chave nova, começar a assinar com ela, e aposentar a antiga depois — sem coordenação, sem janela.

O gateway não precisa saber que houve rotação. Ele encontra um `kid` que não conhece, busca o JWKS de novo, e segue.

## O `kid` desconhecido, e por que ele tem cooldown

Se o gateway só atualizasse o JWKS no ciclo periódico, toda rotação de chave produziria até um intervalo inteiro de refresh de 401 em massa — os tokens novos chegariam assinados com uma chave que o cache ainda não tem.

Então `kid` desconhecido dispara uma busca imediata. Duas proteções cercam esse mecanismo:

**Single-flight.** Várias requisições concorrentes com o mesmo `kid` novo esperam a mesma busca, em vez de dispararem uma cada. Sem isso, o instante da rotação viraria uma rajada proporcional ao tráfego contra o Auth Service — exatamente quando ele está em transição.

**Cooldown.** Há um intervalo mínimo entre buscas forçadas. Sem ele, qualquer cliente poderia mandar tokens com `kid` aleatório em loop e transformar o gateway em um amplificador de tráfego contra o Auth Service. O cooldown transforma um vetor de negação de serviço em um custo fixo.

## Cache stale: a indisponibilidade que preferimos

Se o Auth Service estiver inacessível, o gateway continua aceitando o snapshot de JWKS anterior por uma janela configurável. Passando dela, responde 503.

Isso é um trade-off explícito, com um custo real: uma chave revogada durante a queda do Auth Service continua sendo aceita até a janela expirar.

Aceitamos esse custo porque a alternativa é pior na prática. Sem cache stale, um restart de trinta segundos do Auth Service derruba todo o tráfego autenticado do sistema. Revogação de chave é um evento raro e deliberado; restart é rotina. Otimizar para o caso raro ao custo do caso comum produz um sistema frágil.

Quem quiser deslocar esse equilíbrio reduz a janela — o mecanismo continua o mesmo.

## Por que autenticação falha fechado

Rate limit falha aberto quando o Redis cai: a requisição passa sem ser contada. Autenticação faz o oposto — se o gateway não consegue validar, responde 503 e não encaminha.

A assimetria é intencional. Rate limit é uma proteção contra abuso, e derrubar tráfego legítimo porque a infraestrutura de contagem caiu causa mais dano do que o abuso que ela evitaria. Autenticação é uma garantia de correção: encaminhar uma requisição não validada significa entregar ao backend algo que ele confia sem base. Não existe versão degradada aceitável disso.

## O token original segue adiante

O gateway preserva o header `Authorization` intacto ao encaminhar, além de injetar `X-User-Id`, `X-User-Scopes` e `X-Auth-Method`.

Os headers derivados existem por conveniência: um controller em Java lê `X-User-Id` sem precisar de biblioteca de JWT. O token original existe porque o backend pode precisar de claims que o gateway não extraiu, e porque um serviço que queira revalidar por conta própria deve poder fazê-lo.

Isso só é seguro porque os headers de identidade vindos do cliente foram removidos na borda. Veja [Arquitetura](architecture.md#por-que-o-scrub-de-identidade-é-global).
