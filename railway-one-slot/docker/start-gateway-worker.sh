#!/bin/sh
set -eu

APP_PORT="${PORT:-8080}"
WORKER_PORT="${WORKER_PORT:-18081}"

export PORT="$WORKER_PORT"
/app/context-worker &
worker_pid="$!"

cleanup() {
    kill "$worker_pid" 2>/dev/null || true
}
trap cleanup INT TERM EXIT

export PORT="$APP_PORT"
export WORKER_URL="${WORKER_URL:-http://127.0.0.1:${WORKER_PORT}/v1/compress}"
export WORKER_CHAT_COMPLETIONS_URL="${WORKER_CHAT_COMPLETIONS_URL:-http://127.0.0.1:${WORKER_PORT}/v1/chat/completions}"
export WORKER_MODELS_URL="${WORKER_MODELS_URL:-http://127.0.0.1:${WORKER_PORT}/v1/models}"
export WORKER_ADMIN_ACCOUNTS_URL="${WORKER_ADMIN_ACCOUNTS_URL:-http://127.0.0.1:${WORKER_PORT}/api/admin/accounts}"

/app/context-gateway &
gateway_pid="$!"

wait "$gateway_pid"
status="$?"
cleanup
exit "$status"
