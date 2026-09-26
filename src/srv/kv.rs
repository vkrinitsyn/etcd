#[cfg(feature = "tracer")]
use opentelemetry::trace::Tracer;

use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse};
use tonic::{async_trait, Request, Response, Status};


impl EtcdNode {
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
}

#[async_trait]
impl Kv for EtcdNode {
    
    /// TODO implement all variation
    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        self.deny_remote(&request.get_ref().key, request.remote_addr()).await?;
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("get"));
        let result = self.get_impl(request).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end());
        result
    }

    /// TODO implement all variation of Requests ignores
    /// 1. if from peer - do not send to any peers
    /// 2. else if queue producer - do not put to vault, send to queue dispatcher host
    /// 3. else if send to 
    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        self.deny_remote(&request.get_ref().key, request.remote_addr()).await?;
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("put"));

        let result = self.put_impl(request).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end());
        result
    }


    /// TODO implement all variation of request
    /// TODO implement kv owner and queue consumer access 
    async fn delete_range(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
        self.deny_remote(&request.get_ref().key, request.remote_addr()).await?;
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("delete"));
        let result = self.delete_impl(request).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end()); 
        result
    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        // every key the txn touches, or a restricted one could ride in on an
        // unrestricted request
        let remote = request.remote_addr();
        for k in crate::kv::txn_keys(request.get_ref()) {
            self.deny_remote(&k, remote).await?;
        }
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("txn"));
        self.txn_impl(request).await
    }

    async fn compact(&self, _request: Request<CompactionRequest>) -> Result<Response<CompactionResponse>, Status> {
        Ok(Response::new(CompactionResponse { header: None }))
    }
}
