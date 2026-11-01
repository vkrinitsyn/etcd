#!/usr/bin/env python3
"""AMQP 1.0 over TLS with a SASL PLAIN login: a JMS-style consumer of a queue the etcds
bridge sends to - a broker's (Artemis), or the bridge's own listener (--plain, no TLS).

    python3 amqp_tls.py --url amqps://sec-artemis:5671 --ca /certs/ca.pem \
        --user app --password secret --address jobs --count 3

The broker's certificate is checked against --ca and its name against the URL's host.
Each message is accepted - the bridge's listener acknowledges it in etcd then. Prints one
JSON line per message ({"body"}). Exit 0 once --count arrived, 1 on a timeout, 2 when the
TLS handshake or the login is refused."""
import argparse
import json
import sys

from proton import Data, SSLDomain
from proton.reactor import ReceiverOption
from proton.utils import BlockingConnection


class AsQueue(ReceiverOption):
    """The source a JMS queue consumer asks for: the anycast queue, not a topic."""

    def apply(self, receiver):
        caps = receiver.source.capabilities
        caps.put_array(False, Data.SYMBOL)
        caps.enter()
        caps.put_symbol("queue")
        caps.exit()


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--url", required=True, help="amqps://host:port, or amqp://host:port with --plain")
    p.add_argument("--ca", help="the CA to trust (PEM)")
    p.add_argument("--plain", action="store_true", help="no TLS")
    p.add_argument("--user")
    p.add_argument("--password")
    p.add_argument("--address", required=True)
    p.add_argument("--count", type=int, default=1)
    p.add_argument("--timeout", type=float, default=30)
    a = p.parse_args()

    ssl_domain = None
    if not a.plain:
        ssl_domain = SSLDomain(SSLDomain.MODE_CLIENT)
        if a.ca:
            ssl_domain.set_trusted_ca_db(a.ca)
        ssl_domain.set_peer_authentication(SSLDomain.VERIFY_PEER_NAME)
    login = dict(user=a.user, password=a.password, allowed_mechs="PLAIN") if a.user else dict(allowed_mechs="ANONYMOUS")
    try:
        conn = BlockingConnection(a.url, timeout=a.timeout, ssl_domain=ssl_domain, sasl_enabled=True, **login)
        receiver = conn.create_receiver(a.address, options=AsQueue())
    except Exception as e:  # proton raises ConnectionException, SSLException, Timeout
        print(json.dumps({"error": f"{type(e).__name__}: {e}"}), flush=True)
        return 2
    got = 0
    try:
        while got < a.count:
            msg = receiver.receive(timeout=a.timeout)
            body = msg.body
            if isinstance(body, (bytes, memoryview)):
                body = bytes(body).hex()
            print(json.dumps({"body": body}), flush=True)
            receiver.accept()
            got += 1
    except Exception as e:
        print(json.dumps({"error": f"{type(e).__name__}: {e}"}), flush=True)
    finally:
        conn.close()
    return 0 if got >= a.count else 1


if __name__ == "__main__":
    sys.exit(main())
