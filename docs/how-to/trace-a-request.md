# Acompanhar uma requisição de ponta a ponta

## Ligue a exportação de traces

```bash
docker compose -f docker-compose.yml -f docker-compose.tracing.yml up --build
```

Sobe um Jaeger com o receptor OTLP ligado, e troca a configuração do gateway por uma com `tracing.otlp_endpoint` apontando para ele. A UI fica em [`localhost:16686`](http://localhost:16686).

Isso é só o destino dos spans. A propagação de `traceparent` — aceitar o contexto que chega e continuar o mesmo trace no upstream — funciona com ou sem este overlay.

## Dispare uma requisição com um trace conhecido

```bash
curl -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' \
  localhost:8080/users/perfil
```

O primeiro grupo de 32 caracteres hexadecimais é o `trace_id`; o segundo, de 16, é o `parent-id` — aqui, um valor inventado, representando "quem chamou antes do gateway".

## Encontre o trace no Jaeger

```bash
curl -s http://localhost:16686/api/traces/4bf92f3577b34da6a3ce929d0e0e4736 | jq .
```

Ou pela UI, em Search, colando o `trace_id` no campo de busca.

O span do gateway aparece como filho do `parent-id` que você inventou, com `route_id`, `status` e `request_id` como atributos:

```json
{
  "traceID": "4bf92f3577b34da6a3ce929d0e0e4736",
  "spanID": "4cd5598ab451033d",
  "references": [{ "refType": "CHILD_OF", "spanID": "00f067aa0ba902b7" }],
  "tags": [
    { "key": "route_id", "value": "users" },
    { "key": "status", "value": "200" },
    { "key": "request_id", "value": "01a0..." }
  ]
}
```

## Confirme que o upstream recebeu o trace continuado

O `traceparent` que chega ao backend tem o **mesmo `trace_id`**, mas o `parent-id` agora é o `spanID` do gateway — é a continuação, não uma cópia:

```bash
curl -H 'traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' \
  localhost:8080/users/perfil | grep -i traceparent
```

```
Traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-4cd5598ab451033d-01
```

Se o seu backend também estiver instrumentado com OpenTelemetry, ele lê esse header e o trace continua atravessando Rust → Java → Python como um único trace, não três desconexos.

## Vá do log direto para o trace

Todo span de requisição carrega o campo `trace_id`:

```bash
docker compose logs gateway | grep 4bf92f3577b34da6a3ce929d0e0e4736
```

```json
{"level":"WARN","message":"tentativa estourou o upstream_timeout","span":{"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","route_id":"payments"}}
```

O mesmo valor que aparece no log é o que você cola na busca do Jaeger — é o elo entre "o que o log diz que aconteceu" e "o que o trace mostra sobre onde o tempo foi gasto".

## Sem um `traceparent` de entrada

Um cliente sem tracing próprio (`curl` puro, por exemplo) não manda `traceparent`. O gateway não deixa o trace vazio: origina um novo, com um `trace_id` aleatório, e ele aparece no Jaeger como uma raiz — sem span pai. É o comportamento correto para o primeiro salto de uma cadeia sem tracing ainda.

## Um `traceparent` malformado

Um header que não segue o formato W3C (`00-<32 hex>-<16 hex>-<2 hex>`) é descartado, e o gateway se comporta como se ele estivesse ausente: origina um trace novo. Não é erro do cliente — é só ignorado.

## Depurando sem um coletor

Sem `tracing.otlp_endpoint` configurado, nada disto muda de comportamento: o `traceparent` de saída continua sendo reescrito corretamente, e o campo `trace_id` continua aparecendo nos logs. Só não há para onde os spans serem exportados — o que é suficiente se tudo que você precisa é correlacionar logs, sem montar um coletor.
