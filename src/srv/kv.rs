#[cfg(feature = "tracer")]
use opentelemetry::trace::Tracer;

use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, RangeStreamResponse, TxnRequest, TxnResponse};
use tonic::{async_trait, Request, Response, Status};


impl EtcdNode {
    /// [sys] Whether a call reaches [`crate::sys::SYS_PREFIX`]: `None` it does not; `Some`
    /// a peer's call on it; a client's call on it is refused here. Only a call that starts
    /// in the prefix: the records are not in the store, so a range over every key - a
    /// peer's sync at start, a client's - reads past them, as it should.
    pub(crate) async fn sys_call(&self, meta: &tonic::metadata::MetadataMap, key: &[u8], remote: Option<std::net::SocketAddr>) -> Result<Option<()>, Status> {
        if !key.starts_with(crate::sys::SYS_PREFIX.as_bytes()) {
            return Ok(None);
        }
        self.verified_peer(meta, remote).await.map(Some)
    }

    /// [review 2026-11-21, 2] A peer's call, proved: `x-peer` with the cluster's `peer_token`,
    /// or without a token set, from the address of a peer this node holds - `x-peer` alone is
    /// any client's header
    pub(crate) async fn verified_peer(&self, meta: &tonic::metadata::MetadataMap, remote: Option<std::net::SocketAddr>) -> Result<(), Status> {
        let token = self.cfg.read().await.peer_token.clone();
        match crate::guard::peer_proof(meta, &token) {
            None => Err(Status::permission_denied(tr!("sys.peers_only"))),
            Some(Err(e)) => Err(e),
            Some(Ok(())) if token.is_empty() => self.known_peer(remote).await,
            Some(Ok(())) => Ok(()),
        }
    }

    /// [otel] One KV call, on the instruments (metrics.rs), when the host gave a meter
    #[cfg(feature = "tracer")]
    pub(crate) async fn metric_kv(&self, op: &'static str, ok: bool, took: std::time::Duration) {
        if let Some(i) = self.meter.read().await.as_ref() {
            i.kv(op, ok, took);
        }
    }

    /// Refuse a key that is served only locally, when the caller is not.
    ///
    /// Applied at the **handler** boundary because that is where the connection
    /// is still visible: `Request::remote_addr()` is populated by tonic's
    /// `TcpConnectInfo`, and by the time a request reaches `*_impl` it has been
    /// consumed into its inner message. `None` means a transport that cannot
    /// place the caller - a unix socket, or an in-process call - and is treated
    /// as local, since refusing those would refuse ourselves.
    ///
    /// This is a property of the connection, taken from the kernel. It is not
    /// `XPEER`, which any client can set.
    async fn deny_remote(&self, key: &[u8], remote: Option<std::net::SocketAddr>)
        -> Result<(), Status>
    {
        if self.policy.read().await.may_serve(key, remote) {
            return Ok(());
        }
        // The message names the rule rather than the key: telling a remote
        // caller which key it may not have is a catalogue it did not have
        // before it asked.
        Err(Status::permission_denied(tr!("srv.local_only")))
    }

    /// Refuse an API write under a `readonly` prefix: only the embedder writes
    /// there, in-process, and it does not come through these handlers.
    async fn deny_readonly(&self, key: &[u8], range_end: &[u8]) -> Result<(), Status> {
        let p = self.policy.read().await;
        let ok = if range_end.is_empty() { p.writable(key) } else { p.range_writable(key, range_end) };
        if ok {
            return Ok(());
        }
        Err(Status::permission_denied(tr!("srv.readonly")))
    }
}

#[async_trait]
impl Kv for EtcdNode {
    type RangeStreamStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<RangeStreamResponse, Status>> + Send>>;

    /// [proto] The range as `range` answers it, in one chunk: what a client merges from the
    /// chunks is that same answer, and etcds holds its keys in memory, so chunks would save nothing
    async fn range_stream(&self, request: Request<RangeRequest>) -> Result<Response<Self::RangeStreamStream>, Status> {
        let range = Kv::range(self, request).await?.into_inner();
        let chunk = RangeStreamResponse { range_response: Some(range) };
        Ok(Response::new(Box::pin(tokio_stream::once(Ok(chunk)))))
    }

    /// TODO implement all variation
    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        // [auth] reading the range
        self.check_call(request.metadata(), Some((&request.get_ref().key, &request.get_ref().range_end, crate::coord::READ))).await?;
        // [sys] the system records: a peer's range over them, no client's - [review 2026-11-21, 2]
        // a takeover's with its ballot, promised here
        if self.sys_call(request.metadata(), &request.get_ref().key, request.remote_addr()).await?.is_some() {
            let ballot = request.metadata().get(crate::sys::XBALLOT).and_then(|v| v.to_str().ok()).and_then(crate::sys::parse_ballot);
            let kvs = self.sys_range_for_peer(ballot).await?;
            let count = kvs.len() as i64;
            return Ok(Response::new(RangeResponse { header: None, kvs, more: false, count }));
        }
        let remote = request.remote_addr();
        self.deny_remote(&request.get_ref().key, remote).await?;
        // [E2] a range is checked over its whole span, not by its first key: a
        // count of a restricted span is refused outright (a count cannot be
        // filtered), and the keys of any other span are filtered one by one
        let (key, end, count_only) = {
            let r = request.get_ref();
            (r.key.clone(), r.range_end.clone(), r.count_only)
        };
        let span_ok = self.policy.read().await.range_may_serve(&key, &end, remote);
        if !span_ok && count_only {
            return Err(Status::permission_denied(tr!("srv.local_only")));
        }
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("get"));
        let _started = std::time::Instant::now();
        let got = self.get_impl(request).await;
        #[cfg(feature = "tracer")]
        self.metric_kv("get", got.is_ok(), _started.elapsed()).await;
        let mut result = got?;
        if !span_ok {
            let p = self.policy.read().await;
            let r = result.get_mut();
            let before = r.kvs.len();
            r.kvs.retain(|kv| p.may_serve(&kv.key, remote));
            r.count -= (before - r.kvs.len()) as i64;
        }
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end());
        Ok(result)
    }

    /// TODO implement all variation of Requests ignores
    /// 1. if from peer - do not send to any peers
    /// 2. else if queue producer - do not put to vault, send to queue dispatcher host
    /// 3. else if send to 
    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        // [approve] a queue's approval definition, or a vote on a waiting message: root's and
        // labac's to allow, not a key range's - asked before the key's own permission
        #[cfg(feature = "approvals")]
        if let Some(answer) = self.approve_put(request.metadata(), request.get_ref(), true).await? {
            return Ok(answer);
        }
        self.check_call(request.metadata(), Some((&request.get_ref().key, &[], crate::coord::WRITE))).await?;
        // [sys] a record the leader replicates, from a peer; no client's
        if self.sys_call(request.metadata(), &request.get_ref().key, request.remote_addr()).await?.is_some() {
            let key = String::from_utf8_lossy(&request.get_ref().key).trim_start_matches(crate::sys::SYS_PREFIX).to_string();
            // a peer waking this leader's consumer of the sequences' requests
            if key == crate::sys::SEQ_WAKE && crate::coord::has(request.metadata(), crate::coord::XFWD) {
                if self.route_to_leader(true).await?.is_none() {
                    self.seq_consumer_start().await;
                }
                return Ok(Response::new(PutResponse { header: None, prev_kv: None }));
            }
            if !crate::coord::has(request.metadata(), crate::coord::XREP) {
                return Err(Status::permission_denied(tr!("sys.peers_only")));
            }
            self.sys_put_from_peer(&key, &request.get_ref().value, request.remote_addr()).await?;
            return Ok(Response::new(PutResponse { header: None, prev_kv: None }));
        }
        // [sys] the sequences' own queues: a client puts a request only - no definition, no
        // answer on a node's reply queue ([review 2026-11-21, 2] a peer's put proved)
        if crate::sys::reserved_queue(&request.get_ref().key) && !crate::sys::is_seq_key(&request.get_ref().key) {
            self.verified_peer(request.metadata(), request.remote_addr()).await?;
        }
        // [sys] a client's request for numbers: the leader's consumer woken before it lands
        if crate::sys::is_seq_key(&request.get_ref().key) && request.metadata().get(crate::etcdpb::XPEER).is_none() {
            self.seq_wake_leader().await?;
        }
        // [review 2026-11-21] the previous value comes back: reading it is needed too
        if request.get_ref().prev_kv {
            self.check_call(request.metadata(), Some((&request.get_ref().key, &[], crate::coord::READ))).await?;
        }
        self.deny_remote(&request.get_ref().key, request.remote_addr()).await?;
        self.deny_readonly(&request.get_ref().key, &[]).await?;
        // [approve] a message put to a gated queue waits for its approvals: held on the leader,
        // not handed to a consumer
        #[cfg(feature = "approvals")]
        if let Some(answer) = self.approve_put(request.metadata(), request.get_ref(), false).await? {
            return Ok(answer);
        }
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("put"));

        let _started = std::time::Instant::now();
        let result = self.put_impl(request).await;
        #[cfg(feature = "tracer")]
        self.metric_kv("put", result.is_ok(), _started.elapsed()).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end());
        result
    }


    /// TODO implement all variation of request
    /// TODO implement kv owner and queue consumer access 
    async fn delete_range(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
        // [approve] a waiting message withdrawn, or a queue's definition taken away
        #[cfg(feature = "approvals")]
        if let Some(answer) = self.approve_delete(request.metadata(), request.get_ref()).await? {
            return Ok(answer);
        }
        self.check_call(request.metadata(), Some((&request.get_ref().key, &request.get_ref().range_end, crate::coord::WRITE))).await?;
        // [sys] the records are deleted by the leader's put of a tombstone, never by a delete
        if self.sys_call(request.metadata(), &request.get_ref().key, request.remote_addr()).await?.is_some() {
            return Err(Status::permission_denied(tr!("sys.peers_only")));
        }
        // [review 2026-11-21, 2] nor what the sequences' queues hold, but by a peer
        if crate::sys::reserved_queue(&request.get_ref().key) {
            self.verified_peer(request.metadata(), request.remote_addr()).await?;
        }
        if request.get_ref().prev_kv {
            self.check_call(request.metadata(), Some((&request.get_ref().key, &request.get_ref().range_end, crate::coord::READ))).await?;
        }
        self.deny_remote(&request.get_ref().key, request.remote_addr()).await?;
        self.deny_readonly(&request.get_ref().key, &request.get_ref().range_end).await?;
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("delete"));
        let _started = std::time::Instant::now();
        let result = self.delete_impl(request).await;
        #[cfg(feature = "tracer")]
        self.metric_kv("delete", result.is_ok(), _started.elapsed()).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end()); 
        result
    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        // [auth] each range it compares or reads, each it writes
        let wants = EtcdNode::txn_access(request.get_ref());
        if wants.is_empty() {
            self.check_call(request.metadata(), None).await?;
        }
        for (k, e, w) in &wants {
            self.check_call(request.metadata(), Some((k, e, *w))).await?;
            // [sys] no txn reaches the system records or the sequences' queues
            let _ = e;
            if k.starts_with(crate::sys::SYS_PREFIX.as_bytes()) || crate::sys::reserved_queue(k) {
                return Err(Status::permission_denied(tr!("sys.peers_only")));
            }
        }
        // every key the txn touches, or a restricted one could ride in on an
        // unrestricted request
        let remote = request.remote_addr();
        for k in crate::kv::txn_keys(request.get_ref()) {
            self.deny_remote(&k, remote).await?;
            self.deny_readonly(&k, &[]).await?;
        }
        // [approve] nor the approvals' keys, nor a put into a gated queue
        #[cfg(feature = "approvals")]
        {
            let writes: Vec<(Vec<u8>, Vec<u8>)> = wants.iter().filter(|(_, _, w)| *w == crate::coord::WRITE).map(|(k, e, _)| (k.clone(), e.clone())).collect();
            self.approve_txn(request.metadata(), &writes).await?;
        }
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("txn"));
        let _started = std::time::Instant::now();
        let result = self.txn_impl(request).await;
        #[cfg(feature = "tracer")]
        self.metric_kv("txn", result.is_ok(), _started.elapsed()).await;
        result
    }

    async fn compact(&self, request: Request<CompactionRequest>) -> Result<Response<CompactionResponse>, Status> {
        self.check_call(request.metadata(), None).await?;
        Ok(Response::new(CompactionResponse { header: None }))
    }
}

#[cfg(test)]
mod remote_tests {
    //! [E2] `local_only_prefix` is a property of the CONNECTION: a remote caller
    //! is refused, a loopback or in-process one is served. These requests carry
    //! the connection info tonic's TCP server would attach, so the handlers see
    //! a caller on the network.
    use super::*;
    use crate::etcdpb::etcdserverpb::{request_op, RequestOp};
    use tonic::transport::server::TcpConnectInfo;

    async fn node() -> EtcdNode {
        let mut cfg = crate::cli::EtcdConfig::with_defaults();
        cfg.cluster = "11111111-1111-5111-8111-111111111111".to_string();
        cfg.listen_client_urls = "127.0.0.1:0".to_string();
        cfg.local_only_prefix = "/secret/".to_string();
        cfg.local_only_prefix_except = "/secret/public/".to_string();
        let n = EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()),
            #[cfg(feature = "tracer")] None).await.unwrap();
        for k in ["/secret/a", "/secret/public/b", "/open/c"] {
            n.kv_put(k.as_bytes().to_vec(), b"v".to_vec()).await.unwrap();
        }
        n
    }

    fn from<T>(addr: &str, mut r: Request<T>) -> Request<T> {
        r.extensions_mut().insert(TcpConnectInfo {
            local_addr: "127.0.0.1:2379".parse().ok(),
            remote_addr: addr.parse().ok(),
        });
        r
    }

    const FAR: &str = "10.0.0.7:5555";
    const NEAR: &str = "127.0.0.1:5555";

    fn get(key: &str, end: &str, count_only: bool) -> RangeRequest {
        RangeRequest { key: key.into(), range_end: end.into(), count_only, ..Default::default() }
    }

    #[tokio::test]
    async fn a_remote_caller_cannot_read_a_local_only_key() {
        let n = node().await;
        let e = n.range(from(FAR, Request::new(get("/secret/a", "", false)))).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        assert!(!e.message().contains("/secret/a"), "the refusal does not name the key");
        // the exception is served to anyone, and loopback gets everything
        let r = n.range(from(FAR, Request::new(get("/secret/public/b", "", false)))).await.unwrap();
        assert_eq!(r.get_ref().count, 1);
        let r = n.range(from(NEAR, Request::new(get("/secret/a", "", false)))).await.unwrap();
        assert_eq!(r.get_ref().count, 1);
    }

    /// A remote range over everything comes back without the local-only keys,
    /// and its count says so; a count of such a range is refused outright.
    #[tokio::test]
    async fn a_remote_range_is_filtered_and_its_count_refused() {
        let n = node().await;
        let r = n.range(from(FAR, Request::new(get("/", "\0", false)))).await.unwrap().into_inner();
        let keys: Vec<String> = r.kvs.iter().map(|kv| String::from_utf8_lossy(&kv.key).to_string()).collect();
        assert_eq!(keys, vec!["/open/c", "/secret/public/b"]);
        assert_eq!(r.count, 2, "the count matches what was served");

        let e = n.range(from(FAR, Request::new(get("/", "\0", true)))).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "a count cannot be filtered");
        let r = n.range(from(NEAR, Request::new(get("/", "\0", true)))).await.unwrap().into_inner();
        assert_eq!(r.count, 3, "loopback counts everything");
    }

    #[tokio::test]
    async fn a_remote_caller_cannot_write_or_delete_a_local_only_key() {
        let n = node().await;
        let put = PutRequest { key: b"/secret/x".to_vec(), value: b"v".to_vec(), ..Default::default() };
        let e = n.put(from(FAR, Request::new(put.clone()))).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        assert!(n.kv_get(b"/secret/x").await.is_none());
        n.put(from(NEAR, Request::new(put))).await.unwrap();
        assert!(n.kv_get(b"/secret/x").await.is_some());

        let del = DeleteRangeRequest { key: b"/secret/a".to_vec(), ..Default::default() };
        let e = n.delete_range(from(FAR, Request::new(del))).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        assert!(n.kv_get(b"/secret/a").await.is_some());
    }

    /// Every key a txn touches is checked, the compares included: an
    /// unrestricted request cannot carry a restricted key in.
    #[tokio::test]
    async fn a_txn_touching_a_local_only_key_is_refused_remotely() {
        let n = node().await;
        let txn = TxnRequest {
            success: vec![
                RequestOp { request: Some(request_op::Request::RequestRange(get("/open/c", "", false))) },
                RequestOp { request: Some(request_op::Request::RequestRange(get("/secret/a", "", false))) },
            ],
            ..Default::default()
        };
        let e = n.txn(from(FAR, Request::new(txn.clone()))).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        let r = n.txn(from(NEAR, Request::new(txn))).await.unwrap().into_inner();
        assert_eq!(r.responses.len(), 2);
    }

    /// `readonly_prefix` refuses a txn write as it refuses a put.
    #[tokio::test]
    async fn a_txn_write_under_a_readonly_prefix_is_refused() {
        let mut cfg = crate::cli::EtcdConfig::with_defaults();
        cfg.listen_client_urls = "127.0.0.1:0".to_string();
        cfg.cluster = "11111111-1111-5111-8111-111111111111".to_string();
        cfg.readonly_prefix = "/ro/".to_string();
        let n = EtcdNode::init(cfg, slog::Logger::root(slog::Discard, slog::o!()),
            #[cfg(feature = "tracer")] None).await.unwrap();
        let txn = TxnRequest {
            success: vec![RequestOp { request: Some(request_op::Request::RequestPut(
                PutRequest { key: b"/ro/k".to_vec(), value: b"v".to_vec(), ..Default::default() })) }],
            ..Default::default()
        };
        let e = n.txn(Request::new(txn)).await.unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        assert!(n.kv_get(b"/ro/k").await.is_none());
    }
}
