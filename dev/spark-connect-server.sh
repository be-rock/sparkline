#!/usr/bin/env bash
# Run a local Spark Connect server (foreground) for development and testing.
#
# Uses pyspark from PyPI (which bundles the Spark jars, including the Connect
# server) in ./.spark-venv, created on first run with uv.
#
#   SPARK_VERSION=4.0.5 PORT=15002 dev/spark-connect-server.sh
set -euo pipefail

cd "$(dirname "$0")/.."
SPARK_VERSION="${SPARK_VERSION:-4.0.5}"
PORT="${PORT:-15002}"
VENV=.spark-venv

if [ ! -x "$VENV/bin/python" ]; then
  uv venv -q --python 3.12 "$VENV"
fi
if ! "$VENV/bin/python" -c "import pyspark, sys; sys.exit(pyspark.__version__ != '$SPARK_VERSION')" 2>/dev/null; then
  uv pip install -q --python "$VENV/bin/python" "pyspark[connect]==$SPARK_VERSION"
fi

SPARK_HOME="$("$VENV/bin/python" -c 'import os, pyspark; print(os.path.dirname(pyspark.__file__))')"
export SPARK_HOME
# Binding to the LAN address breaks local jobs on some machines; stay on loopback.
export SPARK_LOCAL_IP="${SPARK_LOCAL_IP:-127.0.0.1}"
mkdir -p .spark-dev/warehouse

exec "$SPARK_HOME/bin/spark-submit" \
  --class org.apache.spark.sql.connect.service.SparkConnectServer \
  --name "sparkline-dev" \
  --conf spark.connect.grpc.binding.port="$PORT" \
  --conf spark.sql.warehouse.dir="$PWD/.spark-dev/warehouse" \
  --conf spark.driver.extraJavaOptions="-Dderby.system.home=$PWD/.spark-dev" \
  --conf spark.ui.showConsoleProgress=false \
  spark-internal
