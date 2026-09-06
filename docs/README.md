# Documentação do Rust Gateway

Esta documentação segue o [framework Diátaxis](https://diataxis.fr/), que separa
documentação por aquilo que o leitor precisa no momento.

| Quadrante | Serve para | Estado |
|---|---|---|
| [Explanation](explanation/) | Entender por que o gateway é como é | Disponível |
| [Reference](reference/) | Consultar campos, códigos e métricas | Fase 1 |
| [How-to](how-to/) | Resolver uma tarefa específica | Fase 1 |
| [Tutorial](tutorial/) | Aprender do zero, com o gateway rodando | Fase 1 |

Os três últimos quadrantes descrevem software em funcionamento e serão escritos junto
com o código da Fase 1, verificados contra o binário real. Documentação de uso escrita
antes do uso erra nos detalhes, e detalhe errado é pior que ausência.

## Explanation

- [Arquitetura](explanation/architecture.md) — a pilha de processamento, o que é global
  e o que é por rota, e onde vive o estado.
- [Modelo de autenticação](explanation/authentication-model.md) — a fronteira entre
  Auth Service, gateway e backend, e o que o gateway deliberadamente não faz.
- [Rate limiting](explanation/rate-limiting.md) — por que token bucket, por que a
  decisão roda dentro do store, e por que ele falha aberto.
- [Resiliência](explanation/resilience.md) — a ordem entre retry e circuit breaker, o
  que conta como falha, e por que o breaker não é compartilhado.
- [Observabilidade](explanation/observability.md) — liveness contra readiness, saúde
  passiva contra probe ativo, e a regra de cardinalidade.

## Documentos relacionados

A [spec de design](superpowers/specs/2026-09-06-rust-gateway-design.md) é o registro
histórico da decisão, com o schema completo e as decisões arquiteturais numeradas.
Ela não é mantida em sincronia com o código: quando divergir, o código e esta
documentação vencem.
