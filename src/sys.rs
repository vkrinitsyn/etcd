//! [sys] The system records (owner, 2026-11-21): what the leader decides for the cluster,
//! kept so a leader switch carries it - the leases, the lock holders and the sequences.
//!
//! - **Written before the answer.** The leader writes a record before it answers the call
//!   that made it: a lease granted or revoked, a lock granted or freed, a block of a
//!   sequence handed out. A new leader loads them all and goes on from there.
//! - **Where.** Every node holds them in a map of its own, each with a version. The leader
//!   puts each to every peer as an etcd KV call on [`SYS_PREFIX`] (marked
//!   [`crate::coord::XREP`]); a node taking over or joining reads them with a peer's range
//!   over it - etcd's own API, no service of etcds' own (owner, 2026-11-21: the v3 proto
//!   files never change). Memory only, as the rest of etcds. An embedder with a store of
//!   its own - YT Cluster's, committed through the cluster - sets a [`CoordStore`]
//!   (`EtcdNode::set_coord_store`): the leader writes there first, and a new leader loads
//!   from there, so the records survive the whole cluster stopping.
//! - **Not keys.** The records are no client's: a client's call on the prefix is refused,
//!   and they are in no range, watch or snapshot.
//! - **Sequences** go to the leader by a queue pair (owner, 2026-11-21): a request queue
//!   the leader consumes, [`SEQ_Q`] (and [`SEQDEF_Q`] for the definitions, an embedder's
//!   and a peer's only), and a reply queue per asker. Any etcd client takes numbers the
//!   same way: a put on the request queue naming its reply queue, a watch on that one.
//!
//! The records, each a JSON value:
//!
//! | key | value |
//! |---|---|
//! | `lease/<id>` | `{"ttl": <granted TTL>}` |
//! | `lock/<name, hex>` | `{"key": "<holder key, hex>", "lease": <id>}` |
//! | `seq/<name>` | `{"cache": <n>, "next": <the next number to hand out>, "dropped": <bool>}` |

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tonic::Status;

use crate::cluster::{EtcdNode, NodeId};
use crate::coord::{XFWD, XREP};
use crate::etcdpb::etcdserverpb::kv_client::KvClient;
use crate::etcdpb::etcdserverpb::{PutRequest, RangeRequest};

/// [sys] where the records go between nodes: no client's
pub const SYS_PREFIX: &str = "/.etcds/sys/";
/// [sys] the queue the leader takes sequence numbers' requests on
pub const SEQ_Q: &str = "etcds-seq";
/// [sys] the queue of sequences' definitions: an embedder's and a peer's puts only
pub const SEQDEF_Q: &str = "etcds-seqdef";
/// [sys] a peer's put of this record key wakes the leader's consumer of the request queues
pub(crate) const SEQ_WAKE: &str = "!seq-wake";

/// [sys] A record as it goes between nodes: [review 2026-11-21, 2] the ballot of the leader
/// that wrote it (`r` its round, `n` its node id), its version, its value, a delete
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Wire {
    #[serde(default)]
    pub(crate) r: u64,
    #[serde(default)]
    pub(crate) n: u64,
    pub(crate) v: u64,
    #[serde(default)]
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) deleted: bool,
}

/// [sys] A sequence's definition, as `seq_define` takes it
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqDefine {
    pub name: String,
    /// numbers a node takes at once: 1 every number from the leader, in order
    #[serde(default)]
    pub cache: i64,
    /// the first number of a new sequence (1 unless set)
    #[serde(default)]
    pub start: i64,
    /// the next number handed out is at least this
    #[serde(default)]
    pub at_least: i64,
    #[serde(default)]
    pub create: bool,
    #[serde(default)]
    pub drop: bool,
}

/// [sys] What `seq_define` answers: the next number the leader hands out, the cache size
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqDefined {
    pub next: i64,
    pub cache: i64,
    pub exists: bool,
}

/// [sys] A block of a sequence: its first number, and how many, itself included
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqBlock {
    pub first: i64,
    pub count: i64,
}

/// [sys] An embedder's per-node keep of the records (owner, 2026-11-21: YT Cluster's, its
/// node's own rows of `yt_config`): each node writes the records it holds - those of the
/// prefixes it was set with - as it takes them, before it counts or acknowledges one, and
/// reads them back as it starts. The majority and the ballots stay as they are: after the
/// whole cluster stopped, a takeover reads a majority of the kept copies. A record as it
/// goes between nodes: its wire JSON (ballot, version, value, tombstone).
#[tonic::async_trait]
pub trait SysKeep: Send + Sync + 'static {
    /// every record this node kept, as (key, wire JSON)
    async fn load(&self) -> Result<Vec<(String, String)>, String>;
    /// a record as this node holds it now
    async fn keep(&self, key: &str, wire: &str) -> Result<(), String>;
}

/// [sys] An embedder's durable store of the system records - one store for the cluster,
/// which the leader writes first and a new leader loads from (no majority then). Keys and
/// values as [`sys`](self) lists them. YT Cluster keeps the records per node instead
/// ([`SysKeep`]).
#[tonic::async_trait]
pub trait CoordStore: Send + Sync + 'static {
    /// every record, as (key, value)
    async fn load(&self) -> Result<Vec<(String, String)>, String>;
    /// a record written: the leader answers its caller only after this returns
    async fn put(&self, key: &str, value: &str) -> Result<(), String>;
    /// a record gone
    async fn delete(&self, key: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LeaseRec {
    pub(crate) ttl: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LockRec {
    pub(crate) key: String,
    pub(crate) lease: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SeqRec {
    pub(crate) cache: i64,
    pub(crate) next: i64,
    /// [review 2026-11-21] dropped: no numbers, but its mark kept - a sequence made again
    /// under its name starts above it, so a block a node still holds is never repeated
    #[serde(default)]
    pub(crate) dropped: bool,
}

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub(crate) fn lease_key(id: i64) -> String {
    format!("lease/{}", id)
}

pub(crate) fn lock_rec_key(name: &[u8]) -> String {
    format!("lock/{}", hex(name))
}

pub(crate) fn seq_key(name: &str) -> String {
    format!("seq/{}", name)
}

/// [sys] a put on the requests' queue, either spelling
pub(crate) fn is_seq_key(key: &[u8]) -> bool {
    let k = String::from_utf8_lossy(key);
    k.starts_with(&format!("/q/{}/p/", SEQ_Q)) || k.starts_with(&format!("/queue/{}/producer/", SEQ_Q))
}

/// [sys] [review 2026-11-21, 2] A key of the sequences' own queues, either spelling: the
/// requests', the definitions', and each node's reply queue. No client's but a put of a
/// request ([`is_seq_key`]): no consumer of them - which would take the leader's requests,
/// or a node's answers - no put of an answer or a definition, no delete.
pub(crate) fn reserved_queue(key: &[u8]) -> bool {
    let k = String::from_utf8_lossy(key);
    [format!("{}/", SEQ_Q), format!("{}/", SEQDEF_Q), format!("{}-node-", SEQ_Q)].iter()
        .any(|q| k.starts_with(&format!("/q/{}", q)) || k.starts_with(&format!("/queue/{}", q)))
}

/// [review 2026-11-21, 2] A reply queue the leader answers on: `etcds-seq-<name>`, one path
/// segment - no other queue, and none of the request queues
pub(crate) fn reply_queue_ok(reply: &str) -> bool {
    let prefix = format!("{}-", SEQ_Q);
    reply.len() > prefix.len() && reply.starts_with(&prefix) && !reply.contains('/')
}

/// the most numbers one block may hold: a client's count past it is refused
pub(crate) const MAX_BLOCK: i64 = 1_000_000;

/// What the records say, read back: the leases, the lock holders by name, the sequences.
/// A record that does not read is skipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Records {
    pub(crate) leases: HashMap<i64, i64>,
    pub(crate) locks: HashMap<Vec<u8>, (Vec<u8>, i64)>,
    pub(crate) seqs: HashMap<String, SeqRec>,
}

pub(crate) fn read_records<'a>(all: impl IntoIterator<Item = (&'a str, &'a str)>) -> Records {
    let mut r = Records::default();
    for (k, v) in all {
        if let Some(id) = k.strip_prefix("lease/").and_then(|i| i.parse::<i64>().ok()) {
            if let Ok(l) = serde_json::from_str::<LeaseRec>(v) {
                r.leases.insert(id, l.ttl);
            }
        } else if let Some(name) = k.strip_prefix("lock/").and_then(unhex) {
            if let Ok(l) = serde_json::from_str::<LockRec>(v)
                && let Some(key) = unhex(&l.key) {
                    r.locks.insert(name, (key, l.lease));
                }
        } else if let Some(name) = k.strip_prefix("seq/")
            && let Ok(s) = serde_json::from_str::<SeqRec>(v) {
                r.seqs.insert(name.to_string(), s);
            }
    }
    r
}

/// The block a sequence hands out: `count` numbers from `next`, or its cache size for 0;
/// the record's next moved past them. `None` for a count that overflows.
pub(crate) fn take_block(s: &SeqRec, count: i64) -> Option<(i64, i64, SeqRec)> {
    let n = if count > 0 { count } else { s.cache.max(1) };
    let next = s.next.checked_add(n)?;
    Some((s.next, n, SeqRec { cache: s.cache, next, dropped: false }))
}

/// [review 2026-11-21] A record with its version - the leader's counter, raised past every
/// version a new leader saw - and a delete kept as a tombstone, so a late put of an older
/// version is no put and a revoked lease is not brought back. [review 2026-11-21, 2] Ordered
/// by the ballot of the leader that wrote it first, then by the version: a later leader's
/// record stands over anything a superseded one wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Versioned {
    pub(crate) b: Ballot,
    pub(crate) v: u64,
    pub(crate) value: String,
    pub(crate) deleted: bool,
}

/// [review 2026-11-21, 2] A leader's ballot: the round of its takeover, then its node id -
/// one takeover's own, and ordered. A node that answers a takeover's range promises its
/// ballot, and from then on refuses a record of a lower one: a leader superseded while
/// its write was on the way is refused by the majority the new one read, so it cannot
/// answer for a block the new leader hands out again.
pub(crate) type Ballot = (u64, u64);

/// [review 2026-11-21, 2] the header a takeover's range carries its ballot in (`round:node`),
/// and a refusal the ballot the node promised
pub(crate) const XBALLOT: &str = "x-etcds-ballot";

pub(crate) fn ballot_text(b: Ballot) -> String {
    format!("{}:{}", b.0, b.1)
}

pub(crate) fn parse_ballot(s: &str) -> Option<Ballot> {
    let (r, n) = s.split_once(':')?;
    Some((r.parse().ok()?, n.parse().ok()?))
}

/// [review 2026-11-21, 2] A record of a ballot lower than the one this node promised: refused,
/// with the promised one for the writer to go above
pub(crate) fn ballot_refusal(promised: Ballot) -> Status {
    let mut s = Status::failed_precondition(tr!("sys.superseded"));
    if let Ok(v) = ballot_text(promised).parse() {
        s.metadata_mut().insert(XBALLOT, v);
    }
    s
}

/// the fewest of `n` nodes that are a majority
pub(crate) fn majority(n: usize) -> usize {
    n / 2 + 1
}

/// `incoming` over `map[key]` when it is newer - a later ballot, or the same and a later
/// version: the rule each node keeps its records by
pub(crate) fn apply_newer(map: &mut BTreeMap<String, Versioned>, key: &str, incoming: Versioned) -> bool {
    if map.get(key).is_some_and(|have| (have.b, have.v) >= (incoming.b, incoming.v)) {
        return false;
    }
    map.insert(key.to_string(), incoming);
    true
}

/// [review 2026-11-21, 2] the stripes that keep each record's writes in order
const STRIPES: usize = 64;

/// [sys] The system state on a node: the replicated records, the version counter, the
/// embedder's store, the sequences' blocks this node serves from, and the leader's lock
/// per sequence over its allocations
pub(crate) struct Sys {
    pub(crate) records: RwLock<BTreeMap<String, Versioned>>,
    pub(crate) version: std::sync::atomic::AtomicU64,
    pub(crate) store: RwLock<Option<Arc<dyn CoordStore>>>,
    /// a sequence's block on this node: the next number, and the end (not included)
    pub(crate) blocks: Mutex<HashMap<String, (i64, i64)>>,
    /// [review 2026-11-21] one allocation at a time per sequence, not one for them all
    pub(crate) alloc: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// [sys] requests asked by the queue pair, awaiting their answer
    pub(crate) pending: std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<SeqAnswer>>>,
    pub(crate) reply_started: std::sync::atomic::AtomicBool,
    /// [sys] the leader's consumer of the request queues: [review 2026-11-21, 2] its
    /// generation, so an old one ending does not clear a new one's slot, and its stop
    pub(crate) consumer: Mutex<Option<(u64, tokio::sync::oneshot::Sender<()>)>>,
    pub(crate) consumer_gen: std::sync::atomic::AtomicU64,
    /// [review 2026-11-21, 2] this node's ballot as the leader, from its last takeover
    pub(crate) ballot: std::sync::Mutex<Ballot>,
    /// [review 2026-11-21, 2] the highest ballot this node promised a takeover
    pub(crate) promised: std::sync::Mutex<Ballot>,
    /// [review 2026-11-21, 2] the highest round seen: a takeover goes above it
    pub(crate) round: std::sync::atomic::AtomicU64,
    /// [review 2026-11-21, 2] each record's writes one at a time, to the store and here
    stripes: Vec<Mutex<()>>,
    /// [sys] the embedder's per-node keep, and the record prefixes it keeps
    pub(crate) keep: RwLock<Option<(Arc<dyn SysKeep>, Vec<String>)>>,
}

impl Sys {
    pub(crate) fn new() -> Arc<Sys> {
        Arc::new(Sys {
            records: RwLock::new(BTreeMap::new()),
            version: std::sync::atomic::AtomicU64::new(0),
            store: RwLock::new(None),
            blocks: Mutex::new(HashMap::new()),
            alloc: std::sync::Mutex::new(HashMap::new()),
            pending: std::sync::Mutex::new(HashMap::new()),
            reply_started: std::sync::atomic::AtomicBool::new(false),
            consumer: Mutex::new(None),
            consumer_gen: std::sync::atomic::AtomicU64::new(0),
            ballot: std::sync::Mutex::new((0, 0)),
            promised: std::sync::Mutex::new((0, 0)),
            round: std::sync::atomic::AtomicU64::new(0),
            stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(),
            keep: RwLock::new(None),
        })
    }

    /// the live records, as (key, value)
    pub(crate) async fn live(&self) -> Vec<(String, String)> {
        self.records.read().await.iter().filter(|(_, r)| !r.deleted).map(|(k, r)| (k.clone(), r.value.clone())).collect()
    }

    /// a record's live value
    pub(crate) async fn get(&self, key: &str) -> Option<String> {
        self.records.read().await.get(key).filter(|r| !r.deleted).map(|r| r.value.clone())
    }

    fn alloc_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.alloc.lock().unwrap_or_else(|p| p.into_inner()).entry(name.to_string()).or_default().clone()
    }

    fn seen(&self, v: u64) {
        self.version.fetch_max(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn seen_round(&self, r: u64) {
        self.round.fetch_max(r, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn ballot(&self) -> Ballot {
        *self.ballot.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn promised(&self) -> Ballot {
        *self.promised.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn stripe(&self, key: &str) -> &Mutex<()> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        &self.stripes[(h.finish() as usize) % STRIPES]
    }
}

impl EtcdNode {
    /// [sys] The embedder's store: from now on the leader writes the records there before
    /// it answers, and a new leader loads from there. [review 2026-11-21] The records this
    /// node holds already are written there first, so none is lost by the change -
    /// [review 2026-11-21, 2] no record written meanwhile: a write waits for the change.
    /// The store keeps the order of the writers too: one that takes a superseded leader's
    /// write after the next leader loaded breaks the rule that no number goes out twice
    /// (YT Cluster's: the leader is its Boss, whose writes it commits).
    pub async fn set_coord_store(&self, store: Arc<dyn CoordStore>) -> Result<(), String> {
        let mut slot = self.sys.store.write().await;
        for (k, v) in self.sys.live().await {
            store.put(&k, &v).await?;
        }
        *slot = Some(store);
        Ok(())
    }

    /// [sys] The embedder's per-node keep: the records it kept read back first - each newer
    /// than the one held taken - then from now on every record of `prefixes` this node takes
    /// written there before it counts or acknowledges it. How many were read back.
    pub async fn set_sys_keep(&self, keep: Arc<dyn SysKeep>, prefixes: Vec<String>) -> Result<usize, String> {
        let rows = keep.load().await?;
        let mut n = 0;
        {
            let mut map = self.sys.records.write().await;
            for (k, v) in rows {
                let Ok(w) = serde_json::from_str::<Wire>(&v) else {
                    slog::warn!(self.log, "{}{}", crate::LP, tr!("sys.bad_record", err = k));
                    continue;
                };
                self.sys.seen(w.v);
                self.sys.seen_round(w.r);
                if apply_newer(&mut map, &k, Versioned { b: (w.r, w.n), v: w.v, value: w.value, deleted: w.deleted }) {
                    n += 1;
                }
            }
        }
        *self.sys.keep.write().await = Some((keep, prefixes));
        Ok(n)
    }

    /// [sys] Record `key` as this node holds it now, into the embedder's keep when it keeps
    /// that prefix - one write at a time per record, the newest version last
    async fn keep_record(&self, key: &str) -> Result<(), Status> {
        let Some((keep, prefixes)) = self.sys.keep.read().await.clone() else {
            return Ok(());
        };
        if !prefixes.iter().any(|p| key.starts_with(p.as_str())) {
            return Ok(());
        }
        let _order = self.sys.stripe(key).lock().await;
        let wire = self.sys.records.read().await.get(key)
            .map(|r| Wire { r: r.b.0, n: r.b.1, v: r.v, value: r.value.clone(), deleted: r.deleted });
        let Some(w) = wire else {
            return Ok(());
        };
        keep.keep(key, &serde_json::to_string(&w).unwrap_or_default()).await
            .map_err(|e| Status::unavailable(tr!("sys.not_kept", key = key, err = e)))
    }

    /// [sys] Writes a record (`None` deletes it), versioned: the embedder's store first - a
    /// failure there fails the call, nothing answered as done - then this node's map, then
    /// each peer's. [review 2026-11-21] Without an embedder's store, a majority of the
    /// nodes must hold it before it counts: else the call fails (`unavailable`), so a new
    /// leader, reading a majority, never misses a record a caller was answered for.
    /// [review 2026-11-21, 2] And none of it once a later takeover was promised - here or by
    /// a peer: this node leads no more until it takes over again (`unavailable`, ask again).
    pub(crate) async fn sys_write(&self, key: &str, value: Option<String>) -> Result<(), Status> {
        let (rec, stored) = {
            let slot = self.sys.store.read().await;
            let _order = self.sys.stripe(key).lock().await;
            if let Some(s) = slot.as_ref() {
                let done = match &value {
                    Some(v) => s.put(key, v).await,
                    None => s.delete(key).await,
                };
                done.map_err(|e| Status::unavailable(tr!("sys.store", err = e)))?;
            }
            let b = self.sys.ballot();
            let v = self.sys.version.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            let rec = Versioned { b, v, value: value.clone().unwrap_or_default(), deleted: value.is_none() };
            let mut map = self.sys.records.write().await;
            if slot.is_none() && self.sys.promised() > b {
                drop(map);
                return Err(self.superseded(None));
            }
            apply_newer(&mut map, key, rec.clone());
            (rec, slot.is_some())
        };
        // [sys] this node's own copy kept before it counts
        self.keep_record(key).await?;
        let (acked, held, by) = self.sys_replicate(key, Wire { r: rec.b.0, n: rec.b.1, v: rec.v, value: rec.value.clone(), deleted: rec.deleted }).await;
        if stored {
            return Ok(());
        }
        if by.is_some() {
            return Err(self.superseded(by));
        }
        let n = self.members(held).await;
        if acked + 1 < majority(n) {
            return Err(Status::unavailable(tr!("sys.no_majority", held = acked + 1, of = n)));
        }
        Ok(())
    }

    /// [review 2026-11-21, 2] A later takeover was promised: this node takes over again before
    /// it decides anything more - its lock waiters and its queue consumer end - and goes
    /// above the round it was told of
    pub(crate) fn superseded(&self, by: Option<Ballot>) -> Status {
        if let Some(b) = by {
            self.sys.seen_round(b.0);
        }
        self.coord.was_leader.store(false, std::sync::atomic::Ordering::Relaxed);
        self.coord.changed.notify_waiters();
        Status::unavailable(tr!("sys.superseded"))
    }

    /// [review 2026-11-21, 2] How many nodes the cluster has: this one, and every peer it
    /// holds or is configured with - a majority is never counted over only the peers held
    pub(crate) async fn members(&self, held: usize) -> usize {
        held.max(self.cfg.read().await.peers().len()) + 1
    }

    /// [sys] One record to every peer this node knows - a peer's KV put on [`SYS_PREFIX`] -
    /// each awaited a little: how many took it, of how many held, and the highest ballot a
    /// refusal named
    async fn sys_replicate(&self, key: &str, rec: Wire) -> (usize, usize, Option<Ballot>) {
        let targets = self.peer_channels(true).await;
        let n = targets.len();
        let put = PutRequest { key: format!("{}{}", SYS_PREFIX, key).into_bytes(), value: serde_json::to_vec(&rec).unwrap_or_default(), ..Default::default() };
        let mut calls = Vec::new();
        for (pid, ch) in targets {
            let node = self.clone();
            let put = put.clone();
            calls.push(tokio::spawn(async move {
                let r = node.peer_call(put, XREP, None).await;
                let sent = tokio::time::timeout(std::time::Duration::from_secs(2), KvClient::new(ch).put(r)).await;
                let err = match sent {
                    Ok(Ok(_)) => return (true, None),
                    Ok(Err(e)) => {
                        if let Some(b) = e.metadata().get(XBALLOT).and_then(|v| v.to_str().ok()).and_then(parse_ballot) {
                            return (false, Some(b));
                        }
                        e.message().to_string()
                    }
                    Err(_) => "timeout".to_string(),
                };
                slog::warn!(node.log, "{}{}", crate::LP, tr!("sys.not_replicated", peer = pid, err = err));
                (false, None)
            }));
        }
        let mut acked = 0;
        let mut by = None;
        for c in calls {
            let (ok, b) = c.await.unwrap_or((false, None));
            if ok {
                acked += 1;
            }
            by = by.max(b);
        }
        (acked, n, by)
    }

    /// [sys] The channels of the peers this node holds: every one with `all`, else those it
    /// writes to (not Spare). [review 2026-11-21, 2] Each handle's lock taken after the
    /// peers' lock is released.
    pub(crate) async fn peer_channels(&self, all: bool) -> Vec<(NodeId, tonic::transport::Channel)> {
        let handles = self.peers.read().await.handles();
        let mut v = Vec::new();
        for h in handles {
            let p = h.lock().await;
            if all || p.state != crate::peer::PeerState::Spare {
                v.push((p.peer_id, p.channel.clone()));
            }
        }
        v
    }

    /// [sys] Every record a peer holds, versions and tombstones too: a peer's range over
    /// [`SYS_PREFIX`] - a takeover's with its ballot, which the peer promises; refused, the
    /// ballot it promised instead
    async fn sys_load_peer(&self, ch: tonic::transport::Channel, ballot: Option<Ballot>) -> Result<Vec<(String, Wire)>, (String, Option<Ballot>)> {
        let range = RangeRequest { key: SYS_PREFIX.as_bytes().to_vec(), range_end: crate::prefix::prefix_end(SYS_PREFIX.as_bytes()).unwrap_or_else(|| vec![0]), ..Default::default() };
        let mut r = self.peer_call(range, XFWD, None).await;
        if let Some(b) = ballot
            && let Ok(v) = ballot_text(b).parse() {
                r.metadata_mut().insert(XBALLOT, v);
            }
        match tokio::time::timeout(std::time::Duration::from_secs(2), KvClient::new(ch).range(r)).await {
            Ok(Ok(got)) => Ok(got.into_inner().kvs.into_iter().filter_map(|kv| {
                let key = String::from_utf8(kv.key).ok()?.strip_prefix(SYS_PREFIX)?.to_string();
                Some((key, serde_json::from_slice::<Wire>(&kv.value).ok()?))
            }).collect()),
            Ok(Err(e)) => Err((e.message().to_string(), e.metadata().get(XBALLOT).and_then(|v| v.to_str().ok()).and_then(parse_ballot))),
            Err(_) => Err(("timeout".to_string(), None)),
        }
    }

    /// [sys] A peer's call on [`SYS_PREFIX`], as `srv/kv.rs` hands it here: a put is a
    /// record the leader replicates - taken when newer than the one held, never on the
    /// leader itself, and without a `peer_token` only from the address of a peer this node
    /// holds; a range is every record, for a node joining or taking over. A client's call
    /// on the prefix is refused before it gets here. [review 2026-11-21, 2] A record of a
    /// ballot below the one promised is refused.
    pub(crate) async fn sys_put_from_peer(&self, key: &str, value: &[u8], from: Option<std::net::SocketAddr>) -> Result<(), Status> {
        self.known_peer(from).await?;
        if self.is_leader().await {
            return Err(Status::failed_precondition(tr!("coord.replica_on_leader")));
        }
        let w: Wire = serde_json::from_slice(value).map_err(|e| Status::invalid_argument(tr!("sys.bad_record", err = e)))?;
        self.sys.seen(w.v);
        self.sys.seen_round(w.r);
        let mut map = self.sys.records.write().await;
        let promised = self.sys.promised();
        if (w.r, w.n) < promised {
            drop(map);
            return Err(ballot_refusal(promised));
        }
        let newer = apply_newer(&mut map, key, Versioned { b: (w.r, w.n), v: w.v, value: w.value, deleted: w.deleted });
        drop(map);
        // [sys] kept before it is acknowledged
        if newer {
            self.keep_record(key).await?;
        }
        Ok(())
    }

    /// [sys] Every record, for a peer's range. [review 2026-11-21, 2] A takeover's range
    /// (`ballot`) is promised first, in the same step as the read: refused when this node
    /// promised as high a ballot already.
    pub(crate) async fn sys_range_for_peer(&self, ballot: Option<Ballot>) -> Result<Vec<crate::etcdpb::mvccpb::KeyValue>, Status> {
        let map = self.sys.records.write().await;
        if let Some(b) = ballot {
            let mut p = self.sys.promised.lock().unwrap_or_else(|p| p.into_inner());
            if b <= *p {
                return Err(ballot_refusal(*p));
            }
            *p = b;
            self.sys.seen_round(b.0);
        }
        Ok(map.iter().map(|(k, r)| crate::etcdpb::mvccpb::KeyValue {
            key: format!("{}{}", SYS_PREFIX, k).into_bytes(),
            value: serde_json::to_vec(&Wire { r: r.b.0, n: r.b.1, v: r.v, value: r.value.clone(), deleted: r.deleted }).unwrap_or_default(),
            ..Default::default()
        }).collect())
    }

    /// [review 2026-11-21] Without a `peer_token`, `x-peer` is any client's header: a record
    /// is taken only from the address of a peer this node holds. With one, the token proved
    /// it already (`guard::peer_proof`). [review 2026-11-21, 2] The addresses are taken
    /// under the peers' lock and resolved after it.
    pub(crate) async fn known_peer(&self, from: Option<std::net::SocketAddr>) -> Result<(), Status> {
        if !self.cfg.read().await.peer_token.is_empty() {
            return Ok(());
        }
        let Some(from) = from.map(|a| a.ip()) else {
            return Err(Status::permission_denied(tr!("sys.peers_only")));
        };
        let conns: Vec<String> = self.peers.read().await.peer_info().await.into_iter().map(|(_, c, _)| c).collect();
        for conn in conns {
            let host = conn.trim().trim_start_matches("http://").trim_start_matches("https://").split('/').next().unwrap_or("").to_string();
            if let Ok(a) = host.parse::<std::net::SocketAddr>() {
                if a.ip() == from {
                    return Ok(());
                }
                continue;
            }
            if let Ok(addrs) = tokio::net::lookup_host(host.as_str()).await {
                for a in addrs {
                    if a.ip() == from {
                        return Ok(());
                    }
                }
            }
        }
        Err(Status::permission_denied(tr!("sys.peers_only")))
    }

    /// [sys] The records a takeover goes from, as (key, value): the embedder's store when
    /// there is one; else [review 2026-11-21] the newest version of each record among this
    /// node and every peer that answers - a majority of the nodes, or no takeover yet.
    /// [review 2026-11-21, 2] Under a ballot of its own, above every round seen: each node
    /// that answers promises it, so a leader before this one can write no more.
    pub(crate) async fn sys_load(&self) -> Result<Vec<(String, String)>, Status> {
        let round = self.sys.round.load(std::sync::atomic::Ordering::Relaxed)
            .max(self.sys.promised().0).max(self.sys.ballot().0) + 1;
        let b: Ballot = (round, self.node_id);
        self.sys.seen_round(round);
        let store = self.sys.store.read().await.clone();
        if let Some(s) = store {
            let all = s.load().await.map_err(|e| Status::unavailable(tr!("sys.store", err = e)))?;
            // this node's map follows what it loaded, past every version it has seen
            let base = self.sys.version.load(std::sync::atomic::Ordering::Relaxed);
            let mut map = self.sys.records.write().await;
            map.clear();
            for (i, (k, v)) in all.iter().enumerate() {
                map.insert(k.clone(), Versioned { b, v: base + 1 + i as u64, value: v.clone(), deleted: false });
            }
            self.sys.seen(base + all.len() as u64 + 1);
            *self.sys.ballot.lock().unwrap_or_else(|p| p.into_inner()) = b;
            return Ok(all);
        }
        {
            // this node promises its own takeover first, as a peer would
            let _map = self.sys.records.write().await;
            let mut p = self.sys.promised.lock().unwrap_or_else(|p| p.into_inner());
            *p = (*p).max(b);
        }
        let peers = self.peer_channels(true).await;
        let n = self.members(peers.len()).await;
        let mut answered = 1;
        for (pid, ch) in peers {
            match self.sys_load_peer(ch, Some(b)).await {
                Ok(recs) => {
                    answered += 1;
                    let mut newer = Vec::new();
                    {
                        let mut map = self.sys.records.write().await;
                        for (k, r) in recs {
                            self.sys.seen(r.v);
                            self.sys.seen_round(r.r);
                            if apply_newer(&mut map, &k, Versioned { b: (r.r, r.n), v: r.v, value: r.value, deleted: r.deleted }) {
                                newer.push(k);
                            }
                        }
                    }
                    for k in newer {
                        self.keep_record(&k).await?;
                    }
                }
                Err((e, by)) => {
                    if let Some(by) = by {
                        self.sys.seen_round(by.0);
                    }
                    slog::info!(self.log, "{}{}", crate::LP, tr!("sys.not_loaded", peer = pid, err = e))
                }
            }
        }
        if answered < majority(n) {
            return Err(Status::unavailable(tr!("sys.no_majority", held = answered, of = n)));
        }
        *self.sys.ballot.lock().unwrap_or_else(|p| p.into_inner()) = b;
        Ok(self.sys.live().await)
    }

    /// [sys] As a node joins: the newest of each record among the peers that answer, and
    /// the leases among them held here as the leader's replicas are
    pub(crate) async fn sys_sync_from_peers(&self) {
        for (pid, ch) in self.peer_channels(true).await {
            match self.sys_load_peer(ch, None).await {
                Ok(recs) => {
                    let mut newer = Vec::new();
                    {
                        let mut map = self.sys.records.write().await;
                        for (k, r) in recs {
                            self.sys.seen(r.v);
                            self.sys.seen_round(r.r);
                            if apply_newer(&mut map, &k, Versioned { b: (r.r, r.n), v: r.v, value: r.value, deleted: r.deleted }) {
                                newer.push(k);
                            }
                        }
                    }
                    for k in newer {
                        if let Err(e) = self.keep_record(&k).await {
                            slog::warn!(self.log, "{}{}", crate::LP, e.message());
                        }
                    }
                }
                Err((e, _)) => slog::info!(self.log, "{}{}", crate::LP, tr!("sys.not_loaded", peer = pid, err = e)),
            }
        }
        let read = read_records(self.sys.live().await.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let now = tokio::time::Instant::now();
        let mut leases = self.coord.leases.write().await;
        for (id, ttl) in read.leases {
            leases.entry(id).or_insert(crate::coord::Lease { granted_ttl: ttl, deadline: now + std::time::Duration::from_secs(ttl.max(1) as u64) });
        }
    }

    // ---- sequences ---------------------------------------------------------------

    /// [sys] The next number of sequence `name` on this node: from its block here, or a
    /// new block from the leader when it is used up - one number at a time for a sequence
    /// of cache 1, in order across the cluster
    pub async fn seq_next(&self, name: &str) -> Result<i64, Status> {
        {
            let mut blocks = self.sys.blocks.lock().await;
            if let Some((next, end)) = blocks.get_mut(name)
                && *next < *end {
                    let n = *next;
                    *next += 1;
                    return Ok(n);
                }
        }
        let got = self.seq_take(name, 0).await?;
        let mut blocks = self.sys.blocks.lock().await;
        if got.count > 1 {
            blocks.insert(name.to_string(), (got.first + 1, got.first + got.count));
        } else {
            blocks.remove(name);
        }
        Ok(got.first)
    }

    /// [embed] This node's block of sequence `name` forgotten: its next number comes from the
    /// leader - after an `at_least` past numbers the block still holds (a gap, never a repeat)
    pub async fn seq_forget_block(&self, name: &str) {
        self.sys.blocks.lock().await.remove(name);
    }

    /// [sys] A block of a sequence: `count` numbers, or its cache size for 0. Decided on
    /// the leader - here when this node leads, else asked by the queue pair
    pub async fn seq_take(&self, name: &str, count: i64) -> Result<SeqBlock, Status> {
        if !(0..=MAX_BLOCK).contains(&count) {
            return Err(Status::invalid_argument(tr!("sys.bad_count", count = count, max = MAX_BLOCK)));
        }
        if self.route_to_leader(false).await?.is_none() {
            return self.seq_block_here(name, count).await;
        }
        let a = self.seq_ask(SEQ_Q, SeqAsk { op: "next".into(), name: name.to_string(), count, ..Default::default() }).await?;
        a.block.ok_or_else(|| Status::internal(tr!("sys.bad_answer")))
    }

    /// [sys] Makes, changes or drops a sequence, on the leader:
    /// - `create`: a new one from `start` (1 unless set), refused when it exists; one made
    ///   again under a dropped one's name starts above the dropped one's mark;
    /// - `at_least`: the next number handed out is at least this - past what a local
    ///   sequence gave while the cluster was off;
    /// - `cache` (above 0): the block a node takes;
    /// - `drop`: gone, its mark kept, with this node's block of it.
    pub async fn seq_define(&self, d: SeqDefine) -> Result<SeqDefined, Status> {
        if d.name.is_empty() || d.name.contains('/') {
            return Err(Status::invalid_argument(tr!("sys.bad_seq_name", name = d.name)));
        }
        let drop = d.drop;
        let name = d.name.clone();
        let a = if self.route_to_leader(false).await?.is_none() {
            self.seq_define_here(d).await
        } else {
            self.seq_ask(SEQDEF_Q, SeqAsk { op: "define".into(), name: d.name.clone(), def: Some(d), ..Default::default() }).await
                .and_then(|a| a.defined.ok_or_else(|| Status::internal(tr!("sys.bad_answer"))))
        };
        if drop {
            self.sys.blocks.lock().await.remove(&name);
        }
        a
    }

    /// [sys] A block decided here, the leader: its high-water mark written before the
    /// answer, so no number is handed out twice - a leader switch or a restart loses what
    /// was not used, a gap, never a repeat
    pub(crate) async fn seq_block_here(&self, name: &str, count: i64) -> Result<SeqBlock, Status> {
        let lock = self.sys.alloc_lock(name);
        let _one = lock.lock().await;
        let key = seq_key(name);
        let current = self.sys.get(&key).await.and_then(|v| serde_json::from_str::<SeqRec>(&v).ok()).filter(|s| !s.dropped);
        let Some(s) = current else {
            return Err(Status::not_found(tr!("sys.no_seq", name = name)));
        };
        let Some((first, count, after)) = take_block(&s, count) else {
            return Err(Status::out_of_range(tr!("sys.seq_overflow", name = name)));
        };
        self.sys_write(&key, Some(serde_json::to_string(&after).unwrap_or_default())).await?;
        Ok(SeqBlock { first, count })
    }

    /// [sys] A definition decided here, the leader (`seq_define`)
    pub(crate) async fn seq_define_here(&self, r: SeqDefine) -> Result<SeqDefined, Status> {
        if r.name.is_empty() || r.name.contains('/') {
            return Err(Status::invalid_argument(tr!("sys.bad_seq_name", name = r.name)));
        }
        let lock = self.sys.alloc_lock(&r.name);
        let _one = lock.lock().await;
        let key = seq_key(&r.name);
        let current = self.sys.get(&key).await.and_then(|v| serde_json::from_str::<SeqRec>(&v).ok());
        let live = current.filter(|s| !s.dropped);
        if r.drop {
            // [ycoord] the mark answered: what a local sequence is moved above. Dropped
            // already: its mark again - each node that goes back to its own asks the same
            let Some(mut s) = live else {
                return match current {
                    Some(d) if d.dropped => Ok(SeqDefined { next: d.next, cache: d.cache, exists: false }),
                    _ => Err(Status::not_found(tr!("sys.no_seq", name = r.name))),
                };
            };
            s.dropped = true;
            self.sys_write(&key, Some(serde_json::to_string(&s).unwrap_or_default())).await?;
            self.sys.blocks.lock().await.remove(&r.name);
            return Ok(SeqDefined { next: s.next, cache: s.cache, exists: false });
        }
        let mut s = match (live, r.create) {
            (Some(_), true) => return Err(Status::already_exists(tr!("sys.seq_exists", name = r.name))),
            (Some(s), false) => s,
            (None, true) => {
                let start = if r.start == 0 { 1 } else { r.start };
                // above a dropped one's mark: a block a node still holds stays its own
                let floor = current.map(|d| d.next).unwrap_or(i64::MIN);
                SeqRec { cache: r.cache.max(1), next: start.max(floor), dropped: false }
            }
            (None, false) => return Err(Status::not_found(tr!("sys.no_seq", name = r.name))),
        };
        if r.cache > 0 {
            s.cache = r.cache;
        }
        if r.at_least > s.next {
            s.next = r.at_least;
        }
        self.sys_write(&key, Some(serde_json::to_string(&s).unwrap_or_default())).await?;
        Ok(SeqDefined { next: s.next, cache: s.cache, exists: true })
    }

    // ---- the queue pair ------------------------------------------------------------

    /// this node's reply queue: [review 2026-11-21, 2] `etcds-seq-node-<id hex>`, no client's
    pub(crate) fn seq_reply_queue(&self) -> String {
        format!("{}-node-{:x}", SEQ_Q, self.node_id)
    }

    /// [sys] A request to the leader by the queue pair: put on `queue`, the answer awaited
    /// on this node's reply queue (consumed from the first request on). No answer in
    /// [`ASK_TIMEOUT`]: `unavailable`, and the caller asks again - a request the leader took
    /// late is a gap, never a number twice.
    async fn seq_ask(&self, queue: &str, mut ask: SeqAsk) -> Result<SeqAnswer, Status> {
        self.seq_reply_consumer().await;
        self.seq_wake_leader().await?;
        ask.req = uuid::Uuid::new_v4().simple().to_string();
        ask.reply = self.seq_reply_queue();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.sys.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(ask.req.clone(), tx);
        let key = format!("/q/{}/p/{}", queue, ask.req);
        if let Err(e) = self.kv_put(key.into_bytes(), serde_json::to_vec(&ask).unwrap_or_default()).await {
            self.sys.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&ask.req);
            return Err(e);
        }
        self.queue_link(&format!("/q/{}", queue), &ask.reply).await;
        let got = tokio::time::timeout(ASK_TIMEOUT, rx).await;
        self.sys.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&ask.req);
        match got {
            Ok(Ok(a)) if a.ok => Ok(a),
            Ok(Ok(a)) => Err(Status::new(tonic::Code::from(a.code), a.msg)),
            _ => Err(Status::unavailable(tr!("sys.no_answer", timeout = format!("{:?}", ASK_TIMEOUT)))),
        }
    }

    /// [sys] This node's reply queue consumed, once: each answer to the request it names,
    /// then acknowledged
    async fn seq_reply_consumer(&self) {
        if self.sys.reply_started.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let key = format!("/q/{}/c/{}", self.seq_reply_queue(), uuid::Uuid::new_v4());
        let mut events = self.consume(&key).await;
        let node = self.clone();
        tokio::spawn(async move {
            while let Some((dk, value)) = events.recv().await {
                if let Ok(a) = serde_json::from_slice::<SeqAnswer>(&value)
                    && let Some(tx) = node.sys.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(&a.req) {
                        let _ = tx.send(a);
                    }
                let _ = node.kv_delete(dk).await;
            }
            node.sys.reply_started.store(false, std::sync::atomic::Ordering::Relaxed);
        });
    }

    /// [sys] The leader's consumer of the sequences' request queues: started on demand -
    /// a node with no sequence in use keeps no queue of them in its registry - and kept
    /// while it leads. [`seq_wake_leader`](Self::seq_wake_leader) asks for it before a
    /// request is put. A node that stops leading ends it (`seq_consumer_stop`), and what it
    /// held goes to the next leader, woken by the next request.
    /// A request is answered only while this node leads and has taken over: else it is
    /// left unacknowledged and the consumer ends, so the request goes to the next leader.
    pub(crate) async fn seq_consumer_start(&self) {
        let mut held = self.sys.consumer.lock().await;
        if held.is_some() {
            return;
        }
        let generation = self.sys.consumer_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        *held = Some((generation, stop_tx));
        drop(held);
        let mut next_rx = self.consume(&format!("/q/{}/c/{}", SEQ_Q, uuid::Uuid::new_v4())).await;
        let mut def_rx = self.consume(&format!("/q/{}/c/{}", SEQDEF_Q, uuid::Uuid::new_v4())).await;
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                // [review 2026-11-21, 2] with the queue it came on: each takes its own op only
                let (dk, value, from_def) = tokio::select! {
                    _ = &mut stop_rx => break,
                    m = next_rx.recv() => match m { Some((k, v)) => (k, v, false), None => break },
                    m = def_rx.recv() => match m { Some((k, v)) => (k, v, true), None => break },
                };
                // leading and taken over - read as it is, not through route_to_leader, whose
                // takeover starts this consumer
                if !node.is_leader().await || !node.coord.was_leader.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let Ok(ask) = serde_json::from_slice::<SeqAsk>(&value) else {
                    let _ = node.kv_delete(dk).await;
                    continue;
                };
                let answer = node.seq_answer(&ask, from_def).await;
                if reply_queue_ok(&ask.reply) {
                    let key = format!("/q/{}/p/{}", ask.reply, ask.req);
                    let _ = node.kv_put(key.into_bytes(), serde_json::to_vec(&answer).unwrap_or_default()).await;
                }
                let _ = node.kv_delete(dk).await;
            }
            let mut held = node.sys.consumer.lock().await;
            if held.as_ref().is_some_and(|(g, _)| *g == generation) {
                *held = None;
            }
        });
    }

    /// [sys] The leader's consumer running before a request is put: started here when
    /// this node leads, else asked of the leader - a peer's put of [`SEQ_WAKE`]
    pub(crate) async fn seq_wake_leader(&self) -> Result<(), Status> {
        match self.route_to_leader(false).await? {
            None => {
                self.seq_consumer_start().await;
                Ok(())
            }
            Some(ch) => {
                let put = PutRequest { key: format!("{}{}", SYS_PREFIX, SEQ_WAKE).into_bytes(), ..Default::default() };
                let r = self.peer_call(put, XFWD, None).await;
                match tokio::time::timeout(ASK_TIMEOUT, KvClient::new(ch).put(r)).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(Status::unavailable(tr!("sys.no_answer", timeout = format!("{:?}", ASK_TIMEOUT)))),
                }
            }
        }
    }

    pub(crate) async fn seq_consumer_stop(&self) {
        if let Some((_, stop)) = self.sys.consumer.lock().await.take() {
            let _ = stop.send(());
        }
    }

    /// [sys] One request decided here, the leader: [review 2026-11-21, 2] a block from the
    /// requests' queue, a definition from the definitions' queue only - a client's put of a
    /// definition on the requests' queue is no definition
    async fn seq_answer(&self, ask: &SeqAsk, from_def: bool) -> SeqAnswer {
        let got = match (ask.op.as_str(), from_def) {
            ("next", false) => match (0..=MAX_BLOCK).contains(&ask.count) {
                true => self.seq_block_here(&ask.name, ask.count).await.map(|b| (Some(b), None)),
                false => Err(Status::invalid_argument(tr!("sys.bad_count", count = ask.count, max = MAX_BLOCK))),
            },
            ("define", true) => match &ask.def {
                Some(d) => self.seq_define_here(d.clone()).await.map(|d| (None, Some(d))),
                None => Err(Status::invalid_argument(tr!("sys.bad_answer"))),
            },
            (op @ ("next" | "define"), _) => Err(Status::permission_denied(tr!("sys.wrong_queue", op = op,
                queue = if from_def { SEQDEF_Q } else { SEQ_Q }))),
            (other, _) => Err(Status::invalid_argument(tr!("sys.bad_op", op = other))),
        };
        match got {
            Ok((block, defined)) => SeqAnswer { req: ask.req.clone(), ok: true, block, defined, code: 0, msg: String::new() },
            Err(e) => SeqAnswer { req: ask.req.clone(), ok: false, block: None, defined: None, code: e.code() as i32, msg: e.message().to_string() },
        }
    }

    /// [sys] An in-process queue consumer on `key` (`/q/{name}/c/{uuid}`): each message as
    /// its delivery key and its value, for the caller to acknowledge
    async fn consume(&self, key: &str) -> tokio::sync::mpsc::Receiver<(Vec<u8>, Vec<u8>)> {
        use crate::etcdpb::etcdserverpb::watch_request::RequestUnion;
        use crate::etcdpb::etcdserverpb::{WatchCreateRequest, WatchRequest};
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        self.new_watcher(req_rx, tx, &uuid::Uuid::new_v4()).await;
        let _ = req_tx.send(WatchRequest { request_union: Some(RequestUnion::CreateRequest(WatchCreateRequest {
            key: key.as_bytes().to_vec(), watch_id: 1, ..Default::default() })) }).await;
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            // the request side held as long as the stream is read: dropped, the watch ends
            let _hold = req_tx;
            loop {
                // [review 2026-11-21, 2] the reader gone, the watch goes - on a quiet queue too
                let frame = tokio::select! {
                    _ = out_tx.closed() => return,
                    f = rx.recv() => f,
                };
                let Some(Ok(frame)) = frame else {
                    return;
                };
                if frame.canceled {
                    break;
                }
                for ev in frame.events {
                    if ev.r#type != 0 {
                        continue;
                    }
                    if let Some(kv) = ev.kv
                        && out_tx.send((kv.key, kv.value)).await.is_err() {
                            return;
                        }
                }
            }
        });
        out_rx
    }
}

/// how long an asker waits for the leader's answer
pub(crate) const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// [sys] A request on the queue pair, as a node or any etcd client puts it
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SeqAsk {
    /// `next` or `define`
    pub(crate) op: String,
    #[serde(default)]
    pub(crate) req: String,
    /// the queue the answer goes to: one path segment
    #[serde(default)]
    pub(crate) reply: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) count: i64,
    #[serde(default)]
    pub(crate) def: Option<SeqDefine>,
}

/// [sys] The leader's answer on the reply queue
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SeqAnswer {
    pub(crate) req: String,
    pub(crate) ok: bool,
    #[serde(default)]
    pub(crate) block: Option<SeqBlock>,
    #[serde(default)]
    pub(crate) defined: Option<SeqDefined>,
    #[serde(default)]
    pub(crate) code: i32,
    #[serde(default)]
    pub(crate) msg: String,
}

#[cfg(test)]
#[path = "sys_test.rs"]
mod tests;
