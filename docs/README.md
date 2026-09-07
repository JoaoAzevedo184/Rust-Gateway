# Documentação do Rust Gateway

Esta documentação segue o [framework Diátaxis](https://diataxis.fr/), que separa documentação por aquilo que o leitor precisa no momento.

| Quadrante | Serve para |
|---|---|
| [Tutorial](tutorial/) | Aprender do zero, com o gateway rodando |
| [How-to](how-to/) | Resolver uma tarefa específica |
| [Reference](reference/) | Consultar campos, códigos e métricas |
| [Explanation](explanation/) | Entender por que o gateway é como é |

Se é seu primeiro contato, comece pelo [tutorial](tutorial/): em quinze minutos você sobe o gateway e faz um request autenticado atravessá-lo.

## Tutorial

- [Do clone ao primeiro request autenticado](tutorial/) — uma lição guiada, com o ambiente completo no Docker.

## How-to

- [Adicionar uma rota](how-to/add-a-route.md)
- [Proteger uma rota com autenticação e escopos](how-to/protect-a-route.md)
- [Subir múltiplas réplicas](how-to/multiple-replicas.md)
- [Investigar um 429 inesperado](how-to/investigate-429.md)
- [Ajustar timeouts, retry e circuit breaker](how-to/tune-resilience.md)
- [Investigar um 502, 503 ou 504](how-to/investigate-upstream-failures.md)

## Reference

- [Configuração](reference/configuration.md) — todo campo do YAML, com tipo, default e regra de validação.
- [Erros](reference/errors.md) — códigos de status, corpo de erro e o que cada situação produz.
- [Headers](reference/headers.md) — o que o gateway remove, reescreve e injeta, em cada sentido.
- [Observabilidade](reference/observability.md) — endpoints administrativos e métricas com seus labels.

## Explanation

- [Arquitetura](explanation/architecture.md) — a pilha de processamento, o que é global e o que é por rota, e onde vive o estado.
- [Modelo de autenticação](explanation/authentication-model.md) — a fronteira entre Auth Service, gateway e backend, e o que o gateway deliberadamente não faz.
- [Rate limiting](explanation/rate-limiting.md) — por que token bucket, por que a decisão roda dentro do store, e por que ele falha aberto.
- [Resiliência](explanation/resilience.md) — a ordem entre retry e circuit breaker, o que conta como falha, e por que o breaker não é compartilhado.
- [Observabilidade](explanation/observability.md) — liveness contra readiness, saúde passiva contra probe ativo, e a regra de cardinalidade.

## Documentos relacionados

A [spec de design](superpowers/specs/2026-09-06-rust-gateway-design.md) é o registro histórico da decisão, com o schema completo e as decisões arquiteturais numeradas. Ela não é mantida em sincronia com o código: quando divergir, o código e esta documentação vencem.
