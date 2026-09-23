#!/usr/bin/env bash
# Start the simulated IPP printer (see compose.yaml) and wait until it answers
# IPP requests. Received jobs are spooled to .docker/dev/ipp-server/spool.
# Set SIM_PORT to publish on a host port other than 1631.
set -euo pipefail

cd "$(dirname "$0")/.."

mkdir -p .docker/dev/ipp-server/config .docker/dev/ipp-server/spool
docker compose up -d ipp-server

port="${SIM_PORT:-1631}"
uri="ipp://localhost:$port/ipp/print"
for _ in $(seq 1 30); do
    if curl -s -o /dev/null -H 'Content-Type: application/ipp' --data-binary '' "http://localhost:$port/ipp/print" 2>/dev/null; then
        echo "Simulated printer ready at $uri"
        echo "Logs: docker compose logs -f ipp-server"
        exit 0
    fi
    sleep 1
done

echo "Simulated printer did not become ready; check: docker compose logs ipp-server" >&2
exit 1
