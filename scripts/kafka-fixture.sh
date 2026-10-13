#!/usr/bin/env bash
# Starts the Kafka fixtures the broker tests need and prints the variables they read.
#
#   zf-kafka-redpanda   a Redpanda broker (plaintext), advertised as broker.test:$ZIRV_KAFKA_PORT
#   zf-kafka-sasl       an Apache Kafka broker with SASL PLAIN, SCRAM-SHA-256 and SCRAM-SHA-512
#                       on SASL_PLAINTEXT, advertised as broker.test:$ZIRV_KAFKA_SASL_PORT
#
# The brokers advertise the name broker.test, which the tests resolve to 127.0.0.1 through an
# injected lookup, so the same advertised address works for the test process on the host and
# proves metadata-learned brokers go through the relay. Usage:
#
#   eval "$(scripts/kafka-fixture.sh)"      # starts both, exports the variables
#   scripts/kafka-fixture.sh stop           # removes the two containers it started
#
# Only containers named zf-kafka-* are ever touched.
set -euo pipefail

PORT="${ZIRV_KAFKA_PORT:-19092}"
SASL_PORT="${ZIRV_KAFKA_SASL_PORT:-19093}"
# The TLS tests put a terminator on TLS_FRONT (the advertised port) in front of TLS_BACKEND.
TLS_FRONT="${ZIRV_KAFKA_TLS_FRONT_PORT:-19094}"
TLS_BACKEND="${ZIRV_KAFKA_TLS_BACKEND_PORT:-19095}"
REDPANDA_IMAGE="${REDPANDA_IMAGE:-redpandadata/redpanda:v25.1.1}"
KAFKA_IMAGE="${KAFKA_IMAGE:-apache/kafka:4.2.0}"

if [ "${1:-}" = "stop" ]; then
  docker rm -f zf-kafka-redpanda zf-kafka-sasl >/dev/null 2>&1 || true
  exit 0
fi

# Everything but the exports goes to stderr, so `eval "$(...)"` sees only the exports.
{
  docker rm -f zf-kafka-redpanda zf-kafka-sasl >/dev/null 2>&1 || true

  docker run -d --name zf-kafka-redpanda -p "$PORT:$PORT" -p "$TLS_BACKEND:$TLS_BACKEND" "$REDPANDA_IMAGE" \
    redpanda start --mode dev-container --smp 1 --memory 512M --reserve-memory 0M \
    --overprovisioned --node-id 0 \
    --kafka-addr "internal://0.0.0.0:$PORT,tls://0.0.0.0:$TLS_BACKEND" \
    --advertise-kafka-addr "internal://broker.test:$PORT,tls://broker.test:$TLS_FRONT" >/dev/null

  docker run -d --name zf-kafka-sasl -p "$SASL_PORT:$SASL_PORT" --entrypoint sh "$KAFKA_IMAGE" -c "
set -e
cat > /tmp/server.properties <<PROPS
process.roles=broker,controller
node.id=1
controller.quorum.voters=1@localhost:9094
controller.listener.names=CONTROLLER
listeners=INTERNAL://localhost:9095,SASL://0.0.0.0:$SASL_PORT,CONTROLLER://localhost:9094
advertised.listeners=INTERNAL://localhost:9095,SASL://broker.test:$SASL_PORT
listener.security.protocol.map=INTERNAL:PLAINTEXT,SASL:SASL_PLAINTEXT,CONTROLLER:PLAINTEXT
inter.broker.listener.name=INTERNAL
sasl.enabled.mechanisms=PLAIN,SCRAM-SHA-256,SCRAM-SHA-512
listener.name.sasl.plain.sasl.jaas.config=org.apache.kafka.common.security.plain.PlainLoginModule required user_alice=\"alice-secret\";
listener.name.sasl.scram-sha-256.sasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required;
listener.name.sasl.scram-sha-512.sasl.jaas.config=org.apache.kafka.common.security.scram.ScramLoginModule required;
offsets.topic.replication.factor=1
transaction.state.log.replication.factor=1
transaction.state.log.min.isr=1
group.initial.rebalance.delay.ms=0
log.dirs=/tmp/kraft-logs
PROPS
CLUSTER_ID=\$(/opt/kafka/bin/kafka-storage.sh random-uuid)
/opt/kafka/bin/kafka-storage.sh format -t \"\$CLUSTER_ID\" -c /tmp/server.properties \
  --add-scram 'SCRAM-SHA-256=[name=bob,password=bob-secret]' \
  --add-scram 'SCRAM-SHA-512=[name=carol,password=carol-secret]'
exec /opt/kafka/bin/kafka-server-start.sh /tmp/server.properties
" >/dev/null

  for _ in $(seq 1 60); do
    if docker exec zf-kafka-redpanda rpk cluster health 2>/dev/null | grep -q 'Healthy:.*true'; then
      break
    fi
    sleep 1
  done
  for _ in $(seq 1 90); do
    if docker logs zf-kafka-sasl 2>&1 | grep -q 'Kafka Server started'; then
      break
    fi
    sleep 1
  done
} >&2

cat <<EOF
export ZIRV_KAFKA_BROKER=broker.test:$PORT
export ZIRV_KAFKA_SASL_BROKER=broker.test:$SASL_PORT
export ZIRV_KAFKA_TLS_FRONT_PORT=$TLS_FRONT
export ZIRV_KAFKA_TLS_BACKEND_PORT=$TLS_BACKEND
EOF
