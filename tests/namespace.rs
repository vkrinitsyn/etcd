#![allow(dead_code)]
//! Namespace: etcd's is the client's - each key prefixed on the way in and stripped on
//! the way out (`KvClientPrefix`, `LeaseClientPrefix`); the server needs the ranges,
//! the deletes and the txns to hold under a prefix, and nothing else.
mod testing;

use crate::testing::{start_server, Result};
use etcd_client::{Compare, CompareOp, DeleteOptions, GetOptions, KvClientPrefix, LeaseClientPrefix, PutOptions, Txn, TxnOp};

#[tokio::test]
async fn test_namespace_kv() -> Result<()> {
    let (_node, mut client) = start_server().await;
    let mut c = KvClientPrefix::new(client.kv_client(), "foo/".into());

    c.put("abc", "bar", None).await?;

    // get kv
    {
        let kvs = c.get("abc", None).await?.take_kvs();
        assert_eq!(kvs.len(), 1);
        assert_eq!(kvs[0].key(), b"abc");
        assert_eq!(kvs[0].value(), b"bar");
    }

    // get prefixed kv
    {
        let kvs = client.get("foo/abc", None).await?.take_kvs();
        assert_eq!(kvs.len(), 1);
        assert_eq!(kvs[0].key(), b"foo/abc");
        assert_eq!(kvs[0].value(), b"bar");
    }

    // delete kv
    {
        let resp = c
            .delete("abc", Some(DeleteOptions::new().with_prev_key()))
            .await?;
        assert_eq!(resp.deleted(), 1);
        let kvs = resp.prev_kvs();
        assert_eq!(kvs[0].key(), b"abc");
        assert_eq!(kvs[0].value(), b"bar");
    }

    Ok(())
}

/// [namespace] Two namespaces on one server: each sees its own keys only, by range,
/// count, delete and txn, and a lease's keys under its namespace
#[tokio::test]
async fn two_namespaces_see_their_own_keys_only() -> Result<()> {
    let (_node, mut client) = start_server().await;
    let mut a = KvClientPrefix::new(client.kv_client(), "/tenant/a/".into());
    let mut b = KvClientPrefix::new(client.kv_client(), "/tenant/b/".into());
    for (k, v) in [("x/1", "a1"), ("x/2", "a2"), ("y", "ay")] {
        a.put(k, v, None).await?;
    }
    b.put("x/1", "b1", None).await?;

    let xs = a.get("x/", Some(GetOptions::new().with_prefix())).await?;
    assert_eq!(xs.count(), 2);
    assert_eq!(xs.kvs().iter().map(|k| k.key()).collect::<Vec<_>>(), [b"x/1".as_slice(), b"x/2"], "stripped of the prefix");
    // every key of the namespace: the empty prefix (etcd-client's `with_all_keys` sets its
    // range end after the namespace mapped it, and leaves the namespace - a client's matter)
    let all_a = a.get("", Some(GetOptions::new().with_prefix())).await?;
    assert_eq!(all_a.count(), 3, "all keys: the namespace's all");
    let all_b = b.get("", Some(GetOptions::new().with_prefix())).await?;
    assert_eq!(all_b.count(), 1);
    assert_eq!(all_b.kvs()[0].value(), b"b1");

    // a txn compares and writes inside its namespace
    let t = Txn::new()
        .when([Compare::value("y", CompareOp::Equal, "ay")])
        .and_then([TxnOp::put("y", "ay2", None), TxnOp::delete("x/2", None)]);
    assert!(a.txn(t).await?.succeeded());
    assert_eq!(a.get("y", None).await?.kvs()[0].value(), b"ay2");
    assert_eq!(client.get("/tenant/a/y", None).await?.kvs()[0].value(), b"ay2", "the server holds it prefixed");

    // a prefix delete takes the namespace's keys, not the other's
    let gone = a.delete("x/", Some(DeleteOptions::new().with_prefix())).await?;
    assert_eq!(gone.deleted(), 1);
    assert_eq!(b.get("x/1", None).await?.count(), 1, "the other namespace keeps its key");

    // a lease's keys, asked through the namespace
    let lease = client.lease_grant(30, None).await?.id();
    a.put("leased", "v", Some(PutOptions::new().with_lease(lease))).await?;
    let mut la = LeaseClientPrefix::new(client.lease_client(), "/tenant/a/".into());
    let ttl = la.time_to_live(lease, Some(etcd_client::LeaseTimeToLiveOptions::new().with_keys())).await?;
    assert_eq!(ttl.keys(), [b"leased".to_vec()], "stripped of the prefix");
    Ok(())
}
