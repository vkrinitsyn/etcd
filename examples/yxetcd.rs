//! `yxetcd` — a tiny etcd v3 client for testing a LIVE cluster node.
//!
//! The crate's own tests (`tests/client.rs`, `tests/queue.rs`, …) start an
//! `EtcdNode` in-process and talk to it. That covers the implementation; it does
//! not cover the thing the product page actually claims:
//!
//!   "A node can expose a standard etcd API v3 key-value surface, so anything
//!    that already speaks etcd — service discovery, configuration, leader
//!    election in your own stack — needs no bespoke client."
//!
//! The only honest way to test *that* is from outside, over the network, with
//! the ordinary client — so this uses `etcd-client`, the third-party crate, and
//! nothing from `etcds` itself. If this binary works against a ytserv node, a
//! stranger's etcd tooling works against it too; if it needed anything from the
//! crate under test, the claim would still be unproven.
//!
//! The surface lives on the node's ordinary port (7421), multiplexed onto the
//! same tonic server as the peer gRPC — there is no separate etcd listener and
//! no HTTP gateway, so `curl` cannot stand in for this.
//!
//! ```text
//! yxetcd <endpoint> put   <key> <value>
//! yxetcd <endpoint> get   <key>              # prints the value, or exits 3
//! yxetcd <endpoint> range <prefix>           # one "key\tvalue" line each
//! yxetcd <endpoint> del   <key>
//! yxetcd <endpoint> watch <prefix> <secs>    # prints events until the deadline
//! yxetcd <endpoint> status                   # reachability probe
//! ```
//!
//! Exit codes are the test contract: 0 did it, 3 "no such key" (distinct from a
//! failure to reach the node), 1 anything else.

use std::time::Duration;

use etcd_client::{Client, EventType, GetOptions, WatchOptions};

const USAGE: &str = "yxetcd <endpoint> put|get|range|del|watch|status [args]";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 2 {
        eprintln!("{}", USAGE);
        return std::process::ExitCode::from(2);
    }
    let endpoint = if a[0].starts_with("http") { a[0].clone() } else { format!("http://{}", a[0]) };

    // A short connect timeout: a node that is not listening should fail fast and
    // visibly, not hang the suite for the default.
    let mut client = match tokio::time::timeout(
        Duration::from_secs(10),
        Client::connect([endpoint.as_str()], None),
    ).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => { eprintln!("connect {}: {}", endpoint, e); return std::process::ExitCode::from(1); }
        Err(_) => { eprintln!("connect {}: timed out", endpoint); return std::process::ExitCode::from(1); }
    };

    let rc = match a[1].as_str() {
        "status" => match client.status().await {
            Ok(s) => { println!("version={} dbsize={}", s.version(), s.db_size()); 0 }
            Err(e) => { eprintln!("status: {}", e); 1 }
        },
        "put" if a.len() >= 4 => match client.put(a[2].as_str(), a[3].as_str(), None).await {
            Ok(_) => 0,
            Err(e) => { eprintln!("put: {}", e); 1 }
        },
        "get" if a.len() >= 3 => match client.get(a[2].as_str(), None).await {
            // "absent" is its own exit code: a test that could not tell it from
            // "the node refused the call" would report a propagation failure for
            // an unreachable peer, and vice versa.
            Ok(r) => match r.kvs().first() {
                Some(kv) => { println!("{}", kv.value_str().unwrap_or("<binary>")); 0 }
                None => 3,
            },
            Err(e) => { eprintln!("get: {}", e); 1 }
        },
        "range" if a.len() >= 3 => {
            let opt = GetOptions::new().with_prefix();
            match client.get(a[2].as_str(), Some(opt)).await {
                Ok(r) => {
                    for kv in r.kvs() {
                        println!("{}\t{}", kv.key_str().unwrap_or("?"),
                                 kv.value_str().unwrap_or("<binary>"));
                    }
                    0
                }
                Err(e) => { eprintln!("range: {}", e); 1 }
            }
        }
        "del" if a.len() >= 3 => match client.delete(a[2].as_str(), None).await {
            Ok(r) => { println!("{}", r.deleted()); 0 }
            Err(e) => { eprintln!("del: {}", e); 1 }
        },
        "watch" if a.len() >= 4 => {
            let secs: u64 = a[3].parse().unwrap_or(10);
            let opt = WatchOptions::new().with_prefix();
            // This crate's `watch` returns the stream alone; upstream etcd-client
            // hands back a (Watcher, WatchStream) pair. Worth knowing, because it
            // means a client written against the published API does NOT compile
            // unchanged - see the note in 93_etcd.sh.
            let mut stream = match client.watch(a[2].as_str(), Some(opt)).await {
                Ok(v) => v,
                Err(e) => { eprintln!("watch: {}", e); return std::process::ExitCode::from(1); }
            };
            // The deadline is the point: a watch that never fires must END, and
            // end distinguishably, or the test hangs instead of failing.
            let deadline = tokio::time::sleep(Duration::from_secs(secs));
            tokio::pin!(deadline);
            let mut seen = 0u32;
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    msg = stream.message() => match msg {
                        Ok(Some(resp)) => {
                            for ev in resp.events() {
                                let k = ev.kv().and_then(|kv| kv.key_str().ok()).unwrap_or("?");
                                let v = ev.kv().and_then(|kv| kv.value_str().ok()).unwrap_or("");
                                let t = match ev.event_type() { EventType::Put => "PUT", EventType::Delete => "DEL" };
                                println!("{}\t{}\t{}", t, k, v);
                                seen += 1;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => { eprintln!("watch stream: {}", e); break; }
                    }
                }
            }
            eprintln!("watch: {} event(s)", seen);
            if seen > 0 { 0 } else { 3 }
        }
        _ => { eprintln!("{}", USAGE); 2 }
    };
    std::process::ExitCode::from(rc)
}
