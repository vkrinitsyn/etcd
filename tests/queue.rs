#![allow(dead_code)]
mod testing;

use crate::testing::{start_server, Result};
use etcd_client::{EventType};
use tokio_stream::StreamExt;
use uuid::Uuid;

#[tokio::test]
async fn test_queue() -> Result<()> {
    let (_node, mut client) = start_server().await;
    let cid = Uuid::new_v4();
    let mut stream = client.watch(format!("/queue/test_queue/consumer/{}", cid.as_hyphenated()), None).await?;

    let resp = stream.message().await?.unwrap();
    assert!(resp.created());
    let watch_id = resp.watch_id();

    client.put("/queue/test_queue/producer/the-message", "01", None).await?;

    let resp = stream.next().await.unwrap()?;
    assert_eq!(resp.watch_id(), watch_id);
    assert_eq!(resp.events().len(), 1);

    let kv = resp.events()[0].kv().unwrap();
    println!("received: {}:{}", kv.key_str()?, kv.value_str()?);
    assert_eq!(kv.value(), b"01");
    assert_eq!(resp.events()[0].event_type(), EventType::Put);
    client.delete(kv.key(), None).await?; // AKS


    client.put("/queue/test_queue/producer/the-message", "02", None).await?;
    let resp = stream.next().await.unwrap()?;
    let kv = resp.events()[0].kv().unwrap();
    println!("received: {}:{}", kv.key_str()?, kv.value_str()?);
    assert_eq!(resp.events().len(), 1);

    assert_eq!(kv.value(), b"02");
    client.delete(kv.key(), None).await?; // AKS
    
    stream.cancel(watch_id).await?;

    let resp = stream.message().await?.unwrap();
    assert_eq!(resp.watch_id(), watch_id);
    let x = stream.request_progress().await;
    println!("progress: {:?}", x);
    
    assert!(resp.canceled());

    Ok(())
}

/// Read watch frames until one carries events; `None` when nothing arrives in time.
async fn next_event(stream: &mut etcd_client::WatchStream, wait_ms: u64) -> Option<(String, String)> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    loop {
        let resp = tokio::time::timeout_at(deadline, stream.message()).await.ok()?.ok()??;
        if let Some(kv) = resp.events().first().and_then(|e| e.kv()) {
            return Some((kv.key_str().unwrap().to_string(), kv.value_str().unwrap().to_string()));
        }
    }
}

/// queue.md's own example, with the short `/q/{name}/c/{uuid}` spelling: the
/// first event is the message, under its delivery key. That watch key is also
/// the key the server registers the consumer under, and the consumer used to
/// receive that registration - its node id - as if it were a message; acking
/// it deleted the registration.
#[tokio::test]
async fn the_short_spelling_receives_messages_not_its_own_registration() -> Result<()> {
    let (_node, mut client) = start_server().await;
    let me = Uuid::new_v4();
    let reg = format!("/q/jobs/c/{}", me);
    let mut events = client.watch(reg.as_str(), None).await?;

    client.put("/q/jobs/p/job-1", "payload", None).await?;
    let (key, value) = next_event(&mut events, 5000).await.expect("the message");
    assert_eq!((key.as_str(), value.as_str()), ("/q/jobs/1/job-1", "payload"));
    client.delete(key, None).await?;

    assert!(next_event(&mut events, 300).await.is_none(), "nothing else arrives");
    let r = client.get(reg.as_str(), None).await?;
    assert_eq!(r.count(), 1, "the registration is the server's, and still there");
    Ok(())
}

/// A queue's registry is readable like any key: the dispatcher record and the
/// consumer's host both name the node.
#[tokio::test]
async fn the_registry_is_readable() -> Result<()> {
    let (node, mut client) = start_server().await;
    let me = Uuid::new_v4();
    let _events = client.watch(format!("/queue/reg/consumer/{}", me), None).await?;
    let mut found = None;
    for _ in 0..50 {
        if let Some(d) = node.queue_dispatcher("/queue/reg").await { found = Some(d); break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let dispatcher = found.expect("the consumer's node claimed the queue");
    let rec = client.get("/queue/reg", None).await?;
    assert_eq!(rec.kvs()[0].value_str()?, dispatcher.to_string());
    let host = client.get(format!("/queue/reg/c/{}", me), None).await?;
    assert_eq!(host.kvs()[0].value_str()?, dispatcher.to_string());
    Ok(())
}

/// A cancelled consumer watch takes no more messages: the next one waits for
/// a consumer that is watching.
#[tokio::test]
async fn a_cancelled_consumer_takes_no_more_messages() -> Result<()> {
    let (_node, mut client) = start_server().await;
    let first = Uuid::new_v4();
    let mut stream = client.watch(format!("/q/cq/c/{}", first), None).await?;
    let id = stream.message().await?.unwrap().watch_id();
    client.put("/q/cq/p/a", "1", None).await?;
    let (key, _) = next_event(&mut stream, 5000).await.expect("the first message");
    client.delete(key, None).await?;

    stream.cancel(id).await?;
    assert!(stream.message().await?.unwrap().canceled());
    client.put("/q/cq/p/b", "2", None).await?;
    assert!(next_event(&mut stream, 500).await.is_none(), "a cancelled watch is not a consumer");

    let second = Uuid::new_v4();
    let mut other = client.watch(format!("/q/cq/c/{}", second), None).await?;
    let (key, value) = next_event(&mut other, 5000).await.expect("the waiting message");
    assert_eq!((key.as_str(), value.as_str()), ("/q/cq/2/b", "2"));
    Ok(())
}
