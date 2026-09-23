#!/usr/bin/env bash
# Stop and remove the simulated IPP printer. The spool directory is kept.
set -euo pipefail

cd "$(dirname "$0")/.."

docker compose down
