//! [sys] Sequences and the system records (owner, 2026-11-21): on one node, on two - a
//! follower's numbers from the leader, a leader switch going on above the high-water mark
//! - and through an embedder's store.
mod testing;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use etcd_client::{Client, LockOptions};
use etcds::cluster::EtcdNode;
use etcds::sys::{CoordStore, SeqDefine};

fn define(name: &str, cache: i64) -> SeqDefine {
    SeqDefine { name: name.to_string(), cache, create: true, ..Default::default() }
}

async fn until<F, Fut>(what: &str, secs: u64, mut f: F)
where F: FnMut() -> Fut, Fut: std::future::Future<Output = bool> {
    for _ in 0..secs * 10 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{} not within {} s", what, secs);
}

// ---- one node --------------------------------------------------------------------

#[tokio::test]
async fn a_sequence_counts_in_blocks_or_one_by_one() {
    let (node, _c, _url) = testing::start_server_at().await;
    node.seq_define(define("one", 1)).await.unwrap();
    node.seq_define(define("blk", 10)).await.unwrap();
    assert_eq!((node.seq_next("one").await.unwrap(), node.seq_next("one").await.unwrap()), (1, 2));
    let firsts: Vec<i64> = { let mut v = vec![]; for _ in 0..12 { v.push(node.seq_next("blk").await.unwrap()); } v };
    assert_eq!(firsts, (1..=12).collect::<Vec<_>>(), "in order within a node");
    // the mark is past the blocks taken: 1..10, then 11..20
    let def = node.seq_define(SeqDefine { name: "blk".into(), ..Default::default() }).await.unwrap();
    assert_eq!((def.next, def.cache), (21, 10));
    // a block of the count asked
    let b = node.seq_take("one", 5).await.unwrap();
    assert_eq!((b.first, b.count), (3, 5));
    assert_eq!(node.seq_next("one").await.unwrap(), 8);
}

#[tokio::test]
async fn a_sequence_moves_past_a_number_and_is_dropped() {
    let (node, mut c, _url) = testing::start_server_at().await;
    node.seq_define(SeqDefine { start: 100, ..define("s", 1) }).await.unwrap();
    assert_eq!(node.seq_next("s").await.unwrap(), 100);
    // a local twin gave up to 500 while the cluster was off
    let d = node.seq_define(SeqDefine { name: "s".into(), at_least: 501, ..Default::default() }).await.unwrap();
    assert_eq!(d.next, 501);
    assert_eq!(node.seq_next("s").await.unwrap(), 501);
    let back = node.seq_define(SeqDefine { name: "s".into(), at_least: 10, ..Default::default() }).await.unwrap();
    assert_eq!(back.next, 502, "never back");
    let code = |r: Result<_, tonic::Status>| r.map(|_| ()).unwrap_err().code();
    assert_eq!(code(node.seq_define(define("s", 1)).await), tonic::Code::AlreadyExists);
    assert_eq!(node.seq_next("none").await.unwrap_err().code(), tonic::Code::NotFound);
    assert_eq!(code(node.seq_define(define("a/b", 1)).await), tonic::Code::InvalidArgument);
    node.seq_define(SeqDefine { name: "s".into(), drop: true, ..Default::default() }).await.unwrap();
    assert_eq!(node.seq_next("s").await.unwrap_err().code(), tonic::Code::NotFound);
    // the system records are no client's: not read, not written, not deleted
    let sys = |e: etcd_client::Error| match e { etcd_client::Error::GRpcStatus(s) => s.code(), o => panic!("{o:?}") };
    assert_eq!(sys(c.get("/.etcds/sys/seq/s", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(sys(c.put("/.etcds/sys/seq/s", "{\"v\":99}", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(sys(c.delete("/.etcds/sys/seq/s", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(c.get("", Some(etcd_client::GetOptions::new().with_all_keys())).await.unwrap().kvs().iter()
        .filter(|kv| kv.key().starts_with(b"/.etcds/")).count(), 0, "not in a range over every key");
}

#[tokio::test]
async fn a_lock_with_no_lease_holds_one_of_the_lock_timeout_and_the_same_lease_locks_once() {
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    let port = free_port();
    cfg.listen_client_urls = format!("127.0.0.1:{}", port);
    cfg.lock_ttl = 7;
    let _n = served(cfg).await;
    let mut c = Client::connect([format!("http://127.0.0.1:{}", port)], None).await.unwrap();
    let held = c.lock("/nolease", None).await.unwrap();
    let key = String::from_utf8(held.key().to_vec()).unwrap();
    let lease = i64::from_str_radix(key.rsplit('/').next().unwrap(), 16).unwrap();
    assert_eq!(c.lease_time_to_live(lease, None).await.unwrap().granted_ttl(), 7, "the global lock timeout");
    // the same lease again: the one acquisition, at once
    let again = tokio::time::timeout(Duration::from_secs(2), c.lock("/nolease", Some(LockOptions::new().with_lease(lease)))).await.expect("no wait").unwrap();
    assert_eq!(again.key(), held.key());
    assert_eq!(etcds::cli::EtcdConfig::default().lock_ttl_secs(), 60, "an embedder's unset timeout");
}

// ---- an embedder's store ---------------------------------------------------------

/// a store in memory that says what it was asked, and can be made to fail
#[derive(Default)]
struct Mem {
    map: Mutex<std::collections::BTreeMap<String, String>>,
    calls: Mutex<Vec<String>>,
    fail: Mutex<bool>,
}

#[tonic::async_trait]
impl CoordStore for Mem {
    async fn load(&self) -> Result<Vec<(String, String)>, String> {
        Ok(self.map.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), String> {
        if *self.fail.lock().unwrap() { return Err("down".into()); }
        self.calls.lock().unwrap().push(format!("put {}", key));
        self.map.lock().unwrap().insert(key.into(), value.into());
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<(), String> {
        if *self.fail.lock().unwrap() { return Err("down".into()); }
        self.calls.lock().unwrap().push(format!("delete {}", key));
        self.map.lock().unwrap().remove(key);
        Ok(())
    }
}

#[tokio::test]
async fn the_leader_writes_the_store_before_it_answers_and_a_store_down_fails_the_call() {
    let (node, mut c, _url) = testing::start_server_at().await;
    let mem = Arc::new(Mem::default());
    node.set_coord_store(mem.clone()).await.unwrap();
    node.seq_define(define("st", 5)).await.unwrap();
    assert_eq!(node.seq_next("st").await.unwrap(), 1);
    let lease = c.lease_grant(30, None).await.unwrap().id();
    let held = c.lock("/st", Some(LockOptions::new().with_lease(lease))).await.unwrap();
    {
        let calls = mem.calls.lock().unwrap().clone();
        assert_eq!(calls, ["put seq/st", "put seq/st", format!("put lease/{}", lease).as_str(), format!("put lock/{}", "2f7374").as_str()]);
        assert!(mem.map.lock().unwrap().get("seq/st").unwrap().contains("\"next\":6"), "the block's mark, stored");
    }
    c.unlock(held.key()).await.unwrap();
    assert!(mem.calls.lock().unwrap().last().unwrap().starts_with("delete lock/"));
    // the store down: nothing handed out, nothing granted
    *mem.fail.lock().unwrap() = true;
    for _ in 0..4 { node.seq_next("st").await.unwrap(); }
    assert_eq!(node.seq_next("st").await.unwrap_err().code(), tonic::Code::Unavailable, "a new block needs the store");
    assert!(c.lease_grant(30, None).await.is_err());
    *mem.fail.lock().unwrap() = false;
    assert_eq!(node.seq_next("st").await.unwrap(), 6, "nothing was taken while it was down");
}

// ---- two nodes -------------------------------------------------------------------

const CLUSTER: &str = "88888888-8888-5888-8888-888888888888";
const A: &str = "99999999-9999-5999-8999-999999999999";
const B: &str = "aaaaaaaa-aaaa-5aaa-8aaa-aaaaaaaaaaaa";

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn cfg(node: &str, port: u16, peers: &str) -> etcds::cli::EtcdConfig {
    let mut c = etcds::cli::EtcdConfig::with_defaults();
    c.cluster = CLUSTER.to_string();
    c.node = node.to_string();
    c.listen_client_urls = format!("127.0.0.1:{}", port);
    c.initial_advertise_peer_urls = peers.to_string();
    c
}

async fn served(c: etcds::cli::EtcdConfig) -> EtcdNode {
    let port: u16 = c.listen_client_urls.rsplit(':').next().unwrap().parse().unwrap();
    let n = EtcdNode::init(c, slog::Logger::root(slog::Discard, slog::o!()), #[cfg(feature = "tracer")] None).await.unwrap();
    let s = n.clone();
    tokio::spawn(async move { let _ = s.serve().await; });
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    n
}

/// two peered nodes, and which leads: (leader, follower)
async fn two() -> (EtcdNode, EtcdNode) {
    let (pa, pb) = (free_port(), free_port());
    let (ua, ub) = (format!("http://127.0.0.1:{}", pa), format!("http://127.0.0.1:{}", pb));
    let a = served(cfg(A, pa, "")).await;
    let b = served(cfg(B, pb, &ua)).await;
    let both = format!("{},{}", ua, ub);
    a.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg(A, pa, &both)))).await.unwrap();
    b.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg(B, pb, &both)))).await.unwrap();
    let mut ca = Client::connect([ua.as_str()], None).await.unwrap();
    let cb = Client::connect([ub.as_str()], None).await.unwrap();
    until("each holding the other", 15, || { let (mut a, mut b) = (ca.clone(), cb.clone()); async move {
        !a.member_list().await.unwrap().members().is_empty() && !b.member_list().await.unwrap().members().is_empty()
    } }).await;
    let la = ca.status().await.unwrap().leader();
    until("one leader", 5, || { let mut b = cb.clone(); async move { b.status().await.unwrap().leader() == la } }).await;
    if la == a.member_id() { (a, b) } else { (b, a) }
}

/// makes `uuid` the named leader on both
async fn name_leader(nodes: [&EtcdNode; 2], uuid: &str) {
    for n in nodes {
        let mut c = n.config().await;
        c.leader = uuid.to_string();
        n.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(c))).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
}

#[tokio::test]
async fn two_nodes_never_hand_out_one_number_twice() {
    let (leader, follower) = two().await;
    follower.seq_define(define("c5", 5)).await.unwrap();
    follower.seq_define(define("c1", 1)).await.unwrap();
    let mut tasks = Vec::new();
    for n in [leader.clone(), follower.clone()] {
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            for _ in 0..23 { got.push(n.seq_next("c5").await.unwrap()); }
            got
        }));
    }
    let mut all = HashSet::new();
    for t in tasks {
        let got = t.await.unwrap();
        assert!(got.windows(2).all(|w| w[0] < w[1]), "in order within a node: {:?}", got);
        for g in got { assert!(all.insert(g), "{} twice", g); }
    }
    assert_eq!(all.len(), 46);
    // cache 1: every number from the leader, in the order asked, whichever node asks
    let mut last = 0;
    for i in 0..10 {
        let n = if i % 2 == 0 { &leader } else { &follower };
        let v = n.seq_next("c1").await.unwrap();
        assert!(v > last, "{} after {}", v, last);
        last = v;
    }
}

#[tokio::test]
async fn a_new_leader_goes_on_above_the_mark_and_keeps_the_lock_holders() {
    let (leader, follower) = two().await;
    leader.seq_define(define("sw", 10)).await.unwrap();
    let before = leader.seq_next("sw").await.unwrap();
    let lead_url = format!("http://{}", leader.config().await.listen_client_urls);
    let mut lc = Client::connect([lead_url.as_str()], None).await.unwrap();
    let lease = lc.lease_grant(30, None).await.unwrap().id();
    let held = lc.lock("/sw", Some(LockOptions::new().with_lease(lease))).await.unwrap();

    let fuuid = if follower.member_id() == leader.member_id() { unreachable!() } else if follower.config().await.node == A { A } else { B };
    name_leader([&leader, &follower], fuuid).await;
    // the old leader's block is its own; the new leader's next block starts above the mark
    let after = follower.seq_next("sw").await.unwrap();
    assert!(after >= before + 10, "{} after {}: no number of the old leader's block again", after, before);
    let f_url = format!("http://{}", follower.config().await.listen_client_urls);
    let mut fc = Client::connect([f_url.as_str()], None).await.unwrap();
    let l2 = fc.lease_grant(30, None).await.unwrap().id();
    let mut fc2 = fc.clone();
    let waiting = tokio::spawn(async move { fc2.lock("/sw", Some(LockOptions::new().with_lease(l2))).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished(), "the new leader knows the holder from its record");
    fc.unlock(held.key()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("granted once unlocked").unwrap().unwrap();
}

#[tokio::test]
async fn a_node_joining_takes_the_records_over() {
    let pa = free_port();
    let ua = format!("http://127.0.0.1:{}", pa);
    let a = served(cfg(A, pa, "")).await;
    a.seq_define(define("join", 3)).await.unwrap();
    a.seq_next("join").await.unwrap();
    // B starts after the records were written, and loads them from A
    let pb = free_port();
    let b = served(cfg(B, pb, &ua)).await;
    // A learns B as an embedder tells it (B announced itself before it served)
    let both = format!("{},http://127.0.0.1:{}", ua, pb);
    a.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg(A, pa, &both)))).await.unwrap();
    let mut ca = Client::connect([ua.as_str()], None).await.unwrap();
    until("A holding B", 15, || { let mut a = ca.clone(); async move { !a.member_list().await.unwrap().members().is_empty() } }).await;
    let _ = ca.status().await;
    name_leader([&a, &b], B).await;
    let mut cb = Client::connect([format!("http://127.0.0.1:{}", pb)], None).await.unwrap();
    let _ = cb.status().await.unwrap();
    assert_eq!(b.seq_next("join").await.unwrap(), 4, "B goes on from A's mark");
}

// ---- [review 2026-11-21] ---------------------------------------------------------

#[tokio::test]
async fn a_sequence_made_again_starts_above_the_dropped_one_s_mark() {
    let (node, _c, url) = testing::start_server_at().await;
    node.seq_define(define("re", 10)).await.unwrap();
    assert_eq!(node.seq_next("re").await.unwrap(), 1, "this node holds 1..10");
    node.seq_define(SeqDefine { name: "re".into(), drop: true, ..Default::default() }).await.unwrap();
    let again = node.seq_define(define("re", 10)).await.unwrap();
    assert_eq!(again.next, 11, "above the mark: a block another node still holds stays its own");
    // a block is bounded
    let e = node.seq_take("re", etcds_max_block() + 1).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::InvalidArgument);
    let _ = url;
}

fn etcds_max_block() -> i64 {
    1_000_000
}

#[tokio::test]
async fn records_held_before_a_store_is_set_go_into_it() {
    let (node, _c, _url) = testing::start_server_at().await;
    node.seq_define(define("before", 2)).await.unwrap();
    let mem = Arc::new(Mem::default());
    node.set_coord_store(mem.clone()).await.unwrap();
    assert!(mem.map.lock().unwrap().contains_key("seq/before"));
    assert_eq!(node.seq_next("before").await.unwrap(), 1, "and it goes on from there");
}

#[tokio::test]
async fn two_leaders_named_at_once_hand_out_nothing() {
    let (leader, follower) = two().await;
    leader.seq_define(define("split", 1)).await.unwrap();
    // each node told it leads: neither holds a majority of the other's writes
    let (lu, fu) = (if leader.config().await.node == A { A } else { B }, if follower.config().await.node == A { A } else { B });
    let mut lc = leader.config().await; lc.leader = lu.to_string();
    let mut fc = follower.config().await; fc.leader = fu.to_string();
    leader.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(lc))).await.unwrap();
    follower.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(fc))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(leader.seq_next("split").await.unwrap_err().code(), tonic::Code::Unavailable, "its write not held by a majority");
    assert_eq!(follower.seq_next("split").await.unwrap_err().code(), tonic::Code::Unavailable, "nor this one's takeover");
}

#[tokio::test]
async fn a_lock_with_no_lease_waits_past_the_lock_timeout() {
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    let port = free_port();
    cfg.listen_client_urls = format!("127.0.0.1:{}", port);
    cfg.lock_ttl = 5;
    let _n = served(cfg).await;
    let mut c = Client::connect([format!("http://127.0.0.1:{}", port)], None).await.unwrap();
    let lease = c.lease_grant(60, None).await.unwrap().id();
    let held = c.lock("/wait", Some(LockOptions::new().with_lease(lease))).await.unwrap();
    let mut c2 = c.clone();
    let waiting = tokio::spawn(async move { c2.lock("/wait", None).await });
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(!waiting.is_finished(), "still waiting, its lease kept alive while it does");
    c.unlock(held.key()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("granted").unwrap().unwrap();
}

/// [sys] Any etcd client takes numbers by the queue pair: a put on the request queue naming
/// its reply queue, the answer on that one; a definition by a client is refused
#[tokio::test]
async fn an_etcd_client_takes_numbers_by_the_queue_pair() {
    let (node, mut c, _url) = testing::start_server_at().await;
    node.seq_define(define("q", 5)).await.unwrap();
    let me = uuid::Uuid::new_v4();
    let mut answers = c.watch(format!("/q/etcds-seq-myreply/c/{}", me), None).await.unwrap();
    c.put("/q/etcds-seq/p/r1", r#"{"op":"next","name":"q","reply":"etcds-seq-myreply","req":"r1"}"#, None).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = answers.message().await.unwrap().unwrap();
            for ev in frame.events() {
                if let Some(kv) = ev.kv() {
                    return (kv.key().to_vec(), String::from_utf8(kv.value().to_vec()).unwrap());
                }
            }
        }
    }).await.expect("an answer");
    assert!(got.1.contains("\"req\":\"r1\"") && got.1.contains("\"ok\":true") && got.1.contains("\"first\":1") && got.1.contains("\"count\":5"), "{}", got.1);
    c.delete(got.0, None).await.unwrap();
    assert_eq!(node.seq_next("q").await.unwrap(), 6, "the client's block was its own");
    let e = c.put("/q/etcds-seqdef/p/x", r#"{"op":"define"}"#, None).await.unwrap_err();
    assert!(matches!(e, etcd_client::Error::GRpcStatus(ref s) if s.code() == tonic::Code::PermissionDenied), "{e:?}");
}

/// the first answer on a client's reply queue within `wait`, acknowledged
async fn answer_on(c: &mut etcd_client::Client, queue: &str, wait: Duration) -> Option<String> {
    let mut answers = c.watch(format!("/q/{}/c/{}", queue, uuid::Uuid::new_v4()), None).await.unwrap();
    let got = tokio::time::timeout(wait, async {
        loop {
            let frame = answers.message().await.ok()??;
            for ev in frame.events() {
                if let Some(kv) = ev.kv() {
                    return Some((kv.key().to_vec(), String::from_utf8(kv.value().to_vec()).unwrap()));
                }
            }
        }
    }).await.ok().flatten()?;
    c.delete(got.0, None).await.unwrap();
    Some(got.1)
}

/// [review 2026-11-21, 2] A client's definition put on the requests' queue is no definition:
/// answered refused, the sequence as it was; and an answer goes to a reply queue only
#[tokio::test]
async fn a_client_defines_nothing_on_the_requests_queue_and_answers_go_to_reply_queues_only() {
    let (node, mut c, _url) = testing::start_server_at().await;
    node.seq_define(define("kept", 1)).await.unwrap();
    let mut probe = c.clone();
    let wait = tokio::spawn(async move { answer_on(&mut probe, "etcds-seq-r2", Duration::from_secs(5)).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.put("/q/etcds-seq/p/r2", r#"{"op":"define","req":"r2","reply":"etcds-seq-r2","def":{"name":"kept","drop":true}}"#, None).await.unwrap();
    let a = wait.await.unwrap().expect("an answer");
    assert!(a.contains("\"ok\":false") && a.contains("\"code\":7"), "{a}");
    assert_eq!(node.seq_next("kept").await.unwrap(), 1, "not dropped");
    // a reply queue that is none: no answer goes there
    let mut probe = c.clone();
    let wait = tokio::spawn(async move { answer_on(&mut probe, "victim", Duration::from_secs(2)).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    c.put("/q/etcds-seq/p/r3", r#"{"op":"next","req":"r3","reply":"victim","name":"kept"}"#, None).await.unwrap();
    assert_eq!(wait.await.unwrap(), None);
}

/// [review 2026-11-21, 2] No client consumes the sequences' queues, answers on a node's reply
/// queue, or deletes what they hold; and `x-peer` alone reads no record
#[tokio::test]
async fn a_client_reaches_neither_the_sequences_queues_nor_the_records_as_a_peer() {
    let (_node, mut c, url) = testing::start_server_at().await;
    let denied = |e: etcd_client::Error| matches!(e, etcd_client::Error::GRpcStatus(ref s) if s.code() == tonic::Code::PermissionDenied);
    for key in ["/q/etcds-seq/c/me", "/q/etcds-seqdef/c/me", "/q/etcds-seq-node-1/c/me", "/queue/etcds-seq/consumer/me"] {
        let mut w = c.watch(key, None).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(2), w.message()).await.unwrap().unwrap().unwrap();
        assert!(frame.canceled(), "{key}: refused, not consumed");
    }
    assert!(denied(c.put("/q/etcds-seq-node-1/p/x", r#"{"req":"x","ok":true}"#, None).await.unwrap_err()));
    assert!(denied(c.put("/queue/etcds-seqdef/producer/x", "{}", None).await.unwrap_err()));
    assert!(denied(c.delete("/q/etcds-seq/c/someone", None).await.unwrap_err()));
    // x-peer alone, from no peer's address
    use etcds::etcdpb::etcdserverpb::kv_client::KvClient;
    use etcds::etcdpb::etcdserverpb::RangeRequest;
    let mut kv = KvClient::connect(url).await.unwrap();
    let mut r = tonic::Request::new(RangeRequest { key: b"/.etcds/sys/".to_vec(), range_end: b"/.etcds/sys0".to_vec(), ..Default::default() });
    r.metadata_mut().insert(etcds::etcdpb::XPEER, "1".parse().unwrap());
    assert_eq!(kv.range(r).await.unwrap_err().code(), tonic::Code::PermissionDenied);
}

/// a per-node keep in memory, as an embedder's table would hold it
#[derive(Default)]
struct MemKeep(Mutex<std::collections::BTreeMap<String, String>>);

#[tonic::async_trait]
impl etcds::sys::SysKeep for MemKeep {
    async fn load(&self) -> Result<Vec<(String, String)>, String> {
        Ok(self.0.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
    async fn keep(&self, key: &str, wire: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.to_string(), wire.to_string());
        Ok(())
    }
}

/// [sys] Each node keeps its copy of the sequences' records as it takes them, the leader's
/// and a replica's; read back as a node starts, a sequence goes on above its mark - no
/// number twice after the whole cluster stopped. A prefix not kept is not written.
#[tokio::test]
async fn kept_records_bring_a_sequence_back_after_a_full_stop() {
    let (leader, follower) = two().await;
    let (ka, kb) = (Arc::new(MemKeep::default()), Arc::new(MemKeep::default()));
    leader.set_sys_keep(ka.clone(), vec!["seq/".into()]).await.unwrap();
    follower.set_sys_keep(kb.clone(), vec!["seq/".into()]).await.unwrap();
    follower.seq_define(define("k", 1)).await.unwrap();
    let mut got = Vec::new();
    for i in 0..5 {
        got.push(if i % 2 == 0 { leader.seq_next("k").await.unwrap() } else { follower.seq_next("k").await.unwrap() });
    }
    assert_eq!(got, vec![1, 2, 3, 4, 5]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    for k in [&ka, &kb] {
        let kept = k.0.lock().unwrap().get("seq/k").cloned().expect("kept on each node");
        assert!(kept.contains(r#"\"next\":6"#), "{kept}");
        assert!(!k.0.lock().unwrap().keys().any(|k| !k.starts_with("seq/")), "only the prefix kept");
    }
    // the whole cluster again, from what each node kept
    let (c, _cc, _uc) = testing::start_server_at().await;
    assert_eq!(c.set_sys_keep(kb.clone(), vec!["seq/".into()]).await.unwrap(), 1);
    assert_eq!(c.seq_next("k").await.unwrap(), 6, "above the kept mark");
}

/// [embed] The embedder's calls from a node that does not lead go on to the leader: a lease
/// granted, kept and revoked there; a try refused while the leader holds the lock for
/// another lease, granted once it is freed; the unlock sent on as well
#[tokio::test]
async fn the_embedders_calls_from_a_follower_reach_the_leader() {
    let (leader, follower) = two().await;
    let mine = follower.embed_lease_grant(30).await.unwrap();
    assert_eq!(follower.embed_lease_keep(mine).await.unwrap(), 30, "kept at the leader");
    let other = leader.embed_lease_grant(30).await.unwrap();
    let held = leader.embed_lock(b"/.yt/advisory/db/7", other, false).await.unwrap().unwrap();
    assert_eq!(follower.embed_lock(b"/.yt/advisory/db/7", mine, true).await.unwrap(), None, "the try's header goes on too");
    follower.embed_unlock(&held).await.unwrap();
    let key = follower.embed_lock(b"/.yt/advisory/db/7", mine, true).await.unwrap();
    assert_eq!(key, Some(etcds::embed::lock_holder_key(b"/.yt/advisory/db/7", mine)));
    follower.embed_lease_revoke(mine).await.unwrap();
    assert_eq!(follower.embed_lease_keep(mine).await.unwrap(), 0, "gone at the leader");
    assert!(follower.set_leader("not a uuid").await.is_err());
    assert_eq!(follower.lock_ttl().await, 60);
}
