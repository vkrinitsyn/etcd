// This file is @generated by prost-build.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CampaignRequest {
    /// name is the election's identifier for the campaign.
    #[prost(bytes = "vec", tag = "1")]
    pub name: ::prost::alloc::vec::Vec<u8>,
    /// lease is the ID of the lease attached to leadership of the election. If the
    /// lease expires or is revoked before resigning leadership, then the
    /// leadership is transferred to the next campaigner, if any.
    #[prost(int64, tag = "2")]
    pub lease: i64,
    /// value is the initial proclaimed value set when the campaigner wins the
    /// election.
    #[prost(bytes = "vec", tag = "3")]
    pub value: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct CampaignResponse {
    #[prost(message, optional, tag = "1")]
    pub header: ::core::option::Option<super::etcdserverpb::ResponseHeader>,
    /// leader describes the resources used for holding leadership of the election.
    #[prost(message, optional, tag = "2")]
    pub leader: ::core::option::Option<LeaderKey>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LeaderKey {
    /// name is the election identifier that corresponds to the leadership key.
    #[prost(bytes = "vec", tag = "1")]
    pub name: ::prost::alloc::vec::Vec<u8>,
    /// key is an opaque key representing the ownership of the election. If the key
    /// is deleted, then leadership is lost.
    #[prost(bytes = "vec", tag = "2")]
    pub key: ::prost::alloc::vec::Vec<u8>,
    /// rev is the creation revision of the key. It can be used to test for ownership
    /// of an election during transactions by testing the key's creation revision
    /// matches rev.
    #[prost(int64, tag = "3")]
    pub rev: i64,
    /// lease is the lease ID of the election leader.
    #[prost(int64, tag = "4")]
    pub lease: i64,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LeaderRequest {
    /// name is the election identifier for the leadership information.
    #[prost(bytes = "vec", tag = "1")]
    pub name: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LeaderResponse {
    #[prost(message, optional, tag = "1")]
    pub header: ::core::option::Option<super::etcdserverpb::ResponseHeader>,
    /// kv is the key-value pair representing the latest leader update.
    #[prost(message, optional, tag = "2")]
    pub kv: ::core::option::Option<super::mvccpb::KeyValue>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ResignRequest {
    /// leader is the leadership to relinquish by resignation.
    #[prost(message, optional, tag = "1")]
    pub leader: ::core::option::Option<LeaderKey>,
}
#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct ResignResponse {
    #[prost(message, optional, tag = "1")]
    pub header: ::core::option::Option<super::etcdserverpb::ResponseHeader>,
}
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ProclaimRequest {
    /// leader is the leadership hold on the election.
    #[prost(message, optional, tag = "1")]
    pub leader: ::core::option::Option<LeaderKey>,
    /// value is an update meant to overwrite the leader's current value.
    #[prost(bytes = "vec", tag = "2")]
    pub value: ::prost::alloc::vec::Vec<u8>,
}
#[derive(Clone, Copy, PartialEq, ::prost::Message)]
pub struct ProclaimResponse {
    #[prost(message, optional, tag = "1")]
    pub header: ::core::option::Option<super::etcdserverpb::ResponseHeader>,
}
/// Generated client implementations.
pub mod election_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /// The election service exposes client-side election facilities as a gRPC interface.
    #[derive(Debug, Clone)]
    pub struct ElectionClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl<T> ElectionClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> ElectionClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            ElectionClient::new(InterceptedService::new(inner, interceptor))
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        /// Campaign waits to acquire leadership in an election, returning a LeaderKey
        /// representing the leadership if successful. The LeaderKey can then be used
        /// to issue new values on the election, transactionally guard API requests on
        /// leadership still being held, and resign from the election.
        pub async fn campaign(
            &mut self,
            request: impl tonic::IntoRequest<super::CampaignRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CampaignResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/v3electionpb.Election/Campaign",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("v3electionpb.Election", "Campaign"));
            self.inner.unary(req, path, codec).await
        }
        /// Proclaim updates the leader's posted value with a new value.
        pub async fn proclaim(
            &mut self,
            request: impl tonic::IntoRequest<super::ProclaimRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ProclaimResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/v3electionpb.Election/Proclaim",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("v3electionpb.Election", "Proclaim"));
            self.inner.unary(req, path, codec).await
        }
        /// Leader returns the current election proclamation, if any.
        pub async fn leader(
            &mut self,
            request: impl tonic::IntoRequest<super::LeaderRequest>,
        ) -> std::result::Result<tonic::Response<super::LeaderResponse>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/v3electionpb.Election/Leader",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("v3electionpb.Election", "Leader"));
            self.inner.unary(req, path, codec).await
        }
        /// Observe streams election proclamations in-order as made by the election's
        /// elected leaders.
        pub async fn observe(
            &mut self,
            request: impl tonic::IntoRequest<super::LeaderRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::LeaderResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/v3electionpb.Election/Observe",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("v3electionpb.Election", "Observe"));
            self.inner.server_streaming(req, path, codec).await
        }
        /// Resign releases election leadership so other campaigners may acquire
        /// leadership on the election.
        pub async fn resign(
            &mut self,
            request: impl tonic::IntoRequest<super::ResignRequest>,
        ) -> std::result::Result<tonic::Response<super::ResignResponse>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/v3electionpb.Election/Resign",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("v3electionpb.Election", "Resign"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated server implementations.
pub mod election_server {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    /// Generated trait containing gRPC methods that should be implemented for use with ElectionServer.
    #[async_trait]
    pub trait Election: std::marker::Send + std::marker::Sync + 'static {
        /// Campaign waits to acquire leadership in an election, returning a LeaderKey
        /// representing the leadership if successful. The LeaderKey can then be used
        /// to issue new values on the election, transactionally guard API requests on
        /// leadership still being held, and resign from the election.
        async fn campaign(
            &self,
            request: tonic::Request<super::CampaignRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CampaignResponse>,
            tonic::Status,
        >;
        /// Proclaim updates the leader's posted value with a new value.
        async fn proclaim(
            &self,
            request: tonic::Request<super::ProclaimRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ProclaimResponse>,
            tonic::Status,
        >;
        /// Leader returns the current election proclamation, if any.
        async fn leader(
            &self,
            request: tonic::Request<super::LeaderRequest>,
        ) -> std::result::Result<tonic::Response<super::LeaderResponse>, tonic::Status>;
        /// Server streaming response type for the Observe method.
        type ObserveStream: tonic::codegen::tokio_stream::Stream<
                Item = std::result::Result<super::LeaderResponse, tonic::Status>,
            >
            + std::marker::Send
            + 'static;
        /// Observe streams election proclamations in-order as made by the election's
        /// elected leaders.
        async fn observe(
            &self,
            request: tonic::Request<super::LeaderRequest>,
        ) -> std::result::Result<tonic::Response<Self::ObserveStream>, tonic::Status>;
        /// Resign releases election leadership so other campaigners may acquire
        /// leadership on the election.
        async fn resign(
            &self,
            request: tonic::Request<super::ResignRequest>,
        ) -> std::result::Result<tonic::Response<super::ResignResponse>, tonic::Status>;
    }
    /// The election service exposes client-side election facilities as a gRPC interface.
    #[derive(Debug)]
    pub struct ElectionServer<T> {
        inner: Arc<T>,
        accept_compression_encodings: EnabledCompressionEncodings,
        send_compression_encodings: EnabledCompressionEncodings,
        max_decoding_message_size: Option<usize>,
        max_encoding_message_size: Option<usize>,
    }
    impl<T> ElectionServer<T> {
        pub fn new(inner: T) -> Self {
            Self::from_arc(Arc::new(inner))
        }
        pub fn from_arc(inner: Arc<T>) -> Self {
            Self {
                inner,
                accept_compression_encodings: Default::default(),
                send_compression_encodings: Default::default(),
                max_decoding_message_size: None,
                max_encoding_message_size: None,
            }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> InterceptedService<Self, F>
        where
            F: tonic::service::Interceptor,
        {
            InterceptedService::new(Self::new(inner), interceptor)
        }
        /// Enable decompressing requests with the given encoding.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.accept_compression_encodings.enable(encoding);
            self
        }
        /// Compress responses with the given encoding, if the client supports it.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.send_compression_encodings.enable(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.max_decoding_message_size = Some(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.max_encoding_message_size = Some(limit);
            self
        }
    }
    impl<T, B> tonic::codegen::Service<http::Request<B>> for ElectionServer<T>
    where
        T: Election,
        B: Body + std::marker::Send + 'static,
        B::Error: Into<StdError> + std::marker::Send + 'static,
    {
        type Response = http::Response<tonic::body::BoxBody>;
        type Error = std::convert::Infallible;
        type Future = BoxFuture<Self::Response, Self::Error>;
        fn poll_ready(
            &mut self,
            _cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            match req.uri().path() {
                "/v3electionpb.Election/Campaign" => {
                    #[allow(non_camel_case_types)]
                    struct CampaignSvc<T: Election>(pub Arc<T>);
                    impl<T: Election> tonic::server::UnaryService<super::CampaignRequest>
                    for CampaignSvc<T> {
                        type Response = super::CampaignResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::CampaignRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Election>::campaign(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = CampaignSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/v3electionpb.Election/Proclaim" => {
                    #[allow(non_camel_case_types)]
                    struct ProclaimSvc<T: Election>(pub Arc<T>);
                    impl<T: Election> tonic::server::UnaryService<super::ProclaimRequest>
                    for ProclaimSvc<T> {
                        type Response = super::ProclaimResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ProclaimRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Election>::proclaim(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ProclaimSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/v3electionpb.Election/Leader" => {
                    #[allow(non_camel_case_types)]
                    struct LeaderSvc<T: Election>(pub Arc<T>);
                    impl<T: Election> tonic::server::UnaryService<super::LeaderRequest>
                    for LeaderSvc<T> {
                        type Response = super::LeaderResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::LeaderRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Election>::leader(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = LeaderSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/v3electionpb.Election/Observe" => {
                    #[allow(non_camel_case_types)]
                    struct ObserveSvc<T: Election>(pub Arc<T>);
                    impl<
                        T: Election,
                    > tonic::server::ServerStreamingService<super::LeaderRequest>
                    for ObserveSvc<T> {
                        type Response = super::LeaderResponse;
                        type ResponseStream = T::ObserveStream;
                        type Future = BoxFuture<
                            tonic::Response<Self::ResponseStream>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::LeaderRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Election>::observe(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ObserveSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.server_streaming(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                "/v3electionpb.Election/Resign" => {
                    #[allow(non_camel_case_types)]
                    struct ResignSvc<T: Election>(pub Arc<T>);
                    impl<T: Election> tonic::server::UnaryService<super::ResignRequest>
                    for ResignSvc<T> {
                        type Response = super::ResignResponse;
                        type Future = BoxFuture<
                            tonic::Response<Self::Response>,
                            tonic::Status,
                        >;
                        fn call(
                            &mut self,
                            request: tonic::Request<super::ResignRequest>,
                        ) -> Self::Future {
                            let inner = Arc::clone(&self.0);
                            let fut = async move {
                                <T as Election>::resign(&inner, request).await
                            };
                            Box::pin(fut)
                        }
                    }
                    let accept_compression_encodings = self.accept_compression_encodings;
                    let send_compression_encodings = self.send_compression_encodings;
                    let max_decoding_message_size = self.max_decoding_message_size;
                    let max_encoding_message_size = self.max_encoding_message_size;
                    let inner = self.inner.clone();
                    let fut = async move {
                        let method = ResignSvc(inner);
                        let codec = tonic::codec::ProstCodec::default();
                        let mut grpc = tonic::server::Grpc::new(codec)
                            .apply_compression_config(
                                accept_compression_encodings,
                                send_compression_encodings,
                            )
                            .apply_max_message_size_config(
                                max_decoding_message_size,
                                max_encoding_message_size,
                            );
                        let res = grpc.unary(method, req).await;
                        Ok(res)
                    };
                    Box::pin(fut)
                }
                _ => {
                    Box::pin(async move {
                        let mut response = http::Response::new(empty_body());
                        let headers = response.headers_mut();
                        headers
                            .insert(
                                tonic::Status::GRPC_STATUS,
                                (tonic::Code::Unimplemented as i32).into(),
                            );
                        headers
                            .insert(
                                http::header::CONTENT_TYPE,
                                tonic::metadata::GRPC_CONTENT_TYPE,
                            );
                        Ok(response)
                    })
                }
            }
        }
    }
    impl<T> Clone for ElectionServer<T> {
        fn clone(&self) -> Self {
            let inner = self.inner.clone();
            Self {
                inner,
                accept_compression_encodings: self.accept_compression_encodings,
                send_compression_encodings: self.send_compression_encodings,
                max_decoding_message_size: self.max_decoding_message_size,
                max_encoding_message_size: self.max_encoding_message_size,
            }
        }
    }
    /// Generated gRPC service name
    pub const SERVICE_NAME: &str = "v3electionpb.Election";
    impl<T> tonic::server::NamedService for ElectionServer<T> {
        const NAME: &'static str = SERVICE_NAME;
    }
}
