//! [coord] Lease and Lock as an etcd v3 client sees them (owner, 2026-11-21): on one
//! node, then on two peered nodes - a call on the node that is not the leader sent on
//! to it, the leases on both, and a leader that changes taking them over.
mod testing;

use std::time::Duration;

use crate::testing::{start_server, Result};
use etcd_client::{Client, Error, LeaseGrantOptions, LeaseTimeToLiveOptions, LockOptions, PutOptions};
use etcds::cluster::EtcdNode;

fn code(e: Error) -> tonic::Code {
    match e {
        Error::GRpcStatus(s) => s.code(),
        other => panic!("expected a gRPC status, got {:?}", other),
    }
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
async fn a_lease_lives_while_kept_alive_and_takes_its_keys_with_it() -> Result<()> {
    let (_node, mut c) = start_server().await;
    let g = c.lease_grant(2, None).await?;
    assert!(g.id() > 0);
    assert_eq!(g.ttl(), 5, "below etcd's minimum: the minimum");
    let fixed = c.lease_grant(30, Some(LeaseGrantOptions::new().with_id(4242))).await?;
    assert_eq!(fixed.id(), 4242, "an id asked for");
    assert_eq!(code(c.lease_grant(30, Some(LeaseGrantOptions::new().with_id(4242))).await.unwrap_err()), tonic::Code::FailedPrecondition, "taken");

    c.put("/l/a", "1", Some(PutOptions::new().with_lease(g.id()))).await?;
    c.put("/l/b", "2", Some(PutOptions::new().with_lease(g.id()))).await?;
    c.put("/l/c", "3", None).await?;
    let ttl = c.lease_time_to_live(g.id(), Some(LeaseTimeToLiveOptions::new().with_keys())).await?;
    assert_eq!(ttl.granted_ttl(), 5);
    assert!(ttl.ttl() > 0 && ttl.ttl() <= 5, "{}", ttl.ttl());
    assert_eq!(ttl.keys(), [b"/l/a".to_vec(), b"/l/b".to_vec()]);
    let ids: Vec<i64> = c.leases().await?.leases().iter().map(|l| l.id()).collect();
    assert!(ids.contains(&g.id()) && ids.contains(&4242));

    let (mut keeper, mut answers) = c.lease_keep_alive(g.id()).await?;
    keeper.keep_alive().await?;
    let a = answers.message().await?.unwrap();
    assert_eq!((a.id(), a.ttl()), (g.id(), 5));

    // ignore_lease keeps the key's own lease
    c.put("/l/a", "1b", Some(PutOptions::new().with_ignore_lease())).await?;
    c.lease_revoke(g.id()).await?;
    assert_eq!(c.get("/l/a", None).await?.count(), 0, "its keys went with it");
    assert_eq!(c.get("/l/b", None).await?.count(), 0);
    assert_eq!(c.get("/l/c", None).await?.count(), 1, "a key with no lease stays");
    assert_eq!(c.lease_time_to_live(g.id(), None).await?.ttl(), -1, "gone, as etcd says it");
    assert_eq!(code(c.lease_revoke(g.id()).await.unwrap_err()), tonic::Code::NotFound);
    keeper.keep_alive().await?;
    assert_eq!(answers.message().await?.unwrap().ttl(), 0, "a gone lease is kept alive at 0");
    Ok(())
}

#[tokio::test]
async fn a_put_with_a_lease_nobody_granted_is_refused() -> Result<()> {
    let (_node, mut c) = start_server().await;
    let e = c.put("/x", "v", Some(PutOptions::new().with_lease(777))).await.unwrap_err();
    assert_eq!(code(e), tonic::Code::NotFound);
    assert_eq!(c.get("/x", None).await?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn a_lease_not_kept_alive_expires_with_its_keys() -> Result<()> {
    let (_node, mut c) = start_server().await;
    let g = c.lease_grant(5, None).await?;
    c.put("/exp/k", "v", Some(PutOptions::new().with_lease(g.id()))).await?;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(c.get("/exp/k", None).await?.count(), 1, "not yet");
    let mut cc = c.clone();
    until("the lease's expiry", 4, || { let mut c = cc.clone(); async move { c.get("/exp/k", None).await.unwrap().count() == 0 } }).await;
    assert_eq!(cc.lease_time_to_live(g.id(), None).await?.ttl(), -1);
    Ok(())
}

#[tokio::test]
async fn a_lock_is_one_holder_s_until_it_is_unlocked_or_its_lease_ends() -> Result<()> {
    let (_node, mut c) = start_server().await;
    let l1 = c.lease_grant(30, None).await?.id();
    let l2 = c.lease_grant(30, None).await?.id();
    let held = c.lock("/locks/a", Some(LockOptions::new().with_lease(l1))).await?;
    assert_eq!(held.key(), format!("/locks/a/{:x}", l1).as_bytes(), "etcd's own layout");
    assert_eq!(c.get(held.key(), None).await?.kvs()[0].lease(), l1, "the holder's key, with its lease");

    // the second waits while the first holds
    let mut c2 = c.clone();
    let waiting = tokio::spawn(async move { c2.lock("/locks/a", Some(LockOptions::new().with_lease(l2))).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!waiting.is_finished(), "held: the second waits");
    c.unlock(held.key()).await?;
    let second = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("granted once unlocked").unwrap()?;
    assert_eq!(second.key(), format!("/locks/a/{:x}", l2).as_bytes());
    assert_eq!(c.get(held.key(), None).await?.count(), 0, "the first's key is gone");

    // the holder's lease ending frees the lock
    let l3 = c.lease_grant(30, None).await?.id();
    let mut c3 = c.clone();
    let third = tokio::spawn(async move { c3.lock("/locks/a", Some(LockOptions::new().with_lease(l3))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.lease_revoke(l2).await?;
    let got = tokio::time::timeout(Duration::from_secs(5), third).await.expect("granted once the lease went").unwrap()?;
    assert_eq!(got.key(), format!("/locks/a/{:x}", l3).as_bytes());
    // other names do not wait
    let other = tokio::time::timeout(Duration::from_secs(2), c.lock("/locks/b", Some(LockOptions::new().with_lease(l1)))).await.expect("another name is free")?;
    assert!(other.key().starts_with(b"/locks/b/"));
    Ok(())
}

#[tokio::test]
async fn a_lock_with_no_lease_or_a_gone_one_is_refused_and_a_waiter_that_leaves_holds_nobody_up() -> Result<()> {
    let (_node, mut c) = start_server().await;
    assert_eq!(code(c.lock("/z", Some(LockOptions::new().with_lease(12345))).await.unwrap_err()), tonic::Code::NotFound);
    let l1 = c.lease_grant(30, None).await?.id();
    let l2 = c.lease_grant(30, None).await?.id();
    let l3 = c.lease_grant(30, None).await?.id();
    let held = c.lock("/q", Some(LockOptions::new().with_lease(l1))).await?;
    // a waiter whose client gives up
    let mut c2 = c.clone();
    let quitter = tokio::spawn(async move { c2.lock("/q", Some(LockOptions::new().with_lease(l2))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    quitter.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut c3 = c.clone();
    let next = tokio::spawn(async move { c3.lock("/q", Some(LockOptions::new().with_lease(l3))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.unlock(held.key()).await?;
    let got = tokio::time::timeout(Duration::from_secs(5), next).await.expect("the one that left is skipped").unwrap()?;
    assert_eq!(got.key(), format!("/q/{:x}", l3).as_bytes());
    Ok(())
}

// ---- two nodes -------------------------------------------------------------------

const CLUSTER: &str = "44444444-4444-5444-8444-444444444444";

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

/// Two served nodes, each holding the other Online, and a client of each
async fn two() -> (EtcdNode, EtcdNode, Client, Client) {
    let (pa, pb) = (free_port(), free_port());
    let (ua, ub) = (format!("http://127.0.0.1:{}", pa), format!("http://127.0.0.1:{}", pb));
    let a = served(cfg("55555555-5555-5555-8555-555555555555", pa, "")).await;
    let b = served(cfg("66666666-6666-5666-8666-666666666666", pb, &ua)).await;
    // the embedder's way: the whole roster to each node
    let both = format!("{},{}", ua, ub);
    a.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg("55555555-5555-5555-8555-555555555555", pa, &both)))).await.unwrap();
    b.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(cfg("66666666-6666-5666-8666-666666666666", pb, &both)))).await.unwrap();
    let mut ca = Client::connect([ua.as_str()], None).await.unwrap();
    let mut cb = Client::connect([ub.as_str()], None).await.unwrap();
    for c in [&mut ca, &mut cb] {
        let cc = c.clone();
        until("each holding the other", 15, || {
            let mut c = cc.clone();
            async move { !c.member_list().await.unwrap().members().is_empty() }
        }).await;
    }
    until("one leader", 5, || {
        let (mut a, mut b) = (ca.clone(), cb.clone());
        async move { a.status().await.unwrap().leader() == b.status().await.unwrap().leader() }
    }).await;
    (a, b, ca, cb)
}

/// the client of the node that leads, then of the other
async fn by_role(ca: &Client, cb: &Client) -> (Client, Client) {
    let mut a = ca.clone();
    let s = a.status().await.unwrap();
    if s.leader() == s.header().unwrap().member_id() { (ca.clone(), cb.clone()) } else { (cb.clone(), ca.clone()) }
}

#[tokio::test]
async fn a_follower_s_lease_and_lock_calls_are_the_leader_s() -> Result<()> {
    let (_a, _b, ca, cb) = two().await;
    let (mut leader, mut follower) = by_role(&ca, &cb).await;
    // granted through the follower: decided on the leader, held on both
    let g = follower.lease_grant(30, None).await?;
    assert!(leader.leases().await?.leases().iter().any(|l| l.id() == g.id()), "replicated before the grant answered");
    follower.put("/two/k", "v", Some(PutOptions::new().with_lease(g.id()))).await?;
    until("the key on the leader", 5, || { let mut l = leader.clone(); async move { l.get("/two/k", None).await.unwrap().count() == 1 } }).await;
    let ttl = follower.lease_time_to_live(g.id(), Some(LeaseTimeToLiveOptions::new().with_keys())).await?;
    assert_eq!(ttl.keys(), [b"/two/k".to_vec()]);
    let (mut keeper, mut answers) = follower.lease_keep_alive(g.id()).await?;
    keeper.keep_alive().await?;
    assert_eq!(answers.message().await?.unwrap().ttl(), 30, "kept alive through the follower");

    // a lock taken through the follower holds against one through the leader
    let l2 = leader.lease_grant(30, None).await?.id();
    let held = follower.lock("/two/lock", Some(LockOptions::new().with_lease(g.id()))).await?;
    let mut lc = leader.clone();
    let waiting = tokio::spawn(async move { lc.lock("/two/lock", Some(LockOptions::new().with_lease(l2))).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished(), "one holder across the nodes");
    follower.unlock(held.key()).await?;
    tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("granted once unlocked").unwrap()?;

    // revoked through the follower: its keys gone on both, the lease on neither
    follower.lease_revoke(g.id()).await?;
    for c in [&leader, &follower] {
        let mut cc = c.clone();
        until("the leased key gone everywhere", 5, || { let mut c = cc.clone(); async move { c.get("/two/k", None).await.unwrap().count() == 0 } }).await;
        assert!(!cc.leases().await?.leases().iter().any(|l| l.id() == g.id()));
    }
    Ok(())
}

#[tokio::test]
async fn a_new_leader_takes_the_leases_and_the_lock_holders_over() -> Result<()> {
    let (a, b, ca, cb) = two().await;
    let (mut leader, mut follower) = by_role(&ca, &cb).await;
    let g = leader.lease_grant(30, None).await?.id();
    let held = leader.lock("/over/lock", Some(LockOptions::new().with_lease(g))).await?;
    until("the holder's key on the follower", 5, || { let mut f = follower.clone(); let k = held.key().to_vec(); async move { f.get(k, None).await.unwrap().count() == 1 } }).await;

    // the embedder names the other node the leader
    let follower_id = follower.status().await?.header().unwrap().member_id();
    let (fa, fb) = (a.clone(), b.clone());
    let uuid = if follower_id == member_of(&fa).await { "55555555-5555-5555-8555-555555555555" } else { "66666666-6666-5666-8666-666666666666" };
    for n in [&fa, &fb] {
        let mut conf = n_cfg(n).await;
        conf.leader = uuid.to_string();
        n.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(conf))).await.unwrap();
    }
    until("the named leader", 5, || { let mut f = follower.clone(); async move { let s = f.status().await.unwrap(); s.leader() == s.header().unwrap().member_id() } }).await;
    // the lease lives on with a full TTL, and its lock is still held
    let ttl = follower.lease_time_to_live(g, None).await?;
    assert!(ttl.ttl() > 20, "a fresh full TTL: {}", ttl.ttl());
    let mut fc = follower.clone();
    let l2 = fc.lease_grant(30, None).await?.id();
    let waiting = tokio::spawn(async move { fc.lock("/over/lock", Some(LockOptions::new().with_lease(l2))).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished(), "the new leader knows the holder");
    leader.unlock(held.key()).await?;
    tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("granted by the new leader").unwrap()?;
    Ok(())
}

/// a node's member id, as its status says it
async fn member_of(n: &EtcdNode) -> u64 {
    n.member_id()
}

/// the config a node runs with, for a reconfigure that changes one thing
async fn n_cfg(n: &EtcdNode) -> etcds::cli::EtcdConfig {
    n.config().await
}

#[tokio::test]
async fn a_waiter_whose_lease_ends_is_told_and_leaves_the_queue() -> Result<()> {
    let (_node, mut c) = start_server().await;
    let l1 = c.lease_grant(30, None).await?.id();
    let l2 = c.lease_grant(30, None).await?.id();
    let held = c.lock("/w8", Some(LockOptions::new().with_lease(l1))).await?;
    let mut c2 = c.clone();
    let waiting = tokio::spawn(async move { c2.lock("/w8", Some(LockOptions::new().with_lease(l2))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.lease_revoke(l2).await?;
    let e = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("told at once").unwrap().unwrap_err();
    assert_eq!(code(e), tonic::Code::NotFound, "its lease is gone");
    // the holder is untouched, and the queue is free behind it
    assert_eq!(c.get(held.key(), None).await?.count(), 1);
    c.unlock(held.key()).await?;
    let l3 = c.lease_grant(30, None).await?.id();
    tokio::time::timeout(Duration::from_secs(2), c.lock("/w8", Some(LockOptions::new().with_lease(l3)))).await.expect("nobody left waiting")?;
    Ok(())
}

/// [review 2026-11-21] a call passing for a peer cannot rewrite the leader's leases, and a
/// leader that is no uuid is refused at start and kept out at a reconfigure
#[tokio::test]
async fn the_leader_keeps_its_leases_and_its_name() -> Result<()> {
    use etcds::etcdpb::etcdserverpb::lease_client::LeaseClient;
    use etcds::etcdpb::etcdserverpb::{LeaseGrantRequest, LeaseRevokeRequest};
    let (node, mut c, url) = crate::testing::start_server_at().await;
    let id = c.lease_grant(30, None).await?.id();
    let mut l = LeaseClient::connect(url).await.unwrap();
    let mut forged = tonic::Request::new(LeaseRevokeRequest { id });
    forged.metadata_mut().insert(etcds::etcdpb::XPEER, "1".parse().unwrap());
    forged.metadata_mut().insert("x-etcds-rep", "1".parse().unwrap());
    assert_eq!(l.lease_revoke(forged).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
    let mut grant = tonic::Request::new(LeaseGrantRequest { id: 99, ttl: 1 });
    grant.metadata_mut().insert(etcds::etcdpb::XPEER, "1".parse().unwrap());
    grant.metadata_mut().insert("x-etcds-rep", "1".parse().unwrap());
    assert_eq!(l.lease_grant(grant).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
    assert!(c.leases().await?.leases().iter().any(|x| x.id() == id), "the lease stands");

    let mut bad = node.config().await;
    bad.leader = "not a uuid".to_string();
    node.event.send(etcds::EtcdEvents::Mgmt(etcds::EtcdMgmtEvent::Config(bad.clone()))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(node.config().await.leader, "", "the leader there was");
    let e = EtcdNode::init(bad, slog::Logger::root(slog::Discard, slog::o!()), #[cfg(feature = "tracer")] None).await.err().unwrap();
    assert!(e.contains("not a uuid"), "{e}");
    Ok(())
}
