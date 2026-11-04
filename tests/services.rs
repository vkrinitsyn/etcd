#![allow(dead_code)]
//! The service surface as agent.md's "Supported API" table describes it, seen
//! by an etcd v3 client: what answers, what answers `unimplemented`, and what
//! answers and does nothing.
mod testing;

use crate::testing::{start_server, start_server_at, Result};
use etcd_client::{
    AlarmAction, AlarmType, Compare, CompareOp, DeleteOptions, Error, GetOptions, Permission,
    PutOptions, Txn, TxnOp, TxnOpResponse,
};

/// A gRPC status code out of a client error, or a panic naming what came back.
fn code(e: Error) -> tonic::Code {
    match e {
        Error::GRpcStatus(s) => s.code(),
        other => panic!("expected a gRPC status, got {:?}", other),
    }
}

fn unimplemented<T: std::fmt::Debug>(r: std::result::Result<T, Error>, what: &str) {
    match r {
        Ok(v) => panic!("{} answered {:?}, but the table says unimplemented", what, v),
        Err(e) => assert_eq!(code(e), tonic::Code::Unimplemented, "{}", what),
    }
}

/// [proto] What the official API added over this crate's old copy, seen by a client of the
/// generated code: RangeStream is the range in one chunk, under the same limit and the same
/// refusal as Range; Downgrade and AuthStatus are there and answer `unimplemented`; a member
/// list may ask to be linearizable; HashKV has no revision to say.
#[tokio::test]
async fn what_the_official_api_added() -> Result<()> {
    use etcds::etcdpb::etcdserverpb::auth_client::AuthClient;
    use etcds::etcdpb::etcdserverpb::cluster_client::ClusterClient;
    use etcds::etcdpb::etcdserverpb::kv_client::KvClient;
    use etcds::etcdpb::etcdserverpb::maintenance_client::MaintenanceClient;
    use etcds::etcdpb::etcdserverpb::{AuthStatusRequest, DowngradeRequest, HashKvRequest, MemberListRequest, RangeRequest};

    let (_node, mut client, url) = start_server_at().await;
    client.put("/s/a", "1", None).await?;
    client.put("/s/b", "2", None).await?;
    client.put("/s/c", "3", None).await?;

    let mut kv = KvClient::connect(url.clone()).await.unwrap();
    let req = RangeRequest { key: b"/s/".to_vec(), range_end: b"/s0".to_vec(), limit: 2, ..Default::default() };
    let whole = kv.range(req.clone()).await.unwrap().into_inner();
    assert_eq!(whole.kvs.len(), 2);
    let mut stream = kv.range_stream(req).await.unwrap().into_inner();
    let chunk = stream.message().await.unwrap().unwrap().range_response.unwrap();
    assert_eq!(chunk.kvs, whole.kvs, "what a client merges is the same answer");
    assert_eq!(chunk.count, whole.count);
    assert!(stream.message().await.unwrap().is_none(), "one chunk");

    let down = MaintenanceClient::connect(url.clone()).await.unwrap().downgrade(DowngradeRequest::default()).await.unwrap_err();
    assert_eq!(down.code(), tonic::Code::Unimplemented);
    let auth = AuthClient::connect(url.clone()).await.unwrap().auth_status(AuthStatusRequest::default()).await.unwrap_err();
    assert_eq!(auth.code(), tonic::Code::Unimplemented);

    let members = ClusterClient::connect(url.clone()).await.unwrap()
        .member_list(MemberListRequest { linearizable: true }).await.unwrap().into_inner();
    assert!(members.members.is_empty(), "a node alone lists no members, asked either way");
    let hash = MaintenanceClient::connect(url).await.unwrap().hash_kv(HashKvRequest::default()).await.unwrap().into_inner();
    assert_eq!(hash.hash_revision, 0, "etcds has no revisions");
    Ok(())
}

/// [proto] Every call of every service of the official API, through the generated clients: an
/// answer for each, as the table says, on one connection - nothing missing from a service
/// a client of the official files calls, and the node still serving after them all.
#[tokio::test]
async fn every_call_of_every_service_is_answered() -> Result<()> {
    use etcds::etcdpb::etcdserverpb as pb;
    use etcds::etcdpb::etcdserverpb::auth_client::AuthClient;
    use etcds::etcdpb::etcdserverpb::cluster_client::ClusterClient;
    use etcds::etcdpb::etcdserverpb::kv_client::KvClient;
    use etcds::etcdpb::etcdserverpb::lease_client::LeaseClient;
    use etcds::etcdpb::etcdserverpb::maintenance_client::MaintenanceClient;
    use etcds::etcdpb::etcdserverpb::watch_client::WatchClient;
    use etcds::etcdpb::v3electionpb as el;
    use etcds::etcdpb::v3electionpb::election_client::ElectionClient;
    use etcds::etcdpb::v3lockpb as lk;
    use etcds::etcdpb::v3lockpb::lock_client::LockClient;
    use tonic::Code::{Ok as Answered, Unimplemented};

    let (_node, _client, url) = start_server_at().await;
    let conn = tonic::transport::Channel::from_shared(url).unwrap().connect().await.unwrap();
    fn got<T>(r: std::result::Result<tonic::Response<T>, tonic::Status>) -> tonic::Code {
        r.map_or_else(|s| s.code(), |_| tonic::Code::Ok)
    }

    // each client as an embedder may build it: the limits set, and over a channel of its own
    let mut kv = KvClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    let key = |k: &str| k.as_bytes().to_vec();
    assert_eq!(got(kv.put(pb::PutRequest { key: key("/all/a"), value: key("1"), ..Default::default() }).await), Answered);
    assert_eq!(got(kv.range(pb::RangeRequest { key: key("/all/a"), ..Default::default() }).await), Answered);
    let mut chunks = kv.range_stream(pb::RangeRequest { key: key("/all/a"), ..Default::default() }).await.unwrap().into_inner();
    assert_eq!(chunks.message().await.unwrap().unwrap().range_response.unwrap().kvs.len(), 1);
    assert_eq!(got(kv.txn(pb::TxnRequest::default()).await), Answered);
    assert_eq!(got(kv.compact(pb::CompactionRequest::default()).await), Answered);
    assert_eq!(got(kv.delete_range(pb::DeleteRangeRequest { key: key("/all/a"), ..Default::default() }).await), Answered);

    let mut watch = WatchClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    let create = pb::WatchRequest { request_union: Some(pb::watch_request::RequestUnion::CreateRequest(
        pb::WatchCreateRequest { key: key("/all/w"), ..Default::default() })) };
    let mut events = watch.watch(tokio_stream::iter(vec![create])).await.unwrap().into_inner();
    assert!(events.message().await.unwrap().unwrap().created);

    let mut lease = LeaseClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    assert_eq!(got(lease.lease_grant(pb::LeaseGrantRequest::default()).await), Unimplemented);
    assert_eq!(got(lease.lease_revoke(pb::LeaseRevokeRequest::default()).await), Unimplemented);
    assert_eq!(got(lease.lease_time_to_live(pb::LeaseTimeToLiveRequest::default()).await), Unimplemented);
    assert_eq!(got(lease.lease_leases(pb::LeaseLeasesRequest::default()).await), Unimplemented);
    assert_eq!(got(lease.lease_keep_alive(tokio_stream::iter(vec![pb::LeaseKeepAliveRequest::default()])).await), Unimplemented);

    let mut cluster = ClusterClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    assert_eq!(got(cluster.member_list(pb::MemberListRequest::default()).await), Answered);
    assert_eq!(got(cluster.member_remove(pb::MemberRemoveRequest::default()).await), Unimplemented);
    assert_eq!(got(cluster.member_update(pb::MemberUpdateRequest::default()).await), Unimplemented);
    assert_eq!(got(cluster.member_add(pb::MemberAddRequest::default()).await), Answered);
    assert_eq!(got(cluster.member_promote(pb::MemberPromoteRequest::default()).await), tonic::Code::NotFound, "no such peer");

    let mut m = MaintenanceClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    assert_eq!(got(m.status(pb::StatusRequest::default()).await), Answered);
    assert_eq!(got(m.defragment(pb::DefragmentRequest::default()).await), Answered);
    assert_eq!(got(m.hash(pb::HashRequest::default()).await), Answered);
    assert_eq!(got(m.hash_kv(pb::HashKvRequest::default()).await), Answered);
    assert_eq!(got(m.move_leader(pb::MoveLeaderRequest::default()).await), Answered);
    assert_eq!(got(m.alarm(pb::AlarmRequest::default()).await), Unimplemented);
    assert_eq!(got(m.downgrade(pb::DowngradeRequest::default()).await), Unimplemented);
    let mut snap = m.snapshot(pb::SnapshotRequest::default()).await.unwrap().into_inner();
    assert!(snap.message().await.unwrap().is_some());

    let mut a = AuthClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    assert_eq!(got(a.auth_enable(pb::AuthEnableRequest::default()).await), Unimplemented);
    assert_eq!(got(a.auth_disable(pb::AuthDisableRequest::default()).await), Unimplemented);
    assert_eq!(got(a.auth_status(pb::AuthStatusRequest::default()).await), Unimplemented);
    assert_eq!(got(a.authenticate(pb::AuthenticateRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_add(pb::AuthUserAddRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_get(pb::AuthUserGetRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_list(pb::AuthUserListRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_delete(pb::AuthUserDeleteRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_change_password(pb::AuthUserChangePasswordRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_grant_role(pb::AuthUserGrantRoleRequest::default()).await), Unimplemented);
    assert_eq!(got(a.user_revoke_role(pb::AuthUserRevokeRoleRequest::default()).await), Unimplemented);
    assert_eq!(got(a.role_add(pb::AuthRoleAddRequest::default()).await), Unimplemented);
    assert_eq!(got(a.role_get(pb::AuthRoleGetRequest::default()).await), Unimplemented);
    assert_eq!(got(a.role_list(pb::AuthRoleListRequest::default()).await), Unimplemented);
    assert_eq!(got(a.role_delete(pb::AuthRoleDeleteRequest::default()).await), Unimplemented);
    assert_eq!(got(a.role_grant_permission(pb::AuthRoleGrantPermissionRequest::default()).await), Unimplemented);
    assert_eq!(got(a.role_revoke_permission(pb::AuthRoleRevokePermissionRequest::default()).await), Unimplemented);

    let mut e = ElectionClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    assert_eq!(got(e.campaign(el::CampaignRequest::default()).await), Unimplemented);
    assert_eq!(got(e.proclaim(el::ProclaimRequest::default()).await), Unimplemented);
    assert_eq!(got(e.leader(el::LeaderRequest::default()).await), Unimplemented);
    assert_eq!(got(e.observe(el::LeaderRequest::default()).await), Unimplemented);
    assert_eq!(got(e.resign(el::ResignRequest::default()).await), Unimplemented);

    let mut l = LockClient::new(conn.clone()).max_decoding_message_size(1 << 24).max_encoding_message_size(1 << 24);
    assert_eq!(got(l.lock(lk::LockRequest::default()).await), Unimplemented);
    assert_eq!(got(l.unlock(lk::UnlockRequest::default()).await), Unimplemented);

    // still serving, over the connection all of that used
    assert_eq!(got(kv.range(pb::RangeRequest { key: key("/all/a"), ..Default::default() }).await), Answered);
    Ok(())
}

/// [proto] Each generated client as an embedder may build it - connecting itself, over a channel
/// with an origin, with an interceptor that sets a header - answered the same each way.
#[tokio::test]
async fn each_client_however_it_is_built() -> Result<()> {
    use etcds::etcdpb::etcdserverpb as pb;
    use etcds::etcdpb::etcdserverpb::auth_client::AuthClient;
    use etcds::etcdpb::etcdserverpb::cluster_client::ClusterClient;
    use etcds::etcdpb::etcdserverpb::kv_client::KvClient;
    use etcds::etcdpb::etcdserverpb::lease_client::LeaseClient;
    use etcds::etcdpb::etcdserverpb::maintenance_client::MaintenanceClient;
    use etcds::etcdpb::etcdserverpb::watch_client::WatchClient;
    use etcds::etcdpb::v3electionpb as el;
    use etcds::etcdpb::v3electionpb::election_client::ElectionClient;
    use etcds::etcdpb::v3lockpb as lk;
    use etcds::etcdpb::v3lockpb::lock_client::LockClient;

    let (_node, _client, url) = start_server_at().await;
    let conn = tonic::transport::Channel::from_shared(url.clone()).unwrap().connect().await.unwrap();
    let origin: tonic::codegen::http::Uri = url.parse().unwrap();
    let header = |mut r: tonic::Request<()>| {
        r.metadata_mut().insert("x-test", "1".parse().unwrap());
        Ok(r)
    };
    macro_rules! built {
        ($client:ident, $call:ident, $req:expr, $want:expr) => {
            let got = |c: tonic::Code| assert_eq!(c, $want, "{}", stringify!($client));
            got($client::connect(url.clone()).await.unwrap().$call($req).await.map_or_else(|s| s.code(), |_| tonic::Code::Ok));
            got($client::with_origin(conn.clone(), origin.clone()).$call($req).await.map_or_else(|s| s.code(), |_| tonic::Code::Ok));
            got($client::with_interceptor(conn.clone(), header).$call($req).await.map_or_else(|s| s.code(), |_| tonic::Code::Ok));
        };
    }
    built!(KvClient, range, pb::RangeRequest { key: b"k".to_vec(), ..Default::default() }, tonic::Code::Ok);
    built!(MaintenanceClient, status, pb::StatusRequest::default(), tonic::Code::Ok);
    built!(ClusterClient, member_list, pb::MemberListRequest::default(), tonic::Code::Ok);
    built!(AuthClient, auth_status, pb::AuthStatusRequest::default(), tonic::Code::Unimplemented);
    built!(LeaseClient, lease_grant, pb::LeaseGrantRequest::default(), tonic::Code::Unimplemented);
    built!(ElectionClient, leader, el::LeaderRequest::default(), tonic::Code::Unimplemented);
    built!(LockClient, unlock, lk::UnlockRequest::default(), tonic::Code::Unimplemented);
    let create = || pb::WatchRequest { request_union: Some(pb::watch_request::RequestUnion::CreateRequest(
        pb::WatchCreateRequest { key: b"/w".to_vec(), ..Default::default() })) };
    let created = WatchClient::connect(url.clone()).await.unwrap().watch(tokio_stream::iter(vec![create()])).await;
    assert!(created.is_ok());
    assert!(WatchClient::with_origin(conn.clone(), origin.clone()).watch(tokio_stream::iter(vec![create()])).await.is_ok());
    assert!(WatchClient::with_interceptor(conn.clone(), header).watch(tokio_stream::iter(vec![create()])).await.is_ok());
    Ok(())
}

/// Lease, Lock, Election and Auth are registered - a client gets an answer,
/// not a transport error - and every call answers `unimplemented`.
#[tokio::test]
async fn lease_lock_election_and_auth_answer_unimplemented() -> Result<()> {
    let (_node, mut client) = start_server().await;

    unimplemented(client.lease_grant(10, None).await, "lease_grant");
    unimplemented(client.lease_revoke(1).await, "lease_revoke");
    unimplemented(client.lease_time_to_live(1, None).await, "lease_time_to_live");
    unimplemented(client.leases().await, "leases");
    // keep-alive is a stream: the refusal is the stream's first answer
    match client.lease_keep_alive(1).await {
        Ok((_, mut stream)) => unimplemented(stream.message().await, "lease_keep_alive"),
        Err(e) => assert_eq!(code(e), tonic::Code::Unimplemented),
    }

    unimplemented(client.lock("l", None).await, "lock");
    unimplemented(client.unlock("l").await, "unlock");

    unimplemented(client.campaign("e", "v", 0).await, "campaign");
    unimplemented(client.proclaim("v", None).await, "proclaim");
    unimplemented(client.leader("e").await, "leader");
    match client.observe("e").await {
        Ok(mut stream) => unimplemented(stream.message().await, "observe"),
        Err(e) => assert_eq!(code(e), tonic::Code::Unimplemented),
    }
    unimplemented(client.resign(None).await, "resign");

    unimplemented(client.auth_enable().await, "auth_enable");
    unimplemented(client.auth_disable().await, "auth_disable");
    unimplemented(client.user_add("u", "p", None).await, "user_add");
    unimplemented(client.user_get("u").await, "user_get");
    unimplemented(client.user_list().await, "user_list");
    unimplemented(client.user_delete("u").await, "user_delete");
    unimplemented(client.user_change_password("u", "p2").await, "user_change_password");
    unimplemented(client.user_grant_role("u", "r").await, "user_grant_role");
    unimplemented(client.user_revoke_role("u", "r").await, "user_revoke_role");
    unimplemented(client.role_add("r").await, "role_add");
    unimplemented(client.role_get("r").await, "role_get");
    unimplemented(client.role_list().await, "role_list");
    unimplemented(client.role_delete("r").await, "role_delete");
    unimplemented(client.role_grant_permission("r", Permission::read("k")).await,
        "role_grant_permission");
    unimplemented(client.role_revoke_permission("r", "k", None).await, "role_revoke_permission");
    // a client that logs in is told the same
    unimplemented(client.update_user(Some(("u".to_string(), "p".to_string()))).await,
        "authenticate");

    unimplemented(client.member_remove(1).await, "member_remove");
    unimplemented(client.member_update(1, vec!["http://h:1".to_string()]).await, "member_update");
    unimplemented(client.alarm(AlarmAction::Get, AlarmType::None, None).await, "alarm");

    // ...and the node is still serving after all of that
    client.put("still", "here", None).await?;
    assert_eq!(client.get("still", None).await?.kvs()[0].value(), b"here");
    Ok(())
}

/// `status` is the reachability probe: it reports the store size and the term.
/// The rest of Maintenance answers without doing anything a memory-only store
/// has no use for.
#[tokio::test]
async fn maintenance_reports_the_store_and_accepts_the_no_ops() -> Result<()> {
    let (_node, mut client) = start_server().await;

    let before = client.status().await?;
    assert_eq!(before.db_size(), 0, "a fresh node holds nothing");
    assert_eq!(before.raft_term(), 1, "term defaults to 1");
    client.put("m1", "1", None).await?;
    client.put("m2", "2", None).await?;
    let after = client.status().await?;
    assert_eq!(after.db_size(), 2, "db_size counts the keys held");

    client.defragment().await?;
    client.move_leader(0).await?;
    client.compact(5, None).await?;
    assert_eq!(client.get("m1", None).await?.count(), 1, "compact is a no-op: nothing goes");
    Ok(())
}

/// `hash` and `hash_kv` cover the replicated keys, so two calls over the same
/// store agree, and a change to a value changes the hash.
#[tokio::test]
async fn the_hashes_follow_the_store() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("h1", "1", None).await?;
    let a = client.hash().await?.hash();
    let a_kv = client.hash_kv(0).await?.hash();
    assert_eq!(a, client.hash().await?.hash(), "same store, same hash");
    assert_eq!(a_kv, client.hash_kv(0).await?.hash());

    client.put("h1", "2", None).await?;
    assert_ne!(a, client.hash().await?.hash(), "a changed value changes the hash");
    assert_ne!(a_kv, client.hash_kv(0).await?.hash());
    Ok(())
}

/// A snapshot is one blob of length-prefixed `KeyValue`s, every key included.
#[tokio::test]
async fn a_snapshot_carries_every_key() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("s1", "one", None).await?;
    client.put("s2", "two", None).await?;

    let mut stream = client.snapshot().await?;
    let mut blob = Vec::new();
    while let Some(part) = stream.message().await? {
        assert_eq!(part.remaining_bytes(), 0, "the whole store is sent as one message");
        blob.extend_from_slice(part.blob());
    }
    // walk the u32-big-endian length prefixes
    let mut records = 0;
    let mut at = 0;
    while at < blob.len() {
        let len = u32::from_be_bytes(blob[at..at + 4].try_into().unwrap()) as usize;
        at += 4 + len;
        records += 1;
    }
    assert_eq!(at, blob.len(), "the prefixes tile the blob exactly");
    assert_eq!(records, 2);
    let text = String::from_utf8_lossy(&blob);
    assert!(text.contains("s1") && text.contains("one") && text.contains("s2") && text.contains("two"));
    Ok(())
}

/// A standalone node has no peers, so the member list is empty and a promote
/// names a peer it does not have.
#[tokio::test]
async fn a_lone_node_lists_no_members_and_promotes_none() -> Result<()> {
    let (_node, mut client) = start_server().await;
    assert!(client.member_list().await?.members().is_empty());
    match client.member_promote(42).await {
        Ok(r) => panic!("promoting an unknown peer answered {:?}", r),
        Err(e) => assert_eq!(code(e), tonic::Code::NotFound),
    }
    // member_add of a url that does not answer adds nothing, and says ok
    client.member_add(["http://127.0.0.1:1"], None).await?;
    assert!(client.member_list().await?.members().is_empty());
    Ok(())
}

/// Every compare target a txn can test, against a key that exists and one that
/// does not. A missing key compares as zero / empty, as in etcd.
#[tokio::test]
async fn txn_compares_every_target() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("t", "b", None).await?;

    let cases: Vec<(Compare, bool)> = vec![
        (Compare::value("t", CompareOp::Equal, "b"), true),
        (Compare::value("t", CompareOp::NotEqual, "b"), false),
        (Compare::value("t", CompareOp::Greater, "a"), true),
        (Compare::value("t", CompareOp::Less, "a"), false),
        (Compare::value("missing", CompareOp::Equal, ""), true),
        // no revisions here: every stored version and revision is 0
        (Compare::version("t", CompareOp::Equal, 0), true),
        (Compare::version("t", CompareOp::Greater, 0), false),
        (Compare::create_revision("t", CompareOp::Less, 1), true),
        (Compare::create_revision("t", CompareOp::NotEqual, 0), false),
        (Compare::mod_revision("t", CompareOp::Equal, 0), true),
        (Compare::mod_revision("t", CompareOp::Greater, 0), false),
        (Compare::lease("t", CompareOp::Equal, 0), true),
        (Compare::lease("missing", CompareOp::NotEqual, 0), false),
    ];
    for (cmp, expect) in cases {
        let what = format!("{:?}", cmp);
        let resp = client.txn(Txn::new().when([cmp])).await?;
        assert_eq!(resp.succeeded(), expect, "{}", what);
    }
    Ok(())
}

/// The operations inside a txn: get one key, get everything, delete with the
/// previous value back, and a nested txn - each answered in order.
#[tokio::test]
async fn txn_runs_gets_deletes_and_nested_txns() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("x1", "1", None).await?;
    client.put("x2", "2", None).await?;

    let resp = client.txn(Txn::new().and_then([
        TxnOp::get("x1", None),
        TxnOp::get("", Some(GetOptions::new().with_all_keys().with_count_only())),
        TxnOp::get("nope", None),
        TxnOp::delete("x2", Some(DeleteOptions::new().with_prev_key())),
        TxnOp::delete("nope", None),
        TxnOp::txn(Txn::new()
            .when([Compare::value("x1", CompareOp::Equal, "1")])
            .and_then([TxnOp::put("x3", "3", Some(PutOptions::new().with_prev_key()))])
            .or_else([TxnOp::get("x1", None)])),
    ])).await?;
    assert!(resp.succeeded(), "no compares: the success branch runs");
    let ops = resp.op_responses();
    assert_eq!(ops.len(), 6);
    match &ops[0] {
        TxnOpResponse::Get(g) => { assert_eq!(g.count(), 1); assert_eq!(g.kvs()[0].value(), b"1"); }
        o => panic!("{:?}", o),
    }
    match &ops[1] {
        TxnOpResponse::Get(g) => { assert_eq!(g.count(), 2, "every key, counted"); assert!(g.kvs().is_empty()); }
        o => panic!("{:?}", o),
    }
    match &ops[2] {
        TxnOpResponse::Get(g) => assert_eq!(g.count(), 0),
        o => panic!("{:?}", o),
    }
    match &ops[3] {
        TxnOpResponse::Delete(d) => {
            assert_eq!(d.deleted(), 1);
            assert_eq!(d.prev_kvs()[0].value(), b"2", "the deleted value comes back");
        }
        o => panic!("{:?}", o),
    }
    match &ops[4] {
        TxnOpResponse::Delete(d) => assert_eq!(d.deleted(), 0),
        o => panic!("{:?}", o),
    }
    match &ops[5] {
        TxnOpResponse::Txn(t) => {
            assert!(t.succeeded());
            match &t.op_responses()[0] {
                TxnOpResponse::Put(p) => assert!(p.prev_key().is_none(), "x3 is new"),
                o => panic!("{:?}", o),
            }
        }
        o => panic!("{:?}", o),
    }

    assert_eq!(client.get("x2", None).await?.count(), 0, "the txn delete took effect");
    assert_eq!(client.get("x3", None).await?.kvs()[0].value(), b"3", "and the nested put");
    // the whole store, read back as a txn would: x1 and x3
    let all = client.txn(Txn::new().and_then([TxnOp::get("", Some(GetOptions::new().with_all_keys()))])).await?;
    match &all.op_responses()[0] {
        TxnOpResponse::Get(g) => assert_eq!(g.kvs().len(), 2),
        o => panic!("{:?}", o),
    }
    Ok(())
}

/// A txn get or delete over a prefix covers the prefix, as a plain range does.
#[tokio::test]
async fn txn_ranges_cover_the_range() -> Result<()> {
    let (_node, mut client) = start_server().await;
    for k in ["r/a", "r/b", "r/c", "s/a"] {
        client.put(k, "v", None).await?;
    }
    let resp = client.txn(Txn::new().and_then([
        TxnOp::get("r/", Some(GetOptions::new().with_prefix().with_limit(2))),
        TxnOp::delete("r/", Some(DeleteOptions::new().with_prefix().with_prev_key())),
    ])).await?;
    match &resp.op_responses()[0] {
        TxnOpResponse::Get(g) => {
            assert_eq!(g.count(), 3, "count is the whole prefix");
            assert_eq!(g.kvs().len(), 2, "kvs honour the limit");
            assert_eq!(g.kvs()[0].key(), b"r/a", "in key order");
            assert!(g.more());
        }
        o => panic!("{:?}", o),
    }
    match &resp.op_responses()[1] {
        TxnOpResponse::Delete(d) => {
            assert_eq!(d.deleted(), 3);
            assert_eq!(d.prev_kvs().len(), 3);
        }
        o => panic!("{:?}", o),
    }
    assert_eq!(client.get("r/", Some(GetOptions::new().with_prefix())).await?.count(), 0);
    assert_eq!(client.get("s/a", None).await?.count(), 1, "outside the prefix stays");
    Ok(())
}

/// A txn put is seen by a watcher of that key, as a plain put is.
#[tokio::test]
async fn a_txn_put_notifies_its_watchers() -> Result<()> {
    let (_node, mut client) = start_server().await;
    let mut stream = client.watch("tw", None).await?;
    assert!(stream.message().await?.unwrap().created());
    client.txn(Txn::new().and_then([TxnOp::put("tw", "from-txn", None)])).await?;
    let resp = stream.message().await?.unwrap();
    assert_eq!(resp.events()[0].kv().unwrap().value(), b"from-txn");
    Ok(())
}

/// A delete returns the values it took when asked, and nothing when not.
#[tokio::test]
async fn delete_returns_prev_kvs_only_when_asked() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("d1", "1", None).await?;
    client.put("d2", "2", None).await?;
    let r = client.delete("d1", None).await?;
    assert_eq!(r.deleted(), 1);
    assert!(r.prev_kvs().is_empty());
    let r = client.delete("d2", Some(DeleteOptions::new().with_prev_key())).await?;
    assert_eq!(r.prev_kvs()[0].value(), b"2");
    Ok(())
}
