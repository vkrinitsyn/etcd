# etcd

[![Minimum rustc version](https://img.shields.io/badge/rustc-1.64+-lightgray.svg)](https://github.com/etcdv3/etcd-client#rust-version-requirements)
[![Crate](https://img.shields.io/crates/v/etcd-client.svg)](https://crates.io/crates/etcd-client)
[![API](https://docs.rs/etcd-client/badge.svg)](https://docs.rs/etcd-client)

[![License: Apache](https://img.shields.io/badge/License-Apache%202.0-red.svg)](LICENSE-APACHE)
OR
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)
 
Server implementation of a [etcd](https://github.com/etcd-io/etcd) v3 API client on Rust.
It provides asynchronous client backed by [tokio](https://github.com/tokio-rs/tokio)
and [tonic](https://github.com/hyperium/tonic).


> [!IMPORTANT]
This is an experimental Prof of Concept of the features bellow

## Features
- etcd API v3 compatible client using protobuf to leverage existing ecosystem
- priority is a [queue](https://github.com/vkrinitsyn/etcd/blob/main/queue.md#etcd-based-queue) implementation with order and delivery guarantee
- no message storage, cluster election, as use another cluster implementation
- ability to build into another rust application as a component, see [rppd](https://github.com/vkrinitsyn/rppd?tab=readme-ov-file#rppd---rust-python-postgres-discovery)

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
etcd-client = "0.14"
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


## License

Dual-licensed to be compatible with the Rust project.

Licensed under the Apache License, Version 2.0 http://www.apache.org/licenses/LICENSE-2.0 or the MIT
license http://opensource.org/licenses/MIT, at your option. This file may not be copied, modified, or distributed except
according to those terms.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in `etcd` by you, shall be licensed as Apache-2.0 and MIT, without any additional
terms or conditions.
