//! [otel] etcds' instruments (yt-pgxt resilience.md phase 3), on the meter its host
//! hands it (`EtcdMgmtEvent::Meter`); none, and nothing is recorded.
//!
//! | name | kind | attributes |
//! |---|---|---|
//! | `etcd.kv.ops` | counter | op: get, put, delete, txn; outcome: ok, error |
//! | `etcd.kv.duration` | histogram, ms | op |

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

pub(crate) struct Instruments {
    ops: Counter<u64>,
    duration: Histogram<f64>,
}

impl Instruments {
    pub(crate) fn new(m: Meter) -> Instruments {
        Instruments {
            ops: m.u64_counter("etcd.kv.ops").build(),
            duration: m.f64_histogram("etcd.kv.duration").with_unit("ms").build(),
        }
    }

    /// One KV call answered: `ok` or not, how long it took
    pub(crate) fn kv(&self, op: &'static str, ok: bool, took: std::time::Duration) {
        self.ops.add(1, &[KeyValue::new("op", op), KeyValue::new("outcome", if ok { "ok" } else { "error" })]);
        self.duration.record(took.as_secs_f64() * 1000.0, &[KeyValue::new("op", op)]);
    }
}
