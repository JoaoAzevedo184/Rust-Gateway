#!/usr/bin/env bash
#
# Benchmark de carga (spec §13, §14 Fase 3): RPS, P50, P95, P99, CPU e RAM.
#
# Usa `oha` (https://github.com/hatoolab/oha), um gerador de carga HTTP em Rust
# com relatório de percentis embutido. Instale com `cargo install oha`.
#
# Roda contra o gateway do Docker Compose já no ar, e amostra `docker stats` do
# container em paralelo para capturar CPU e RAM sob a mesma carga.
#
#   docker compose up -d --build
#   ./scripts/load-test.sh                      # rota anônima, 50 conexões, 20s
#   ./scripts/load-test.sh /orders/42 100 30s    # rota, concorrência, duração

set -euo pipefail

URL="${1:-http://localhost:8080/users/perfil}"
CONCURRENCY="${2:-50}"
DURATION="${3:-20s}"
CONTAINER="${GATEWAY_CONTAINER:-rust-gateway-gateway-1}"

require() {
    command -v "$1" >/dev/null 2>&1 || { echo "erro: $1 não encontrado no PATH" >&2; exit 1; }
}

require oha
require docker

if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    echo "erro: container '$CONTAINER' não está rodando." >&2
    echo "Suba o Compose primeiro: docker compose up -d --build" >&2
    echo "Ou informe o nome certo em GATEWAY_CONTAINER=<nome> $0 ..." >&2
    exit 1
fi

echo "alvo:        $URL"
echo "concorrência: $CONCURRENCY"
echo "duração:     $DURATION"
echo "container:   $CONTAINER"
echo

# Amostra docker stats em paralelo, uma vez por segundo, para o resto da
# execução. `--no-stream` some por si só se rodasse fora de um loop.
STATS_FILE="$(mktemp)"
trap 'rm -f "$STATS_FILE"' EXIT

(
    while true; do
        docker stats --no-stream --format '{{.CPUPerc}} {{.MemUsage}}' "$CONTAINER" >> "$STATS_FILE" 2>/dev/null
        sleep 1
    done
) &
STATS_PID=$!
trap 'kill "$STATS_PID" 2>/dev/null; rm -f "$STATS_FILE"' EXIT

oha --no-tui -z "$DURATION" -c "$CONCURRENCY" "$URL" | tee /dev/stderr | \
    grep -E "Requests/sec|Total:|Slowest|Fastest|Average|50%|90%|95%|99%|Status" || true

kill "$STATS_PID" 2>/dev/null
wait "$STATS_PID" 2>/dev/null || true

echo
echo "=== CPU e RAM do gateway durante a carga ($(wc -l < "$STATS_FILE") amostras) ==="
python3 - "$STATS_FILE" <<'PY'
import sys

path = sys.argv[1]
cpu, mem = [], []

with open(path) as f:
    for line in f:
        parts = line.split()
        if len(parts) < 2:
            continue
        try:
            cpu.append(float(parts[0].rstrip('%')))
        except ValueError:
            continue
        mem.append(parts[1])

if cpu:
    print(f"CPU:  média {sum(cpu)/len(cpu):.1f}%  pico {max(cpu):.1f}%")
if mem:
    print(f"RAM:  {mem[-1]} (última amostra)")
PY
