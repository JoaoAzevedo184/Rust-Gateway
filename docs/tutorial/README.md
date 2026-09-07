# Tutorial: do clone ao primeiro request autenticado

Nesta lição você sobe o gateway com três serviços atrás dele, vê uma requisição atravessar, descobre um header de identidade sendo descartado na borda, esbarra num limite de taxa e termina com um request autenticado por JWT chegando ao backend com a identidade injetada pelo gateway.

Leva cerca de quinze minutos. Você vai precisar de Docker com Compose, `curl` e `openssl`.

Não é preciso saber Rust: nada aqui é compilado à mão.

## 1. Suba o ambiente

```bash
git clone <url-do-repositorio> rust-gateway
cd rust-gateway
docker compose up --build -d
```

A primeira execução compila o gateway e leva alguns minutos. Ao final:

```bash
docker compose ps
```

```
SERVICE           STATUS
gateway           Up 8 seconds
order-service     Up 11 seconds
payment-service   Up 11 seconds
redis             Up 11 seconds (healthy)
user-service      Up 11 seconds
```

Cinco containers: o gateway, um Redis para o estado de rate limit e três upstreams. Os upstreams são `traefik/whoami`, um serviço que responde ecoando tudo que recebeu — o que faz dele um excelente espelho para ver o trabalho do gateway.

Confirme que o gateway subiu:

```bash
docker compose logs gateway | tail -3
```

```json
{"level":"INFO","message":"rate limit store pronto","store":"redis"}
{"level":"INFO","message":"configuração carregada","rotas":4,"upstreams":3}
{"level":"INFO","message":"gateway ouvindo","publico":"0.0.0.0:8080","admin":"0.0.0.0:9090"}
```

Os logs são JSON de propósito: são feitos para serem consultados por máquina, não lidos em sequência.

## 2. Atravesse o gateway

```bash
curl localhost:8080/users/perfil
```

```
Hostname: 17076bf10a55
Name: user-service
GET /users/perfil HTTP/1.1
Host: user-service:80
X-Forwarded-For: 172.20.0.1
X-Forwarded-Host: localhost:8080
X-Forwarded-Proto: http
X-Request-Id: 01a07981-8a94-7413-8791-dbe1e7377273
```

Você falou com `localhost:8080`, e o `user-service` respondeu. Repare no que o gateway fez no caminho:

- **Roteou** `/users/perfil` para o `user-service`, por causa do prefixo `/users` na configuração.
- **Reescreveu o `Host`** para a autoridade do upstream, guardando o original em `X-Forwarded-Host`.
- **Gerou um `X-Request-Id`**, que também volta na resposta. É por ele que se casa uma resposta com a linha de log correspondente.

Agora peça outra rota:

```bash
curl localhost:8080/orders/42
```

```
Name: order-service
GET /42 HTTP/1.1
```

Outro serviço, e o caminho mudou: `/orders/42` chegou como `/42`. Essa rota tem `strip_prefix: true`, e o backend não precisa saber sob qual prefixo foi publicado. A query string, quando existe, é sempre preservada.

Peça um caminho que não existe:

```bash
curl localhost:8080/nao-existe
```

```json
{"error":"not_found","message":"No route matches this path","request_id":"01a07981-8ac6-7ca1-8519-6bcc9e211d41"}
```

Todo erro **gerado pelo gateway** tem esse formato, com o correlation id dentro. Erros gerados pelos backends passam intactos.

## 3. Veja um header de identidade ser descartado

Vamos tentar enganar o gateway, dizendo a ele que somos administradores:

```bash
curl -H 'X-User-Id: administrador' localhost:8080/users/perfil | grep -i x-user
```

Nenhuma saída. O header não chegou ao backend.

Isso não é sobre esta rota estar protegida — ela é anônima. O gateway remove `X-User-Id`, `X-User-Scopes` e qualquer `X-Auth-*` de **toda** requisição que entra, antes mesmo de descobrir qual rota é.

A razão é direta: se a remoção morasse junto com a autenticação, uma rota anônima — que não roda autenticação — repassaria a identidade forjada direto ao backend. Com a regra sendo global, o backend pode confiar: se um `X-User-Id` chegou, foi o gateway que escreveu.

## 4. Esbarre em um limite de taxa

A rota `/payments` está configurada com `capacity: 30`. Gaste os trinta:

```bash
for i in $(seq 1 32); do curl -s -o /dev/null -w '%{http_code} ' localhost:8080/payments; done
```

```
200 200 200 ... 200 429 429
```

Exatamente trinta passaram. Veja o que a recusa diz:

```bash
curl -i localhost:8080/payments
```

```
HTTP/1.1 429 Too Many Requests
retry-after: 2
x-ratelimit-limit: 30
x-ratelimit-remaining: 0
```

```json
{"error":"rate_limited","message":"Request rate exceeded for this route","request_id":"..."}
```

O `retry-after` não é um número redondo escolhido a esmo: é calculado a partir da taxa de recuperação do bucket. Espere isso e você terá um token.

O estado vive no Redis, não na memória do processo:

```bash
docker compose exec redis redis-cli --scan --pattern 'rl:v1:*'
```

```
rl:v1:payments:ip:172.20.0.1
rl:v1:users:ip:172.20.0.1
```

É por isso que subir uma segunda réplica do gateway não dobra o limite: as duas consultam o mesmo bucket.

Aguarde alguns segundos e tente de novo — o bucket se recupera sozinho.

## 5. Ligue a autenticação

Até aqui todas as rotas eram anônimas. Vamos exigir um JWT.

O gateway **não emite** tokens: ele valida os que um Auth Service emitiu. Como não há Auth Service aqui, o repositório traz um de desenvolvimento, que gera a própria chave na sua máquina:

```bash
./scripts/dev-auth.sh init
```

```
chave em      /caminho/para/rust-gateway/.dev-auth/key.pem
JWKS em       /caminho/para/rust-gateway/.dev-auth/jwks.json
issuer        https://auth.dev.local
audience      rust-gateway
```

Duas peças. A **chave privada**, que assina tokens e nunca sai da sua máquina. E a **JWKS**, que publica só a parte pública, e é o que o gateway busca para conferir assinaturas.

Suba o ambiente de novo, agora com a autenticação ligada:

```bash
docker compose -f docker-compose.yml -f docker-compose.auth.yml up -d --build
```

```bash
docker compose logs gateway | grep -i jwks
```

```json
{"level":"INFO","message":"JWKS atualizada","keys":1}
```

O gateway já buscou a chave pública. A partir daqui ele valida tokens sem tocar na rede: a JWKS fica em cache, com refresh em segundo plano.

## 6. Faça o primeiro request autenticado

Sem token, a rota agora recusa:

```bash
curl -i localhost:8080/users/perfil
```

```
HTTP/1.1 401 Unauthorized
```

Peça um token ao Auth Service de desenvolvimento e use-o:

```bash
TOKEN=$(./scripts/dev-auth.sh token "user.read")
curl -H "Authorization: Bearer $TOKEN" localhost:8080/users/perfil
```

```
Name: user-service
GET /users/perfil HTTP/1.1
Authorization: Bearer eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImRldi1rZXktMSJ9...
X-Auth-Method: jwt
X-User-Id: dev-user-1
X-User-Scopes: user.read
```

Este é o objetivo da lição. Repare no que o backend recebeu:

- **`X-User-Id`**, com o `sub` do token. O backend não precisa validar JWT para saber quem está falando.
- **`X-User-Scopes`**, com os escopos concedidos.
- **`Authorization` intacto**, para o backend que quiser reexaminar o token por conta própria.

E, pelo passo 3, o backend sabe que esses headers são confiáveis: nenhum `X-User-Id` de fora sobrevive à borda.

## 7. Veja os escopos serem exigidos

A rota `/users` exige o escopo `user.read`. Peça um token sem ele:

```bash
TOKEN_ERRADO=$(./scripts/dev-auth.sh token "outra.coisa")
curl -i -H "Authorization: Bearer $TOKEN_ERRADO" localhost:8080/users/perfil
```

```
HTTP/1.1 403 Forbidden
```

**403, não 401.** A diferença é deliberada: 401 diz "identifique-se", 403 diz "sei quem você é, e você não pode". Trocar um pelo outro faria o cliente tentar renovar um token que já estava bom.

Agora um token expirado:

```bash
TOKEN_VELHO=$(./scripts/dev-auth.sh token "user.read" -3600)
curl -i -H "Authorization: Bearer $TOKEN_VELHO" localhost:8080/users/perfil
```

```
HTTP/1.1 401 Unauthorized
```

De volta ao 401: a credencial em si não vale.

## 8. Olhe as métricas

A porta administrativa não é publicada no host, de propósito: `/metrics` e `/ready` não passam pela pilha de políticas, e não deveriam estar acessíveis de fora. Consulte de dentro da rede:

```bash
docker compose run --rm --entrypoint sh redis -c 'wget -qO- http://gateway:9090/metrics' \
  | grep gateway_requests_total
```

```
gateway_requests_total{method="GET",outcome="ok",route="users",status="200"} 4
gateway_requests_total{method="GET",outcome="rejected_auth",route="users",status="401"} 2
gateway_requests_total{method="GET",outcome="rejected_ratelimit",route="payments",status="429"} 2
gateway_requests_total{method="GET",outcome="no_route",route="unmatched",status="404"} 1
```

Cada coisa que você fez nesta lição está aqui. Repare no label `route`: ele é sempre o `id` vindo da configuração, nunca o caminho da requisição. Um caminho cru como label transformaria um scanner de diretórios em um incidente de memória no Prometheus — e `unmatched` é o valor constante que absorve tudo que não casou.

## 9. Encerre

```bash
docker compose -f docker-compose.yml -f docker-compose.auth.yml down -v
```

## O que você viu

| Passo | Comportamento |
|---|---|
| 2 | Roteamento por prefixo, reescrita de `Host`, `strip_prefix`, correlation id, corpo de erro padrão |
| 3 | Scrub de identidade, global e incondicional |
| 4 | Token bucket, `Retry-After` calculado, estado compartilhado no Redis |
| 5–6 | Validação de JWT por JWKS em cache, identidade injetada para o backend |
| 7 | Escopos em conjunção, e a distinção entre 401 e 403 |
| 8 | Métricas com cardinalidade controlada, em listener separado |

## Para onde ir agora

- Para resolver uma tarefa concreta — adicionar uma rota, subir réplicas, investigar um 429 —, veja os [guias how-to](../how-to/).
- Para o detalhe exato de um campo, status ou métrica, veja a [referência](../reference/).
- Para entender **por que** cada uma dessas decisões foi tomada, veja as [explicações](../explanation/). O scrub global, a ordem dos layers e o "falha aberto" do rate limit têm razões que valem a leitura.
