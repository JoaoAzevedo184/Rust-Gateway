# Estágio de build. As dependências são compiladas em uma camada própria, para
# que uma mudança em src/ não recompile a árvore inteira de crates.
FROM rust:1-slim-bookworm AS builder

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# `cargo build` usa mtime: sem o touch, os stubs acima seriam considerados atuais.
RUN touch src/main.rs src/lib.rs && cargo build --release

# Imagem final: só o binário e as âncoras de confiança de TLS, necessárias para
# buscar a JWKS de um Auth Service em HTTPS.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home gateway

COPY --from=builder /build/target/release/rust-gateway /usr/local/bin/rust-gateway

USER 10001
EXPOSE 8080 9090

ENTRYPOINT ["/usr/local/bin/rust-gateway"]
CMD ["/etc/rust-gateway/gateway.yaml"]
