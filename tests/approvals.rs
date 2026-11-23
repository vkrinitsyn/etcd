//! [approve] Approvals of a queue's messages (feature `approvals`), as etcd v3 clients see them,
//! against signon's scratch database: `SIGNON_TEST_DB`, or `signon_test` over the local socket -
//! skipped with a note without one.
//!
//! `cargo test --features approvals --test approvals`
#![cfg(feature = "approvals")]

use std::time::Duration;

use etcd_client::{Client, ConnectOptions, Error, EventType, WatchStream};
use etcds::cluster::EtcdNode;

const PEER_TOKEN: &str = "the cluster's shared secret";
const PASSWORD: &str = "a long enough pass 123";
const CLUSTER: &str = "88888888-8888-5888-8888-888888888888";

fn code(e: Error) -> tonic::Code {
    match e {
        Error::GRpcStatus(s) => s.code(),
        other => panic!("expected a gRPC status, got {:?}", other),
    }
}

fn db_url() -> String {
    std::env::var("SIGNON_TEST_DB").unwrap_or_else(|_| "postgres:///signon_test?host=/var/run/postgresql".to_string())
}

/// one node start at a time: each applies signon's schema, and two at once race
static START: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

async fn pool() -> Option<sqlx::PgPool> {
    match sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(&db_url()).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("approval tests skipped: no test database ({e})");
            None
        }
    }
}

fn cfg(node: &str, port: u16, peers: &str) -> etcds::cli::EtcdConfig {
    let mut c = etcds::cli::EtcdConfig::with_defaults();
    c.cluster = CLUSTER.to_string();
    if !node.is_empty() {
        c.node = node.to_string();
    }
    c.listen_client_urls = format!("127.0.0.1:{}", port);
    c.initial_advertise_peer_urls = peers.to_string();
    c.auth_db_url = db_url();
    c.peer_token = PEER_TOKEN.to_string();
    c.auth = true;
    c
}

/// a served node with signon behind it and authentication on, and its url
async fn served(c: etcds::cli::EtcdConfig) -> (EtcdNode, String) {
    let port: u16 = c.listen_client_urls.rsplit(':').next().unwrap().parse().unwrap();
    let n = {
        let _one = START.lock().await;
        EtcdNode::init(c, slog::Logger::root(slog::Discard, slog::o!()), #[cfg(feature = "tracer")] None).await.unwrap()
    };
    let s = n.clone();
    tokio::spawn(async move { let _ = s.serve().await; });
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (n, format!("http://127.0.0.1:{}", port))
}

/// a fresh account, with `roles`
async fn account(pool: &sqlx::PgPool, tag: &str, roles: &[&str]) -> String {
    let email = format!("apv-{tag}-{}@test.example", uuid::Uuid::new_v4().simple());
    let id = signon::api::add_account(pool, &email, PASSWORD).await.unwrap().unwrap();
    for r in roles {
        signon::api::add_role(pool, r).await.unwrap();
        signon::grant_role(pool, id, r).await.unwrap();
    }
    email
}

async fn signed_in(url: &str, user: &str) -> Client {
    Client::connect([url], Some(ConnectOptions::new().with_user(user, PASSWORD))).await.unwrap()
}

/// The people of a queue, each a fresh account: who puts its messages, who may recommend (3),
/// approve (4), veto (5) and override (6) by signon's ACL on the queue, and one with nothing
struct People {
    queue: String,
    root: String,
    producer: String,
    rae: String,
    ann: String,
    bob: String,
    vic: String,
    ovi: String,
    out: String,
}

async fn people(pool: &sqlx::PgPool) -> People {
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let queue = format!("pay-{tag}");
    let role = |what: &str| format!("{what}-{tag}");
    // labac: the queue is a resource of the kind `queue`; a group - here a signon role - has its level there
    signon::labac::put_resource(pool, &signon::labac::Resource { kind: "queue".into(), id: queue.clone(), ..Default::default() }).await.unwrap();
    for (what, level) in [("rec", 3), ("apv", 4), ("vet", 5), ("ovr", 6)] {
        signon::labac::add_entry(pool, &signon::labac::Entry { kind: "queue".into(), resource: queue.clone(), group: role(what), level,
            ..Default::default() }).await.unwrap();
    }
    People {
        root: account(pool, "root", &["root"]).await,
        producer: account(pool, "prod", &[&role("prod")]).await,
        rae: account(pool, "rae", &[&role("rec")]).await,
        ann: account(pool, "ann", &[&role("apv")]).await,
        bob: account(pool, "bob", &[&role("apv")]).await,
        vic: account(pool, "vic", &[&role("vet")]).await,
        ovi: account(pool, "ovi", &[&role("ovr")]).await,
        out: account(pool, "out", &[]).await,
        queue,
    }
}

impl People {
    fn state(&self, key: &str) -> String {
        format!("/approval/{}/{key}", self.queue)
    }
    fn put_key(&self, key: &str) -> String {
        format!("/q/{}/p/{key}", self.queue)
    }
    /// the producer may put the queue's messages, as any producer: a key range of its role
    async fn grant_producer(&self, root: &mut Client) {
        let role = self.producer_role(root).await;
        root.role_grant_permission(role.as_str(), etcd_client::Permission::write(format!("/q/{}/p/", self.queue)).with_prefix()).await.unwrap();
    }
    async fn producer_role(&self, root: &mut Client) -> String {
        root.user_get(self.producer.as_str()).await.unwrap().roles()[0].clone()
    }
}

/// the state a waiting message's key shows, read as `root`
async fn shown(root: &mut Client, key: &str) -> Option<serde_json::Value> {
    let r = root.get(key, None).await.unwrap();
    r.kvs().first().map(|kv| serde_json::from_slice(kv.value()).unwrap())
}

/// the next message a consumer's watch receives within `ms`, acknowledged; `None` nothing came
async fn received(c: &mut Client, s: &mut WatchStream, ms: u64) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let resp = tokio::time::timeout(left, s.message()).await.ok()?.unwrap()?;
        for ev in resp.events() {
            if ev.event_type() == EventType::Put
                && let Some(kv) = ev.kv() {
                    let v = String::from_utf8_lossy(kv.value()).to_string();
                    c.delete(kv.key(), None).await.unwrap();
                    return Some(v);
                }
        }
    }
}

/// A message put to a gated queue waits: nobody consumes it until enough different accounts
/// approved, each by its level; a veto drops the voting, an override lifts it; then it is the
/// queue's as any message
#[tokio::test]
async fn a_message_waits_for_its_approvals() {
    let Some(pool) = pool().await else { return };
    let (_n, url) = served(cfg("", free_port(), "")).await;
    let p = people(&pool).await;
    let mut root = signed_in(&url, &p.root).await;
    p.grant_producer(&mut root).await;
    root.put(format!("/approval/{}", p.queue), r#"{"need": 2}"#, None).await.unwrap();
    assert_eq!(shown(&mut root, &format!("/approval/{}", p.queue)).await, Some(serde_json::json!({ "need": 2, "alias": "queue" })));

    let me = uuid::Uuid::new_v4();
    let mut consumer = root.watch(format!("/q/{}/c/{me}", p.queue), None).await.unwrap();
    let mut producer = signed_in(&url, &p.producer).await;
    producer.put(p.put_key("k1"), "pay 100", None).await.unwrap();
    assert_eq!(received(&mut root, &mut consumer, 400).await, None, "held: nothing to consume");
    let s = shown(&mut root, &p.state("k1")).await.unwrap();
    assert_eq!((s["need"].as_u64(), s["by"].as_str(), s["approved"].as_array().map(Vec::len), s["value"].as_str()),
        (Some(2), Some(p.producer.as_str()), Some(0), Some("pay 100")), "{s}");

    // by level: nothing, recommend 3, approve 4
    let mut out = signed_in(&url, &p.out).await;
    assert_eq!(code(out.put(p.state("k1"), "approve", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(code(out.put(p.state("k1"), "comment hello", None).await.unwrap_err()), tonic::Code::PermissionDenied, "no level: not even a comment");
    let mut rae = signed_in(&url, &p.rae).await;
    assert_eq!(code(rae.put(p.state("k1"), "approve", None).await.unwrap_err()), tonic::Code::PermissionDenied, "to recommend is not to approve");
    rae.put(p.state("k1"), "recommend", None).await.unwrap();
    rae.put(p.state("k1"), "comment the amount matches the order", None).await.unwrap();
    let mut ann = signed_in(&url, &p.ann).await;
    ann.put(p.state("k1"), "approve", None).await.unwrap();
    ann.put(p.state("k1"), "approve", None).await.unwrap();
    let s = shown(&mut root, &p.state("k1")).await.unwrap();
    assert_eq!(s["approved"], serde_json::json!([p.ann]), "the same account twice: once");
    assert_eq!((s["recommended"].clone(), s["comments"][0]["text"].as_str()), (serde_json::json!([p.rae]), Some("the amount matches the order")));
    assert_eq!(received(&mut root, &mut consumer, 300).await, None, "one of two");

    // a veto drops the voting; to approve is not to veto, nor to override
    assert_eq!(code(ann.put(p.state("k1"), "veto", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    let mut vic = signed_in(&url, &p.vic).await;
    vic.put(p.state("k1"), "veto", None).await.unwrap();
    let s = shown(&mut root, &p.state("k1")).await.unwrap();
    assert_eq!((s["vetoed_by"].as_str(), s["approved"].as_array().map(Vec::len)), (Some(p.vic.as_str()), Some(0)));
    let mut bob = signed_in(&url, &p.bob).await;
    assert_eq!(code(bob.put(p.state("k1"), "approve", None).await.unwrap_err()), tonic::Code::FailedPrecondition, "vetoed");
    assert_eq!(code(vic.put(p.state("k1"), "override", None).await.unwrap_err()), tonic::Code::PermissionDenied, "to veto is not to override");
    let mut ovi = signed_in(&url, &p.ovi).await;
    ovi.put(p.state("k1"), "override", None).await.unwrap();
    assert_eq!(code(ovi.put(p.state("k1"), "override", None).await.unwrap_err()), tonic::Code::FailedPrecondition, "nothing to lift now");

    // the voting starts again: two different accounts, and the message is the queue's
    ann.put(p.state("k1"), "approve", None).await.unwrap();
    assert_eq!(received(&mut root, &mut consumer, 300).await, None, "one of two again");
    bob.put(p.state("k1"), "approve", None).await.unwrap();
    assert_eq!(received(&mut root, &mut consumer, 5000).await.as_deref(), Some("pay 100"), "approved: consumed");
    assert_eq!(shown(&mut root, &p.state("k1")).await, None, "no state left");
    assert_eq!(code(ann.put(p.state("k1"), "approve", None).await.unwrap_err()), tonic::Code::NotFound, "nothing waits");

    // a queue with no definition is as it was: a message goes straight to its consumer
    let free = format!("free-{}", p.queue);
    let mut other = root.watch(format!("/q/{free}/c/{}", uuid::Uuid::new_v4()), None).await.unwrap();
    root.put(format!("/q/{free}/p/k"), "at once", None).await.unwrap();
    assert_eq!(received(&mut root, &mut other, 5000).await.as_deref(), Some("at once"));
}

/// A definition is root's; a message waits once under a key; its producer, or root, withdraws
/// it; a definition goes only when nothing waits - and then the queue is as any other
#[tokio::test]
async fn a_definition_and_a_withdrawal() {
    let Some(pool) = pool().await else { return };
    let (_n, url) = served(cfg("", free_port(), "")).await;
    let p = people(&pool).await;
    let mut root = signed_in(&url, &p.root).await;
    p.grant_producer(&mut root).await;
    let def = format!("/approval/{}", p.queue);
    let mut ann = signed_in(&url, &p.ann).await;
    assert_eq!(code(ann.put(def.as_str(), r#"{"need": 1}"#, None).await.unwrap_err()), tonic::Code::PermissionDenied, "root's to say");
    for bad in [r#"{"need": 0}"#, r#"{"need": 2, "alias": "a b"}"#, "two", r#"{"ned": 2}"#] {
        assert_eq!(code(root.put(def.as_str(), bad, None).await.unwrap_err()), tonic::Code::InvalidArgument, "{bad}");
    }
    root.put(def.as_str(), r#"{"need": 1}"#, None).await.unwrap();

    let mut producer = signed_in(&url, &p.producer).await;
    producer.put(p.put_key("k2"), "pay 200", None).await.unwrap();
    assert_eq!(code(producer.put(p.put_key("k2"), "pay 201", None).await.unwrap_err()), tonic::Code::AlreadyExists, "one waits under this key");
    assert_eq!(code(ann.put(p.state("k2"), "disapprove", None).await.unwrap_err()), tonic::Code::InvalidArgument, "no such vote");
    assert_eq!(code(ann.put(p.state("nope"), "approve", None).await.unwrap_err()), tonic::Code::NotFound);
    // an account that may not put the queue's messages cannot put one to wait either
    assert_eq!(code(ann.put(p.put_key("k9"), "x", None).await.unwrap_err()), tonic::Code::PermissionDenied);

    // no way around the count: a transaction, a put on a key of the queue that is the server's, a
    // put or a range delete in the approvals' own keys - root's too
    let t = etcd_client::Txn::new().and_then([etcd_client::TxnOp::put(p.put_key("kt"), "slipped in", None)]);
    assert_eq!(code(producer.txn(t).await.unwrap_err()), tonic::Code::PermissionDenied, "a txn into a gated queue");
    let t = etcd_client::Txn::new().and_then([etcd_client::TxnOp::put(p.state("k2"), "approve", None)]);
    assert_eq!(code(root.txn(t).await.unwrap_err()), tonic::Code::PermissionDenied, "a vote in a txn");
    assert_eq!(code(root.put(format!("/q/{}/1700000000000000/k2", p.queue), "delivered?", None).await.unwrap_err()), tonic::Code::PermissionDenied,
        "a delivery key is the server's");
    assert_eq!(code(root.put(format!("{}/more", p.state("k2")), "approve", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(code(root.delete("/approval/", Some(etcd_client::DeleteOptions::new().with_prefix())).await.unwrap_err()), tonic::Code::PermissionDenied,
        "the states are not swept away from under their records");
    assert_eq!(code(root.put("/approval/etcds-seq", r#"{"need": 1}"#, None).await.unwrap_err()), tonic::Code::InvalidArgument, "the server's own queue");
    assert!(shown(&mut root, &p.state("k2")).await.is_some(), "still waiting, untouched");

    assert_eq!(code(root.delete(def.as_str(), None).await.unwrap_err()), tonic::Code::FailedPrecondition, "a message waits");
    assert_eq!(code(ann.delete(p.state("k2"), None).await.unwrap_err()), tonic::Code::PermissionDenied, "not hers to withdraw");
    assert_eq!(producer.delete(p.state("k2"), None).await.unwrap().deleted(), 1);
    assert_eq!(shown(&mut root, &p.state("k2")).await, None);
    assert_eq!(code(ann.put(p.state("k2"), "approve", None).await.unwrap_err()), tonic::Code::NotFound, "withdrawn");

    // one approval where one is asked
    let me = uuid::Uuid::new_v4();
    let mut consumer = root.watch(format!("/q/{}/c/{me}", p.queue), None).await.unwrap();
    producer.put(p.put_key("k3"), "pay 300", None).await.unwrap();
    ann.put(p.state("k3"), "approve", None).await.unwrap();
    assert_eq!(received(&mut root, &mut consumer, 5000).await.as_deref(), Some("pay 300"));

    // the definition gone: the queue is as any other
    assert_eq!(root.delete(def.as_str(), None).await.unwrap().deleted(), 1);
    assert_eq!(shown(&mut root, &def).await, None);
    producer.put(p.put_key("k4"), "pay 400", None).await.unwrap();
    assert_eq!(received(&mut root, &mut consumer, 5000).await.as_deref(), Some("pay 400"), "not gated any more");
}

/// One node decides: a definition, a message and the votes put on the other node are the
/// leader's - sent on with the account's token - and the approved message reaches a consumer
#[tokio::test]
async fn a_follower_s_votes_are_the_leader_s() {
    let Some(pool) = pool().await else { return };
    let (pa, pb) = (free_port(), free_port());
    let (ua, ub) = (format!("http://127.0.0.1:{}", pa), format!("http://127.0.0.1:{}", pb));
    let (na, nb) = ("55555555-5555-5555-8555-5555555555a1", "66666666-6666-5666-8666-6666666666a1");
    let (a, _) = served(cfg(na, pa, "")).await;
    let (b, _) = served(cfg(nb, pb, &ua)).await;
    let both = format!("{},{}", ua, ub);
    a.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg(na, pa, &both)))).await.unwrap();
    b.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg(nb, pb, &both)))).await.unwrap();
    let p = people(&pool).await;
    let mut ra = signed_in(&ua, &p.root).await;
    let mut rb = signed_in(&ub, &p.root).await;
    for _ in 0..150 {
        let (sa, sb) = (ra.status().await.unwrap(), rb.status().await.unwrap());
        if sa.leader() != 0 && sa.leader() == sb.leader() && !ra.member_list().await.unwrap().members().is_empty()
            && !rb.member_list().await.unwrap().members().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let s = ra.status().await.unwrap();
    // the follower's url, and the leader's
    let (follower, leader) = if s.leader() == s.header().unwrap().member_id() { (ub.clone(), ua.clone()) } else { (ua.clone(), ub.clone()) };
    let mut root_f = signed_in(&follower, &p.root).await;
    let mut root_l = signed_in(&leader, &p.root).await;
    p.grant_producer(&mut root_f).await;
    root_f.put(format!("/approval/{}", p.queue), r#"{"need": 2}"#, None).await.unwrap();
    // the definition is every node's: the leader gates a put made there too
    let me = uuid::Uuid::new_v4();
    let mut consumer = root_l.watch(format!("/q/{}/c/{me}", p.queue), None).await.unwrap();
    let mut producer = signed_in(&follower, &p.producer).await;
    producer.put(p.put_key("k1"), "pay 100", None).await.unwrap();
    assert_eq!(received(&mut root_l, &mut consumer, 400).await, None, "held");
    // the state is an ordinary key: read on either node
    let mut seen = None;
    for _ in 0..50 {
        seen = shown(&mut root_l, &p.state("k1")).await;
        if seen.is_some() && shown(&mut root_f, &p.state("k1")).await.is_some() { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(seen.unwrap()["by"].as_str(), Some(p.producer.as_str()), "the producer's account went with its put");
    let mut out = signed_in(&follower, &p.out).await;
    assert_eq!(code(out.put(p.state("k1"), "approve", None).await.unwrap_err()), tonic::Code::PermissionDenied, "judged by the account, on the leader");
    let mut ann = signed_in(&follower, &p.ann).await;
    let mut bob = signed_in(&leader, &p.bob).await;
    ann.put(p.state("k1"), "approve", None).await.unwrap();
    bob.put(p.state("k1"), "approve", None).await.unwrap();
    assert_eq!(received(&mut root_l, &mut consumer, 8000).await.as_deref(), Some("pay 100"));
}
