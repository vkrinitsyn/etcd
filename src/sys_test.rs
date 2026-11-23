//! [sys] the records read back, the blocks a sequence hands out
use super::*;

#[test]
fn hex_round_trips_and_refuses_what_is_not_hex() {
    assert_eq!(hex(b"/a\x00\xff"), "2f6100ff");
    assert_eq!(unhex("2f6100ff"), Some(b"/a\x00\xff".to_vec()));
    assert_eq!(unhex("2f6"), None, "odd");
    assert_eq!(unhex("zz"), None);
    assert_eq!(unhex(""), Some(vec![]));
}

#[test]
fn the_records_read_back_and_a_bad_one_is_skipped() {
    let lock = serde_json::to_string(&LockRec { key: hex(b"/l/2a"), lease: 42 }).unwrap();
    let all = [
        ("lease/42", r#"{"ttl":30}"#),
        ("lease/x", r#"{"ttl":30}"#),
        ("lease/7", "not json"),
        (lock_rec_key(b"/l").leak() as &str, lock.as_str()),
        ("seq/orders", r#"{"cache":10,"next":101}"#),
        ("other/thing", "{}"),
    ];
    let r = read_records(all.iter().map(|(k, v)| (*k, *v)));
    assert_eq!(r.leases, HashMap::from([(42, 30)]));
    assert_eq!(r.locks, HashMap::from([(b"/l".to_vec(), (b"/l/2a".to_vec(), 42))]));
    assert_eq!(r.seqs, HashMap::from([("orders".to_string(), SeqRec { cache: 10, next: 101, dropped: false })]));
}

#[test]
fn a_block_is_the_cache_or_the_count_asked_and_moves_the_mark_past_it() {
    let s = SeqRec { cache: 10, next: 101, dropped: false };
    assert_eq!(take_block(&s, 0), Some((101, 10, SeqRec { cache: 10, next: 111, dropped: false })));
    assert_eq!(take_block(&s, 3), Some((101, 3, SeqRec { cache: 10, next: 104, dropped: false })));
    assert_eq!(take_block(&SeqRec { cache: 0, next: 5, dropped: false }, 0), Some((5, 1, SeqRec { cache: 0, next: 6, dropped: false })), "a cache of 0 is 1");
    assert_eq!(take_block(&SeqRec { cache: 2, next: i64::MAX - 1, dropped: false }, 0), None, "past the end");
}

/// [review 2026-11-21] a peer keeps the newest version: a late put of an older one, a
/// tombstone's older put, change nothing
#[test]
fn the_newest_version_stands() {
    let mut m = BTreeMap::new();
    let r = |v: u64, value: &str, deleted: bool| Versioned { b: (0, 0), v, value: value.into(), deleted };
    assert!(apply_newer(&mut m, "seq/a", r(5, "five", false)));
    assert!(!apply_newer(&mut m, "seq/a", r(4, "four", false)), "older: no put");
    assert!(!apply_newer(&mut m, "seq/a", r(5, "again", false)), "the same version: kept");
    assert!(apply_newer(&mut m, "seq/a", r(7, "", true)), "a newer delete");
    assert!(!apply_newer(&mut m, "seq/a", r(6, "six", false)), "nothing older comes back over the tombstone");
    assert_eq!(m["seq/a"], r(7, "", true));
}

#[test]
fn a_majority_is_more_than_half() {
    assert_eq!((majority(1), majority(2), majority(3), majority(4), majority(5)), (1, 2, 2, 3, 3));
}

#[test]
fn a_dropped_record_reads_with_its_mark() {
    let all = [("seq/old", r#"{"cache":5,"next":60,"dropped":true}"#), ("seq/new", r#"{"cache":5,"next":60}"#)];
    let r = read_records(all.iter().map(|(k, v)| (*k, *v)));
    assert!(r.seqs["old"].dropped);
    assert!(!r.seqs["new"].dropped, "a record from before the field: not dropped");
}

/// [review 2026-11-21, 2] A later leader's record stands over one a superseded leader wrote,
/// whatever its version; within one ballot the version decides
#[test]
fn a_later_ballot_stands_over_a_higher_version() {
    let mut m = BTreeMap::new();
    let r = |b: Ballot, v: u64, value: &str| Versioned { b, v, value: value.into(), deleted: false };
    assert!(apply_newer(&mut m, "seq/a", r((1, 7), 9, "old leader")));
    assert!(apply_newer(&mut m, "seq/a", r((2, 3), 1, "new leader")), "a later round");
    assert!(!apply_newer(&mut m, "seq/a", r((1, 7), 10, "late")), "the superseded one's later write");
    assert!(apply_newer(&mut m, "seq/a", r((2, 5), 1, "same round, higher node")));
    assert!(apply_newer(&mut m, "seq/a", r((2, 5), 2, "next")));
    assert_eq!(m["seq/a"].value, "next");
    assert_eq!(parse_ballot(&ballot_text((12, 99))), Some((12, 99)));
    assert_eq!(parse_ballot("12"), None);
    assert_eq!(parse_ballot("a:1"), None);
}

/// [review 2026-11-21, 2] The sequences' own queues, either spelling, and the reply queues
/// the leader answers on
#[test]
fn the_sequences_queues_are_reserved_and_answers_go_to_reply_queues_only() {
    for k in ["/q/etcds-seq/p/1", "/q/etcds-seq/c/x", "/queue/etcds-seq/producer/1", "/q/etcds-seqdef/p/1",
        "/queue/etcds-seqdef/consumer/x", "/q/etcds-seq-node-1f/p/r", "/queue/etcds-seq-node-1f/consumer/x"] {
        assert!(reserved_queue(k.as_bytes()), "{k}");
    }
    for k in ["/q/etcds-seq-mine/c/x", "/q/etcds-sequel/p/1", "/q/other/p/1", "/.etcds/sys/seq/a", "/q/etcds-seq"] {
        assert!(!reserved_queue(k.as_bytes()), "{k}");
    }
    assert!(is_seq_key(b"/q/etcds-seq/p/1") && is_seq_key(b"/queue/etcds-seq/producer/1"));
    assert!(!is_seq_key(b"/q/etcds-seqdef/p/1"));
    for ok in ["etcds-seq-mine", "etcds-seq-node-1f"] {
        assert!(reply_queue_ok(ok), "{ok}");
    }
    for bad in ["etcds-seq", "etcds-seqdef", "etcds-seq-", "victim", "etcds-seq-a/b", ""] {
        assert!(!reply_queue_ok(bad), "{bad}");
    }
}

mod node {
    use super::super::*;
    use crate::queue::node_tests::{node, until};

    const A: &str = "33333333-3333-5333-8333-333333333331";
    const B: &str = "33333333-3333-5333-8333-333333333332";

    /// [review 2026-11-21, 2] A node answering a takeover's range promises its ballot: the
    /// same or a lower one is refused, naming the promised one, and so is a record a lower
    /// ballot wrote
    #[tokio::test]
    async fn a_peer_promises_a_takeover_and_refuses_what_a_lower_ballot_sends() {
        let n = node(A).await;
        {
            let mut cfg = n.cfg.write().await;
            cfg.peer_token = "t".into();
            cfg.leader = B.into();
        }
        assert!(n.sys_range_for_peer(Some((5, 1))).await.is_ok());
        for b in [(5, 1), (4, 9)] {
            let e = n.sys_range_for_peer(Some(b)).await.unwrap_err();
            assert_eq!(e.code(), tonic::Code::FailedPrecondition);
            assert_eq!(e.metadata().get(XBALLOT).and_then(|v| v.to_str().ok()), Some("5:1"));
        }
        assert!(n.sys_range_for_peer(None).await.is_ok(), "a joining node's read promises nothing");
        let wire = |r: u64, n: u64, value: &str| serde_json::to_vec(&Wire { r, n, v: 1, value: value.into(), deleted: false }).unwrap();
        let e = n.sys_put_from_peer("seq/s", &wire(4, 9, "old"), None).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition, "{e:?}");
        n.sys_put_from_peer("seq/s", &wire(5, 1, "new"), None).await.unwrap();
        assert_eq!(n.sys.get("seq/s").await.as_deref(), Some("new"));
    }

    /// [review 2026-11-21, 2] A leader that promised a later takeover writes nothing and
    /// leads no more until it takes over again - above the round it was told of
    #[tokio::test]
    async fn a_superseded_leader_writes_nothing_and_takes_over_again() {
        let n = node(A).await;
        n.seq_define(SeqDefine { name: "s".into(), create: true, ..Default::default() }).await.unwrap();
        let mine = n.sys.ballot();
        assert!(mine.0 >= 1 && mine.1 == n.node_id, "{mine:?}");
        *n.sys.promised.lock().unwrap() = (mine.0 + 5, 0);
        let e = n.sys_write("seq/s", Some("{}".into())).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unavailable);
        assert!(!n.coord.was_leader.load(std::sync::atomic::Ordering::Relaxed));
        // the next call takes over again, under a ballot above the promised one
        assert_eq!(n.seq_next("s").await.unwrap(), 1);
        assert!(n.sys.ballot() > (mine.0 + 5, 0), "{:?}", n.sys.ballot());
    }

    /// [review 2026-11-21, 2] A majority of every node configured, not of the peers held
    #[tokio::test]
    async fn a_majority_counts_the_configured_peers_too() {
        let n = node(A).await;
        n.cfg.write().await.initial_advertise_peer_urls = "http://127.0.0.1:1,http://127.0.0.1:2".into();
        assert_eq!(n.members(0).await, 3);
        assert_eq!(n.members(4).await, 5);
        let e = n.seq_define(SeqDefine { name: "s".into(), create: true, ..Default::default() }).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::Unavailable, "alone of three: no takeover - {e:?}");
    }

    /// [review 2026-11-21, 2] The leader's consumer stopped, its watches go too, though no
    /// message comes; a consumer started again keeps its slot when the old one ends
    #[tokio::test]
    async fn a_stopped_consumer_leaves_no_watch_and_a_new_one_keeps_its_slot() {
        let n = node(A).await;
        let base = n.watchers.read().await.len();
        n.seq_consumer_start().await;
        assert!(until(|| async { n.watchers.read().await.len() >= base + 2 }).await, "two watches");
        n.seq_consumer_stop().await;
        n.seq_consumer_start().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(n.sys.consumer.lock().await.is_some(), "the new consumer's slot");
        n.seq_consumer_stop().await;
        assert!(until(|| async { n.watchers.read().await.len() <= base }).await, "every watch gone");
    }
}
