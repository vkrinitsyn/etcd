//! [embed] the embedder's leases and locks, in process
use super::*;
use crate::queue::node_tests::node;

const A: &str = "44444444-4444-5444-8444-444444444441";

/// [embed] A lease granted, kept and revoked in process; a lock taken on it, refused to a
/// try while held, taken again by the same lease at once, freed by its key
#[tokio::test]
async fn an_embedder_takes_leases_and_locks_in_process() {
    let n = node(A).await;
    let l1 = n.embed_lease_grant(30).await.unwrap();
    let l2 = n.embed_lease_grant(30).await.unwrap();
    assert_eq!(n.embed_lease_keep(l1).await.unwrap(), 30);
    let key = n.embed_lock(b"/.yt/advisory/db/1", l1, false).await.unwrap().unwrap();
    assert_eq!(key, lock_holder_key(b"/.yt/advisory/db/1", l1));
    assert_eq!(n.embed_lock(b"/.yt/advisory/db/1", l2, true).await.unwrap(), None, "held: a try is refused");
    assert_eq!(n.embed_lock(b"/.yt/advisory/db/1", l1, true).await.unwrap(), Some(key.clone()), "the same lease: the one acquisition");
    n.embed_unlock(&key).await.unwrap();
    let k2 = n.embed_lock(b"/.yt/advisory/db/1", l2, true).await.unwrap();
    assert_eq!(k2, Some(lock_holder_key(b"/.yt/advisory/db/1", l2)), "free: a try takes it");
    // the lease's end frees its lock
    n.embed_lease_revoke(l2).await.unwrap();
    n.embed_lease_revoke(l2).await.unwrap();
    assert_eq!(n.embed_lease_keep(l2).await.unwrap(), 0, "gone");
    assert!(n.embed_lock(b"/.yt/advisory/db/1", l1, true).await.unwrap().is_some());
    assert_eq!(n.lock_ttl().await, 60);
}

/// [embed] A try waits behind no one: a lock someone waits for is refused to it too
#[tokio::test]
async fn a_try_does_not_jump_the_queue() {
    let n = node(A).await;
    let (l1, l2, l3) = (n.embed_lease_grant(30).await.unwrap(), n.embed_lease_grant(30).await.unwrap(), n.embed_lease_grant(30).await.unwrap());
    let key = n.embed_lock(b"q", l1, false).await.unwrap().unwrap();
    let waiter = { let n = n.clone(); tokio::spawn(async move { n.embed_lock(b"q", l2, false).await }) };
    tokio::time::sleep(Duration::from_millis(200)).await;
    n.embed_unlock(&key).await.unwrap();
    assert_eq!(n.embed_lock(b"q", l3, true).await.unwrap(), None, "the waiter's, not the try's");
    assert!(waiter.await.unwrap().unwrap().is_some());
}

/// [embed] A block forgotten: the next number comes from the leader, past an `at_least`
#[tokio::test]
async fn a_forgotten_block_gives_way_to_the_leaders_next() {
    let n = node(A).await;
    n.seq_define(crate::sys::SeqDefine { name: "db.public.s".into(), cache: 10, create: true, ..Default::default() }).await.unwrap();
    assert_eq!(n.seq_next("db.public.s").await.unwrap(), 1);
    n.seq_define(crate::sys::SeqDefine { name: "db.public.s".into(), at_least: 500, ..Default::default() }).await.unwrap();
    assert_eq!(n.seq_next("db.public.s").await.unwrap(), 2, "the block held yet");
    n.seq_forget_block("db.public.s").await;
    assert_eq!(n.seq_next("db.public.s").await.unwrap(), 500);
}

/// [embed] The embedder's leader over the configured one, kept across a reconfigure; a
/// uuid that does not read refused
#[tokio::test]
async fn the_embedders_leader_stands_over_the_configured_one() {
    const B: &str = "44444444-4444-5444-8444-444444444442";
    let n = node(A).await;
    assert!(n.is_leader().await, "alone: itself");
    assert_eq!(n.set_leader(B).await, Ok(true));
    assert_eq!(n.set_leader(B).await, Ok(false), "the same: no change");
    assert!(!n.is_leader().await, "B named");
    n.cfg.write().await.leader = A.into();
    assert!(!n.is_leader().await, "the embedder's over the configured one");
    assert!(n.set_leader("not a uuid").await.is_err());
    assert!(!n.is_leader().await, "kept");
    assert_eq!(n.set_leader("").await, Ok(true));
    assert!(n.is_leader().await, "the configured one again");
}

/// [ycoord] A drop answers the mark, and a drop of one dropped the mark again; one that never
/// was is not found
#[tokio::test]
async fn a_drop_answers_the_mark_each_time() {
    let n = node(A).await;
    n.seq_define(crate::sys::SeqDefine { name: "d".into(), create: true, start: 10, ..Default::default() }).await.unwrap();
    assert_eq!(n.seq_next("d").await.unwrap(), 10);
    let drop = || crate::sys::SeqDefine { name: "d".into(), drop: true, ..Default::default() };
    assert_eq!(n.seq_define(drop()).await.unwrap().next, 11);
    let again = n.seq_define(drop()).await.unwrap();
    assert_eq!((again.next, again.exists), (11, false));
    let none = crate::sys::SeqDefine { name: "never".into(), drop: true, ..Default::default() };
    assert_eq!(n.seq_define(none).await.unwrap_err().code(), tonic::Code::NotFound);
}
