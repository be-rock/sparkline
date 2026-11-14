#!/usr/bin/env bash
# Runs a command with a Spark Connect server at sc://localhost:$PORT.
# Uses a server that's already listening, otherwise starts a temporary one
# (dev/spark-connect-server.sh) and stops it afterwards.
#
#   dev/with-server.sh cargo test -p sparkline-core --test live
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-15002}"
STARTUP_TIMEOUT="${STARTUP_TIMEOUT:-300}"
export SPARKLINE_TEST_REMOTE="sc://localhost:$PORT"

listening() { nc -z localhost "$PORT" 2>/dev/null; }

if listening; then
  exec "$@"
fi

mkdir -p .spark-dev
LOG=.spark-dev/server.log
echo "Starting Spark Connect server on :$PORT (log: $LOG)..." >&2
PORT="$PORT" dev/spark-connect-server.sh >"$LOG" 2>&1 &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 "$STARTUP_TIMEOUT"); do
  listening && break
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "Spark Connect server exited during startup:" >&2
    tail -50 "$LOG" >&2
    exit 1
  fi
  sleep 1
done
if ! listening; then
  echo "Spark Connect server not listening after ${STARTUP_TIMEOUT}s:" >&2
  tail -50 "$LOG" >&2
  exit 1
fi

"$@"
