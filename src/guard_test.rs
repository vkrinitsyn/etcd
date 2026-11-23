//! [auth] the peer proof and a txn's ranges, without a server
use super::*;
use crate::etcdpb::etcdserverpb::{request_op, Compare, DeleteRangeRequest, PutRequest, RangeRequest, RequestOp, TxnRequest};
use tonic::metadata::MetadataValue;

fn meta(pairs: &[(&'static str, &str)]) -> MetadataMap {
    let mut m = MetadataMap::new();
    for (k, v) in pairs {
        m.insert(*k, MetadataValue::try_from(*v).unwrap());
    }
    m
}

#[test]
fn a_secret_compares_whole() {
    assert!(same_secret(b"abc", b"abc"));
    assert!(!same_secret(b"abc", b"abd"));
    assert!(!same_secret(b"abc", b"ab"), "a prefix is not the secret");
    assert!(same_secret(b"", b""));
}

#[test]
fn a_call_is_a_peer_s_only_with_x_peer_and_the_cluster_s_token() {
    assert!(peer_proof(&meta(&[]), "t").is_none(), "a client's call");
    assert!(peer_proof(&meta(&[(crate::etcdpb::XPEER, "1")]), "").unwrap().is_ok(), "no token set: as before");
    assert!(peer_proof(&meta(&[(crate::etcdpb::XPEER, "1")]), "t").unwrap().is_err(), "x-peer alone, a token set");
    assert!(peer_proof(&meta(&[(crate::etcdpb::XPEER, "1"), (crate::coord::XPEER_TOKEN, "u")]), "t").unwrap().is_err());
    assert!(peer_proof(&meta(&[(crate::etcdpb::XPEER, "1"), (crate::coord::XPEER_TOKEN, "t")]), "t").unwrap().is_ok());
    assert!(peer_proof(&meta(&[(crate::coord::XPEER_TOKEN, "t")]), "t").is_none(), "the token without x-peer is a client's call");
}

#[test]
fn a_txn_reads_its_compares_and_ranges_and_writes_its_puts_and_deletes() {
    let op = |r| RequestOp { request: Some(r) };
    let inner = TxnRequest { success: vec![op(request_op::Request::RequestPut(PutRequest { key: b"/n".to_vec(), ..Default::default() }))], ..Default::default() };
    let t = TxnRequest {
        compare: vec![Compare { key: b"/c".to_vec(), range_end: b"/d".to_vec(), ..Default::default() }],
        success: vec![
            op(request_op::Request::RequestRange(RangeRequest { key: b"/r".to_vec(), ..Default::default() })),
            op(request_op::Request::RequestTxn(inner)),
        ],
        failure: vec![
            op(request_op::Request::RequestDeleteRange(DeleteRangeRequest { key: b"/x".to_vec(), range_end: b"/y".to_vec(), ..Default::default() })),
            op(request_op::Request::RequestPut(PutRequest { key: b"/p".to_vec(), prev_kv: true, ..Default::default() })),
        ],
    };
    let got = EtcdNode::txn_access(&t);
    assert_eq!(got, vec![
        (b"/c".to_vec(), b"/d".to_vec(), crate::coord::READ),
        (b"/r".to_vec(), vec![], crate::coord::READ),
        (b"/n".to_vec(), vec![], crate::coord::WRITE),
        (b"/x".to_vec(), b"/y".to_vec(), crate::coord::WRITE),
        (b"/p".to_vec(), vec![], crate::coord::WRITE),
        (b"/p".to_vec(), vec![], crate::coord::READ),
    ]);
}
