# Contributing to etcd (etcds)

Thank you for helping. etcds is licensed MIT OR Apache-2.0; a contribution is accepted
under the same terms (see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)).

## Issues

Open an issue on GitHub for a bug or a feature request. For a bug, give the etcds
version, the features it is built with (`tracer`, `clickhouse`, `bridge`), whether it
runs as the `etcds` binary or embedded, the client used (etcdctl, etcd-client, ...), and
the smallest sequence of calls that shows it: what you did, what you expected, and what
you got.

A security problem is not reported in a public issue: write to
**support@dbinvent.com** instead.

## Pull requests

1. Fork the repository and branch from `main`.
2. Keep a change to one subject; describe what it changes and why.
3. Build and test it (below); add tests for what you change.
4. Messages a user reads go through the locale files (`locales/*.yml`): add a new key in
   every language there, an error with its `ETCD-E` code.
5. The generated protos in `src/etcdpb/` are checked in and carry hand changes (the
   `x-peer` argument of the `KvClient` calls): after a regeneration, put them back.
6. Open the pull request against `main`.

## Build and test

`protoc` and the protobuf includes are needed (`build.rs`, and etcd-client's for the
tests and the bridge): `apt install protobuf-compiler libprotobuf-dev`.

```
cargo test                               # default features
cargo test --features tracer             # OpenTelemetry
cargo test --features clickhouse         # the ClickHouse queue consumer
cargo test --features bridge             # the AMQP 1.0, MQTT 5 and Kafka bridge
cargo llvm-cov --all-features --summary-only   # CI fails under 77% of lines
cargo test --features bridge --test bridge_live -- --ignored   # against real brokers
```

Before a pull request, run the checks as well (`cargo make audit`, `cargo make geiger`):

```
cargo audit      # known vulnerabilities in the dependencies
cargo geiger     # unsafe code in etcds and its dependencies
```

etcds forbids unsafe code of its own (`#![forbid(unsafe_code)]`).

## Code of conduct

Taking part in this project means following its
[Code of Conduct](CODE_OF_CONDUCT.md).
