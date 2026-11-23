#![forbid(unsafe_code)]

use tokio::sync::mpsc::Sender;
use tonic::Status;
use crate::cli::EtcdConfig;
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, PutRequest, TxnRequest, WatchResponse};

// [i18n] locales/*.yml, compiled in; a language without a message is English
rust_i18n::i18n!("locales", fallback = "en");

pub use i18n::{lang, message, set_lang};

// first: tr! is declared there, for the modules below
#[macro_use]
mod i18n;

#[cfg(feature = "clickhouse")]
pub(crate) mod clickhouse;
#[cfg(any(feature = "clickhouse", feature = "bridge"))]
mod b64;
/// [bridge] a queue or key prefix to a broker: JMS over AMQP 1.0, MQTT 5, Kafka
#[cfg(feature = "bridge")]
pub mod bridge;
#[cfg(feature = "tracer")]
mod metrics;
pub mod etcdpb;
pub mod cli;
pub mod cluster;
mod srv;
pub mod queue;
pub mod kv;
pub mod prefix;
/// [q-route] queue dispatcher + consumer registries, and the routing they buy
pub mod route;
mod peer;
// [coord] the leader, leases and locks
mod coord;
// [sys] the system records a leader switch carries, and the sequences
pub mod sys;
/// [embed] leases and locks for the embedder, in process
pub mod embed;
// [auth] what every call is checked for
mod guard;
// [auth] the Auth API on signon's accounts
#[cfg(feature = "auth")]
mod auth;
// [approve] approvals of a queue's messages: the rules, and the node's side
#[cfg(feature = "approvals")]
pub mod approve;


/// Logging prefix, to start every logging message with a string to use in a server as a module 
const LP: &str = "[etcd] ";


pub type WatchSender = Sender<Result<WatchResponse, Status>>;

/// Etcd reconfiguration and integration events
// the variants stay unboxed: an embedder builds and matches them, and a Box would change that public API
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum EtcdEvents {
    /// Cache modification event
    Data(KvEvent),
    /// Node management event
    Mgmt(EtcdMgmtEvent),
}

// the variants stay unboxed: an embedder builds and matches them, and a Box would change that public API
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum EtcdMgmtEvent {
    /// runtime reconfigure event
    Config(EtcdConfig),
    /// change tracer runtime
    #[cfg(feature = "tracer")]
    Tracer(Option<opentelemetry_sdk::trace::SdkTracer>),
    /// [otel] change the meter: `etcd.kv.ops`, `etcd.kv.duration` (metrics.rs)
    #[cfg(feature = "tracer")]
    Meter(Option<opentelemetry::metrics::Meter>),
    /// Node management event to stop
    Stop,
    /// Node management event to soft restart 
    Restart,
    /// Node management event to pause(sec)  
    Pause(u16)
}

#[derive(Clone)]
pub enum KvEvent {
    Put(PutRequest),
    Delete(DeleteRangeRequest),
    Txn(TxnRequest),
}

