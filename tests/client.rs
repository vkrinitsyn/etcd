#![allow(dead_code)]
mod testing;

use crate::testing::{start_server, Result, TESTING_PREFIX, TESTING_RANGE};
use etcd_client::{
    Compare, CompareOp, DeleteOptions, EventType, GetOptions, PutOptions, Txn, TxnOp,
    TxnOpResponse,
};

#[tokio::test]
async fn test_put() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("put", "123", None).await?;

    // overwrite with prev key
    {
        let resp = client
            .put("put", "456", Some(PutOptions::new().with_prev_key()))
            .await?;
        let prev_key = resp.prev_key();
        assert!(prev_key.is_some());
        let prev_key = prev_key.unwrap();
        assert_eq!(prev_key.key(), b"put");
        assert_eq!(prev_key.value(), b"123");
    }

    // overwrite again with prev key
    {
        let resp = client
            .put("put", "789", Some(PutOptions::new().with_prev_key()))
            .await?;
        let prev_key = resp.prev_key();
        assert!(prev_key.is_some());
        let prev_key = prev_key.unwrap();
        assert_eq!(prev_key.key(), b"put");
        assert_eq!(prev_key.value(), b"456");
    }

    Ok(())
}

#[tokio::test]
async fn test_get() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("get10", "10", None).await?;
    client.put("get11", "11", None).await?;
    client.put("get20", "20", None).await?;
    client.put("get21", "21", None).await?;

    // get key
    {
        let resp = client.get("get11", None).await?;
        assert_eq!(resp.count(), 1);
        assert!(!resp.more());
        assert_eq!(resp.kvs().len(), 1);
        assert_eq!(resp.kvs()[0].key(), b"get11");
        assert_eq!(resp.kvs()[0].value(), b"11");
    }

    // get from key
    if TESTING_RANGE {
        let resp = client
            .get(
                "get11",
                Some(GetOptions::new().with_from_key().with_limit(2)),
            )
            .await?;
        assert!(resp.more());
        assert_eq!(resp.kvs().len(), 2);
        assert_eq!(resp.kvs()[0].key(), b"get11");
        assert_eq!(resp.kvs()[0].value(), b"11");
        assert_eq!(resp.kvs()[1].key(), b"get20");
        assert_eq!(resp.kvs()[1].value(), b"20");
    }

    // get prefix keys
    if TESTING_PREFIX {
        let resp = client
            .get("get1", Some(GetOptions::new().with_prefix()))
            .await?;
        assert_eq!(resp.count(), 2);
        assert!(!resp.more());
        assert_eq!(resp.kvs().len(), 2);
        assert_eq!(resp.kvs()[0].key(), b"get10");
        assert_eq!(resp.kvs()[0].value(), b"10");
        assert_eq!(resp.kvs()[1].key(), b"get11");
        assert_eq!(resp.kvs()[1].value(), b"11");
    }

    Ok(())
}

#[tokio::test]
async fn test_delete() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("del10", "10", None).await?;
    client.put("del11", "11", None).await?;
    client.put("del20", "20", None).await?;
    client.put("del21", "21", None).await?;
    client.put("del31", "31", None).await?;
    client.put("del32", "32", None).await?;

    // delete key
    {
        let resp = client.delete("del11", None).await?;
        assert_eq!(resp.deleted(), 1);
        let resp = client
            .get("del11", Some(GetOptions::new().with_count_only()))
            .await?;
        assert_eq!(resp.count(), 0);
    }

    // delete a range of keys
    if TESTING_RANGE {
        let resp = client
            .delete("del11", Some(DeleteOptions::new().with_range("del22")))
            .await?;
        assert_eq!(resp.deleted(), 2);
        let resp = client
            .get(
                "del11",
                Some(GetOptions::new().with_range("del22").with_count_only()),
            )
            .await?;
        assert_eq!(resp.count(), 0);
    }

    // delete key with prefix
    if TESTING_PREFIX {
        let resp = client
            .delete("del3", Some(DeleteOptions::new().with_prefix()))
            .await?;
        assert_eq!(resp.deleted(), 2);
        let resp = client
            .get("del32", Some(GetOptions::new().with_count_only()))
            .await?;
        assert_eq!(resp.count(), 0);
    }

    Ok(())
}

#[tokio::test]
async fn test_txn() -> Result<()> {
    let (_node, mut client) = start_server().await;
    client.put("txn01", "01", None).await?;

    // transaction 1
    {
        let resp = client
            .txn(
                Txn::new()
                    .when(&[Compare::value("txn01", CompareOp::Equal, "01")][..])
                    .and_then(
                        &[TxnOp::put(
                            "txn01",
                            "02",
                            Some(PutOptions::new().with_prev_key()),
                        )][..],
                    )
                    .or_else(&[TxnOp::get("txn01", None)][..]),
            )
            .await?;

        assert!(resp.succeeded());
        let op_responses = resp.op_responses();
        assert_eq!(op_responses.len(), 1);

        match op_responses[0] {
            TxnOpResponse::Put(ref resp) => assert_eq!(resp.prev_key().unwrap().value(), b"01"),
            _ => panic!("unexpected response"),
        }

        let resp = client.get("txn01", None).await?;
        assert_eq!(resp.kvs()[0].key(), b"txn01");
        assert_eq!(resp.kvs()[0].value(), b"02");
    }

    // transaction 2
    {
        let resp = client
            .txn(
                Txn::new()
                    .when(&[Compare::value("txn01", CompareOp::Equal, "01")][..])
                    .and_then(&[TxnOp::put("txn01", "02", None)][..])
                    .or_else(&[TxnOp::get("txn01", None)][..]),
            )
            .await?;

        assert!(!resp.succeeded());
        let op_responses = resp.op_responses();
        assert_eq!(op_responses.len(), 1);

        match op_responses[0] {
            TxnOpResponse::Get(ref resp) => assert_eq!(resp.kvs()[0].value(), b"02"),
            _ => panic!("unexpected response"),
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_watch() -> Result<()> {
    let (_node, mut client) = start_server().await;

    let mut stream = client.watch("watch01", None).await?;

    let resp = stream.message().await?.unwrap();
    assert!(resp.created());
    let watch_id = resp.watch_id();

    client.put("watch01", "01", None).await?;

    let resp = stream.message().await?.unwrap();
    assert_eq!(resp.watch_id(), watch_id);
    assert_eq!(resp.events().len(), 1);

    let kv = resp.events()[0].kv().unwrap();
    assert_eq!(kv.key(), b"watch01");
    assert_eq!(kv.value(), b"01");
    assert_eq!(resp.events()[0].event_type(), EventType::Put);

    stream.cancel(watch_id).await?;

    let resp = stream.message().await?.unwrap();
    assert_eq!(resp.watch_id(), watch_id);
    assert!(resp.canceled());

    Ok(())
}

/// [range] A PREFIX watch, which never fired before.
///
/// `create_watcher` dropped `range_end` and registered the watcher under its
/// literal start key, and `watch_notify` looked the changed key up in that map
/// directly - so a watch on `/p/` sat under `/p/` and a put to `/p/x` matched
/// nothing at all. Every prefix watch was silent. That is how a client
/// subscribes to a key range, and how rppd registers a queue consumer
/// (`rppd_function.schema_table` beginning with `/`).
#[tokio::test]
async fn test_watch_prefix() -> Result<()> {
    use etcd_client::WatchOptions;
    let (_node, mut client) = start_server().await;

    let mut stream = client
        .watch("wp/", Some(WatchOptions::new().with_prefix()))
        .await?;

    // under the prefix, and NOT equal to it - the exact-key path must not be
    // what makes this pass
    client.put("wp/one", "1", None).await?;

    // The first frame on a new watch is the `created` ack, which carries no
    // events - read past it rather than mistaking it for a delivery.
    let events = next_events(&mut stream).await;
    assert_eq!(events.len(), 1, "one put under the prefix, one event");
    assert_eq!(events[0].0, b"wp/one".to_vec());
    assert_eq!(events[0].1, b"1".to_vec());
    Ok(())
}

/// ...and a key OUTSIDE the prefix must stay silent, or the fix traded a watch
/// that never fires for one that fires on everything.
#[tokio::test]
async fn test_watch_prefix_ignores_outsiders() -> Result<()> {
    use etcd_client::WatchOptions;
    let (_node, mut client) = start_server().await;

    let mut stream = client
        .watch("inside/", Some(WatchOptions::new().with_prefix()))
        .await?;

    client.put("outside/key", "x", None).await?;
    // Only a frame WITH events counts as waking the watcher; the created ack
    // arrives regardless and is not a delivery.
    let quiet = tokio::time::timeout(std::time::Duration::from_secs(2), next_events(&mut stream)).await;
    match quiet {
        Err(_) => {}                       // nothing delivered: correct
        Ok(ev) => panic!("a put outside the prefix woke the watcher: {:?}", ev),
    }

    // and the watcher is still live for one that IS inside
    client.put("inside/key", "y", None).await?;
    let events = next_events(&mut stream).await;
    assert_eq!(events[0].0, b"inside/key".to_vec());
    Ok(())
}

/// Read watch frames until one actually carries events, returning (key, value)
/// pairs. A new watch's first frame is the `created` ack and carries none.
async fn next_events(stream: &mut etcd_client::WatchStream) -> Vec<(Vec<u8>, Vec<u8>)> {
    loop {
        let resp = stream.message().await.expect("watch stream").expect("watch open");
        let evs: Vec<(Vec<u8>, Vec<u8>)> = resp.events().iter()
            .filter_map(|e| e.kv().map(|kv| (kv.key().to_vec(), kv.value().to_vec())))
            .collect();
        if !evs.is_empty() {
            return evs;
        }
    }
}
