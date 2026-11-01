#!/usr/bin/env bash
# certs.sh <dir> <password>: a test CA and the brokers' certificate it signs (sec-artemis,
# sec-mosquitto, sec-kafka), as PEM and as a PKCS12 key store; and a second, "rogue" CA
# that signs nothing a broker shows - what a client trusting the wrong CA is refused with.
# Valid two days: made for one run.
set -euo pipefail
d="$1"; pw="$2"
mkdir -p "$d"; cd "$d"
ca() { # ca <name> <CN>
	openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=$2" -keyout "$1.key" -out "$1.pem" \
		-addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign 2>/dev/null
}
ca ca "etcds security test CA"
ca rogue "etcds rogue CA"
openssl req -newkey rsa:2048 -nodes -subj "/CN=sec-brokers" -keyout broker.key -out broker.csr 2>/dev/null
printf '%s\n' "subjectAltName=DNS:sec-artemis,DNS:sec-mosquitto,DNS:sec-kafka,DNS:localhost" \
	"extendedKeyUsage=serverAuth" "basicConstraints=CA:FALSE" > ext.cnf
openssl x509 -req -in broker.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 -out broker.pem -extfile ext.cnf 2>/dev/null
openssl pkcs12 -export -in broker.pem -inkey broker.key -certfile ca.pem -name broker -out broker.p12 -passout "pass:$pw"
chmod 644 ./*.pem ./*.key ./*.p12
