#[cfg(feature = "tracer")]
use opentelemetry::trace::Tracer;

use crate::cluster::EtcdNode;
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse};
use tonic::{async_trait, Request, Response, Status};

#[async_trait]
impl Kv for EtcdNode {
    
    /// TODO implement all variation
    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
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
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("put"));

        let result = self.put_impl(request).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end());
        result
    }


    /// TODO implement all variation of request
    /// TODO implement kv owner and queue consumer access 
    async fn delete_range(&self, request: Request<DeleteRangeRequest>) -> Result<Response<DeleteRangeResponse>, Status> {
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("delete"));
        let result = self.delete_impl(request).await;
        // #[cfg(feature = "tracer")] let _ = _s.map(|mut s| s.end()); 
        result
    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        #[cfg(feature = "tracer")]
        let _s = self.tracer.read().await.as_ref().map(|t| t.start("txn"));
        self.txn_impl(request).await
    }

    async fn compact(&self, _request: Request<CompactionRequest>) -> Result<Response<CompactionResponse>, Status> {
        Ok(Response::new(CompactionResponse { header: None }))
    }
}
