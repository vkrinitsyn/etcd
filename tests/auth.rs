//! [auth] The Auth API on signon (feature `auth`, owner, 2026-11-21), as an etcd v3 client
//! sees it, against signon's scratch database: `SIGNON_TEST_DB`, or `signon_test` over the
//! local socket - skipped with a note without one.
//!
//! `cargo test --features auth --test auth`
#![cfg(feature = "auth")]

use std::time::Duration;

use etcd_client::{Client, ConnectOptions, Error, GetOptions, LockOptions, Permission, WatchOptions};
use etcds::cluster::EtcdNode;

const PEER_TOKEN: &str = "the cluster's shared secret";
const PASSWORD: &str = "a long enough pass 123";

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

/// A served node with signon behind it, its url and a pool on signon's database;
/// `None` without a database
async fn node(auth_on: bool) -> Option<(EtcdNode, String, sqlx::PgPool)> {
    let url = db_url();
    let pool = match sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("auth tests skipped: no test database ({e})");
            return None;
        }
    };
    let port = free_port();
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    cfg.cluster = "77777777-7777-5777-8777-777777777777".to_string();
    cfg.listen_client_urls = format!("127.0.0.1:{}", port);
    cfg.auth_db_url = url;
    cfg.peer_token = PEER_TOKEN.to_string();
    cfg.auth = auth_on;
    let n = {
        let _one = START.lock().await;
        EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()), #[cfg(feature = "tracer")] None).await.unwrap()
    };
    let s = n.clone();
    tokio::spawn(async move { let _ = s.serve().await; });
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Some((n, format!("http://127.0.0.1:{}", port), pool))
}

/// a fresh account, with `roles`
async fn account(pool: &sqlx::PgPool, tag: &str, roles: &[&str]) -> String {
    let email = format!("etcd-{tag}-{}@test.example", uuid::Uuid::new_v4().simple());
    let id = signon::api::add_account(pool, &email, PASSWORD).await.unwrap().unwrap();
    for r in roles {
        signon::api::add_role(pool, r).await.unwrap();
        signon::grant_role(pool, id, r).await.unwrap();
    }
    email
}

/// AuthStatus, through the generated client (etcd-client has no call for it)
async fn status(url: &str) -> bool {
    use etcds::etcdpb::etcdserverpb::auth_client::AuthClient;
    AuthClient::connect(url.to_string()).await.unwrap()
        .auth_status(etcds::etcdpb::etcdserverpb::AuthStatusRequest::default()).await.unwrap().into_inner().enabled
}

async fn signed_in(url: &str, user: &str) -> Client {
    Client::connect([url], Some(ConnectOptions::new().with_user(user, PASSWORD))).await.unwrap()
}

#[tokio::test]
async fn off_nothing_is_asked_and_on_every_call_needs_a_token() {
    let Some((_n, url, pool)) = node(false).await else { return };
    let mut c = Client::connect([url.as_str()], None).await.unwrap();
    c.put("/free", "v", None).await.unwrap();
    assert!(!status(&url).await, "off");
    let e = Client::connect([url.as_str()], Some(ConnectOptions::new().with_user("x@test.example", PASSWORD))).await.err().unwrap();
    assert_eq!(code(e), tonic::Code::FailedPrecondition, "no sign-in while it is off");

    // a fresh database may hold a root from another test: a node of its own still needs one
    let root = account(&pool, "root", &["root"]).await;
    c.auth_enable().await.unwrap();
    assert!(status(&url).await, "on");
    assert_eq!(code(c.get("/free", None).await.unwrap_err()), tonic::Code::Unauthenticated, "no token now");
    let mut r = signed_in(&url, &root).await;
    assert_eq!(r.get("/free", None).await.unwrap().count(), 1, "root reads everything");
    r.auth_disable().await.unwrap();
    assert_eq!(c.get("/free", None).await.unwrap().count(), 1, "off again");
}

#[tokio::test]
async fn a_role_s_permissions_are_what_its_accounts_may_do() {
    let Some((_n, url, pool)) = node(true).await else { return };
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let root = account(&pool, "root", &["root"]).await;
    let mut r = signed_in(&url, &root).await;
    let role = format!("reader-{tag}");
    r.role_add(role.as_str()).await.unwrap();
    assert_eq!(code(r.role_add(role.as_str()).await.unwrap_err()), tonic::Code::FailedPrecondition, "there already");
    r.role_grant_permission(role.as_str(), Permission::read("/r/").with_prefix()).await.unwrap();
    r.role_grant_permission(role.as_str(), Permission::read_write("/rw")).await.unwrap();
    let perms = r.role_get(role.as_str()).await.unwrap();
    assert_eq!(perms.permissions().len(), 2);
    let user = account(&pool, "user", &[]).await;
    r.user_grant_role(user.as_str(), role.as_str()).await.unwrap();
    assert_eq!(r.user_get(user.as_str()).await.unwrap().roles(), std::slice::from_ref(&role));
    assert!(r.user_list().await.unwrap().users().contains(&user));
    r.put("/r/a", "1", None).await.unwrap();
    r.put("/w", "1", None).await.unwrap();

    let mut u = signed_in(&url, &user).await;
    assert_eq!(u.get("/r/a", None).await.unwrap().count(), 1, "read on /r/");
    assert_eq!(u.get("/r/", Some(GetOptions::new().with_prefix())).await.unwrap().count(), 1, "the whole prefix");
    assert_eq!(code(u.put("/r/a", "2", None).await.unwrap_err()), tonic::Code::PermissionDenied, "read only");
    assert_eq!(code(u.get("/w", None).await.unwrap_err()), tonic::Code::PermissionDenied, "not granted");
    u.put("/rw", "x", None).await.unwrap();
    assert_eq!(code(u.get("/", Some(GetOptions::new().with_prefix())).await.unwrap_err()), tonic::Code::PermissionDenied, "a range past its grants");
    // a txn needs each of its ranges
    let t = etcd_client::Txn::new().and_then([etcd_client::TxnOp::get("/r/a", None), etcd_client::TxnOp::put("/w", "x", None)]);
    assert_eq!(code(u.txn(t).await.unwrap_err()), tonic::Code::PermissionDenied);
    // a watch of what it may not read is cancelled at once, of what it may, created
    let mut s = u.watch("/w", None).await.unwrap();
    let first = s.message().await.unwrap().unwrap();
    assert!(first.canceled(), "{:?}", first);
    let mut s2 = u.watch("/r/", Some(WatchOptions::new().with_prefix())).await.unwrap();
    assert!(!s2.message().await.unwrap().unwrap().canceled());
    // the management calls are root's
    assert_eq!(code(u.role_list().await.unwrap_err()), tonic::Code::PermissionDenied);
    // [review 2026-11-21] a previous value is read: a write-only grant does not give it
    let writer = format!("writer-{tag}");
    r.role_add(writer.as_str()).await.unwrap();
    r.role_grant_permission(writer.as_str(), Permission::write("/wo")).await.unwrap();
    let wo = account(&pool, "wo", &[writer.as_str()]).await;
    let mut w = signed_in(&url, &wo).await;
    w.put("/wo", "1", None).await.unwrap();
    assert_eq!(code(w.put("/wo", "2", Some(etcd_client::PutOptions::new().with_prev_key())).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(code(w.delete("/wo", Some(etcd_client::DeleteOptions::new().with_prev_key())).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(code(w.leases().await.unwrap_err()), tonic::Code::PermissionDenied, "every lease id is root's to list");
    let lease = r.lease_grant(30, None).await.unwrap().id();
    r.put("/w", "x", Some(etcd_client::PutOptions::new().with_lease(lease))).await.unwrap();
    assert_eq!(code(w.lease_time_to_live(lease, Some(etcd_client::LeaseTimeToLiveOptions::new().with_keys())).await.unwrap_err()), tonic::Code::PermissionDenied, "its keys' names are read");
    assert!(w.lease_time_to_live(lease, None).await.is_ok(), "its TTL alone is not");
    // keeping it alive keeps its keys: written - refused as the stream opens, or on its first answer
    match w.lease_keep_alive(lease).await {
        Err(e) => assert_eq!(code(e), tonic::Code::PermissionDenied),
        Ok((mut keeper, mut answers)) => {
            let _ = keeper.keep_alive().await;
            assert_eq!(code(answers.message().await.unwrap_err()), tonic::Code::PermissionDenied);
        }
    }
    r.lease_revoke(lease).await.unwrap();

    // the permission revoked: refused within the cache's few seconds
    r.role_revoke_permission(role.as_str(), "/r/", Some(etcd_client::RoleRevokePermissionOptions::new().with_prefix())).await.unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(code(u.get("/r/a", None).await.unwrap_err()), tonic::Code::PermissionDenied);
    r.user_revoke_role(user.as_str(), role.as_str()).await.unwrap();
    assert_eq!(code(r.user_revoke_role(user.as_str(), role.as_str()).await.unwrap_err()), tonic::Code::FailedPrecondition);
    r.role_delete(role.as_str()).await.unwrap();
    assert_eq!(code(r.role_get(role.as_str()).await.unwrap_err()), tonic::Code::FailedPrecondition);
    assert_eq!(code(r.role_delete("root").await.unwrap_err()), tonic::Code::FailedPrecondition, "root stays");
}

#[tokio::test]
async fn accounts_are_signon_s_and_a_wrong_password_is_refused() {
    let Some((_n, url, pool)) = node(true).await else { return };
    let root = account(&pool, "root", &["root"]).await;
    let mut r = signed_in(&url, &root).await;
    assert_eq!(code(r.user_add("new@test.example", "pw", None).await.unwrap_err()), tonic::Code::FailedPrecondition);
    assert_eq!(code(r.user_delete(root.as_str()).await.unwrap_err()), tonic::Code::FailedPrecondition);
    assert_eq!(code(r.user_change_password(root.as_str(), "pw2").await.unwrap_err()), tonic::Code::FailedPrecondition);
    assert_eq!(code(r.user_get("nobody@test.example").await.unwrap_err()), tonic::Code::FailedPrecondition);
    let e = Client::connect([url.as_str()], Some(ConnectOptions::new().with_user(root.as_str(), "wrong"))).await.err().unwrap();
    assert_eq!(code(e), tonic::Code::InvalidArgument);
    // the session is signon's: signed out there, the token stops at the next read of it
    signon::api::accounts(&pool).await.unwrap();
}

#[tokio::test]
async fn leases_and_locks_need_a_token_and_a_lock_its_name() {
    let Some((_n, url, pool)) = node(true).await else { return };
    let mut anon = Client::connect([url.as_str()], None).await.unwrap();
    assert_eq!(code(anon.lease_grant(30, None).await.unwrap_err()), tonic::Code::Unauthenticated);
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let root = account(&pool, "root", &["root"]).await;
    let mut r = signed_in(&url, &root).await;
    let role = format!("locker-{tag}");
    r.role_add(role.as_str()).await.unwrap();
    r.role_grant_permission(role.as_str(), Permission::write("/locks/mine/").with_prefix()).await.unwrap();
    let user = account(&pool, "locker", &[role.as_str()]).await;
    let mut u = signed_in(&url, &user).await;
    let lease = u.lease_grant(30, None).await.unwrap().id();
    u.lock("/locks/mine", Some(LockOptions::new().with_lease(lease))).await.unwrap();
    // a lease holding a key the account may not write is not its to revoke
    let other = r.lease_grant(30, None).await.unwrap().id();
    r.put("/locks/theirs", "v", Some(etcd_client::PutOptions::new().with_lease(other))).await.unwrap();
    assert_eq!(code(u.lease_revoke(other).await.unwrap_err()), tonic::Code::PermissionDenied);
    assert_eq!(r.get("/locks/theirs", None).await.unwrap().count(), 1, "its key stays");
    r.lease_revoke(other).await.unwrap();
    assert_eq!(code(u.lock("/locks/other", Some(LockOptions::new().with_lease(lease))).await.unwrap_err()), tonic::Code::PermissionDenied);
}

#[tokio::test]
async fn a_call_passing_for_a_peer_needs_the_cluster_s_token() {
    use etcds::etcdpb::etcdserverpb::kv_client::KvClient;
    use etcds::etcdpb::etcdserverpb::RangeRequest;
    let Some((_n, url, _pool)) = node(true).await else { return };
    let mut kv = KvClient::connect(url.clone()).await.unwrap();
    let mut forged = tonic::Request::new(RangeRequest { key: b"/".to_vec(), range_end: vec![0], ..Default::default() });
    forged.metadata_mut().insert(etcds::etcdpb::XPEER, "1".parse().unwrap());
    assert_eq!(kv.range(forged).await.unwrap_err().code(), tonic::Code::Unauthenticated, "x-peer alone is no peer");
    let mut wrong = tonic::Request::new(RangeRequest { key: b"/".to_vec(), ..Default::default() });
    wrong.metadata_mut().insert(etcds::etcdpb::XPEER, "1".parse().unwrap());
    wrong.metadata_mut().insert("x-etcds-peer-token", "guess".parse().unwrap());
    assert_eq!(kv.range(wrong).await.unwrap_err().code(), tonic::Code::Unauthenticated);
    let mut peer = tonic::Request::new(RangeRequest { key: b"/".to_vec(), ..Default::default() });
    peer.metadata_mut().insert(etcds::etcdpb::XPEER, "1".parse().unwrap());
    peer.metadata_mut().insert("x-etcds-peer-token", PEER_TOKEN.parse().unwrap());
    assert!(kv.range(peer).await.is_ok(), "the cluster's token: a peer's call");
}

#[tokio::test]
async fn a_node_with_signon_needs_a_peer_token() {
    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    cfg.listen_client_urls = format!("127.0.0.1:{}", free_port());
    cfg.auth_db_url = db_url();
    let e = EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()), #[cfg(feature = "tracer")] None).await.err().unwrap();
    assert!(e.contains("ETCD-E173"), "{e}");
    let mut on = etcds::cli::EtcdConfig::with_defaults();
    on.listen_client_urls = format!("127.0.0.1:{}", free_port());
    on.auth = true;
    let e = EtcdNode::init(on, slog::Logger::root(slog::Discard, slog::o!()), #[cfg(feature = "tracer")] None).await.err().unwrap();
    assert!(e.contains("ETCD-E172"), "auth on without a database: {e}");
}

#[tokio::test]
async fn bad_grants_are_refused_as_etcd_refuses_them() {
    use etcds::etcdpb::authpb::Permission as Pb;
    use etcds::etcdpb::etcdserverpb::auth_client::AuthClient;
    use etcds::etcdpb::etcdserverpb::AuthRoleGrantPermissionRequest;
    let Some((_n, url, pool)) = node(true).await else { return };
    let root = account(&pool, "root", &["root"]).await;
    let mut r = signed_in(&url, &root).await;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let role = format!("bad-{tag}");
    r.role_add(role.as_str()).await.unwrap();
    assert_eq!(code(r.role_add("").await.unwrap_err()), tonic::Code::InvalidArgument, "an empty name");
    assert_eq!(code(r.user_grant_role("nobody@test.example", role.as_str()).await.unwrap_err()), tonic::Code::FailedPrecondition, "no such user");
    assert_eq!(code(r.user_grant_role(root.as_str(), format!("none-{tag}")).await.unwrap_err()), tonic::Code::FailedPrecondition, "no such role");
    assert_eq!(code(r.user_revoke_role("nobody@test.example", role.as_str()).await.unwrap_err()), tonic::Code::FailedPrecondition);
    assert_eq!(code(r.role_grant_permission(format!("none-{tag}"), Permission::read("/k")).await.unwrap_err()), tonic::Code::FailedPrecondition);
    assert_eq!(code(r.role_revoke_permission(role.as_str(), "/never", None).await.unwrap_err()), tonic::Code::FailedPrecondition, "never granted");
    assert_eq!(code(r.role_delete(format!("none-{tag}")).await.unwrap_err()), tonic::Code::FailedPrecondition);
    // a permission type etcd has not, and none at all - through the generated client, with the root's token
    let mut a = AuthClient::connect(url.clone()).await.unwrap();
    let tok = a.authenticate(etcds::etcdpb::etcdserverpb::AuthenticateRequest { name: root.clone(), password: PASSWORD.to_string() }).await.unwrap().into_inner().token;
    let call = |perm: Option<Pb>| {
        let mut q = tonic::Request::new(AuthRoleGrantPermissionRequest { name: role.clone(), perm });
        q.metadata_mut().insert("token", tok.parse().unwrap());
        q
    };
    assert_eq!(a.role_grant_permission(call(Some(Pb { perm_type: 7, key: b"/k".to_vec(), range_end: vec![] }))).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    assert_eq!(a.role_grant_permission(call(None)).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    assert!(r.role_list().await.unwrap().roles().contains(&role));
}
