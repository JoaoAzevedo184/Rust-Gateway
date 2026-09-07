# Proteger uma rota com autenticação e escopos

## Configure a seção `auth`, uma vez

```yaml
auth:
  jwks_url: http://auth-service:9000/.well-known/jwks.json
  issuer: https://auth.homelab.local
  audience: rust-gateway
```

`issuer` e `audience` são comparados por igualdade exata contra as claims `iss` e `aud` do token. Se não baterem, o token é recusado com 401 mesmo tendo assinatura válida — o que é o ponto: um token emitido para outro serviço não deve valer aqui.

## Exija token na rota

```yaml
  - id: inventory
    match: { prefix: /inventory }
    upstream: inventory-service
    auth: { required: true }
```

## Exija escopos

```yaml
    auth: { required: true, scopes: [inventory.read, inventory.write] }
```

A lista é uma **conjunção**: o token precisa de todos os escopos. Acrescentar um escopo à lista sempre restringe, nunca amplia.

Escopos são lidos da claim configurada em `auth.scope_claim` (`scope`, por padrão), no formato OAuth2 — string separada por espaço. Uma claim `roles` em array, quando presente, tem seus valores somados aos escopos.

## Deixe aberto o que precisa ficar aberto

Cadastro e login não podem exigir o token que ainda não existe. Um prefixo mais longo resolve isso sem mecanismo especial:

```yaml
  - id: inventory
    match: { prefix: /inventory }
    auth: { required: true, scopes: [inventory.read] }
    upstream: inventory-service

  - id: inventory-catalogo
    match: { prefix: /inventory/catalogo }   # mais longo, vence
    auth: { required: false }
    upstream: inventory-service
```

## Verifique

```bash
curl -i localhost:8080/inventory/skus                        # 401
curl -i -H "Authorization: Bearer $TOKEN" localhost:8080/inventory/skus   # 200 ou 403
```

Distinga os dois erros ao depurar:

| Resposta | Significado |
|---|---|
| `401 unauthorized` | Sem token, ou token inválido: assinatura, `exp`, `nbf`, `iss`, `aud` ou `kid`. |
| `403 forbidden` | Token válido, escopo insuficiente. |

Para ver o que chegou ao backend, o upstream stub do Compose ecoa os headers:

```bash
curl -H "Authorization: Bearer $TOKEN" localhost:8080/users/perfil | grep X-User
```

```
X-User-Id: dev-user-1
X-User-Scopes: user.read
```

## Rate limit por usuário

Com a rota autenticada, o bucket pode ser por identidade em vez de por IP:

```yaml
    rate_limit:
      - { key: sub, capacity: 600, refill_per_sec: 10 }
```

`key: sub` só é aceito em rota com `auth.required: true`. Em rota anônima é erro de startup, porque sem token não há `sub` — e um fallback silencioso agruparia todos os anônimos no mesmo bucket.

## Armadilhas

**`required: false` com `scopes` é erro de startup.** Escopo só é verificável em rota autenticada; a combinação é ambígua o bastante para ser recusada.

**Um token inválido é recusado mesmo em rota anônima.** A rota dispensa credencial, não perdoa credencial ruim.

**Não confie no `alg` do token.** O gateway já não confia: o algoritmo vem da chave da JWKS, e `alg: none` não tem como passar. Nada a configurar aqui — mas vale saber ao comparar com outros gateways.

**Rotação de chave não exige restart.** Um `kid` desconhecido dispara um refresh imediato da JWKS, com single-flight e cooldown de `unknown_kid_cooldown`.
