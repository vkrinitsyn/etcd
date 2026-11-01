#!/usr/bin/env python3
"""MQTT 5 over TLS with a login: a consumer of what the etcds bridge publishes.

    python3 mqtt_tls.py --host sec-mosquitto --port 8883 --ca /certs/ca.pem \
        --user app --password secret --topic 'jobs/#' --count 3

The broker's certificate is checked against --ca (and its name against --host); with
--plain there is no TLS at all. Prints one JSON line per message ({"topic", "payload"}),
and {"subscribed": ...} once the subscription stands. Exit 0 once --count arrived, 1 on
a timeout, 2 when the TLS handshake or the login is refused."""
import argparse
import json
import ssl
import sys
import threading

import paho.mqtt.client as mqtt


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--host", required=True)
    p.add_argument("--port", type=int, default=8883)
    p.add_argument("--ca", help="the CA to trust (PEM); the system's when not given")
    p.add_argument("--plain", action="store_true", help="no TLS")
    p.add_argument("--user")
    p.add_argument("--password")
    p.add_argument("--topic", required=True)
    p.add_argument("--count", type=int, default=1)
    p.add_argument("--timeout", type=float, default=30)
    a = p.parse_args()

    got, refused, done = [], [], threading.Event()
    c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id="py-security-test", protocol=mqtt.MQTTv5)
    if not a.plain:
        c.tls_set_context(ssl.create_default_context(cafile=a.ca))
    if a.user:
        c.username_pw_set(a.user, a.password)

    def on_connect(client, userdata, flags, reason_code, properties):
        if reason_code.is_failure:
            refused.append(f"the broker refused the login: {reason_code}")
            done.set()
            return
        client.subscribe(a.topic, qos=1)

    def on_subscribe(client, userdata, mid, reason_codes, properties):
        print(json.dumps({"subscribed": a.topic}), flush=True)

    def on_disconnect(client, userdata, flags, reason_code, properties):
        if not got:
            refused.append(f"disconnected: {reason_code}")
            done.set()

    def on_message(client, userdata, msg):
        got.append(msg)
        print(json.dumps({"topic": msg.topic, "payload": msg.payload.decode("utf-8", "replace")}), flush=True)
        if len(got) >= a.count:
            done.set()

    c.on_connect, c.on_subscribe, c.on_disconnect, c.on_message = on_connect, on_subscribe, on_disconnect, on_message
    try:
        c.connect(a.host, a.port, keepalive=30)
    except (ssl.SSLError, OSError) as e:
        print(json.dumps({"error": f"{type(e).__name__}: {e}"}), flush=True)
        return 2
    c.loop_start()
    done.wait(a.timeout)
    c.loop_stop()
    if refused and len(got) < a.count:
        print(json.dumps({"error": refused[0]}), flush=True)
        return 2
    return 0 if len(got) >= a.count else 1


if __name__ == "__main__":
    sys.exit(main())
