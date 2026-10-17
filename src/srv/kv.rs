#[cfg(feature = "tracer")]
use opentelemetry::trace::Tracer;

use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse};
use tonic::{async_trait, Request, Response, Status};


impl EtcdNode {
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
    /// place the caller — a unix socket, or an in-process call — and is treated
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
        Err(Status::permission_denied(
            "this key prefix is served only on a local connection"))
    }

    /// Refuse an API write under a `readonly` prefix: only the embedder writes
    /// there, in-process, and it does not come through these handlers.
    async fn deny_readonly(&self, key: &[u8], range_end: &[u8]) -> Result<(), Status> {
        let p = self.policy.read().await;
        let ok = if range_end.is_empty() { p.writable(key) } else { p.range_writable(key, range_end) };
        if ok {
            return Ok(());
        }
        Err(Status::permission_denied("this key prefix is written only by the node itself"))
    }
}

#[async_trait]
impl Kv for EtcdNode {
    
    /// TODO implement all variation
    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
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
            return Err(Status::permission_denied("this key prefix is served only on a local connection"));
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
        self.deny_remote(&request.get_ref().key, request.remote_addr()).await?;
        self.deny_readonly(&request.get_ref().key, &[]).await?;
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
        // every key the txn touches, or a restricted one could ride in on an
        // unrestricted request
        let remote = request.remote_addr();
        for k in crate::kv::txn_keys(request.get_ref()) {
            self.deny_remote(&k, remote).await?;
            self.deny_readonly(&k, &[]).await?;
        }
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("txn"));
        let _started = std::time::Instant::now();
        let result = self.txn_impl(request).await;
        #[cfg(feature = "tracer")]
        self.metric_kv("txn", result.is_ok(), _started.elapsed()).await;
        result
    }

    async fn compact(&self, _request: Request<CompactionRequest>) -> Result<Response<CompactionResponse>, Status> {
        Ok(Response::new(CompactionResponse { header: None }))
    }
}
