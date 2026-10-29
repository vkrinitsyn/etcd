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
    // `/q/jobs/{idx}/job-1`: the index is the dispatcher's, from its start
    let parts: Vec<&str> = key.split('/').collect();
    assert_eq!((parts.len(), parts[1], parts[2], parts[4], value.as_str()), (5, "q", "jobs", "job-1", "payload"), "{}", key);
    assert!(parts[3].parse::<u64>().is_ok(), "an index: {}", key);
    client.delete(key, None).await?;

    assert!(next_event(&mut events, 300).await.is_none(), "nothing else arrives");
    let r = client.get(reg.as_str(), None).await?;
    assert_eq!(r.count(), 1, "the registration is the server's, and still there");
    Ok(())
}

/// A queue's registry is readable like any key: the dispatcher record names
/// the node, and the consumer's registration is that node and its stream,
/// `<node>;g=<generation>;s=<start>` (agent.md, "A registration names a
/// stream").
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
    let reg = host.kvs()[0].value_str()?;
    let mut fields = reg.split(';');
    assert_eq!(fields.next(), Some(dispatcher.to_string().as_str()), "{}", reg);
    let rest: Vec<&str> = fields.collect();
    assert!(rest.iter().any(|f| f.starts_with("g=")), "a stream generation: {}", reg);
    assert!(rest.iter().any(|f| f.starts_with("s=")), "the node's start: {}", reg);
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
    let first_idx: u64 = key.split('/').nth(3).unwrap().parse().unwrap();
    client.delete(key, None).await?;

    stream.cancel(id).await?;
    assert!(stream.message().await?.unwrap().canceled());
    client.put("/q/cq/p/b", "2", None).await?;
    assert!(next_event(&mut stream, 500).await.is_none(), "a cancelled watch is not a consumer");

    let second = Uuid::new_v4();
    let mut other = client.watch(format!("/q/cq/c/{}", second), None).await?;
    let (key, value) = next_event(&mut other, 5000).await.expect("the waiting message");
    assert_eq!((key, value.as_str()), (format!("/q/cq/{}/b", first_idx + 1), "2"));
    Ok(())
}

/// [q-idx] A dispatcher's indexes start from the clock, not 1: a node started
/// later starts higher. (A dispatcher moving to a node whose queue was made
/// earlier is `a_new_dispatcher_starts_its_indexes_from_now`, in queue.rs.)
#[tokio::test]
async fn indexes_do_not_repeat_across_dispatchers() -> Result<()> {
    async fn first_index() -> Result<u64> {
        let (_node, mut client) = start_server().await;
        let mut stream = client.watch(format!("/q/idx/c/{}", Uuid::new_v4()), None).await?;
        client.put("/q/idx/p/m", "1", None).await?;
        let (key, _) = next_event(&mut stream, 3000).await.expect("delivered");
        Ok(key.split('/').nth(3).unwrap().parse().unwrap())
    }
    let older = first_index().await?;
    let newer = first_index().await?;
    assert!(older > 1_000_000_000_000_000, "from the clock, not 1: {}", older);
    assert!(newer > older, "a later dispatcher starts above: {} then {}", older, newer);
    Ok(())
}

/// [kv-delete] A delete under the queue prefixes is no event: a consumer
/// watching its registration key hears nothing of the registry's changes, and
/// a key watcher over `/q/` neither.
#[tokio::test]
async fn queue_deletes_are_no_watch_events() -> Result<()> {
    use etcd_client::WatchOptions;
    let (_node, mut client) = start_server().await;
    let me = Uuid::new_v4();
    let reg = format!("/q/qd/c/{}", me);
    let mut consumer = client.watch(reg.as_str(), None).await?;
    let mut range = client.watch("/q/", Some(WatchOptions::new().with_prefix())).await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // the registry's records, deleted as a node withdrawing would
    client.delete("/q/qd", None).await?;
    client.delete("/q/", Some(etcd_client::DeleteOptions::new().with_prefix())).await?;
    for s in [&mut consumer, &mut range] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(700);
        while let Ok(Ok(Some(resp))) = tokio::time::timeout_at(deadline, s.message()).await {
            for ev in resp.events() {
                assert!(ev.event_type() != EventType::Delete, "a queue delete reached a watcher: {:?}", ev.kv().map(|k| k.key_str().unwrap_or("").to_string()));
            }
        }
    }
    // and the consumer still consumes
    client.put("/q/qd/p/m", "1", None).await?;
    assert!(next_event(&mut consumer, 3000).await.is_some());
    Ok(())
}
