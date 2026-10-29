use crate::cluster::{EtcdNode, WatcherId};
use crate::etcdpb::etcdserverpb::watch_server::Watch;
use crate::etcdpb::etcdserverpb::{
    WatchRequest,
    WatchResponse,
};
use std::pin::Pin;
use std::vec;
use slog::{debug, info, warn};
use tokio::sync::{mpsc};
use tokio::sync::mpsc::Receiver;
use tokio_stream::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{async_trait, Request, Response, Status, Streaming};
use uuid::Uuid;
use crate::etcdpb::etcdserverpb::watch_request::RequestUnion;
use crate::etcdpb::mvccpb::{Event};
use crate::{WatchSender, LP};

type WatchResult<T> = Result<Response<T>, Status>;

/// [q-dead] How long a watcher's stream may stay full before the watcher is
/// cancelled: a notification waits that long, then the next one, and the
/// writes behind them, wait no more.
pub(crate) const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
type ResponseStream = Pin<Box<dyn Stream<Item = Result<WatchResponse, Status>> + Send>>;

impl EtcdNode {
    pub(crate) async fn watch_notify(&self, mut receiver: Receiver<(crate::kv::Kv, bool)>) {
        let observers = self.observers.clone();
        let observer_ranges = self.observer_ranges.clone();
        let watchers = self.watchers.clone();
        let log = self.log.clone();
        let node = self.clone();
        tokio::spawn(async move {
            while let Some((kv, deleted)) = receiver.recv().await {
                // [kv-delete] a delete is told as etcd tells it: a DELETE event,
                // the key with no value. Not under the queue prefixes: the
                // registry is the server's, and a queue consumer watching its
                // registration key takes messages there, never its withdrawal.
                if deleted && (kv.key.starts_with(b"/q/") || kv.key.starts_with(b"/queue/")) {
                    continue;
                }
                // [range] Exact-key watchers first - unchanged, and still the
                // fast path - then the ones registered over a range. Without the
                // second part a prefix watch never fired at all: the watcher sat
                // under its literal start key and the changed key never equalled
                // it. That is how a client subscribes to a key range, and how
                // rppd registers a queue consumer.
                let mut targets: Vec<(crate::cluster::ClientId, i64)> =
                    observers.read().await.get(&kv.key).cloned().unwrap_or_default();
                {
                    let ranges = observer_ranges.read().await;
                    let obs = observers.read().await;
                    for (start, end) in ranges.iter() {
                        // the exact-key hit above already covered start == key
                        if start == &kv.key {
                            continue;
                        }
                        if crate::cluster::EtcdNode::in_range(&kv.key, start, end) {
                            if let Some(v) = obs.get(start) {
                                targets.extend(v.iter().cloned());
                            }
                        }
                    }
                }
                // [q-dead] The senders taken under the locks, sent to after:
                // a send into a stream the client does not read waits for room,
                // and held across it the watchers' lock stopped every watch made
                // and cancelled on the node - a queue consumer dropped as slow
                // included - for as long as that client did not read.
                let mut sends = Vec::new();
                if !targets.is_empty() {
                    let all = watchers.read().await;
                    for (cid, wid) in targets { // expected watcher per client
                        if let Some(client_watcher) = all.get(&cid) {
                            if let Some(client) = client_watcher.read().await.watchers.get(&wid) {
                                sends.push((cid, wid, client.client.clone()));
                            }
                        }
                    }
                }
                for (cid, wid, client) in sends {
                    info!(log, "{}{}", LP, tr!("srv.watch_notify", key = String::from_utf8_lossy(&kv.key)));
                    // [q-dead] bounded: a watcher that does not take it in time
                    // is cancelled, as a stalled queue consumer is, rather than
                    // holding up every write behind it. In a task: the cancel
                    // takes locks a write waiting on this task may hold.
                    let sent = tokio::time::timeout(NOTIFY_TIMEOUT, client.send(Ok(
                        WatchResponse {
                            header: None,
                            watch_id: wid,
                            created: false,
                            canceled: false,
                            compact_revision: 0,
                            cancel_reason: "".to_string(),
                            fragment: false,
                            events: vec![ Event {
                                r#type: if deleted { 1 } else { 0 }, // put, delete
                                kv: Some(if deleted {
                                    crate::etcdpb::mvccpb::KeyValue { key: kv.key.clone(), ..Default::default() }
                                } else {
                                    kv.clone().into()
                                }),
                                prev_kv: None,
                            }],
                        }))).await;
                    if sent.is_err() {
                        let node = node.clone();
                        tokio::spawn(async move { node.cancel_slow_watch(cid, wid, client).await });
                    }
                }
            }
        });
    }
}

#[async_trait]
impl Watch for EtcdNode {
    type WatchStream = ResponseStream;

    async fn watch(&self, request: Request<Streaming<WatchRequest>>) -> WatchResult<ResponseStream> {
        // request
        let (sender, receiver) = mpsc::channel::<Result<WatchResponse, Status>>(10);
        // [E3] the caller, from the connection - a watch used to stream any key
        let remote = request.remote_addr();
        let mut stream = request.into_inner();
        let etcd = self.clone();
        tokio::spawn(async move {
            let mut cid = Uuid::new_v4(); // lets give this client a temporary ID 
            
            while let Some(item) = stream.next().await {
                match item {
                    Ok(r) => {
                        if let Some(RequestUnion::CreateRequest(c)) = &r.request_union {
                            if !etcd.policy.read().await.range_may_serve(&c.key, &c.range_end, remote) {
                                // refused, as etcd refuses a watch: created and
                                // canceled at once, with the reason
                                let _ = sender.send(Ok(WatchResponse {
                                    header: None,
                                    watch_id: c.watch_id,
                                    created: true,
                                    canceled: true,
                                    compact_revision: 0,
                                    cancel_reason: "this key prefix is served only on a local connection".to_string(),
                                    fragment: false,
                                    events: vec![],
                                })).await;
                                continue;
                            }
                        }
                        cid = etcd.handle_watch_request(r.request_union, &sender, &cid).await;
                    }
                    Err(e) => {
                        warn!(etcd.log, "{}{}", LP, tr!("srv.watch_error", client = cid, err = e.message()));
                    }
                }
            }
            info!(etcd.log, "{}{}", LP, tr!("srv.watch_end", client = cid));
        });

        let output_stream = ReceiverStream::new(receiver);
        Ok(Response::new(Box::pin(output_stream) as Self::WatchStream))
    }
}

impl EtcdNode {
    /// use as library watcher entry point
    pub async fn new_watcher(&self, mut stream: Receiver<WatchRequest>, sender: WatchSender, client_id: &Uuid) {
        let etcd = self.clone();
        let mut cid = client_id.clone();

        tokio::spawn(async move {
            while let Some(r) = stream.recv().await {
                cid = etcd.handle_watch_request(r.request_union, &sender, &cid).await;
            }
            info!(etcd.log, "{}{}", LP, tr!("srv.watch_end", client = cid));
        });
    }

    async fn handle_watch_request(&self, request: Option<RequestUnion>, sender: &WatchSender, cid: &Uuid) -> Uuid {
        if let Some(rx) = request {
            match rx {
                RequestUnion::CreateRequest(r) => {
                    return self.create_watcher(r, cid.clone(), sender.clone()).await;
                }
                RequestUnion::CancelRequest(r) => {
                    let _ = self.remove_watcher(r, cid.clone()).await;
                }
                RequestUnion::ProgressRequest(_r) => {
                    let watchers = self.list_watchers(cid).await;
                    debug!(self.log, "{}{}", LP, tr!("srv.progress", client = cid));
                    if let Err(e) = sender.send(Ok(
                            WatchResponse {
                                watch_id: *watchers.get(0).unwrap_or(&0),
                                fragment: watchers.len() > 1, ..Default::default()
                            }
                        )
                    ).await {
                        warn!(self.log, "{}{}", LP, tr!("srv.progress_send_failed", err = e));
                    }
                }
            }
        }
        cid.clone()
    }
    
    async fn list_watchers(&self, cid: &Uuid) -> Vec<WatcherId> {
        match self.watchers.read().await.get(cid) {
            None => vec![],
            Some(w) => w.read().await.watchers.keys().map(|e| *e).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::queue::node_tests::{node, Consumer};
    use uuid::Uuid;

    const A: &str = "22222222-2222-5222-8222-222222222222";

    /// A put to a prefix watch's own start key is one event, not two: the
    /// exact-key path and the range scan must not both fire.
    #[tokio::test]
    async fn a_put_to_a_range_start_key_fires_once() {
        let n = node(A).await;
        let mut c = Consumer::watch(&n, &Uuid::new_v4(), "/plain/", 1).await;
        c.req.send(crate::etcdpb::etcdserverpb::WatchRequest { request_union: Some(
            crate::etcdpb::etcdserverpb::watch_request::RequestUnion::CreateRequest(
                crate::etcdpb::etcdserverpb::WatchCreateRequest {
                    key: b"/range/".to_vec(), range_end: b"/range0".to_vec(), watch_id: 2, ..Default::default() })) })
            .await.unwrap();
        assert!(c.frame().await.unwrap().unwrap().created);
        n.kv_put(b"/range/".to_vec(), b"v".to_vec()).await.unwrap();
        assert_eq!(c.next().await.0, "/range/");
        assert!(c.quiet().await, "exactly one event");
    }

    /// A watcher whose stream has gone does not stop the others being told.
    #[tokio::test]
    async fn a_closed_watcher_does_not_stop_the_others() {
        let n = node(A).await;
        let gone = Consumer::watch(&n, &Uuid::new_v4(), "/shared-key", 1).await;
        let mut live = Consumer::watch(&n, &Uuid::new_v4(), "/shared-key", 1).await;
        drop(gone);
        n.kv_put(b"/shared-key".to_vec(), b"1".to_vec()).await.unwrap();
        assert_eq!(live.next().await, ("/shared-key".to_string(), "1".to_string()));
        n.kv_put(b"/shared-key".to_vec(), b"2".to_vec()).await.unwrap();
        assert_eq!(live.next().await.1, "2");
    }
}
