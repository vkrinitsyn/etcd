# etcd

[![ci](https://github.com/vkrinitsyn/etcd/actions/workflows/ci.yml/badge.svg)](https://github.com/vkrinitsyn/etcd/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/vkrinitsyn/etcd/graph/badge.svg)](https://codecov.io/gh/vkrinitsyn/etcd)
[![audit](https://github.com/vkrinitsyn/etcd/actions/workflows/audit.yml/badge.svg)](https://github.com/vkrinitsyn/etcd/actions/workflows/audit.yml)
[![crates.io](https://img.shields.io/crates/v/etcds.svg)](https://crates.io/crates/etcds)
[![docs.rs](https://docs.rs/etcds/badge.svg)](https://docs.rs/etcds)

[![License: Apache](https://img.shields.io/badge/License-Apache%202.0-red.svg)](LICENSE-APACHE)
OR
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
 
Server implementation of a [etcd](https://github.com/etcd-io/etcd) v3 API client on Rust.
It provides asynchronous client backed by [tokio](https://github.com/tokio-rs/tokio)
and [tonic](https://github.com/hyperium/tonic).


> [!NOTE]
> Provided by [DBinvent](https://www.dbinvent.com), and an essential part of
> [YT Cluster](https://www.dbinvent.com/cluster/)

## Features
- etcd API v3 compatible client using protobuf to leverage existing ecosystem
- priority is a [queue](https://github.com/vkrinitsyn/etcd/blob/main/queue.md#etcd-based-queue) implementation with order and delivery guarantee
- point-to-point [routing](https://github.com/vkrinitsyn/etcd/blob/main/queue-p2p-route.md#point-to-point-routing-for-linked-queues)
  for a linked request/reply queue pair, and using that route as transport for a
  requested process
- a queue named `ch:<table>` writes its messages straight into a ClickHouse table
  ([clickhouse.md](clickhouse.md), feature `clickhouse`)
- a bridge from a queue or key prefix to a JMS queue (AMQP 1.0), an MQTT 5 topic
  or a Kafka topic, or JMS clients served as an AMQP peer
  ([bridge.md](bridge.md), feature `bridge`)
- no message storage, cluster election, as use another cluster implementation
- messages, errors and `--help` in English, Russian, French, German, Chinese and
  Japanese: `LANG`, `etcds::set_lang`, or the embedder's logger `o!("lang" => ..)`;
  every error starts with its `ETCD-Ennn` code
- ability to build into another rust application as a component, see [rppd](https://github.com/vkrinitsyn/rppd?tab=readme-ov-file#rppd---rust-python-postgres-discovery)

## Documentation

- [queue.md](queue.md) - using a queue: produce, consume, acknowledge, and what delivery guarantees
- [clickhouse.md](clickhouse.md) - queues that insert into ClickHouse
- [queue-p2p-route.md](queue-p2p-route.md) - how a message is routed between nodes
- [agent.md](agent.md) - for coding agents and embedders: the API, the configuration, YT Cluster's use of it, and the known gaps

## Supported APIs

- [x] KV (wo: filter, range, version, pagination, linearizable read)
- [x] Watch (wo: filter, fragment, ranges, revision)
 
### Low priority:
- [ ] Lock
- [ ] Namespace
- [ ] Lease

### Lowest priority:
- [ ] Auth
- [ ] Maintenance
- [ ] Cluster
- [ ] Election


## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
etcd-client = "0.21"
tokio = { version = "1.0", features = ["full"] }
```

To get started using `etcd`:

```rust
use etcd_client::{Client, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client = Client::connect(["localhost:2379"], None).await?;
    // put kv
    client.put("foo", "bar", None).await?;
    // get kv
    let resp = client.get("foo", None).await?;
    if let Some(kv) = resp.kvs().first() {
        println!("Get kv: {{{}: {}}}", kv.key_str()?, kv.value_str()?);
    }

    Ok(())
}
```

## Examples

Examples can be found in [`examples`](./examples).


## Checks

```shell
cargo audit      # known vulnerabilities (RustSec) in the dependencies Cargo.lock pins
cargo geiger     # unsafe code in etcds and in each dependency it uses
```

`cargo install cargo-audit cargo-geiger` installs them; with cargo-make,
`cargo make audit` and `cargo make geiger` run the same. A library keeps no `Cargo.lock` in git, so the audit reads the one the
last build resolved; `cargo generate-lockfile` makes one. geiger reads
every dependency file in the target directory, so give it one of its own
(`CARGO_TARGET_DIR=target/geiger`), or an old build's can name a source file
that is gone.

## License

Dual-licensed to be compatible with the Rust project.

Licensed under the Apache License, Version 2.0 http://www.apache.org/licenses/LICENSE-2.0 or the MIT
license http://opensource.org/licenses/MIT, at your option. This file may not be copied, modified, or distributed except
according to those terms.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `etcd` by you, shall be licensed as Apache-2.0 and MIT, without any additional
terms or conditions.

## End-to-end tests

The etcd API v3 surface a YT Cluster node exposes, built on this crate, is tested
from the yt-cluster-tests repository (DBinvent/yt-cluster-tests) next to this one:
`yt-cluster-tests/topology/93_etcd.sh` drives a seven-node docker cluster with the
`yxetcd` example of this repository (`cargo build --release --example yxetcd`).
`yt-cluster-tests/nightly/nightly.sh` builds that example and runs the suite every
night this repository (or another watched one) changed.
