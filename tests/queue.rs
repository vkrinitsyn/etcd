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