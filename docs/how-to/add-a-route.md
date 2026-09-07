# Adicionar uma rota

## Declare o upstream, se ainda não existir

```yaml
upstreams:
  inventory-service: { url: http://inventory-service:8083 }
```

Reaproveite o nome quando o serviço já estiver declarado. Rotas diferentes para o mesmo serviço **devem** apontar para o mesmo upstream: o estado de circuit breaker é por upstream, e duplicá-lo faz o circuito abrir N vezes sem proteger ninguém.

## Declare a rota

```yaml
routes:
  - id: inventory
    match: { prefix: /inventory }
    upstream: inventory-service
    auth: { required: false }
```

O bloco `auth` é obrigatório, mesmo para declarar que a rota é anônima. Uma rota que ficou pública por esquecimento é a falha mais cara que este schema permite.

## Decida se o prefixo é removido

Por padrão o caminho segue inteiro:

```
/inventory/skus/42  ->  http://inventory-service:8083/inventory/skus/42
```

Com `strip_prefix: true`:

```yaml
    strip_prefix: true
```

```
/inventory/skus/42  ->  http://inventory-service:8083/skus/42
```

A query string é sempre preservada, nos dois casos.

## Aplique e verifique

```bash
docker compose restart gateway
docker compose logs gateway | tail -5
```

Um `configuração carregada` com a contagem de rotas esperada significa que passou pela validação. Se a configuração for inválida, o processo não sobe e o log lista **todos** os problemas de uma vez.

```bash
curl -i localhost:8080/inventory/skus/42
```

## Casos que costumam surpreender

**Uma rota mais específica não precisa vir antes.** A ordem no arquivo é irrelevante: o casamento é sempre por prefixo mais longo. `/inventory/public` vence `/inventory` mesmo declarado depois.

**O prefixo casa na fronteira de segmento.** `/inventory` casa `/inventory` e `/inventory/42`, e não casa `/inventoryzinho`.

**`/inventory` e `/inventory/` são o mesmo prefixo.** Declarar os dois é erro de startup por duplicata.

**Um caminho na URL do upstream vira prefixo.** Com `url: http://svc:8080/api/v1` e `strip_prefix: true`, `/inventory/skus` chega como `/api/v1/skus`.
