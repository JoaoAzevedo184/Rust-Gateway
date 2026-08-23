## 1. API Gateway

Um excelente projeto de Rust para portfólio.

```
Client
  ↓
Rust Gateway
  ├── User Service
  ├── Payment Service
  └── Order Service
```

Implemente:

- reverse proxy;
- JWT;
- rate limiting;
- circuit breaker;
- retries;
- timeout;
- correlation ID;
- health checks;
- métricas.

Stack:

```
Rust
Tokio
Hyper
Axum
Tower
Redis
Prometheus
OpenTelemetry
Docker
```

Você pode configurar rotas:

```yaml
routes:
  - path: /users
    upstream: http://user-service:8080

  - path: /orders
    upstream: http://order-service:8081
```

Depois medir:

```
Requests/sec
P50
P95
P99
CPU
RAM
```

Esse tipo de projeto mostra Rust em um contexto onde a escolha da linguagem faz sentido.