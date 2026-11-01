#!/usr/bin/env bash
# The etcds bridge to brokers over TLS with logins - Artemis (AMQP 1.0, amqps), Mosquitto
# (MQTT 5, mqtts), Kafka (kafkas) - and Python clients reading back what it sent
# (python/: what an application does). Each broker trusts a test CA made for the run
# (certs.sh); each bridge trusts it through `?ca=` and logs in. Then what must be refused:
# a bridge trusting another CA, or the public roots, or with a wrong password; a client
# with no TLS or the wrong CA, to each broker, or a wrong password, to the two with logins;
# the bridge's own listener with a wrong login or none; a bridge prefix, a bridge queue or a listener client's queue the node keeps
# local; and ca= trusting the file's CAs alone - every one in it, and not the system's.
#
#   DOCKER_HOST=ssh://<docker host> ./run.sh      (KEEP=1: the containers stay)
#
# Everything runs in one docker network on DOCKER_HOST (no host port is published); the
# images are built there. etcds is built here first (cargo build --features bridge) for an
# ubuntu:24.04 image: a debug build, as a release build leaves out the debug log lines the
# acknowledgement check reads.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
W="$(mktemp -d)"
# this run's passwords: the logins' (brokers, listener) with an @, a % and a ? - in a URL
# written %40 %25 %3F, read back decoded - and the key stores' (a URI in Artemis' acceptor)
PW="s@c%?$(date +%s)$RANDOM"
PWU="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$PW")"
KPW="ks$(date +%s)$RANDOM"
NET=sec-net
VOL=sec-certs
CLUSTER=7a1e3b9c-5d2f-4e8a-9b6c-0d1e2f3a4b5c
pass=0; fail=0
ok()  { echo "  OK    $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2], got [$3]"; fi; }
say() { echo "-- $(date -u +%T) $*"; }

# the sec-* containers, the network and the volume removed; over ssh one `rm` can miss one
clear_all() {
	local _
	for _ in 1 2 3; do
		docker rm -f $(docker ps -aq --filter name=^sec-) >/dev/null 2>&1
		docker volume rm $VOL >/dev/null 2>&1; docker network rm $NET >/dev/null 2>&1
		[ -z "$(docker ps -aq --filter name=^sec-)" ] && ! docker network inspect $NET >/dev/null 2>&1 && return 0
		sleep 2
	done
	return 1
}
cleanup() {
	clear_all || echo "  left on the docker host: $(docker ps -a --filter name=^sec- --format '{{.Names}}' | tr '\n' ' ')"
	rm -rf "$W"
	true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
# what a run before left (KEEP=1, or one cut short): gone, or no run - a container left
# would serve this run with an older image
clear_all || { bad "containers of a run before still there: $(docker ps -a --filter name=^sec- --format '{{.Names}}' | tr '\n' ' ')"; exit 1; }

# a container's log holds `pattern` (read whole: no grep -q under pipefail) within `secs`
wait_log() { # wait_log <container> <pattern> [secs]
	local i
	for i in $(seq "${3:-120}"); do
		[ "$(docker logs "$1" 2>&1 | grep -c -- "$2")" -gt 0 ] && return 0
		sleep 1
	done
	return 1
}
py() { docker run --rm --network $NET -v $VOL:/certs:ro sec-python python3 "$@"; }
put() { docker exec "$1" etcdctl --endpoints=127.0.0.1:2379 put "$2" "$3" >/dev/null; }
uuid() { python3 -c 'import uuid; print(uuid.uuid4())'; }
# bridge <name> <flags...>: an etcds node of its own with a bridge from its settings
bridge() {
	local n="$1"; shift
	docker run -d --name "$n" --network $NET -v $VOL:/certs:ro sec-etcds --cluster $CLUSTER --node "$(uuid)" \
		--listen-client-urls 0.0.0.0:2379 "$@" >/dev/null
	for _ in $(seq 30); do docker exec "$n" etcdctl --endpoints=127.0.0.1:2379 endpoint health >/dev/null 2>&1 && return 0; sleep 1; done
	bad "$n: etcds did not start: $(docker logs "$n" 2>&1 | tail -2 | tr '\n' ' ')"
}
# every message the bridge of node `n` took was acknowledged: `count` deletes of a delivery key
# etcd answered (the bridge logs "acknowledged <key>" at debug only once the delete succeeded;
# a fresh consumer sees nothing either way while the bridge's stream still holds a message)
drained() { # drained <node> <count>
	local i
	for i in $(seq 20); do
		[ "$(docker logs "$1" 2>&1 | grep -c 'acknowledged /q/')" -ge "$2" ] && return 0
		sleep 1
	done
	return 1
}

echo "== the etcds bridge over TLS, with logins, read back by Python clients =="
say "etcds with the bridge, built here"
(cd "$ROOT" && cargo build --features bridge -q) || { bad "cargo build"; exit 1; }
say "certificates: a CA for the run, the brokers' certificate, a rogue CA"
bash "$HERE/certs.sh" "$W/certs" "$KPW" || { bad "certs.sh"; exit 1; }
printf '%s' "$KPW" > "$W/certs/creds"
cat "$W/certs/rogue.pem" "$W/certs/ca.pem" > "$W/certs/both.pem"   # a CA file of two: a rotation
cp "$HERE/mosquitto.conf" "$W/certs/"
docker run --rm eclipse-mosquitto:2.0.22 sh -c "mosquitto_passwd -b -c /tmp/p app '$PW' && cat /tmp/p" > "$W/certs/passwd" 2>/dev/null \
	|| { bad "mosquitto_passwd"; exit 1; }
chmod 644 "$W/certs/passwd" "$W/certs/creds"

say "images: etcds with etcdctl, the Python clients"
mkdir -p "$W/etcds"; cp "$ROOT/target/debug/etcds" "$HERE/Dockerfile.etcds" "$W/etcds/"
docker build -q -t sec-etcds -f "$W/etcds/Dockerfile.etcds" "$W/etcds" >/dev/null || { bad "docker build sec-etcds"; exit 1; }
docker build -q -t sec-python -f "$HERE/Dockerfile.python" "$HERE" >/dev/null || { bad "docker build sec-python"; exit 1; }
docker network create $NET >/dev/null
docker volume create $VOL >/dev/null
docker create --name sec-fill -v $VOL:/certs python:3.12-slim true >/dev/null
docker cp "$W/certs/." sec-fill:/certs/ && docker rm sec-fill >/dev/null

say "brokers: TLS only (Kafka's TLS for clients), each a login"
docker run -d --name sec-mosquitto --network $NET -v $VOL:/certs:ro eclipse-mosquitto:2.0.22 \
	mosquitto -c /certs/mosquitto.conf >/dev/null
docker run -d --name sec-artemis --network $NET -v $VOL:/certs:ro -e ARTEMIS_USER=app -e ARTEMIS_PASSWORD="$PW" \
	-e ANONYMOUS_LOGIN=false apache/activemq-artemis:2.42.0 >/dev/null
docker run -d --name sec-kafka --network $NET -v $VOL:/etc/kafka/secrets:ro \
	-e KAFKA_NODE_ID=1 -e KAFKA_PROCESS_ROLES=broker,controller \
	-e KAFKA_LISTENERS=SSL://:9093,INTERNAL://:9092,CONTROLLER://:9094 \
	-e KAFKA_ADVERTISED_LISTENERS=SSL://sec-kafka:9093,INTERNAL://sec-kafka:9092 \
	-e KAFKA_LISTENER_SECURITY_PROTOCOL_MAP=SSL:SSL,INTERNAL:PLAINTEXT,CONTROLLER:PLAINTEXT \
	-e KAFKA_INTER_BROKER_LISTENER_NAME=INTERNAL -e KAFKA_CONTROLLER_LISTENER_NAMES=CONTROLLER \
	-e KAFKA_CONTROLLER_QUORUM_VOTERS=1@localhost:9094 -e KAFKA_OFFSETS_TOPIC_REPLICATION_FACTOR=1 \
	-e KAFKA_TRANSACTION_STATE_LOG_REPLICATION_FACTOR=1 -e KAFKA_TRANSACTION_STATE_LOG_MIN_ISR=1 \
	-e KAFKA_AUTO_CREATE_TOPICS_ENABLE=false -e KAFKA_SSL_CLIENT_AUTH=none \
	-e KAFKA_SSL_KEYSTORE_FILENAME=broker.p12 -e KAFKA_SSL_KEYSTORE_TYPE=PKCS12 \
	-e KAFKA_SSL_KEYSTORE_CREDENTIALS=creds -e KAFKA_SSL_KEY_CREDENTIALS=creds \
	-e KAFKA_SSL_KEYSTORE_LOCATION=/etc/kafka/secrets/broker.p12 -e KAFKA_SSL_KEYSTORE_PASSWORD="$KPW" \
	-e KAFKA_SSL_KEY_PASSWORD="$KPW" apache/kafka:3.9.1 >/dev/null
wait_log sec-mosquitto "running" 60 && ok "Mosquitto started (8883 with TLS and a login: P1-P3 below)" || bad "Mosquitto did not start: $(docker logs sec-mosquitto 2>&1 | tail -2 | tr '\n' ' ')"
# Artemis makes its instance at the first start: its AMQPS acceptor added then, a restart
if wait_log sec-artemis "AMQ221007" 120; then
	docker exec sec-artemis sed -i "s#</acceptors>#<acceptor name=\"amqps\">tcp://0.0.0.0:5671?protocols=AMQP;sslEnabled=true;keyStorePath=/certs/broker.p12;keyStorePassword=$KPW;keyStoreType=PKCS12</acceptor></acceptors>#" \
		/var/lib/artemis-instance/etc/broker.xml
	t0="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
	docker restart sec-artemis >/dev/null
	for _ in $(seq 120); do [ "$(docker logs --since "$t0" sec-artemis 2>&1 | grep -c 'AMQ221007')" -gt 0 ] && break; sleep 1; done
	[ "$(docker logs --since "$t0" sec-artemis 2>&1 | grep -c 'amqps.*5671\|5671.*AMQP')" -gt 0 ] && ok "Artemis: its AMQPS acceptor on 5671 started (TLS and a login: P4, P5, P8 below)" \
		|| bad "Artemis: no AMQPS acceptor: $(docker logs --since "$t0" sec-artemis 2>&1 | grep -i 'acceptor\|ERROR' | tail -2 | tr '\n' ' ')"
else
	bad "Artemis did not start: $(docker logs sec-artemis 2>&1 | tail -2 | tr '\n' ' ')"
fi
if wait_log sec-kafka "Kafka Server started" 120; then
	docker exec sec-kafka /opt/kafka/bin/kafka-topics.sh --bootstrap-server localhost:9092 --create --topic jobs \
		--partitions 1 --replication-factor 1 >/dev/null 2>&1 && ok "Kafka started, topic jobs made (9093 TLS only: P6, P9 below)" || bad "Kafka: topic jobs"
else
	bad "Kafka did not start: $(docker logs sec-kafka 2>&1 | grep -i 'error\|exception' | tail -2 | tr '\n' ' ')"
fi
CA="?ca=/certs/ca.pem"

# ── what must work ────────────────────────────────────────────────────────────
say "M1: the bridge to MQTT over TLS, logged in (a password with @ % ?); a Python subscriber over TLS reads it"
docker run -d --name sec-py-mqtt --network $NET -v $VOL:/certs:ro sec-python python3 mqtt_tls.py --host sec-mosquitto \
	--ca /certs/ca.pem --user app --password "$PW" --topic 'jobs/#' --count 3 --timeout 60 >/dev/null
wait_log sec-py-mqtt '"subscribed"' 30 || bad "M1 the Python subscriber: $(docker logs sec-py-mqtt 2>&1 | tail -1)"
bridge sec-etcds-mqtt --log-level debug --bridge-queue jobs --bridge-to "mqtts://app:$PWU@sec-mosquitto:8883$CA" --bridge-address jobs
for i in 1 2 3; do put sec-etcds-mqtt "/q/jobs/p/k$i" "mqtt over tls $i"; done
rc="$(docker wait sec-py-mqtt)"
check "M1 the Python subscriber read all three over TLS" 0 "$rc"
check "M1 their topics and payloads" "jobs/k1 mqtt over tls 1|jobs/k2 mqtt over tls 2|jobs/k3 mqtt over tls 3" \
	"$(docker logs sec-py-mqtt 2>&1 | python3 -c 'import sys, json; print("|".join(sorted(m["topic"] + " " + m["payload"] for m in map(json.loads, sys.stdin) if "topic" in m)))')"
drained sec-etcds-mqtt 3 && ok "M1 every message acknowledged in etcd" || bad "M1 a message left unacknowledged"

say "A1: the bridge to AMQP 1.0 over TLS, SASL PLAIN (a password with @ % ?); a Python consumer over TLS reads it"
bridge sec-etcds-amqp --log-level debug --bridge-queue jobs --bridge-to "amqps://app:$PWU@sec-artemis:5671$CA" --bridge-address jobs
for i in 1 2 3; do put sec-etcds-amqp "/q/jobs/p/k$i" "amqp over tls $i"; done
out="$(py amqp_tls.py --url amqps://sec-artemis:5671 --ca /certs/ca.pem --user app --password "$PW" --address jobs --count 3 --timeout 30)"
check "A1 the Python consumer read all three over TLS" "amqp over tls 1|amqp over tls 2|amqp over tls 3" \
	"$(printf '%s\n' "$out" | python3 -c 'import sys, json; print("|".join(m["body"] for m in map(json.loads, sys.stdin) if "body" in m))')"
drained sec-etcds-amqp 3 && ok "A1 every message acknowledged in etcd" || bad "A1 a message left unacknowledged"

say "K1: the bridge to Kafka over TLS; a Python consumer over TLS reads it"
bridge sec-etcds-kafka --log-level debug --bridge-queue jobs --bridge-to "kafkas://sec-kafka:9093$CA" --bridge-address jobs
for i in 1 2 3; do put sec-etcds-kafka "/q/jobs/p/k$i" "kafka over tls $i"; done
out="$(py kafka_tls.py --bootstrap sec-kafka:9093 --ca /certs/ca.pem --topic jobs --count 3 --timeout 60)"
check "K1 the Python consumer read all three over TLS" "k1 kafka over tls 1|k2 kafka over tls 2|k3 kafka over tls 3" \
	"$(printf '%s\n' "$out" | python3 -c 'import sys, json; print("|".join(sorted(m["key"] + " " + m["value"] for m in map(json.loads, sys.stdin) if "key" in m)))')"
drained sec-etcds-kafka 3 && ok "K1 every message acknowledged in etcd" || bad "K1 a message left unacknowledged"

say "M2: MQTT with a CA file of two - another CA first, the brokers' second (a rotation)"
bridge sec-etcds-m2 --log-level debug --bridge-queue m2 --bridge-to "mqtts://app:$PWU@sec-mosquitto:8883?ca=/certs/both.pem" --bridge-address m2
put sec-etcds-m2 /q/m2/p/k1 "trusted by the second CA"
drained sec-etcds-m2 1 && ok "M2 sent and acknowledged: every CA in the file trusted" \
	|| bad "M2 not sent: $(docker logs sec-etcds-m2 2>&1 | grep -i 'warn\|erro' | tail -1 | cut -c1-200)"

# sys_bridge <name> <flags...>: `bridge`, on a node whose system store trusts this run's CA
sys_bridge() {
	local n="$1"; shift
	docker run -d --name "$n" --network $NET -v $VOL:/certs:ro --entrypoint sh sec-etcds -c \
		'cp /certs/ca.pem /usr/local/share/ca-certificates/sec-run.crt && update-ca-certificates >/dev/null 2>&1 && exec etcds "$@"' \
		etcds --cluster $CLUSTER --node "$(uuid)" --listen-client-urls 0.0.0.0:2379 "$@" >/dev/null
	for _ in $(seq 30); do docker exec "$n" etcdctl --endpoints=127.0.0.1:2379 endpoint health >/dev/null 2>&1 && return 0; sleep 1; done
	bad "$n: etcds did not start: $(docker logs "$n" 2>&1 | tail -2 | tr '\n' ' ')"
}
say "M3: MQTT with no ca=, on a node whose system store trusts the run's CA (the control for N6)"
sys_bridge sec-etcds-m3 --log-level debug --bridge-queue m3 --bridge-to "mqtts://app:$PWU@sec-mosquitto:8883" --bridge-address m3
put sec-etcds-m3 /q/m3/p/k1 "trusted by the system store"
drained sec-etcds-m3 1 && ok "M3 sent and acknowledged: the system store is what trusts it here" \
	|| bad "M3 not sent: $(docker logs sec-etcds-m3 2>&1 | grep -i 'warn\|erro' | tail -1 | cut -c1-200)"

say "L1: the bridge's own listener, SASL PLAIN (plain AMQP - no TLS on the listener)"
bridge sec-etcds-listen --bridge-listen 0.0.0.0:5672 --bridge-listen-user app --bridge-listen-password "$PW"
put sec-etcds-listen /q/lq/p/k1 "through the listener"
out="$(py amqp_tls.py --plain --url amqp://sec-etcds-listen:5672 --user app --password "$PW" --address lq --count 1 --timeout 20)"
check "L1 a Python consumer logged in reads it" "through the listener" \
	"$(printf '%s\n' "$out" | python3 -c 'import sys, json; print("|".join(m["body"] for m in map(json.loads, sys.stdin) if "body" in m))')"

# ── what must be refused ──────────────────────────────────────────────────────
held() { # held <label> <node> <reason, grep -E>: the bridge says why, sends nothing, holds the message
	local i seen=0
	for i in $(seq 30); do [ "$(docker logs "$2" 2>&1 | grep -ciE -- "$3")" -gt 0 ] && { seen=1; break; }; sleep 1; done
	[ "$seen" = 1 ] && ok "$1 refused: $(docker logs "$2" 2>&1 | grep -iE -- "$3" | tail -1 | grep -oiE -- ".{0,60}($3).{0,60}" | head -1)" \
		|| bad "$1 not refused as expected: $(docker logs "$2" 2>&1 | grep -i 'warn\|erro' | tail -1 | cut -c1-220)"
	# an absence proves something only from a bridge that ran into the refusal
	if [ "$seen" != 1 ]; then bad "$1 nothing sent: not shown, the bridge never got to its refusal"
	elif [ "$(docker logs "$2" 2>&1 | grep -c 'acknowledged /q/\|sending to')" = 0 ]; then ok "$1 nothing sent, nothing acknowledged"
	else bad "$1 something went out: $(docker logs "$2" 2>&1 | grep 'acknowledged /q/\|sending to' | head -1)"; fi
	# taken from etcd's line by the bridge's stream and never deleted: etcd hands it out
	# again when that stream closes (queue.md; tests/bridge_delivery.rs, kept back on stop)
	[ "$(docker logs "$2" 2>&1 | grep -c 'received /q/')" -gt 0 ] && ok "$1 the message held by the bridge, not acknowledged" \
		|| bad "$1 the message never reached the bridge"
}
say "N1: a bridge trusting another CA (MQTT, native-tls)"
bridge sec-etcds-n1 --log-level debug --bridge-queue n1 --bridge-to "mqtts://app:$PWU@sec-mosquitto:8883?ca=/certs/rogue.pem" --bridge-address n1
put sec-etcds-n1 /q/n1/p/k1 "must not leave"
held N1 sec-etcds-n1 "certificate verify failed"
say "N2: a bridge trusting the public roots only (AMQP, rustls)"
bridge sec-etcds-n2 --log-level debug --bridge-queue n2 --bridge-to "amqps://app:$PWU@sec-artemis:5671" --bridge-address n2
put sec-etcds-n2 /q/n2/p/k1 "must not leave"
held N2 sec-etcds-n2 "UnknownIssuer"
say "N3: a bridge trusting the public roots only (Kafka, rustls)"
bridge sec-etcds-n3 --log-level debug --bridge-queue n3 --bridge-to "kafkas://sec-kafka:9093" --bridge-address jobs
put sec-etcds-n3 /q/n3/p/k1 "must not leave"
held N3 sec-etcds-n3 "UnknownIssuer"
say "N4, N5: a bridge with the right CA and a wrong password (AMQP SASL, MQTT)"
bridge sec-etcds-n4 --log-level debug --bridge-queue n4 --bridge-to "amqps://app:wrong@sec-artemis:5671$CA" --bridge-address n4
put sec-etcds-n4 /q/n4/p/k1 "must not leave"
held N4 sec-etcds-n4 "SASL error code Auth"
bridge sec-etcds-n5 --log-level debug --bridge-queue n5 --bridge-to "mqtts://app:wrong@sec-mosquitto:8883$CA" --bridge-address n5
put sec-etcds-n5 /q/n5/p/k1 "must not leave"
held N5 sec-etcds-n5 "NotAuthorized"
say "N6: MQTT trusting another CA, on a node whose system store trusts the brokers' (ca= is the only trust)"
sys_bridge sec-etcds-n6 --log-level debug --bridge-queue n6 --bridge-to "mqtts://app:$PWU@sec-mosquitto:8883?ca=/certs/rogue.pem" --bridge-address n6
put sec-etcds-n6 /q/n6/p/k1 "must not leave"
held N6 sec-etcds-n6 "certificate verify failed"

say "P1-P10: Python clients that must be refused"
refused() { # refused <label> <reason, grep -E> <python args...>: exit 2, the reason it was refused, no message
	local out rc err
	out="$(py "${@:3}" 2>&1)"; rc=$?
	# the client's "error", read as JSON: a broker's reason may hold quotes of its own
	err="$(printf '%s\n' "$out" | python3 -c 'import sys, json
for l in sys.stdin:
    try:
        m = json.loads(l)
    except ValueError:
        continue
    if isinstance(m, dict) and "error" in m:
        print(m["error"]); break')"
	if [ "$rc" = 2 ] && [ "$(printf '%s\n' "$out" | grep -c '"body"\|"payload"\|"value"')" = 0 ] \
		&& [ "$(printf '%s\n' "$err" | grep -ciE -- "$2")" -gt 0 ]; then
		ok "$1: $(printf '%s\n' "$err" | grep -oiE -- ".{0,50}($2).{0,30}" | head -1)"
	else
		bad "$1: exit $rc, not [$2]: $(printf '%s\n' "$out" | tail -1 | cut -c1-220)"
	fi
}
refused "P1 MQTT with no TLS to the TLS port" "disconnected" mqtt_tls.py --host sec-mosquitto --plain --user app --password "$PW" --topic 'jobs/#' --timeout 15
refused "P2 MQTT trusting another CA" "CERTIFICATE_VERIFY_FAILED" mqtt_tls.py --host sec-mosquitto --ca /certs/rogue.pem --user app --password "$PW" --topic 'jobs/#' --timeout 15
refused "P3 MQTT with a wrong password" "not authori[sz]ed" mqtt_tls.py --host sec-mosquitto --ca /certs/ca.pem --user app --password wrong --topic 'jobs/#' --timeout 15
refused "P4 AMQP trusting another CA" "certificate verify failed" amqp_tls.py --url amqps://sec-artemis:5671 --ca /certs/rogue.pem --user app --password "$PW" --address jobs --timeout 15
refused "P5 AMQP with a wrong password" "unauthorized-access" amqp_tls.py --url amqps://sec-artemis:5671 --ca /certs/ca.pem --user app --password wrong --address jobs --timeout 15
refused "P6 Kafka trusting another CA" "certificate verify failed" kafka_tls.py --bootstrap sec-kafka:9093 --ca /certs/rogue.pem --topic jobs --timeout 20
refused "P7 the listener with a wrong password" "unauthorized-access" amqp_tls.py --plain --url amqp://sec-etcds-listen:5672 --user app --password wrong --address lq --timeout 15
refused "P8 AMQP with no TLS to the TLS port" "framing-error|aborted|SSL" amqp_tls.py --plain --url amqp://sec-artemis:5671 --user app --password "$PW" --address jobs --timeout 15
refused "P9 Kafka with no TLS to the TLS port" "security.protocol|SSL listener" kafka_tls.py --plain --bootstrap sec-kafka:9093 --topic jobs --timeout 20
refused "P10 the listener with no login (ANONYMOUS)" "unauthorized-access|Authentication failed|mech" amqp_tls.py --plain --url amqp://sec-etcds-listen:5672 --address lq --timeout 15

say "S1: a bridge prefix over the node's local-only keys"
docker run --rm --network $NET -v $VOL:/certs:ro sec-etcds --cluster $CLUSTER --node "$(uuid)" --listen-client-urls 0.0.0.0:2379 \
	--local-only-prefix /secret/ --bridge-prefix /secret/ --bridge-to "mqtts://sec-mosquitto:8883$CA" > "$W/s1.log" 2>&1
check "S1 etcds refuses to start (exit code 14)" 14 "$?"
grep -q 'serves to no remote caller' "$W/s1.log" && ok "S1 saying why: $(grep -o 'prefix "/secret/".*caller' "$W/s1.log" | head -1)" \
	|| bad "S1 its reason: $(tail -1 "$W/s1.log")"

say "S2: a bridge queue the node gives no remote consumer"
docker run --rm --network $NET -v $VOL:/certs:ro sec-etcds --cluster $CLUSTER --node "$(uuid)" --listen-client-urls 0.0.0.0:2379 \
	--local-only-prefix /q/payroll/ --bridge-queue payroll --bridge-to "mqtts://sec-mosquitto:8883$CA" > "$W/s2.log" 2>&1
check "S2 etcds refuses to start (exit code 14)" 14 "$?"
grep -q 'no remote consumer' "$W/s2.log" && ok "S2 saying why: $(grep -o 'queue "payroll".*consumer' "$W/s2.log" | head -1)" \
	|| bad "S2 its reason: $(tail -1 "$W/s2.log")"

say "S3: a listener client of a queue the node gives no remote consumer"
bridge sec-etcds-s3 --log-level debug --local-only-prefix /q/payroll/ --bridge-listen 0.0.0.0:5672 \
	--bridge-listen-user app --bridge-listen-password "$PW"
put sec-etcds-s3 /q/payroll/p/k1 "salary"
out="$(py amqp_tls.py --plain --url amqp://sec-etcds-s3:5672 --user app --password "$PW" --address payroll --count 1 --timeout 10 2>&1)"
if [ "$(docker logs sec-etcds-s3 2>&1 | grep -c 'link refused.*no remote consumer')" -gt 0 ]; then
	ok "S3 the link refused: $(docker logs sec-etcds-s3 2>&1 | grep -o 'queue "payroll".*consumer' | head -1)"
	# the client logged in and attached (the refusal is of its link): what it got counts
	[ "$(printf '%s\n' "$out" | grep -c '"body"')" = 0 ] && ok "S3 the client got nothing" || bad "S3 the client got: $out"
else
	bad "S3 no refusal in the log: $(docker logs sec-etcds-s3 2>&1 | tail -1 | cut -c1-200); the client: $(printf '%s\n' "$out" | tail -1 | cut -c1-120)"
fi
[ "$(docker exec sec-etcds-s3 timeout 5 etcdctl --endpoints=127.0.0.1:2379 watch "/q/payroll/c/$(uuid)" 2>/dev/null | grep -c salary)" -gt 0 ] \
	&& ok "S3 the message still in etcd: a local consumer takes it" || bad "S3 a local consumer got nothing"

echo "---- $pass passed, $fail failed ----"
[ "$fail" = 0 ]
