#!/usr/bin/env python3
"""Kafka over TLS: a consumer of the topic the etcds bridge produces to.

    python3 kafka_tls.py --bootstrap sec-kafka:9093 --ca /certs/ca.pem --topic jobs --count 3

The broker's certificate is checked against --ca and its name against the bootstrap
host (librdkafka's ssl.endpoint.identification.algorithm=https). Prints one JSON line
per record ({"key", "value"}). Exit 0 once --count arrived, 1 on a timeout, 2 when the
TLS handshake is refused. --plain: PLAINTEXT, no TLS - refused by a TLS listener."""
import argparse
import json
import sys
import time

from confluent_kafka import Consumer


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--bootstrap", required=True)
    p.add_argument("--ca", help="the CA to trust (PEM)")
    p.add_argument("--plain", action="store_true", help="no TLS")
    p.add_argument("--topic", required=True)
    p.add_argument("--count", type=int, default=1)
    p.add_argument("--timeout", type=float, default=60)
    a = p.parse_args()

    errors = []
    conf = {
        "bootstrap.servers": a.bootstrap,
        "group.id": "py-security-test",
        "auto.offset.reset": "earliest",
        "enable.auto.commit": False,
        "error_cb": lambda e: errors.append(str(e)),
    }
    if not a.plain:
        conf["security.protocol"] = "SSL"
        conf["ssl.endpoint.identification.algorithm"] = "https"
    if a.ca:
        conf["ssl.ca.location"] = a.ca
    c = Consumer(conf)
    c.subscribe([a.topic])
    got, deadline = 0, time.monotonic() + a.timeout
    while got < a.count and time.monotonic() < deadline:
        m = c.poll(1.0)
        if m is None:
            if any("SSL" in e or "certificate" in e for e in errors):
                break
            continue
        if m.error():
            errors.append(str(m.error()))
            continue
        print(json.dumps({"key": (m.key() or b"").decode("utf-8", "replace"),
                          "value": (m.value() or b"").decode("utf-8", "replace")}), flush=True)
        got += 1
    c.close()
    if got < a.count and any("SSL" in e or "certificate" in e for e in errors):
        print(json.dumps({"error": errors[0]}), flush=True)
        return 2
    return 0 if got >= a.count else 1


if __name__ == "__main__":
    sys.exit(main())
